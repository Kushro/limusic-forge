//! Copy or move tracks from one playlist to another: a drop on a sidebar playlist, or "Move to…" in
//! the selection bar. PlaylistForge's fast-lane drop (`pf-app/src/jobs/drop_plan.rs`), with its
//! three duplicate policies:
//!
//! | policy        | track already in the target                | moving: the source row  |
//! |---------------|--------------------------------------------|-------------------------|
//! | `Skip`        | not added again                            | stays in the source     |
//! | `Allow`       | added again (a second copy)                | goes                    |
//! | `Consolidate` | not added again                            | goes: it lives only there now |
//!
//! Every row that went in is removed from the source on a move; a row YouTube refused is never
//! removed, whatever the policy, so a move can't lose a track.

use std::collections::HashMap;
use std::sync::Arc;

use innertube::SongItem;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use super::journal::{self, Named, OpRecord, Restore, Step, Summary};
use super::rows::{self, handle};
use crate::jobs::engine::{Engine, QueueTarget};
use crate::jobs::planner::{self, action_name, INNERTUBE_ROWS_PER_ITEM};
use crate::jobs::{JobItem, JobItemStatus, JobKind, NewJob, NewJobItem, ENGINE_KEY};
use crate::state::AppState;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    Copy,
    Move,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Duplicates {
    Skip,
    Allow,
    Consolidate,
}

impl Duplicates {
    pub fn as_str(self) -> &'static str {
        match self {
            Duplicates::Skip => "skip",
            Duplicates::Allow => "allow",
            Duplicates::Consolidate => "consolidate",
        }
    }
}

/// What a transfer did, for the toast.
#[derive(Debug, Serialize)]
pub struct Transferred {
    pub added: usize,
    pub duplicates: usize,
    pub refused: usize,
    pub removed: usize,
    pub op: Option<OpRecord>,
    /// The job it went through, when it was queued (`jobs/`). Absent when it ran directly. The
    /// counts are the job's when it settled in time, and zero when it is still waiting its turn.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub job_id: Option<i64>,
}

/// Which of the dragged rows the target already holds (by the index, so no request per
/// duplicate) and which to add, as indices into `rows`. With duplicates allowed, nothing is known.
pub fn split_known(
    rows: &[Restore],
    target: &str,
    policy: Duplicates,
    index: &HashMap<String, Vec<String>>,
) -> (Vec<usize>, Vec<usize>) {
    if policy == Duplicates::Allow {
        return (Vec::new(), (0..rows.len()).collect());
    }
    (0..rows.len()).partition(|&i| {
        index.get(&rows[i].song.video_id).is_some_and(|ids| ids.iter().any(|id| id == target))
    })
}

/// Whether this request takes rows out of its source.
pub fn is_move(req: &Request) -> bool {
    matches!((&req.source, req.mode), (Some(source), Mode::Move) if source.id != req.target.id)
}

/// The job a queued transfer runs as, or `None` when there is nothing to write. `known` and
/// `add` come from [`split_known`]; `occurrences` (one per row, `planner::occurrences`) is how
/// the Data API finds a moved row in its source.
pub fn queued_job(
    req: &Request,
    known: &[usize],
    add: &[usize],
    occurrences: &[i64],
    q: &QueueTarget,
) -> Option<NewJob> {
    let moving = is_move(req);
    let consolidate = moving && req.duplicates == Duplicates::Consolidate && !known.is_empty();
    let song = |i: usize| SongItem { set_video_id: None, ..req.rows[i].song.clone() };
    let occurrence = |i: usize| occurrences.get(i).copied().unwrap_or(0);
    let mut playlists: Vec<Named> = req.source.iter().cloned().collect();
    playlists.push(req.target.clone());
    let summary = Summary { playlists, count: req.rows.len() };
    let (kind, op_kind) =
        if moving { (JobKind::MoveItems, "move") } else { (JobKind::CopyItems, "copy") };
    let mut params = json!({
        ENGINE_KEY: q.engine.as_str(),
        "account": q.account,
        "op_kind": op_kind,
        "summary": summary,
        "target_id": req.target.id,
        "duplicates": req.duplicates.as_str(),
    });
    let (account_id, items, est) = match q.engine {
        Engine::Innertube => {
            let songs: Vec<SongItem> = add.iter().map(|&i| song(i)).collect();
            let allow = req.duplicates == Duplicates::Allow;
            let items: Vec<NewJobItem> = songs
                .chunks(INNERTUBE_ROWS_PER_ITEM)
                .map(|chunk| NewJobItem {
                    phase: 1,
                    action: action_name::IT_ADD_ROWS.to_string(),
                    params: json!({
                        "playlist_id": req.target.id,
                        "songs": chunk,
                        "allow_duplicates": allow,
                    }),
                })
                .collect();
            if moving {
                let source = req.source.as_ref().map(|s| s.id.clone());
                let known_songs: Vec<SongItem> = known.iter().map(|&i| song(i)).collect();
                params["source_id"] = json!(source);
                params["rows"] = json!(req.rows);
                params["known_dupes"] = json!(known_songs);
            }
            (None, items, 0)
        }
        Engine::Ytdata => {
            let dest = planner::ytdata_playlist_id(&req.target.id)?;
            let items: Vec<NewJobItem> = add
                .iter()
                .map(|&i| NewJobItem {
                    phase: 1,
                    action: action_name::PLAYLIST_ITEM_INSERT.to_string(),
                    params: json!({
                        "playlist_id": dest,
                        "video_id": req.rows[i].song.video_id,
                        "position": null,
                        "occurrence": occurrence(i),
                    }),
                })
                .collect();
            params["dest_playlist_id"] = json!(dest);
            if moving {
                let source = planner::ytdata_playlist_id(&req.source.as_ref()?.id)?;
                params["source_playlist_id"] = json!(source);
                if consolidate {
                    let extra: Vec<Value> = known
                        .iter()
                        .map(|&i| {
                            json!({
                                "video_id": req.rows[i].song.video_id,
                                "occurrence": occurrence(i),
                            })
                        })
                        .collect();
                    params["consolidate"] = json!(extra);
                }
            }
            let consolidated = if consolidate { known.len() } else { 0 };
            let deletes = if moving { add.len() + consolidated } else { 0 };
            let est = planner::estimate_transfer_units(add.len(), false)
                + planner::estimate_remove_units(deletes, req.rows.len()) * i64::from(moving);
            (q.channel_id.clone(), items, est)
        }
    };
    if items.is_empty() && !consolidate {
        return None;
    }
    Some(NewJob {
        account_id,
        kind,
        params,
        priority: q.priority,
        total_phases: if moving { 3 } else { 1 },
        est_units_total: est,
        items,
    })
}

/// What a queued transfer did so far, from its items. `known` is how many the index already
/// knew to be in the target (they never became items).
pub fn from_job(job_id: i64, items: &[JobItem], known: usize) -> Transferred {
    let mut t = Transferred {
        added: 0,
        duplicates: known,
        refused: 0,
        removed: 0,
        op: None,
        job_id: Some(job_id),
    };
    let count = |v: Option<&Value>, key: &str| {
        v.and_then(|r| r.get(key)).and_then(Value::as_array).map_or(0, Vec::len)
    };
    for item in items {
        let done = item.status == JobItemStatus::Done;
        let lost = matches!(item.status, JobItemStatus::Failed | JobItemStatus::Skipped);
        match item.action.as_str() {
            action_name::IT_ADD_ROWS if done => {
                t.added += count(item.api_result.as_ref(), "rows");
                t.duplicates += count(item.api_result.as_ref(), "duplicates");
                t.refused += count(item.api_result.as_ref(), "refused");
            }
            action_name::IT_ADD_ROWS if lost => t.refused += count(Some(&item.params), "songs"),
            action_name::IT_REMOVE_ROWS if done => {
                t.removed += item
                    .api_result
                    .as_ref()
                    .and_then(|r| r.get("removed"))
                    .and_then(Value::as_u64)
                    .unwrap_or(0) as usize;
            }
            action_name::PLAYLIST_ITEM_INSERT if done => t.added += 1,
            action_name::PLAYLIST_ITEM_INSERT if lost => t.refused += 1,
            action_name::PLAYLIST_ITEM_DELETE if done => t.removed += 1,
            _ => {}
        }
    }
    t
}

/// The source rows a move takes out: one per added song, plus, under `Consolidate`, one per song
/// the target already had. Matched by videoId and counted, so a song dragged twice and added once
/// takes out one row, not both.
pub fn rows_to_remove(
    rows: &[Restore],
    added: &[SongItem],
    duplicates: &[SongItem],
    policy: Duplicates,
) -> Vec<Restore> {
    let mut quota: HashMap<&str, usize> = HashMap::new();
    for s in added {
        *quota.entry(s.video_id.as_str()).or_default() += 1;
    }
    if policy == Duplicates::Consolidate {
        for s in duplicates {
            *quota.entry(s.video_id.as_str()).or_default() += 1;
        }
    }
    rows.iter()
        .filter(|r| {
            let Some(n) = quota.get_mut(r.song.video_id.as_str()) else { return false };
            if *n == 0 || r.song.set_video_id.is_none() {
                return false;
            }
            *n -= 1;
            true
        })
        .cloned()
        .collect()
}

pub struct Request {
    pub source: Option<Named>,
    pub target: Named,
    pub rows: Vec<Restore>,
    pub mode: Mode,
    pub duplicates: Duplicates,
}

pub async fn run(state: &Arc<AppState>, req: Request) -> Result<Transferred, String> {
    let none = &|| false;
    let target = &req.target.id;
    let songs: Vec<SongItem> =
        req.rows.iter().map(|r| SongItem { set_video_id: None, ..r.song.clone() }).collect();

    // The index answers "already there" without a request per duplicate. YouTube rejects a whole
    // batch over one duplicate and `add_rows` halves its way to it, so on a target that holds a
    // dozen of them this is the difference between one request and forty.
    let (known_dupes, to_add): (Vec<SongItem>, Vec<SongItem>) =
        if req.duplicates == Duplicates::Allow {
            (Vec::new(), songs)
        } else {
            let index = state.db.playlist_memberships();
            let (known, add) = split_known(&req.rows, target, req.duplicates, &index);
            let pick = |ix: Vec<usize>| -> Vec<SongItem> {
                ix.into_iter().map(|i| songs[i].clone()).collect()
            };
            (pick(known), pick(add))
        };
    let mut added =
        rows::add_rows(state, target, &to_add, req.duplicates == Duplicates::Allow, none).await?;
    added.duplicates.extend(known_dupes);

    let removed = match (&req.source, req.mode) {
        (Some(source), Mode::Move) if source.id != *target => {
            let out = rows_to_remove(&req.rows, &added.rows, &added.duplicates, req.duplicates);
            let gone: Vec<SongItem> = out.iter().map(|r| r.song.clone()).collect();
            rows::remove_rows(state, &source.id, &gone, none).await?;
            out
        }
        _ => Vec::new(),
    };

    let mut touched = vec![target.clone()];
    let mut undo = Vec::new();
    let mut playlists = Vec::new();
    if let Some(source) = &req.source {
        playlists.push(source.clone());
        if !removed.is_empty() {
            touched.push(source.id.clone());
            // Put the source back first, so its anchors are still there to aim at.
            undo.push(journal::restore_step(&source.id, &removed));
        }
    }
    playlists.push(req.target.clone());
    let added_rows: Vec<SongItem> =
        added.rows.iter().filter(|r| handle(r).is_some()).cloned().collect();
    if !added_rows.is_empty() {
        undo.push(Step::Remove { playlist_id: target.clone(), rows: added_rows });
    }
    journal::announce(state, &touched);

    let kind = if removed.is_empty() { "copy" } else { "move" };
    let summary = Summary { playlists, count: added.rows.len().max(removed.len()) };
    let op = if undo.is_empty() { None } else { journal::record(state, kind, &summary, &undo) };
    Ok(Transferred {
        added: added.rows.len(),
        duplicates: added.duplicates.len(),
        refused: added.refused.len(),
        removed: removed.len(),
        op,
        job_id: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(v: &str, set: &str) -> Restore {
        Restore {
            song: SongItem {
                video_id: v.into(),
                set_video_id: Some(set.into()),
                ..Default::default()
            },
            before: None,
        }
    }
    fn song(v: &str) -> SongItem {
        SongItem { video_id: v.into(), ..Default::default() }
    }
    fn sets(rows: &[Restore]) -> Vec<&str> {
        rows.iter().map(|r| r.song.set_video_id.as_deref().unwrap()).collect()
    }

    #[test]
    fn a_move_takes_out_what_went_in() {
        let rows = [row("a", "1"), row("b", "2"), row("c", "3")];
        // b was already in the target; c was refused.
        let out = rows_to_remove(&rows, &[song("a")], &[song("b")], Duplicates::Skip);
        assert_eq!(sets(&out), ["1"], "skip: b stays in the source, c is never removed");
        let out = rows_to_remove(&rows, &[song("a")], &[song("b")], Duplicates::Consolidate);
        assert_eq!(sets(&out), ["1", "2"], "consolidate: b now lives only in the target");
    }

    #[test]
    fn repeats_are_counted_not_matched_wholesale() {
        // The same song twice in the source, added once: one row goes, the other stays.
        let rows = [row("a", "1"), row("a", "2")];
        assert_eq!(sets(&rows_to_remove(&rows, &[song("a")], &[], Duplicates::Allow)), ["1"]);
    }

    fn req(mode: Mode, duplicates: Duplicates, rows: Vec<Restore>) -> Request {
        Request {
            source: Some(Named { id: "VLPLsrc".into(), title: "Src".into() }),
            target: Named { id: "VLPLdst".into(), title: "Dst".into() },
            rows,
            mode,
            duplicates,
        }
    }

    fn target(engine: Engine) -> QueueTarget {
        QueueTarget {
            engine,
            channel_id: (engine == Engine::Ytdata).then(|| "UC1".to_string()),
            account: Some("ga1".into()),
            priority: 2,
        }
    }

    #[test]
    fn the_index_answers_what_the_target_already_has() {
        let rows = [row("a", "1"), row("b", "2"), row("c", "3")];
        let index: HashMap<String, Vec<String>> =
            [("b".to_string(), vec!["VLPLdst".to_string()])].into();
        assert_eq!(split_known(&rows, "VLPLdst", Duplicates::Skip, &index), (vec![1], vec![0, 2]));
        let all = (vec![], vec![0, 1, 2]);
        assert_eq!(split_known(&rows, "VLPLdst", Duplicates::Allow, &index), all);
        assert_eq!(split_known(&rows, "VLPLx", Duplicates::Skip, &index), (vec![], vec![0, 1, 2]));
    }

    #[test]
    fn a_queued_innertube_move_carries_what_its_barrier_needs() {
        let r = req(Mode::Move, Duplicates::Consolidate, vec![row("a", "1"), row("b", "2")]);
        let job = queued_job(&r, &[1], &[0], &[0, 0], &target(Engine::Innertube)).unwrap();
        assert_eq!(job.kind, JobKind::MoveItems);
        assert_eq!(job.total_phases, 3);
        assert_eq!(job.account_id, None, "an InnerTube job has no Data API channel");
        assert_eq!(job.params["engine"], json!("innertube"));
        assert_eq!(job.params["account"], json!("ga1"));
        assert_eq!(job.params["source_id"], json!("VLPLsrc"));
        assert_eq!(job.params["duplicates"], json!("consolidate"));
        assert_eq!(job.params["known_dupes"][0]["video_id"], json!("b"));
        assert_eq!(job.items.len(), 1);
        assert_eq!(job.items[0].action, action_name::IT_ADD_ROWS);
        assert!(job.items[0].params["songs"][0].get("set_video_id").is_none(), "no old handle");

        let copy = req(Mode::Copy, Duplicates::Skip, vec![row("a", "1")]);
        let job = queued_job(&copy, &[], &[0], &[0], &target(Engine::Innertube)).unwrap();
        assert_eq!((job.kind, job.total_phases), (JobKind::CopyItems, 1));
        assert!(job.params.get("rows").is_none());
        assert!(queued_job(&copy, &[0], &[], &[0], &target(Engine::Innertube)).is_none());
    }

    #[test]
    fn a_queued_data_api_move_plans_inserts_by_occurrence() {
        let rows = vec![row("a", "1"), row("a", "2"), row("b", "3")];
        let r = req(Mode::Move, Duplicates::Consolidate, rows);
        let job = queued_job(&r, &[2], &[0, 1], &[0, 1, 0], &target(Engine::Ytdata)).unwrap();
        assert_eq!(job.account_id.as_deref(), Some("UC1"));
        assert_eq!(job.params["dest_playlist_id"], json!("PLdst"));
        assert_eq!(job.params["source_playlist_id"], json!("PLsrc"));
        assert_eq!(job.params["consolidate"], json!([{"video_id": "b", "occurrence": 0}]));
        let occ: Vec<i64> =
            job.items.iter().map(|i| i.params["occurrence"].as_i64().unwrap()).collect();
        assert_eq!(occ, [0, 1]);
        assert!(job.items.iter().all(|i| i.action == action_name::PLAYLIST_ITEM_INSERT));
        assert_eq!(job.est_units_total, 100 + (150 + 1));
        // A target the Data API can't address is not planned on it.
        let mut local = r;
        local.target.id = "VLLM".into();
        assert!(queued_job(&local, &[], &[0], &[0], &target(Engine::Ytdata)).is_none());
    }

    #[test]
    fn a_queued_transfer_reports_from_its_items() {
        let at: chrono::DateTime<chrono::Utc> =
            chrono::DateTime::parse_from_rfc3339("2026-07-15T12:00:00Z").unwrap().into();
        let item = |action: &str, status, params: Value, result: Option<Value>| JobItem {
            id: 1,
            job_id: 9,
            seq: 0,
            phase: 1,
            action: action.into(),
            params,
            status,
            api_result: result,
            inverse: None,
            attempts: 1,
            last_error: None,
            updated_at: at,
        };
        let items = [
            item(
                action_name::IT_ADD_ROWS,
                JobItemStatus::Done,
                json!({}),
                Some(json!({
                    "rows": [song("a"), song("b")],
                    "duplicates": [song("c")],
                    "refused": [],
                })),
            ),
            item(
                action_name::IT_ADD_ROWS,
                JobItemStatus::Failed,
                json!({"songs": [song("d")]}),
                None,
            ),
            item(
                action_name::IT_REMOVE_ROWS,
                JobItemStatus::Done,
                json!({}),
                Some(json!({"removed": 2})),
            ),
        ];
        let t = from_job(9, &items, 1);
        assert_eq!((t.added, t.duplicates, t.refused, t.removed), (2, 2, 1, 2));
        assert_eq!(t.job_id, Some(9));
    }

    #[test]
    fn a_row_without_a_handle_is_never_removed() {
        let mut r = row("a", "1");
        r.song.set_video_id = None;
        assert!(rows_to_remove(&[r], &[song("a")], &[], Duplicates::Allow).is_empty());
    }
}
