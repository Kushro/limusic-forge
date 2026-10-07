//! OAuth tests against `wiremock` on 127.0.0.1: the full PKCE + loopback + code exchange (a real
//! local HTTP request stands in for the browser redirect), PKCE S256 correctness, `state`
//! rejection, refresh / rotation / `invalid_grant`, the `AccountManager` scenarios, the
//! "401 → refresh → one retry" policy, client_secret fixtures and redacted `Debug`.
//! Ported from PlaylistForge's `pf-yt/tests/integration_oauth.rs`. No test reaches Google, the
//! OS keyring or a real user directory (only `tempfile` dirs).

use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::{Duration, Instant};

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use sha2::{Digest, Sha256};

use ytdata::auth::accounts::{AccountManager, AccountStatus, AccountsRepo, InMemoryAccountsRepo};
use ytdata::auth::flow::{self, AuthorizedTokens, PendingAuthorization, YOUTUBE_SCOPE};
use ytdata::auth::store::{InMemoryTokenStore, TokenStore};
use ytdata::client::YouTubeClient;
use ytdata::client_secret::{self, ClientSecretFile, CLIENT_SECRET_FILE_NAME};
use ytdata::error::{AuthError, Error};

use wiremock::matchers::{body_string_contains, header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn fixture_path(name: &str) -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests").join("fixtures").join(name)
}

fn fixture(name: &str) -> serde_json::Value {
    let raw = std::fs::read_to_string(fixture_path(name)).unwrap();
    serde_json::from_str(&raw).unwrap()
}

fn client_secret_for(mock_base: &str) -> ClientSecretFile {
    let json = format!(
        r#"{{"installed": {{
            "client_id": "FAKE-test-client-id",
            "client_secret": "FAKE-test-client-secret",
            "auth_uri": "{mock_base}/o/oauth2/auth",
            "token_uri": "{mock_base}/token"
        }}}}"#
    );
    ClientSecretFile::parse(&json).unwrap()
}

fn query_map(url: &str) -> HashMap<String, String> {
    url::Url::parse(url).unwrap().query_pairs().into_owned().collect()
}

fn form_map(body: &[u8]) -> HashMap<String, String> {
    url::form_urlencoded::parse(body).into_owned().collect()
}

// ---------------------------------------------------------------------
// Full installed-app flow: PKCE + real loopback server + wiremock token endpoint.
// ---------------------------------------------------------------------

#[tokio::test]
async fn full_installed_app_flow_exchanges_code_for_tokens() {
    let mock_server = MockServer::start().await;
    let secret = client_secret_for(&mock_server.uri());

    Mock::given(method("POST"))
        .and(path("/token"))
        .and(body_string_contains("grant_type=authorization_code"))
        .and(body_string_contains("code_verifier="))
        .and(body_string_contains("client_secret=FAKE-test-client-secret"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("token_ok.json")))
        .expect(1)
        .mount(&mock_server)
        .await;

    let pending = PendingAuthorization::start(&secret, &[YOUTUBE_SCOPE]).unwrap();
    let pairs = query_map(&pending.authorize_url);
    assert_eq!(pairs.get("code_challenge_method").map(String::as_str), Some("S256"));
    assert_eq!(pairs.get("access_type").map(String::as_str), Some("offline"));
    assert_eq!(pairs.get("prompt").map(String::as_str), Some("consent select_account"));
    let state = pairs.get("state").unwrap().clone();
    let redirect_uri = pairs.get("redirect_uri").unwrap().clone();
    assert!(redirect_uri.starts_with("http://127.0.0.1:"), "{redirect_uri}");

    let cancel = Arc::new(AtomicBool::new(false));
    let waiting = tokio::spawn(pending.wait_for_tokens(cancel, Duration::from_secs(10)));

    tokio::time::sleep(Duration::from_millis(100)).await;
    let resp = reqwest::Client::new()
        .get(format!("{redirect_uri}/?code=FAKE-auth-code&state={state}"))
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success());

    let tokens = waiting.await.unwrap().unwrap();
    assert_eq!(tokens.access_token, "FAKE-access-token-from-fixture");
    assert_eq!(tokens.refresh_token, "FAKE-refresh-token-from-fixture");
    assert!(tokens.expires_at > Instant::now());
}

#[tokio::test]
async fn pkce_verifier_sent_to_the_token_endpoint_matches_the_s256_challenge() {
    let mock_server = MockServer::start().await;
    let secret = client_secret_for(&mock_server.uri());

    Mock::given(method("POST"))
        .and(path("/token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("token_ok.json")))
        .expect(1)
        .mount(&mock_server)
        .await;

    let pending = PendingAuthorization::start(&secret, &[YOUTUBE_SCOPE]).unwrap();
    let pairs = query_map(&pending.authorize_url);
    let challenge = pairs.get("code_challenge").unwrap().clone();
    let state = pairs.get("state").unwrap().clone();
    let redirect_uri = pending.redirect_uri().to_string();

    let cancel = Arc::new(AtomicBool::new(false));
    let waiting = tokio::spawn(pending.wait_for_tokens(cancel, Duration::from_secs(10)));
    tokio::time::sleep(Duration::from_millis(100)).await;
    reqwest::Client::new()
        .get(format!("{redirect_uri}/?code=FAKE-auth-code&state={state}"))
        .send()
        .await
        .unwrap();
    waiting.await.unwrap().unwrap();

    let requests = mock_server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 1);
    let form = form_map(&requests[0].body);
    assert_eq!(form.get("code").map(String::as_str), Some("FAKE-auth-code"));
    assert_eq!(form.get("redirect_uri").map(String::as_str), Some(redirect_uri.as_str()));
    let verifier = form.get("code_verifier").expect("code_verifier must be sent");

    // RFC 7636: 43..=128 chars from the unreserved set.
    assert!((43..=128).contains(&verifier.len()), "verifier length {}", verifier.len());
    assert!(
        verifier.chars().all(|c| c.is_ascii_alphanumeric() || "-._~".contains(c)),
        "verifier has non-unreserved characters"
    );
    // S256: challenge = BASE64URL-NOPAD(SHA256(ASCII(verifier))).
    let expected = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    assert_eq!(challenge, expected);
    assert_ne!(&challenge, verifier, "challenge must not be the plain verifier");
}

#[tokio::test]
async fn every_authorization_gets_a_fresh_state_and_challenge() {
    let secret = client_secret_for("http://127.0.0.1:9");
    let a = PendingAuthorization::start(&secret, &[YOUTUBE_SCOPE]).unwrap();
    let b = PendingAuthorization::start(&secret, &[YOUTUBE_SCOPE]).unwrap();
    let (qa, qb) = (query_map(&a.authorize_url), query_map(&b.authorize_url));
    assert_ne!(qa.get("state"), qb.get("state"));
    assert_ne!(qa.get("code_challenge"), qb.get("code_challenge"));
    assert_ne!(a.redirect_uri(), b.redirect_uri(), "each flow binds its own ephemeral port");
}

#[tokio::test]
async fn installed_app_flow_rejects_state_mismatch() {
    let mock_server = MockServer::start().await;
    let secret = client_secret_for(&mock_server.uri());
    // /token must never be hit: a wrong state aborts before any token request.
    Mock::given(method("POST"))
        .and(path("/token"))
        .respond_with(ResponseTemplate::new(500))
        .expect(0)
        .mount(&mock_server)
        .await;

    let pending = PendingAuthorization::start(&secret, &[YOUTUBE_SCOPE]).unwrap();
    let redirect_uri = pending.redirect_uri().to_string();

    let cancel = Arc::new(AtomicBool::new(false));
    let waiting = tokio::spawn(pending.wait_for_tokens(cancel, Duration::from_secs(10)));

    tokio::time::sleep(Duration::from_millis(100)).await;
    reqwest::Client::new()
        .get(format!("{redirect_uri}/?code=some-code&state=wrong-state"))
        .send()
        .await
        .unwrap();

    let result = waiting.await.unwrap();
    assert!(matches!(result, Err(Error::Auth(AuthError::StateMismatch))), "{result:?}");
}

#[tokio::test]
async fn installed_app_flow_rejects_a_missing_state() {
    let mock_server = MockServer::start().await;
    let secret = client_secret_for(&mock_server.uri());

    let pending = PendingAuthorization::start(&secret, &[YOUTUBE_SCOPE]).unwrap();
    let redirect_uri = pending.redirect_uri().to_string();
    let cancel = Arc::new(AtomicBool::new(false));
    let waiting = tokio::spawn(pending.wait_for_tokens(cancel, Duration::from_secs(10)));

    tokio::time::sleep(Duration::from_millis(100)).await;
    reqwest::Client::new().get(format!("{redirect_uri}/?code=some-code")).send().await.unwrap();

    let result = waiting.await.unwrap();
    assert!(matches!(result, Err(Error::Auth(AuthError::StateMismatch))), "{result:?}");
    assert!(mock_server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn installed_app_flow_surfaces_access_denied_as_provider_error() {
    let mock_server = MockServer::start().await;
    let secret = client_secret_for(&mock_server.uri());

    let pending = PendingAuthorization::start(&secret, &[YOUTUBE_SCOPE]).unwrap();
    let redirect_uri = pending.redirect_uri().to_string();
    let cancel = Arc::new(AtomicBool::new(false));
    let waiting = tokio::spawn(pending.wait_for_tokens(cancel, Duration::from_secs(10)));

    tokio::time::sleep(Duration::from_millis(100)).await;
    reqwest::Client::new()
        .get(format!("{redirect_uri}/?error=access_denied"))
        .send()
        .await
        .unwrap();

    let result = waiting.await.unwrap();
    assert!(
        matches!(&result, Err(Error::Auth(AuthError::Provider(e))) if e == "access_denied"),
        "{result:?}"
    );
}

// ---------------------------------------------------------------------
// Refresh.
// ---------------------------------------------------------------------

#[tokio::test]
async fn refresh_access_token_succeeds() {
    let mock_server = MockServer::start().await;
    let secret = client_secret_for(&mock_server.uri());

    Mock::given(method("POST"))
        .and(path("/token"))
        .and(body_string_contains("grant_type=refresh_token"))
        .and(body_string_contains("refresh_token=some-refresh-token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "access_token": "new-access-token",
            "expires_in": 3600,
            "token_type": "Bearer",
        })))
        .mount(&mock_server)
        .await;

    let refreshed = flow::refresh_access_token(&secret, "some-refresh-token").await.unwrap();
    assert_eq!(refreshed.access_token, "new-access-token");
    assert!(refreshed.refresh_token.is_none(), "Google did not rotate the refresh token here");
}

#[tokio::test]
async fn refresh_access_token_picks_up_a_rotated_refresh_token() {
    let mock_server = MockServer::start().await;
    let secret = client_secret_for(&mock_server.uri());

    Mock::given(method("POST"))
        .and(path("/token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("token_ok.json")))
        .mount(&mock_server)
        .await;

    let refreshed = flow::refresh_access_token(&secret, "old-refresh-token").await.unwrap();
    assert_eq!(refreshed.refresh_token.as_deref(), Some("FAKE-refresh-token-from-fixture"));
}

#[tokio::test]
async fn refresh_access_token_detects_invalid_grant() {
    let mock_server = MockServer::start().await;
    let secret = client_secret_for(&mock_server.uri());

    Mock::given(method("POST"))
        .and(path("/token"))
        .respond_with(ResponseTemplate::new(400).set_body_json(fixture("token_invalid_grant.json")))
        .expect(1)
        .mount(&mock_server)
        .await;

    let err = flow::refresh_access_token(&secret, "revoked-token").await.unwrap_err();
    assert!(matches!(err, Error::Auth(AuthError::InvalidGrant)));
}

#[tokio::test]
async fn revoke_token_posts_the_token_form_field() {
    let mock_server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/revoke"))
        .and(body_string_contains("token=FAKE-refresh"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&mock_server)
        .await;

    flow::revoke_token("FAKE-refresh", &format!("{}/revoke", mock_server.uri())).await.unwrap();
}

// ---------------------------------------------------------------------
// AccountManager (InMemoryTokenStore/InMemoryAccountsRepo + wiremock only).
// ---------------------------------------------------------------------

struct Harness {
    manager: AccountManager,
    store: Arc<InMemoryTokenStore>,
    repo: Arc<InMemoryAccountsRepo>,
}

fn harness(mock_server: &MockServer, secret: ClientSecretFile) -> Harness {
    let store = Arc::new(InMemoryTokenStore::default());
    let repo = Arc::new(InMemoryAccountsRepo::default());
    let manager = AccountManager::new(store.clone(), repo.clone())
        .unwrap()
        .with_yt_client(YouTubeClient::new().unwrap().with_base_url(mock_server.uri()))
        .with_revoke_uri(format!("{}/revoke", mock_server.uri()));
    manager.set_client_secret(secret);
    Harness { manager, store, repo }
}

async fn mount_channel(mock_server: &MockServer, id: &str, title: &str) {
    Mock::given(method("GET"))
        .and(path("/youtube/v3/channels"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "items": [{"id": id, "snippet": {"title": title, "thumbnails": {}}}]
        })))
        .mount(mock_server)
        .await;
}

fn tokens(access: &str, refresh: &str, valid_for: Duration) -> AuthorizedTokens {
    AuthorizedTokens {
        access_token: access.to_string(),
        refresh_token: refresh.to_string(),
        expires_at: Instant::now() + valid_for,
    }
}

#[tokio::test]
async fn account_manager_add_account_then_transparently_refreshes() {
    let mock_server = MockServer::start().await;
    let secret = client_secret_for(&mock_server.uri());
    mount_channel(&mock_server, "UC-account-1", "Account One").await;

    Mock::given(method("POST"))
        .and(path("/token"))
        .and(body_string_contains("grant_type=refresh_token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "access_token": "refreshed-access-token",
            "expires_in": 3600,
            "token_type": "Bearer",
        })))
        .mount(&mock_server)
        .await;

    let h = harness(&mock_server, secret);
    // Expiring now: with the 60s skew the cached token is stale, forcing a refresh.
    let account = h
        .manager
        .add_account(tokens("initial-access-token", "initial-refresh-token", Duration::ZERO))
        .await
        .unwrap();
    assert_eq!(account.channel_id, "UC-account-1");
    assert_eq!(account.title, "Account One");
    assert_eq!(account.status, AccountStatus::Connected);
    assert_eq!(h.repo.load().unwrap().len(), 1, "metadata persisted through the repo");
    assert_eq!(h.store.load("UC-account-1").unwrap().as_deref(), Some("initial-refresh-token"));

    let token = h.manager.get_valid_access_token("UC-account-1").await.unwrap();
    assert_eq!(token, "refreshed-access-token");
}

#[tokio::test]
async fn account_manager_reconnecting_the_same_channel_updates_instead_of_duplicating() {
    let mock_server = MockServer::start().await;
    let secret = client_secret_for(&mock_server.uri());
    mount_channel(&mock_server, "UC-account-1", "Renamed Channel").await;

    let h = harness(&mock_server, secret);
    let hour = Duration::from_secs(3600);
    h.manager.add_account(tokens("access-first", "refresh-first", hour)).await.unwrap();
    h.manager.add_account(tokens("access-second", "refresh-second", hour)).await.unwrap();

    let accounts = h.manager.accounts();
    assert_eq!(accounts.len(), 1, "reconnecting the same channel_id must not duplicate it");
    assert_eq!(accounts[0].title, "Renamed Channel");
    assert_eq!(accounts[0].status, AccountStatus::Connected);
    assert_eq!(h.store.load("UC-account-1").unwrap().as_deref(), Some("refresh-second"));
}

#[tokio::test]
async fn invalid_grant_marks_reauth_required_and_never_deletes_credentials_or_account() {
    let mock_server = MockServer::start().await;
    let secret = client_secret_for(&mock_server.uri());
    mount_channel(&mock_server, "UC-account-1", "Account One").await;

    Mock::given(method("POST"))
        .and(path("/token"))
        .and(body_string_contains("grant_type=refresh_token"))
        .respond_with(ResponseTemplate::new(400).set_body_json(fixture("token_invalid_grant.json")))
        .expect(1) // invalid_grant is never retried
        .mount(&mock_server)
        .await;

    let h = harness(&mock_server, secret);
    h.manager
        .add_account(tokens("initial-access-token", "initial-refresh-token", Duration::ZERO))
        .await
        .unwrap();

    let err = h.manager.get_valid_access_token("UC-account-1").await.unwrap_err();
    assert!(err.is_invalid_grant(), "{err:?}");

    let account = h.manager.account("UC-account-1").unwrap();
    assert_eq!(account.status, AccountStatus::ReauthRequired);
    assert_eq!(h.manager.accounts().len(), 1, "invalid_grant must never delete the account");
    assert_eq!(
        h.repo.load().unwrap()[0].status,
        AccountStatus::ReauthRequired,
        "the status change is persisted"
    );
    assert_eq!(
        h.store.load("UC-account-1").unwrap().as_deref(),
        Some("initial-refresh-token"),
        "invalid_grant must never delete the stored refresh token"
    );
}

#[tokio::test]
async fn reauthorizing_a_reauth_required_account_brings_it_back_to_connected() {
    let mock_server = MockServer::start().await;
    let secret = client_secret_for(&mock_server.uri());
    mount_channel(&mock_server, "UC-account-1", "Account One").await;
    Mock::given(method("POST"))
        .and(path("/token"))
        .respond_with(ResponseTemplate::new(400).set_body_json(fixture("token_invalid_grant.json")))
        .mount(&mock_server)
        .await;

    let h = harness(&mock_server, secret);
    h.manager.add_account(tokens("a1", "r1", Duration::ZERO)).await.unwrap();
    assert!(h.manager.get_valid_access_token("UC-account-1").await.is_err());
    assert_eq!(h.manager.account("UC-account-1").unwrap().status, AccountStatus::ReauthRequired);

    h.manager.add_account(tokens("a2", "r2", Duration::from_secs(3600))).await.unwrap();
    assert_eq!(h.manager.account("UC-account-1").unwrap().status, AccountStatus::Connected);
    assert_eq!(h.manager.get_valid_access_token("UC-account-1").await.unwrap(), "a2");
}

#[tokio::test]
async fn account_manager_remove_account_revokes_best_effort_and_clears_local_state() {
    let mock_server = MockServer::start().await;
    let secret = client_secret_for(&mock_server.uri());
    mount_channel(&mock_server, "UC-account-1", "Account One").await;

    // The revoke endpoint fails: removal must still succeed locally.
    Mock::given(method("POST"))
        .and(path("/revoke"))
        .respond_with(ResponseTemplate::new(400))
        .expect(1)
        .mount(&mock_server)
        .await;

    let h = harness(&mock_server, secret);
    h.manager.add_account(tokens("access", "refresh", Duration::from_secs(3600))).await.unwrap();
    assert_eq!(h.manager.accounts().len(), 1);

    h.manager.revoke_and_remove_account("UC-account-1").await.unwrap();

    assert!(h.manager.accounts().is_empty());
    assert!(h.manager.account("UC-account-1").is_none());
    assert!(h.repo.load().unwrap().is_empty());
    assert_eq!(h.store.load("UC-account-1").unwrap(), None);
    assert!(h.manager.get_valid_access_token("UC-account-1").await.is_err());
}

// ---------------------------------------------------------------------
// 401 → refresh → exactly one retry.
// ---------------------------------------------------------------------

#[tokio::test]
async fn a_401_triggers_one_refresh_and_one_retry_with_the_new_token() {
    let mock_server = MockServer::start().await;
    let secret = client_secret_for(&mock_server.uri());
    mount_channel(&mock_server, "UC-account-1", "Account One").await;

    Mock::given(method("GET"))
        .and(path("/youtube/v3/playlists"))
        .and(header("authorization", "Bearer stale-access-token"))
        .respond_with(ResponseTemplate::new(401).set_body_json(serde_json::json!({
            "error": {"code": 401, "message": "Invalid Credentials", "errors": [{"reason": "authError"}]}
        })))
        .expect(1)
        .mount(&mock_server)
        .await;
    Mock::given(method("GET"))
        .and(path("/youtube/v3/playlists"))
        .and(header("authorization", "Bearer refreshed-access-token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("playlists_mine_p2.json")))
        .expect(1)
        .mount(&mock_server)
        .await;
    Mock::given(method("POST"))
        .and(path("/token"))
        .and(body_string_contains("grant_type=refresh_token"))
        .and(body_string_contains("refresh_token=stored-refresh-token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "access_token": "refreshed-access-token",
            "expires_in": 3600,
            "token_type": "Bearer",
        })))
        .expect(1)
        .mount(&mock_server)
        .await;

    let h = harness(&mock_server, secret);
    // Cached and "valid" for an hour, but the server has revoked it: only a 401 reveals that.
    h.manager
        .add_account(tokens(
            "stale-access-token",
            "stored-refresh-token",
            Duration::from_secs(3600),
        ))
        .await
        .unwrap();

    let client = YouTubeClient::new().unwrap().with_base_url(mock_server.uri());
    let client = &client;
    let page = h
        .manager
        .with_access_token("UC-account-1", |token| async move {
            client.playlists_list_mine(&token, "UC-account-1", None).await
        })
        .await
        .unwrap();
    assert_eq!(page.items.len(), 1);
    assert_eq!(page.items[0].id, "PLfixture0003");
    assert_eq!(h.manager.account("UC-account-1").unwrap().status, AccountStatus::Connected);
}

#[tokio::test]
async fn a_second_401_is_returned_without_further_retries() {
    let mock_server = MockServer::start().await;
    let secret = client_secret_for(&mock_server.uri());
    mount_channel(&mock_server, "UC-account-1", "Account One").await;

    Mock::given(method("GET"))
        .and(path("/youtube/v3/playlists"))
        .respond_with(ResponseTemplate::new(401).set_body_json(serde_json::json!({
            "error": {"code": 401, "message": "Invalid Credentials"}
        })))
        .expect(2) // original call + exactly one retry
        .mount(&mock_server)
        .await;
    Mock::given(method("POST"))
        .and(path("/token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "access_token": "refreshed-access-token",
            "expires_in": 3600,
            "token_type": "Bearer",
        })))
        .expect(1)
        .mount(&mock_server)
        .await;

    let h = harness(&mock_server, secret);
    h.manager
        .add_account(tokens(
            "stale-access-token",
            "stored-refresh-token",
            Duration::from_secs(3600),
        ))
        .await
        .unwrap();

    let client = YouTubeClient::new().unwrap().with_base_url(mock_server.uri());
    let client = &client;
    let err = h
        .manager
        .with_access_token("UC-account-1", |token| async move {
            client.playlists_list_mine(&token, "UC-account-1", None).await
        })
        .await
        .unwrap_err();
    assert!(err.is_unauthorized(), "{err:?}");
}

#[tokio::test]
async fn a_401_whose_refresh_hits_invalid_grant_marks_reauth_required_and_keeps_credentials() {
    let mock_server = MockServer::start().await;
    let secret = client_secret_for(&mock_server.uri());
    mount_channel(&mock_server, "UC-account-1", "Account One").await;

    Mock::given(method("GET"))
        .and(path("/youtube/v3/playlists"))
        .respond_with(ResponseTemplate::new(401))
        .expect(1) // no retry: the refresh failed
        .mount(&mock_server)
        .await;
    Mock::given(method("POST"))
        .and(path("/token"))
        .respond_with(ResponseTemplate::new(400).set_body_json(fixture("token_invalid_grant.json")))
        .expect(1)
        .mount(&mock_server)
        .await;

    let h = harness(&mock_server, secret);
    h.manager
        .add_account(tokens(
            "stale-access-token",
            "stored-refresh-token",
            Duration::from_secs(3600),
        ))
        .await
        .unwrap();

    let client = YouTubeClient::new().unwrap().with_base_url(mock_server.uri());
    let client = &client;
    let err = h
        .manager
        .with_access_token("UC-account-1", |token| async move {
            client.playlists_list_mine(&token, "UC-account-1", None).await
        })
        .await
        .unwrap_err();
    assert!(err.is_invalid_grant(), "{err:?}");
    assert_eq!(h.manager.account("UC-account-1").unwrap().status, AccountStatus::ReauthRequired);
    assert_eq!(h.store.load("UC-account-1").unwrap().as_deref(), Some("stored-refresh-token"));
}

#[tokio::test]
async fn non_401_errors_are_not_retried_by_with_access_token() {
    let mock_server = MockServer::start().await;
    let secret = client_secret_for(&mock_server.uri());
    mount_channel(&mock_server, "UC-account-1", "Account One").await;

    Mock::given(method("GET"))
        .and(path("/youtube/v3/playlists"))
        .respond_with(ResponseTemplate::new(403).set_body_json(fixture("err_quota_exceeded.json")))
        .expect(1)
        .mount(&mock_server)
        .await;
    Mock::given(method("POST"))
        .and(path("/token"))
        .respond_with(ResponseTemplate::new(500))
        .expect(0)
        .mount(&mock_server)
        .await;

    let h = harness(&mock_server, secret);
    h.manager
        .add_account(tokens(
            "valid-access-token",
            "stored-refresh-token",
            Duration::from_secs(3600),
        ))
        .await
        .unwrap();

    let client = YouTubeClient::new().unwrap().with_base_url(mock_server.uri());
    let client = &client;
    let err = h
        .manager
        .with_access_token("UC-account-1", |token| async move {
            client.playlists_list_mine(&token, "UC-account-1", None).await
        })
        .await
        .unwrap_err();
    assert_eq!(err.api_error_kind(), Some(ytdata::error::ApiErrorKind::QuotaExceeded));
}

// ---------------------------------------------------------------------
// client_secret fixtures (tempfile dirs only).
// ---------------------------------------------------------------------

#[test]
fn desktop_client_secret_fixture_parses() {
    let parsed = ClientSecretFile::load(&fixture_path("client_secret_desktop.json")).unwrap();
    assert!(parsed.installed.client_id.ends_with(".apps.googleusercontent.com"));
    assert_eq!(parsed.installed.token_uri, "https://oauth2.googleapis.com/token");
}

#[test]
fn web_client_secret_fixture_is_rejected_with_a_desktop_app_hint() {
    let err = ClientSecretFile::load(&fixture_path("client_secret_web.json")).unwrap_err();
    match err {
        Error::Auth(AuthError::InvalidClientSecret(msg)) => assert!(msg.contains("Desktop app")),
        other => panic!("expected InvalidClientSecret, got {other:?}"),
    }
}

#[test]
fn import_copies_the_desktop_fixture_and_refuses_the_web_one() {
    let dir = tempfile::tempdir().unwrap();
    let app_data = dir.path().join("app_data");

    assert!(client_secret::import(&fixture_path("client_secret_web.json"), &app_data).is_err());
    assert!(!app_data.join(CLIENT_SECRET_FILE_NAME).exists());
    assert!(client_secret::load_existing(&app_data).is_none());

    client_secret::import(&fixture_path("client_secret_desktop.json"), &app_data).unwrap();
    assert_eq!(
        std::fs::read(app_data.join(CLIENT_SECRET_FILE_NAME)).unwrap(),
        std::fs::read(fixture_path("client_secret_desktop.json")).unwrap(),
        "imported verbatim"
    );
    assert!(client_secret::load_existing(&app_data).is_some());
}

// ---------------------------------------------------------------------
// Debug never leaks secrets.
// ---------------------------------------------------------------------

#[test]
fn debug_output_never_contains_tokens_or_client_secret() {
    let secret = ClientSecretFile::load(&fixture_path("client_secret_desktop.json")).unwrap();
    let debug = format!("{secret:?}");
    assert!(!debug.contains("FAKE-NOT-A-REAL-SECRET"), "{debug}");

    let t = tokens("ya29.FAKE-access-leak", "1//FAKE-refresh-leak", Duration::from_secs(60));
    let debug = format!("{t:?}");
    assert!(!debug.contains("FAKE-access-leak"), "{debug}");
    assert!(!debug.contains("FAKE-refresh-leak"), "{debug}");
    assert!(debug.contains("***"), "{debug}");

    let store = InMemoryTokenStore::default();
    store.save("UC-x", "1//FAKE-stored-leak").unwrap();
    assert!(!format!("{store:?}").contains("FAKE-stored-leak"));
}

#[test]
fn keyring_service_names_are_fixed() {
    assert_eq!(ytdata::KEYRING_SERVICE, "LiMusicForge");
    assert_eq!(ytdata::PF_KEYRING_SERVICE, "PlaylistForge");
}
