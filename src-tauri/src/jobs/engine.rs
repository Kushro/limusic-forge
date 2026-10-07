//! Which engine runs a playlist write, and whether it goes through the queue.
//!
//! - `playlist_engine` = `auto` | `ytdata` | `innertube` (default `auto`). `auto` picks the Data
//!   API when it is usable for this account and the estimate fits in what jobs may spend today
//!   ([`super::budget::available_for_jobs_now`]); otherwise InnerTube, which costs no quota.
//! - `job_queue_mode` = `unified` | `ytdata_only` (default `unified`). Unified: InnerTube writes
//!   are queued as jobs too, so one queue orders, pauses and undoes everything. Data-API-only: an
//!   InnerTube write runs right away through `playlist_tools`, paced by `import`'s shared pacer,
//!   as it did before the queue existed.
//!
//! The engine a job was given is kept in its `params_json.engine` (D24).

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::{Job, ENGINE_KEY, PRIORITY_HIGH, PRIORITY_LOW, PRIORITY_NORMAL};
use crate::db::Db;

pub const PLAYLIST_ENGINE_KEY: &str = "playlist_engine";
pub const JOB_QUEUE_MODE_KEY: &str = "job_queue_mode";
pub const DEFAULT_JOB_PRIORITY_KEY: &str = "jobs.default_job_priority";
/// Whether a headless `--monitor` run also advances the queue (commit 29 reads it).
pub const ADVANCE_JOBS_HEADLESS_KEY: &str = "jobs.advance_jobs_headless";
/// Set by the status reader when Google answered `accessNotConfigured` (commit 26).
pub const API_DISABLED_KEY: &str = "ytdata.api_disabled_at";
/// The Pacific date (`YYYY-MM-DD`) on which Google last answered `quotaExceeded` (commit 26).
pub const QUOTA_EXCEEDED_DAY_KEY: &str = "ytdata.quota_exceeded_day";

/// The engine a job runs on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Engine {
    Ytdata,
    Innertube,
}

impl Engine {
    pub fn as_str(self) -> &'static str {
        match self {
            Engine::Ytdata => "ytdata",
            Engine::Innertube => "innertube",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim() {
            "ytdata" => Some(Engine::Ytdata),
            "innertube" => Some(Engine::Innertube),
            _ => None,
        }
    }
}

/// `playlist_engine`, or a per-operation override.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EngineSetting {
    Auto,
    Ytdata,
    Innertube,
}

impl EngineSetting {
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim() {
            "auto" => Some(EngineSetting::Auto),
            "ytdata" => Some(EngineSetting::Ytdata),
            "innertube" => Some(EngineSetting::Innertube),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueueMode {
    Unified,
    YtdataOnly,
}

/// The Data API's state for one account, in precedence order: the first that applies wins.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DataApiState {
    NotConfigured,
    ApiDisabled,
    NeedsAuth,
    QuotaExhausted,
    Ok,
}

pub fn playlist_engine(db: &Db) -> EngineSetting {
    db.get_setting(PLAYLIST_ENGINE_KEY)
        .and_then(|v| EngineSetting::parse(&v))
        .unwrap_or(EngineSetting::Auto)
}

pub fn queue_mode(db: &Db) -> QueueMode {
    match db.get_setting(JOB_QUEUE_MODE_KEY).as_deref().map(str::trim) {
        Some("ytdata_only") => QueueMode::YtdataOnly,
        _ => QueueMode::Unified,
    }
}

/// The priority a user's job gets: HIGH, NORMAL (default) or LOW. SYSTEM is the app's own.
pub fn default_job_priority(db: &Db) -> i64 {
    db.get_setting(DEFAULT_JOB_PRIORITY_KEY)
        .and_then(|v| v.trim().parse::<i64>().ok())
        .map(|p| p.clamp(PRIORITY_HIGH, PRIORITY_LOW))
        .unwrap_or(PRIORITY_NORMAL)
}

/// Picks the engine. An explicit choice (the operation's override first, then the setting) is
/// honoured even when the Data API is short of quota or needs reconnecting: the job then waits,
/// which is what asking for the Data API means. It falls back to InnerTube only when the Data
/// API cannot run this write at all (`NotConfigured`: no client secret, no channel for this
/// account, or a playlist it can't address). `auto` takes the Data API only when it is `Ok` and
/// the estimate fits in `available`.
pub fn resolve_engine(
    setting: EngineSetting,
    override_: Option<EngineSetting>,
    status: DataApiState,
    est_units: i64,
    available: i64,
) -> Engine {
    match override_.unwrap_or(setting) {
        EngineSetting::Innertube => Engine::Innertube,
        EngineSetting::Ytdata if status == DataApiState::NotConfigured => Engine::Innertube,
        EngineSetting::Ytdata => Engine::Ytdata,
        EngineSetting::Auto if status == DataApiState::Ok && est_units <= available => {
            Engine::Ytdata
        }
        EngineSetting::Auto => Engine::Innertube,
    }
}

/// The engine a stored job runs on: its `params_json.engine`, or for a job written without one
/// (PlaylistForge's, imported) the Data API when it names a channel.
pub fn engine_of(job: &Job) -> Engine {
    match job.params.get(ENGINE_KEY).and_then(|v| v.as_str()).and_then(Engine::parse) {
        Some(engine) => engine,
        None if job.account_id.is_some() => Engine::Ytdata,
        None => Engine::Innertube,
    }
}

/// Where a queued operation goes: its engine, the Data API channel it writes as (`None` on
/// InnerTube), the cookie account that queued it (InnerTube jobs run only as that account), and
/// its priority.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueueTarget {
    pub engine: Engine,
    pub channel_id: Option<String>,
    pub account: Option<String>,
    pub priority: i64,
}

/// The Data API's state for the signed-in account, and the channel it would write as.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DataApiContext {
    pub state: DataApiState,
    pub channel_id: Option<String>,
}

/// Works out [`DataApiContext`]. The channel is the one linked to `active_account` (the cookie
/// account signed in now), or the only channel connected when none is linked to anything.
/// `client_secret_present` comes from the account manager. This is the dispatch layer's reading;
/// the settings warnings get their fuller one (with the keyring probe) in commit 26.
pub fn data_api_state(
    db: &Db,
    client_secret_present: bool,
    active_account: Option<&str>,
    now: DateTime<Utc>,
) -> DataApiContext {
    let not_configured = DataApiContext { state: DataApiState::NotConfigured, channel_id: None };
    if !client_secret_present {
        return not_configured;
    }
    let accounts = crate::ytdata_accounts::list(db).unwrap_or_default();
    let linked = accounts
        .iter()
        .find(|a| active_account.is_some() && a.linked_account.as_deref() == active_account);
    let only = (accounts.len() == 1 && accounts[0].linked_account.is_none()).then(|| &accounts[0]);
    let Some(account) = linked.or(only) else { return not_configured };
    let channel_id = Some(account.channel_id.clone());
    let state = if db.get_setting(API_DISABLED_KEY).is_some_and(|v| !v.is_empty()) {
        DataApiState::ApiDisabled
    } else if account.status == ytdata::auth::accounts::AccountStatus::ReauthRequired {
        DataApiState::NeedsAuth
    } else if quota_exhausted(db, now) {
        DataApiState::QuotaExhausted
    } else {
        DataApiState::Ok
    };
    DataApiContext { state, channel_id }
}

fn quota_exhausted(db: &Db, now: DateTime<Utc>) -> bool {
    let today = now.with_timezone(&chrono_tz::America::Los_Angeles).date_naive().to_string();
    db.get_setting(QUOTA_EXCEEDED_DAY_KEY).is_some_and(|day| day.trim() == today)
        || super::budget::available_for_jobs_now(db, now).map(|n| n <= 0).unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use DataApiState::{ApiDisabled, NeedsAuth, NotConfigured, QuotaExhausted};
    const OK: DataApiState = DataApiState::Ok;
    use Engine::{Innertube as It, Ytdata as Yt};
    use EngineSetting as S;

    fn dt(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    #[test]
    fn resolve_engine_table() {
        // (setting, override, status, estimate, available) -> engine
        let table = [
            (S::Auto, None, OK, 100, 8000, Yt),
            (S::Auto, None, OK, 8000, 8000, Yt),
            (S::Auto, None, OK, 8001, 8000, It),
            (S::Auto, None, QuotaExhausted, 50, 8000, It),
            (S::Auto, None, NeedsAuth, 50, 8000, It),
            (S::Auto, None, ApiDisabled, 50, 8000, It),
            (S::Auto, None, NotConfigured, 50, 8000, It),
            (S::Ytdata, None, OK, 99_999, 0, Yt),
            (S::Ytdata, None, NeedsAuth, 50, 8000, Yt),
            (S::Ytdata, None, QuotaExhausted, 50, 0, Yt),
            (S::Ytdata, None, NotConfigured, 50, 8000, It),
            (S::Innertube, None, OK, 50, 8000, It),
            (S::Innertube, Some(S::Ytdata), OK, 50, 8000, Yt),
            (S::Ytdata, Some(S::Innertube), OK, 50, 8000, It),
            (S::Ytdata, Some(S::Auto), OK, 9000, 8000, It),
        ];
        for (setting, ov, status, est, available, want) in table {
            assert_eq!(
                resolve_engine(setting, ov, status, est, available),
                want,
                "{setting:?} {ov:?} {status:?} {est}/{available}"
            );
        }
    }

    #[test]
    fn settings_parse_with_defaults() {
        let db = Db::open(std::path::Path::new(":memory:")).unwrap();
        assert_eq!(playlist_engine(&db), S::Auto);
        assert_eq!(queue_mode(&db), QueueMode::Unified);
        assert_eq!(default_job_priority(&db), PRIORITY_NORMAL);
        db.set_setting(PLAYLIST_ENGINE_KEY, "innertube");
        db.set_setting(JOB_QUEUE_MODE_KEY, "ytdata_only");
        db.set_setting(DEFAULT_JOB_PRIORITY_KEY, "0");
        assert_eq!(playlist_engine(&db), S::Innertube);
        assert_eq!(queue_mode(&db), QueueMode::YtdataOnly);
        assert_eq!(default_job_priority(&db), PRIORITY_HIGH, "SYSTEM is not the user's");
        db.set_setting(PLAYLIST_ENGINE_KEY, "bogus");
        assert_eq!(playlist_engine(&db), S::Auto);
        assert_eq!(Engine::parse(Engine::Ytdata.as_str()), Some(Engine::Ytdata));
    }

    #[test]
    fn data_api_state_follows_its_precedence() {
        let db = Db::open(std::path::Path::new(":memory:")).unwrap();
        let now = dt("2026-07-15T19:00:00Z");
        let state = |secret, active| data_api_state(&db, secret, active, now);
        assert_eq!(state(true, Some("ga1")).state, NotConfigured, "no channel connected");

        crate::ytdata_accounts::upsert(&db, "UC1", "Me", None, 1).unwrap();
        assert_eq!(state(false, Some("ga1")).state, NotConfigured, "no client secret");
        let ctx = state(true, Some("ga1"));
        assert_eq!((ctx.state, ctx.channel_id.as_deref()), (OK, Some("UC1")), "the only one");

        crate::ytdata_accounts::upsert(&db, "UC2", "Other", None, 2).unwrap();
        assert_eq!(state(true, Some("ga1")).state, NotConfigured, "two and none linked");
        crate::ytdata_accounts::set_linked_account(&db, "UC2", Some("ga1")).unwrap();
        assert_eq!(state(true, Some("ga1")).channel_id.as_deref(), Some("UC2"));
        assert_eq!(state(true, Some("ga9")).state, NotConfigured, "not this account's");

        db.set_setting(QUOTA_EXCEEDED_DAY_KEY, "2026-07-15");
        assert_eq!(state(true, Some("ga1")).state, QuotaExhausted);
        db.set_setting(QUOTA_EXCEEDED_DAY_KEY, "2026-07-14");
        assert_eq!(state(true, Some("ga1")).state, OK, "yesterday's is over");
        crate::ytdata_accounts::set_status(
            &db,
            "UC2",
            ytdata::auth::accounts::AccountStatus::ReauthRequired,
        )
        .unwrap();
        assert_eq!(state(true, Some("ga1")).state, NeedsAuth);
        db.set_setting(API_DISABLED_KEY, "2026-07-15T10:00:00Z");
        assert_eq!(state(true, Some("ga1")).state, ApiDisabled);
    }

    #[test]
    fn a_job_without_an_engine_runs_where_its_account_says() {
        let job = |params: serde_json::Value, account: Option<&str>| Job {
            id: 1,
            account_id: account.map(Into::into),
            kind: super::super::JobKind::CopyItems,
            params,
            status: super::super::JobStatus::Queued,
            priority: 2,
            phase: 1,
            total_phases: 1,
            created_at: dt("2026-07-15T19:00:00Z"),
            started_at: None,
            finished_at: None,
            resume_at: None,
            est_units_total: 0,
            spent_units: 0,
            total_items: 0,
            done_items: 0,
            failed_items: 0,
            skipped_items: 0,
            planned_items: 0,
            retried_items: 0,
            last_error: None,
        };
        let j = job(serde_json::json!({"engine": "innertube"}), Some("UC1"));
        assert_eq!(engine_of(&j), Engine::Innertube);
        assert_eq!(engine_of(&job(serde_json::json!({}), Some("UC1"))), Engine::Ytdata);
        assert_eq!(engine_of(&job(serde_json::json!({}), None)), Engine::Innertube);
    }
}
