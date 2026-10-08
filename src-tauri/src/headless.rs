//! `limusic-forge --monitor [--all]`: the playlist check with no window, for the Windows scheduled
//! task (wintask.rs). Port of PlaylistForge's `playlist-forge monitor --all` (`pf-app/src/cli.rs`).
//!
//! ## Two ways in
//!
//! - **The app is open (D26).** The second process starts as usual and the single-instance plugin
//!   hands its arguments to the running one, then exits 0. The running app's callback (lib.rs)
//!   sees `--monitor` ([`forwarded`]) and runs the check there ([`delegated_monitor`]), without
//!   showing its window.
//! - **It is not.** `run()` sends `--monitor` here before anything else ([`parse_args`], [`run`]).
//!   A Tauri app with no window at all: the database, an InnerTube session, the Data API's account
//!   manager, and an [`AppState`] built for the occasion, because the sync core
//!   (`commands::sync_all`) and the jobs' InnerTube engine work on one. Its player is never asked
//!   to play and no webview is ever made. The single-instance plugin is registered here too, so a
//!   launch of the app meanwhile is held, and replayed once the check ends ([`finish`]).
//!
//! ## The run
//!
//! 1. `runner_lock` (jobs/lock.rs), renewed every 30 s: busy means another process runs the queue
//!    (`monitor_runs` gets `lock_busy`, exit 2).
//! 2. Signed in: `commands::sync_all(.., "headless")`, which writes `monitor_runs` with trigger
//!    `headless`, backs up the snapshots, and reads through the Data API when it is set up and the
//!    day's backup reserve allows it. Signed out: a `failed` run (`signed_out`), exit 2.
//! 3. `jobs.advance_jobs_headless` (default on, D27): the queue, both engines, until nothing is
//!    eligible. InnerTube items go through the shared pacer; YouTube's pushback (a bot check)
//!    starts the shared cooldown, after which every InnerTube item waits for it, so none is tried
//!    again before the next run. A transient failure backs off past the end of this run too.
//! 4. A toast when the check found new alerts (notify.rs).
//!
//! Requests run without session healing: the healer re-mints a session through the login webview,
//! which a headless run does not have, so a dead session fails fast instead of waiting for one.
//!
//! Exit code ([`cli_exit_code`]): 0 every playlist read, 2 partial / lock busy / signed out, 3
//! failed. The log goes to `<data>\logs\monitor.log.<YYYY-MM-DD>` synchronously (every exit path
//! ends in `process::exit`, which would drop a buffered line). Messages and toasts follow the
//! `locale` setting through a small en/es table (D33), since the UI's translations live in the
//! webview this process never opens.

// The dispatch is not built on macOS (lib.rs): a window from tauri.macos.conf.json would show.
#![cfg_attr(target_os = "macos", allow(dead_code))]

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::Utc;
use innertube::{Clients, InnerTube, Locale, Session};
use serde_json::{json, Value};
use tauri::{AppHandle, Manager};

use crate::db::Db;
use crate::jobs::exec_innertube::InnerTubeExecutor;
use crate::jobs::exec_ytdata::YtDataExecutor;
use crate::jobs::runner::{self, RunSummary, RunnerDeps, RunnerHooks};
use crate::jobs::{lock, Job, JobsState};
use crate::playlist_tools::monitor::SyncSummary;
use crate::state::AppState;

pub const EXIT_OK: i32 = 0;
pub const EXIT_PARTIAL: i32 = 2;
pub const EXIT_FAILED: i32 = 3;

/// The `runner_lock` heartbeat, well inside `lock::STALE_AFTER_SECS`.
const HEARTBEAT_EVERY: Duration = Duration::from_secs(30);
/// Steps of the queue in one run, so a pathological queue cannot keep the process alive forever.
const MAX_JOB_STEPS: usize = 10_000;

/// What `--monitor` was given. `--all` is what the scheduled task passes; every connected account's
/// playlists are checked either way, as PlaylistForge's bare `monitor` did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MonitorArgs {
    pub all: bool,
}

/// `Some` when the command line (`argv[0]` first) asks for a headless check.
pub fn parse_args<I, S>(args: I) -> Option<MonitorArgs>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let rest: Vec<S> = args.into_iter().skip(1).collect();
    let has = |flag: &str| rest.iter().any(|a| a.as_ref() == flag);
    has("--monitor").then(|| MonitorArgs { all: has("--all") })
}

/// What a second launch handed over through the single-instance plugin asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Forwarded {
    /// The scheduled task: run the check, keep the window as it is.
    Monitor,
    /// Anything else: the user opening the app (perhaps with a link).
    Open,
}

/// Decide from the forwarded `argv` (`argv[0]` first, as the plugin passes it).
pub fn forwarded(argv: &[String]) -> Forwarded {
    if argv.iter().skip(1).any(|a| a == "--monitor") {
        Forwarded::Monitor
    } else {
        Forwarded::Open
    }
}

/// How a headless check went, for its exit code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunOutcome {
    /// Every playlist read to the end.
    Ok,
    /// Some were, some were not.
    Partial,
    /// Another process holds `runner_lock`.
    LockBusy,
    /// No YouTube Music session to check with.
    NotSignedIn,
    /// Nothing could be read, or the run could not start.
    Failed,
}

/// The exit code: 0 ok, 2 partial or skipped (busy, signed out), 3 failed.
pub fn cli_exit_code(outcome: RunOutcome) -> i32 {
    match outcome {
        RunOutcome::Ok => EXIT_OK,
        RunOutcome::Partial | RunOutcome::LockBusy | RunOutcome::NotSignedIn => EXIT_PARTIAL,
        RunOutcome::Failed => EXIT_FAILED,
    }
}

/// The outcome of `commands::sync_all`'s answer: its summary's own `ok`/`partial`/`failed`, and
/// `busy` (the monitor already running in this process) as a busy lock.
pub fn outcome_of(sync: &Result<SyncSummary, String>) -> RunOutcome {
    match sync {
        Ok(summary) => match summary.outcome() {
            "ok" => RunOutcome::Ok,
            "partial" => RunOutcome::Partial,
            _ => RunOutcome::Failed,
        },
        Err(e) if e == "busy" => RunOutcome::LockBusy,
        Err(_) => RunOutcome::Failed,
    }
}

// --- messages (D33) ------------------------------------------------------------------------------

/// The language of the headless messages: Spanish for any `es*` locale, English otherwise (the
/// UI's other languages fall back to English for these few lines).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lang {
    En,
    Es,
}

impl Lang {
    pub fn from_locale(locale: Option<&str>) -> Self {
        match locale {
            Some(l) if l.trim().to_ascii_lowercase().starts_with("es") => Lang::Es,
            _ => Lang::En,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Msg {
    Started,
    Delegated,
    NotSignedIn,
    LockBusy,
    StartFailed,
    SyncDone,
    SyncFailed,
    JobsDone,
    JobsOff,
    Exit,
    ToastTitle,
    ToastBody,
}

impl Msg {
    #[cfg(test)]
    const ALL: [Msg; 12] = [
        Msg::Started,
        Msg::Delegated,
        Msg::NotSignedIn,
        Msg::LockBusy,
        Msg::StartFailed,
        Msg::SyncDone,
        Msg::SyncFailed,
        Msg::JobsDone,
        Msg::JobsOff,
        Msg::Exit,
        Msg::ToastTitle,
        Msg::ToastBody,
    ];

    fn is_problem(self) -> bool {
        matches!(self, Msg::LockBusy | Msg::StartFailed | Msg::SyncFailed)
    }
}

/// The table. `{name}` placeholders, filled by [`fill`].
pub fn text(lang: Lang, msg: Msg) -> &'static str {
    use Lang::{En, Es};
    match (msg, lang) {
        (Msg::Started, En) => "Playlist check started.",
        (Msg::Started, Es) => "Revisión de playlists iniciada.",
        (Msg::Delegated, En) => "Check requested by the scheduled task; running it in the open app.",
        (Msg::Delegated, Es) => {
            "Revisión pedida por la tarea programada; se hace en la app abierta."
        }
        (Msg::NotSignedIn, En) => "Not signed in to YouTube Music: no playlists to check.",
        (Msg::NotSignedIn, Es) => {
            "No hay sesión iniciada en YouTube Music: no hay playlists que revisar."
        }
        (Msg::LockBusy, En) => "Another LiMusic Forge process (pid {pid}) is running the queue; skipped.",
        (Msg::LockBusy, Es) => {
            "Otro proceso de LiMusic Forge (pid {pid}) está procesando la cola; se omite."
        }
        (Msg::StartFailed, En) => "The check could not start: {error}",
        (Msg::StartFailed, Es) => "La revisión no pudo empezar: {error}",
        (Msg::SyncDone, En) => {
            "Check finished ({outcome}): {complete} of {playlists} playlists read, {alerts} new alerts."
        }
        (Msg::SyncDone, Es) => {
            "Revisión terminada ({outcome}): {complete} de {playlists} playlists leídas, {alerts} alertas nuevas."
        }
        (Msg::SyncFailed, En) => "The check failed: {error}",
        (Msg::SyncFailed, Es) => "La revisión falló: {error}",
        (Msg::JobsDone, En) => "Job queue: {items} items processed, {blocked} jobs left waiting.",
        (Msg::JobsDone, Es) => {
            "Cola de trabajos: {items} elementos procesados, {blocked} trabajos en espera."
        }
        (Msg::JobsOff, En) => "Job queue left as it was (turned off in Settings).",
        (Msg::JobsOff, Es) => "La cola de trabajos no se tocó (desactivado en Ajustes).",
        (Msg::Exit, En) => "Exiting with code {code}.",
        (Msg::Exit, Es) => "Saliendo con código {code}.",
        (Msg::ToastTitle, En) => "Changes in your playlists",
        (Msg::ToastTitle, Es) => "Cambios en tus playlists",
        (Msg::ToastBody, En) => "The daily check found {alerts} new alerts.",
        (Msg::ToastBody, Es) => "La revisión diaria encontró {alerts} alertas nuevas.",
    }
}

/// `template` with each `{name}` replaced by its value.
pub fn fill(template: &str, params: &[(&str, String)]) -> String {
    params
        .iter()
        .fold(template.to_owned(), |s, (name, value)| s.replace(&format!("{{{name}}}"), value))
}

/// Log one message in the user's language and answer it.
fn note(lang: Lang, msg: Msg, params: &[(&str, String)]) -> String {
    let line = fill(text(lang, msg), params);
    if msg.is_problem() {
        tracing::warn!(target: "monitor", "{line}");
    } else {
        tracing::info!(target: "monitor", "{line}");
    }
    line
}

fn summary_params(s: &SyncSummary) -> Vec<(&'static str, String)> {
    vec![
        ("outcome", s.outcome().to_owned()),
        ("complete", s.complete.to_string()),
        ("playlists", s.playlists.to_string()),
        ("alerts", s.alerts_new.to_string()),
    ]
}

// --- the app is open (D26) -------------------------------------------------------------------------

/// The running app was handed `--monitor` (lib.rs, single-instance callback): run the check here,
/// as the scheduled task would have, and leave the window alone. The app's own job runner already
/// works through the queue; it is only nudged.
pub fn delegated_monitor(app: &AppHandle) {
    let Some(state) = app.try_state::<Arc<AppState>>().map(|s| s.inner().clone()) else {
        tracing::info!("monitor: requested before the app finished starting; skipped");
        return;
    };
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        let lang = Lang::from_locale(state.db.get_setting("locale").as_deref());
        note(lang, Msg::Delegated, &[]);
        if state.it.is_logged_in() {
            match crate::commands::sync_all(&state, "headless").await {
                Ok(summary) => {
                    note(lang, Msg::SyncDone, &summary_params(&summary));
                    toast(&app, lang, &summary, false);
                }
                Err(e) => {
                    note(lang, Msg::SyncFailed, &[("error", e)]);
                }
            }
        } else {
            note(lang, Msg::NotSignedIn, &[]);
        }
        if let Some(jobs) = app.try_state::<Arc<JobsState>>() {
            jobs.nudge();
        }
    });
}

fn toast(app: &AppHandle, lang: Lang, summary: &SyncSummary, wait: bool) {
    if summary.alerts_new == 0 {
        return;
    }
    let body = fill(text(lang, Msg::ToastBody), &[("alerts", summary.alerts_new.to_string())]);
    crate::notify::monitor_toast(app, text(lang, Msg::ToastTitle), &body, wait);
}

// --- no app open: the headless process -----------------------------------------------------------

/// A launch of the app that arrived while this process held the single instance: replayed by
/// [`finish`], once the instance is released.
static RELAUNCH: Mutex<Option<Vec<String>>> = Mutex::new(None);

/// Run the check with no window and exit with its code. Never returns.
pub fn run(args: MonitorArgs, context: tauri::Context<tauri::Wry>) -> ! {
    let mut builder = tauri::Builder::default();
    // First, as in the app: an open app takes the request (D26) and this process exits 0 here.
    if std::env::var_os("LIMUSIC_MULTI").is_none() {
        builder = builder.plugin(tauri_plugin_single_instance::init(|_app, argv, _cwd| {
            if forwarded(&argv) == Forwarded::Open {
                tracing::info!("the app was opened during a check; it opens when the check ends");
                if let Ok(mut slot) = RELAUNCH.lock() {
                    *slot = Some(argv.into_iter().skip(1).collect());
                }
            }
        }));
    }
    let app = builder
        .setup(move |app| {
            let handle = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                let code = monitor(&handle, args).await;
                let on_main = handle.clone();
                // The plugin's window and mutex belong to the main thread.
                let on_main_thread = move || {
                    finish(&on_main, code);
                };
                if handle.run_on_main_thread(on_main_thread).is_err() {
                    finish(&handle, code);
                }
            });
            Ok(())
        })
        .build(context);
    match app {
        Ok(app) => app.run(|_, _| {}),
        Err(e) => {
            eprintln!("{} --monitor: {e}", crate::brand::APP_NAME);
            std::process::exit(EXIT_FAILED);
        }
    }
    std::process::exit(EXIT_OK)
}

/// Release the single instance, start the app if someone opened it meanwhile, and exit.
fn finish(handle: &AppHandle, code: i32) -> ! {
    tauri_plugin_single_instance::destroy(handle);
    let pending = RELAUNCH.lock().ok().and_then(|mut slot| slot.take());
    if let Some(args) = pending {
        match std::env::current_exe() {
            Ok(exe) => {
                if let Err(e) = std::process::Command::new(exe).args(args).spawn() {
                    tracing::warn!(error = %e, "could not open the app after the check");
                }
            }
            Err(e) => tracing::warn!(error = %e, "could not open the app after the check"),
        }
    }
    handle.cleanup_before_exit();
    std::process::exit(code)
}

/// The whole check; answers the exit code.
async fn monitor(handle: &AppHandle, args: MonitorArgs) -> i32 {
    let started = crate::db::now_secs();
    let data_dir = crate::paths::data_dir(handle);
    let _ = std::fs::create_dir_all(&data_dir);
    let log = init_log(&data_dir);
    let db = match Db::open(&data_dir.join("limusic.sqlite")) {
        Ok(db) => Arc::new(db),
        Err(e) => {
            note(Lang::En, Msg::StartFailed, &[("error", e.to_string())]);
            return exit(Lang::En, RunOutcome::Failed);
        }
    };
    let lang = Lang::from_locale(db.get_setting("locale").as_deref());
    let pid = std::process::id();
    note(lang, Msg::Started, &[]);
    tracing::info!(all = args.all, pid, log = ?log, portable = crate::paths::is_portable(), "headless monitor");

    match lock::try_acquire(&db, pid, Utc::now()) {
        Ok(lock::AcquireOutcome::Acquired) => {}
        Ok(lock::AcquireOutcome::Busy(holder)) => {
            note(lang, Msg::LockBusy, &[("pid", holder.pid.to_string())]);
            record_run(&db, started, "lock_busy", json!({}));
            return exit(lang, RunOutcome::LockBusy);
        }
        Err(e) => {
            note(lang, Msg::StartFailed, &[("error", e.to_string())]);
            record_run(&db, started, "failed", json!({ "error": e.to_string() }));
            return exit(lang, RunOutcome::Failed);
        }
    }
    let lost = Arc::new(AtomicBool::new(false));
    let beat = heartbeat(db.clone(), pid, lost.clone());
    let outcome = check(handle, &db, &data_dir, lang, &lost, started).await;
    beat.abort();
    if let Err(e) = lock::release(&db, pid) {
        tracing::warn!(error = %e, "could not release the runner lock");
    }
    exit(lang, outcome)
}

fn exit(lang: Lang, outcome: RunOutcome) -> i32 {
    let code = cli_exit_code(outcome);
    note(lang, Msg::Exit, &[("code", code.to_string())]);
    code
}

/// With the lock held: the sync, then the queue, then the toast.
async fn check(
    handle: &AppHandle,
    db: &Arc<Db>,
    data_dir: &Path,
    lang: Lang,
    lost: &Arc<AtomicBool>,
    started: i64,
) -> RunOutcome {
    let state = match build_state(handle, db.clone(), data_dir) {
        Ok(state) => state,
        Err(e) => {
            note(lang, Msg::StartFailed, &[("error", e.clone())]);
            record_run(db, started, "failed", json!({ "error": e }));
            return RunOutcome::Failed;
        }
    };
    // `ytdata_sync` and the journal look these up on the handle, as in the app.
    handle.manage(state.clone());
    let jobs = jobs_state(handle, db, data_dir);
    handle.manage(jobs.clone());

    let outcome = if state.it.is_logged_in() {
        // As the app does on first run: a session without visitorData gets one first.
        if state.it.visitor_data().is_none() {
            if let Ok(vd) = state.it.fetch_visitor_data().await {
                state.it.set_visitor_data(Some(vd.clone()));
                db.set_setting("visitor_data", &vd);
            }
        }
        let sync = innertube::without_healing(crate::commands::sync_all(&state, "headless")).await;
        match &sync {
            Ok(summary) => {
                note(lang, Msg::SyncDone, &summary_params(summary));
            }
            Err(e) => {
                note(lang, Msg::SyncFailed, &[("error", e.clone())]);
            }
        }
        if let Ok(summary) = &sync {
            let (h, summary) = (handle.clone(), summary.clone());
            let shown =
                tauri::async_runtime::spawn_blocking(move || toast(&h, lang, &summary, true));
            let _ = shown.await;
        }
        outcome_of(&sync)
    } else {
        note(lang, Msg::NotSignedIn, &[]);
        record_run(db, started, "failed", json!({ "error": "signed_out" }));
        RunOutcome::NotSignedIn
    };
    // A response that rotated the cookie: keep it, or the next launch starts from a stale jar.
    state.persist_rotated_cookie();

    if !advance_jobs(db) {
        note(lang, Msg::JobsOff, &[]);
    } else if !lost.load(Ordering::Relaxed) {
        let done = innertube::without_healing(advance_queue(&state, &jobs, lost)).await;
        let params =
            [("items", done.items_progressed.to_string()), ("blocked", done.blocked.to_string())];
        note(lang, Msg::JobsDone, &params);
        state.persist_rotated_cookie();
    }
    outcome
}

/// `jobs.advance_jobs_headless`, on unless set to `false`.
fn advance_jobs(db: &Db) -> bool {
    db.get_setting(crate::jobs::engine::ADVANCE_JOBS_HEADLESS_KEY).as_deref() != Some("false")
}

/// The runner's `finished` hook without a UI to tell: the journal entry for the undo history.
struct HeadlessHooks {
    state: Arc<AppState>,
}

impl RunnerHooks for HeadlessHooks {
    fn finished(&self, job: &Job) {
        crate::jobs::control::on_job_finished(&self.state, job);
    }
}

/// The queue, both engines, until nothing is eligible now (D27). Nothing here sleeps: a job that
/// went waiting (quota, the user, a cooldown, a back-off) stays so until a later run.
async fn advance_queue(
    state: &Arc<AppState>,
    jobs: &Arc<JobsState>,
    lost: &Arc<AtomicBool>,
) -> RunSummary {
    let hooks: Arc<dyn RunnerHooks> = Arc::new(HeadlessHooks { state: state.clone() });
    let deps = RunnerDeps {
        db: state.db.clone(),
        ytdata: YtDataExecutor::new(state.db.clone(), jobs.accounts()),
        innertube: InnerTubeExecutor::new(state.clone()),
        hooks,
    };
    runner::recover_on_start(&deps, Utc::now()).await;
    let lost = lost.clone();
    runner::run_until_blocked(&deps, Utc::now, MAX_JOB_STEPS, move || lost.load(Ordering::Relaxed))
        .await
}

/// Renews the lock; flags `lost` if another process took it over.
fn heartbeat(db: Arc<Db>, pid: u32, lost: Arc<AtomicBool>) -> tauri::async_runtime::JoinHandle<()> {
    tauri::async_runtime::spawn(async move {
        let mut tick = tokio::time::interval(HEARTBEAT_EVERY);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        tick.tick().await;
        loop {
            tick.tick().await;
            match lock::renew_heartbeat(&db, pid, Utc::now()) {
                Ok(true) => {}
                Ok(false) => {
                    lost.store(true, Ordering::Relaxed);
                    break;
                }
                Err(e) => tracing::warn!(error = %e, "could not renew the runner lock"),
            }
        }
    })
}

/// A `monitor_runs` row for a check that never got to `sync_all` (which writes its own).
fn record_run(db: &Db, started: i64, outcome: &str, detail: Value) {
    let run = crate::db::MonitorRun {
        id: 0,
        started_at: started,
        finished_at: crate::db::now_secs(),
        trigger: "headless".into(),
        outcome: outcome.into(),
        playlists_ok: 0,
        playlists_failed: 0,
        alerts_new: 0,
        units_spent: 0,
        detail_json: detail.to_string(),
    };
    if let Err(e) = db.record_monitor_run(&run) {
        tracing::warn!(error = %e, "could not log the monitor run");
    }
}

/// `monitor.log.<YYYY-MM-DD>`, local date: one file per day, appended by each run that day.
pub fn log_file_name(date: chrono::NaiveDate) -> String {
    format!("monitor.log.{}", date.format("%Y-%m-%d"))
}

/// Logging for this process: `<data>\logs\monitor.log.<date>`, written synchronously. Answers the
/// file, `None` when it could not be opened (the check runs anyway).
fn init_log(data_dir: &Path) -> Option<PathBuf> {
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;
    use tracing_subscriber::Layer;

    let dir = data_dir.join("logs");
    std::fs::create_dir_all(&dir).ok()?;
    let path = dir.join(log_file_name(chrono::Local::now().date_naive()));
    let file = std::fs::OpenOptions::new().create(true).append(true).open(&path).ok()?;
    let filter =
        tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into());
    let layer = tracing_subscriber::fmt::layer()
        .with_ansi(false)
        .with_writer(Mutex::new(file))
        .with_filter(filter);
    tracing_subscriber::registry().with(layer).try_init().ok()?;
    Some(path)
}

/// The Data API's account manager, as the app's setup makes it (lib.rs).
fn jobs_state(handle: &AppHandle, db: &Arc<Db>, data_dir: &Path) -> Arc<JobsState> {
    let jobs = JobsState::new();
    let store = crate::ytdata_secrets::token_store(handle);
    let repo = Arc::new(crate::ytdata_accounts::DbAccountsRepo::new(db.clone()));
    match ytdata::auth::accounts::AccountManager::new(store, repo) {
        Ok(manager) => {
            if let Some(secret) = ytdata::client_secret::load_existing(data_dir) {
                manager.set_client_secret(secret);
            }
            jobs.set_account_manager(Some(Arc::new(manager)));
        }
        Err(e) => tracing::warn!(error = %e, "headless: no Data API account manager"),
    }
    jobs
}

/// An [`AppState`] for a process with no window, from the same settings the app's setup reads:
/// the proxy, the session, the language, hidden videos and blocked artists (which shape what a
/// playlist read returns, so a headless read must match the app's). The player is made because
/// `AppState` holds one; nothing plays. No media controls, Discord or tray.
fn build_state(handle: &AppHandle, db: Arc<Db>, data_dir: &Path) -> Result<Arc<AppState>, String> {
    use crate::cipher::{CipherDeobfuscator, PlayerConfigStore};

    let proxy = std::env::var("LIMUSIC_PROXY")
        .ok()
        .filter(|p| !p.trim().is_empty())
        .or_else(|| db.get_setting("proxy").filter(|p| !p.trim().is_empty()))
        .filter(|p| reqwest::Proxy::all(p.as_str()).is_ok());
    crate::http::set_proxy(proxy.as_deref());
    let session = Session {
        locale: Locale::default(),
        visitor_data: db.get_setting("visitor_data").filter(|s| !s.is_empty()),
        data_sync_id: crate::state::persisted_data_sync_id(&db),
        cookie: db.get_setting("session_cookie").filter(|s| !s.is_empty()),
    };
    let it = InnerTube::new(session, proxy.as_deref()).map_err(|e| e.to_string())?;
    if let Some(hl) = db.get_setting("locale") {
        it.set_locale(&hl);
    }
    it.set_hide_videos(db.get_setting("hide_videos").as_deref() == Some("true"));
    it.set_blocked(crate::blocked::block_list(&db));
    let clients = Clients::bundled();

    let cache_dir = data_dir.join("audio-cache");
    let player = player::Player::new(&cache_dir.to_string_lossy()).map_err(|e| e.to_string())?;
    let config = Arc::new(PlayerConfigStore::new(data_dir));
    let cipher = Arc::new(CipherDeobfuscator::new(handle.clone(), data_dir, config));
    let potoken = Arc::new(crate::potoken::PoTokenGenerator::new(db.clone()));
    let orchestrator = Arc::new(crate::orchestrator::Orchestrator::new(
        it.clone(),
        clients.clone(),
        cipher,
        potoken,
    ));
    // Headless never scrobbles: no session key, so the config is never read.
    let lastfm = crate::lastfm::spawn(None, crate::lastfm::ScrobbleConfig::default());
    let lt_url = db.get_setting("lt_server_url").unwrap_or_default();
    let (lt, _sync) = crate::listentogether::LtSession::new(handle.clone(), lt_url);
    Ok(Arc::new(AppState::new(
        it,
        clients,
        player,
        db,
        handle.clone(),
        orchestrator,
        lt,
        cache_dir,
        None,
        None,
        lastfm,
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn summary(playlists: u32, complete: u32) -> SyncSummary {
        SyncSummary { playlists, complete, ..SyncSummary::new("headless", 0) }
    }

    #[test]
    fn exit_codes() {
        assert_eq!(cli_exit_code(RunOutcome::Ok), 0);
        assert_eq!(cli_exit_code(RunOutcome::Partial), 2);
        assert_eq!(cli_exit_code(RunOutcome::LockBusy), 2);
        assert_eq!(cli_exit_code(RunOutcome::NotSignedIn), 2);
        assert_eq!(cli_exit_code(RunOutcome::Failed), 3);
    }

    #[test]
    fn the_sync_answer_decides_the_outcome() {
        assert_eq!(outcome_of(&Ok(summary(5, 5))), RunOutcome::Ok);
        assert_eq!(outcome_of(&Ok(summary(0, 0))), RunOutcome::Ok, "no playlists: nothing failed");
        assert_eq!(outcome_of(&Ok(summary(5, 3))), RunOutcome::Partial);
        assert_eq!(outcome_of(&Ok(summary(5, 0))), RunOutcome::Failed);
        assert_eq!(outcome_of(&Err("busy".into())), RunOutcome::LockBusy);
        assert_eq!(outcome_of(&Err("empty_library".into())), RunOutcome::Failed);
        let codes: Vec<i32> = [summary(2, 2), summary(2, 1), summary(2, 0)]
            .into_iter()
            .map(|s| cli_exit_code(outcome_of(&Ok(s))))
            .collect();
        assert_eq!(codes, [0, 2, 3]);
    }

    #[test]
    fn args_ask_for_a_check_only_with_monitor() {
        let parse = |a: &[&str]| parse_args(a.iter().copied());
        assert_eq!(parse(&["forge.exe", "--monitor", "--all"]), Some(MonitorArgs { all: true }));
        assert_eq!(parse(&["forge.exe", "--all", "--monitor"]), Some(MonitorArgs { all: true }));
        assert_eq!(parse(&["forge.exe", "--monitor"]), Some(MonitorArgs { all: false }));
        assert_eq!(parse(&["forge.exe"]), None);
        assert_eq!(parse(&["forge.exe", "--autostart"]), None);
        assert_eq!(parse(&["forge.exe", "https://music.youtube.com/watch?v=x"]), None);
        // argv[0] is never a flag, whatever it is called.
        assert_eq!(parse(&["--monitor"]), None);
        assert_eq!(parse(&["forge.exe", "--monitor=all"]), None);
        // As `run()` gets them: OsStrings.
        let os: Vec<std::ffi::OsString> = vec!["forge.exe".into(), "--monitor".into()];
        assert_eq!(parse_args(os), Some(MonitorArgs { all: false }));
    }

    #[test]
    fn a_forwarded_monitor_is_delegated_and_anything_else_opens() {
        let argv = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let task =
            argv(&[r"C:\Program Files\LiMusic Forge\limusic-forge.exe", "--monitor", "--all"]);
        assert_eq!(forwarded(&task), Forwarded::Monitor);
        assert_eq!(forwarded(&argv(&["forge.exe"])), Forwarded::Open);
        assert_eq!(forwarded(&argv(&["forge.exe", "https://youtu.be/abc"])), Forwarded::Open);
        assert_eq!(forwarded(&argv(&["--monitor"])), Forwarded::Open, "argv[0] only");
        assert_eq!(forwarded(&[]), Forwarded::Open);
    }

    #[test]
    fn locale_picks_the_table() {
        assert_eq!(Lang::from_locale(Some("es")), Lang::Es);
        assert_eq!(Lang::from_locale(Some("es-419")), Lang::Es);
        assert_eq!(Lang::from_locale(Some("ES")), Lang::Es);
        assert_eq!(Lang::from_locale(Some("en")), Lang::En);
        assert_eq!(Lang::from_locale(Some("ko")), Lang::En);
        assert_eq!(Lang::from_locale(None), Lang::En);
    }

    fn placeholders(s: &str) -> Vec<String> {
        let mut out: Vec<String> = s
            .split('{')
            .skip(1)
            .filter_map(|p| p.split_once('}').map(|(name, _)| name.to_owned()))
            .collect();
        out.sort();
        out
    }

    #[test]
    fn every_message_has_both_languages_with_the_same_placeholders() {
        for msg in Msg::ALL {
            let (en, es) = (text(Lang::En, msg), text(Lang::Es, msg));
            assert!(!en.is_empty() && !es.is_empty(), "{msg:?}");
            assert_ne!(en, es, "{msg:?} is not translated");
            assert_eq!(placeholders(en), placeholders(es), "{msg:?}");
        }
    }

    #[test]
    fn fill_replaces_every_placeholder() {
        let s = fill(
            text(Lang::Es, Msg::SyncDone),
            &[
                ("outcome", "ok".into()),
                ("complete", "3".into()),
                ("playlists", "4".into()),
                ("alerts", "2".into()),
            ],
        );
        assert_eq!(s, "Revisión terminada (ok): 3 de 4 playlists leídas, 2 alertas nuevas.");
        assert_eq!(
            fill(text(Lang::En, Msg::Exit), &[("code", "3".into())]),
            "Exiting with code 3."
        );
    }

    #[test]
    fn log_file_is_per_day() {
        let day = chrono::NaiveDate::from_ymd_opt(2026, 7, 5).unwrap();
        assert_eq!(log_file_name(day), "monitor.log.2026-07-05");
    }

    #[test]
    fn the_queue_advances_unless_turned_off() {
        let db = Db::open(Path::new(":memory:")).unwrap();
        assert!(advance_jobs(&db), "on by default");
        db.set_setting(crate::jobs::engine::ADVANCE_JOBS_HEADLESS_KEY, "false");
        assert!(!advance_jobs(&db));
        db.set_setting(crate::jobs::engine::ADVANCE_JOBS_HEADLESS_KEY, "true");
        assert!(advance_jobs(&db));
    }

    #[test]
    fn a_run_that_never_synced_is_logged_as_headless() {
        let db = Db::open(Path::new(":memory:")).unwrap();
        record_run(&db, 100, "lock_busy", json!({}));
        let runs = db.monitor_runs(5);
        assert_eq!(runs.len(), 1);
        assert_eq!((runs[0].trigger.as_str(), runs[0].outcome.as_str()), ("headless", "lock_busy"));
        assert_eq!(runs[0].started_at, 100);
    }
}
