//! Refresh-token storage, keyed by YouTube `channel_id`. Access tokens are never persisted:
//! [`super::accounts::AccountManager`] keeps them in memory only.
//!
//! The OS-backed stores (keyring, DPAPI file for portable builds) implement [`TokenStore`] in
//! their own modules; [`InMemoryTokenStore`] is the test double and the default seam.

use std::collections::HashMap;
use std::fmt;
use std::sync::Mutex;

use crate::error::Error;

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
}
