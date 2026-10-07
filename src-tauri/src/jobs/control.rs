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
