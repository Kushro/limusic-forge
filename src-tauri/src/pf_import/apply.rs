//! Applying a PlaylistForge import to Forge's database (commit 31): PlaylistForge's last view of
//! each playlist becomes the baseline of Forge's playlist index, and its alerts, downloads,
//! settings and Data API state (accounts, pending jobs, today's quota spend, the OAuth client)
//! come along. Everything here is **idempotent**: applying the same import twice adds nothing the
//! second time.
//!
//! - **Playlists.** Each selected PlaylistForge playlist (`PL…`) is Forge's `VL…`. Where it lands
//!   depends on the account it belongs to ([`Target`]):
//!   - the account is the signed-in cookie account (paired by `account_json.channelId`, or by the
//!     user's pick in the preview): its current snapshot becomes the index (`playlist_track` with
//!     `song_json` from the catalog's `best_*`, `first_seen`, `added_at`), plus `playlist_sync`
//!     (`synced_at` = the account's `last_sync_at`) and every snapshot. **D36**: if Forge already
//!     synced that playlist more recently than PlaylistForge did, Forge's index stays and only the
//!     older snapshots it lacks are added;
//!   - another saved cookie account: its history only (snapshots), since the index is the active
//!     account's;
//!   - no account here, or deleted on YouTube: "missing", created as a playlist on this machine or
//!     on the signed-in account by `video_id` (through the Spotify import's paced writer,
//!     `import::start_known`), or skipped, as the user picks ([`MissingAs`]).
//! - **Data API (D-F7)**, in foreign-key order: `ytdata_accounts` first (as `reauth_required` until
//!   a token is brought over with [`import_tokens`]), then the pending jobs with new ids and their
//!   items, then today's (Pacific day) quota ledger rows with no job. `client_secret.json` is
//!   copied only when Forge has none. Tokens only with the user's explicit consent.
//!
//! What has been brought over is remembered in `pf_import_map` (local playlists, account
//! playlists, jobs), so nothing is created twice.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use innertube::SongItem;
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use ytdata::auth::store::TokenStore;

use super::credentials::read_pf_refresh_token;
use super::mapping::{self, epoch_secs, SkipReason, SkippedSetting, ThemeChoice};
use super::reader::{PfData, PfFiles, PfPlaylist, PfSnapshot, PfSnapshotItem, PfSummary};
use super::PfImportError;
use crate::db::{snapshot_hash, Db, Privacy, SnapItem};
use crate::spotify::{ListKind, SourceList, SourceTrack};

/// What has been imported already: `(kind, PlaylistForge id) → Forge id`. Created on first use,
/// outside the numbered schema: it is the importer's own bookkeeping, nothing else reads it.
const MAP_TABLE: &str = "CREATE TABLE IF NOT EXISTS pf_import_map (
    kind     TEXT NOT NULL,
    pf_id    TEXT NOT NULL,
    forge_id TEXT NOT NULL,
    PRIMARY KEY (kind, pf_id)
)";
const MAP_LOCAL: &str = "local_playlist";
const MAP_ACCOUNT: &str = "account_playlist";
const MAP_JOB: &str = "job";

/// Forge's id for a PlaylistForge playlist: the browse id (`VL` + the playlist id), as the index,
/// the snapshots and the alerts file account playlists.
pub fn forge_playlist_id(pf_id: &str) -> String {
    format!("VL{pf_id}")
}

// --- Forge's side -------------------------------------------------------------------------------

/// A cookie account saved in Forge, as the preview's account picker shows it. No cookie.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ForgeAccount {
    pub id: String,
    pub name: Option<String>,
    /// From its `account_json.channelId`.
    pub channel_id: Option<String>,
    /// The signed-in one: only its playlists go into the index.
    pub active: bool,
}

pub fn forge_accounts(db: &Db) -> Vec<ForgeAccount> {
    let active = db.get_setting("active_account");
    db.list_accounts()
        .into_iter()
        .map(|a| {
            let json: Value = a
                .account_json
                .as_deref()
                .and_then(|j| serde_json::from_str(j).ok())
                .unwrap_or_default();
            let text = |k: &str| {
                json.get(k).and_then(Value::as_str).filter(|s| !s.is_empty()).map(str::to_owned)
            };
            ForgeAccount {
                active: active.as_deref() == Some(a.id.as_str()),
                name: text("name"),
                channel_id: text("channelId"),
                id: a.id,
            }
        })
        .collect()
}

/// Each PlaylistForge account (channel id) → the Forge cookie account of the same channel, if any.
pub fn auto_account_map(
    pf_ids: &[&str],
    forge: &[ForgeAccount],
) -> HashMap<String, Option<String>> {
    pf_ids
        .iter()
        .map(|pf| {
            let found =
                forge.iter().find(|f| f.channel_id.as_deref() == Some(*pf)).map(|f| f.id.clone());
            (pf.to_string(), found)
        })
        .collect()
}

/// The user's pairing where they made one (an unknown Forge id counts as none), the automatic one
/// otherwise.
fn effective_map(
    pf_ids: &[&str],
    forge: &[ForgeAccount],
    chosen: &HashMap<String, Option<String>>,
) -> HashMap<String, Option<String>> {
    let mut map = auto_account_map(pf_ids, forge);
    for (pf, pick) in chosen {
        if map.contains_key(pf) {
            let valid = pick.clone().filter(|id| forge.iter().any(|f| &f.id == id));
            map.insert(pf.clone(), valid);
        }
    }
    map
}

/// Where a playlist goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Target {
    /// The signed-in account's: the index baseline.
    Index,
    /// Another saved account's: its snapshots only.
    History,
    /// No account here for it, or gone from YouTube: [`MissingAs`].
    Missing,
}

fn target(pl: &PfPlaylist, account: Option<&ForgeAccount>) -> Target {
    if pl.deleted_remotely {
        return Target::Missing;
    }
    match account {
        Some(a) if a.active => Target::Index,
        Some(_) => Target::History,
        None => Target::Missing,
    }
}

/// D36: which side's index wins.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Winner {
    Pf,
    Forge,
}

/// The most recent sync wins: Forge's `playlist_sync.synced_at` against PlaylistForge's
/// `accounts.last_sync_at`. Forge never synced it: PlaylistForge. PlaylistForge never synced (or a
/// tie): Forge, whose index is not overwritten for nothing.
pub fn d36_winner(forge_synced_at: Option<i64>, pf_last_sync_at: Option<&str>) -> Winner {
    match (forge_synced_at, pf_last_sync_at.map(epoch_secs)) {
        (None, _) => Winner::Pf,
        (Some(forge), Some(pf)) if pf > forge => Winner::Pf,
        _ => Winner::Forge,
    }
}

// --- the preview --------------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PreviewPlaylist {
    pub id: String,
    pub title: String,
    pub account_id: String,
    pub privacy: String,
    /// Items in PlaylistForge's last view of it.
    pub items: usize,
    pub deleted_remotely: bool,
    /// Forge has a sync record for it.
    pub in_forge: bool,
    pub forge_synced_at: Option<i64>,
    pub pf_synced_at: Option<String>,
    /// D36, should it go into the index.
    pub winner: Winner,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PreviewAccount {
    pub id: String,
    pub title: String,
    pub last_sync_at: Option<String>,
    /// The Forge cookie account of the same channel, if one is saved.
    pub forge_account: Option<String>,
    /// PlaylistForge has a Data API refresh token entry to offer for it (`accounts.json`).
    pub data_api: bool,
}

/// What the preview shows. Counts and names only: no token, no client secret.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Preview {
    pub summary: PfSummary,
    pub playlists: Vec<PreviewPlaylist>,
    pub accounts: Vec<PreviewAccount>,
    pub forge_accounts: Vec<ForgeAccount>,
    /// PlaylistForge has a `client_secret.json`, Forge has one, and whether PlaylistForge's
    /// tokens would work with Forge's client (the same one, or Forge has none yet).
    pub pf_client_secret: bool,
    pub forge_client_secret: bool,
    pub tokens_compatible: bool,
    /// `config.json`'s theme and language, as Forge would apply them.
    pub theme: Option<ThemeChoice>,
    pub locale: Option<&'static str>,
    /// Short codes: unreadable JSON files, unreadable backups.
    pub problems: Vec<&'static str>,
}

pub fn preview(db: &Db, data: &PfData, forge_client_id: Option<&str>) -> Preview {
    let syncs = db.playlist_syncs();
    let forge = forge_accounts(db);
    let pf_ids: Vec<&str> = data.db.accounts.iter().map(|a| a.id.as_str()).collect();
    let auto = auto_account_map(&pf_ids, &forge);
    let last_sync = |account: &str| {
        data.db.accounts.iter().find(|a| a.id == account).and_then(|a| a.last_sync_at.clone())
    };
    let playlists = data
        .db
        .playlists
        .iter()
        .map(|p| {
            let forge_synced_at = syncs.get(&forge_playlist_id(&p.id)).map(|s| s.synced_at);
            let pf_synced_at = last_sync(&p.account_id);
            PreviewPlaylist {
                id: p.id.clone(),
                title: p.title.clone(),
                account_id: p.account_id.clone(),
                privacy: p.privacy.clone(),
                items: data.db.current_snapshot(&p.id).map_or(0, |s| s.items.len()),
                deleted_remotely: p.deleted_remotely,
                in_forge: forge_synced_at.is_some(),
                winner: d36_winner(forge_synced_at, pf_synced_at.as_deref()),
                forge_synced_at,
                pf_synced_at,
            }
        })
        .collect();
    let accounts = data
        .db
        .accounts
        .iter()
        .map(|a| PreviewAccount {
            id: a.id.clone(),
            title: a.title.clone(),
            last_sync_at: a.last_sync_at.clone(),
            forge_account: auto.get(&a.id).cloned().flatten(),
            data_api: data.files.accounts.iter().any(|j| j.channel_id == a.id),
        })
        .collect();
    let pf_client = data.files.client_secret.as_ref().map(|c| c.client_id().to_owned());
    let mut problems = data.files.problems.clone();
    if data.backups.unreadable > 0 {
        problems.push("backups_unreadable");
    }
    Preview {
        summary: data.summary(),
        playlists,
        accounts,
        forge_accounts: forge,
        pf_client_secret: pf_client.is_some(),
        forge_client_secret: forge_client_id.is_some_and(|c| !c.trim().is_empty()),
        tokens_compatible: super::credentials::clients_compatible(
            pf_client.as_deref(),
            forge_client_id,
        )
        .is_ok(),
        theme: data.files.config.as_ref().map(|c| mapping::theme(&c.theme)),
        locale: data.files.config.as_ref().map(|c| mapping::locale(&c.lang)),
        problems,
    }
}

// --- the selection ------------------------------------------------------------------------------

/// What becomes of a "missing" playlist ([`Target::Missing`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum MissingAs {
    /// A playlist on this machine, in PlaylistForge's order.
    #[default]
    Local,
    /// A new playlist on the signed-in account, filled by `video_id` at the import's pace.
    Account,
    Skip,
}

/// The user's choices in the preview.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct Selection {
    /// PlaylistForge's folder, when not the default one.
    pub path: Option<String>,
    /// The PlaylistForge playlist ids to bring over.
    pub playlists: Vec<String>,
    /// PlaylistForge channel id → Forge cookie account id (`null`: none). Accounts left out are
    /// paired automatically.
    pub account_map: HashMap<String, Option<String>>,
    pub missing_as: MissingAs,
    pub alerts: bool,
    pub downloads: bool,
    pub settings: bool,
    pub appearance: bool,
    /// Data API accounts, pending jobs, today's quota, `client_secret.json` (D-F7).
    pub data_api: bool,
}

impl Default for Selection {
    fn default() -> Self {
        Self {
            path: None,
            playlists: Vec::new(),
            account_map: HashMap::new(),
            missing_as: MissingAs::Local,
            alerts: true,
            downloads: true,
            settings: true,
            appearance: true,
            data_api: true,
        }
    }
}

/// What the import did, by kind, plus warnings (short codes). Shown as the final summary.
#[derive(Debug, Clone, Default, Serialize)]
pub struct Report {
    pub playlists_indexed: usize,
    /// D36: Forge's own sync was newer, its index stayed.
    pub playlists_kept: usize,
    pub playlists_history: usize,
    pub local_created: usize,
    /// Created by an earlier import already.
    pub local_existing: usize,
    /// Handed to the import queue for the account (created at its pace, after this returns).
    pub account_queued: usize,
    pub missing_skipped: usize,
    pub tracks: usize,
    pub snapshots: usize,
    pub alerts: usize,
    pub videos: usize,
    pub downloads: usize,
    pub downloads_available: usize,
    pub downloads_missing: usize,
    pub settings: usize,
    pub settings_skipped: Vec<SkippedSetting>,
    /// For the UI to apply: the theme lives in its localStorage, the language in both.
    pub theme: Option<ThemeChoice>,
    pub locale: Option<&'static str>,
    pub ytdata_accounts: usize,
    pub jobs: usize,
    pub job_items: usize,
    pub quota_entries: usize,
    pub client_secret_copied: bool,
    pub warnings: Vec<String>,
    /// The playlists to create on the account, for `import::start_known`. Not sent to the UI.
    #[serde(skip)]
    pub account_lists: Vec<SourceList>,
    /// Their PlaylistForge ids, for [`mark_account_lists`] once they are queued.
    #[serde(skip)]
    pub account_list_ids: Vec<String>,
}

/// What [`apply`] needs from outside, injected so tests touch nothing real.
pub struct ApplyCtx<'a> {
    pub now: DateTime<Utc>,
    /// `set_setting`'s own per-key validation (`commands::pf_settable`).
    pub validate: &'a dyn Fn(&str, &str) -> Result<(), String>,
    /// Folders a backups folder may not be inside (`backups::forbidden_roots`).
    pub forbidden: &'a [PathBuf],
    /// Where Forge keeps `client_secret.json` (`ytdata_secrets::client_secret_path`).
    pub client_secret_dest: Option<&'a Path>,
    /// `(step, done, total)`, for the `pf-import-progress` event.
    pub progress: &'a dyn Fn(&'static str, usize, usize),
}

fn sql(e: rusqlite::Error) -> PfImportError {
    PfImportError::Sqlite(e.to_string())
}

// --- songs --------------------------------------------------------------------------------------

// YouTube's stand-in titles for a gone video: never taken as a real title.
use crate::playlist_tools::recover::is_placeholder_title as is_placeholder;

/// A Topic channel (`Artist - Topic`) is the artist.
fn artist_of(channel: &str) -> String {
    channel.strip_suffix(" - Topic").unwrap_or(channel).trim().to_owned()
}

/// PlaylistForge's statuses that YouTube greys out.
fn unavailable(status: &str) -> bool {
    matches!(status, "deleted" | "private" | "region_blocked")
}

fn thumbnail(video_id: &str) -> String {
    format!("https://i.ytimg.com/vi/{video_id}/mqdefault.jpg")
}

/// One snapshot row as Forge's `SongItem`: the catalog's never-degrading `best_*` first, the
/// snapshot's own title only when it is not YouTube's placeholder.
pub fn song_of(item: &PfSnapshotItem) -> SongItem {
    let title = item
        .best_title
        .clone()
        .filter(|t| !is_placeholder(t))
        .or_else(|| item.title_at_time.clone().filter(|t| !is_placeholder(t)))
        .or_else(|| item.title_at_time.clone())
        .unwrap_or_default();
    let channel = item
        .best_channel_title
        .clone()
        .or_else(|| item.channel_at_time.clone())
        .unwrap_or_default();
    SongItem {
        video_id: item.video_id.clone(),
        title,
        artists: artist_of(&channel),
        artist_id: item.best_channel_id.clone(),
        duration: item.duration_s.map(crate::ytdata_sync::format_duration),
        thumbnail: Some(thumbnail(&item.video_id)),
        set_video_id: Some(item.playlist_item_id.clone()),
        is_video: !channel.is_empty() && !channel.ends_with(" - Topic"),
        unavailable: unavailable(&item.status),
        ..Default::default()
    }
}

/// When each video was first seen in this playlist: unknown (NULL) for what its first snapshot
/// already held, as Forge does on a first read; the snapshot's time for what came later.
fn first_seen(snapshots: &[&PfSnapshot]) -> HashMap<String, Option<i64>> {
    let mut out = HashMap::new();
    for (i, snap) in snapshots.iter().enumerate() {
        let at = (i > 0).then(|| epoch_secs(&snap.taken_at));
        for item in &snap.items {
            out.entry(item.video_id.clone()).or_insert(at);
        }
    }
    out
}

// --- writing ------------------------------------------------------------------------------------

fn mapped(conn: &Connection, kind: &str, pf_id: &str) -> rusqlite::Result<Option<String>> {
    conn.query_row(
        "SELECT forge_id FROM pf_import_map WHERE kind = ?1 AND pf_id = ?2",
        [kind, pf_id],
        |r| r.get(0),
    )
    .optional()
}

fn remember(conn: &Connection, kind: &str, pf_id: &str, forge_id: &str) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO pf_import_map(kind, pf_id, forge_id) VALUES(?1, ?2, ?3) \
         ON CONFLICT(kind, pf_id) DO UPDATE SET forge_id = excluded.forge_id",
        [kind, pf_id, forge_id],
    )?;
    Ok(())
}

/// Replace the playlist's index rows with PlaylistForge's current view, and its sync record.
fn write_index(
    conn: &Connection,
    vl: &str,
    current: &PfSnapshot,
    seen: &HashMap<String, Option<i64>>,
    synced_at: i64,
    privacy: Option<Privacy>,
) -> rusqlite::Result<usize> {
    conn.execute("DELETE FROM playlist_track WHERE playlist_id = ?1", [vl])?;
    let mut written = 0;
    for item in &current.items {
        let json = serde_json::to_string(&song_of(item)).unwrap_or_default();
        let first = seen.get(&item.video_id).copied().flatten();
        let added = item.added_at.as_deref().map(epoch_secs);
        written += conn.execute(
            "INSERT OR IGNORE INTO playlist_track(playlist_id, video_id, song_json, first_seen, \
             added_at) VALUES(?1, ?2, ?3, ?4, ?5)",
            params![vl, item.video_id, json, first, added],
        )?;
    }
    conn.execute(
        "INSERT INTO playlist_sync(playlist_id, synced_at, item_count, added, removed, moved, \
         privacy) VALUES(?1, ?2, ?3, 0, 0, 0, ?4) ON CONFLICT(playlist_id) DO UPDATE SET \
         synced_at = excluded.synced_at, item_count = excluded.item_count, added = 0, \
         removed = 0, moved = 0, privacy = COALESCE(excluded.privacy, playlist_sync.privacy)",
        params![vl, synced_at, current.items.len() as i64, privacy.map(Privacy::as_str)],
    )?;
    Ok(written)
}

/// Add one PlaylistForge snapshot unless Forge has it (same playlist, time and content).
fn add_snapshot(
    conn: &Connection,
    vl: &str,
    account_id: Option<&str>,
    title: &str,
    snap: &PfSnapshot,
) -> rusqlite::Result<bool> {
    let items: Vec<SnapItem> =
        snap.items.iter().map(|i| SnapItem::from_song(&song_of(i))).collect();
    let hash = snapshot_hash(&items);
    let taken = epoch_secs(&snap.taken_at);
    let exists: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM playlist_snapshot WHERE playlist_id = ?1 AND taken_at = ?2 \
         AND hash = ?3)",
        params![vl, taken, hash],
        |r| r.get(0),
    )?;
    if exists {
        return Ok(false);
    }
    conn.execute(
        "INSERT INTO playlist_snapshot(playlist_id, account_id, title, taken_at, item_count, \
         hash, items_json) VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            vl,
            account_id,
            title,
            taken,
            items.len() as i64,
            hash,
            serde_json::to_string(&items).unwrap_or_else(|_| "[]".into())
        ],
    )?;
    Ok(true)
}

/// A playlist on this machine with PlaylistForge's current items, in order. `None` when an earlier
/// import made it already (and it is still there).
fn create_local(
    conn: &Connection,
    pl: &PfPlaylist,
    current: Option<&PfSnapshot>,
    now: i64,
) -> rusqlite::Result<Option<i64>> {
    if let Some(id) = mapped(conn, MAP_LOCAL, &pl.id)?.and_then(|id| id.parse::<i64>().ok()) {
        let alive: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM local_playlists WHERE id = ?1)",
            [id],
            |r| r.get(0),
        )?;
        if alive {
            return Ok(None);
        }
    }
    conn.execute(
        "INSERT INTO local_playlists(title, description, created_at, updated_at) \
         VALUES(?1, ?2, ?3, ?4)",
        params![pl.title, pl.description, epoch_secs(&pl.first_seen).min(now), now],
    )?;
    let id = conn.last_insert_rowid();
    let mut position = 0i64;
    for item in current.map(|s| s.items.as_slice()).unwrap_or_default() {
        let json = serde_json::to_string(&song_of(item)).unwrap_or_default();
        let added = item.added_at.as_deref().map(epoch_secs).unwrap_or(now);
        let n = conn.execute(
            "INSERT OR IGNORE INTO local_playlist_tracks(playlist_id, video_id, song_json, \
             added_at, position) VALUES(?1, ?2, ?3, ?4, ?5)",
            params![id, item.video_id, json, added, position + 1],
        )?;
        position += n as i64;
    }
    remember(conn, MAP_LOCAL, &pl.id, &id.to_string())?;
    Ok(Some(id))
}

/// The playlist as a Spotify-import list whose tracks name their videos. Greyed-out videos are
/// left out: YouTube would refuse them.
fn account_list(pl: &PfPlaylist, current: Option<&PfSnapshot>) -> SourceList {
    let tracks = current
        .map(|s| s.items.as_slice())
        .unwrap_or_default()
        .iter()
        .filter(|i| !unavailable(&i.status))
        .map(|i| {
            let song = song_of(i);
            SourceTrack {
                title: song.title,
                artists: if song.artists.is_empty() { Vec::new() } else { vec![song.artists] },
                duration_ms: i.duration_s.and_then(|s| u64::try_from(s).ok()).map(|s| s * 1000),
                video_id: Some(i.video_id.clone()),
                ..Default::default()
            }
        })
        .collect();
    SourceList {
        kind: ListKind::Playlist,
        name: pl.title.clone(),
        owner: None,
        cover: None,
        url: None,
        tracks,
        skipped: 0,
        truncated: false,
    }
}

/// Remember that these account playlists were handed to the import queue, so the next import does
/// not create them again.
pub fn mark_account_lists(db: &Db, pf_ids: &[String]) -> Result<(), PfImportError> {
    let conn = db.conn();
    conn.execute_batch(MAP_TABLE).map_err(sql)?;
    for id in pf_ids {
        remember(&conn, MAP_ACCOUNT, id, &forge_playlist_id(id)).map_err(sql)?;
    }
    Ok(())
}

/// Apply `sel` to Forge's database. Safe to run again: nothing is added twice.
pub fn apply(
    db: &Db,
    data: &PfData,
    sel: &Selection,
    ctx: &ApplyCtx<'_>,
) -> Result<Report, PfImportError> {
    let now = ctx.now.timestamp();
    let mut report = Report::default();
    let forge = forge_accounts(db);
    let pf_ids: Vec<&str> = data.db.accounts.iter().map(|a| a.id.as_str()).collect();
    let account_map = effective_map(&pf_ids, &forge, &sel.account_map);
    let wanted: HashSet<&str> = sel.playlists.iter().map(String::as_str).collect();
    let playlists: Vec<&PfPlaylist> =
        data.db.playlists.iter().filter(|p| wanted.contains(p.id.as_str())).collect();
    let syncs = db.playlist_syncs();
    {
        db.conn().execute_batch(MAP_TABLE).map_err(sql)?;
    }

    // Videos first: downloads reference them, and they name what the alerts are about.
    (ctx.progress)("videos", 0, 1);
    {
        let conn = db.conn();
        let tx = conn.unchecked_transaction().map_err(sql)?;
        for v in &data.db.videos {
            report.videos += tx
                .execute(
                    "INSERT INTO videos(video_id, title, channel, duration_s, updated_at) \
                     VALUES(?1, ?2, ?3, ?4, ?5) ON CONFLICT(video_id) DO UPDATE SET \
                     title = COALESCE(videos.title, excluded.title), \
                     channel = COALESCE(videos.channel, excluded.channel), \
                     duration_s = COALESCE(videos.duration_s, excluded.duration_s) \
                     WHERE videos.title IS NULL OR videos.channel IS NULL \
                     OR videos.duration_s IS NULL",
                    params![v.id, v.best_title, v.best_channel_title, v.duration_s, now],
                )
                .map_err(sql)?;
        }
        tx.commit().map_err(sql)?;
    }

    // Playlists.
    let mut on_account: HashSet<String> = HashSet::new();
    let total = playlists.len();
    for (n, pl) in playlists.iter().enumerate() {
        (ctx.progress)("playlists", n, total);
        let forge_id = account_map.get(&pl.account_id).cloned().flatten();
        let account = forge_id.as_deref().and_then(|id| forge.iter().find(|f| f.id == id));
        let vl = forge_playlist_id(&pl.id);
        let mut snaps: Vec<&PfSnapshot> =
            data.db.snapshots.iter().filter(|s| s.playlist_id == pl.id).collect();
        snaps.sort_by_key(|s| epoch_secs(&s.taken_at));
        let current = data.db.current_snapshot(&pl.id);
        let conn = db.conn();
        let tx = conn.unchecked_transaction().map_err(sql)?;
        match target(pl, account) {
            Target::Index => {
                on_account.insert(pl.id.clone());
                let pf_sync = data
                    .db
                    .accounts
                    .iter()
                    .find(|a| a.id == pl.account_id)
                    .and_then(|a| a.last_sync_at.clone());
                let forge_sync = syncs.get(&vl).map(|s| s.synced_at);
                // The newest snapshot Forge holds: PlaylistForge's go in only before it, so the
                // monitor's next "before" stays Forge's own.
                let forge_latest: Option<i64> = tx
                    .query_row(
                        "SELECT MAX(taken_at) FROM playlist_snapshot WHERE playlist_id = ?1",
                        [&vl],
                        |r| r.get(0),
                    )
                    .map_err(sql)?;
                let winner = d36_winner(forge_sync, pf_sync.as_deref());
                if winner == Winner::Pf {
                    if let Some(current) = current {
                        let synced_at = pf_sync
                            .as_deref()
                            .map(epoch_secs)
                            .unwrap_or_else(|| epoch_secs(&current.taken_at));
                        report.tracks += write_index(
                            &tx,
                            &vl,
                            current,
                            &first_seen(&snaps),
                            synced_at,
                            Privacy::parse(&pl.privacy),
                        )
                        .map_err(sql)?;
                    }
                    report.playlists_indexed += 1;
                } else {
                    report.playlists_kept += 1;
                }
                for snap in &snaps {
                    let older = winner == Winner::Pf
                        || forge_latest.is_none_or(|latest| epoch_secs(&snap.taken_at) < latest);
                    if older
                        && add_snapshot(&tx, &vl, forge_id.as_deref(), &pl.title, snap)
                            .map_err(sql)?
                    {
                        report.snapshots += 1;
                    }
                }
            }
            Target::History => {
                on_account.insert(pl.id.clone());
                for snap in &snaps {
                    if add_snapshot(&tx, &vl, forge_id.as_deref(), &pl.title, snap).map_err(sql)? {
                        report.snapshots += 1;
                    }
                }
                report.playlists_history += 1;
            }
            Target::Missing => {
                // Its history stays filed under its YouTube id, as a forgotten playlist's does.
                for snap in &snaps {
                    if add_snapshot(&tx, &vl, forge_id.as_deref(), &pl.title, snap).map_err(sql)? {
                        report.snapshots += 1;
                    }
                }
                match sel.missing_as {
                    MissingAs::Local => match create_local(&tx, pl, current, now).map_err(sql)? {
                        Some(_) => report.local_created += 1,
                        None => report.local_existing += 1,
                    },
                    MissingAs::Account => {
                        let queued = mapped(&tx, MAP_ACCOUNT, &pl.id).map_err(sql)?.is_some();
                        let list = account_list(pl, current);
                        if queued {
                            report.warnings.push(format!("account_already:{}", pl.title));
                        } else if list.tracks.is_empty() {
                            report.warnings.push(format!("account_empty:{}", pl.title));
                        } else {
                            report.account_lists.push(list);
                            report.account_list_ids.push(pl.id.clone());
                            report.account_queued += 1;
                        }
                    }
                    MissingAs::Skip => report.missing_skipped += 1,
                }
            }
        }
        tx.commit().map_err(sql)?;
    }

    // Alerts, for the playlists filed under their YouTube id on an account here.
    if sel.alerts {
        (ctx.progress)("alerts", 0, 1);
        let conn = db.conn();
        let tx = conn.unchecked_transaction().map_err(sql)?;
        for a in &data.db.alerts {
            let Some(m) = mapping::alert(a) else { continue };
            if !on_account.contains(&m.playlist_id) {
                continue;
            }
            let video = data.db.video(&m.video_id);
            let title = m
                .title
                .clone()
                .or_else(|| video.and_then(|v| v.best_title.clone()))
                .unwrap_or_default();
            let channel = m
                .channel
                .clone()
                .or_else(|| video.and_then(|v| v.best_channel_title.clone()))
                .unwrap_or_default();
            let song = SongItem {
                video_id: m.video_id.clone(),
                title,
                artists: artist_of(&channel),
                thumbnail: Some(thumbnail(&m.video_id)),
                unavailable: m.kind == "unavailable",
                ..Default::default()
            };
            report.alerts += tx
                .execute(
                    "INSERT OR IGNORE INTO playlist_alert(dedupe_key, playlist_id, video_id, kind, \
                     song_json, at, from_pos, to_pos, seen) VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                    params![
                        m.dedupe_key,
                        forge_playlist_id(&m.playlist_id),
                        m.video_id,
                        m.kind,
                        serde_json::to_string(&song).ok(),
                        m.at,
                        m.from_pos,
                        m.to_pos,
                        m.seen
                    ],
                )
                .map_err(sql)?;
        }
        tx.commit().map_err(sql)?;
    }

    // Downloads, then a look at whether their files are still there.
    if sel.downloads && !data.db.downloads.is_empty() {
        (ctx.progress)("downloads", 0, 1);
        let mut ids: Vec<String> = Vec::new();
        {
            let conn = db.conn();
            let tx = conn.unchecked_transaction().map_err(sql)?;
            for d in &data.db.downloads {
                // Its video row (the foreign key), even for one the catalog did not have.
                tx.execute(
                    "INSERT OR IGNORE INTO videos(video_id, updated_at) VALUES(?1, ?2)",
                    params![d.video_id, now],
                )
                .map_err(sql)?;
                let status = if d.status == "running" { "queued" } else { d.status.as_str() };
                report.downloads += tx
                    .execute(
                        "INSERT OR IGNORE INTO downloads(video_id, format, status, \
                         requested_quality, thumbnail_mode, dest_dir, file_path, file_size_bytes, \
                         container, error, attempts, created_at, completed_at, last_verified_at) \
                         VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
                        params![
                            d.video_id,
                            d.format,
                            status,
                            d.requested_quality,
                            d.thumbnail_mode,
                            d.dest_dir,
                            d.file_path,
                            d.file_size_bytes,
                            d.container,
                            d.error,
                            d.attempts,
                            d.created_at,
                            d.completed_at,
                            d.last_verified_at
                        ],
                    )
                    .map_err(sql)?;
                ids.push(d.video_id.clone());
            }
            tx.commit().map_err(sql)?;
        }
        ids.sort();
        ids.dedup();
        let checked = crate::download::verify::verify_for_video_ids(db, &ids, now);
        for row in checked.values().flatten() {
            match row.status.as_str() {
                "available" => report.downloads_available += 1,
                "missing" => report.downloads_missing += 1,
                _ => {}
            }
        }
    }

    // Settings, through `set_setting`'s own validation.
    if sel.settings {
        (ctx.progress)("settings", 0, 1);
        let mapped_settings = mapping::settings(&data.db.settings, ctx.forbidden);
        report.settings_skipped = mapped_settings.skipped;
        for (key, value) in mapped_settings.settings {
            match (ctx.validate)(&key, &value) {
                Ok(()) => {
                    db.set_setting(&key, &value);
                    report.settings += 1;
                }
                Err(_) => report
                    .settings_skipped
                    .push(SkippedSetting { key, reason: SkipReason::BadValue }),
            }
        }
    }
    if sel.appearance {
        if let Some(config) = &data.files.config {
            report.theme = Some(mapping::theme(&config.theme));
            report.locale = Some(mapping::locale(&config.lang));
        }
    }

    if sel.data_api {
        (ctx.progress)("data_api", 0, 1);
        apply_data_api(db, data, &account_map, ctx, &mut report)?;
    }
    (ctx.progress)("done", 1, 1);
    Ok(report)
}

/// D-F7, in foreign-key order: accounts, then jobs, then their items; the ledger; the client.
fn apply_data_api(
    db: &Db,
    data: &PfData,
    account_map: &HashMap<String, Option<String>>,
    ctx: &ApplyCtx<'_>,
    report: &mut Report,
) -> Result<(), PfImportError> {
    let now = ctx.now.timestamp();
    {
        let conn = db.conn();
        let tx = conn.unchecked_transaction().map_err(sql)?;
        // 1. Accounts: every channel PlaylistForge knew, `reauth_required` until its token comes
        //    over (`import_tokens`). One Forge already has stays as it is.
        let mut channels: Vec<(String, String, Option<String>, i64)> = data
            .files
            .accounts
            .iter()
            .map(|a| {
                (
                    a.channel_id.clone(),
                    a.title.clone(),
                    a.thumbnail_url.clone(),
                    a.added_at_unix as i64,
                )
            })
            .collect();
        for a in &data.db.accounts {
            if !channels.iter().any(|c| c.0 == a.id) {
                channels.push((a.id.clone(), a.title.clone(), None, epoch_secs(&a.added_at)));
            }
        }
        for (channel, title, thumb, added_at) in &channels {
            let title = if title.is_empty() { channel } else { title };
            report.ytdata_accounts += tx
                .execute(
                    "INSERT INTO ytdata_accounts(channel_id, title, thumb, status, added_at) \
                     VALUES(?1, ?2, ?3, 'reauth_required', ?4) ON CONFLICT(channel_id) DO NOTHING",
                    params![channel, title, thumb, if *added_at > 0 { *added_at } else { now }],
                )
                .map_err(sql)?;
            // Paired with its cookie account, when neither is paired yet.
            if let Some(Some(cookie)) = account_map.get(channel) {
                tx.execute(
                    "UPDATE ytdata_accounts SET linked_account = ?2 WHERE channel_id = ?1 \
                     AND linked_account IS NULL AND NOT EXISTS \
                     (SELECT 1 FROM ytdata_accounts WHERE linked_account = ?2)",
                    params![channel, cookie],
                )
                .map_err(sql)?;
            }
        }

        // 2. Pending jobs with new ids, on the Data API engine (D24), then their items.
        for job in data.db.jobs.iter().filter(|j| mapping::job_is_pending(&j.status)) {
            if mapped(&tx, MAP_JOB, &job.id.to_string()).map_err(sql)?.is_some() {
                continue;
            }
            let mut params_json: Value = serde_json::from_str(&job.params_json).unwrap_or_default();
            if !params_json.is_object() {
                params_json = Value::Object(Default::default());
            }
            params_json[crate::jobs::ENGINE_KEY] = Value::String("ytdata".into());
            let owner: Option<&String> = job.account_id.as_ref().filter(|id| {
                tx.query_row(
                    "SELECT EXISTS(SELECT 1 FROM ytdata_accounts WHERE channel_id = ?1)",
                    [id.as_str()],
                    |r| r.get::<_, bool>(0),
                )
                .unwrap_or(false)
            });
            tx.execute(
                "INSERT INTO jobs(account_id, kind, params_json, status, priority, phase, \
                 total_phases, resume_at, created_at, started_at, finished_at, est_units_total, \
                 spent_units, total_items, done_items, failed_items, skipped_items, last_error, \
                 planned_items, retried_items) VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, \
                 ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20)",
                params![
                    owner,
                    job.kind,
                    params_json.to_string(),
                    mapping::job_status_on_import(&job.status),
                    job.priority,
                    job.phase,
                    job.total_phases,
                    job.resume_at,
                    job.created_at,
                    job.started_at,
                    job.finished_at,
                    job.est_units_total,
                    job.spent_units,
                    job.total_items,
                    job.done_items,
                    job.failed_items,
                    job.skipped_items,
                    job.last_error,
                    job.planned_items,
                    job.retried_items
                ],
            )
            .map_err(sql)?;
            let new_id = tx.last_insert_rowid();
            for item in &job.items {
                // Mid-flight when PlaylistForge closed: tried again, as Forge's runner does.
                let status =
                    if item.status == "in_flight" { "pending" } else { item.status.as_str() };
                tx.execute(
                    "INSERT INTO job_items(job_id, seq, phase, action, params_json, status, \
                     api_result_json, inverse_json, attempts, last_error, updated_at) \
                     VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
                    params![
                        new_id,
                        item.seq,
                        item.phase,
                        item.action,
                        item.params_json,
                        status,
                        item.api_result_json,
                        item.inverse_json,
                        item.attempts,
                        item.last_error,
                        item.updated_at
                    ],
                )
                .map_err(sql)?;
                report.job_items += 1;
            }
            remember(&tx, MAP_JOB, &job.id.to_string(), &new_id.to_string()).map_err(sql)?;
            report.jobs += 1;
        }

        // 3. Today's spend (Pacific day), with no job: PlaylistForge's job ids mean nothing here.
        for q in &data.db.quota_today {
            report.quota_entries += tx
                .execute(
                    "INSERT INTO quota_ledger(ts, endpoint, units, account_id, job_id) \
                     SELECT ?1, ?2, ?3, ?4, NULL WHERE NOT EXISTS (SELECT 1 FROM quota_ledger \
                     WHERE ts = ?1 AND endpoint = ?2 AND units = ?3 AND account_id IS ?4)",
                    params![q.ts, q.endpoint, q.units, q.account_id],
                )
                .map_err(sql)?;
        }
        tx.commit().map_err(sql)?;
    }

    // 4. The OAuth client, only when Forge has none.
    if let (Some(dest), Some(secret)) = (ctx.client_secret_dest, data.files.client_secret.as_ref())
    {
        if !dest.exists() {
            if let Some(dir) = dest.parent() {
                std::fs::create_dir_all(dir).map_err(|e| PfImportError::io(dir, e))?;
            }
            std::fs::write(dest, &secret.raw).map_err(|e| PfImportError::io(dest, e))?;
            report.client_secret_copied = true;
        }
    }
    Ok(())
}

// --- tokens -------------------------------------------------------------------------------------

/// What became of one channel's token.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TokenOutcome {
    pub channel_id: String,
    /// `imported`, `none` (PlaylistForge has no token for it) or `error`.
    pub outcome: &'static str,
    /// `client_mismatch`, `no_pf_client`, `keyring_unavailable`, `store`, `not_pf_account`.
    pub code: Option<&'static str>,
}

/// Bring PlaylistForge's refresh tokens over, **only with `consent`** (the user ticked the box):
/// read with `load` alone from `pf_store` (`credentials::pf_token_store`), saved in Forge's own
/// store and the channel marked connected in `ytdata_accounts`. Only channels PlaylistForge knows.
/// Both stores block: call this off the async runtime.
#[allow(clippy::too_many_arguments)]
pub fn import_tokens(
    db: &Db,
    files: &PfFiles,
    pf_store: &dyn TokenStore,
    forge_store: &dyn TokenStore,
    channel_ids: &[String],
    forge_client_id: Option<&str>,
    consent: bool,
    now: i64,
) -> Result<Vec<TokenOutcome>, String> {
    if !consent {
        return Err("consent_required".into());
    }
    let pf_client = files.client_secret.as_ref().map(|c| c.client_id().to_owned());
    let mut out = Vec::new();
    for id in channel_ids {
        let outcome = |outcome, code| TokenOutcome { channel_id: id.clone(), outcome, code };
        let Some(account) = files.accounts.iter().find(|a| &a.channel_id == id) else {
            out.push(outcome("error", Some("not_pf_account")));
            continue;
        };
        match read_pf_refresh_token(pf_store, id, pf_client.as_deref(), forge_client_id) {
            Ok(Some(token)) => {
                if let Err(e) = forge_store.save(id, token.expose()) {
                    let code =
                        if e.is_keyring_unavailable() { "keyring_unavailable" } else { "store" };
                    out.push(outcome("error", Some(code)));
                    continue;
                }
                let title = if account.title.is_empty() { id.as_str() } else { &account.title };
                let added =
                    if account.added_at_unix > 0 { account.added_at_unix as i64 } else { now };
                crate::ytdata_accounts::upsert(
                    db,
                    id,
                    title,
                    account.thumbnail_url.as_deref(),
                    added,
                )
                .map_err(|e| e.to_string())?;
                out.push(outcome("imported", None));
            }
            Ok(None) => out.push(outcome("none", None)),
            Err(e) => out.push(outcome("error", Some(e.code()))),
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::super::reader::{read_all, tests::pf_install};
    use super::*;
    use crate::db::StoredAccount;
    use ytdata::auth::store::InMemoryTokenStore;

    const ONE: &str = "UCfakeAccountOne00000001";
    const TWO: &str = "UCfakeAccountTwo00000002";

    fn now() -> DateTime<Utc> {
        "2026-10-05T18:00:00Z".parse().unwrap()
    }

    struct Env {
        _tmp: tempfile::TempDir,
        _pf_conn: Connection,
        pf: PathBuf,
        data_dir: PathBuf,
        db: Db,
    }

    fn env() -> Env {
        let tmp = tempfile::tempdir().unwrap();
        let pf = tmp.path().join("PlaylistForge");
        let pf_conn = pf_install(&pf);
        let data_dir = tmp.path().join("forge-data");
        std::fs::create_dir_all(&data_dir).unwrap();
        let db = Db::open(&data_dir.join("forge.db")).unwrap();
        Env { _tmp: tmp, _pf_conn: pf_conn, pf, data_dir, db }
    }

    fn data(e: &Env) -> PfData {
        read_all(&e.pf, &e.data_dir, now()).unwrap()
    }

    /// A saved cookie account of channel One, signed in.
    fn sign_in_one(db: &Db) {
        db.upsert_account(&StoredAccount {
            id: "ga-one".into(),
            session_cookie: "SAPISID=fake".into(),
            data_sync_id: None,
            selected_identity_json: None,
            account_json: Some(format!(r#"{{"name":"Uno","channelId":"{ONE}"}}"#)),
            visitor_data: None,
            added_at: 1,
        })
        .unwrap();
        db.set_setting("active_account", "ga-one");
    }

    fn all_playlists() -> Vec<String> {
        vec!["PLfakeOne".into(), "PLfakeTwo".into(), "PLfakeThree".into()]
    }

    fn run(e: &Env, data: &PfData, sel: &Selection) -> Report {
        let validate = |k: &str, v: &str| crate::commands::validate_setting(k, v);
        let forbidden = [e.pf.clone()];
        let dest = e.data_dir.join("client_secret.json");
        let ctx = ApplyCtx {
            now: now(),
            validate: &validate,
            forbidden: &forbidden,
            client_secret_dest: Some(dest.as_path()),
            progress: &|_: &'static str, _: usize, _: usize| {},
        };
        apply(&e.db, data, sel, &ctx).unwrap()
    }

    fn count(db: &Db, sql_text: &str) -> i64 {
        db.conn().query_row(sql_text, [], |r| r.get(0)).unwrap()
    }

    fn index_rows(db: &Db, vl: &str) -> Vec<(String, Option<i64>, Option<i64>)> {
        let conn = db.conn();
        let mut stmt = conn
            .prepare(
                "SELECT video_id, first_seen, added_at FROM playlist_track WHERE playlist_id = ?1 \
                 ORDER BY video_id",
            )
            .unwrap();
        let rows = stmt.query_map([vl], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).unwrap();
        rows.map(Result::unwrap).collect()
    }

    #[test]
    fn pf_apply_creates_local_playlists_in_order_with_metadata_once() {
        let e = env();
        let data = data(&e);
        // Signed out: nothing pairs, so both playlists are "missing" and become local ones.
        let sel = Selection {
            playlists: vec!["PLfakeOne".into(), "PLfakeTwo".into()],
            ..Selection::default()
        };
        let first = run(&e, &data, &sel);
        assert_eq!((first.local_created, first.local_existing, first.playlists_indexed), (2, 0, 0));

        let locals = e.db.local_playlists();
        assert_eq!(locals.len(), 2);
        let mix = locals.iter().find(|p| p.title == "Fake Mix").unwrap();
        assert_eq!(mix.description, "first");
        let songs: Vec<SongItem> =
            e.db.local_playlist_tracks(mix.id)
                .into_iter()
                .map(|(_, json)| serde_json::from_str(&json).unwrap())
                .collect();
        let order: Vec<&str> = songs.iter().map(|s| s.video_id.as_str()).collect();
        assert_eq!(order, ["vidBBBBBBB2", "vidAAAAAAA1", "vidCCCCCCC3"], "PlaylistForge's order");
        // The dead one keeps the catalog's best title, not "Deleted video", and is greyed out.
        assert_eq!(songs[0].title, "Fake Song Two");
        assert_eq!(songs[0].artists, "Fake Artist B");
        assert_eq!(songs[0].duration.as_deref(), Some("3:07"));
        assert!(songs[0].unavailable);
        assert_eq!(songs[1].artist_id.as_deref(), Some("UCchanA"));
        assert!(!songs[1].unavailable);
        assert_eq!(songs[2].title, "Fake Song Three", "private: catalog title, not the stand-in");
        assert!(songs[2].unavailable);
        let gone = locals.iter().find(|p| p.title == "Gone Mix").unwrap();
        let rows = e.db.local_playlist_tracks(gone.id);
        assert_eq!(rows.len(), 1);
        let only: SongItem = serde_json::from_str(&rows[0].1).unwrap();
        assert_eq!(
            (only.video_id.as_str(), only.title.as_str()),
            ("vidDDDDDDD4", "Fake Song Four")
        );

        // Again: nothing new.
        let second = run(&e, &data, &sel);
        assert_eq!((second.local_created, second.local_existing), (0, 2));
        assert_eq!(e.db.local_playlists().len(), 2);
        assert_eq!(e.db.local_playlist_tracks(mix.id).len(), 3);
        assert_eq!(second.snapshots, 0, "the history went in the first time");
    }

    #[test]
    fn pf_apply_builds_the_index_baseline_and_is_idempotent() {
        let e = env();
        sign_in_one(&e.db);
        let data = data(&e);
        let sel = Selection { playlists: all_playlists(), ..Selection::default() };
        let first = run(&e, &data, &sel);
        // One is the signed-in channel's; Two was deleted on YouTube; Three's account is not here.
        assert_eq!(first.playlists_indexed, 1);
        assert_eq!(first.local_created, 2);

        let rows = index_rows(&e.db, "VLPLfakeOne");
        let synced = epoch_secs("2026-10-05T10:00:00Z");
        assert_eq!(
            rows,
            [
                // In the first snapshot already: since before anything kept track.
                ("vidAAAAAAA1".into(), None, Some(epoch_secs("2026-09-01T10:00:00Z"))),
                ("vidBBBBBBB2".into(), None, Some(epoch_secs("2026-09-02T10:00:00Z"))),
                // Came with the second snapshot.
                ("vidCCCCCCC3".into(), Some(synced), None),
            ]
        );
        let sync = e.db.playlist_syncs()["VLPLfakeOne"];
        assert_eq!((sync.synced_at, sync.item_count), (synced, 3));
        assert_eq!(sync.privacy, Some(Privacy::Public));
        let json: Vec<(String, Option<String>)> = e.db.playlist_songs("VLPLfakeOne");
        let b: SongItem = serde_json::from_str(
            json.iter().find(|(v, _)| v == "vidBBBBBBB2").unwrap().1.as_deref().unwrap(),
        )
        .unwrap();
        assert_eq!(
            (b.title.as_str(), b.set_video_id.as_deref()),
            ("Fake Song Two", Some("PIone1"))
        );

        let snaps = e.db.snapshots("VLPLfakeOne");
        assert_eq!(snaps.len(), 2);
        assert_eq!(snaps[0].item_count, 3, "newest first: the current one");
        assert_eq!(snaps[0].account_id.as_deref(), Some("ga-one"));
        assert_eq!(snaps[0].items[0].v, "vidBBBBBBB2");

        // Alerts about the indexed playlist, `seen` kept; the unknown kind and the one with no
        // playlist are not; the deleted playlist's are not either.
        let alerts = e.db.alert_rows(true);
        assert_eq!(first.alerts, alerts.len());
        assert_eq!(alerts.len(), 5);
        assert!(alerts.iter().all(|a| a.playlist_id == "VLPLfakeOne"));
        let seen: Vec<&str> = alerts.iter().filter(|a| a.seen).map(|a| a.kind.as_str()).collect();
        assert_eq!(seen.len(), 2, "{seen:?}");
        let moved = alerts.iter().find(|a| a.kind == "moved").unwrap();
        assert_eq!((moved.from_pos, moved.to_pos), (Some(0), Some(1)));

        // Settings through set_setting's validation; appearance handed to the UI.
        assert_eq!(e.db.get_setting("drop_mode").as_deref(), Some("move"));
        assert_eq!(e.db.get_setting("budget.safety_margin_percent").as_deref(), Some("5"));
        assert!(e.db.get_setting("downloads.cookies_browser").is_none());
        assert_eq!(first.theme.map(|t| t.id), Some("nord"));
        assert_eq!(first.locale, Some("en"));

        // Downloads, checked on disk: the recorded file is not there, so it is missing.
        assert_eq!(first.downloads, 2);
        assert_eq!((first.downloads_available, first.downloads_missing), (0, 1));

        // Data API: two channels, the two pending jobs, today's two ledger rows, the client.
        assert_eq!((first.ytdata_accounts, first.jobs, first.job_items), (2, 2, 2));
        assert_eq!(first.quota_entries, 2);
        assert!(first.client_secret_copied);
        assert!(e.data_dir.join("client_secret.json").is_file());

        let tables = [
            "SELECT COUNT(*) FROM playlist_track",
            "SELECT COUNT(*) FROM playlist_snapshot",
            "SELECT COUNT(*) FROM playlist_alert",
            "SELECT COUNT(*) FROM downloads",
            "SELECT COUNT(*) FROM local_playlists",
            "SELECT COUNT(*) FROM local_playlist_tracks",
            "SELECT COUNT(*) FROM ytdata_accounts",
            "SELECT COUNT(*) FROM jobs",
            "SELECT COUNT(*) FROM job_items",
            "SELECT COUNT(*) FROM quota_ledger",
        ];
        let before: Vec<i64> = tables.iter().map(|q| count(&e.db, q)).collect();
        let second = run(&e, &data, &sel);
        let after: Vec<i64> = tables.iter().map(|q| count(&e.db, q)).collect();
        assert_eq!(before, after, "applying twice adds nothing");
        assert_eq!(
            (second.snapshots, second.alerts, second.jobs, second.quota_entries, second.downloads),
            (0, 0, 0, 0, 0)
        );
        assert_eq!(second.ytdata_accounts, 0);
        assert!(!second.client_secret_copied);
        assert_eq!(index_rows(&e.db, "VLPLfakeOne"), rows);
    }

    #[test]
    fn pf_apply_d36_the_newer_sync_wins() {
        // Forge synced after PlaylistForge did: its index stays, only older history is added.
        let e = env();
        sign_in_one(&e.db);
        let later = epoch_secs("2026-10-06T08:00:00Z");
        e.db.set_playlist_songs_at("VLPLfakeOne", &[("vidZZZZZZZ9".into(), "{}".into())], later);
        e.db.set_playlist_sync(
            "VLPLfakeOne",
            &crate::db::PlaylistSync {
                synced_at: later,
                item_count: 1,
                added: 0,
                removed: 0,
                moved: 0,
                privacy: None,
            },
        )
        .unwrap();
        let forge_snap = SnapItem {
            v: "vidZZZZZZZ9".into(),
            s: None,
            t: "Z".into(),
            a: String::new(),
            d: None,
            u: false,
            th: None,
        };
        e.db.put_snapshot_if_changed("VLPLfakeOne", Some("ga-one"), None, later, &[forge_snap]);
        let first = data(&e);
        let preview = preview(&e.db, &first, None);
        let one = preview.playlists.iter().find(|p| p.id == "PLfakeOne").unwrap();
        assert_eq!((one.in_forge, one.winner), (true, Winner::Forge));

        let sel = Selection { playlists: vec!["PLfakeOne".into()], ..Selection::default() };
        let report = run(&e, &first, &sel);
        assert_eq!((report.playlists_indexed, report.playlists_kept), (0, 1));
        assert_eq!(index_rows(&e.db, "VLPLfakeOne").len(), 1, "Forge's index untouched");
        assert_eq!(e.db.playlist_syncs()["VLPLfakeOne"].synced_at, later);
        let snaps = e.db.snapshots("VLPLfakeOne");
        assert_eq!(snaps.len(), 3, "PlaylistForge's two older snapshots joined Forge's");
        assert_eq!(snaps[0].items[0].v, "vidZZZZZZZ9", "Forge's stays the newest");

        // Forge synced before PlaylistForge did: PlaylistForge's view replaces it.
        let e = env();
        sign_in_one(&e.db);
        let earlier = epoch_secs("2026-10-01T00:00:00Z");
        e.db.set_playlist_songs_at("VLPLfakeOne", &[("vidZZZZZZZ9".into(), "{}".into())], earlier);
        e.db.set_playlist_sync(
            "VLPLfakeOne",
            &crate::db::PlaylistSync {
                synced_at: earlier,
                item_count: 1,
                added: 0,
                removed: 0,
                moved: 0,
                privacy: None,
            },
        )
        .unwrap();
        let pf = data(&e);
        let report = run(&e, &pf, &sel);
        assert_eq!((report.playlists_indexed, report.playlists_kept), (1, 0));
        let rows: Vec<String> = index_rows(&e.db, "VLPLfakeOne").into_iter().map(|r| r.0).collect();
        assert_eq!(rows, ["vidAAAAAAA1", "vidBBBBBBB2", "vidCCCCCCC3"]);
        assert_eq!(
            e.db.playlist_syncs()["VLPLfakeOne"].synced_at,
            epoch_secs("2026-10-05T10:00:00Z")
        );
    }

    #[test]
    fn pf_apply_d36_rule() {
        assert_eq!(d36_winner(None, None), Winner::Pf);
        assert_eq!(d36_winner(None, Some("2026-10-05T10:00:00Z")), Winner::Pf);
        assert_eq!(d36_winner(Some(5), None), Winner::Forge);
        let pf = epoch_secs("2026-10-05T10:00:00Z");
        assert_eq!(d36_winner(Some(pf - 1), Some("2026-10-05T10:00:00Z")), Winner::Pf);
        assert_eq!(d36_winner(Some(pf), Some("2026-10-05T10:00:00Z")), Winner::Forge, "a tie");
        assert_eq!(d36_winner(Some(pf + 1), Some("2026-10-05T10:00:00Z")), Winner::Forge);
    }

    #[test]
    fn pf_apply_accounts_go_in_before_jobs_and_their_items() {
        let e = env();
        let data = data(&e);
        let sel = Selection { playlists: Vec::new(), ..Selection::default() };
        let report = run(&e, &data, &sel);
        assert_eq!((report.jobs, report.job_items), (2, 2));
        // Foreign keys are on (Db::open) and nothing points nowhere.
        let fk_on: i64 = count(&e.db, "PRAGMA foreign_keys");
        assert_eq!(fk_on, 1);
        let conn = e.db.conn();
        let broken: Vec<String> = conn
            .prepare("PRAGMA foreign_key_check")
            .unwrap()
            .query_map([], |r| r.get::<_, String>(0))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        assert!(broken.is_empty(), "{broken:?}");
        let jobs: Vec<(i64, Option<String>, String, String, Option<String>)> = conn
            .prepare("SELECT id, account_id, status, params_json, resume_at FROM jobs ORDER BY id")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        assert_eq!(jobs.len(), 2, "the completed one is history, not brought over");
        assert_eq!(jobs[0].1.as_deref(), Some(ONE));
        assert_eq!(jobs[0].2, "queued");
        let params: Value = serde_json::from_str(&jobs[0].3).unwrap();
        assert_eq!(params["engine"], "ytdata");
        assert_eq!(params["playlist_id"], "PLfakeOne");
        assert_eq!(jobs[1].1.as_deref(), Some(TWO));
        assert_eq!(
            (jobs[1].2.as_str(), jobs[1].4.as_deref()),
            ("waiting_quota", Some("2026-10-06T07:00:00Z"))
        );
        let items: Vec<(i64, i64)> = conn
            .prepare("SELECT job_id, seq FROM job_items ORDER BY seq")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        assert_eq!(items, [(jobs[0].0, 0), (jobs[0].0, 1)], "remapped to the new job id");
        let status: String = conn
            .query_row("SELECT status FROM ytdata_accounts WHERE channel_id = ?1", [ONE], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(status, "reauth_required", "no token yet");
        let null_jobs: i64 = conn
            .query_row("SELECT COUNT(*) FROM quota_ledger WHERE job_id IS NOT NULL", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(null_jobs, 0);
    }

    #[test]
    fn pf_apply_missing_playlists_on_the_account_are_queued_by_video_id() {
        let e = env();
        let data = data(&e);
        let sel = Selection {
            playlists: vec!["PLfakeOne".into(), "PLfakeThree".into()],
            missing_as: MissingAs::Account,
            ..Selection::default()
        };
        let report = run(&e, &data, &sel);
        assert_eq!(report.account_queued, 1, "the empty one has nothing to create");
        let list = &report.account_lists[0];
        assert_eq!(list.name, "Fake Mix");
        let ids: Vec<&str> = list.tracks.iter().filter_map(|t| t.video_id.as_deref()).collect();
        assert_eq!(ids, ["vidAAAAAAA1"], "greyed-out videos are left out");
        assert_eq!(e.db.local_playlists().len(), 0);
        mark_account_lists(&e.db, &report.account_list_ids).unwrap();
        let again = run(&e, &data, &sel);
        assert_eq!(again.account_queued, 0, "queued once");
    }

    #[test]
    fn pf_apply_tokens_only_with_consent() {
        let e = env();
        let data = data(&e);
        let pf_store = InMemoryTokenStore::default();
        pf_store.save(ONE, "FAKE-pf-refresh-token").unwrap();
        let forge_store = InMemoryTokenStore::default();
        let ids = vec![ONE.to_string(), TWO.to_string(), "UCnotPf".to_string()];

        let refused =
            import_tokens(&e.db, &data.files, &pf_store, &forge_store, &ids, None, false, 1);
        assert_eq!(refused.unwrap_err(), "consent_required");
        assert_eq!(forge_store.load(ONE).unwrap(), None, "nothing saved without consent");
        assert!(crate::ytdata_accounts::list(&e.db).unwrap().is_empty());

        let got = import_tokens(&e.db, &data.files, &pf_store, &forge_store, &ids, None, true, 1)
            .unwrap();
        let by: HashMap<&str, &TokenOutcome> =
            got.iter().map(|o| (o.channel_id.as_str(), o)).collect();
        assert_eq!(by[ONE].outcome, "imported");
        assert_eq!(by[TWO].outcome, "none");
        assert_eq!(by["UCnotPf"].code, Some("not_pf_account"));
        assert_eq!(forge_store.load(ONE).unwrap().as_deref(), Some("FAKE-pf-refresh-token"));
        assert_eq!(pf_store.load(ONE).unwrap().as_deref(), Some("FAKE-pf-refresh-token"), "kept");
        let acc = crate::ytdata_accounts::get(&e.db, ONE).unwrap().unwrap();
        assert_eq!(acc.status, ytdata::auth::accounts::AccountStatus::Connected);
        assert_eq!(acc.title, "Cuenta Uno");
        let debug = format!("{got:?}");
        assert!(!debug.contains("FAKE-pf-refresh-token"), "{debug}");

        // Another OAuth client in Forge: the token would not work there, so it is not taken.
        let other = InMemoryTokenStore::default();
        let got = import_tokens(
            &e.db,
            &data.files,
            &pf_store,
            &other,
            &[ONE.to_string()],
            Some("FAKE-forge-client.apps.googleusercontent.com"),
            true,
            1,
        )
        .unwrap();
        assert_eq!(got[0].code, Some("client_mismatch"));
        assert_eq!(other.load(ONE).unwrap(), None);
    }

    #[test]
    fn pf_apply_preview_pairs_accounts_and_hides_secrets() {
        let e = env();
        sign_in_one(&e.db);
        let data = data(&e);
        let p = preview(&e.db, &data, None);
        let one = p.accounts.iter().find(|a| a.id == ONE).unwrap();
        assert_eq!(one.forge_account.as_deref(), Some("ga-one"));
        assert!(one.data_api);
        assert_eq!(p.accounts.iter().find(|a| a.id == TWO).unwrap().forge_account, None);
        assert_eq!(p.playlists.len(), 3);
        assert!(p.playlists.iter().all(|pl| !pl.in_forge && pl.winner == Winner::Pf));
        assert!(p.pf_client_secret && !p.forge_client_secret && p.tokens_compatible);
        assert_eq!(p.forge_accounts.len(), 1);
        assert!(p.forge_accounts[0].active);
        let json = serde_json::to_string(&p).unwrap();
        assert!(!json.contains("FAKE-pf-secret"), "{json}");
        assert!(!json.contains("SAPISID"), "{json}");
        // The user's pick overrides the automatic pairing; an unknown Forge id pairs nothing.
        let chosen: HashMap<String, Option<String>> =
            [(TWO.to_string(), Some("ga-one".to_string())), (ONE.to_string(), Some("nope".into()))]
                .into();
        let forge = forge_accounts(&e.db);
        let map = effective_map(&[ONE, TWO], &forge, &chosen);
        assert_eq!(map[TWO].as_deref(), Some("ga-one"));
        assert_eq!(map[ONE], None);
    }
}
