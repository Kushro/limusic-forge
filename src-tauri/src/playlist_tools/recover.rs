//! The recovery assistant (F4): find the dead tracks of your account playlists, work out what they
//! were called, and (in the steps after this one) search a replacement and put it where the dead
//! one sat.
//!
//! This part reads only the database, no network:
//! - [`collect_candidates`] lists what is worth recovering, from three sources: the rows each
//!   playlist's newest snapshot marks unavailable (or, for a playlist with no snapshot, the index's
//!   unavailable rows), the `unavailable` alerts no `restored` answered, and the `removed` alerts
//!   of tracks that never came back. Playlists on this machine and Liked Music are never listed
//!   (invariant 6).
//! - [`best_local_titles`] picks a title for the ones whose own row lost it, from what the app has
//!   seen of that video anywhere else.
//! - The session ([`with_session`]) keeps one [`RecoverRow`] per candidate between the steps, as
//!   the import keeps its rows: one assistant per process.
//!
//! The steps that go out to the network run in the background, one at a time, and report through
//! `recover-progress` events carrying a [`RecoverSnapshot`], as the import does with its own:
//! - [`start_titles`] looks up what the untitled ones were called: the `recover_titles` cache,
//!   then (when asked) the Wayback Machine ([`crate::wayback`]), one video at a time, 2 to 4 s
//!   apart, backing off when the Archive says so;
//! - [`start_search`] searches YouTube Music for a replacement of each titled one with the
//!   importer's search and scoring ([`crate::import::search`], [`crate::import::classify`]),
//!   sharing its request slot, its hourly budget and its cooldown, and its `import_matches` cache
//!   under `recover:<videoId>`, so a stopped search picks up where it was.
//!
//! [`apply`] puts the approved replacements in, one playlist at a time and waited for (the command
//! answers what it did), still reporting through `recover-progress`. [`plan_replacements`] works
//! out each playlist's edit from a fresh read (invariant 3); a dead track only goes once its
//! replacement is in, or already was (invariant 1, D2).

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};

use innertube::SongItem;
use regex::Regex;
use serde::Serialize;
use serde_json::{json, Value};
use tauri::Emitter;

use super::dedup;
use super::journal::{self, Named, OpRecord, Restore, Step, Summary};
use super::lis::Move;
use super::rows::{self, handle};
use crate::commands::LIKED_MUSIC_ID;
use crate::db::{now_secs, AlertRow, CachedTitle, Db, LocalTitleRows, SnapItem, Snapshot};
use crate::import::{self, Gate, Halt, Tier};
use crate::jobs::engine::{Engine, QueueTarget};
use crate::jobs::planner::{self, action_name};
use crate::jobs::{JobItemStatus, JobKind, JobsState, NewJob, NewJobItem, ENGINE_KEY};
use crate::spotify::SourceTrack;
use crate::state::{is_local_playlist, AppState};
use crate::wayback::{self, WaybackError};

// --- placeholder titles ---------------------------------------------------------------------------

/// What YouTube shows where a gone video's title was, in English and Spanish. Compared trimmed and
/// lowercased. Shared by every reader that must not take one of these for a real title (the
/// PlaylistForge import, the Data API sync, Wayback, this assistant).
const PLACEHOLDER_TITLES: &[&str] = &[
    "",
    "deleted video",
    "private video",
    "video unavailable",
    "this video is unavailable",
    "this video isn't available anymore",
    "video eliminado",
    "vídeo eliminado",
    "video privado",
    "vídeo privado",
    "video no disponible",
    "vídeo no disponible",
    "este video no está disponible",
    "este vídeo no está disponible",
];

/// YouTube's stand-in title for a gone video (or none at all): never taken as a real title.
pub(crate) fn is_placeholder_title(title: &str) -> bool {
    let lower = title.trim().to_lowercase();
    PLACEHOLDER_TITLES.contains(&lower.as_str())
}

/// `Some(trimmed)` unless it is a placeholder.
fn real_title(title: &str) -> Option<String> {
    (!is_placeholder_title(title)).then(|| title.trim().to_owned())
}

fn non_empty(s: &str) -> Option<String> {
    let s = s.trim();
    (!s.is_empty()).then(|| s.to_owned())
}

// --- candidates -----------------------------------------------------------------------------------

/// A row of the newest snapshot marked unavailable (or an index row so marked).
pub const SOURCE_UNAVAILABLE: &str = "unavailable";
/// An `unavailable` alert no later `restored` answered.
pub const SOURCE_ALERT_UNAVAILABLE: &str = "alert_unavailable";
/// A `removed` alert of a track that is not back in that playlist.
pub const SOURCE_ALERT_REMOVED: &str = "alert_removed";

/// A candidate's (and its session row's) key: playlist and videoId joined by U+001F.
pub fn key_of(playlist_id: &str, video_id: &str) -> String {
    format!("{playlist_id}\u{1f}{video_id}")
}

/// One dead track in one playlist, once however many sources name it.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct RecoverCandidate {
    /// [`key_of`] `(playlist_id, video_id)`.
    pub key: String,
    pub playlist_id: String,
    pub video_id: String,
    /// [`SOURCE_UNAVAILABLE`], [`SOURCE_ALERT_UNAVAILABLE`], [`SOURCE_ALERT_REMOVED`], each once,
    /// in the order found.
    pub sources: Vec<&'static str>,
    /// The alerts behind it, newest first: dismissed and marked seen once it is recovered.
    pub alert_ids: Vec<i64>,
    /// Still listed in the playlist (newest snapshot or the index), dead.
    pub in_playlist: bool,
    /// 0-based. The live one from the newest snapshot when it is still there; where it was before
    /// its removal for a removed one; null when unknown.
    pub position: Option<u32>,
    /// The row's handle in the newest snapshot. Display and matching only: the edits read the
    /// playlist fresh (invariant 3).
    pub set_video_id: Option<String>,
    /// The handle of the row after it (in the newest snapshot, or in the one before its removal):
    /// what its replacement is put in front of. Null when it was last, or unknown.
    pub next_handle: Option<String>,
    pub title: Option<String>,
    pub artists: Option<String>,
    pub duration: Option<String>,
    pub thumbnail: Option<String>,
    /// Every alert behind it is dismissed (only listed when asked for, D5).
    pub dismissed: bool,
    /// Where `title` came from (`snapshot`, `alert`, `index`, or a [`best_local_titles`] source):
    /// the session row's `title_source`. Not sent to the UI.
    #[serde(skip)]
    pub title_source: Option<String>,
}

/// What [`collect_candidates`] reads, already loaded.
pub struct Stored<'a> {
    /// Each playlist's newest snapshot ([`Db::latest_snapshots`]).
    pub latest: &'a HashMap<String, Snapshot>,
    /// Every alert, dismissed ones included, newest first ([`Db::alert_rows`]).
    pub alerts: &'a [AlertRow],
    /// videoId → the playlists holding it ([`Db::playlist_memberships`]).
    pub memberships: &'a HashMap<String, Vec<String>>,
    /// `(playlist id, videoId, song_json)` of the index rows marked unavailable
    /// ([`Db::unavailable_index_rows`]): the fallback for a playlist with no snapshot.
    pub unavailable_index: &'a [(String, String, String)],
}

/// Never touched by the assistant (invariant 6).
fn excluded(playlist_id: &str) -> bool {
    playlist_id == LIKED_MUSIC_ID || is_local_playlist(playlist_id)
}

fn blank(playlist_id: &str, video_id: &str, source: &'static str) -> RecoverCandidate {
    RecoverCandidate {
        key: key_of(playlist_id, video_id),
        playlist_id: playlist_id.to_owned(),
        video_id: video_id.to_owned(),
        sources: vec![source],
        alert_ids: Vec::new(),
        in_playlist: false,
        position: None,
        set_video_id: None,
        next_handle: None,
        title: None,
        artists: None,
        duration: None,
        thumbnail: None,
        dismissed: false,
        title_source: None,
    }
}

/// Fill in whatever of `song` the candidate is missing. The title only when it is a real one.
fn fill_from_song(c: &mut RecoverCandidate, song: &SongItem, source: &str) {
    if c.title.is_none() {
        if let Some(t) = real_title(&song.title) {
            c.title = Some(t);
            c.title_source = Some(source.to_owned());
            if c.artists.is_none() {
                c.artists = non_empty(&song.artists);
            }
        }
    }
    if c.duration.is_none() {
        c.duration = song.duration.clone();
    }
    if c.thumbnail.is_none() {
        c.thumbnail = song.thumbnail.clone();
    }
}

fn fill_from_item(c: &mut RecoverCandidate, item: &SnapItem) {
    if c.title.is_none() {
        if let Some(t) = real_title(&item.t) {
            c.title = Some(t);
            c.title_source = Some("snapshot".to_owned());
            if c.artists.is_none() {
                c.artists = non_empty(&item.a);
            }
        }
    }
    if c.duration.is_none() {
        c.duration = item.d.clone();
    }
    if c.thumbnail.is_none() {
        c.thumbnail = item.th.clone();
    }
}

/// Every dead track worth recovering, once per `(playlist, videoId)`, sorted by playlist and
/// position (unknown positions last).
///
/// - (a) the rows of each playlist's newest snapshot marked unavailable: position, handle and the
///   next row's handle. A video dead twice in one playlist is listed at its first copy. A playlist
///   with no snapshot falls back to its index rows marked unavailable, with no position.
/// - (b) `unavailable` alerts not resolved by a later `restored`, unless the newest snapshot shows
///   every copy of the track playing again.
/// - (c) `removed` alerts of tracks not back in that playlist (newest snapshot or index). The
///   position is where it was; the anchor is the row after it in the newest snapshot taken before
///   the alert (`prior(playlist, alert.at)`), when that snapshot has the track at that position.
///
/// Dismissed alerts count only with `include_dismissed` (D5). Sources and alert ids of one track
/// are merged; a live position from (a) wins over an alert's.
pub fn collect_candidates(
    stored: &Stored<'_>,
    include_dismissed: bool,
    mut prior: impl FnMut(&str, i64) -> Option<Snapshot>,
) -> Vec<RecoverCandidate> {
    let mut out: Vec<RecoverCandidate> = Vec::new();
    let mut at: HashMap<String, usize> = HashMap::new();
    // Per candidate: whether an alert behind it is open, and whether one is dismissed.
    let mut alert_flags: Vec<(bool, bool)> = Vec::new();

    // (a) the newest snapshots.
    for (pid, snap) in stored.latest {
        if excluded(pid) {
            continue;
        }
        for (i, item) in snap.items.iter().enumerate() {
            if !item.u {
                continue;
            }
            let key = key_of(pid, &item.v);
            if at.contains_key(&key) {
                continue;
            }
            let mut c = blank(pid, &item.v, SOURCE_UNAVAILABLE);
            c.in_playlist = true;
            c.position = Some(i as u32);
            c.set_video_id = item.s.clone();
            c.next_handle = snap.items.get(i + 1).and_then(|n| n.s.clone());
            fill_from_item(&mut c, item);
            at.insert(key, out.len());
            out.push(c);
            alert_flags.push((false, false));
        }
    }
    // (a) fallback: the index, for a playlist never snapshotted.
    for (pid, vid, json) in stored.unavailable_index {
        if excluded(pid) || stored.latest.contains_key(pid) {
            continue;
        }
        let key = key_of(pid, vid);
        if at.contains_key(&key) {
            continue;
        }
        let mut c = blank(pid, vid, SOURCE_UNAVAILABLE);
        c.in_playlist = true;
        if let Ok(song) = serde_json::from_str::<SongItem>(json) {
            fill_from_song(&mut c, &song, "index");
        }
        at.insert(key, out.len());
        out.push(c);
        alert_flags.push((false, false));
    }

    let held = |pid: &str, vid: &str| {
        stored.latest.get(pid).is_some_and(|s| s.items.iter().any(|i| i.v == vid))
            || stored.memberships.get(vid).is_some_and(|ps| ps.iter().any(|p| p == pid))
    };
    // Plays again: the newest snapshot holds it, and every copy plays.
    let playing = |pid: &str, vid: &str| {
        stored.latest.get(pid).is_some_and(|s| {
            let mut copies = s.items.iter().filter(|i| i.v == vid).peekable();
            copies.peek().is_some() && copies.all(|i| !i.u)
        })
    };
    let mut before: HashMap<(String, i64), Option<Snapshot>> = HashMap::new();

    // (b) and (c), newest first.
    for a in stored.alerts {
        if excluded(&a.playlist_id) || (a.dismissed && !include_dismissed) {
            continue;
        }
        let (pid, vid) = (a.playlist_id.as_str(), a.video_id.as_str());
        let source = match a.kind.as_str() {
            "unavailable" if !a.resolved && !playing(pid, vid) => SOURCE_ALERT_UNAVAILABLE,
            "removed" if !held(pid, vid) => SOURCE_ALERT_REMOVED,
            _ => continue,
        };
        let key = key_of(pid, vid);
        let i = match at.get(&key) {
            Some(&i) => {
                if !out[i].sources.contains(&source) {
                    out[i].sources.push(source);
                }
                i
            }
            None => {
                let mut c = blank(pid, vid, source);
                c.in_playlist = held(pid, vid);
                at.insert(key, out.len());
                out.push(c);
                alert_flags.push((false, false));
                out.len() - 1
            }
        };
        let c = &mut out[i];
        c.alert_ids.push(a.id);
        let flags = &mut alert_flags[i];
        if a.dismissed {
            flags.1 = true;
        } else {
            flags.0 = true;
        }
        let song = a.song_json.as_deref().and_then(|j| serde_json::from_str::<SongItem>(j).ok());
        if let Some(song) = song {
            fill_from_song(c, &song, "alert");
        }
        // Where a removed one was, and the row it sat in front of. Never over a live position.
        if source == SOURCE_ALERT_REMOVED && c.position.is_none() {
            if let Some(from) = a.from_pos.and_then(|p| u32::try_from(p).ok()) {
                c.position = Some(from);
                let snap =
                    before.entry((pid.to_owned(), a.at)).or_insert_with(|| prior(pid, a.at));
                if let Some(snap) = snap.as_ref() {
                    let from = from as usize;
                    if let Some(item) = snap.items.get(from).filter(|it| it.v == vid) {
                        c.next_handle = snap.items.get(from + 1).and_then(|n| n.s.clone());
                        fill_from_item(c, item);
                    }
                }
            }
        }
    }

    for (c, (open, dismissed)) in out.iter_mut().zip(&alert_flags) {
        c.dismissed = *dismissed && !*open;
    }
    out.sort_by(|a, b| {
        a.playlist_id
            .cmp(&b.playlist_id)
            .then_with(|| a.position.is_none().cmp(&b.position.is_none()))
            .then_with(|| a.position.cmp(&b.position))
            .then_with(|| a.video_id.cmp(&b.video_id))
    });
    out
}

// --- local titles ---------------------------------------------------------------------------------

/// `(title, artists, source)`; `artists` may be empty.
pub type LocalTitle = (String, String, &'static str);

/// The best title the app has seen for each of `wanted`, in this order: the newest snapshot row
/// where it still played (`snapshot`), an alert's song (`alert`), the play history (`history`),
/// `videos` (downloads and the PlaylistForge import, `download`), the index (`index`).
/// Placeholders never count; a video with nothing else is left out.
pub fn best_local_titles(
    wanted: &HashSet<String>,
    rows: &LocalTitleRows,
) -> HashMap<String, LocalTitle> {
    let mut out: HashMap<String, LocalTitle> = HashMap::new();
    let mut offer = |vid: &str, title: &str, artists: &str, source: &'static str| {
        if !wanted.contains(vid) || out.contains_key(vid) {
            return;
        }
        if let Some(title) = real_title(title) {
            out.insert(vid.to_owned(), (title, artists.trim().to_owned(), source));
        }
    };
    for (vid, title, artists) in &rows.snapshots {
        offer(vid, title, artists, "snapshot");
    }
    for (list, source) in [(&rows.alerts, "alert"), (&rows.plays, "history")] {
        for (vid, json) in list {
            if let Ok(song) = serde_json::from_str::<SongItem>(json) {
                offer(vid, &song.title, &song.artists, source);
            }
        }
    }
    for (vid, title, channel) in &rows.videos {
        // A Topic channel (`Artist - Topic`) is the artist.
        let channel = channel.as_deref().unwrap_or("");
        let artist = channel.strip_suffix(" - Topic").unwrap_or(channel);
        offer(vid, title, artist, "download");
    }
    for (vid, json) in &rows.index {
        if let Ok(song) = serde_json::from_str::<SongItem>(json) {
            offer(vid, &song.title, &song.artists, "index");
        }
    }
    out
}

// --- the session ----------------------------------------------------------------------------------

/// One candidate's progress through the assistant.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct RecoverRow {
    pub key: String,
    /// What it was called, as far as known: the search's input.
    pub title: Option<String>,
    pub artists: Option<String>,
    /// `snapshot`, `alert`, `history`, `download`, `index`, `wayback` or `manual`.
    pub title_source: Option<String>,
    /// `pending` until searched.
    pub tier: Tier,
    /// The replacement chosen (the search's best, or the user's).
    pub pick: Option<SongItem>,
    pub candidates: Vec<SongItem>,
    /// The user confirmed `pick`.
    pub approved: bool,
    /// `idle`, `done`, `failed` or `already_present`.
    pub status: &'static str,
    pub error: Option<String>,
    /// The dead track's length ("3:45") when known: the search scores candidates by it. The UI
    /// has it from the candidate.
    #[serde(skip)]
    pub duration: Option<String>,
}

impl RecoverRow {
    /// A fresh row for a candidate.
    pub fn of(c: &RecoverCandidate) -> Self {
        RecoverRow {
            key: c.key.clone(),
            title: c.title.clone(),
            artists: c.artists.clone(),
            title_source: c.title.as_ref().and(c.title_source.clone()),
            tier: Tier::Pending,
            pick: None,
            candidates: Vec::new(),
            approved: false,
            status: "idle",
            error: None,
            duration: c.duration.clone(),
        }
    }
}

/// The assistant's progress, as the `recover-progress` event carries it.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct RecoverSnapshot {
    /// `idle`, `titles`, `searching`, `applying`, `done`, `cancelled` or `failed`.
    pub phase: &'static str,
    pub done: usize,
    pub total: usize,
    pub current: Option<String>,
    pub waiting_until: Option<i64>,
    pub message: Option<String>,
}

impl Default for RecoverSnapshot {
    fn default() -> Self {
        RecoverSnapshot {
            phase: "idle",
            done: 0,
            total: 0,
            current: None,
            waiting_until: None,
            message: None,
        }
    }
}

/// The assistant's rows, in the candidates' order, and where its current step is.
#[derive(Debug, Default)]
pub struct Session {
    rows: Vec<RecoverRow>,
    index: HashMap<String, usize>,
    pub snapshot: RecoverSnapshot,
    /// Bumped by whoever starts a step, so a cancelled or replaced one can tell (as `import`'s).
    pub gen: u64,
}

impl Session {
    /// A step is running: titles, search or apply.
    pub fn busy(&self) -> bool {
        matches!(self.snapshot.phase, "titles" | "searching" | "applying")
    }

    pub fn row(&self, key: &str) -> Option<&RecoverRow> {
        self.index.get(key).map(|&i| &self.rows[i])
    }

    pub fn row_mut(&mut self, key: &str) -> Option<&mut RecoverRow> {
        self.index.get(key).map(|&i| &mut self.rows[i])
    }

    pub fn rows(&self) -> &[RecoverRow] {
        &self.rows
    }

    /// Take a new list of candidates' rows. A row already here keeps its progress (title, pick,
    /// approval, status), and only gains a title it lacked. Rows no longer listed go, unless a step
    /// is running, which may still be working on them: then they stay, after the rest.
    pub fn refresh(&mut self, fresh: Vec<RecoverRow>) {
        let mut old: HashMap<String, RecoverRow> =
            std::mem::take(&mut self.rows).into_iter().map(|r| (r.key.clone(), r)).collect();
        let mut rows: Vec<RecoverRow> = Vec::with_capacity(fresh.len());
        let mut seen: HashSet<String> = HashSet::new();
        for f in fresh {
            if !seen.insert(f.key.clone()) {
                continue;
            }
            let row = match old.remove(&f.key) {
                Some(mut kept) => {
                    if kept.title.is_none() && f.title.is_some() {
                        kept.title = f.title;
                        kept.title_source = f.title_source;
                        if kept.artists.is_none() {
                            kept.artists = f.artists;
                        }
                    }
                    kept
                }
                None => f,
            };
            rows.push(row);
        }
        if self.busy() {
            let mut left: Vec<RecoverRow> = old.into_values().collect();
            left.sort_by(|a, b| a.key.cmp(&b.key));
            rows.extend(left);
        }
        self.index = rows.iter().enumerate().map(|(i, r)| (r.key.clone(), i)).collect();
        self.rows = rows;
    }
}

// ponytail: one assistant per process, in a static as the import's `IMPORT`. There is one window
// and one assistant page; a second step while one runs is refused (`busy`), not queued.
static SESSION: Mutex<Option<Session>> = Mutex::new(None);

/// Run `f` on the session, opening an empty one first if there is none.
pub fn with_session<R>(f: impl FnOnce(&mut Session) -> R) -> R {
    let mut g = SESSION.lock().unwrap();
    f(g.get_or_insert_with(Session::default))
}

/// Forget the session: rows, picks and progress. The generation goes on counting, so a step still
/// running for the old session can tell it has none left.
pub fn reset_session() {
    let mut g = SESSION.lock().unwrap();
    let gen = g.as_ref().map_or(0, |s| s.gen) + 1;
    *g = Some(Session { gen, ..Session::default() });
}

/// The session's rows: these keys (in this order, unknown ones skipped), or all of them.
pub fn rows(keys: Option<&[String]>) -> Vec<RecoverRow> {
    with_session(|s| match keys {
        Some(keys) => keys.iter().filter_map(|k| s.row(k).cloned()).collect(),
        None => s.rows().to_vec(),
    })
}

/// Where the session is.
pub fn snapshot() -> RecoverSnapshot {
    with_session(|s| s.snapshot.clone())
}

/// `recover_candidates`, blocking (the database only): the candidates with their titles filled
/// from what is stored locally (then from an earlier lookup's cache), and the session refreshed
/// with a row for each.
pub fn load_candidates(db: &Db, include_dismissed: bool) -> Vec<RecoverCandidate> {
    let latest = db.latest_snapshots();
    let alerts = db.alert_rows(true);
    let memberships = db.playlist_memberships();
    let unavailable_index = db.unavailable_index_rows();
    let stored = Stored {
        latest: &latest,
        alerts: &alerts,
        memberships: &memberships,
        unavailable_index: &unavailable_index,
    };
    let mut out =
        collect_candidates(&stored, include_dismissed, |pid, at| db.snapshot_before(pid, at));

    let wanted: HashSet<String> =
        out.iter().filter(|c| c.title.is_none()).map(|c| c.video_id.clone()).collect();
    if !wanted.is_empty() {
        let ids: Vec<String> = wanted.iter().cloned().collect();
        let found = best_local_titles(&wanted, &db.local_title_rows(&ids));
        let mut cached: HashMap<String, Option<(String, Option<String>, String)>> = HashMap::new();
        for c in out.iter_mut().filter(|c| c.title.is_none()) {
            if let Some((title, artists, source)) = found.get(&c.video_id) {
                c.title = Some(title.clone());
                c.title_source = Some((*source).to_owned());
                if c.artists.is_none() {
                    c.artists = non_empty(artists);
                }
                continue;
            }
            // A title an earlier lookup found (Wayback, or typed in).
            let hit = cached.entry(c.video_id.clone()).or_insert_with(|| {
                let t = db.recover_title_get(&c.video_id).filter(|t| t.found)?;
                let title = real_title(t.title.as_deref()?)?;
                Some((title, t.artists, t.source))
            });
            if let Some((title, artists, source)) = hit.clone() {
                c.title = Some(title);
                c.title_source = Some(source);
                if c.artists.is_none() {
                    c.artists = artists.as_deref().and_then(non_empty);
                }
            }
        }
    }

    let fresh: Vec<RecoverRow> = out.iter().map(RecoverRow::of).collect();
    with_session(|s| s.refresh(fresh));
    out
}

// --- a dead track as the search's input -----------------------------------------------------------

/// A music video's dressing that says nothing about the recording: "(Official Video)", "[Lyrics]",
/// "(Video Oficial)" and the like, in parentheses or brackets, any case.
static VIDEO_DRESSING: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(concat!(
        r"(?i)\s*[(\[]\s*(?:",
        r"official\s+(?:music\s+|lyric\s+)?video|official\s+audio|lyric\s+video|lyrics",
        r"|video\s+oficial|audio\s+oficial|letra",
        r")\s*[)\]]",
    ))
    .unwrap()
});

/// A Topic channel (`Artist - Topic`) is the artist.
fn topicless(artist: &str) -> String {
    let a = artist.trim();
    a.strip_suffix(" - Topic").unwrap_or(a).trim().to_owned()
}

/// "Left - Right" at the first spaced dash (hyphen, en or em dash).
fn split_dash(s: &str) -> Option<(&str, &str)> {
    [" - ", " \u{2013} ", " \u{2014} "]
        .iter()
        .filter_map(|sep| s.find(sep).map(|i| (i, sep.len())))
        .min_by_key(|&(i, _)| i)
        .map(|(i, n)| (&s[..i], &s[i + n..]))
}

/// What the importer's search and scoring take, from what a dead track was called. A video's title
/// is often "Artist - Song": with no artist known, the left side becomes the artist and the right
/// the title; with the artist known and on the left, it is dropped from the title. The video
/// dressing ([`VIDEO_DRESSING`]) goes, and so does a Topic channel's " - Topic".
pub(crate) fn source_from_dead(
    title: &str,
    artists: Option<&str>,
    duration: Option<&str>,
) -> SourceTrack {
    let words = |s: &str| s.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut title = match words(&*VIDEO_DRESSING.replace_all(title, " ")) {
        // All dressing ("(Official Video)"): the dressing is the title.
        t if t.is_empty() => words(title),
        t => t,
    };
    let mut artists: Vec<String> =
        artists.unwrap_or("").split(',').map(topicless).filter(|a| !a.is_empty()).collect();
    if let Some((left, right)) = split_dash(&title) {
        let (left, right) = (topicless(left), right.trim().to_owned());
        if !left.is_empty() && !right.is_empty() {
            if artists.is_empty() {
                artists.push(left);
                title = right;
            } else if artists.iter().any(|a| a.to_lowercase() == left.to_lowercase()) {
                title = right;
            }
        }
    }
    SourceTrack {
        title,
        artists,
        duration_ms: duration.and_then(import::secs).map(|s| (s * 1000.0) as u64),
        ..Default::default()
    }
}

// --- the background steps -------------------------------------------------------------------------

/// The key's videoId (its part after U+001F).
fn video_of(key: &str) -> &str {
    key.split_once('\u{1f}').map_or(key, |(_, v)| v)
}

/// Run `f` on the session if `gen` is still its step and the step is still `phase` (not
/// cancelled, finished, reset or replaced).
fn with_step<R>(gen: u64, phase: &str, f: impl FnOnce(&mut Session) -> R) -> Option<R> {
    with_session(|s| (s.gen == gen && s.snapshot.phase == phase).then(|| f(s)))
}

fn alive(gen: u64, phase: &str) -> bool {
    with_step(gen, phase, |_| ()).is_some()
}

/// Tell the UI where the session is.
fn emit(state: &AppState) {
    let _ = state.app.emit("recover-progress", snapshot());
}

/// [`emit`], at most four times a second: a run of cache hits would otherwise flood the UI.
fn emit_paced(state: &AppState, last: &mut Option<Instant>) {
    if last.is_some_and(|t| t.elapsed() < Duration::from_millis(250)) {
        return;
    }
    *last = Some(Instant::now());
    emit(state);
}

/// End step `step` of generation `gen` as `phase` (`done`, `failed`).
fn finish(state: &AppState, gen: u64, step: &str, phase: &'static str, message: Option<String>) {
    let ended = with_step(gen, step, |s| {
        s.snapshot.phase = phase;
        s.snapshot.current = None;
        s.snapshot.waiting_until = None;
        s.snapshot.message = message;
    });
    if ended.is_some() {
        emit(state);
    }
}

fn set_waiting(state: &AppState, gen: u64, step: &str, until: Option<i64>) {
    let changed =
        with_step(gen, step, |s| std::mem::replace(&mut s.snapshot.waiting_until, until) != until);
    if changed == Some(true) {
        emit(state);
    }
}

/// Sleep `dur`, waking twice a second to see whether the step was stopped. False when it was.
async fn nap(gen: u64, step: &'static str, dur: Duration) -> bool {
    let end = tokio::time::Instant::now() + dur;
    loop {
        if !alive(gen, step) {
            return false;
        }
        let now = tokio::time::Instant::now();
        if now >= end {
            return true;
        }
        tokio::time::sleep((end - now).min(Duration::from_millis(500))).await;
    }
}

/// Resolves once the step is stopped: raced against a request so a cancel does not wait it out.
async fn stopped(gen: u64, step: &'static str) {
    while alive(gen, step) {
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

fn between((lo, hi): (u64, u64)) -> Duration {
    Duration::from_millis(lo + rand::random::<u64>() % (hi - lo))
}

/// Open step `phase` over `keys` (in their order, once each, unknown ones skipped) that `wanted`
/// accepts. Refused with `busy` while another step runs. Answers the generation, the keys and the
/// snapshot the UI starts from.
fn begin(
    phase: &'static str,
    keys: &[String],
    wanted: impl Fn(&RecoverRow) -> bool,
) -> Result<(u64, Vec<String>, RecoverSnapshot), String> {
    with_session(|s| {
        if s.busy() {
            return Err("busy".to_owned());
        }
        let mut seen = HashSet::new();
        let todo: Vec<String> = keys
            .iter()
            .filter(|k| seen.insert(k.as_str()))
            .filter(|k| s.row(k).is_some_and(&wanted))
            .cloned()
            .collect();
        s.gen += 1;
        s.snapshot = RecoverSnapshot { phase, total: todo.len(), ..RecoverSnapshot::default() };
        Ok((s.gen, todo, s.snapshot.clone()))
    })
}

/// Between two videos asked of the Wayback Machine, never in lockstep.
const WAYBACK_GAP_MS: (u64, u64) = (2_000, 4_000);
/// The first backoff after the Archive says to slow down (429/503); doubled on the next.
const WAYBACK_BACKOFF: Duration = Duration::from_secs(15);
/// Slow-downs in a row that make the lookup stop for [`WAYBACK_PAUSE_SECS`].
const WAYBACK_STRIKES: u32 = 3;
const WAYBACK_PAUSE_SECS: i64 = 600;
/// Network failures in a row that end a step: the network is down, not the one video.
const FAILURES: u32 = 3;

/// Look up a title for each of `keys` that has none: the `recover_titles` cache, then, with
/// `wayback`, the Wayback Machine. In the background; follow it through `recover-progress`.
pub fn start_titles(
    state: &Arc<AppState>,
    keys: Vec<String>,
    wayback: bool,
) -> Result<RecoverSnapshot, String> {
    let (gen, todo, snapshot) = begin("titles", &keys, |r| r.title.is_none())?;
    tauri::async_runtime::spawn(run_titles(Arc::clone(state), gen, todo, wayback));
    Ok(snapshot)
}

/// Put a found title on a row that still has none.
fn give_title(s: &mut Session, key: &str, title: String, artists: Option<String>, source: &str) {
    if let Some(row) = s.row_mut(key).filter(|r| r.title.is_none()) {
        row.title = Some(title);
        row.title_source = Some(source.to_owned());
        if row.artists.is_none() {
            row.artists = artists;
        }
    }
}

async fn run_titles(state: Arc<AppState>, gen: u64, todo: Vec<String>, wayback: bool) {
    const STEP: &str = "titles";
    // The cache first, so what is known fills in at once.
    let mut network: Vec<String> = Vec::new();
    for key in todo {
        let vid = video_of(&key).to_owned();
        let hit = state.db.recover_title_get(&vid);
        let ask = hit.is_none() && wayback && wayback::valid_id(&vid);
        let ticked = with_step(gen, STEP, |s| {
            if let Some(t) = hit.filter(|t| t.found) {
                if let Some(title) = t.title.as_deref().and_then(real_title) {
                    let artists = t.artists.as_deref().and_then(non_empty);
                    give_title(s, &key, title, artists, &t.source);
                }
            }
            if !ask {
                s.snapshot.done += 1;
            }
        });
        if ticked.is_none() {
            return;
        }
        if ask {
            network.push(key);
        }
    }
    emit(&state);

    let mut strikes = 0u32;
    let mut failures = 0u32;
    let mut first = true;
    let mut i = 0;
    while let Some(key) = network.get(i) {
        let vid = video_of(key).to_owned();
        if !first && !nap(gen, STEP, between(WAYBACK_GAP_MS)).await {
            return;
        }
        first = false;
        if with_step(gen, STEP, |s| s.snapshot.current = Some(key.clone())).is_none() {
            return;
        }
        emit(&state);
        let got = tokio::select! {
            got = wayback::fetch_title(&vid) => got,
            _ = stopped(gen, STEP) => return,
        };
        match got {
            Err(WaybackError::RateLimited) => {
                strikes += 1;
                if strikes >= WAYBACK_STRIKES {
                    strikes = 0;
                    tracing::warn!("recover: the Wayback Machine keeps refusing, pausing");
                    let until = now_secs() + WAYBACK_PAUSE_SECS;
                    let paused = with_step(gen, STEP, |s| {
                        s.snapshot.message = Some("wayback_rate_limited".to_owned());
                        s.snapshot.waiting_until = Some(until);
                    });
                    if paused.is_none() {
                        return;
                    }
                    emit(&state);
                    if !nap(gen, STEP, Duration::from_secs(WAYBACK_PAUSE_SECS as u64)).await {
                        return;
                    }
                    let resumed = with_step(gen, STEP, |s| {
                        s.snapshot.message = None;
                        s.snapshot.waiting_until = None;
                    });
                    if resumed.is_none() {
                        return;
                    }
                    emit(&state);
                } else if !nap(gen, STEP, WAYBACK_BACKOFF * 2u32.pow(strikes - 1)).await {
                    return;
                }
                // The same video again.
                continue;
            }
            Ok(found) => {
                strikes = 0;
                failures = 0;
                let title = found.as_deref().and_then(real_title);
                // Positive or negative, a verdict is kept: a negative is asked again in 30 days.
                state.db.recover_title_put(
                    &vid,
                    &CachedTitle {
                        title: title.clone(),
                        artists: None,
                        source: "wayback".to_owned(),
                        found: title.is_some(),
                        checked_at: now_secs(),
                    },
                );
                let ticked = with_step(gen, STEP, |s| {
                    if let Some(title) = title {
                        give_title(s, key, title, None, "wayback");
                    }
                    s.snapshot.done += 1;
                });
                if ticked.is_none() {
                    return;
                }
            }
            Err(e) => {
                // Not a verdict about the video: nothing is cached, a later run asks again.
                strikes = 0;
                failures += 1;
                tracing::warn!(error = %e, "recover: Wayback lookup failed");
                if failures >= FAILURES {
                    return finish(&state, gen, STEP, "failed", Some(e.to_string()));
                }
                if with_step(gen, STEP, |s| s.snapshot.done += 1).is_none() {
                    return;
                }
            }
        }
        i += 1;
        emit(&state);
    }
    finish(&state, gen, STEP, "done", None);
}

/// Search a replacement for each of `keys` that has a title and was not searched yet (a stopped
/// search resumes there; the cache answers what was asked before). Refused while the YouTube
/// cooldown runs (`cooldown:<until>`) or another step does (`busy`). In the background; follow it
/// through `recover-progress`.
pub fn start_search(state: &Arc<AppState>, keys: Vec<String>) -> Result<RecoverSnapshot, String> {
    if let Some(until) = import::youtube_cooldown(state) {
        return Err(Halt::Cooldown(until).code());
    }
    let (gen, todo, snapshot) =
        begin("searching", &keys, |r| r.title.is_some() && r.tier == Tier::Pending)?;
    tauri::async_runtime::spawn(run_search(Arc::clone(state), gen, todo));
    Ok(snapshot)
}

/// The `import_matches` key of a dead video's search.
fn cache_key(video_id: &str) -> String {
    format!("recover:{video_id}")
}

async fn run_search(state: Arc<AppState>, gen: u64, todo: Vec<String>) {
    const STEP: &str = "searching";
    let cancelled = || !alive(gen, STEP);
    let waiting = |until: Option<i64>| set_waiting(&state, gen, STEP, until);
    let gate = Gate { cancelled: &cancelled, waiting: &waiting };
    let mut failures = 0u32;
    let mut last_emit: Option<Instant> = None;
    for key in todo {
        let vid = video_of(&key).to_owned();
        // Read now, not when the step began: a title typed in meanwhile is the one searched.
        let Some(row) = with_step(gen, STEP, |s| {
            s.snapshot.current = Some(key.clone());
            s.row(&key).cloned()
        }) else {
            return;
        };
        let Some(row) = row.filter(|r| r.tier == Tier::Pending && r.title.is_some()) else {
            // Picked by hand, or the title cleared, since the step began.
            if with_step(gen, STEP, |s| s.snapshot.done += 1).is_none() {
                return;
            }
            continue;
        };
        let title = row.title.as_deref().unwrap_or_default();
        let src = source_from_dead(title, row.artists.as_deref(), row.duration.as_deref());
        // A title typed in may not be what the cached answer was searched with.
        let manual = row.title_source.as_deref() == Some("manual");
        let hit = (!manual).then(|| import::cached(&state, &cache_key(&vid))).flatten();
        let answer = match hit {
            Some(answer) => answer,
            None => {
                // A search is seconds apart from the last: always worth showing which one is out.
                last_emit = Some(Instant::now());
                emit(&state);
                match import::search(&state, &gate, &src).await {
                    Ok(mut ranked) => {
                        failures = 0;
                        // Never the dead video itself, nor anything as dead.
                        ranked.retain(|(_, c)| c.video_id != vid && !c.unavailable);
                        let answer = import::classify(ranked);
                        import::remember(&state, &cache_key(&vid), &answer, false);
                        answer
                    }
                    Err(Halt::Failed(e)) => {
                        tracing::warn!(error = %e, "recover: search failed");
                        failures += 1;
                        if failures >= FAILURES {
                            return finish(&state, gen, STEP, "failed", Some(e));
                        }
                        // Not remembered and left pending: the next search asks again.
                        let ticked = with_step(gen, STEP, |s| {
                            if let Some(r) = s.row_mut(&key) {
                                r.error = Some(e);
                            }
                            s.snapshot.done += 1;
                        });
                        if ticked.is_none() {
                            return;
                        }
                        emit_paced(&state, &mut last_emit);
                        continue;
                    }
                    Err(Halt::Cancelled) => return,
                    Err(h) => return finish(&state, gen, STEP, "failed", Some(h.code())),
                }
            }
        };
        let (tier, pick, candidates) = answer;
        let ticked = with_step(gen, STEP, |s| {
            // A pick made by hand while this one was out stays.
            if let Some(r) = s.row_mut(&key).filter(|r| r.tier == Tier::Pending) {
                r.tier = tier;
                r.pick = pick;
                r.candidates = candidates;
                r.approved = false;
                r.error = None;
            }
            s.snapshot.done += 1;
        });
        if ticked.is_none() {
            return;
        }
        emit_paced(&state, &mut last_emit);
    }
    finish(&state, gen, STEP, "done", None);
}

// --- the user's calls on a row --------------------------------------------------------------------

/// The user's replacement for a row (`None`: none, leave it out) and whether it is approved. A song
/// other than the proposed one is the user's pick: matched, and remembered for the next search of
/// that video as the import remembers its picks.
pub fn pick(
    state: &AppState,
    key: &str,
    song: Option<SongItem>,
    approved: bool,
) -> Result<RecoverRow, String> {
    let (row, manual) = with_session(|s| {
        let row = s.row_mut(key).ok_or_else(|| "gone".to_owned())?;
        let manual = match &song {
            Some(p) => {
                let changed = row.pick.as_ref().is_none_or(|c| c.video_id != p.video_id);
                if changed {
                    row.tier = Tier::Matched;
                }
                changed
            }
            None => {
                row.tier = Tier::Missing;
                false
            }
        };
        row.pick = song;
        row.approved = approved && row.pick.is_some();
        row.error = None;
        Ok::<_, String>((row.clone(), manual))
    })?;
    if manual {
        let answer = (Tier::Matched, row.pick.clone(), row.candidates.clone());
        import::remember(state, &cache_key(video_of(key)), &answer, true);
    }
    Ok(row)
}

/// A title typed in for a row: its search starts over (pending, no pick), and the title is kept in
/// `recover_titles` for the next session. A blank (or placeholder) title clears it.
pub fn set_title(
    state: &AppState,
    key: &str,
    title: &str,
    artists: Option<&str>,
) -> Result<RecoverRow, String> {
    let title = real_title(title);
    let artists = artists.and_then(non_empty);
    let row = with_session(|s| {
        let row = s.row_mut(key).ok_or_else(|| "gone".to_owned())?;
        row.title_source = title.as_ref().map(|_| "manual".to_owned());
        row.title = title.clone();
        row.artists = artists.clone();
        row.tier = Tier::Pending;
        row.pick = None;
        row.candidates = Vec::new();
        row.approved = false;
        row.error = None;
        Ok::<_, String>(row.clone())
    })?;
    if let Some(title) = title {
        state.db.recover_title_put(
            video_of(key),
            &CachedTitle {
                title: Some(title),
                artists,
                source: "manual".to_owned(),
                found: true,
                checked_at: now_secs(),
            },
        );
    }
    Ok(row)
}

/// Stop the running step. What it did stays done.
pub fn cancel(state: &AppState) {
    let was_running = with_session(|s| {
        if !s.busy() {
            return false;
        }
        s.snapshot.phase = "cancelled";
        s.snapshot.current = None;
        s.snapshot.waiting_until = None;
        s.snapshot.message = None;
        true
    });
    if was_running {
        emit(state);
    }
}

/// Forget the session (stopping any step) and tell the UI.
pub fn reset(state: &AppState) {
    reset_session();
    emit(state);
}

// --- applying -------------------------------------------------------------------------------------

/// What "Apply" does with a replacement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// Where the dead track is (or was), and the dead one out once the new one is in.
    Replace,
    /// At the end; the dead track stays.
    Append,
}

impl Action {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "replace" => Some(Action::Replace),
            "append" => Some(Action::Append),
            _ => None,
        }
    }
}

/// One approved replacement in one playlist, as [`plan_replacements`] takes it.
#[derive(Debug, Clone, PartialEq)]
pub struct Wanted {
    /// The session row's key.
    pub key: String,
    /// The dead track's videoId.
    pub dead: String,
    /// The dead row's handle as last seen. Only matched against the fresh read.
    pub dead_handle: Option<String>,
    /// Listed when last seen: a dead row whose handle changed is then found by its videoId.
    pub in_playlist: bool,
    /// The handle of the row after it, as last seen: where it goes when the dead row is gone.
    pub next_handle: Option<String>,
    /// The replacement.
    pub song: SongItem,
    pub action: Action,
}

/// A replacement to add.
#[derive(Debug, Clone, PartialEq)]
pub struct Add {
    pub key: String,
    /// With no handle.
    pub song: SongItem,
}

/// One playlist's edit, worked out from a fresh read.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Plan {
    /// The replacements to add, in the order wanted.
    pub adds: Vec<Add>,
    /// Key → the handle of the fresh row its replacement goes in front of. A key not here goes
    /// at the end (where the add puts it).
    pub anchors: HashMap<String, String>,
    /// Key → its dead row (fresh, with its handle): taken out only once that key's replacement is
    /// in, or already was (invariant 1).
    pub dead_rows: Vec<(String, SongItem)>,
    /// Keys whose replacement the playlist already holds (or an earlier key of this plan adds):
    /// nothing added, the dead row still goes (D2).
    pub already_present: Vec<String>,
}

/// The dead row of `w` in `fresh`, unless already `claimed`: the row with its handle, or, when it
/// was listed and its handle changed, its first copy that is still dead. Never a row that plays:
/// a video back under a new handle is not dead any more.
fn dead_row<'a>(
    fresh: &'a [SongItem],
    w: &Wanted,
    claimed: &HashSet<&str>,
) -> Option<&'a SongItem> {
    let free = |r: &&'a SongItem| {
        r.video_id == w.dead && handle(r).is_some_and(|h| !claimed.contains(h))
    };
    if let Some(h) = w.dead_handle.as_deref() {
        if let Some(r) = fresh.iter().filter(free).find(|r| handle(r) == Some(h)) {
            return Some(r);
        }
    }
    if !w.in_playlist {
        return None;
    }
    fresh.iter().filter(free).find(|r| r.unavailable)
}

/// One playlist's edit, from its fresh rows and the replacements wanted in it:
/// - a replacement the playlist already holds (or an earlier one here adds) is not added again,
///   and is `already_present`;
/// - with `replace`, the new row goes in front of the dead row when that is still there, else in
///   front of the row that followed it when that one is, else at the end; and the dead row goes;
/// - with `append`, at the end, and nothing goes;
/// - a replacement that is the dead video itself never takes it out.
pub fn plan_replacements(fresh: &[SongItem], wanted: &[Wanted]) -> Plan {
    let present: HashSet<&str> = fresh.iter().map(|r| r.video_id.as_str()).collect();
    let handles: HashSet<&str> = fresh.iter().filter_map(handle).collect();
    let mut adding: HashSet<&str> = HashSet::new();
    let mut claimed: HashSet<&str> = HashSet::new();
    let mut plan = Plan::default();
    for w in wanted {
        let new = w.song.video_id.as_str();
        let replace = w.action == Action::Replace && new != w.dead;
        let dead = if replace { dead_row(fresh, w, &claimed) } else { None };
        let dead_handle = dead.and_then(handle);
        if let Some(h) = dead_handle {
            claimed.insert(h);
        }
        if present.contains(new) || adding.contains(new) {
            plan.already_present.push(w.key.clone());
        } else {
            adding.insert(new);
            plan.adds.push(Add {
                key: w.key.clone(),
                song: SongItem { set_video_id: None, ..w.song.clone() },
            });
            let next = w.next_handle.as_deref().filter(|h| handles.contains(*h));
            if let Some(anchor) = dead_handle.or(next).filter(|_| replace) {
                plan.anchors.insert(w.key.clone(), anchor.to_owned());
            }
        }
        if let Some(row) = dead {
            plan.dead_rows.push((w.key.clone(), row.clone()));
        }
    }
    plan
}

/// The rows of `list` whose handle is in `gone`, each with the next row that stays: where an undo
/// puts it back (as `everywhere`'s).
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

/// The Data API inserts of a plan, in the order they must run: each at the fresh position of its
/// anchor, highest first so no insert shifts one still to come; two at the same position go in
/// reverse, so they end up in the order wanted. The ones with no anchor (the end) last.
pub fn insert_order<'a>(fresh: &[SongItem], plan: &'a Plan) -> Vec<(&'a Add, Option<usize>)> {
    let at = |a: &Add| {
        let anchor = plan.anchors.get(&a.key)?;
        fresh.iter().position(|r| handle(r) == Some(anchor.as_str()))
    };
    let mut out: Vec<(&Add, Option<usize>)> = plan.adds.iter().rev().map(|a| (a, at(a))).collect();
    // Stable: ties keep the reversed order. `None` sorts below any position, so last.
    out.sort_by_key(|&(_, p)| std::cmp::Reverse(p));
    out
}

/// What [`apply`] did in one playlist.
#[derive(Debug, Serialize)]
pub struct AppliedPlaylist {
    pub playlist_id: String,
    /// Its journal entries: one on InnerTube, up to two (inserts, removals) on the Data API.
    pub ops: Vec<OpRecord>,
    /// Rows recovered (`done` or `already_present`).
    pub done: usize,
    /// `(key, error)` of the rows that were not.
    pub failed: Vec<(String, String)>,
}

#[derive(Debug, Serialize)]
pub struct RecoverApplied {
    pub playlists: Vec<AppliedPlaylist>,
}

/// One playlist's run: the journal entries, each row's `(key, status, error)`, and the error that
/// must stop the rest (YouTube's cooldown).
#[derive(Default)]
struct Outcome {
    ops: Vec<OpRecord>,
    rows: Vec<(String, &'static str, Option<String>)>,
    halt: Option<String>,
}

fn stops(e: &str) -> bool {
    e.starts_with("cooldown:")
}

impl Outcome {
    /// Nothing written: every row failed with `e`.
    fn failed(wanted: &[Wanted], e: String) -> Self {
        Outcome {
            ops: Vec::new(),
            rows: wanted.iter().map(|w| (w.key.clone(), "failed", Some(e.clone()))).collect(),
            halt: stops(&e).then_some(e),
        }
    }

    /// Each row's status once written: `errors` first, then whether its replacement is in
    /// (`present`), and whether this run added it (`added`) or it was there.
    fn settle(
        &mut self,
        wanted: &[Wanted],
        errors: &HashMap<String, String>,
        present: &HashSet<String>,
        added: &HashSet<String>,
    ) {
        for w in wanted {
            let (status, error) = if let Some(e) = errors.get(&w.key) {
                ("failed", Some(e.clone()))
            } else if !present.contains(&w.song.video_id) {
                ("failed", Some("refused".to_owned()))
            } else if added.contains(&w.key) {
                ("done", None)
            } else {
                ("already_present", None)
            };
            self.rows.push((w.key.clone(), status, error));
        }
        if self.halt.is_none() {
            self.halt = errors.values().find(|e| stops(e)).cloned();
        }
    }
}

/// The dead rows to take out: those whose replacement is in and that nothing went wrong for.
fn confirmed_dead(
    plan: &Plan,
    wanted: &[Wanted],
    errors: &HashMap<String, String>,
    present: &HashSet<String>,
) -> Vec<(String, SongItem)> {
    let song_of: HashMap<&str, &str> =
        wanted.iter().map(|w| (w.key.as_str(), w.song.video_id.as_str())).collect();
    plan.dead_rows
        .iter()
        .filter(|(k, _)| !errors.contains_key(k))
        .filter(|(k, _)| song_of.get(k.as_str()).is_some_and(|v| present.contains(*v)))
        .cloned()
        .collect()
}

/// InnerTube, directly (D1), as `everywhere::keep_only_in`: read fresh, add, move each new row in
/// front of its anchor, take out the dead rows whose replacement is in, and journal it all as one
/// `recover` entry (undo: the new rows out, the dead ones back where they were).
async fn apply_direct(state: &Arc<AppState>, pid: &str, title: &str, wanted: &[Wanted]) -> Outcome {
    // A playlist is done whole once started: a cancel waits for the next one.
    let none = &|| false;
    let fresh = match rows::read_all(state, pid).await {
        Ok(l) => l,
        Err(e) => return Outcome::failed(wanted, e),
    };
    let plan = plan_replacements(&fresh, wanted);
    let songs: Vec<SongItem> = plan.adds.iter().map(|a| a.song.clone()).collect();
    let added = if songs.is_empty() {
        rows::Added::default()
    } else {
        match rows::add_rows(state, pid, &songs, false, none).await {
            Ok(a) => a,
            Err(e) => return Outcome::failed(wanted, e),
        }
    };
    let mut present: HashSet<String> = fresh.iter().map(|r| r.video_id.clone()).collect();
    present.extend(added.rows.iter().chain(&added.duplicates).map(|r| r.video_id.clone()));
    let new_vids: HashSet<&str> = added.rows.iter().map(|r| r.video_id.as_str()).collect();
    let added_keys: HashSet<String> = plan
        .adds
        .iter()
        .filter(|a| new_vids.contains(a.song.video_id.as_str()))
        .map(|a| a.key.clone())
        .collect();
    let mut errors: HashMap<String, String> = HashMap::new();

    // Into place. A new row that came back with no handle stays at the end.
    let new_handle: HashMap<&str, &str> =
        added.rows.iter().filter_map(|r| Some((r.video_id.as_str(), handle(r)?))).collect();
    let moves: Vec<(String, Move)> = plan
        .adds
        .iter()
        .filter_map(|a| {
            let before = plan.anchors.get(&a.key)?;
            let row = new_handle.get(a.song.video_id.as_str())?;
            Some((a.key.clone(), Move { row: (*row).to_owned(), before: Some(before.clone()) }))
        })
        .collect();
    if !moves.is_empty() {
        let only: Vec<Move> = moves.iter().map(|(_, m)| m.clone()).collect();
        if let Err(e) = rows::move_rows(state, pid, &only, none).await {
            // In, but not in place: its dead row stays, so nothing is lost and nothing moves.
            for (k, _) in &moves {
                errors.insert(k.clone(), e.clone());
            }
        }
    }

    // Out with the dead whose replacement is in (invariant 1).
    let out_rows = confirmed_dead(&plan, wanted, &errors, &present);
    let gone: HashSet<String> =
        out_rows.iter().filter_map(|(_, r)| handle(r)).map(str::to_owned).collect();
    let back = restore_rows(&fresh, &gone);
    let mut removed = false;
    if !back.is_empty() {
        let dead: Vec<SongItem> = back.iter().map(|r| r.song.clone()).collect();
        match rows::remove_rows(state, pid, &dead, none).await {
            Ok(()) => removed = true,
            Err(e) => {
                for (k, _) in &out_rows {
                    errors.insert(k.clone(), e.clone());
                }
            }
        }
    }

    let mut out = Outcome::default();
    out.settle(wanted, &errors, &present, &added_keys);
    // The new rows out first, so the dead ones go back in front of rows that are still there.
    let mut undo: Vec<Step> = Vec::new();
    let new_rows: Vec<SongItem> =
        added.rows.iter().filter(|r| handle(r).is_some()).cloned().collect();
    if !new_rows.is_empty() {
        undo.push(Step::Remove { playlist_id: pid.to_owned(), rows: new_rows });
    }
    if removed {
        undo.push(journal::restore_step(pid, &back));
    }
    if !undo.is_empty() {
        journal::announce(state, &[pid.to_owned()]);
        let count = out.rows.iter().filter(|(_, s, _)| *s != "failed").count();
        let named = Named { id: pid.to_owned(), title: title.to_owned() };
        let summary = Summary { playlists: vec![named], count };
        out.ops.extend(journal::record(state, "recover", &summary, &undo));
    }
    out
}

/// The Data API (D3), queued: first a job of inserts at their anchors' positions (highest first),
/// then, for the dead rows whose replacement landed (or was there), a removal job
/// (`dedup::removal_job`). Each job is its own journal entry.
async fn apply_queued(
    state: &Arc<AppState>,
    jobs: &JobsState,
    q: &QueueTarget,
    pid: &str,
    title: &str,
    wanted: &[Wanted],
) -> Outcome {
    let Some(dest) = planner::ytdata_playlist_id(pid) else {
        return Outcome::failed(wanted, "not_addressable".to_owned());
    };
    let fresh = match rows::read_all(state, pid).await {
        Ok(l) => l,
        Err(e) => return Outcome::failed(wanted, e),
    };
    let plan = plan_replacements(&fresh, wanted);
    let named = Named { id: pid.to_owned(), title: title.to_owned() };
    let mut out = Outcome::default();
    let mut present: HashSet<String> = fresh.iter().map(|r| r.video_id.clone()).collect();
    let mut added_keys: HashSet<String> = HashSet::new();
    let mut errors: HashMap<String, String> = HashMap::new();
    let key_of_new = |v: &str| -> Vec<String> {
        plan.adds.iter().filter(|a| a.song.video_id == v).map(|a| a.key.clone()).collect()
    };

    // 1. The inserts.
    let items: Vec<NewJobItem> = insert_order(&fresh, &plan)
        .into_iter()
        .map(|(a, position)| NewJobItem {
            phase: 1,
            action: action_name::PLAYLIST_ITEM_INSERT.to_string(),
            params: json!({
                "playlist_id": dest,
                "video_id": a.song.video_id,
                "position": position,
                "occurrence": 0,
            }),
        })
        .collect();
    if !items.is_empty() {
        let n = items.len();
        let summary = Summary { playlists: vec![named.clone()], count: n };
        let new = NewJob {
            account_id: q.channel_id.clone(),
            kind: JobKind::CopyItems,
            params: json!({
                ENGINE_KEY: q.engine.as_str(),
                "account": q.account,
                "op_kind": "recover",
                "summary": summary,
                "target_id": pid,
                "dest_playlist_id": dest,
                "duplicates": "skip",
            }),
            priority: q.priority,
            total_phases: 1,
            est_units_total: planner::estimate_transfer_units(n, false),
            items,
        };
        match crate::commands::enqueue_and_wait(state, jobs, &new).await {
            Err(e) => return Outcome::failed(wanted, e),
            // Still waiting its turn: nothing is known in yet, so nothing goes. A later apply
            // finds the replacement there and takes the dead row out then.
            Ok(None) => {
                for a in &plan.adds {
                    errors.insert(a.key.clone(), "queued".to_owned());
                }
            }
            Ok(Some(job)) => {
                let done = crate::jobs::repo::list_job_items(&state.db, job.id).unwrap_or_default();
                for item in done.iter().filter(|i| i.action == action_name::PLAYLIST_ITEM_INSERT) {
                    let Some(v) = item.params.get("video_id").and_then(Value::as_str) else {
                        continue;
                    };
                    if item.status == JobItemStatus::Done {
                        present.insert(v.to_owned());
                        added_keys.extend(key_of_new(v));
                    } else {
                        let e = item.last_error.clone().unwrap_or_else(|| "refused".to_owned());
                        for k in key_of_new(v) {
                            errors.insert(k, e.clone());
                        }
                    }
                }
                out.ops.extend(crate::commands::job_op(state, Some(&job)).await);
            }
        }
    }

    // 2. The dead rows whose replacement is in (invariant 1).
    let out_rows = confirmed_dead(&plan, wanted, &errors, &present);
    let gone: HashSet<String> =
        out_rows.iter().filter_map(|(_, r)| handle(r)).map(str::to_owned).collect();
    let back = restore_rows(&fresh, &gone);
    let picked: Vec<SongItem> = back.iter().map(|r| r.song.clone()).collect();
    let occurrences = planner::occurrences(&fresh, &picked);
    if let Some(new) = dedup::removal_job(&named, &back, "recover", &occurrences, q) {
        let dead_key = |v: &str| key_of(pid, v);
        match crate::commands::enqueue_and_wait(state, jobs, &new).await {
            Err(e) => {
                for (k, _) in &out_rows {
                    errors.insert(k.clone(), e.clone());
                }
            }
            // Waiting its turn: the replacement is in and the removal will follow.
            Ok(None) => {}
            Ok(Some(job)) => {
                let done = crate::jobs::repo::list_job_items(&state.db, job.id).unwrap_or_default();
                for item in done.iter().filter(|i| i.action == action_name::PLAYLIST_ITEM_DELETE) {
                    // Skipped: the row was gone already, which is what was wanted.
                    if matches!(item.status, JobItemStatus::Done | JobItemStatus::Skipped) {
                        continue;
                    }
                    let Some(v) = item.params.get("video_id").and_then(Value::as_str) else {
                        continue;
                    };
                    let e = item.last_error.clone().unwrap_or_else(|| "not_removed".to_owned());
                    errors.insert(dead_key(v), e);
                }
                out.ops.extend(crate::commands::job_op(state, Some(&job)).await);
            }
        }
    }
    out.settle(wanted, &errors, &present, &added_keys);
    out
}

/// Every candidate (dismissed ones too: a row the user approved is applied whatever its alerts
/// say), and each playlist's title as its newest snapshot has it. The database only.
fn applicable(db: &Db) -> (Vec<RecoverCandidate>, HashMap<String, String>) {
    let latest = db.latest_snapshots();
    let alerts = db.alert_rows(true);
    let memberships = db.playlist_memberships();
    let unavailable_index = db.unavailable_index_rows();
    let stored = Stored {
        latest: &latest,
        alerts: &alerts,
        memberships: &memberships,
        unavailable_index: &unavailable_index,
    };
    let cands = collect_candidates(&stored, true, |pid, at| db.snapshot_before(pid, at));
    let titles =
        latest.iter().filter_map(|(id, s)| Some((id.clone(), s.title.clone()?))).collect();
    (cands, titles)
}

/// The row's outcome, on the session that started this apply (not one reset since).
fn settle_row(gen: u64, key: &str, status: &'static str, error: Option<String>) {
    with_session(|s| {
        if s.gen != gen {
            return;
        }
        if let Some(r) = s.row_mut(key) {
            r.status = status;
            r.error = error;
        }
        if s.snapshot.phase == "applying" {
            s.snapshot.done += 1;
        }
    });
}

/// A recovered track's alerts: dismissed (so seen), and seen by id as well.
fn close_alerts(state: &AppState, c: &RecoverCandidate) {
    for source in &c.sources {
        let kind = match *source {
            SOURCE_ALERT_UNAVAILABLE => "unavailable",
            SOURCE_ALERT_REMOVED => "removed",
            _ => continue,
        };
        state.db.dismiss_playlist_alert(&c.playlist_id, &c.video_id, kind);
    }
    if !c.alert_ids.is_empty() {
        state.db.mark_alerts_seen(Some(c.alert_ids.as_slice()));
    }
}

/// Put the approved replacements of `keys` in, playlist by playlist (rows approved with a pick
/// and not recovered already; the rest are skipped). The engine is `queue_target`'s for each
/// playlist: the Data API queues ([`apply_queued`]), InnerTube writes directly ([`apply_direct`]).
/// Each row ends `done`, `already_present` or `failed` with its error; a recovered one's alerts are
/// dismissed. A cancel stops it between playlists; YouTube's cooldown stops it where it is.
/// Never touches a playlist on this machine or Liked Music (invariant 6).
pub async fn apply(
    state: &Arc<AppState>,
    jobs: &JobsState,
    keys: Vec<String>,
    action: Action,
    engine: Option<&str>,
) -> Result<RecoverApplied, String> {
    const STEP: &str = "applying";
    if let Some(until) = import::youtube_cooldown(state) {
        return Err(Halt::Cooldown(until).code());
    }
    let (gen, todo, _) = begin(STEP, &keys, |r| {
        r.approved && r.pick.is_some() && !matches!(r.status, "done" | "already_present")
    })?;
    emit(state);
    let db = Arc::clone(&state.db);
    let (cands, titles) = match tauri::async_runtime::spawn_blocking(move || applicable(&db)).await
    {
        Ok(loaded) => loaded,
        Err(e) => {
            finish(state, gen, STEP, "failed", Some(e.to_string()));
            return Err(e.to_string());
        }
    };
    let by_key: HashMap<&str, &RecoverCandidate> =
        cands.iter().map(|c| (c.key.as_str(), c)).collect();

    // By playlist, in the order the keys came.
    let mut groups: Vec<(String, Vec<Wanted>, Vec<(String, String)>)> = Vec::new();
    for key in &todo {
        let pid = key.split_once('\u{1f}').map_or(key.as_str(), |(p, _)| p);
        let g = match groups.iter().position(|g| g.0 == pid) {
            Some(i) => i,
            None => {
                groups.push((pid.to_owned(), Vec::new(), Vec::new()));
                groups.len() - 1
            }
        };
        let pick = with_session(|s| s.row(key).and_then(|r| r.pick.clone()));
        let cand = by_key.get(key.as_str()).filter(|c| !excluded(&c.playlist_id));
        match (cand, pick) {
            (Some(c), Some(song)) if song.video_id != c.video_id => groups[g].1.push(Wanted {
                key: key.clone(),
                dead: c.video_id.clone(),
                dead_handle: c.set_video_id.clone(),
                in_playlist: c.in_playlist,
                next_handle: c.next_handle.clone(),
                song,
                action,
            }),
            (Some(_), Some(_)) => groups[g].2.push((key.clone(), "same_video".to_owned())),
            // Not a candidate any more (back, or recovered elsewhere), or the pick was cleared.
            _ => groups[g].2.push((key.clone(), "gone".to_owned())),
        }
    }

    let stop = move || !alive(gen, STEP);
    let mut applied = RecoverApplied { playlists: Vec::new() };
    let mut halt: Option<String> = None;
    let mut first = true;
    for (pid, wanted, failed) in groups {
        for (k, e) in &failed {
            settle_row(gen, k, "failed", Some(e.clone()));
        }
        let mut entry =
            AppliedPlaylist { playlist_id: pid.clone(), ops: Vec::new(), done: 0, failed };
        if wanted.is_empty() {
            applied.playlists.push(entry);
            continue;
        }
        if halt.is_none() && !stop() && !first {
            // Paced like every run of playlist writes; the cancel is heard while it waits.
            if let Err(e) = import::before_playlist_write(state, &stop).await {
                if stops(&e) {
                    halt = Some(e);
                }
            }
        }
        if let Some(e) = &halt {
            for w in &wanted {
                settle_row(gen, &w.key, "failed", Some(e.clone()));
                entry.failed.push((w.key.clone(), e.clone()));
            }
            applied.playlists.push(entry);
            continue;
        }
        if stop() {
            // Cancelled: what is left stays as it was.
            applied.playlists.push(entry);
            continue;
        }
        first = false;
        if with_step(gen, STEP, |s| s.snapshot.current = Some(pid.clone())).is_some() {
            emit(state);
        }

        let title = titles.get(&pid).cloned().unwrap_or_default();
        let n = wanted.len();
        let est = planner::estimate_transfer_units(n, false) + planner::estimate_remove_units(n, n);
        let target = crate::commands::queue_target(state, jobs, &[pid.as_str()], est, engine);
        let outcome = match target {
            Some(q) if q.engine == Engine::Ytdata => {
                apply_queued(state, jobs, &q, &pid, &title, &wanted).await
            }
            // InnerTube writes directly (D1), whatever the queue mode.
            _ => apply_direct(state, &pid, &title, &wanted).await,
        };

        for (key, status, error) in outcome.rows {
            settle_row(gen, &key, status, error.clone());
            if status == "failed" {
                entry.failed.push((key, error.unwrap_or_default()));
            } else {
                entry.done += 1;
                if let Some(c) = by_key.get(key.as_str()) {
                    close_alerts(state, c);
                }
            }
        }
        entry.ops = outcome.ops;
        applied.playlists.push(entry);
        halt = outcome.halt;
        emit(state);
    }
    match halt {
        Some(e) => finish(state, gen, STEP, "failed", Some(e)),
        None => finish(state, gen, STEP, "done", None),
    }
    Ok(applied)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(v: &str, s: &str, t: &str, u: bool) -> SnapItem {
        SnapItem {
            v: v.into(),
            s: Some(s.into()),
            t: t.into(),
            a: format!("Artist {v}"),
            d: Some("3:00".into()),
            u,
            th: None,
        }
    }

    fn snap(id: i64, pid: &str, taken_at: i64, items: Vec<SnapItem>) -> Snapshot {
        Snapshot {
            id,
            playlist_id: pid.into(),
            account_id: None,
            title: None,
            taken_at,
            item_count: items.len() as i64,
            hash: String::new(),
            items,
        }
    }

    fn alert(id: i64, pid: &str, vid: &str, kind: &str, at: i64) -> AlertRow {
        AlertRow {
            id,
            playlist_id: pid.into(),
            video_id: vid.into(),
            kind: kind.into(),
            song_json: None,
            at,
            from_pos: None,
            to_pos: None,
            seen: false,
            dismissed: false,
            resolved: false,
        }
    }

    fn song_json(vid: &str, title: &str, artists: &str) -> String {
        serde_json::to_string(&SongItem {
            video_id: vid.into(),
            title: title.into(),
            artists: artists.into(),
            ..Default::default()
        })
        .unwrap()
    }

    fn collect(
        latest: Vec<Snapshot>,
        alerts: Vec<AlertRow>,
        memberships: Vec<(&str, &str)>,
        index: Vec<(&str, &str, String)>,
        priors: Vec<Snapshot>,
        include_dismissed: bool,
    ) -> Vec<RecoverCandidate> {
        let latest: HashMap<String, Snapshot> =
            latest.into_iter().map(|s| (s.playlist_id.clone(), s)).collect();
        let mut held: HashMap<String, Vec<String>> = HashMap::new();
        for (vid, pid) in memberships {
            held.entry(vid.into()).or_default().push(pid.into());
        }
        let index: Vec<(String, String, String)> =
            index.into_iter().map(|(p, v, j)| (p.into(), v.into(), j)).collect();
        let stored = Stored {
            latest: &latest,
            alerts: &alerts,
            memberships: &held,
            unavailable_index: &index,
        };
        collect_candidates(&stored, include_dismissed, |pid, at| {
            priors
                .iter()
                .filter(|s| s.playlist_id == pid && s.taken_at < at)
                .max_by_key(|s| (s.taken_at, s.id))
                .cloned()
        })
    }

    #[test]
    fn placeholders_in_both_languages_any_case() {
        for t in [
            "",
            "  ",
            "Deleted video",
            "Private video",
            "PRIVATE VIDEO",
            " deleted video ",
            "Video eliminado",
            "Vídeo eliminado",
            "VÍDEO PRIVADO",
            "Video privado",
            "Video no disponible",
        ] {
            assert!(is_placeholder_title(t), "{t:?}");
        }
        for t in ["Deleted", "Private video (live)", "Real Song", "YouTube"] {
            assert!(!is_placeholder_title(t), "{t:?}");
        }
    }

    #[test]
    fn a_dead_row_of_the_newest_snapshot_has_its_place_and_anchor() {
        let s = snap(
            1,
            "VLPL1",
            100,
            vec![
                item("a", "h0", "A", false),
                item("dead", "h1", "Deleted video", true),
                item("b", "h2", "B", false),
                item("last", "h3", "Gone Song", true),
            ],
        );
        let got = collect(vec![s], vec![], vec![], vec![], vec![], false);
        assert_eq!(got.len(), 2);
        let dead = &got[0];
        assert_eq!(dead.key, "VLPL1\u{1f}dead");
        assert_eq!(dead.sources, vec![SOURCE_UNAVAILABLE]);
        assert_eq!((dead.position, dead.in_playlist), (Some(1), true));
        assert_eq!(dead.set_video_id.as_deref(), Some("h1"));
        assert_eq!(dead.next_handle.as_deref(), Some("h2"));
        assert_eq!(dead.title, None, "a placeholder is no title");
        let last = &got[1];
        assert_eq!((last.position, last.next_handle.as_deref()), (Some(3), None));
        assert_eq!(last.title.as_deref(), Some("Gone Song"));
        assert_eq!(last.title_source.as_deref(), Some("snapshot"));
    }

    #[test]
    fn local_playlists_and_liked_music_are_never_listed() {
        let local = format!("{}7", crate::state::LOCAL_PLAYLIST_PREFIX);
        let latest = vec![
            snap(1, LIKED_MUSIC_ID, 100, vec![item("x", "h", "", true)]),
            snap(2, &local, 100, vec![item("y", "h", "", true)]),
        ];
        let alerts = vec![
            alert(1, LIKED_MUSIC_ID, "x", "unavailable", 100),
            alert(2, &local, "z", "removed", 100),
        ];
        let index = vec![
            (local.as_str(), "w", song_json("w", "", "")),
            (LIKED_MUSIC_ID, "v", song_json("v", "", "")),
        ];
        assert!(collect(latest, alerts, vec![], index, vec![], true).is_empty());
    }

    #[test]
    fn the_index_stands_in_for_a_playlist_with_no_snapshot_only() {
        let latest = vec![snap(1, "VLPL1", 100, vec![item("a", "h0", "A", false)])];
        let index = vec![
            ("VLPL1", "stale", song_json("stale", "", "")),
            ("VLPL2", "dead", song_json("dead", "Old Song", "Old Artist")),
        ];
        let got = collect(latest, vec![], vec![], index, vec![], false);
        assert_eq!(got.len(), 1);
        assert_eq!((got[0].key.as_str(), got[0].position), ("VLPL2\u{1f}dead", None));
        assert_eq!(got[0].title.as_deref(), Some("Old Song"));
        assert_eq!(got[0].title_source.as_deref(), Some("index"));
        assert!(got[0].in_playlist);
    }

    #[test]
    fn sources_merge_and_the_live_position_wins() {
        let latest = vec![snap(
            5,
            "VLPL1",
            300,
            vec![item("a", "h0", "A", false), item("dead", "h9", "", true)],
        )];
        let mut gone = alert(1, "VLPL1", "dead", "unavailable", 200);
        gone.song_json = Some(song_json("dead", "Real Name", "Real Artist"));
        let mut older = alert(2, "VLPL1", "dead", "unavailable", 100);
        older.to_pos = Some(7);
        let got = collect(latest, vec![gone, older], vec![], vec![], vec![], false);
        assert_eq!(got.len(), 1, "one candidate per playlist and video");
        let c = &got[0];
        assert_eq!(c.sources, vec![SOURCE_UNAVAILABLE, SOURCE_ALERT_UNAVAILABLE]);
        assert_eq!(c.alert_ids, vec![1, 2]);
        assert_eq!((c.position, c.set_video_id.as_deref()), (Some(1), Some("h9")));
        assert_eq!(c.title.as_deref(), Some("Real Name"));
        assert_eq!(c.title_source.as_deref(), Some("alert"));
        assert_eq!(c.artists.as_deref(), Some("Real Artist"));
    }

    #[test]
    fn resolved_and_playing_again_unavailable_alerts_are_skipped() {
        let latest = vec![snap(1, "VLPL1", 300, vec![item("back", "h0", "Back", false)])];
        let mut resolved = alert(1, "VLPL1", "fixed", "unavailable", 200);
        resolved.resolved = true;
        let quiet = alert(2, "VLPL1", "back", "unavailable", 200);
        let open = alert(3, "VLPL2", "still", "unavailable", 200);
        let got = collect(latest, vec![resolved, quiet, open], vec![], vec![], vec![], false);
        assert_eq!(got.iter().map(|c| c.key.as_str()).collect::<Vec<_>>(), ["VLPL2\u{1f}still"]);
        assert_eq!(got[0].sources, vec![SOURCE_ALERT_UNAVAILABLE]);
        assert!(!got[0].in_playlist);
    }

    #[test]
    fn dismissed_alerts_only_when_asked() {
        let mut a = alert(1, "VLPL1", "v", "unavailable", 100);
        a.dismissed = true;
        assert!(collect(vec![], vec![a.clone()], vec![], vec![], vec![], false).is_empty());
        let got = collect(vec![], vec![a.clone()], vec![], vec![], vec![], true);
        assert_eq!((got.len(), got[0].dismissed), (1, true));
        // One open alert among them and it is not dismissed.
        let open = alert(2, "VLPL1", "v", "unavailable", 200);
        let got = collect(vec![], vec![open, a], vec![], vec![], vec![], true);
        assert_eq!((got[0].alert_ids.clone(), got[0].dismissed), (vec![2, 1], false));
        // A dead row of the snapshot is listed either way, its dismissed alert left out.
        let mut d = alert(3, "VLPL1", "x", "unavailable", 50);
        d.dismissed = true;
        let latest = vec![snap(1, "VLPL1", 300, vec![item("x", "h", "", true)])];
        let got = collect(latest, vec![d], vec![], vec![], vec![], false);
        assert_eq!((got[0].alert_ids.len(), got[0].dismissed), (0, false));
    }

    #[test]
    fn a_removed_track_is_anchored_by_the_snapshot_before_its_alert() {
        // Before (t=100): a, gone, b. Now (t=200): a, b. The alert is filed at the new read's time.
        let before = snap(
            1,
            "VLPL1",
            100,
            vec![
                item("a", "h0", "A", false),
                item("gone", "h1", "Gone", false),
                item("b", "h2", "B", false),
            ],
        );
        let now = snap(2, "VLPL1", 200, vec![item("a", "h0", "A", false), item("b", "h2", "B", false)]);
        let mut removed = alert(7, "VLPL1", "gone", "removed", 200);
        removed.from_pos = Some(1);
        let got = collect(
            vec![now.clone()],
            vec![removed.clone()],
            vec![],
            vec![],
            vec![before.clone(), now.clone()],
            false,
        );
        assert_eq!(got.len(), 1);
        let c = &got[0];
        assert_eq!(c.sources, vec![SOURCE_ALERT_REMOVED]);
        assert_eq!((c.position, c.next_handle.as_deref()), (Some(1), Some("h2")));
        assert_eq!((c.in_playlist, c.set_video_id.as_deref()), (false, None));
        assert_eq!(c.title.as_deref(), Some("Gone"), "the old snapshot's title");

        // A snapshot that does not have it there gives no anchor, the position stays.
        let mut off = removed.clone();
        off.from_pos = Some(0);
        let got = collect(vec![now.clone()], vec![off], vec![], vec![], vec![before.clone()], false);
        assert_eq!((got[0].position, got[0].next_handle.as_deref()), (Some(0), None));

        // Back in the playlist (the index, or the newest snapshot): not a candidate.
        let back = vec![("gone", "VLPL1")];
        assert!(collect(vec![now.clone()], vec![removed.clone()], back, vec![], vec![], false)
            .is_empty());
        let readded = snap(3, "VLPL1", 300, vec![item("gone", "h7", "Gone", false)]);
        assert!(collect(vec![readded], vec![removed], vec![], vec![], vec![], false).is_empty());
    }

    #[test]
    fn candidates_sort_by_playlist_then_position() {
        let latest = vec![
            snap(1, "VLPL2", 100, vec![item("b", "h", "", true)]),
            snap(2, "VLPL1", 100, vec![item("x", "h0", "", false), item("a", "h1", "", true)]),
        ];
        let mut r = alert(1, "VLPL1", "r", "removed", 100);
        r.from_pos = Some(0);
        let u = alert(2, "VLPL1", "q", "unavailable", 100);
        let got = collect(latest, vec![r, u], vec![], vec![], vec![], false);
        let keys: Vec<&str> = got.iter().map(|c| c.video_id.as_str()).collect();
        assert_eq!(keys, ["r", "a", "q", "b"]);
    }

    #[test]
    fn local_titles_follow_their_priority_and_skip_placeholders() {
        let wanted: HashSet<String> =
            ["s", "al", "h", "dl", "ix", "none"].iter().map(|s| s.to_string()).collect();
        let rows = LocalTitleRows {
            snapshots: vec![
                ("s".into(), "Deleted video".into(), "".into()),
                ("s".into(), "Snap Title".into(), "Snap Artist".into()),
                ("s".into(), "Older Snap".into(), "".into()),
                ("other".into(), "Not Wanted".into(), "".into()),
            ],
            alerts: vec![
                ("s".into(), song_json("s", "Alert For S", "")),
                ("al".into(), song_json("al", "Private video", "")),
                ("al".into(), song_json("al", "Alert Title", "Alert Artist")),
            ],
            plays: vec![
                ("al".into(), song_json("al", "Played Al", "")),
                ("h".into(), song_json("h", "Played", "P")),
            ],
            videos: vec![
                ("h".into(), "Download For H".into(), None),
                ("dl".into(), "Downloaded".into(), Some("Band - Topic".into())),
            ],
            index: vec![
                ("dl".into(), song_json("dl", "Index For Dl", "")),
                ("ix".into(), song_json("ix", "Indexed", "I")),
                ("none".into(), song_json("none", "Vídeo eliminado", "")),
            ],
        };
        let got = best_local_titles(&wanted, &rows);
        let at = |v: &str| got.get(v).map(|(t, a, s)| (t.as_str(), a.as_str(), *s));
        assert_eq!(at("s"), Some(("Snap Title", "Snap Artist", "snapshot")));
        assert_eq!(at("al"), Some(("Alert Title", "Alert Artist", "alert")));
        assert_eq!(at("h"), Some(("Played", "P", "history")));
        assert_eq!(at("dl"), Some(("Downloaded", "Band", "download")));
        assert_eq!(at("ix"), Some(("Indexed", "I", "index")));
        assert_eq!(at("none"), None, "only placeholders: nothing");
        assert_eq!(at("other"), None, "not asked for");
    }

    fn row(key: &str, title: Option<&str>) -> RecoverRow {
        RecoverRow {
            key: key.into(),
            title: title.map(str::to_owned),
            artists: None,
            title_source: title.map(|_| "snapshot".to_owned()),
            tier: Tier::Pending,
            pick: None,
            candidates: Vec::new(),
            approved: false,
            status: "idle",
            error: None,
            duration: None,
        }
    }

    #[test]
    fn a_refresh_keeps_progress_and_drops_rows_no_longer_listed() {
        let mut s = Session::default();
        s.refresh(vec![row("a", None), row("b", Some("B")), row("gone", None)]);
        s.row_mut("b").unwrap().approved = true;
        s.row_mut("b").unwrap().title = Some("Typed".into());
        s.refresh(vec![row("b", Some("B")), row("a", Some("Found")), row("c", None)]);
        let keys: Vec<&str> = s.rows().iter().map(|r| r.key.as_str()).collect();
        assert_eq!(keys, ["b", "a", "c"]);
        let b = s.row("b").unwrap();
        assert_eq!((b.approved, b.title.as_deref()), (true, Some("Typed")), "progress kept");
        assert_eq!(s.row("a").unwrap().title.as_deref(), Some("Found"), "a missing title filled");
        assert!(s.row("gone").is_none());

        // While a step runs nothing is dropped.
        s.snapshot.phase = "searching";
        s.refresh(vec![row("c", None)]);
        let keys: Vec<&str> = s.rows().iter().map(|r| r.key.as_str()).collect();
        assert_eq!(keys, ["c", "a", "b"]);
    }

    #[test]
    fn a_row_serializes_for_the_ui() {
        let v = serde_json::to_value(row("VLPL1\u{1f}v", None)).unwrap();
        assert_eq!(v["tier"], "pending");
        assert_eq!(v["status"], "idle");
        assert_eq!(v["approved"], false);
        assert!(v["pick"].is_null());
        let snap = serde_json::to_value(RecoverSnapshot::default()).unwrap();
        assert_eq!(snap["phase"], "idle");
        assert!(v.get("duration").is_none(), "the search's input only");
    }

    #[test]
    fn a_video_title_splits_into_artist_and_song() {
        let s = source_from_dead("Fake Band - Fake Song", None, Some("3:45"));
        assert_eq!((s.title.as_str(), s.artists.clone()), ("Fake Song", vec!["Fake Band".into()]));
        assert_eq!(s.duration_ms, Some(225_000));
        assert_eq!((s.id, s.video_id, s.album), (None, None, None));
        // En dash too; only the first dash splits, the rest stays the song's ("- Live").
        let s = source_from_dead("Fake Band \u{2013} Fake Song - Live", None, None);
        assert_eq!(s.artists, vec!["Fake Band".to_owned()]);
        assert_eq!(s.title, "Fake Song - Live");
        assert_eq!(s.duration_ms, None);
        // No dash, no artist: the title as it is.
        let s = source_from_dead("Just A Title", None, None);
        assert_eq!((s.title.as_str(), s.artists.len()), ("Just A Title", 0));
    }

    #[test]
    fn a_known_artist_is_kept_and_dropped_from_the_title() {
        let s = source_from_dead("fake band - Fake Song", Some("Fake Band"), None);
        assert_eq!((s.title.as_str(), s.artists.clone()), ("Fake Song", vec!["Fake Band".into()]));
        // A dash that is not the artist's stays in the title (a version, a subtitle).
        let s = source_from_dead("Fake Song - Acoustic", Some("Fake Band"), None);
        assert_eq!(s.title, "Fake Song - Acoustic");
        // Several artists, comma separated, each trimmed.
        let s = source_from_dead("Fake Song", Some("Fake A, Fake B "), None);
        assert_eq!(s.artists, vec!["Fake A".to_owned(), "Fake B".to_owned()]);
    }

    #[test]
    fn video_dressing_and_topic_channels_go() {
        for (raw, want) in [
            ("Fake Song (Official Video)", "Fake Song"),
            ("Fake Song [OFFICIAL MUSIC VIDEO]", "Fake Song"),
            ("Fake Song (Official Audio)", "Fake Song"),
            ("Fake Song (official lyric video)", "Fake Song"),
            ("Fake Song (Lyric Video)", "Fake Song"),
            ("Fake Song [Lyrics]", "Fake Song"),
            ("Fake Song (Video Oficial)", "Fake Song"),
            ("Fake Song (Audio Oficial)", "Fake Song"),
            ("Fake Song (Letra)", "Fake Song"),
            ("Fake Song ( Official  Video ) (Live)", "Fake Song (Live)"),
            // Dressing that is the recording's stays.
            ("Fake Song (Remix)", "Fake Song (Remix)"),
            // All dressing: the dressing is the title.
            ("(Official Video)", "(Official Video)"),
        ] {
            assert_eq!(source_from_dead(raw, Some("X"), None).title, want, "{raw:?}");
        }
        let s = source_from_dead("Fake Band - Fake Song (Video Oficial)", None, None);
        assert_eq!((s.title.as_str(), s.artists.clone()), ("Fake Song", vec!["Fake Band".into()]));
        let s = source_from_dead("Fake Song", Some("Fake Band - Topic"), None);
        assert_eq!(s.artists, vec!["Fake Band".to_owned()]);
        let s = source_from_dead("Fake Song", Some("Fake A - Topic, Fake B"), None);
        assert_eq!(s.artists, vec!["Fake A".to_owned(), "Fake B".to_owned()]);
    }

    #[test]
    fn keys_give_back_their_video() {
        assert_eq!(video_of(&key_of("VLPL1", "dQw4w9WgXcQ")), "dQw4w9WgXcQ");
        assert_eq!(video_of("no-separator"), "no-separator");
    }

    // --- plan_replacements ---

    fn fr(v: &str, h: &str) -> SongItem {
        SongItem { video_id: v.into(), set_video_id: Some(h.into()), ..Default::default() }
    }

    fn gone(v: &str, h: &str) -> SongItem {
        SongItem { unavailable: true, ..fr(v, h) }
    }

    fn want(dead: &str, dead_handle: Option<&str>, next: Option<&str>, new: &str) -> Wanted {
        Wanted {
            key: key_of("VLPL1", dead),
            dead: dead.into(),
            dead_handle: dead_handle.map(Into::into),
            in_playlist: dead_handle.is_some(),
            next_handle: next.map(Into::into),
            song: SongItem { video_id: new.into(), ..Default::default() },
            action: Action::Replace,
        }
    }

    fn k(v: &str) -> String {
        key_of("VLPL1", v)
    }

    fn added(plan: &Plan) -> Vec<&str> {
        plan.adds.iter().map(|a| a.song.video_id.as_str()).collect()
    }

    fn dead_handles(plan: &Plan) -> Vec<(&str, &str)> {
        plan.dead_rows.iter().map(|(k, r)| (video_of(k), handle(r).unwrap())).collect()
    }

    #[test]
    fn a_replacement_goes_in_front_of_its_dead_row_or_the_next_or_last() {
        let fresh = vec![fr("a", "h0"), gone("d1", "h1"), fr("b", "h2"), fr("c", "h3")];
        let plan = plan_replacements(
            &fresh,
            &[
                // Still there: in front of itself, and out.
                want("d1", Some("h1"), Some("h2"), "n1"),
                // Removed before, its next row still there: in front of that one.
                want("r1", None, Some("h3"), "n2"),
                // Removed before, its next row gone too: at the end.
                want("r2", None, Some("h9"), "n3"),
                // Removed before, it was last: at the end.
                want("r3", None, None, "n4"),
            ],
        );
        assert_eq!(added(&plan), ["n1", "n2", "n3", "n4"]);
        assert_eq!(plan.anchors.get(&k("d1")).map(String::as_str), Some("h1"));
        assert_eq!(plan.anchors.get(&k("r1")).map(String::as_str), Some("h3"));
        assert!(!plan.anchors.contains_key(&k("r2")), "an anchor that is gone: the end");
        assert!(!plan.anchors.contains_key(&k("r3")));
        assert_eq!(dead_handles(&plan), [("d1", "h1")], "only a dead row that is there goes");
        assert!(plan.already_present.is_empty());
        assert!(plan.adds.iter().all(|a| a.song.set_video_id.is_none()));
    }

    #[test]
    fn a_replacement_already_there_is_not_added_and_the_dead_row_still_goes() {
        let fresh = vec![fr("n1", "h0"), gone("d1", "h1"), fr("b", "h2")];
        let plan = plan_replacements(&fresh, &[want("d1", Some("h1"), Some("h2"), "n1")]);
        assert!(plan.adds.is_empty());
        assert!(plan.anchors.is_empty());
        assert_eq!(plan.already_present, [k("d1")]);
        assert_eq!(dead_handles(&plan), [("d1", "h1")], "D2");

        // The same replacement for two dead rows: added once, the second already present.
        let fresh = vec![gone("d1", "h1"), gone("d2", "h2")];
        let plan = plan_replacements(
            &fresh,
            &[want("d1", Some("h1"), None, "n"), want("d2", Some("h2"), None, "n")],
        );
        assert_eq!(added(&plan), ["n"]);
        assert_eq!(plan.already_present, [k("d2")]);
        assert_eq!(dead_handles(&plan), [("d1", "h1"), ("d2", "h2")]);
    }

    #[test]
    fn append_only_adds_at_the_end() {
        let fresh = vec![gone("d1", "h1"), fr("b", "h2"), fr("n2", "h3")];
        let mut a = want("d1", Some("h1"), Some("h2"), "n1");
        a.action = Action::Append;
        let mut b = want("d2", None, Some("h2"), "n2");
        b.action = Action::Append;
        let plan = plan_replacements(&fresh, &[a, b]);
        assert_eq!(added(&plan), ["n1"]);
        assert!(plan.anchors.is_empty());
        assert!(plan.dead_rows.is_empty(), "append never takes anything out");
        assert_eq!(plan.already_present, [k("d2")]);
    }

    #[test]
    fn a_dead_row_never_goes_without_its_replacement() {
        // The handle changed: found again by videoId, but only a copy that is still dead.
        let fresh = vec![fr("p", "h5"), gone("d1", "h7"), fr("same", "h8")];
        let plan = plan_replacements(&fresh, &[want("d1", Some("h1"), None, "n1")]);
        assert_eq!(dead_handles(&plan), [("d1", "h7")]);
        assert_eq!(plan.anchors.get(&k("d1")).map(String::as_str), Some("h7"));
        let plan = plan_replacements(&fresh, &[want("p", Some("h1"), None, "n2")]);
        assert!(plan.dead_rows.is_empty(), "a row that plays again is not dead");
        // A replacement that is the dead video itself never takes it out.
        let plan = plan_replacements(&fresh, &[want("d1", Some("h7"), None, "d1")]);
        assert!(plan.dead_rows.is_empty());
        // One dead row per wanted, even for two keys naming the same video.
        let fresh = vec![gone("d1", "h1")];
        let mut twice = want("d1", Some("h1"), None, "n2");
        twice.key = key_of("VLPL1", "other");
        let plan = plan_replacements(&fresh, &[want("d1", Some("h1"), None, "n1"), twice]);
        assert_eq!(plan.dead_rows.len(), 1);

        // Every dead row taken out belongs to a replacement added or already there.
        let fresh = vec![gone("d1", "h1"), gone("d2", "h2"), fr("x", "h3"), gone("d3", "h4")];
        let wanted = [
            want("d1", Some("h1"), Some("h2"), "x"),
            want("d2", Some("h2"), Some("h3"), "n2"),
            want("d3", None, None, "n3"),
        ];
        let plan = plan_replacements(&fresh, &wanted);
        for (key, _) in &plan.dead_rows {
            let covered = plan.adds.iter().any(|a| &a.key == key)
                || plan.already_present.contains(key);
            assert!(covered, "{key:?} would go with no replacement");
        }
        assert_eq!(dead_handles(&plan), [("d1", "h1"), ("d2", "h2")]);
    }

    #[test]
    fn data_api_inserts_go_highest_position_first_and_the_end_last() {
        // d1 at 1 and d3 at 3 still there; r was removed and sat before d3 too (its next row).
        let fresh = vec![fr("a", "h0"), gone("d1", "h1"), fr("b", "h2"), gone("d3", "h3")];
        let plan = plan_replacements(
            &fresh,
            &[
                want("d1", Some("h1"), None, "n1"),
                want("r", None, Some("h3"), "nr"),
                want("d3", Some("h3"), None, "n3"),
                want("e", None, None, "ne"),
            ],
        );
        let order: Vec<(&str, Option<usize>)> = insert_order(&fresh, &plan)
            .into_iter()
            .map(|(a, p)| (a.song.video_id.as_str(), p))
            .collect();
        // At 3, n3 first then nr in front of it: nr, n3 is the order wanted.
        assert_eq!(order, [("n3", Some(3)), ("nr", Some(3)), ("n1", Some(1)), ("ne", None)]);
    }

    #[test]
    fn undo_puts_a_dead_row_back_before_the_next_row_that_stays() {
        let list = vec![fr("a", "h0"), gone("d1", "h1"), gone("d2", "h2"), fr("b", "h3")];
        let out: HashSet<String> = ["h1".to_owned(), "h2".to_owned()].into();
        let got: Vec<(String, Option<String>)> =
            restore_rows(&list, &out).into_iter().map(|r| (r.song.video_id, r.before)).collect();
        assert_eq!(got, [("d1".into(), Some("h3".into())), ("d2".into(), Some("h3".into()))]);
    }

    #[test]
    fn actions_parse() {
        assert_eq!(Action::parse("replace"), Some(Action::Replace));
        assert_eq!(Action::parse("append"), Some(Action::Append));
        assert_eq!(Action::parse("move"), None);
    }

    #[test]
    fn a_reset_keeps_counting_generations() {
        // The session is process-wide: this test only reads what it set.
        reset_session();
        let before = with_session(|s| {
            s.gen += 5;
            s.snapshot.phase = "searching";
            s.gen
        });
        reset_session();
        let (gen, phase) = with_session(|s| (s.gen, s.snapshot.phase));
        assert!(gen > before, "a step of the old session must not match the new one");
        assert_eq!(phase, "idle");
        assert!(!alive(before, "searching"));
    }
}
