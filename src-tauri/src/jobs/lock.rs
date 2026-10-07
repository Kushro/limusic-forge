//! `runner_lock`: the one row that says which process runs jobs right now, so the app and a
//! headless `--monitor` run never write the same playlists at once. Port of PlaylistForge's
//! `pf-core/src/db/repo/runner_lock.rs`.
//!
//! The holder renews `heartbeat_at` on a timer. A process that died without releasing leaves a
//! heartbeat that stops moving; once it is [`STALE_AFTER_SECS`] old the next caller takes the
//! lock over. Plain synchronous calls on the database; the timer belongs to the runner.

use chrono::{DateTime, Utc};
use rusqlite::{params, OptionalExtension, Transaction, TransactionBehavior};

use crate::db::{parse_rfc3339, rfc3339_text, Db};

/// A heartbeat this old is abandoned and may be taken over. Well above the runner's renewal
/// interval, so a slow write or a few missed ticks never cost a live runner its lock.
pub const STALE_AFTER_SECS: i64 = 90;

/// Who holds the lock, as stored.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LockHolder {
    pub pid: u32,
    pub heartbeat_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AcquireOutcome {
    /// The caller holds the lock now: it was free, already the caller's, or stale.
    Acquired,
    /// Another process holds a live lock. Skip the run; do not spin on it.
    Busy(LockHolder),
}

/// Takes the lock for `pid`, in one transaction so two callers cannot both win.
pub fn try_acquire(db: &Db, pid: u32, now: DateTime<Utc>) -> rusqlite::Result<AcquireOutcome> {
    let conn = db.conn();
    // IMMEDIATE takes the write lock before the read, so another connection to the same file
    // cannot read the same free row in between and also take it.
    let tx = Transaction::new_unchecked(&conn, TransactionBehavior::Immediate)?;
    let existing: Option<(i64, String)> = tx
        .query_row("SELECT pid, heartbeat_at FROM runner_lock WHERE id = 1", [], |row| {
            Ok((row.get(0)?, row.get(1)?))
        })
        .optional()?;
    if let Some((holder, heartbeat)) = existing {
        let heartbeat_at = parse_rfc3339(&heartbeat);
        let ours = holder == i64::from(pid);
        if !ours && (now - heartbeat_at).num_seconds() < STALE_AFTER_SECS {
            return Ok(AcquireOutcome::Busy(LockHolder { pid: holder as u32, heartbeat_at }));
        }
    }
    tx.execute(
        "INSERT INTO runner_lock (id, pid, heartbeat_at) VALUES (1, ?1, ?2) \
         ON CONFLICT(id) DO UPDATE SET pid = ?1, heartbeat_at = ?2",
        params![i64::from(pid), rfc3339_text(now)],
    )?;
    tx.commit()?;
    Ok(AcquireOutcome::Acquired)
}

/// Renews `pid`'s heartbeat. `false` means the lock was taken over (this process went quiet
/// past [`STALE_AFTER_SECS`]): stop, do not retry.
pub fn renew_heartbeat(db: &Db, pid: u32, now: DateTime<Utc>) -> rusqlite::Result<bool> {
    let changed = db.conn().execute(
        "UPDATE runner_lock SET heartbeat_at = ?2 WHERE id = 1 AND pid = ?1",
        params![i64::from(pid), rfc3339_text(now)],
    )?;
    Ok(changed > 0)
}

/// Releases the lock if `pid` holds it; otherwise does nothing, so cleanup can call it blindly.
pub fn release(db: &Db, pid: u32) -> rusqlite::Result<()> {
    db.conn()
        .execute("DELETE FROM runner_lock WHERE id = 1 AND pid = ?1", params![i64::from(pid)])?;
    Ok(())
}

/// The current holder, if any ("another process is running jobs").
pub fn current_holder(db: &Db) -> rusqlite::Result<Option<LockHolder>> {
    db.conn()
        .query_row("SELECT pid, heartbeat_at FROM runner_lock WHERE id = 1", [], |row| {
            Ok(LockHolder {
                pid: row.get::<_, i64>(0)? as u32,
                heartbeat_at: parse_rfc3339(&row.get::<_, String>(1)?),
            })
        })
        .optional()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dt(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    fn db() -> Db {
        Db::open(std::path::Path::new(":memory:")).unwrap()
    }

    #[test]
    fn acquire_when_free_succeeds() {
        let db = db();
        assert_eq!(current_holder(&db).unwrap(), None);
        let at = dt("2026-07-15T12:00:00Z");
        assert_eq!(try_acquire(&db, 111, at).unwrap(), AcquireOutcome::Acquired);
        let holder = current_holder(&db).unwrap().unwrap();
        assert_eq!(holder, LockHolder { pid: 111, heartbeat_at: at });
    }

    #[test]
    fn second_process_is_busy_while_heartbeat_is_live() {
        let db = db();
        try_acquire(&db, 111, dt("2026-07-15T12:00:00Z")).unwrap();
        // 89 s later: one second short of stale.
        match try_acquire(&db, 222, dt("2026-07-15T12:01:29Z")).unwrap() {
            AcquireOutcome::Busy(holder) => assert_eq!(holder.pid, 111),
            other => panic!("expected Busy, got {other:?}"),
        }
        assert_eq!(current_holder(&db).unwrap().unwrap().pid, 111, "untouched");
    }

    #[test]
    fn same_pid_reacquiring_is_idempotent() {
        let db = db();
        try_acquire(&db, 111, dt("2026-07-15T12:00:00Z")).unwrap();
        let outcome = try_acquire(&db, 111, dt("2026-07-15T12:00:10Z")).unwrap();
        assert_eq!(outcome, AcquireOutcome::Acquired);
        let holder = current_holder(&db).unwrap().unwrap();
        assert_eq!(holder.heartbeat_at, dt("2026-07-15T12:00:10Z"), "the heartbeat moves");
    }

    #[test]
    fn a_stale_heartbeat_is_taken_over_by_another_process() {
        let db = db();
        try_acquire(&db, 111, dt("2026-07-15T12:00:00Z")).unwrap();
        // Exactly 90 s: stale.
        let now = dt("2026-07-15T12:01:30Z");
        assert_eq!((now - dt("2026-07-15T12:00:00Z")).num_seconds(), STALE_AFTER_SECS);
        assert_eq!(try_acquire(&db, 222, now).unwrap(), AcquireOutcome::Acquired);
        assert_eq!(current_holder(&db).unwrap().unwrap().pid, 222);
    }

    #[test]
    fn renew_heartbeat_keeps_the_lock_alive_and_fails_once_taken_over() {
        let db = db();
        try_acquire(&db, 111, dt("2026-07-15T12:00:00Z")).unwrap();
        assert!(renew_heartbeat(&db, 111, dt("2026-07-15T12:01:00Z")).unwrap());
        // 120 s after acquiring but 60 s after the renewal: still live.
        assert!(matches!(
            try_acquire(&db, 222, dt("2026-07-15T12:02:00Z")).unwrap(),
            AcquireOutcome::Busy(_)
        ));
        assert!(!renew_heartbeat(&db, 222, dt("2026-07-15T12:02:00Z")).unwrap(), "not the holder");

        try_acquire(&db, 222, dt("2026-07-15T12:05:00Z")).unwrap();
        assert!(
            !renew_heartbeat(&db, 111, dt("2026-07-15T12:05:05Z")).unwrap(),
            "the old holder's renewal fails once its lock was taken over"
        );
    }

    #[test]
    fn release_is_idempotent_and_only_affects_the_actual_holder() {
        let db = db();
        try_acquire(&db, 111, dt("2026-07-15T12:00:00Z")).unwrap();
        release(&db, 999).unwrap();
        assert!(current_holder(&db).unwrap().is_some(), "a wrong pid releases nothing");
        release(&db, 111).unwrap();
        assert!(current_holder(&db).unwrap().is_none());
        release(&db, 111).unwrap();
        assert!(!renew_heartbeat(&db, 111, dt("2026-07-15T12:00:20Z")).unwrap());
    }

    #[test]
    fn after_release_another_process_can_acquire_immediately() {
        let db = db();
        try_acquire(&db, 111, dt("2026-07-15T12:00:00Z")).unwrap();
        release(&db, 111).unwrap();
        let outcome = try_acquire(&db, 222, dt("2026-07-15T12:00:05Z")).unwrap();
        assert_eq!(outcome, AcquireOutcome::Acquired);
    }

    #[test]
    fn two_connections_to_one_file_share_the_lock() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("limusic.sqlite");
        let app = Db::open(&path).unwrap();
        let headless = Db::open(&path).unwrap();
        let at = dt("2026-07-15T12:00:00Z");
        assert_eq!(try_acquire(&app, 111, at).unwrap(), AcquireOutcome::Acquired);
        assert!(matches!(try_acquire(&headless, 222, at).unwrap(), AcquireOutcome::Busy(_)));
        release(&app, 111).unwrap();
        assert_eq!(try_acquire(&headless, 222, at).unwrap(), AcquireOutcome::Acquired);
    }
}
