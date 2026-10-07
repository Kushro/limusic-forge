//! The day's Data API budget, partitioned. Port of PlaylistForge's
//! `pf-core/src/planner/budget.rs`:
//!
//! ```text
//! budget.daily_units (10 000 u/day, reset at midnight Pacific)
//! ├── safety margin      (budget.safety_margin_percent, 3 %)  never touched
//! ├── backup reserve     (budget.backup_reserve_units, at least 500 u)
//! └── jobs               the rest: the user's writes
//! ```
//!
//! The reserve shrinks by what the monitor actually spent today (`monitor_runs.units_spent`, not
//! the ledger: a ledger row alone can't tell the monitor from a manual sync). With
//! `budget.opportunistic_mode` on (the default), the rest of it is released as soon as the day's
//! scheduled runs (`budget.backup_runs_per_day`) have happened, so no units go to waste.
//!
//! Settings are plain `settings` rows; a missing or unparseable value reads as its default.

use chrono::{DateTime, Utc};
use rusqlite::params;

use crate::db::{rfc3339_text, Db};
use crate::quota::{pt_day_bounds_utc, spent_today, DAILY_PROJECT_QUOTA};

pub const DAILY_UNITS_KEY: &str = "budget.daily_units";
pub const SAFETY_MARGIN_PERCENT_KEY: &str = "budget.safety_margin_percent";
pub const BACKUP_RESERVE_UNITS_KEY: &str = "budget.backup_reserve_units";
pub const OPPORTUNISTIC_MODE_KEY: &str = "budget.opportunistic_mode";
pub const BACKUP_RUNS_PER_DAY_KEY: &str = "budget.backup_runs_per_day";

pub const DEFAULT_SAFETY_MARGIN_PERCENT: i64 = 3;
/// The floor of the backup reserve, whatever is stored.
pub const DEFAULT_MIN_BACKUP_RESERVE: i64 = 500;
pub const DEFAULT_OPPORTUNISTIC_MODE: bool = true;
pub const DEFAULT_BACKUP_RUNS_PER_DAY: i64 = 1;

fn setting_i64(db: &Db, key: &str) -> Option<i64> {
    db.get_setting(key)?.trim().parse().ok()
}

/// The project's daily quota: Google's default 10 000 unless the user was granted more.
pub fn daily_units(db: &Db) -> i64 {
    setting_i64(db, DAILY_UNITS_KEY).filter(|n| *n > 0).unwrap_or(DAILY_PROJECT_QUOTA)
}

pub fn safety_margin_percent(db: &Db) -> i64 {
    setting_i64(db, SAFETY_MARGIN_PERCENT_KEY)
        .filter(|p| (0..=100).contains(p))
        .unwrap_or(DEFAULT_SAFETY_MARGIN_PERCENT)
}

pub fn set_safety_margin_percent(db: &Db, percent: i64) {
    db.set_setting(SAFETY_MARGIN_PERCENT_KEY, &percent.clamp(0, 100).to_string());
}

/// The margin in units of the day's quota.
pub fn safety_margin_units(db: &Db) -> i64 {
    daily_units(db) * safety_margin_percent(db) / 100
}

/// The stored reserve, never below [`DEFAULT_MIN_BACKUP_RESERVE`].
pub fn backup_reserve_units(db: &Db) -> i64 {
    setting_i64(db, BACKUP_RESERVE_UNITS_KEY)
        .map(|v| v.max(DEFAULT_MIN_BACKUP_RESERVE))
        .unwrap_or(DEFAULT_MIN_BACKUP_RESERVE)
}

pub fn set_backup_reserve_units(db: &Db, units: i64) {
    db.set_setting(BACKUP_RESERVE_UNITS_KEY, &units.max(DEFAULT_MIN_BACKUP_RESERVE).to_string());
}

pub fn opportunistic_mode(db: &Db) -> bool {
    match db.get_setting(OPPORTUNISTIC_MODE_KEY).as_deref().map(str::trim) {
        Some("true" | "1") => true,
        Some("false" | "0") => false,
        _ => DEFAULT_OPPORTUNISTIC_MODE,
    }
}

pub fn set_opportunistic_mode(db: &Db, enabled: bool) {
    db.set_setting(OPPORTUNISTIC_MODE_KEY, if enabled { "true" } else { "false" });
}

pub fn backup_runs_per_day(db: &Db) -> i64 {
    setting_i64(db, BACKUP_RUNS_PER_DAY_KEY)
        .filter(|n| *n > 0)
        .unwrap_or(DEFAULT_BACKUP_RUNS_PER_DAY)
}

pub fn set_backup_runs_per_day(db: &Db, runs: i64) {
    db.set_setting(BACKUP_RUNS_PER_DAY_KEY, &runs.max(1).to_string());
}

/// `(units the monitor spent today, scheduled runs today)`. `monitor_runs` dates are epoch
/// seconds. Every run's units count; only scheduled and headless runs count as the day's backup
/// runs (a manual sync is not one).
fn backup_spend_and_runs_today(db: &Db, now: DateTime<Utc>) -> rusqlite::Result<(i64, i64)> {
    let (start, end) = pt_day_bounds_utc(now);
    db.conn().query_row(
        "SELECT COALESCE(SUM(units_spent), 0), \
                COALESCE(SUM(\"trigger\" IN ('scheduler', 'headless')), 0) \
         FROM monitor_runs WHERE started_at >= ?1 AND started_at < ?2",
        params![start.timestamp(), end.timestamp()],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )
}

/// What the monitor and its backups spent today.
pub fn backup_spent_today(db: &Db, now: DateTime<Utc>) -> rusqlite::Result<i64> {
    Ok(backup_spend_and_runs_today(db, now)?.0)
}

/// What job items spent today: every ledger row carrying a `job_id`.
pub fn jobs_spent_today(db: &Db, now: DateTime<Utc>) -> rusqlite::Result<i64> {
    let (start, end) = pt_day_bounds_utc(now);
    db.conn().query_row(
        "SELECT COALESCE(SUM(units), 0) FROM quota_ledger \
         WHERE job_id IS NOT NULL AND ts >= ?1 AND ts < ?2",
        params![rfc3339_text(start), rfc3339_text(end)],
        |row| row.get(0),
    )
}

/// How much of the reserve is still walled off from jobs.
pub fn backup_reserve_remaining_today(db: &Db, now: DateTime<Utc>) -> rusqlite::Result<i64> {
    let reserve = backup_reserve_units(db);
    let (spent, runs) = backup_spend_and_runs_today(db, now)?;
    if opportunistic_mode(db) && runs >= backup_runs_per_day(db) {
        return Ok(0);
    }
    Ok((reserve - spent).max(0))
}

/// The jobs' share of a whole day, the reserve kept whole (the forecast's later days).
pub fn full_day_jobs_ceiling(db: &Db) -> i64 {
    (daily_units(db) - safety_margin_units(db) - backup_reserve_units(db)).max(0)
}

/// The hard rule: how many units a job item may still spend now. Never negative.
pub fn available_for_jobs_now(db: &Db, now: DateTime<Utc>) -> rusqlite::Result<i64> {
    let spent = spent_today(db, now)?;
    let ceiling =
        (daily_units(db) - safety_margin_units(db) - backup_reserve_remaining_today(db, now)?)
            .max(0);
    Ok((ceiling - spent).max(0))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::MonitorRun;

    fn dt(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }
    // Noon PDT on 2026-07-15.
    fn noon() -> DateTime<Utc> {
        dt("2026-07-15T19:00:00Z")
    }

    fn db() -> Db {
        Db::open(std::path::Path::new(":memory:")).unwrap()
    }

    fn run(db: &Db, at: DateTime<Utc>, units: i64, trigger: &str) {
        db.record_monitor_run(&MonitorRun {
            id: 0,
            started_at: at.timestamp(),
            finished_at: at.timestamp() + 5,
            trigger: trigger.into(),
            outcome: "ok".into(),
            playlists_ok: 1,
            playlists_failed: 0,
            alerts_new: 0,
            units_spent: units,
            detail_json: "{}".into(),
        })
        .unwrap();
    }

    #[test]
    fn defaults_when_nothing_is_configured() {
        let db = db();
        assert_eq!(daily_units(&db), 10_000);
        assert_eq!(safety_margin_percent(&db), 3);
        assert_eq!(safety_margin_units(&db), 300);
        assert_eq!(backup_reserve_units(&db), 500);
        assert!(opportunistic_mode(&db));
        assert_eq!(backup_runs_per_day(&db), 1);
        db.set_setting(SAFETY_MARGIN_PERCENT_KEY, "banana");
        db.set_setting(BACKUP_RUNS_PER_DAY_KEY, "0");
        assert_eq!(safety_margin_percent(&db), 3, "unparseable reads as the default");
        assert_eq!(backup_runs_per_day(&db), 1);
    }

    #[test]
    fn the_reserve_never_goes_below_its_floor() {
        let db = db();
        set_backup_reserve_units(&db, 10);
        assert_eq!(backup_reserve_units(&db), 500);
        db.set_setting(BACKUP_RESERVE_UNITS_KEY, "20");
        assert_eq!(backup_reserve_units(&db), 500, "even when written by hand");
        set_backup_reserve_units(&db, 2000);
        assert_eq!(backup_reserve_units(&db), 2000);
    }

    #[test]
    fn available_for_jobs_now_subtracts_margin_reserve_and_spend() {
        let db = db();
        set_backup_reserve_units(&db, 1000);
        set_opportunistic_mode(&db, false);
        // 10 000 - 300 - 1000.
        assert_eq!(available_for_jobs_now(&db, noon()).unwrap(), 8700);
        crate::quota::record_units(&db, "playlistItems.insert", 200, None, None, noon()).unwrap();
        assert_eq!(available_for_jobs_now(&db, noon()).unwrap(), 8500);
    }

    #[test]
    fn a_bigger_project_quota_scales_the_margin_too() {
        let db = db();
        db.set_setting(DAILY_UNITS_KEY, "20000");
        set_opportunistic_mode(&db, false);
        // 20 000 - 600 - 500.
        assert_eq!(available_for_jobs_now(&db, noon()).unwrap(), 18_900);
        assert_eq!(full_day_jobs_ceiling(&db), 18_900);
    }

    #[test]
    fn the_reserve_shrinks_as_the_monitor_spends_and_never_goes_negative() {
        let db = db();
        set_backup_reserve_units(&db, 1000);
        set_opportunistic_mode(&db, false);
        run(&db, noon(), 300, "scheduler");
        crate::quota::record_units(&db, "playlistItems.list", 300, None, None, noon()).unwrap();
        assert_eq!(backup_reserve_remaining_today(&db, noon()).unwrap(), 700);
        // 10 000 - 300 - 700 - 300 spent.
        assert_eq!(available_for_jobs_now(&db, noon()).unwrap(), 8700);
        run(&db, noon(), 900, "manual_ui");
        assert_eq!(backup_reserve_remaining_today(&db, noon()).unwrap(), 0);
    }

    #[test]
    fn opportunistic_mode_releases_the_reserve_once_the_scheduled_runs_happened() {
        let db = db();
        set_backup_reserve_units(&db, 1000);
        assert_eq!(backup_reserve_remaining_today(&db, noon()).unwrap(), 1000);
        run(&db, noon(), 150, "manual_ui");
        assert_eq!(
            backup_reserve_remaining_today(&db, noon()).unwrap(),
            850,
            "a manual sync is not the day's backup run"
        );
        run(&db, noon(), 150, "scheduler");
        assert_eq!(backup_reserve_remaining_today(&db, noon()).unwrap(), 0);
        set_opportunistic_mode(&db, false);
        assert_eq!(backup_reserve_remaining_today(&db, noon()).unwrap(), 700);
    }

    #[test]
    fn a_run_on_another_pacific_day_does_not_count() {
        let db = db();
        set_backup_reserve_units(&db, 1000);
        run(&db, dt("2026-07-14T19:00:00Z"), 150, "scheduler");
        assert_eq!(backup_reserve_remaining_today(&db, noon()).unwrap(), 1000);
        assert_eq!(backup_spent_today(&db, noon()).unwrap(), 0);
    }

    #[test]
    fn jobs_and_backup_spend_are_separate_slices() {
        let db = db();
        crate::ytdata_accounts::upsert(&db, "UC1", "Channel", None, 1).unwrap();
        db.conn()
            .execute(
                "INSERT INTO jobs(id, account_id, kind, created_at) VALUES(9, 'UC1', 'k', 't')",
                [],
            )
            .unwrap();
        run(&db, noon(), 200, "scheduler");
        crate::quota::record_units(&db, "playlistItems.insert", 50, Some("UC1"), Some(9), noon())
            .unwrap();
        crate::quota::record_units(&db, "playlists.list", 5, Some("UC1"), None, noon()).unwrap();
        assert_eq!(backup_spent_today(&db, noon()).unwrap(), 200);
        assert_eq!(jobs_spent_today(&db, noon()).unwrap(), 50);
        assert_eq!(spent_today(&db, noon()).unwrap(), 55);
    }
}
