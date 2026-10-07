//! Refresh-token storage, keyed by YouTube `channel_id`. Access tokens are never persisted:
//! [`super::accounts::AccountManager`] keeps them in memory only.
//!
//! - [`KeyringTokenStore`]: the OS keyring (Windows Credential Manager, macOS Keychain, the
//!   Secret Service on Linux). Without a usable keyring every call fails with
//!   [`AuthError::KeyringUnavailable`]; there is no fallback to a file in clear.
//! - `super::dpapi::DpapiFileStore` (Windows only): one DPAPI-encrypted file per account, for
//!   portable builds.
//! - [`InMemoryTokenStore`]: the test double and the default seam.
//!
//! The OS-backed stores reject account ids that are not plain channel ids
//! ([`validate_account_id`]) and are blocking: call them from `spawn_blocking` when on an async
//! executor that must not stall.

use std::collections::HashMap;
use std::fmt;
use std::sync::Mutex;

use crate::error::{AuthError, Error};

/// Longest account id accepted. YouTube channel ids are 24 characters.
const MAX_ACCOUNT_ID_LEN: usize = 128;

/// Accepts only non-empty ids of ASCII letters, digits, `-` and `_` (at most 128 characters),
/// so an id can safely become part of a file name or a keyring entry.
pub fn validate_account_id(account_id: &str) -> Result<(), Error> {
    let ok = !account_id.is_empty()
        && account_id.len() <= MAX_ACCOUNT_ID_LEN
        && account_id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
    if ok {
        Ok(())
    } else {
        Err(AuthError::InvalidAccountId(account_id.chars().take(MAX_ACCOUNT_ID_LEN).collect())
            .into())
    }
}

/// Pluggable refresh-token storage. Implementations must never put the token itself in an
/// error message or a log line.
pub trait TokenStore: Send + Sync {
    fn save(&self, account_id: &str, refresh_token: &str) -> Result<(), Error>;
    /// `Ok(None)` when nothing is stored for this account (not an error).
    fn load(&self, account_id: &str) -> Result<Option<String>, Error>;
    /// Deleting a missing entry is not an error (idempotent).
    fn delete(&self, account_id: &str) -> Result<(), Error>;
}

/// In-memory store. `Debug` lists account ids only, never tokens.
#[derive(Default)]
pub struct InMemoryTokenStore {
    inner: Mutex<HashMap<String, String>>,
}

impl fmt::Debug for InMemoryTokenStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let accounts: Vec<String> = match self.inner.lock() {
            Ok(map) => map.keys().cloned().collect(),
            Err(_) => Vec::new(),
        };
        f.debug_struct("InMemoryTokenStore").field("accounts", &accounts).finish()
    }
}

impl TokenStore for InMemoryTokenStore {
    fn save(&self, account_id: &str, refresh_token: &str) -> Result<(), Error> {
        self.inner
            .lock()
            .expect("in-memory token store mutex poisoned")
            .insert(account_id.to_string(), refresh_token.to_string());
        Ok(())
    }

    fn load(&self, account_id: &str) -> Result<Option<String>, Error> {
        Ok(self
            .inner
            .lock()
            .expect("in-memory token store mutex poisoned")
            .get(account_id)
            .cloned())
    }

    fn delete(&self, account_id: &str) -> Result<(), Error> {
        self.inner.lock().expect("in-memory token store mutex poisoned").remove(account_id);
        Ok(())
    }
}

/// OS keyring store: one entry per account, `service` + `username = channel_id`. Only the
/// refresh token is stored; the access token lives in memory.
///
/// A store for [`crate::PF_KEYRING_SERVICE`] (or one built with [`Self::read_only`]) only
/// loads: `save` and `delete` fail before touching the keyring, so PlaylistForge's own
/// credentials are never modified.
#[derive(Debug, Clone)]
pub struct KeyringTokenStore {
    service: String,
    read_only: bool,
}

impl KeyringTokenStore {
    /// Read-write store under `service`, except for [`crate::PF_KEYRING_SERVICE`], which is
    /// always read-only.
    pub fn new(service: impl Into<String>) -> Self {
        let service = service.into();
        let read_only = service == crate::PF_KEYRING_SERVICE;
        Self { service, read_only }
    }

    /// Store that only ever loads from `service` (importing another app's tokens).
    pub fn read_only(service: impl Into<String>) -> Self {
        Self { service: service.into(), read_only: true }
    }

    pub fn service(&self) -> &str {
        &self.service
    }

    pub fn is_read_only(&self) -> bool {
        self.read_only
    }

    fn ensure_writable(&self) -> Result<(), Error> {
        if self.read_only {
            return Err(AuthError::Store(format!(
                "the keyring service {:?} is read-only for this app",
                self.service
            ))
            .into());
        }
        Ok(())
    }

    fn entry(&self, account_id: &str) -> Result<keyring::Entry, Error> {
        validate_account_id(account_id)?;
        keyring::Entry::new(&self.service, account_id)
            .map_err(|err| keyring_error(err, true).into())
    }
}

impl TokenStore for KeyringTokenStore {
    fn save(&self, account_id: &str, refresh_token: &str) -> Result<(), Error> {
        self.ensure_writable()?;
        let entry = self.entry(account_id)?;
        entry.set_password(refresh_token).map_err(|err| keyring_error(err, false))?;
        Ok(())
    }

    fn load(&self, account_id: &str) -> Result<Option<String>, Error> {
        let entry = self.entry(account_id)?;
        match entry.get_password() {
            Ok(token) => Ok(Some(token)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(err) => Err(keyring_error(err, false).into()),
        }
    }

    fn delete(&self, account_id: &str) -> Result<(), Error> {
        self.ensure_writable()?;
        let entry = self.entry(account_id)?;
        match entry.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(err) => Err(keyring_error(err, false).into()),
        }
    }
}

/// Maps a `keyring` failure to [`AuthError`] without ever formatting secret bytes.
///
/// "No keyring" shows up as `NoDefaultStore` (the platform store could not be created; later
/// calls), as a platform failure while opening the entry (the first call: on Linux, the
/// Secret Service connection failing) or as `NoStorageAccess` (locked / prompt dismissed). All
/// of them are [`AuthError::KeyringUnavailable`]; anything else is [`AuthError::Store`].
fn keyring_error(err: keyring::Error, opening: bool) -> AuthError {
    match err {
        keyring::Error::NoDefaultStore => {
            AuthError::KeyringUnavailable("no credential store could be opened".to_string())
        }
        keyring::Error::NoStorageAccess(platform) => AuthError::KeyringUnavailable(format!(
            "the credential store is locked or not accessible: {platform}"
        )),
        keyring::Error::PlatformFailure(platform) if opening => {
            AuthError::KeyringUnavailable(platform.to_string())
        }
        // Both carry the stored bytes: never format them.
        keyring::Error::BadEncoding(_) => {
            AuthError::Store("the stored credential is not valid UTF-8".to_string())
        }
        keyring::Error::BadDataFormat(_, _) => {
            AuthError::Store("the stored credential is malformed".to_string())
        }
        other => AuthError::Store(other.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn in_memory_store_round_trips() {
        let store = InMemoryTokenStore::default();
        assert_eq!(store.load("acct1").unwrap(), None);
        store.save("acct1", "refresh-token-abc").unwrap();
        assert_eq!(store.load("acct1").unwrap(), Some("refresh-token-abc".to_string()));
        store.delete("acct1").unwrap();
        assert_eq!(store.load("acct1").unwrap(), None);
    }

    #[test]
    fn in_memory_store_delete_of_missing_entry_is_not_an_error() {
        let store = InMemoryTokenStore::default();
        store.delete("does-not-exist").unwrap();
    }

    #[test]
    fn in_memory_store_keeps_accounts_independent() {
        let store = InMemoryTokenStore::default();
        store.save("acct1", "token-1").unwrap();
        store.save("acct2", "token-2").unwrap();
        assert_eq!(store.load("acct1").unwrap(), Some("token-1".to_string()));
        assert_eq!(store.load("acct2").unwrap(), Some("token-2".to_string()));
        store.delete("acct1").unwrap();
        assert_eq!(store.load("acct1").unwrap(), None);
        assert_eq!(store.load("acct2").unwrap(), Some("token-2".to_string()));
    }

    #[test]
    fn in_memory_store_debug_shows_ids_not_tokens() {
        let store = InMemoryTokenStore::default();
        store.save("UC-debug", "super-secret-refresh-token").unwrap();
        let debug = format!("{store:?}");
        assert!(debug.contains("UC-debug"), "{debug}");
        assert!(!debug.contains("super-secret-refresh-token"), "{debug}");
    }

    #[test]
    fn account_ids_are_plain_channel_ids() {
        let longest = "x".repeat(MAX_ACCOUNT_ID_LEN);
        for ok in ["UCabcdefghijklmnopqrstuv", "UC-x_Y9", "a", longest.as_str()] {
            validate_account_id(ok).unwrap_or_else(|e| panic!("{ok:?} rejected: {e}"));
        }
        let too_long = "x".repeat(MAX_ACCOUNT_ID_LEN + 1);
        let bad_ids = [
            "",
            ".",
            "..",
            "../UC1",
            "UC/1",
            "UC\\1",
            "UC.1",
            "UC 1",
            "UC:1",
            "UCé",
            too_long.as_str(),
        ];
        for bad in bad_ids {
            let err = validate_account_id(bad).unwrap_err();
            assert!(matches!(err, Error::Auth(AuthError::InvalidAccountId(_))), "{bad:?}: {err}");
        }
    }

    fn platform(msg: &str) -> Box<dyn std::error::Error + Send + Sync> {
        Box::new(std::io::Error::other(msg.to_string()))
    }

    #[test]
    fn missing_keyring_maps_to_keyring_unavailable() {
        assert!(matches!(
            keyring_error(keyring::Error::NoDefaultStore, false),
            AuthError::KeyringUnavailable(_)
        ));
        assert!(matches!(
            keyring_error(keyring::Error::PlatformFailure(platform("no dbus")), true),
            AuthError::KeyringUnavailable(_)
        ));
        assert!(matches!(
            keyring_error(keyring::Error::NoStorageAccess(platform("locked")), false),
            AuthError::KeyringUnavailable(_)
        ));
    }

    #[test]
    fn other_keyring_failures_map_to_store_errors() {
        assert!(matches!(
            keyring_error(keyring::Error::PlatformFailure(platform("boom")), false),
            AuthError::Store(_)
        ));
        assert!(matches!(
            keyring_error(keyring::Error::Invalid("user".into(), "bad".into()), true),
            AuthError::Store(_)
        ));
    }

    #[test]
    fn keyring_errors_never_echo_stored_bytes() {
        let secret = b"super-secret-refresh-token".to_vec();
        for err in [
            keyring::Error::BadEncoding(secret.clone()),
            keyring::Error::BadDataFormat(secret.clone(), platform("bad")),
        ] {
            let msg = keyring_error(err, false).to_string();
            assert!(!msg.contains("super-secret"), "{msg}");
        }
    }

    /// The PlaylistForge service is only read: writes are refused before the keyring is touched
    /// (so this runs everywhere and never reaches the real PlaylistForge entries).
    #[test]
    fn playlistforge_keyring_is_read_only() {
        let store = KeyringTokenStore::new(crate::PF_KEYRING_SERVICE);
        assert!(store.is_read_only());
        let err = store.save("UCtest", "token").unwrap_err();
        assert!(matches!(err, Error::Auth(AuthError::Store(_))), "{err}");
        let err = store.delete("UCtest").unwrap_err();
        assert!(matches!(err, Error::Auth(AuthError::Store(_))), "{err}");

        assert!(KeyringTokenStore::read_only("Other").is_read_only());
        assert!(!KeyringTokenStore::new(crate::KEYRING_SERVICE).is_read_only());
    }

    /// Invalid ids are rejected before the keyring is touched.
    #[test]
    fn keyring_store_rejects_invalid_ids_without_touching_the_keyring() {
        let store = KeyringTokenStore::new("LiMusicForge-ytdata-tests");
        let err = store.load("../evil").unwrap_err();
        assert!(matches!(err, Error::Auth(AuthError::InvalidAccountId(_))), "{err}");
    }

    /// Real Windows Credential Manager round trip under a dedicated test service (never
    /// `KEYRING_SERVICE` nor PlaylistForge's). Ignored by default so a plain `cargo test` never
    /// writes to the user's credential store: `cargo test -p ytdata -- --ignored keyring_real`.
    #[cfg(windows)]
    #[test]
    #[ignore = "writes to the real Windows Credential Manager"]
    fn keyring_real_round_trip_against_credential_manager() {
        let store = KeyringTokenStore::new("LiMusicForge-ytdata-tests");
        let account_id = "UC-ytdata-keyring-test";
        let _ = store.delete(account_id); // leftovers from an interrupted run

        let result = std::panic::catch_unwind(|| {
            assert_eq!(store.load(account_id).unwrap(), None);
            store.save(account_id, "unit-test-refresh-token").unwrap();
            assert_eq!(store.load(account_id).unwrap().as_deref(), Some("unit-test-refresh-token"));
            store.save(account_id, "unit-test-refresh-token-2").unwrap();
            assert_eq!(
                store.load(account_id).unwrap().as_deref(),
                Some("unit-test-refresh-token-2")
            );
            store.delete(account_id).unwrap();
            assert_eq!(store.load(account_id).unwrap(), None);
            store.delete(account_id).unwrap();
        });
        let _ = store.delete(account_id); // always clean up
        if let Err(panic) = result {
            std::panic::resume_unwind(panic);
        }
    }
}
