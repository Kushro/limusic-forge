//! Multi-account management: add/remove accounts, resolve a valid access token (refreshing
//! when needed), the "401 → refresh → one retry" policy, and the `Connected` /
//! `ReauthRequired` status machine driven by `invalid_grant`.
//!
//! Metadata persistence is behind [`AccountsRepo`] (the app stores it in its database) and
//! refresh tokens behind [`TokenStore`]; this module ships only the in-memory repo.

use std::collections::HashMap;
use std::fmt;
use std::future::Future;
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use super::flow::{self, AuthorizedTokens, GOOGLE_REVOKE_URI};
use super::store::TokenStore;
use crate::client::YouTubeClient;
use crate::client_secret::ClientSecretFile;
use crate::error::{AuthError, Error};

/// Margin before expiry at which a cached access token is considered stale.
const EXPIRY_SKEW: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AccountStatus {
    /// A refresh token is stored and, as far as we know, still valid.
    Connected,
    /// The last refresh returned `invalid_grant`. The account, its stored refresh token and all
    /// local data are kept; only a new OAuth round-trip clears this.
    ReauthRequired,
}

impl AccountStatus {
    /// The string stored in the app's database (`connected` / `reauth_required`).
    pub fn as_str(self) -> &'static str {
        match self {
            AccountStatus::Connected => "connected",
            AccountStatus::ReauthRequired => "reauth_required",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "connected" => Some(AccountStatus::Connected),
            "reauth_required" => Some(AccountStatus::ReauthRequired),
            _ => None,
        }
    }
}

/// Non-secret account metadata.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Account {
    pub channel_id: String,
    pub title: String,
    pub thumbnail_url: Option<String>,
    pub status: AccountStatus,
    /// Unix seconds.
    pub added_at_unix: u64,
}

/// Account metadata persistence. Refresh tokens never go through this trait.
pub trait AccountsRepo: Send + Sync {
    fn load(&self) -> Result<Vec<Account>, Error>;
    fn save(&self, accounts: &[Account]) -> Result<(), Error>;
}

/// In-memory [`AccountsRepo`].
#[derive(Debug, Default)]
pub struct InMemoryAccountsRepo {
    inner: Mutex<Vec<Account>>,
}

impl AccountsRepo for InMemoryAccountsRepo {
    fn load(&self) -> Result<Vec<Account>, Error> {
        Ok(self.inner.lock().expect("in-memory accounts repo mutex poisoned").clone())
    }

    fn save(&self, accounts: &[Account]) -> Result<(), Error> {
        *self.inner.lock().expect("in-memory accounts repo mutex poisoned") = accounts.to_vec();
        Ok(())
    }
}

struct CachedAccessToken {
    token: String,
    expires_at: Instant,
}

impl fmt::Debug for CachedAccessToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CachedAccessToken")
            .field("token", &"***")
            .field("expires_at", &self.expires_at)
            .finish()
    }
}

/// Multi-account façade. Holds the injected [`TokenStore`] and [`AccountsRepo`], the imported
/// client secret (settable after construction, since the manager exists before onboarding),
/// and an in-memory-only access-token cache.
///
/// No lock here is ever held across an `.await`, so plain `std::sync` locks suffice.
pub struct AccountManager {
    store: Arc<dyn TokenStore>,
    repo: Arc<dyn AccountsRepo>,
    client_secret: RwLock<Option<ClientSecretFile>>,
    yt_client: YouTubeClient,
    revoke_uri: String,
    accounts: Mutex<Vec<Account>>,
    access_tokens: Mutex<HashMap<String, CachedAccessToken>>,
}

impl AccountManager {
    pub fn new(store: Arc<dyn TokenStore>, repo: Arc<dyn AccountsRepo>) -> Result<Self, Error> {
        let accounts = repo.load()?;
        Ok(Self {
            store,
            repo,
            client_secret: RwLock::new(None),
            yt_client: YouTubeClient::new()?,
            revoke_uri: GOOGLE_REVOKE_URI.to_string(),
            accounts: Mutex::new(accounts),
            access_tokens: Mutex::new(HashMap::new()),
        })
    }

    /// Overrides the client used for `channels.list` on [`Self::add_account`].
    #[must_use]
    pub fn with_yt_client(mut self, client: YouTubeClient) -> Self {
        self.yt_client = client;
        self
    }

    /// Overrides the revoke endpoint (tests).
    #[must_use]
    pub fn with_revoke_uri(mut self, uri: impl Into<String>) -> Self {
        self.revoke_uri = uri.into();
        self
    }

    /// Sets/replaces the OAuth client used for refresh. Until called, refreshes fail with
    /// [`AuthError::NoClientSecret`].
    pub fn set_client_secret(&self, secret: ClientSecretFile) {
        *self.client_secret.write().expect("client_secret lock poisoned") = Some(secret);
    }

    pub fn client_secret(&self) -> Option<ClientSecretFile> {
        self.client_secret.read().expect("client_secret lock poisoned").clone()
    }

    /// Current accounts, in insertion order.
    pub fn accounts(&self) -> Vec<Account> {
        self.accounts.lock().expect("accounts lock poisoned").clone()
    }

    pub fn account(&self, channel_id: &str) -> Option<Account> {
        self.accounts
            .lock()
            .expect("accounts lock poisoned")
            .iter()
            .find(|a| a.channel_id == channel_id)
            .cloned()
    }

    /// Registers a freshly authorized account: resolves its channel via
    /// `channels.list?mine=true`, stores the refresh token and upserts the metadata. An existing
    /// `channel_id` (re-authorizing a `ReauthRequired` account, or connecting the same login
    /// again) is updated back to `Connected` instead of duplicated.
    pub async fn add_account(&self, tokens: AuthorizedTokens) -> Result<Account, Error> {
        let channel = self.yt_client.channels_list_mine(&tokens.access_token).await?;

        self.store.save(&channel.channel_id, &tokens.refresh_token)?;
        self.access_tokens.lock().expect("access_tokens lock poisoned").insert(
            channel.channel_id.clone(),
            CachedAccessToken { token: tokens.access_token, expires_at: tokens.expires_at },
        );

        let mut accounts = self.accounts.lock().expect("accounts lock poisoned");
        let account = match accounts.iter_mut().find(|a| a.channel_id == channel.channel_id) {
            Some(existing) => {
                existing.title = channel.title;
                existing.thumbnail_url = channel.thumbnail_url;
                existing.status = AccountStatus::Connected;
                existing.clone()
            }
            None => {
                let account = Account {
                    channel_id: channel.channel_id,
                    title: channel.title,
                    thumbnail_url: channel.thumbnail_url,
                    status: AccountStatus::Connected,
                    added_at_unix: unix_now(),
                };
                accounts.push(account.clone());
                account
            }
        };
        self.repo.save(&accounts)?;
        tracing::info!(channel_id = %account.channel_id, "ytdata account connected");
        Ok(account)
    }

    /// Explicit user action: best-effort revoke, then removes the account's credentials and
    /// metadata. A failed revoke never blocks local removal. Playlist data is not touched.
    pub async fn revoke_and_remove_account(&self, channel_id: &str) -> Result<(), Error> {
        if let Ok(Some(refresh_token)) = self.store.load(channel_id) {
            let _ = flow::revoke_token(&refresh_token, &self.revoke_uri).await;
        }
        let _ = self.store.delete(channel_id);
        self.access_tokens.lock().expect("access_tokens lock poisoned").remove(channel_id);

        let mut accounts = self.accounts.lock().expect("accounts lock poisoned");
        accounts.retain(|a| a.channel_id != channel_id);
        self.repo.save(&accounts)?;
        tracing::info!(channel_id, "ytdata account removed");
        Ok(())
    }

    /// A valid access token for `channel_id`, refreshing first when missing or within 60s of
    /// expiry. Transient refresh failures are retried twice with short backoff;
    /// `invalid_grant` is not retried: it marks the account [`AccountStatus::ReauthRequired`]
    /// and returns [`AuthError::InvalidGrant`], leaving the stored refresh token in place.
    pub async fn get_valid_access_token(&self, channel_id: &str) -> Result<String, Error> {
        {
            let cache = self.access_tokens.lock().expect("access_tokens lock poisoned");
            if let Some(cached) = cache.get(channel_id) {
                if cached.expires_at > Instant::now() + EXPIRY_SKEW {
                    return Ok(cached.token.clone());
                }
            }
        }

        let Some(client_secret) = self.client_secret() else {
            return Err(AuthError::NoClientSecret.into());
        };
        let Some(refresh_token) = self.store.load(channel_id)? else {
            return Err(AuthError::NoRefreshToken.into());
        };

        match refresh_with_retry(&client_secret, &refresh_token).await {
            Ok(refreshed) => {
                if let Some(rotated) = &refreshed.refresh_token {
                    self.store.save(channel_id, rotated)?;
                }
                self.access_tokens.lock().expect("access_tokens lock poisoned").insert(
                    channel_id.to_string(),
                    CachedAccessToken {
                        token: refreshed.access_token.clone(),
                        expires_at: refreshed.expires_at,
                    },
                );
                self.set_status(channel_id, AccountStatus::Connected)?;
                tracing::debug!(channel_id, "refreshed access token");
                Ok(refreshed.access_token)
            }
            Err(Error::Auth(AuthError::InvalidGrant)) => {
                tracing::warn!(
                    channel_id,
                    "refresh token rejected (invalid_grant), marking account reauth_required"
                );
                self.access_tokens.lock().expect("access_tokens lock poisoned").remove(channel_id);
                self.set_status(channel_id, AccountStatus::ReauthRequired)?;
                Err(AuthError::InvalidGrant.into())
            }
            Err(other) => Err(other),
        }
    }

    /// Runs `call` with a valid access token. If it fails with HTTP 401, the cached token is
    /// dropped, a refresh is forced and `call` is retried **once** with the new token; a second
    /// 401 is returned as-is. A refresh that hits `invalid_grant` marks the account
    /// `ReauthRequired` (see [`Self::get_valid_access_token`]).
    pub async fn with_access_token<T, F, Fut>(
        &self,
        channel_id: &str,
        mut call: F,
    ) -> Result<T, Error>
    where
        F: FnMut(String) -> Fut,
        Fut: Future<Output = Result<T, Error>>,
    {
        let token = self.get_valid_access_token(channel_id).await?;
        match call(token).await {
            Err(err) if err.is_unauthorized() => {
                tracing::debug!(channel_id, "401 from the Data API, refreshing and retrying once");
                self.invalidate_cached_token(channel_id);
                let token = self.get_valid_access_token(channel_id).await?;
                call(token).await
            }
            other => other,
        }
    }

    /// Forces the next [`Self::get_valid_access_token`] to refresh. Evicting an id that is not
    /// cached is a no-op.
    pub fn invalidate_cached_token(&self, channel_id: &str) {
        self.access_tokens.lock().expect("access_tokens lock poisoned").remove(channel_id);
    }

    fn set_status(&self, channel_id: &str, status: AccountStatus) -> Result<(), Error> {
        let mut accounts = self.accounts.lock().expect("accounts lock poisoned");
        let changed = match accounts.iter_mut().find(|a| a.channel_id == channel_id) {
            Some(account) if account.status != status => {
                account.status = status;
                true
            }
            _ => false,
        };
        if changed {
            self.repo.save(&accounts)?;
        }
        Ok(())
    }
}

/// Retries transient refresh failures twice more with short backoff; `invalid_grant`
/// short-circuits since retrying it can never succeed.
async fn refresh_with_retry(
    client_secret: &ClientSecretFile,
    refresh_token: &str,
) -> Result<flow::RefreshedTokens, Error> {
    const MAX_ATTEMPTS: u32 = 3;
    let mut last_err = None;
    for attempt in 0..MAX_ATTEMPTS {
        match flow::refresh_access_token(client_secret, refresh_token).await {
            Ok(tokens) => return Ok(tokens),
            Err(err @ Error::Auth(AuthError::InvalidGrant)) => return Err(err),
            Err(err) => {
                if attempt + 1 < MAX_ATTEMPTS {
                    let backoff = Duration::from_millis(300 * 2u64.pow(attempt));
                    tracing::debug!(attempt, ?backoff, error = %err, "refresh failed, retrying");
                    tokio::time::sleep(backoff).await;
                }
                last_err = Some(err);
            }
        }
    }
    Err(last_err.expect("loop always assigns last_err before exhausting MAX_ATTEMPTS"))
}

fn unix_now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_account(channel_id: &str, status: AccountStatus) -> Account {
        Account {
            channel_id: channel_id.to_string(),
            title: "Test Channel".to_string(),
            thumbnail_url: Some("http://example.com/thumb.jpg".to_string()),
            status,
            added_at_unix: 1_700_000_000,
        }
    }

    #[test]
    fn account_status_serializes_snake_case() {
        assert_eq!(serde_json::to_string(&AccountStatus::Connected).unwrap(), "\"connected\"");
        assert_eq!(
            serde_json::to_string(&AccountStatus::ReauthRequired).unwrap(),
            "\"reauth_required\""
        );
        for status in [AccountStatus::Connected, AccountStatus::ReauthRequired] {
            assert_eq!(AccountStatus::parse(status.as_str()), Some(status));
        }
        assert_eq!(AccountStatus::parse("bogus"), None);
    }

    #[test]
    fn in_memory_accounts_repo_round_trips() {
        let repo = InMemoryAccountsRepo::default();
        assert_eq!(repo.load().unwrap(), Vec::new());
        let accounts = vec![sample_account("UC1", AccountStatus::Connected)];
        repo.save(&accounts).unwrap();
        assert_eq!(repo.load().unwrap(), accounts);
    }

    #[test]
    fn manager_loads_existing_accounts_from_the_repo() {
        let store: Arc<dyn TokenStore> =
            Arc::new(crate::auth::store::InMemoryTokenStore::default());
        let repo = Arc::new(InMemoryAccountsRepo::default());
        repo.save(&[sample_account("UC1", AccountStatus::ReauthRequired)]).unwrap();
        let manager = AccountManager::new(store, repo).unwrap();
        assert_eq!(manager.accounts().len(), 1);
        assert_eq!(manager.account("UC1").unwrap().status, AccountStatus::ReauthRequired);
    }

    #[test]
    fn invalidate_cached_token_evicts_the_cache_entry() {
        let store: Arc<dyn TokenStore> =
            Arc::new(crate::auth::store::InMemoryTokenStore::default());
        let repo: Arc<dyn AccountsRepo> = Arc::new(InMemoryAccountsRepo::default());
        let manager = AccountManager::new(store, repo).unwrap();

        manager.access_tokens.lock().unwrap().insert(
            "UC1".to_string(),
            CachedAccessToken {
                token: "cached".to_string(),
                expires_at: Instant::now() + Duration::from_secs(600),
            },
        );
        assert!(manager.access_tokens.lock().unwrap().contains_key("UC1"));

        manager.invalidate_cached_token("UC1");
        assert!(!manager.access_tokens.lock().unwrap().contains_key("UC1"));

        manager.invalidate_cached_token("never-cached");
    }

    #[test]
    fn cached_access_token_debug_is_redacted() {
        let cached =
            CachedAccessToken { token: "ya29.FAKE-cached".to_string(), expires_at: Instant::now() };
        let debug = format!("{cached:?}");
        assert!(!debug.contains("FAKE-cached"), "{debug}");
    }

    #[tokio::test]
    async fn get_valid_access_token_without_client_secret_fails_cleanly() {
        let store = Arc::new(crate::auth::store::InMemoryTokenStore::default());
        store.save("UC1", "FAKE-refresh").unwrap();
        let repo: Arc<dyn AccountsRepo> = Arc::new(InMemoryAccountsRepo::default());
        let manager = AccountManager::new(store.clone(), repo).unwrap();
        let err = manager.get_valid_access_token("UC1").await.unwrap_err();
        assert!(matches!(err, Error::Auth(AuthError::NoClientSecret)));
        assert_eq!(store.load("UC1").unwrap().as_deref(), Some("FAKE-refresh"));
    }
}
