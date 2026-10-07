//! OAuth "installed application" flow: authorization-code grant + PKCE (S256) + loopback
//! redirect against Google's endpoints.
//!
//! Built on `oauth2` 5 (first-class PKCE, async reqwest transport, typed `invalid_grant`) plus
//! our own [`super::loopback`] server. `yup-oauth2`'s `InstalledFlow` was ruled out in
//! PlaylistForge because it sends no PKCE challenge at all.
//!
//! Authorization requests always carry `access_type=offline` (so a refresh token comes back)
//! and `prompt=consent select_account` (so "connect another account" really asks which one, and
//! a re-consent reliably issues a fresh refresh token). The `state` returned to the loopback is
//! compared with the one sent; a mismatch aborts before any token request.

use std::fmt;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::{Duration, Instant};

use oauth2::basic::{BasicClient, BasicErrorResponse, BasicErrorResponseType};
use oauth2::{
    AuthType, AuthUrl, AuthorizationCode, ClientId, ClientSecret, CsrfToken, EndpointNotSet,
    EndpointSet, HttpClientError, PkceCodeChallenge, PkceCodeVerifier, RedirectUrl, RefreshToken,
    RequestTokenError, Scope, TokenResponse, TokenUrl,
};

use super::loopback::{CallbackOutcome, LoopbackServer};
use crate::client::build_http_client;
use crate::client_secret::ClientSecretFile;
use crate::error::{AuthError, Error};

/// The single scope this app requests.
pub const YOUTUBE_SCOPE: &str = "https://www.googleapis.com/auth/youtube.force-ssl";

/// RFC 7009 token revocation endpoint.
pub const GOOGLE_REVOKE_URI: &str = "https://oauth2.googleapis.com/revoke";

/// Tokens from a completed authorization-code exchange. `Debug` prints `***` for both tokens.
#[derive(Clone)]
pub struct AuthorizedTokens {
    pub access_token: String,
    pub refresh_token: String,
    pub expires_at: Instant,
}

impl fmt::Debug for AuthorizedTokens {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AuthorizedTokens")
            .field("access_token", &"***")
            .field("refresh_token", &"***")
            .field("expires_at", &self.expires_at)
            .finish()
    }
}

/// Result of a refresh. `refresh_token` is `Some` only when Google rotates it; callers must
/// persist it over the old one then. `Debug` prints `***` for tokens.
#[derive(Clone)]
pub struct RefreshedTokens {
    pub access_token: String,
    pub expires_at: Instant,
    pub refresh_token: Option<String>,
}

impl fmt::Debug for RefreshedTokens {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RefreshedTokens")
            .field("access_token", &"***")
            .field("expires_at", &self.expires_at)
            .field("refresh_token", &self.refresh_token.as_ref().map(|_| "***"))
            .finish()
    }
}

/// `BasicClient` with auth + token URIs set (oauth2's typestate requires both before
/// `authorize_url` / `exchange_code` / `exchange_refresh_token` can be called).
type ConfiguredClient =
    BasicClient<EndpointSet, EndpointNotSet, EndpointNotSet, EndpointNotSet, EndpointSet>;

fn build_client(
    secret: &ClientSecretFile,
    redirect_uri: Option<&str>,
) -> Result<ConfiguredClient, Error> {
    let auth_uri = AuthUrl::new(secret.installed.auth_uri.clone())
        .map_err(|err| AuthError::InvalidClientSecret(format!("auth_uri: {err}")))?;
    let token_uri = TokenUrl::new(secret.installed.token_uri.clone())
        .map_err(|err| AuthError::InvalidClientSecret(format!("token_uri: {err}")))?;

    let mut client = BasicClient::new(ClientId::new(secret.installed.client_id.clone()))
        .set_client_secret(ClientSecret::new(secret.installed.client_secret.clone()))
        // Google documents client_secret in the POST body for installed apps.
        .set_auth_type(AuthType::RequestBody)
        .set_auth_uri(auth_uri)
        .set_token_uri(token_uri);

    if let Some(uri) = redirect_uri {
        let redirect_uri = RedirectUrl::new(uri.to_string())
            .map_err(|err| AuthError::Loopback(format!("redirect_uri: {err}")))?;
        client = client.set_redirect_uri(redirect_uri);
    }

    Ok(client)
}

/// A started authorization: the loopback server is listening and [`Self::authorize_url`] is
/// ready to open in the system browser. Two-phase because the caller needs the URL before it
/// can await completion (to show "waiting..." with a cancel button).
pub struct PendingAuthorization {
    pub authorize_url: String,
    client_secret: ClientSecretFile,
    redirect_uri: String,
    pkce_verifier: PkceCodeVerifier,
    csrf_state: CsrfToken,
    server: LoopbackServer,
}

impl PendingAuthorization {
    /// Binds the loopback server and builds the authorization URL. Synchronous, no network.
    pub fn start(client_secret: &ClientSecretFile, scopes: &[&str]) -> Result<Self, Error> {
        let server = LoopbackServer::start()?;
        let redirect_uri = server.redirect_uri();

        let client = build_client(client_secret, Some(&redirect_uri))?;
        let (pkce_challenge, pkce_verifier) = PkceCodeChallenge::new_random_sha256();

        let mut request = client.authorize_url(CsrfToken::new_random);
        for scope in scopes {
            request = request.add_scope(Scope::new((*scope).to_string()));
        }
        let (authorize_url, csrf_state) = request
            .add_extra_param("access_type", "offline")
            .add_extra_param("prompt", "consent select_account")
            .set_pkce_challenge(pkce_challenge)
            .url();

        Ok(Self {
            authorize_url: authorize_url.to_string(),
            client_secret: client_secret.clone(),
            redirect_uri,
            pkce_verifier,
            csrf_state,
            server,
        })
    }

    /// The loopback `redirect_uri` (`http://127.0.0.1:<port>`) this authorization waits on.
    pub fn redirect_uri(&self) -> &str {
        &self.redirect_uri
    }

    /// Waits for the browser redirect (or `cancel` / `timeout`), verifies `state`, and
    /// exchanges the code (with the PKCE verifier) for tokens.
    pub async fn wait_for_tokens(
        self,
        cancel: Arc<AtomicBool>,
        timeout: Duration,
    ) -> Result<AuthorizedTokens, Error> {
        let outcome = self.server.wait_for_callback(cancel, timeout).await?;
        let code = match outcome {
            CallbackOutcome::Code { code, state } => {
                if state.as_deref() != Some(self.csrf_state.secret().as_str()) {
                    return Err(AuthError::StateMismatch.into());
                }
                code
            }
            CallbackOutcome::ProviderError(err) => return Err(AuthError::Provider(err).into()),
        };

        let client = build_client(&self.client_secret, Some(&self.redirect_uri))?;
        let http_client = build_http_client()?;
        let started = Instant::now();
        let response = client
            .exchange_code(AuthorizationCode::new(code))
            .set_pkce_verifier(self.pkce_verifier)
            .request_async(&http_client)
            .await
            .map_err(classify_token_error)?;

        let refresh_token = response
            .refresh_token()
            .ok_or_else(|| {
                AuthError::Provider(
                    "Google did not return a refresh_token for this authorization — unexpected \
                     with access_type=offline and prompt=consent; try connecting the account again"
                        .to_string(),
                )
            })?
            .secret()
            .clone();

        Ok(AuthorizedTokens {
            access_token: response.access_token().secret().clone(),
            refresh_token,
            expires_at: started + response.expires_in().unwrap_or(Duration::from_secs(3600)),
        })
    }
}

/// Exchanges a stored refresh token for a new access token.
///
/// Returns [`AuthError::InvalidGrant`] when Google rejects the refresh token (revoked,
/// expired). Callers turn that into a per-account "re-authorization required" state and never
/// delete credentials or data because of it.
pub async fn refresh_access_token(
    client_secret: &ClientSecretFile,
    refresh_token: &str,
) -> Result<RefreshedTokens, Error> {
    let client = build_client(client_secret, None)?;
    let http_client = build_http_client()?;
    let started = Instant::now();
    let response = client
        .exchange_refresh_token(&RefreshToken::new(refresh_token.to_string()))
        .request_async(&http_client)
        .await
        .map_err(classify_token_error)?;

    Ok(RefreshedTokens {
        access_token: response.access_token().secret().clone(),
        expires_at: started + response.expires_in().unwrap_or(Duration::from_secs(3600)),
        refresh_token: response.refresh_token().map(|t| t.secret().clone()),
    })
}

/// Best-effort revocation (RFC 7009: `POST <revoke_uri>` with `token=...`). Callers must not
/// let a failure here block removing an account locally.
pub async fn revoke_token(token: &str, revoke_uri: &str) -> Result<(), Error> {
    let http_client = build_http_client()?;
    let response = http_client.post(revoke_uri).form(&[("token", token)]).send().await?;
    if response.status().is_success() {
        Ok(())
    } else {
        Err(AuthError::Provider(format!("revoke endpoint returned HTTP {}", response.status()))
            .into())
    }
}

type TokenErr = RequestTokenError<HttpClientError<reqwest::Error>, BasicErrorResponse>;

fn classify_token_error(err: TokenErr) -> Error {
    let auth_err = match err {
        RequestTokenError::ServerResponse(resp) => match resp.error() {
            BasicErrorResponseType::InvalidGrant => AuthError::InvalidGrant,
            other => AuthError::Provider(format!(
                "{other}{}",
                resp.error_description().map(|d| format!(": {d}")).unwrap_or_default()
            )),
        },
        RequestTokenError::Request(err) => AuthError::Oauth(format!("request failed: {err}")),
        RequestTokenError::Parse(err, _) => {
            AuthError::Oauth(format!("failed to parse token response: {err}"))
        }
        RequestTokenError::Other(msg) => AuthError::Oauth(msg),
    };
    auth_err.into()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_secret() -> ClientSecretFile {
        ClientSecretFile::parse(
            r#"{"installed": {
                "client_id": "FAKE-test-client-id",
                "client_secret": "FAKE-test-client-secret",
                "auth_uri": "https://accounts.google.com/o/oauth2/auth",
                "token_uri": "https://oauth2.googleapis.com/token"
            }}"#,
        )
        .unwrap()
    }

    #[test]
    fn pending_authorization_builds_a_well_formed_google_auth_url() {
        let secret = test_secret();
        let pending = PendingAuthorization::start(&secret, &[YOUTUBE_SCOPE]).unwrap();

        let url = url::Url::parse(&pending.authorize_url).unwrap();
        assert_eq!(url.host_str(), Some("accounts.google.com"));
        let pairs: std::collections::HashMap<_, _> = url.query_pairs().into_owned().collect();
        assert_eq!(pairs.get("client_id").map(String::as_str), Some("FAKE-test-client-id"));
        assert_eq!(pairs.get("scope").map(String::as_str), Some(YOUTUBE_SCOPE));
        assert_eq!(pairs.get("response_type").map(String::as_str), Some("code"));
        assert_eq!(pairs.get("access_type").map(String::as_str), Some("offline"));
        assert_eq!(pairs.get("prompt").map(String::as_str), Some("consent select_account"));
        assert!(pairs.contains_key("code_challenge"), "PKCE code_challenge must be present");
        assert_eq!(
            pairs.get("code_challenge_method").map(String::as_str),
            Some("S256"),
            "must use PKCE S256, not plain"
        );
        assert!(pairs.get("state").is_some_and(|s| !s.is_empty()), "state must be present");
        assert_eq!(
            pairs.get("redirect_uri").map(String::as_str),
            Some(pending.redirect_uri()),
            "redirect_uri must point at the loopback server"
        );
        assert!(pending.redirect_uri().starts_with("http://127.0.0.1:"));
        assert!(
            !pending.authorize_url.contains("FAKE-test-client-secret"),
            "the client secret must never be in the browser URL"
        );
    }

    #[test]
    fn build_client_rejects_malformed_uris_as_invalid_client_secret() {
        let mut secret = test_secret();
        secret.installed.auth_uri = "not a url".to_string();
        let err = build_client(&secret, None).unwrap_err();
        assert!(matches!(err, Error::Auth(AuthError::InvalidClientSecret(_))));
    }

    #[test]
    fn token_debug_output_is_redacted() {
        let tokens = AuthorizedTokens {
            access_token: "ya29.FAKE-access".to_string(),
            refresh_token: "1//FAKE-refresh".to_string(),
            expires_at: Instant::now(),
        };
        let debug = format!("{tokens:?}");
        assert!(!debug.contains("FAKE-access") && !debug.contains("FAKE-refresh"), "{debug}");
        assert!(debug.contains("***"), "{debug}");

        let refreshed = RefreshedTokens {
            access_token: "ya29.FAKE-access-2".to_string(),
            expires_at: Instant::now(),
            refresh_token: Some("1//FAKE-rotated".to_string()),
        };
        let debug = format!("{refreshed:?}");
        assert!(!debug.contains("FAKE-access-2") && !debug.contains("FAKE-rotated"), "{debug}");
    }
}
