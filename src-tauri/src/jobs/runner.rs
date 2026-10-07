//! The job runner: one item at a time, highest priority first, then oldest. Port of
//! PlaylistForge's `pf-app/src/jobs/runner.rs`, with the writes behind an [`Executor`] so one
//! queue runs both engines (`exec_ytdata`, `exec_innertube`) and the tests run a fake one.
//!
//! ## Invariants
//!
//! - One process runs jobs at a time: the runner holds `runner_lock` (commit 24) and renews its
//!   heartbeat from a task of its own, so a long item never lets the lock go stale.
//! - An item is `in_flight` from just before its call until its outcome is recorded. A Data API
//!   write's quota lands in the same transaction as its item's `done` (`repo::complete_item`), so
//!   the executors' client carries no quota sink (it would count every write twice).
//! - At start, items left `in_flight` by a crash are reconciled by their executor (an insert is
//!   looked for before it is retried: inserts are not idempotent), then `running` jobs go back to
//!   `queued`.
//! - A move runs in three phases: the copies, then the `verify_destination` barrier (appended once
//!   every copy is terminal), then the deletes the barrier appends for copies it confirmed. A copy
//!   that never landed never gets a delete, so a move cannot lose a track.
//! - Failures: `quotaExceeded` (or the budget running out) waits for the next reset
//!   (`waiting_quota`); a rejected or missing token waits for the user (`waiting_auth`); anything
//!   transient backs off (`paused_network`, 2^attempts minutes up to an hour) until an item has
//!   failed [`MAX_ITEM_ATTEMPTS`] times. InnerTube pushback (cooldown, bot check) pauses the job
//!   until the cooldown ends, and is not retried before (D27).
//! - A pause or a cancel that lands while an item is in flight is never overwritten by that
//!   item's outcome.

use std::collections::HashSet;
use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde_json::{json, Value};
use tokio::sync::Notify;

use super::engine::{engine_of, Engine};
use super::exec_innertube::InnerTubeExecutor;
use super::exec_ytdata::{SharedAccounts, YtDataExecutor};
use super::planner::{action_name, cost_for_action_name};
use super::{budget, control, lock, repo};
use super::{Job, JobItem, JobItemStatus, JobKind, JobStatus, NewJobItem};
use crate::db::Db;
use crate::quota;
use crate::state::AppState;

/// Attempts before an item that keeps failing transiently is marked `failed`.
pub const MAX_ITEM_ATTEMPTS: i64 = 5;
/// The heartbeat's period: well inside `lock::STALE_AFTER_SECS`.
const HEARTBEAT_EVERY: Duration = Duration::from_secs(30);
/// How long to wait before asking for a lock another process holds.
const LOCK_BUSY_RETRY: Duration = Duration::from_secs(60);
/// The longest the runner sleeps with nothing to do; a new job nudges it awake anyway.
const IDLE_POLL: Duration = Duration::from_secs(300);
/// Steps in one wake, so a pathological queue can't spin forever.
const MAX_STEPS_PER_WAKE: usize = 10_000;

/// What one item's execution came to. The runner turns it into the state change.
#[derive(Debug, Clone, PartialEq)]
pub enum ItemOutcome {
    /// The write landed. `units` and `endpoint` are billed with the item, in its transaction.
    Done { api_result: Value, inverse: Option<Value>, units: i64, endpoint: Option<&'static str> },
    /// `verify_destination`'s answer: the videos in the destination now, and the read's cost.
    Verified { present: HashSet<String>, units: i64 },
    /// This item can never succeed (the video or row is gone); the job goes on.
    Skipped(String),
    /// This item failed for good; the job goes on.
    Failed(String),
    /// The whole job can't go on (a playlist not in manual order, the destination gone).
    FailJob(String),
    /// Transient (network, 5xx, rate limit): back off, up to [`MAX_ITEM_ATTEMPTS`].
    Retry(String),
    /// Google says the day's quota is gone.
    QuotaExceeded(String),
    /// The account needs the user: reconnect, sign in, switch back, enable the API.
    NeedsAuth(String),
    /// YouTube pushed back (InnerTube): nothing goes out before `until`.
    Cooldown { until: DateTime<Utc>, reason: String },
}

/// What the move barrier decided.
#[derive(Debug, Clone, PartialEq)]
pub enum Barrier {
    /// These copies were not found: they run again, and so does the barrier after them.
    Missing(Vec<i64>),
    /// Everything done is confirmed: these phase-3 deletes go in.
    Proceed(Vec<NewJobItem>),
}

/// How an item left `in_flight` by a crash is settled at start.
#[derive(Debug, Clone, PartialEq)]
pub enum Reconcile {
    /// Safe to run again.
    Requeue(String),
    /// Run again, with these params (made safe to repeat).
    RequeueWith { params: Value, reason: String },
    /// It had landed: record it.
    Done { api_result: Value, inverse: Option<Value>, units: i64, endpoint: Option<&'static str> },
    /// Can't tell now (no network): left in flight, its job backs off.
    Leave(String),
    /// Can't tell without the user (no token): left in flight, its job waits for them.
    LeaveAuth(String),
}

/// One engine's writes. `execute` makes the item's call and says how it went; it never touches
/// the item's or the job's state, which is the runner's.
pub trait Executor: Send + Sync {
    fn execute(&self, job: &Job, item: &JobItem) -> impl Future<Output = ItemOutcome> + Send;

    /// Settles an item a crash left `in_flight`.
    fn reconcile(&self, job: &Job, item: &JobItem) -> impl Future<Output = Reconcile> + Send;

    /// The move barrier: given the copies (phase 1) and what the destination holds now.
    fn after_verify(&self, job: &Job, phase1: &[JobItem], present: &HashSet<String>) -> Barrier;

    /// Called once an item is recorded done (the local echo). Must never fail the job.
    fn after_done(&self, _job: &Job, _item: &JobItem, _api_result: &Value) {}
}

/// What the runner tells the rest of the app.
pub trait RunnerHooks: Send + Sync {
    /// A job changed (`jobs-changed`). `0`: possibly any.
    fn changed(&self, _job_id: i64) {}
    /// Units were spent (`quota-changed`).
    fn quota_spent(&self) {}
    /// A job ended (completed, with errors, failed): the journal entry, the playlists to re-read.
    fn finished(&self, _job: &Job) {}
}

pub struct NoHooks;
impl RunnerHooks for NoHooks {}

pub struct RunnerDeps<Y, I> {
    pub db: Arc<Db>,
    pub ytdata: Y,
    pub innertube: I,
    pub hooks: Arc<dyn RunnerHooks>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StepOutcome {
    /// Nothing eligible to run now.
    Idle,
    /// An item finished (done, skipped or failed for good).
    Progressed {
        job_id: i64,
        item_id: i64,
    },
    /// A job moved on without running an item (a phase boundary, its end, a reconcile).
    Advanced {
        job_id: i64,
    },
    WaitingQuota {
        job_id: i64,
    },
    WaitingAuth {
        job_id: i64,
    },
    PausedNetwork {
        job_id: i64,
    },
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RunSummary {
    pub steps: usize,
    pub items_progressed: usize,
    /// Jobs that went to a waiting state on the way.
    pub blocked: usize,
    pub idle: bool,
}

/// Job-level backoff for `paused_network`: 2^attempts minutes, capped at an hour.
pub fn backoff_for_attempt(attempts: i64) -> chrono::Duration {
    chrono::Duration::minutes(2i64.saturating_pow(attempts.clamp(0, 6) as u32).min(60))
}

/// Run once, before the first step, with the lock held: reconcile what a crash left in flight,
/// then put `running` jobs back in the queue.
pub async fn recover_on_start<Y: Executor, I: Executor>(
    deps: &RunnerDeps<Y, I>,
    now: DateTime<Utc>,
) {
    match repo::list_in_flight_items(&deps.db) {
        Ok(items) => {
            for item in items {
                let Ok(Some(job)) = repo::get_job(&deps.db, item.job_id) else {
                    let _ = repo::requeue_item(&deps.db, item.id, Some("recovered"), now);
                    continue;
                };
                match engine_of(&job) {
                    Engine::Ytdata => reconcile_with(deps, &deps.ytdata, &job, &item, now).await,
                    Engine::Innertube => {
                        reconcile_with(deps, &deps.innertube, &job, &item, now).await
                    }
                }
            }
        }
        Err(e) => tracing::warn!(error = %e, "jobs: could not list in-flight items"),
    }
    match repo::requeue_running_jobs(&deps.db) {
        Ok(n) if n > 0 => tracing::info!(jobs = n, "jobs: interrupted jobs back in the queue"),
        Ok(_) => {}
        Err(e) => tracing::warn!(error = %e, "jobs: could not requeue interrupted jobs"),
    }
}

async fn reconcile_with<Y: Executor, I: Executor, E: Executor>(
    deps: &RunnerDeps<Y, I>,
    exec: &E,
    job: &Job,
    item: &JobItem,
    now: DateTime<Utc>,
) {
    let db = &deps.db;
    match exec.reconcile(job, item).await {
        Reconcile::Requeue(reason) => {
            let _ = repo::requeue_item(db, item.id, Some(&reason), now);
        }
        Reconcile::RequeueWith { params, reason } => {
            let _ = repo::set_item_params(db, item.id, &params);
            let _ = repo::requeue_item(db, item.id, Some(&reason), now);
        }
        Reconcile::Done { api_result, inverse, units, endpoint } => {
            let done = repo::complete_item(
                db,
                item.id,
                Some(&api_result),
                inverse.as_ref(),
                units,
                endpoint,
                job.account_id.as_deref(),
                now,
            );
            if done.is_ok() {
                exec.after_done(job, item, &api_result);
                if units > 0 {
                    deps.hooks.quota_spent();
                }
            }
            finalize_job_progress(db, deps.hooks.as_ref(), job.id, now);
        }
        Reconcile::Leave(reason) => {
            if still_active(db, job.id) {
                let at = now + backoff_for_attempt(1);
                let _ = repo::set_paused_network(db, job.id, at, &reason);
            }
        }
        Reconcile::LeaveAuth(reason) => {
            if still_active(db, job.id) {
                let _ = repo::set_waiting_auth(db, job.id, &reason);
            }
        }
    }
    deps.hooks.changed(job.id);
}

/// One unit of work: the best eligible job's next item (or its phase boundary).
pub async fn run_next_step<Y: Executor, I: Executor>(
    deps: &RunnerDeps<Y, I>,
    now: DateTime<Utc>,
) -> StepOutcome {
    let db = &deps.db;
    if let Err(e) = repo::promote_ready_jobs(db, now) {
        tracing::warn!(error = %e, "jobs: could not promote waiting jobs");
    }
    match control::materialize_pending_reverts(db, now) {
        Ok(made) => made.into_iter().for_each(|id| deps.hooks.changed(id)),
        Err(e) => tracing::warn!(error = %e, "jobs: could not queue a revert"),
    }
    let job = match repo::next_eligible_job(db) {
        Ok(Some(job)) => job,
        Ok(None) => return StepOutcome::Idle,
        Err(e) => {
            tracing::warn!(error = %e, "jobs: could not pick the next job");
            return StepOutcome::Idle;
        }
    };
    match engine_of(&job) {
        Engine::Ytdata => step(deps, &deps.ytdata, job, now, true).await,
        Engine::Innertube => step(deps, &deps.innertube, job, now, false).await,
    }
}

async fn step<Y: Executor, I: Executor, E: Executor>(
    deps: &RunnerDeps<Y, I>,
    exec: &E,
    job: Job,
    now: DateTime<Utc>,
    metered: bool,
) -> StepOutcome {
    let db = &deps.db;
    let hooks = deps.hooks.as_ref();
    let item = match repo::next_pending_item(db, job.id) {
        Ok(Some(item)) => item,
        Ok(None) => {
            if finalize_job_progress(db, hooks, job.id, now) {
                hooks.changed(job.id);
                return StepOutcome::Advanced { job_id: job.id };
            }
            // Only leftovers in flight (their reconcile said "later" before): try them again
            // here rather than let this job hold the head of the queue doing nothing.
            let stuck: Vec<JobItem> = repo::list_job_items(db, job.id)
                .unwrap_or_default()
                .into_iter()
                .filter(|i| i.status == JobItemStatus::InFlight)
                .collect();
            if stuck.is_empty() {
                return StepOutcome::Idle;
            }
            for item in &stuck {
                reconcile_with(deps, exec, &job, item, now).await;
            }
            return StepOutcome::Advanced { job_id: job.id };
        }
        Err(e) => {
            tracing::warn!(error = %e, "jobs: could not pick the next item");
            return StepOutcome::Idle;
        }
    };

    // The hard budget rule, for Data API writes only (a re-list costs a unit or two and is what
    // keeps a move safe, so it is never held back).
    let cost = cost_for_action_name(&item.action);
    if metered && cost > 1 {
        match budget::available_for_jobs_now(db, now) {
            Ok(available) if available < cost => {
                let _ = repo::set_waiting_quota(db, job.id, quota::next_reset_utc(now));
                hooks.changed(job.id);
                return StepOutcome::WaitingQuota { job_id: job.id };
            }
            Ok(_) => {}
            Err(e) => {
                tracing::warn!(error = %e, "jobs: could not read the budget");
                return StepOutcome::Idle;
            }
        }
    }

    if !repo::begin_item(db, item.id, now).unwrap_or(false) {
        return StepOutcome::Idle;
    }
    hooks.changed(job.id);
    let outcome = exec.execute(&job, &item).await;
    let result = apply_outcome(db, exec, hooks, &job, &item, outcome, now);
    hooks.changed(job.id);
    result
}

/// Whether the job may still be put in a waiting state: not ended, not paused by the user.
fn still_active(db: &Db, job_id: i64) -> bool {
    repo::get_job(db, job_id).ok().flatten().is_some_and(|j| {
        matches!(j.status, JobStatus::Queued | JobStatus::Running | JobStatus::Verifying)
    })
}

fn apply_outcome<E: Executor>(
    db: &Db,
    exec: &E,
    hooks: &dyn RunnerHooks,
    job: &Job,
    item: &JobItem,
    outcome: ItemOutcome,
    now: DateTime<Utc>,
) -> StepOutcome {
    let progressed = StepOutcome::Progressed { job_id: job.id, item_id: item.id };
    let account = job.account_id.as_deref();
    match outcome {
        ItemOutcome::Done { api_result, inverse, units, endpoint } => {
            let done = repo::complete_item(
                db,
                item.id,
                Some(&api_result),
                inverse.as_ref(),
                units,
                endpoint,
                account,
                now,
            );
            match done {
                Ok(()) => {
                    exec.after_done(job, item, &api_result);
                    if units > 0 {
                        hooks.quota_spent();
                    }
                }
                Err(e) => tracing::warn!(error = %e, item = item.id, "jobs: could not record"),
            }
            finalize_job_progress(db, hooks, job.id, now);
            progressed
        }
        ItemOutcome::Verified { present, units } => {
            let phase1: Vec<JobItem> = repo::list_job_items(db, job.id)
                .unwrap_or_default()
                .into_iter()
                .filter(|i| i.phase == 1)
                .collect();
            let read = (units > 0).then_some(quota::endpoint::PLAYLIST_ITEMS_LIST);
            match exec.after_verify(job, &phase1, &present) {
                Barrier::Missing(ids) => {
                    for id in &ids {
                        let why = "verify: not found in the destination, copying again";
                        let _ = repo::requeue_item(db, *id, Some(why), now);
                    }
                    let why = "verify: some copies unconfirmed, checking again after them";
                    let _ = repo::requeue_item(db, item.id, Some(why), now);
                    if let Some(endpoint) = read {
                        let job_id = Some(job.id);
                        let _ = quota::record_units(db, endpoint, units, account, job_id, now);
                    }
                }
                Barrier::Proceed(deletes) => {
                    let _ = repo::add_items(db, job.id, &deletes, now);
                    let result = json!({"confirmed": true, "deletes": deletes.len()});
                    let _ = repo::complete_item(
                        db,
                        item.id,
                        Some(&result),
                        None,
                        units,
                        read,
                        account,
                        now,
                    );
                    finalize_job_progress(db, hooks, job.id, now);
                }
            }
            if units > 0 {
                hooks.quota_spent();
            }
            progressed
        }
        ItemOutcome::Skipped(reason) => {
            let _ = repo::skip_item(db, item.id, &reason, now);
            finalize_job_progress(db, hooks, job.id, now);
            progressed
        }
        ItemOutcome::Failed(reason) => {
            let _ = repo::fail_item(db, item.id, &reason, now);
            finalize_job_progress(db, hooks, job.id, now);
            progressed
        }
        ItemOutcome::FailJob(reason) => {
            let _ = repo::fail_item(db, item.id, &reason, now);
            if still_active(db, job.id) {
                let _ = repo::set_job_status(db, job.id, JobStatus::Failed, Some(&reason), now);
                if let Ok(Some(ended)) = repo::get_job(db, job.id) {
                    hooks.finished(&ended);
                }
            }
            progressed
        }
        ItemOutcome::Retry(reason) => {
            let attempts = repo::get_job_item(db, item.id)
                .ok()
                .flatten()
                .map(|i| i.attempts)
                .unwrap_or(MAX_ITEM_ATTEMPTS);
            if attempts >= MAX_ITEM_ATTEMPTS {
                let _ = repo::fail_item(db, item.id, &reason, now);
                finalize_job_progress(db, hooks, job.id, now);
                return progressed;
            }
            let _ = repo::requeue_item(db, item.id, Some(&reason), now);
            if still_active(db, job.id) {
                let at = now + backoff_for_attempt(attempts);
                let _ = repo::set_paused_network(db, job.id, at, &reason);
            }
            StepOutcome::PausedNetwork { job_id: job.id }
        }
        ItemOutcome::QuotaExceeded(reason) => {
            let _ = repo::requeue_item(db, item.id, Some(&reason), now);
            if still_active(db, job.id) {
                let _ = repo::set_waiting_quota(db, job.id, quota::next_reset_utc(now));
            }
            hooks.quota_spent();
            StepOutcome::WaitingQuota { job_id: job.id }
        }
        ItemOutcome::NeedsAuth(reason) => {
            let _ = repo::requeue_item(db, item.id, Some(&reason), now);
            if still_active(db, job.id) {
                let _ = repo::set_waiting_auth(db, job.id, &reason);
            }
            StepOutcome::WaitingAuth { job_id: job.id }
        }
        ItemOutcome::Cooldown { until, reason } => {
            let _ = repo::requeue_item(db, item.id, Some(&reason), now);
            if still_active(db, job.id) {
                let at = until.max(now + chrono::Duration::seconds(30));
                let _ = repo::set_paused_network(db, job.id, at, &reason);
            }
            StepOutcome::PausedNetwork { job_id: job.id }
        }
    }
}

/// After any item settles: when its phase is all terminal, the next phase starts (a move's copy
/// phase gets the barrier) or the job ends. Answers whether anything changed. Safe to call
/// whenever: every branch checks its own precondition.
pub(crate) fn finalize_job_progress(
    db: &Db,
    hooks: &dyn RunnerHooks,
    job_id: i64,
    now: DateTime<Utc>,
) -> bool {
    let mut changed = false;
    // A phase can be empty (a move whose copies all failed has no deletes), so walk on.
    for _ in 0..4 {
        let Ok(Some(job)) = repo::get_job(db, job_id) else { break };
        if job.status.is_terminal()
            || !repo::phase_items_all_terminal(db, job_id, job.phase).unwrap_or(false)
        {
            break;
        }
        if job.phase >= job.total_phases {
            if let Ok(Some(_)) = repo::finalize_if_complete(db, job_id, now) {
                changed = true;
                if let Ok(Some(ended)) = repo::get_job(db, job_id) {
                    hooks.finished(&ended);
                }
            }
            break;
        }
        if job.kind == JobKind::MoveItems && job.phase == 1 {
            let verify = NewJobItem {
                phase: 2,
                action: action_name::VERIFY_DESTINATION.to_string(),
                params: json!({}),
            };
            let _ = repo::add_items(db, job_id, &[verify], now);
        }
        let _ = repo::advance_phase(db, job_id);
        changed = true;
    }
    changed
}

/// Steps until nothing is eligible (`max_steps` at most, or until `stop` says so). A job that
/// goes waiting no longer blocks the others: the loop carries on with the next one.
pub async fn run_until_blocked<Y: Executor, I: Executor>(
    deps: &RunnerDeps<Y, I>,
    now: impl Fn() -> DateTime<Utc>,
    max_steps: usize,
    stop: impl Fn() -> bool,
) -> RunSummary {
    let mut summary = RunSummary::default();
    while summary.steps < max_steps && !stop() {
        summary.steps += 1;
        match run_next_step(deps, now()).await {
            StepOutcome::Idle => {
                summary.idle = true;
                break;
            }
            StepOutcome::Progressed { .. } => summary.items_progressed += 1,
            StepOutcome::Advanced { .. } => {}
            StepOutcome::WaitingQuota { .. }
            | StepOutcome::WaitingAuth { .. }
            | StepOutcome::PausedNetwork { .. } => summary.blocked += 1,
        }
    }
    summary
}

// --- the app's runner ---------------------------------------------------------------------------

/// What the commands share with the runner: its wake-up, the jobs-changed signal the dispatch
/// layer waits on, and the Data API account manager.
pub struct JobsState {
    wake: Notify,
    changed: Notify,
    accounts: SharedAccounts,
}

impl JobsState {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            wake: Notify::new(),
            changed: Notify::new(),
            accounts: Arc::new(std::sync::RwLock::new(None)),
        })
    }

    /// Something was queued or changed: look now rather than at the next poll.
    pub fn nudge(&self) {
        self.wake.notify_one();
    }

    pub fn accounts(&self) -> SharedAccounts {
        self.accounts.clone()
    }

    pub fn account_manager(&self) -> Option<Arc<ytdata::auth::accounts::AccountManager>> {
        self.accounts.read().ok()?.clone()
    }

    pub fn set_account_manager(
        &self,
        manager: Option<Arc<ytdata::auth::accounts::AccountManager>>,
    ) {
        if let Ok(mut slot) = self.accounts.write() {
            *slot = manager;
        }
    }

    /// Whether a `client_secret.json` has been imported (the Data API's first requirement).
    pub fn client_secret_present(&self) -> bool {
        self.account_manager().is_some_and(|m| m.client_secret().is_some())
    }

    fn notify_changed(&self) {
        self.changed.notify_waiters();
    }

    /// Waits until the job has settled (ended, or waiting on something: quota, the user, the
    /// network, a pause) or `timeout` passed, and answers it as it is then.
    pub async fn wait_settled(&self, db: &Db, job_id: i64, timeout: Duration) -> Option<Job> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let notified = self.changed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            let job = repo::get_job(db, job_id).ok().flatten()?;
            if is_settled(job.status) || tokio::time::Instant::now() >= deadline {
                return Some(job);
            }
            if tokio::time::timeout_at(deadline, notified).await.is_err() {
                return repo::get_job(db, job_id).ok().flatten();
            }
        }
    }
}

/// Ended, or parked until something outside the runner happens.
pub fn is_settled(status: JobStatus) -> bool {
    status.is_terminal()
        || matches!(
            status,
            JobStatus::WaitingQuota
                | JobStatus::WaitingAuth
                | JobStatus::PausedNetwork
                | JobStatus::PausedUser
        )
}

struct AppHooks {
    state: Arc<AppState>,
    jobs: Arc<JobsState>,
}

impl RunnerHooks for AppHooks {
    fn changed(&self, job_id: i64) {
        use tauri::Emitter;
        let _ = self.state.app.emit("jobs-changed", json!({ "job_id": job_id }));
        self.jobs.notify_changed();
    }

    fn quota_spent(&self) {
        use tauri::Emitter;
        let _ = self.state.app.emit("quota-changed", ());
    }

    fn finished(&self, job: &Job) {
        control::on_job_finished(&self.state, job);
        self.jobs.notify_changed();
    }
}

/// Starts the app's runner: takes the lock (or waits for whoever has it), recovers, then runs
/// whatever is eligible, sleeping until a nudge, the next `resume_at`, or the idle poll.
pub fn spawn(state: Arc<AppState>, jobs: Arc<JobsState>) {
    let db = state.db.clone();
    let hooks: Arc<dyn RunnerHooks> =
        Arc::new(AppHooks { state: state.clone(), jobs: jobs.clone() });
    let deps = RunnerDeps {
        db: db.clone(),
        ytdata: YtDataExecutor::new(db, jobs.accounts()),
        innertube: InnerTubeExecutor::new(state),
        hooks,
    };
    tauri::async_runtime::spawn(run_loop(deps, jobs));
}

async fn run_loop<Y: Executor + 'static, I: Executor + 'static>(
    deps: RunnerDeps<Y, I>,
    jobs: Arc<JobsState>,
) {
    let pid = std::process::id();
    let mut recovered = false;
    loop {
        match lock::try_acquire(&deps.db, pid, Utc::now()) {
            Ok(lock::AcquireOutcome::Acquired) => {}
            Ok(lock::AcquireOutcome::Busy(holder)) => {
                tracing::debug!(pid = holder.pid, "jobs: another process is running the queue");
                wait(&jobs.wake, LOCK_BUSY_RETRY).await;
                continue;
            }
            Err(e) => {
                tracing::warn!(error = %e, "jobs: could not take the runner lock");
                wait(&jobs.wake, LOCK_BUSY_RETRY).await;
                continue;
            }
        }
        let lost = Arc::new(AtomicBool::new(false));
        let beat = heartbeat(deps.db.clone(), pid, lost.clone());
        if !recovered {
            recover_on_start(&deps, Utc::now()).await;
            recovered = true;
            deps.hooks.changed(0);
        }
        while !lost.load(Ordering::Relaxed) {
            let stop = || lost.load(Ordering::Relaxed);
            run_until_blocked(&deps, Utc::now, MAX_STEPS_PER_WAKE, stop).await;
            if lost.load(Ordering::Relaxed) {
                break;
            }
            wait(&jobs.wake, next_wake(&deps.db, Utc::now())).await;
        }
        beat.abort();
        tracing::warn!("jobs: the runner lock was taken over; waiting to get it back");
    }
}

/// Renews the lock every [`HEARTBEAT_EVERY`]; flags `lost` once another process has taken it.
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
                Err(e) => tracing::warn!(error = %e, "jobs: could not renew the runner lock"),
            }
        }
    })
}

/// Until the earliest `resume_at`, at most [`IDLE_POLL`], at least a second.
fn next_wake(db: &Db, now: DateTime<Utc>) -> Duration {
    match repo::next_resume_at(db).ok().flatten() {
        Some(at) => {
            (at - now).to_std().unwrap_or(Duration::ZERO).clamp(Duration::from_secs(1), IDLE_POLL)
        }
        None => IDLE_POLL,
    }
}

async fn wait(wake: &Notify, at_most: Duration) {
    let _ = tokio::time::timeout(at_most, wake.notified()).await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jobs::{planner, NewJob};
    use crate::jobs::{PRIORITY_HIGH, PRIORITY_LOW, PRIORITY_NORMAL, PRIORITY_SYSTEM};
    use std::collections::{HashMap, VecDeque};
    use std::sync::Mutex;

    fn dt(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }
    /// Noon PDT.
    fn now() -> DateTime<Utc> {
        dt("2026-07-15T19:00:00Z")
    }

    /// An engine that answers from a script: per video id, the outcomes of its next calls in
    /// order; anything unscripted is done, with an inverse that deletes what it inserted.
    #[derive(Default)]
    struct Fake {
        script: Mutex<HashMap<String, VecDeque<ItemOutcome>>>,
        present: Mutex<HashSet<String>>,
        calls: Mutex<Vec<(i64, String, String)>>,
    }

    impl Fake {
        fn script(&self, video: &str, outcomes: Vec<ItemOutcome>) {
            self.script.lock().unwrap().insert(video.into(), outcomes.into());
        }
        fn present(&self, videos: &[&str]) {
            *self.present.lock().unwrap() = videos.iter().map(|v| v.to_string()).collect();
        }
        fn calls(&self) -> Vec<(i64, String, String)> {
            self.calls.lock().unwrap().clone()
        }
    }

    fn video(item: &JobItem) -> String {
        item.params.get("video_id").and_then(Value::as_str).unwrap_or_default().to_string()
    }

    impl Executor for Fake {
        async fn execute(&self, job: &Job, item: &JobItem) -> ItemOutcome {
            let v = video(item);
            self.calls.lock().unwrap().push((job.id, item.action.clone(), v.clone()));
            if item.action == action_name::VERIFY_DESTINATION {
                let present = self.present.lock().unwrap().clone();
                return ItemOutcome::Verified { present, units: 1 };
            }
            let scripted = self.script.lock().unwrap().get_mut(&v).and_then(VecDeque::pop_front);
            if let Some(next) = scripted {
                return next;
            }
            let units = cost_for_action_name(&item.action);
            let api_result = json!({"playlist_item_id": format!("pi-{v}")});
            let inverse = planner::PlannedAction::from_params(&item.action, &item.params)
                .and_then(|a| a.inverse_json(Some(&api_result)));
            ItemOutcome::Done { api_result, inverse, units, endpoint: Some("playlistItems.insert") }
        }

        async fn reconcile(&self, _job: &Job, item: &JobItem) -> Reconcile {
            if self.present.lock().unwrap().contains(&video(item)) {
                Reconcile::Done {
                    api_result: json!({"playlist_item_id": "found"}),
                    inverse: None,
                    units: 50,
                    endpoint: Some("playlistItems.insert"),
                }
            } else {
                Reconcile::Requeue("not found".into())
            }
        }

        fn after_verify(
            &self,
            job: &Job,
            phase1: &[JobItem],
            present: &HashSet<String>,
        ) -> Barrier {
            planner::ytdata_move_barrier(job, phase1, present)
        }
    }

    #[derive(Default)]
    struct Recorder {
        finished: Mutex<Vec<(i64, JobStatus)>>,
    }
    impl RunnerHooks for Recorder {
        fn finished(&self, job: &Job) {
            self.finished.lock().unwrap().push((job.id, job.status));
        }
    }

    struct Harness {
        deps: RunnerDeps<Fake, Fake>,
        hooks: Arc<Recorder>,
    }

    fn harness() -> Harness {
        let db = Arc::new(Db::open(std::path::Path::new(":memory:")).unwrap());
        crate::ytdata_accounts::upsert(&db, "UC1", "Channel", None, 1).unwrap();
        let hooks = Arc::new(Recorder::default());
        let deps = RunnerDeps {
            db,
            ytdata: Fake::default(),
            innertube: Fake::default(),
            hooks: hooks.clone(),
        };
        Harness { deps, hooks }
    }

    fn insert(v: &str) -> NewJobItem {
        NewJobItem {
            phase: 1,
            action: action_name::PLAYLIST_ITEM_INSERT.into(),
            params: json!({"playlist_id": "PLdst", "video_id": v, "occurrence": 0}),
        }
    }

    fn enqueue(
        h: &Harness,
        kind: JobKind,
        priority: i64,
        videos: &[&str],
        at: DateTime<Utc>,
    ) -> i64 {
        let new = NewJob {
            account_id: Some("UC1".into()),
            kind: kind.clone(),
            params: json!({
                "engine": "ytdata",
                "dest_playlist_id": "PLdst",
                "source_playlist_id": "PLsrc",
            }),
            priority,
            total_phases: if kind == JobKind::MoveItems { 3 } else { 1 },
            est_units_total: 50 * videos.len() as i64,
            items: videos.iter().map(|v| insert(v)).collect(),
        };
        repo::insert_job(&h.deps.db, &new, at).unwrap()
    }

    fn job(h: &Harness, id: i64) -> Job {
        repo::get_job(&h.deps.db, id).unwrap().unwrap()
    }

    async fn drain(h: &Harness) -> RunSummary {
        run_until_blocked(&h.deps, now, 1000, || false).await
    }

    #[tokio::test]
    async fn jobs_run_by_priority_then_age() {
        let h = harness();
        let low = enqueue(&h, JobKind::CopyItems, PRIORITY_LOW, &["l"], now());
        let normal_old = enqueue(&h, JobKind::CopyItems, PRIORITY_NORMAL, &["n1"], now());
        let later = |s: i64| now() + chrono::Duration::seconds(s);
        let normal_new = enqueue(&h, JobKind::CopyItems, PRIORITY_NORMAL, &["n2"], later(5));
        let high = enqueue(&h, JobKind::CopyItems, PRIORITY_HIGH, &["h"], later(60));
        let system = enqueue(&h, JobKind::CopyItems, PRIORITY_SYSTEM, &["s"], later(120));
        let summary = drain(&h).await;
        assert!(summary.idle);
        let order: Vec<i64> = h.deps.ytdata.calls().iter().map(|c| c.0).collect();
        assert_eq!(order, [system, high, normal_old, normal_new, low]);
        for id in [low, normal_old, normal_new, high, system] {
            assert_eq!(job(&h, id).status, JobStatus::Completed);
        }
        assert_eq!(crate::quota::spent_today(&h.deps.db, now()).unwrap(), 250);
        assert_eq!(h.hooks.finished.lock().unwrap().len(), 5);
    }

    #[tokio::test]
    async fn the_engine_in_params_picks_the_executor() {
        let h = harness();
        let mut new = NewJob {
            account_id: None,
            kind: JobKind::CopyItems,
            params: json!({"engine": "innertube"}),
            priority: PRIORITY_NORMAL,
            total_phases: 1,
            est_units_total: 0,
            items: vec![NewJobItem {
                phase: 1,
                action: action_name::IT_ADD_ROWS.into(),
                params: json!({"playlist_id": "VLPL1", "video_id": "x"}),
            }],
        };
        let it = repo::insert_job(&h.deps.db, &new, now()).unwrap();
        new.params = json!({"engine": "ytdata"});
        new.account_id = Some("UC1".into());
        new.items = vec![insert("y")];
        let yt = repo::insert_job(&h.deps.db, &new, now()).unwrap();
        drain(&h).await;
        let added = (it, action_name::IT_ADD_ROWS.to_string(), "x".to_string());
        assert_eq!(h.deps.innertube.calls(), [added]);
        assert_eq!(h.deps.ytdata.calls().len(), 1);
        assert_eq!(h.deps.ytdata.calls()[0].0, yt);
    }

    #[tokio::test]
    async fn a_paused_job_waits_and_resumes_where_it_was() {
        let h = harness();
        let paused = enqueue(&h, JobKind::CopyItems, PRIORITY_HIGH, &["a", "b"], now());
        let other = enqueue(&h, JobKind::CopyItems, PRIORITY_LOW, &["c"], now());
        control::pause(&h.deps.db, paused).unwrap();
        drain(&h).await;
        assert_eq!(job(&h, paused).status, JobStatus::PausedUser);
        assert_eq!(job(&h, other).status, JobStatus::Completed, "a pause doesn't hold the queue");
        assert_eq!(job(&h, paused).done_items, 0);

        control::resume(&h.deps.db, paused).unwrap();
        drain(&h).await;
        assert_eq!(job(&h, paused).status, JobStatus::Completed);
        assert_eq!(job(&h, paused).done_items, 2);
    }

    #[tokio::test]
    async fn a_pause_landing_while_an_item_is_in_flight_survives_its_outcome() {
        let h = harness();
        let id = enqueue(&h, JobKind::CopyItems, PRIORITY_NORMAL, &["a"], now());
        let item = repo::next_pending_item(&h.deps.db, id).unwrap().unwrap();
        repo::begin_item(&h.deps.db, item.id, now()).unwrap();
        control::pause(&h.deps.db, id).unwrap();
        let j = job(&h, id);
        let outcome = ItemOutcome::Retry("connection reset".into());
        apply_outcome(&h.deps.db, &h.deps.ytdata, h.hooks.as_ref(), &j, &item, outcome, now());
        assert_eq!(job(&h, id).status, JobStatus::PausedUser, "not turned into paused_network");
    }

    #[tokio::test]
    async fn cancel_with_revert_queues_the_inverses_in_reverse_order_as_a_system_job() {
        let h = harness();
        let id = enqueue(&h, JobKind::CopyItems, PRIORITY_NORMAL, &["a", "b", "c"], now());
        // Two copies land, then the third waits on the network.
        h.deps.ytdata.script("c", vec![ItemOutcome::Retry("timeout".into())]);
        drain(&h).await;
        assert_eq!(job(&h, id).status, JobStatus::PausedNetwork);

        control::cancel(&h.deps.db, id, true, now()).unwrap();
        assert_eq!(job(&h, id).status, JobStatus::Cancelled);
        drain(&h).await;

        let revert_id = job(&h, id).params["revert_job_id"].as_i64().expect("a revert was queued");
        let revert = job(&h, revert_id);
        assert_eq!(revert.priority, PRIORITY_SYSTEM);
        assert_eq!(revert.kind, JobKind::Other("undo".into()));
        assert_eq!(revert.params["undo_of_job_id"], json!(id));
        assert_eq!(revert.status, JobStatus::Completed);
        let undone: Vec<(String, String)> = repo::list_job_items(&h.deps.db, revert_id)
            .unwrap()
            .into_iter()
            .map(|i| (i.action, i.params["playlist_item_id"].as_str().unwrap().to_string()))
            .collect();
        let delete = action_name::PLAYLIST_ITEM_DELETE.to_string();
        assert_eq!(undone, [(delete.clone(), "pi-b".into()), (delete, "pi-a".into())]);
        assert_eq!(
            h.deps.ytdata.calls().iter().filter(|c| c.2 == "c").count(),
            1,
            "the cancelled job's last item never ran again"
        );
        let finished = h.hooks.finished.lock().unwrap().clone();
        assert!(finished.contains(&(revert_id, JobStatus::Completed)));
    }

    #[tokio::test]
    async fn a_move_deletes_only_after_the_barrier_confirms_every_copy() {
        let h = harness();
        let id = enqueue(&h, JobKind::MoveItems, PRIORITY_NORMAL, &["a", "b"], now());
        // The first look finds only `a`: `b` is copied again, then the barrier passes.
        h.deps.ytdata.present(&["a"]);
        assert!(matches!(run_next_step(&h.deps, now()).await, StepOutcome::Progressed { .. }));
        assert!(matches!(run_next_step(&h.deps, now()).await, StepOutcome::Progressed { .. }));
        assert_eq!(job(&h, id).phase, 2, "copies done: the barrier is next");
        run_next_step(&h.deps, now()).await; // the barrier: b missing
        let items = repo::list_job_items(&h.deps.db, id).unwrap();
        assert!(items.iter().all(|i| i.phase < 3), "no delete before every copy is confirmed");
        let b = items.iter().find(|i| video(i) == "b").unwrap();
        assert_eq!(b.status, JobItemStatus::Pending, "b is copied again");

        h.deps.ytdata.present(&["a", "b"]);
        drain(&h).await;
        let j = job(&h, id);
        assert_eq!(j.status, JobStatus::Completed);
        assert_eq!(j.phase, 3);
        let deletes: Vec<String> = repo::list_job_items(&h.deps.db, id)
            .unwrap()
            .into_iter()
            .filter(|i| i.phase == 3)
            .map(|i| video(&i))
            .collect();
        assert_eq!(deletes, ["a", "b"]);
        let calls: Vec<String> = h.deps.ytdata.calls().into_iter().map(|c| c.1).collect();
        let first_delete =
            calls.iter().position(|a| a == action_name::PLAYLIST_ITEM_DELETE).unwrap();
        let last_verify = calls.iter().rposition(|a| a == action_name::VERIFY_DESTINATION).unwrap();
        assert!(last_verify < first_delete);
    }

    #[tokio::test]
    async fn a_copy_that_never_landed_never_deletes_its_source() {
        let h = harness();
        let id = enqueue(&h, JobKind::MoveItems, PRIORITY_NORMAL, &["a", "gone"], now());
        h.deps.ytdata.script("gone", vec![ItemOutcome::Skipped("videoNotFound".into())]);
        h.deps.ytdata.present(&["a"]);
        drain(&h).await;
        let j = job(&h, id);
        assert_eq!(j.status, JobStatus::CompletedWithErrors);
        let deletes: Vec<String> = repo::list_job_items(&h.deps.db, id)
            .unwrap()
            .into_iter()
            .filter(|i| i.phase == 3)
            .map(|i| video(&i))
            .collect();
        assert_eq!(deletes, ["a"]);
    }

    #[tokio::test]
    async fn quota_exceeded_waits_until_the_next_reset() {
        let h = harness();
        let id = enqueue(&h, JobKind::CopyItems, PRIORITY_NORMAL, &["a", "b"], now());
        h.deps.ytdata.script("b", vec![ItemOutcome::QuotaExceeded("quotaExceeded".into())]);
        let summary = drain(&h).await;
        assert_eq!(summary.blocked, 1);
        let j = job(&h, id);
        assert_eq!(j.status, JobStatus::WaitingQuota);
        assert_eq!(j.resume_at, Some(dt("2026-07-16T07:00:00Z")), "midnight Pacific");
        let b = repo::list_job_items(&h.deps.db, id).unwrap().into_iter().find(|i| video(i) == "b");
        assert_eq!(b.unwrap().status, JobItemStatus::Pending);

        // Not before the reset...
        run_until_blocked(&h.deps, || dt("2026-07-16T06:59:00Z"), 10, || false).await;
        assert_eq!(job(&h, id).status, JobStatus::WaitingQuota);
        // ...and on its own after it.
        run_until_blocked(&h.deps, || dt("2026-07-16T07:00:01Z"), 10, || false).await;
        assert_eq!(job(&h, id).status, JobStatus::Completed);
    }

    #[tokio::test]
    async fn the_budget_rule_holds_a_write_back_before_it_is_made() {
        let h = harness();
        crate::quota::record_units(&h.deps.db, "playlists.list", 9200, None, None, now()).unwrap();
        // 10 000 - 300 margin - 500 reserve - 9200 spent: nothing left for a 50 u write.
        let id = enqueue(&h, JobKind::CopyItems, PRIORITY_NORMAL, &["a"], now());
        let outcome = run_next_step(&h.deps, now()).await;
        assert_eq!(outcome, StepOutcome::WaitingQuota { job_id: id });
        assert!(h.deps.ytdata.calls().is_empty(), "no call was made");
        assert_eq!(job(&h, id).resume_at, Some(dt("2026-07-16T07:00:00Z")));
    }

    #[tokio::test]
    async fn needs_auth_waits_for_the_user_and_cooldown_for_its_end() {
        let h = harness();
        let auth = enqueue(&h, JobKind::CopyItems, PRIORITY_HIGH, &["a"], now());
        h.deps.ytdata.script("a", vec![ItemOutcome::NeedsAuth("invalid_grant".into())]);
        let until = now() + chrono::Duration::minutes(20);
        let cool = enqueue(&h, JobKind::CopyItems, PRIORITY_NORMAL, &["b"], now());
        h.deps.ytdata.script("b", vec![ItemOutcome::Cooldown { until, reason: "429".into() }]);
        drain(&h).await;
        assert_eq!(job(&h, auth).status, JobStatus::WaitingAuth);
        assert_eq!(job(&h, auth).last_error.as_deref(), Some("invalid_grant"));
        assert_eq!(job(&h, cool).status, JobStatus::PausedNetwork);
        assert_eq!(job(&h, cool).resume_at, Some(until));
        run_until_blocked(&h.deps, || now() + chrono::Duration::days(3), 10, || false).await;
        assert_eq!(job(&h, auth).status, JobStatus::WaitingAuth, "never resumes by itself");
        assert_eq!(job(&h, cool).status, JobStatus::Completed);
    }

    #[tokio::test]
    async fn a_transient_failure_backs_off_and_fails_the_item_after_max_attempts() {
        let h = harness();
        let id = enqueue(&h, JobKind::CopyItems, PRIORITY_NORMAL, &["a", "b"], now());
        let flaky = (0..MAX_ITEM_ATTEMPTS).map(|_| ItemOutcome::Retry("503".into())).collect();
        h.deps.ytdata.script("a", flaky);
        let mut at = now();
        for attempt in 1..MAX_ITEM_ATTEMPTS {
            run_until_blocked(&h.deps, || at, 10, || false).await;
            let j = job(&h, id);
            assert_eq!(j.status, JobStatus::PausedNetwork, "attempt {attempt}");
            assert_eq!(j.resume_at, Some(at + backoff_for_attempt(attempt)));
            at = j.resume_at.unwrap();
        }
        run_until_blocked(&h.deps, || at, 10, || false).await;
        let j = job(&h, id);
        assert_eq!(j.status, JobStatus::CompletedWithErrors);
        assert_eq!((j.failed_items, j.done_items), (1, 1));
        let a = repo::list_job_items(&h.deps.db, id).unwrap().into_iter().find(|i| video(i) == "a");
        assert_eq!(a.unwrap().attempts, MAX_ITEM_ATTEMPTS);
    }

    #[tokio::test]
    async fn recovery_reconciles_in_flight_items_and_requeues_running_jobs() {
        let h = harness();
        let videos = ["landed", "lost", "todo"];
        let id = enqueue(&h, JobKind::CopyItems, PRIORITY_NORMAL, &videos, now());
        let items = repo::list_job_items(&h.deps.db, id).unwrap();
        // A crash with two inserts in flight: one had reached YouTube, one hadn't.
        repo::begin_item(&h.deps.db, items[0].id, now()).unwrap();
        repo::begin_item(&h.deps.db, items[1].id, now()).unwrap();
        assert_eq!(job(&h, id).status, JobStatus::Running);
        h.deps.ytdata.present(&["landed"]);

        recover_on_start(&h.deps, now()).await;
        let j = job(&h, id);
        assert_eq!(j.status, JobStatus::Queued, "running -> queued");
        let items = repo::list_job_items(&h.deps.db, id).unwrap();
        assert_eq!(items[0].status, JobItemStatus::Done, "found on the playlist: recorded");
        assert_eq!(items[1].status, JobItemStatus::Pending, "not found: runs again");
        assert!(h.deps.ytdata.calls().is_empty(), "nothing re-sent during recovery");
        assert_eq!(crate::quota::spent_today(&h.deps.db, now()).unwrap(), 50);

        drain(&h).await;
        assert_eq!(job(&h, id).status, JobStatus::Completed);
        let sent: Vec<String> = h.deps.ytdata.calls().into_iter().map(|c| c.2).collect();
        assert_eq!(sent, ["lost", "todo"], "the landed insert is never sent twice");
    }

    #[tokio::test]
    async fn fail_job_ends_the_job_and_retry_failed_revives_it() {
        let h = harness();
        let id = enqueue(&h, JobKind::CopyItems, PRIORITY_NORMAL, &["a", "b"], now());
        h.deps.ytdata.script("a", vec![ItemOutcome::FailJob("manualSortRequired".into())]);
        drain(&h).await;
        let j = job(&h, id);
        assert_eq!(j.status, JobStatus::Failed);
        assert_eq!(j.last_error.as_deref(), Some("manualSortRequired"));
        assert_eq!(control::retry_failed(&h.deps.db, id, now()).unwrap(), 1);
        drain(&h).await;
        assert_eq!(job(&h, id).status, JobStatus::Completed);
    }

    #[tokio::test]
    async fn wait_settled_returns_once_the_job_settles_or_times_out() {
        let h = harness();
        let jobs = JobsState::new();
        let id = enqueue(&h, JobKind::CopyItems, PRIORITY_NORMAL, &["a"], now());
        let quick = jobs.wait_settled(&h.deps.db, id, Duration::from_millis(20)).await.unwrap();
        assert_eq!(quick.status, JobStatus::Queued, "timed out, answered as it is");
        drain(&h).await;
        let done = jobs.wait_settled(&h.deps.db, id, Duration::from_secs(5)).await.unwrap();
        assert_eq!(done.status, JobStatus::Completed);
        assert!(is_settled(JobStatus::WaitingAuth) && !is_settled(JobStatus::Running));
    }
}
