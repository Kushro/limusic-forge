//! JSON backups of the monitor's playlist snapshots, and their retention.
//!
//! SQLite holds the snapshots (`playlist_snapshot`, db.rs); this writes an independent,
//! human-readable copy of each one to disk, one file per snapshot at
//! `<dir>/<title>__<playlist id>/<taken_at>.json`. The file format is PlaylistForge's
//! (`pf-core/src/export.rs`, [`SnapshotExport`]) field for field, so a backup written by either
//! app reads in the other. The monitor (commands.rs `sync_all`, `sync_playlist`) exports at the end
//! of every run, then prunes the database and the folder to the newest `retention_keep_last`.
//!
//! Pruning deletes files only, never folders, and only files whose name is exactly a snapshot
//! timestamp, inside a `*__*` folder directly under the backups folder, after canonicalizing both:
//! pointing the setting at a folder full of other things must not cost any of them.

use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::db::{Db, Snapshot};

/// Where the backups go (an absolute path). Empty, missing or relative means [`default_dir`].
/// Same key as PlaylistForge.
pub const DIR_KEY: &str = "monitor.backups_dir";
/// How many snapshots each playlist keeps, in the database and on disk. Same key as PlaylistForge.
pub const KEEP_KEY: &str = "retention_keep_last";
pub const KEEP_DEFAULT: usize = 30;

/// One item inside a [`SnapshotExport`]: PlaylistForge's `SnapshotExportItem`, same fields in the
/// same order. A Forge snapshot keeps one title and one byline per row, so `best_*` repeat the
/// `*_at_time` values; it keeps no add date, so `added_at` is always `null`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SnapshotExportItem {
    pub position: i64,
    pub playlist_item_id: String,
    pub video_id: String,
    pub title_at_time: Option<String>,
    pub channel_at_time: Option<String>,
    pub added_at: Option<DateTime<Utc>>,
    pub best_title: Option<String>,
    pub best_channel_title: Option<String>,
    pub duration_s: Option<i64>,
    pub status: String,
    pub url: String,
}

/// One snapshot as a backup file: PlaylistForge's `SnapshotExport`, same fields in the same order.
///
/// ```json
/// {
///   "playlist_id": "PLxxxx",
///   "playlist_title": "My Playlist",
///   "account_id": "a1b2c3",
///   "snapshot_id": 42,
///   "taken_at": "2026-07-15T12:00:00Z",
///   "item_count": 1,
///   "items": [
///     {
///       "position": 0,
///       "playlist_item_id": "56B44F6D10557CC6",
///       "video_id": "dQw4w9WgXcQ",
///       "title_at_time": "Great Song",
///       "channel_at_time": "Great Artist",
///       "added_at": null,
///       "best_title": "Great Song",
///       "best_channel_title": "Great Artist",
///       "duration_s": 213,
///       "status": "available",
///       "url": "https://youtu.be/dQw4w9WgXcQ"
///     }
///   ]
/// }
/// ```
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SnapshotExport {
    pub playlist_id: String,
    pub playlist_title: String,
    pub account_id: String,
    pub snapshot_id: i64,
    pub taken_at: DateTime<Utc>,
    pub item_count: i64,
    pub items: Vec<SnapshotExportItem>,
}

/// What `export_backups_now` (and a monitor run) did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct Outcome {
    pub written: usize,
    pub pruned_files: usize,
    pub pruned_rows: usize,
}

/// The default backups folder, under the app's data folder (paths.rs).
pub fn default_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("backups")
}

/// Folders Forge never writes backups into (nor prunes): PlaylistForge's own data folder (its
/// default backups folder lives there, with the same file names, so pruning would delete its
/// backups) and upstream LiMusic's folders. Each per-user base folder `dirs` knows (config,
/// data, local data; on Windows Roaming and Local, on Linux `~/.config` and `~/.local/share`)
/// joined with each of those apps' folder names.
pub fn forbidden_roots() -> Vec<PathBuf> {
    // PlaylistForge: `config_dir()/PlaylistForge` (pf-app `config::app_data_dir`); its Dioxus
    // bundle identifier names the webview's folder.
    const NAMES: [&str; 3] =
        ["PlaylistForge", "com.playlistforge.app", crate::migrate_upstream::UPSTREAM_ID];
    let mut roots = Vec::new();
    for base in [dirs::config_dir(), dirs::data_dir(), dirs::data_local_dir()].into_iter().flatten()
    {
        for name in NAMES {
            let root = base.join(name);
            if root.is_absolute() && !roots.contains(&root) {
                roots.push(root);
            }
        }
    }
    roots
}

/// `path` with `.` and `..` resolved, then its longest existing ancestor canonicalized (links
/// followed, and on Windows the `\\?\` form) and the rest appended, so an existing folder and a
/// not-yet-created one under it compare alike.
fn normalize(path: &Path) -> PathBuf {
    use std::path::Component;
    let mut lexical = PathBuf::new();
    for c in path.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                lexical.pop();
            }
            c => lexical.push(c.as_os_str()),
        }
    }
    let mut existing = lexical.as_path();
    let mut rest: Vec<std::ffi::OsString> = Vec::new();
    loop {
        if let Ok(canon) = existing.canonicalize() {
            return rest.iter().rev().fold(canon, |acc, part| acc.join(part));
        }
        match (existing.parent(), existing.file_name()) {
            (Some(parent), Some(name)) => {
                rest.push(name.to_owned());
                existing = parent;
            }
            _ => break,
        }
    }
    // Nothing on the path exists (not even its root): compare it as written.
    lexical
}

/// A path's components as comparable strings: folded to lower case where the file system is
/// usually case-insensitive (Windows, macOS).
fn comparable(path: &Path) -> Vec<String> {
    path.components()
        .map(|c| {
            let s = c.as_os_str().to_string_lossy();
            if cfg!(any(windows, target_os = "macos")) {
                s.to_lowercase()
            } else {
                s.into_owned()
            }
        })
        .collect()
}

/// Whether `dir` is one of `forbidden`, or inside one, compared component by component after
/// [`normalize`]: `PlaylistForge2` is not inside `PlaylistForge`.
pub fn is_forbidden(dir: &Path, forbidden: &[PathBuf]) -> bool {
    let dir = comparable(&normalize(dir));
    forbidden.iter().any(|root| dir.starts_with(&comparable(&normalize(root))))
}

/// The stored folder if it is an absolute path, the default otherwise.
pub fn resolve_dir(stored: Option<&str>, data_dir: &Path) -> PathBuf {
    resolve_dir_with(stored, data_dir, &forbidden_roots())
}

/// [`resolve_dir`] with the forbidden roots given: a stored folder inside one of them is ignored
/// for the default.
pub fn resolve_dir_with(stored: Option<&str>, data_dir: &Path, forbidden: &[PathBuf]) -> PathBuf {
    match stored.map(str::trim).filter(|s| !s.is_empty()).map(PathBuf::from) {
        Some(dir) if dir.is_absolute() => {
            if is_forbidden(&dir, forbidden) {
                tracing::warn!(
                    "the backups folder setting points into another app's data; using the default"
                );
                default_dir(data_dir)
            } else {
                dir
            }
        }
        _ => default_dir(data_dir),
    }
}

/// Whether the stored folder is one [`resolve_dir`] turns down for lying inside
/// [`forbidden_roots`], so the default is used instead: what the settings warn about.
pub fn dir_rejected(db: &Db) -> bool {
    rejected_with(db.get_setting(DIR_KEY).as_deref(), &forbidden_roots())
}

/// [`dir_rejected`] with the stored value and the forbidden roots given.
pub fn rejected_with(stored: Option<&str>, forbidden: &[PathBuf]) -> bool {
    stored
        .map(|s| Path::new(s.trim()))
        .is_some_and(|dir| dir.is_absolute() && is_forbidden(dir, forbidden))
}

pub fn backups_dir(db: &Db, data_dir: &Path) -> PathBuf {
    resolve_dir(db.get_setting(DIR_KEY).as_deref(), data_dir)
}

/// The stored count if it is a whole number of at least 1, [`KEEP_DEFAULT`] otherwise.
pub fn parse_keep(stored: Option<&str>) -> usize {
    stored.and_then(|s| s.trim().parse::<usize>().ok()).filter(|n| *n >= 1).unwrap_or(KEEP_DEFAULT)
}

pub fn keep_last(db: &Db) -> usize {
    parse_keep(db.get_setting(KEEP_KEY).as_deref())
}

/// A duration as YouTube writes it (`3:45`, `1:02:03`) in seconds; `None` for anything else.
pub fn duration_secs(d: &str) -> Option<i64> {
    let parts: Vec<&str> = d.trim().split(':').collect();
    if parts.len() > 3 {
        return None;
    }
    let mut total: i64 = 0;
    for (i, part) in parts.iter().enumerate() {
        if part.is_empty() || part.len() > 4 || !part.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        let n: i64 = part.parse().ok()?;
        // Every field but the first is a sexagesimal digit.
        if i > 0 && n >= 60 {
            return None;
        }
        total = total * 60 + n;
    }
    Some(total)
}

/// A snapshot as its backup file holds it. A greyed-out row (`u`) is `unknown`: YouTube does not
/// say whether it was deleted, made private or blocked (D18); every other row is `available`, as
/// PlaylistForge writes it.
pub fn to_export(snap: &Snapshot) -> SnapshotExport {
    let non_empty = |s: &str| (!s.is_empty()).then(|| s.to_owned());
    let items = snap
        .items
        .iter()
        .enumerate()
        .map(|(i, it)| SnapshotExportItem {
            position: i as i64,
            playlist_item_id: it.s.clone().unwrap_or_default(),
            video_id: it.v.clone(),
            title_at_time: non_empty(&it.t),
            channel_at_time: non_empty(&it.a),
            added_at: None,
            best_title: non_empty(&it.t),
            best_channel_title: non_empty(&it.a),
            duration_s: it.d.as_deref().and_then(duration_secs),
            status: if it.u { "unknown" } else { "available" }.to_owned(),
            url: format!("https://youtu.be/{}", it.v),
        })
        .collect();
    SnapshotExport {
        playlist_id: snap.playlist_id.clone(),
        playlist_title: snap.title.clone().unwrap_or_default(),
        account_id: snap.account_id.clone().unwrap_or_default(),
        snapshot_id: snap.id,
        taken_at: DateTime::from_timestamp(snap.taken_at, 0).unwrap_or_default(),
        item_count: snap.item_count,
        items,
    }
}

/// Windows-safe path segment, ported from PlaylistForge (`export.rs` `sanitize_path_segment`):
/// characters invalid in a Windows path and control characters become `_`, trailing dots and
/// spaces go (Windows strips them silently), and an empty result is `untitled`.
pub fn sanitize_path_segment(raw: &str) -> String {
    let mut out: String = raw
        .chars()
        .map(|c| match c {
            '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*' => '_',
            c if (c as u32) < 0x20 => '_',
            c => c,
        })
        .collect();
    while matches!(out.chars().last(), Some('.') | Some(' ')) {
        out.pop();
    }
    let trimmed = out.trim();
    if trimmed.is_empty() {
        "untitled".to_string()
    } else {
        trimmed.to_string()
    }
}

/// `taken_at` as a file name stem: RFC 3339 with `:` (not allowed on Windows) as `-`, so
/// `2026-07-15T12:00:00+00:00` is `2026-07-15T12-00-00+00-00`, as PlaylistForge names them.
pub fn file_stem(taken_at: DateTime<Utc>) -> String {
    taken_at.to_rfc3339().replace(':', "-")
}

/// Where a snapshot's backup goes under `dir`.
pub fn snapshot_path(dir: &Path, export: &SnapshotExport) -> PathBuf {
    let folder = format!(
        "{}__{}",
        sanitize_path_segment(&export.playlist_title),
        sanitize_path_segment(&export.playlist_id)
    );
    dir.join(folder).join(format!("{}.json", file_stem(export.taken_at)))
}

/// Write one snapshot's backup (pretty JSON), creating its folder. Answers the path written.
pub fn write_snapshot(dir: &Path, snap: &Snapshot) -> std::io::Result<PathBuf> {
    let export = to_export(snap);
    let path = snapshot_path(dir, &export);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let json = serde_json::to_string_pretty(&export).map_err(std::io::Error::other)?;
    std::fs::write(&path, json)?;
    Ok(path)
}

/// A backup file's name: a snapshot timestamp and nothing else. Pruning touches no other file.
static BACKUP_NAME: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(r"^\d{4}-\d{2}-\d{2}T\d{2}-\d{2}-\d{2}(Z|[+-]\d{2}-\d{2})\.json$").unwrap()
});

pub fn is_backup_name(name: &str) -> bool {
    BACKUP_NAME.is_match(name)
}

/// The instant a backup file's name stands for, `None` if it is not a real date.
fn name_instant(name: &str) -> Option<DateTime<Utc>> {
    if !is_backup_name(name) {
        return None;
    }
    let stem = name.strip_suffix(".json")?;
    // `YYYY-MM-DDTHH-MM-SS` then `Z` or `±HH-MM`: put the colons back.
    let (date_time, offset) = stem.split_at(19);
    let date_time = format!("{}:{}:{}", &date_time[..13], &date_time[14..16], &date_time[17..]);
    let offset = match offset {
        "Z" => "Z".to_owned(),
        o => format!("{}:{}", &o[..3], &o[4..]),
    };
    DateTime::parse_from_rfc3339(&format!("{date_time}{offset}"))
        .ok()
        .map(|d| d.with_timezone(&Utc))
}

/// Keep the newest `keep` backup files in each `*__*` folder directly under `dir` and delete the
/// rest. Answers how many were deleted. A missing `dir` is nothing to prune.
///
/// Deletes only regular files (never folders, never symlinks) named like [`is_backup_name`] with
/// a real date, and only after checking their canonical path is inside the canonical `dir`.
///
/// A `dir` inside one of [`forbidden_roots`] is never pruned (it answers 0).
#[cfg(test)]
pub fn prune_files(dir: &Path, keep: usize) -> std::io::Result<usize> {
    prune_files_with(dir, keep, &forbidden_roots())
}

/// [`prune_files`] with the forbidden roots given.
pub fn prune_files_with(dir: &Path, keep: usize, forbidden: &[PathBuf]) -> std::io::Result<usize> {
    let keep = keep.max(1);
    let root = match dir.canonicalize() {
        Ok(root) => root,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(e) => return Err(e),
    };
    if is_forbidden(&root, forbidden) {
        tracing::warn!("refusing to prune a backups folder inside another app's data");
        return Ok(0);
    }
    let mut pruned = 0;
    for entry in std::fs::read_dir(&root)? {
        let Ok(entry) = entry else { continue };
        // `file_type` does not follow links: a link to a folder elsewhere is not a folder here.
        let Ok(kind) = entry.file_type() else { continue };
        if !kind.is_dir() || kind.is_symlink() {
            continue;
        }
        if !entry.file_name().to_string_lossy().contains("__") {
            continue;
        }
        let Ok(folder) = entry.path().canonicalize() else { continue };
        if !folder.starts_with(&root) || folder.parent() != Some(root.as_path()) {
            continue;
        }
        pruned += prune_folder(&folder, keep);
    }
    Ok(pruned)
}

fn prune_folder(folder: &Path, keep: usize) -> usize {
    let Ok(entries) = std::fs::read_dir(folder) else { return 0 };
    let mut files: Vec<(DateTime<Utc>, String, PathBuf)> = Vec::new();
    for entry in entries.flatten() {
        let Ok(kind) = entry.file_type() else { continue };
        if !kind.is_file() || kind.is_symlink() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        let Some(at) = name_instant(&name) else { continue };
        let Ok(path) = entry.path().canonicalize() else { continue };
        if !path.starts_with(folder) || path.parent() != Some(folder) {
            continue;
        }
        files.push((at, name, path));
    }
    // Newest first; the name breaks a tie so the order never depends on the directory listing.
    files.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| b.1.cmp(&a.1)));
    let mut pruned = 0;
    for (_, _, path) in files.into_iter().skip(keep) {
        match std::fs::remove_file(&path) {
            Ok(()) => pruned += 1,
            Err(e) => {
                tracing::warn!(error = %e, path = %path.display(), "could not prune a backup")
            }
        }
    }
    pruned
}

/// Export the newest snapshot of each of `playlist_ids` into `dir`, then prune the database and
/// the folder to `keep`. With `only_missing`, a snapshot whose file is already there is skipped
/// (the monitor's end-of-run export: its new snapshots never have one, and a folder that was just
/// set up gets the current state of every playlist); without it every file is rewritten.
pub fn export_and_prune(
    db: &Db,
    dir: &Path,
    keep: usize,
    playlist_ids: &[String],
    only_missing: bool,
) -> Outcome {
    let mut outcome = Outcome::default();
    let forbidden = forbidden_roots();
    if is_forbidden(dir, &forbidden) {
        // `backups_dir` never answers such a folder; this guards any other caller.
        tracing::warn!("refusing to write backups inside another app's data");
        outcome.pruned_rows = db.prune_snapshots(keep).len();
        return outcome;
    }
    for id in playlist_ids {
        let Some(snap) = db.latest_snapshot(id) else { continue };
        if only_missing && snapshot_path(dir, &to_export(&snap)).exists() {
            continue;
        }
        match write_snapshot(dir, &snap) {
            Ok(_) => outcome.written += 1,
            Err(e) => tracing::warn!(error = %e, playlist = %id, "could not write a backup"),
        }
    }
    outcome.pruned_rows = db.prune_snapshots(keep).len();
    outcome.pruned_files = prune_files_with(dir, keep, &forbidden).unwrap_or_else(|e| {
        tracing::warn!(error = %e, dir = %dir.display(), "could not prune the backups folder");
        0
    });
    outcome
}

/// Open a folder in the system file manager, creating it first so there is something to open.
pub fn open_dir(dir: &Path) -> Result<(), String> {
    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    #[cfg(target_os = "windows")]
    {
        std::process::Command::new("explorer")
            .arg(dir)
            .spawn()
            .map(|_| ())
            .map_err(|e| format!("Couldn't open the folder: {e}"))
    }
    #[cfg(not(target_os = "windows"))]
    {
        crate::lastfm::open_browser(&dir.to_string_lossy())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::SnapItem;

    fn at() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-07-15T12:00:00Z").unwrap().with_timezone(&Utc)
    }

    fn snap() -> Snapshot {
        Snapshot {
            id: 42,
            playlist_id: "PLabc".into(),
            account_id: Some("a1b2c3".into()),
            title: Some("My Playlist".into()),
            taken_at: at().timestamp(),
            item_count: 2,
            hash: "x".into(),
            items: vec![
                SnapItem {
                    v: "dQw4w9WgXcQ".into(),
                    s: Some("56B44F6D10557CC6".into()),
                    t: "Great Song".into(),
                    a: "Great Artist".into(),
                    d: Some("3:33".into()),
                    u: false,
                    th: Some("https://i.ytimg.com/x.jpg".into()),
                },
                SnapItem {
                    v: "deadbeefdea".into(),
                    s: None,
                    t: "Gone Song".into(),
                    a: String::new(),
                    d: None,
                    u: true,
                    th: None,
                },
            ],
        }
    }

    /// PlaylistForge's format, literally: a file it wrote must read here and come back the same.
    const PF_FIXTURE: &str = r#"{
  "playlist_id": "PLxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx",
  "playlist_title": "My Playlist",
  "account_id": "UCxxxxxxxxxxxxxxxxxxxxxxxx",
  "snapshot_id": 42,
  "taken_at": "2026-07-15T12:00:00Z",
  "item_count": 2,
  "items": [
    {
      "position": 0,
      "playlist_item_id": "UExxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx",
      "video_id": "dQw4w9WgXcQ",
      "title_at_time": "Great Song (Official Video)",
      "channel_at_time": "Great Channel",
      "added_at": "2026-01-01T00:00:00Z",
      "best_title": "Great Song (Official Video)",
      "best_channel_title": "Great Channel",
      "duration_s": 213,
      "status": "available",
      "url": "https://youtu.be/dQw4w9WgXcQ"
    },
    {
      "position": 1,
      "playlist_item_id": "UEyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyy",
      "video_id": "deadbeefdead",
      "title_at_time": "Deleted video",
      "channel_at_time": null,
      "added_at": "2026-01-02T00:00:00Z",
      "best_title": "The Video That Died (Official Video)",
      "best_channel_title": "Some Channel",
      "duration_s": 180,
      "status": "deleted",
      "url": "https://youtu.be/deadbeefdead"
    }
  ]
}"#;

    #[test]
    fn playlistforge_fixture_round_trips_byte_for_byte() {
        let parsed: SnapshotExport = serde_json::from_str(PF_FIXTURE).unwrap();
        assert_eq!(parsed.items.len(), 2);
        assert_eq!(parsed.items[1].channel_at_time, None);
        assert_eq!(serde_json::to_string_pretty(&parsed).unwrap(), PF_FIXTURE);
    }

    #[test]
    fn export_has_pf_fields_in_pf_order() {
        let json = serde_json::to_string(&to_export(&snap())).unwrap();
        let expected = concat!(
            r#"{"playlist_id":"PLabc","playlist_title":"My Playlist","account_id":"a1b2c3","#,
            r#""snapshot_id":42,"taken_at":"2026-07-15T12:00:00Z","item_count":2,"items":["#,
            r#"{"position":0,"playlist_item_id":"56B44F6D10557CC6","video_id":"dQw4w9WgXcQ","#,
            r#""title_at_time":"Great Song","channel_at_time":"Great Artist","added_at":null,"#,
            r#""best_title":"Great Song","best_channel_title":"Great Artist","duration_s":213,"#,
            r#""status":"available","url":"https://youtu.be/dQw4w9WgXcQ"},"#,
            r#"{"position":1,"playlist_item_id":"","video_id":"deadbeefdea","#,
            r#""title_at_time":"Gone Song","channel_at_time":null,"added_at":null,"#,
            r#""best_title":"Gone Song","best_channel_title":null,"duration_s":null,"#,
            r#""status":"unknown","url":"https://youtu.be/deadbeefdea"}]}"#
        );
        assert_eq!(json, expected);
        let back: SnapshotExport = serde_json::from_str(&json).unwrap();
        assert_eq!(back, to_export(&snap()));
    }

    #[test]
    fn greyed_out_rows_are_unknown_and_the_rest_available() {
        let export = to_export(&snap());
        assert_eq!(export.items[0].status, "available");
        assert_eq!(export.items[1].status, "unknown");
    }

    #[test]
    fn durations_parse_as_youtube_writes_them() {
        assert_eq!(duration_secs("3:45"), Some(225));
        assert_eq!(duration_secs("0:07"), Some(7));
        assert_eq!(duration_secs("1:02:03"), Some(3723));
        assert_eq!(duration_secs("42"), Some(42));
        assert_eq!(duration_secs(""), None);
        assert_eq!(duration_secs("3:4x"), None);
        assert_eq!(duration_secs("3:75"), None);
        assert_eq!(duration_secs("1:2:3:4"), None);
        assert_eq!(duration_secs("-1:00"), None);
    }

    #[test]
    fn sanitize_path_segment_strips_invalid_windows_characters() {
        assert_eq!(sanitize_path_segment("a/b\\c:d*e?f\"g<h>i|j"), "a_b_c_d_e_f_g_h_i_j");
        assert_eq!(sanitize_path_segment("trailing dot."), "trailing dot");
        assert_eq!(sanitize_path_segment("tab\there"), "tab_here");
        assert_eq!(sanitize_path_segment("   "), "untitled");
        assert_eq!(sanitize_path_segment(""), "untitled");
    }

    #[test]
    fn file_names_swap_colons_for_dashes() {
        assert_eq!(file_stem(at()), "2026-07-15T12-00-00+00-00");
        let mut s = snap();
        s.title = Some("Weird / Name: Playlist".into());
        let path = snapshot_path(Path::new("/b"), &to_export(&s));
        let folder = Path::new("/b").join("Weird _ Name_ Playlist__PLabc");
        assert_eq!(path, folder.join("2026-07-15T12-00-00+00-00.json"));
        assert!(is_backup_name("2026-07-15T12-00-00+00-00.json"));
        assert!(is_backup_name("2026-07-15T12-00-00Z.json"));
        assert!(is_backup_name("2026-07-15T12-00-00-05-30.json"));
        assert!(!is_backup_name("2026-07-15T12:00:00Z.json"));
        assert!(!is_backup_name("2026-07-15T12-00-00Z.json.bak"));
        assert!(!is_backup_name("notes.json"));
    }

    #[test]
    fn name_instants_read_every_offset_form() {
        assert_eq!(name_instant("2026-07-15T12-00-00Z.json"), Some(at()));
        assert_eq!(name_instant("2026-07-15T12-00-00+00-00.json"), Some(at()));
        assert_eq!(name_instant("2026-07-15T14-00-00+02-00.json"), Some(at()));
        assert_eq!(name_instant("2026-07-15T07-00-00-05-00.json"), Some(at()));
        assert_eq!(name_instant("2026-13-45T12-00-00Z.json"), None);
    }

    #[test]
    fn written_backups_read_back() {
        let tmp = tempfile::tempdir().unwrap();
        let path = write_snapshot(tmp.path(), &snap()).unwrap();
        let rel = path.strip_prefix(tmp.path()).unwrap();
        assert_eq!(rel, Path::new("My Playlist__PLabc").join("2026-07-15T12-00-00+00-00.json"));
        let back: SnapshotExport =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(back, to_export(&snap()));
    }

    #[test]
    fn dir_and_keep_settings_fall_back_to_defaults() {
        let data = Path::new(if cfg!(windows) { r"C:\data" } else { "/data" });
        let abs = if cfg!(windows) { r"D:\bk" } else { "/bk" };
        assert_eq!(resolve_dir(None, data), data.join("backups"));
        assert_eq!(resolve_dir(Some("  "), data), data.join("backups"));
        assert_eq!(resolve_dir(Some("relative/dir"), data), data.join("backups"));
        assert_eq!(resolve_dir(Some(abs), data), PathBuf::from(abs));
        assert_eq!(parse_keep(None), 30);
        assert_eq!(parse_keep(Some("0")), 30);
        assert_eq!(parse_keep(Some("x")), 30);
        assert_eq!(parse_keep(Some(" 5 ")), 5);
    }

    /// Forbidden roots that are subfolders of a temp dir, never the real ones.
    fn fake_roots(tmp: &Path) -> Vec<PathBuf> {
        vec![
            tmp.join("Roaming").join("PlaylistForge"),
            tmp.join("Local").join("com.limusic.desktop"),
        ]
    }

    fn stored(path: &Path) -> &str {
        path.to_str().unwrap()
    }

    #[test]
    fn a_folder_inside_a_forbidden_root_falls_back_to_the_default() {
        let tmp = tempfile::tempdir().unwrap();
        let roots = fake_roots(tmp.path());
        let data = tmp.path().join("data");
        let inside = roots[0].join("backups");
        // Missing on disk, then existing: both are refused.
        assert_eq!(resolve_dir_with(Some(stored(&inside)), &data, &roots), data.join("backups"));
        std::fs::create_dir_all(&inside).unwrap();
        assert_eq!(resolve_dir_with(Some(stored(&inside)), &data, &roots), data.join("backups"));
        let deep = roots[1].join("a").join("b");
        assert_eq!(resolve_dir_with(Some(stored(&deep)), &data, &roots), data.join("backups"));
        // Sneaking in with `..` does not help.
        let sneaky = tmp.path().join("Roaming").join("x").join("..").join("PlaylistForge");
        assert_eq!(resolve_dir_with(Some(stored(&sneaky)), &data, &roots), data.join("backups"));
        // And the settings say so, rather than showing the default as if it had been picked.
        assert!(rejected_with(Some(stored(&deep)), &roots));
        let own = tmp.path().join("Music").join("backups");
        for fine in [Some(stored(&own)), Some(""), Some("relative"), None] {
            assert!(!rejected_with(fine, &roots), "{fine:?}");
        }
    }

    #[test]
    fn a_forbidden_root_itself_falls_back_to_the_default() {
        let tmp = tempfile::tempdir().unwrap();
        let roots = fake_roots(tmp.path());
        let data = tmp.path().join("data");
        for root in &roots {
            assert_eq!(resolve_dir_with(Some(stored(root)), &data, &roots), data.join("backups"));
        }
        std::fs::create_dir_all(&roots[0]).unwrap();
        assert!(is_forbidden(&roots[0], &roots));
    }

    #[test]
    fn a_sibling_with_a_similar_name_is_allowed() {
        let tmp = tempfile::tempdir().unwrap();
        let roots = fake_roots(tmp.path());
        let data = tmp.path().join("data");
        let sibling = tmp.path().join("Roaming").join("PlaylistForge2").join("backups");
        assert_eq!(resolve_dir_with(Some(stored(&sibling)), &data, &roots), sibling);
        std::fs::create_dir_all(&sibling).unwrap();
        std::fs::create_dir_all(&roots[0]).unwrap();
        assert_eq!(resolve_dir_with(Some(stored(&sibling)), &data, &roots), sibling);
        let parent = tmp.path().join("Roaming");
        assert!(!is_forbidden(&parent, &roots));
    }

    #[cfg(windows)]
    #[test]
    fn a_different_case_is_still_forbidden_on_windows() {
        let tmp = tempfile::tempdir().unwrap();
        let roots = fake_roots(tmp.path());
        let data = tmp.path().join("data");
        let shouty = tmp.path().join("ROAMING").join("playlistforge").join("Backups");
        assert_eq!(resolve_dir_with(Some(stored(&shouty)), &data, &roots), data.join("backups"));
        std::fs::create_dir_all(&roots[0]).unwrap();
        assert_eq!(resolve_dir_with(Some(stored(&shouty)), &data, &roots), data.join("backups"));
    }

    #[test]
    fn pruning_inside_a_forbidden_root_deletes_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let roots = fake_roots(tmp.path());
        for dir in [roots[0].join("backups"), roots[0].clone()] {
            let folder = dir.join("Mix__PL1");
            for day in 1..=3 {
                touch(&folder.join(format!("2026-07-0{day}T12-00-00Z.json")));
            }
            assert_eq!(prune_files_with(&dir, 1, &roots).unwrap(), 0);
            assert_eq!(std::fs::read_dir(&folder).unwrap().count(), 3);
        }
        // The same tree outside a forbidden root is pruned as usual.
        let free = tmp.path().join("free");
        let folder = free.join("Mix__PL1");
        for day in 1..=3 {
            touch(&folder.join(format!("2026-07-0{day}T12-00-00Z.json")));
        }
        assert_eq!(prune_files_with(&free, 1, &roots).unwrap(), 2);
    }

    #[test]
    fn the_real_forbidden_roots_name_the_other_apps() {
        // Only builds paths; reads and writes nothing.
        let roots = forbidden_roots();
        assert!(roots.iter().all(|r| r.is_absolute()));
        if let Some(config) = dirs::config_dir() {
            assert!(roots.contains(&config.join("PlaylistForge")));
            assert!(roots.contains(&config.join("com.limusic.desktop")));
        }
        if let Some(local) = dirs::data_local_dir() {
            assert!(roots.contains(&local.join("com.limusic.desktop")));
        }
        assert!(!roots.iter().any(|r| r.ends_with(crate::paths::IDENTIFIER)));
    }

    fn touch(path: &Path) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, "{}").unwrap();
    }

    #[test]
    fn pruning_keeps_the_newest_and_touches_nothing_else() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("backups");
        let folder = dir.join("Mix__PL1");
        for day in 1..=5 {
            touch(&folder.join(format!("2026-07-0{day}T12-00-00+00-00.json")));
        }
        // An older one with a `Z`, and the newest one written in another offset (12:00 UTC).
        touch(&folder.join("2026-06-30T12-00-00Z.json"));
        touch(&folder.join("2026-07-06T14-00-00+02-00.json"));
        // Not ours: wrong names, a folder named like a backup, a folder without `__`, a file in
        // the root, and a sibling of the backups folder.
        touch(&folder.join("notes.txt"));
        touch(&folder.join("2026-01-01T00-00-00Z.json.bak"));
        touch(&folder.join("2026-13-01T00-00-00Z.json"));
        std::fs::create_dir_all(folder.join("2020-01-01T00-00-00Z.json")).unwrap();
        touch(&dir.join("plain").join("2020-01-01T00-00-00Z.json"));
        touch(&dir.join("2020-01-01T00-00-00Z.json"));
        touch(&tmp.path().join("other__X").join("2020-01-01T00-00-00Z.json"));

        assert_eq!(prune_files(&dir, 3).unwrap(), 4);

        let mut left: Vec<String> = std::fs::read_dir(&folder)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        left.sort();
        assert_eq!(
            left,
            [
                "2020-01-01T00-00-00Z.json", // the folder
                "2026-01-01T00-00-00Z.json.bak",
                "2026-07-04T12-00-00+00-00.json",
                "2026-07-05T12-00-00+00-00.json",
                "2026-07-06T14-00-00+02-00.json",
                "2026-13-01T00-00-00Z.json",
                "notes.txt",
            ]
        );
        assert!(folder.join("2020-01-01T00-00-00Z.json").is_dir());
        assert!(dir.join("plain").join("2020-01-01T00-00-00Z.json").exists());
        assert!(dir.join("2020-01-01T00-00-00Z.json").exists());
        assert!(tmp.path().join("other__X").join("2020-01-01T00-00-00Z.json").exists());
        // Run again: already at the limit, nothing more goes.
        assert_eq!(prune_files(&dir, 3).unwrap(), 0);
    }

    #[test]
    fn pruning_a_missing_folder_is_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(prune_files(&tmp.path().join("nope"), 1).unwrap(), 0);
    }

    #[cfg(unix)]
    #[test]
    fn pruning_never_follows_a_link_out_of_the_folder() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("backups");
        let outside = tmp.path().join("outside");
        for day in 1..=3 {
            touch(&outside.join(format!("2026-07-0{day}T12-00-00Z.json")));
        }
        std::fs::create_dir_all(&dir).unwrap();
        std::os::unix::fs::symlink(&outside, dir.join("Linked__PL")).unwrap();
        let real = dir.join("Real__PL");
        touch(&real.join("2026-07-09T12-00-00Z.json"));
        std::os::unix::fs::symlink(
            outside.join("2026-07-01T12-00-00Z.json"),
            real.join("2026-07-01T12-00-00Z.json"),
        )
        .unwrap();

        assert_eq!(prune_files(&dir, 1).unwrap(), 0);
        assert_eq!(std::fs::read_dir(&outside).unwrap().count(), 3);
    }
}
