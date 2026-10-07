//! Whether the YouTube Data API can be used right now, and if not, why: the one reading the
//! settings warnings, the monitor's reader choice and the job dispatch all share.
//!
//! ```text
//! { state: not_configured | needs_auth | quota_exhausted | api_disabled | ok,
//!   reason?: no_client_secret | no_account_for_active | invalid_grant | no_refresh_token
//!            | keyring_unavailable | secret_undecryptable,
//!   account?, account_title?, spent_today, daily_units, next_reset }
//! ```
//!
//! Precedence, first match wins: `not_configured` > `api_disabled` > `needs_auth` >
//! `quota_exhausted` > `ok` ([`evaluate`], pure and table-tested).
//!
//! - **not_configured**: no OS keyring to keep a refresh token in (`keyring_unavailable`, D28:
//!   reported here rather than under `needs_auth` because reconnecting cannot fix it, and because
//!   `jobs::engine::resolve_engine` then falls back to InnerTube instead of parking jobs that could
//!   never run; it outranks the other two reasons for the same cause), no `client_secret.json`
//!   (`no_client_secret`), or no channel for the cookie account signed in now
//!   (`no_account_for_active`). The channel is the one linked to that account
//!   (`ytdata_accounts.linked_account`), or the only one connected when none is linked to anything.
//! - **api_disabled**: Google answered `accessNotConfigured` / `SERVICE_DISABLED` (the API is off
//!   in the user's Cloud project). Kept in `ytdata.api_disabled_at` until a call succeeds.
//! - **needs_auth**: the channel's refresh token was rejected (`invalid_grant`), is missing
//!   (`no_refresh_token`) or cannot be decrypted (`secret_undecryptable`, a DPAPI file from another
//!   Windows user or machine). Only the full status probes the token store; the dispatch layer's
//!   reading skips that (it runs on every write) and learns it when a job fails instead.
//! - **quota_exhausted**: Google answered `quotaExceeded` on this Pacific day
//!   (`ytdata.quota_exceeded_day`, worth that day only), or the caller says nothing is left of the
//!   budget it spends from (the jobs' share for the dispatch, the whole day for the warnings).
//!
//! The marks are written by [`note_error`] / [`note_success`], which a Data API caller passes its
//! outcomes to (the monitor's reader, `ytdata_sync`). Changes go out as the
//! `ytdata-status-changed` event ([`announce`]).

use chrono::{DateTime, Utc};
use serde::Serialize;
use ytdata::auth::accounts::AccountStatus;
use ytdata::auth::store::TokenStore;
use ytdata::error::{ApiErrorKind, Error as YtError};

use crate::db::{rfc3339_text, Db};
use crate::jobs::engine::{DataApiState, API_DISABLED_KEY, QUOTA_EXCEEDED_DAY_KEY};
use crate::ytdata_accounts::YtDataAccount;

/// Emitted with the new [`YtDataStatus`] whenever something it reads may have changed.
pub const STATUS_EVENT: &str = "ytdata-status-changed";

/// The account id the keyring is probed with when no channel is chosen yet: a well-formed id no
/// channel has (channel ids start with `UC`), so the read answers "nothing stored" on a working
/// keyring and fails only when there is none.
const PROBE_ACCOUNT_ID: &str = "limusic-keyring-probe";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Reason {
    NoClientSecret,
    NoAccountForActive,
    InvalidGrant,
    NoRefreshToken,
    KeyringUnavailable,
    SecretUndecryptable,
}

/// What the token store said about the chosen channel's refresh token (or, with no channel, about
/// the probe entry).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Credential {
    Present,
    /// Nothing stored for the chosen channel.
    Missing,
    KeyringUnavailable,
    Undecryptable,
    /// Not probed, or the store failed some other way: no verdict from it.
    Unknown,
}

/// Everything [`evaluate`] decides from.
#[derive(Debug, Clone, Copy)]
pub struct Inputs<'a> {
    pub client_secret_present: bool,
    /// The channel for the active cookie account ([`account_for_active`]).
    pub account: Option<&'a YtDataAccount>,
    pub credential: Credential,
    pub api_disabled: bool,
    pub quota_exceeded_today: bool,
    /// Whether anything is left of the budget the caller spends from.
    pub quota_left: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Verdict {
    pub state: DataApiState,
    pub reason: Option<Reason>,
}

impl Verdict {
    fn of(state: DataApiState, reason: Option<Reason>) -> Self {
        Self { state, reason }
    }
}

/// The state, in precedence order. Pure.
pub fn evaluate(i: &Inputs<'_>) -> Verdict {
    use DataApiState as S;
    if i.credential == Credential::KeyringUnavailable {
        return Verdict::of(S::NotConfigured, Some(Reason::KeyringUnavailable));
    }
    if !i.client_secret_present {
        return Verdict::of(S::NotConfigured, Some(Reason::NoClientSecret));
    }
    let Some(account) = i.account else {
        return Verdict::of(S::NotConfigured, Some(Reason::NoAccountForActive));
    };
    if i.api_disabled {
        return Verdict::of(S::ApiDisabled, None);
    }
    if account.status == AccountStatus::ReauthRequired {
        return Verdict::of(S::NeedsAuth, Some(Reason::InvalidGrant));
    }
    if i.credential == Credential::Missing {
        return Verdict::of(S::NeedsAuth, Some(Reason::NoRefreshToken));
    }
    if i.credential == Credential::Undecryptable {
        return Verdict::of(S::NeedsAuth, Some(Reason::SecretUndecryptable));
    }
    if i.quota_exceeded_today || !i.quota_left {
        return Verdict::of(S::QuotaExhausted, None);
    }
    Verdict::of(S::Ok, None)
}

/// The channel the Data API acts as for `active_account` (the cookie account signed in now): the
/// one linked to it, or the only one connected when that one is linked to nothing.
pub fn account_for_active<'a>(
    accounts: &'a [YtDataAccount],
    active_account: Option<&str>,
) -> Option<&'a YtDataAccount> {
    let linked = accounts
        .iter()
        .find(|a| active_account.is_some() && a.linked_account.as_deref() == active_account);
    let only = (accounts.len() == 1 && accounts[0].linked_account.is_none()).then(|| &accounts[0]);
    linked.or(only)
}

/// The Pacific calendar date (`YYYY-MM-DD`) of `now`: the quota day.
pub fn pt_date(now: DateTime<Utc>) -> String {
    now.with_timezone(&chrono_tz::America::Los_Angeles).date_naive().to_string()
}

pub fn api_disabled(db: &Db) -> bool {
    db.get_setting(API_DISABLED_KEY).is_some_and(|v| !v.trim().is_empty())
}

/// Whether Google said `quotaExceeded` on the Pacific day of `now`. Yesterday's mark is over.
pub fn quota_exceeded_today(db: &Db, now: DateTime<Utc>) -> bool {
    db.get_setting(QUOTA_EXCEEDED_DAY_KEY).is_some_and(|day| day.trim() == pt_date(now))
}

/// Files what a Data API failure says about the API as a whole: `accessNotConfigured` sets
/// `ytdata.api_disabled_at`, `quotaExceeded` sets `ytdata.quota_exceeded_day` to today (Pacific).
/// Anything else changes nothing. Answers whether a mark changed.
pub fn note_error(db: &Db, err: &YtError, now: DateTime<Utc>) -> bool {
    match err.api_error_kind() {
        Some(ApiErrorKind::ApiDisabled) => {
            let was = api_disabled(db);
            db.set_setting(API_DISABLED_KEY, &rfc3339_text(now));
            !was
        }
        Some(ApiErrorKind::QuotaExceeded) => {
            let was = quota_exceeded_today(db, now);
            db.set_setting(QUOTA_EXCEEDED_DAY_KEY, &pt_date(now));
            !was
        }
        _ => false,
    }
}

/// A Data API call went through: the API is evidently enabled again. Answers whether the mark was
/// there to clear.
pub fn note_success(db: &Db) -> bool {
    if api_disabled(db) {
        db.delete_setting(API_DISABLED_KEY);
        return true;
    }
    false
}

/// What the token store says for `channel_id` (or, with none, for the probe entry). The store
/// blocks: call this off the async runtime. The token read is dropped at once, never logged.
pub fn probe_store(store: &dyn TokenStore, channel_id: Option<&str>) -> Credential {
    match store.load(channel_id.unwrap_or(PROBE_ACCOUNT_ID)) {
        Ok(Some(_)) => Credential::Present,
        Ok(None) if channel_id.is_some() => Credential::Missing,
        Ok(None) => Credential::Unknown,
        Err(e) if e.is_keyring_unavailable() => Credential::KeyringUnavailable,
        Err(e) if e.is_secret_undecryptable() => Credential::Undecryptable,
        Err(_) => Credential::Unknown,
    }
}

/// The verdict with the channel it was reached for.
#[derive(Debug, Clone)]
pub struct Resolved {
    pub verdict: Verdict,
    /// The chosen channel; `None` whenever the state is `not_configured`.
    pub account: Option<YtDataAccount>,
}

/// Reads the database and asks `probe` about the chosen channel's credential, then [`evaluate`]s.
/// `quota_left` is the caller's budget (see [`Inputs::quota_left`]).
pub fn resolve(
    db: &Db,
    client_secret_present: bool,
    active_account: Option<&str>,
    quota_left: bool,
    probe: &dyn Fn(Option<&str>) -> Credential,
    now: DateTime<Utc>,
) -> Resolved {
    let accounts = crate::ytdata_accounts::list(db).unwrap_or_default();
    let account = account_for_active(&accounts, active_account);
    let credential = probe(account.map(|a| a.channel_id.as_str()));
    let verdict = evaluate(&Inputs {
        client_secret_present,
        account,
        credential,
        api_disabled: api_disabled(db),
        quota_exceeded_today: quota_exceeded_today(db, now),
        quota_left,
    });
    let account = account.filter(|_| verdict.state != DataApiState::NotConfigured).cloned();
    Resolved { verdict, account }
}

/// What the webview gets: no token, no secret, only states and counters.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct YtDataStatus {
    pub state: DataApiState,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<Reason>,
    /// The channel id the Data API acts as.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub account: Option<String>,
    /// That channel's title, for the warning's wording.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub account_title: Option<String>,
    pub spent_today: i64,
    pub daily_units: i64,
    /// The next quota reset (midnight Pacific), RFC 3339 UTC.
    pub next_reset: String,
}

/// The whole status, for the settings warnings and the monitor. Quota counts as exhausted when the
/// day's spend reached the daily units less the safety margin. Blocking (the probe): call it from
/// `spawn_blocking`.
pub fn status(
    db: &Db,
    client_secret_present: bool,
    active_account: Option<&str>,
    probe: &dyn Fn(Option<&str>) -> Credential,
    now: DateTime<Utc>,
) -> YtDataStatus {
    let spent_today = crate::quota::spent_today(db, now).unwrap_or(0);
    let daily_units = crate::jobs::budget::daily_units(db);
    let ceiling = daily_units - crate::jobs::budget::safety_margin_units(db);
    let resolved =
        resolve(db, client_secret_present, active_account, spent_today < ceiling, probe, now);
    YtDataStatus {
        state: resolved.verdict.state,
        reason: resolved.verdict.reason,
        account: resolved.account.as_ref().map(|a| a.channel_id.clone()),
        account_title: resolved.account.map(|a| a.title),
        spent_today,
        daily_units,
        next_reset: rfc3339_text(crate::quota::next_reset_utc(now)),
    }
}

/// [`status`] for the running app: its account manager's client secret, the active cookie
/// account, and its token store probed on a blocking thread.
pub async fn current(state: &crate::state::AppState) -> YtDataStatus {
    use tauri::Manager;
    let secret = state
        .app
        .try_state::<std::sync::Arc<crate::jobs::JobsState>>()
        .is_some_and(|jobs| jobs.client_secret_present());
    let db = state.db.clone();
    let store = crate::ytdata_secrets::token_store(&state.app);
    let joined = tauri::async_runtime::spawn_blocking(move || {
        let active = db.get_setting("active_account");
        let probe = |id: Option<&str>| probe_store(store.as_ref(), id);
        status(&db, secret, active.as_deref(), &probe, Utc::now())
    })
    .await;
    joined.unwrap_or_else(|_| {
        let now = Utc::now();
        let unknown = |_: Option<&str>| Credential::Unknown;
        status(&state.db, secret, None, &unknown, now)
    })
}

/// Sends `ytdata-status-changed` with the current status.
pub async fn announce(state: &crate::state::AppState) {
    use tauri::Emitter;
    let status = current(state).await;
    let _ = state.app.emit(STATUS_EVENT, &status);
}

#[cfg(test)]
mod tests {
    use super::*;
    use DataApiState::{ApiDisabled, NeedsAuth, NotConfigured, QuotaExhausted};
    const OK: DataApiState = DataApiState::Ok;
    use Credential as C;
    use Reason as R;

    fn dt(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    fn account(status: AccountStatus) -> YtDataAccount {
        YtDataAccount {
            channel_id: "UC1".into(),
            title: "Me".into(),
            thumb: None,
            status,
            added_at: 1,
            linked_account: None,
        }
    }

    #[test]
    fn evaluate_precedence_table() {
        let connected = account(AccountStatus::Connected);
        let reauth = account(AccountStatus::ReauthRequired);
        let (t, f) = (true, false);
        let a = Some(&connected);
        let r = Some(&reauth);
        let none: Option<&YtDataAccount> = None;
        // (secret, account, credential, disabled, exceeded today, quota left) -> (state, reason)
        let table = [
            (t, a, C::Present, f, f, t, OK, None),
            (t, a, C::Unknown, f, f, t, OK, None),
            (f, a, C::Present, t, t, f, NotConfigured, Some(R::NoClientSecret)),
            (t, none, C::Unknown, t, t, f, NotConfigured, Some(R::NoAccountForActive)),
            // No keyring outranks everything: nothing else can be fixed until there is one.
            (f, none, C::KeyringUnavailable, t, t, f, NotConfigured, Some(R::KeyringUnavailable)),
            (t, a, C::KeyringUnavailable, f, f, t, NotConfigured, Some(R::KeyringUnavailable)),
            // api_disabled beats needs_auth and quota.
            (t, r, C::Missing, t, t, f, ApiDisabled, None),
            (t, a, C::Present, t, f, t, ApiDisabled, None),
            // needs_auth beats quota; invalid_grant beats what the store says.
            (t, r, C::Present, f, t, f, NeedsAuth, Some(R::InvalidGrant)),
            (t, r, C::Missing, f, f, t, NeedsAuth, Some(R::InvalidGrant)),
            (t, a, C::Missing, f, t, t, NeedsAuth, Some(R::NoRefreshToken)),
            (t, a, C::Undecryptable, f, t, t, NeedsAuth, Some(R::SecretUndecryptable)),
            // quota: Google's mark, or nothing left of the caller's budget.
            (t, a, C::Present, f, t, t, QuotaExhausted, None),
            (t, a, C::Present, f, f, f, QuotaExhausted, None),
            (t, a, C::Unknown, f, t, f, QuotaExhausted, None),
            (t, a, C::Present, f, f, t, OK, None),
        ];
        for (secret, account, credential, disabled, exceeded, left, state, reason) in table {
            let got = evaluate(&Inputs {
                client_secret_present: secret,
                account,
                credential,
                api_disabled: disabled,
                quota_exceeded_today: exceeded,
                quota_left: left,
            });
            assert_eq!(
                (got.state, got.reason),
                (state, reason),
                "{secret} {:?} {credential:?} {disabled} {exceeded} {left}",
                account.map(|a| a.status)
            );
        }
    }

    #[test]
    fn the_channel_is_the_linked_one_or_the_only_one() {
        let mut one = account(AccountStatus::Connected);
        let only = account_for_active(std::slice::from_ref(&one), Some("ga1"));
        assert_eq!(only.map(|a| a.title.as_str()), Some("Me"));
        assert!(account_for_active(&[], Some("ga1")).is_none());
        one.linked_account = Some("ga2".into());
        let list = [one.clone()];
        assert!(account_for_active(&list, Some("ga1")).is_none(), "linked to someone else");
        assert!(account_for_active(&list, Some("ga2")).is_some());
        let two = [account(AccountStatus::Connected), account(AccountStatus::Connected)];
        assert!(account_for_active(&two, Some("ga1")).is_none(), "two and none linked");
        assert!(account_for_active(&two, None).is_none());
    }

    #[test]
    fn marks_are_set_cleared_and_expire() {
        let db = Db::open(std::path::Path::new(":memory:")).unwrap();
        let noon = dt("2026-07-15T19:00:00Z");
        let api = |reason: &str, status: u16| YtError::Api {
            status,
            reason: Some(reason.into()),
            message: String::new(),
        };
        assert!(!api_disabled(&db) && !quota_exceeded_today(&db, noon));
        // Something else (a 404) marks nothing.
        assert!(!note_error(&db, &api("videoNotFound", 404), noon));
        assert!(note_error(&db, &api("accessNotConfigured", 403), noon));
        assert!(api_disabled(&db));
        assert!(!note_error(&db, &api("SERVICE_DISABLED", 403), noon), "already marked");
        assert!(note_success(&db), "a call that worked clears it");
        assert!(!api_disabled(&db) && !note_success(&db));

        assert!(note_error(&db, &api("quotaExceeded", 403), noon));
        assert_eq!(db.get_setting(QUOTA_EXCEEDED_DAY_KEY).as_deref(), Some("2026-07-15"));
        assert!(quota_exceeded_today(&db, noon));
        // 23:59 PDT is still the same quota day; 00:00 PDT (07:00Z) is the next one.
        assert!(quota_exceeded_today(&db, dt("2026-07-16T06:59:00Z")));
        assert!(!quota_exceeded_today(&db, dt("2026-07-16T07:00:00Z")), "worth one day only");
        assert!(!note_success(&db), "success does not clear the quota mark");
        assert!(quota_exceeded_today(&db, noon));
        // UTC midnight is not the quota day: 2026-07-16T02:00Z is still the 15th in Pacific.
        assert_eq!(pt_date(dt("2026-07-16T02:00:00Z")), "2026-07-15");
    }

    #[test]
    fn resolve_and_status_read_the_database() {
        let db = Db::open(std::path::Path::new(":memory:")).unwrap();
        let now = dt("2026-07-15T19:00:00Z");
        let present = |_: Option<&str>| C::Present;
        let st = status(&db, true, Some("ga1"), &present, now);
        assert_eq!((st.state, st.reason), (NotConfigured, Some(R::NoAccountForActive)));
        assert_eq!((st.spent_today, st.daily_units), (0, 10_000));
        assert_eq!(st.next_reset, "2026-07-16T07:00:00Z");

        crate::ytdata_accounts::upsert(&db, "UC1", "Me", None, 1).unwrap();
        let st = status(&db, true, Some("ga1"), &present, now);
        assert_eq!(st.state, OK);
        assert_eq!((st.account.as_deref(), st.account_title.as_deref()), (Some("UC1"), Some("Me")));
        // The probe is asked about the chosen channel, and its answer counts.
        let asked = std::cell::RefCell::new(Vec::new());
        let missing = |id: Option<&str>| {
            asked.borrow_mut().push(id.map(str::to_owned));
            C::Missing
        };
        let st = status(&db, true, Some("ga1"), &missing, now);
        assert_eq!((st.state, st.reason), (NeedsAuth, Some(R::NoRefreshToken)));
        assert_eq!(asked.borrow().as_slice(), [Some("UC1".to_string())]);
        // Spent up to the margin: exhausted for the warnings.
        crate::quota::record_units(&db, "playlistItems.insert", 9_700, None, None, now).unwrap();
        let st = status(&db, true, Some("ga1"), &present, now);
        assert_eq!((st.state, st.spent_today), (QuotaExhausted, 9_700));
        // Not configured hides the channel.
        let st = status(&db, false, Some("ga1"), &present, now);
        assert_eq!((st.state, st.account.as_deref()), (NotConfigured, None));
        let json = serde_json::to_value(&st).unwrap();
        assert_eq!(json["state"], "not_configured");
        assert_eq!(json["reason"], "no_client_secret");
        assert!(json.get("account").is_none());
    }

    #[test]
    fn the_store_probe_maps_its_answers() {
        let store = ytdata::auth::store::InMemoryTokenStore::default();
        assert_eq!(probe_store(&store, Some("UC1")), C::Missing);
        assert_eq!(probe_store(&store, None), C::Unknown, "a working store, nothing chosen");
        store.save("UC1", "refresh").unwrap();
        assert_eq!(probe_store(&store, Some("UC1")), C::Present);
        struct NoKeyring;
        impl TokenStore for NoKeyring {
            fn save(&self, _: &str, _: &str) -> Result<(), YtError> {
                unreachable!()
            }
            fn load(&self, _: &str) -> Result<Option<String>, YtError> {
                Err(ytdata::error::AuthError::KeyringUnavailable("no secret service".into()).into())
            }
            fn delete(&self, _: &str) -> Result<(), YtError> {
                unreachable!()
            }
        }
        assert_eq!(probe_store(&NoKeyring, None), C::KeyringUnavailable);
    }
}
