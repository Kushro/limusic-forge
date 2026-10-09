//! What a dead YouTube video was called, from the Wayback Machine (F4, the recovery assistant).
//!
//! A deleted or private video keeps nothing but its id in a playlist, and the title is what the
//! search for a replacement needs. The Internet Archive often crawled the watch page while it was
//! up, and the archived HTML still carries the title.
//!
//! The flow, one video at a time:
//! 1. the id is checked against `^[A-Za-z0-9_-]{11}$` ([`valid_id`]), so nothing else ever leaves;
//! 2. CDX lists the newest few successful captures of the watch page ([`cdx_url`], [`parse_cdx`]),
//!    and the availability API stands in when CDX has none ([`availability_url`],
//!    [`parse_availability`]);
//! 3. each capture is fetched raw (`id_`, no Wayback toolbar) and read for a title
//!    ([`snapshot_url`], [`title_from_html`]) until one yields a real one.
//!
//! Security (design "Seguridad (Wayback)"): GET only, to `web.archive.org` and `archive.org`; the
//! final host after redirects must be on `archive.org`; no cookies (the shared client keeps no
//! jar); the app's own User-Agent; the user's proxy through [`crate::http::client`]; 20 s per
//! request; at most [`MAX_BODY`] bytes read from any response. Pacing between videos, backoff on
//! [`WaybackError::RateLimited`] and caching the verdict (`db::recover_titles`) are the caller's.

use std::sync::LazyLock;
use std::time::Duration;

use regex::Regex;

/// Per request, connect to last body byte.
const TIMEOUT: Duration = Duration::from_secs(20);

/// A watch page's title sits in its `<head>`, well inside this. Everything past it is dropped
/// unread, so a huge (or hostile) response costs at most this much memory.
pub const MAX_BODY: usize = 512 * 1024;

/// Not a browser UA: the Archive asks API clients to identify themselves.
const USER_AGENT: &str = concat!("LiMusicForge/", env!("CARGO_PKG_VERSION"));

/// Between the requests one [`fetch_title`] makes (CDX, then each capture). The Archive throttles
/// bursts; the caller paces between videos on top of this.
const BETWEEN_REQUESTS: Duration = Duration::from_secs(2);

#[derive(Debug, thiserror::Error)]
pub enum WaybackError {
    /// Not an 11-character YouTube id; nothing was sent.
    #[error("invalid_id")]
    InvalidId,
    /// 429 or 503: back off and try later. Not a verdict about the video.
    #[error("wayback_rate_limited")]
    RateLimited,
    /// A redirect left `archive.org`; the response was not read.
    #[error("wayback redirected off archive.org")]
    OffSite,
    /// Network failure, timeout or an unexpected status. Not a verdict about the video either.
    #[error("wayback: {0}")]
    Http(String),
}

/// `^[A-Za-z0-9_-]{11}$`, by hand.
pub fn valid_id(id: &str) -> bool {
    id.len() == 11 && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// A CDX timestamp: up to 14 digits (`YYYYMMDDhhmmss`). Checked before one goes into a URL.
fn valid_timestamp(ts: &str) -> bool {
    (1..=14).contains(&ts.len()) && ts.bytes().all(|b| b.is_ascii_digit())
}

/// The newest three 200 captures of the watch page. `?` and `=` are percent-encoded so they stay
/// part of the `url` parameter instead of splitting the CDX query. `id` must pass [`valid_id`].
pub fn cdx_url(id: &str) -> String {
    debug_assert!(valid_id(id));
    format!(
        "https://web.archive.org/cdx/search/cdx?url=youtube.com/watch%3Fv%3D{id}\
         &output=json&fl=timestamp&filter=statuscode:200&limit=-3"
    )
}

/// The availability API's closest capture of the watch page. `id` must pass [`valid_id`].
pub fn availability_url(id: &str) -> String {
    debug_assert!(valid_id(id));
    format!("https://archive.org/wayback/available?url=youtube.com/watch%3Fv%3D{id}")
}

/// One capture, raw: `id_` serves the archived bytes without the Wayback toolbar or rewritten
/// links. `ts` comes from [`parse_cdx`] or [`parse_availability`], `id` must pass [`valid_id`].
pub fn snapshot_url(ts: &str, id: &str) -> String {
    debug_assert!(valid_timestamp(ts) && valid_id(id));
    format!("https://web.archive.org/web/{ts}id_/https://www.youtube.com/watch?v={id}")
}

/// Timestamps from a CDX `output=json&fl=timestamp` answer, newest first. The answer is an array
/// of rows whose first is the header (`["timestamp"]`); `limit=-3` returns the last three captures
/// oldest first, hence the sort. Anything malformed yields nothing.
pub fn parse_cdx(body: &[u8]) -> Vec<String> {
    let Ok(rows) = serde_json::from_slice::<Vec<Vec<String>>>(body) else {
        return Vec::new();
    };
    let mut stamps: Vec<String> = rows
        .into_iter()
        .filter_map(|row| row.into_iter().next())
        .filter(|ts| valid_timestamp(ts))
        .collect();
    // Fixed-width digits, so the string order is the time order.
    stamps.sort_unstable_by(|a, b| b.cmp(a));
    stamps.dedup();
    stamps
}

/// The closest capture's timestamp from an availability answer
/// (`{"archived_snapshots":{"closest":{"available":true,"status":"200","timestamp":"..."}}}`), or
/// `None` when there is none, it is not available, or it was not a 200.
pub fn parse_availability(body: &[u8]) -> Option<String> {
    let v: serde_json::Value = serde_json::from_slice(body).ok()?;
    let closest = v.get("archived_snapshots")?.get("closest")?;
    if closest.get("available").and_then(|a| a.as_bool()) != Some(true) {
        return None;
    }
    if closest.get("status").and_then(|s| s.as_str()).is_some_and(|s| s != "200") {
        return None;
    }
    let ts = closest.get("timestamp")?.as_str()?;
    valid_timestamp(ts).then(|| ts.to_owned())
}

/// What an archived page's own chrome puts where a title should be: the site's name or an error
/// page. Compared lowercased. A gone video's stand-in titles are the shared
/// [`is_placeholder_title`](crate::playlist_tools::recover::is_placeholder_title)'s.
const PAGE_PLACEHOLDERS: &[&str] = &["youtube", "youtube music", "404 not found"];

fn is_placeholder(title: &str) -> bool {
    let lower = title.to_lowercase();
    PAGE_PLACEHOLDERS.contains(&lower.as_str())
        || crate::playlist_tools::recover::is_placeholder_title(title)
}

static META_TAG: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?is)<meta\b[^>]*>").unwrap());
static ATTR: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?is)([a-z][a-z0-9:_-]*)\s*=\s*(?:"([^"]*)"|'([^']*)'|([^\s"'>]+))"#).unwrap()
});
static TITLE_TAG: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?is)<title\b[^>]*>(.*?)</title\s*>").unwrap());

/// The `content` of the first `<meta>` whose `attr` (`name`, `property`) is `value`, attributes in
/// any order and any quoting.
fn meta_content(html: &str, attr: &str, value: &str) -> Option<String> {
    META_TAG.find_iter(html).find_map(|tag| {
        let mut key_matches = false;
        let mut content = None;
        for c in ATTR.captures_iter(tag.as_str()) {
            let name = c.get(1).map_or("", |m| m.as_str());
            let val =
                c.get(2).or_else(|| c.get(3)).or_else(|| c.get(4)).map_or("", |m| m.as_str());
            if name.eq_ignore_ascii_case(attr) && val.trim().eq_ignore_ascii_case(value) {
                key_matches = true;
            } else if name.eq_ignore_ascii_case("content") {
                content = Some(val.to_owned());
            }
        }
        content.filter(|_| key_matches)
    })
}

/// A video's title from its archived watch page: `<meta name="title">`, then
/// `<meta property="og:title">`, then `<title>`. Each is entity-decoded, whitespace-collapsed and
/// stripped of " - YouTube"; the first that is not a placeholder wins.
pub fn title_from_html(html: &str) -> Option<String> {
    let candidates = [
        meta_content(html, "name", "title"),
        meta_content(html, "property", "og:title"),
        TITLE_TAG.captures(html).and_then(|c| c.get(1)).map(|m| m.as_str().to_owned()),
    ];
    candidates.into_iter().flatten().find_map(|raw| clean_title(&raw))
}

fn clean_title(raw: &str) -> Option<String> {
    let decoded = decode_entities(raw);
    // Leading space so a bare "- YouTube" (an empty title) loses its suffix too.
    let padded = format!(" {}", decoded.split_whitespace().collect::<Vec<_>>().join(" "));
    let title = padded.strip_suffix(" - YouTube").unwrap_or(&padded).trim();
    (!is_placeholder(title)).then(|| title.to_owned())
}

/// The HTML entities a title realistically carries: the XML five, a few typographic named ones,
/// and every numeric one. Anything else is left as written.
fn decode_entities(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(at) = rest.find('&') {
        out.push_str(&rest[..at]);
        let tail = &rest[at..];
        // `;` is ASCII, so both slices below land on char boundaries.
        let decoded = tail[1..]
            .find(';')
            .filter(|&end| end <= 10)
            .and_then(|end| entity(&tail[1..1 + end]).map(|c| (c, end + 2)));
        match decoded {
            Some((c, used)) => {
                out.push(c);
                rest = &tail[used..];
            }
            None => {
                out.push('&');
                rest = &tail[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

fn entity(name: &str) -> Option<char> {
    if let Some(num) = name.strip_prefix('#') {
        let code = match num.strip_prefix(['x', 'X']) {
            Some(hex) => u32::from_str_radix(hex, 16).ok()?,
            None => num.parse::<u32>().ok()?,
        };
        return char::from_u32(code).filter(|&c| c != '\0');
    }
    Some(match name {
        "amp" => '&',
        "lt" => '<',
        "gt" => '>',
        "quot" => '"',
        "apos" => '\'',
        "nbsp" => ' ',
        "ndash" => '\u{2013}',
        "mdash" => '\u{2014}',
        "hellip" => '\u{2026}',
        "lsquo" => '\u{2018}',
        "rsquo" => '\u{2019}',
        "ldquo" => '\u{201C}',
        "rdquo" => '\u{201D}',
        _ => return None,
    })
}

/// `archive.org` or a subdomain of it. A bare `ends_with("archive.org")` would let
/// `notarchive.org` through.
fn on_archive(host: &str) -> bool {
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    host == "archive.org" || host.ends_with(".archive.org")
}

/// GET `url` and read at most [`MAX_BODY`] bytes of a 2xx body.
async fn get(url: &str) -> Result<Vec<u8>, WaybackError> {
    let mut resp = crate::http::client()
        .get(url)
        .header(reqwest::header::USER_AGENT, USER_AGENT)
        .timeout(TIMEOUT)
        .send()
        .await
        .map_err(|e| WaybackError::Http(e.to_string()))?;
    if !resp.url().host_str().is_some_and(on_archive) {
        return Err(WaybackError::OffSite);
    }
    let status = resp.status();
    if status == reqwest::StatusCode::TOO_MANY_REQUESTS
        || status == reqwest::StatusCode::SERVICE_UNAVAILABLE
    {
        return Err(WaybackError::RateLimited);
    }
    if !status.is_success() {
        return Err(WaybackError::Http(format!("HTTP {status}")));
    }
    let mut body = Vec::new();
    while let Some(chunk) = resp.chunk().await.map_err(|e| WaybackError::Http(e.to_string()))? {
        let room = MAX_BODY - body.len();
        body.extend_from_slice(&chunk[..chunk.len().min(room)]);
        if body.len() >= MAX_BODY {
            break;
        }
    }
    Ok(body)
}

/// The title the Wayback Machine has for `id`, newest capture first.
///
/// `Ok(None)` is a verdict: no capture, or none with a real title (worth caching as a negative).
/// An error is not: [`WaybackError::RateLimited`] means back off, and a network failure on every
/// capture is returned rather than turned into a false "nothing archived". Cancelling is dropping
/// the future.
pub async fn fetch_title(id: &str) -> Result<Option<String>, WaybackError> {
    if !valid_id(id) {
        return Err(WaybackError::InvalidId);
    }
    let mut stamps = parse_cdx(&get(&cdx_url(id)).await?);
    if stamps.is_empty() {
        tokio::time::sleep(BETWEEN_REQUESTS).await;
        stamps.extend(parse_availability(&get(&availability_url(id)).await?));
    }
    // A capture that was read and had no title is a verdict; one that failed to load is not.
    let mut read_any = false;
    let mut first_err = None;
    for ts in &stamps {
        tokio::time::sleep(BETWEEN_REQUESTS).await;
        match get(&snapshot_url(ts, id)).await {
            Ok(html) => {
                if let Some(title) = title_from_html(&String::from_utf8_lossy(&html)) {
                    return Ok(Some(title));
                }
                read_any = true;
            }
            Err(WaybackError::RateLimited) => return Err(WaybackError::RateLimited),
            Err(e) => {
                tracing::debug!(ts = ts.as_str(), "wayback capture failed: {e}");
                if first_err.is_none() {
                    first_err = Some(e);
                }
            }
        }
    }
    match first_err {
        Some(e) if !read_any => Err(e),
        _ => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_checked_strictly() {
        assert!(valid_id("dQw4w9WgXcQ"));
        assert!(valid_id("a_b-c_d-e_f"));
        assert!(!valid_id("dQw4w9WgXc"), "10 chars");
        assert!(!valid_id("dQw4w9WgXcQQ"), "12 chars");
        assert!(!valid_id("dQw4w9WgX/Q"));
        assert!(!valid_id("dQw4w9WgX&Q"));
        assert!(!valid_id("dQw4w9WgXñ"), "11 bytes, but not ASCII");
        assert!(!valid_id(""));
    }

    #[test]
    fn urls_encode_the_watch_url_as_one_parameter() {
        assert_eq!(
            cdx_url("dQw4w9WgXcQ"),
            "https://web.archive.org/cdx/search/cdx?url=youtube.com/watch%3Fv%3DdQw4w9WgXcQ\
             &output=json&fl=timestamp&filter=statuscode:200&limit=-3"
        );
        assert_eq!(
            availability_url("dQw4w9WgXcQ"),
            "https://archive.org/wayback/available?url=youtube.com/watch%3Fv%3DdQw4w9WgXcQ"
        );
        assert_eq!(
            snapshot_url("20200101123456", "dQw4w9WgXcQ"),
            "https://web.archive.org/web/20200101123456id_/https://www.youtube.com/watch?v=dQw4w9WgXcQ"
        );
    }

    #[test]
    fn cdx_answers_come_back_newest_first() {
        let body = br#"[["timestamp"],["20150101000000"],["20190505101010"],["20170303030303"]]"#;
        assert_eq!(parse_cdx(body), vec!["20190505101010", "20170303030303", "20150101000000"]);
        assert!(parse_cdx(b"[]").is_empty(), "no captures");
        assert!(parse_cdx(br#"[["timestamp"]]"#).is_empty(), "header only");
        assert!(parse_cdx(b"<html>error</html>").is_empty(), "not JSON");
        assert_eq!(
            parse_cdx(br#"[["timestamp"],["2019/../x"],["20190505101010"]]"#),
            vec!["20190505101010"],
            "a non-digit timestamp never reaches a URL"
        );
    }

    #[test]
    fn availability_answers_yield_the_closest_200() {
        let ok = br#"{"url":"youtube.com/watch?v=dQw4w9WgXcQ","archived_snapshots":{"closest":
            {"status":"200","available":true,"url":"http://web.archive.org/web/20200101000000/x",
            "timestamp":"20200101000000"}}}"#;
        assert_eq!(parse_availability(ok).as_deref(), Some("20200101000000"));
        assert_eq!(parse_availability(br#"{"archived_snapshots":{}}"#), None);
        let not_200 = br#"{"archived_snapshots":{"closest":
            {"status":"404","available":true,"timestamp":"20200101000000"}}}"#;
        assert_eq!(parse_availability(not_200), None);
        let unavailable = br#"{"archived_snapshots":{"closest":
            {"status":"200","available":false,"timestamp":"20200101000000"}}}"#;
        assert_eq!(parse_availability(unavailable), None);
        assert_eq!(parse_availability(b"nope"), None);
    }

    #[test]
    fn titles_come_from_meta_title_first() {
        let html = r#"<html><head>
            <title>Ignored &amp; later - YouTube</title>
            <meta property="og:title" content="Also ignored">
            <meta name="title" content="Rick Astley - Never Gonna Give You Up (Official Music Video)">
            </head></html>"#;
        assert_eq!(
            title_from_html(html).as_deref(),
            Some("Rick Astley - Never Gonna Give You Up (Official Music Video)")
        );
    }

    #[test]
    fn titles_fall_back_to_og_title_with_entities_decoded() {
        let html = r#"<head><meta content="Simon &amp; Garfunkel &#8211; The Boxer &quot;Live&quot; &#x2764;" property="og:title" />
            <title>YouTube</title></head>"#;
        assert_eq!(
            title_from_html(html).as_deref(),
            Some("Simon & Garfunkel \u{2013} The Boxer \"Live\" \u{2764}")
        );
    }

    #[test]
    fn titles_fall_back_to_the_title_tag_without_the_suffix() {
        let html = "<head><TITLE>\n  Daft Punk &#39;Around the World&#39;\n - YouTube</TITLE></head>";
        assert_eq!(title_from_html(html).as_deref(), Some("Daft Punk 'Around the World'"));
    }

    #[test]
    fn placeholders_are_not_titles() {
        let deleted = r#"<meta name="title" content="Deleted video"><title> - YouTube</title>"#;
        assert_eq!(title_from_html(deleted), None);
        let private = r#"<meta property="og:title" content="Private video"><title>YouTube</title>"#;
        assert_eq!(title_from_html(private), None);
        assert_eq!(title_from_html("<title>Video no disponible</title>"), None);
        assert_eq!(title_from_html("<p>no head at all</p>"), None);
        // A placeholder in the first place does not hide a real title in a later one.
        let mixed = r#"<meta name="title" content="YouTube"><title>Real Song - YouTube</title>"#;
        assert_eq!(title_from_html(mixed).as_deref(), Some("Real Song"));
    }

    #[test]
    fn entities_decode_and_unknown_ones_survive() {
        assert_eq!(decode_entities("a &amp;amp; b"), "a &amp; b", "one pass only");
        assert_eq!(decode_entities("R&B &unknown; & &#0; &#65;"), "R&B &unknown; & &#0; A");
        assert_eq!(decode_entities("caf&eacute;"), "caf&eacute;");
        assert_eq!(decode_entities("ñ &lt;3"), "ñ <3");
    }

    #[test]
    fn only_archive_org_hosts_are_trusted() {
        assert!(on_archive("web.archive.org"));
        assert!(on_archive("archive.org"));
        assert!(on_archive("WEB.ARCHIVE.ORG."));
        assert!(!on_archive("notarchive.org"));
        assert!(!on_archive("archive.org.evil.com"));
        assert!(!on_archive("youtube.com"));
    }
}
