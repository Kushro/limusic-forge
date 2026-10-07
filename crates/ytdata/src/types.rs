//! Typed request/response shapes for the Data API endpoints.
//!
//! Wire structs (`*ListResponse`, `*ListItem`, `*Snippet`, ...) are `pub(crate)`: they only
//! `Deserialize` the `fields=` subset [`crate::client::YouTubeClient`] requests and are converted
//! right away into the public `*Summary` types. `#[serde(default)]` is used on everything Google
//! is known to omit (a private video's `snippet`, a video without region restrictions, ...).

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------
// Shared
// ---------------------------------------------------------------------

/// A page of results plus the token for the next one. The client never loops pagination
/// itself: the caller drives it so it can report progress and check cancellation per page.
#[derive(Debug, Clone, PartialEq)]
pub struct Page<T> {
    pub items: Vec<T>,
    pub next_page_token: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub(crate) struct MediumThumbnail {
    #[serde(default)]
    pub medium: Option<ThumbnailInfo>,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct ThumbnailInfo {
    pub url: String,
}

pub(crate) fn parse_rfc3339(raw: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(raw).ok().map(|dt| dt.with_timezone(&Utc))
}

/// Parses a `contentDetails.duration` ISO-8601 duration (`PT4M13S`, `P1DT2H`) into seconds.
/// `None` for anything that does not parse: a malformed duration must never fail a sync.
pub(crate) fn parse_iso8601_duration_seconds(s: &str) -> Option<i64> {
    let s = s.strip_prefix('P')?;
    let (date_part, time_part) = match s.split_once('T') {
        Some((d, t)) => (d, Some(t)),
        None => (s, None),
    };

    let mut seconds: i64 = duration_component(date_part, 'D')? * 86_400;
    if let Some(time_part) = time_part {
        seconds += duration_component(time_part, 'H')? * 3_600;
        seconds += duration_component(time_part, 'M')? * 60;
        seconds += duration_component(time_part, 'S')?;
    }
    Some(seconds)
}

/// The integer right before `unit` in `s` (`"1H30M"`, `'M'` → `30`), or `0` when `unit` is
/// absent.
fn duration_component(s: &str, unit: char) -> Option<i64> {
    match s.find(unit) {
        None => Some(0),
        Some(idx) => {
            let start = s[..idx].rfind(|c: char| !c.is_ascii_digit()).map(|i| i + 1).unwrap_or(0);
            s[start..idx].parse::<i64>().ok()
        }
    }
}

// ---------------------------------------------------------------------
// playlists.list
// ---------------------------------------------------------------------

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PlaylistsListResponse {
    #[serde(default)]
    pub items: Vec<PlaylistListItem>,
    #[serde(default)]
    pub next_page_token: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PlaylistListItem {
    pub id: String,
    pub snippet: PlaylistSnippet,
    #[serde(default)]
    pub content_details: PlaylistContentDetails,
    #[serde(default)]
    pub status: PlaylistStatus,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PlaylistSnippet {
    pub title: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub thumbnails: MediumThumbnail,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PlaylistContentDetails {
    #[serde(default)]
    pub item_count: i64,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PlaylistStatus {
    #[serde(default)]
    pub privacy_status: Option<String>,
}

/// One of the authenticated user's own playlists.
#[derive(Debug, Clone, PartialEq)]
pub struct PlaylistSummary {
    pub id: String,
    pub title: String,
    pub description: String,
    pub thumbnail_url: Option<String>,
    pub item_count: i64,
    /// Raw `status.privacyStatus` (`"public"` / `"unlisted"` / `"private"`); `"private"` when
    /// Google omitted it.
    pub privacy_status: String,
}

impl From<PlaylistListItem> for PlaylistSummary {
    fn from(item: PlaylistListItem) -> Self {
        Self {
            id: item.id,
            title: item.snippet.title,
            description: item.snippet.description,
            thumbnail_url: item.snippet.thumbnails.medium.map(|t| t.url),
            item_count: item.content_details.item_count,
            privacy_status: item.status.privacy_status.unwrap_or_else(|| "private".to_string()),
        }
    }
}

// ---------------------------------------------------------------------
// playlistItems.list
// ---------------------------------------------------------------------

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PlaylistItemsListResponse {
    #[serde(default)]
    pub items: Vec<PlaylistItemListItem>,
    #[serde(default)]
    pub next_page_token: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PlaylistItemListItem {
    pub id: String,
    pub snippet: PlaylistItemSnippet,
    #[serde(default)]
    pub content_details: PlaylistItemContentDetails,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PlaylistItemSnippet {
    /// `"Deleted video"` / `"Private video"` placeholders land here verbatim; this crate does
    /// not interpret them.
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub video_owner_channel_title: Option<String>,
    #[serde(default)]
    pub video_owner_channel_id: Option<String>,
    /// When the item was *added to the playlist*, not the video's publish date.
    #[serde(default)]
    pub published_at: Option<String>,
    pub position: i64,
    pub resource_id: ResourceId,
    #[serde(default)]
    pub thumbnails: MediumThumbnail,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct ResourceId {
    #[serde(rename = "videoId")]
    pub video_id: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PlaylistItemContentDetails {
    #[serde(default)]
    pub video_published_at: Option<String>,
}

/// One entry in a playlist, in playlist order.
#[derive(Debug, Clone, PartialEq)]
pub struct PlaylistItemSummary {
    /// The *playlist item's* id (stable per occurrence, so a duplicated video can be addressed
    /// individually).
    pub playlist_item_id: String,
    pub video_id: String,
    pub position: i64,
    pub title: String,
    pub channel_title: Option<String>,
    pub channel_id: Option<String>,
    /// When the item was added to the playlist.
    pub added_at: Option<DateTime<Utc>>,
    pub video_published_at: Option<DateTime<Utc>>,
    pub thumbnail_url: Option<String>,
}

impl From<PlaylistItemListItem> for PlaylistItemSummary {
    fn from(item: PlaylistItemListItem) -> Self {
        Self {
            playlist_item_id: item.id,
            video_id: item.snippet.resource_id.video_id,
            position: item.snippet.position,
            title: item.snippet.title,
            channel_title: item.snippet.video_owner_channel_title,
            channel_id: item.snippet.video_owner_channel_id,
            added_at: item.snippet.published_at.as_deref().and_then(parse_rfc3339),
            video_published_at: item
                .content_details
                .video_published_at
                .as_deref()
                .and_then(parse_rfc3339),
            thumbnail_url: item.snippet.thumbnails.medium.map(|t| t.url),
        }
    }
}

// ---------------------------------------------------------------------
// videos.list
// ---------------------------------------------------------------------

#[derive(Debug, Clone, Default, Deserialize)]
pub(crate) struct VideosListResponse {
    #[serde(default)]
    pub items: Vec<VideoListItem>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct VideoListItem {
    pub id: String,
    #[serde(default)]
    pub snippet: Option<VideoSnippet>,
    #[serde(default)]
    pub content_details: Option<VideoContentDetails>,
    #[serde(default)]
    pub status: Option<VideoStatusPayload>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct VideoSnippet {
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub channel_id: Option<String>,
    #[serde(default)]
    pub channel_title: Option<String>,
    #[serde(default)]
    pub published_at: Option<String>,
    #[serde(default)]
    pub thumbnails: MediumThumbnail,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct VideoContentDetails {
    #[serde(default)]
    pub duration: Option<String>,
    #[serde(default)]
    pub region_restriction: Option<RegionRestriction>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RegionRestriction {
    #[serde(default)]
    pub blocked: Vec<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct VideoStatusPayload {
    #[serde(default)]
    pub privacy_status: Option<String>,
    #[serde(default)]
    pub upload_status: Option<String>,
}

/// A video's state as of this `videos.list` call. A video absent from the response altogether
/// is deleted (or otherwise gone); detecting that by diffing requested vs. returned ids is the
/// caller's job, it is not representable here.
#[derive(Debug, Clone, PartialEq)]
pub struct VideoSummary {
    pub id: String,
    pub title: Option<String>,
    pub channel_id: Option<String>,
    pub channel_title: Option<String>,
    pub duration_s: Option<i64>,
    pub published_at: Option<DateTime<Utc>>,
    pub thumbnail_url: Option<String>,
    /// Raw `status.privacyStatus`, `None` if Google omitted `status`.
    pub privacy_status: Option<String>,
    pub upload_status: Option<String>,
    /// Whether `contentDetails.regionRestriction.blocked` was present and non-empty.
    pub region_restricted: bool,
    /// The blocked region codes, verbatim (empty when not restricted).
    pub blocked_regions: Vec<String>,
}

impl From<VideoListItem> for VideoSummary {
    fn from(item: VideoListItem) -> Self {
        let thumbnail_url =
            item.snippet.as_ref().and_then(|s| s.thumbnails.medium.as_ref()).map(|t| t.url.clone());
        let blocked_regions = item
            .content_details
            .as_ref()
            .and_then(|c| c.region_restriction.as_ref())
            .map(|r| r.blocked.clone())
            .unwrap_or_default();
        Self {
            id: item.id,
            title: item.snippet.as_ref().and_then(|s| s.title.clone()),
            channel_id: item.snippet.as_ref().and_then(|s| s.channel_id.clone()),
            channel_title: item.snippet.as_ref().and_then(|s| s.channel_title.clone()),
            duration_s: item
                .content_details
                .as_ref()
                .and_then(|c| c.duration.as_deref())
                .and_then(parse_iso8601_duration_seconds),
            published_at: item
                .snippet
                .as_ref()
                .and_then(|s| s.published_at.as_deref())
                .and_then(parse_rfc3339),
            thumbnail_url,
            privacy_status: item.status.as_ref().and_then(|s| s.privacy_status.clone()),
            upload_status: item.status.as_ref().and_then(|s| s.upload_status.clone()),
            region_restricted: !blocked_regions.is_empty(),
            blocked_regions,
        }
    }
}

// ---------------------------------------------------------------------
// Write endpoints: playlists.insert/update/delete, playlistItems.insert/update/delete.
// ---------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
pub(crate) struct PlaylistSnippetWrite<'a> {
    pub title: &'a str,
    pub description: &'a str,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PlaylistStatusWrite<'a> {
    pub privacy_status: &'a str,
}

/// Body for `playlists.insert`. `playlists.update` always resends the whole snippet (a missing
/// `description` would blank it), so there is no partial-update body.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct PlaylistInsertBody<'a> {
    pub snippet: PlaylistSnippetWrite<'a>,
    pub status: PlaylistStatusWrite<'a>,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct PlaylistUpdateBody<'a> {
    pub id: &'a str,
    pub snippet: PlaylistSnippetWrite<'a>,
    pub status: PlaylistStatusWrite<'a>,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct PlaylistWriteResponse {
    pub id: String,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct ResourceIdWrite<'a> {
    pub kind: &'a str,
    #[serde(rename = "videoId")]
    pub video_id: &'a str,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PlaylistItemSnippetInsert<'a> {
    pub playlist_id: &'a str,
    pub resource_id: ResourceIdWrite<'a>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub position: Option<u32>,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct PlaylistItemInsertBody<'a> {
    pub snippet: PlaylistItemSnippetInsert<'a>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PlaylistItemSnippetUpdate<'a> {
    pub playlist_id: &'a str,
    pub resource_id: ResourceIdWrite<'a>,
    pub position: u32,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct PlaylistItemUpdateBody<'a> {
    pub id: &'a str,
    pub snippet: PlaylistItemSnippetUpdate<'a>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PlaylistItemWriteResponse {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub snippet: Option<PlaylistItemWriteSnippetResponse>,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct PlaylistItemWriteSnippetResponse {
    #[serde(default)]
    pub position: Option<i64>,
}

/// Result of `playlist_items_insert` / `playlist_items_update_position`: the server-assigned
/// identity and position a job runner needs to record and to build the inverse action.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlaylistItemWriteResult {
    pub playlist_item_id: String,
    pub position: Option<i64>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn duration_minutes_and_seconds() {
        assert_eq!(parse_iso8601_duration_seconds("PT4M13S"), Some(253));
    }

    #[test]
    fn duration_hours_minutes_seconds() {
        assert_eq!(parse_iso8601_duration_seconds("PT1H2M3S"), Some(3723));
    }

    #[test]
    fn duration_days_and_hours() {
        assert_eq!(parse_iso8601_duration_seconds("P1DT2H"), Some(93_600));
    }

    #[test]
    fn duration_zero_seconds() {
        assert_eq!(parse_iso8601_duration_seconds("PT0S"), Some(0));
    }

    #[test]
    fn duration_hours_only_no_minutes_or_seconds() {
        assert_eq!(parse_iso8601_duration_seconds("PT1H"), Some(3600));
    }

    #[test]
    fn duration_missing_p_prefix_is_none() {
        assert_eq!(parse_iso8601_duration_seconds("T4M13S"), None);
    }

    #[test]
    fn duration_garbage_is_none() {
        assert_eq!(parse_iso8601_duration_seconds("not-a-duration"), None);
    }

    #[test]
    fn video_list_item_deserializes_camel_case_content_details() {
        let json = r#"{
            "id": "v1",
            "snippet": {"title": "T"},
            "contentDetails": {"duration": "PT3M20S"},
            "status": {"privacyStatus": "public", "uploadStatus": "processed"}
        }"#;
        let item: VideoListItem = serde_json::from_str(json).unwrap();
        let summary: VideoSummary = item.into();
        assert_eq!(summary.duration_s, Some(200));
    }

    #[test]
    fn playlist_item_list_item_deserializes_camel_case_content_details() {
        let json = r#"{
            "id": "pli1",
            "snippet": {
                "title": "T",
                "position": 0,
                "resourceId": {"videoId": "v1"}
            },
            "contentDetails": {"videoPublishedAt": "2020-01-02T03:04:05Z"}
        }"#;
        let item: PlaylistItemListItem = serde_json::from_str(json).unwrap();
        let summary: PlaylistItemSummary = item.into();
        assert!(summary.video_published_at.is_some());
    }

    #[test]
    fn playlist_list_item_deserializes_camel_case_item_count() {
        let json = r#"{
            "id": "PL1",
            "snippet": {"title": "T", "description": ""},
            "contentDetails": {"itemCount": 126},
            "status": {"privacyStatus": "public"}
        }"#;
        let item: PlaylistListItem = serde_json::from_str(json).unwrap();
        let summary: PlaylistSummary = item.into();
        assert_eq!(summary.item_count, 126);
        assert_eq!(summary.privacy_status, "public");
    }

    #[test]
    fn playlist_summary_defaults_missing_privacy_status_to_private() {
        let item = PlaylistListItem {
            id: "PL1".to_string(),
            snippet: PlaylistSnippet {
                title: "T".to_string(),
                description: String::new(),
                thumbnails: MediumThumbnail::default(),
            },
            content_details: PlaylistContentDetails { item_count: 0 },
            status: PlaylistStatus { privacy_status: None },
        };
        let summary: PlaylistSummary = item.into();
        assert_eq!(summary.privacy_status, "private");
    }

    #[test]
    fn video_summary_from_item_with_no_snippet_or_status_is_all_none() {
        let item = VideoListItem {
            id: "v1".to_string(),
            snippet: None,
            content_details: None,
            status: None,
        };
        let summary: VideoSummary = item.into();
        assert_eq!(summary.title, None);
        assert_eq!(summary.duration_s, None);
        assert!(!summary.region_restricted);
        assert!(summary.blocked_regions.is_empty());
    }

    #[test]
    fn video_summary_region_restricted_when_blocked_list_non_empty() {
        let item = VideoListItem {
            id: "v1".to_string(),
            snippet: None,
            content_details: Some(VideoContentDetails {
                duration: None,
                region_restriction: Some(RegionRestriction { blocked: vec!["AR".to_string()] }),
            }),
            status: None,
        };
        let summary: VideoSummary = item.into();
        assert!(summary.region_restricted);
        assert_eq!(summary.blocked_regions, vec!["AR".to_string()]);
    }
}
