//! What changed in a playlist between two syncs of the membership index (PlaylistForge's alerts,
//! from the same comparison it made between snapshots). The index already re-reads every playlist
//! you own every six hours; keeping each track's metadata with it is what lets this say *which*
//! song went, and not only that one did.
//!
//! - **gone**: in the playlist last time, not now. Removals made in this app update the index as
//!   they happen, so what is left is a removal made elsewhere, or YouTube dropping a track.
//! - **unavailable**: still listed, but greyed out now and not last time (taken down, private,
//!   blocked where you are).
//!
//! A first sync, or one cut short at the page cap, says nothing: there is nothing to compare, or
//! the missing tail would read as hundreds of removals.

use std::collections::{HashMap, HashSet};

use innertube::SongItem;

#[derive(Debug, Clone, PartialEq)]
pub struct Change {
    pub video_id: String,
    pub kind: &'static str,
    /// The song as last known (gone) or as now listed (unavailable), for the alert to show.
    pub song: Option<SongItem>,
}

/// Compare the index's last copy of a playlist (`(videoId, song_json)`) with a complete fresh read.
pub fn diff(before: &[(String, Option<String>)], now: &[SongItem]) -> Vec<Change> {
    if before.is_empty() {
        return Vec::new();
    }
    let parse =
        |j: &Option<String>| j.as_deref().and_then(|j| serde_json::from_str::<SongItem>(j).ok());
    let was: HashMap<&str, Option<SongItem>> =
        before.iter().map(|(v, j)| (v.as_str(), parse(j))).collect();
    let present: HashSet<&str> = now.iter().map(|s| s.video_id.as_str()).collect();
    let mut out: Vec<Change> = before
        .iter()
        .filter(|(v, _)| !present.contains(v.as_str()))
        .map(|(v, j)| Change { video_id: v.clone(), kind: "removed", song: parse(j) })
        .collect();
    let mut seen = HashSet::new();
    for s in now.iter().filter(|s| s.unavailable) {
        // Newly greyed out only: a track that was unavailable last time already said so, and one
        // added since was added unavailable (nothing changed under you).
        let newly = matches!(was.get(s.video_id.as_str()), Some(Some(prev)) if !prev.unavailable)
            || matches!(was.get(s.video_id.as_str()), Some(None));
        if newly && seen.insert(s.video_id.as_str()) {
            out.push(Change {
                video_id: s.video_id.clone(),
                kind: "unavailable",
                song: Some(s.clone()),
            });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn song(v: &str, unavailable: bool) -> SongItem {
        SongItem { video_id: v.into(), title: v.into(), unavailable, ..Default::default() }
    }
    fn stored(v: &str, unavailable: bool) -> (String, Option<String>) {
        (v.into(), Some(serde_json::to_string(&song(v, unavailable)).unwrap()))
    }

    #[test]
    fn a_first_sync_says_nothing() {
        assert!(diff(&[], &[song("a", true)]).is_empty());
    }

    #[test]
    fn gone_and_newly_unavailable() {
        let before =
            vec![stored("a", false), stored("b", false), stored("c", true), ("d".into(), None)];
        let now = vec![song("b", true), song("c", true), song("d", true), song("e", true)];
        let got: Vec<(String, &str, bool)> = diff(&before, &now)
            .into_iter()
            .map(|c| (c.video_id, c.kind, c.song.is_some()))
            .collect();
        assert_eq!(
            got,
            [
                ("a".to_string(), "removed", true),
                ("b".to_string(), "unavailable", true), // greyed out since
                ("d".to_string(), "unavailable", true), // no metadata last time: say it
            ]
        );
        // c was already unavailable; e was added after the last sync.
    }
}
