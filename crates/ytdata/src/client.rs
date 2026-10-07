//! Thin YouTube Data API v3 HTTP client: `reqwest` (rustls) + `serde`, Google's structured error
//! envelope mapped to [`Error::Api`].
//!
//! Every method takes the access token explicitly; resolving (and refreshing) it is the job of
//! [`crate::auth::accounts::AccountManager`], whose `with_access_token` wraps a call here with
//! the "401 → refresh → one retry" policy. This client itself only retries 5xx and network
//! failures, never a 401/403 and never blindly after a write may have landed.

use std::sync::Arc;
use std::time::Duration;

use serde::Deserialize;

use crate::error::Error;
use crate::quota::{cost, endpoint, QuotaSink};
use crate::types::{
    Page, PlaylistInsertBody, PlaylistItemInsertBody, PlaylistItemSnippetInsert,
    PlaylistItemSnippetUpdate, PlaylistItemSummary, PlaylistItemUpdateBody,
    PlaylistItemWriteResponse, PlaylistItemWriteResult, PlaylistItemsListResponse,
    PlaylistSnippetWrite, PlaylistStatusWrite, PlaylistSummary, PlaylistUpdateBody,
    PlaylistWriteResponse, PlaylistsListResponse, ResourceIdWrite, VideoSummary,
    VideosListResponse,
};

const DEFAULT_BASE_URL: &str = "https://www.googleapis.com";

/// Attempts (first try included) for transient failures: 5xx and network errors.
const MAX_RETRY_ATTEMPTS: u32 = 4;

/// Most ids `videos.list` accepts in one call (and bills as a single unit).
pub const VIDEOS_LIST_BATCH: usize = 50;

/// The `reqwest::Client` shared by the Data API client and the OAuth token/revoke calls.
/// Redirects are disabled (oauth2's SSRF guidance; sound for a REST client too).
pub(crate) fn build_http_client() -> Result<reqwest::Client, Error> {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(30))
        .build()
        .map_err(Error::from)
}

/// Thin wrapper over a `reqwest::Client` pointed at the YouTube Data API.
pub struct YouTubeClient {
    http: reqwest::Client,
    base_url: String,
    /// `None` unless [`Self::with_quota_sink`] was called; then every successful billable call
    /// is reported.
    quota_sink: Option<Arc<dyn QuotaSink>>,
}

impl YouTubeClient {
    pub fn new() -> Result<Self, Error> {
        Ok(Self {
            http: build_http_client()?,
            base_url: DEFAULT_BASE_URL.to_string(),
            quota_sink: None,
        })
    }

    /// Overrides the base URL (tests point it at a local `wiremock` server).
    #[must_use]
    pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = base_url.into();
        self
    }

    /// Wires a quota-accounting sink (the app's ledger).
    #[must_use]
    pub fn with_quota_sink(mut self, sink: Arc<dyn QuotaSink>) -> Self {
        self.quota_sink = Some(sink);
        self
    }

    fn record_quota(&self, endpoint_name: &'static str, units: u32, account_id: Option<&str>) {
        if let Some(sink) = &self.quota_sink {
            sink.record(endpoint_name, units, account_id);
        }
    }

    /// Authenticated `GET <base_url><path>?<query>`, parsed as JSON `T` on success, mapped to
    /// [`Error::Api`] on any non-2xx status. No retry, no quota record.
    pub async fn get_json<T: serde::de::DeserializeOwned>(
        &self,
        path: &str,
        access_token: &str,
        query: &[(&str, &str)],
    ) -> Result<T, Error> {
        let url = format!("{}{}", self.base_url, path);
        let response = self.http.get(url).bearer_auth(access_token).query(query).send().await?;
        Self::parse_response(response).await
    }

    async fn parse_response<T: serde::de::DeserializeOwned>(
        response: reqwest::Response,
    ) -> Result<T, Error> {
        let status = response.status();
        let bytes = response.bytes().await?;
        if status.is_success() {
            serde_json::from_slice(&bytes).map_err(Error::from)
        } else {
            Err(api_error_from_body(status.as_u16(), &bytes))
        }
    }

    /// `GET channels?part=snippet&mine=true` — the authenticated user's own channel. Used once
    /// per account right after authorization, before an account id exists, so the unit is
    /// recorded without one.
    pub async fn channels_list_mine(&self, access_token: &str) -> Result<ChannelInfo, Error> {
        let response: ChannelsListResponse = self
            .get_json_with_retry(
                "/youtube/v3/channels",
                access_token,
                &[("part", "snippet"), ("mine", "true")],
            )
            .await?;
        self.record_quota(endpoint::CHANNELS_LIST, cost::CHANNELS_LIST, None);
        response.items.into_iter().next().map(ChannelInfo::from).ok_or_else(|| Error::Api {
            status: 200,
            reason: None,
            message: "channels.list?mine=true returned no channels for this account".to_string(),
        })
    }

    /// [`Self::get_json`] with exponential backoff + jitter for 5xx and network errors only.
    async fn get_json_with_retry<T: serde::de::DeserializeOwned>(
        &self,
        path: &str,
        access_token: &str,
        query: &[(&str, &str)],
    ) -> Result<T, Error> {
        let mut attempt: u32 = 0;
        loop {
            attempt += 1;
            match self.get_json(path, access_token, query).await {
                Ok(value) => return Ok(value),
                Err(err) if attempt < MAX_RETRY_ATTEMPTS && is_retryable(&err) => {
                    let delay = backoff_delay(attempt);
                    tracing::debug!(attempt, ?delay, error = %err, path, "retrying transient YouTube API error");
                    tokio::time::sleep(delay).await;
                }
                Err(err) => return Err(err),
            }
        }
    }

    /// `GET playlists?part=snippet,contentDetails,status&mine=true` — one page (≤50) of the
    /// user's own playlists. 1 unit per page. `account_id` is only for quota attribution.
    pub async fn playlists_list_mine(
        &self,
        access_token: &str,
        account_id: &str,
        page_token: Option<&str>,
    ) -> Result<Page<PlaylistSummary>, Error> {
        let mut query = vec![
            ("part", "snippet,contentDetails,status"),
            ("mine", "true"),
            ("maxResults", "50"),
            (
                "fields",
                "items(id,snippet(title,description,thumbnails/medium),contentDetails/itemCount,status/privacyStatus),nextPageToken",
            ),
        ];
        if let Some(token) = page_token {
            query.push(("pageToken", token));
        }
        let response: PlaylistsListResponse =
            self.get_json_with_retry("/youtube/v3/playlists", access_token, &query).await?;
        self.record_quota(endpoint::PLAYLISTS_LIST, cost::PLAYLISTS_LIST, Some(account_id));
        Ok(Page {
            items: response.items.into_iter().map(PlaylistSummary::from).collect(),
            next_page_token: response.next_page_token,
        })
    }

    /// `GET playlistItems?part=snippet,contentDetails&playlistId=...` — one page (≤50) of a
    /// playlist's items, in playlist order. 1 unit per page.
    pub async fn playlist_items_list(
        &self,
        access_token: &str,
        account_id: &str,
        playlist_id: &str,
        page_token: Option<&str>,
    ) -> Result<Page<PlaylistItemSummary>, Error> {
        let mut query = vec![
            ("part", "snippet,contentDetails"),
            ("playlistId", playlist_id),
            ("maxResults", "50"),
            (
                "fields",
                "items(id,snippet(title,position,resourceId,videoOwnerChannelTitle,videoOwnerChannelId,publishedAt,thumbnails/medium),contentDetails/videoPublishedAt),nextPageToken",
            ),
        ];
        if let Some(token) = page_token {
            query.push(("pageToken", token));
        }
        let response: PlaylistItemsListResponse =
            self.get_json_with_retry("/youtube/v3/playlistItems", access_token, &query).await?;
        self.record_quota(
            endpoint::PLAYLIST_ITEMS_LIST,
            cost::PLAYLIST_ITEMS_LIST,
            Some(account_id),
        );
        Ok(Page {
            items: response.items.into_iter().map(PlaylistItemSummary::from).collect(),
            next_page_token: response.next_page_token,
        })
    }

    /// `GET videos?part=snippet,contentDetails,status&id=...` for every id in `ids`, chunked
    /// into batches of [`VIDEOS_LIST_BATCH`] (1 unit each). Ids absent from the result are the
    /// caller's signal that the video is gone; no placeholder is synthesized for them. An empty
    /// `ids` makes no request.
    pub async fn videos_list(
        &self,
        access_token: &str,
        account_id: &str,
        ids: &[String],
    ) -> Result<Vec<VideoSummary>, Error> {
        let mut all = Vec::with_capacity(ids.len());
        for chunk in ids.chunks(VIDEOS_LIST_BATCH) {
            let id_param = chunk.join(",");
            let query = [
                ("part", "snippet,contentDetails,status"),
                ("id", id_param.as_str()),
                ("maxResults", "50"),
                (
                    "fields",
                    "items(id,snippet(title,channelId,channelTitle,publishedAt,thumbnails/medium),contentDetails(duration,regionRestriction),status(privacyStatus,uploadStatus))",
                ),
            ];
            let response: VideosListResponse =
                self.get_json_with_retry("/youtube/v3/videos", access_token, &query).await?;
            self.record_quota(endpoint::VIDEOS_LIST, cost::VIDEOS_LIST, Some(account_id));
            all.extend(response.items.into_iter().map(VideoSummary::from));
        }
        Ok(all)
    }

    // -------------------------------------------------------------------
    // Write endpoints. Each retries only 5xx/network, records quota only once the call
    // succeeded, and returns the identity Google assigned. Idempotent re-checks of a write that
    // may have landed belong to the job runner, not here.
    // -------------------------------------------------------------------

    async fn post_json<T: serde::de::DeserializeOwned, B: serde::Serialize>(
        &self,
        path: &str,
        access_token: &str,
        query: &[(&str, &str)],
        body: &B,
    ) -> Result<T, Error> {
        let url = format!("{}{}", self.base_url, path);
        let response =
            self.http.post(url).bearer_auth(access_token).query(query).json(body).send().await?;
        Self::parse_response(response).await
    }

    async fn put_json<T: serde::de::DeserializeOwned, B: serde::Serialize>(
        &self,
        path: &str,
        access_token: &str,
        query: &[(&str, &str)],
        body: &B,
    ) -> Result<T, Error> {
        let url = format!("{}{}", self.base_url, path);
        let response =
            self.http.put(url).bearer_auth(access_token).query(query).json(body).send().await?;
        Self::parse_response(response).await
    }

    /// `DELETE` has no body to parse: Google answers a bare 204.
    async fn delete_call(
        &self,
        path: &str,
        access_token: &str,
        query: &[(&str, &str)],
    ) -> Result<(), Error> {
        let url = format!("{}{}", self.base_url, path);
        let response = self.http.delete(url).bearer_auth(access_token).query(query).send().await?;
        let status = response.status();
        if status.is_success() {
            Ok(())
        } else {
            let bytes = response.bytes().await?;
            Err(api_error_from_body(status.as_u16(), &bytes))
        }
    }

    async fn post_json_with_retry<T: serde::de::DeserializeOwned, B: serde::Serialize>(
        &self,
        path: &str,
        access_token: &str,
        query: &[(&str, &str)],
        body: &B,
    ) -> Result<T, Error> {
        let mut attempt: u32 = 0;
        loop {
            attempt += 1;
            match self.post_json(path, access_token, query, body).await {
                Ok(value) => return Ok(value),
                Err(err) if attempt < MAX_RETRY_ATTEMPTS && is_retryable(&err) => {
                    tokio::time::sleep(backoff_delay(attempt)).await;
                }
                Err(err) => return Err(err),
            }
        }
    }

    async fn put_json_with_retry<T: serde::de::DeserializeOwned, B: serde::Serialize>(
        &self,
        path: &str,
        access_token: &str,
        query: &[(&str, &str)],
        body: &B,
    ) -> Result<T, Error> {
        let mut attempt: u32 = 0;
        loop {
            attempt += 1;
            match self.put_json(path, access_token, query, body).await {
                Ok(value) => return Ok(value),
                Err(err) if attempt < MAX_RETRY_ATTEMPTS && is_retryable(&err) => {
                    tokio::time::sleep(backoff_delay(attempt)).await;
                }
                Err(err) => return Err(err),
            }
        }
    }

    async fn delete_with_retry(
        &self,
        path: &str,
        access_token: &str,
        query: &[(&str, &str)],
    ) -> Result<(), Error> {
        let mut attempt: u32 = 0;
        loop {
            attempt += 1;
            match self.delete_call(path, access_token, query).await {
                Ok(()) => return Ok(()),
                Err(err) if attempt < MAX_RETRY_ATTEMPTS && is_retryable(&err) => {
                    tokio::time::sleep(backoff_delay(attempt)).await;
                }
                Err(err) => return Err(err),
            }
        }
    }

    /// `POST playlists?part=snippet,status` (50 u) — creates a playlist. `privacy` is a raw
    /// `privacyStatus` (`public` / `unlisted` / `private`). Returns the new playlist id.
    pub async fn playlists_insert(
        &self,
        access_token: &str,
        account_id: &str,
        title: &str,
        description: &str,
        privacy: &str,
    ) -> Result<String, Error> {
        let body = PlaylistInsertBody {
            snippet: PlaylistSnippetWrite { title, description },
            status: PlaylistStatusWrite { privacy_status: privacy },
        };
        let response: PlaylistWriteResponse = self
            .post_json_with_retry(
                "/youtube/v3/playlists",
                access_token,
                &[("part", "snippet,status")],
                &body,
            )
            .await?;
        self.record_quota(endpoint::PLAYLISTS_INSERT, cost::PLAYLISTS_INSERT, Some(account_id));
        Ok(response.id)
    }

    /// `PUT playlists?part=snippet,status` (50 u). Always resends the full snippet, so callers
    /// pass the *final* title/description/privacy, not a partial patch.
    pub async fn playlists_update(
        &self,
        access_token: &str,
        account_id: &str,
        playlist_id: &str,
        title: &str,
        description: &str,
        privacy: &str,
    ) -> Result<(), Error> {
        let body = PlaylistUpdateBody {
            id: playlist_id,
            snippet: PlaylistSnippetWrite { title, description },
            status: PlaylistStatusWrite { privacy_status: privacy },
        };
        let _response: PlaylistWriteResponse = self
            .put_json_with_retry(
                "/youtube/v3/playlists",
                access_token,
                &[("part", "snippet,status")],
                &body,
            )
            .await?;
        self.record_quota(endpoint::PLAYLISTS_UPDATE, cost::PLAYLISTS_UPDATE, Some(account_id));
        Ok(())
    }

    /// `DELETE playlists?id=...` (50 u).
    pub async fn playlists_delete(
        &self,
        access_token: &str,
        account_id: &str,
        playlist_id: &str,
    ) -> Result<(), Error> {
        self.delete_with_retry("/youtube/v3/playlists", access_token, &[("id", playlist_id)])
            .await?;
        self.record_quota(endpoint::PLAYLISTS_DELETE, cost::PLAYLISTS_DELETE, Some(account_id));
        Ok(())
    }

    /// `POST playlistItems?part=snippet` (50 u) — adds `video_id` to `playlist_id`, appended
    /// when `position` is `None`. A dead/private video classifies as
    /// [`crate::error::ApiErrorKind::ItemGone`].
    pub async fn playlist_items_insert(
        &self,
        access_token: &str,
        account_id: &str,
        playlist_id: &str,
        video_id: &str,
        position: Option<u32>,
    ) -> Result<PlaylistItemWriteResult, Error> {
        let body = PlaylistItemInsertBody {
            snippet: PlaylistItemSnippetInsert {
                playlist_id,
                resource_id: ResourceIdWrite { kind: "youtube#video", video_id },
                position,
            },
        };
        let response: PlaylistItemWriteResponse = self
            .post_json_with_retry(
                "/youtube/v3/playlistItems",
                access_token,
                &[("part", "snippet")],
                &body,
            )
            .await?;
        self.record_quota(
            endpoint::PLAYLIST_ITEMS_INSERT,
            cost::PLAYLIST_ITEMS_INSERT,
            Some(account_id),
        );
        Ok(PlaylistItemWriteResult {
            playlist_item_id: response.id,
            position: response.snippet.and_then(|s| s.position),
        })
    }

    /// `PUT playlistItems?part=snippet` (50 u) — moves one item to `position` (0-based). The
    /// playlist must be in manual sort order, otherwise
    /// [`crate::error::ApiErrorKind::ManualSortRequired`].
    pub async fn playlist_items_update_position(
        &self,
        access_token: &str,
        account_id: &str,
        playlist_item_id: &str,
        playlist_id: &str,
        video_id: &str,
        position: u32,
    ) -> Result<PlaylistItemWriteResult, Error> {
        let body = PlaylistItemUpdateBody {
            id: playlist_item_id,
            snippet: PlaylistItemSnippetUpdate {
                playlist_id,
                resource_id: ResourceIdWrite { kind: "youtube#video", video_id },
                position,
            },
        };
        let response: PlaylistItemWriteResponse = self
            .put_json_with_retry(
                "/youtube/v3/playlistItems",
                access_token,
                &[("part", "snippet")],
                &body,
            )
            .await?;
        self.record_quota(
            endpoint::PLAYLIST_ITEMS_UPDATE,
            cost::PLAYLIST_ITEMS_UPDATE,
            Some(account_id),
        );
        Ok(PlaylistItemWriteResult {
            playlist_item_id: response.id,
            position: response.snippet.and_then(|s| s.position),
        })
    }

    /// `DELETE playlistItems?id=...` (50 u) — removes one occurrence by its playlist-item id.
    pub async fn playlist_items_delete(
        &self,
        access_token: &str,
        account_id: &str,
        playlist_item_id: &str,
    ) -> Result<(), Error> {
        self.delete_with_retry(
            "/youtube/v3/playlistItems",
            access_token,
            &[("id", playlist_item_id)],
        )
        .await?;
        self.record_quota(
            endpoint::PLAYLIST_ITEMS_DELETE,
            cost::PLAYLIST_ITEMS_DELETE,
            Some(account_id),
        );
        Ok(())
    }
}

/// Only 5xx responses and network-level failures are retried.
fn is_retryable(err: &Error) -> bool {
    match err {
        Error::Api { status, .. } => (500..600).contains(status),
        Error::Network(_) => true,
        _ => false,
    }
}

/// Exponential backoff (250ms, 500ms, 1s, 2s, ... capped) plus up to 150ms of jitter.
fn backoff_delay(attempt: u32) -> Duration {
    let exponent = (attempt.saturating_sub(1)).min(4);
    let base_ms = 250u64.saturating_mul(1u64 << exponent);
    let jitter_ms = rand::random::<u64>() % 150;
    Duration::from_millis(base_ms + jitter_ms)
}

#[derive(Debug, Clone, Deserialize)]
struct ChannelsListResponse {
    #[serde(default)]
    items: Vec<ChannelListItem>,
}

#[derive(Debug, Clone, Deserialize)]
struct ChannelListItem {
    id: String,
    snippet: ChannelSnippet,
}

#[derive(Debug, Clone, Deserialize)]
struct ChannelSnippet {
    title: String,
    #[serde(default)]
    thumbnails: Thumbnails,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct Thumbnails {
    default: Option<ThumbnailInfo>,
    medium: Option<ThumbnailInfo>,
}

#[derive(Debug, Clone, Deserialize)]
struct ThumbnailInfo {
    url: String,
}

/// Resolved identity of a YouTube channel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChannelInfo {
    pub channel_id: String,
    pub title: String,
    pub thumbnail_url: Option<String>,
}

impl From<ChannelListItem> for ChannelInfo {
    fn from(item: ChannelListItem) -> Self {
        let thumbnail_url =
            item.snippet.thumbnails.medium.or(item.snippet.thumbnails.default).map(|t| t.url);
        Self { channel_id: item.id, title: item.snippet.title, thumbnail_url }
    }
}

/// Parses Google's error envelope
/// (`{"error":{"code":403,"message":"...","errors":[{"reason":"quotaExceeded"}],
/// "details":[{"reason":"SERVICE_DISABLED"}]}}`) into [`Error::Api`]. The reason comes from
/// `errors[]` first and `details[]` (google.rpc.ErrorInfo) second. Falls back to the raw body
/// text when it is not that shape (an HTML page from a proxy, ...).
pub(crate) fn api_error_from_body(status: u16, body: &[u8]) -> Error {
    #[derive(Deserialize)]
    struct GoogleErrorEnvelope {
        error: GoogleError,
    }
    #[derive(Deserialize)]
    struct GoogleError {
        #[serde(default)]
        message: String,
        #[serde(default)]
        errors: Vec<GoogleErrorDetail>,
        #[serde(default)]
        details: Vec<GoogleErrorDetail>,
    }
    #[derive(Deserialize)]
    struct GoogleErrorDetail {
        #[serde(default)]
        reason: Option<String>,
    }

    match serde_json::from_slice::<GoogleErrorEnvelope>(body) {
        Ok(envelope) => {
            let GoogleError { message, errors, details } = envelope.error;
            let reason = errors
                .into_iter()
                .find_map(|d| d.reason)
                .or_else(|| details.into_iter().find_map(|d| d.reason));
            Error::Api { status, reason, message }
        }
        Err(_) => Error::Api {
            status,
            reason: None,
            message: String::from_utf8_lossy(body).trim().to_string(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn channel_info_prefers_medium_thumbnail_over_default() {
        let item = ChannelListItem {
            id: "UC123".to_string(),
            snippet: ChannelSnippet {
                title: "My Channel".to_string(),
                thumbnails: Thumbnails {
                    default: Some(ThumbnailInfo {
                        url: "http://example.com/default.jpg".to_string(),
                    }),
                    medium: Some(ThumbnailInfo {
                        url: "http://example.com/medium.jpg".to_string(),
                    }),
                },
            },
        };
        let info: ChannelInfo = item.into();
        assert_eq!(info.channel_id, "UC123");
        assert_eq!(info.title, "My Channel");
        assert_eq!(info.thumbnail_url.as_deref(), Some("http://example.com/medium.jpg"));
    }

    #[test]
    fn channel_info_falls_back_to_default_thumbnail() {
        let item = ChannelListItem {
            id: "UC123".to_string(),
            snippet: ChannelSnippet {
                title: "My Channel".to_string(),
                thumbnails: Thumbnails {
                    default: Some(ThumbnailInfo {
                        url: "http://example.com/default.jpg".to_string(),
                    }),
                    medium: None,
                },
            },
        };
        let info: ChannelInfo = item.into();
        assert_eq!(info.thumbnail_url.as_deref(), Some("http://example.com/default.jpg"));
    }

    #[test]
    fn api_error_parses_google_structured_envelope() {
        let body = br#"{"error":{"code":403,"message":"The request cannot be completed because you have exceeded your quota.","errors":[{"domain":"youtube.quota","reason":"quotaExceeded","message":"quota exceeded"}]}}"#;
        match api_error_from_body(403, body) {
            Error::Api { status, reason, message } => {
                assert_eq!(status, 403);
                assert_eq!(reason.as_deref(), Some("quotaExceeded"));
                assert!(message.contains("exceeded your quota"));
            }
            other => panic!("expected Error::Api, got {other:?}"),
        }
    }

    #[test]
    fn api_error_takes_the_reason_from_details_when_errors_has_none() {
        let body = br#"{"error":{"code":403,"message":"API disabled","status":"PERMISSION_DENIED","details":[{"@type":"type.googleapis.com/google.rpc.ErrorInfo","reason":"SERVICE_DISABLED","domain":"googleapis.com"}]}}"#;
        let err = api_error_from_body(403, body);
        assert!(matches!(&err, Error::Api { reason: Some(r), .. } if r == "SERVICE_DISABLED"));
        assert_eq!(err.api_error_kind(), Some(crate::error::ApiErrorKind::ApiDisabled));
    }

    #[test]
    fn api_error_falls_back_to_raw_body_when_not_json() {
        match api_error_from_body(502, b"<html>Bad Gateway</html>") {
            Error::Api { status, reason, message } => {
                assert_eq!(status, 502);
                assert_eq!(reason, None);
                assert!(message.contains("Bad Gateway"));
            }
            other => panic!("expected Error::Api, got {other:?}"),
        }
    }

    #[test]
    fn retryable_5xx_and_network_errors() {
        assert!(is_retryable(&Error::Api {
            status: 500,
            reason: None,
            message: "boom".to_string()
        }));
        assert!(is_retryable(&Error::Api {
            status: 503,
            reason: None,
            message: "boom".to_string()
        }));
    }

    #[test]
    fn not_retryable_401_403_and_4xx() {
        assert!(!is_retryable(&Error::Api { status: 401, reason: None, message: "u".to_string() }));
        assert!(!is_retryable(&Error::Api {
            status: 403,
            reason: Some("quotaExceeded".to_string()),
            message: "quota".to_string(),
        }));
        assert!(!is_retryable(&Error::Api {
            status: 403,
            reason: Some("rateLimitExceeded".to_string()),
            message: "rate".to_string(),
        }));
        assert!(!is_retryable(&Error::Api {
            status: 404,
            reason: None,
            message: "not found".to_string()
        }));
    }

    #[test]
    fn backoff_delay_grows_and_stays_bounded() {
        let d1 = backoff_delay(1);
        let d2 = backoff_delay(2);
        let d3 = backoff_delay(3);
        assert!(d1.as_millis() >= 250 && d1.as_millis() < 400);
        assert!(d2.as_millis() >= 500 && d2.as_millis() < 650);
        assert!(d3.as_millis() >= 1000 && d3.as_millis() < 1150);
        // The exponent is capped: a huge attempt count must not overflow.
        assert!(backoff_delay(1000).as_millis() < 5000);
    }
}
