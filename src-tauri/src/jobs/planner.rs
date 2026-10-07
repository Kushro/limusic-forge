//! What a job's items are: one Data API write per item ([`PlannedAction`], port of PlaylistForge's
//! `pf-core/src/planner/mod.rs`), or one InnerTube batch per item (the `it_*` actions, which reuse
//! `playlist_tools::rows`). Plus the pure helpers the dispatch layer and the move barrier need.
//!
//! ## `PlannedAction` <-> `job_items`
//!
//! [`PlannedAction::action_name`] is `job_items.action`; [`PlannedAction::to_params`] /
//! [`PlannedAction::from_params`] round-trip `job_items.params_json`. The two creates learn their
//! inverse only from the API result (the id Google assigned), so [`PlannedAction::inverse`] takes
//! it; a deleted playlist has no inverse (there is no undelete).
//!
//! ## Deletes by occurrence
//!
//! The app knows rows by InnerTube's `setVideoId`; the Data API by its own `playlistItem` id, and
//! the two are not derivable from each other. A Data API delete is therefore planned as "the
//! `occurrence`-th copy of `video_id` in `playlist_id`" (0-based, playlist order) and resolved by
//! the executor with one listing of the playlist, the first time any of the job's deletes runs
//! (`exec_ytdata`). Both APIs list a playlist in the same order, so the k-th copy is the same row.

use std::collections::{HashMap, HashSet};

use innertube::SongItem;
use serde_json::{json, Value};

use super::runner::Barrier;
use super::{Job, JobItem, JobItemStatus, NewJobItem};
use crate::playlist_tools::rows::handle;
use crate::quota::{endpoint, unit_cost};

/// `job_items.action` strings.
pub mod action_name {
    pub const PLAYLIST_INSERT: &str = "playlist_insert";
    pub const PLAYLIST_UPDATE: &str = "playlist_update";
    pub const PLAYLIST_DELETE: &str = "playlist_delete";
    pub const PLAYLIST_ITEM_INSERT: &str = "playlist_item_insert";
    pub const PLAYLIST_ITEM_UPDATE_POSITION: &str = "playlist_item_update_position";
    pub const PLAYLIST_ITEM_DELETE: &str = "playlist_item_delete";
    /// The move barrier (phase 2): never planned, the runner appends it.
    pub const VERIFY_DESTINATION: &str = crate::jobs::action_name::VERIFY_DESTINATION;
    /// InnerTube: append songs (`{playlist_id, songs, allow_duplicates}`), one request per 100.
    pub const IT_ADD_ROWS: &str = "it_add_rows";
    /// InnerTube: take rows out by handle (`{playlist_id, rows: [Restore]}`), each with the row it
    /// sat before, so its inverse can put it back there.
    pub const IT_REMOVE_ROWS: &str = "it_remove_rows";
    /// InnerTube: run one undo journal step (`{step}`), the inverse of a removal.
    pub const IT_STEP: &str = "it_step";
}

/// Rows per InnerTube item: one `edit_playlist` request (`rows::ACTIONS_PER_REQUEST`).
pub const INNERTUBE_ROWS_PER_ITEM: usize = 100;

/// One Data API write call, fully parameterized.
#[derive(Debug, Clone, PartialEq)]
pub enum PlannedAction {
    /// `privacy` is a raw `privacyStatus`: `public`, `unlisted` or `private`.
    PlaylistInsert {
        title: String,
        description: String,
        privacy: String,
    },
    PlaylistUpdate {
        playlist_id: String,
        title: String,
        description: String,
        privacy: String,
        prev_title: String,
        prev_description: String,
        prev_privacy: String,
    },
    /// No inverse: recreating a playlist cannot bring its items back.
    PlaylistDelete {
        playlist_id: String,
    },
    PlaylistItemInsert {
        playlist_id: String,
        video_id: String,
        position: Option<i64>,
    },
    PlaylistItemUpdatePosition {
        playlist_item_id: String,
        playlist_id: String,
        video_id: String,
        position: i64,
        prev_position: i64,
    },
    /// `video_id`, `playlist_id` and `prev_position` are what the inverse re-inserts.
    PlaylistItemDelete {
        playlist_item_id: String,
        video_id: String,
        playlist_id: String,
        prev_position: Option<i64>,
    },
}

impl PlannedAction {
    pub fn action_name(&self) -> &'static str {
        use action_name::*;
        match self {
            PlannedAction::PlaylistInsert { .. } => PLAYLIST_INSERT,
            PlannedAction::PlaylistUpdate { .. } => PLAYLIST_UPDATE,
            PlannedAction::PlaylistDelete { .. } => PLAYLIST_DELETE,
            PlannedAction::PlaylistItemInsert { .. } => PLAYLIST_ITEM_INSERT,
            PlannedAction::PlaylistItemUpdatePosition { .. } => PLAYLIST_ITEM_UPDATE_POSITION,
            PlannedAction::PlaylistItemDelete { .. } => PLAYLIST_ITEM_DELETE,
        }
    }

    /// The ledger's endpoint name for this call.
    pub fn endpoint(&self) -> &'static str {
        match self {
            PlannedAction::PlaylistInsert { .. } => endpoint::PLAYLISTS_INSERT,
            PlannedAction::PlaylistUpdate { .. } => endpoint::PLAYLISTS_UPDATE,
            PlannedAction::PlaylistDelete { .. } => endpoint::PLAYLISTS_DELETE,
            PlannedAction::PlaylistItemInsert { .. } => endpoint::PLAYLIST_ITEMS_INSERT,
            PlannedAction::PlaylistItemUpdatePosition { .. } => endpoint::PLAYLIST_ITEMS_UPDATE,
            PlannedAction::PlaylistItemDelete { .. } => endpoint::PLAYLIST_ITEMS_DELETE,
        }
    }

    pub fn cost(&self) -> i64 {
        unit_cost(self.endpoint())
    }

    pub fn to_params(&self) -> Value {
        match self {
            PlannedAction::PlaylistInsert { title, description, privacy } => {
                json!({"title": title, "description": description, "privacy": privacy})
            }
            PlannedAction::PlaylistUpdate {
                playlist_id,
                title,
                description,
                privacy,
                prev_title,
                prev_description,
                prev_privacy,
            } => json!({
                "playlist_id": playlist_id, "title": title, "description": description,
                "privacy": privacy, "prev_title": prev_title,
                "prev_description": prev_description, "prev_privacy": prev_privacy,
            }),
            PlannedAction::PlaylistDelete { playlist_id } => json!({"playlist_id": playlist_id}),
            PlannedAction::PlaylistItemInsert { playlist_id, video_id, position } => {
                json!({"playlist_id": playlist_id, "video_id": video_id, "position": position})
            }
            PlannedAction::PlaylistItemUpdatePosition {
                playlist_item_id,
                playlist_id,
                video_id,
                position,
                prev_position,
            } => json!({
                "playlist_item_id": playlist_item_id, "playlist_id": playlist_id,
                "video_id": video_id, "position": position, "prev_position": prev_position,
            }),
            PlannedAction::PlaylistItemDelete {
                playlist_item_id,
                video_id,
                playlist_id,
                prev_position,
            } => json!({
                "playlist_item_id": playlist_item_id, "video_id": video_id,
                "playlist_id": playlist_id, "prev_position": prev_position,
            }),
        }
    }

    /// `None` for an action name this module does not plan (`verify_destination`, the `it_*`
    /// ones) or params missing a required key, so a corrupt row never panics the runner. A delete
    /// still waiting for its `playlist_item_id` (see the module doc) is `None` too.
    pub fn from_params(action: &str, params: &Value) -> Option<Self> {
        let s = |key: &str| params.get(key)?.as_str().map(str::to_string);
        let i = |key: &str| params.get(key)?.as_i64();
        use action_name::*;
        Some(match action {
            PLAYLIST_INSERT => PlannedAction::PlaylistInsert {
                title: s("title")?,
                description: s("description").unwrap_or_default(),
                privacy: s("privacy").unwrap_or_else(|| "private".into()),
            },
            PLAYLIST_UPDATE => PlannedAction::PlaylistUpdate {
                playlist_id: s("playlist_id")?,
                title: s("title")?,
                description: s("description").unwrap_or_default(),
                privacy: s("privacy")?,
                prev_title: s("prev_title")?,
                prev_description: s("prev_description").unwrap_or_default(),
                prev_privacy: s("prev_privacy")?,
            },
            PLAYLIST_DELETE => PlannedAction::PlaylistDelete { playlist_id: s("playlist_id")? },
            PLAYLIST_ITEM_INSERT => PlannedAction::PlaylistItemInsert {
                playlist_id: s("playlist_id")?,
                video_id: s("video_id")?,
                position: i("position"),
            },
            PLAYLIST_ITEM_UPDATE_POSITION => PlannedAction::PlaylistItemUpdatePosition {
                playlist_item_id: s("playlist_item_id")?,
                playlist_id: s("playlist_id")?,
                video_id: s("video_id")?,
                position: i("position")?,
                prev_position: i("prev_position")?,
            },
            PLAYLIST_ITEM_DELETE => PlannedAction::PlaylistItemDelete {
                playlist_item_id: s("playlist_item_id")?,
                video_id: s("video_id")?,
                playlist_id: s("playlist_id")?,
                prev_position: i("prev_position"),
            },
            _ => return None,
        })
    }

    /// The action that undoes this one. `api_result` is the item's own result once done.
    pub fn inverse(&self, api_result: Option<&Value>) -> Option<PlannedAction> {
        match self {
            PlannedAction::PlaylistInsert { .. } => {
                let playlist_id = api_result?.get("id")?.as_str()?.to_string();
                Some(PlannedAction::PlaylistDelete { playlist_id })
            }
            PlannedAction::PlaylistUpdate {
                playlist_id,
                title,
                description,
                privacy,
                prev_title,
                prev_description,
                prev_privacy,
            } => Some(PlannedAction::PlaylistUpdate {
                playlist_id: playlist_id.clone(),
                title: prev_title.clone(),
                description: prev_description.clone(),
                privacy: prev_privacy.clone(),
                prev_title: title.clone(),
                prev_description: description.clone(),
                prev_privacy: privacy.clone(),
            }),
            PlannedAction::PlaylistDelete { .. } => None,
            PlannedAction::PlaylistItemInsert { playlist_id, video_id, .. } => {
                let playlist_item_id = api_result?.get("playlist_item_id")?.as_str()?.to_string();
                Some(PlannedAction::PlaylistItemDelete {
                    playlist_item_id,
                    video_id: video_id.clone(),
                    playlist_id: playlist_id.clone(),
                    prev_position: None,
                })
            }
            PlannedAction::PlaylistItemUpdatePosition {
                playlist_item_id,
                playlist_id,
                video_id,
                position,
                prev_position,
            } => Some(PlannedAction::PlaylistItemUpdatePosition {
                playlist_item_id: playlist_item_id.clone(),
                playlist_id: playlist_id.clone(),
                video_id: video_id.clone(),
                position: *prev_position,
                prev_position: *position,
            }),
            PlannedAction::PlaylistItemDelete { video_id, playlist_id, prev_position, .. } => {
                Some(PlannedAction::PlaylistItemInsert {
                    playlist_id: playlist_id.clone(),
                    video_id: video_id.clone(),
                    position: *prev_position,
                })
            }
        }
    }

    /// [`Self::inverse`] in the self-describing `{"action", "params"}` shape `inverse_json` stores.
    pub fn inverse_json(&self, api_result: Option<&Value>) -> Option<Value> {
        self.inverse(api_result)
            .map(|inv| json!({"action": inv.action_name(), "params": inv.to_params()}))
    }
}

/// The quota an item costs, by action name (forecasts, the budget check, a revert's estimate).
/// The barrier's re-list is priced at 1, the InnerTube actions at 0: they spend no Data API quota.
pub fn cost_for_action_name(action: &str) -> i64 {
    use action_name::*;
    match action {
        PLAYLIST_INSERT => unit_cost(endpoint::PLAYLISTS_INSERT),
        PLAYLIST_UPDATE => unit_cost(endpoint::PLAYLISTS_UPDATE),
        PLAYLIST_DELETE => unit_cost(endpoint::PLAYLISTS_DELETE),
        PLAYLIST_ITEM_INSERT => unit_cost(endpoint::PLAYLIST_ITEMS_INSERT),
        PLAYLIST_ITEM_UPDATE_POSITION => unit_cost(endpoint::PLAYLIST_ITEMS_UPDATE),
        PLAYLIST_ITEM_DELETE => unit_cost(endpoint::PLAYLIST_ITEMS_DELETE),
        VERIFY_DESTINATION => 1,
        _ => 0,
    }
}

/// The Data API id of an app playlist id, when the Data API can write it: `VLPL…` or `PL…`.
/// Liked Music, On Repeat, albums and playlists on this machine have none.
pub fn ytdata_playlist_id(browse_id: &str) -> Option<String> {
    let id = browse_id.strip_prefix("VL").unwrap_or(browse_id);
    (id.starts_with("PL") && id.len() > 2).then(|| id.to_string())
}

/// What a copy or move via the Data API is expected to cost: 50 u per insert, as much again for a
/// move's deletes, plus a page per 50 rows for the barrier and the deletes' resolution.
pub fn estimate_transfer_units(rows: usize, moving: bool) -> i64 {
    let rows = rows as i64;
    let write = unit_cost(endpoint::PLAYLIST_ITEMS_INSERT);
    let pages = (rows + 49) / 50;
    if moving {
        rows * write * 2 + 2 * pages.max(1)
    } else {
        rows * write
    }
}

/// What removing `rows` via the Data API is expected to cost: one delete each, plus the listing
/// that resolves them (a page per 50, at least one).
pub fn estimate_remove_units(rows: usize, playlist_len: usize) -> i64 {
    let rows = rows as i64;
    rows * unit_cost(endpoint::PLAYLIST_ITEMS_DELETE) + ((playlist_len as i64 + 49) / 50).max(1)
}

/// For each picked row, which copy of its video it is in `current` (0-based, playlist order), so
/// a Data API delete can find the same row. Found by handle; a row whose handle is gone (or that
/// carries none) falls back to the next copy not already picked.
pub fn occurrences(current: &[SongItem], picked: &[SongItem]) -> Vec<i64> {
    let mut index_of: HashMap<&str, i64> = HashMap::new();
    let mut seen: HashMap<&str, i64> = HashMap::new();
    for row in current {
        let n = seen.entry(row.video_id.as_str()).or_default();
        if let Some(h) = handle(row) {
            index_of.insert(h, *n);
        }
        *n += 1;
    }
    let mut taken: HashMap<&str, HashSet<i64>> = HashMap::new();
    picked
        .iter()
        .map(|row| {
            let used = taken.entry(row.video_id.as_str()).or_default();
            let k = match handle(row).and_then(|h| index_of.get(h)) {
                Some(&k) if !used.contains(&k) => k,
                _ => (0..).find(|k| !used.contains(k)).unwrap_or(0),
            };
            used.insert(k);
            k
        })
        .collect()
}

/// A Data API delete still to be resolved (see the module doc).
pub fn unresolved_delete(playlist_id: &str, video_id: &str, occurrence: i64) -> NewJobItem {
    NewJobItem {
        phase: 1,
        action: action_name::PLAYLIST_ITEM_DELETE.to_string(),
        params: json!({"playlist_id": playlist_id, "video_id": video_id, "occurrence": occurrence}),
    }
}

/// Whether a delete item still needs its `playlist_item_id`.
pub fn is_unresolved_delete(item: &JobItem) -> bool {
    item.action == action_name::PLAYLIST_ITEM_DELETE
        && item.params.get("playlist_item_id").and_then(Value::as_str).is_none()
}

/// Assigns listed rows (`(video_id, playlist_item_id, position)`, playlist order) to unresolved
/// deletes by occurrence. Answers `(item id, params with the id and prev_position)` for every item
/// it could match; one it could not (the row is gone) is left out.
pub fn resolve_occurrences(
    listed: &[(String, String, i64)],
    deletes: &[JobItem],
) -> Vec<(i64, Value)> {
    let mut by_video: HashMap<&str, Vec<(&str, i64)>> = HashMap::new();
    for (video, item_id, position) in listed {
        by_video.entry(video.as_str()).or_default().push((item_id.as_str(), *position));
    }
    deletes
        .iter()
        .filter_map(|item| {
            let video = item.params.get("video_id")?.as_str()?;
            let k = item.params.get("occurrence").and_then(Value::as_i64).unwrap_or(0).max(0);
            let (item_id, position) = *by_video.get(video)?.get(k as usize)?;
            let mut params = item.params.clone();
            params["playlist_item_id"] = json!(item_id);
            params["prev_position"] = json!(position);
            Some((item.id, params))
        })
        .collect()
}

/// The Data API move barrier: every done copy whose video is now in the destination gets a delete
/// of its source row (phase 3); any done copy not found sends itself (and the barrier) back.
/// Copies that were skipped or failed never get a delete, so a move never loses a track. Songs the
/// destination already had, moved under "consolidate", are deleted from the source as well
/// (`params.consolidate: [{video_id, occurrence}]`).
pub fn ytdata_move_barrier(job: &Job, phase1: &[JobItem], present: &HashSet<String>) -> Barrier {
    let source = job.params.get("source_playlist_id").and_then(Value::as_str).unwrap_or_default();
    let mut missing = Vec::new();
    let mut deletes = Vec::new();
    for copy in phase1.iter().filter(|i| i.status == JobItemStatus::Done) {
        let Some(video) = copy.params.get("video_id").and_then(Value::as_str) else { continue };
        if !present.contains(video) {
            missing.push(copy.id);
            continue;
        }
        let k = copy.params.get("occurrence").and_then(Value::as_i64).unwrap_or(0);
        deletes.push(phase3(unresolved_delete(source, video, k)));
    }
    if !missing.is_empty() {
        return Barrier::Missing(missing);
    }
    if let Some(extra) = job.params.get("consolidate").and_then(Value::as_array) {
        for row in extra {
            let Some(video) = row.get("video_id").and_then(Value::as_str) else { continue };
            let k = row.get("occurrence").and_then(Value::as_i64).unwrap_or(0);
            deletes.push(phase3(unresolved_delete(source, video, k)));
        }
    }
    Barrier::Proceed(deletes)
}

fn phase3(mut item: NewJobItem) -> NewJobItem {
    item.phase = 3;
    item
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jobs::{JobKind, JobStatus};

    fn song(v: &str, set: Option<&str>) -> SongItem {
        SongItem { video_id: v.into(), set_video_id: set.map(Into::into), ..Default::default() }
    }

    fn at() -> chrono::DateTime<chrono::Utc> {
        chrono::DateTime::parse_from_rfc3339("2026-07-15T12:00:00Z").unwrap().into()
    }

    fn item(id: i64, phase: i64, action: &str, params: Value, status: JobItemStatus) -> JobItem {
        JobItem {
            id,
            job_id: 1,
            seq: id,
            phase,
            action: action.into(),
            params,
            status,
            api_result: None,
            inverse: None,
            attempts: 1,
            last_error: None,
            updated_at: at(),
        }
    }

    fn job(params: Value) -> Job {
        Job {
            id: 1,
            account_id: Some("UC1".into()),
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

    #[test]
    fn every_write_costs_50_and_params_round_trip() {
        let actions = [
            PlannedAction::PlaylistInsert {
                title: "Mix".into(),
                description: "d".into(),
                privacy: "private".into(),
            },
            PlannedAction::PlaylistItemInsert {
                playlist_id: "PL1".into(),
                video_id: "v1".into(),
                position: Some(3),
            },
            PlannedAction::PlaylistItemDelete {
                playlist_item_id: "pi1".into(),
                video_id: "v1".into(),
                playlist_id: "PL1".into(),
                prev_position: Some(2),
            },
        ];
        for action in actions {
            assert_eq!(action.cost(), 50);
            assert_eq!(cost_for_action_name(action.action_name()), 50);
            let back = PlannedAction::from_params(action.action_name(), &action.to_params());
            assert_eq!(back.as_ref(), Some(&action));
        }
        assert_eq!(cost_for_action_name(action_name::VERIFY_DESTINATION), 1);
        assert_eq!(cost_for_action_name(action_name::IT_ADD_ROWS), 0);
        assert!(PlannedAction::from_params("verify_destination", &json!({})).is_none());
    }

    #[test]
    fn inverses_need_the_api_result_for_creates_and_round_trip() {
        let insert = PlannedAction::PlaylistItemInsert {
            playlist_id: "PL1".into(),
            video_id: "v1".into(),
            position: None,
        };
        assert_eq!(insert.inverse(None), None);
        let inv = insert.inverse_json(Some(&json!({"playlist_item_id": "new1"}))).unwrap();
        assert_eq!(inv["action"], json!(action_name::PLAYLIST_ITEM_DELETE));
        let back = PlannedAction::from_params(inv["action"].as_str().unwrap(), &inv["params"]);
        assert!(matches!(back, Some(PlannedAction::PlaylistItemDelete { .. })));
        let delete = PlannedAction::PlaylistItemDelete {
            playlist_item_id: "pi".into(),
            video_id: "v".into(),
            playlist_id: "PL".into(),
            prev_position: Some(4),
        };
        assert_eq!(
            delete.inverse(None),
            Some(PlannedAction::PlaylistItemInsert {
                playlist_id: "PL".into(),
                video_id: "v".into(),
                position: Some(4)
            })
        );
        assert_eq!(PlannedAction::PlaylistDelete { playlist_id: "PL".into() }.inverse(None), None);
    }

    #[test]
    fn only_real_playlists_have_a_data_api_id() {
        assert_eq!(ytdata_playlist_id("VLPLabc").as_deref(), Some("PLabc"));
        assert_eq!(ytdata_playlist_id("PLabc").as_deref(), Some("PLabc"));
        assert_eq!(ytdata_playlist_id("VLLM"), None);
        assert_eq!(ytdata_playlist_id("LOCALPLAYLIST:3"), None);
        assert_eq!(ytdata_playlist_id("VLPL"), None);
    }

    #[test]
    fn occurrences_follow_handles_and_fall_back_to_the_next_free_copy() {
        let current = [
            song("a", Some("1")),
            song("b", Some("2")),
            song("a", Some("3")),
            song("a", Some("4")),
        ];
        // The second and third copies of `a`, by handle.
        assert_eq!(occurrences(&current, &[song("a", Some("3")), song("a", Some("4"))]), [1, 2]);
        // No handles: the first free copies, in order.
        assert_eq!(occurrences(&current, &[song("a", None), song("a", None)]), [0, 1]);
        // A vanished handle doesn't reuse a copy already picked.
        assert_eq!(occurrences(&current, &[song("a", Some("1")), song("a", Some("9"))]), [0, 1]);
    }

    #[test]
    fn resolution_picks_the_kth_copy_and_records_its_position() {
        let listed = vec![
            ("a".to_string(), "pa0".to_string(), 0),
            ("b".to_string(), "pb0".to_string(), 1),
            ("a".to_string(), "pa1".to_string(), 2),
        ];
        let delete = |id, v: &str, k: i64| {
            let params = json!({"playlist_id": "PL", "video_id": v, "occurrence": k});
            item(id, 1, "playlist_item_delete", params, JobItemStatus::Pending)
        };
        let deletes = [delete(7, "a", 1), delete(8, "c", 0)];
        assert!(is_unresolved_delete(&deletes[0]));
        let got = resolve_occurrences(&listed, &deletes);
        assert_eq!(got.len(), 1, "c is not in the playlist: left out");
        assert_eq!(got[0].0, 7);
        assert_eq!(got[0].1["playlist_item_id"], json!("pa1"));
        assert_eq!(got[0].1["prev_position"], json!(2));
        let resolved = item(7, 1, "playlist_item_delete", got[0].1.clone(), JobItemStatus::Pending);
        assert!(!is_unresolved_delete(&resolved));
        assert!(PlannedAction::from_params(&resolved.action, &resolved.params).is_some());
    }

    #[test]
    fn the_barrier_deletes_only_confirmed_copies_and_sends_missing_ones_back() {
        let j = job(json!({
            "source_playlist_id": "PLsrc",
            "consolidate": [{"video_id": "d", "occurrence": 0}],
        }));
        let insert = |id, v: &str, status| {
            let params = json!({"playlist_id": "PLdst", "video_id": v, "occurrence": 0});
            item(id, 1, "playlist_item_insert", params, status)
        };
        let phase1 = [
            insert(1, "a", JobItemStatus::Done),
            insert(2, "b", JobItemStatus::Done),
            insert(3, "c", JobItemStatus::Skipped),
        ];
        let present: HashSet<String> = ["a".to_string()].into();
        assert_eq!(ytdata_move_barrier(&j, &phase1, &present), Barrier::Missing(vec![2]));

        let present: HashSet<String> = ["a".to_string(), "b".to_string()].into();
        let Barrier::Proceed(deletes) = ytdata_move_barrier(&j, &phase1, &present) else {
            panic!("everything done is confirmed")
        };
        let videos: Vec<&str> =
            deletes.iter().map(|d| d.params["video_id"].as_str().unwrap()).collect();
        assert_eq!(videos, ["a", "b", "d"], "c was skipped: its source row stays");
        assert!(deletes.iter().all(|d| d.phase == 3 && d.params["playlist_id"] == json!("PLsrc")));
    }

    #[test]
    fn estimates_count_writes_and_pages() {
        assert_eq!(estimate_transfer_units(2, false), 100);
        assert_eq!(estimate_transfer_units(2, true), 200 + 2);
        assert_eq!(estimate_remove_units(3, 120), 150 + 3);
    }
}
