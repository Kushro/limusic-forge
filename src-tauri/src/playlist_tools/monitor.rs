//! What changed in a playlist between two syncs (PlaylistForge's alerts, from the same comparison
//! it made between snapshots). Every complete read of a playlist you own is compared with its
//! newest snapshot, stored as a new snapshot when it differs, and each change is filed as an
//! alert:
//!
//! - **added**: a row that was not there last time.
//! - **removed**: a row there last time, not now.
//! - **moved**: a row still there whose place changed relative to the others. Only the fewest
//!   rows that explain the new order count (the ones outside a longest increasing subsequence),
//!   so one track dragged to the top is one move, not a shift of everything below it.
//! - **unavailable**: still listed, but greyed out now and not last time (taken down, private,
//!   blocked where you are).
//! - **restored**: greyed out last time, playable again now.
//!
//! A row is the same row across reads by its `setVideoId` (YouTube's id for one entry of a
//! playlist), or by `videoId` plus which occurrence of it this is when there is none, so the same
//! song twice is two rows.
//!
//! A first read says nothing: there is nothing to compare. A snapshot that is empty is not "no
//! snapshot", though: a playlist emptied and filled again is compared like any other. Neither
//! does a read cut short at the page cap say anything, where the missing tail would read as
//! hundreds of removals, nor a read of nothing after one that held rows, unless the playlist's
//! header says it is empty ([`read_complete`]). A playlist with no snapshot yet (synced before
//! snapshots existed) is compared with the membership index instead, which has no order, so it
//! cannot say what moved.
//!
//! A reorder made in this app takes a snapshot in its new order as it lands
//! ([`after_reorder`]), so the next sync does not report your own moves back to you.

use std::collections::{HashMap, HashSet};

use innertube::SongItem;
use serde_json::{json, Value};

use crate::commands::playlist_row;
use crate::db::{alert_dedupe_key, Db, MonitorRun, NewAlert, PlaylistSync, Privacy, SnapItem};
use crate::playlist_tools::lis::{self, lis_indices};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Kind {
    Added,
    Removed,
    Moved,
    Unavailable,
    Restored,
}

impl Kind {
    /// The kind as `playlist_alert` stores it (and the UI's `AlertKind`).
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Added => "added",
            Kind::Removed => "removed",
            Kind::Moved => "moved",
            Kind::Unavailable => "unavailable",
            Kind::Restored => "restored",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Change {
    pub video_id: String,
    pub kind: Kind,
    /// The song as last known (removed) or as now listed (everything else), for the alert.
    pub song: Option<SongItem>,
    /// Where the row was (0-based) in the snapshot, where that is known.
    pub from: Option<usize>,
    /// Where it is now (0-based).
    pub to: Option<usize>,
}

/// Each row's identity: `s:<setVideoId>`, or `v:<videoId>#<occurrence>` without one.
fn identities<'a>(rows: impl Iterator<Item = (&'a str, Option<&'a str>)>) -> Vec<String> {
    let mut seen: HashMap<&str, usize> = HashMap::new();
    let mut out = Vec::new();
    for (v, s) in rows {
        let key = match s.filter(|s| !s.is_empty()) {
            Some(s) => format!("s:{s}"),
            None => {
                let n = seen.entry(v).or_insert(0);
                *n += 1;
                format!("v:{v}#{}", *n - 1)
            }
        };
        out.push(key);
    }
    out
}

/// A snapshot row as a song, for an alert about a row that is no longer there to read.
fn song_of(item: &SnapItem) -> SongItem {
    SongItem {
        video_id: item.v.clone(),
        set_video_id: item.s.clone(),
        title: item.t.clone(),
        artists: item.a.clone(),
        duration: item.d.clone(),
        unavailable: item.u,
        thumbnail: item.th.clone(),
        ..Default::default()
    }
}

/// Whether two reads name their rows with the same handles. InnerTube's `setVideoId` and the Data
/// API's playlist-item id are two id spaces for the same row, so a snapshot taken by one reader
/// shares no handle with a read by the other. With handles on both sides and none in common, the
/// rows are matched by `videoId` and occurrence instead, so switching readers is not every row
/// removed and added again. (A playlist emptied and refilled with the same songs between two reads
/// of one reader then reads as unchanged, which is what it is to the listener.)
fn same_handles(before: &[SnapItem], now: &[SongItem]) -> bool {
    let was: HashSet<&str> =
        before.iter().filter_map(|i| i.s.as_deref()).filter(|h| !h.is_empty()).collect();
    let mut now_handles =
        now.iter().filter_map(|s| s.set_video_id.as_deref()).filter(|h| !h.is_empty()).peekable();
    let disjoint = !was.is_empty() && now_handles.peek().is_some();
    !disjoint || now_handles.any(|h| was.contains(h))
}

/// Compare a playlist's newest snapshot with a complete fresh read of it. An empty snapshot is
/// still a snapshot: everything read now was added since (the caller skips a playlist with none).
pub fn diff(before: &[SnapItem], now: &[SongItem]) -> Vec<Change> {
    let keep = same_handles(before, now);
    let was_keys =
        identities(before.iter().map(|i| (i.v.as_str(), i.s.as_deref().filter(|_| keep))));
    let now_keys =
        identities(now.iter().map(|s| (&*s.video_id, s.set_video_id.as_deref().filter(|_| keep))));
    let was_at: HashMap<&str, usize> =
        was_keys.iter().enumerate().map(|(i, k)| (k.as_str(), i)).collect();
    let still: HashSet<&str> = now_keys.iter().map(String::as_str).collect();

    let mut out: Vec<Change> = before
        .iter()
        .enumerate()
        .filter(|(i, _)| !still.contains(was_keys[*i].as_str()))
        .map(|(i, item)| Change {
            video_id: item.v.clone(),
            kind: Kind::Removed,
            song: Some(song_of(item)),
            from: Some(i),
            to: None,
        })
        .collect();

    // The rows in both reads, in their new order: (now index, snapshot index).
    let mut kept: Vec<(usize, usize)> = Vec::new();
    for (j, song) in now.iter().enumerate() {
        let Some(&i) = was_at.get(now_keys[j].as_str()) else {
            out.push(Change {
                video_id: song.video_id.clone(),
                kind: Kind::Added,
                song: Some(song.clone()),
                from: None,
                to: Some(j),
            });
            continue;
        };
        kept.push((j, i));
        let kind = match (before[i].u, song.unavailable) {
            (false, true) => Kind::Unavailable,
            (true, false) => Kind::Restored,
            _ => continue,
        };
        out.push(Change {
            video_id: song.video_id.clone(),
            kind,
            song: Some(song.clone()),
            from: Some(i),
            to: Some(j),
        });
    }

    // Rows whose old positions still increase in the new order kept their place relative to each
    // other; the longest such run stays put and every other kept row moved.
    let old_order: Vec<usize> = kept.iter().map(|&(_, i)| i).collect();
    let stayed: HashSet<usize> = lis_indices(&old_order).into_iter().collect();
    for (k, &(j, i)) in kept.iter().enumerate() {
        if !stayed.contains(&k) {
            out.push(Change {
                video_id: now[j].video_id.clone(),
                kind: Kind::Moved,
                song: Some(now[j].clone()),
                from: Some(i),
                to: Some(j),
            });
        }
    }
    out
}

/// The fallback for a playlist with no snapshot yet: compare the membership index's copy of it
/// (`(videoId, song_json)`, one row per track, no order) with a complete fresh read. Nothing can
/// read as moved, and a track listed twice is one track.
pub fn diff_index(before: &[(String, Option<String>)], now: &[SongItem]) -> Vec<Change> {
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
        .map(|(v, j)| Change {
            video_id: v.clone(),
            kind: Kind::Removed,
            song: parse(j),
            from: None,
            to: None,
        })
        .collect();
    let mut seen = HashSet::new();
    for (j, s) in now.iter().enumerate() {
        if !seen.insert(s.video_id.as_str()) {
            continue;
        }
        let kind = match was.get(s.video_id.as_str()) {
            None => Kind::Added,
            Some(Some(prev)) if !prev.unavailable && s.unavailable => Kind::Unavailable,
            Some(Some(prev)) if prev.unavailable && !s.unavailable => Kind::Restored,
            // No metadata last time: greyed out now is all there is to go on, so say it.
            Some(None) if s.unavailable => Kind::Unavailable,
            Some(_) => continue,
        };
        out.push(Change {
            video_id: s.video_id.clone(),
            kind,
            song: Some(s.clone()),
            from: None,
            to: Some(j),
        });
    }
    out
}

/// What one playlist's sync found.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Counts {
    pub added: u32,
    pub removed: u32,
    pub moved: u32,
    pub unavailable: u32,
    pub restored: u32,
    /// Alerts actually filed (a change made in this app files none, a repeat neither).
    pub alerts_new: u32,
}

impl Counts {
    fn of(changes: &[Change]) -> Self {
        let mut c = Counts::default();
        for change in changes {
            match change.kind {
                Kind::Added => c.added += 1,
                Kind::Removed => c.removed += 1,
                Kind::Moved => c.moved += 1,
                Kind::Unavailable => c.unavailable += 1,
                Kind::Restored => c.restored += 1,
            }
        }
        c
    }
}

/// Which of `changes` were made in this app, and so file no alert, by counting copies per video:
/// `before` is how many rows of each the snapshot (or, without one, the index) held, `held` the
/// videos the index holds now. Adds and removals made here patch the index as they happen, but the
/// index keeps a video once per playlist, so its count is 0 or 1 and it can only vouch for a
/// change that count shows:
///
/// - an add of a video the snapshot did not hold, once the index holds it (one add, not a second
///   copy: a duplicate added elsewhere still alerts);
/// - the removals of a video the index no longer holds at all.
///
/// A removal and an add of the same video cancel out first: a row swapped for another copy of
/// itself (taken out and put back elsewhere) leaves the song where it was, which is no news.
fn made_here(changes: &[Change], before: &HashMap<&str, usize>, held: &HashSet<&str>) -> Vec<bool> {
    let mut adds: HashMap<&str, Vec<usize>> = HashMap::new();
    let mut removes: HashMap<&str, Vec<usize>> = HashMap::new();
    for (i, c) in changes.iter().enumerate() {
        match c.kind {
            Kind::Added => adds.entry(c.video_id.as_str()).or_default().push(i),
            Kind::Removed => removes.entry(c.video_id.as_str()).or_default().push(i),
            _ => {}
        }
    }
    let mut quiet = vec![false; changes.len()];
    let none: Vec<usize> = Vec::new();
    for (v, added) in &adds {
        let removed = removes.get(v).unwrap_or(&none);
        let paired = added.len().min(removed.len());
        for &i in added[..paired].iter().chain(&removed[..paired]) {
            quiet[i] = true;
        }
        let was = before.get(v).copied().unwrap_or(0);
        if let Some(&i) = added.get(paired).filter(|_| was == 0 && held.contains(v)) {
            quiet[i] = true;
        }
    }
    for (v, removed) in &removes {
        let paired = adds.get(v).map_or(0, |a| a.len().min(removed.len()));
        if !held.contains(v) {
            for &i in &removed[paired..] {
                quiet[i] = true;
            }
        }
    }
    quiet
}

/// One fresh read of a playlist, to be written down.
pub struct Read<'a> {
    pub playlist_id: &'a str,
    pub title: Option<&'a str>,
    /// The account it was read as (`active_account`), filed with its snapshot.
    pub account_id: Option<&'a str>,
    /// Every row read, in playlist order, as YouTube sent them: `setVideoId` is what tells two
    /// copies of a song apart, and what an alert's "remove it" needs.
    pub songs: &'a [SongItem],
    /// Read to the end. Only then is a missing row a removal, and only then is the read a sync.
    pub complete: bool,
    /// Compare, keep history and file alerts. Off for Liked Music, which changes every time you
    /// like or unlike something anywhere: not news.
    pub watch: bool,
    pub at: i64,
    /// How many tracks the playlist's header says it holds, when it says
    /// ([`header_track_count`]). Only a header saying 0 lets an empty read follow one that was not.
    pub listed: Option<usize>,
    /// When each track was added to the playlist (`videoId` → epoch seconds), when the reader
    /// knows (the Data API). Written on the index rows; `None` keeps what is stored.
    pub added_at: Option<&'a HashMap<String, i64>>,
    /// The playlist's privacy (`public` / `unlisted` / `private`), when the reader knows.
    pub privacy: Option<&'a str>,
    /// Why a track is unavailable (`deleted`, `private`, `region_restricted`), by `videoId`, when
    /// the reader knows: kept in its `song_json` as `unavailable_reason`.
    pub reasons: Option<&'a HashMap<String, String>>,
}

/// A song as JSON, with the reason it is unavailable when one is known.
fn song_json(song: &SongItem, reasons: Option<&HashMap<String, String>>) -> Option<String> {
    let reason = reasons.and_then(|r| r.get(&song.video_id)).filter(|_| song.unavailable);
    let Some(reason) = reason else { return serde_json::to_string(song).ok() };
    let mut value = serde_json::to_value(song).ok()?;
    if let Some(obj) = value.as_object_mut() {
        obj.insert("unavailable_reason".into(), json!(reason));
    }
    Some(value.to_string())
}

/// The track count a playlist header's subtitle states ("12 songs • 45 minutes", "1 track"), when
/// it states one in a form this knows (digits, then an English unit). Anything else is `None`:
/// unknown, never 0.
pub fn header_track_count(subtitle: &str) -> Option<usize> {
    subtitle.split('•').find_map(|part| {
        let mut words = part.split_whitespace();
        let n = words.next()?.replace(',', "").parse().ok()?;
        let unit = words.next()?.to_lowercase();
        ["song", "track", "episode"].iter().any(|u| unit.starts_with(u)).then_some(n)
    })
}

/// Whether a read may be taken as the whole playlist. One cut short is not, and neither is a read
/// of nothing after a snapshot or an index that held rows, unless the header says the playlist is
/// empty: a degraded response or a parser that stopped matching reads exactly like that, and taken
/// at its word it would file every track as removed, then as added (dated now) on the next good
/// read. Such a read changes nothing and counts as not read to the end.
pub fn read_complete(db: &Db, read: &Read<'_>) -> bool {
    if !read.complete {
        return false;
    }
    if !read.songs.is_empty() || read.listed == Some(0) {
        return true;
    }
    let snapshot_held = db.latest_snapshot(read.playlist_id).is_some_and(|s| !s.items.is_empty());
    !snapshot_held && db.playlist_songs(read.playlist_id).is_empty()
}

/// Write one read down: alerts for what changed since the newest snapshot (or the index, with
/// none), a new snapshot when the content differs, the index rows, and the playlist's sync record.
/// A read [`read_complete`] doubts is written as one cut short.
pub fn record(db: &Db, read: &Read<'_>) -> Counts {
    let complete = read_complete(db, read);
    let indexed = db.playlist_songs(read.playlist_id);
    let mut counts = Counts::default();
    if complete && read.watch {
        let mut before: HashMap<&str, usize> = HashMap::new();
        let latest = db.latest_snapshot(read.playlist_id);
        let changes = match &latest {
            Some(snap) => {
                for item in &snap.items {
                    *before.entry(item.v.as_str()).or_default() += 1;
                }
                diff(&snap.items, read.songs)
            }
            None => {
                for (v, _) in &indexed {
                    *before.entry(v.as_str()).or_default() += 1;
                }
                diff_index(&indexed, read.songs)
            }
        };
        let items: Vec<SnapItem> = read.songs.iter().map(SnapItem::from_song).collect();
        let snapshot = db.put_snapshot_if_changed(
            read.playlist_id,
            read.account_id,
            read.title,
            read.at,
            &items,
        );
        counts = Counts::of(&changes);
        // A repeat of the same event later (a track removed, re-added, removed again) is a new
        // alert: the key carries the snapshot it was found in (D11).
        let scope = match snapshot {
            Some(id) => format!("snapshot:{id}"),
            None => format!("at:{}", read.at),
        };
        let in_index: HashMap<&str, Option<&str>> =
            indexed.iter().map(|(v, j)| (v.as_str(), j.as_deref())).collect();
        let held: HashSet<&str> = in_index.keys().copied().collect();
        let quiet = made_here(&changes, &before, &held);
        for (c, _) in changes.iter().zip(&quiet).filter(|(_, q)| !**q) {
            // A removed row's richest copy is the index's (album, artist links), when it has one.
            let indexed_json = match c.kind {
                Kind::Removed => in_index.get(c.video_id.as_str()).copied().flatten(),
                _ => None,
            };
            let json = indexed_json
                .map(str::to_owned)
                .or_else(|| c.song.as_ref().and_then(|s| song_json(s, read.reasons)));
            let kind = c.kind.as_str();
            let key = alert_dedupe_key(read.playlist_id, &c.video_id, kind, Some(&scope));
            let filed = db.insert_alert(&NewAlert {
                playlist_id: read.playlist_id,
                video_id: &c.video_id,
                kind,
                song_json: json.as_deref(),
                at: read.at,
                from_pos: c.from.map(|p| p as i64),
                to_pos: c.to.map(|p| p as i64),
                dedupe_key: &key,
            });
            if filed.is_some() {
                counts.alerts_new += 1;
            }
        }
    }
    // The index keeps the song, not the row: no setVideoId, rating or contributor.
    let rows: Vec<(String, String)> = read
        .songs
        .iter()
        .map(|s| {
            let json = song_json(&playlist_row(s.clone()), read.reasons).unwrap_or_default();
            (s.video_id.clone(), json)
        })
        .collect();
    // The index before the sync record: a track's `first_seen` depends on there being none yet.
    // A read cut short only adds to it: the rows it did not reach are not gone, and dropping
    // them would date them "new" on the next complete read.
    if complete {
        db.set_playlist_songs_at(read.playlist_id, &rows, read.at);
    } else {
        db.upsert_playlist_songs_at(read.playlist_id, &rows, read.at);
    }
    // The rewrite kept every surviving track's date; the reader's dates, when it has them, are
    // the playlist's own word and replace them.
    if let Some(dates) = read.added_at {
        db.set_playlist_added_at(read.playlist_id, dates);
    }
    if complete {
        let _ = db.set_playlist_sync(
            read.playlist_id,
            &PlaylistSync {
                synced_at: read.at,
                item_count: read.songs.len() as i64,
                added: i64::from(counts.added),
                removed: i64::from(counts.removed),
                moved: i64::from(counts.moved),
                privacy: read.privacy.and_then(Privacy::parse),
            },
        );
    }
    counts
}

/// The setting the last full sync's summary is kept in, as JSON.
pub const SUMMARY_SETTING: &str = "playlist_index_last_summary";

/// One sync run, summed over its playlists: what the library line and the monitor show.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SyncSummary {
    pub at: i64,
    /// `manual_ui`, `scheduler` or `headless`.
    pub trigger: String,
    /// Playlists read (yours; the ones merely saved are skipped).
    pub playlists: u32,
    /// Read to the end.
    pub complete: u32,
    /// Could not be read at all.
    pub failed: u32,
    pub added: u32,
    pub removed: u32,
    pub moved: u32,
    pub unavailable: u32,
    pub restored: u32,
    pub alerts_new: u32,
    /// Data API units the run spent (`monitor_runs.units_spent`, what the backup reserve shrinks
    /// by); 0 when it read through InnerTube.
    #[serde(default)]
    pub units_spent: i64,
}

impl SyncSummary {
    pub fn new(trigger: &str, at: i64) -> Self {
        SyncSummary { at, trigger: trigger.to_owned(), ..Default::default() }
    }

    pub fn add(&mut self, c: Counts) {
        self.added += c.added;
        self.removed += c.removed;
        self.moved += c.moved;
        self.unavailable += c.unavailable;
        self.restored += c.restored;
        self.alerts_new += c.alerts_new;
    }

    /// The run's `monitor_runs.outcome`: `ok` when every playlist was read to the end, `partial`
    /// when some were, `failed` when none were.
    pub fn outcome(&self) -> &'static str {
        let short = self.playlists.saturating_sub(self.complete);
        if short == 0 {
            "ok"
        } else if self.complete > 0 {
            "partial"
        } else {
            "failed"
        }
    }
}

pub fn save_summary(db: &Db, summary: &SyncSummary) {
    if let Ok(json) = serde_json::to_string(summary) {
        db.set_setting(SUMMARY_SETTING, &json);
    }
}

pub fn last_summary(db: &Db) -> Option<SyncSummary> {
    db.get_setting(SUMMARY_SETTING).and_then(|j| serde_json::from_str(&j).ok())
}

/// A snapshot's rows in the order a reorder left the playlist in (`order`, row handles). Only the
/// rows both hold trade places, each taking a slot another of them had: a row the snapshot holds
/// and the order does not (gone since the last sync) stays where it was, and a row the order
/// holds that the snapshot does not (new since) stays out. So the next sync still reports those as
/// the removals and adds they are, and only the moves made here go unsaid.
pub fn reordered(items: &[SnapItem], order: &[String]) -> Vec<SnapItem> {
    let at: HashMap<&str, usize> = order.iter().enumerate().map(|(i, h)| (h.as_str(), i)).collect();
    let place = |item: &SnapItem| item.s.as_deref().and_then(|s| at.get(s).copied());
    let mut moving: Vec<&SnapItem> = items.iter().filter(|i| place(*i).is_some()).collect();
    moving.sort_by_key(|i| place(*i));
    let mut next = moving.into_iter();
    items
        .iter()
        .map(|item| match place(item) {
            Some(_) => next.next().unwrap_or(item).clone(),
            None => item.clone(),
        })
        .collect()
}

/// After a reorder made in this app: `before` is the order it found (row handles), `target` the
/// one it asked for. Store the newest snapshot again in the order the moves left it (same account
/// and title), so the next sync compares against your order and files no `moved` for it. The
/// membership index has no order, so it has nothing to update. Answers the new snapshot's id;
/// `None` with no snapshot to rebuild from (a local playlist, one never synced) or no change.
pub fn after_reorder(
    db: &Db,
    playlist_id: &str,
    before: &[String],
    target: &[String],
    at: i64,
) -> Option<i64> {
    let snap = db.latest_snapshot(playlist_id)?;
    let mut order = before.to_vec();
    lis::apply_moves(&mut order, &lis::reorder_moves(before, target));
    let items = reordered(&snap.items, &order);
    db.put_snapshot_if_changed(
        playlist_id,
        snap.account_id.as_deref(),
        snap.title.as_deref(),
        at,
        &items,
    )
}

/// The shortest wait before the scheduler tries again after a failed sync, doubled for each
/// failure in a row after the first.
pub const RETRY_BASE_SECS: i64 = 15 * 60;

/// Whether the scheduler may try a sync again: always after a success (`failures == 0`), never
/// with the interval off, and otherwise once `min(15 min · 2^(failures − 1), interval)` (15 min
/// at least) has passed since `last_attempt`. A run someone asked for does not wait for this.
///
/// With the interval off this is never due after a failure, even for an index that was never
/// built (which `playlist_index_due` always calls due). The failure count is stored, so a first
/// sync that fails is not retried by the scheduler, a launch or a sign-in, restart or not: only a
/// sync someone asks for (which clears the count when it works) builds it.
pub fn retry_due(
    now: i64,
    last_attempt: Option<i64>,
    failures: u32,
    interval: Option<i64>,
) -> bool {
    if failures == 0 {
        return true;
    }
    let Some(every) = interval else { return false };
    let Some(last) = last_attempt else { return true };
    let doubled = RETRY_BASE_SECS.saturating_mul(1_i64 << (failures - 1).min(20));
    let wait = doubled.min(every).max(RETRY_BASE_SECS);
    now >= last.saturating_add(wait)
}

/// A scheduler failure right after another one is not logged as a row of its own: it folds into
/// that row. Answers the row to update and its new `detail_json` (this failure's detail, with
/// `repeats` one more than the row had), or `None` when `run` gets a row of its own.
pub fn fold_failure(prev: Option<&MonitorRun>, run: &MonitorRun) -> Option<(i64, String)> {
    let scheduler_failure = |r: &MonitorRun| r.trigger == "scheduler" && r.outcome == "failed";
    let prev = prev.filter(|p| scheduler_failure(*p))?;
    if !scheduler_failure(run) {
        return None;
    }
    let repeats = serde_json::from_str::<Value>(&prev.detail_json)
        .ok()
        .and_then(|d| d.get("repeats").and_then(Value::as_u64))
        .unwrap_or(0);
    let mut detail = serde_json::from_str::<Value>(&run.detail_json)
        .ok()
        .filter(Value::is_object)
        .unwrap_or_else(|| json!({}));
    detail["repeats"] = json!(repeats + 1);
    Some((prev.id, detail.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn song(v: &str, unavailable: bool) -> SongItem {
        SongItem { video_id: v.into(), title: v.into(), unavailable, ..Default::default() }
    }
    fn row(v: &str, set: &str) -> SongItem {
        SongItem { set_video_id: Some(set.into()), ..song(v, false) }
    }
    fn snap(songs: &[SongItem]) -> Vec<SnapItem> {
        songs.iter().map(SnapItem::from_song).collect()
    }
    fn stored(v: &str, unavailable: bool) -> (String, Option<String>) {
        (v.into(), Some(serde_json::to_string(&song(v, unavailable)).unwrap()))
    }
    fn kinds(changes: &[Change]) -> Vec<(&str, Kind, Option<usize>, Option<usize>)> {
        changes.iter().map(|c| (c.video_id.as_str(), c.kind, c.from, c.to)).collect()
    }
    fn mem() -> Db {
        Db::open(std::path::Path::new(":memory:")).unwrap()
    }
    fn read(songs: &[SongItem], at: i64) -> Read<'_> {
        Read {
            playlist_id: "VLPL1",
            title: Some("Mix"),
            account_id: Some("ga-1"),
            songs,
            complete: true,
            watch: true,
            at,
            listed: None,
            added_at: None,
            privacy: None,
            reasons: None,
        }
    }

    #[test]
    fn switching_readers_is_not_every_row_removed_and_added() {
        // InnerTube's setVideoIds in the snapshot, the Data API's item ids now: same rows.
        let before = snap(&[row("a", "S1"), row("b", "S2"), row("c", "S3"), row("d", "S4")]);
        let now = [row("a", "UExA"), row("b", "UExB"), row("c", "UExC"), row("d", "UExD")];
        assert!(diff(&before, &now).is_empty());
        // Matched by video and occurrence, a move and a removal still show.
        let now = [row("d", "UExD"), row("a", "UExA"), row("c", "UExC")];
        let changes = diff(&before, &now);
        let got = kinds(&changes);
        let want = [("b", Kind::Removed, Some(1), None), ("d", Kind::Moved, Some(3), Some(0))];
        assert_eq!(got, want);
        // One handle in common: the same reader, matched by handle as before.
        let now = [row("a", "S1"), row("a", "S9")];
        let changes = diff(&snap(&[row("a", "S1"), row("a", "S2")]), &now);
        let got = kinds(&changes);
        assert_eq!(got, [("a", Kind::Removed, Some(1), None), ("a", Kind::Added, None, Some(1))]);
    }

    #[test]
    fn a_data_api_read_writes_dates_privacy_and_reasons() {
        let db = mem();
        record(&db, &read(&[row("a", "1"), row("b", "2")], 100));
        let dates: HashMap<String, i64> = [("a".to_owned(), 50), ("b".to_owned(), 60)].into();
        let reasons: HashMap<String, String> = [("b".to_owned(), "deleted".to_owned())].into();
        let songs = [row("a", "UExA"), SongItem { unavailable: true, ..row("b", "UExB") }];
        let got = record(
            &db,
            &Read {
                added_at: Some(&dates),
                privacy: Some("unlisted"),
                reasons: Some(&reasons),
                ..read(&songs, 200)
            },
        );
        assert_eq!((got.unavailable, got.alerts_new), (1, 1), "b greyed out, nothing else");
        assert_eq!(db.playlist_syncs()["VLPL1"].privacy, Some(Privacy::Unlisted));
        assert_eq!(db.playlist_added_dates().get("a"), Some(&50));
        let stored: HashMap<String, Option<String>> =
            db.playlist_songs("VLPL1").into_iter().collect();
        let b: Value = serde_json::from_str(stored["b"].as_deref().unwrap()).unwrap();
        assert_eq!(b["unavailable_reason"], "deleted");
        let a: Value = serde_json::from_str(stored["a"].as_deref().unwrap()).unwrap();
        assert!(a.get("unavailable_reason").is_none(), "only on an unavailable track");
        let alert = db.alert_rows(true).into_iter().find(|a| a.kind == "unavailable").unwrap();
        assert!(alert.song_json.unwrap_or_default().contains("\"unavailable_reason\":\"deleted\""));
        // An InnerTube read next keeps the dates and the privacy it cannot see.
        record(&db, &read(&[row("a", "1"), row("b", "2")], 300));
        assert_eq!(db.playlist_added_dates().get("a"), Some(&50));
        assert_eq!(db.playlist_syncs()["VLPL1"].privacy, Some(Privacy::Unlisted));
    }

    #[test]
    fn a_first_sync_says_nothing_but_an_empty_snapshot_is_compared() {
        assert!(diff_index(&[], &[song("a", true)]).is_empty());
        // An empty snapshot is a playlist that was empty: what is there now was added.
        assert_eq!(kinds(&diff(&[], &[song("a", false)])), [("a", Kind::Added, None, Some(0))]);
    }

    #[test]
    fn a_playlist_emptied_and_filled_again_alerts() {
        let db = mem();
        record(&db, &read(&[row("a", "1")], 100));
        // Emptied elsewhere, as the header confirms: one removal, and an empty snapshot.
        assert_eq!(record(&db, &Read { listed: Some(0), ..read(&[], 200) }).alerts_new, 1);
        assert_eq!(db.latest_snapshot("VLPL1").unwrap().item_count, 0);
        // Filled again elsewhere: compared with the empty snapshot, not taken for a first read.
        let got = record(&db, &read(&[row("b", "2"), row("c", "3")], 300));
        assert_eq!((got.added, got.alerts_new), (2, 2));
        // A playlist whose first read is empty says nothing then, and everything after.
        record(&db, &Read { playlist_id: "VLPL2", ..read(&[], 400) });
        assert!(db.alert_rows(true).iter().all(|a| a.playlist_id != "VLPL2"));
        let got = record(&db, &Read { playlist_id: "VLPL2", ..read(&[row("x", "9")], 500) });
        assert_eq!(got.alerts_new, 1);
    }

    #[test]
    fn an_empty_read_after_rows_is_doubted_unless_the_header_says_empty() {
        let db = mem();
        let full = [row("a", "1"), row("b", "2")];
        record(&db, &read(&full, 100));
        let empty = read(&[], 200);
        assert!(!read_complete(&db, &empty));
        assert_eq!(record(&db, &empty), Counts::default());
        assert_eq!(db.latest_snapshot("VLPL1").unwrap().item_count, 2, "no empty snapshot");
        assert_eq!(db.playlist_songs("VLPL1").len(), 2, "the index stands");
        assert_eq!(db.playlist_syncs()["VLPL1"].synced_at, 100, "not a sync");
        // The next good read finds nothing: no removals, nothing re-added as new.
        assert_eq!(record(&db, &read(&full, 300)), Counts::default());
        assert!(db.alert_rows(true).is_empty());
        let dated: Vec<Option<i64>> = db.indexed_songs().into_iter().map(|r| r.3).collect();
        assert_eq!(dated, [None, None]);
        // A header saying something other than 0, or nothing, does not vouch for it.
        assert!(!read_complete(&db, &Read { listed: Some(2), ..read(&[], 400) }));
        assert!(read_complete(&db, &Read { listed: Some(0), ..read(&[], 400) }));
        // With no snapshot (Liked Music keeps none), the index alone is enough to doubt it.
        let liked_songs = [row("x", "9")];
        record(&db, &Read { watch: false, playlist_id: "VLLM", ..read(&liked_songs, 500) });
        let liked_empty = Read { watch: false, playlist_id: "VLLM", ..read(&[], 600) };
        assert!(!read_complete(&db, &liked_empty));
        record(&db, &liked_empty);
        assert_eq!(db.playlist_songs("VLLM").len(), 1);
        // Never held anything: an empty read is just an empty playlist.
        assert!(read_complete(&db, &Read { playlist_id: "VLPL3", ..read(&[], 700) }));
    }

    #[test]
    fn header_track_count_reads_only_what_it_knows() {
        assert_eq!(header_track_count("12 songs • 45 minutes"), Some(12));
        assert_eq!(header_track_count("1 track"), Some(1));
        assert_eq!(header_track_count("0 songs"), Some(0));
        assert_eq!(header_track_count("1,204 songs • 80+ hours"), Some(1204));
        assert_eq!(header_track_count("Playlist • Private • 2024"), None);
        assert_eq!(header_track_count("No songs"), None);
        assert_eq!(header_track_count("12 canciones • 45 minutos"), None);
    }

    #[test]
    fn copies_are_counted_when_telling_edits_made_here() {
        let db = mem();
        record(&db, &read(&[row("a", "1"), row("b", "2")], 100));
        // A second copy of a, added on another device: the index (which holds a once) cannot
        // have seen it, so it alerts.
        let got = record(&db, &read(&[row("a", "1"), row("b", "2"), row("a", "3")], 200));
        assert_eq!((got.added, got.alerts_new), (1, 1));
        // b taken out and put back at the end elsewhere: still there, so neither a removal nor
        // an add to tell.
        let got = record(&db, &read(&[row("a", "1"), row("a", "3"), row("b", "4")], 300));
        assert_eq!((got.added, got.removed, got.alerts_new), (1, 1, 0));
        // One copy of a taken out here (the index drops a), the other still there.
        db.remove_playlist_track("VLPL1", "a");
        let got = record(&db, &read(&[row("a", "1"), row("b", "4")], 400));
        assert_eq!((got.removed, got.alerts_new), (1, 0));
        // c added here once, and once more elsewhere: one of the two adds is news.
        db.add_playlist_track("VLPL1", "c");
        let doubled = [row("a", "1"), row("b", "4"), row("c", "5"), row("c", "6")];
        let got = record(&db, &read(&doubled, 500));
        assert_eq!((got.added, got.alerts_new), (2, 1));
    }

    #[test]
    fn made_here_pairs_and_counts() {
        let change = |v: &str, kind: Kind| Change {
            video_id: v.into(),
            kind,
            song: None,
            from: None,
            to: None,
        };
        let changes = [
            change("a", Kind::Removed),
            change("a", Kind::Added),
            change("b", Kind::Added),
            change("c", Kind::Added),
            change("d", Kind::Removed),
            change("e", Kind::Removed),
        ];
        let before: HashMap<&str, usize> = [("a", 1), ("c", 1), ("d", 2), ("e", 1)].into();
        // Index: a, b and c held (b added here), d dropped here, e still held.
        let held: HashSet<&str> = ["a", "b", "c", "e"].into();
        let quiet = made_here(&changes, &before, &held);
        assert_eq!(quiet, [true, true, true, false, true, false]);
    }

    #[test]
    fn a_short_read_keeps_the_unread_tail_in_the_index() {
        let db = mem();
        let full = [row("a", "1"), row("b", "2"), row("c", "3")];
        record(&db, &read(&full, 100));
        let first_page = [row("a", "1")];
        record(&db, &Read { complete: false, ..read(&first_page, 200) });
        assert_eq!(db.playlist_songs("VLPL1").len(), 3, "the tail is not gone");
        record(&db, &read(&full, 300));
        let dated: Vec<Option<i64>> = db.indexed_songs().into_iter().map(|r| r.3).collect();
        assert_eq!(dated, [None, None, None], "nothing in the tail reads as new");
        assert!(db.alert_rows(true).is_empty());
    }

    #[test]
    fn reordered_moves_only_the_rows_both_hold() {
        let items = snap(&[row("a", "1"), row("b", "2"), row("c", "3"), row("d", "4")]);
        let order = |s: &[&str]| s.iter().map(|h| h.to_string()).collect::<Vec<_>>();
        // The whole list: the snapshot takes its order.
        let got = reordered(&items, &order(&["4", "1", "2", "3"]));
        let handles: Vec<&str> = got.iter().map(|i| i.s.as_deref().unwrap()).collect();
        assert_eq!(handles, ["4", "1", "2", "3"]);
        // b gone and e new since the last sync: b keeps its slot, e stays out.
        let got = reordered(&items, &order(&["5", "3", "1", "4"]));
        let handles: Vec<&str> = got.iter().map(|i| i.s.as_deref().unwrap()).collect();
        assert_eq!(handles, ["3", "2", "1", "4"]);
    }

    #[test]
    fn a_reorder_made_here_is_not_reported_as_moves() {
        let db = mem();
        record(&db, &read(&[row("a", "1"), row("b", "2"), row("c", "3")], 100));
        let before = ["1".to_string(), "2".into(), "3".into()];
        let target = ["3".to_string(), "1".into(), "2".into()];
        assert!(after_reorder(&db, "VLPL1", &before, &target, 150).is_some());
        let snap = db.latest_snapshot("VLPL1").unwrap();
        let filed_as = (snap.title.as_deref(), snap.account_id.as_deref());
        assert_eq!(filed_as, (Some("Mix"), Some("ga-1")));
        let got = record(&db, &read(&[row("c", "3"), row("a", "1"), row("b", "2")], 200));
        assert_eq!(got, Counts::default());
        // Nothing to rebuild from: no snapshot, no write.
        assert_eq!(after_reorder(&db, "VLPL9", &before, &target, 300), None);
    }

    #[test]
    fn the_scheduler_backs_off_after_failures() {
        let hour = Some(3600);
        let six = Some(6 * 3600);
        assert!(retry_due(0, None, 0, None), "no failure: nothing to wait for");
        assert!(!retry_due(1_000_000, Some(0), 1, None), "interval off: no automatic retries");
        // 15 min, 30, 60, 120... capped at the interval.
        assert!(!retry_due(899, Some(0), 1, six));
        assert!(retry_due(900, Some(0), 1, six));
        assert!(!retry_due(1799, Some(0), 2, six));
        assert!(retry_due(1800, Some(0), 2, six));
        assert!(!retry_due(6 * 3600 - 1, Some(0), 10, six));
        assert!(retry_due(6 * 3600, Some(0), 10, six));
        assert!(retry_due(3600, Some(0), 40, hour), "no overflow, capped at an hour");
        assert!(retry_due(5, None, 3, six), "no attempt on record");
    }

    #[test]
    fn scheduler_failures_in_a_row_fold_into_one_row() {
        let run = |id, trigger: &str, outcome: &str, detail: &str| MonitorRun {
            id,
            started_at: 0,
            finished_at: 0,
            trigger: trigger.into(),
            outcome: outcome.into(),
            playlists_ok: 0,
            playlists_failed: 0,
            alerts_new: 0,
            units_spent: 0,
            detail_json: detail.into(),
        };
        let first = run(7, "scheduler", "failed", r#"{"error":"offline"}"#);
        let again = run(0, "scheduler", "failed", r#"{"error":"cookie"}"#);
        let (id, detail) = fold_failure(Some(&first), &again).unwrap();
        assert_eq!(id, 7);
        let detail: Value = serde_json::from_str(&detail).unwrap();
        assert_eq!(detail, json!({ "error": "cookie", "repeats": 1 }));
        let folded = run(7, "scheduler", "failed", r#"{"error":"cookie","repeats":1}"#);
        let (_, detail) = fold_failure(Some(&folded), &again).unwrap();
        assert!(detail.contains(r#""repeats":2"#));
        // A first failure, one after a success, a manual one: rows of their own.
        assert_eq!(fold_failure(None, &again), None);
        assert_eq!(fold_failure(Some(&run(7, "scheduler", "ok", "{}")), &again), None);
        assert_eq!(fold_failure(Some(&run(7, "manual_ui", "failed", "{}")), &again), None);
        assert_eq!(fold_failure(Some(&first), &run(0, "manual_ui", "failed", "{}")), None);
    }

    #[test]
    fn the_same_song_twice_is_two_rows() {
        // By occurrence, with no setVideoId: the second copy went.
        let before = snap(&[song("a", false), song("b", false), song("a", false)]);
        let got = diff(&before, &[song("a", false), song("b", false)]);
        assert_eq!(kinds(&got), [("a", Kind::Removed, Some(2), None)]);
        // And a second copy turning up is an add, not nothing.
        let got = diff(&snap(&[song("a", false)]), &[song("a", false), song("a", false)]);
        assert_eq!(kinds(&got), [("a", Kind::Added, None, Some(1))]);
        // By setVideoId: it is the first copy that went, and the second did not move.
        let before = snap(&[row("a", "s1"), row("b", "s2"), row("a", "s3")]);
        let got = diff(&before, &[row("b", "s2"), row("a", "s3")]);
        assert_eq!(kinds(&got), [("a", Kind::Removed, Some(0), None)]);
    }

    #[test]
    fn moved_counts_only_the_fewest_rows() {
        let before = snap(&[row("a", "1"), row("b", "2"), row("c", "3"), row("d", "4")]);
        // One track dragged from the top to the bottom: one move, not three.
        let got = diff(&before, &[row("b", "2"), row("c", "3"), row("d", "4"), row("a", "1")]);
        assert_eq!(kinds(&got), [("a", Kind::Moved, Some(0), Some(3))]);
        // A swap is one move either way.
        let got = diff(&before, &[row("b", "2"), row("a", "1"), row("c", "3"), row("d", "4")]);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].kind, Kind::Moved);
        // A removal above a row shifts its index but is not a move.
        let got = diff(&before, &[row("b", "2"), row("c", "3"), row("d", "4")]);
        assert_eq!(kinds(&got), [("a", Kind::Removed, Some(0), None)]);
        // The same order is no change at all.
        let same = [row("a", "1"), row("b", "2"), row("c", "3"), row("d", "4")];
        assert!(diff(&before, &same).is_empty());
    }

    #[test]
    fn unavailable_and_restored() {
        let before = snap(&[song("a", false), song("b", true), song("c", true)]);
        let got = diff(&before, &[song("a", true), song("b", false), song("c", true)]);
        assert_eq!(
            kinds(&got),
            [("a", Kind::Unavailable, Some(0), Some(0)), ("b", Kind::Restored, Some(1), Some(1))]
        );
        // c was already unavailable: nothing changed under you.
        let added_grey = diff(&snap(&[song("a", false)]), &[song("a", false), song("e", true)]);
        assert_eq!(kinds(&added_grey), [("e", Kind::Added, None, Some(1))]);
    }

    #[test]
    fn with_no_snapshot_the_index_stands_in_without_moves() {
        let before =
            vec![stored("a", false), stored("b", false), stored("c", true), ("d".into(), None)];
        let now = vec![song("c", false), song("b", true), song("d", true), song("e", false)];
        let got = diff_index(&before, &now);
        assert_eq!(
            kinds(&got),
            [
                ("a", Kind::Removed, None, None),
                ("c", Kind::Restored, None, Some(0)),
                ("b", Kind::Unavailable, None, Some(1)), // greyed out since; reordered, not moved
                ("d", Kind::Unavailable, None, Some(2)), // no metadata last time: say it
                ("e", Kind::Added, None, Some(3)),
            ]
        );
        assert!(got.iter().all(|c| c.kind != Kind::Moved));
    }

    #[test]
    fn record_files_nothing_on_a_first_read_and_keeps_a_snapshot() {
        let db = mem();
        let songs = [row("a", "1"), row("b", "2")];
        assert_eq!(record(&db, &read(&songs, 100)), Counts::default());
        assert!(db.alert_rows(true).is_empty());
        let snap = db.latest_snapshot("VLPL1").unwrap();
        assert_eq!((snap.item_count, snap.account_id.as_deref()), (2, Some("ga-1")));
        assert_eq!(db.playlist_syncs()["VLPL1"].item_count, 2);
        assert_eq!(db.playlist_songs("VLPL1").len(), 2);
    }

    #[test]
    fn record_files_each_change_and_a_repeat_files_again() {
        let db = mem();
        record(&db, &read(&[row("a", "1"), row("b", "2"), row("c", "3")], 100));
        // Elsewhere: c moved to the top, b went, d came.
        let second = [row("c", "3"), row("a", "1"), row("d", "4")];
        let got = record(&db, &read(&second, 200));
        assert_eq!(
            got,
            Counts { added: 1, removed: 1, moved: 1, alerts_new: 3, ..Default::default() }
        );
        let sync = db.playlist_syncs()["VLPL1"];
        assert_eq!((sync.added, sync.removed, sync.moved), (1, 1, 1));
        let alerts = db.alert_rows(true);
        let moved = alerts.iter().find(|a| a.kind == "moved").unwrap();
        let moved = (moved.video_id.as_str(), moved.from_pos, moved.to_pos);
        assert_eq!(moved, ("c", Some(2), Some(0)));
        // The same read again: no new snapshot, no new alerts.
        assert_eq!(record(&db, &read(&second, 300)).alerts_new, 0);
        assert_eq!(db.snapshots("VLPL1").len(), 2);
        // b back, then gone again: a second `removed` row for it, not a swallowed repeat.
        record(&db, &read(&[row("c", "3"), row("a", "1"), row("d", "4"), row("b", "5")], 400));
        record(&db, &read(&second, 500));
        let removed_b = db
            .alert_rows(true)
            .into_iter()
            .filter(|a| a.kind == "removed" && a.video_id == "b")
            .count();
        assert_eq!(removed_b, 2);
    }

    #[test]
    fn record_says_nothing_about_edits_made_here() {
        let db = mem();
        record(&db, &read(&[row("a", "1"), row("b", "2")], 100));
        // Removed b and added c in this app: the index already knows.
        db.remove_playlist_track("VLPL1", "b");
        db.add_playlist_track("VLPL1", "c");
        let got = record(&db, &read(&[row("a", "1"), row("c", "3")], 200));
        assert_eq!((got.added, got.removed, got.alerts_new), (1, 1, 0));
        assert!(db.alert_rows(true).is_empty());
    }

    #[test]
    fn record_of_a_short_read_or_liked_music_compares_nothing() {
        let db = mem();
        record(&db, &read(&[row("a", "1"), row("b", "2")], 100));
        let first_page = [row("a", "1")];
        let short = Read { complete: false, ..read(&first_page, 200) };
        assert_eq!(record(&db, &short), Counts::default());
        assert_eq!(db.playlist_syncs()["VLPL1"].synced_at, 100, "a short read is not a sync");
        let liked_songs = [row("x", "9")];
        record(&db, &Read { watch: false, playlist_id: "VLLM", ..read(&liked_songs, 300) });
        let emptied = Read { listed: Some(0), ..read(&[], 400) };
        record(&db, &Read { watch: false, playlist_id: "VLLM", ..emptied });
        assert!(db.alert_rows(true).is_empty());
        assert!(db.latest_snapshot("VLLM").is_none());
        assert_eq!(db.playlist_syncs()["VLLM"].synced_at, 400);
    }

    #[test]
    fn summary_outcome_and_round_trip() {
        let db = mem();
        let mut s = SyncSummary::new("scheduler", 10);
        assert_eq!(s.outcome(), "ok");
        s.playlists = 3;
        s.complete = 2;
        s.failed = 1;
        assert_eq!(s.outcome(), "partial");
        s.complete = 0;
        assert_eq!(s.outcome(), "failed");
        s.add(Counts { added: 2, restored: 1, alerts_new: 3, ..Default::default() });
        assert_eq!(last_summary(&db), None);
        save_summary(&db, &s);
        assert_eq!(last_summary(&db), Some(s));
    }
}
