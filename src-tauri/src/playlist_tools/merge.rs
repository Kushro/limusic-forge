//! Merge several playlists into one. PlaylistForge has no merge; this is the obvious companion to
//! its split. Pure: the lists come in already read, the merged list goes out, and the caller writes
//! it into a new playlist or appends it to an existing one.

use std::collections::HashSet;

use innertube::SongItem;
use serde::Deserialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Interleave {
    /// One list after another, in the order given.
    Concat,
    /// One track from each list in turn, round robin, until every list runs out.
    RoundRobin,
}

/// The merged list. `skip` holds videos left out from the start (what an existing target already
/// has); with `dedupe`, a video appears once, at its first place in the merged order.
pub fn merge(
    lists: &[Vec<SongItem>],
    how: Interleave,
    dedupe: bool,
    skip: &HashSet<String>,
) -> Vec<SongItem> {
    let order: Vec<&SongItem> = match how {
        Interleave::Concat => lists.iter().flatten().collect(),
        Interleave::RoundRobin => {
            let longest = lists.iter().map(Vec::len).max().unwrap_or(0);
            (0..longest).flat_map(|i| lists.iter().filter_map(move |l| l.get(i))).collect()
        }
    };
    let mut seen: HashSet<&str> = HashSet::new();
    order
        .into_iter()
        .filter(|s| !skip.contains(&s.video_id))
        .filter(|s| !dedupe || seen.insert(s.video_id.as_str()))
        .map(|s| SongItem { set_video_id: None, ..s.clone() })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn l(ids: &str) -> Vec<SongItem> {
        ids.chars()
            .map(|c| SongItem {
                video_id: c.to_string(),
                set_video_id: Some(format!("s{c}")),
                ..Default::default()
            })
            .collect()
    }
    fn ids(v: &[SongItem]) -> String {
        v.iter().map(|s| s.video_id.as_str()).collect()
    }

    #[test]
    fn concat_and_round_robin() {
        let lists = [l("abc"), l("xy")];
        let none = HashSet::new();
        assert_eq!(ids(&merge(&lists, Interleave::Concat, false, &none)), "abcxy");
        assert_eq!(ids(&merge(&lists, Interleave::RoundRobin, false, &none)), "axbyc");
    }

    #[test]
    fn dedupe_keeps_the_first_and_skip_leaves_out_what_the_target_has() {
        let lists = [l("abc"), l("bcd")];
        let none = HashSet::new();
        assert_eq!(ids(&merge(&lists, Interleave::Concat, true, &none)), "abcd");
        assert_eq!(ids(&merge(&lists, Interleave::Concat, false, &none)), "abcbcd");
        let has: HashSet<String> = ["a".to_string()].into();
        let m = merge(&lists, Interleave::Concat, true, &has);
        assert_eq!(ids(&m), "bcd");
        assert!(m.iter().all(|s| s.set_video_id.is_none()), "handles belong to the old rows");
    }
}
