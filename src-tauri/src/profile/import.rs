//! Import a `.lmfprofile` file, replacing the current profile.
//!
//! Same two phases as migrate_upstream.rs, for the same reason: the database cannot be swapped
//! under a running app.
//!
//! - **Phase 1** ([`prepare`], app running). The archive is extracted into a staging directory
//!   `<data>/tmp/profile-import-<ts>/`; the secrets are unsealed and written into the *staged*
//!   database (or the accounts are dropped when there are none); paths are remapped; the Data API
//!   refresh tokens go into the token store right away (the keyring is not swappable and cannot be
//!   staged); `profile-import.pending` is written and the caller restarts. The marker carries the
//!   staging directory's name and the parts of the report known so far: never a secret.
//! - **Phase 2** ([`apply_pending`], next launch, from `run()` before anything opens the
//!   database). The current `limusic.sqlite` (+`-wal`/`-shm`), `covers/`, `app-icon.png` and, when
//!   the import brings one, `client_secret.json` are moved aside to `<data>/pre-import-<ts>/`; the
//!   staged ones are moved in; any failure puts everything back. The outcome is left in
//!   `profile-import.result.json` ([`take_result`]) and the remapped localStorage in
//!   `profile-import.webstorage.json` ([`take_webstorage`]) for the UI to apply on first boot.
//!   The marker is honoured for [`PENDING_TTL`] (10 minutes) only; an older one is left alone,
//!   marked `expired`, for the UI to cancel ([`cancel`]) or replace by importing again.
//!
//! **Invariants.**
//! - Nothing outside the data directory is touched, and the only directories deleted are the ones
//!   this module created: `<data>/tmp/profile-import-*` and `<data>/tmp/profile-preview-*`
//!   (checked by [`export::remove_scratch`]). What the profile replaces is moved aside, never
//!   deleted.
//! - The [`SecretsBundle`] lives only inside [`prepare`] and is dropped (wiped) as soon as the
//!   staged database and the token store have it. The unsealed values are written to disk in two
//!   places only, both the app's own: the staged database and the staged `client_secret.json`.
//! - The source file is only read.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use ytdata::auth::store::TokenStore;

use super::export::{remove_scratch, Scratch, CLIENT_SECRET_FILE, DB_FILE};
use super::{
    archive, credentials, crypto, remap, scrub, Counts, PathRoot, ProfileError, Result,
    RootDecision, SecretsBundle, SourceOs, WebStorage,
};
use crate::migrate_upstream::{self as mu, FsOps};

/// The marker phase 1 leaves in the data directory.
pub const PENDING_FILE: &str = "profile-import.pending";
/// What phase 2 found, for the UI to read once.
pub const RESULT_FILE: &str = "profile-import.result.json";
/// The remapped localStorage phase 2 leaves for the UI to apply on first boot.
pub const WEBSTORAGE_FILE: &str = "profile-import.webstorage.json";
/// How long a marker stays valid, in seconds (same window as migrate_upstream.rs).
pub const PENDING_TTL: u64 = mu::PENDING_TTL;
/// How far in the future `requested_at` may be (clock adjustments) before it counts as invalid.
const PENDING_FUTURE_SKEW: u64 = 60;

const STAGE_PREFIX: &str = "profile-import-";
const PREVIEW_PREFIX: &str = "profile-preview-";
const DB_SUFFIXES: [&str; 3] = ["", "-wal", "-shm"];
/// The name the staged localStorage has inside the staging directory.
const STAGED_WEBSTORAGE: &str = "webstorage.json";

pub struct ImportCtx {
    pub data_dir: PathBuf,
    pub token_store: Arc<dyn TokenStore>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Preview {
    pub manifest: super::Manifest,
    pub roots: Vec<PathRoot>,
    /// Whether the secrets can be opened: the passphrase was given and verified. Always `true`
    /// for a file without secrets (nothing to open).
    pub secrets_ok: bool,
}

/// What remapping did (same shape as `remap::RemapReport`, which is not `Deserialize`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemapCounts {
    pub rewritten: u64,
    pub kept: u64,
    pub dropped: u64,
}

/// What happened to the Data API refresh tokens (same shape as `credentials::TokenReport`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenSummary {
    /// Channel ids saved into the token store.
    pub saved: Vec<String>,
    /// `(channel id, code)`: `client_mismatch`, `keyring_unavailable`, …
    pub failed: Vec<(String, String)>,
}

/// The outcome, for the UI summary. Never holds a secret.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImportReport {
    /// `done`, `retry` (a file stayed held; the marker stays for the next launch within the
    /// window), `expired` (past the window: nothing was done) or `error`.
    pub status: String,
    pub error: Option<String>,
    /// The counts the file announced.
    pub imported: Counts,
    pub remap: RemapCounts,
    pub tokens: TokenSummary,
    pub secrets_restored: bool,
    /// `monitor.schedule_time` of the imported profile, for the "re-register the monitor" offer.
    pub monitor_time: Option<String>,
    /// Whether the imported profile had login autostart on.
    pub autostart_was_on: bool,
    /// Codes: `client_secret_kept`, `keyring_unavailable`, …
    pub warnings: Vec<String>,
    /// Where the previous profile went, when there was one.
    pub aside: Option<String>,
}

/// The marker phase 1 writes.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct Pending {
    /// Unix seconds. Only honoured for [`PENDING_TTL`] after it.
    #[serde(default)]
    requested_at: u64,
    /// Name (not path) of the staging directory under `<data>/tmp`.
    #[serde(default)]
    staging: String,
    /// What phase 2 last made of it (`retry`, `expired`).
    #[serde(default)]
    last_status: Option<String>,
    /// The report parts phase 1 already knows.
    #[serde(default)]
    report: ImportReport,
}

/// The marker as the UI sees it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PendingInfo {
    pub requested_at: u64,
    pub expired: bool,
    pub last_status: Option<String>,
}

fn pending_expired(p: &Pending, now: u64) -> bool {
    p.requested_at == 0
        || p.requested_at > now.saturating_add(PENDING_FUTURE_SKEW)
        || now.saturating_sub(p.requested_at) > PENDING_TTL
}

fn remap_ctx(manifest: &super::Manifest, data: &Path) -> remap::RemapContext {
    remap::RemapContext {
        source_os: manifest.source_os,
        source_data_dir: manifest.source_data_dir.clone(),
        dest_data_dir: data.to_path_buf(),
        dest_os: SourceOs::current(),
    }
}

fn setting(conn: &Connection, key: &str) -> Option<String> {
    conn.query_row("SELECT value FROM settings WHERE key = ?1", [key], |r| r.get(0)).ok()
}

fn read_opt(path: &Path) -> Option<String> {
    std::fs::read_to_string(path).ok().filter(|s| !s.trim().is_empty())
}

// --- preview ---------------------------------------------------------------------------------

/// Read what a file holds without applying anything: the manifest, the path roots found in it and
/// whether `passphrase` opens its secrets. A wrong passphrase is an error
/// ([`ProfileError::WrongPassphrase`]); a missing one is `secrets_ok: false`.
pub fn preview(src: &Path, passphrase: Option<&str>, data_dir: &Path) -> Result<Preview> {
    let manifest = archive::read_manifest(src)?;
    // The roots live in the database, so the archive is extracted to a scratch directory that is
    // removed when this returns.
    let scratch = Scratch::create(data_dir, PREVIEW_PREFIX)?;
    let into = scratch.path().join("x");
    std::fs::create_dir_all(&into)?;
    let ex = archive::extract(src, &into)?;
    let secrets_ok = if manifest.includes_secrets {
        match passphrase.filter(|p| !p.is_empty()) {
            None => false,
            Some(p) => {
                let bytes = ex
                    .secrets
                    .as_deref()
                    .ok_or_else(|| ProfileError::BadFormat("secrets.bin is missing".into()))?;
                drop(crypto::unseal(bytes, p)?);
                true
            }
        }
    } else {
        true
    };
    let conn = Connection::open(&ex.db_file)?;
    let roots = remap::detect_roots(&conn, &ex.webstorage, &remap_ctx(&manifest, data_dir))?;
    drop(conn);
    Ok(Preview { manifest, roots, secrets_ok })
}

// --- phase 1 ---------------------------------------------------------------------------------

/// Phase 1: stage everything and leave the marker. The caller restarts the app afterwards.
///
/// Side effect before the restart: the Data API refresh tokens are already in the token store
/// (they cannot be staged). Cancelling afterwards leaves them there, which is harmless: they are
/// the same accounts the file named.
pub fn prepare(
    src: &Path,
    passphrase: Option<&str>,
    decisions: &[RootDecision],
    ctx: &ImportCtx,
) -> Result<()> {
    let data = ctx.data_dir.as_path();
    let now = mu::now_secs();
    match load_pending(data) {
        Some(p) if !pending_expired(&p, now) => return Err(ProfileError::AlreadyPending),
        // An expired marker is replaced: its staging goes first.
        Some(p) => discard_staging(data, &p),
        None => {}
    }
    let manifest = archive::read_manifest(src)?;
    let pass = if manifest.includes_secrets {
        Some(passphrase.filter(|p| !p.is_empty()).ok_or(ProfileError::PassphraseRequired)?)
    } else {
        None
    };

    let stage = Scratch::create(data, STAGE_PREFIX)?;
    let name = stage
        .path()
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .ok_or_else(|| ProfileError::Other("bad staging directory".into()))?;
    let report = prepare_into(src, pass, decisions, ctx, &manifest, stage.path())?;
    let pending = Pending { requested_at: now, staging: name, last_status: None, report };
    write_pending(data, &pending)?;
    // From here the staging directory belongs to the marker: phase 2 or `cancel` removes it.
    stage.keep();
    Ok(())
}

/// Extract into `stage`, apply secrets and remap, save the tokens. Returns the report parts.
fn prepare_into(
    src: &Path,
    pass: Option<&str>,
    decisions: &[RootDecision],
    ctx: &ImportCtx,
    manifest: &super::Manifest,
    stage: &Path,
) -> Result<ImportReport> {
    let data = ctx.data_dir.as_path();
    let x = stage.join("x");
    std::fs::create_dir_all(&x)?;
    let ex = archive::extract(src, &x)?;

    // Unseal first: a wrong passphrase must not leave anything half-done.
    let bundle: Option<SecretsBundle> = match pass {
        Some(p) => {
            let bytes = ex
                .secrets
                .as_deref()
                .ok_or_else(|| ProfileError::BadFormat("secrets.bin is missing".into()))?;
            Some(crypto::unseal(bytes, p)?)
        }
        None => None,
    };

    // Move the pieces into place inside the staging directory (same volume: renames).
    let staged_db = stage.join(DB_FILE);
    std::fs::rename(&ex.db_file, &staged_db)?;
    let covers = ex.files_dir.join("covers");
    if covers.is_dir() {
        std::fs::rename(&covers, stage.join("covers"))?;
    }
    let icon = ex.files_dir.join("app-icon.png");
    if icon.is_file() {
        std::fs::rename(&icon, stage.join("app-icon.png"))?;
    }

    let mut web: WebStorage = ex.webstorage.clone();
    let mut warnings: Vec<String> = Vec::new();
    let remap_report;
    let monitor_time;
    let autostart_was_on;
    {
        let conn = Connection::open(&staged_db)?;
        // One self-contained file: no `-wal` to strand when phase 2 moves it.
        conn.query_row("PRAGMA journal_mode=DELETE", [], |r| r.get::<_, String>(0))?;
        match &bundle {
            Some(b) => scrub::restore_secrets(&conn, b)?,
            None => scrub::drop_signed_out_accounts(&conn)?,
        }
        let r = remap::apply(&conn, &mut web, &remap_ctx(manifest, data), decisions)?;
        remap_report = RemapCounts { rewritten: r.rewritten, kept: r.kept, dropped: r.dropped };
        monitor_time = setting(&conn, "monitor.schedule_time").filter(|t| !t.trim().is_empty());
        autostart_was_on = setting(&conn, "autostart").as_deref() == Some("true");
    }

    let ws_json = serde_json::to_vec(&web).map_err(|e| ProfileError::Other(e.to_string()))?;
    std::fs::write(stage.join(STAGED_WEBSTORAGE), ws_json)?;

    // client_secret.json and the Data API tokens.
    let mut tokens = TokenSummary::default();
    if let Some(b) = &bundle {
        let dest_cs = read_opt(&data.join(CLIENT_SECRET_FILE));
        if let Some(cs) = &b.client_secret_json {
            let differs = dest_cs
                .as_deref()
                .is_some_and(|d| credentials::client_id_of(d) != credentials::client_id_of(cs));
            if differs {
                // The destination's OAuth client stays; the tokens below then fail with
                // `client_mismatch`, because they only work with the client that issued them.
                warnings.push("client_secret_kept".into());
            } else {
                write_private(&stage.join(CLIENT_SECRET_FILE), cs.as_bytes())?;
            }
        }
        let rep = credentials::apply_tokens(
            ctx.token_store.as_ref(),
            &b.ytdata_refresh_tokens,
            b.client_secret_json.as_deref(),
            dest_cs.as_deref(),
        );
        if rep.failed.iter().any(|(_, code)| code == "keyring_unavailable") {
            warnings.push("keyring_unavailable".into());
        }
        tokens = TokenSummary { saved: rep.saved, failed: rep.failed };
    }
    let secrets_restored = bundle.is_some();
    drop(bundle); // wipes the strings

    // What was extracted and not moved (secrets.bin, the rest of files/) has no further use.
    // `x` is a literal child of the staging directory this call created.
    let _ = std::fs::remove_dir_all(&x);

    Ok(ImportReport {
        status: "pending".into(),
        imported: manifest.counts.clone(),
        remap: remap_report,
        tokens,
        secrets_restored,
        monitor_time,
        autostart_was_on,
        warnings,
        ..Default::default()
    })
}

/// Write a file only its owner reads (0600 on unix; on Windows the data directory's ACL stands).
fn write_private(path: &Path, bytes: &[u8]) -> io::Result<()> {
    std::fs::write(path, bytes)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

// --- the marker and the result -----------------------------------------------------------------

fn write_pending(data: &Path, p: &Pending) -> io::Result<()> {
    std::fs::create_dir_all(data)?;
    let json = serde_json::to_vec(p).map_err(io::Error::other)?;
    std::fs::write(data.join(PENDING_FILE), json)
}

/// The marker, if there is a file: one that does not parse reads as a default marker
/// (`requested_at` 0), which is expired, so it is never acted on but can still be cancelled.
fn load_pending(data: &Path) -> Option<Pending> {
    let path = data.join(PENDING_FILE);
    if !std::fs::symlink_metadata(&path).is_ok_and(|m| m.is_file()) {
        return None;
    }
    let parsed = std::fs::read(&path).ok().and_then(|b| serde_json::from_slice(&b).ok());
    Some(parsed.unwrap_or_default())
}

/// The staging directory the marker names, if the name is one this module could have made.
fn staging_path(data: &Path, p: &Pending) -> Option<PathBuf> {
    let ok = p.staging.starts_with(STAGE_PREFIX)
        && p.staging.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-');
    ok.then(|| data.join("tmp").join(&p.staging))
}

fn discard_staging(data: &Path, p: &Pending) {
    if let Some(dir) = staging_path(data, p) {
        let _ = remove_scratch(data, &dir, STAGE_PREFIX);
    }
}

fn write_result(data: &Path, report: &ImportReport) {
    if let Ok(json) = serde_json::to_vec_pretty(report) {
        let _ = std::fs::write(data.join(RESULT_FILE), json);
    }
}

fn peek_result(data: &Path) -> Option<ImportReport> {
    let bytes = std::fs::read(data.join(RESULT_FILE)).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// The last result, read once: the UI shows it and it is gone.
pub fn take_result(data: &Path) -> Option<ImportReport> {
    let report = peek_result(data);
    let _ = std::fs::remove_file(data.join(RESULT_FILE));
    report
}

/// The remapped localStorage phase 2 left, read once and deleted (it is also removed when it does
/// not parse).
pub fn take_webstorage(data: &Path) -> Option<WebStorage> {
    let path = data.join(WEBSTORAGE_FILE);
    let parsed = std::fs::read(&path).ok().and_then(|b| serde_json::from_slice(&b).ok());
    let _ = std::fs::remove_file(&path);
    parsed
}

/// The marker still waiting, if any.
pub fn pending(data: &Path, now: u64) -> Option<PendingInfo> {
    let p = load_pending(data)?;
    Some(PendingInfo {
        expired: pending_expired(&p, now),
        requested_at: p.requested_at,
        last_status: p.last_status,
    })
}

/// Drop the pending import: removes `<data>/profile-import.pending`, its staging directory and
/// its `retry`/`expired` result. `Ok(false)` when there was no marker.
pub fn cancel(data: &Path) -> io::Result<bool> {
    if !data.is_absolute() {
        return Err(io::Error::other("the data directory is not absolute"));
    }
    let Some(p) = load_pending(data) else { return Ok(false) };
    discard_staging(data, &p);
    std::fs::remove_file(data.join(PENDING_FILE))?;
    if peek_result(data).is_some_and(|r| r.status == "retry" || r.status == "expired") {
        let _ = std::fs::remove_file(data.join(RESULT_FILE));
    }
    Ok(true)
}

// --- phase 2 -----------------------------------------------------------------------------------

/// Phase 2, from `run()` right after `migrate_upstream::apply_pending()`: nothing has the
/// database open yet. `None` without a marker (or when a second Forge instance is running).
pub fn apply_pending() -> Option<ImportReport> {
    let data = crate::paths::data_dir_early()?;
    let p = load_pending(&data)?;
    // A restart overlaps the old process's last moments, and it still holds the database. Wait for
    // it to let go of the single-instance lock; a Forge still running after that is a second
    // launch, which leaves the marker for the real restart. An expired marker is never acted on,
    // so nothing is worth waiting for.
    if !pending_expired(&p, mu::now_secs()) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while mu::forge_running() {
            if std::time::Instant::now() > deadline {
                return None;
            }
            std::thread::sleep(std::time::Duration::from_millis(200));
        }
    }
    apply_pending_at(&data, mu::now_secs())
}

/// Phase 2 for `data`, at `now` (unix seconds). `None` when there is no marker. A marker past
/// [`PENDING_TTL`] is not acted on: it stays, marked `expired`, for the UI to cancel or replace.
pub fn apply_pending_at(data: &Path, now: u64) -> Option<ImportReport> {
    apply_pending_with(data, now, FsOps::default())
}

/// One thing phase 2 moves: its name in the data directory (and in the staging directory).
struct Run {
    ops: FsOps,
    /// `<data>/pre-import-<ts>`, created when the first thing goes aside.
    pre: Option<PathBuf>,
    /// (original, where it went), to put back on failure.
    asides: Vec<(PathBuf, PathBuf)>,
    /// (where it is now, the staging path it came from), to move back on failure.
    placed: Vec<(PathBuf, PathBuf)>,
}

struct SwapErr {
    path: PathBuf,
    source: io::Error,
    /// A transient lock (a file the previous instance still holds) rather than a real failure.
    locked: bool,
}

impl SwapErr {
    fn message(&self) -> String {
        format!("{}: {}", self.path.display(), self.source)
    }
}

impl Run {
    fn mv(&self, from: &Path, to: &Path) -> std::result::Result<(), SwapErr> {
        self.ops.rename_retrying(from, to).map_err(|source| SwapErr {
            path: from.to_path_buf(),
            locked: (self.ops.is_transient)(&source),
            source,
        })
    }

    fn pre_dir(&mut self, data: &Path, now: u64) -> std::result::Result<PathBuf, SwapErr> {
        if let Some(p) = &self.pre {
            return Ok(p.clone());
        }
        let mut n = 0;
        let pre = loop {
            let name =
                if n == 0 { format!("pre-import-{now}") } else { format!("pre-import-{now}-{n}") };
            let p = data.join(name);
            if std::fs::symlink_metadata(&p).is_err() {
                break p;
            }
            n += 1;
        };
        std::fs::create_dir_all(&pre).map_err(|source| SwapErr {
            path: pre.clone(),
            source,
            locked: false,
        })?;
        self.pre = Some(pre.clone());
        Ok(pre)
    }

    /// Move `<data>/<name>` aside, if it exists.
    fn aside(&mut self, data: &Path, name: &str, now: u64) -> std::result::Result<(), SwapErr> {
        let from = data.join(name);
        if std::fs::symlink_metadata(&from).is_err() {
            return Ok(());
        }
        let to = self.pre_dir(data, now)?.join(name);
        self.mv(&from, &to)?;
        self.asides.push((from, to));
        Ok(())
    }

    /// Move `<stage>/<name>` into the data directory, if the import brought it.
    fn place(&mut self, data: &Path, stage: &Path, name: &str) -> std::result::Result<(), SwapErr> {
        let from = stage.join(name);
        if std::fs::symlink_metadata(&from).is_err() {
            return Ok(());
        }
        let to = data.join(name);
        self.mv(&from, &to)?;
        self.placed.push((to, from));
        Ok(())
    }

    /// Best effort: what was placed goes back to staging, what was set aside goes back to its
    /// place. `true` when everything is back where it was.
    fn rollback(&mut self) -> bool {
        let mut all_back = true;
        for (now_at, staged) in self.placed.iter().rev() {
            all_back &= self.mv(now_at, staged).is_ok();
        }
        for (orig, moved) in self.asides.iter().rev() {
            all_back &= self.mv(moved, orig).is_ok();
        }
        if all_back {
            if let Some(pre) = &self.pre {
                // Non-recursive: only an empty directory goes, never set-aside content.
                let _ = std::fs::remove_dir(pre);
            }
        }
        all_back
    }
}

/// Move the current profile aside and the staged one in. Items: the database files, `covers/`,
/// `app-icon.png`, and `client_secret.json` only when the import brings one.
fn swap(run: &mut Run, data: &Path, stage: &Path, now: u64) -> std::result::Result<(), SwapErr> {
    let mut names: Vec<String> = DB_SUFFIXES.iter().map(|s| format!("{DB_FILE}{s}")).collect();
    names.push("covers".into());
    names.push("app-icon.png".into());
    if std::fs::symlink_metadata(stage.join(CLIENT_SECRET_FILE)).is_ok() {
        names.push(CLIENT_SECRET_FILE.into());
    }
    for name in &names {
        run.aside(data, name, now)?;
    }
    for name in &names {
        run.place(data, stage, name)?;
    }
    Ok(())
}

fn apply_pending_with(data: &Path, now: u64, ops: FsOps) -> Option<ImportReport> {
    let mut pending = load_pending(data)?;
    if pending_expired(&pending, now) {
        pending.last_status = Some("expired".into());
        let _ = write_pending(data, &pending);
        let report = ImportReport { status: "expired".into(), ..pending.report.clone() };
        write_result(data, &report);
        return Some(report);
    }
    let mut report = pending.report.clone();
    let stage = staging_path(data, &pending).filter(|s| s.join(DB_FILE).is_file());
    let Some(stage) = stage else {
        let _ = std::fs::remove_file(data.join(PENDING_FILE));
        discard_staging(data, &pending);
        report.status = "error".into();
        report.error = Some("the staged profile is missing".into());
        write_result(data, &report);
        return Some(report);
    };

    let mut run = Run { ops, pre: None, asides: Vec::new(), placed: Vec::new() };
    match swap(&mut run, data, &stage, now) {
        Ok(()) => {
            // For the UI to apply on first boot. Not part of the rollback: the swap is done.
            let ws = stage.join(STAGED_WEBSTORAGE);
            if ws.is_file() {
                let _ = std::fs::rename(&ws, data.join(WEBSTORAGE_FILE));
            }
            let _ = remove_scratch(data, &stage, STAGE_PREFIX);
            let _ = std::fs::remove_file(data.join(PENDING_FILE));
            report.status = "done".into();
            report.aside = run.pre.as_ref().map(|p| p.to_string_lossy().into_owned());
        }
        Err(e) => {
            let restored = run.rollback();
            report.error = Some(e.message());
            if e.locked && restored {
                // Kept for the next launch within the window; staging stays for it.
                report.status = "retry".into();
                pending.last_status = Some("retry".into());
                let _ = write_pending(data, &pending);
            } else {
                report.status = "error".into();
                let _ = std::fs::remove_file(data.join(PENDING_FILE));
                // Only when everything is back: otherwise the staged files may be the only copy
                // of something that did not return, and stay for a manual look.
                if restored {
                    let _ = remove_scratch(data, &stage, STAGE_PREFIX);
                }
            }
        }
    }
    write_result(data, &report);
    Some(report)
}

#[cfg(test)]
mod tests {
    use super::super::export::testutil::*;
    use super::super::export::{export, ExportCtx};
    use super::*;
    use std::ffi::OsStr;
    use ytdata::auth::store::InMemoryTokenStore;

    const OLD_MARKER: &str = "old-destination-setting";

    /// A destination install: its own database, one cover, no client secret, an empty token store.
    struct Dest {
        _tmp: tempfile::TempDir,
        data: PathBuf,
        store: Arc<InMemoryTokenStore>,
    }

    fn dest() -> Dest {
        let tmp = tempfile::tempdir().unwrap();
        let data = tmp.path().join("data");
        std::fs::create_dir_all(data.join("covers")).unwrap();
        {
            let db = crate::db::Db::open(&data.join(DB_FILE)).unwrap();
            db.set_setting("limusic_marker", OLD_MARKER);
        }
        std::fs::write(data.join("covers").join("old.jpg"), b"old-cover").unwrap();
        Dest { _tmp: tmp, data, store: Arc::new(InMemoryTokenStore::default()) }
    }

    fn ictx(d: &Dest) -> ImportCtx {
        ImportCtx { data_dir: d.data.clone(), token_store: d.store.clone() }
    }

    /// Export the source fixture to a file and return its path.
    fn exported(fx: &Fixture, secrets: bool) -> PathBuf {
        let file = fx._tmp.path().join(format!("p{secrets}.lmfprofile"));
        let ectx = ExportCtx {
            data_dir: fx.data.clone(),
            app_version: "9.9.9-test".into(),
            token_store: fx.store.clone(),
        };
        export(&ectx, request(file.clone(), secrets, false)).unwrap();
        file
    }

    fn db_value(data: &Path, key: &str) -> Option<String> {
        let conn = Connection::open(data.join(DB_FILE)).unwrap();
        setting(&conn, key)
    }

    #[test]
    fn profile_import_round_trip_with_secrets() {
        let fx = fixture();
        let file = exported(&fx, true);
        let d = dest();

        let pv = preview(&file, Some(PASSPHRASE), &d.data).unwrap();
        assert!(pv.secrets_ok && pv.manifest.includes_secrets);
        let pv = preview(&file, None, &d.data).unwrap();
        assert!(!pv.secrets_ok);
        assert!(!d.data.join("tmp").exists(), "preview left its scratch directory");

        prepare(&file, Some(PASSPHRASE), &[], &ictx(&d)).unwrap();
        // Phase 1 changed nothing live, but the tokens are in the destination's store already.
        assert_eq!(db_value(&d.data, "limusic_marker").as_deref(), Some(OLD_MARKER));
        assert_eq!(d.store.load(CHANNEL).unwrap().as_deref(), Some(REFRESH));
        // No secret in the marker.
        let marker = std::fs::read_to_string(d.data.join(PENDING_FILE)).unwrap();
        for m in SECRET_MARKERS.iter().chain(["CLIENT-SECRET-MARKER"].iter()) {
            assert!(!marker.contains(m), "marker holds {m}");
        }
        assert!(pending(&d.data, mu::now_secs()).is_some_and(|p| !p.expired));

        let report = apply_pending_at(&d.data, mu::now_secs()).expect("report");
        assert_eq!(report.status, "done", "{:?}", report.error);
        assert!(report.secrets_restored && report.tokens.saved == vec![CHANNEL.to_string()]);
        assert!(!d.data.join(PENDING_FILE).exists());
        // The imported profile is live, secrets restored into it.
        assert_eq!(db_value(&d.data, "limusic_marker").as_deref(), Some("kept-setting"));
        assert_eq!(db_value(&d.data, "session_cookie").as_deref(), Some(ACTIVE_COOKIE));
        assert_eq!(db_value(&d.data, "lastfm_session_key").as_deref(), Some(LASTFM));
        let conn = Connection::open(d.data.join(DB_FILE)).unwrap();
        let acc: String = conn
            .query_row("SELECT session_cookie FROM accounts WHERE id='acc1'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(acc, ACCOUNT_COOKIE);
        drop(conn);
        assert!(d.data.join("covers").join("playlists").join("1.jpg").is_file());
        assert!(!d.data.join("covers").join("old.jpg").exists());
        assert!(d.data.join("app-icon.png").is_file());
        assert_eq!(
            std::fs::read_to_string(d.data.join(CLIENT_SECRET_FILE)).unwrap(),
            CLIENT_SECRET
        );
        // The previous profile was moved aside, not deleted.
        let aside = PathBuf::from(report.aside.clone().expect("aside"));
        assert_eq!(std::fs::read(aside.join("covers").join("old.jpg")).unwrap(), b"old-cover");
        assert!(aside.join(DB_FILE).is_file());
        // Staging is gone, the localStorage and the result wait for the UI, once.
        assert!(!d.data.join("tmp").exists());
        let web = take_webstorage(&d.data).expect("webstorage");
        assert!(web.contains_key("appearance") && !web.contains_key("not-allowed-key"));
        assert!(take_webstorage(&d.data).is_none());
        assert_eq!(take_result(&d.data).map(|r| r.status), Some("done".into()));
        assert!(take_result(&d.data).is_none());
    }

    #[test]
    fn profile_import_without_secrets_signs_out_and_saves_no_tokens() {
        let fx = fixture();
        let file = exported(&fx, false);
        let d = dest();
        prepare(&file, None, &[], &ictx(&d)).unwrap();
        let report = apply_pending_at(&d.data, mu::now_secs()).unwrap();
        assert_eq!(report.status, "done", "{:?}", report.error);
        assert!(!report.secrets_restored && report.tokens.saved.is_empty());
        assert_eq!(db_value(&d.data, "limusic_marker").as_deref(), Some("kept-setting"));
        assert!(db_value(&d.data, "session_cookie").unwrap_or_default().is_empty());
        assert_eq!(db_value(&d.data, "lastfm_session_key").unwrap_or_default(), "");
        assert!(d.store.load(CHANNEL).unwrap().is_none());
        assert_eq!(file_has_marker(&d.data.join(DB_FILE)), None);
        assert!(!d.data.join(CLIENT_SECRET_FILE).exists());
    }

    #[test]
    fn profile_import_needs_the_right_passphrase_and_leaves_nothing_behind() {
        let fx = fixture();
        let file = exported(&fx, true);
        let d = dest();
        let err = prepare(&file, None, &[], &ictx(&d)).unwrap_err();
        assert_eq!(err.code(), "passphrase_required");
        let err = prepare(&file, Some("a wrong passphrase"), &[], &ictx(&d)).unwrap_err();
        assert_eq!(err.code(), "wrong_passphrase");
        let err = preview(&file, Some("a wrong passphrase"), &d.data).unwrap_err();
        assert_eq!(err.code(), "wrong_passphrase");
        assert!(!d.data.join(PENDING_FILE).exists());
        assert!(!d.data.join("tmp").exists(), "a failed prepare left its staging directory");
        assert!(d.store.load(CHANNEL).unwrap().is_none());
        assert_eq!(db_value(&d.data, "limusic_marker").as_deref(), Some(OLD_MARKER));
    }

    #[test]
    fn profile_import_second_prepare_is_refused_while_one_is_pending() {
        let fx = fixture();
        let file = exported(&fx, false);
        let d = dest();
        prepare(&file, None, &[], &ictx(&d)).unwrap();
        let err = prepare(&file, None, &[], &ictx(&d)).unwrap_err();
        assert_eq!(err.code(), "already_pending");
        assert!(cancel(&d.data).unwrap());
        assert!(!cancel(&d.data).unwrap());
        assert!(!d.data.join("tmp").exists(), "cancel left the staging directory");
    }

    #[test]
    fn profile_import_expired_marker_is_not_applied() {
        let fx = fixture();
        let file = exported(&fx, false);
        let d = dest();
        prepare(&file, None, &[], &ictx(&d)).unwrap();
        let late = mu::now_secs() + PENDING_TTL + 5;
        let report = apply_pending_at(&d.data, late).unwrap();
        assert_eq!(report.status, "expired");
        // Nothing was touched; the marker stays for the UI.
        assert_eq!(db_value(&d.data, "limusic_marker").as_deref(), Some(OLD_MARKER));
        assert!(d.data.join(PENDING_FILE).is_file());
        let info = pending(&d.data, late).unwrap();
        assert!(info.expired && info.last_status.as_deref() == Some("expired"));
        assert_eq!(take_result(&d.data).map(|r| r.status), Some("expired".into()));
        // Importing again replaces the expired marker and its staging.
        let mut p = load_pending(&d.data).unwrap();
        p.requested_at = 1;
        write_pending(&d.data, &p).unwrap();
        prepare(&file, None, &[], &ictx(&d)).unwrap();
        let staged = std::fs::read_dir(d.data.join("tmp")).unwrap().flatten().count();
        assert_eq!(staged, 1, "the expired staging was not replaced");
    }

    /// A marker with a zero or far-future `requested_at` is expired too.
    #[test]
    fn profile_import_expiry_rules() {
        let p = |t| Pending { requested_at: t, ..Default::default() };
        let now = 10_000;
        assert!(pending_expired(&p(0), now));
        assert!(pending_expired(&p(now + 3600), now));
        assert!(pending_expired(&p(now - PENDING_TTL - 1), now));
        assert!(!pending_expired(&p(now - PENDING_TTL), now));
        assert!(!pending_expired(&p(now + 30), now));
    }

    #[test]
    fn profile_import_phase_2_failure_rolls_everything_back() {
        let fx = fixture();
        let file = exported(&fx, true);
        let d = dest();
        std::fs::write(d.data.join("app-icon.png"), b"old-icon").unwrap();
        prepare(&file, Some(PASSPHRASE), &[], &ictx(&d)).unwrap();

        // The move of the staged `covers` into the data directory fails, after the database
        // was already swapped.
        let data = d.data.clone();
        let ops = FsOps {
            rename: Box::new(move |from, to| {
                if from.starts_with(data.join("tmp")) && to == data.join("covers") {
                    Err(io::Error::other("injected failure"))
                } else {
                    std::fs::rename(from, to)
                }
            }),
            is_transient: Box::new(|_| false),
            backoff: Vec::new(),
            sleep: Box::new(|_| {}),
        };
        let report = apply_pending_with(&d.data, mu::now_secs(), ops).unwrap();
        assert_eq!(report.status, "error");
        assert!(report.error.as_deref().is_some_and(|e| e.contains("injected failure")));
        // The old profile is exactly as it was.
        assert_eq!(db_value(&d.data, "limusic_marker").as_deref(), Some(OLD_MARKER));
        assert_eq!(std::fs::read(d.data.join("covers").join("old.jpg")).unwrap(), b"old-cover");
        assert_eq!(std::fs::read(d.data.join("app-icon.png")).unwrap(), b"old-icon");
        assert!(!d.data.join(CLIENT_SECRET_FILE).exists());
        // No aside directory left over, no marker, staging cleaned, the result says why.
        let leftovers: Vec<_> = std::fs::read_dir(&d.data)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.starts_with("pre-import-") || n == PENDING_FILE)
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
        assert!(!d.data.join("tmp").exists());
        assert_eq!(take_result(&d.data).map(|r| r.status), Some("error".into()));
        assert!(take_webstorage(&d.data).is_none());
    }

    #[test]
    fn profile_import_held_file_keeps_the_marker_for_a_retry() {
        let fx = fixture();
        let file = exported(&fx, false);
        let d = dest();
        prepare(&file, None, &[], &ictx(&d)).unwrap();
        let data = d.data.clone();
        let ops = FsOps {
            rename: Box::new(move |from, to| {
                if from == data.join("covers") && to.file_name() == Some(OsStr::new("covers")) {
                    Err(io::Error::other("held"))
                } else {
                    std::fs::rename(from, to)
                }
            }),
            is_transient: Box::new(|e| e.to_string() == "held"),
            backoff: vec![std::time::Duration::ZERO; 2],
            sleep: Box::new(|_| {}),
        };
        let report = apply_pending_with(&d.data, mu::now_secs(), ops).unwrap();
        assert_eq!(report.status, "retry");
        assert_eq!(db_value(&d.data, "limusic_marker").as_deref(), Some(OLD_MARKER));
        assert!(d.data.join(PENDING_FILE).is_file());
        assert_eq!(pending(&d.data, mu::now_secs()).unwrap().last_status.as_deref(), Some("retry"));
        // The next launch within the window finishes the job.
        let report = apply_pending_at(&d.data, mu::now_secs()).unwrap();
        assert_eq!(report.status, "done", "{:?}", report.error);
        assert_eq!(db_value(&d.data, "limusic_marker").as_deref(), Some("kept-setting"));
    }

    #[test]
    fn profile_import_staging_names_are_validated() {
        let data = Path::new("/data");
        let p = |s: &str| Pending { staging: s.into(), ..Default::default() };
        assert!(staging_path(data, &p("profile-import-123")).is_some());
        assert!(staging_path(data, &p("profile-import-123-1")).is_some());
        for bad in [
            "",
            "..",
            "profile-import-../x",
            "profile-import-1/2",
            "other-1",
            "profile-import-1\\2",
        ] {
            assert!(staging_path(data, &p(bad)).is_none(), "{bad}");
        }
    }
}
