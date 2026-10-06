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

use super::journal::{self, Named, OpRecord, Restore, Step, Summary};
use super::rows::{self, handle};
use crate::state::AppState;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    Copy,
    Move,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Duplicates {
    Skip,
    Allow,
    Consolidate,
}

/// What a transfer did, for the toast.
#[derive(Debug, Serialize)]
pub struct Transferred {
    pub added: usize,
    pub duplicates: usize,
    pub refused: usize,
    pub removed: usize,
    pub op: Option<OpRecord>,
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
            songs.into_iter().partition(|s| {
                index.get(&s.video_id).is_some_and(|ids| ids.iter().any(|id| id == target))
            })
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

    #[test]
    fn a_row_without_a_handle_is_never_removed() {
        let mut r = row("a", "1");
        r.song.set_video_id = None;
        assert!(rows_to_remove(&[r], &[song("a")], &[], Duplicates::Allow).is_empty());
    }
}
