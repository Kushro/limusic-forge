//! Duplicate tracks in one playlist. Ported from PlaylistForge's `pf-core/src/dedup.rs`, with one
//! change for music: two rows are only the same song when they are by the same artist. A YouTube
//! video title often carries the artist; a YouTube Music row splits it out, and "Intro" by one
//! artist is not "Intro" by another.
//!
//! Three levels, combined through union-find so a chain of matches is one cluster:
//!
//! 1. **Exact**: the same videoId twice.
//! 2. **Title**: the same normalized title and primary artist. Normalizing strips decoration
//!    brackets ("(Official Video)", "[HD]", "(Lyrics)") but never version markers ("(Live)",
//!    "(Remix)", "(Acoustic)", "(Sped Up)"): those are different recordings, and merging them is the
//!    false positive that matters most.
//! 3. **Similar**: Jaro-Winkler ≥ 0.92 over the normalized titles, same primary artist, and lengths
//!    within 3 s. The length gate is what keeps "Song" and "Song (Live)" apart where the title
//!    alone would not; a row with no length never matches at this level. On top of PlaylistForge's
//!    rule, the titles must have the same words and differ only by typos inside words of four
//!    letters or more, with any number exactly equal: "Part 1" and "Part 2", or "Song A" and
//!    "Song B", score above 0.92 and usually share a length, and are never the same song.
//!
//! Title levels only compare rows within one artist, so a 5000-row playlist is a few thousand
//! small comparisons, not twelve million.

use std::collections::HashMap;

use innertube::SongItem;
use serde::{Deserialize, Serialize};
use serde_json::json;
use unicode_normalization::UnicodeNormalization;

use super::journal::{Named, Restore, Summary};
use crate::jobs::engine::{Engine, QueueTarget};
use crate::jobs::planner::{self, action_name, INNERTUBE_ROWS_PER_ITEM};
use crate::jobs::{JobKind, NewJob, NewJobItem, ENGINE_KEY};

/// The job that takes the duplicates (or any picked rows) out of one playlist, queued
/// (`remove_tracks` with the queue on): InnerTube removes by handle, 100 rows a request, and its
/// undo puts each row back where it was; the Data API deletes the `occurrences[i]`-th copy of each
/// row's video (`planner::occurrences`). `kind` is the journal's (`dedupe` or `remove`). `None`
/// when there is nothing to remove, or the Data API can't address the playlist.
pub fn removal_job(
    playlist: &Named,
    rows: &[Restore],
    kind: &str,
    occurrences: &[i64],
    q: &QueueTarget,
) -> Option<NewJob> {
    if rows.is_empty() {
        return None;
    }
    let summary = Summary { playlists: vec![playlist.clone()], count: rows.len() };
    let params = json!({
        ENGINE_KEY: q.engine.as_str(),
        "account": q.account,
        "op_kind": kind,
        "summary": summary,
        "target_id": playlist.id,
    });
    let (account_id, items, est) = match q.engine {
        Engine::Innertube => {
            let items: Vec<NewJobItem> = rows
                .chunks(INNERTUBE_ROWS_PER_ITEM)
                .map(|chunk| NewJobItem {
                    phase: 1,
                    action: action_name::IT_REMOVE_ROWS.to_string(),
                    params: json!({"playlist_id": playlist.id, "rows": chunk}),
                })
                .collect();
            (None, items, 0)
        }
        Engine::Ytdata => {
            let id = planner::ytdata_playlist_id(&playlist.id)?;
            let items: Vec<NewJobItem> = rows
                .iter()
                .enumerate()
                .map(|(i, r)| {
                    let k = occurrences.get(i).copied().unwrap_or(0);
                    planner::unresolved_delete(&id, &r.song.video_id, k)
                })
                .collect();
            (q.channel_id.clone(), items, planner::estimate_remove_units(rows.len(), rows.len()))
        }
    };
    Some(NewJob {
        account_id,
        kind: JobKind::RemoveItems,
        params,
        priority: q.priority,
        total_phases: 1,
        est_units_total: est,
        items,
    })
}

#[derive(Debug, Clone, Copy, Deserialize)]
pub struct Options {
    pub exact: bool,
    pub title: bool,
    pub similar: bool,
}

const SIMILAR_THRESHOLD: f64 = 0.92;
const LENGTH_TOLERANCE_S: i64 = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Reason {
    Exact,
    Title,
    Similar,
}

/// Rows (indices into the list that was searched) that are copies of each other, in list order.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Cluster {
    pub rows: Vec<usize>,
    /// Every level that linked two of its rows, strongest first.
    pub reasons: Vec<Reason>,
    /// The row a keep rule picked, offered as the default.
    pub keep: usize,
}

/// Which copy stays.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Keep {
    /// The one highest in the playlist.
    First,
    /// The one lowest in the playlist (the most recently added, in an unsorted playlist).
    Last,
    /// The audio track over a music video, then the highest.
    PreferSong,
}

const NOISE_CONTAINS: [&str; 8] = [
    "official",
    "lyric",
    "video oficial",
    "audio oficial",
    "letra",
    "visualizer",
    "video clip",
    "remaster",
];
const NOISE_EXACT: [&str; 9] =
    ["hd", "4k", "8k", "hq", "audio", "audio only", "explicit", "clean", "mv"];
const BRACKETS: [(char, char); 4] = [('(', ')'), ('[', ']'), ('{', '}'), ('【', '】')];

fn is_noise(inner: &str) -> bool {
    let inner = inner.trim().to_lowercase();
    inner.is_empty()
        || NOISE_CONTAINS.iter().any(|n| inner.contains(n))
        || NOISE_EXACT.contains(&inner.as_str())
}

/// NFKC, lowercase, whitespace collapsed, decoration brackets gone, version markers kept.
pub fn normalize_title(raw: &str) -> String {
    let chars: Vec<char> = raw.chars().collect();
    let mut out = String::with_capacity(raw.len());
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if let Some(&(_, close)) = BRACKETS.iter().find(|(open, _)| *open == c) {
            if let Some(len) = chars[i + 1..].iter().position(|&ch| ch == close) {
                let inner: String = chars[i + 1..i + 1 + len].iter().collect();
                if !is_noise(&inner) {
                    out.push(c);
                    out.push_str(&inner);
                    out.push(close);
                }
                i += len + 2;
                continue;
            }
        }
        out.push(c);
        i += 1;
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ").nfkc().collect::<String>().to_lowercase()
}

/// The first name on the artist line, normalized: "Future & Metro Boomin" → "future".
pub fn primary_artist(artists: &str) -> String {
    let first = [", ", " & ", " • ", " x ", " feat. ", " ft. ", " and "]
        .iter()
        .fold(artists, |s, sep| s.split(sep).next().unwrap_or(s));
    first.trim().nfkc().collect::<String>().to_lowercase()
}

/// "3:45" or "1:02:03" in seconds.
pub fn duration_secs(d: Option<&str>) -> Option<i64> {
    let parts: Vec<i64> = d?.split(':').map(|p| p.trim().parse().ok()).collect::<Option<_>>()?;
    (!parts.is_empty() && parts.len() <= 3).then(|| parts.iter().fold(0, |acc, p| acc * 60 + p))
}

/// Two normalized titles that differ only by a typo (see the module doc).
fn similar_titles(a: &str, b: &str) -> bool {
    if strsim::jaro_winkler(a, b) < SIMILAR_THRESHOLD {
        return false;
    }
    let (ta, tb): (Vec<&str>, Vec<&str>) = (a.split(' ').collect(), b.split(' ').collect());
    ta.len() == tb.len()
        && ta.iter().zip(&tb).all(|(x, y)| {
            x == y
                || (x.chars().count() >= 4
                    && y.chars().count() >= 4
                    && !x.chars().any(|c| c.is_ascii_digit())
                    && !y.chars().any(|c| c.is_ascii_digit())
                    && strsim::jaro_winkler(x, y) >= 0.85)
        })
}

struct UnionFind(Vec<usize>);

impl UnionFind {
    fn find(&mut self, x: usize) -> usize {
        if self.0[x] != x {
            let root = self.find(self.0[x]);
            self.0[x] = root;
        }
        self.0[x]
    }
    fn union(&mut self, a: usize, b: usize) {
        let (ra, rb) = (self.find(a), self.find(b));
        if ra != rb {
            // The lower index is the root, so a cluster's root is its first row.
            let (lo, hi) = (ra.min(rb), ra.max(rb));
            self.0[hi] = lo;
        }
    }
}

pub fn find_clusters(rows: &[SongItem], o: &Options, keep: Keep) -> Vec<Cluster> {
    let n = rows.len();
    let mut uf = UnionFind((0..n).collect());
    let mut links: Vec<(usize, usize, Reason)> = Vec::new();
    let mut link = |uf: &mut UnionFind, a: usize, b: usize, r: Reason| {
        uf.union(a, b);
        links.push((a, b, r));
    };

    if o.exact {
        let mut first: HashMap<&str, usize> = HashMap::new();
        for (i, r) in rows.iter().enumerate() {
            match first.get(r.video_id.as_str()) {
                Some(&j) => link(&mut uf, j, i, Reason::Exact),
                None => {
                    first.insert(&r.video_id, i);
                }
            }
        }
    }

    if o.title || o.similar {
        let titles: Vec<String> = rows.iter().map(|r| normalize_title(&r.title)).collect();
        let lengths: Vec<Option<i64>> =
            rows.iter().map(|r| duration_secs(r.duration.as_deref())).collect();
        let mut by_artist: HashMap<String, Vec<usize>> = HashMap::new();
        for (i, r) in rows.iter().enumerate() {
            by_artist.entry(primary_artist(&r.artists)).or_default().push(i);
        }
        for members in by_artist.values() {
            for (x, &i) in members.iter().enumerate() {
                for &j in &members[x + 1..] {
                    let (a, b) = (&titles[i], &titles[j]);
                    if a.is_empty() || b.is_empty() || rows[i].video_id == rows[j].video_id {
                        continue;
                    }
                    if o.title && a == b {
                        link(&mut uf, i, j, Reason::Title);
                    } else if o.similar {
                        let close = matches!((lengths[i], lengths[j]),
                            (Some(x), Some(y)) if (x - y).abs() <= LENGTH_TOLERANCE_S);
                        if close && similar_titles(a, b) {
                            link(&mut uf, i, j, Reason::Similar);
                        }
                    }
                }
            }
        }
    }

    let mut groups: HashMap<usize, Vec<usize>> = HashMap::new();
    for i in 0..n {
        let root = uf.find(i);
        groups.entry(root).or_default().push(i);
    }
    let mut reasons: HashMap<usize, Vec<Reason>> = HashMap::new();
    for (a, _, r) in links {
        let root = uf.find(a);
        let v = reasons.entry(root).or_default();
        if !v.contains(&r) {
            v.push(r);
        }
    }
    let mut out: Vec<Cluster> = groups
        .into_iter()
        .filter(|(_, m)| m.len() > 1)
        .map(|(root, mut m)| {
            m.sort_unstable();
            let mut r = reasons.remove(&root).unwrap_or_default();
            r.sort();
            let keep = keeper(rows, &m, keep);
            Cluster { rows: m, reasons: r, keep }
        })
        .collect();
    out.sort_by_key(|c| c.rows[0]);
    out
}

fn keeper(rows: &[SongItem], members: &[usize], keep: Keep) -> usize {
    match keep {
        Keep::First => members[0],
        Keep::Last => *members.last().unwrap(),
        Keep::PreferSong => *members.iter().find(|&&i| !rows[i].is_video).unwrap_or(&members[0]),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &str, title: &str, artists: &str, dur: &str) -> SongItem {
        SongItem {
            video_id: v.into(),
            title: title.into(),
            artists: artists.into(),
            duration: Some(dur.into()),
            ..Default::default()
        }
    }
    const ALL: Options = Options { exact: true, title: true, similar: true };

    #[test]
    fn titles_lose_decoration_but_keep_versions() {
        assert_eq!(normalize_title("Song  (Official Video) [HD]"), "song");
        assert_eq!(normalize_title("Song (Live)"), "song (live)");
        assert_eq!(normalize_title("Song【MV】"), "song");
        assert_eq!(normalize_title("Ｓｏｎｇ"), "song", "NFKC folds full-width");
        assert_eq!(primary_artist("Future & Metro Boomin"), "future");
        assert_eq!(primary_artist("Daft Punk, Pharrell Williams"), "daft punk");
        assert_eq!(duration_secs(Some("3:45")), Some(225));
        assert_eq!(duration_secs(Some("1:02:03")), Some(3723));
        assert_eq!(duration_secs(Some("Cast of EPIC: The Musical")), None);
    }

    #[test]
    fn exact_title_and_similar_levels_cluster_transitively() {
        let rows = vec![
            s("a", "Blinding Lights", "The Weeknd", "3:20"),
            s("b", "Other", "Someone", "2:00"),
            s("a", "Blinding Lights", "The Weeknd", "3:20"), // exact copy of 0
            s("c", "Blinding Lights (Official Video)", "The Weeknd", "4:22"), // title of 0
            s("d", "Blinding Light", "The Weeknd", "4:21"),  // similar to 3
        ];
        let c = find_clusters(&rows, &ALL, Keep::First);
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].rows, vec![0, 2, 3, 4]);
        assert_eq!(c[0].reasons, vec![Reason::Exact, Reason::Title, Reason::Similar]);
        assert_eq!(c[0].keep, 0);
        assert_eq!(find_clusters(&rows, &ALL, Keep::Last)[0].keep, 4);
    }

    #[test]
    fn versions_and_other_artists_are_not_duplicates() {
        let rows = vec![
            s("a", "Song", "Band", "3:00"),
            s("b", "Song (Live)", "Band", "4:10"),
            s("c", "Song (Remix)", "Band", "3:02"), // close in length, but a version marker
            s("d", "Song", "Another Band", "3:00"),
            s("e", "Intro", "Band", "1:00"),
            s("f", "Intro", "Band", ""), // same title, no length: the title level still links it
        ];
        let c = find_clusters(&rows, &ALL, Keep::First);
        assert_eq!(c.len(), 1, "{c:?}");
        assert_eq!(c[0].rows, vec![4, 5]);
    }

    #[test]
    fn a_letter_or_a_number_apart_is_another_song() {
        let rows = vec![
            s("a", "Song A", "Band", "3:00"),
            s("b", "Song C", "Band", "3:00"),
            s("c", "Part 1", "Band", "3:00"),
            s("d", "Part 2", "Band", "3:00"),
            s("e", "Despacito", "Band", "3:48"),
            s("f", "Despasito", "Band", "3:49"), // a typo: still the same song
        ];
        let c = find_clusters(&rows, &ALL, Keep::First);
        assert_eq!(c.len(), 1, "{c:?}");
        assert_eq!(c[0].rows, vec![4, 5]);
    }

    #[test]
    fn levels_switch_off() {
        let rows =
            vec![s("a", "X", "A", "1:00"), s("a", "X", "A", "1:00"), s("b", "X", "A", "1:00")];
        let only_exact = Options { exact: true, title: false, similar: false };
        assert_eq!(find_clusters(&rows, &only_exact, Keep::First)[0].rows, vec![0, 1]);
        let none = Options { exact: false, title: false, similar: false };
        assert!(find_clusters(&rows, &none, Keep::First).is_empty());
    }

    #[test]
    fn a_queued_removal_is_batched_on_innertube_and_per_copy_on_the_data_api() {
        let named = Named { id: "VLPLx".into(), title: "X".into() };
        let rows: Vec<Restore> = (0..150)
            .map(|i| Restore {
                song: SongItem {
                    video_id: "dup".into(),
                    set_video_id: Some(i.to_string()),
                    ..Default::default()
                },
                before: None,
            })
            .collect();
        let q = |engine: Engine| QueueTarget {
            engine,
            channel_id: (engine == Engine::Ytdata).then(|| "UC1".to_string()),
            account: Some("ga1".into()),
            priority: 2,
        };
        let it = removal_job(&named, &rows, "dedupe", &[], &q(Engine::Innertube)).unwrap();
        assert_eq!(it.items.len(), 2, "100 rows a request");
        assert_eq!(it.items[0].action, action_name::IT_REMOVE_ROWS);
        assert_eq!(it.params["op_kind"], serde_json::json!("dedupe"));
        assert_eq!(it.kind, JobKind::RemoveItems);

        let occ: Vec<i64> = (0..150).collect();
        let yt = removal_job(&named, &rows, "dedupe", &occ, &q(Engine::Ytdata)).unwrap();
        assert_eq!(yt.items.len(), 150);
        assert_eq!(yt.items[149].params["occurrence"], serde_json::json!(149));
        assert_eq!(yt.items[0].params["playlist_id"], serde_json::json!("PLx"));
        assert_eq!(yt.account_id.as_deref(), Some("UC1"));
        assert!(removal_job(&named, &[], "remove", &[], &q(Engine::Innertube)).is_none());
        let liked = Named { id: "VLLM".into(), title: "Liked".into() };
        assert!(removal_job(&liked, &rows, "remove", &occ, &q(Engine::Ytdata)).is_none());
    }

    #[test]
    fn prefer_song_keeps_the_audio_track() {
        let mut video = s("v", "Song", "Band", "3:30");
        video.is_video = true;
        let rows = vec![video, s("a", "Song", "Band", "3:01")];
        assert_eq!(find_clusters(&rows, &ALL, Keep::PreferSong)[0].keep, 1);
    }
}
