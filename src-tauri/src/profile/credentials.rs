//! Data API refresh tokens in and out of the token store, for the profile export/import.
//!
//! A refresh token is bound to the OAuth client that minted it, so tokens only travel together
//! with the `client_secret.json` they came from and are refused on a machine whose own client has
//! a different id ([`crate::pf_import::credentials::clients_compatible`] decides, the same rule
//! the PlaylistForge importer uses). Reports carry channel ids and short codes, never a token.

use std::collections::BTreeMap;

use serde::Serialize;
use ytdata::auth::store::{validate_account_id, TokenStore};
use ytdata::error::Error;

use crate::pf_import::credentials::{clients_compatible, CredentialError};

/// What happened to each channel's token. `failed` holds `(channel_id, code)`.
#[derive(Debug, Default, Serialize)]
pub struct TokenReport {
    pub saved: Vec<String>,
    pub failed: Vec<(String, String)>,
}

fn code_of(err: &Error) -> &'static str {
    if err.is_keyring_unavailable() {
        "keyring_unavailable"
    } else {
        "store"
    }
}

/// The refresh token of every channel in `channel_ids` that has one. A channel with no stored
/// token is simply absent; one whose entry could not be read goes to `failed`. Blocking.
pub fn collect_tokens(
    store: &dyn TokenStore,
    channel_ids: &[String],
) -> (BTreeMap<String, String>, TokenReport) {
    let mut tokens = BTreeMap::new();
    let mut report = TokenReport::default();
    for id in channel_ids {
        if validate_account_id(id).is_err() {
            report.failed.push((id.clone(), "invalid_id".to_string()));
            continue;
        }
        match store.load(id) {
            Ok(Some(token)) if !token.trim().is_empty() => {
                tokens.insert(id.clone(), token);
            }
            Ok(_) => {}
            Err(e) => report.failed.push((id.clone(), code_of(&e).to_string())),
        }
    }
    (tokens, report)
}

/// Saves `tokens` into the destination store. Refuses every one (`client_mismatch`, or `no_client`
/// when the bundle carries no client to tie them to) unless the bundle's client id is compatible
/// with `dest_client_secret`'s. When the keyring is unavailable the first failure marks the rest
/// `keyring_unavailable` without asking it again. Blocking.
pub fn apply_tokens(
    store: &dyn TokenStore,
    tokens: &BTreeMap<String, String>,
    bundle_client_secret: Option<&str>,
    dest_client_secret: Option<&str>,
) -> TokenReport {
    let mut report = TokenReport::default();
    if tokens.is_empty() {
        return report;
    }
    let bundle_id = bundle_client_secret.and_then(client_id_of);
    let dest_id = dest_client_secret.and_then(client_id_of);
    if let Err(e) = clients_compatible(bundle_id.as_deref(), dest_id.as_deref()) {
        let code = match e {
            CredentialError::NoPfClient => "no_client",
            _ => "client_mismatch",
        };
        report.failed = tokens.keys().map(|id| (id.clone(), code.to_string())).collect();
        return report;
    }

    let mut keyring_down = false;
    for (id, token) in tokens {
        if keyring_down {
            report.failed.push((id.clone(), "keyring_unavailable".into()));
            continue;
        }
        if validate_account_id(id).is_err() {
            report.failed.push((id.clone(), "invalid_id".into()));
            continue;
        }
        match store.save(id, token) {
            Ok(()) => report.saved.push(id.clone()),
            Err(e) => {
                keyring_down = e.is_keyring_unavailable();
                report.failed.push((id.clone(), code_of(&e).to_string()));
            }
        }
    }
    report
}

/// The OAuth client id of a Google `client_secret.json` (`installed.client_id`, or `web.client_id`
/// for a "Web application" client), trimmed; `None` when the JSON is invalid or has none.
pub fn client_id_of(client_secret_json: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(client_secret_json).ok()?;
    ["installed", "web"]
        .iter()
        .find_map(|k| value.get(*k)?.get("client_id")?.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ytdata::auth::store::InMemoryTokenStore;
    use ytdata::error::AuthError;

    const CH1: &str = "UCfakeAccountOne00000001";
    const CH2: &str = "UCfakeAccountTwo00000002";

    fn secret(id: &str) -> String {
        format!(r#"{{"installed":{{"client_id":"{id}","client_secret":"FAKE-secret"}}}}"#)
    }

    /// Every call fails the way a Linux box without a Secret Service does.
    struct NoKeyring;

    impl TokenStore for NoKeyring {
        fn save(&self, _: &str, _: &str) -> Result<(), Error> {
            Err(AuthError::KeyringUnavailable("no secret service".into()).into())
        }
        fn load(&self, _: &str) -> Result<Option<String>, Error> {
            Err(AuthError::KeyringUnavailable("no secret service".into()).into())
        }
        fn delete(&self, _: &str) -> Result<(), Error> {
            Ok(())
        }
    }

    #[test]
    fn profile_credentials_client_id_parsing() {
        assert_eq!(
            client_id_of(&secret(" abc.apps.googleusercontent.com ")).as_deref(),
            Some("abc.apps.googleusercontent.com")
        );
        assert_eq!(client_id_of(r#"{"web":{"client_id":"w-1"}}"#).as_deref(), Some("w-1"));
        assert_eq!(client_id_of("not json"), None);
        assert_eq!(client_id_of(r#"{"installed":{}}"#), None);
        assert_eq!(client_id_of(r#"{"installed":{"client_id":"  "}}"#), None);
    }

    #[test]
    fn profile_credentials_collect_and_apply_round_trip() {
        let src = InMemoryTokenStore::default();
        src.save(CH1, "fake-refresh-1").unwrap();
        // CH2 has no token; "bad id" is not a channel id.
        let ids = vec![CH1.to_string(), CH2.to_string(), "bad/id".to_string()];
        let (tokens, report) = collect_tokens(&src, &ids);
        assert_eq!(tokens.len(), 1);
        assert_eq!(report.failed, vec![("bad/id".to_string(), "invalid_id".to_string())]);

        let dest = InMemoryTokenStore::default();
        let client = secret("client-A");
        // Destination without a client yet: accepted (the import brings the client_secret along).
        let r = apply_tokens(&dest, &tokens, Some(&client), None);
        assert_eq!(r.saved, vec![CH1.to_string()]);
        assert!(r.failed.is_empty());
        assert_eq!(dest.load(CH1).unwrap().as_deref(), Some("fake-refresh-1"));
        // Same client: fine too.
        let r = apply_tokens(&dest, &tokens, Some(&client), Some(&client));
        assert_eq!(r.saved.len(), 1);
    }

    #[test]
    fn profile_credentials_client_mismatch_refuses_all_tokens() {
        let mut tokens = BTreeMap::new();
        tokens.insert(CH1.to_string(), "fake-1".to_string());
        tokens.insert(CH2.to_string(), "fake-2".to_string());
        let dest = InMemoryTokenStore::default();

        let r = apply_tokens(&dest, &tokens, Some(&secret("client-A")), Some(&secret("client-B")));
        assert!(r.saved.is_empty());
        assert_eq!(r.failed.len(), 2);
        assert!(r.failed.iter().all(|(_, c)| c == "client_mismatch"));
        assert_eq!(dest.load(CH1).unwrap(), None, "nothing written");

        // Tokens without any client in the bundle cannot be tied to one.
        let r = apply_tokens(&dest, &tokens, None, None);
        assert!(r.saved.is_empty() && r.failed.iter().all(|(_, c)| c == "no_client"));
        assert_eq!(dest.load(CH2).unwrap(), None);
    }

    #[test]
    fn profile_credentials_keyring_unavailable_code() {
        let mut tokens = BTreeMap::new();
        tokens.insert(CH1.to_string(), "fake-1".to_string());
        tokens.insert(CH2.to_string(), "fake-2".to_string());
        let r = apply_tokens(&NoKeyring, &tokens, Some(&secret("c")), None);
        assert!(r.saved.is_empty());
        assert_eq!(r.failed.len(), 2);
        assert!(r.failed.iter().all(|(_, c)| c == "keyring_unavailable"));

        let (got, report) = collect_tokens(&NoKeyring, &[CH1.to_string()]);
        assert!(got.is_empty());
        assert_eq!(report.failed, vec![(CH1.to_string(), "keyring_unavailable".to_string())]);
    }
}
