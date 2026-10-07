//! The InnerTube engine: the same writes the playlist tools make (`playlist_tools::rows`), one
//! batch per item, queued instead of run on the spot (`job_queue_mode = unified`).
//!
//! Every item but a job's first waits its turn on the shared pacer
//! (`import::before_playlist_write`), as a bulk tool's requests do, so the queue, an import and a
//! dedupe never fire together. Pushback (a 429/403, the bot check) starts the shared cooldown; the
//! item goes back and the job pauses until the cooldown ends, with no retry before (D27). A job is
//! run as the account that queued it: signed out or switched away, it waits for the user.
//!
//! Item actions (`planner::action_name`):
//! - `it_add_rows {playlist_id, songs, allow_duplicates}`: result `{rows, duplicates, refused}`,
//!   inverse `it_remove_rows` of the rows it made.
//! - `it_remove_rows {playlist_id, rows: [Restore]}`: rows no longer there are left alone; inverse
//!   `it_step` with the journal's `Restore`, which puts each one back where it was.
//! - `it_step {step}`: one undo journal step.
//! - `verify_destination`: the move barrier, re-reading the destination.

use std::collections::HashSet;
use std::sync::Arc;

use chrono::{DateTime, TimeZone, Utc};
use innertube::SongItem;
use serde_json::{json, Value};

use super::planner::action_name;
use super::runner::{Barrier, Executor, ItemOutcome, Reconcile};
use super::{set_param, Job, JobItem, JobItemStatus, NewJobItem};
use crate::import::{before_playlist_write, youtube_cooldown};
use crate::playlist_tools::journal::{self, Restore, Step};
use crate::playlist_tools::rows::{self, handle};
use crate::playlist_tools::transfer::{self, Duplicates};
use crate::state::{is_local_playlist, AppState};

pub struct InnerTubeExecutor {
    state: Arc<AppState>,
}

impl InnerTubeExecutor {
    pub fn new(state: Arc<AppState>) -> Self {
        Self { state }
    }

    /// Why this job can't run as things stand, if it can't: nobody signed in, or another account.
    fn account_check(&self, job: &Job) -> Option<ItemOutcome> {
        if !self.state.it.is_logged_in() {
            return Some(ItemOutcome::NeedsAuth("signed_out".into()));
        }
        let wanted = job.params.get("account").and_then(Value::as_str)?;
        let active = self.state.db.get_setting("active_account");
        (active.as_deref() != Some(wanted))
            .then(|| ItemOutcome::NeedsAuth("account_mismatch".into()))
    }

    async fn add(&self, item: &JobItem) -> ItemOutcome {
        let playlist = str_param(&item.params, "playlist_id");
        let songs: Vec<SongItem> = list_param(&item.params, "songs");
        let allow = item.params.get("allow_duplicates").and_then(Value::as_bool).unwrap_or(false);
        match rows::add_rows(&self.state, &playlist, &songs, allow, &|| false).await {
            Ok(added) => {
                let made: Vec<Restore> = added
                    .rows
                    .iter()
                    .filter(|r| handle(r).is_some())
                    .map(|r| Restore { song: r.clone(), before: None })
                    .collect();
                let inverse = (!made.is_empty()).then(|| {
                    json!({"action": action_name::IT_REMOVE_ROWS,
                           "params": {"playlist_id": playlist, "rows": made}})
                });
                let api_result = json!({
                    "rows": added.rows,
                    "duplicates": added.duplicates,
                    "refused": added.refused,
                });
                ItemOutcome::Done { api_result, inverse, units: 0, endpoint: None }
            }
            Err(e) => from_error(e),
        }
    }

    async fn remove(&self, item: &JobItem) -> ItemOutcome {
        let playlist = str_param(&item.params, "playlist_id");
        let wanted: Vec<Restore> = list_param(&item.params, "rows");
        // Only rows still there: one removed by hand meanwhile would fail the whole request.
        let now: HashSet<String> = match rows::read_all(&self.state, &playlist).await {
            Ok(current) => current.iter().filter_map(handle).map(str::to_owned).collect(),
            Err(e) => return from_error(e),
        };
        let gone: Vec<Restore> = wanted
            .into_iter()
            .filter(|r| handle(&r.song).is_some_and(|h| now.contains(h)))
            .collect();
        let songs: Vec<SongItem> = gone.iter().map(|r| r.song.clone()).collect();
        if let Err(e) = rows::remove_rows(&self.state, &playlist, &songs, &|| false).await {
            return from_error(e);
        }
        let inverse = (!gone.is_empty()).then(|| {
            let step = journal::restore_step(&playlist, &gone);
            json!({"action": action_name::IT_STEP, "params": {"step": step}})
        });
        let api_result = json!({"removed": gone.len()});
        ItemOutcome::Done { api_result, inverse, units: 0, endpoint: None }
    }

    async fn step(&self, item: &JobItem) -> ItemOutcome {
        let step = item.params.get("step").cloned().map(serde_json::from_value::<Step>);
        let Some(Ok(step)) = step else {
            return ItemOutcome::Failed("malformed journal step".into());
        };
        match journal::run_step(&self.state, &step).await {
            Ok(()) => {
                ItemOutcome::Done { api_result: json!({}), inverse: None, units: 0, endpoint: None }
            }
            Err(e) => from_error(e),
        }
    }
}

fn str_param(params: &Value, key: &str) -> String {
    params.get(key).and_then(Value::as_str).unwrap_or_default().to_string()
}

fn list_param<T: serde::de::DeserializeOwned>(params: &Value, key: &str) -> Vec<T> {
    params.get(key).and_then(|v| serde_json::from_value(v.clone()).ok()).unwrap_or_default()
}

/// The playlist an item writes (or, for the barrier, reads).
fn item_playlist(job: &Job, item: &JobItem) -> String {
    if item.action == action_name::VERIFY_DESTINATION {
        return str_param(&job.params, "target_id");
    }
    match item.params.get("playlist_id").and_then(Value::as_str) {
        Some(id) => id.to_string(),
        // A journal step names its own playlist.
        None => item
            .params
            .get("step")
            .and_then(|s| s.get("playlist_id"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
    }
}

/// The playlist tools' error codes (`import::playlist_write_error`): `cooldown:<until>` is
/// pushback, `gone` a stop; anything else is a message.
pub fn from_error(e: String) -> ItemOutcome {
    if let Some(rest) = e.strip_prefix("cooldown:") {
        let until = rest
            .trim()
            .parse::<i64>()
            .ok()
            .and_then(|s| Utc.timestamp_opt(s, 0).single())
            .unwrap_or_else(|| Utc::now() + chrono::Duration::minutes(15));
        return cooldown(until);
    }
    if e.starts_with("Sign in first") {
        return ItemOutcome::NeedsAuth("signed_out".into());
    }
    ItemOutcome::Retry(e)
}

fn cooldown(until: DateTime<Utc>) -> ItemOutcome {
    ItemOutcome::Cooldown { until, reason: "YouTube asked to slow down".into() }
}

impl Executor for InnerTubeExecutor {
    async fn execute(&self, job: &Job, item: &JobItem) -> ItemOutcome {
        let playlist = item_playlist(job, item);
        if !is_local_playlist(&playlist) {
            if let Some(until) = youtube_cooldown(&self.state) {
                return cooldown(Utc.timestamp_opt(until, 0).single().unwrap_or_else(Utc::now));
            }
            if let Some(outcome) = self.account_check(job) {
                return outcome;
            }
            // A job's first request goes straight out (one drop isn't made to wait); every one
            // after it waits its turn, like a bulk tool's.
            let first =
                job.done_items + job.failed_items + job.skipped_items == 0 && item.attempts <= 1;
            if !first {
                if let Err(e) = before_playlist_write(&self.state, || false).await {
                    return from_error(e);
                }
            }
        }
        match item.action.as_str() {
            action_name::VERIFY_DESTINATION => match rows::read_all(&self.state, &playlist).await {
                Ok(current) => ItemOutcome::Verified {
                    present: current.into_iter().map(|r| r.video_id).collect(),
                    units: 0,
                },
                Err(e) => from_error(e),
            },
            action_name::IT_ADD_ROWS => self.add(item).await,
            action_name::IT_REMOVE_ROWS => self.remove(item).await,
            action_name::IT_STEP => self.step(item).await,
            other => ItemOutcome::Failed(format!("unrecognized action {other}")),
        }
    }

    async fn reconcile(&self, _job: &Job, item: &JobItem) -> Reconcile {
        if item.action == action_name::IT_ADD_ROWS {
            // Whatever part of the batch landed must not land twice: the retry refuses
            // duplicates, so what is already there comes back as a duplicate instead.
            let params = set_param(item.params.clone(), "allow_duplicates", json!(false));
            return Reconcile::RequeueWith {
                params,
                reason: "interrupted; retried without duplicates".into(),
            };
        }
        // Removals skip rows already gone; a re-read and a journal step are safe to repeat.
        Reconcile::Requeue("interrupted; safe to repeat".into())
    }

    fn after_verify(&self, job: &Job, phase1: &[JobItem], present: &HashSet<String>) -> Barrier {
        innertube_move_barrier(job, phase1, present)
    }
}

/// The InnerTube move barrier. The source rows to take out are the transfer's own rule
/// (`transfer::rows_to_remove`: one per song that went in, plus the target's existing copies
/// under "consolidate"), counted only over songs the destination is confirmed to hold now. An
/// InnerTube add is not re-sent for a song not found (with duplicates allowed that could add it
/// twice): its source row simply stays. So nothing is ever "missing" here.
pub fn innertube_move_barrier(job: &Job, phase1: &[JobItem], present: &HashSet<String>) -> Barrier {
    let source = str_param(&job.params, "source_id");
    if source.is_empty() {
        return Barrier::Proceed(Vec::new());
    }
    let rows: Vec<Restore> = list_param(&job.params, "rows");
    let policy = match job.params.get("duplicates").and_then(Value::as_str) {
        Some("allow") => Duplicates::Allow,
        Some("consolidate") => Duplicates::Consolidate,
        _ => Duplicates::Skip,
    };
    let mut duplicates: Vec<SongItem> = list_param(&job.params, "known_dupes");
    let mut added: Vec<SongItem> = Vec::new();
    for copy in phase1.iter().filter(|i| i.status == JobItemStatus::Done) {
        let Some(result) = &copy.api_result else { continue };
        let rows: Vec<SongItem> = list_param(result, "rows");
        added.extend(rows.into_iter().filter(|s| present.contains(&s.video_id)));
        duplicates.extend(list_param::<SongItem>(result, "duplicates"));
    }
    let out = transfer::rows_to_remove(&rows, &added, &duplicates, policy);
    if out.is_empty() {
        return Barrier::Proceed(Vec::new());
    }
    Barrier::Proceed(vec![NewJobItem {
        phase: 3,
        action: action_name::IT_REMOVE_ROWS.to_string(),
        params: json!({"playlist_id": source, "rows": out}),
    }])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jobs::{JobKind, JobStatus};

    fn at() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-07-15T12:00:00Z").unwrap().with_timezone(&Utc)
    }

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

    fn job(params: Value) -> Job {
        Job {
            id: 1,
            account_id: None,
            kind: JobKind::MoveItems,
            params,
            status: JobStatus::Running,
            priority: 2,
            phase: 2,
            total_phases: 3,
            created_at: at(),
            started_at: None,
            finished_at: None,
            resume_at: None,
            est_units_total: 0,
            spent_units: 0,
            total_items: 0,
            done_items: 0,
            failed_items: 0,
            skipped_items: 0,
            planned_items: 0,
            retried_items: 0,
            last_error: None,
        }
    }

    fn add_item(id: i64, status: JobItemStatus, result: Option<Value>) -> JobItem {
        JobItem {
            id,
            job_id: 1,
            seq: id,
            phase: 1,
            action: action_name::IT_ADD_ROWS.into(),
            params: json!({}),
            status,
            api_result: result,
            inverse: None,
            attempts: 1,
            last_error: None,
            updated_at: at(),
        }
    }

    #[test]
    fn the_barrier_takes_out_only_rows_confirmed_in_the_destination() {
        let j = job(json!({
            "source_id": "VLPLsrc",
            "rows": [row("a", "1"), row("b", "2"), row("c", "3"), row("d", "4")],
            "duplicates": "consolidate",
            "known_dupes": [song("d")],
        }));
        // As `it_add_rows` stores them: whole serialized `SongItem`s.
        let added = |v: &str, set: &str| SongItem { set_video_id: Some(set.into()), ..song(v) };
        let result = json!({
            "rows": [added("a", "n1"), added("b", "n2")],
            "duplicates": [],
            "refused": [{"video_id": "c"}],
        });
        let phase1 = [add_item(1, JobItemStatus::Done, Some(result))];
        // b went in but isn't there now: its source row stays.
        let present: HashSet<String> = ["a", "d"].map(String::from).into();
        let Barrier::Proceed(items) = innertube_move_barrier(&j, &phase1, &present) else {
            panic!("never missing")
        };
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].phase, 3);
        assert_eq!(items[0].params["playlist_id"], json!("VLPLsrc"));
        let out: Vec<Restore> = list_param(&items[0].params, "rows");
        let handles: Vec<&str> = out.iter().map(|r| handle(&r.song).unwrap()).collect();
        assert_eq!(handles, ["1", "4"], "a moved; d consolidated; b unconfirmed; c refused");
    }

    #[test]
    fn a_copy_has_no_source_and_nothing_to_take_out() {
        let j = job(json!({"rows": [row("a", "1")]}));
        let phase1 = [add_item(1, JobItemStatus::Done, Some(json!({"rows": [song("a")]})))];
        let present: HashSet<String> = ["a".to_string()].into();
        assert_eq!(innertube_move_barrier(&j, &phase1, &present), Barrier::Proceed(Vec::new()));
    }

    #[test]
    fn error_codes_map_to_outcomes() {
        let until = at() + chrono::Duration::minutes(10);
        assert_eq!(
            from_error(format!("cooldown:{}", until.timestamp())),
            ItemOutcome::Cooldown { until, reason: "YouTube asked to slow down".into() }
        );
        let signed_out = ItemOutcome::NeedsAuth("signed_out".into());
        assert_eq!(from_error("Sign in first to use this.".into()), signed_out);
        assert_eq!(from_error("boom".into()), ItemOutcome::Retry("boom".into()));
        assert_eq!(from_error("gone".into()), ItemOutcome::Retry("gone".into()));
    }

    #[test]
    fn an_item_names_its_playlist() {
        let j = job(json!({"target_id": "VLPLdst"}));
        let mut item = add_item(1, JobItemStatus::Pending, None);
        item.params = json!({"playlist_id": "VLPLx"});
        assert_eq!(item_playlist(&j, &item), "VLPLx");
        item.action = action_name::VERIFY_DESTINATION.into();
        assert_eq!(item_playlist(&j, &item), "VLPLdst");
        item.action = action_name::IT_STEP.into();
        let step = json!({"step": "restore", "playlist_id": "LOCALPLAYLIST:2", "rows": []});
        item.params = json!({ "step": step });
        assert_eq!(item_playlist(&j, &item), "LOCALPLAYLIST:2");
    }
}
