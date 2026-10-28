//! Export the profile to one `.lmfprofile` file.
//!
//! The live database is never modified. A snapshot (`VACUUM INTO`) goes to a scratch directory
//! `<data>/tmp/profile-export-<ts>/`, is scrubbed there (scrub.rs) and zipped (archive.rs); the
//! scratch directory is removed afterwards, whatever happened.
//!
//! **Invariants.**
//! - `profile.sqlite` in the archive never holds a secret, with or without `include_secrets`:
//!   the scrub moves them out of the snapshot into a [`SecretsBundle`], and the snapshot is
//!   vacuumed afterwards so the old values are not left in free pages. With `include_secrets` the
//!   bundle is sealed with the passphrase into `secrets.bin`; without it the bundle is dropped
//!   (zeroized) and the secrets are simply not exported.
//! - The passphrase is checked before any work, so a short one fails fast.
//! - Only `covers/playlists/**` (the user's own playlist covers) and `app-icon.png` always go in;
//!   the rest of `covers/` (extracted caches) only with `include_caches`.
//! - The only directory this deletes is the scratch directory it created ([`Scratch`] refuses
//!   anything that is not `<data>/tmp/profile-export-*`).

use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use rusqlite::Connection;
use serde::Serialize;
use ytdata::auth::store::TokenStore;

use super::{
    archive, credentials, crypto, scrub, Manifest, ProfileError, Result, SourceOs, WebStorage,
    DB_ENTRY, FORMAT_VERSION, WEBSTORAGE_KEYS,
};

/// The live database's file name in the data directory.
pub(crate) const DB_FILE: &str = "limusic.sqlite";
/// `<data>/client_secret.json`: the OAuth client of the Data API.
pub(crate) const CLIENT_SECRET_FILE: &str = "client_secret.json";

pub struct ExportRequest {
    /// Where the `.lmfprofile` goes. An existing file is overwritten.
    pub dest: PathBuf,
    pub include_secrets: bool,
    pub include_caches: bool,
    /// Required (at least [`super::MIN_PASSPHRASE_CHARS`] chars) when `include_secrets`.
    pub passphrase: Option<String>,
    /// The webview's localStorage, from the frontend; filtered to [`WEBSTORAGE_KEYS`].
    pub webstorage: WebStorage,
}

pub struct ExportCtx {
    pub data_dir: PathBuf,
    pub app_version: String,
    pub token_store: Arc<dyn TokenStore>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ExportReport {
    pub path: String,
    pub bytes: u64,
    pub manifest: Manifest,
    /// `(channel id, code)` of Data API tokens that could not be read from the store, so they are
    /// not in `secrets.bin`.
    pub token_failures: Vec<(String, String)>,
}

/// A scratch directory `<data>/tmp/<prefix><ts>` this module created, removed on drop unless
/// [`Scratch::keep`] disarms it. Removal goes through [`remove_scratch`].
pub(crate) struct Scratch {
    dir: PathBuf,
    data: PathBuf,
    prefix: &'static str,
    armed: bool,
}

impl Scratch {
    /// Create a fresh, empty `<data>/tmp/<prefix><unix ts>[-n]`.
    pub(crate) fn create(data: &Path, prefix: &'static str) -> io::Result<Scratch> {
        if !data.is_absolute() {
            return Err(io::Error::other("the data directory is not absolute"));
        }
        let tmp = data.join("tmp");
        std::fs::create_dir_all(&tmp)?;
        let ts = crate::migrate_upstream::now_secs();
        let mut n = 0u32;
        loop {
            let name = if n == 0 { format!("{prefix}{ts}") } else { format!("{prefix}{ts}-{n}") };
            let dir = tmp.join(name);
            match std::fs::create_dir(&dir) {
                Ok(()) => {
                    return Ok(Scratch { dir, data: data.to_path_buf(), prefix, armed: true })
                }
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => n += 1,
                Err(e) => return Err(e),
            }
        }
    }

    pub(crate) fn path(&self) -> &Path {
        &self.dir
    }

    /// Stop removing it on drop and hand back its path (import staging that must outlive phase 1).
    pub(crate) fn keep(mut self) -> PathBuf {
        self.armed = false;
        self.dir.clone()
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        if self.armed {
            let _ = remove_scratch(&self.data, &self.dir, self.prefix);
        }
    }
}

/// Remove `dir` recursively, but only when it is exactly `<data>/tmp/<prefix>…` (a direct child of
/// the data directory's `tmp`, named with the prefix, not a link). Anything else is refused
/// without touching it. A missing `dir` is fine. Also drops `<data>/tmp` when that leaves it empty.
pub(crate) fn remove_scratch(data: &Path, dir: &Path, prefix: &str) -> io::Result<()> {
    let tmp = data.join("tmp");
    let named = dir.file_name().is_some_and(|n| n.to_string_lossy().starts_with(prefix));
    if !data.is_absolute() || prefix.is_empty() || !named || dir.parent() != Some(tmp.as_path()) {
        return Err(io::Error::other("refusing to delete outside <data>/tmp"));
    }
    match std::fs::symlink_metadata(dir) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
        Ok(m) if m.file_type().is_symlink() => {
            return Err(io::Error::other("the scratch directory is a link"));
        }
        Ok(_) => {}
    }
    std::fs::remove_dir_all(dir)?;
    // Non-recursive: only succeeds when nothing else lives in `tmp`.
    let _ = std::fs::remove_dir(&tmp);
    Ok(())
}

/// Export the profile. Synchronous; callers wrap it in `spawn_blocking`.
pub fn export(ctx: &ExportCtx, req: ExportRequest) -> Result<ExportReport> {
    // Fail fast, before any work, on a missing or short passphrase.
    let pass = if req.include_secrets {
        let p = req.passphrase.as_deref().ok_or(ProfileError::PassphraseTooShort)?;
        crypto::check_passphrase(p)?;
        Some(p)
    } else {
        None
    };
    let live = ctx.data_dir.join(DB_FILE);
    if !live.is_file() {
        return Err(ProfileError::Other(format!("no database at {}", live.display())));
    }
    let scratch = Scratch::create(&ctx.data_dir, "profile-export-")?;
    let out = export_inner(ctx, &req, pass, &live, scratch.path());
    drop(scratch); // removes the scratch directory, on success and on failure alike
    out
}

fn export_inner(
    ctx: &ExportCtx,
    req: &ExportRequest,
    pass: Option<&str>,
    live: &Path,
    work: &Path,
) -> Result<ExportReport> {
    let snap = work.join(DB_ENTRY);
    scrub::snapshot(live, &snap)?;

    let mut counts;
    let mut sealed: Option<Vec<u8>> = None;
    let mut secret_counts = None;
    let mut token_failures = Vec::new();
    {
        let conn = Connection::open(&snap)?;
        // No `-wal` left beside the snapshot, and deleted rows overwritten rather than just
        // unlinked.
        conn.query_row("PRAGMA journal_mode=DELETE", [], |r| r.get::<_, String>(0))?;
        conn.execute_batch("PRAGMA secure_delete=ON;")?;
        let mut bundle = scrub::scrub(&conn, req.include_caches)?;
        // Rewrites the file without the pages the scrub freed.
        conn.execute_batch("VACUUM;")?;
        counts = scrub::counts(&conn)?;
        if let Some(pass) = pass {
            let ids = scrub::ytdata_channel_ids(&conn)?;
            let (tokens, report) = credentials::collect_tokens(ctx.token_store.as_ref(), &ids);
            token_failures = report.failed;
            bundle.ytdata_refresh_tokens = tokens;
            bundle.client_secret_json = read_client_secret(&ctx.data_dir)?;
            secret_counts = Some(bundle.counts());
            sealed = Some(crypto::seal(&bundle, pass)?);
        }
        // Without `include_secrets` the bundle is dropped here, wiped: the secrets are not exported.
        drop(bundle);
    }

    let files = gather_files(&ctx.data_dir, req.include_caches)?;
    counts.files = files.len() as u64;

    let webstorage: WebStorage = req
        .webstorage
        .iter()
        .filter(|(k, _)| WEBSTORAGE_KEYS.contains(&k.as_str()))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();

    let manifest = Manifest {
        format_version: FORMAT_VERSION,
        app_version: ctx.app_version.clone(),
        source_os: SourceOs::current(),
        source_data_dir: ctx.data_dir.display().to_string(),
        created_at: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        includes_secrets: req.include_secrets,
        includes_caches: req.include_caches,
        counts,
        secret_counts,
    };

    if let Some(parent) = req.dest.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)?;
    }
    archive::write(
        &req.dest,
        &archive::ArchiveInput {
            manifest: &manifest,
            db_file: &snap,
            webstorage: &webstorage,
            files: &files,
            secrets: sealed.as_deref(),
        },
    )?;
    let bytes = std::fs::metadata(&req.dest)?.len();
    Ok(ExportReport { path: req.dest.display().to_string(), bytes, manifest, token_failures })
}

/// `<data>/client_secret.json` verbatim, or `None` when there is none.
fn read_client_secret(data: &Path) -> Result<Option<String>> {
    match std::fs::read_to_string(data.join(CLIENT_SECRET_FILE)) {
        Ok(s) if s.trim().is_empty() => Ok(None),
        Ok(s) => Ok(Some(s)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// The files that go in the archive: `(path relative to the data dir with '/', absolute source)`.
/// `covers/playlists/**` always, the rest of `covers/` only with `include_caches`, and
/// `app-icon.png`. Links are skipped, never followed.
fn gather_files(data: &Path, include_caches: bool) -> Result<Vec<(String, PathBuf)>> {
    let mut out = Vec::new();
    let covers = data.join("covers");
    if std::fs::symlink_metadata(&covers).is_ok_and(|m| m.is_dir()) {
        for entry in std::fs::read_dir(&covers)? {
            let entry = entry?;
            let ft = entry.file_type()?;
            if ft.is_symlink() {
                continue;
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            let user_owned = ft.is_dir() && name == "playlists";
            if !user_owned && !include_caches {
                continue;
            }
            let rel = format!("covers/{name}");
            if ft.is_dir() {
                walk(&entry.path(), &rel, &mut out)?;
            } else if ft.is_file() {
                out.push((rel, entry.path()));
            }
        }
    }
    let icon = data.join("app-icon.png");
    if std::fs::symlink_metadata(&icon).is_ok_and(|m| m.is_file()) {
        out.push(("app-icon.png".to_string(), icon));
    }
    out.sort();
    Ok(out)
}

fn walk(dir: &Path, rel: &str, out: &mut Vec<(String, PathBuf)>) -> Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let ft = entry.file_type()?;
        if ft.is_symlink() {
            continue;
        }
        let child = format!("{rel}/{}", entry.file_name().to_string_lossy());
        if ft.is_dir() {
            walk(&entry.path(), &child, out)?;
        } else if ft.is_file() {
            out.push((child, entry.path()));
        }
    }
    Ok(())
}

/// Fixtures shared with import.rs's tests: a data directory with a real database (every
/// migration) holding secrets and machine state, covers, an icon and a client secret.
#[cfg(test)]
pub(crate) mod testutil {
    use super::*;

    pub const ACTIVE_COOKIE: &str = "SAPISID=ACTIVE-COOKIE-MARKER; SID=a";
    pub const ACCOUNT_COOKIE: &str = "SAPISID=ACCOUNT-COOKIE-MARKER; SID=b";
    pub const LASTFM: &str = "LASTFM-MARKER-KEY";
    pub const LISTENBRAINZ: &str = "LB-MARKER-TOKEN";
    pub const REFRESH: &str = "REFRESH-MARKER-TOKEN";
    pub const CLIENT_SECRET: &str = r#"{"installed":{"client_id":"cid-1.apps.googleusercontent.com","client_secret":"CLIENT-SECRET-MARKER"}}"#;
    pub const CHANNEL: &str = "UCtestchannel0000000001";
    pub const PASSPHRASE: &str = "correct horse battery";

    /// Every secret value above that must never be found in a `profile.sqlite`.
    pub const SECRET_MARKERS: [&str; 5] =
        ["ACTIVE-COOKIE-MARKER", "ACCOUNT-COOKIE-MARKER", LASTFM, LISTENBRAINZ, "REFRESH-MARKER"];

    pub struct Fixture {
        pub _tmp: tempfile::TempDir,
        pub data: PathBuf,
        pub store: Arc<ytdata::auth::store::InMemoryTokenStore>,
    }

    pub fn fixture() -> Fixture {
        let tmp = tempfile::tempdir().unwrap();
        let data = tmp.path().join("data");
        std::fs::create_dir_all(&data).unwrap();
        {
            let db = crate::db::Db::open(&data.join(DB_FILE)).expect("open db");
            db.set_setting("session_cookie", ACTIVE_COOKIE);
            db.set_setting("lastfm_session_key", LASTFM);
            db.set_setting("listenbrainz_token", LISTENBRAINZ);
            db.set_setting("monitor.schedule_time", "03:30");
            db.set_setting("autostart", "true");
            db.set_setting("limusic_marker", "kept-setting");
            db.upsert_account(&crate::db::StoredAccount {
                id: "acc1".into(),
                session_cookie: ACCOUNT_COOKIE.into(),
                data_sync_id: None,
                selected_identity_json: None,
                account_json: None,
                visitor_data: None,
                added_at: 1,
            })
            .unwrap();
        }
        {
            let conn = Connection::open(data.join(DB_FILE)).unwrap();
            conn.execute(
                "INSERT INTO ytdata_accounts(channel_id, title, added_at) VALUES(?1, 'Me', 1)",
                [CHANNEL],
            )
            .unwrap();
        }
        let pl = data.join("covers").join("playlists");
        std::fs::create_dir_all(&pl).unwrap();
        std::fs::write(pl.join("1.jpg"), b"playlist-cover").unwrap();
        std::fs::create_dir_all(data.join("covers").join("ab")).unwrap();
        std::fs::write(data.join("covers").join("ab").join("c.jpg"), b"cache-cover").unwrap();
        std::fs::write(data.join("app-icon.png"), b"icon").unwrap();
        std::fs::write(data.join(CLIENT_SECRET_FILE), CLIENT_SECRET).unwrap();
        let store = Arc::new(ytdata::auth::store::InMemoryTokenStore::default());
        store.save(CHANNEL, REFRESH).unwrap();
        Fixture { _tmp: tmp, data, store }
    }

    pub fn ctx(fx: &Fixture) -> ExportCtx {
        ExportCtx {
            data_dir: fx.data.clone(),
            app_version: "9.9.9-test".into(),
            token_store: fx.store.clone(),
        }
    }

    pub fn request(dest: PathBuf, secrets: bool, caches: bool) -> ExportRequest {
        let mut web = WebStorage::new();
        web.insert("appearance".into(), "{\"dark\":true}".into());
        web.insert("not-allowed-key".into(), "x".into());
        ExportRequest {
            dest,
            include_secrets: secrets,
            include_caches: caches,
            passphrase: secrets.then(|| PASSPHRASE.to_string()),
            webstorage: web,
        }
    }

    /// Whether any byte sequence of a marker is in `file` (the raw SQLite file, free pages too).
    pub fn file_has_marker(file: &Path) -> Option<&'static str> {
        let bytes = std::fs::read(file).unwrap();
        SECRET_MARKERS.iter().copied().find(|m| bytes.windows(m.len()).any(|w| w == m.as_bytes()))
    }
}

#[cfg(test)]
mod tests {
    use super::testutil::*;
    use super::*;

    fn out_dir(fx: &Fixture) -> PathBuf {
        let d = fx._tmp.path().join("out");
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn profile_export_without_secrets_leaves_none_in_the_sqlite() {
        let fx = fixture();
        let dest = out_dir(&fx).join("p.lmfprofile");
        let report = export(&ctx(&fx), request(dest.clone(), false, false)).unwrap();
        assert!(!report.manifest.includes_secrets && report.manifest.secret_counts.is_none());
        assert!(report.bytes > 0 && dest.is_file());

        let into = fx._tmp.path().join("x");
        let ex = archive::extract(&dest, &into).unwrap();
        assert_eq!(ex.secrets, None);
        assert_eq!(file_has_marker(&ex.db_file), None);
        // Only whitelisted localStorage keys travel.
        assert!(ex.webstorage.contains_key("appearance"));
        assert!(!ex.webstorage.contains_key("not-allowed-key"));
        // The live database keeps its secrets: only the snapshot was scrubbed.
        let live = Connection::open(fx.data.join(DB_FILE)).unwrap();
        let cookie: String = live
            .query_row("SELECT value FROM settings WHERE key='session_cookie'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(cookie, ACTIVE_COOKIE);
        // The scratch directory is gone.
        assert!(!fx.data.join("tmp").exists());
    }

    #[test]
    fn profile_export_with_secrets_keeps_them_out_of_the_sqlite_but_seals_them() {
        let fx = fixture();
        let dest = out_dir(&fx).join("p.lmfprofile");
        let report = export(&ctx(&fx), request(dest.clone(), true, false)).unwrap();
        assert!(report.manifest.includes_secrets);
        let sc = report.manifest.secret_counts.clone().unwrap();
        assert_eq!(sc.ytdata_tokens, 1);
        assert!(sc.client_secret && sc.lastfm && sc.listenbrainz);
        assert!(report.token_failures.is_empty());

        let ex = archive::extract(&dest, &fx._tmp.path().join("x")).unwrap();
        assert_eq!(file_has_marker(&ex.db_file), None);
        let sealed = ex.secrets.expect("secrets.bin");
        // Sealed: no marker in clear, and the right passphrase opens it.
        let text = String::from_utf8_lossy(&sealed).into_owned();
        assert!(!text.contains("REFRESH-MARKER") && !text.contains("CLIENT-SECRET-MARKER"));
        let bundle = crypto::unseal(&sealed, PASSPHRASE).unwrap();
        assert_eq!(bundle.lastfm_session_key.as_deref(), Some(LASTFM));
        assert_eq!(bundle.ytdata_refresh_tokens.get(CHANNEL).map(String::as_str), Some(REFRESH));
        assert_eq!(bundle.client_secret_json.as_deref(), Some(CLIENT_SECRET));
        assert!(crypto::unseal(&sealed, "another passphrase!").is_err());
    }

    #[test]
    fn profile_export_checks_the_passphrase_before_any_work() {
        let fx = fixture();
        let dest = out_dir(&fx).join("p.lmfprofile");
        let mut req = request(dest.clone(), true, false);
        req.passphrase = Some("short".into());
        let err = export(&ctx(&fx), req).unwrap_err();
        assert_eq!(err.code(), "passphrase_too_short");
        let mut req = request(dest.clone(), true, false);
        req.passphrase = None;
        assert_eq!(export(&ctx(&fx), req).unwrap_err().code(), "passphrase_too_short");
        // Nothing was created: no scratch directory, no file.
        assert!(!dest.exists() && !fx.data.join("tmp").exists());
    }

    #[test]
    fn profile_export_caches_only_when_asked() {
        let fx = fixture();
        let a =
            export(&ctx(&fx), request(out_dir(&fx).join("a.lmfprofile"), false, false)).unwrap();
        let b = export(&ctx(&fx), request(out_dir(&fx).join("b.lmfprofile"), false, true)).unwrap();
        // playlists/1.jpg + icon, versus + ab/c.jpg.
        assert_eq!(a.manifest.counts.files, 2);
        assert_eq!(b.manifest.counts.files, 3);
        assert!(b.manifest.includes_caches && !a.manifest.includes_caches);
    }

    #[test]
    fn profile_export_scratch_refuses_foreign_paths() {
        let fx = fixture();
        let data = &fx.data;
        // Not under <data>/tmp, wrong prefix, the data dir itself, tmp itself.
        for bad in [
            data.clone(),
            data.join("tmp"),
            data.join("covers"),
            data.join("tmp").join("other-1"),
            data.join("tmp").join("profile-export-1").join("deeper"),
        ] {
            assert!(remove_scratch(data, &bad, "profile-export-").is_err(), "{}", bad.display());
        }
        assert!(data.join("covers").is_dir());
        // The real thing works.
        let s = Scratch::create(data, "profile-export-").unwrap();
        let p = s.path().to_path_buf();
        assert!(p.is_dir());
        drop(s);
        assert!(!p.exists());
        // keep() disarms.
        let s = Scratch::create(data, "profile-export-").unwrap();
        let p = s.keep();
        assert!(p.is_dir());
        remove_scratch(data, &p, "profile-export-").unwrap();
    }
}
