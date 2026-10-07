//! `jobs` and `job_items`: the queue's state machine, one transaction per transition. Port of
//! PlaylistForge's `pf-core/src/db/repo/jobs.rs`.
//!
//! Status columns are free text with no `CHECK`; [`JobStatus::parse`] and
//! [`JobItemStatus::parse`] refuse an unknown value on read, and nothing outside this module
//! writes them.
//!
//! Phases (move's copy, verify, delete): a job starts with its first phase's items only, and the
//! runner appends the next phase with [`add_items`] once the barrier before it holds. A delete
//! item for a video whose copy never verified therefore never exists.

use chrono::{DateTime, Utc};
use rusqlite::{params, OptionalExtension, Row};

use super::{
    action_name, Job, JobFilter, JobItem, JobItemStatus, JobKind, JobStatus, NewJob, NewJobItem,
    PRIORITY_LOW, PRIORITY_SYSTEM,
};
use crate::db::{parse_rfc3339, rfc3339_text, Db};

fn json_to_text(value: &serde_json::Value) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "{}".to_string())
}

fn text_to_json(raw: &str) -> serde_json::Value {
    serde_json::from_str(raw).unwrap_or(serde_json::Value::Null)
}

const INSERT_ITEM: &str = "INSERT INTO job_items \
     (job_id, seq, phase, action, params_json, status, attempts, updated_at) \
     VALUES (?1, ?2, ?3, ?4, ?5, 'pending', 0, ?6)";

/// Inserts a job and its first items in one transaction. `total_items` and `planned_items`
/// start at the item count. Fails if `account_id` names no connected account.
pub fn insert_job(db: &Db, new: &NewJob, now: DateTime<Utc>) -> rusqlite::Result<i64> {
    let conn = db.conn();
    let tx = conn.unchecked_transaction()?;
    let count = new.items.len() as i64;
    tx.execute(
        "INSERT INTO jobs \
            (account_id, kind, params_json, status, priority, phase, total_phases, created_at, \
             est_units_total, spent_units, total_items, done_items, failed_items, skipped_items, \
             planned_items, retried_items) \
         VALUES (?1, ?2, ?3, 'queued', ?4, 1, ?5, ?6, ?7, 0, ?8, 0, 0, 0, ?8, 0)",
        params![
            new.account_id,
            new.kind.as_str(),
            json_to_text(&new.params),
            new.priority.clamp(PRIORITY_SYSTEM, PRIORITY_LOW),
            new.total_phases.max(1),
            rfc3339_text(now),
            new.est_units_total,
            count,
        ],
    )?;
    let job_id = tx.last_insert_rowid();
    {
        let mut stmt = tx.prepare(INSERT_ITEM)?;
        for (seq, item) in new.items.iter().enumerate() {
            stmt.execute(params![
                job_id,
                seq as i64,
                item.phase,
                item.action,
                json_to_text(&item.params),
                rfc3339_text(now),
            ])?;
        }
    }
    tx.commit()?;
    Ok(job_id)
}

/// Appends a later phase's items, continuing `seq` and growing `total_items`. Returns the new
/// ids in the order given.
pub fn add_items(
    db: &Db,
    job_id: i64,
    items: &[NewJobItem],
    now: DateTime<Utc>,
) -> rusqlite::Result<Vec<i64>> {
    if items.is_empty() {
        return Ok(Vec::new());
    }
    let conn = db.conn();
    let tx = conn.unchecked_transaction()?;
    let next_seq: i64 = tx.query_row(
        "SELECT COALESCE(MAX(seq), -1) + 1 FROM job_items WHERE job_id = ?1",
        params![job_id],
        |row| row.get(0),
    )?;
    let mut ids = Vec::with_capacity(items.len());
    {
        let mut stmt = tx.prepare(INSERT_ITEM)?;
        for (offset, item) in items.iter().enumerate() {
            stmt.execute(params![
                job_id,
                next_seq + offset as i64,
                item.phase,
                item.action,
                json_to_text(&item.params),
                rfc3339_text(now),
            ])?;
            ids.push(tx.last_insert_rowid());
        }
    }
    tx.execute(
        "UPDATE jobs SET total_items = total_items + ?2 WHERE id = ?1",
        params![job_id, items.len() as i64],
    )?;
    tx.commit()?;
    Ok(ids)
}

pub fn get_job(db: &Db, job_id: i64) -> rusqlite::Result<Option<Job>> {
    db.conn()
        .query_row(&format!("{JOB_SELECT} WHERE id = ?1"), params![job_id], row_to_job)
        .optional()
}

pub fn list_jobs(db: &Db, filter: &JobFilter) -> rusqlite::Result<Vec<Job>> {
    let mut sql = format!("{JOB_SELECT} WHERE 1 = 1");
    let mut bound: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
    if let Some(account_id) = &filter.account_id {
        sql.push_str(" AND account_id = ?");
        bound.push(Box::new(account_id.clone()));
    }
    if let Some(statuses) = filter.statuses.as_ref().filter(|s| !s.is_empty()) {
        let marks = std::iter::repeat_n("?", statuses.len()).collect::<Vec<_>>().join(",");
        sql.push_str(&format!(" AND status IN ({marks})"));
        bound.extend(statuses.iter().map(|s| Box::new(s.as_str()) as Box<dyn rusqlite::ToSql>));
    }
    sql.push_str(" ORDER BY created_at DESC, id DESC");
    if let Some(limit) = filter.limit {
        sql.push_str(" LIMIT ?");
        bound.push(Box::new(limit));
    }
    let conn = db.conn();
    let mut stmt = conn.prepare(&sql)?;
    let refs: Vec<&dyn rusqlite::ToSql> = bound.iter().map(|b| b.as_ref()).collect();
    let rows = stmt.query_map(refs.as_slice(), row_to_job)?;
    rows.collect()
}

/// The runner's pick: the `queued` or `running` job with the lowest `(priority, created_at)`.
/// Run [`promote_ready_jobs`] first so a wait that has ended is considered in the same pass.
pub fn next_eligible_job(db: &Db) -> rusqlite::Result<Option<Job>> {
    db.conn()
        .query_row(
            &format!(
                "{JOB_SELECT} WHERE status IN ('queued', 'running') \
                 ORDER BY priority ASC, created_at ASC, id ASC LIMIT 1"
            ),
            [],
            row_to_job,
        )
        .optional()
}

/// `waiting_quota` and `paused_network` jobs whose `resume_at` has passed go back to `queued`.
/// Returns how many did. `waiting_auth` never resumes by itself.
pub fn promote_ready_jobs(db: &Db, now: DateTime<Utc>) -> rusqlite::Result<usize> {
    db.conn().execute(
        "UPDATE jobs SET status = 'queued', resume_at = NULL \
         WHERE status IN ('waiting_quota', 'paused_network') \
           AND resume_at IS NOT NULL AND resume_at <= ?1",
        params![rfc3339_text(now)],
    )
}

pub fn list_job_items(db: &Db, job_id: i64) -> rusqlite::Result<Vec<JobItem>> {
    let conn = db.conn();
    let mut stmt =
        conn.prepare(&format!("{ITEM_SELECT} WHERE job_id = ?1 ORDER BY phase ASC, seq ASC"))?;
    let rows = stmt.query_map(params![job_id], row_to_item)?;
    rows.collect()
}

pub fn get_job_item(db: &Db, item_id: i64) -> rusqlite::Result<Option<JobItem>> {
    db.conn()
        .query_row(&format!("{ITEM_SELECT} WHERE id = ?1"), params![item_id], row_to_item)
        .optional()
}

/// The job's next `pending` item, lowest phase and seq first.
pub fn next_pending_item(db: &Db, job_id: i64) -> rusqlite::Result<Option<JobItem>> {
    db.conn()
        .query_row(
            &format!(
                "{ITEM_SELECT} WHERE job_id = ?1 AND status = 'pending' \
                 ORDER BY phase ASC, seq ASC LIMIT 1"
            ),
            params![job_id],
            row_to_item,
        )
        .optional()
}

/// Every `in_flight` item of every job: what a starting runner has to reconcile first, since
/// the call behind each may or may not have reached Google.
pub fn list_in_flight_items(db: &Db) -> rusqlite::Result<Vec<JobItem>> {
    let conn = db.conn();
    let mut stmt = conn.prepare(&format!(
        "{ITEM_SELECT} WHERE status = 'in_flight' ORDER BY job_id ASC, phase ASC, seq ASC"
    ))?;
    let rows = stmt.query_map([], row_to_item)?;
    rows.collect()
}

/// Whether every item so far at phase `<= phase` is terminal (none `pending` or `in_flight`).
pub fn phase_items_all_terminal(db: &Db, job_id: i64, phase: i64) -> rusqlite::Result<bool> {
    let open: i64 = db.conn().query_row(
        "SELECT COUNT(*) FROM job_items \
         WHERE job_id = ?1 AND phase <= ?2 AND status IN ('pending', 'in_flight')",
        params![job_id, phase],
        |row| row.get(0),
    )?;
    Ok(open == 0)
}

/// `pending -> in_flight`, before the call is made. Bumps `attempts`, counts the item as retried
/// on its second attempt (once), and starts the job (`running`, `started_at` the first time).
/// `false`, touching nothing, if the item was not `pending`.
pub fn begin_item(db: &Db, item_id: i64, now: DateTime<Utc>) -> rusqlite::Result<bool> {
    let conn = db.conn();
    let tx = conn.unchecked_transaction()?;
    let claimed: Option<(i64, i64, String)> = tx
        .query_row(
            "SELECT job_id, attempts, action FROM job_items WHERE id = ?1 AND status = 'pending'",
            params![item_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()?;
    let Some((job_id, prior_attempts, action)) = claimed else {
        return Ok(false);
    };
    tx.execute(
        "UPDATE job_items SET status = 'in_flight', attempts = attempts + 1, updated_at = ?2 \
         WHERE id = ?1",
        params![item_id, rfc3339_text(now)],
    )?;
    if prior_attempts == 1 && action != action_name::VERIFY_DESTINATION {
        tx.execute(
            "UPDATE jobs SET retried_items = retried_items + 1 WHERE id = ?1",
            params![job_id],
        )?;
    }
    tx.execute(
        "UPDATE jobs SET status = 'running', started_at = COALESCE(started_at, ?2) \
         WHERE id = ?1 AND status IN ('queued', 'running')",
        params![job_id, rfc3339_text(now)],
    )?;
    tx.commit()?;
    Ok(true)
}

/// `in_flight -> done`, with its quota ledger row in the same transaction: the units are
/// recorded if and only if the item is. `units_spent` 0 writes no ledger row.
#[allow(clippy::too_many_arguments)]
pub fn complete_item(
    db: &Db,
    item_id: i64,
    api_result: Option<&serde_json::Value>,
    inverse: Option<&serde_json::Value>,
    units_spent: i64,
    endpoint: Option<&str>,
    account_id: Option<&str>,
    now: DateTime<Utc>,
) -> rusqlite::Result<()> {
    let conn = db.conn();
    let tx = conn.unchecked_transaction()?;
    let job_id: i64 =
        tx.query_row("SELECT job_id FROM job_items WHERE id = ?1", params![item_id], |row| {
            row.get(0)
        })?;
    tx.execute(
        "UPDATE job_items SET status = 'done', api_result_json = ?2, inverse_json = ?3, \
         updated_at = ?4 WHERE id = ?1",
        params![
            item_id,
            api_result.map(json_to_text),
            inverse.map(json_to_text),
            rfc3339_text(now)
        ],
    )?;
    if units_spent > 0 {
        tx.execute(
            "INSERT INTO quota_ledger (ts, endpoint, units, account_id, job_id) \
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                rfc3339_text(now),
                endpoint.unwrap_or("unknown"),
                units_spent,
                account_id,
                job_id
            ],
        )?;
    }
    tx.execute(
        "UPDATE jobs SET spent_units = spent_units + ?2, done_items = done_items + 1 WHERE id = ?1",
        params![job_id, units_spent],
    )?;
    tx.commit()
}

/// `in_flight` (or `done`) `-> pending`: a retryable failure, or the verify barrier sending an
/// unconfirmed copy back. Keeps `attempts`; un-counts a `done` item so `done_items` stays "items
/// done now", never "completions ever".
pub fn requeue_item(
    db: &Db,
    item_id: i64,
    reason: Option<&str>,
    now: DateTime<Utc>,
) -> rusqlite::Result<()> {
    let conn = db.conn();
    let tx = conn.unchecked_transaction()?;
    let was_done: bool = tx
        .query_row("SELECT status = 'done' FROM job_items WHERE id = ?1", params![item_id], |r| {
            r.get(0)
        })
        .optional()?
        .unwrap_or(false);
    tx.execute(
        "UPDATE job_items SET status = 'pending', last_error = ?2, updated_at = ?3 WHERE id = ?1",
        params![item_id, reason, rfc3339_text(now)],
    )?;
    if was_done {
        tx.execute(
            "UPDATE jobs SET done_items = done_items - 1 \
             WHERE id = (SELECT job_id FROM job_items WHERE id = ?1)",
            params![item_id],
        )?;
    }
    tx.commit()
}

/// Moves an item to a terminal failure status and bumps the job's matching counter. The job's
/// own status is left alone: it carries on.
fn finish_item_badly(
    db: &Db,
    item_id: i64,
    status: JobItemStatus,
    counter: &str,
    reason: &str,
    now: DateTime<Utc>,
) -> rusqlite::Result<()> {
    let conn = db.conn();
    let tx = conn.unchecked_transaction()?;
    let job_id: i64 =
        tx.query_row("SELECT job_id FROM job_items WHERE id = ?1", params![item_id], |row| {
            row.get(0)
        })?;
    tx.execute(
        "UPDATE job_items SET status = ?2, last_error = ?3, updated_at = ?4 WHERE id = ?1",
        params![item_id, status.as_str(), reason, rfc3339_text(now)],
    )?;
    tx.execute(&format!("UPDATE jobs SET {counter} = {counter} + 1 WHERE id = ?1"), [job_id])?;
    tx.commit()
}

/// `in_flight -> skipped`: a permanent failure of this item alone (the video is gone).
pub fn skip_item(db: &Db, item_id: i64, reason: &str, now: DateTime<Utc>) -> rusqlite::Result<()> {
    finish_item_badly(db, item_id, JobItemStatus::Skipped, "skipped_items", reason, now)
}

/// `in_flight -> failed`: attempts exhausted.
pub fn fail_item(db: &Db, item_id: i64, reason: &str, now: DateTime<Utc>) -> rusqlite::Result<()> {
    finish_item_badly(db, item_id, JobItemStatus::Failed, "failed_items", reason, now)
}

/// The next phase, never past `total_phases`.
pub fn advance_phase(db: &Db, job_id: i64) -> rusqlite::Result<()> {
    db.conn().execute(
        "UPDATE jobs SET phase = phase + 1 WHERE id = ?1 AND phase < total_phases",
        params![job_id],
    )?;
    Ok(())
}

/// Replaces `params_json` whole (no merge).
pub fn set_job_params(db: &Db, job_id: i64, params: &serde_json::Value) -> rusqlite::Result<()> {
    db.conn().execute(
        "UPDATE jobs SET params_json = ?2 WHERE id = ?1",
        params![job_id, json_to_text(params)],
    )?;
    Ok(())
}

/// Sets the status, stamping `finished_at` for a terminal one and clearing it otherwise (a
/// revived job must not keep an old finish date).
pub fn set_job_status(
    db: &Db,
    job_id: i64,
    status: JobStatus,
    last_error: Option<&str>,
    now: DateTime<Utc>,
) -> rusqlite::Result<()> {
    let finished_at = status.is_terminal().then(|| rfc3339_text(now));
    db.conn().execute(
        "UPDATE jobs SET status = ?2, last_error = ?3, finished_at = ?4 WHERE id = ?1",
        params![job_id, status.as_str(), last_error, finished_at],
    )?;
    Ok(())
}

/// `-> waiting_quota` until `resume_at` (the next reset). The item that hit the limit goes back
/// to `pending` through [`requeue_item`].
pub fn set_waiting_quota(db: &Db, job_id: i64, resume_at: DateTime<Utc>) -> rusqlite::Result<()> {
    db.conn().execute(
        "UPDATE jobs SET status = 'waiting_quota', resume_at = ?2 WHERE id = ?1",
        params![job_id, rfc3339_text(resume_at)],
    )?;
    Ok(())
}

/// `-> waiting_auth`. No `resume_at`: only reconnecting the account and [`resume_job`] end it.
pub fn set_waiting_auth(db: &Db, job_id: i64, reason: &str) -> rusqlite::Result<()> {
    db.conn().execute(
        "UPDATE jobs SET status = 'waiting_auth', last_error = ?2 WHERE id = ?1",
        params![job_id, reason],
    )?;
    Ok(())
}

/// `-> paused_network` until `resume_at` (now plus the backoff).
pub fn set_paused_network(
    db: &Db,
    job_id: i64,
    resume_at: DateTime<Utc>,
    reason: &str,
) -> rusqlite::Result<()> {
    db.conn().execute(
        "UPDATE jobs SET status = 'paused_network', resume_at = ?2, last_error = ?3 WHERE id = ?1",
        params![job_id, rfc3339_text(resume_at), reason],
    )?;
    Ok(())
}

/// A starting runner's second step (after reconciling in-flight items): every job left
/// `running` or `verifying` by the process before goes back to `queued`. Returns how many.
pub fn requeue_running_jobs(db: &Db) -> rusqlite::Result<usize> {
    db.conn()
        .execute("UPDATE jobs SET status = 'queued' WHERE status IN ('running', 'verifying')", [])
}

/// Replaces an item's `params_json` whole (a Data API delete once its row is resolved, an
/// interrupted InnerTube add made safe to repeat).
pub fn set_item_params(db: &Db, item_id: i64, params: &serde_json::Value) -> rusqlite::Result<()> {
    db.conn().execute(
        "UPDATE job_items SET params_json = ?2 WHERE id = ?1",
        params![item_id, json_to_text(params)],
    )?;
    Ok(())
}

/// The earliest `resume_at` of a job that will resume by itself: when the runner next has to look.
pub fn next_resume_at(db: &Db) -> rusqlite::Result<Option<DateTime<Utc>>> {
    let at: Option<String> = db.conn().query_row(
        "SELECT MIN(resume_at) FROM jobs \
         WHERE status IN ('waiting_quota', 'paused_network') AND resume_at IS NOT NULL",
        [],
        |row| row.get(0),
    )?;
    Ok(at.map(|s| parse_rfc3339(&s)))
}

/// The user's pause, from `queued`, `running`, `waiting_quota` or `paused_network`.
pub fn pause_job(db: &Db, job_id: i64) -> rusqlite::Result<()> {
    db.conn().execute(
        "UPDATE jobs SET status = 'paused_user', resume_at = NULL \
         WHERE id = ?1 AND status IN ('queued', 'running', 'waiting_quota', 'paused_network')",
        params![job_id],
    )?;
    Ok(())
}

/// The user's resume, from `paused_user` or `waiting_auth`, back to `queued`.
pub fn resume_job(db: &Db, job_id: i64) -> rusqlite::Result<()> {
    db.conn().execute(
        "UPDATE jobs SET status = 'queued', resume_at = NULL \
         WHERE id = ?1 AND status IN ('paused_user', 'waiting_auth')",
        params![job_id],
    )?;
    Ok(())
}

/// The user's cancel, from any non-terminal status. Items are left as they are: what completed
/// keeps its `inverse_json` for an undo.
pub fn cancel_job(db: &Db, job_id: i64, now: DateTime<Utc>) -> rusqlite::Result<()> {
    db.conn().execute(
        "UPDATE jobs SET status = 'cancelled', finished_at = ?2, resume_at = NULL \
         WHERE id = ?1 \
           AND status NOT IN ('completed', 'completed_with_errors', 'failed', 'cancelled')",
        params![job_id, rfc3339_text(now)],
    )?;
    Ok(())
}

/// Sets the priority, clamped to SYSTEM..=LOW: there is no `CHECK` behind the column.
pub fn reprioritize_job(db: &Db, job_id: i64, priority: i64) -> rusqlite::Result<()> {
    db.conn().execute(
        "UPDATE jobs SET priority = ?2 WHERE id = ?1",
        params![job_id, priority.clamp(PRIORITY_SYSTEM, PRIORITY_LOW)],
    )?;
    Ok(())
}

/// The user's drag in the queue: `ids` in the order wanted. The runner orders by `(priority,
/// created_at, id)`, so within each priority level the named jobs swap their `created_at` values
/// to follow `ids` (kept strictly increasing, a second apart where two were equal); priorities are
/// left alone, so a job never jumps a level by being dragged. Unknown, repeated and ended jobs are
/// ignored. Returns how many jobs changed.
pub fn reorder_queue(db: &Db, ids: &[i64]) -> rusqlite::Result<usize> {
    let conn = db.conn();
    let tx = conn.unchecked_transaction()?;
    // (id, priority, created_at) of each named open job, in the order given.
    let mut rows: Vec<(i64, i64, String)> = Vec::new();
    {
        let mut stmt = tx.prepare(
            "SELECT priority, created_at FROM jobs WHERE id = ?1 \
               AND status NOT IN ('completed', 'completed_with_errors', 'failed', 'cancelled')",
        )?;
        for &id in ids {
            if rows.iter().any(|r| r.0 == id) {
                continue;
            }
            let found: Option<(i64, String)> =
                stmt.query_row(params![id], |row| Ok((row.get(0)?, row.get(1)?))).optional()?;
            if let Some((priority, created_at)) = found {
                rows.push((id, priority, created_at));
            }
        }
    }
    let mut levels: Vec<i64> = rows.iter().map(|r| r.1).collect();
    levels.sort_unstable();
    levels.dedup();
    let mut changed = 0;
    for level in levels {
        let group: Vec<&(i64, i64, String)> = rows.iter().filter(|r| r.1 == level).collect();
        let mut stamps: Vec<DateTime<Utc>> = group.iter().map(|r| parse_rfc3339(&r.2)).collect();
        stamps.sort_unstable();
        let mut prev: Option<DateTime<Utc>> = None;
        for (row, stamp) in group.iter().zip(stamps) {
            let at = match prev {
                Some(p) if stamp <= p => p + chrono::Duration::seconds(1),
                _ => stamp,
            };
            prev = Some(at);
            let text = rfc3339_text(at);
            if text != row.2 {
                tx.execute("UPDATE jobs SET created_at = ?2 WHERE id = ?1", params![row.0, text])?;
                changed += 1;
            }
        }
    }
    tx.commit()?;
    Ok(changed)
}

/// "Retry failed": every `failed` item back to `pending` with a fresh attempt count, and a job
/// that had ended (`failed`, `completed_with_errors`, `cancelled`) back to `queued`. Returns how
/// many items were retried.
pub fn retry_failed_items(db: &Db, job_id: i64, now: DateTime<Utc>) -> rusqlite::Result<usize> {
    let conn = db.conn();
    let tx = conn.unchecked_transaction()?;
    let retried = tx.execute(
        "UPDATE job_items SET status = 'pending', attempts = 0, last_error = NULL, updated_at = ?2 \
         WHERE job_id = ?1 AND status = 'failed'",
        params![job_id, rfc3339_text(now)],
    )?;
    if retried > 0 {
        tx.execute(
            "UPDATE jobs SET failed_items = MAX(failed_items - ?2, 0) WHERE id = ?1",
            params![job_id, retried as i64],
        )?;
        tx.execute(
            "UPDATE jobs SET status = 'queued', finished_at = NULL, resume_at = NULL \
             WHERE id = ?1 AND status IN ('failed', 'completed_with_errors', 'cancelled')",
            params![job_id],
        )?;
    }
    tx.commit()?;
    Ok(retried)
}

/// Ends the job once its last phase is reached and every item is terminal: `completed`, or
/// `completed_with_errors` if anything failed or was skipped. `None` if it was not ready (or had
/// already ended).
pub fn finalize_if_complete(
    db: &Db,
    job_id: i64,
    now: DateTime<Utc>,
) -> rusqlite::Result<Option<JobStatus>> {
    let Some(job) = get_job(db, job_id)? else { return Ok(None) };
    if job.status.is_terminal()
        || job.phase < job.total_phases
        || !phase_items_all_terminal(db, job_id, job.total_phases)?
    {
        return Ok(None);
    }
    let status = if job.failed_items > 0 || job.skipped_items > 0 {
        JobStatus::CompletedWithErrors
    } else {
        JobStatus::Completed
    };
    set_job_status(db, job_id, status, None, now)?;
    Ok(Some(status))
}

/// Deletes a job and its items. The items are deleted explicitly, in the same transaction,
/// rather than left to `ON DELETE CASCADE` alone; the job's ledger rows stay, unlinked
/// (`ON DELETE SET NULL`). Returns whether the job existed.
pub fn delete_job(db: &Db, job_id: i64) -> rusqlite::Result<bool> {
    let conn = db.conn();
    let tx = conn.unchecked_transaction()?;
    tx.execute("DELETE FROM job_items WHERE job_id = ?1", params![job_id])?;
    let deleted = tx.execute("DELETE FROM jobs WHERE id = ?1", params![job_id])?;
    tx.commit()?;
    Ok(deleted > 0)
}

/// Deletes every terminal job that finished before `before`, with its items (history pruning).
/// Returns how many jobs went.
pub fn delete_finished_before(db: &Db, before: DateTime<Utc>) -> rusqlite::Result<usize> {
    let conn = db.conn();
    let tx = conn.unchecked_transaction()?;
    let finished = "SELECT id FROM jobs WHERE finished_at IS NOT NULL AND finished_at < ?1 \
         AND status IN ('completed', 'completed_with_errors', 'failed', 'cancelled')";
    let cutoff = rfc3339_text(before);
    tx.execute(&format!("DELETE FROM job_items WHERE job_id IN ({finished})"), [&cutoff])?;
    let deleted = tx.execute(&format!("DELETE FROM jobs WHERE id IN ({finished})"), [&cutoff])?;
    tx.commit()?;
    Ok(deleted)
}

const JOB_SELECT: &str = "SELECT id, account_id, kind, params_json, status, priority, phase, \
     total_phases, created_at, started_at, finished_at, resume_at, est_units_total, spent_units, \
     total_items, done_items, failed_items, skipped_items, planned_items, retried_items, \
     last_error FROM jobs";

const ITEM_SELECT: &str = "SELECT id, job_id, seq, phase, action, params_json, status, \
     api_result_json, inverse_json, attempts, last_error, updated_at FROM job_items";

fn unknown(column: usize, what: &str, raw: &str) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(
        column,
        rusqlite::types::Type::Text,
        format!("unknown {what}: {raw}").into(),
    )
}

fn row_to_job(row: &Row<'_>) -> rusqlite::Result<Job> {
    let kind: String = row.get(2)?;
    let params: String = row.get(3)?;
    let status: String = row.get(4)?;
    let date = |i: usize| -> rusqlite::Result<Option<DateTime<Utc>>> {
        Ok(row.get::<_, Option<String>>(i)?.map(|s| parse_rfc3339(&s)))
    };
    Ok(Job {
        id: row.get(0)?,
        account_id: row.get(1)?,
        kind: JobKind::parse(&kind),
        params: text_to_json(&params),
        status: JobStatus::parse(&status).ok_or_else(|| unknown(4, "job status", &status))?,
        priority: row.get(5)?,
        phase: row.get(6)?,
        total_phases: row.get(7)?,
        created_at: parse_rfc3339(&row.get::<_, String>(8)?),
        started_at: date(9)?,
        finished_at: date(10)?,
        resume_at: date(11)?,
        est_units_total: row.get(12)?,
        spent_units: row.get(13)?,
        total_items: row.get(14)?,
        done_items: row.get(15)?,
        failed_items: row.get(16)?,
        skipped_items: row.get(17)?,
        planned_items: row.get(18)?,
        retried_items: row.get(19)?,
        last_error: row.get(20)?,
    })
}

fn row_to_item(row: &Row<'_>) -> rusqlite::Result<JobItem> {
    let params: String = row.get(5)?;
    let status: String = row.get(6)?;
    let api_result: Option<String> = row.get(7)?;
    let inverse: Option<String> = row.get(8)?;
    Ok(JobItem {
        id: row.get(0)?,
        job_id: row.get(1)?,
        seq: row.get(2)?,
        phase: row.get(3)?,
        action: row.get(4)?,
        params: text_to_json(&params),
        status: JobItemStatus::parse(&status)
            .ok_or_else(|| unknown(6, "job item status", &status))?,
        api_result: api_result.map(|s| text_to_json(&s)),
        inverse: inverse.map(|s| text_to_json(&s)),
        attempts: row.get(9)?,
        last_error: row.get(10)?,
        updated_at: parse_rfc3339(&row.get::<_, String>(11)?),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jobs::{PRIORITY_HIGH, PRIORITY_NORMAL};
    use serde_json::json;

    fn dt(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }
    fn now() -> DateTime<Utc> {
        dt("2026-07-15T12:00:00Z")
    }
    fn later() -> DateTime<Utc> {
        now() + chrono::Duration::minutes(5)
    }

    /// A database with the Data API account `UC1` connected (jobs reference it).
    fn db() -> Db {
        let db = Db::open(std::path::Path::new(":memory:")).unwrap();
        crate::ytdata_accounts::upsert(&db, "UC1", "Channel", None, 1).unwrap();
        db
    }

    fn sample_job(items: usize) -> NewJob {
        NewJob {
            account_id: Some("UC1".to_string()),
            kind: JobKind::CopyItems,
            params: json!({"dest": "PL2"}),
            priority: PRIORITY_NORMAL,
            total_phases: 1,
            est_units_total: 50 * items as i64,
            items: (0..items)
                .map(|i| NewJobItem {
                    phase: 1,
                    action: "playlist_item_insert".to_string(),
                    params: json!({"video_id": format!("v{i}")}),
                })
                .collect(),
        }
    }

    fn job(db: &Db, id: i64) -> Job {
        get_job(db, id).unwrap().unwrap()
    }

    fn complete(db: &Db, item_id: i64, at: DateTime<Utc>) {
        complete_item(db, item_id, None, None, 50, Some("playlistItems.insert"), Some("UC1"), at)
            .unwrap();
    }

    #[test]
    fn insert_job_persists_job_and_items() {
        let db = db();
        let job_id = insert_job(&db, &sample_job(3), now()).unwrap();

        let j = job(&db, job_id);
        assert_eq!(j.status, JobStatus::Queued);
        assert_eq!(j.kind, JobKind::CopyItems);
        assert_eq!(j.total_items, 3);
        assert_eq!(j.planned_items, 3);
        assert_eq!(j.phase, 1);
        assert_eq!(j.params["dest"], json!("PL2"));
        assert_eq!(j.created_at, now());
        assert_eq!(j.account_id.as_deref(), Some("UC1"));

        let items = list_job_items(&db, job_id).unwrap();
        assert_eq!(items.len(), 3);
        assert_eq!(items[0].seq, 0);
        assert_eq!(items[0].status, JobItemStatus::Pending);
        assert_eq!(items[2].params["video_id"], json!("v2"));
    }

    #[test]
    fn a_job_keeps_its_engine_in_params() {
        let db = db();
        let mut new = sample_job(0);
        new.params = json!({"engine": "innertube", "dest": "PL2"});
        new.account_id = None;
        let job_id = insert_job(&db, &new, now()).unwrap();
        let j = job(&db, job_id);
        assert_eq!(j.engine(), Some("innertube"));
        assert_eq!(j.account_id, None, "a job needs no Data API account");
        assert_eq!(job(&db, insert_job(&db, &sample_job(0), now()).unwrap()).engine(), None);
    }

    #[test]
    fn a_job_for_an_unknown_account_is_refused() {
        let db = db();
        let mut new = sample_job(1);
        new.account_id = Some("UC-nobody".into());
        assert!(insert_job(&db, &new, now()).is_err());
        assert!(list_jobs(&db, &JobFilter::default()).unwrap().is_empty(), "nothing half-written");
        let items: i64 =
            db.conn().query_row("SELECT COUNT(*) FROM job_items", [], |r| r.get(0)).unwrap();
        assert_eq!(items, 0);
    }

    /// PlaylistForge's "14/13" regression: a done copy requeued by the verify barrier must be
    /// un-counted, and its re-run lands in `retried_items`.
    #[test]
    fn a_verify_requeue_of_a_done_item_never_double_counts_it_and_lands_in_retried() {
        let db = db();
        let job_id = insert_job(&db, &sample_job(2), now()).unwrap();
        let items = list_job_items(&db, job_id).unwrap();
        for item in &items {
            assert!(begin_item(&db, item.id, now()).unwrap());
            complete(&db, item.id, now());
        }
        assert_eq!(job(&db, job_id).done_items, 2);

        requeue_item(&db, items[0].id, Some("verify: not found"), later()).unwrap();
        let j = job(&db, job_id);
        assert_eq!(j.done_items, 1, "an un-done item is un-counted");
        assert_eq!(j.retried_items, 0, "requeueing alone is not yet a retry");

        assert!(begin_item(&db, items[0].id, later()).unwrap());
        complete(&db, items[0].id, later());
        let j = job(&db, job_id);
        assert_eq!(j.done_items, 2);
        assert_eq!(j.total_items, 2);
        assert_eq!(j.retried_items, 1);

        requeue_item(&db, items[0].id, None, later()).unwrap();
        assert!(begin_item(&db, items[0].id, later()).unwrap());
        assert_eq!(job(&db, job_id).retried_items, 1, "retried counts items, not attempts");
    }

    #[test]
    fn the_verify_barriers_own_recheck_is_not_counted_as_a_retry() {
        let db = db();
        let mut new = sample_job(1);
        new.items.push(NewJobItem {
            phase: 1,
            action: action_name::VERIFY_DESTINATION.to_string(),
            params: json!({}),
        });
        let job_id = insert_job(&db, &new, now()).unwrap();
        let verify = list_job_items(&db, job_id).unwrap()[1].clone();

        assert!(begin_item(&db, verify.id, now()).unwrap());
        requeue_item(&db, verify.id, Some("verify: some copies unconfirmed"), now()).unwrap();
        assert!(begin_item(&db, verify.id, later()).unwrap());
        assert_eq!(job(&db, job_id).retried_items, 0);
    }

    #[test]
    fn planned_items_stays_frozen_while_total_items_grows_with_appended_phases() {
        let db = db();
        let job_id = insert_job(&db, &sample_job(6), now()).unwrap();
        let verify = NewJobItem {
            phase: 2,
            action: action_name::VERIFY_DESTINATION.to_string(),
            params: json!({}),
        };
        add_items(&db, job_id, &[verify], now()).unwrap();
        let deletes: Vec<NewJobItem> = (0..6)
            .map(|i| NewJobItem {
                phase: 3,
                action: "playlist_item_delete".to_string(),
                params: json!({"video_id": format!("v{i}")}),
            })
            .collect();
        add_items(&db, job_id, &deletes, now()).unwrap();

        let j = job(&db, job_id);
        assert_eq!(j.total_items, 13, "6 copies + 1 verify + 6 deletes");
        assert_eq!(j.planned_items, 6);
    }

    #[test]
    fn begin_item_marks_in_flight_and_starts_the_job() {
        let db = db();
        let job_id = insert_job(&db, &sample_job(1), now()).unwrap();
        let item = next_pending_item(&db, job_id).unwrap().unwrap();

        assert!(begin_item(&db, item.id, now()).unwrap());
        let j = job(&db, job_id);
        assert_eq!(j.status, JobStatus::Running);
        assert_eq!(j.started_at, Some(now()));
        let refreshed = get_job_item(&db, item.id).unwrap().unwrap();
        assert_eq!(refreshed.status, JobItemStatus::InFlight);
        assert_eq!(refreshed.attempts, 1);
    }

    #[test]
    fn begin_item_is_a_no_op_when_not_pending() {
        let db = db();
        let job_id = insert_job(&db, &sample_job(1), now()).unwrap();
        let item = next_pending_item(&db, job_id).unwrap().unwrap();
        assert!(begin_item(&db, item.id, now()).unwrap());
        assert!(!begin_item(&db, item.id, later()).unwrap());
        assert_eq!(get_job_item(&db, item.id).unwrap().unwrap().attempts, 1);
    }

    #[test]
    fn complete_item_writes_ledger_entry_in_the_same_transaction() {
        let db = db();
        let job_id = insert_job(&db, &sample_job(1), now()).unwrap();
        let item = next_pending_item(&db, job_id).unwrap().unwrap();
        begin_item(&db, item.id, now()).unwrap();

        complete_item(
            &db,
            item.id,
            Some(&json!({"playlist_item_id": "new1"})),
            Some(&json!({"action": "playlist_item_delete", "playlist_item_id": "new1"})),
            50,
            Some("playlistItems.insert"),
            Some("UC1"),
            later(),
        )
        .unwrap();

        let refreshed = get_job_item(&db, item.id).unwrap().unwrap();
        assert_eq!(refreshed.status, JobItemStatus::Done);
        assert_eq!(refreshed.api_result.unwrap()["playlist_item_id"], json!("new1"));
        assert_eq!(refreshed.inverse.unwrap()["action"], json!("playlist_item_delete"));
        let j = job(&db, job_id);
        assert_eq!(j.spent_units, 50);
        assert_eq!(j.done_items, 1);
        assert_eq!(crate::quota::spent_today(&db, later()).unwrap(), 50);
        let entries = crate::quota::entries_today(&db, later()).unwrap();
        assert_eq!(entries[0].job_id, Some(job_id));
    }

    #[test]
    fn a_failing_complete_item_records_no_units() {
        let db = db();
        // No such item: the transaction fails before anything is written.
        assert!(complete_item(&db, 999, None, None, 50, Some("x"), None, now()).is_err());
        assert_eq!(crate::quota::spent_today(&db, now()).unwrap(), 0);
    }

    #[test]
    fn skip_item_increments_skipped_and_leaves_job_running() {
        let db = db();
        let job_id = insert_job(&db, &sample_job(2), now()).unwrap();
        let item = next_pending_item(&db, job_id).unwrap().unwrap();
        begin_item(&db, item.id, now()).unwrap();

        skip_item(&db, item.id, "videoNotFound", later()).unwrap();
        let refreshed = get_job_item(&db, item.id).unwrap().unwrap();
        assert_eq!(refreshed.status, JobItemStatus::Skipped);
        assert_eq!(refreshed.last_error.as_deref(), Some("videoNotFound"));
        let j = job(&db, job_id);
        assert_eq!(j.skipped_items, 1);
        assert_eq!(j.status, JobStatus::Running, "the job keeps going");
    }

    #[test]
    fn requeue_item_returns_it_to_pending_without_resetting_attempts() {
        let db = db();
        let job_id = insert_job(&db, &sample_job(1), now()).unwrap();
        let item = next_pending_item(&db, job_id).unwrap().unwrap();
        begin_item(&db, item.id, now()).unwrap();

        requeue_item(&db, item.id, Some("network error"), later()).unwrap();
        let refreshed = get_job_item(&db, item.id).unwrap().unwrap();
        assert_eq!(refreshed.status, JobItemStatus::Pending);
        assert_eq!(refreshed.attempts, 1);
        assert_eq!(refreshed.last_error.as_deref(), Some("network error"));
    }

    #[test]
    fn fail_item_increments_failed_and_job_keeps_going() {
        let db = db();
        let job_id = insert_job(&db, &sample_job(1), now()).unwrap();
        let item = next_pending_item(&db, job_id).unwrap().unwrap();
        begin_item(&db, item.id, now()).unwrap();

        fail_item(&db, item.id, "persistent 500", later()).unwrap();
        let j = job(&db, job_id);
        assert_eq!(j.failed_items, 1);
        assert!(!j.status.is_terminal());
    }

    #[test]
    fn waiting_quota_transition_sets_resume_at_and_promote_ready_jobs_reverts_it() {
        let db = db();
        let job_id = insert_job(&db, &sample_job(1), now()).unwrap();
        let resume_at = dt("2026-07-16T07:00:00Z");
        set_waiting_quota(&db, job_id, resume_at).unwrap();
        let j = job(&db, job_id);
        assert_eq!(j.status, JobStatus::WaitingQuota);
        assert_eq!(j.resume_at, Some(resume_at));

        promote_ready_jobs(&db, resume_at - chrono::Duration::minutes(1)).unwrap();
        assert_eq!(job(&db, job_id).status, JobStatus::WaitingQuota);
        assert!(next_eligible_job(&db).unwrap().is_none());

        assert_eq!(promote_ready_jobs(&db, resume_at).unwrap(), 1);
        let j = job(&db, job_id);
        assert_eq!(j.status, JobStatus::Queued);
        assert_eq!(j.resume_at, None);
        assert_eq!(next_eligible_job(&db).unwrap().unwrap().id, job_id);
    }

    #[test]
    fn waiting_auth_never_auto_promotes() {
        let db = db();
        let job_id = insert_job(&db, &sample_job(1), now()).unwrap();
        set_waiting_auth(&db, job_id, "invalid_grant").unwrap();

        promote_ready_jobs(&db, now() + chrono::Duration::days(30)).unwrap();
        assert_eq!(job(&db, job_id).status, JobStatus::WaitingAuth);
        resume_job(&db, job_id).unwrap();
        assert_eq!(job(&db, job_id).status, JobStatus::Queued);
    }

    #[test]
    fn paused_network_auto_resumes_via_promote_ready_jobs() {
        let db = db();
        let job_id = insert_job(&db, &sample_job(1), now()).unwrap();
        let resume_at = now() + chrono::Duration::minutes(2);
        set_paused_network(&db, job_id, resume_at, "connection reset").unwrap();

        promote_ready_jobs(&db, resume_at).unwrap();
        assert_eq!(job(&db, job_id).status, JobStatus::Queued);
    }

    #[test]
    fn next_eligible_job_orders_by_priority_then_created_at() {
        let db = db();
        let mut low = sample_job(1);
        low.priority = PRIORITY_LOW;
        insert_job(&db, &low, now()).unwrap();
        let mut high = sample_job(1);
        high.priority = PRIORITY_HIGH;
        let high_id = insert_job(&db, &high, later()).unwrap();
        let mut system = sample_job(1);
        system.priority = PRIORITY_SYSTEM;
        let system_id = insert_job(&db, &system, later() + chrono::Duration::minutes(1)).unwrap();

        assert_eq!(next_eligible_job(&db).unwrap().unwrap().id, system_id);
        cancel_job(&db, system_id, later()).unwrap();
        assert_eq!(next_eligible_job(&db).unwrap().unwrap().id, high_id, "priority beats age");
    }

    #[test]
    fn pause_resume_and_cancel_transitions() {
        let db = db();
        let job_id = insert_job(&db, &sample_job(1), now()).unwrap();

        pause_job(&db, job_id).unwrap();
        assert_eq!(job(&db, job_id).status, JobStatus::PausedUser);
        assert!(next_eligible_job(&db).unwrap().is_none());

        resume_job(&db, job_id).unwrap();
        assert_eq!(job(&db, job_id).status, JobStatus::Queued);

        cancel_job(&db, job_id, later()).unwrap();
        let j = job(&db, job_id);
        assert_eq!(j.status, JobStatus::Cancelled);
        assert_eq!(j.finished_at, Some(later()));

        cancel_job(&db, job_id, later() + chrono::Duration::minutes(1)).unwrap();
        assert_eq!(job(&db, job_id).finished_at, Some(later()), "already terminal: no-op");
    }

    #[test]
    fn reprioritize_job_clamps_to_the_valid_range() {
        let db = db();
        let job_id = insert_job(&db, &sample_job(1), now()).unwrap();
        reprioritize_job(&db, job_id, 99).unwrap();
        assert_eq!(job(&db, job_id).priority, PRIORITY_LOW);
        reprioritize_job(&db, job_id, -5).unwrap();
        assert_eq!(job(&db, job_id).priority, PRIORITY_SYSTEM);
    }

    #[test]
    fn retry_failed_items_revives_the_job_and_resets_attempts() {
        let db = db();
        let job_id = insert_job(&db, &sample_job(1), now()).unwrap();
        let item = next_pending_item(&db, job_id).unwrap().unwrap();
        begin_item(&db, item.id, now()).unwrap();
        fail_item(&db, item.id, "boom", now()).unwrap();
        set_job_status(&db, job_id, JobStatus::Failed, Some("boom"), now()).unwrap();

        assert_eq!(retry_failed_items(&db, job_id, later()).unwrap(), 1);
        let refreshed = get_job_item(&db, item.id).unwrap().unwrap();
        assert_eq!(refreshed.status, JobItemStatus::Pending);
        assert_eq!(refreshed.attempts, 0);
        assert_eq!(refreshed.last_error, None);
        let j = job(&db, job_id);
        assert_eq!(j.status, JobStatus::Queued);
        assert_eq!(j.failed_items, 0);
        assert_eq!(j.finished_at, None);
    }

    #[test]
    fn retry_failed_items_is_a_no_op_when_nothing_failed() {
        let db = db();
        let job_id = insert_job(&db, &sample_job(1), now()).unwrap();
        assert_eq!(retry_failed_items(&db, job_id, now()).unwrap(), 0);
    }

    #[test]
    fn finalize_if_complete_reports_completed_or_completed_with_errors() {
        let db = db();
        let ok = insert_job(&db, &sample_job(1), now()).unwrap();
        let item = next_pending_item(&db, ok).unwrap().unwrap();
        begin_item(&db, item.id, now()).unwrap();
        complete(&db, item.id, later());
        assert_eq!(finalize_if_complete(&db, ok, later()).unwrap(), Some(JobStatus::Completed));
        assert_eq!(job(&db, ok).finished_at, Some(later()));
        assert_eq!(finalize_if_complete(&db, ok, later()).unwrap(), None, "already ended");

        let skipped = insert_job(&db, &sample_job(1), now()).unwrap();
        let item = next_pending_item(&db, skipped).unwrap().unwrap();
        begin_item(&db, item.id, now()).unwrap();
        skip_item(&db, item.id, "videoNotFound", later()).unwrap();
        assert_eq!(
            finalize_if_complete(&db, skipped, later()).unwrap(),
            Some(JobStatus::CompletedWithErrors)
        );
    }

    #[test]
    fn finalize_if_complete_returns_none_while_items_are_still_pending() {
        let db = db();
        let job_id = insert_job(&db, &sample_job(2), now()).unwrap();
        let item = next_pending_item(&db, job_id).unwrap().unwrap();
        begin_item(&db, item.id, now()).unwrap();
        complete(&db, item.id, later());
        assert_eq!(finalize_if_complete(&db, job_id, later()).unwrap(), None);
    }

    #[test]
    fn finalize_if_complete_waits_for_the_last_phase() {
        let db = db();
        let mut new = sample_job(1);
        new.total_phases = 2;
        let job_id = insert_job(&db, &new, now()).unwrap();
        let item = next_pending_item(&db, job_id).unwrap().unwrap();
        begin_item(&db, item.id, now()).unwrap();
        complete(&db, item.id, later());
        assert_eq!(finalize_if_complete(&db, job_id, later()).unwrap(), None, "phase 1 of 2");

        advance_phase(&db, job_id).unwrap();
        advance_phase(&db, job_id).unwrap();
        assert_eq!(job(&db, job_id).phase, 2, "never past total_phases");
        let verify = NewJobItem {
            phase: 2,
            action: action_name::VERIFY_DESTINATION.to_string(),
            params: json!({}),
        };
        add_items(&db, job_id, &[verify], later()).unwrap();
        assert_eq!(job(&db, job_id).total_items, 2);
        assert_eq!(finalize_if_complete(&db, job_id, later()).unwrap(), None);

        let verify = next_pending_item(&db, job_id).unwrap().unwrap();
        begin_item(&db, verify.id, later()).unwrap();
        complete_item(&db, verify.id, None, None, 0, None, None, later()).unwrap();
        assert_eq!(finalize_if_complete(&db, job_id, later()).unwrap(), Some(JobStatus::Completed));
    }

    #[test]
    fn list_jobs_filters_by_status_and_account() {
        let db = db();
        crate::ytdata_accounts::upsert(&db, "UC2", "Other", None, 2).unwrap();
        let job1 = insert_job(&db, &sample_job(1), now()).unwrap();
        let mut other = sample_job(1);
        other.account_id = Some("UC2".to_string());
        let job2 = insert_job(&db, &other, later()).unwrap();
        pause_job(&db, job1).unwrap();

        let filter = JobFilter { account_id: Some("UC1".to_string()), ..Default::default() };
        let uc1 = list_jobs(&db, &filter).unwrap();
        assert_eq!(uc1.iter().map(|j| j.id).collect::<Vec<_>>(), [job1]);

        let paused_user = Some(vec![JobStatus::PausedUser]);
        let filter = JobFilter { statuses: paused_user, ..Default::default() };
        let paused = list_jobs(&db, &filter).unwrap();
        assert_eq!(paused.iter().map(|j| j.id).collect::<Vec<_>>(), [job1]);

        let all = list_jobs(&db, &JobFilter::default()).unwrap();
        assert_eq!(all.iter().map(|j| j.id).collect::<Vec<_>>(), [job2, job1], "newest first");
        let one = list_jobs(&db, &JobFilter { limit: Some(1), ..Default::default() }).unwrap();
        assert_eq!(one.len(), 1);
    }

    #[test]
    fn list_in_flight_items_finds_items_across_jobs() {
        let db = db();
        let job1 = insert_job(&db, &sample_job(1), now()).unwrap();
        let job2 = insert_job(&db, &sample_job(1), now()).unwrap();
        let item1 = next_pending_item(&db, job1).unwrap().unwrap();
        begin_item(&db, item1.id, now()).unwrap();

        let in_flight = list_in_flight_items(&db).unwrap();
        assert_eq!(in_flight.len(), 1);
        assert_eq!(in_flight[0].job_id, job1);
        assert_eq!(next_pending_item(&db, job2).unwrap().unwrap().status, JobItemStatus::Pending);
    }

    #[test]
    fn set_job_params_overwrites_the_whole_json_value() {
        let db = db();
        let job_id = insert_job(&db, &sample_job(1), now()).unwrap();
        let replaced = json!({"dest_playlist_id": "PLnew", "engine": "ytdata"});
        set_job_params(&db, job_id, &replaced).unwrap();
        let j = job(&db, job_id);
        assert_eq!(j.params, replaced);
        assert_eq!(j.engine(), Some("ytdata"));
    }

    #[test]
    fn add_items_continues_seq_numbering_and_bumps_total_items() {
        let db = db();
        let job_id = insert_job(&db, &sample_job(2), now()).unwrap();
        let verify = NewJobItem {
            phase: 2,
            action: action_name::VERIFY_DESTINATION.to_string(),
            params: json!({}),
        };
        assert_eq!(add_items(&db, job_id, &[verify], later()).unwrap().len(), 1);
        assert!(add_items(&db, job_id, &[], later()).unwrap().is_empty());
        let items = list_job_items(&db, job_id).unwrap();
        assert_eq!(items.len(), 3);
        assert_eq!(items[2].seq, 2, "seq continues");
        assert_eq!(job(&db, job_id).total_items, 3);
    }

    #[test]
    fn deleting_a_job_deletes_its_items_and_keeps_its_spend() {
        let db = db();
        let job_id = insert_job(&db, &sample_job(2), now()).unwrap();
        let keep = insert_job(&db, &sample_job(1), now()).unwrap();
        let item = next_pending_item(&db, job_id).unwrap().unwrap();
        begin_item(&db, item.id, now()).unwrap();
        complete(&db, item.id, now());

        assert!(delete_job(&db, job_id).unwrap());
        assert!(!delete_job(&db, job_id).unwrap(), "already gone");
        assert!(get_job(&db, job_id).unwrap().is_none());
        assert!(list_job_items(&db, job_id).unwrap().is_empty());
        assert_eq!(list_job_items(&db, keep).unwrap().len(), 1, "other jobs untouched");
        assert_eq!(crate::quota::spent_today(&db, now()).unwrap(), 50, "the units were spent");
        let entries = crate::quota::entries_today(&db, now()).unwrap();
        assert_eq!(entries[0].job_id, None, "unlinked from the deleted job");
    }

    #[test]
    fn delete_finished_before_prunes_only_old_terminal_jobs() {
        let db = db();
        let old = insert_job(&db, &sample_job(1), now()).unwrap();
        cancel_job(&db, old, now()).unwrap();
        let recent = insert_job(&db, &sample_job(1), now()).unwrap();
        cancel_job(&db, recent, later()).unwrap();
        let running = insert_job(&db, &sample_job(1), now()).unwrap();

        assert_eq!(delete_finished_before(&db, now() + chrono::Duration::minutes(1)).unwrap(), 1);
        assert!(get_job(&db, old).unwrap().is_none());
        assert!(list_job_items(&db, old).unwrap().is_empty());
        assert!(get_job(&db, recent).unwrap().is_some());
        assert!(get_job(&db, running).unwrap().is_some());
    }

    #[test]
    fn disconnecting_the_account_keeps_its_jobs_unowned() {
        let db = db();
        let job_id = insert_job(&db, &sample_job(1), now()).unwrap();
        assert!(crate::ytdata_accounts::remove(&db, "UC1").unwrap());
        let j = job(&db, job_id);
        assert_eq!(j.account_id, None);
        assert_eq!(list_job_items(&db, job_id).unwrap().len(), 1);
    }

    #[test]
    fn a_restart_requeues_running_jobs_and_leaves_the_rest() {
        let db = db();
        let running = insert_job(&db, &sample_job(1), now()).unwrap();
        let item = next_pending_item(&db, running).unwrap().unwrap();
        begin_item(&db, item.id, now()).unwrap();
        let paused = insert_job(&db, &sample_job(1), now()).unwrap();
        pause_job(&db, paused).unwrap();
        assert_eq!(requeue_running_jobs(&db).unwrap(), 1);
        assert_eq!(job(&db, running).status, JobStatus::Queued);
        assert_eq!(job(&db, paused).status, JobStatus::PausedUser);
        assert_eq!(get_job_item(&db, item.id).unwrap().unwrap().status, JobItemStatus::InFlight);
    }

    #[test]
    fn item_params_are_replaced_and_the_next_resume_is_the_earliest() {
        let db = db();
        let a = insert_job(&db, &sample_job(1), now()).unwrap();
        let b = insert_job(&db, &sample_job(1), now()).unwrap();
        let item = next_pending_item(&db, a).unwrap().unwrap();
        set_item_params(&db, item.id, &json!({"video_id": "v0", "playlist_item_id": "pi"}))
            .unwrap();
        let params = get_job_item(&db, item.id).unwrap().unwrap().params;
        assert_eq!(params["playlist_item_id"], json!("pi"));

        assert_eq!(next_resume_at(&db).unwrap(), None);
        set_waiting_quota(&db, a, dt("2026-07-16T07:00:00Z")).unwrap();
        set_paused_network(&db, b, dt("2026-07-15T12:04:00Z"), "reset").unwrap();
        assert_eq!(next_resume_at(&db).unwrap(), Some(dt("2026-07-15T12:04:00Z")));
        pause_job(&db, b).unwrap();
        assert_eq!(next_resume_at(&db).unwrap(), Some(dt("2026-07-16T07:00:00Z")));
    }

    #[test]
    fn reorder_queue_follows_the_given_order_within_each_priority() {
        let db = db();
        let a = insert_job(&db, &sample_job(1), now()).unwrap();
        let b = insert_job(&db, &sample_job(1), later()).unwrap();
        // Same second as `b`: the reorder has to separate them.
        let c = insert_job(&db, &sample_job(1), later()).unwrap();
        let mut high = sample_job(1);
        high.priority = PRIORITY_HIGH;
        let h = insert_job(&db, &high, later() + chrono::Duration::minutes(1)).unwrap();
        let done = insert_job(&db, &sample_job(1), now()).unwrap();
        cancel_job(&db, done, now()).unwrap();

        // c, a, b among the NORMAL ones; `h` stays first as HIGH; the ended one and an unknown id
        // are ignored, and a repeat counts once.
        reorder_queue(&db, &[c, done, a, 999, h, b, c]).unwrap();
        // The runner's picks, one after another (pausing each so the next comes up).
        let mut order = Vec::new();
        while let Some(next) = next_eligible_job(&db).unwrap() {
            pause_job(&db, next.id).unwrap();
            order.push(next.id);
        }
        assert_eq!(order, [h, c, a, b]);
        assert_eq!(job(&db, h).priority, PRIORITY_HIGH, "a drag never changes the level");
        assert_eq!(job(&db, done).created_at, now(), "an ended job is left alone");
        let stamps: Vec<_> = [c, a, b].iter().map(|id| job(&db, *id).created_at).collect();
        assert!(stamps[0] < stamps[1] && stamps[1] < stamps[2], "{stamps:?}");
        assert_eq!(reorder_queue(&db, &[]).unwrap(), 0);
    }

    #[test]
    fn an_unknown_status_on_read_is_an_error_not_a_guess() {
        let db = db();
        let job_id = insert_job(&db, &sample_job(1), now()).unwrap();
        db.conn().execute("UPDATE jobs SET status = 'bogus' WHERE id = ?1", [job_id]).unwrap();
        assert!(get_job(&db, job_id).is_err());
    }
}
