//! Reading PlaylistForge's data: a read-only copy of `forge.db`, its JSON files and its backups.
//!
//! [`stage`] copies `forge.db`, `forge.db-wal` and `forge.db-shm` (the WAL holds whatever
//! PlaylistForge had not checkpointed yet) into `<data>/tmp/pf-import-<ts>/` and opens the copy
//! with `SQLITE_OPEN_READ_ONLY | SQLITE_OPEN_NO_MUTEX`. The original is only ever read by the file
//! copy. `user_version` 0 is not PlaylistForge; above [`PF_SCHEMA_VERSION`] is refused (D34);
//! below it, missing tables and columns read as empty or as their defaults (`PRAGMA table_info`).
//! `PRAGMA quick_check` must answer `ok`.
//!
//! [`Staged::finish`] (or dropping the [`Staged`]) deletes that folder, and only after checking
//! it is a direct child of `<data>/tmp` (both canonicalized) whose name starts with `pf-import-`.
//!
//! Every date comes out in Forge's text form (`YYYY-MM-DDTHH:MM:SSZ`, [`mapping::forge_date`]).

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use rusqlite::{Connection, OpenFlags};
use serde::{Deserialize, Serialize};

use super::mapping::{forge_date, forge_date_opt};
use super::{
    PfImportError, PF_ACCOUNTS_FILE, PF_BACKUPS_DIR, PF_CLIENT_SECRET_FILE, PF_CONFIG_FILE,
    PF_DB_FILE, PF_SCHEMA_VERSION, WORK_PREFIX,
};
use crate::backups::SnapshotExport;

/// The three files of a SQLite database in WAL mode.
const DB_SUFFIXES: [&str; 3] = ["", "-wal", "-shm"];

// --- the model ----------------------------------------------------------------------------------

/// `accounts`: one per connected YouTube channel (id = channel id).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PfAccount {
    pub id: String,
    pub title: String,
    pub email_hint: Option<String>,
    pub added_at: String,
    /// The last successful sync: D36 weighs it against Forge's `playlist_sync.synced_at`.
    pub last_sync_at: Option<String>,
}

/// `playlists`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PfPlaylist {
    pub id: String,
    pub account_id: String,
    pub title: String,
    pub description: String,
    /// `public|unlisted|private` (`db::Privacy::parse` reads it).
    pub privacy: String,
    pub item_count: i64,
    pub thumb_url: Option<String>,
    pub first_seen: String,
    pub last_seen: String,
    pub deleted_remotely: bool,
}

/// `videos`: the never-degrading catalog (`best_*`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PfVideo {
    pub id: String,
    pub best_title: Option<String>,
    pub best_channel_id: Option<String>,
    pub best_channel_title: Option<String>,
    pub duration_s: Option<i64>,
    pub published_at: Option<String>,
    /// `available|private|deleted|region_blocked|age_restricted|unknown`.
    pub status: String,
    pub status_changed_at: Option<String>,
    pub first_seen: String,
    pub last_seen: String,
}

/// One row of a snapshot, with its video's catalog entry joined in.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PfSnapshotItem {
    pub position: i64,
    pub playlist_item_id: String,
    pub video_id: String,
    pub title_at_time: Option<String>,
    pub channel_at_time: Option<String>,
    pub added_at: Option<String>,
    pub best_title: Option<String>,
    pub best_channel_id: Option<String>,
    pub best_channel_title: Option<String>,
    pub duration_s: Option<i64>,
    /// The video's status (`unknown` when it has no catalog row).
    pub status: String,
}

/// `playlist_snapshots` + `snapshot_items`. The one with `is_current` is the playlist as
/// PlaylistForge last saw it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PfSnapshot {
    pub id: i64,
    pub playlist_id: String,
    pub taken_at: String,
    pub content_hash: String,
    pub item_count: i64,
    pub is_current: bool,
    /// In position order.
    pub items: Vec<PfSnapshotItem>,
}

/// `alerts`. [`super::mapping::alert`] turns one into Forge's model.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PfAlert {
    pub id: i64,
    pub account_id: String,
    pub playlist_id: Option<String>,
    /// PlaylistForge's kind (`video_deleted`, `item_moved`...), untranslated.
    pub kind: String,
    pub video_id: Option<String>,
    pub payload_json: String,
    pub created_at: String,
    pub seen: bool,
    /// Empty before migration 0002.
    pub dedupe_key: String,
}

/// `downloads` (migration 0004; none before it).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PfDownload {
    pub video_id: String,
    /// `audio|video`.
    pub format: String,
    /// `queued|running|available|error|missing`.
    pub status: String,
    pub requested_quality: String,
    pub thumbnail_mode: String,
    pub dest_dir: String,
    pub file_path: Option<String>,
    pub file_size_bytes: Option<i64>,
    pub container: Option<String>,
    pub error: Option<String>,
    pub attempts: i64,
    pub created_at: String,
    pub completed_at: Option<String>,
    pub last_verified_at: Option<String>,
}

/// `job_items`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PfJobItem {
    pub id: i64,
    pub job_id: i64,
    pub seq: i64,
    pub phase: i64,
    pub action: String,
    pub params_json: String,
    pub status: String,
    pub api_result_json: Option<String>,
    pub inverse_json: Option<String>,
    pub attempts: i64,
    pub last_error: Option<String>,
    pub updated_at: String,
}

/// `jobs`, with their items. Ids are PlaylistForge's: the importer gives them new ones and remaps
/// `job_items.job_id`. Insert accounts first (`jobs.account_id` references them), then the job,
/// then its items.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PfJob {
    pub id: i64,
    pub account_id: Option<String>,
    pub kind: String,
    pub params_json: String,
    pub status: String,
    pub priority: i64,
    pub phase: i64,
    pub total_phases: i64,
    pub resume_at: Option<String>,
    pub created_at: String,
    pub started_at: Option<String>,
    pub finished_at: Option<String>,
    pub est_units_total: i64,
    pub spent_units: i64,
    pub total_items: i64,
    pub done_items: i64,
    pub failed_items: i64,
    pub skipped_items: i64,
    pub last_error: Option<String>,
    /// 0 before migration 0005.
    pub planned_items: i64,
    pub retried_items: i64,
    /// In `(phase, seq)` order.
    pub items: Vec<PfJobItem>,
}

/// A `quota_ledger` row of the current Pacific-time quota day (D-F7). Its `job_id` is not
/// carried: PlaylistForge's job ids mean nothing in Forge.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PfQuotaEntry {
    pub ts: String,
    pub endpoint: String,
    pub units: i64,
    pub account_id: Option<String>,
}

/// What `forge.db` holds.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PfDb {
    pub user_version: i64,
    pub accounts: Vec<PfAccount>,
    pub playlists: Vec<PfPlaylist>,
    pub videos: Vec<PfVideo>,
    /// Every snapshot, current or not, ordered by playlist then `taken_at`.
    pub snapshots: Vec<PfSnapshot>,
    pub alerts: Vec<PfAlert>,
    pub downloads: Vec<PfDownload>,
    /// `settings` as stored, by key. [`super::mapping::settings`] translates them.
    pub settings: Vec<(String, String)>,
    pub jobs: Vec<PfJob>,
    /// Today's (Pacific day) quota spend.
    pub quota_today: Vec<PfQuotaEntry>,
}

impl PfDb {
    /// The playlist as PlaylistForge last saw it.
    pub fn current_snapshot(&self, playlist_id: &str) -> Option<&PfSnapshot> {
        self.snapshots.iter().find(|s| s.is_current && s.playlist_id == playlist_id)
    }

    pub fn setting(&self, key: &str) -> Option<&str> {
        self.settings.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
    }

    pub fn video(&self, id: &str) -> Option<&PfVideo> {
        self.videos.iter().find(|v| v.id == id)
    }
}

/// `config.json`. Missing fields take PlaylistForge's own defaults.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PfConfig {
    #[serde(default = "default_theme")]
    pub theme: String,
    #[serde(default = "default_lang")]
    pub lang: String,
    #[serde(default)]
    pub active_account_id: Option<String>,
}

fn default_theme() -> String {
    "dark".to_string()
}

fn default_lang() -> String {
    "es".to_string()
}

/// One `accounts.json` entry (pf-yt `Account`): non-secret metadata of a Data API account.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PfJsonAccount {
    pub channel_id: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub thumbnail_url: Option<String>,
    /// `connected|reauth_required`.
    #[serde(default = "default_status")]
    pub status: String,
    #[serde(default)]
    pub added_at_unix: u64,
}

fn default_status() -> String {
    "connected".to_string()
}

/// PlaylistForge's `client_secret.json`: the parsed client plus its bytes, which the importer
/// copies verbatim (as `ytdata::client_secret::import` does). `Debug` never shows the secret.
#[derive(Clone)]
pub struct PfClientSecret {
    pub raw: String,
    pub parsed: ytdata::client_secret::ClientSecretFile,
}

impl PfClientSecret {
    pub fn client_id(&self) -> &str {
        &self.parsed.installed.client_id
    }
}

impl fmt::Debug for PfClientSecret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PfClientSecret")
            .field("client_id", &self.parsed.masked_client_id())
            .finish_non_exhaustive()
    }
}

/// PlaylistForge's JSON files. A missing file is `None`/empty; an unreadable one too, with a
/// short code in `problems` (`config_unreadable`, `accounts_unreadable`, `client_secret_invalid`).
#[derive(Debug, Clone, Default)]
pub struct PfFiles {
    pub config: Option<PfConfig>,
    pub accounts: Vec<PfJsonAccount>,
    pub client_secret: Option<PfClientSecret>,
    pub problems: Vec<&'static str>,
}

/// One JSON backup (PlaylistForge's `SnapshotExport`, the same format Forge writes).
#[derive(Debug, Clone, PartialEq)]
pub struct PfBackup {
    pub path: PathBuf,
    pub export: SnapshotExport,
}

/// The backups folder's contents.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PfBackups {
    pub dir: PathBuf,
    pub files: Vec<PfBackup>,
    /// Backup-named files that did not parse.
    pub unreadable: usize,
}

/// Everything read from a PlaylistForge installation.
#[derive(Debug, Clone)]
pub struct PfData {
    pub pf_dir: PathBuf,
    pub db: PfDb,
    pub files: PfFiles,
    pub backups: PfBackups,
}

/// Counts for the import preview. Nothing secret.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PfSummary {
    pub user_version: i64,
    pub accounts: usize,
    pub playlists: usize,
    pub playlists_with_items: usize,
    pub items: usize,
    pub snapshots: usize,
    pub alerts: usize,
    pub downloads: usize,
    pub jobs_pending: usize,
    pub jobs_total: usize,
    pub quota_units_today: i64,
    pub settings: usize,
    pub backups: usize,
    pub has_client_secret: bool,
    pub json_accounts: usize,
}

impl PfData {
    pub fn summary(&self) -> PfSummary {
        let current: Vec<&PfSnapshot> = self.db.snapshots.iter().filter(|s| s.is_current).collect();
        PfSummary {
            user_version: self.db.user_version,
            accounts: self.db.accounts.len(),
            playlists: self.db.playlists.len(),
            playlists_with_items: current.iter().filter(|s| !s.items.is_empty()).count(),
            items: current.iter().map(|s| s.items.len()).sum(),
            snapshots: self.db.snapshots.len(),
            alerts: self.db.alerts.len(),
            downloads: self.db.downloads.len(),
            jobs_pending: self
                .db
                .jobs
                .iter()
                .filter(|j| super::mapping::job_is_pending(&j.status))
                .count(),
            jobs_total: self.db.jobs.len(),
            quota_units_today: self.db.quota_today.iter().map(|q| q.units).sum(),
            settings: self.db.settings.len(),
            backups: self.backups.files.len(),
            has_client_secret: self.files.client_secret.is_some(),
            json_accounts: self.files.accounts.len(),
        }
    }
}

// --- staging ------------------------------------------------------------------------------------

/// A read-only copy of PlaylistForge's database under `<data>/tmp/pf-import-<ts>/`. Deleted by
/// [`Self::finish`], or when dropped.
pub struct Staged {
    conn: Option<Connection>,
    work: PathBuf,
    tmp_root: PathBuf,
    user_version: i64,
    removed: bool,
}

impl fmt::Debug for Staged {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Staged")
            .field("work", &self.work)
            .field("user_version", &self.user_version)
            .finish_non_exhaustive()
    }
}

/// Copy `<pf_dir>/forge.db` (with its `-wal`/`-shm`) under `<data_dir>/tmp/pf-import-<ts>/`, open
/// the copy read-only and check it. The caller should have asked to close PlaylistForge first
/// ([`super::pf_running`]). On any failure the copy is already gone.
pub fn stage(pf_dir: &Path, data_dir: &Path) -> Result<Staged, PfImportError> {
    let src = pf_dir.join(PF_DB_FILE);
    match fs::symlink_metadata(&src) {
        Ok(m) if m.is_file() => {}
        _ => return Err(PfImportError::NotFound(src)),
    }
    let tmp_root = data_dir.join("tmp");
    // Never stage inside PlaylistForge's own folder: that would be writing there. Checked before
    // anything is created.
    if crate::backups::is_forbidden(&tmp_root, &[pf_dir.to_path_buf()]) {
        return Err(PfImportError::UnsafePath(tmp_root));
    }
    fs::create_dir_all(&tmp_root).map_err(|e| PfImportError::io(&tmp_root, e))?;
    let work = new_work_dir(&tmp_root)?;
    let mut staged =
        Staged { conn: None, work: work.clone(), tmp_root, user_version: 0, removed: false };
    for suffix in DB_SUFFIXES {
        let from = with_suffix(&src, suffix);
        match fs::symlink_metadata(&from) {
            Ok(m) if m.is_file() => {
                let to = work.join(format!("{PF_DB_FILE}{suffix}"));
                fs::copy(&from, &to).map_err(|e| PfImportError::io(&from, e))?;
            }
            _ => {}
        }
    }
    let conn = Connection::open_with_flags(
        work.join(PF_DB_FILE),
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    staged.user_version = check(&conn)?;
    staged.conn = Some(conn);
    Ok(staged)
}

/// `user_version`, the tables PlaylistForge always has, and `quick_check`.
fn check(conn: &Connection) -> Result<i64, PfImportError> {
    let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    if version <= 0 {
        return Err(PfImportError::NotPlaylistForge);
    }
    if version > PF_SCHEMA_VERSION {
        return Err(PfImportError::TooNew { found: version, known: PF_SCHEMA_VERSION });
    }
    for table in ["accounts", "playlists", "settings"] {
        if columns(conn, table)?.is_empty() {
            return Err(PfImportError::NotPlaylistForge);
        }
    }
    let mut stmt = conn.prepare("PRAGMA quick_check")?;
    let verdict: Vec<String> =
        stmt.query_map([], |r| r.get::<_, String>(0))?.take(5).collect::<Result<_, _>>()?;
    if verdict.first().map(String::as_str) != Some("ok") {
        return Err(PfImportError::Corrupt(verdict.join("; ")));
    }
    Ok(version)
}

/// A fresh `pf-import-<millis>[-n]` under `tmp_root`, created exclusively.
fn new_work_dir(tmp_root: &Path) -> Result<PathBuf, PfImportError> {
    let ts = Utc::now().timestamp_millis();
    for n in 0..100 {
        let suffix = if n == 0 { String::new() } else { format!("-{n}") };
        let dir = tmp_root.join(format!("{WORK_PREFIX}{ts}{suffix}"));
        match fs::create_dir(&dir) {
            Ok(()) => return Ok(dir),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(PfImportError::io(&dir, e)),
        }
    }
    Err(PfImportError::UnsafePath(tmp_root.join(format!("{WORK_PREFIX}{ts}"))))
}

fn with_suffix(p: &Path, suffix: &str) -> PathBuf {
    let mut s = p.as_os_str().to_owned();
    s.push(suffix);
    PathBuf::from(s)
}

/// Delete a staging folder: only a real directory (not a link) directly under `tmp_root`, both
/// canonicalized, named `pf-import-…`. A folder already gone is fine.
pub(crate) fn remove_work_dir(work: &Path, tmp_root: &Path) -> Result<(), PfImportError> {
    let named_ours = work
        .file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.starts_with(WORK_PREFIX) && n.len() > WORK_PREFIX.len());
    if !named_ours {
        return Err(PfImportError::UnsafePath(work.to_path_buf()));
    }
    let meta = match fs::symlink_metadata(work) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(PfImportError::io(work, e)),
    };
    if !meta.is_dir() {
        return Err(PfImportError::UnsafePath(work.to_path_buf()));
    }
    let canon_work = work.canonicalize().map_err(|e| PfImportError::io(work, e))?;
    let canon_tmp = tmp_root.canonicalize().map_err(|e| PfImportError::io(tmp_root, e))?;
    if canon_work.parent() != Some(canon_tmp.as_path()) {
        return Err(PfImportError::UnsafePath(work.to_path_buf()));
    }
    fs::remove_dir_all(&canon_work).map_err(|e| PfImportError::io(work, e))
}

/// Delete `pf-import-*` folders an interrupted import left under `<data_dir>/tmp`, with the same
/// checks as [`Staged::finish`]. Answers how many went.
pub fn sweep_stale(data_dir: &Path) -> usize {
    let tmp_root = data_dir.join("tmp");
    let Ok(entries) = fs::read_dir(&tmp_root) else { return 0 };
    entries
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().starts_with(WORK_PREFIX))
        .filter(|e| remove_work_dir(&e.path(), &tmp_root).is_ok())
        .count()
}

impl Staged {
    pub fn user_version(&self) -> i64 {
        self.user_version
    }

    pub fn work_dir(&self) -> &Path {
        &self.work
    }

    fn conn(&self) -> &Connection {
        self.conn.as_ref().expect("a Staged always holds its connection until finished")
    }

    /// Read the whole database. `now` decides the quota day.
    pub fn read(&self, now: DateTime<Utc>) -> Result<PfDb, PfImportError> {
        let conn = self.conn();
        let videos = read_videos(conn)?;
        Ok(PfDb {
            user_version: self.user_version,
            accounts: read_accounts(conn)?,
            playlists: read_playlists(conn)?,
            snapshots: read_snapshots(conn)?,
            alerts: read_alerts(conn)?,
            downloads: read_downloads(conn)?,
            settings: read_settings(conn)?,
            jobs: read_jobs(conn)?,
            quota_today: read_quota_today(conn, now)?,
            videos,
        })
    }

    /// Close the copy and delete its folder.
    pub fn finish(mut self) -> Result<(), PfImportError> {
        self.cleanup()
    }

    fn cleanup(&mut self) -> Result<(), PfImportError> {
        if self.removed {
            return Ok(());
        }
        // Close first: Windows will not delete a file SQLite still holds.
        drop(self.conn.take());
        self.removed = true;
        remove_work_dir(&self.work, &self.tmp_root)
    }
}

impl Drop for Staged {
    fn drop(&mut self) {
        if let Err(e) = self.cleanup() {
            tracing::warn!(error = %e, "could not remove the PlaylistForge import copy");
        }
    }
}

// --- reading the database -----------------------------------------------------------------------

/// A table's columns (`PRAGMA table_info`); empty when the table does not exist. Table names are
/// this module's constants, never input.
fn columns(conn: &Connection, table: &str) -> rusqlite::Result<HashSet<String>> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let rows = stmt.query_map([], |r| r.get::<_, String>(1))?;
    rows.collect()
}

/// `name`, or `fallback AS name` when the column is not there (an older schema).
fn pick(cols: &HashSet<String>, name: &str, fallback: &str) -> String {
    if cols.contains(name) {
        name.to_string()
    } else {
        format!("{fallback} AS {name}")
    }
}

fn read_accounts(conn: &Connection) -> rusqlite::Result<Vec<PfAccount>> {
    let mut stmt = conn.prepare(
        "SELECT id, title, email_hint, added_at, last_sync_at FROM accounts ORDER BY added_at, id",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok(PfAccount {
            id: r.get(0)?,
            title: r.get(1)?,
            email_hint: r.get(2)?,
            added_at: forge_date(&r.get::<_, String>(3)?),
            last_sync_at: forge_date_opt(r.get::<_, Option<String>>(4)?.as_deref()),
        })
    })?;
    rows.collect()
}

fn read_playlists(conn: &Connection) -> rusqlite::Result<Vec<PfPlaylist>> {
    let mut stmt = conn.prepare(
        "SELECT id, account_id, title, description, privacy, item_count, thumb_url, first_seen, \
         last_seen, deleted_remotely FROM playlists ORDER BY account_id, title, id",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok(PfPlaylist {
            id: r.get(0)?,
            account_id: r.get(1)?,
            title: r.get(2)?,
            description: r.get(3)?,
            privacy: r.get(4)?,
            item_count: r.get(5)?,
            thumb_url: r.get(6)?,
            first_seen: forge_date(&r.get::<_, String>(7)?),
            last_seen: forge_date(&r.get::<_, String>(8)?),
            deleted_remotely: r.get::<_, i64>(9)? != 0,
        })
    })?;
    rows.collect()
}

fn read_videos(conn: &Connection) -> rusqlite::Result<Vec<PfVideo>> {
    if columns(conn, "videos")?.is_empty() {
        return Ok(Vec::new());
    }
    let mut stmt = conn.prepare(
        "SELECT id, best_title, best_channel_id, best_channel_title, duration_s, published_at, \
         status, status_changed_at, first_seen, last_seen FROM videos ORDER BY id",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok(PfVideo {
            id: r.get(0)?,
            best_title: r.get(1)?,
            best_channel_id: r.get(2)?,
            best_channel_title: r.get(3)?,
            duration_s: r.get(4)?,
            published_at: forge_date_opt(r.get::<_, Option<String>>(5)?.as_deref()),
            status: r.get(6)?,
            status_changed_at: forge_date_opt(r.get::<_, Option<String>>(7)?.as_deref()),
            first_seen: forge_date(&r.get::<_, String>(8)?),
            last_seen: forge_date(&r.get::<_, String>(9)?),
        })
    })?;
    rows.collect()
}

fn read_snapshots(conn: &Connection) -> rusqlite::Result<Vec<PfSnapshot>> {
    if columns(conn, "playlist_snapshots")?.is_empty()
        || columns(conn, "snapshot_items")?.is_empty()
    {
        return Ok(Vec::new());
    }
    let mut items: HashMap<i64, Vec<PfSnapshotItem>> = HashMap::new();
    {
        let mut stmt = conn.prepare(
            "SELECT si.snapshot_id, si.position, si.playlist_item_id, si.video_id, \
             si.title_at_time, si.channel_at_time, si.added_at, v.best_title, v.best_channel_id, \
             v.best_channel_title, v.duration_s, COALESCE(v.status, 'unknown') \
             FROM snapshot_items si LEFT JOIN videos v ON v.id = si.video_id \
             ORDER BY si.snapshot_id, si.position",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                PfSnapshotItem {
                    position: r.get(1)?,
                    playlist_item_id: r.get(2)?,
                    video_id: r.get(3)?,
                    title_at_time: r.get(4)?,
                    channel_at_time: r.get(5)?,
                    added_at: forge_date_opt(r.get::<_, Option<String>>(6)?.as_deref()),
                    best_title: r.get(7)?,
                    best_channel_id: r.get(8)?,
                    best_channel_title: r.get(9)?,
                    duration_s: r.get(10)?,
                    status: r.get(11)?,
                },
            ))
        })?;
        for row in rows {
            let (snapshot_id, item) = row?;
            items.entry(snapshot_id).or_default().push(item);
        }
    }
    let mut stmt = conn.prepare(
        "SELECT id, playlist_id, taken_at, content_hash, item_count, is_current \
         FROM playlist_snapshots ORDER BY playlist_id, taken_at, id",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok(PfSnapshot {
            id: r.get(0)?,
            playlist_id: r.get(1)?,
            taken_at: forge_date(&r.get::<_, String>(2)?),
            content_hash: r.get(3)?,
            item_count: r.get(4)?,
            is_current: r.get::<_, i64>(5)? != 0,
            items: Vec::new(),
        })
    })?;
    let mut out = Vec::new();
    for row in rows {
        let mut snap = row?;
        snap.items = items.remove(&snap.id).unwrap_or_default();
        out.push(snap);
    }
    Ok(out)
}

fn read_alerts(conn: &Connection) -> rusqlite::Result<Vec<PfAlert>> {
    let cols = columns(conn, "alerts")?;
    if cols.is_empty() {
        return Ok(Vec::new());
    }
    let sql = format!(
        "SELECT id, account_id, playlist_id, kind, video_id, payload_json, created_at, seen, {} \
         FROM alerts ORDER BY created_at, id",
        pick(&cols, "dedupe_key", "''")
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map([], |r| {
        Ok(PfAlert {
            id: r.get(0)?,
            account_id: r.get(1)?,
            playlist_id: r.get(2)?,
            kind: r.get(3)?,
            video_id: r.get(4)?,
            payload_json: r.get(5)?,
            created_at: forge_date(&r.get::<_, String>(6)?),
            seen: r.get::<_, i64>(7)? != 0,
            dedupe_key: r.get(8)?,
        })
    })?;
    rows.collect()
}

fn read_downloads(conn: &Connection) -> rusqlite::Result<Vec<PfDownload>> {
    if columns(conn, "downloads")?.is_empty() {
        return Ok(Vec::new());
    }
    let mut stmt = conn.prepare(
        "SELECT video_id, format, status, requested_quality, thumbnail_mode, dest_dir, file_path, \
         file_size_bytes, container, error, attempts, created_at, completed_at, last_verified_at \
         FROM downloads ORDER BY created_at, video_id, format",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok(PfDownload {
            video_id: r.get(0)?,
            format: r.get(1)?,
            status: r.get(2)?,
            requested_quality: r.get(3)?,
            thumbnail_mode: r.get(4)?,
            dest_dir: r.get(5)?,
            file_path: r.get(6)?,
            file_size_bytes: r.get(7)?,
            container: r.get(8)?,
            error: r.get(9)?,
            attempts: r.get(10)?,
            created_at: forge_date(&r.get::<_, String>(11)?),
            completed_at: forge_date_opt(r.get::<_, Option<String>>(12)?.as_deref()),
            last_verified_at: forge_date_opt(r.get::<_, Option<String>>(13)?.as_deref()),
        })
    })?;
    rows.collect()
}

fn read_settings(conn: &Connection) -> rusqlite::Result<Vec<(String, String)>> {
    let mut stmt = conn.prepare("SELECT key, value FROM settings ORDER BY key")?;
    let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
    rows.collect()
}

fn read_jobs(conn: &Connection) -> rusqlite::Result<Vec<PfJob>> {
    let cols = columns(conn, "jobs")?;
    if cols.is_empty() {
        return Ok(Vec::new());
    }
    let mut items: HashMap<i64, Vec<PfJobItem>> = HashMap::new();
    if !columns(conn, "job_items")?.is_empty() {
        let mut stmt = conn.prepare(
            "SELECT id, job_id, seq, phase, action, params_json, status, api_result_json, \
             inverse_json, attempts, last_error, updated_at FROM job_items \
             ORDER BY job_id, phase, seq, id",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(PfJobItem {
                id: r.get(0)?,
                job_id: r.get(1)?,
                seq: r.get(2)?,
                phase: r.get(3)?,
                action: r.get(4)?,
                params_json: r.get(5)?,
                status: r.get(6)?,
                api_result_json: r.get(7)?,
                inverse_json: r.get(8)?,
                attempts: r.get(9)?,
                last_error: r.get(10)?,
                updated_at: forge_date(&r.get::<_, String>(11)?),
            })
        })?;
        for row in rows {
            let item = row?;
            items.entry(item.job_id).or_default().push(item);
        }
    }
    let sql = format!(
        "SELECT id, account_id, kind, params_json, status, priority, phase, total_phases, \
         resume_at, created_at, started_at, finished_at, est_units_total, spent_units, \
         total_items, done_items, failed_items, skipped_items, last_error, {}, {} \
         FROM jobs ORDER BY id",
        pick(&cols, "planned_items", "0"),
        pick(&cols, "retried_items", "0"),
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map([], |r| {
        Ok(PfJob {
            id: r.get(0)?,
            account_id: r.get(1)?,
            kind: r.get(2)?,
            params_json: r.get(3)?,
            status: r.get(4)?,
            priority: r.get(5)?,
            phase: r.get(6)?,
            total_phases: r.get(7)?,
            resume_at: forge_date_opt(r.get::<_, Option<String>>(8)?.as_deref()),
            created_at: forge_date(&r.get::<_, String>(9)?),
            started_at: forge_date_opt(r.get::<_, Option<String>>(10)?.as_deref()),
            finished_at: forge_date_opt(r.get::<_, Option<String>>(11)?.as_deref()),
            est_units_total: r.get(12)?,
            spent_units: r.get(13)?,
            total_items: r.get(14)?,
            done_items: r.get(15)?,
            failed_items: r.get(16)?,
            skipped_items: r.get(17)?,
            last_error: r.get(18)?,
            planned_items: r.get(19)?,
            retried_items: r.get(20)?,
            items: Vec::new(),
        })
    })?;
    let mut out = Vec::new();
    for row in rows {
        let mut job = row?;
        job.items = items.remove(&job.id).unwrap_or_default();
        out.push(job);
    }
    Ok(out)
}

/// The ledger rows inside `now`'s Pacific quota day. Compared as instants, not as text:
/// PlaylistForge wrote `+00:00` with or without fractions.
fn read_quota_today(conn: &Connection, now: DateTime<Utc>) -> rusqlite::Result<Vec<PfQuotaEntry>> {
    if columns(conn, "quota_ledger")?.is_empty() {
        return Ok(Vec::new());
    }
    let (start, end) = crate::quota::pt_day_bounds_utc(now);
    let mut stmt =
        conn.prepare("SELECT ts, endpoint, units, account_id FROM quota_ledger ORDER BY ts, id")?;
    let rows = stmt.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, i64>(2)?,
            r.get::<_, Option<String>>(3)?,
        ))
    })?;
    let mut out = Vec::new();
    for row in rows {
        let (ts, endpoint, units, account_id) = row?;
        let at = crate::db::parse_rfc3339(&ts);
        if at >= start && at < end {
            out.push(PfQuotaEntry { ts: forge_date(&ts), endpoint, units, account_id });
        }
    }
    Ok(out)
}

// --- files --------------------------------------------------------------------------------------

/// Read `config.json`, `accounts.json` and `client_secret.json` from PlaylistForge's folder.
pub fn read_files(pf_dir: &Path) -> PfFiles {
    let mut files = PfFiles::default();
    match fs::read_to_string(pf_dir.join(PF_CONFIG_FILE)) {
        Ok(raw) => match serde_json::from_str::<PfConfig>(&raw) {
            Ok(c) => files.config = Some(c),
            Err(_) => files.problems.push("config_unreadable"),
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(_) => files.problems.push("config_unreadable"),
    }
    match fs::read_to_string(pf_dir.join(PF_ACCOUNTS_FILE)) {
        Ok(raw) => match serde_json::from_str::<Vec<PfJsonAccount>>(&raw) {
            Ok(a) => files.accounts = a,
            Err(_) => files.problems.push("accounts_unreadable"),
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(_) => files.problems.push("accounts_unreadable"),
    }
    match fs::read_to_string(pf_dir.join(PF_CLIENT_SECRET_FILE)) {
        Ok(raw) => match ytdata::client_secret::ClientSecretFile::parse(&raw) {
            Ok(parsed) => files.client_secret = Some(PfClientSecret { raw, parsed }),
            Err(_) => files.problems.push("client_secret_invalid"),
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(_) => files.problems.push("client_secret_invalid"),
    }
    files
}

/// Where PlaylistForge kept its backups: `monitor.backups_dir` when it is an absolute path,
/// `<pf_dir>/backups` otherwise.
pub fn backups_dir_for(pf_dir: &Path, stored: Option<&str>) -> PathBuf {
    match stored.map(str::trim).filter(|s| !s.is_empty()).map(PathBuf::from) {
        Some(dir) if dir.is_absolute() => dir,
        _ => pf_dir.join(PF_BACKUPS_DIR),
    }
}

/// Read every backup file (`<dir>/<title>__<id>/<taken_at>.json`), without following links.
/// A missing folder is no backups.
pub fn read_backups(dir: &Path) -> PfBackups {
    let mut out = PfBackups { dir: dir.to_path_buf(), ..PfBackups::default() };
    let Ok(folders) = fs::read_dir(dir) else { return out };
    let mut folders: Vec<_> = folders
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .filter(|e| e.file_name().to_string_lossy().contains("__"))
        .map(|e| e.path())
        .collect();
    folders.sort();
    for folder in folders {
        let Ok(entries) = fs::read_dir(&folder) else { continue };
        let mut paths: Vec<_> = entries
            .flatten()
            .filter(|e| e.file_type().is_ok_and(|t| t.is_file()))
            .filter(|e| crate::backups::is_backup_name(&e.file_name().to_string_lossy()))
            .map(|e| e.path())
            .collect();
        paths.sort();
        for path in paths {
            match fs::read_to_string(&path)
                .ok()
                .and_then(|raw| serde_json::from_str::<SnapshotExport>(&raw).ok())
            {
                Some(export) => out.files.push(PfBackup { path, export }),
                None => out.unreadable += 1,
            }
        }
    }
    out
}

/// Stage, read and clean up in one go: the database copy, the JSON files and the backups.
pub fn read_all(
    pf_dir: &Path,
    data_dir: &Path,
    now: DateTime<Utc>,
) -> Result<PfData, PfImportError> {
    let staged = stage(pf_dir, data_dir)?;
    let db = staged.read(now);
    staged.finish()?;
    let db = db?;
    let files = read_files(pf_dir);
    let backups = read_backups(&backups_dir_for(pf_dir, db.setting(crate::backups::DIR_KEY)));
    Ok(PfData { pf_dir: pf_dir.to_path_buf(), db, files, backups })
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::time::SystemTime;

    const MIGRATIONS: [&str; 5] = [
        include_str!("../../tests/fixtures/pf/0001_initial_schema.sql"),
        include_str!("../../tests/fixtures/pf/0002_alerts_dedupe_key.sql"),
        include_str!("../../tests/fixtures/pf/0003_monitor_runs.sql"),
        include_str!("../../tests/fixtures/pf/0004_downloads.sql"),
        include_str!("../../tests/fixtures/pf/0005_job_counters.sql"),
    ];
    const SEED: &str = include_str!("../../tests/fixtures/pf/seed.sql");

    pub(crate) const FAKE_CLIENT_ID: &str = "FAKE-pf-client-0001.apps.googleusercontent.com";
    const FAKE_SECRET: &str = "FAKE-pf-secret-do-not-log";

    /// The test's "now": the Pacific day is 2026-10-05T07:00Z..2026-10-06T07:00Z.
    fn now() -> DateTime<Utc> {
        "2026-10-05T18:00:00Z".parse().unwrap()
    }

    /// `<root>/PlaylistForge/forge.db` migrated to `version` (0001.. in order) in WAL mode, with
    /// automatic checkpoints off so everything stays in `-wal` while the connection is open.
    fn pf_db(pf_dir: &Path, version: usize) -> Connection {
        fs::create_dir_all(pf_dir).unwrap();
        let conn = Connection::open(pf_dir.join(PF_DB_FILE)).unwrap();
        // Both answer with a row, hence query_row.
        let mode: String = conn.query_row("PRAGMA journal_mode=WAL", [], |r| r.get(0)).unwrap();
        assert_eq!(mode, "wal");
        let _: i64 = conn.query_row("PRAGMA wal_autocheckpoint=0", [], |r| r.get(0)).unwrap();
        for (i, sql) in MIGRATIONS.iter().take(version).enumerate() {
            conn.execute_batch(sql).unwrap();
            conn.pragma_update(None, "user_version", (i + 1) as i64).unwrap();
        }
        conn
    }

    /// A full PlaylistForge folder: v5 database with the seed, the three JSON files and backups.
    /// Answers the open connection (keep it alive to keep the WAL).
    pub(crate) fn pf_install(pf_dir: &Path) -> Connection {
        let conn = pf_db(pf_dir, 5);
        conn.execute_batch(SEED).unwrap();
        fs::write(
            pf_dir.join(PF_CONFIG_FILE),
            r#"{"theme":"nord","lang":"en","active_account_id":"UCfakeAccountOne00000001"}"#,
        )
        .unwrap();
        fs::write(
            pf_dir.join(PF_ACCOUNTS_FILE),
            r#"[{"channel_id":"UCfakeAccountOne00000001","title":"Cuenta Uno","thumbnail_url":null,
                "status":"connected","added_at_unix":1788000000},
               {"channel_id":"UCfakeAccountTwo00000002","title":"Cuenta Dos","thumbnail_url":null,
                "status":"reauth_required","added_at_unix":1788100000}]"#,
        )
        .unwrap();
        fs::write(
            pf_dir.join(PF_CLIENT_SECRET_FILE),
            format!(
                r#"{{"installed":{{"client_id":"{FAKE_CLIENT_ID}","client_secret":"{FAKE_SECRET}",
                "redirect_uris":["http://localhost"]}}}}"#
            ),
        )
        .unwrap();
        let folder = pf_dir.join(PF_BACKUPS_DIR).join("Fake Mix__PLfakeOne");
        fs::create_dir_all(&folder).unwrap();
        let export = SnapshotExport {
            playlist_id: "PLfakeOne".into(),
            playlist_title: "Fake Mix".into(),
            account_id: "UCfakeAccountOne00000001".into(),
            snapshot_id: 2,
            taken_at: "2026-10-05T10:00:00Z".parse().unwrap(),
            item_count: 0,
            items: Vec::new(),
        };
        fs::write(
            folder.join("2026-10-05T10-00-00+00-00.json"),
            serde_json::to_string_pretty(&export).unwrap(),
        )
        .unwrap();
        fs::write(folder.join("2026-10-01T10-00-00+00-00.json"), "{ not json").unwrap();
        fs::write(folder.join("notes.txt"), "not a backup").unwrap();
        conn
    }

    /// Every file under `dir` with its length and mtime.
    fn listing(dir: &Path) -> Vec<(PathBuf, u64, SystemTime)> {
        let mut out = Vec::new();
        let mut stack = vec![dir.to_path_buf()];
        while let Some(d) = stack.pop() {
            for e in fs::read_dir(&d).unwrap().flatten() {
                let m = e.metadata().unwrap();
                if m.is_dir() {
                    stack.push(e.path());
                } else {
                    out.push((e.path(), m.len(), m.modified().unwrap()));
                }
            }
        }
        out.sort();
        out
    }

    fn work_dirs(data: &Path) -> Vec<PathBuf> {
        match fs::read_dir(data.join("tmp")) {
            Ok(rd) => rd
                .flatten()
                .filter(|e| e.file_name().to_string_lossy().starts_with(WORK_PREFIX))
                .map(|e| e.path())
                .collect(),
            Err(_) => Vec::new(),
        }
    }

    #[test]
    fn pf_import_reads_the_current_snapshot_with_best_metadata() {
        let tmp = tempfile::tempdir().unwrap();
        let pf = tmp.path().join("PlaylistForge");
        let data = tmp.path().join("data");
        let _conn = pf_install(&pf);

        let all = read_all(&pf, &data, now()).unwrap();
        let db = &all.db;
        assert_eq!(db.user_version, 5);

        assert_eq!(db.accounts.len(), 2);
        let one = db.accounts.iter().find(|a| a.id == "UCfakeAccountOne00000001").unwrap();
        assert_eq!(one.last_sync_at.as_deref(), Some("2026-10-05T10:00:00Z"));
        assert_eq!(one.added_at, "2026-09-01T10:00:00Z");
        assert!(db.accounts.iter().any(|a| a.last_sync_at.is_none()));

        let two = db.playlists.iter().find(|p| p.id == "PLfakeTwo").unwrap();
        assert_eq!(
            (two.privacy.as_str(), two.item_count, two.deleted_remotely),
            ("unlisted", 1, true)
        );
        let first = db.playlists.iter().find(|p| p.id == "PLfakeOne").unwrap();
        assert_eq!((first.privacy.as_str(), first.deleted_remotely), ("public", false));

        assert_eq!(db.snapshots.len(), 3);
        let current = db.current_snapshot("PLfakeOne").unwrap();
        assert_eq!(
            (current.id, current.item_count, current.taken_at.as_str()),
            (2, 3, "2026-10-05T10:00:00Z")
        );
        let ids: Vec<_> = current.items.iter().map(|i| i.video_id.as_str()).collect();
        assert_eq!(ids, ["vidBBBBBBB2", "vidAAAAAAA1", "vidCCCCCCC3"]);
        // A dead video keeps the catalog's best title, not the snapshot's sentinel.
        let dead = &current.items[0];
        assert_eq!(dead.title_at_time.as_deref(), Some("Deleted video"));
        assert_eq!(dead.best_title.as_deref(), Some("Fake Song Two"));
        assert_eq!(dead.best_channel_title.as_deref(), Some("Fake Artist B"));
        assert_eq!((dead.status.as_str(), dead.duration_s), ("deleted", Some(187)));
        assert_eq!(dead.added_at.as_deref(), Some("2026-09-02T10:00:00Z"));
        assert_eq!(current.items[2].added_at, None);
        assert_eq!(db.current_snapshot("PLfakeTwo").unwrap().items.len(), 1);
        assert!(db.current_snapshot("PLfakeThree").is_none());
        let old = db.snapshots.iter().find(|s| s.id == 1).unwrap();
        assert!(!old.is_current);
        assert_eq!(old.items.len(), 2);

        assert_eq!(db.videos.len(), 4);
        assert_eq!(db.alerts.len(), 8);
        let seen: Vec<_> = db.alerts.iter().filter(|a| a.seen).map(|a| a.id).collect();
        assert_eq!(seen, [1, 4]);
        assert_eq!(db.downloads.len(), 2);
        assert_eq!(db.downloads[0].completed_at.as_deref(), Some("2026-10-02T10:01:00Z"));
        assert_eq!(db.setting("ui.drop_mode"), Some("move"));
        assert_eq!(db.settings.len(), 14);

        let s = all.summary();
        assert_eq!((s.playlists, s.playlists_with_items, s.items), (3, 2, 4));
        assert_eq!((s.jobs_total, s.jobs_pending, s.quota_units_today), (3, 2, 51));
        assert!(s.has_client_secret);
    }

    #[test]
    fn pf_import_reads_jobs_with_items_and_the_pacific_quota_day() {
        let tmp = tempfile::tempdir().unwrap();
        let pf = tmp.path().join("PlaylistForge");
        let _conn = pf_install(&pf);
        let db = read_all(&pf, &tmp.path().join("data"), now()).unwrap().db;

        assert_eq!(db.jobs.iter().map(|j| j.id).collect::<Vec<_>>(), [1, 2, 3]);
        let queued = &db.jobs[0];
        assert_eq!(queued.account_id.as_deref(), Some("UCfakeAccountOne00000001"));
        assert_eq!(
            (queued.status.as_str(), queued.priority, queued.planned_items),
            ("queued", 1, 2)
        );
        assert_eq!(queued.items.iter().map(|i| i.seq).collect::<Vec<_>>(), [0, 1]);
        assert!(queued.items.iter().all(|i| i.job_id == 1));
        assert_eq!(queued.created_at, "2026-10-05T09:00:00Z");
        let waiting = &db.jobs[2];
        assert_eq!(waiting.resume_at.as_deref(), Some("2026-10-06T07:00:00Z"));
        assert!(waiting.items.is_empty());
        assert_eq!(
            db.jobs[1].items[0].inverse_json.as_deref(),
            Some(r#"{"action":"insert_item"}"#)
        );

        // 06:59:59Z belongs to the previous Pacific day, 07:00Z the next day's start.
        let ts: Vec<_> = db.quota_today.iter().map(|q| q.ts.as_str()).collect();
        assert_eq!(ts, ["2026-10-05T07:00:00Z", "2026-10-05T17:30:00Z"]);
        assert_eq!(db.quota_today.iter().map(|q| q.units).sum::<i64>(), 51);
        assert_eq!(db.quota_today[1].account_id, None);
    }

    #[test]
    fn pf_import_reads_the_json_files_and_backups() {
        let tmp = tempfile::tempdir().unwrap();
        let pf = tmp.path().join("PlaylistForge");
        let _conn = pf_install(&pf);
        let all = read_all(&pf, &tmp.path().join("data"), now()).unwrap();

        let cfg = all.files.config.as_ref().unwrap();
        assert_eq!((cfg.theme.as_str(), cfg.lang.as_str()), ("nord", "en"));
        assert_eq!(all.files.accounts.len(), 2);
        assert_eq!(all.files.accounts[1].status, "reauth_required");
        let secret = all.files.client_secret.as_ref().unwrap();
        assert_eq!(secret.client_id(), FAKE_CLIENT_ID);
        assert!(secret.raw.contains(FAKE_SECRET), "kept verbatim for the copy");
        let debug = format!("{:?}", all.files);
        assert!(!debug.contains(FAKE_SECRET), "{debug}");
        assert!(all.files.problems.is_empty());

        assert_eq!(all.backups.dir, pf.join("backups"));
        assert_eq!(all.backups.files.len(), 1);
        assert_eq!(all.backups.unreadable, 1, "the broken one counts, notes.txt is ignored");
        assert_eq!(all.backups.files[0].export.playlist_id, "PLfakeOne");
    }

    #[test]
    fn pf_import_json_problems_do_not_fail_the_read() {
        let tmp = tempfile::tempdir().unwrap();
        let pf = tmp.path().join("PlaylistForge");
        fs::create_dir_all(&pf).unwrap();
        fs::write(pf.join(PF_CONFIG_FILE), "{").unwrap();
        fs::write(
            pf.join(PF_CLIENT_SECRET_FILE),
            r#"{"web":{"client_id":"x","client_secret":"y"}}"#,
        )
        .unwrap();
        let files = read_files(&pf);
        assert!(files.config.is_none() && files.client_secret.is_none());
        assert!(files.accounts.is_empty());
        assert_eq!(files.problems, ["config_unreadable", "client_secret_invalid"]);
        // Nothing at all is not a problem.
        assert!(read_files(&tmp.path().join("nowhere")).problems.is_empty());
    }

    #[test]
    fn pf_import_backups_dir_follows_the_setting() {
        let pf = Path::new("/pf");
        assert_eq!(backups_dir_for(pf, None), pf.join("backups"));
        assert_eq!(backups_dir_for(pf, Some("  ")), pf.join("backups"));
        assert_eq!(backups_dir_for(pf, Some("relative")), pf.join("backups"));
        let abs = std::env::temp_dir().join("pf-backups-elsewhere");
        assert_eq!(backups_dir_for(pf, abs.to_str()), abs);
    }

    #[test]
    fn pf_import_copies_the_wal_and_leaves_the_source_untouched() {
        let tmp = tempfile::tempdir().unwrap();
        let pf = tmp.path().join("PlaylistForge");
        let data = tmp.path().join("data");
        let conn = pf_install(&pf);
        // Everything is still in the WAL: the main file has not been checkpointed into.
        let wal = fs::metadata(pf.join("forge.db-wal")).unwrap().len();
        assert!(wal > 0, "the seed lives in the WAL");
        let before = listing(&pf);
        std::thread::sleep(std::time::Duration::from_millis(20));

        let staged = stage(&pf, &data).unwrap();
        let work = staged.work_dir().to_path_buf();
        assert!(work.starts_with(data.join("tmp")));
        assert!(work.file_name().unwrap().to_string_lossy().starts_with("pf-import-"));
        assert!(work.join("forge.db-wal").is_file(), "the WAL was copied");
        let db = staged.read(now()).unwrap();
        assert_eq!(db.accounts.len(), 2, "rows only in the WAL are read");
        staged.finish().unwrap();

        assert_eq!(listing(&pf), before, "no file in PlaylistForge's folder changed or appeared");
        assert!(!work.exists());
        assert!(data.join("tmp").is_dir(), "only the pf-import folder goes");
        drop(conn);
    }

    #[test]
    fn pf_import_never_stages_inside_playlistforge() {
        let tmp = tempfile::tempdir().unwrap();
        let pf = tmp.path().join("PlaylistForge");
        let _conn = pf_install(&pf);
        let before = listing(&pf);
        assert!(matches!(stage(&pf, &pf.join("data")), Err(PfImportError::UnsafePath(_))));
        assert_eq!(listing(&pf), before);
        assert!(!pf.join("data").exists(), "nothing was created there");
    }

    #[test]
    fn pf_import_drop_removes_only_its_own_folder() {
        let tmp = tempfile::tempdir().unwrap();
        let pf = tmp.path().join("PlaylistForge");
        let data = tmp.path().join("data");
        let _conn = pf_install(&pf);
        let neighbour = data.join("tmp").join("migrate-123");
        fs::create_dir_all(&neighbour).unwrap();
        fs::write(neighbour.join("keep.txt"), "x").unwrap();

        let staged = stage(&pf, &data).unwrap();
        let work = staged.work_dir().to_path_buf();
        drop(staged);
        assert!(!work.exists());
        assert!(neighbour.join("keep.txt").is_file());
        assert!(work_dirs(&data).is_empty());
    }

    #[test]
    fn pf_import_remove_refuses_foreign_folders() {
        let tmp = tempfile::tempdir().unwrap();
        let tmp_root = tmp.path().join("data").join("tmp");
        fs::create_dir_all(&tmp_root).unwrap();
        // Not named like ours.
        let other = tmp_root.join("cookies-1");
        fs::create_dir_all(&other).unwrap();
        assert!(matches!(remove_work_dir(&other, &tmp_root), Err(PfImportError::UnsafePath(_))));
        assert!(other.is_dir());
        // Named like ours but not directly under tmp.
        let nested = tmp_root.join("x").join("pf-import-1");
        fs::create_dir_all(&nested).unwrap();
        assert!(matches!(remove_work_dir(&nested, &tmp_root), Err(PfImportError::UnsafePath(_))));
        assert!(nested.is_dir());
        let outside = tmp.path().join("pf-import-2");
        fs::create_dir_all(&outside).unwrap();
        assert!(matches!(remove_work_dir(&outside, &tmp_root), Err(PfImportError::UnsafePath(_))));
        assert!(outside.is_dir());
        // A file with our name is not a folder of ours.
        let file = tmp_root.join("pf-import-3");
        fs::write(&file, "x").unwrap();
        assert!(matches!(remove_work_dir(&file, &tmp_root), Err(PfImportError::UnsafePath(_))));
        assert!(file.is_file());
        // The bare prefix is not a name we make.
        let bare = tmp_root.join("pf-import-");
        fs::create_dir_all(&bare).unwrap();
        assert!(remove_work_dir(&bare, &tmp_root).is_err());
        // Ours goes; gone already is fine.
        let ours = tmp_root.join("pf-import-4");
        fs::create_dir_all(ours.join("sub")).unwrap();
        remove_work_dir(&ours, &tmp_root).unwrap();
        assert!(!ours.exists());
        remove_work_dir(&ours, &tmp_root).unwrap();

        // The sweep takes leftovers and nothing else.
        let left = tmp_root.join("pf-import-5");
        fs::create_dir_all(&left).unwrap();
        assert_eq!(sweep_stale(&tmp.path().join("data")), 1);
        assert!(!left.exists() && other.is_dir() && file.is_file());
    }

    #[test]
    fn pf_import_rejects_unknown_versions_and_cleans_up() {
        let tmp = tempfile::tempdir().unwrap();
        let data = tmp.path().join("data");

        // user_version 0: some other SQLite file.
        let zero = tmp.path().join("zero");
        fs::create_dir_all(&zero).unwrap();
        Connection::open(zero.join(PF_DB_FILE))
            .unwrap()
            .execute_batch("CREATE TABLE t (x INTEGER);")
            .unwrap();
        assert!(matches!(stage(&zero, &data), Err(PfImportError::NotPlaylistForge)));

        // user_version 6: a PlaylistForge newer than this importer (D34).
        let six = tmp.path().join("six");
        let conn = pf_db(&six, 5);
        conn.pragma_update(None, "user_version", 6).unwrap();
        drop(conn);
        assert!(matches!(stage(&six, &data), Err(PfImportError::TooNew { found: 6, known: 5 })));

        // A version but none of PlaylistForge's tables.
        let fake = tmp.path().join("fake");
        fs::create_dir_all(&fake).unwrap();
        let conn = Connection::open(fake.join(PF_DB_FILE)).unwrap();
        conn.execute_batch("CREATE TABLE t (x INTEGER); PRAGMA user_version = 3;").unwrap();
        drop(conn);
        assert!(matches!(stage(&fake, &data), Err(PfImportError::NotPlaylistForge)));

        // No database at all.
        assert!(matches!(
            stage(&tmp.path().join("missing"), &data),
            Err(PfImportError::NotFound(_))
        ));

        assert!(work_dirs(&data).is_empty(), "every failed attempt removed its copy");
    }

    #[test]
    fn pf_import_tolerates_an_older_schema() {
        let tmp = tempfile::tempdir().unwrap();
        let pf = tmp.path().join("PlaylistForge");
        // v3: no downloads table, no planned_items/retried_items.
        let conn = pf_db(&pf, 3);
        conn.execute_batch(
            "INSERT INTO accounts (id, title, added_at) VALUES ('UCold', 'Old', '2026-01-01T00:00:00+00:00');
             INSERT INTO jobs (account_id, kind, status, created_at) VALUES ('UCold', 'add_items', 'queued', '2026-01-02T00:00:00+00:00');",
        )
        .unwrap();
        let db = read_all(&pf, &tmp.path().join("data"), now()).unwrap().db;
        assert_eq!(db.user_version, 3);
        assert_eq!(db.accounts.len(), 1);
        assert!(db.downloads.is_empty());
        assert_eq!((db.jobs.len(), db.jobs[0].planned_items, db.jobs[0].retried_items), (1, 0, 0));
        drop(conn);
    }
}
