//! Copy an upstream LiMusic install's data into LiMusic Forge (D3, D7).
//!
//! The fork has its own identifier (`com.limusicforge.desktop`), so its data directory and webview
//! profile start empty. This brings the upstream ones over: the SQLite file (accounts, settings,
//! the local library, theme and language), the caches worth keeping, the custom icon, the window
//! geometry and, optionally, the webview profile that holds the Google session.
//!
//! **Two phases.** The data cannot be swapped under a running app: the database is open and the
//! webview profile is locked by WebView2. So Settings ▸ Import & migrate only writes a marker
//! (`migrate-upstream.pending`) and restarts ([`write_pending`]); the next launch runs
//! [`apply_pending`] from `run()`, before the Tauri builder exists and before anything opens the
//! database, and leaves `migrate-upstream.result.json` for the UI to read once ([`take_result`]).
//! The marker is only honoured for [`PENDING_TTL`] (10 minutes) after it was written; an older one
//! is left alone, marked `expired`, for the UI to retry ([`write_pending`] anew) or cancel
//! ([`cancel_pending`]).
//!
//! **Invariants.**
//! - Upstream is only ever read. Nothing here creates, writes, renames or deletes anything under
//!   its directories, and its database is never opened: SQLite would create a `-shm` beside it.
//!   The three database files are copied first, and `VACUUM INTO` runs from that copy.
//! - Every source is canonicalised and has to stay under the upstream root it came from; a
//!   symlink or junction is skipped, never followed.
//! - A destination equal to or nested in an upstream directory (or the reverse) is refused.
//! - What Forge already had is moved aside to `<data>/pre-migrate-<ts>/`, never deleted. A failure
//!   halfway puts it back.
//! - Every path is absolute and built from `dirs`; a directory `dirs` cannot resolve is an error,
//!   not an empty string that would turn into a relative path.

use std::ffi::OsStr;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

/// Upstream's Tauri identifier: the name of its data, local-data and config directories.
pub const UPSTREAM_ID: &str = "com.limusic.desktop";
/// The marker phase 1 leaves in Forge's data directory.
pub const PENDING_FILE: &str = "migrate-upstream.pending";
/// What phase 2 found, for the UI to read once.
pub const RESULT_FILE: &str = "migrate-upstream.result.json";
/// The setting the first-run prompt keeps its answer in (D8).
pub const PROMPTED_KEY: &str = "onboarding_import_prompted";
/// The id the first-run prompt and the setting use for this source.
pub const SOURCE_ID: &str = "limusic";

const DB_FILE: &str = "limusic.sqlite";
const DB_SUFFIXES: [&str; 3] = ["", "-wal", "-shm"];

/// Where upstream keeps its data on this machine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Upstream {
    /// `%APPDATA%\com.limusic.desktop` (Linux: `~/.local/share/com.limusic.desktop`): the
    /// database, caches and icon. On Linux the WebKit profile lives here too.
    pub roaming: PathBuf,
    /// `%LOCALAPPDATA%\com.limusic.desktop`, holding `EBWebView`. `None` on Linux.
    pub local: Option<PathBuf>,
    /// Where the window-state plugin wrote `.window-state.json`: the same as `roaming` on
    /// Windows, `~/.config/com.limusic.desktop` on Linux.
    pub config: PathBuf,
}

/// Which upstream layout to copy from. A parameter rather than a `cfg` so both plans are testable
/// on either OS.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Layout {
    /// WebView2: the profile is `EBWebView` under the local app data directory.
    Windows,
    /// WebKitGTK: the profile's directories sit beside the database.
    Linux,
}

impl Layout {
    pub fn current() -> Option<Layout> {
        if cfg!(windows) {
            Some(Layout::Windows)
        } else if cfg!(target_os = "linux") {
            Some(Layout::Linux)
        } else {
            None
        }
    }
}

/// Where the copy goes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Dest {
    /// Forge's data directory (`paths::data_dir`).
    pub data: PathBuf,
    /// The directory the webview profile goes in: `%LOCALAPPDATA%\com.limusicforge.desktop`
    /// (gets `EBWebView`), `data\webview` in portable mode, the data directory on Linux.
    pub webview: PathBuf,
    /// The window-state plugin's file, or `None` in portable mode (winstate.rs keeps its own).
    pub window_state: Option<PathBuf>,
    pub layout: Layout,
    /// Copy the webview profile too (the Google session, localStorage).
    pub include_webview: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CopyKind {
    /// The three SQLite files, copied to a work directory and `VACUUM INTO` the destination.
    Database,
    /// A directory, copied whole.
    Dir,
    /// One file.
    File,
    /// A webview profile directory, without its caches, crash dumps and lock file.
    Webview,
    /// Linux: every entry of the upstream data directory that is not one of the other items, the
    /// log, or a cache. This is where WebKitGTK keeps localStorage and cookies.
    WebkitBeside,
}

/// One thing to copy. `root` is the upstream directory `src` must stay under.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CopyItem {
    pub kind: CopyKind,
    pub src: PathBuf,
    pub dest: PathBuf,
    pub root: PathBuf,
}

#[derive(Debug, thiserror::Error)]
pub enum MigrateError {
    #[error("could not resolve the {0} directory")]
    UnresolvedDir(&'static str),
    #[error("LiMusic is running")]
    UpstreamRunning,
    #[error("{} is in use", .0.display())]
    Locked(PathBuf),
    #[error("the destination is the same as, or inside, the LiMusic data")]
    SameOrNested,
    #[error("{}: {source}", path.display())]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("database: {0}")]
    Sqlite(String),
}

fn io_err(path: &Path, source: io::Error) -> MigrateError {
    // ERROR_SHARING_VIOLATION / ERROR_LOCK_VIOLATION: another process holds the file.
    if matches!(source.raw_os_error(), Some(32 | 33)) && cfg!(windows) {
        return MigrateError::Locked(path.to_path_buf());
    }
    MigrateError::Io { path: path.to_path_buf(), source }
}

/// How long phase 2 waits, in total, for a file of Forge's that something still holds.
const LOCK_WAIT: Duration = Duration::from_secs(15);

/// The waits between attempts: 250 ms, 500 ms, 750 ms, then 1 s each, adding up to at most
/// [`LOCK_WAIT`].
fn lock_backoff() -> Vec<Duration> {
    let mut out = Vec::new();
    let mut total = Duration::ZERO;
    let mut step = Duration::from_millis(250);
    while total + step <= LOCK_WAIT {
        out.push(step);
        total += step;
        step = (step + Duration::from_millis(250)).min(Duration::from_secs(1));
    }
    out
}

/// Windows errors a restart's leftovers cause and that clear once they exit: the previous
/// instance's `msedgewebview2.exe` still holding the profile gives ERROR_ACCESS_DENIED on a
/// rename; ERROR_SHARING_VIOLATION and ERROR_LOCK_VIOLATION are a file held open.
fn is_transient_lock(e: &io::Error) -> bool {
    cfg!(windows) && matches!(e.raw_os_error(), Some(5 | 32 | 33))
}

/// Run `op` until it succeeds, fails with an error `is_transient` rejects, or `sleeps` runs out
/// (then the last error). `sleep` is given each wait, so tests need not sleep.
fn retry_locked<T>(
    mut op: impl FnMut() -> io::Result<T>,
    is_transient: impl Fn(&io::Error) -> bool,
    sleeps: impl IntoIterator<Item = Duration>,
    sleep: impl Fn(Duration),
) -> io::Result<T> {
    let mut sleeps = sleeps.into_iter();
    loop {
        match op() {
            Ok(v) => return Ok(v),
            Err(e) if is_transient(&e) => match sleeps.next() {
                Some(d) => sleep(d),
                None => return Err(e),
            },
            Err(e) => return Err(e),
        }
    }
}

/// The renames phase 2 does on Forge's own files, and how it waits for a held one. Injectable so
/// a lock can be simulated without holding one.
struct FsOps {
    rename: Box<dyn Fn(&Path, &Path) -> io::Result<()>>,
    is_transient: Box<dyn Fn(&io::Error) -> bool>,
    backoff: Vec<Duration>,
    sleep: Box<dyn Fn(Duration)>,
}

impl Default for FsOps {
    fn default() -> Self {
        FsOps {
            rename: Box::new(|from: &Path, to: &Path| std::fs::rename(from, to)),
            is_transient: Box::new(is_transient_lock),
            backoff: lock_backoff(),
            sleep: Box::new(std::thread::sleep),
        }
    }
}

impl FsOps {
    fn rename_retrying(&self, from: &Path, to: &Path) -> io::Result<()> {
        retry_locked(
            || (self.rename)(from, to),
            |e| (self.is_transient)(e),
            self.backoff.iter().copied(),
            |d| (self.sleep)(d),
        )
    }
}

/// What a migration did, or why it did not. Written as `migrate-upstream.result.json`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Report {
    /// `done`, `retry` (upstream was open, or a file stayed held past the wait; the marker stays
    /// and the next launch within [`PENDING_TTL`] tries again), `expired` (the marker is past its
    /// window: nothing was done, the marker stays for the UI to retry or cancel) or `error`.
    pub status: String,
    pub error: Option<String>,
    pub files: u64,
    pub bytes: u64,
    /// Where Forge's previous data went, when there was any.
    pub aside: Option<String>,
    pub webview: bool,
    /// Whether upstream starts at login; filled in when the UI reads the result.
    pub upstream_autostart: Option<bool>,
    /// Forge's `onboarding_import_prompted` before the database was replaced.
    #[serde(default)]
    pub prompted: Option<String>,
}

/// The marker phase 1 writes.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Pending {
    #[serde(default)]
    pub include_webview: bool,
    /// Carried across the swap: the database that held it is about to be replaced.
    #[serde(default)]
    pub prompted: Option<String>,
    /// Unix seconds when the user asked. The marker is only honoured for [`PENDING_TTL`] after it.
    #[serde(default)]
    pub requested_at: u64,
    /// What phase 2 last made of it (`retry`, `expired`), for the UI to offer a retry or a cancel.
    #[serde(default)]
    pub last_status: Option<String>,
}

/// How long a marker stays valid. A migration asked for and not done within it (LiMusic kept
/// open, a file kept held) is no longer carried out on its own: it could otherwise replace, days
/// later, data the user has gone on using. The UI asks instead.
pub const PENDING_TTL: u64 = 10 * 60;
/// How far in the future `requested_at` may be (clock adjustments) before it counts as invalid.
const PENDING_FUTURE_SKEW: u64 = 60;

/// Whether a marker is past its window at `now` (unix seconds). A zero or far-future
/// `requested_at` (an unreadable marker, a clock that jumped) counts as expired.
pub fn pending_expired(p: &Pending, now: u64) -> bool {
    p.requested_at == 0
        || p.requested_at > now.saturating_add(PENDING_FUTURE_SKEW)
        || now.saturating_sub(p.requested_at) > PENDING_TTL
}

/// The pending marker as the UI sees it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PendingInfo {
    pub requested_at: u64,
    pub expired: bool,
    pub last_status: Option<String>,
    pub include_webview: bool,
}

/// The marker's state at `now`, or `None` when there is none.
pub fn pending_info(data: &Path, now: u64) -> Option<PendingInfo> {
    let p = load_pending(data)?;
    Some(PendingInfo {
        expired: pending_expired(&p, now),
        requested_at: p.requested_at,
        last_status: p.last_status,
        include_webview: p.include_webview,
    })
}

pub(crate) fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Upstream's directories on this OS, or `None` when it was never run here (or on macOS, which
/// is not offered). `Err` only when `dirs` cannot answer at all.
pub fn locate() -> Result<Option<Upstream>, MigrateError> {
    #[cfg(debug_assertions)]
    let ov = test_overrides(|k| std::env::var_os(k));
    #[cfg(not(debug_assertions))]
    let ov = Overrides::default();
    locate_at(Layout::current(), ov)
}

/// Upstream directories to use instead of the real ones. Only debug builds ever fill it in.
#[derive(Debug, Default)]
struct Overrides {
    roaming: Option<PathBuf>,
    local: Option<PathBuf>,
    config: Option<PathBuf>,
}

/// Debug builds only: point the migration at a copy of upstream's data, for end-to-end tests
/// that must not read the real folders' live state. The paths are still canonicalised, must be
/// absolute, and are only ever read, like the real ones.
#[cfg(debug_assertions)]
fn test_overrides(env: impl Fn(&str) -> Option<std::ffi::OsString>) -> Overrides {
    let get = |key: &str| {
        let v = env(key).filter(|v| !v.is_empty())?;
        let p = PathBuf::from(v);
        tracing::warn!(var = key, dir = %p.display(), "using a test override for the LiMusic data");
        Some(p.canonicalize().unwrap_or(p))
    };
    Overrides {
        roaming: get("LIMUSIC_FORGE_UPSTREAM_ROAMING"),
        local: get("LIMUSIC_FORGE_UPSTREAM_LOCAL"),
        config: get("LIMUSIC_FORGE_UPSTREAM_CONFIG"),
    }
}

fn locate_at(layout: Option<Layout>, ov: Overrides) -> Result<Option<Upstream>, MigrateError> {
    let Some(layout) = layout else { return Ok(None) };
    let roaming_overridden = ov.roaming.is_some();
    let roaming = match ov.roaming {
        Some(p) => p,
        None => dirs::data_dir().ok_or(MigrateError::UnresolvedDir("data"))?.join(UPSTREAM_ID),
    };
    let config = match (ov.config, layout) {
        (Some(p), _) => p,
        // On Windows the window-state file sits in the roaming directory: follow its override.
        (None, Layout::Windows) if roaming_overridden => roaming.clone(),
        (None, _) => {
            dirs::config_dir().ok_or(MigrateError::UnresolvedDir("config"))?.join(UPSTREAM_ID)
        }
    };
    let local = match layout {
        Layout::Windows => Some(match ov.local {
            Some(p) => p,
            None => dirs::data_local_dir()
                .ok_or(MigrateError::UnresolvedDir("local data"))?
                .join(UPSTREAM_ID),
        }),
        Layout::Linux => None,
    };
    for p in [&roaming, &config].into_iter().chain(local.as_ref()) {
        if !p.is_absolute() {
            return Err(MigrateError::UnresolvedDir("absolute"));
        }
    }
    if !roaming.is_dir() {
        return Ok(None);
    }
    Ok(Some(Upstream { roaming, local, config }))
}

/// What to copy where. Pure: nothing is checked on disk here, [`execute`] skips what is missing.
pub fn plan(src: &Upstream, dest: &Dest) -> Vec<CopyItem> {
    let r = &src.roaming;
    let d = &dest.data;
    let item = |kind, src: PathBuf, dest: PathBuf, root: &Path| CopyItem {
        kind,
        src,
        dest,
        root: root.to_path_buf(),
    };
    let mut items = vec![
        item(CopyKind::Database, r.join(DB_FILE), d.join(DB_FILE), r),
        item(CopyKind::Dir, r.join("cipher_cache"), d.join("cipher_cache"), r),
        item(CopyKind::Dir, r.join("covers"), d.join("covers"), r),
        item(CopyKind::File, r.join("app-icon.png"), d.join("app-icon.png"), r),
    ];
    if let Some(ws) = &dest.window_state {
        items.push(item(
            CopyKind::File,
            src.config.join(".window-state.json"),
            ws.clone(),
            &src.config,
        ));
    }
    if dest.include_webview {
        match (dest.layout, &src.local) {
            (Layout::Windows, Some(local)) => items.push(item(
                CopyKind::Webview,
                local.join("EBWebView"),
                dest.webview.join("EBWebView"),
                local,
            )),
            (Layout::Windows, None) => {}
            (Layout::Linux, _) => {
                items.push(item(CopyKind::WebkitBeside, r.clone(), dest.webview.clone(), r))
            }
        }
    }
    items
}

/// Names a webview copy leaves behind, at any depth: caches rebuild themselves, crash dumps are
/// worthless, and a `lockfile` copied in would claim a running browser.
fn webview_skip(name: &OsStr) -> bool {
    let n = name.to_string_lossy();
    n.to_ascii_lowercase().contains("cache") || n == "Crashpad" || n == "lockfile"
}

/// Top-level entries of the Linux data directory that are not WebKit's: the other plan items, the
/// log, our own bookkeeping.
fn beside_skip_top(name: &OsStr) -> bool {
    let n = name.to_string_lossy();
    n.starts_with(DB_FILE)
        || n.starts_with("limusic.log")
        || n.starts_with("pre-migrate-")
        || n.starts_with("migrate-upstream.")
        || matches!(n.as_ref(), "cipher_cache" | "covers" | "app-icon.png" | "tmp" | "audio-cache")
}

/// The longest existing ancestor, canonicalised, with the rest appended: a lenient canonicalise
/// for destinations that do not exist yet.
fn canon_lenient(p: &Path) -> PathBuf {
    let mut tail = Vec::new();
    let mut cur = p;
    loop {
        if let Ok(c) = cur.canonicalize() {
            let mut out = c;
            for part in tail.iter().rev() {
                out.push(part);
            }
            return out;
        }
        match (cur.file_name(), cur.parent()) {
            (Some(name), Some(parent)) => {
                tail.push(name.to_os_string());
                cur = parent;
            }
            _ => return p.to_path_buf(),
        }
    }
}

fn nested(a: &Path, b: &Path) -> bool {
    a.starts_with(b) || b.starts_with(a)
}

/// Refuse a plan whose destinations touch upstream, or with a relative path anywhere.
fn validate(plan: &[CopyItem], dest: &Dest) -> Result<(), MigrateError> {
    let mut roots: Vec<PathBuf> = Vec::new();
    for it in plan {
        if !it.src.is_absolute() || !it.dest.is_absolute() || !it.root.is_absolute() {
            return Err(MigrateError::UnresolvedDir("absolute"));
        }
        let root = canon_lenient(&it.root);
        if !roots.contains(&root) {
            roots.push(root);
        }
    }
    if !dest.data.is_absolute() || !dest.webview.is_absolute() {
        return Err(MigrateError::UnresolvedDir("absolute"));
    }
    let dests = plan
        .iter()
        .map(|it| it.dest.as_path())
        .chain([dest.data.as_path(), dest.webview.as_path()]);
    for d in dests {
        let d = canon_lenient(d);
        if roots.iter().any(|r| nested(&d, r)) {
            return Err(MigrateError::SameOrNested);
        }
    }
    Ok(())
}

/// `Some(canonical)` for a source that exists, is not a link and stays under its root; `None` for
/// one that is missing or a link (skipped).
fn checked_src(src: &Path, root: &Path) -> Result<Option<PathBuf>, MigrateError> {
    match std::fs::symlink_metadata(src) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(io_err(src, e)),
        Ok(m) if m.file_type().is_symlink() => return Ok(None),
        Ok(_) => {}
    }
    let canon = src.canonicalize().map_err(|e| io_err(src, e))?;
    let root = root.canonicalize().map_err(|e| io_err(root, e))?;
    if !canon.starts_with(&root) {
        return Err(MigrateError::Io {
            path: src.to_path_buf(),
            source: io::Error::other("outside the LiMusic directory"),
        });
    }
    Ok(Some(canon))
}

#[derive(Default)]
struct Run {
    files: u64,
    bytes: u64,
    /// (original, where it went), to put back on failure.
    asides: Vec<(PathBuf, PathBuf)>,
    /// Destinations this run created, to move out of the way on failure.
    created: Vec<PathBuf>,
    pre: Option<PathBuf>,
    ops: FsOps,
}

impl Run {
    fn pre_dir(&mut self, data: &Path, ts: u64) -> Result<PathBuf, MigrateError> {
        if let Some(p) = &self.pre {
            return Ok(p.clone());
        }
        let mut n = 0;
        let pre = loop {
            let name =
                if n == 0 { format!("pre-migrate-{ts}") } else { format!("pre-migrate-{ts}-{n}") };
            let p = data.join(name);
            if !p.exists() {
                break p;
            }
            n += 1;
        };
        std::fs::create_dir_all(&pre).map_err(|e| io_err(&pre, e))?;
        self.pre = Some(pre.clone());
        Ok(pre)
    }

    /// Move `path` (Forge's) into the aside directory under `rel`, if it exists. Rename only:
    /// across volumes (a portable stick, an odd LOCALAPPDATA) it goes beside itself instead.
    /// A path still held (the previous instance's webview processes exiting) is retried with
    /// backoff; held past the wait, it is [`MigrateError::Locked`].
    fn aside(&mut self, path: &Path, rel: &Path, data: &Path, ts: u64) -> Result<(), MigrateError> {
        if std::fs::symlink_metadata(path).is_err() {
            return Ok(());
        }
        let pre = self.pre_dir(data, ts)?;
        let to = pre.join(rel);
        if let Some(parent) = to.parent() {
            std::fs::create_dir_all(parent).map_err(|e| io_err(parent, e))?;
        }
        let moved = match self.ops.rename_retrying(path, &to) {
            Ok(()) => to,
            Err(e) if (self.ops.is_transient)(&e) => {
                return Err(MigrateError::Locked(path.to_path_buf()));
            }
            Err(_) => {
                let name = path.file_name().unwrap_or_default().to_string_lossy();
                let beside = path.with_file_name(format!("{name}.pre-migrate-{ts}"));
                match self.ops.rename_retrying(path, &beside) {
                    Ok(()) => beside,
                    Err(e) if (self.ops.is_transient)(&e) => {
                        return Err(MigrateError::Locked(path.to_path_buf()));
                    }
                    Err(e) => return Err(io_err(path, e)),
                }
            }
        };
        self.asides.push((path.to_path_buf(), moved));
        Ok(())
    }

    /// Best effort: whatever this run wrote goes into `<pre>/partial`, and the asides go back.
    /// The aside directory is only created when there is something to put in it, and its empty
    /// skeleton is removed again once everything went back, so a failed run leaves no empty
    /// `pre-migrate-<ts>` behind. If any aside could not be put back, nothing is pruned: what is
    /// still in there is the user's data and stays exactly as it was set aside.
    fn rollback(&mut self, data: &Path, ts: u64) {
        let written: Vec<(usize, PathBuf)> = self
            .created
            .iter()
            .enumerate()
            .filter(|(_, p)| std::fs::symlink_metadata(p).is_ok())
            .map(|(i, p)| (i, p.clone()))
            .collect();
        if !written.is_empty() {
            if let Ok(pre) = self.pre_dir(data, ts) {
                let partial = pre.join("partial");
                for (i, p) in &written {
                    let _ = std::fs::create_dir_all(&partial);
                    let name = p.file_name().unwrap_or_default().to_string_lossy();
                    let _ = std::fs::rename(p, partial.join(format!("{i}-{name}")));
                }
            }
        }
        let mut all_back = true;
        for (orig, moved) in self.asides.iter().rev() {
            all_back &= self.ops.rename_retrying(moved, orig).is_ok();
        }
        if let Some(pre) = self.pre.as_ref().filter(|_| all_back) {
            prune_aside_skeleton(pre);
        }
    }
}

/// The directories a run creates under its own `pre-migrate-<ts>`: the `rel` bases `Run::aside`
/// is called with, and `partial` from `Run::rollback`.
const ASIDE_SKELETON: [&str; 4] = ["data", "config", "webview", "partial"];

/// Remove the empty skeleton a run created: `pre`'s direct subfolders in [`ASIDE_SKELETON`], then
/// `pre` itself. Non-recursive `remove_dir` only, which refuses a non-empty directory, and nothing
/// below those subfolders is ever walked, so set-aside content (empty folders included) is never
/// touched.
fn prune_aside_skeleton(pre: &Path) {
    for sub in ASIDE_SKELETON {
        let _ = std::fs::remove_dir(pre.join(sub));
    }
    let _ = std::fs::remove_dir(pre);
}

/// Copy one file, refusing to follow a link (the caller has checked `src` is a plain file).
fn copy_file(src: &Path, dest: &Path, run: &mut Run) -> Result<(), MigrateError> {
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent).map_err(|e| io_err(parent, e))?;
    }
    let n = std::fs::copy(src, dest).map_err(|e| io_err(src, e))?;
    run.files += 1;
    run.bytes += n;
    Ok(())
}

/// Copy a tree, skipping links and whatever `skip` names, at any depth.
fn copy_tree(
    src: &Path,
    dest: &Path,
    skip: &dyn Fn(&OsStr) -> bool,
    run: &mut Run,
) -> Result<(), MigrateError> {
    std::fs::create_dir_all(dest).map_err(|e| io_err(dest, e))?;
    for entry in std::fs::read_dir(src).map_err(|e| io_err(src, e))? {
        let entry = entry.map_err(|e| io_err(src, e))?;
        let name = entry.file_name();
        if skip(&name) {
            continue;
        }
        let from = entry.path();
        let ft = entry.file_type().map_err(|e| io_err(&from, e))?;
        if ft.is_symlink() {
            continue;
        }
        let to = dest.join(&name);
        if ft.is_dir() {
            copy_tree(&from, &to, skip, run)?;
        } else if ft.is_file() {
            copy_file(&from, &to, run)?;
        }
    }
    Ok(())
}

fn with_suffix(p: &Path, suffix: &str) -> PathBuf {
    let mut s = p.as_os_str().to_owned();
    s.push(suffix);
    PathBuf::from(s)
}

/// Copy upstream's three database files into `work`. Before anything of Forge's is touched, so a
/// file upstream still holds stops the run with nothing to undo.
fn stage_database(src: &Path, work: &Path, run: &mut Run) -> Result<PathBuf, MigrateError> {
    std::fs::create_dir_all(work).map_err(|e| io_err(work, e))?;
    let staged = work.join(DB_FILE);
    for suffix in DB_SUFFIXES {
        let from = with_suffix(src, suffix);
        match std::fs::symlink_metadata(&from) {
            Ok(m) if m.is_file() => {
                let n = std::fs::copy(&from, with_suffix(&staged, suffix))
                    .map_err(|e| io_err(&from, e))?;
                run.bytes += n;
            }
            _ => {}
        }
    }
    Ok(staged)
}

/// `VACUUM INTO` from the staged copy (which folds its WAL in), then check what came out.
fn vacuum_into(staged: &Path, dest: &Path) -> Result<(), MigrateError> {
    let sql = |e: rusqlite::Error| MigrateError::Sqlite(e.to_string());
    let target = dest.to_str().ok_or_else(|| MigrateError::Sqlite("non-UTF-8 path".into()))?;
    {
        let conn = rusqlite::Connection::open(staged).map_err(sql)?;
        conn.execute("VACUUM INTO ?1", [target]).map_err(sql)?;
    }
    let conn = rusqlite::Connection::open_with_flags(
        dest,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(sql)?;
    let verdict: String =
        conn.query_row("PRAGMA integrity_check", [], |r| r.get(0)).map_err(sql)?;
    if verdict != "ok" {
        return Err(MigrateError::Sqlite(format!("integrity_check: {verdict}")));
    }
    Ok(())
}

/// Run a plan. `is_running` is asked first; a running upstream changes nothing.
#[cfg(test)]
pub fn execute(
    plan: &[CopyItem],
    dest: &Dest,
    is_running: impl Fn() -> bool,
) -> Result<Report, MigrateError> {
    execute_with(plan, dest, is_running, FsOps::default())
}

fn execute_with(
    plan: &[CopyItem],
    dest: &Dest,
    is_running: impl Fn() -> bool,
    ops: FsOps,
) -> Result<Report, MigrateError> {
    validate(plan, dest)?;
    if is_running() {
        return Err(MigrateError::UpstreamRunning);
    }
    let ts = now_secs();
    let mut run = Run { ops, ..Run::default() };
    let work = dest.data.join("tmp").join(format!("migrate-{ts}"));
    let result = execute_inner(plan, dest, ts, &work, &mut run);
    // Our own scratch copy, under Forge's data directory, named by us.
    if work.file_name().is_some_and(|n| n.to_string_lossy().starts_with("migrate-")) {
        let _ = std::fs::remove_dir_all(&work);
        let _ = std::fs::remove_dir(dest.data.join("tmp"));
    }
    match result {
        Ok(()) => Ok(Report {
            status: "done".into(),
            files: run.files,
            bytes: run.bytes,
            aside: run.pre.as_ref().map(|p| p.to_string_lossy().into_owned()),
            webview: dest.include_webview,
            ..Default::default()
        }),
        Err(e) => {
            run.rollback(&dest.data, ts);
            Err(e)
        }
    }
}

fn execute_inner(
    plan: &[CopyItem],
    dest: &Dest,
    ts: u64,
    work: &Path,
    run: &mut Run,
) -> Result<(), MigrateError> {
    let data = &dest.data;
    std::fs::create_dir_all(data).map_err(|e| io_err(data, e))?;

    // 1. Resolve every source; stage the database while nothing of Forge's has moved yet.
    let mut resolved = Vec::new();
    let mut staged = None;
    for it in plan {
        let Some(src) = checked_src(&it.src, &it.root)? else { continue };
        if it.kind == CopyKind::Database {
            staged = Some(stage_database(&src, work, run)?);
        }
        resolved.push((it, src));
    }

    // 2. Forge's own copies of what is about to be written go aside.
    let rel_of = |p: &Path, base: &str| PathBuf::from(base).join(p.file_name().unwrap_or_default());
    for (it, src) in &resolved {
        match it.kind {
            CopyKind::Database => {
                for suffix in DB_SUFFIXES {
                    let p = with_suffix(&it.dest, suffix);
                    run.aside(&p, &rel_of(&p, "data"), data, ts)?;
                }
            }
            CopyKind::Dir | CopyKind::File => {
                let base = if it.dest.parent() == Some(data.as_path()) { "data" } else { "config" };
                run.aside(&it.dest, &rel_of(&it.dest, base), data, ts)?;
            }
            CopyKind::Webview => run.aside(&it.dest, &rel_of(&it.dest, "webview"), data, ts)?,
            CopyKind::WebkitBeside => {
                for entry in std::fs::read_dir(src).map_err(|e| io_err(src, e))? {
                    let name = entry.map_err(|e| io_err(src, e))?.file_name();
                    if beside_skip_top(&name) || webview_skip(&name) {
                        continue;
                    }
                    let p = it.dest.join(&name);
                    run.aside(&p, &PathBuf::from("webview").join(&name), data, ts)?;
                }
            }
        }
    }

    // 3. Copy.
    for (it, src) in &resolved {
        match it.kind {
            CopyKind::Database => {
                run.created.push(it.dest.clone());
                let staged = staged.as_ref().expect("staged with the database item");
                vacuum_into(staged, &it.dest)?;
                run.files += 1;
            }
            CopyKind::Dir => {
                run.created.push(it.dest.clone());
                copy_tree(src, &it.dest, &|_| false, run)?;
            }
            CopyKind::File => {
                if src.is_file() {
                    run.created.push(it.dest.clone());
                    copy_file(src, &it.dest, run)?;
                }
            }
            CopyKind::Webview => {
                run.created.push(it.dest.clone());
                copy_tree(src, &it.dest, &webview_skip, run)?;
            }
            CopyKind::WebkitBeside => {
                std::fs::create_dir_all(&it.dest).map_err(|e| io_err(&it.dest, e))?;
                for entry in std::fs::read_dir(src).map_err(|e| io_err(src, e))? {
                    let entry = entry.map_err(|e| io_err(src, e))?;
                    let name = entry.file_name();
                    if beside_skip_top(&name) || webview_skip(&name) {
                        continue;
                    }
                    let from = entry.path();
                    let ft = entry.file_type().map_err(|e| io_err(&from, e))?;
                    let to = it.dest.join(&name);
                    if ft.is_symlink() {
                        continue;
                    }
                    run.created.push(to.clone());
                    if ft.is_dir() {
                        copy_tree(&from, &to, &webview_skip, run)?;
                    } else if ft.is_file() {
                        copy_file(&from, &to, run)?;
                    }
                }
            }
        }
    }
    Ok(())
}

// --- the marker and the result -------------------------------------------------------------------

/// Phase 1: leave the marker for the next launch.
pub fn write_pending(data: &Path, pending: &Pending) -> io::Result<()> {
    std::fs::create_dir_all(data)?;
    let json = serde_json::to_vec(pending).map_err(io::Error::other)?;
    std::fs::write(data.join(PENDING_FILE), json)
}

pub fn read_pending(data: &Path) -> Option<Pending> {
    let bytes = std::fs::read(data.join(PENDING_FILE)).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// The marker, if there is a file: one that does not parse reads as a default marker
/// (`requested_at` 0), which is expired, so it is never acted on but can still be cancelled.
fn load_pending(data: &Path) -> Option<Pending> {
    let path = data.join(PENDING_FILE);
    if !std::fs::symlink_metadata(&path).is_ok_and(|m| m.is_file()) {
        return None;
    }
    Some(read_pending(data).unwrap_or_default())
}

/// Drop the pending marker (Settings ▸ Import & migrate ▸ Cancel). Removes exactly
/// `<data>/migrate-upstream.pending`, and the result file only when it reports that marker's
/// `retry` or `expired`. `Ok(false)` when there was no marker.
pub fn cancel_pending(data: &Path) -> io::Result<bool> {
    if !data.is_absolute() {
        return Err(io::Error::other("the data directory is not absolute"));
    }
    let path = data.join(PENDING_FILE);
    if path.parent() != Some(data) || path.file_name() != Some(OsStr::new(PENDING_FILE)) {
        return Err(io::Error::other("unexpected marker path"));
    }
    let existed = match std::fs::symlink_metadata(&path) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => false,
        Err(e) => return Err(e),
        Ok(m) if m.is_file() => {
            std::fs::remove_file(&path)?;
            true
        }
        Ok(_) => return Err(io::Error::other("the marker is not a plain file")),
    };
    if peek_result(data).is_some_and(|r| r.status == "retry" || r.status == "expired") {
        let result = data.join(RESULT_FILE);
        if std::fs::symlink_metadata(&result).is_ok_and(|m| m.is_file()) {
            let _ = std::fs::remove_file(result);
        }
    }
    Ok(existed)
}

fn write_result(data: &Path, report: &Report) {
    if let Ok(json) = serde_json::to_vec_pretty(report) {
        let _ = std::fs::write(data.join(RESULT_FILE), json);
    }
}

/// The last result, without clearing it (the setup reads it to fix the prompt setting).
pub fn peek_result(data: &Path) -> Option<Report> {
    let bytes = std::fs::read(data.join(RESULT_FILE)).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// The last result, read once: the UI shows it and it is gone.
pub fn take_result(data: &Path) -> Option<Report> {
    let report = peek_result(data);
    let _ = std::fs::remove_file(data.join(RESULT_FILE));
    report
}

/// Phase 2 with everything injected, `now` included (unix seconds). `None` when there is no
/// marker. A marker past [`PENDING_TTL`] is not acted on: it stays, marked `expired`, and the
/// result says so, for the UI to offer a retry or a cancel. Otherwise the marker stays when
/// upstream is running or holds a file (`retry`) and is removed after anything else.
/// `dest.include_webview` comes from the marker.
pub fn apply_pending_at(
    dest: Dest,
    upstream: Result<Option<Upstream>, MigrateError>,
    is_running: impl Fn() -> bool,
    now: u64,
) -> Option<Report> {
    apply_pending_with(dest, upstream, is_running, now, FsOps::default())
}

fn apply_pending_with(
    dest: Dest,
    upstream: Result<Option<Upstream>, MigrateError>,
    is_running: impl Fn() -> bool,
    now: u64,
    ops: FsOps,
) -> Option<Report> {
    let data = dest.data.clone();
    let mut pending = load_pending(&data)?;
    if pending_expired(&pending, now) {
        pending.last_status = Some("expired".into());
        let _ = write_pending(&data, &pending);
        let report = Report {
            status: "expired".into(),
            webview: pending.include_webview,
            prompted: pending.prompted.clone(),
            ..Default::default()
        };
        write_result(&data, &report);
        return Some(report);
    }
    let dest = Dest { include_webview: pending.include_webview, ..dest };
    let outcome = upstream.and_then(|up| {
        let up = up.ok_or_else(|| MigrateError::Io {
            path: data.clone(),
            source: io::Error::new(io::ErrorKind::NotFound, "no LiMusic data on this machine"),
        })?;
        execute_with(&plan(&up, &dest), &dest, &is_running, ops)
    });
    let mut report = match outcome {
        Ok(r) => r,
        Err(e @ (MigrateError::UpstreamRunning | MigrateError::Locked(_))) => Report {
            status: "retry".into(),
            error: Some(e.to_string()),
            webview: dest.include_webview,
            ..Default::default()
        },
        Err(e) => Report {
            status: "error".into(),
            error: Some(e.to_string()),
            webview: dest.include_webview,
            ..Default::default()
        },
    };
    report.prompted = pending.prompted.clone();
    if report.status == "retry" {
        // Kept for the next launch within the window; the UI can see it is waiting.
        pending.last_status = Some("retry".into());
        let _ = write_pending(&data, &pending);
    } else {
        let _ = std::fs::remove_file(data.join(PENDING_FILE));
    }
    write_result(&data, &report);
    Some(report)
}

/// Phase 2, from `run()` before the builder: nothing has the database open yet.
pub fn apply_pending() -> Option<Report> {
    let data = crate::paths::data_dir_early()?;
    let pending = load_pending(&data)?;
    // A restart overlaps the old process's last moments, and it still holds the database and the
    // webview profile. Wait for it to let go of the single-instance lock; a Forge still running
    // after that is a second launch, which leaves the marker for the real restart. Only for a
    // marker still in its window: an expired one is never acted on, so nothing is worth waiting
    // for.
    if !pending_expired(&pending, now_secs()) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while forge_running() {
            if std::time::Instant::now() > deadline {
                return None;
            }
            std::thread::sleep(std::time::Duration::from_millis(200));
        }
    }
    let webview = crate::paths::webview_root_early()?;
    let window_state = crate::paths::window_state_early();
    let layout = Layout::current()?;
    apply_pending_at(
        Dest { data, webview, window_state, layout, include_webview: false },
        locate(),
        upstream_running,
        now_secs(),
    )
}

/// The setting value after a migration: answered `now`, with this source among those seen.
pub fn prompted_after_migration(prev: Option<&str>) -> String {
    let mut sources: Vec<String> = prev
        .and_then(|v| serde_json::from_str::<serde_json::Value>(v).ok())
        .and_then(|v| v.get("sources").cloned())
        .and_then(|s| serde_json::from_value(s).ok())
        .unwrap_or_default();
    if !sources.iter().any(|s| s == SOURCE_ID) {
        sources.push(SOURCE_ID.into());
    }
    serde_json::json!({ "answer": "now", "sources": sources }).to_string()
}

// --- is it running -------------------------------------------------------------------------------

/// Whether a single-instance lock named after `id` is held: the mutex the single-instance plugin
/// creates on Windows (`<id>-sim`), its D-Bus name on Linux (`<id>.SingleInstance`).
#[cfg(windows)]
fn instance_running(id: &str) -> bool {
    use windows::core::HSTRING;
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Threading::{OpenMutexW, SYNCHRONIZATION_SYNCHRONIZE};
    let name = HSTRING::from(format!("{id}-sim"));
    // SAFETY: a valid wide string for the name; the handle is closed right away.
    match unsafe { OpenMutexW(SYNCHRONIZATION_SYNCHRONIZE, false, &name) } {
        Ok(h) => {
            let _ = unsafe { CloseHandle(h) };
            true
        }
        Err(_) => false,
    }
}

#[cfg(target_os = "linux")]
fn instance_running(id: &str) -> bool {
    let name = format!("{id}.SingleInstance");
    let Ok(conn) = zbus::blocking::Connection::session() else { return false };
    let Ok(proxy) = zbus::blocking::fdo::DBusProxy::new(&conn) else { return false };
    let Ok(bus) = zbus::names::BusName::try_from(name.as_str()) else { return false };
    proxy.name_has_owner(bus).unwrap_or(false)
}

#[cfg(not(any(windows, target_os = "linux")))]
fn instance_running(_id: &str) -> bool {
    false
}

pub fn upstream_running() -> bool {
    instance_running(UPSTREAM_ID)
}

fn forge_running() -> bool {
    instance_running(crate::paths::IDENTIFIER)
}

/// Whether upstream starts at login. Its entry is named after its package, `limusic` (the
/// autostart plugin's default); ours is never touched here.
pub fn upstream_autostart() -> Option<bool> {
    if Layout::current().is_none() {
        return None;
    }
    let exe = std::env::current_exe().ok()?;
    auto_launch::AutoLaunchBuilder::new()
        .set_app_name("limusic")
        .set_app_path(&exe.to_string_lossy())
        .build()
        .ok()?
        .is_enabled()
        .ok()
}

/// Bytes under `dir`, not following links. Metadata only: nothing is opened.
pub fn tree_size(dir: &Path) -> u64 {
    let mut total = 0;
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else { continue };
        for entry in rd.flatten() {
            let Ok(ft) = entry.file_type() else { continue };
            if ft.is_symlink() {
                continue;
            }
            if ft.is_dir() {
                stack.push(entry.path());
            } else if let Ok(m) = entry.metadata() {
                total += m.len();
            }
        }
    }
    total
}

/// PlaylistForge's database, when there is one. Only its existence is checked (the importer
/// comes later); opening it would create a `-shm` beside it.
pub fn playlistforge_db() -> Option<PathBuf> {
    let p = dirs::data_dir()?.join("PlaylistForge").join("forge.db");
    p.is_file().then_some(p)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::time::SystemTime;

    /// A fake upstream install under a temp dir: never the real folders.
    struct Fake {
        _tmp: tempfile::TempDir,
        up: Upstream,
        dest: Dest,
    }

    fn fake(layout: Layout) -> Fake {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().canonicalize().unwrap();
        let roaming = base.join("Roaming").join(UPSTREAM_ID);
        let local = base.join("Local").join(UPSTREAM_ID);
        std::fs::create_dir_all(&roaming).unwrap();

        // A WAL-mode database with rows still in the WAL: the copy has to fold them in.
        {
            let conn = rusqlite::Connection::open(roaming.join(DB_FILE)).unwrap();
            conn.query_row("PRAGMA journal_mode=WAL", [], |r| r.get::<_, String>(0)).unwrap();
            conn.execute_batch(
                "PRAGMA wal_autocheckpoint=0;
                 CREATE TABLE settings(key TEXT PRIMARY KEY, value TEXT);
                 INSERT INTO settings VALUES ('locale','es'),('theme','dark');",
            )
            .unwrap();
            for i in 0..50 {
                conn.execute("INSERT INTO settings VALUES (?1, 'x')", [format!("k{i}")]).unwrap();
            }
            // Leak the connection's WAL: copy the files while it is still open, then close it.
            let snap = base.join("snap");
            std::fs::create_dir_all(&snap).unwrap();
            for s in DB_SUFFIXES {
                let _ = std::fs::copy(
                    with_suffix(&roaming.join(DB_FILE), s),
                    with_suffix(&snap.join(DB_FILE), s),
                );
            }
            drop(conn);
            for s in DB_SUFFIXES {
                let _ = std::fs::remove_file(with_suffix(&roaming.join(DB_FILE), s));
                let _ = std::fs::copy(
                    with_suffix(&snap.join(DB_FILE), s),
                    with_suffix(&roaming.join(DB_FILE), s),
                );
            }
            std::fs::remove_dir_all(&snap).unwrap();
            let wal = std::fs::metadata(with_suffix(&roaming.join(DB_FILE), "-wal")).unwrap();
            assert!(wal.len() > 0, "the fake needs rows still in its WAL");
        }
        std::fs::create_dir_all(roaming.join("cipher_cache")).unwrap();
        std::fs::write(roaming.join("cipher_cache").join("player.js"), b"cipher").unwrap();
        std::fs::create_dir_all(roaming.join("covers").join("ab")).unwrap();
        std::fs::write(roaming.join("covers").join("ab").join("c.jpg"), b"jpeg").unwrap();
        std::fs::write(roaming.join("app-icon.png"), b"png").unwrap();
        std::fs::write(roaming.join("limusic.log"), b"log").unwrap();

        let (config, webview_src, webview_dest, data) = match layout {
            Layout::Windows => {
                let wv = local.join("EBWebView");
                (
                    roaming.clone(),
                    wv,
                    base.join("Local").join("forge"),
                    base.join("Roaming").join("forge"),
                )
            }
            Layout::Linux => {
                let cfg = base.join("config").join(UPSTREAM_ID);
                let data = base.join("share").join("forge");
                (cfg, roaming.clone(), data.clone(), data)
            }
        };
        std::fs::create_dir_all(&config).unwrap();
        std::fs::write(config.join(".window-state.json"), b"{\"main\":{}}").unwrap();
        // A webview profile: session data, plus caches and a lock that must stay behind.
        let profile = match layout {
            Layout::Windows => webview_src.join("Default"),
            Layout::Linux => webview_src.join("localstorage"),
        };
        std::fs::create_dir_all(profile.join("Cache")).unwrap();
        std::fs::write(profile.join("Cookies"), b"cookies").unwrap();
        std::fs::write(profile.join("Cache").join("data_0"), b"cache").unwrap();
        std::fs::create_dir_all(webview_src.join("Crashpad")).unwrap();
        std::fs::write(webview_src.join("Crashpad").join("dump"), b"dump").unwrap();
        std::fs::write(webview_src.join("lockfile"), b"").unwrap();

        let up = Upstream { roaming, local: (layout == Layout::Windows).then_some(local), config };
        let dest = Dest {
            data,
            webview: webview_dest,
            window_state: Some(base.join("forge-config").join(".window-state.json")),
            layout,
            include_webview: true,
        };
        Fake { _tmp: tmp, up, dest }
    }

    /// (len, mtime) of every entry under the upstream roots, links included as entries.
    fn snapshot(up: &Upstream) -> BTreeMap<PathBuf, (u64, Option<SystemTime>)> {
        let mut out = BTreeMap::new();
        let mut stack: Vec<PathBuf> = vec![up.roaming.clone(), up.config.clone()];
        stack.extend(up.local.clone());
        while let Some(d) = stack.pop() {
            let Ok(rd) = std::fs::read_dir(&d) else { continue };
            for e in rd.flatten() {
                let m = std::fs::symlink_metadata(e.path()).unwrap();
                out.insert(e.path(), (m.len(), m.modified().ok()));
                if m.is_dir() {
                    stack.push(e.path());
                }
            }
        }
        out
    }

    fn rows(db: &Path) -> i64 {
        let conn = rusqlite::Connection::open(db).unwrap();
        conn.query_row("SELECT count(*) FROM settings", [], |r| r.get(0)).unwrap()
    }

    #[test]
    fn migrate_upstream_copies_everything_and_leaves_the_source_alone() {
        let f = fake(Layout::Windows);
        let before = snapshot(&f.up);
        let report = execute(&plan(&f.up, &f.dest), &f.dest, || false).unwrap();
        assert_eq!(report.status, "done");
        assert_eq!(report.aside, None, "nothing of Forge's to move aside");

        let d = &f.dest.data;
        assert_eq!(std::fs::read(d.join("cipher_cache/player.js")).unwrap(), b"cipher");
        assert_eq!(std::fs::read(d.join("covers/ab/c.jpg")).unwrap(), b"jpeg");
        assert_eq!(std::fs::read(d.join("app-icon.png")).unwrap(), b"png");
        assert!(!d.join("limusic.log").exists(), "the log stays behind");
        assert!(f.dest.window_state.as_ref().unwrap().is_file());
        let wv = f.dest.webview.join("EBWebView");
        assert_eq!(std::fs::read(wv.join("Default/Cookies")).unwrap(), b"cookies");
        assert!(!wv.join("Default/Cache").exists());
        assert!(!wv.join("Crashpad").exists());
        assert!(!wv.join("lockfile").exists());
        assert!(!d.join("tmp").exists(), "the staging copy is cleaned up");

        // The database: whole, with the rows that were still in the WAL.
        let db = d.join(DB_FILE);
        let conn = rusqlite::Connection::open(&db).unwrap();
        let ok: String = conn.query_row("PRAGMA integrity_check", [], |r| r.get(0)).unwrap();
        assert_eq!(ok, "ok");
        drop(conn);
        assert_eq!(rows(&db), 52);

        assert_eq!(snapshot(&f.up), before, "upstream changed");
    }

    #[test]
    fn migrate_upstream_puts_forges_previous_data_aside() {
        let f = fake(Layout::Windows);
        let d = &f.dest.data;
        std::fs::create_dir_all(d).unwrap();
        {
            let conn = rusqlite::Connection::open(d.join(DB_FILE)).unwrap();
            conn.execute_batch("CREATE TABLE mine(x); INSERT INTO mine VALUES (1);").unwrap();
        }
        std::fs::write(with_suffix(&d.join(DB_FILE), "-wal"), b"").unwrap();
        std::fs::create_dir_all(f.dest.webview.join("EBWebView/Default")).unwrap();
        std::fs::write(f.dest.webview.join("EBWebView/Default/Cookies"), b"forge").unwrap();

        let report = execute(&plan(&f.up, &f.dest), &f.dest, || false).unwrap();
        let aside = PathBuf::from(report.aside.expect("an aside directory"));
        assert!(aside.starts_with(d));
        assert!(aside.file_name().unwrap().to_string_lossy().starts_with("pre-migrate-"));
        let old = rusqlite::Connection::open(aside.join("data").join(DB_FILE)).unwrap();
        let n: i64 = old.query_row("SELECT count(*) FROM mine", [], |r| r.get(0)).unwrap();
        assert_eq!(n, 1);
        assert!(aside.join("data").join("limusic.sqlite-wal").exists());
        assert_eq!(
            std::fs::read(aside.join("webview/EBWebView/Default/Cookies")).unwrap(),
            b"forge"
        );
        assert_eq!(rows(&d.join(DB_FILE)), 52, "the new database is upstream's");
    }

    #[test]
    fn migrate_upstream_refuses_a_destination_inside_upstream() {
        let f = fake(Layout::Windows);
        for data in [
            f.up.roaming.clone(),
            f.up.roaming.join("nested"),
            f.up.roaming.parent().unwrap().to_path_buf(),
        ] {
            let dest = Dest { data, ..f.dest.clone() };
            let err = execute(&plan(&f.up, &dest), &dest, || false).unwrap_err();
            assert!(matches!(err, MigrateError::SameOrNested), "{err:?}");
        }
        let dest = Dest { webview: f.up.local.clone().unwrap(), ..f.dest.clone() };
        assert!(matches!(
            execute(&plan(&f.up, &dest), &dest, || false),
            Err(MigrateError::SameOrNested)
        ));
    }

    #[test]
    fn migrate_upstream_running_changes_nothing() {
        let f = fake(Layout::Windows);
        let before = snapshot(&f.up);
        let err = execute(&plan(&f.up, &f.dest), &f.dest, || true).unwrap_err();
        assert!(matches!(err, MigrateError::UpstreamRunning));
        assert!(!f.dest.data.exists(), "nothing written");
        assert_eq!(snapshot(&f.up), before);
    }

    #[test]
    fn migrate_upstream_marker_round_trip() {
        let f = fake(Layout::Windows);
        let data = f.dest.data.clone();
        let dest = f.dest.clone();
        // No marker: nothing happens.
        assert_eq!(apply_pending_at(dest.clone(), Ok(Some(f.up.clone())), || false, NOW), None);

        let pending = Pending {
            include_webview: false,
            prompted: Some(r#"{"answer":"later","sources":["playlistforge"]}"#.into()),
            requested_at: NOW,
            last_status: None,
        };
        write_pending(&data, &pending).unwrap();
        assert_eq!(read_pending(&data), Some(pending.clone()));

        // Upstream open: retry, and the marker stays, saying so.
        let r = apply_pending_at(dest.clone(), Ok(Some(f.up.clone())), || true, NOW + 5).unwrap();
        assert_eq!(r.status, "retry");
        assert!(data.join(PENDING_FILE).exists());
        assert_eq!(read_pending(&data).unwrap().last_status.as_deref(), Some("retry"));
        assert_eq!(take_result(&data).unwrap().status, "retry");
        assert!(!data.join(RESULT_FILE).exists(), "read once");

        // Closed, still within the window: done, marker gone, the prompt answer carried over,
        // no webview this time.
        let r = apply_pending_at(dest.clone(), Ok(Some(f.up.clone())), || false, NOW + PENDING_TTL)
            .unwrap();
        assert_eq!(r.status, "done", "{:?}", r.error);
        assert!(!r.webview);
        assert!(!data.join(PENDING_FILE).exists());
        assert!(!f.dest.webview.join("EBWebView").exists());
        let peeked = peek_result(&data).unwrap();
        assert_eq!(peeked.prompted, pending.prompted);
        let v: serde_json::Value =
            serde_json::from_str(&prompted_after_migration(peeked.prompted.as_deref())).unwrap();
        assert_eq!(v["answer"], "now");
        assert_eq!(v["sources"], serde_json::json!(["playlistforge", "limusic"]));
        assert_eq!(take_result(&data), Some(peeked));

        // An unresolvable directory is an error that does not loop.
        write_pending(&data, &pending).unwrap();
        let r =
            apply_pending_at(dest.clone(), Err(MigrateError::UnresolvedDir("data")), || false, NOW)
                .unwrap();
        assert_eq!(r.status, "error");
        assert!(!data.join(PENDING_FILE).exists());
    }

    #[test]
    fn migrate_upstream_linux_plan_and_copy() {
        let f = fake(Layout::Linux);
        let items = plan(&f.up, &f.dest);
        let kinds: Vec<CopyKind> = items.iter().map(|i| i.kind).collect();
        assert_eq!(
            kinds,
            [
                CopyKind::Database,
                CopyKind::Dir,
                CopyKind::Dir,
                CopyKind::File,
                CopyKind::File,
                CopyKind::WebkitBeside
            ]
        );
        assert!(items.iter().all(|i| i.src.starts_with(&i.root)));
        assert_eq!(items[4].src, f.up.config.join(".window-state.json"));

        let before = snapshot(&f.up);
        execute(&items, &f.dest, || false).unwrap();
        let d = &f.dest.data;
        assert_eq!(std::fs::read(d.join("localstorage/Cookies")).unwrap(), b"cookies");
        assert!(!d.join("localstorage/Cache").exists());
        assert!(!d.join("Crashpad").exists() && !d.join("lockfile").exists());
        assert!(!d.join("limusic.log").exists());
        assert_eq!(rows(&d.join(DB_FILE)), 52);
        assert_eq!(snapshot(&f.up), before);

        // Without the webview, Linux copies only the data items.
        let dest = Dest { include_webview: false, ..f.dest.clone() };
        assert!(plan(&f.up, &dest).iter().all(|i| i.kind != CopyKind::WebkitBeside));
    }

    #[test]
    fn migrate_upstream_portable_plan_skips_window_state() {
        let f = fake(Layout::Windows);
        let dest = Dest { window_state: None, ..f.dest.clone() };
        let items = plan(&f.up, &dest);
        assert!(items.iter().all(|i| !i.src.ends_with(".window-state.json")));
        assert!(items
            .iter()
            .any(|i| i.kind == CopyKind::Webview && i.dest == dest.webview.join("EBWebView")));
    }

    #[cfg(unix)]
    #[test]
    fn migrate_upstream_does_not_follow_symlinks() {
        let f = fake(Layout::Windows);
        let outside = f.up.roaming.parent().unwrap().join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("secret"), b"s").unwrap();
        std::os::unix::fs::symlink(&outside, f.up.roaming.join("covers").join("link")).unwrap();
        execute(&plan(&f.up, &f.dest), &f.dest, || false).unwrap();
        assert!(!f.dest.data.join("covers/link").exists());
    }

    #[test]
    fn migrate_upstream_test_overrides_point_locate_at_a_copy() {
        let f = fake(Layout::Windows);
        let roaming = f.up.roaming.clone();
        let local = f.up.local.clone().unwrap();
        std::fs::create_dir_all(&local).unwrap();
        let env = |k: &str| match k {
            "LIMUSIC_FORGE_UPSTREAM_ROAMING" => Some(roaming.clone().into_os_string()),
            "LIMUSIC_FORGE_UPSTREAM_LOCAL" => Some(local.clone().into_os_string()),
            _ => None,
        };
        let up = locate_at(Some(Layout::Windows), test_overrides(env)).unwrap().unwrap();
        assert_eq!(up.roaming, roaming.canonicalize().unwrap());
        assert_eq!(up.local, Some(local.canonicalize().unwrap()));
        assert_eq!(up.config, up.roaming, "Windows: the window state follows roaming");

        // Linux: an explicit config override; no local directory.
        let cfg = f.up.config.clone();
        let env = |k: &str| match k {
            "LIMUSIC_FORGE_UPSTREAM_ROAMING" => Some(roaming.clone().into_os_string()),
            "LIMUSIC_FORGE_UPSTREAM_CONFIG" => Some(cfg.clone().into_os_string()),
            _ => None,
        };
        let up = locate_at(Some(Layout::Linux), test_overrides(env)).unwrap().unwrap();
        assert_eq!(up.config, cfg.canonicalize().unwrap());
        assert_eq!(up.local, None);

        // An override that does not exist: no upstream, not the real one.
        let missing = roaming.parent().unwrap().join("missing");
        let env = |k: &str| {
            (k == "LIMUSIC_FORGE_UPSTREAM_ROAMING").then(|| missing.clone().into_os_string())
        };
        assert_eq!(locate_at(Some(Layout::Windows), test_overrides(env)).unwrap(), None);

        // A relative override is refused; an empty one is ignored.
        let env = |k: &str| (k == "LIMUSIC_FORGE_UPSTREAM_ROAMING").then(|| "relative".into());
        assert!(matches!(
            locate_at(Some(Layout::Windows), test_overrides(env)),
            Err(MigrateError::UnresolvedDir(_))
        ));
        let ov = test_overrides(|_| Some(std::ffi::OsString::new()));
        assert!(ov.roaming.is_none() && ov.local.is_none() && ov.config.is_none());
    }

    fn os_err(code: i32) -> io::Error {
        io::Error::from_raw_os_error(code)
    }

    fn code_is(codes: &'static [i32]) -> impl Fn(&io::Error) -> bool {
        move |e| e.raw_os_error().is_some_and(|c| codes.contains(&c))
    }

    #[test]
    fn migrate_upstream_retry_locked_retries_then_succeeds() {
        let calls = std::cell::Cell::new(0);
        let slept = std::cell::RefCell::new(Vec::new());
        let out = retry_locked(
            || {
                calls.set(calls.get() + 1);
                if calls.get() < 3 {
                    Err(os_err(32))
                } else {
                    Ok(7)
                }
            },
            code_is(&[5, 32, 33]),
            lock_backoff(),
            |d| slept.borrow_mut().push(d),
        );
        assert_eq!(out.unwrap(), 7);
        assert_eq!(calls.get(), 3);
        assert_eq!(*slept.borrow(), [Duration::from_millis(250), Duration::from_millis(500)]);
    }

    #[test]
    fn migrate_upstream_retry_locked_gives_up_with_the_error() {
        let calls = std::cell::Cell::new(0);
        let slept = std::cell::Cell::new(0);
        let sleeps = vec![Duration::from_millis(1); 4];
        let err = retry_locked(
            || -> io::Result<()> {
                calls.set(calls.get() + 1);
                Err(os_err(5))
            },
            code_is(&[5]),
            sleeps,
            |_| slept.set(slept.get() + 1),
        )
        .unwrap_err();
        assert_eq!(err.raw_os_error(), Some(5));
        assert_eq!(calls.get(), 5, "one try plus one per wait");
        assert_eq!(slept.get(), 4);
    }

    #[test]
    fn migrate_upstream_retry_locked_does_not_retry_other_errors() {
        let calls = std::cell::Cell::new(0);
        let err = retry_locked(
            || -> io::Result<()> {
                calls.set(calls.get() + 1);
                Err(io::Error::new(io::ErrorKind::NotFound, "gone"))
            },
            code_is(&[5, 32, 33]),
            lock_backoff(),
            |_| panic!("must not sleep"),
        )
        .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
        assert_eq!(calls.get(), 1);
    }

    #[test]
    fn migrate_upstream_lock_backoff_is_bounded() {
        let b = lock_backoff();
        assert_eq!(b[0], Duration::from_millis(250));
        assert!(b.iter().all(|d| *d <= Duration::from_secs(1)));
        let total: Duration = b.iter().sum();
        assert!(total <= LOCK_WAIT && total >= Duration::from_secs(14), "{total:?}");
    }

    #[test]
    fn migrate_upstream_held_webview_profile_is_retry_and_rolls_back() {
        let f = fake(Layout::Windows);
        let data = f.dest.data.clone();
        std::fs::create_dir_all(&data).unwrap();
        // Forge already has a database (goes aside first) and a webview profile (held).
        {
            let conn = rusqlite::Connection::open(data.join(DB_FILE)).unwrap();
            conn.execute_batch("CREATE TABLE mine(x); INSERT INTO mine VALUES (1);").unwrap();
        }
        let profile = f.dest.webview.join("EBWebView");
        std::fs::create_dir_all(profile.join("Default")).unwrap();
        std::fs::write(profile.join("Default/Cookies"), b"forge").unwrap();
        let pending =
            Pending { include_webview: true, prompted: None, requested_at: NOW, last_status: None };
        write_pending(&data, &pending).unwrap();
        let before = snapshot(&f.up);

        // Every rename of the profile is refused as the old webview processes would.
        let held = profile.clone();
        let tries = std::rc::Rc::new(std::cell::Cell::new(0));
        let slept = std::rc::Rc::new(std::cell::Cell::new(0));
        let (t, s) = (tries.clone(), slept.clone());
        let ops = FsOps {
            rename: Box::new(move |from: &Path, to: &Path| {
                if from == held.as_path() {
                    t.set(t.get() + 1);
                    return Err(os_err(5));
                }
                std::fs::rename(from, to)
            }),
            is_transient: Box::new(code_is(&[5, 32, 33])),
            backoff: vec![Duration::from_millis(1); 3],
            sleep: Box::new(move |_| s.set(s.get() + 1)),
        };
        let r =
            apply_pending_with(f.dest.clone(), Ok(Some(f.up.clone())), || false, NOW, ops).unwrap();

        assert_eq!(r.status, "retry", "{:?}", r.error);
        assert!(r.error.as_deref().unwrap_or_default().contains("in use"), "{:?}", r.error);
        assert_eq!(tries.get(), 4, "one try plus three retries, no beside fallback");
        assert_eq!(slept.get(), 3);
        assert!(data.join(PENDING_FILE).exists(), "the marker stays for the next launch");
        assert_eq!(peek_result(&data).unwrap().status, "retry");
        // What had gone aside is back; nothing new was written.
        let conn = rusqlite::Connection::open(data.join(DB_FILE)).unwrap();
        let n: i64 = conn.query_row("SELECT count(*) FROM mine", [], |r| r.get(0)).unwrap();
        assert_eq!(n, 1);
        assert_eq!(std::fs::read(profile.join("Default/Cookies")).unwrap(), b"forge");
        assert!(!data.join("cipher_cache").exists() && !data.join("covers").exists());
        assert!(pre_migrate_dirs(&data).is_empty(), "everything went back: no aside directory");
        assert_eq!(snapshot(&f.up), before, "upstream changed");
    }

    /// An arbitrary "now" for the marker tests: the clock is a parameter, nothing sleeps.
    const NOW: u64 = 1_800_000_000;

    fn pre_migrate_dirs(data: &Path) -> Vec<PathBuf> {
        let Ok(rd) = std::fs::read_dir(data) else { return Vec::new() };
        rd.flatten()
            .filter(|e| e.file_name().to_string_lossy().starts_with("pre-migrate-"))
            .map(|e| e.path())
            .collect()
    }

    fn marker(requested_at: u64) -> Pending {
        Pending { include_webview: false, prompted: None, requested_at, last_status: None }
    }

    #[test]
    fn migrate_upstream_pending_expiry_window() {
        let p = marker(NOW);
        assert!(!pending_expired(&p, NOW));
        assert!(!pending_expired(&p, NOW + PENDING_TTL), "the last second still counts");
        assert!(pending_expired(&p, NOW + PENDING_TTL + 1));
        assert!(!pending_expired(&p, NOW - 30), "a small clock step back is tolerated");
        assert!(pending_expired(&p, NOW - 3600), "far in the future is invalid");
        assert!(pending_expired(&marker(0), NOW), "no timestamp is invalid");
    }

    #[test]
    fn migrate_upstream_valid_marker_migrates() {
        let f = fake(Layout::Windows);
        let data = f.dest.data.clone();
        write_pending(&data, &marker(NOW)).unwrap();
        let r =
            apply_pending_at(f.dest.clone(), Ok(Some(f.up.clone())), || false, NOW + 60).unwrap();
        assert_eq!(r.status, "done", "{:?}", r.error);
        assert_eq!(rows(&data.join(DB_FILE)), 52);
        assert!(!data.join(PENDING_FILE).exists());
        assert_eq!(pending_info(&data, NOW + 60), None);
    }

    #[test]
    fn migrate_upstream_expired_marker_does_not_migrate() {
        let f = fake(Layout::Windows);
        let data = f.dest.data.clone();
        let before = snapshot(&f.up);
        let mut p = marker(NOW);
        p.include_webview = true;
        p.last_status = Some("retry".into());
        write_pending(&data, &p).unwrap();
        let later = NOW + PENDING_TTL + 1;

        // Upstream closed and everything available: still nothing happens.
        let r = apply_pending_at(f.dest.clone(), Ok(Some(f.up.clone())), || false, later).unwrap();
        assert_eq!(r.status, "expired");
        assert!(!data.join(DB_FILE).exists(), "nothing copied");
        assert!(!f.dest.webview.join("EBWebView").exists());
        assert!(pre_migrate_dirs(&data).is_empty());
        assert_eq!(snapshot(&f.up), before);

        // The marker stays, with what it asked for, marked expired; the result says so.
        let kept = read_pending(&data).unwrap();
        assert_eq!(kept.requested_at, NOW);
        assert!(kept.include_webview);
        assert_eq!(kept.last_status.as_deref(), Some("expired"));
        assert_eq!(peek_result(&data).unwrap().status, "expired");
        let info = pending_info(&data, later).unwrap();
        assert!(info.expired && info.include_webview);
        assert_eq!(info.last_status.as_deref(), Some("expired"));
        assert_eq!(info.requested_at, NOW);

        // A later launch does the same, however long after.
        let r = apply_pending_at(f.dest.clone(), Ok(Some(f.up.clone())), || false, later + 86_400)
            .unwrap();
        assert_eq!(r.status, "expired");
        assert!(!data.join(DB_FILE).exists());
    }

    #[test]
    fn migrate_upstream_invalid_requested_at_is_expired() {
        let f = fake(Layout::Windows);
        let data = f.dest.data.clone();
        for p in [marker(0), marker(NOW + 3600)] {
            write_pending(&data, &p).unwrap();
            let r =
                apply_pending_at(f.dest.clone(), Ok(Some(f.up.clone())), || false, NOW).unwrap();
            assert_eq!(r.status, "expired", "requested_at {}", p.requested_at);
            assert!(data.join(PENDING_FILE).exists());
            assert!(!data.join(DB_FILE).exists());
        }
        // A marker that does not parse: expired too, never acted on, still cancellable.
        std::fs::write(data.join(PENDING_FILE), b"{\"requested_at\":\"soon\"").unwrap();
        assert!(pending_info(&data, NOW).unwrap().expired);
        let r = apply_pending_at(f.dest.clone(), Ok(Some(f.up.clone())), || false, NOW).unwrap();
        assert_eq!(r.status, "expired");
        assert!(!data.join(DB_FILE).exists());
        assert!(cancel_pending(&data).unwrap());
        assert!(!data.join(PENDING_FILE).exists());
    }

    #[test]
    fn migrate_upstream_cancel_removes_only_the_marker() {
        let tmp = tempfile::tempdir().unwrap();
        let data = tmp.path().canonicalize().unwrap().join("forge");
        std::fs::create_dir_all(data.join("pre-migrate-1")).unwrap();
        std::fs::write(data.join("pre-migrate-1").join("keep"), b"k").unwrap();
        std::fs::write(data.join(DB_FILE), b"db").unwrap();
        std::fs::write(data.join("migrate-upstream.pending.bak"), b"other").unwrap();
        write_pending(&data, &marker(NOW)).unwrap();
        write_result(&data, &Report { status: "retry".into(), ..Default::default() });

        assert!(cancel_pending(&data).unwrap());
        assert!(!data.join(PENDING_FILE).exists());
        assert!(!data.join(RESULT_FILE).exists(), "the retry result went with its marker");
        assert_eq!(std::fs::read(data.join(DB_FILE)).unwrap(), b"db");
        assert!(data.join("pre-migrate-1").join("keep").is_file());
        assert!(data.join("migrate-upstream.pending.bak").is_file());
        assert_eq!(pending_info(&data, NOW), None);

        // Nothing pending: nothing to do. A `done` result is not this marker's and stays.
        write_result(&data, &Report { status: "done".into(), ..Default::default() });
        assert!(!cancel_pending(&data).unwrap());
        assert!(data.join(RESULT_FILE).is_file());

        // A directory where the marker should be is not removed; a relative data dir is refused.
        std::fs::create_dir_all(data.join(PENDING_FILE)).unwrap();
        assert!(cancel_pending(&data).is_err());
        assert!(data.join(PENDING_FILE).is_dir());
        assert!(cancel_pending(Path::new("relative")).is_err());
    }

    #[test]
    fn migrate_upstream_rollback_without_asides_creates_no_folder() {
        let tmp = tempfile::tempdir().unwrap();
        let data = tmp.path().canonicalize().unwrap().join("forge");
        std::fs::create_dir_all(&data).unwrap();
        let mut run = Run::default();
        run.rollback(&data, 7);
        assert!(pre_migrate_dirs(&data).is_empty());
        assert!(run.pre.is_none());

        // A run that fails without having moved anything aside leaves no aside directory behind.
        let f = fake(Layout::Windows);
        let ops = FsOps {
            rename: Box::new(|_: &Path, _: &Path| -> io::Result<()> { Err(os_err(32)) }),
            is_transient: Box::new(code_is(&[5, 32, 33])),
            backoff: vec![],
            sleep: Box::new(|_: Duration| {}),
        };
        // Forge has only a webview profile, held: its aside fails, nothing else went aside.
        let profile = f.dest.webview.join("EBWebView");
        std::fs::create_dir_all(profile.join("Default")).unwrap();
        let err = execute_with(&plan(&f.up, &f.dest), &f.dest, || false, ops).unwrap_err();
        assert!(matches!(err, MigrateError::Locked(_)), "{err:?}");
        assert!(pre_migrate_dirs(&f.dest.data).is_empty());
        assert!(profile.join("Default").is_dir(), "Forge's profile untouched");
    }

    #[test]
    fn migrate_upstream_failed_restore_prunes_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let data = tmp.path().canonicalize().unwrap().join("forge");
        // Forge's own data: a folder whose content includes empty subfolders, and a plain file.
        let covers = data.join("covers");
        std::fs::create_dir_all(covers.join("empty-a")).unwrap();
        std::fs::create_dir_all(covers.join("nested").join("empty-b")).unwrap();
        std::fs::write(covers.join("c.jpg"), b"img").unwrap();
        let settings = data.join("settings.json");
        std::fs::write(&settings, b"{}").unwrap();

        // Putting `covers` back fails (injected); every other rename is real.
        let blocked = covers.clone();
        let ops = FsOps {
            rename: Box::new(move |from: &Path, to: &Path| {
                if to == blocked.as_path() {
                    return Err(io::Error::other("injected restore failure"));
                }
                std::fs::rename(from, to)
            }),
            is_transient: Box::new(|_: &io::Error| false),
            backoff: vec![],
            sleep: Box::new(|_: Duration| {}),
        };
        let mut run = Run { ops, ..Default::default() };
        run.aside(&covers, Path::new("data/covers"), &data, 7).unwrap();
        run.aside(&settings, Path::new("config/settings.json"), &data, 7).unwrap();
        let pre = run.pre.clone().unwrap();
        run.rollback(&data, 7);

        // settings went back; covers could not and stays aside exactly as it was, empty
        // subfolders included, and the skeleton around it is kept too.
        assert_eq!(std::fs::read(&settings).unwrap(), b"{}");
        assert!(!covers.exists());
        let kept = pre.join("data").join("covers");
        assert!(kept.join("empty-a").is_dir(), "empty folder inside the aside data was pruned");
        assert!(kept.join("nested").join("empty-b").is_dir(), "nested empty folder was pruned");
        assert_eq!(std::fs::read(kept.join("c.jpg")).unwrap(), b"img");
        assert!(pre.join("config").is_dir(), "nothing is pruned when a restore failed");
    }

    #[test]
    fn migrate_upstream_rollback_prunes_only_the_skeleton() {
        let tmp = tempfile::tempdir().unwrap();
        let data = tmp.path().canonicalize().unwrap().join("forge");
        let covers = data.join("covers");
        std::fs::create_dir_all(covers.join("empty-a")).unwrap();
        let mut run = Run::default();
        run.aside(&covers, Path::new("data/covers"), &data, 7).unwrap();
        // Something this run wrote, moved into `partial` on rollback, with an empty folder in it.
        let written = data.join("cipher_cache");
        std::fs::create_dir_all(written.join("empty")).unwrap();
        run.created.push(written.clone());
        let pre = run.pre.clone().unwrap();
        run.rollback(&data, 7);

        assert!(covers.join("empty-a").is_dir(), "covers went back whole");
        assert!(!pre.join("data").exists(), "empty skeleton folder pruned");
        let moved = pre.join("partial").join("0-cipher_cache");
        assert!(moved.is_dir(), "partial keeps its content");
        assert!(moved.join("empty").is_dir(), "empty folder inside partial content is not walked");
    }

    #[test]
    fn migrate_upstream_relative_paths_are_refused() {
        let f = fake(Layout::Windows);
        let dest = Dest { data: PathBuf::from("relative"), ..f.dest.clone() };
        assert!(matches!(
            execute(&plan(&f.up, &dest), &dest, || false),
            Err(MigrateError::UnresolvedDir(_))
        ));
    }
}
