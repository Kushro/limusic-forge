//! Every song across your playlists, once each, with the playlists that hold it (PlaylistForge's
//! global Videos screen), and its two actions: keep a song in one playlist only (its "exclusive"
//! move), or take it out of all of them. Read from the membership index, so the list opens with no
//! network at all; the edits read each playlist they touch fresh, since stored handles go stale.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use innertube::SongItem;
use serde::Serialize;

use super::journal::{self, Named, OpRecord, Restore, Step, Summary};
use super::rows::{self, handle};
use crate::commands::LIKED_MUSIC_ID;
use crate::import::before_playlist_write;
use crate::state::{is_local_playlist, AppState};

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Everywhere {
    pub song: SongItem,
    /// The playlists holding it, in the order the index listed them.
    pub playlists: Vec<String>,
    /// When it was first seen in any of them (epoch seconds), the earliest one known; null when
    /// every one of them held it from before tracking began. An unknown date never wins over a
    /// known one, so a song that sat in one playlist since before tracking began shows the date it
    /// was later added to another playlist.
    pub first_seen: Option<i64>,
}

/// The earlier of two dates, either of which may be unknown. Unknown (`None`) means "held from
/// before tracking began", not "older than everything": it is skipped rather than treated as the
/// earliest, so the result is the earliest *known* date (see [`Everywhere::first_seen`]).
fn earliest(a: Option<i64>, b: Option<i64>) -> Option<i64> {
    match (a, b) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    }
}

/// Group `(videoId, playlist id, song_json, first_seen)` rows by song. The ones in the most
/// playlists first, then by title. Liked Music is left out: a like is not a playlist you filed a
/// song in, and its date doesn't count either.
pub fn group(rows: Vec<(String, String, String, Option<i64>)>) -> Vec<Everywhere> {
    let mut order: Vec<String> = Vec::new();
    let mut by: HashMap<String, Everywhere> = HashMap::new();
    for (video_id, playlist, json, seen) in rows {
        if playlist == LIKED_MUSIC_ID {
            continue;
        }
        match by.get_mut(&video_id) {
            Some(e) => {
                if !e.playlists.contains(&playlist) {
                    e.playlists.push(playlist);
                }
                e.first_seen = earliest(e.first_seen, seen);
            }
            None => {
                let Ok(song) = serde_json::from_str::<SongItem>(&json) else { continue };
                order.push(video_id.clone());
                let e = Everywhere { song, playlists: vec![playlist], first_seen: seen };
                by.insert(video_id, e);
            }
        }
    }
    let mut out: Vec<Everywhere> = order.into_iter().filter_map(|v| by.remove(&v)).collect();
    out.sort_by(|a, b| {
        b.playlists
            .len()
            .cmp(&a.playlists.len())
            .then_with(|| a.song.title.to_lowercase().cmp(&b.song.title.to_lowercase()))
    });
    out
}

/// The rows of `list` whose handle is in `gone`, each with the next row that stays: where an undo
/// puts it back.
fn restore_rows(list: &[SongItem], gone: &HashSet<String>) -> Vec<Restore> {
    let mut next: Option<String> = None;
    let mut out = Vec::new();
    for row in list.iter().rev() {
        let Some(h) = handle(row) else { continue };
        if gone.contains(h) {
            out.push(Restore { song: row.clone(), before: next.clone() });
        } else {
            next = Some(h.to_owned());
        }
    }
    out.reverse();
    out
}

#[derive(Debug, Serialize)]
pub struct Kept {
    pub added: usize,
    pub removed: usize,
    /// Playlists that refused the removal (not yours any more, say), by id.
    pub failed: Vec<String>,
    pub op: Option<OpRecord>,
}

/// Keep `songs` only in `target` (adding them there if missing), or with no target, take them out
/// of every playlist of yours. `titles` names the playlists for the history.
pub async fn keep_only_in(
    state: &Arc<AppState>,
    songs: Vec<SongItem>,
    target: Option<Named>,
    titles: HashMap<String, String>,
) -> Result<Kept, String> {
    let none = &|| false;
    let index = state.db.playlist_memberships();
    let mut undo: Vec<Step> = Vec::new();
    let mut touched: Vec<String> = Vec::new();
    let mut summary: Vec<Named> = Vec::new();
    let mut first = true;

    // 1. Into the target, whatever it lacks. A song that doesn't make it in is never taken out of
    //    anywhere else: this must not lose a track.
    let mut keep: HashSet<String> = songs.iter().map(|s| s.video_id.clone()).collect();
    let mut added = 0;
    if let Some(t) = &target {
        let missing: Vec<SongItem> = songs
            .iter()
            .filter(|s| !index.get(&s.video_id).is_some_and(|ids| ids.contains(&t.id)))
            .map(|s| SongItem { set_video_id: None, ..s.clone() })
            .collect();
        if !missing.is_empty() {
            let a = rows::add_rows(state, &t.id, &missing, false, none).await?;
            first = false;
            for r in &a.refused {
                keep.remove(&r.video_id);
            }
            added = a.rows.len();
            let rows: Vec<SongItem> = a.rows.into_iter().filter(|r| handle(r).is_some()).collect();
            if !rows.is_empty() {
                undo.push(Step::Remove { playlist_id: t.id.clone(), rows });
            }
        }
        touched.push(t.id.clone());
        summary.push(t.clone());
    }

    // 2. Out of every other playlist holding them.
    let mut sources: Vec<String> = Vec::new();
    for s in &songs {
        for p in index.get(&s.video_id).into_iter().flatten() {
            let mine = target.as_ref().is_some_and(|t| &t.id == p);
            if !mine && p != LIKED_MUSIC_ID && keep.contains(&s.video_id) && !sources.contains(p) {
                sources.push(p.clone());
            }
        }
    }
    let mut removed = 0;
    let mut failed = Vec::new();
    for pid in sources {
        if !first && !is_local_playlist(&pid) {
            before_playlist_write(state, none).await?;
        }
        let list = match rows::read_all(state, &pid).await {
            Ok(l) => l,
            Err(_) => {
                failed.push(pid);
                continue;
            }
        };
        let gone: HashSet<String> = list
            .iter()
            .filter(|r| keep.contains(&r.video_id))
            .filter_map(handle)
            .map(str::to_owned)
            .collect();
        if gone.is_empty() {
            continue;
        }
        let back = restore_rows(&list, &gone);
        let out: Vec<SongItem> = back.iter().map(|r| r.song.clone()).collect();
        first = false;
        match rows::remove_rows(state, &pid, &out, none).await {
            Ok(()) => {
                removed += out.len();
                undo.insert(0, journal::restore_step(&pid, &back));
                touched.push(pid.clone());
                summary
                    .push(Named { title: titles.get(&pid).cloned().unwrap_or_default(), id: pid });
            }
            Err(e) if e.starts_with("cooldown:") => return Err(e),
            Err(_) => failed.push(pid),
        }
    }

    journal::announce(state, &touched);
    let kind = if target.is_some() { "move" } else { "remove" };
    let op = if undo.is_empty() {
        None
    } else {
        journal::record(state, kind, &Summary { playlists: summary, count: songs.len() }, &undo)
    };
    Ok(Kept { added, removed, failed, op })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn json(v: &str, title: &str) -> String {
        serde_json::to_string(&SongItem {
            video_id: v.into(),
            title: title.into(),
            ..Default::default()
        })
        .unwrap()
    }

    #[test]
    fn grouped_by_song_most_playlists_first_liked_left_out() {
        let rows = vec![
            ("a".into(), "VL1".into(), json("a", "Zeta"), None),
            ("b".into(), "VL1".into(), json("b", "Alpha"), Some(50)),
            ("a".into(), "VL2".into(), json("a", "Zeta"), Some(300)),
            ("a".into(), "VLLM".into(), json("a", "Zeta"), Some(10)),
            ("c".into(), "VL2".into(), json("c", "beta"), None),
            ("x".into(), "VL2".into(), "not json".into(), Some(1)),
        ];
        let g = group(rows);
        let got: Vec<(&str, Vec<&str>, Option<i64>)> = g
            .iter()
            .map(|e| {
                (
                    e.song.video_id.as_str(),
                    e.playlists.iter().map(String::as_str).collect(),
                    e.first_seen,
                )
            })
            .collect();
        assert_eq!(
            got,
            [
                ("a", vec!["VL1", "VL2"], Some(300)),
                ("b", vec!["VL1"], Some(50)),
                ("c", vec!["VL2"], None),
            ]
        );
    }

    #[test]
    fn first_seen_is_the_earliest_known_date() {
        let rows = vec![
            ("a".into(), "VL1".into(), json("a", "A"), Some(500)),
            ("a".into(), "VL2".into(), json("a", "A"), None),
            ("a".into(), "VL3".into(), json("a", "A"), Some(200)),
            ("a".into(), "VL4".into(), json("a", "A"), Some(900)),
        ];
        let g = group(rows);
        assert_eq!(g.len(), 1);
        assert_eq!(g[0].first_seen, Some(200), "unknown dates don't win over known ones");
        assert_eq!(earliest(None, None), None);
        assert_eq!(earliest(Some(3), None), Some(3));
        assert_eq!(earliest(None, Some(4)), Some(4));
    }

    #[test]
    fn restore_rows_anchor_on_the_next_row_that_stays() {
        let row = |v: &str| SongItem {
            video_id: v.into(),
            set_video_id: Some(format!("s{v}")),
            ..Default::default()
        };
        let list = vec![row("a"), row("b"), row("c"), row("d")];
        let gone: HashSet<String> = ["sb".to_string(), "sd".to_string()].into();
        let got: Vec<(String, Option<String>)> =
            restore_rows(&list, &gone).into_iter().map(|r| (r.song.video_id, r.before)).collect();
        assert_eq!(got, [("b".into(), Some("sc".into())), ("d".into(), None)]);
    }
}
