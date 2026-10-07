//! The monitor's second reader: a playlist read through the YouTube Data API instead of InnerTube,
//! for the two things only it knows, when each track was added
//! (`playlistItems.snippet.publishedAt`) and the playlist's privacy
//! (`playlists.status.privacyStatus`), plus why a track is unavailable.
//!
//! - **Which reader** ([`choose_reader`]): the Data API when `playlist_engine` is not `innertube`,
//!   the status ([`crate::ytdata_status`]) is `ok`, and the run's estimated cost fits what the
//!   monitor may spend ([`monitor_allowance`]). Otherwise InnerTube, which costs nothing, and the
//!   run's `detail_json` says why. A playlist the Data API cannot address (Liked Music, someone
//!   else's collaborative one) is read through InnerTube within the same run, and so is one whose
//!   Data API read fails; a failure that rules the API out for the day (disabled, out of quota,
//!   the account needing reconnecting) switches the rest of the run to InnerTube.
//! - **Estimate** ([`estimate_units`], port of PlaylistForge's `pf-app/src/monitor/estimate.rs`):
//!   `playlists.list` pages + `playlistItems.list` pages + `videos.list` pages, from the sizes the
//!   last sync stored (no call made to estimate). One deviation: item pages are counted per
//!   playlist (`Σ ceil(nᵢ / 50)`, at least one each), since a page never spans two playlists.
//! - **Budget**: a scheduled or headless run is the day's backup and may spend the backup reserve
//!   on top of what is free; a run someone asked for may not eat into the reserve and spends only
//!   the free part ([`crate::jobs::budget`]). The units spent go to the ledger
//!   ([`crate::quota::LedgerQuotaSink`]) and to `monitor_runs.units_spent`, which is what the
//!   reserve shrinks by.
//! - **Mapping** ([`song_from`]): title, artist = the uploading channel (` - Topic` dropped),
//!   duration from ISO 8601 to `m:ss`, thumbnail, the playlist-item id as `set_video_id`. A video
//!   missing from `videos.list` is `deleted`; a private one `private`; one with blocked regions
//!   `region_restricted`; each `unavailable`, with the reason in its `song_json`. A track the index
//!   already knows keeps the richer InnerTube metadata stored for it ([`merge_known`]).
//!
//! The token store blocks, so tokens are fetched on a blocking thread, as the job runner does.

use std::collections::HashMap;
use std::future::Future;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::Arc;

use chrono::{DateTime, Utc};
use innertube::SongItem;
use serde_json::{json, Value};
use ytdata::auth::accounts::AccountManager;
use ytdata::client::{YouTubeClient, VIDEOS_LIST_BATCH};
use ytdata::error::{ApiErrorKind, AuthError, Error as YtError};
use ytdata::quota::QuotaSink;
use ytdata::types::{PlaylistItemSummary, PlaylistSummary, VideoSummary};

use crate::db::Db;
use crate::jobs::engine::{DataApiState, EngineSetting};

/// A playlist's items, at most this many pages (10 000 rows, twice YouTube's cap), so a runaway
/// `nextPageToken` can't loop forever. A read that hits it is not complete.
const MAX_ITEM_PAGES: usize = 200;
const MAX_PLAYLIST_PAGES: usize = 50;
const PAGE: i64 = 50;

pub const REASON_DELETED: &str = "deleted";
pub const REASON_PRIVATE: &str = "private";
pub const REASON_REGION: &str = "region_restricted";

// --- estimate -----------------------------------------------------------------------------------

fn ceil_div_50(n: i64) -> i64 {
    if n <= 0 {
        0
    } else {
        (n + PAGE - 1) / PAGE
    }
}

/// The sizes an estimate is made from: the playlists to read and their tracks.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LibraryStats {
    pub playlists: i64,
    pub items: i64,
    /// `playlistItems.list` pages: one per 50 tracks of each playlist, and one for an empty one.
    pub item_pages: i64,
}

impl LibraryStats {
    /// From each playlist's track count.
    pub fn of(counts: impl IntoIterator<Item = i64>) -> Self {
        let mut stats = LibraryStats::default();
        for n in counts {
            let n = n.max(0);
            stats.playlists += 1;
            stats.items += n;
            stats.item_pages += ceil_div_50(n).max(1);
        }
        stats
    }
}

/// Units a read of `stats` costs, `listed` being how many playlists `playlists.list` has to page
/// through to find them (all of the account's). Pure and total.
pub fn estimate_units(listed: i64, stats: &LibraryStats) -> i64 {
    ceil_div_50(listed) + stats.item_pages + ceil_div_50(stats.items)
}

/// The account playlists the last syncs stored that the Data API can read (`VLPL…`), with their
/// track counts. Liked Music and other non-`PL` ids are read through InnerTube, for free.
pub fn known_counts(db: &Db) -> HashMap<String, i64> {
    db.playlist_syncs()
        .into_iter()
        .filter(|(id, _)| crate::jobs::planner::ytdata_playlist_id(id).is_some())
        .map(|(id, sync)| (id, sync.item_count))
        .collect()
}

/// What the monitor may spend now. A backup run (scheduled or headless) owns the reserve: it gets
/// what is free plus what is left of the reserve. Any other run gets only what is free, so it
/// never eats the day's backup.
pub fn monitor_allowance(db: &Db, now: DateTime<Utc>, backup_run: bool) -> i64 {
    use crate::jobs::budget;
    if !backup_run {
        return budget::available_for_jobs_now(db, now).unwrap_or(0);
    }
    // Free plus the reserve is the whole day short of the margin.
    let spent = crate::quota::spent_today(db, now).unwrap_or(0);
    (budget::daily_units(db) - budget::safety_margin_units(db) - spent).max(0)
}

/// Why a run reads through InnerTube.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Skip {
    /// `playlist_engine = innertube`.
    Setting,
    /// The Data API is not usable now.
    State(DataApiState),
    /// The estimate does not fit what the monitor may spend.
    OverBudget { estimate: i64, allowance: i64 },
}

impl Skip {
    pub fn detail(self) -> Value {
        match self {
            Skip::Setting => json!({ "reader": "innertube", "why": "setting" }),
            Skip::State(state) => json!({ "reader": "innertube", "why": state }),
            Skip::OverBudget { estimate, allowance } => json!({
                "reader": "innertube",
                "why": "over_budget",
                "estimate": estimate,
                "allowance": allowance,
            }),
        }
    }
}

/// The reader for a run. Pure.
pub fn choose_reader(
    setting: EngineSetting,
    state: DataApiState,
    estimate: i64,
    allowance: i64,
) -> Result<(), Skip> {
    if setting == EngineSetting::Innertube {
        return Err(Skip::Setting);
    }
    if state != DataApiState::Ok {
        return Err(Skip::State(state));
    }
    if estimate > allowance {
        return Err(Skip::OverBudget { estimate, allowance });
    }
    Ok(())
}

/// A failure after which the rest of the run should not try the Data API again.
pub fn rules_out_the_run(err: &YtError) -> bool {
    matches!(err.api_error_kind(), Some(ApiErrorKind::ApiDisabled | ApiErrorKind::QuotaExceeded))
        || err.needs_reauthorization()
        || err.is_keyring_unavailable()
        || err.is_unauthorized()
        || matches!(err, YtError::Auth(AuthError::NoClientSecret))
}

// --- mapping ------------------------------------------------------------------------------------

/// `253` → `"4:13"`, `3723` → `"1:02:03"`.
pub fn format_duration(secs: i64) -> String {
    let secs = secs.max(0);
    let (h, m, s) = (secs / 3600, secs % 3600 / 60, secs % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

/// YouTube's stand-in titles for a gone video: never taken as a real title.
fn is_placeholder(title: &str) -> bool {
    matches!(title, "Deleted video" | "Private video")
}

/// The artist a channel stands for: YouTube Music's auto-generated `Artist - Topic` is the artist.
fn artist_of(channel: &str) -> &str {
    channel.strip_suffix(" - Topic").unwrap_or(channel).trim()
}

/// Why a track is unavailable, `None` when it plays. `video` is its `videos.list` entry; absent
/// means the video is gone.
pub fn unavailable_reason(video: Option<&VideoSummary>) -> Option<&'static str> {
    let Some(video) = video else { return Some(REASON_DELETED) };
    if video.privacy_status.as_deref() == Some("private") {
        return Some(REASON_PRIVATE);
    }
    if matches!(video.upload_status.as_deref(), Some("rejected" | "deleted")) {
        return Some(REASON_DELETED);
    }
    if video.region_restricted {
        return Some(REASON_REGION);
    }
    None
}

/// One playlist item as a row, from its `videos.list` entry when there is one.
pub fn song_from(item: &PlaylistItemSummary, video: Option<&VideoSummary>) -> SongItem {
    let title = video
        .and_then(|v| v.title.clone())
        .filter(|t| !t.is_empty())
        .unwrap_or_else(|| item.title.clone());
    let channel = video
        .and_then(|v| v.channel_title.clone())
        .or_else(|| item.channel_title.clone())
        .unwrap_or_default();
    let channel_id = video.and_then(|v| v.channel_id.clone()).or_else(|| item.channel_id.clone());
    let thumbnail =
        video.and_then(|v| v.thumbnail_url.clone()).or_else(|| item.thumbnail_url.clone());
    SongItem {
        video_id: item.video_id.clone(),
        title,
        artists: artist_of(&channel).to_owned(),
        artist_id: channel_id,
        duration: video.and_then(|v| v.duration_s).map(format_duration),
        thumbnail,
        set_video_id: Some(item.playlist_item_id.clone()),
        // The Data API does not tell a song from a music video; a Topic channel's upload is the
        // audio track YouTube Music generates, anything else a video.
        is_video: !channel.is_empty() && !channel.ends_with(" - Topic"),
        unavailable: unavailable_reason(video).is_some(),
        ..Default::default()
    }
}

/// A row the index already knows keeps what InnerTube told it (album, artist links, the YouTube
/// Music title), taking from the Data API only the row's handle, whether it plays, and whatever
/// the stored copy lacks. A placeholder title never replaces a real one.
pub fn merge_known(mapped: SongItem, known: Option<SongItem>) -> SongItem {
    let Some(known) = known else { return mapped };
    let title = if known.title.is_empty() || is_placeholder(&known.title) {
        mapped.title
    } else {
        known.title.clone()
    };
    let artists = if known.artists.is_empty() { mapped.artists } else { known.artists.clone() };
    SongItem {
        title,
        artists,
        artist_id: known.artist_id.clone().or(mapped.artist_id),
        duration: known.duration.clone().or(mapped.duration),
        thumbnail: known.thumbnail.clone().or(mapped.thumbnail),
        set_video_id: mapped.set_video_id,
        unavailable: mapped.unavailable,
        ..known
    }
}

/// One playlist read through the Data API, ready for `monitor::Read`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct DataApiRead {
    pub songs: Vec<SongItem>,
    /// `videoId` → when it was added (the earliest, for a track listed twice), epoch seconds.
    pub added_at: HashMap<String, i64>,
    /// `videoId` → why it is unavailable.
    pub reasons: HashMap<String, String>,
    pub title: String,
    pub privacy: String,
    /// The playlist's own count of its items.
    pub listed: usize,
    /// Every page was read.
    pub complete: bool,
}

/// Turns a playlist's items and their videos into rows, dates and reasons. `known` is the
/// index's copy of the playlist (`videoId`, `song_json`).
pub fn to_read(
    playlist: &PlaylistSummary,
    mut items: Vec<PlaylistItemSummary>,
    videos: &HashMap<String, VideoSummary>,
    known: &[(String, Option<String>)],
    complete: bool,
) -> DataApiRead {
    let known: HashMap<&str, SongItem> = known
        .iter()
        .filter_map(|(v, j)| Some((v.as_str(), serde_json::from_str(j.as_deref()?).ok()?)))
        .collect();
    items.sort_by_key(|i| i.position);
    let mut read = DataApiRead {
        title: playlist.title.clone(),
        privacy: playlist.privacy_status.clone(),
        listed: usize::try_from(playlist.item_count).unwrap_or(0),
        complete,
        ..Default::default()
    };
    for item in &items {
        let video = videos.get(&item.video_id);
        if let Some(at) = item.added_at.map(|at| at.timestamp()) {
            let earliest = read.added_at.entry(item.video_id.clone()).or_insert(at);
            *earliest = (*earliest).min(at);
        }
        if let Some(reason) = unavailable_reason(video) {
            read.reasons.insert(item.video_id.clone(), reason.to_owned());
        }
        let known = known.get(item.video_id.as_str()).cloned();
        read.songs.push(merge_known(song_from(item, video), known));
    }
    read
}

// --- fetching -----------------------------------------------------------------------------------

/// The ledger, plus a count of what this run spent.
struct CountingSink {
    ledger: crate::quota::LedgerQuotaSink,
    spent: Arc<AtomicI64>,
}

impl QuotaSink for CountingSink {
    fn record(&self, endpoint: &'static str, units: u32, account_id: Option<&str>) {
        self.spent.fetch_add(i64::from(units), Ordering::Relaxed);
        self.ledger.record(endpoint, units, account_id);
    }
}

/// Reads one channel's playlists, billing every call to the ledger.
pub struct Reader {
    manager: Arc<AccountManager>,
    client: YouTubeClient,
    channel_id: String,
    spent: Arc<AtomicI64>,
}

impl Reader {
    pub fn new(
        db: Arc<Db>,
        manager: Arc<AccountManager>,
        channel_id: String,
    ) -> Result<Self, YtError> {
        let spent = Arc::new(AtomicI64::new(0));
        let ledger = crate::quota::LedgerQuotaSink::new(db);
        let sink = CountingSink { ledger, spent: spent.clone() };
        let client = YouTubeClient::new()?.with_quota_sink(Arc::new(sink));
        Ok(Self { manager, client, channel_id, spent })
    }

    /// Units spent so far.
    pub fn spent(&self) -> i64 {
        self.spent.load(Ordering::Relaxed)
    }

    /// A valid access token, fetched on a blocking thread (the token store blocks). `fresh` drops
    /// the cached one first.
    async fn token(&self, fresh: bool) -> Result<String, YtError> {
        let manager = self.manager.clone();
        let id = self.channel_id.clone();
        let joined = tokio::task::spawn_blocking(move || {
            if fresh {
                manager.invalidate_cached_token(&id);
            }
            tokio::runtime::Handle::current().block_on(manager.get_valid_access_token(&id))
        })
        .await;
        joined.unwrap_or_else(|e| Err(YtError::Io(std::io::Error::other(e.to_string()))))
    }

    /// Runs `call` with a token; on a 401, once more with a fresh one.
    async fn call<T, F, Fut>(&self, call: F) -> Result<T, YtError>
    where
        F: Fn(String) -> Fut,
        Fut: Future<Output = Result<T, YtError>>,
    {
        let token = self.token(false).await?;
        match call(token).await {
            Err(e) if e.is_unauthorized() => call(self.token(true).await?).await,
            other => other,
        }
    }

    /// Every playlist of the channel, with its privacy and item count.
    pub async fn list_playlists(&self) -> Result<Vec<PlaylistSummary>, YtError> {
        let (client, channel) = (&self.client, self.channel_id.as_str());
        let mut all = Vec::new();
        let mut page_token: Option<String> = None;
        for _ in 0..MAX_PLAYLIST_PAGES {
            let current = page_token.take();
            let page = self
                .call(|token| {
                    let current = current.clone();
                    async move {
                        client.playlists_list_mine(&token, channel, current.as_deref()).await
                    }
                })
                .await?;
            all.extend(page.items);
            match page.next_page_token {
                Some(next) => page_token = Some(next),
                None => break,
            }
        }
        Ok(all)
    }

    /// Every item of a playlist (`PL…`), and whether every page was read.
    async fn list_items(
        &self,
        playlist_id: &str,
    ) -> Result<(Vec<PlaylistItemSummary>, bool), YtError> {
        let (client, channel) = (&self.client, self.channel_id.as_str());
        let mut all = Vec::new();
        let mut page_token: Option<String> = None;
        for _ in 0..MAX_ITEM_PAGES {
            let current = page_token.take();
            let page = self
                .call(|token| {
                    let current = current.clone();
                    async move {
                        let at = current.as_deref();
                        client.playlist_items_list(&token, channel, playlist_id, at).await
                    }
                })
                .await?;
            all.extend(page.items);
            match page.next_page_token {
                Some(next) => page_token = Some(next),
                None => return Ok((all, true)),
            }
        }
        Ok((all, false))
    }

    /// The videos behind `ids`, 50 to a call. A video missing from the answer is gone.
    async fn videos(&self, ids: &[String]) -> Result<HashMap<String, VideoSummary>, YtError> {
        let (client, channel) = (&self.client, self.channel_id.as_str());
        let mut found = HashMap::with_capacity(ids.len());
        for chunk in ids.chunks(VIDEOS_LIST_BATCH) {
            let videos = self
                .call(|token| async move { client.videos_list(&token, channel, chunk).await })
                .await?;
            found.extend(videos.into_iter().map(|v| (v.id.clone(), v)));
        }
        Ok(found)
    }
}

/// One monitor run's use of the Data API: the reader, the channel's playlists, and whether a
/// failure has ruled the API out for the rest of the run.
pub struct DataApiRun {
    db: Arc<Db>,
    reader: Reader,
    /// By Data API id (`PL…`).
    playlists: HashMap<String, PlaylistSummary>,
    stopped: AtomicBool,
    estimate: i64,
}

impl DataApiRun {
    /// The channel's playlist for an index id (`VLPL…`), while the API is still in use this run.
    pub fn covers(&self, playlist_id: &str) -> Option<&PlaylistSummary> {
        if self.stopped.load(Ordering::Relaxed) {
            return None;
        }
        self.playlists.get(&crate::jobs::planner::ytdata_playlist_id(playlist_id)?)
    }

    /// Reads one playlist the run [`covers`](Self::covers). `known` is the index's copy of it.
    pub async fn read(
        &self,
        playlist: &PlaylistSummary,
        known: &[(String, Option<String>)],
    ) -> Result<DataApiRead, YtError> {
        let (items, complete) = self.reader.list_items(&playlist.id).await?;
        let mut ids: Vec<String> = items.iter().map(|i| i.video_id.clone()).collect();
        ids.sort();
        ids.dedup();
        let videos = self.reader.videos(&ids).await?;
        crate::ytdata_status::note_success(&self.db);
        Ok(to_read(playlist, items, &videos, known, complete))
    }

    /// A read failed: file what it says about the API, and stop using it this run when it rules
    /// the API out.
    pub fn failed(&self, err: &YtError) {
        crate::ytdata_status::note_error(&self.db, err, Utc::now());
        if rules_out_the_run(err) {
            self.stopped.store(true, Ordering::Relaxed);
        }
    }

    pub fn units(&self) -> i64 {
        self.reader.spent()
    }

    /// For the run's `detail_json`.
    pub fn detail(&self) -> Value {
        json!({
            "reader": "ytdata",
            "estimate": self.estimate,
            "spent": self.units(),
            "stopped": self.stopped.load(Ordering::Relaxed),
        })
    }
}

/// The detail of a run the Data API failed to start.
fn error_detail(e: &YtError) -> Value {
    json!({ "reader": "innertube", "why": "error", "error": e.to_string() })
}

/// What a run is about.
#[derive(Debug, Clone, Copy)]
pub enum Scope<'a> {
    All,
    One(&'a str),
}

/// Decides the run's reader and, for the Data API, lists the channel's playlists. Answers the run
/// (or `None` for InnerTube) and what to put in the run's `detail_json`. `backup_run`: a scheduled
/// or headless run (see [`monitor_allowance`]).
pub async fn prepare(
    state: &crate::state::AppState,
    scope: Scope<'_>,
    backup_run: bool,
) -> (Option<DataApiRun>, Value) {
    use tauri::Manager;
    let db = state.db.clone();
    let setting = crate::jobs::engine::playlist_engine(&db);
    if setting == EngineSetting::Innertube {
        return (None, Skip::Setting.detail());
    }
    if let Scope::One(id) = scope {
        if crate::jobs::planner::ytdata_playlist_id(id).is_none() {
            return (None, json!({ "reader": "innertube", "why": "not_addressable" }));
        }
    }
    let status = crate::ytdata_status::current(state).await;
    let now = Utc::now();
    let counts = known_counts(&db);
    let listed = i64::try_from(counts.len()).unwrap_or(i64::MAX).max(1);
    let stats = match scope {
        Scope::All => LibraryStats::of(counts.values().copied()),
        Scope::One(id) => LibraryStats::of([counts.get(id).copied().unwrap_or(0)]),
    };
    let estimate = estimate_units(listed, &stats);
    let allowance = monitor_allowance(&db, now, backup_run);
    if let Err(skip) = choose_reader(setting, status.state, estimate, allowance) {
        tracing::info!(?skip, "monitor: reading through InnerTube");
        return (None, skip.detail());
    }
    let manager = state
        .app
        .try_state::<Arc<crate::jobs::JobsState>>()
        .and_then(|jobs| jobs.account_manager());
    let (Some(manager), Some(channel)) = (manager, status.account) else {
        return (None, Skip::State(DataApiState::NotConfigured).detail());
    };
    let reader = match Reader::new(db.clone(), manager, channel) {
        Ok(reader) => reader,
        Err(e) => return (None, error_detail(&e)),
    };
    match reader.list_playlists().await {
        Ok(listed) => {
            crate::ytdata_status::note_success(&db);
            let playlists = listed.into_iter().map(|p| (p.id.clone(), p)).collect();
            let run =
                DataApiRun { db, reader, playlists, stopped: AtomicBool::new(false), estimate };
            let detail = run.detail();
            (Some(run), detail)
        }
        Err(e) => {
            crate::ytdata_status::note_error(&db, &e, Utc::now());
            tracing::info!(error = %e, "monitor: the Data API could not list playlists");
            (None, error_detail(&e))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use DataApiState::{ApiDisabled, NeedsAuth, NotConfigured, QuotaExhausted};

    fn item(video_id: &str, position: i64, added: Option<&str>) -> PlaylistItemSummary {
        PlaylistItemSummary {
            playlist_item_id: format!("UEx{video_id}{position}"),
            video_id: video_id.into(),
            position,
            title: format!("Item {video_id}"),
            channel_title: Some("Owner - Topic".into()),
            channel_id: Some("UCowner".into()),
            added_at: added.map(|s| DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)),
            video_published_at: None,
            thumbnail_url: Some(format!("https://i.ytimg.com/{video_id}/item.jpg")),
        }
    }

    fn video(id: &str) -> VideoSummary {
        VideoSummary {
            id: id.into(),
            title: Some(format!("Song {id}")),
            channel_id: Some("UCartist".into()),
            channel_title: Some("Artist - Topic".into()),
            duration_s: Some(253),
            published_at: None,
            thumbnail_url: Some(format!("https://i.ytimg.com/{id}/mq.jpg")),
            privacy_status: Some("public".into()),
            upload_status: Some("processed".into()),
            region_restricted: false,
            blocked_regions: Vec::new(),
        }
    }

    fn playlist(item_count: i64, privacy: &str) -> PlaylistSummary {
        PlaylistSummary {
            id: "PL1".into(),
            title: "Mix".into(),
            description: String::new(),
            thumbnail_url: None,
            item_count,
            privacy_status: privacy.into(),
        }
    }

    #[test]
    fn estimate_matches_playlistforges_worked_example() {
        // PF's doc 04: 30 playlists / 8 000 items ≈ 330 u: 1 + 160 + 160.
        assert_eq!(estimate_units(30, &LibraryStats::of([8000])), 321);
        assert_eq!(estimate_units(0, &LibraryStats::default()), 0);
        // Exact multiples need no extra page; one over rounds up.
        assert_eq!(estimate_units(50, &LibraryStats::of([100])), 1 + 2 + 2);
        assert_eq!(estimate_units(51, &LibraryStats::of([1])), 2 + 1 + 1);
        // Per playlist: 30 lists of 10 tracks are 30 pages, not 6; an empty one is still a call.
        assert_eq!(LibraryStats::of([10; 30]).item_pages, 30);
        let empty = LibraryStats { playlists: 2, items: 0, item_pages: 2 };
        assert_eq!(LibraryStats::of([0, -5]), empty);
    }

    #[test]
    fn the_reader_is_the_data_api_only_when_ok_and_affordable() {
        use EngineSetting as S;
        let ok = DataApiState::Ok;
        assert_eq!(choose_reader(S::Auto, ok, 100, 100), Ok(()));
        assert_eq!(choose_reader(S::Ytdata, ok, 0, 0), Ok(()));
        assert_eq!(choose_reader(S::Innertube, ok, 1, 9_999), Err(Skip::Setting));
        for state in [NotConfigured, ApiDisabled, NeedsAuth, QuotaExhausted] {
            assert_eq!(choose_reader(S::Auto, state, 1, 9_999), Err(Skip::State(state)));
        }
        let over = Skip::OverBudget { estimate: 101, allowance: 100 };
        assert_eq!(choose_reader(S::Ytdata, ok, 101, 100), Err(over));
        assert_eq!(over.detail()["why"], "over_budget");
        assert_eq!(Skip::State(NeedsAuth).detail()["why"], "needs_auth");
    }

    #[test]
    fn only_a_backup_run_may_spend_the_reserve() {
        let db = Db::open(std::path::Path::new(":memory:")).unwrap();
        let noon = DateTime::parse_from_rfc3339("2026-07-15T19:00:00Z").unwrap();
        let noon = noon.with_timezone(&Utc);
        crate::jobs::budget::set_backup_reserve_units(&db, 1000);
        crate::jobs::budget::set_opportunistic_mode(&db, false);
        // 10 000 - 300 margin - 1000 reserve.
        assert_eq!(monitor_allowance(&db, noon, false), 8700);
        assert_eq!(monitor_allowance(&db, noon, true), 9700);
        crate::quota::record_units(&db, "playlistItems.insert", 9000, None, None, noon).unwrap();
        assert_eq!(monitor_allowance(&db, noon, false), 0, "a manual sync leaves the reserve be");
        assert_eq!(monitor_allowance(&db, noon, true), 700, "what is left short of the margin");
        // The estimate against that: a backup run of 600 u fits, a manual one does not.
        assert!(choose_reader(EngineSetting::Auto, DataApiState::Ok, 600, 700).is_ok());
        assert!(choose_reader(EngineSetting::Auto, DataApiState::Ok, 600, 0).is_err());
    }

    #[test]
    fn durations_read_as_the_player_shows_them() {
        assert_eq!(format_duration(253), "4:13");
        assert_eq!(format_duration(59), "0:59");
        assert_eq!(format_duration(3723), "1:02:03");
        assert_eq!(format_duration(0), "0:00");
        assert_eq!(format_duration(-3), "0:00");
    }

    #[test]
    fn a_video_maps_to_a_row() {
        let v = video("v1");
        let song = song_from(&item("v1", 0, None), Some(&v));
        assert_eq!(song.video_id, "v1");
        assert_eq!(song.title, "Song v1");
        assert_eq!(song.artists, "Artist", "the Topic suffix dropped");
        assert_eq!(song.artist_id.as_deref(), Some("UCartist"));
        assert_eq!(song.duration.as_deref(), Some("4:13"));
        assert_eq!(song.thumbnail.as_deref(), Some("https://i.ytimg.com/v1/mq.jpg"));
        assert_eq!(song.set_video_id.as_deref(), Some("UExv10"));
        assert!(!song.unavailable && !song.is_video);
        // Someone's own upload is a video.
        let upload = VideoSummary { channel_title: Some("Some Band".into()), ..video("v2") };
        let song = song_from(&item("v2", 1, None), Some(&upload));
        assert!(song.is_video);
        assert_eq!(song.artists, "Some Band");
    }

    #[test]
    fn deleted_private_and_region_restricted_are_unavailable() {
        // Deleted: missing from videos.list; the item's own fields stand in.
        let mut gone = item("d", 0, None);
        gone.title = "Deleted video".into();
        let song = song_from(&gone, None);
        assert!(song.unavailable);
        assert_eq!((song.title.as_str(), song.duration), ("Deleted video", None));
        assert_eq!(song.thumbnail.as_deref(), Some("https://i.ytimg.com/d/item.jpg"));
        assert_eq!(unavailable_reason(None), Some(REASON_DELETED));

        let private = VideoSummary { privacy_status: Some("private".into()), ..video("p") };
        assert_eq!(unavailable_reason(Some(&private)), Some(REASON_PRIVATE));
        assert!(song_from(&item("p", 0, None), Some(&private)).unavailable);

        let rejected = VideoSummary { upload_status: Some("rejected".into()), ..video("r") };
        assert_eq!(unavailable_reason(Some(&rejected)), Some(REASON_DELETED));

        let blocked = VideoSummary {
            region_restricted: true,
            blocked_regions: vec!["DE".into()],
            ..video("b")
        };
        assert_eq!(unavailable_reason(Some(&blocked)), Some(REASON_REGION));
        assert_eq!(unavailable_reason(Some(&video("ok"))), None);
        let unlisted = VideoSummary { privacy_status: Some("unlisted".into()), ..video("u") };
        assert_eq!(unavailable_reason(Some(&unlisted)), None, "unlisted still plays");
    }

    #[test]
    fn a_known_row_keeps_its_innertube_metadata() {
        let known = SongItem {
            video_id: "v1".into(),
            title: "Real Title".into(),
            artists: "Real Artist".into(),
            album: Some("Album".into()),
            duration: Some("4:12".into()),
            ..Default::default()
        };
        let mapped = SongItem { unavailable: true, ..song_from(&item("v1", 0, None), None) };
        let got = merge_known(mapped, Some(known.clone()));
        assert_eq!((got.title.as_str(), got.artists.as_str()), ("Real Title", "Real Artist"));
        assert_eq!((got.album.as_deref(), got.duration.as_deref()), (Some("Album"), Some("4:12")));
        assert_eq!(got.set_video_id.as_deref(), Some("UExv10"), "the handle is this read's");
        assert!(got.unavailable, "whether it plays is this read's");
        assert_eq!(got.thumbnail.as_deref(), Some("https://i.ytimg.com/v1/item.jpg"), "filled in");
        // A placeholder stored before gives way to a real title.
        let stale = SongItem { title: "Private video".into(), ..known };
        let fresh = song_from(&item("v1", 0, None), Some(&video("v1")));
        assert_eq!(merge_known(fresh.clone(), Some(stale)).title, "Song v1");
        assert_eq!(merge_known(fresh.clone(), None), fresh);
    }

    #[test]
    fn a_read_carries_dates_reasons_and_privacy() {
        let items = vec![
            item("b", 1, Some("2024-03-02T10:00:00Z")),
            item("a", 0, Some("2024-01-01T00:00:00Z")),
            item("a", 2, Some("2023-12-31T00:00:00Z")),
            item("gone", 3, None),
        ];
        let videos: HashMap<String, VideoSummary> =
            [("a".to_owned(), video("a")), ("b".to_owned(), video("b"))].into();
        let json = r#"{"video_id":"b","title":"Known B","artists":"K"}"#;
        let known = [("b".to_owned(), Some(json.to_owned()))];
        let got = to_read(&playlist(4, "unlisted"), items, &videos, &known, true);
        let order: Vec<&str> = got.songs.iter().map(|s| s.video_id.as_str()).collect();
        assert_eq!(order, ["a", "b", "a", "gone"], "in playlist order");
        assert_eq!(got.songs[1].title, "Known B");
        assert_eq!(got.added_at["a"], 1_703_980_800, "the earliest of a track listed twice");
        assert_eq!(got.added_at["b"], 1_709_373_600);
        assert!(!got.added_at.contains_key("gone"), "no date, none made up");
        assert_eq!(got.reasons.get("gone").map(String::as_str), Some(REASON_DELETED));
        assert_eq!(got.reasons.len(), 1);
        assert!(got.songs[3].unavailable);
        assert_eq!((got.privacy.as_str(), got.title.as_str(), got.listed), ("unlisted", "Mix", 4));
        assert!(got.complete);
    }

    #[test]
    fn known_counts_take_only_what_the_data_api_can_read() {
        let db = Db::open(std::path::Path::new(":memory:")).unwrap();
        let sync = |n| crate::db::PlaylistSync {
            synced_at: 1,
            item_count: n,
            added: 0,
            removed: 0,
            moved: 0,
            privacy: None,
        };
        db.set_playlist_sync("VLPL1", &sync(120)).unwrap();
        db.set_playlist_sync("VLLM", &sync(900)).unwrap();
        db.set_playlist_sync("VLRDCLAK", &sync(50)).unwrap();
        let counts = known_counts(&db);
        assert_eq!(counts.len(), 1);
        assert_eq!(counts["VLPL1"], 120);
    }

    #[test]
    fn failures_that_rule_out_the_rest_of_the_run() {
        let api = |status: u16, reason: &str| YtError::Api {
            status,
            reason: Some(reason.into()),
            message: String::new(),
        };
        assert!(rules_out_the_run(&api(403, "quotaExceeded")));
        assert!(rules_out_the_run(&api(403, "accessNotConfigured")));
        assert!(rules_out_the_run(&YtError::Auth(AuthError::InvalidGrant)));
        assert!(rules_out_the_run(&YtError::Auth(AuthError::KeyringUnavailable("x".into()))));
        assert!(!rules_out_the_run(&api(404, "playlistNotFound")), "that playlist only");
        assert!(!rules_out_the_run(&api(500, "backendError")));
    }
}
