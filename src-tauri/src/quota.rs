//! The YouTube Data API quota ledger: one row per billed call, and what today has cost.
//!
//! Quota belongs to the Google Cloud project, shared by every connected account, so the ledger
//! is not partitioned by account; `account_id` is kept for display and has no foreign key (the
//! day's spend outlives disconnecting the account that spent it). Port of PlaylistForge's
//! `pf-core/src/quota.rs`.
//!
//! "Today" is the America/Los_Angeles calendar day: Google resets the quota at midnight Pacific,
//! DST included (D32). Every function takes `now` from its caller, never the clock, so the tests
//! can pin the edges. Dates are stored as [`crate::db::rfc3339_text`], one fixed width in UTC, so
//! a day is a plain text range.
// used by commit 25+ (the job runner, the quota widget and the 14-day history).
#![allow(dead_code)]

use chrono::{DateTime, Duration, LocalResult, NaiveDate, TimeZone, Utc};
use chrono_tz::America::Los_Angeles;
use rusqlite::params;

use crate::db::{parse_rfc3339, rfc3339_text, Db};

/// Units Google grants a project per day unless it asked for more.
pub const DAILY_PROJECT_QUOTA: i64 = 10_000;

/// Endpoint names, the same strings the client reports to [`LedgerQuotaSink`] and PlaylistForge
/// wrote to its ledger.
#[allow(unused_imports)] // used by commit 25+
pub use ytdata::quota::endpoint;

/// Cost of an endpoint by name (list 1, write 50, unknown 0). The table lives in `ytdata`, next
/// to the calls it prices.
pub fn unit_cost(endpoint_name: &str) -> i64 {
    i64::from(ytdata::quota::unit_cost(endpoint_name))
}

/// Records one call at its table cost. Prefer [`record_units`] when the caller knows what it
/// actually spent.
pub fn record(
    db: &Db,
    endpoint_name: &str,
    account_id: Option<&str>,
    job_id: Option<i64>,
    now: DateTime<Utc>,
) -> rusqlite::Result<i64> {
    record_units(db, endpoint_name, unit_cost(endpoint_name), account_id, job_id, now)
}

/// Records one call with an explicit unit count. Returns the units recorded.
pub fn record_units(
    db: &Db,
    endpoint_name: &str,
    units: i64,
    account_id: Option<&str>,
    job_id: Option<i64>,
    now: DateTime<Utc>,
) -> rusqlite::Result<i64> {
    db.conn().execute(
        "INSERT INTO quota_ledger (ts, endpoint, units, account_id, job_id) \
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![rfc3339_text(now), endpoint_name, units, account_id, job_id],
    )?;
    Ok(units)
}

/// Midnight at the start of a Pacific calendar date, in UTC. US clocks change at 02:00, so
/// midnight always exists exactly once; the other branches are there so a quota counter can
/// never panic, not because they happen.
fn pt_midnight(date: NaiveDate) -> DateTime<Utc> {
    let naive = date.and_hms_opt(0, 0, 0).expect("00:00:00 is a valid time");
    let local = match Los_Angeles.from_local_datetime(&naive) {
        LocalResult::Single(at) => at,
        LocalResult::Ambiguous(earliest, _) => earliest,
        LocalResult::None => Los_Angeles.from_utc_datetime(&(naive + Duration::hours(8))),
    };
    local.with_timezone(&Utc)
}

/// The `[start, end)` UTC range of the Pacific calendar day containing `now`. Both ends are real
/// midnights, so a spring-forward day is 23 hours long and a fall-back day 25 (PlaylistForge
/// added 24 hours to the start, which put the reset an hour off on those two days).
pub(crate) fn pt_day_bounds_utc(now: DateTime<Utc>) -> (DateTime<Utc>, DateTime<Utc>) {
    let date = now.with_timezone(&Los_Angeles).date_naive();
    pt_date_bounds_utc(date)
}

fn pt_date_bounds_utc(date: NaiveDate) -> (DateTime<Utc>, DateTime<Utc>) {
    let start = pt_midnight(date);
    let end = date.succ_opt().map(pt_midnight).unwrap_or(start + Duration::days(1));
    (start, end)
}

fn units_between(db: &Db, start: DateTime<Utc>, end: DateTime<Utc>) -> rusqlite::Result<i64> {
    let total: Option<i64> = db.conn().query_row(
        "SELECT SUM(units) FROM quota_ledger WHERE ts >= ?1 AND ts < ?2",
        params![rfc3339_text(start), rfc3339_text(end)],
        |row| row.get(0),
    )?;
    Ok(total.unwrap_or(0))
}

/// Units spent in the Pacific day containing `now`.
pub fn spent_today(db: &Db, now: DateTime<Utc>) -> rusqlite::Result<i64> {
    let (start, end) = pt_day_bounds_utc(now);
    units_between(db, start, end)
}

/// The next quota reset after `now` (the following midnight Pacific): the widget's countdown,
/// and a `waiting_quota` job's `resume_at`.
pub fn next_reset_utc(now: DateTime<Utc>) -> DateTime<Utc> {
    pt_day_bounds_utc(now).1
}

/// One ledger row.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct QuotaLedgerEntry {
    pub id: i64,
    pub ts: DateTime<Utc>,
    pub endpoint: String,
    pub units: i64,
    pub account_id: Option<String>,
    pub job_id: Option<i64>,
}

/// Today's ledger rows (Pacific day containing `now`), most recent first.
pub fn entries_today(db: &Db, now: DateTime<Utc>) -> rusqlite::Result<Vec<QuotaLedgerEntry>> {
    let (start, end) = pt_day_bounds_utc(now);
    let conn = db.conn();
    let mut stmt = conn.prepare(
        "SELECT id, ts, endpoint, units, account_id, job_id FROM quota_ledger \
         WHERE ts >= ?1 AND ts < ?2 ORDER BY ts DESC, id DESC",
    )?;
    let rows = stmt.query_map(params![rfc3339_text(start), rfc3339_text(end)], |row| {
        let ts: String = row.get(1)?;
        Ok(QuotaLedgerEntry {
            id: row.get(0)?,
            ts: parse_rfc3339(&ts),
            endpoint: row.get(2)?,
            units: row.get(3)?,
            account_id: row.get(4)?,
            job_id: row.get(5)?,
        })
    })?;
    rows.collect()
}

/// One Pacific calendar day's total.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub struct DailyUsage {
    pub date: NaiveDate,
    pub units: i64,
}

/// Totals for the `days` Pacific days ending with today, oldest first (the 14-day usage bars).
/// One `SUM` per day rather than grouping in SQL: SQLite has no time zones, and a grouping that
/// ignored DST would be wrong twice a year. Steps by calendar date, not by 24 hours, so a 23- or
/// 25-hour day is neither skipped nor counted twice. `days <= 0` counts as 1.
pub fn daily_history(db: &Db, now: DateTime<Utc>, days: i64) -> rusqlite::Result<Vec<DailyUsage>> {
    let days = days.max(1);
    let today = now.with_timezone(&Los_Angeles).date_naive();
    let mut out = Vec::with_capacity(days as usize);
    for offset in (0..days).rev() {
        let Some(date) = today.checked_sub_days(chrono::Days::new(offset as u64)) else {
            continue;
        };
        let (start, end) = pt_date_bounds_utc(date);
        out.push(DailyUsage { date, units: units_between(db, start, end)? });
    }
    Ok(out)
}

/// The [`ytdata::quota::QuotaSink`] that writes this ledger: every billed call the client makes
/// lands here with the units the client reports. Reads only; the job runner writes its own rows
/// in the same transaction as the item they paid for, with a client that has no sink.
pub struct LedgerQuotaSink {
    db: std::sync::Arc<Db>,
}

impl LedgerQuotaSink {
    pub fn new(db: std::sync::Arc<Db>) -> Self {
        Self { db }
    }
}

impl ytdata::quota::QuotaSink for LedgerQuotaSink {
    fn record(&self, endpoint: &'static str, units: u32, account_id: Option<&str>) {
        // The call already happened and was billed; a ledger that cannot be written only makes
        // the counter read low, which is not worth failing the caller over.
        let units = i64::from(units);
        if let Err(e) = record_units(&self.db, endpoint, units, account_id, None, Utc::now()) {
            tracing::warn!("quota ledger: could not record {units} units for {endpoint}: {e}");
        }
    }
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

    fn date(y: i32, m: u32, d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, d).unwrap()
    }

    // --- ported from pf-core/src/quota.rs ------------------------------------------------------

    #[test]
    fn unit_cost_matches_doc_02_table() {
        assert_eq!(unit_cost(endpoint::PLAYLISTS_LIST), 1);
        assert_eq!(unit_cost(endpoint::PLAYLIST_ITEMS_LIST), 1);
        assert_eq!(unit_cost(endpoint::VIDEOS_LIST), 1);
        assert_eq!(unit_cost(endpoint::CHANNELS_LIST), 1);
        assert_eq!(unit_cost(endpoint::PLAYLISTS_INSERT), 50);
        assert_eq!(unit_cost(endpoint::PLAYLIST_ITEMS_DELETE), 50);
        assert_eq!(unit_cost("some.unknown.endpoint"), 0);
    }

    #[test]
    fn record_and_spent_today_round_trip() {
        let db = db();
        let now = dt("2026-07-15T18:00:00Z"); // 11:00 PDT
        record(&db, endpoint::PLAYLISTS_LIST, Some("UC1"), None, now).unwrap();
        record_units(&db, endpoint::VIDEOS_LIST, 1, Some("UC1"), None, now).unwrap();
        record(&db, endpoint::PLAYLISTS_INSERT, None, None, now).unwrap();

        assert_eq!(spent_today(&db, now).unwrap(), 1 + 1 + 50);
    }

    #[test]
    fn spent_today_only_counts_the_pt_calendar_day() {
        let db = db();
        // 2026-07-14 23:00 PDT: the 14th, not the 15th.
        record(&db, endpoint::PLAYLISTS_LIST, None, None, dt("2026-07-15T06:00:00Z")).unwrap();
        // 2026-07-15 00:00:01 PDT: the 15th.
        record(&db, endpoint::VIDEOS_LIST, None, None, dt("2026-07-15T07:00:01Z")).unwrap();

        let today_noon_pt = dt("2026-07-15T19:00:00Z");
        assert_eq!(spent_today(&db, today_noon_pt).unwrap(), 1, "only the second entry is today");
    }

    #[test]
    fn spent_today_is_zero_with_no_entries() {
        assert_eq!(spent_today(&db(), dt("2026-07-15T12:00:00Z")).unwrap(), 0);
    }

    #[test]
    fn next_reset_utc_is_the_following_midnight_pt() {
        // PDT (UTC-7): midnight on the 16th is 07:00 UTC.
        assert_eq!(next_reset_utc(dt("2026-07-15T18:30:00Z")), dt("2026-07-16T07:00:00Z"));
    }

    #[test]
    fn next_reset_utc_just_after_midnight_pt_is_the_same_day_tomorrow() {
        // 00:00:01 PDT on the 15th: the reset just happened, the next is the 16th.
        assert_eq!(next_reset_utc(dt("2026-07-15T07:00:01Z")), dt("2026-07-16T07:00:00Z"));
    }

    #[test]
    fn daily_history_buckets_by_pt_calendar_day_oldest_first() {
        let db = db();
        record(&db, endpoint::PLAYLISTS_LIST, None, None, dt("2026-07-14T19:00:00Z")).unwrap();
        record(&db, endpoint::PLAYLISTS_INSERT, None, None, dt("2026-07-15T19:00:00Z")).unwrap();

        let history = daily_history(&db, dt("2026-07-15T19:30:00Z"), 3).unwrap();
        assert_eq!(history.len(), 3);
        assert_eq!(history[0], DailyUsage { date: date(2026, 7, 13), units: 0 });
        assert_eq!(history[1], DailyUsage { date: date(2026, 7, 14), units: 1 });
        assert_eq!(history[2], DailyUsage { date: date(2026, 7, 15), units: 50 }, "today last");
    }

    #[test]
    fn daily_history_clamps_non_positive_days_to_one() {
        let db = db();
        let now = dt("2026-07-15T19:00:00Z");
        assert_eq!(daily_history(&db, now, 0).unwrap().len(), 1);
        assert_eq!(daily_history(&db, now, -5).unwrap().len(), 1);
    }

    #[test]
    fn entries_today_orders_most_recent_first_and_is_scoped() {
        let db = db();
        let now = dt("2026-07-15T18:00:00Z");
        record(&db, endpoint::PLAYLISTS_LIST, Some("UC1"), None, dt("2026-07-15T17:00:00Z"))
            .unwrap();
        record(&db, endpoint::VIDEOS_LIST, Some("UC1"), None, dt("2026-07-15T18:00:00Z")).unwrap();
        record(&db, endpoint::PLAYLISTS_LIST, Some("UC1"), None, dt("2026-07-14T12:00:00Z"))
            .unwrap();

        let entries = entries_today(&db, now).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].endpoint, endpoint::VIDEOS_LIST);
        assert_eq!(entries[0].ts, dt("2026-07-15T18:00:00Z"));
        assert_eq!(entries[0].account_id.as_deref(), Some("UC1"));
        assert_eq!(entries[1].endpoint, endpoint::PLAYLISTS_LIST);
    }

    // --- DST and the rest ------------------------------------------------------------------------

    #[test]
    fn the_reset_follows_standard_time_in_winter() {
        // PST (UTC-8): midnight is 08:00 UTC.
        assert_eq!(next_reset_utc(dt("2026-01-15T20:00:00Z")), dt("2026-01-16T08:00:00Z"));
        let (start, _) = pt_day_bounds_utc(dt("2026-01-15T20:00:00Z"));
        assert_eq!(start, dt("2026-01-15T08:00:00Z"));
    }

    #[test]
    fn spring_forward_day_is_23_hours_and_fall_back_day_25() {
        // 2026-03-08: clocks jump from 02:00 PST to 03:00 PDT.
        let (start, end) = pt_day_bounds_utc(dt("2026-03-08T20:00:00Z"));
        assert_eq!(start, dt("2026-03-08T08:00:00Z"));
        assert_eq!(end, dt("2026-03-09T07:00:00Z"));
        assert_eq!((end - start).num_hours(), 23);
        // 2026-11-01: clocks fall back from 02:00 PDT to 01:00 PST.
        let (start, end) = pt_day_bounds_utc(dt("2026-11-01T20:00:00Z"));
        assert_eq!(start, dt("2026-11-01T07:00:00Z"));
        assert_eq!(end, dt("2026-11-02T08:00:00Z"));
        assert_eq!((end - start).num_hours(), 25);
    }

    #[test]
    fn spent_today_on_a_dst_day_counts_its_whole_day() {
        let db = db();
        // 23:30 PDT on 2026-03-08, the last half hour of the short day.
        record(&db, endpoint::PLAYLISTS_INSERT, None, None, dt("2026-03-09T06:30:00Z")).unwrap();
        // 00:30 PDT on 2026-03-09: the next day.
        record(&db, endpoint::PLAYLISTS_LIST, None, None, dt("2026-03-09T07:30:00Z")).unwrap();
        assert_eq!(spent_today(&db, dt("2026-03-08T10:00:00Z")).unwrap(), 50);
        assert_eq!(spent_today(&db, dt("2026-03-09T07:30:00Z")).unwrap(), 1);
        // 01:30 PST on 2026-11-01, after the fall-back hour repeated: still the 1st.
        record(&db, endpoint::VIDEOS_LIST, None, None, dt("2026-11-01T09:30:00Z")).unwrap();
        assert_eq!(spent_today(&db, dt("2026-11-01T07:10:00Z")).unwrap(), 1);
    }

    #[test]
    fn daily_history_neither_skips_nor_repeats_a_dst_day() {
        let db = db();
        // 00:30 PDT on 2026-03-09, the day after spring forward: 24 hours earlier is still the
        // 7th in Pacific time, which a step of 24 hours would have landed on, skipping the 8th.
        let now = dt("2026-03-09T07:30:00Z");
        record(&db, endpoint::PLAYLISTS_INSERT, None, None, dt("2026-03-08T20:00:00Z")).unwrap();
        let history = daily_history(&db, now, 3).unwrap();
        let dates: Vec<NaiveDate> = history.iter().map(|d| d.date).collect();
        assert_eq!(dates, [date(2026, 3, 7), date(2026, 3, 8), date(2026, 3, 9)]);
        assert_eq!(history[1].units, 50);
    }

    #[test]
    fn the_ledger_outlives_its_account_and_its_job() {
        let db = db();
        let now = dt("2026-07-15T18:00:00Z");
        db.conn()
            .execute_batch(
                "INSERT INTO ytdata_accounts(channel_id, title, added_at) VALUES('UC1', 'Me', 1);
                 INSERT INTO jobs(id, account_id, kind, created_at) VALUES(7, 'UC1', 'k', 't');",
            )
            .unwrap();
        record(&db, endpoint::PLAYLIST_ITEMS_INSERT, Some("UC1"), Some(7), now).unwrap();
        record(&db, endpoint::PLAYLISTS_LIST, Some("UC-gone"), None, now).unwrap();
        db.conn().execute("DELETE FROM jobs WHERE id = 7", []).unwrap();
        db.conn().execute("DELETE FROM ytdata_accounts", []).unwrap();

        assert_eq!(spent_today(&db, now).unwrap(), 51);
        let entries = entries_today(&db, now).unwrap();
        assert!(entries.iter().all(|e| e.job_id.is_none()));
        assert!(entries.iter().any(|e| e.account_id.as_deref() == Some("UC1")));
    }

    #[test]
    fn ledger_sink_records_what_the_client_reports() {
        use ytdata::quota::QuotaSink;
        let db = std::sync::Arc::new(db());
        let sink = LedgerQuotaSink::new(db.clone());
        sink.record(endpoint::PLAYLIST_ITEMS_LIST, 1, Some("UC1"));
        sink.record(endpoint::PLAYLISTS_UPDATE, 50, None);
        // Stamped with the clock, so summed over a range no midnight can split.
        let all = units_between(&db, DateTime::<Utc>::UNIX_EPOCH, Utc::now() + Duration::days(1));
        assert_eq!(all.unwrap(), 51);
    }

    #[test]
    fn stored_dates_sort_as_text_and_read_back() {
        let a = dt("2026-07-15T06:59:59Z");
        let b = dt("2026-07-15T07:00:00Z");
        assert!(rfc3339_text(a) < rfc3339_text(b));
        assert_eq!(rfc3339_text(b), "2026-07-15T07:00:00Z");
        assert_eq!(parse_rfc3339(&rfc3339_text(b)), b);
        assert_eq!(parse_rfc3339("2026-07-15T07:00:00+00:00"), b, "PlaylistForge's form");
        assert_eq!(parse_rfc3339("garbage"), DateTime::<Utc>::UNIX_EPOCH);
    }
}
