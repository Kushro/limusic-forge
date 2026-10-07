//! Typed errors for `ytdata`.
//!
//! - [`AuthError`]: OAuth/token handling. [`AuthError::InvalidGrant`] is the one callers must
//!   branch on: it means "re-authorization required" for that account, never "delete data".
//! - [`Error`]: the crate-wide error, wrapping `AuthError` plus network/JSON/IO failures and
//!   [`Error::Api`] (HTTP status + Google's structured `reason`).
//! - [`ApiErrorKind`]: semantic classification of an [`Error::Api`] so job runners and status
//!   code never parse strings themselves.

use std::fmt;

/// Crate-wide result alias.
pub type Result<T> = std::result::Result<T, Error>;

/// Every fallible `ytdata` operation returns this.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Auth(#[from] AuthError),

    #[error("network error: {0}")]
    Network(#[from] reqwest::Error),

    /// Non-2xx response from the YouTube Data API. `reason` is Google's structured reason
    /// (`error.errors[].reason`, falling back to `error.details[].reason`), e.g.
    /// `"quotaExceeded"`, `"accessNotConfigured"`; `None` when the body did not parse.
    #[error("YouTube API error {status}{}: {message}", ApiReasonDisplay(reason))]
    Api { status: u16, reason: Option<String>, message: String },

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("JSON error: {0}")]
    Serde(#[from] serde_json::Error),
}

/// Formats `" (reason)"` when present, nothing otherwise.
struct ApiReasonDisplay<'a>(&'a Option<String>);

impl fmt::Display for ApiReasonDisplay<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            Some(reason) => write!(f, " ({reason})"),
            None => Ok(()),
        }
    }
}

/// Semantic classification of an [`Error::Api`] failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApiErrorKind {
    /// 403 `quotaExceeded`: the project's daily budget is gone. Wait for the reset (midnight
    /// Pacific); never retried.
    QuotaExceeded,
    /// 403 `accessNotConfigured` / `SERVICE_DISABLED`: the YouTube Data API is not enabled in the
    /// user's Google Cloud project. Nothing works until they enable it; never retried.
    ApiDisabled,
    /// 403 `rateLimitExceeded`, or a bare 429: too fast, not out of budget. Short backoff.
    RateLimitExceeded,
    /// 400 `manualSortRequired`: `playlistItems.update` on a playlist without manual sort order.
    ManualSortRequired,
    /// The targeted video/item is gone: 404, or `videoNotFound`, `playlistItemsNotAccessible`,
    /// `forbidden`, `playlistNotFound`. Skip that item; the job continues.
    ItemGone,
    /// Any other non-2xx response.
    Other,
}

impl Error {
    /// Classifies `self`. `None` for every non-[`Error::Api`] variant (network, IO, auth,
    /// serde): those have their own handling (retry, [`AuthError::InvalidGrant`], ...).
    pub fn api_error_kind(&self) -> Option<ApiErrorKind> {
        let Error::Api { status, reason, .. } = self else { return None };
        let reason = reason.as_deref();
        if *status == 403 && reason == Some("quotaExceeded") {
            return Some(ApiErrorKind::QuotaExceeded);
        }
        if *status == 403 && matches!(reason, Some("accessNotConfigured" | "SERVICE_DISABLED")) {
            return Some(ApiErrorKind::ApiDisabled);
        }
        if (*status == 403 && reason == Some("rateLimitExceeded")) || *status == 429 {
            return Some(ApiErrorKind::RateLimitExceeded);
        }
        if reason == Some("manualSortRequired") {
            return Some(ApiErrorKind::ManualSortRequired);
        }
        if *status == 404
            || matches!(
                reason,
                Some(
                    "videoNotFound"
                        | "playlistItemsNotAccessible"
                        | "forbidden"
                        | "playlistNotFound"
                )
            )
        {
            return Some(ApiErrorKind::ItemGone);
        }
        Some(ApiErrorKind::Other)
    }

    /// `true` for an HTTP 401 from the Data API (expired/revoked access token).
    pub fn is_unauthorized(&self) -> bool {
        matches!(self, Error::Api { status: 401, .. })
    }

    /// `true` when the refresh token was rejected and the account needs re-authorization.
    pub fn is_invalid_grant(&self) -> bool {
        matches!(self, Error::Auth(AuthError::InvalidGrant))
    }

    /// `true` when a stored secret exists but cannot be decrypted (DPAPI file written by another
    /// Windows user or machine, or corrupted). The file is left in place.
    pub fn is_secret_undecryptable(&self) -> bool {
        matches!(self, Error::Auth(AuthError::SecretUndecryptable))
    }

    /// `true` when there is no usable OS keyring (Linux without a Secret Service, or a locked
    /// one). The app reports `reason: keyring_unavailable`; tokens are never stored in clear.
    pub fn is_keyring_unavailable(&self) -> bool {
        matches!(self, Error::Auth(AuthError::KeyringUnavailable(_)))
    }

    /// `true` for every failure the user fixes by connecting the account again: a rejected or
    /// missing refresh token, or one that can no longer be decrypted.
    pub fn needs_reauthorization(&self) -> bool {
        matches!(
            self,
            Error::Auth(
                AuthError::InvalidGrant
                    | AuthError::NoRefreshToken
                    | AuthError::SecretUndecryptable
            )
        )
    }
}

/// OAuth/token-handling errors.
#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    /// The refresh token was rejected (revoked, expired — e.g. the 7-day expiry of an OAuth
    /// consent screen in "Testing" mode). Callers surface a per-account "re-authorization
    /// required" state and must **never** delete credentials or local data because of it.
    #[error(
        "the refresh token was rejected by Google (invalid_grant) — re-authorization is required"
    )]
    InvalidGrant,

    #[error("no refresh token is stored for this account")]
    NoRefreshToken,

    #[error("no client_secret.json has been imported yet")]
    NoClientSecret,

    #[error("client_secret.json is invalid: {0}")]
    InvalidClientSecret(String),

    #[error("authorization was cancelled")]
    Cancelled,

    #[error("timed out waiting for authorization in the browser")]
    TimedOut,

    #[error("local loopback server error: {0}")]
    Loopback(String),

    #[error(
        "state mismatch in the OAuth redirect (possible CSRF) — try connecting the account again"
    )]
    StateMismatch,

    #[error("Google returned an OAuth error: {0}")]
    Provider(String),

    #[error("OAuth request failed: {0}")]
    Oauth(String),

    /// Failure of the secure credential store behind a [`crate::auth::store::TokenStore`]
    /// (keyring, DPAPI file). The message never contains the secret itself.
    #[error("secure credential storage error: {0}")]
    Store(String),

    /// A stored secret exists but could not be decrypted (DPAPI file from another Windows user
    /// or machine, or a corrupted one). Maps to "re-authorization required"; the file is never
    /// deleted because of it.
    #[error("the stored credential could not be decrypted — re-authorization is required")]
    SecretUndecryptable,

    /// No usable OS keyring: on Linux, no Secret Service is running (or it is locked). The Data
    /// API stays disabled; the token is never written anywhere in clear instead.
    #[error(
        "no OS keyring is available (on Linux, install and unlock a Secret Service such as \
         GNOME Keyring or KWallet): {0}"
    )]
    KeyringUnavailable(String),

    /// The account id cannot name a credential: only ASCII letters, digits, `-` and `_` are
    /// accepted (YouTube channel ids use nothing else).
    #[error("invalid account id {0:?}: only letters, digits, '-' and '_' are allowed")]
    InvalidAccountId(String),

    #[error("could not open the system browser: {0}")]
    BrowserOpen(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn api_error_display_includes_reason_when_present() {
        let err = Error::Api {
            status: 403,
            reason: Some("quotaExceeded".to_string()),
            message: "The request cannot be completed.".to_string(),
        };
        assert_eq!(
            err.to_string(),
            "YouTube API error 403 (quotaExceeded): The request cannot be completed."
        );
    }

    #[test]
    fn api_error_display_omits_reason_when_absent() {
        let err = Error::Api { status: 500, reason: None, message: "Internal error".to_string() };
        assert_eq!(err.to_string(), "YouTube API error 500: Internal error");
    }

    #[test]
    fn auth_error_converts_into_crate_error() {
        let err: Error = AuthError::InvalidGrant.into();
        assert!(matches!(err, Error::Auth(AuthError::InvalidGrant)));
        assert!(err.is_invalid_grant());
    }

    fn api(status: u16, reason: Option<&str>) -> Error {
        Error::Api { status, reason: reason.map(str::to_string), message: "boom".to_string() }
    }

    #[test]
    fn api_error_kind_classifies_quota_exceeded() {
        assert_eq!(
            api(403, Some("quotaExceeded")).api_error_kind(),
            Some(ApiErrorKind::QuotaExceeded)
        );
    }

    #[test]
    fn api_error_kind_classifies_api_disabled() {
        assert_eq!(
            api(403, Some("accessNotConfigured")).api_error_kind(),
            Some(ApiErrorKind::ApiDisabled)
        );
        assert_eq!(
            api(403, Some("SERVICE_DISABLED")).api_error_kind(),
            Some(ApiErrorKind::ApiDisabled)
        );
    }

    #[test]
    fn api_error_kind_classifies_rate_limit() {
        assert_eq!(
            api(403, Some("rateLimitExceeded")).api_error_kind(),
            Some(ApiErrorKind::RateLimitExceeded)
        );
        assert_eq!(api(429, None).api_error_kind(), Some(ApiErrorKind::RateLimitExceeded));
    }

    #[test]
    fn api_error_kind_classifies_manual_sort_required() {
        assert_eq!(
            api(400, Some("manualSortRequired")).api_error_kind(),
            Some(ApiErrorKind::ManualSortRequired)
        );
    }

    #[test]
    fn api_error_kind_classifies_item_gone() {
        assert_eq!(api(404, None).api_error_kind(), Some(ApiErrorKind::ItemGone));
        assert_eq!(api(403, Some("videoNotFound")).api_error_kind(), Some(ApiErrorKind::ItemGone));
        assert_eq!(api(403, Some("forbidden")).api_error_kind(), Some(ApiErrorKind::ItemGone));
    }

    #[test]
    fn api_error_kind_falls_back_to_other() {
        assert_eq!(api(400, Some("somethingWeird")).api_error_kind(), Some(ApiErrorKind::Other));
        assert_eq!(api(401, None).api_error_kind(), Some(ApiErrorKind::Other));
    }

    #[test]
    fn api_error_kind_is_none_for_non_api_errors() {
        assert_eq!(Error::from(AuthError::InvalidGrant).api_error_kind(), None);
    }

    #[test]
    fn reauthorization_covers_rejected_missing_and_undecryptable_tokens() {
        assert!(Error::from(AuthError::InvalidGrant).needs_reauthorization());
        assert!(Error::from(AuthError::NoRefreshToken).needs_reauthorization());
        assert!(Error::from(AuthError::SecretUndecryptable).needs_reauthorization());
        assert!(!Error::from(AuthError::KeyringUnavailable("x".into())).needs_reauthorization());
        assert!(!Error::from(AuthError::Store("x".into())).needs_reauthorization());
        assert!(!api(401, None).needs_reauthorization());
    }

    #[test]
    fn storage_error_predicates() {
        assert!(Error::from(AuthError::SecretUndecryptable).is_secret_undecryptable());
        assert!(!Error::from(AuthError::InvalidGrant).is_secret_undecryptable());
        assert!(Error::from(AuthError::KeyringUnavailable("x".into())).is_keyring_unavailable());
        assert!(!Error::from(AuthError::Store("x".into())).is_keyring_unavailable());
    }

    #[test]
    fn unauthorized_is_only_a_401_api_error() {
        assert!(api(401, None).is_unauthorized());
        assert!(!api(403, Some("forbidden")).is_unauthorized());
        assert!(!Error::from(AuthError::InvalidGrant).is_unauthorized());
    }
}
