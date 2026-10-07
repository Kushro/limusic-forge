//! Split one playlist into several (PlaylistForge's `pf-core/src/split.rs`): one per artist, with the
//! artists that have only a few tracks pooled, or into parts by count or by size, after an optional
//! re-order (a seeded shuffle among them). Pure: it answers which rows go where, and the UI names
//! the parts (and lets you rename them) before anything is created.

use std::collections::HashMap;

use innertube::SongItem;
use serde::{Deserialize, Serialize};

use super::dedup::{duration_secs, primary_artist};

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(tag = "by", rename_all = "snake_case")]
pub enum SplitBy {
    /// One playlist per artist; artists with fewer than `min` tracks share one more ("Various").
    Artist { min: usize },
    /// This many parts, as even as they come.
    Count { parts: usize },
    /// Parts of at most this many tracks.
    Size { max: usize },
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(tag = "order", rename_all = "snake_case")]
pub enum Order {
    Playlist,
    Title,
    Artist,
    Duration,
    Shuffle { seed: u64 },
}

/// One playlist to be: rows (indices into the list that was split), and for a split by artist, the
/// artist's name as it reads on its first track. `None` is the pooled part, or a numbered one.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Part {
    pub artist: Option<String>,
    pub rows: Vec<usize>,
}

pub fn split(rows: &[SongItem], by: SplitBy, order: Order) -> Vec<Part> {
    match by {
        SplitBy::Artist { min } => by_artist(rows, min.max(1)),
        SplitBy::Count { parts } => {
            let parts = parts.clamp(1, rows.len().max(1));
            let size = rows.len().div_ceil(parts).max(1);
            chunks(ordered(rows, order), size)
        }
        SplitBy::Size { max } => chunks(ordered(rows, order), max.max(1)),
    }
}

fn chunks(order: Vec<usize>, size: usize) -> Vec<Part> {
    order.chunks(size).map(|c| Part { artist: None, rows: c.to_vec() }).collect()
}

fn by_artist(rows: &[SongItem], min: usize) -> Vec<Part> {
    let mut groups: Vec<(String, Vec<usize>)> = Vec::new();
    let mut index: HashMap<String, usize> = HashMap::new();
    for (i, r) in rows.iter().enumerate() {
        let key = primary_artist(&r.artists);
        match index.get(&key) {
            Some(&g) => groups[g].1.push(i),
            None => {
                index.insert(key, groups.len());
                let shown = first_name(&r.artists);
                groups.push((shown, vec![i]));
            }
        }
    }
    let (big, small): (Vec<_>, Vec<_>) = groups.into_iter().partition(|(_, g)| g.len() >= min);
    let mut parts: Vec<Part> =
        big.into_iter().map(|(name, rows)| Part { artist: Some(name), rows }).collect();
    // Biggest first; equal sizes in the order the artists first appear.
    parts.sort_by(|a, b| b.rows.len().cmp(&a.rows.len()));
    let mut pooled: Vec<usize> = small.into_iter().flat_map(|(_, g)| g).collect();
    if !pooled.is_empty() {
        pooled.sort_unstable();
        parts.push(Part { artist: None, rows: pooled });
    }
    parts
}

/// The artist line's first name, as written.
fn first_name(artists: &str) -> String {
    [", ", " & ", " • ", " x ", " feat. ", " ft. ", " and "]
        .iter()
        .fold(artists, |s, sep| s.split(sep).next().unwrap_or(s))
        .trim()
        .to_owned()
}

/// Row indices in the asked order. Stable, so equal keys keep playlist order.
pub fn ordered(rows: &[SongItem], order: Order) -> Vec<usize> {
    let mut idx: Vec<usize> = (0..rows.len()).collect();
    match order {
        Order::Playlist => {}
        Order::Title => idx.sort_by_cached_key(|&i| rows[i].title.to_lowercase()),
        Order::Artist => idx.sort_by_cached_key(|&i| primary_artist(&rows[i].artists)),
        Order::Duration => {
            idx.sort_by_key(|&i| duration_secs(rows[i].duration.as_deref()).unwrap_or(i64::MAX))
        }
        Order::Shuffle { seed } => shuffle(&mut idx, seed),
    }
    idx
}

/// Fisher–Yates over a SplitMix64 stream: a seed names the shuffle, so the preview and the
/// playlists that come out of it agree.
fn shuffle(v: &mut [usize], seed: u64) {
    let mut s = seed;
    let mut next = || {
        s = s.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = s;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    };
    for i in (1..v.len()).rev() {
        let j = (next() % (i as u64 + 1)) as usize;
        v.swap(i, j);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(title: &str, artists: &str, dur: &str) -> SongItem {
        SongItem {
            video_id: title.into(),
            title: title.into(),
            artists: artists.into(),
            duration: Some(dur.into()),
            ..Default::default()
        }
    }

    #[test]
    fn by_artist_pools_the_small_ones() {
        let rows = vec![
            s("a1", "Alpha", "1:00"),
            s("b1", "Beta & Friend", "1:00"),
            s("a2", "Alpha", "1:00"),
            s("c1", "Gamma", "1:00"),
            s("b2", "Beta", "1:00"),
            s("a3", "Alpha, Someone", "1:00"),
        ];
        let p = split(&rows, SplitBy::Artist { min: 2 }, Order::Playlist);
        assert_eq!(p[0], Part { artist: Some("Alpha".into()), rows: vec![0, 2, 5] });
        assert_eq!(p[1], Part { artist: Some("Beta".into()), rows: vec![1, 4] });
        assert_eq!(p[2], Part { artist: None, rows: vec![3] }, "Gamma has one track: pooled");
    }

    #[test]
    fn by_count_and_size() {
        let rows: Vec<SongItem> = (0..10).map(|i| s(&format!("t{i}"), "X", "1:00")).collect();
        let sizes = |p: Vec<Part>| p.iter().map(|p| p.rows.len()).collect::<Vec<_>>();
        assert_eq!(sizes(split(&rows, SplitBy::Count { parts: 3 }, Order::Playlist)), [4, 4, 2]);
        assert_eq!(sizes(split(&rows, SplitBy::Size { max: 4 }, Order::Playlist)), [4, 4, 2]);
        assert_eq!(sizes(split(&rows, SplitBy::Count { parts: 50 }, Order::Playlist)).len(), 10);
        assert_eq!(sizes(split(&[], SplitBy::Count { parts: 3 }, Order::Playlist)).len(), 0);
    }

    #[test]
    fn orders_are_stable_and_a_seed_repeats() {
        let rows = vec![s("b", "Z", "2:00"), s("a", "Y", "3:00"), s("c", "X", "1:00")];
        assert_eq!(ordered(&rows, Order::Title), [1, 0, 2]);
        assert_eq!(ordered(&rows, Order::Artist), [2, 1, 0]);
        assert_eq!(ordered(&rows, Order::Duration), [2, 0, 1]);
        let many: Vec<SongItem> = (0..30).map(|i| s(&i.to_string(), "X", "1:00")).collect();
        let a = ordered(&many, Order::Shuffle { seed: 7 });
        assert_eq!(a, ordered(&many, Order::Shuffle { seed: 7 }));
        assert_ne!(a, ordered(&many, Order::Shuffle { seed: 8 }));
        let mut sorted = a.clone();
        sorted.sort_unstable();
        assert_eq!(sorted, (0..30).collect::<Vec<_>>());
    }
}
