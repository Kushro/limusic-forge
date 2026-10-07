//! The user's controls over the queue, and how a job's end reaches the undo history. Port of
//! PlaylistForge's `pf-app/src/jobs/control.rs`.
//!
//! Every control is a synchronous database write; the runner picks the change up on its next
//! step (callers nudge it). A cancel never interrupts an item in flight: its outcome is recorded
//! (the write happened), and the job stays cancelled.
//!
//! **Cancel and revert.** A cancel can ask for what the job already did to be undone. The revert
//! is built by the runner at its next step, when nothing of that job can be in flight, from the
//! `inverse_json` of every `done` item, newest first (a move's deletes are put back before its
//! copies are taken away, so no track is ever in neither playlist), and queued as a SYSTEM job on
//! the same engine and account.
//!
//! **History.** A job that ends having changed something is recorded in the `playlist_ops`
//! journal as one step, `RevertJob`, so the existing history and its Undo cover queued work too.

use serde::Serialize;
use serde_json::{json, Value};

use super::engine::engine_of;
use super::planner::cost_for_action_name;
use super::{repo, set_param};
use super::{Job, JobFilter, JobItem, JobItemStatus, JobKind, JobStatus, NewJob, NewJobItem};
use super::{ENGINE_KEY, PRIORITY_HIGH, PRIORITY_LOW, PRIORITY_SYSTEM};
use crate::db::Db;
use crate::playlist_tools::journal::{self, Step, Summary};
use crate::state::AppState;

/// `jobs.kind` of a revert.
pub const UNDO_KIND: &str = "undo";

pub fn pause(db: &Db, job_id: i64) -> rusqlite::Result<()> {
    repo::pause_job(db, job_id)
}

pub fn resume(db: &Db, job_id: i64) -> rusqlite::Result<()> {
    repo::resume_job(db, job_id)
}

/// HIGH, NORMAL or LOW: SYSTEM stays the app's own.
pub fn set_priority(db: &Db, job_id: i64, priority: i64) -> rusqlite::Result<()> {
    repo::reprioritize_job(db, job_id, priority.clamp(PRIORITY_HIGH, PRIORITY_LOW))
}

/// Cancels the job; with `revert`, also asks for what it did to be undone (see the module doc).
/// Reverting a job that already ended is the same request: an undo.
pub fn cancel(
    db: &Db,
    job_id: i64,
    revert: bool,
    now: chrono::DateTime<chrono::Utc>,
) -> rusqlite::Result<()> {
    let Some(job) = repo::get_job(db, job_id)? else { return Ok(()) };
    repo::cancel_job(db, job_id, now)?;
    if revert && job.params.get("revert_job_id").is_none_or(Value::is_null) {
        let params = set_param(job.params, "revert_requested", json!(true));
        repo::set_job_params(db, job_id, &params)?;
    }
    Ok(())
}

pub fn retry_failed(
    db: &Db,
    job_id: i64,
    now: chrono::DateTime<chrono::Utc>,
) -> rusqlite::Result<usize> {
    repo::retry_failed_items(db, job_id, now)
}

/// Once an account is connected again, its jobs waiting for that resume.
pub fn resume_waiting_auth_for_account(db: &Db, account_id: &str) -> rusqlite::Result<usize> {
    let filter = JobFilter {
        account_id: Some(account_id.to_string()),
        statuses: Some(vec![JobStatus::WaitingAuth]),
        limit: None,
    };
    let waiting = repo::list_jobs(db, &filter)?;
    for job in &waiting {
        repo::resume_job(db, job.id)?;
    }
    Ok(waiting.len())
}

/// What reverting a job would take, before the user confirms.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct RevertPreview {
    pub item_count: usize,
    pub estimated_units: i64,
}

fn inverse_items(items: &[JobItem]) -> Vec<NewJobItem> {
    let mut done: Vec<&JobItem> =
        items.iter().filter(|i| i.status == JobItemStatus::Done && i.inverse.is_some()).collect();
    // Newest first. `seq` runs on across phases, so it is the order they were planned in; the
    // items of a phase run in `seq` order, and the phases in order.
    done.sort_by(|a, b| (b.phase, b.seq).cmp(&(a.phase, a.seq)));
    done.into_iter()
        .filter_map(|item| {
            let inverse = item.inverse.as_ref()?;
            let action = inverse.get("action")?.as_str()?;
            let params = inverse.get("params").cloned().unwrap_or_else(|| json!({}));
            Some(NewJobItem { phase: 1, action: action.to_string(), params })
        })
        .collect()
}

pub fn revert_preview(db: &Db, job_id: i64) -> rusqlite::Result<RevertPreview> {
    let items = inverse_items(&repo::list_job_items(db, job_id)?);
    Ok(RevertPreview {
        item_count: items.len(),
        estimated_units: items.iter().map(|i| cost_for_action_name(&i.action)).sum(),
    })
}

/// Queues the job that undoes `job_id`'s done items, newest first, as a SYSTEM job on the same
/// engine and account. `None` when there is nothing to undo or no such job.
pub fn create_revert_job(
    db: &Db,
    job_id: i64,
    now: chrono::DateTime<chrono::Utc>,
) -> rusqlite::Result<Option<i64>> {
    let Some(source) = repo::get_job(db, job_id)? else { return Ok(None) };
    let items = inverse_items(&repo::list_job_items(db, job_id)?);
    if items.is_empty() {
        return Ok(None);
    }
    let mut params = json!({
        ENGINE_KEY: engine_of(&source).as_str(),
        "undo_of_job_id": job_id,
        "op_kind": UNDO_KIND,
    });
    for key in ["account", "summary"] {
        if let Some(v) = source.params.get(key) {
            params[key] = v.clone();
        }
    }
    let new = NewJob {
        account_id: source.account_id.clone(),
        kind: JobKind::Other(UNDO_KIND.to_string()),
        params,
        priority: PRIORITY_SYSTEM,
        total_phases: 1,
        est_units_total: items.iter().map(|i| cost_for_action_name(&i.action)).sum(),
        items,
    };
    repo::insert_job(db, &new, now).map(Some)
}

/// The runner's step: every job whose cancel asked for a revert gets it queued, now that none of
/// its items can be in flight. Answers the revert jobs made.
pub fn materialize_pending_reverts(
    db: &Db,
    now: chrono::DateTime<chrono::Utc>,
) -> rusqlite::Result<Vec<i64>> {
    let pending: Vec<i64> = {
        let conn = db.conn();
        let mut stmt = conn.prepare(
            "SELECT id FROM jobs \
             WHERE status IN ('completed', 'completed_with_errors', 'failed', 'cancelled') \
               AND json_extract(params_json, '$.revert_requested') = 1 \
             ORDER BY id",
        )?;
        let rows = stmt.query_map([], |r| r.get(0))?;
        rows.collect::<rusqlite::Result<_>>()?
    };
    let mut made = Vec::new();
    for job_id in pending {
        let revert = create_revert_job(db, job_id, now)?;
        if let Some(job) = repo::get_job(db, job_id)? {
            let params = set_param(job.params, "revert_requested", json!(false));
            repo::set_job_params(db, job_id, &set_param(params, "revert_job_id", json!(revert)))?;
        }
        made.extend(revert);
    }
    Ok(made)
}

/// How many jobs are in each state the jobs page counts.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct QueueSummary {
    pub active: i64,
    pub waiting_quota: i64,
    pub waiting_auth: i64,
    pub paused_user: i64,
    pub paused_network: i64,
}

pub fn queue_summary(db: &Db) -> rusqlite::Result<QueueSummary> {
    let mut summary = QueueSummary::default();
    for job in repo::list_jobs(db, &JobFilter::default())? {
        match job.status {
            JobStatus::Queued | JobStatus::Running | JobStatus::Verifying => summary.active += 1,
            JobStatus::WaitingQuota => summary.waiting_quota += 1,
            JobStatus::WaitingAuth => summary.waiting_auth += 1,
            JobStatus::PausedUser => summary.paused_user += 1,
            JobStatus::PausedNetwork => summary.paused_network += 1,
            _ => {}
        }
    }
    Ok(summary)
}

/// The user's drag in the queue (see [`repo::reorder_queue`]).
pub fn reorder(db: &Db, ids: &[i64]) -> rusqlite::Result<usize> {
    repo::reorder_queue(db, ids)
}

// --- the jobs page --------------------------------------------------------------------------------

/// A job as the jobs page shows it. Dates are RFC 3339 UTC. From `params_json` only what the page
/// words its title and its history with: the summary (playlists, count), the operation's kind, and
/// the undo links.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct JobView {
    pub id: i64,
    pub kind: String,
    /// The operation the user asked for (`copy`, `move`, `dedupe`...), when the dispatch named it.
    pub op_kind: Option<String>,
    pub status: &'static str,
    pub priority: i64,
    pub engine: &'static str,
    pub account_id: Option<String>,
    pub phase: i64,
    pub total_phases: i64,
    pub created_at: String,
    pub started_at: Option<String>,
    pub finished_at: Option<String>,
    pub resume_at: Option<String>,
    pub est_units_total: i64,
    pub spent_units: i64,
    pub total_items: i64,
    pub done_items: i64,
    pub failed_items: i64,
    pub skipped_items: i64,
    pub planned_items: i64,
    pub retried_items: i64,
    pub last_error: Option<String>,
    pub summary: Option<Summary>,
    /// The job this one undoes.
    pub undo_of_job_id: Option<i64>,
    /// The job queued to undo this one.
    pub revert_job_id: Option<i64>,
    /// A revert asked for and not queued yet.
    pub revert_requested: bool,
    /// Its entry in the undo history (`playlist_ops`).
    pub op_id: Option<i64>,
}

fn text(at: chrono::DateTime<chrono::Utc>) -> String {
    crate::db::rfc3339_text(at)
}

pub fn job_view(job: &Job) -> JobView {
    let p = &job.params;
    let int = |key: &str| p.get(key).and_then(Value::as_i64);
    JobView {
        id: job.id,
        kind: job.kind.as_str().to_string(),
        op_kind: p.get("op_kind").and_then(Value::as_str).map(str::to_string),
        status: job.status.as_str(),
        priority: job.priority,
        engine: engine_of(job).as_str(),
        account_id: job.account_id.clone(),
        phase: job.phase,
        total_phases: job.total_phases,
        created_at: text(job.created_at),
        started_at: job.started_at.map(text),
        finished_at: job.finished_at.map(text),
        resume_at: job.resume_at.map(text),
        est_units_total: job.est_units_total,
        spent_units: job.spent_units,
        total_items: job.total_items,
        done_items: job.done_items,
        failed_items: job.failed_items,
        skipped_items: job.skipped_items,
        planned_items: job.planned_items,
        retried_items: job.retried_items,
        last_error: job.last_error.clone(),
        summary: p.get("summary").and_then(|s| serde_json::from_value(s.clone()).ok()),
        undo_of_job_id: int("undo_of_job_id"),
        revert_job_id: int("revert_job_id"),
        revert_requested: p.get("revert_requested").and_then(Value::as_bool).unwrap_or(false),
        op_id: int("op_id"),
    }
}

/// Which jobs the page lists.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ListFilter {
    /// Every job that has not ended, in the order the runner takes them.
    #[default]
    Active,
    /// The jobs that ended, newest first.
    History,
    /// Both: the active ones first, then the history.
    All,
}

/// How many ended jobs the history shows unless asked for another number.
pub const HISTORY_LIMIT: i64 = 100;

const TERMINAL: [JobStatus; 4] =
    [JobStatus::Completed, JobStatus::CompletedWithErrors, JobStatus::Failed, JobStatus::Cancelled];

fn active_jobs(db: &Db) -> rusqlite::Result<Vec<Job>> {
    let statuses = JobStatus::ALL.into_iter().filter(|s| !s.is_terminal()).collect();
    let filter = JobFilter { statuses: Some(statuses), ..Default::default() };
    let mut jobs = repo::list_jobs(db, &filter)?;
    jobs.sort_by(|a, b| (a.priority, a.created_at, a.id).cmp(&(b.priority, b.created_at, b.id)));
    Ok(jobs)
}

fn ended_jobs(db: &Db, limit: i64) -> rusqlite::Result<Vec<Job>> {
    let statuses = Some(TERMINAL.to_vec());
    let filter = JobFilter { statuses, limit: Some(limit.max(1)), ..Default::default() };
    repo::list_jobs(db, &filter)
}

/// The page's list. `limit` caps the history (default [`HISTORY_LIMIT`]); the active jobs are
/// always all there.
pub fn list(db: &Db, filter: ListFilter, limit: Option<i64>) -> rusqlite::Result<Vec<JobView>> {
    let limit = limit.unwrap_or(HISTORY_LIMIT);
    let jobs = match filter {
        ListFilter::Active => active_jobs(db)?,
        ListFilter::History => ended_jobs(db, limit)?,
        ListFilter::All => {
            let mut all = active_jobs(db)?;
            all.extend(ended_jobs(db, limit)?);
            all
        }
    };
    Ok(jobs.iter().map(job_view).collect())
}

/// One item as the job's detail lists it: what it does, how it went, and why not.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ItemView {
    pub id: i64,
    pub seq: i64,
    pub phase: i64,
    pub action: String,
    pub status: &'static str,
    pub attempts: i64,
    pub last_error: Option<String>,
    pub video_id: Option<String>,
    pub playlist_id: Option<String>,
    pub updated_at: String,
}

fn item_view(item: &JobItem) -> ItemView {
    let param = |key: &str| item.params.get(key).and_then(Value::as_str).map(str::to_string);
    ItemView {
        id: item.id,
        seq: item.seq,
        phase: item.phase,
        action: item.action.clone(),
        status: item.status.as_str(),
        attempts: item.attempts,
        last_error: item.last_error.clone(),
        video_id: param("video_id"),
        playlist_id: param("playlist_id"),
        updated_at: text(item.updated_at),
    }
}

/// A job opened on the page: the job, its items in run order, and what reverting it would take.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct JobDetail {
    pub job: JobView,
    pub items: Vec<ItemView>,
    pub revert: RevertPreview,
}

pub fn detail(db: &Db, job_id: i64) -> rusqlite::Result<Option<JobDetail>> {
    let Some(job) = repo::get_job(db, job_id)? else { return Ok(None) };
    let items = repo::list_job_items(db, job_id)?;
    let inverse = inverse_items(&items);
    let revert = RevertPreview {
        item_count: inverse.len(),
        estimated_units: inverse.iter().map(|i| cost_for_action_name(&i.action)).sum(),
    };
    Ok(Some(JobDetail {
        job: job_view(&job),
        items: items.iter().map(item_view).collect(),
        revert,
    }))
}

/// Today's quota as the page's budget bar splits it (port of PlaylistForge's `BudgetGauge`): what
/// the monitor's backups, the jobs and everything else spent, the backup reserve still walled off,
/// the safety margin, and what jobs may still spend. Units; `daily_units` is the whole bar.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BudgetPartition {
    pub daily_units: i64,
    pub spent_total: i64,
    pub spent_backup: i64,
    pub spent_jobs: i64,
    /// What neither a backup run nor a job spent (a sync read by hand, a settings check): the
    /// bar still has to account for every unit.
    pub spent_other: i64,
    pub reserve_remaining: i64,
    pub safety_margin: i64,
    pub available_for_jobs: i64,
    /// The next reset (midnight Pacific), RFC 3339 UTC.
    pub next_reset: String,
}

pub fn budget_partition(
    db: &Db,
    now: chrono::DateTime<chrono::Utc>,
) -> rusqlite::Result<BudgetPartition> {
    use super::budget;
    let spent_total = crate::quota::spent_today(db, now)?;
    let spent_backup = budget::backup_spent_today(db, now)?;
    let spent_jobs = budget::jobs_spent_today(db, now)?;
    Ok(BudgetPartition {
        daily_units: budget::daily_units(db),
        spent_total,
        spent_backup,
        spent_jobs,
        spent_other: (spent_total - spent_backup - spent_jobs).max(0),
        reserve_remaining: budget::backup_reserve_remaining_today(db, now)?,
        safety_margin: budget::safety_margin_units(db),
        available_for_jobs: budget::available_for_jobs_now(db, now)?,
        next_reset: text(crate::quota::next_reset_utc(now)),
    })
}

/// Whether a job that ended belongs in the undo history: it changed something that can be undone,
/// it is not itself an undo, and it was not already reverted.
pub fn should_journal(job: &Job, items: &[JobItem]) -> bool {
    let flag = |key: &str| job.params.get(key).is_some_and(|v| !v.is_null() && v != &json!(false));
    !flag("undo_of_job_id")
        && !flag("revert_requested")
        && !flag("revert_job_id")
        && !flag("op_id")
        && items.iter().any(|i| i.status == JobItemStatus::Done && i.inverse.is_some())
}

/// The runner's `finished` hook in the app: the playlists it touched are re-read by any open
/// view, and the job goes in the undo history (its `op_id` kept in its params, for the command
/// that waited on it).
pub fn on_job_finished(state: &AppState, job: &Job) {
    let summary: Summary = job
        .params
        .get("summary")
        .and_then(|s| serde_json::from_value(s.clone()).ok())
        .unwrap_or_default();
    let touched: Vec<String> = summary.playlists.iter().map(|p| p.id.clone()).collect();
    if !touched.is_empty() {
        journal::announce(state, &touched);
    }
    let items = repo::list_job_items(&state.db, job.id).unwrap_or_default();
    if !should_journal(job, &items) {
        return;
    }
    let kind = job.params.get("op_kind").and_then(Value::as_str).unwrap_or(job.kind.as_str());
    let account = job.params.get("account").and_then(Value::as_str);
    let undo = [Step::RevertJob { job_id: job.id }];
    let Some(op) = journal::record_with_account(state, kind, &summary, &undo, account) else {
        return;
    };
    let params = set_param(job.params.clone(), "op_id", json!(op.id));
    if let Err(e) = repo::set_job_params(&state.db, job.id, &params) {
        tracing::warn!(error = %e, job = job.id, "jobs: could not keep the journal id");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jobs::PRIORITY_NORMAL;

    fn now() -> chrono::DateTime<chrono::Utc> {
        chrono::DateTime::parse_from_rfc3339("2026-07-15T12:00:00Z").unwrap().into()
    }

    fn db() -> Db {
        let db = Db::open(std::path::Path::new(":memory:")).unwrap();
        crate::ytdata_accounts::upsert(&db, "UC1", "Channel", None, 1).unwrap();
        db
    }

    fn seed(db: &Db, videos: &[&str]) -> i64 {
        let items = videos
            .iter()
            .map(|v| NewJobItem {
                phase: 1,
                action: "playlist_item_insert".into(),
                params: json!({"playlist_id": "PLdst", "video_id": v}),
            })
            .collect();
        let new = NewJob {
            account_id: Some("UC1".into()),
            kind: JobKind::CopyItems,
            params: json!({
                "engine": "ytdata",
                "account": "ga1",
                "summary": {"playlists": [], "count": 2},
            }),
            priority: PRIORITY_NORMAL,
            total_phases: 1,
            est_units_total: 50 * videos.len() as i64,
            items,
        };
        repo::insert_job(db, &new, now()).unwrap()
    }

    fn complete_all(db: &Db, job_id: i64) {
        while let Some(item) = repo::next_pending_item(db, job_id).unwrap() {
            repo::begin_item(db, item.id, now()).unwrap();
            let v = item.params["video_id"].as_str().unwrap().to_string();
            let inverse = json!({"action": "playlist_item_delete", "params": {
                "playlist_item_id": format!("pi-{v}"), "video_id": v, "playlist_id": "PLdst",
            }});
            repo::complete_item(db, item.id, None, Some(&inverse), 50, None, Some("UC1"), now())
                .unwrap();
        }
    }

    fn get(db: &Db, id: i64) -> Job {
        repo::get_job(db, id).unwrap().unwrap()
    }

    #[test]
    fn controls_delegate_and_priority_stays_off_system() {
        let db = db();
        let id = seed(&db, &["a"]);
        pause(&db, id).unwrap();
        assert_eq!(get(&db, id).status, JobStatus::PausedUser);
        resume(&db, id).unwrap();
        assert_eq!(get(&db, id).status, JobStatus::Queued);
        set_priority(&db, id, PRIORITY_SYSTEM).unwrap();
        assert_eq!(get(&db, id).priority, PRIORITY_HIGH);
        set_priority(&db, id, 9).unwrap();
        assert_eq!(get(&db, id).priority, PRIORITY_LOW);
        cancel(&db, id, false, now()).unwrap();
        assert_eq!(get(&db, id).status, JobStatus::Cancelled);
        assert!(materialize_pending_reverts(&db, now()).unwrap().is_empty());
    }

    #[test]
    fn a_revert_undoes_newest_first_once_and_inherits_engine_and_account() {
        let db = db();
        let id = seed(&db, &["a", "b"]);
        complete_all(&db, id);
        let preview = RevertPreview { item_count: 2, estimated_units: 100 };
        assert_eq!(revert_preview(&db, id).unwrap(), preview);
        cancel(&db, id, true, now()).unwrap();
        let made = materialize_pending_reverts(&db, now()).unwrap();
        assert_eq!(made.len(), 1);
        assert!(materialize_pending_reverts(&db, now()).unwrap().is_empty(), "only once");
        let revert = get(&db, made[0]);
        assert_eq!(revert.priority, PRIORITY_SYSTEM);
        assert_eq!(revert.kind, JobKind::Other(UNDO_KIND.into()));
        assert_eq!(revert.account_id.as_deref(), Some("UC1"));
        assert_eq!(revert.params["engine"], json!("ytdata"));
        assert_eq!(revert.params["account"], json!("ga1"));
        assert_eq!(revert.est_units_total, 100);
        let order: Vec<String> = repo::list_job_items(&db, made[0])
            .unwrap()
            .into_iter()
            .map(|i| i.params["video_id"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(order, ["b", "a"]);
        assert_eq!(get(&db, id).params["revert_job_id"], json!(made[0]));
        assert!(!should_journal(&get(&db, id), &repo::list_job_items(&db, id).unwrap()));
        assert!(!should_journal(&revert, &[]), "an undo is not journaled itself");
    }

    #[test]
    fn nothing_to_revert_makes_no_job() {
        let db = db();
        let id = seed(&db, &["a"]);
        assert_eq!(create_revert_job(&db, id, now()).unwrap(), None);
        assert_eq!(create_revert_job(&db, 999, now()).unwrap(), None);
        cancel(&db, id, true, now()).unwrap();
        assert!(materialize_pending_reverts(&db, now()).unwrap().is_empty());
        assert_eq!(get(&db, id).params["revert_requested"], json!(false));
    }

    #[test]
    fn a_finished_job_with_undoable_work_is_journaled_once() {
        let db = db();
        let id = seed(&db, &["a"]);
        assert!(!should_journal(&get(&db, id), &repo::list_job_items(&db, id).unwrap()));
        complete_all(&db, id);
        let job = get(&db, id);
        let items = repo::list_job_items(&db, id).unwrap();
        assert!(should_journal(&job, &items));
        let journaled = set_param(job.params.clone(), "op_id", json!(7));
        repo::set_job_params(&db, id, &journaled).unwrap();
        assert!(!should_journal(&get(&db, id), &items), "already in the history");
    }

    #[test]
    fn budget_partition_accounts_for_every_unit_spent_today() {
        let db = db();
        db.set_setting(super::super::budget::OPPORTUNISTIC_MODE_KEY, "false");
        let empty = budget_partition(&db, now()).unwrap();
        assert_eq!(
            (empty.daily_units, empty.spent_total, empty.safety_margin, empty.reserve_remaining),
            (10_000, 0, 300, 500)
        );
        assert_eq!(empty.available_for_jobs, 10_000 - 300 - 500);
        assert_eq!(empty.next_reset, "2026-07-16T07:00:00Z");

        // A job's write (ledger row with its job id), a backup run, and a sync read by hand.
        let id = seed(&db, &["a"]);
        complete_all(&db, id);
        db.record_monitor_run(&crate::db::MonitorRun {
            id: 0,
            started_at: now().timestamp(),
            finished_at: now().timestamp() + 5,
            trigger: "scheduler".into(),
            outcome: "ok".into(),
            playlists_ok: 1,
            playlists_failed: 0,
            alerts_new: 0,
            units_spent: 20,
            detail_json: "{}".into(),
        })
        .unwrap();
        crate::quota::record_units(&db, "playlistItems.list", 20, Some("UC1"), None, now())
            .unwrap();
        crate::quota::record_units(&db, "playlists.list", 3, Some("UC1"), None, now()).unwrap();

        let p = budget_partition(&db, now()).unwrap();
        assert_eq!((p.spent_total, p.spent_jobs, p.spent_backup, p.spent_other), (73, 50, 20, 3));
        assert_eq!(p.spent_jobs + p.spent_backup + p.spent_other, p.spent_total);
        assert_eq!(p.reserve_remaining, 480, "the reserve shrinks by what the backup spent");
        assert_eq!(p.available_for_jobs, 10_000 - 300 - 480 - 73);
        let snapshot = super::super::budget::snapshot(&db, now()).unwrap();
        assert_eq!(p.available_for_jobs, snapshot.available_for_jobs_now, "one rule, one number");
    }

    #[test]
    fn the_page_lists_active_jobs_in_run_order_and_ended_ones_newest_first() {
        let db = db();
        let low = seed(&db, &["a"]);
        set_priority(&db, low, PRIORITY_LOW).unwrap();
        let normal = seed(&db, &["b"]);
        let ended = seed(&db, &["c"]);
        complete_all(&db, ended);
        repo::finalize_if_complete(&db, ended, now()).unwrap();
        let cancelled = seed(&db, &["d"]);
        cancel(&db, cancelled, false, now()).unwrap();

        let ids = |v: Vec<JobView>| v.into_iter().map(|j| j.id).collect::<Vec<_>>();
        assert_eq!(ids(list(&db, ListFilter::Active, None).unwrap()), [normal, low]);
        assert_eq!(ids(list(&db, ListFilter::History, None).unwrap()), [cancelled, ended]);
        assert_eq!(ids(list(&db, ListFilter::History, Some(1)).unwrap()), [cancelled]);
        assert_eq!(ids(list(&db, ListFilter::All, None).unwrap()), [normal, low, cancelled, ended]);

        let view = &list(&db, ListFilter::History, Some(1)).unwrap()[0];
        assert_eq!(
            (view.status, view.engine, view.kind.as_str()),
            ("cancelled", "ytdata", "copy_items")
        );
        assert_eq!(view.summary.as_ref().map(|s| s.count), Some(2));
        assert!(!view.revert_requested);
    }

    #[test]
    fn a_jobs_detail_lists_its_items_and_what_a_revert_would_take() {
        let db = db();
        let id = seed(&db, &["a", "b"]);
        let item = repo::next_pending_item(&db, id).unwrap().unwrap();
        repo::begin_item(&db, item.id, now()).unwrap();
        repo::fail_item(&db, item.id, "videoNotFound", now()).unwrap();
        complete_all(&db, id);

        let d = detail(&db, id).unwrap().unwrap();
        assert_eq!(d.job.id, id);
        let items: Vec<_> = d
            .items
            .iter()
            .map(|i| (i.status, i.video_id.as_deref(), i.last_error.as_deref()))
            .collect();
        let failed = ("failed", Some("a"), Some("videoNotFound"));
        assert_eq!(items, [failed, ("done", Some("b"), None)]);
        assert_eq!(d.items[0].playlist_id.as_deref(), Some("PLdst"));
        assert_eq!(d.revert, RevertPreview { item_count: 1, estimated_units: 50 });
        assert!(detail(&db, 999).unwrap().is_none());
    }

    #[test]
    fn resume_waiting_auth_only_touches_that_account() {
        let db = db();
        crate::ytdata_accounts::upsert(&db, "UC2", "Other", None, 2).unwrap();
        let a = seed(&db, &["a"]);
        let mut other = repo::get_job(&db, seed(&db, &["b"])).unwrap().unwrap();
        db.conn().execute("UPDATE jobs SET account_id = 'UC2' WHERE id = ?1", [other.id]).unwrap();
        repo::set_waiting_auth(&db, a, "invalid_grant").unwrap();
        repo::set_waiting_auth(&db, other.id, "invalid_grant").unwrap();
        assert_eq!(resume_waiting_auth_for_account(&db, "UC1").unwrap(), 1);
        assert_eq!(get(&db, a).status, JobStatus::Queued);
        other = get(&db, other.id);
        assert_eq!(other.status, JobStatus::WaitingAuth);
        let summary = queue_summary(&db).unwrap();
        assert_eq!((summary.active, summary.waiting_auth), (1, 1));
    }
}
