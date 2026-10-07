//! Data API endpoint tests against `wiremock` on 127.0.0.1 — no test here ever reaches Google.
//! Ported from PlaylistForge's `pf-yt/tests/integration_endpoints.rs`, plus fixture-driven
//! pagination, the `videos.list` availability cases and the `ApiDisabled` classification.

use std::path::Path;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use ytdata::client::YouTubeClient;
use ytdata::error::{ApiErrorKind, Error};
use ytdata::quota::QuotaSink;

use wiremock::matchers::{header, method, path, query_param};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

const TOKEN: &str = "FAKE-test-access-token";
const ACCOUNT_ID: &str = "UC1";

fn fixture(name: &str) -> serde_json::Value {
    let file = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests").join("fixtures").join(name);
    let raw = std::fs::read_to_string(&file).unwrap_or_else(|e| panic!("{}: {e}", file.display()));
    serde_json::from_str(&raw).unwrap_or_else(|e| panic!("{}: {e}", file.display()))
}

/// Records every `(endpoint, units, account_id)` it is given.
#[derive(Default)]
struct RecordingSink {
    calls: Mutex<Vec<(String, u32, Option<String>)>>,
}

impl QuotaSink for RecordingSink {
    fn record(&self, endpoint: &'static str, units: u32, account_id: Option<&str>) {
        self.calls.lock().unwrap().push((
            endpoint.to_string(),
            units,
            account_id.map(str::to_string),
        ));
    }
}

impl RecordingSink {
    fn calls(&self) -> Vec<(String, u32, Option<String>)> {
        self.calls.lock().unwrap().clone()
    }
}

fn client_for(mock_base: &str, sink: Arc<RecordingSink>) -> YouTubeClient {
    YouTubeClient::new().unwrap().with_base_url(mock_base).with_quota_sink(sink)
}

fn one(endpoint: &str, units: u32) -> Vec<(String, u32, Option<String>)> {
    vec![(endpoint.to_string(), units, Some(ACCOUNT_ID.to_string()))]
}

// ---------------------------------------------------------------------
// channels.list?mine=true
// ---------------------------------------------------------------------

#[tokio::test]
async fn channels_list_mine_parses_the_fixture_and_records_one_unit() {
    let server = MockServer::start().await;
    let sink = Arc::new(RecordingSink::default());
    let client = client_for(&server.uri(), sink.clone());

    Mock::given(method("GET"))
        .and(path("/youtube/v3/channels"))
        .and(query_param("mine", "true"))
        .and(header("authorization", format!("Bearer {TOKEN}").as_str()))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("channels_mine.json")))
        .expect(1)
        .mount(&server)
        .await;

    let info = client.channels_list_mine(TOKEN).await.unwrap();
    assert_eq!(info.channel_id, "UCfixtureChannel000000001");
    assert_eq!(info.title, "Fixture Channel");
    assert_eq!(info.thumbnail_url.as_deref(), Some("https://yt3.ggpht.com/fixture-medium.jpg"));
    assert_eq!(sink.calls(), vec![("channels.list".to_string(), 1, None)]);
}

#[tokio::test]
async fn channels_list_mine_maps_quota_exceeded_error() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/youtube/v3/channels"))
        .respond_with(ResponseTemplate::new(403).set_body_json(fixture("err_quota_exceeded.json")))
        .expect(1)
        .mount(&server)
        .await;

    let client = YouTubeClient::new().unwrap().with_base_url(server.uri());
    let err = client.channels_list_mine(TOKEN).await.unwrap_err();
    match &err {
        Error::Api { status, reason, .. } => {
            assert_eq!(*status, 403);
            assert_eq!(reason.as_deref(), Some("quotaExceeded"));
        }
        other => panic!("expected Error::Api, got {other:?}"),
    }
    assert_eq!(err.api_error_kind(), Some(ApiErrorKind::QuotaExceeded));
}

// ---------------------------------------------------------------------
// playlists.list?mine=true — two pages from fixtures
// ---------------------------------------------------------------------

#[tokio::test]
async fn playlists_list_mine_paginates_across_two_fixture_pages() {
    let server = MockServer::start().await;
    let sink = Arc::new(RecordingSink::default());
    let client = client_for(&server.uri(), sink.clone());

    let p1 = fixture("playlists_mine_p1.json");
    let p2 = fixture("playlists_mine_p2.json");
    Mock::given(method("GET"))
        .and(path("/youtube/v3/playlists"))
        .and(query_param("mine", "true"))
        .respond_with(move |req: &Request| {
            let page2 =
                req.url.query_pairs().any(|(k, v)| k == "pageToken" && v == "FIXTURE_PAGE_2");
            ResponseTemplate::new(200).set_body_json(if page2 { p2.clone() } else { p1.clone() })
        })
        .expect(2)
        .mount(&server)
        .await;

    let page1 = client.playlists_list_mine(TOKEN, ACCOUNT_ID, None).await.unwrap();
    assert_eq!(page1.items.len(), 2);
    assert_eq!(page1.items[0].id, "PLfixture0001");
    assert_eq!(page1.items[0].title, "Road Trip");
    assert_eq!(page1.items[0].item_count, 42);
    assert_eq!(page1.items[0].privacy_status, "public");
    assert_eq!(
        page1.items[0].thumbnail_url.as_deref(),
        Some("https://i.ytimg.com/vi/fixtureA/mqdefault.jpg")
    );
    assert_eq!(page1.items[1].privacy_status, "unlisted");
    assert_eq!(page1.next_page_token.as_deref(), Some("FIXTURE_PAGE_2"));

    let page2 = client
        .playlists_list_mine(TOKEN, ACCOUNT_ID, page1.next_page_token.as_deref())
        .await
        .unwrap();
    assert_eq!(page2.items.len(), 1);
    assert_eq!(page2.items[0].id, "PLfixture0003");
    assert_eq!(page2.items[0].privacy_status, "private", "missing status defaults to private");
    assert_eq!(page2.next_page_token, None);

    let calls = sink.calls();
    assert_eq!(calls.len(), 2, "one unit per page");
    assert!(calls
        .iter()
        .all(|(e, u, a)| e == "playlists.list" && *u == 1 && a.as_deref() == Some(ACCOUNT_ID)));
}

// ---------------------------------------------------------------------
// playlistItems.list — two pages from fixtures
// ---------------------------------------------------------------------

#[tokio::test]
async fn playlist_items_list_paginates_across_two_fixture_pages() {
    let server = MockServer::start().await;
    let sink = Arc::new(RecordingSink::default());
    let client = client_for(&server.uri(), sink.clone());

    let p1 = fixture("playlist_items_p1.json");
    let p2 = fixture("playlist_items_p2.json");
    Mock::given(method("GET"))
        .and(path("/youtube/v3/playlistItems"))
        .and(query_param("playlistId", "PLfixture0001"))
        .respond_with(move |req: &Request| {
            let page2 =
                req.url.query_pairs().any(|(k, v)| k == "pageToken" && v == "FIXTURE_ITEMS_PAGE_2");
            ResponseTemplate::new(200).set_body_json(if page2 { p2.clone() } else { p1.clone() })
        })
        .expect(2)
        .mount(&server)
        .await;

    let page1 = client.playlist_items_list(TOKEN, ACCOUNT_ID, "PLfixture0001", None).await.unwrap();
    assert_eq!(page1.items.len(), 2);
    let first = &page1.items[0];
    assert_eq!(first.video_id, "vidAvail001");
    assert_eq!(first.position, 0);
    assert_eq!(first.channel_title.as_deref(), Some("Fixture Artist - Topic"));
    assert_eq!(first.added_at.unwrap().to_rfc3339(), "2024-03-01T10:00:00+00:00");
    assert!(first.video_published_at.is_some());
    assert_eq!(page1.items[1].title, "Deleted video");
    assert_eq!(page1.items[1].channel_title, None);
    assert_eq!(page1.next_page_token.as_deref(), Some("FIXTURE_ITEMS_PAGE_2"));

    let page2 = client
        .playlist_items_list(TOKEN, ACCOUNT_ID, "PLfixture0001", page1.next_page_token.as_deref())
        .await
        .unwrap();
    assert_eq!(page2.items.len(), 2);
    assert_eq!(page2.items[0].title, "Private video");
    assert_eq!(page2.items[1].video_id, "vidRegion04");
    assert_eq!(page2.items[1].position, 3);
    assert_eq!(page2.next_page_token, None);

    assert_eq!(sink.calls().len(), 2);
    assert!(sink.calls().iter().all(|(e, u, _)| e == "playlistItems.list" && *u == 1));
}

#[tokio::test]
async fn playlist_items_list_paginates_across_two_pages() {
    let server = MockServer::start().await;
    let sink = Arc::new(RecordingSink::default());
    let client = client_for(&server.uri(), sink.clone());

    Mock::given(method("GET"))
        .and(path("/youtube/v3/playlistItems"))
        .and(query_param("playlistId", "PL1"))
        .respond_with(|req: &Request| {
            let has_page_token = req.url.query_pairs().any(|(k, v)| k == "pageToken" && v == "abc");
            if has_page_token {
                ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "items": [{
                        "id": "item2",
                        "snippet": {"title": "Video 2", "position": 1, "resourceId": {"videoId": "v2"}},
                    }],
                }))
            } else {
                ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "items": [{
                        "id": "item1",
                        "snippet": {"title": "Video 1", "position": 0, "resourceId": {"videoId": "v1"}},
                    }],
                    "nextPageToken": "abc",
                }))
            }
        })
        .expect(2)
        .mount(&server)
        .await;

    let page1 = client.playlist_items_list(TOKEN, ACCOUNT_ID, "PL1", None).await.unwrap();
    assert_eq!(page1.items[0].video_id, "v1");
    assert_eq!(page1.next_page_token.as_deref(), Some("abc"));
    let page2 = client
        .playlist_items_list(TOKEN, ACCOUNT_ID, "PL1", page1.next_page_token.as_deref())
        .await
        .unwrap();
    assert_eq!(page2.items[0].video_id, "v2");
    assert_eq!(page2.next_page_token, None);
    assert_eq!(sink.calls().len(), 2, "one quota unit recorded per page");
}

// ---------------------------------------------------------------------
// videos.list — available / deleted / private / regionRestriction, batching
// ---------------------------------------------------------------------

#[tokio::test]
async fn videos_list_fixture_covers_available_deleted_private_and_region_restricted() {
    let server = MockServer::start().await;
    let sink = Arc::new(RecordingSink::default());
    let client = client_for(&server.uri(), sink.clone());

    Mock::given(method("GET"))
        .and(path("/youtube/v3/videos"))
        .and(query_param("id", "vidAvail001,vidGone0002,vidPriv0003,vidRegion04"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("videos_list.json")))
        .expect(1)
        .mount(&server)
        .await;

    let ids: Vec<String> =
        ["vidAvail001", "vidGone0002", "vidPriv0003", "vidRegion04"].map(String::from).to_vec();
    let videos = client.videos_list(TOKEN, ACCOUNT_ID, &ids).await.unwrap();
    assert_eq!(videos.len(), 3);

    let available = videos.iter().find(|v| v.id == "vidAvail001").unwrap();
    assert_eq!(available.title.as_deref(), Some("Fixture Song One"));
    assert_eq!(available.duration_s, Some(213));
    assert_eq!(available.privacy_status.as_deref(), Some("public"));
    assert!(!available.region_restricted);

    assert!(!videos.iter().any(|v| v.id == "vidGone0002"), "deleted = absent from the response");

    let private = videos.iter().find(|v| v.id == "vidPriv0003").unwrap();
    assert_eq!(private.privacy_status.as_deref(), Some("private"));
    assert_eq!(private.title, None, "private videos come without a snippet");

    let restricted = videos.iter().find(|v| v.id == "vidRegion04").unwrap();
    assert!(restricted.region_restricted);
    assert_eq!(restricted.blocked_regions, vec!["AR".to_string(), "DE".to_string()]);
    assert_eq!(restricted.duration_s, Some(3723));

    assert_eq!(sink.calls(), one("videos.list", 1));
}

#[tokio::test]
async fn videos_list_chunks_more_than_fifty_ids_into_multiple_calls() {
    let server = MockServer::start().await;
    let sink = Arc::new(RecordingSink::default());
    let client = client_for(&server.uri(), sink.clone());

    Mock::given(method("GET"))
        .and(path("/youtube/v3/videos"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({ "items": [] })))
        .expect(3)
        .mount(&server)
        .await;

    let ids: Vec<String> = (0..101).map(|i| format!("v{i}")).collect();
    let videos = client.videos_list(TOKEN, ACCOUNT_ID, &ids).await.unwrap();
    assert!(videos.is_empty());
    assert_eq!(sink.calls().len(), 3, "101 ids = batches of 50 + 50 + 1, one unit each");

    let requests = server.received_requests().await.unwrap();
    let sizes: Vec<usize> = requests
        .iter()
        .map(|r| {
            let ids = r.url.query_pairs().find(|(k, _)| k == "id").unwrap().1.into_owned();
            ids.split(',').count()
        })
        .collect();
    assert_eq!(sizes, vec![50, 50, 1]);
}

#[tokio::test]
async fn videos_list_with_empty_ids_makes_no_request_and_records_no_quota() {
    let server = MockServer::start().await;
    let sink = Arc::new(RecordingSink::default());
    let client = client_for(&server.uri(), sink.clone());

    let videos = client.videos_list(TOKEN, ACCOUNT_ID, &[]).await.unwrap();
    assert!(videos.is_empty());
    assert!(sink.calls().is_empty());
    assert!(server.received_requests().await.unwrap().is_empty());
}

// ---------------------------------------------------------------------
// Typed 403s — no blind retry
// ---------------------------------------------------------------------

#[tokio::test]
async fn playlists_list_mine_maps_quota_exceeded_without_retrying() {
    let server = MockServer::start().await;
    let sink = Arc::new(RecordingSink::default());
    let client = client_for(&server.uri(), sink.clone());

    Mock::given(method("GET"))
        .and(path("/youtube/v3/playlists"))
        .respond_with(ResponseTemplate::new(403).set_body_json(fixture("err_quota_exceeded.json")))
        .expect(1)
        .mount(&server)
        .await;

    let err = client.playlists_list_mine(TOKEN, ACCOUNT_ID, None).await.unwrap_err();
    assert!(
        matches!(&err, Error::Api { status: 403, reason: Some(r), .. } if r == "quotaExceeded")
    );
    assert_eq!(err.api_error_kind(), Some(ApiErrorKind::QuotaExceeded));
    assert!(sink.calls().is_empty(), "a failed call must not record quota spend");
}

#[tokio::test]
async fn access_not_configured_is_classified_as_api_disabled_without_retrying() {
    let server = MockServer::start().await;
    let sink = Arc::new(RecordingSink::default());
    let client = client_for(&server.uri(), sink.clone());

    Mock::given(method("GET"))
        .and(path("/youtube/v3/playlists"))
        .respond_with(
            ResponseTemplate::new(403).set_body_json(fixture("err_access_not_configured.json")),
        )
        .expect(1)
        .mount(&server)
        .await;

    let err = client.playlists_list_mine(TOKEN, ACCOUNT_ID, None).await.unwrap_err();
    assert!(
        matches!(&err, Error::Api { status: 403, reason: Some(r), .. } if r == "accessNotConfigured")
    );
    assert_eq!(err.api_error_kind(), Some(ApiErrorKind::ApiDisabled));
    assert!(sink.calls().is_empty());
}

#[tokio::test]
async fn service_disabled_only_in_details_is_also_api_disabled() {
    let server = MockServer::start().await;
    let client = YouTubeClient::new().unwrap().with_base_url(server.uri());

    Mock::given(method("GET"))
        .and(path("/youtube/v3/channels"))
        .respond_with(ResponseTemplate::new(403).set_body_json(serde_json::json!({
            "error": {
                "code": 403,
                "message": "API disabled",
                "status": "PERMISSION_DENIED",
                "details": [{
                    "@type": "type.googleapis.com/google.rpc.ErrorInfo",
                    "reason": "SERVICE_DISABLED"
                }]
            }
        })))
        .expect(1)
        .mount(&server)
        .await;

    let err = client.channels_list_mine(TOKEN).await.unwrap_err();
    assert_eq!(err.api_error_kind(), Some(ApiErrorKind::ApiDisabled));
}

#[tokio::test]
async fn a_401_is_returned_as_is_by_the_bare_client() {
    let server = MockServer::start().await;
    let client = YouTubeClient::new().unwrap().with_base_url(server.uri());

    Mock::given(method("GET"))
        .and(path("/youtube/v3/playlists"))
        .respond_with(ResponseTemplate::new(401).set_body_json(serde_json::json!({
            "error": {"code": 401, "message": "Invalid Credentials", "errors": [{"reason": "authError"}]}
        })))
        .expect(1)
        .mount(&server)
        .await;

    let err = client.playlists_list_mine(TOKEN, ACCOUNT_ID, None).await.unwrap_err();
    assert!(err.is_unauthorized());
}

// ---------------------------------------------------------------------
// Retry on 500
// ---------------------------------------------------------------------

struct FlakyThenOk {
    calls: AtomicU32,
    fail_times: u32,
}

impl Respond for FlakyThenOk {
    fn respond(&self, _request: &Request) -> ResponseTemplate {
        let n = self.calls.fetch_add(1, Ordering::SeqCst);
        if n < self.fail_times {
            ResponseTemplate::new(500).set_body_string("internal error")
        } else {
            ResponseTemplate::new(200).set_body_json(serde_json::json!({ "items": [] }))
        }
    }
}

#[tokio::test]
async fn playlist_items_list_retries_on_500_then_succeeds() {
    let server = MockServer::start().await;
    let sink = Arc::new(RecordingSink::default());
    let client = client_for(&server.uri(), sink.clone());

    Mock::given(method("GET"))
        .and(path("/youtube/v3/playlistItems"))
        .respond_with(FlakyThenOk { calls: AtomicU32::new(0), fail_times: 2 })
        .expect(3)
        .mount(&server)
        .await;

    let page = client.playlist_items_list(TOKEN, ACCOUNT_ID, "PL1", None).await.unwrap();
    assert!(page.items.is_empty());
    assert_eq!(sink.calls().len(), 1, "quota is recorded once, for the successful call");
}

#[tokio::test]
async fn videos_list_gives_up_after_exhausting_retries_on_persistent_500() {
    let server = MockServer::start().await;
    let sink = Arc::new(RecordingSink::default());
    let client = client_for(&server.uri(), sink.clone());

    Mock::given(method("GET"))
        .and(path("/youtube/v3/videos"))
        .respond_with(ResponseTemplate::new(500).set_body_string("still broken"))
        .expect(4)
        .mount(&server)
        .await;

    let err = client.videos_list(TOKEN, ACCOUNT_ID, &["v1".to_string()]).await.unwrap_err();
    assert!(matches!(err, Error::Api { status: 500, .. }));
    assert!(sink.calls().is_empty());
}

// ---------------------------------------------------------------------
// Write endpoints (50 units each)
// ---------------------------------------------------------------------

#[tokio::test]
async fn playlists_insert_returns_the_new_id_and_records_50_units() {
    let server = MockServer::start().await;
    let sink = Arc::new(RecordingSink::default());
    let client = client_for(&server.uri(), sink.clone());

    Mock::given(method("POST"))
        .and(path("/youtube/v3/playlists"))
        .and(query_param("part", "snippet,status"))
        .respond_with(|req: &Request| {
            let body: serde_json::Value = serde_json::from_slice(&req.body).unwrap();
            assert_eq!(body["snippet"]["title"], serde_json::json!("My Mix"));
            assert_eq!(body["status"]["privacyStatus"], serde_json::json!("private"));
            ResponseTemplate::new(200).set_body_json(serde_json::json!({"id": "PLnew"}))
        })
        .expect(1)
        .mount(&server)
        .await;

    let id = client.playlists_insert(TOKEN, ACCOUNT_ID, "My Mix", "desc", "private").await.unwrap();
    assert_eq!(id, "PLnew");
    assert_eq!(sink.calls(), one("playlists.insert", 50));
}

#[tokio::test]
async fn playlists_update_sends_the_full_snippet_and_records_quota() {
    let server = MockServer::start().await;
    let sink = Arc::new(RecordingSink::default());
    let client = client_for(&server.uri(), sink.clone());

    Mock::given(method("PUT"))
        .and(path("/youtube/v3/playlists"))
        .respond_with(|req: &Request| {
            let body: serde_json::Value = serde_json::from_slice(&req.body).unwrap();
            assert_eq!(body["id"], serde_json::json!("PL1"));
            assert_eq!(body["snippet"]["title"], serde_json::json!("New Title"));
            assert_eq!(body["snippet"]["description"], serde_json::json!("New Description"));
            assert_eq!(body["status"]["privacyStatus"], serde_json::json!("unlisted"));
            ResponseTemplate::new(200).set_body_json(serde_json::json!({"id": "PL1"}))
        })
        .expect(1)
        .mount(&server)
        .await;

    client
        .playlists_update(TOKEN, ACCOUNT_ID, "PL1", "New Title", "New Description", "unlisted")
        .await
        .unwrap();
    assert_eq!(sink.calls(), one("playlists.update", 50));
}

#[tokio::test]
async fn playlists_delete_succeeds_on_a_bare_204() {
    let server = MockServer::start().await;
    let sink = Arc::new(RecordingSink::default());
    let client = client_for(&server.uri(), sink.clone());

    Mock::given(method("DELETE"))
        .and(path("/youtube/v3/playlists"))
        .and(query_param("id", "PL1"))
        .respond_with(ResponseTemplate::new(204))
        .expect(1)
        .mount(&server)
        .await;

    client.playlists_delete(TOKEN, ACCOUNT_ID, "PL1").await.unwrap();
    assert_eq!(sink.calls(), one("playlists.delete", 50));
}

#[tokio::test]
async fn playlist_items_insert_returns_the_created_item_id_and_position() {
    let server = MockServer::start().await;
    let sink = Arc::new(RecordingSink::default());
    let client = client_for(&server.uri(), sink.clone());

    Mock::given(method("POST"))
        .and(path("/youtube/v3/playlistItems"))
        .respond_with(|req: &Request| {
            let body: serde_json::Value = serde_json::from_slice(&req.body).unwrap();
            assert_eq!(body["snippet"]["playlistId"], serde_json::json!("PLdest"));
            assert_eq!(body["snippet"]["resourceId"]["videoId"], serde_json::json!("v1"));
            assert_eq!(body["snippet"]["resourceId"]["kind"], serde_json::json!("youtube#video"));
            assert!(body["snippet"].get("position").is_none(), "omitted, not null, when appending");
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({"id": "newitem1", "snippet": {"position": 3}}))
        })
        .expect(1)
        .mount(&server)
        .await;

    let result =
        client.playlist_items_insert(TOKEN, ACCOUNT_ID, "PLdest", "v1", None).await.unwrap();
    assert_eq!(result.playlist_item_id, "newitem1");
    assert_eq!(result.position, Some(3));
    assert_eq!(sink.calls(), one("playlistItems.insert", 50));
}

#[tokio::test]
async fn playlist_items_insert_with_explicit_position_sends_it() {
    let server = MockServer::start().await;
    let sink = Arc::new(RecordingSink::default());
    let client = client_for(&server.uri(), sink.clone());

    Mock::given(method("POST"))
        .and(path("/youtube/v3/playlistItems"))
        .respond_with(|req: &Request| {
            let body: serde_json::Value = serde_json::from_slice(&req.body).unwrap();
            assert_eq!(body["snippet"]["position"], serde_json::json!(0));
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({"id": "newitem1", "snippet": {"position": 0}}))
        })
        .expect(1)
        .mount(&server)
        .await;

    client.playlist_items_insert(TOKEN, ACCOUNT_ID, "PLdest", "v1", Some(0)).await.unwrap();
}

#[tokio::test]
async fn playlist_items_update_position_sends_the_required_fields_and_records_quota() {
    let server = MockServer::start().await;
    let sink = Arc::new(RecordingSink::default());
    let client = client_for(&server.uri(), sink.clone());

    Mock::given(method("PUT"))
        .and(path("/youtube/v3/playlistItems"))
        .respond_with(|req: &Request| {
            let body: serde_json::Value = serde_json::from_slice(&req.body).unwrap();
            assert_eq!(body["id"], serde_json::json!("item1"));
            assert_eq!(body["snippet"]["playlistId"], serde_json::json!("PL1"));
            assert_eq!(body["snippet"]["resourceId"]["videoId"], serde_json::json!("v1"));
            assert_eq!(body["snippet"]["position"], serde_json::json!(2));
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({"id": "item1", "snippet": {"position": 2}}))
        })
        .expect(1)
        .mount(&server)
        .await;

    let result = client
        .playlist_items_update_position(TOKEN, ACCOUNT_ID, "item1", "PL1", "v1", 2)
        .await
        .unwrap();
    assert_eq!(result.position, Some(2));
    assert_eq!(sink.calls(), one("playlistItems.update", 50));
}

#[tokio::test]
async fn playlist_items_update_position_maps_manual_sort_required_without_retrying() {
    let server = MockServer::start().await;
    let sink = Arc::new(RecordingSink::default());
    let client = client_for(&server.uri(), sink.clone());

    Mock::given(method("PUT"))
        .and(path("/youtube/v3/playlistItems"))
        .respond_with(ResponseTemplate::new(400).set_body_json(serde_json::json!({
            "error": {
                "code": 400,
                "message": "The playlist cannot be sorted manually because it has an automatic sort order.",
                "errors": [{"domain": "youtube.playlistItem", "reason": "manualSortRequired"}],
            }
        })))
        .expect(1)
        .mount(&server)
        .await;

    let err = client
        .playlist_items_update_position(TOKEN, ACCOUNT_ID, "item1", "PL1", "v1", 2)
        .await
        .unwrap_err();
    assert_eq!(err.api_error_kind(), Some(ApiErrorKind::ManualSortRequired));
    assert!(sink.calls().is_empty(), "a failed write must not record quota spend");
}

#[tokio::test]
async fn playlist_items_insert_maps_a_dead_video_404_as_item_gone() {
    let server = MockServer::start().await;
    let sink = Arc::new(RecordingSink::default());
    let client = client_for(&server.uri(), sink.clone());

    Mock::given(method("POST"))
        .and(path("/youtube/v3/playlistItems"))
        .respond_with(ResponseTemplate::new(404).set_body_json(serde_json::json!({
            "error": {"code": 404, "message": "Video not found.", "errors": [{"reason": "videoNotFound"}]}
        })))
        .expect(1)
        .mount(&server)
        .await;

    let err = client
        .playlist_items_insert(TOKEN, ACCOUNT_ID, "PLdest", "v-dead", None)
        .await
        .unwrap_err();
    assert_eq!(err.api_error_kind(), Some(ApiErrorKind::ItemGone));
    assert!(sink.calls().is_empty());
}

#[tokio::test]
async fn playlist_items_delete_succeeds_and_records_quota() {
    let server = MockServer::start().await;
    let sink = Arc::new(RecordingSink::default());
    let client = client_for(&server.uri(), sink.clone());

    Mock::given(method("DELETE"))
        .and(path("/youtube/v3/playlistItems"))
        .and(query_param("id", "item1"))
        .respond_with(ResponseTemplate::new(204))
        .expect(1)
        .mount(&server)
        .await;

    client.playlist_items_delete(TOKEN, ACCOUNT_ID, "item1").await.unwrap();
    assert_eq!(sink.calls(), one("playlistItems.delete", 50));
}

#[tokio::test]
async fn playlists_insert_maps_quota_exceeded_without_retrying() {
    let server = MockServer::start().await;
    let sink = Arc::new(RecordingSink::default());
    let client = client_for(&server.uri(), sink.clone());

    Mock::given(method("POST"))
        .and(path("/youtube/v3/playlists"))
        .respond_with(ResponseTemplate::new(403).set_body_json(fixture("err_quota_exceeded.json")))
        .expect(1)
        .mount(&server)
        .await;

    let err = client.playlists_insert(TOKEN, ACCOUNT_ID, "T", "D", "private").await.unwrap_err();
    assert_eq!(err.api_error_kind(), Some(ApiErrorKind::QuotaExceeded));
    assert!(sink.calls().is_empty());
}

#[tokio::test]
async fn playlist_items_insert_retries_on_500_then_succeeds() {
    let server = MockServer::start().await;
    let sink = Arc::new(RecordingSink::default());
    let client = client_for(&server.uri(), sink.clone());

    Mock::given(method("POST"))
        .and(path("/youtube/v3/playlistItems"))
        .respond_with(FlakyThenOk { calls: AtomicU32::new(0), fail_times: 2 })
        .expect(3)
        .mount(&server)
        .await;

    // The eventual 200 body has no `id`; it defaults to "" — this proves the retry path only.
    let result =
        client.playlist_items_insert(TOKEN, ACCOUNT_ID, "PLdest", "v1", None).await.unwrap();
    assert_eq!(result.playlist_item_id, "");
    assert_eq!(sink.calls().len(), 1, "quota is recorded once, for the successful call");
}

#[tokio::test]
async fn playlist_items_delete_gives_up_after_exhausting_retries_on_persistent_500() {
    let server = MockServer::start().await;
    let sink = Arc::new(RecordingSink::default());
    let client = client_for(&server.uri(), sink.clone());

    Mock::given(method("DELETE"))
        .and(path("/youtube/v3/playlistItems"))
        .respond_with(ResponseTemplate::new(500).set_body_string("still broken"))
        .expect(4)
        .mount(&server)
        .await;

    let err = client.playlist_items_delete(TOKEN, ACCOUNT_ID, "item1").await.unwrap_err();
    assert!(matches!(err, Error::Api { status: 500, .. }));
    assert!(sink.calls().is_empty());
}

// ---------------------------------------------------------------------
// Quota cost table (ported from pf-core/src/quota.rs)
// ---------------------------------------------------------------------

#[test]
fn unit_cost_matches_the_reference_table() {
    use ytdata::quota::{endpoint, unit_cost};
    assert_eq!(unit_cost(endpoint::PLAYLISTS_LIST), 1);
    assert_eq!(unit_cost(endpoint::PLAYLIST_ITEMS_LIST), 1);
    assert_eq!(unit_cost(endpoint::VIDEOS_LIST), 1);
    assert_eq!(unit_cost(endpoint::CHANNELS_LIST), 1);
    assert_eq!(unit_cost(endpoint::PLAYLISTS_INSERT), 50);
    assert_eq!(unit_cost(endpoint::PLAYLIST_ITEMS_DELETE), 50);
    assert_eq!(unit_cost("some.unknown.endpoint"), 0);
}
