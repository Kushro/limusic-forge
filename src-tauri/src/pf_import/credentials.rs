//! PlaylistForge's refresh tokens, read from its keyring service (`ytdata::PF_KEYRING_SERVICE`,
//! account = channel id) so a connected account does not have to sign in again.
//!
//! Only `load` is ever called: [`pf_token_store`] is read-only (its `save`/`delete` fail before
//! touching the keyring) and [`read_pf_refresh_token`] calls nothing else on whatever store it is
//! given. A refresh token is bound to the OAuth client that minted it, so one is only handed over
//! when PlaylistForge's client id is Forge's, or Forge has none yet (it then takes PlaylistForge's
//! `client_secret.json` too, D-F7). Saving it under Forge's own store, with the user's opt-in, is
//! commit 31's.

use std::fmt;

use ytdata::auth::store::{validate_account_id, KeyringTokenStore, TokenStore};

/// The read-only store over PlaylistForge's keyring entries.
pub fn pf_token_store() -> KeyringTokenStore {
    KeyringTokenStore::read_only(ytdata::PF_KEYRING_SERVICE)
}

/// A refresh token. `Debug` never shows it; [`Self::expose`] is for handing it to a store.
#[derive(Clone, PartialEq, Eq)]
pub struct RefreshToken(String);

impl RefreshToken {
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for RefreshToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("RefreshToken(***)")
    }
}

/// Why a token was not read. Never carries the token.
#[derive(Debug, thiserror::Error)]
pub enum CredentialError {
    #[error("PlaylistForge has no usable client_secret.json, so its tokens cannot be used")]
    NoPfClient,
    #[error("PlaylistForge's OAuth client is not the one Forge uses")]
    ClientMismatch,
    #[error(transparent)]
    Store(#[from] ytdata::error::Error),
}

impl CredentialError {
    pub fn code(&self) -> &'static str {
        match self {
            CredentialError::NoPfClient => "no_pf_client",
            CredentialError::ClientMismatch => "client_mismatch",
            CredentialError::Store(e) if e.is_keyring_unavailable() => "keyring_unavailable",
            CredentialError::Store(_) => "store",
        }
    }
}

/// Whether PlaylistForge's tokens work with Forge's OAuth client: PlaylistForge must have one, and
/// Forge either none or the same.
pub fn clients_compatible(
    pf_client_id: Option<&str>,
    forge_client_id: Option<&str>,
) -> Result<(), CredentialError> {
    let pf = pf_client_id.map(str::trim).filter(|s| !s.is_empty());
    let forge = forge_client_id.map(str::trim).filter(|s| !s.is_empty());
    match (pf, forge) {
        (None, _) => Err(CredentialError::NoPfClient),
        (Some(_), None) => Ok(()),
        (Some(pf), Some(forge)) if pf == forge => Ok(()),
        (Some(_), Some(_)) => Err(CredentialError::ClientMismatch),
    }
}

/// PlaylistForge's refresh token for `channel_id`, `Ok(None)` when it has none. Calls `load` and
/// nothing else on `store` (in the app, [`pf_token_store`]; blocking: run it off the async
/// runtime).
pub fn read_pf_refresh_token(
    store: &dyn TokenStore,
    channel_id: &str,
    pf_client_id: Option<&str>,
    forge_client_id: Option<&str>,
) -> Result<Option<RefreshToken>, CredentialError> {
    clients_compatible(pf_client_id, forge_client_id)?;
    validate_account_id(channel_id)?;
    Ok(store.load(channel_id)?.filter(|t| !t.trim().is_empty()).map(RefreshToken))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use ytdata::auth::store::InMemoryTokenStore;
    use ytdata::error::Error;

    const PF_CLIENT: &str = "FAKE-pf-client-0001.apps.googleusercontent.com";
    const CHANNEL: &str = "UCfakeAccountOne00000001";

    /// Records every call; refuses writes loudly.
    #[derive(Default)]
    struct Spy {
        inner: InMemoryTokenStore,
        calls: Mutex<Vec<&'static str>>,
    }

    impl TokenStore for Spy {
        fn save(&self, _: &str, _: &str) -> Result<(), Error> {
            self.calls.lock().unwrap().push("save");
            panic!("the importer must never write to PlaylistForge's store");
        }
        fn load(&self, account_id: &str) -> Result<Option<String>, Error> {
            self.calls.lock().unwrap().push("load");
            self.inner.load(account_id)
        }
        fn delete(&self, _: &str) -> Result<(), Error> {
            self.calls.lock().unwrap().push("delete");
            panic!("the importer must never delete from PlaylistForge's store");
        }
    }

    #[test]
    fn pf_import_reads_the_token_with_load_only() {
        let spy = Spy::default();
        spy.inner.save(CHANNEL, "fake-refresh-token").unwrap();
        let token = read_pf_refresh_token(&spy, CHANNEL, Some(PF_CLIENT), None).unwrap().unwrap();
        assert_eq!(token.expose(), "fake-refresh-token");
        assert_eq!(
            read_pf_refresh_token(&spy, "UCnobody", Some(PF_CLIENT), Some(PF_CLIENT)).unwrap(),
            None
        );
        assert_eq!(*spy.calls.lock().unwrap(), ["load", "load"]);
        // Still there: reading takes nothing away.
        assert_eq!(spy.inner.load(CHANNEL).unwrap().as_deref(), Some("fake-refresh-token"));
    }

    #[test]
    fn pf_import_token_needs_a_matching_client() {
        let spy = Spy::default();
        spy.inner.save(CHANNEL, "fake-refresh-token").unwrap();
        let other = "FAKE-forge-client-0002.apps.googleusercontent.com";
        let err = read_pf_refresh_token(&spy, CHANNEL, Some(PF_CLIENT), Some(other)).unwrap_err();
        assert!(matches!(err, CredentialError::ClientMismatch));
        assert_eq!(err.code(), "client_mismatch");
        let err = read_pf_refresh_token(&spy, CHANNEL, None, None).unwrap_err();
        assert!(matches!(err, CredentialError::NoPfClient));
        let err = read_pf_refresh_token(&spy, CHANNEL, Some("  "), None).unwrap_err();
        assert!(matches!(err, CredentialError::NoPfClient));
        assert!(spy.calls.lock().unwrap().is_empty(), "nothing read when the clients differ");
        // Same client (surrounding blanks aside), or none on Forge's side: fine.
        clients_compatible(Some(PF_CLIENT), Some(&format!(" {PF_CLIENT} "))).unwrap();
        clients_compatible(Some(PF_CLIENT), Some("")).unwrap();
    }

    #[test]
    fn pf_import_token_rejects_odd_ids_and_hides_the_token() {
        let store = InMemoryTokenStore::default();
        store.save("../x", "t").unwrap();
        assert!(read_pf_refresh_token(&store, "../x", Some(PF_CLIENT), None).is_err());
        store.save(CHANNEL, "   ").unwrap();
        assert_eq!(read_pf_refresh_token(&store, CHANNEL, Some(PF_CLIENT), None).unwrap(), None);
        let shown = format!("{:?}", RefreshToken("fake-secret-token".into()));
        assert!(!shown.contains("fake-secret-token"), "{shown}");
    }

    /// Building the store touches no keyring; it is read-only and names PlaylistForge's service.
    #[test]
    fn pf_import_pf_token_store_is_read_only() {
        let store = pf_token_store();
        assert!(store.is_read_only());
        assert_eq!(store.service(), "PlaylistForge");
        assert!(store.save(CHANNEL, "x").is_err());
        assert!(store.delete(CHANNEL).is_err());
    }
}
