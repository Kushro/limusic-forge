//! The undo journal. Every playlist tool records what it did and the steps that take it back; the
//! UI shows the newest 20 (`Db::PLAYLIST_OPS_KEPT`) and offers an undo on each, and a toast offers
//! the newest one for a few seconds after it lands.
//!
//! Undo is best effort by nature: the playlist may have changed since. Each step is written to
//! cope with that the way the rest of the tools do: a row that is gone is skipped, an anchor that
//! is gone sends its row to the end.

use std::collections::HashSet;
use std::sync::Arc;

use innertube::SongItem;
use serde::{Deserialize, Serialize};

use super::lis::Move;
use super::rows::{self, handle};
use crate::db::{now_secs, PlaylistOpRow};
use crate::state::{is_local_playlist, AppState};

/// One step of an undo, run in the order stored.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "step", rename_all = "snake_case")]
pub enum Step {
    /// Put taken-out rows back: add the songs, then move each one before the row it used to
    /// precede (`rows::anchors`).
    Restore { playlist_id: String, rows: Vec<Restore> },
    /// Take out rows the operation added, by handle.
    Remove { playlist_id: String, rows: Vec<SongItem> },
    /// Put the playlist back in this order (row handles).
    Reorder { playlist_id: String, order: Vec<String> },
    /// Delete a playlist the operation created.
    DeletePlaylist { playlist_id: String },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Restore {
    pub song: SongItem,
    /// The handle of the row it went back in front of; `None` for the end.
    pub before: Option<String>,
}

/// What the history shows for an operation. The UI words it from `kind` and these numbers.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Summary {
    /// The playlists it touched, source first.
    pub playlists: Vec<Named>,
    /// How many tracks it moved, removed, added or reordered.
    pub count: usize,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Named {
    pub id: String,
    pub title: String,
}

/// A journal row as the UI gets it.
#[derive(Debug, Clone, Serialize)]
pub struct OpRecord {
    pub id: i64,
    pub kind: String,
    pub summary: Summary,
    pub created_at: i64,
    pub undone: bool,
    /// Not undone yet, and either local only or run under the account signed in now.
    pub undoable: bool,
}

fn active_account(state: &AppState) -> Option<String> {
    state.db.get_setting("active_account")
}

fn step_is_local(s: &Step) -> bool {
    match s {
        Step::Restore { playlist_id, .. }
        | Step::Remove { playlist_id, .. }
        | Step::Reorder { playlist_id, .. }
        | Step::DeletePlaylist { playlist_id } => is_local_playlist(playlist_id),
    }
}

/// Whether every playlist the steps touch lives on this machine (then any account may undo it).
fn local_only(steps: &[Step]) -> bool {
    steps.iter().all(step_is_local)
}

fn to_record(state: &AppState, row: PlaylistOpRow) -> OpRecord {
    let steps: Vec<Step> = serde_json::from_str(&row.inverse_json).unwrap_or_default();
    let mine = row.account.is_none() || row.account == active_account(state) || local_only(&steps);
    OpRecord {
        id: row.id,
        kind: row.kind,
        summary: serde_json::from_str(&row.summary_json).unwrap_or_default(),
        created_at: row.created_at,
        undone: row.undone_at.is_some(),
        undoable: row.undone_at.is_none() && mine && !steps.is_empty(),
    }
}

/// Record an operation. A journal that can't be written costs the undo, not the edit, so this
/// logs and answers `None` rather than failing an operation that already happened.
pub fn record(state: &AppState, kind: &str, summary: &Summary, undo: &[Step]) -> Option<OpRecord> {
    let account = if local_only(undo) { None } else { active_account(state) };
    let summary_json = serde_json::to_string(summary).ok()?;
    let inverse_json = serde_json::to_string(undo).ok()?;
    match state.db.record_playlist_op(
        kind,
        account.as_deref(),
        &summary_json,
        &inverse_json,
        now_secs(),
    ) {
        Ok(id) => state.db.playlist_op(id).map(|r| to_record(state, r)),
        Err(e) => {
            tracing::warn!(error = %e, "playlist tools: could not journal an operation");
            None
        }
    }
}

pub fn history(state: &AppState) -> Vec<OpRecord> {
    state.db.playlist_ops().into_iter().map(|r| to_record(state, r)).collect()
}

/// One undo at a time: two clicks racing must not both replay the same inverse.
static UNDO: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Undo one operation. Answers its record, now marked undone.
pub async fn undo(state: &Arc<AppState>, id: i64) -> Result<OpRecord, String> {
    let _guard = UNDO.lock().await;
    let row = state.db.playlist_op(id).ok_or("That change is no longer in the history.")?;
    let record = to_record(state, row.clone());
    if record.undone {
        return Err("That change was already undone.".into());
    }
    if !record.undoable {
        return Err("Switch to the account that made that change to undo it.".into());
    }
    let steps: Vec<Step> = serde_json::from_str(&row.inverse_json).map_err(|e| e.to_string())?;
    for (i, step) in steps.iter().enumerate() {
        // An undo of a split is a delete per playlist it made: on the account those wait their
        // turn like any other run of writes. The first goes straight out.
        if i > 0 && !step_is_local(step) {
            crate::import::before_playlist_write(state, &|| false).await?;
        }
        run(state, step).await?;
    }
    announce(state, &record.summary.playlists.iter().map(|p| p.id.clone()).collect::<Vec<_>>());
    state.db.mark_playlist_op_undone(id, now_secs()).map_err(|e| e.to_string())?;
    state.db.playlist_op(id).map(|r| to_record(state, r)).ok_or_else(|| "gone".into())
}

async fn run(state: &Arc<AppState>, step: &Step) -> Result<(), String> {
    let none = &|| false;
    match step {
        Step::Remove { playlist_id, rows: gone } => {
            // Only rows still there: one removed by hand meanwhile would fail the whole request.
            let now: HashSet<String> = rows::read_all(state, playlist_id)
                .await?
                .iter()
                .filter_map(handle)
                .map(str::to_owned)
                .collect();
            let gone: Vec<SongItem> = gone
                .iter()
                .filter(|r| handle(r).is_some_and(|h| now.contains(h)))
                .cloned()
                .collect();
            rows::remove_rows(state, playlist_id, &gone, none).await
        }
        Step::Reorder { playlist_id, order } => {
            let (before, moved) = rows::reorder(state, playlist_id, order, none).await?;
            // The order put back is yours too: no `moved` alerts for it on the next sync.
            if moved > 0 {
                super::monitor::after_reorder(&state.db, playlist_id, &before, order, now_secs());
            }
            Ok(())
        }
        Step::Restore { playlist_id, rows: back } => {
            let songs: Vec<SongItem> = back.iter().map(|r| r.song.clone()).collect();
            // Duplicates allowed: a dedupe's undo puts back a second copy on purpose.
            let added = rows::add_rows(state, playlist_id, &songs, true, none).await?;
            let present: HashSet<String> = rows::read_all(state, playlist_id)
                .await?
                .iter()
                .filter_map(handle)
                .map(str::to_owned)
                .collect();
            let moves = restore_moves(back, &added.rows, &present);
            rows::move_rows(state, playlist_id, &moves, none).await
        }
        Step::DeletePlaylist { playlist_id } => {
            crate::commands::delete_playlist_inner(state, playlist_id).await
        }
    }
}

/// The moves that put re-added rows back where they were. `added` are the new rows, in the order
/// they were added (the order of `back`); each is matched to its `Restore` by videoId. A row whose
/// anchor is gone stays where the add put it, at the end.
fn restore_moves(back: &[Restore], added: &[SongItem], present: &HashSet<String>) -> Vec<Move> {
    let mut pool: Vec<&SongItem> = added.iter().collect();
    let mut moves = Vec::new();
    for r in back {
        let Some(i) = pool.iter().position(|a| a.video_id == r.song.video_id) else { continue };
        let row = pool.remove(i);
        let (Some(h), Some(before)) = (handle(row), r.before.as_ref()) else { continue };
        if present.contains(before) {
            moves.push(Move { row: h.to_owned(), before: Some(before.clone()) });
        }
    }
    moves
}

/// The undo of a reorder: the order it replaced.
pub fn reorder_undo(playlist_id: &str, before: Vec<String>) -> Vec<Step> {
    vec![Step::Reorder { playlist_id: playlist_id.to_owned(), order: before }]
}

/// The undo of taking rows out when the caller already knows where each one sat (the UI does: it
/// has the list on screen). Old handles are dropped, since the rows that come back get new ones.
pub fn restore_step(playlist_id: &str, rows: &[Restore]) -> Step {
    let rows = rows
        .iter()
        .map(|r| Restore { song: SongItem { set_video_id: None, ..r.song.clone() }, ..r.clone() })
        .collect();
    Step::Restore { playlist_id: playlist_id.to_owned(), rows }
}

/// Tell every open view these playlists changed, so a page showing one re-reads it. Sent after
/// each operation and each undo, wherever it was started from.
pub fn announce(state: &AppState, playlist_ids: &[String]) {
    use tauri::Emitter;
    let _ = state.app.emit("playlists-edited", playlist_ids);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(v: &str, set: &str) -> SongItem {
        SongItem { video_id: v.into(), set_video_id: Some(set.into()), ..Default::default() }
    }

    #[test]
    fn restore_moves_match_new_rows_and_skip_lost_anchors() {
        let back = vec![
            Restore { song: row("b", ""), before: Some("4".into()) },
            Restore { song: row("c", ""), before: Some("9".into()) },
            Restore { song: row("e", ""), before: None },
        ];
        let added = vec![row("b", "n1"), row("c", "n2"), row("e", "n3")];
        let present: HashSet<String> = ["1", "4", "n1", "n2", "n3"].map(String::from).into();
        let moves = restore_moves(&back, &added, &present);
        // c's anchor 9 is gone and e had none: both stay where the add put them, at the end.
        assert_eq!(moves, vec![Move { row: "n1".into(), before: Some("4".into()) }]);
    }

    #[test]
    fn steps_round_trip_as_json() {
        let steps = vec![
            Step::Reorder { playlist_id: "PL".into(), order: vec!["1".into()] },
            Step::DeletePlaylist { playlist_id: "LOCALPLAYLIST:3".into() },
        ];
        let json = serde_json::to_string(&steps).unwrap();
        assert!(json.contains(r#""step":"reorder""#));
        assert_eq!(serde_json::from_str::<Vec<Step>>(&json).unwrap(), steps);
        assert!(!local_only(&steps));
        assert!(local_only(&steps[1..]));
    }
}
