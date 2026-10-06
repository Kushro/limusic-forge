//! The download runner, ported from PlaylistForge: one tokio task, one yt-dlp at a time.
//!
//! Sequential on purpose: the bottleneck is bandwidth, so two parallel downloads take as long as
//! two in a row, with twice the chance of YouTube throttling and a progress display that has to
//! explain itself. Cancelling is then "kill the one run" (yt-dlp with everything it started,
//! [`tools::ProcessTree`]), with nothing to look up.
//!
//! At start, off the async threads: the cookies files a crash left behind are swept, rows a
//! previous run left `running` go back to `queued` (yt-dlp resumes from its own `.part`, which
//! this module never deletes), and every `available`/`missing` row is checked against the disk.
//!
//! The loop then takes the oldest queued row, runs it, records the result and goes for the next;
//! with nothing queued it sleeps until [`Runner::nudge`] or a coarse poll. Without yt-dlp it
//! leaves the queue as it is (every row would fail the same way) and says so once.
//!
//! Events: `download-progress` (a [`Progress`], or `null` when nothing runs),
//! `downloads-changed` (a [`Changed`]) and, from the install commands, `tools-install-progress`.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tauri::{AppHandle, Emitter};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
use tokio::sync::Notify;

use crate::db::{now_secs, DownloadRow};
use crate::state::AppState;

use super::cookies::CookieFile;
use super::settings::{self, AudioQuality, CookiesMode, Format, ThumbnailMode, VideoQuality};
use super::ytdlp_args::{self, ArgsSpec};
use super::{tools, verify};

/// How long an empty queue sleeps before looking again; enqueueing nudges it awake anyway.
const IDLE_POLL: Duration = Duration::from_secs(30);
/// How long to wait before looking for yt-dlp again. Installing it nudges.
const TOOLS_MISSING_POLL: Duration = Duration::from_secs(120);
/// The fastest `download-progress` goes out.
const PROGRESS_EVERY: Duration = Duration::from_millis(250);
/// The most of yt-dlp's stderr kept for the error message.
const STDERR_CAP: usize = 256 * 1024;

/// The download running now, as `download-progress` carries it.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct Progress {
    pub video_id: String,
    pub format: String,
    pub percent: f64,
    pub speed: Option<String>,
    pub eta: Option<String>,
}

/// `downloads-changed`: which videos' rows moved (empty: possibly any), and how a finished run
/// ended.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct Changed {
    pub video_ids: Vec<String>,
    pub outcome: Option<Outcome>,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct Outcome {
    pub video_id: String,
    pub format: String,
    /// `available`, `error`, `cancelled` or `tools_missing`.
    pub status: &'static str,
    pub title: Option<String>,
    pub error: Option<String>,
}

pub fn emit_changed(app: &AppHandle, video_ids: Vec<String>, outcome: Option<Outcome>) {
    let _ = app.emit("downloads-changed", Changed { video_ids, outcome });
}

/// The runner's shared side: what the commands poke.
#[derive(Default)]
pub struct Runner {
    wake: Notify,
    cancel: Notify,
    /// A cancel arrived for the current run. Set before `cancel` fires, so a cancel landing
    /// between the start of a run and its first wait is not lost.
    cancel_requested: AtomicBool,
    active: Mutex<Option<Progress>>,
    installing: AtomicBool,
}

/// Holds [`Runner`]'s install flag; lets it go on drop, however the install ends.
pub struct InstallGuard<'a>(&'a AtomicBool);

impl Drop for InstallGuard<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

impl Runner {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Something was queued: look now rather than at the next poll.
    pub fn nudge(&self) {
        self.wake.notify_one();
    }

    pub fn active(&self) -> Option<Progress> {
        self.active.lock().unwrap().clone()
    }

    /// Whether `(video_id, format)` is the download running now.
    pub fn is_active(&self, video_id: &str, format: &str) -> bool {
        self.active
            .lock()
            .unwrap()
            .as_ref()
            .is_some_and(|p| p.video_id == video_id && p.format == format)
    }

    /// Kills the running download, whose row is then dropped. `false` when nothing runs.
    pub fn cancel_active(&self) -> bool {
        if self.active.lock().unwrap().is_none() {
            return false;
        }
        self.cancel_requested.store(true, Ordering::SeqCst);
        self.cancel.notify_waiters();
        true
    }

    /// One tool install at a time. `None` while one runs.
    pub fn begin_install(&self) -> Option<InstallGuard<'_>> {
        self.installing
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .ok()
            .map(|_| InstallGuard(&self.installing))
    }

    fn set_active(&self, progress: Option<Progress>) {
        *self.active.lock().unwrap() = progress;
    }
}

enum Step {
    Idle,
    /// The row was taken by something else between the pick and the claim: look again now.
    Again,
    ToolsMissing(DownloadRow),
    Ran(Outcome),
}

/// Starts the runner's task. Called once, from setup.
pub fn spawn(state: Arc<AppState>, runner: Arc<Runner>) {
    tauri::async_runtime::spawn(async move {
        let data = crate::paths::data_dir(&state.app);
        let db = state.db.clone();
        let tmp = data.join("tmp");
        let changed = tauri::async_runtime::spawn_blocking(move || {
            super::cookies::sweep_stale(&tmp);
            let requeued = db.reset_orphaned_downloads();
            if requeued > 0 {
                tracing::info!(requeued, "downloads: requeued runs a previous session left");
            }
            requeued + verify::verify_all(&db, now_secs())
        })
        .await
        .unwrap_or(0);
        if changed > 0 {
            emit_changed(&state.app, Vec::new(), None);
        }

        // Said once per stretch without yt-dlp, not on every poll.
        let mut warned_missing = false;
        loop {
            match run_next(&state, &runner, &data).await {
                Step::Idle => wait(&runner.wake, IDLE_POLL).await,
                Step::Again => {}
                Step::ToolsMissing(row) => {
                    if !warned_missing {
                        warned_missing = true;
                        let outcome = Outcome {
                            video_id: row.video_id.clone(),
                            format: row.format.clone(),
                            status: "tools_missing",
                            title: None,
                            error: None,
                        };
                        emit_changed(&state.app, Vec::new(), Some(outcome));
                    }
                    wait(&runner.wake, TOOLS_MISSING_POLL).await;
                }
                Step::Ran(outcome) => {
                    warned_missing = false;
                    emit_changed(&state.app, vec![outcome.video_id.clone()], Some(outcome));
                }
            }
        }
    });
}

async fn wait(wake: &Notify, interval: Duration) {
    tokio::select! {
        _ = tokio::time::sleep(interval) => {}
        _ = wake.notified() => {}
    }
}

async fn run_next(state: &Arc<AppState>, runner: &Runner, data: &Path) -> Step {
    let Some(row) = state.db.next_queued_download() else { return Step::Idle };
    let Some(ytdlp) = tools::ytdlp_path(data) else { return Step::ToolsMissing(row) };
    // Registered as active before the row is claimed: a remove or cancel from here on goes
    // through `cancel_active` (the run then drops its own row) instead of deleting a row whose
    // download would go on and leave an orphaned file. The flag is cleared before, so such a
    // cancel is never lost.
    runner.cancel_requested.store(false, Ordering::SeqCst);
    let start = Progress {
        video_id: row.video_id.clone(),
        format: row.format.clone(),
        percent: 0.0,
        speed: None,
        eta: None,
    };
    runner.set_active(Some(start.clone()));
    if !state.db.mark_download_running(&row.video_id, &row.format) || !still_claimed(state, &row) {
        // Taken, or removed by a remove that looked just before the run became active.
        runner.set_active(None);
        return Step::Again;
    }
    let title = state.db.video_title(&row.video_id);
    let _ = state.app.emit("download-progress", Some(start));
    emit_changed(&state.app, vec![row.video_id.clone()], None);

    let outcome = execute(state, runner, data, &ytdlp, &row, title).await;

    runner.set_active(None);
    let _ = state.app.emit("download-progress", None::<Progress>);
    Step::Ran(outcome)
}

/// The claimed row is still there and `running`: a remove that checked `is_active` before the
/// run registered, and deleted after the claim, would otherwise go unnoticed.
fn still_claimed(state: &AppState, row: &DownloadRow) -> bool {
    state
        .db
        .downloads_for(std::slice::from_ref(&row.video_id))
        .iter()
        .any(|r| r.format == row.format && r.status == "running")
}

async fn execute(
    state: &Arc<AppState>,
    runner: &Runner,
    data: &Path,
    ytdlp: &Path,
    row: &DownloadRow,
    title: Option<String>,
) -> Outcome {
    let db = &state.db;
    // Enqueueing filters these out; a row put in by hand must not reach yt-dlp either.
    if !ytdlp_args::is_valid_video_id(&row.video_id) {
        return fail(db, row, title, "not a valid YouTube video id".into());
    }
    // A cancel or remove that landed between the claim and here: nothing to start.
    if runner.cancel_requested.load(Ordering::SeqCst) {
        return cancelled_run(db, row, title);
    }
    let dest = PathBuf::from(&row.dest_dir);
    if let Err(e) = std::fs::create_dir_all(&dest) {
        return fail(db, row, title, format!("could not create the download folder: {e}"));
    }

    // Lives until yt-dlp has exited, then deletes its file.
    let cookies = session_cookies(state, data);
    let spec =
        spec_for(row, &dest, tools::ffmpeg_dir(data), cookies.as_ref().map(|c| c.path().into()));
    let mut command = tools::hidden_command(ytdlp);
    command
        .args(ytdlp_args::build_args(&spec))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    tools::ProcessTree::prepare(&mut command);
    // The arguments are not logged: with cookies they name the credential's file.
    tracing::info!(video_id = %row.video_id, format = %row.format, "downloads: starting yt-dlp");
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(e) => return fail(db, row, title, format!("could not start yt-dlp: {e}")),
    };
    // yt-dlp with what it starts (PyInstaller's child, ffmpeg). Dropped at the end of the run,
    // which on Windows also kills anything of it still alive.
    let tree = tools::ProcessTree::attach(&child);

    // Drained on its own: a child that fills an unread stderr pipe blocks forever.
    let stderr = child.stderr.take();
    let stderr_task = tokio::spawn(async move {
        let mut collected = Vec::new();
        if let Some(stderr) = stderr {
            let _ = stderr.take(STDERR_CAP as u64).read_to_end(&mut collected).await;
        }
        String::from_utf8_lossy(&collected).into_owned()
    });

    let cancelled_signal = runner.cancel.notified();
    tokio::pin!(cancelled_signal);
    cancelled_signal.as_mut().enable();
    let mut cancelled = runner.cancel_requested.load(Ordering::SeqCst);
    let mut final_path: Option<PathBuf> = None;
    let mut last_emit = Instant::now();

    if let (Some(stdout), false) = (child.stdout.take(), cancelled) {
        let mut lines = BufReader::new(stdout).lines();
        loop {
            tokio::select! {
                line = lines.next_line() => match line {
                    Ok(Some(line)) => {
                        if let Some(update) = ytdlp_args::parse_progress_line(&line) {
                            let progress = Progress {
                                video_id: row.video_id.clone(),
                                format: row.format.clone(),
                                percent: update.percent,
                                speed: update.speed,
                                eta: update.eta,
                            };
                            runner.set_active(Some(progress.clone()));
                            if last_emit.elapsed() >= PROGRESS_EVERY || update.percent >= 100.0 {
                                last_emit = Instant::now();
                                let _ = state.app.emit("download-progress", Some(progress));
                            }
                        } else if let Some(path) = ytdlp_args::parse_final_path_line(&line) {
                            final_path = Some(path);
                        }
                    }
                    // End of the stream or an unreadable pipe: go wait for the exit.
                    Ok(None) | Err(_) => break,
                },
                _ = &mut cancelled_signal => {
                    cancelled = true;
                    break;
                }
            }
        }
    }
    // Post-processing runs after the last output line, and is cancellable too.
    let waited = if cancelled {
        None
    } else {
        tokio::select! {
            status = child.wait() => Some(status),
            _ = &mut cancelled_signal => None,
        }
    };
    let status = match waited {
        Some(status) => status,
        None => {
            cancelled = true;
            tree.kill(&mut child);
            child.wait().await
        }
    };
    // yt-dlp has exited: whatever of it is left goes now (Windows), rather than holding the
    // stderr pipe open.
    drop(tree);
    let stderr_text = stderr_task.await.unwrap_or_default();
    drop(cookies);

    if cancelled {
        return cancelled_run(db, row, title);
    }

    match status {
        Ok(status) if status.success() => match resolve_output(row, &dest, final_path) {
            Some(path) => {
                let size = std::fs::metadata(&path).ok().map(|m| m.len() as i64);
                let container = ytdlp_args::container_of(&path);
                db.mark_download_available(
                    &row.video_id,
                    &row.format,
                    &path.to_string_lossy(),
                    size,
                    container.as_deref(),
                    now_secs(),
                );
                Outcome {
                    video_id: row.video_id.clone(),
                    format: row.format.clone(),
                    status: "available",
                    title,
                    error: None,
                }
            }
            None => fail(db, row, title, "yt-dlp reported success but left no file".into()),
        },
        Ok(status) => {
            let message = ytdlp_args::extract_error_message(&stderr_text)
                .unwrap_or_else(|| format!("yt-dlp exited with {status}"));
            fail(db, row, title, message)
        }
        Err(e) => fail(db, row, title, format!("yt-dlp could not be waited on: {e}")),
    }
}

/// The session's cookies as a file for this run, when the setting asks for them and someone is
/// signed in. A file that cannot be written runs anonymous rather than failing the download.
fn session_cookies(state: &AppState, data: &Path) -> Option<CookieFile> {
    if settings::cookies(&state.db) != CookiesMode::Session || !state.it.is_logged_in() {
        return None;
    }
    let header = state.it.cookie().filter(|c| !c.trim().is_empty())?;
    match CookieFile::write(&data.join("tmp"), &header, now_secs()) {
        Ok(file) => file,
        Err(e) => {
            tracing::warn!(kind = ?e.kind(), "downloads: no cookies file, running anonymous");
            None
        }
    }
}

/// Where the finished file is: the printed path when it exists, else the `[id]` marker scan of
/// the folder (post-processing sometimes renames past what `after_move` printed).
fn resolve_output(row: &DownloadRow, dest: &Path, printed: Option<PathBuf>) -> Option<PathBuf> {
    if let Some(path) = printed.filter(|p| p.is_file()) {
        return Some(path);
    }
    let format = Format::parse(&row.format)?;
    verify::find_by_marker(dest, &row.video_id, format, None, &[])
}

/// A cancelled run drops its row (the `.part` stays for a later resume).
fn cancelled_run(db: &crate::db::Db, row: &DownloadRow, title: Option<String>) -> Outcome {
    db.delete_download(&row.video_id, &row.format);
    Outcome {
        video_id: row.video_id.clone(),
        format: row.format.clone(),
        status: "cancelled",
        title,
        error: None,
    }
}

fn fail(db: &crate::db::Db, row: &DownloadRow, title: Option<String>, error: String) -> Outcome {
    db.mark_download_error(&row.video_id, &row.format, &error);
    Outcome {
        video_id: row.video_id.clone(),
        format: row.format.clone(),
        status: "error",
        title,
        error: Some(error),
    }
}

/// A row back into the [`ArgsSpec`] it asks for. The row's own quality and thumbnail mode win
/// over today's settings (a download runs as it was queued); the defaults only stand in for a
/// value that does not parse.
fn spec_for(
    row: &DownloadRow,
    dest: &Path,
    ffmpeg_dir: Option<PathBuf>,
    cookies_file: Option<PathBuf>,
) -> ArgsSpec {
    ArgsSpec {
        video_id: row.video_id.clone(),
        format: Format::parse(&row.format).unwrap_or(settings::DEFAULT_FORMAT),
        audio_quality: AudioQuality::parse(&row.requested_quality)
            .unwrap_or(settings::DEFAULT_AUDIO_QUALITY),
        video_quality: VideoQuality::parse(&row.requested_quality)
            .unwrap_or(settings::DEFAULT_VIDEO_QUALITY),
        thumbnail_mode: ThumbnailMode::parse(&row.thumbnail_mode)
            .unwrap_or(settings::DEFAULT_THUMBNAIL_MODE),
        dest_dir: dest.to_path_buf(),
        ffmpeg_dir,
        cookies_file,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(quality: &str, thumbnail_mode: &str, format: &str, dest: &Path) -> DownloadRow {
        DownloadRow {
            video_id: "vid1".to_string(),
            format: format.to_string(),
            status: "running".to_string(),
            requested_quality: quality.to_string(),
            thumbnail_mode: thumbnail_mode.to_string(),
            dest_dir: dest.to_string_lossy().into_owned(),
            file_path: None,
            file_size_bytes: None,
            container: None,
            error: None,
            attempts: 1,
            created_at: "2026-07-15T12:00:00Z".to_string(),
            completed_at: None,
            last_verified_at: None,
        }
    }

    #[test]
    fn a_rows_own_quality_wins_over_todays_defaults() {
        let dir = PathBuf::from("/forge-test/dl");
        let spec = spec_for(&row("mp3_192", "none", "audio", &dir), &dir, None, None);
        assert_eq!(spec.format, Format::Audio);
        assert_eq!(spec.audio_quality, AudioQuality::Mp3_192);
        assert_eq!(spec.thumbnail_mode, ThumbnailMode::None);
        assert_eq!(spec.dest_dir, dir);
        assert_eq!(spec.cookies_file, None);
    }

    #[test]
    fn a_video_rows_quality_lands_on_the_video_side() {
        let dir = PathBuf::from("/forge-test/dl");
        let spec = spec_for(&row("720p", "embed", "video", &dir), &dir, None, None);
        assert_eq!(spec.video_quality, VideoQuality::P720);
        assert_eq!(ytdlp_args::format_selector(&spec), "bv*[height<=720]+ba/b[height<=720]");
    }

    #[test]
    fn a_hand_edited_row_degrades_to_the_defaults_instead_of_failing() {
        let dir = PathBuf::from("/forge-test/dl");
        let spec = spec_for(&row("mp3_128", "chartreuse", "audio", &dir), &dir, None, None);
        assert_eq!(spec.audio_quality, settings::DEFAULT_AUDIO_QUALITY);
        assert_eq!(spec.thumbnail_mode, settings::DEFAULT_THUMBNAIL_MODE);
    }

    #[test]
    fn the_cookies_file_and_ffmpeg_reach_the_arguments() {
        let dir = PathBuf::from("/forge-test/dl");
        let cookies = PathBuf::from("/forge-test/tmp/cookies-0123456789abcdef.txt");
        let ffmpeg = PathBuf::from("/forge-test/bin");
        let r = row("best", "embed", "audio", &dir);
        let spec = spec_for(&r, &dir, Some(ffmpeg.clone()), Some(cookies.clone()));
        let args = ytdlp_args::build_args(&spec);
        let after = |flag: &str| args.iter().position(|a| a == flag).map(|i| args[i + 1].clone());
        assert_eq!(after("--cookies"), Some(cookies.to_string_lossy().into_owned()));
        assert_eq!(after("--ffmpeg-location"), Some(ffmpeg.to_string_lossy().into_owned()));
    }

    #[test]
    fn resolve_output_prefers_the_printed_path_when_it_exists() {
        let dir = tempfile::tempdir().unwrap();
        let printed = dir.path().join("Song [vid1].mp3");
        std::fs::write(&printed, b"audio").unwrap();
        let r = row("mp3_320", "embed", "audio", dir.path());
        assert_eq!(resolve_output(&r, dir.path(), Some(printed.clone())), Some(printed));
    }

    #[test]
    fn resolve_output_falls_back_to_the_marker_scan_after_a_rename() {
        let dir = tempfile::tempdir().unwrap();
        let printed = dir.path().join("Song [vid1].webm");
        let real = dir.path().join("Song [vid1].mp3");
        std::fs::write(&real, b"audio").unwrap();
        let r = row("mp3_320", "embed", "audio", dir.path());
        assert_eq!(resolve_output(&r, dir.path(), Some(printed)), Some(real));
    }

    #[test]
    fn resolve_output_reports_nothing_for_an_empty_folder() {
        let dir = tempfile::tempdir().unwrap();
        let r = row("mp3_320", "embed", "audio", dir.path());
        assert_eq!(resolve_output(&r, dir.path(), None), None);
    }

    #[test]
    fn cancelling_needs_a_running_download_and_installs_go_one_at_a_time() {
        let runner = Runner::new();
        assert!(!runner.cancel_active(), "nothing runs");
        assert!(!runner.cancel_requested.load(Ordering::SeqCst));
        runner.set_active(Some(Progress {
            video_id: "vid1".into(),
            format: "audio".into(),
            percent: 10.0,
            speed: None,
            eta: None,
        }));
        assert!(runner.is_active("vid1", "audio"));
        assert!(!runner.is_active("vid1", "video"));
        assert!(runner.cancel_active());
        assert!(runner.cancel_requested.load(Ordering::SeqCst));

        let first = runner.begin_install();
        assert!(first.is_some());
        assert!(runner.begin_install().is_none(), "busy while one runs");
        drop(first);
        assert!(runner.begin_install().is_some(), "free again once it ends");
    }
}
