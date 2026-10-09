//! Tauri commands — the ONLY API the UI calls. context/11 UI contract. No YouTube shapes leak
//! past here; the UI never sees a stream URL.

use std::sync::Arc;

use innertube::{
    AlbumPage, ArtistPage, BrowseItem, HistoryGroup, HomePage, MoodSection, PlaylistContinuation,
    PlaylistPage, PlaylistSort, Rating, SearchResults, SearchSuggestions, SongItem,
};
use serde_json::{json, Value};
use tauri::{Emitter, Manager, State};

use crate::blocked::BlockedArtist;
use crate::playlist_tools::journal::{self, Named, OpRecord, Restore, Summary};
use crate::playlist_tools::monitor::{self, SyncSummary};
use crate::playlist_tools::{build, dedup, everywhere, export, merge, rows, split, transfer};
use crate::state::{
    is_local_playlist, AppState, LOCAL_PLAYLIST_PREFIX, ON_REPEAT_ID, ON_REPEAT_LIMIT,
    ON_REPEAT_WINDOW_SECS,
};

type St<'a> = State<'a, Arc<AppState>>;

/// `record_history`: true only for a query the user submitted. A typeahead preview passes false and
/// goes out unauthenticated, so half-typed prefixes never reach the account's search history (#203).
#[tauri::command]
pub async fn search(
    state: St<'_>,
    query: String,
    record_history: bool,
) -> Result<Vec<SongItem>, String> {
    let client = state.clients.get(innertube::METADATA_CLIENT).ok_or("metadata client missing")?;
    let result =
        state.it.search_songs(client, &query, record_history).await.map_err(|e| e.to_string())?;
    Ok(result.items)
}

/// Search video uploads only: the Videos shelf and its "Show more" page (#209, #266). Never
/// records history, the page's other searches already did.
#[tauri::command]
pub async fn search_videos(state: St<'_>, query: String) -> Result<Vec<SongItem>, String> {
    let client = metadata_client(&state)?;
    let result = state.it.search_videos(client, &query).await.map_err(|e| e.to_string())?;
    Ok(result.items)
}

/// Unfiltered search → categorized sections for the search page. `record_history` as in [`search`].
#[tauri::command]
pub async fn search_all(
    state: St<'_>,
    query: String,
    record_history: bool,
) -> Result<SearchResults, String> {
    let client = metadata_client(&state)?;
    state.it.search_all(client, &query, record_history).await.map_err(|e| e.to_string())
}

/// Typeahead completions + a few matching rows, signed in (see `InnerTube::search_suggestions`).
#[tauri::command]
pub async fn search_suggestions(state: St<'_>, query: String) -> Result<SearchSuggestions, String> {
    let client = metadata_client(&state)?;
    state.it.search_suggestions(client, &query).await.map_err(|e| e.to_string())
}

/// Filtered "Show more" search for one category (albums / artists / playlists).
#[tauri::command]
pub async fn search_cards(
    state: St<'_>,
    query: String,
    category: String,
) -> Result<Vec<BrowseItem>, String> {
    let client = metadata_client(&state)?;
    state.it.search_cards(client, &query, &category).await.map_err(|e| e.to_string())
}

/// Play a track (from a search result). The UI passes the full item so we can seed the queue
/// with its metadata without another round-trip.
#[tauri::command]
pub async fn play(state: St<'_>, item: SongItem) -> Result<(), String> {
    let state = state.inner().clone();
    state.play_song(item).await;
    Ok(())
}

#[tauri::command]
pub async fn play_index(state: St<'_>, index: usize) -> Result<(), String> {
    let state = state.inner().clone();
    state.play_index(index).await;
    Ok(())
}

/// Remove an upcoming track from the queue (not the one playing). Guests are add-only — blocked
/// inside AppState.
#[tauri::command]
pub async fn remove_from_queue(state: St<'_>, index: usize) -> Result<(), String> {
    state.inner().clone().remove_from_queue(index).await;
    Ok(())
}

/// "Play next" from a ⋯ menu: one track or a whole album/playlist, inserted right after the
/// current song (behind any earlier manual adds). `from` is the album/playlist title, which heads
/// the block in the queue panel.
#[tauri::command]
pub async fn play_next(
    state: St<'_>,
    items: Vec<SongItem>,
    from: Option<String>,
) -> Result<(), String> {
    state.inner().clone().play_next(items, from).await;
    Ok(())
}

/// Drag-to-reorder in the queue panel: move the upcoming track at `from` to `to`. Out-of-range or
/// already-played indices are ignored.
#[tauri::command]
pub async fn move_in_queue(state: St<'_>, from: usize, to: usize) -> Result<(), String> {
    state.inner().clone().move_in_queue(from, to).await;
    Ok(())
}

/// "Add to queue": the tracks go at the back of the "Next in queue" block, so they play after
/// everything already queued by hand and ahead of the playing context (and its radio/filler).
/// `from` heads the block in the queue panel; `continuation` is the source page's next-page token —
/// the rest of a long playlist is walked in in the background.
#[tauri::command]
pub async fn add_to_queue(
    state: St<'_>,
    items: Vec<SongItem>,
    from: Option<String>,
    continuation: Option<String>,
) -> Result<(), String> {
    state.inner().clone().add_to_queue(items, from, continuation).await;
    Ok(())
}

/// Clear every upcoming manually-queued track (the queue panel's "Next in queue" section).
#[tauri::command]
pub async fn clear_queued(state: St<'_>) -> Result<(), String> {
    state.inner().clone().clear_queued().await;
    Ok(())
}

#[tauri::command]
pub async fn next_track(state: St<'_>) -> Result<(), String> {
    state.inner().clone().next_in_queue().await;
    Ok(())
}

#[tauri::command]
pub async fn prev_track(state: St<'_>) -> Result<(), String> {
    state.inner().clone().prev_in_queue().await;
    Ok(())
}

/// The queue panel's "Back to …": put back the queue a click replaced, at the track and position
/// it was left at. Previous does this too, but only from the top of a track; this one is the whole
/// point of the line, so it doesn't rewind first.
#[tauri::command]
pub async fn back_to_previous(state: St<'_>) -> Result<(), String> {
    state.inner().clone().restore_prev_context().await;
    Ok(())
}

#[tauri::command]
pub async fn toggle_shuffle(state: St<'_>) -> Result<(), String> {
    state.inner().clone().toggle_shuffle().await;
    Ok(())
}

/// `mode` ∈ "off" | "all" | "one".
#[tauri::command]
pub async fn set_repeat(state: St<'_>, mode: String) -> Result<(), String> {
    let mode = match mode.as_str() {
        "off" => crate::state::RepeatMode::Off,
        "all" => crate::state::RepeatMode::All,
        "one" => crate::state::RepeatMode::One,
        other => return Err(format!("unknown repeat mode: {other}")),
    };
    state.inner().clone().set_repeat(mode).await;
    Ok(())
}

#[tauri::command]
pub async fn toggle_pause(state: St<'_>) -> Result<(), String> {
    let state = state.inner().clone();
    state.resume_or_toggle().await;
    Ok(())
}

#[tauri::command]
pub async fn seek(state: St<'_>, position: f64) -> Result<(), String> {
    // Routed through AppState so a Listen Together host broadcasts the seek and a guest is blocked.
    state.user_seek(position).await
}

#[tauri::command]
pub async fn set_volume(state: St<'_>, volume: i64) -> Result<(), String> {
    state.player.set_volume(volume).map_err(|e| e.to_string())?;
    if volume > 0 {
        crate::hotkeys::LAST_NONZERO_VOLUME.store(volume, std::sync::atomic::Ordering::Relaxed);
    }
    // There is one volume and there can be two windows (the mini player). Without this the one
    // that didn't move the slider keeps showing the old level and lies about what you're hearing.
    let _ = state.app.emit("volume", volume);
    state.media_set_volume(volume);
    Ok(())
}

/// Tempo (0.25–2.0) and pitch (−12..=12 semitones), the "Advanced" dialog. Volatile by design:
/// both reset to 1.0 / 0 on restart, so nobody wonders next week why everything sounds wrong.
#[tauri::command]
pub async fn set_playback_params(state: St<'_>, speed: f64, semitones: i32) -> Result<(), String> {
    // Pitch first: it's the one that can fail (no librubberband), and it rolls itself back, so a
    // failure leaves nothing applied and the UI can revert both steppers together.
    state.player.set_pitch(semitones).map_err(|e| e.to_string())?;
    state.player.set_speed(speed).map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn get_queue(state: St<'_>) -> Result<serde_json::Value, String> {
    Ok(state.queue_snapshot().await)
}

/// Settings the UI is allowed to read *and write*. Session/auth material (`session_cookie`,
/// `selected_identity_json`, `data_sync_id`, `account_json`, `account_selection_pending`,
/// `visitor_data`) and internal blobs (`queue_json`, `queue_index`, `queue_position`) never cross
/// into the webview: they'd otherwise ship the login credential to the renderer on every open, and
/// the webview can't overwrite them either.
const UI_SETTINGS: &[&str] = &[
    "volume",
    "proxy",
    "quality",
    "normalize_volume",
    "enable_history",
    "disabled_stream_clients",
    "discord_rpc",
    "discord_rpc_config",
    "close_to_tray",
    "track_notifications",
    "autostart",
    "start_minimized",
    "autoplay",
    "hide_videos",
    "prevent_duplicates",
    "update_banner",
    "update_channel",
    "lyrics_providers",
    "music_videos",
    // The music video in the mini player too (mini.rs, MiniPlayer.svelte). Only on top of
    // `music_videos`: see `setting_prerequisite`.
    "mini_video",
    "ambient_light",
    "sticky_shuffle",
    "shuffle_whole_queue",
    "system_titlebar",
    "lastfm_primary_artist",
    "lastfm_primary_strict",
    "lastfm_config",
    "crossfade",
    "crossfade_secs",
    "locale",
    "drop_mode",
    "drop_dupes",
    // The first-run import prompt's answer and the sources it covered (D8, onboarding.ts).
    "onboarding_import_prompted",
    // Hours between playlist syncs: 0 (off), 1, 3, 6, 12 or 24 (monitor_interval_secs).
    "monitor_interval_hours",
    // Snapshot backups (backups.rs): the folder, and how many each playlist keeps. PF's keys.
    "monitor.backups_dir",
    "retention_keep_last",
    // The Library's playlists tab: `default|title|count|synced` (`:desc` reverses) and `grid|list`
    // (plsort.ts).
    "library_playlists_sort",
    "library_playlists_view",
    // Tools ▸ Extract into a new playlist: `build` (one undoable build, the default) or
    // `create_transfer` (make the playlist, then copy or move into it).
    "tools.extract_new_mode",
    // Downloads (download/settings.rs): PF's keys, plus where the cookies come from
    // (`session|none`) and which yt-dlp builds to install (`stable|nightly`).
    "downloads.dir",
    "downloads.default_format",
    "downloads.audio_quality",
    "downloads.video_quality",
    "downloads.thumbnail_mode",
    "downloads.cookies",
    "downloads.ytdlp_channel",
    // The job queue (jobs/): which engine writes playlists (`auto|ytdata|innertube`), whether
    // InnerTube writes are queued too (`unified|ytdata_only`), the day's Data API budget, the
    // priority a new job gets, the local echo's window (seconds; 0 off, -1 always) and whether a
    // headless run advances the queue.
    "playlist_engine",
    "job_queue_mode",
    "budget.safety_margin_percent",
    "budget.backup_reserve_units",
    "budget.opportunistic_mode",
    "budget.backup_runs_per_day",
    "budget.daily_units",
    "jobs.default_job_priority",
    "jobs.local_echo_max_age_s",
    "jobs.advance_jobs_headless",
];

/// What a setting accepts. Keys without a rule take any string, as every key did before.
#[derive(Debug, Clone)]
enum SettingRule {
    OneOf(&'static [&'static str]),
    /// A whole number in the range.
    Int(std::ops::RangeInclusive<i64>),
    /// `true` or `false`.
    Bool,
    /// One of the local echo slider's positions (`jobs::local_echo::EchoWindow::SLIDER`).
    EchoSlider,
}

/// Per-key validation for [`set_setting`]: a value its reader would misread (an engine it does not
/// know, a margin of 400 %) is refused with a message, instead of stored and silently ignored.
fn setting_rule(key: &str) -> Option<SettingRule> {
    use crate::jobs::budget as b;
    use crate::jobs::engine as e;
    use SettingRule::{Bool, EchoSlider, Int, OneOf};
    Some(match key {
        "playlist_engine" => OneOf(&e::PLAYLIST_ENGINE_VALUES),
        "job_queue_mode" => OneOf(&e::JOB_QUEUE_MODE_VALUES),
        "budget.daily_units" => Int(b::DAILY_UNITS_RANGE),
        "budget.safety_margin_percent" => Int(b::SAFETY_MARGIN_PERCENT_RANGE),
        "budget.backup_reserve_units" => Int(b::BACKUP_RESERVE_UNITS_RANGE),
        "budget.backup_runs_per_day" => Int(b::BACKUP_RUNS_PER_DAY_RANGE),
        "budget.opportunistic_mode" => Bool,
        "jobs.default_job_priority" => Int(crate::jobs::PRIORITY_HIGH..=crate::jobs::PRIORITY_LOW),
        "jobs.local_echo_max_age_s" => EchoSlider,
        "jobs.advance_jobs_headless" => Bool,
        "drop_mode" => OneOf(&["ask", "copy", "move"]),
        "drop_dupes" => OneOf(&["skip", "allow", "consolidate"]),
        "tools.extract_new_mode" => OneOf(&["build", "create_transfer"]),
        _ => return None,
    })
}

/// The local echo slider's positions as `jobs.local_echo_max_age_s` stores them.
fn echo_slider_values() -> Vec<String> {
    crate::jobs::local_echo::EchoWindow::SLIDER.iter().map(|w| w.to_setting_value()).collect()
}

pub(crate) fn validate_setting(key: &str, value: &str) -> Result<(), String> {
    let Some(rule) = setting_rule(key) else { return Ok(()) };
    let ok = match &rule {
        SettingRule::OneOf(allowed) => allowed.contains(&value),
        SettingRule::Int(range) => value.parse::<i64>().is_ok_and(|n| range.contains(&n)),
        SettingRule::Bool => matches!(value, "true" | "false"),
        SettingRule::EchoSlider => echo_slider_values().iter().any(|v| v == value),
    };
    if ok {
        return Ok(());
    }
    let expected = match rule {
        SettingRule::OneOf(allowed) => format!("one of {}", allowed.join(", ")),
        SettingRule::Int(range) => {
            format!("a whole number from {} to {}", range.start(), range.end())
        }
        SettingRule::Bool => "true or false".to_string(),
        SettingRule::EchoSlider => format!("one of {}", echo_slider_values().join(", ")),
    };
    Err(format!("invalid value {value:?} for {key}: expected {expected}"))
}

/// A setting that only means something on top of another one, checked against what is stored now
/// (`stored` reads a key). `mini_video` plays the music video in the mini player, so it cannot be
/// switched on while `music_videos` is off: the UI greys the toggle, and this refuses it with a code
/// the UI can name. Turning it off is always allowed, and so is turning `music_videos` off under it:
/// every reader takes both (mini.rs, `initApp`), so a stale `mini_video=true` does nothing.
fn setting_prerequisite(
    key: &str,
    value: &str,
    stored: impl Fn(&str) -> Option<String>,
) -> Result<(), String> {
    if key == "mini_video"
        && value == "true"
        && stored("music_videos").as_deref() != Some("true")
    {
        return Err("needs_music_videos".into());
    }
    Ok(())
}

/// Resolve the music video for `video_id` and hand back a `limusicvideo://` URL the player view
/// can put in a `<video src>`. `None` when YouTube has no usable video stream for it, which is the
/// ordinary answer for a song and leaves the artwork in place. The real googlevideo URL never
/// leaves Rust (context/11).
#[tauri::command]
pub async fn video_stream(
    state: St<'_>,
    video_id: String,
    max_height: i32,
) -> Result<Option<String>, String> {
    if crate::local::is_local_song(&video_id) {
        return Ok(None);
    }
    // Already resolved and still live: the loopback URL is a pure function of the videoId, so
    // there is nothing left to do. Re-resolving costs up to two `/player` round trips, and the
    // player view pays them on every reopen otherwise.
    if state.video_url(&video_id).is_some() {
        return Ok(crate::videoproxy::url_for(&video_id));
    }
    // The webview picks the height from its own box, so clamp it here rather than trusting it.
    let max_height = max_height.clamp(144, 1080);
    match state.orchestrator.resolve_video(&video_id, max_height, &state.disabled_clients()).await {
        Some(url) => {
            state.put_video_url(&video_id, url);
            Ok(crate::videoproxy::url_for(&video_id))
        }
        None => Ok(None),
    }
}

/// The glow around the music video, where mpv draws the picture (Linux): the newest small frame
/// other than `after` (nativevideo.rs has the layout), or nothing if none came within a quarter
/// second. Raw bytes, so the ~22 KB a frame skips JSON both ways.
#[tauri::command]
pub async fn ambient_frame(after: u32) -> tauri::ipc::Response {
    #[cfg(any(target_os = "linux", windows))]
    let frame = crate::nativevideo::next_frame(after).await.map(|f| f.to_vec());
    // Typed: on macOS a bare `None` leaves nothing to infer from, and the PR checks only build on
    // Linux, so this broke rc.3's Windows and macOS builds with every check green.
    #[cfg(not(any(target_os = "linux", windows)))]
    let frame: Option<Vec<u8>> = {
        let _ = after;
        None
    };
    tauri::ipc::Response::new(frame.unwrap_or_default())
}

/// Where the page's hole for the music video is (`[x, y, w, h]`, CSS pixels, viewport-relative),
/// or `None` when it has none. mpv draws the picture there, underneath the webview
/// (nativevideo.rs, nativevideo_windows.rs). `false` means no picture is up, and for a rect that
/// there never will be: the page falls back to the `<video>` element. `dpr` is the page's
/// `devicePixelRatio`, which Windows needs for the page zoom; Linux reads the zoom off the webview.
#[tauri::command]
pub async fn native_video_rect(
    app: tauri::AppHandle,
    state: St<'_>,
    rect: Option<[f64; 4]>,
    dpr: Option<f64>,
) -> Result<bool, String> {
    #[cfg(target_os = "linux")]
    {
        let _ = dpr;
        Ok(crate::nativevideo::set_rect(&app, state.inner().clone(), rect).await)
    }
    #[cfg(windows)]
    return Ok(
        crate::nativevideo::set_rect(&app, state.inner().clone(), rect, dpr.unwrap_or(1.0)).await
    );
    #[cfg(not(any(target_os = "linux", windows)))]
    {
        let _ = (app, state, rect, dpr);
        Ok(false)
    }
}

/// Forget a resolved music-video URL, so the next `video_stream` for this id resolves a fresh one.
/// The player view calls this when the `<video>` element fails to load, which is what an expired
/// or revoked googlevideo link looks like from the webview. It also sends that track's next resolve
/// to VISIONOS first, so a WEB_REMIX URL that failed is not rebuilt from the same reply.
#[tauri::command]
pub async fn forget_video_stream(state: St<'_>, video_id: String) -> Result<(), String> {
    state.forget_video_url(&video_id);
    state.orchestrator.mark_video_failed(&video_id);
    Ok(())
}

/// Who draws the window frame, as the SPA needs to know it (issue #65). Read-only, derived: the
/// stored `system_titlebar` preference on Linux/Windows, and always `overlay` on macOS, where the
/// traffic lights come from `tauri.macos.conf.json`'s `titleBarStyle: Overlay` and there is no
/// preference to make. Anything but `off` means the app hides its own window buttons, drops its
/// corner rounding and leaves resizing to the compositor.
fn native_chrome(db: &crate::db::Db) -> &'static str {
    if cfg!(target_os = "macos") {
        "overlay"
    } else if db.get_setting("system_titlebar").as_deref() == Some("true") {
        "on"
    } else {
        "off"
    }
}

#[tauri::command]
pub async fn get_settings(state: St<'_>) -> Result<serde_json::Value, String> {
    let mut map: serde_json::Map<String, serde_json::Value> = state
        .db
        .all_settings()
        .into_iter()
        .filter(|(k, _)| UI_SETTINGS.contains(&k.as_str()))
        .map(|(k, v)| (k, serde_json::Value::String(v)))
        .collect();
    map.insert("native_chrome".into(), native_chrome(&state.db).into());
    map.insert("native_video".into(), crate::state::native_video().to_string().into());
    // Whether this build has a Discord application id (D4): without one the Discord tab and the
    // titlebar toggle say so and stay disabled.
    map.insert("discord_available".into(), crate::discord::available().to_string().into());
    Ok(serde_json::Value::Object(map))
}

/// Why the database's schema upgrade failed at startup, or `None`. The app runs at the old
/// schema then (the monitor, backups and downloads may not work); the UI warns about it once.
#[tauri::command]
pub fn db_migration_error(state: St<'_>) -> Option<String> {
    state.db.migration_error()
}

#[tauri::command]
pub async fn set_setting(
    app: tauri::AppHandle,
    state: St<'_>,
    key: String,
    value: String,
) -> Result<(), String> {
    if !UI_SETTINGS.contains(&key.as_str()) {
        return Err(format!("unknown setting: {key}"));
    }
    validate_setting(&key, &value)?;
    setting_prerequisite(&key, &value, |k| state.db.get_setting(k))?;
    // Registers/removes the login autostart entry on toggle; the OS persists it from there, and
    // startup repoints an existing entry at the running binary (lib.rs). Before the write, so a
    // failure leaves the setting as it was. A dev build would register itself, and at login its
    // window needs a vite server that isn't running.
    if key == "autostart" {
        // A portable copy installs nothing, and a login entry pointing into a folder that moves
        // (or a stick that is not plugged in) would be worse than none. The UI greys the toggle.
        if crate::paths::is_portable() {
            return Err("portable".into());
        }
        if tauri::is_dev() {
            return Err(
                "autostart: a dev build can't register itself, use an installed build".into()
            );
        }
        use tauri_plugin_autostart::ManagerExt;
        let al = app.autolaunch();
        let res = if value == "true" {
            al.enable()
        } else if al.is_enabled().unwrap_or(false) {
            al.disable()
        } else {
            Ok(())
        };
        res.map_err(|e| format!("autostart: {e}"))?;
    }
    state.db.set_setting(&key, &value);
    // A music video track already playing gets its picture now rather than from the next track.
    if key == "music_videos" && value == "true" {
        state.inner().attach_current_video().await;
    }
    #[cfg(target_os = "linux")]
    if key == "ambient_light" {
        crate::set_webgl(&app, value == "true");
    }
    // Presence connects/clears the moment it's toggled — the user shouldn't have to skip a track
    // to see it take effect.
    if key == "discord_rpc" {
        state.set_discord_enabled(value == "true");
    }
    // Same reasoning for the card layout: the settings tab previews it live, so the real card has
    // to follow without waiting for the next track.
    if key == "discord_rpc_config" {
        state.set_discord_config(&value);
    }
    // The Scrobbling tab's settings, read whole each time: the two switches from #231 keep rows of
    // their own, and the scrobbler wants all of it in one message.
    if matches!(key.as_str(), "lastfm_config" | "lastfm_primary_artist" | "lastfm_primary_strict") {
        state.lastfm.set_config(crate::lastfm::ScrobbleConfig::load(&state.db));
    }
    // Retune the track that's playing. Unlike crossfade below, this one has to apply to what the
    // user is hearing right now: the switch exists so they can A/B the same loud section (#298).
    if key == "normalize_volume" {
        state.reapply_gain().await;
    }
    // The queue panel's switch sits right above the tracks it adds, so they come and go with it.
    // Spawned: turning it on can mean a radio fetch, and the switch shouldn't wait on the network.
    if key == "autoplay" {
        let (state, on) = (state.inner().clone(), value != "false");
        tauri::async_runtime::spawn(async move { state.autoplay_changed(on).await });
    }
    // Both halves are one player setting. Applies from the next track change: the transition the
    // user is already hearing keeps the length it started with.
    if key == "crossfade" || key == "crossfade_secs" {
        state.apply_crossfade().await;
    }
    // The language YouTube answers in (#274). The SPA writes it whenever the two disagree, which is
    // also how a fresh install's language gets here at all. It drops its own browse cache and
    // remounts the route afterwards, so what is already on screen follows without a restart.
    if key == "locale" {
        state.it.set_locale(&value);
    }
    // Applies to what's fetched from here on: the live queue keeps whatever is already in it.
    if key == "hide_videos" {
        state.it.set_hide_videos(value == "true");
    }
    // Cached lyrics outlive the order that produced them, so a reorder would otherwise reach only
    // songs never played. Songs whose source was picked by hand keep it.
    if key == "lyrics_providers" {
        state.db.clear_lyrics_cache();
    }
    // The daily units and the margin decide when the quota reads as used up.
    if key.starts_with("budget.") {
        crate::ytdata_status::announce(&state).await;
    }
    // Hand the frame back to the compositor (or take it again). macOS is not on this path: its
    // titlebar style is fixed at window creation, so the setting is hidden there.
    #[cfg(not(target_os = "macos"))]
    if key == "system_titlebar" {
        use tauri::Manager;
        if let Some(w) = app.get_webview_window("main") {
            w.set_decorations(value == "true").map_err(|e| format!("decorations: {e}"))?;
        }
    }
    Ok(())
}

#[tauri::command]
pub async fn get_global_hotkeys(
    hotkeys: State<'_, Arc<crate::hotkeys::HotkeysManager>>,
) -> Result<crate::hotkeys::HotkeysConfig, String> {
    Ok(hotkeys.get_config())
}

/// A Wayland session, where the X11 grab the hotkeys use only fires if the compositor passes keys
/// on to XWayland (KDE Plasma does, GNOME does not). The GDK backend doesn't matter, the session does.
#[tauri::command]
pub fn global_hotkeys_on_wayland() -> bool {
    cfg!(target_os = "linux") && std::env::var_os("WAYLAND_DISPLAY").is_some()
}

#[tauri::command]
pub async fn set_global_hotkeys(
    app: tauri::AppHandle,
    state: St<'_>,
    hotkeys: State<'_, Arc<crate::hotkeys::HotkeysManager>>,
    config: crate::hotkeys::HotkeysConfig,
) -> Result<crate::hotkeys::HotkeyRegisterResult, String> {
    let result = hotkeys.apply_config(&app, config);
    // Saved even on partial failure: apply_config already made this the live config, and a
    // combo another app holds shouldn't cost the user every other binding on the next launch.
    crate::hotkeys::save_config(&state.db, &result.config);
    Ok(result)
}

#[tauri::command]
pub async fn reset_global_hotkeys(
    app: tauri::AppHandle,
    state: St<'_>,
    hotkeys: State<'_, Arc<crate::hotkeys::HotkeysManager>>,
) -> Result<crate::hotkeys::HotkeyRegisterResult, String> {
    // Resets the bindings only: the default has hotkeys off, and the button is on the enabled page.
    let default_config = crate::hotkeys::HotkeysConfig {
        enabled: hotkeys.get_config().enabled,
        ..Default::default()
    };
    let result = hotkeys.apply_config(&app, default_config);
    crate::hotkeys::save_config(&state.db, &result.config);
    Ok(result)
}

/// The streamable client keys the orchestrator tries, for the "disabled clients" setting. Names
/// come from the innertube crate so the UI stays free of YouTube-shaped identity strings.
#[tauri::command]
pub async fn get_stream_clients() -> Result<Vec<String>, String> {
    let mut v = vec![innertube::MAIN_CLIENT.to_string()];
    v.extend(innertube::STREAM_FALLBACK_ORDER.iter().map(|s| s.to_string()));
    Ok(v)
}

/// Let the webview fetch one font file the user picked in the Themes tab, so a `@font-face` can
/// point at it.
///
/// Same runtime-scope trick as local artwork (`local::allow_covers`): the static asset scope stays
/// empty, and only the exact file gets a URL. The extension check keeps the command from being a
/// general "give the page a URL for any path on this machine" — today only the main window holds a
/// capability to call commands at all, and this stays safe if that ever widens.
#[tauri::command]
pub async fn allow_font_file(app: tauri::AppHandle, path: String) -> Result<(), String> {
    use tauri::Manager;
    const FONT_EXTS: [&str; 4] = ["ttf", "otf", "woff", "woff2"];
    let p = std::path::Path::new(&path);
    let ext = p.extension().and_then(|e| e.to_str()).unwrap_or_default().to_ascii_lowercase();
    if !FONT_EXTS.contains(&ext.as_str()) {
        return Err(format!("not a font file: {path}"));
    }
    // Scope grants succeed for paths that don't exist, so check here: this failing is how the UI
    // learns a loaded font was deleted or moved, and drops it instead of listing a dead entry.
    if !p.is_file() {
        return Err(format!("font file not found: {path}"));
    }
    let scope = app.asset_protocol_scope();
    scope.allow_file(&path).map_err(|e| e.to_string())?;
    // The scope check canonicalizes what it is asked about, so a font reached through a symlinked
    // folder needs the real path allowed too (see local::allow_covers).
    if let Ok(real) = p.canonicalize() {
        let _ = scope.allow_file(real);
    }
    Ok(())
}

// --- app icon (#173) -------------------------------------------------------------------------

/// Set the app icon to a PNG the user picked, or clear it back to the bundled one with `None`.
/// The file is copied rather than referenced (see appicon.rs).
#[tauri::command]
pub async fn set_app_icon(app: tauri::AppHandle, path: Option<String>) -> Result<(), String> {
    let dest = crate::appicon::path(&app).ok_or("no app data directory")?;
    match path {
        Some(src) => {
            // `from_path` reads and decodes the whole file, so bound it first: a compressed
            // 16 MB of uniform 8192x8192 still unpacks into 256 MB before anything can reject it.
            // The signature and IHDR are the first 24 bytes of any PNG.
            let len =
                std::fs::metadata(&src).map_err(|e| format!("couldn't read that PNG: {e}"))?.len();
            if len > 16 * 1024 * 1024 {
                return Err("that file is too big; use a PNG under 16 MB".into());
            }
            let mut head = [0u8; 24];
            {
                use std::io::Read;
                std::fs::File::open(&src)
                    .and_then(|mut f| f.read_exact(&mut head))
                    .map_err(|e| format!("couldn't read that PNG: {e}"))?;
            }
            if head[..8] != *b"\x89PNG\r\n\x1a\n" {
                return Err("that file isn't a PNG".into());
            }
            let width = u32::from_be_bytes([head[16], head[17], head[18], head[19]]);
            let height = u32::from_be_bytes([head[20], head[21], head[22], head[23]]);
            // Nothing draws an icon above 256px, and every launch would pay to decode whatever
            // was picked: a 5000px photo is a 100 MB buffer for a 16px titlebar logo.
            if width > 1024 || height > 1024 {
                return Err(format!(
                    "that image is {width}x{height}; use one no larger than 1024x1024"
                ));
            }
            // Decoding it here is the validation, now bounded to 1024x1024. Storing a file the
            // icon loader can't read would fail silently at every launch instead, with the old
            // icon still showing.
            tauri::image::Image::from_path(&src)
                .map_err(|e| format!("couldn't read that PNG: {e}"))?;
            if let Some(dir) = dest.parent() {
                std::fs::create_dir_all(dir).map_err(|e| format!("couldn't save the icon: {e}"))?;
            }
            // Copy beside the destination and rename over it. `fs::copy` truncates the old file
            // before writing, so a failure part way through would leave a half-written PNG that
            // every later launch still treats as the custom icon.
            let tmp = dest.with_extension("png.tmp");
            std::fs::copy(&src, &tmp).and_then(|_| std::fs::rename(&tmp, &dest)).map_err(|e| {
                let _ = std::fs::remove_file(&tmp);
                format!("couldn't save the icon: {e}")
            })?;
        }
        None => match std::fs::remove_file(&dest) {
            Ok(()) => {}
            // Already gone is the state the caller asked for; anything else means the custom icon
            // is still on disk and `apply` below would just put it back.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(format!("couldn't remove the icon: {e}")),
        },
    }
    crate::appicon::apply(&app);
    Ok(())
}

/// The custom icon's path, granted to the asset protocol so the in-app logo can render it. `None`
/// when the bundled icon is in use.
#[tauri::command]
pub async fn app_icon_path(app: tauri::AppHandle) -> Result<Option<String>, String> {
    use tauri::Manager;
    let Some(p) = crate::appicon::custom_path(&app) else { return Ok(None) };
    let scope = app.asset_protocol_scope();
    scope.allow_file(&p).map_err(|e| e.to_string())?;
    // Same symlink dance as the font grant: the scope check canonicalizes what it is asked about.
    if let Ok(real) = p.canonicalize() {
        let _ = scope.allow_file(real);
    }
    Ok(Some(p.to_string_lossy().into_owned()))
}

/// Wipe both cache tiers (URL cache + mpv on-disk audio cache). context/14.
#[tauri::command]
pub async fn clear_caches(state: St<'_>) -> Result<(), String> {
    state.clear_caches();
    Ok(())
}

// --- auth (context/15) ---------------------------------------------------------------------

#[tauri::command]
pub async fn get_account(state: St<'_>) -> Result<serde_json::Value, String> {
    Ok(state.account_snapshot())
}

#[tauri::command]
pub async fn get_account_identities(state: St<'_>) -> Result<Vec<serde_json::Value>, String> {
    state.account_identities().await
}

#[tauri::command]
pub async fn switch_account(
    state: St<'_>,
    selection_key: String,
) -> Result<serde_json::Value, String> {
    state.switch_account(&selection_key).await
}

#[tauri::command]
pub async fn sign_out(state: St<'_>) -> Result<(), String> {
    let state = state.inner().clone();
    state.sign_out().await;
    Ok(())
}

// --- saved Google accounts (multi-account, context/15) ---------------------------------------

/// Saved Google accounts for the account menu. Display fields only — cookies, delegated ids and
/// visitorData never cross the Tauri boundary.
#[tauri::command]
pub async fn get_google_accounts(state: St<'_>) -> Result<Vec<serde_json::Value>, String> {
    Ok(state.google_accounts())
}

/// Activate a saved Google account without a Google re-login. Fails if the stored cookie no
/// longer authenticates, in which case the caller should re-sign-in with that account.
#[tauri::command]
pub async fn switch_google_account(state: St<'_>, id: String) -> Result<serde_json::Value, String> {
    state.switch_google_account(&id).await
}

/// Delete a saved account. Removing the active one signs out; the others stay listed.
#[tauri::command]
pub async fn remove_google_account(state: St<'_>, id: String) -> Result<(), String> {
    state.remove_google_account(&id).await;
    Ok(())
}

/// Open the in-app Google sign-in webview (context/15 Path A). Completes asynchronously; the UI
/// hears back via `auth-changed` (success) or `login-error`. With `add_account`, the webview
/// opens Google's AddSession screen so a second account can be added even when the webview
/// already holds a Google session.
#[tauri::command]
pub async fn login_webview(state: St<'_>, add_account: Option<bool>) -> Result<(), String> {
    let state = state.inner().clone();
    let app = state.app.clone();
    crate::session::open_login(app, state, add_account.unwrap_or(false));
    Ok(())
}

/// The current track, play state, position and duration in one shot. Events are the normal
/// channel; this is for a webview that started after them (the mini player, or the main window
/// on a cold start, where the queue is restored before the UI subscribes).
#[tauri::command]
pub async fn get_playback(state: St<'_>) -> Result<serde_json::Value, String> {
    Ok(state.playback_snapshot().await)
}

// --- mini player (mini.rs) ------------------------------------------------------------------

/// Swap the app for the floating widget: the main window hides to the tray behind it.
#[tauri::command]
pub async fn open_mini(app: tauri::AppHandle) -> Result<(), String> {
    // GTK wants window creation on the main thread, so hop and post the result back rather than
    // logging a failure the user would only see as a click that did nothing.
    let (tx, rx) = tokio::sync::oneshot::channel();
    let handle = app.clone();
    app.run_on_main_thread(move || {
        let _ = tx.send(crate::mini::open(&handle));
    })
    .map_err(|e| e.to_string())?;
    rx.await.map_err(|_| "the mini player never answered".to_string())?
}

/// Swap back. Same path as the tray, so the widget and the tray can't disagree about what
/// "show Limusic" means.
#[tauri::command]
pub async fn close_mini(app: tauri::AppHandle) -> Result<(), String> {
    crate::tray::show_main(&app);
    Ok(())
}

/// The widget's shrink/expand button (#301).
#[tauri::command]
pub async fn set_mini_compact(app: tauri::AppHandle, compact: bool) -> Result<(), String> {
    crate::mini::set_compact(&app, compact)
}

/// The arguments this process was launched with, handed over once (#348). See `LAUNCH_ARGS`.
#[tauri::command]
pub fn take_launch_args() -> Vec<String> {
    std::mem::take(&mut *crate::LAUNCH_ARGS.lock().unwrap())
}

#[tauri::command]
pub async fn show_main(state: St<'_>, window: tauri::WebviewWindow) -> Result<bool, String> {
    if crate::should_start_minimized(&state.db) {
        return Ok(false);
    }
    window.show().map_err(|e| e.to_string())?;
    window.unminimize().map_err(|e| e.to_string())?;
    window.set_focus().map_err(|e| e.to_string())?;
    crate::tray::set_main_visible(window.app_handle(), true);
    Ok(true)
}

// --- browse / library (context/08) ---------------------------------------------------------

pub(crate) fn metadata_client(state: &Arc<AppState>) -> Result<&innertube::YouTubeClient, String> {
    state.clients.get(innertube::METADATA_CLIENT).ok_or_else(|| "metadata client missing".into())
}

#[tauri::command]
pub async fn get_home(state: St<'_>, params: Option<String>) -> Result<HomePage, String> {
    let client = metadata_client(&state)?;
    state.it.home(client, params.as_deref()).await.map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn get_home_more(state: St<'_>, token: String) -> Result<HomePage, String> {
    let client = metadata_client(&state)?;
    state.it.home_continuation(client, &token).await.map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn get_library(state: St<'_>) -> Result<Vec<BrowseItem>, String> {
    let client = metadata_client(&state)?;
    // Signed out there is no YouTube library to ask for (the browse would come back as a sign-in
    // shell), but On Repeat is built from this machine's play history and is still real.
    let mut items = if state.it.is_logged_in() {
        state.it.library_playlists(client).await.map_err(|e| e.to_string())?
    } else {
        Vec::new()
    };
    // On Repeat leads the library once there's anything in it. Hidden while empty rather than
    // shown as a dead tile on a fresh install.
    let songs = on_repeat_songs(&state);
    if !songs.is_empty() {
        items.insert(
            0,
            BrowseItem {
                kind: "playlist",
                id: ON_REPEAT_ID.into(),
                title: "On Repeat".into(),
                subtitle: Some(format!("{} songs", songs.len())),
                thumbnail: None, // the UI draws an icon cover for this one
                duration: None,
                album_id: None,
                artist_runs: Vec::new(),
                play_count: None,
                is_video: false,
                is_upload: false,
                explicit: false,
            },
        );
    }
    // The playlists on this machine next, signed in or not: they belong to no account.
    let at = usize::from(items.first().is_some_and(|i| i.id == ON_REPEAT_ID));
    let local: Vec<BrowseItem> =
        state.db.local_playlists().iter().map(|p| local_playlist_card(&state, p)).collect();
    items.splice(at..at, local);
    // A card has nowhere to put two images, so a custom cover simply is the artwork here.
    for item in &mut items {
        if let Some(cover) = custom_cover(&state, &item.id) {
            item.thumbnail = Some(cover);
        }
    }
    Ok(items)
}

/// Empty rather than an error when signed out: the Library page merges the user's local saves into
/// these grids, so "nothing of yours on YouTube" is an answer, not a failure.
#[tauri::command]
pub async fn get_library_albums(state: St<'_>) -> Result<Vec<BrowseItem>, String> {
    if !state.it.is_logged_in() {
        return Ok(Vec::new());
    }
    let client = metadata_client(&state)?;
    state.it.library_albums(client).await.map_err(|e| e.to_string())
}

/// The user's own uploaded albums (Library ▸ Uploads ▸ Albums).
#[tauri::command]
pub async fn get_upload_albums(state: St<'_>) -> Result<Vec<BrowseItem>, String> {
    if !state.it.is_logged_in() {
        return Ok(Vec::new());
    }
    let client = metadata_client(&state)?;
    state.it.upload_albums(client).await.map_err(|e| e.to_string())
}

/// The account's YouTube Music play history, grouped by day. Empty when signed out, same as the
/// library grids: history lives on the account, and there is nothing to fail about not having one.
#[tauri::command]
pub async fn get_history(state: St<'_>) -> Result<Vec<HistoryGroup>, String> {
    if !state.it.is_logged_in() {
        return Ok(Vec::new());
    }
    let client = metadata_client(&state)?;
    state.it.history(client).await.map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn get_library_artists(state: St<'_>) -> Result<Vec<BrowseItem>, String> {
    if !state.it.is_logged_in() {
        return Ok(Vec::new());
    }
    let client = metadata_client(&state)?;
    state.it.library_artists(client).await.map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn get_library_subscriptions(state: St<'_>) -> Result<Vec<BrowseItem>, String> {
    if !state.it.is_logged_in() {
        return Ok(Vec::new());
    }
    let client = metadata_client(&state)?;
    state.it.library_subscriptions(client).await.map_err(|e| e.to_string())
}

/// A playlist or album page. `id` is the browseId (`VL…` / `MPRE…`); Liked Songs is `VLLM`, and
/// `LIMUSIC_ON_REPEAT` is the local auto-playlist rather than anything YouTube knows about.
///
/// `sort` asks YouTube for the tracks in a given order; `None` gets whatever order the account
/// already has the list in, which is what a fresh visit wants (it matches YouTube Music).
#[tauri::command]
pub async fn get_playlist(
    state: St<'_>,
    id: String,
    sort: Option<PlaylistSort>,
    desc: Option<bool>,
) -> Result<PlaylistPage, String> {
    if id == ON_REPEAT_ID {
        let items = on_repeat_songs(&state);
        return Ok(PlaylistPage {
            title: Some("On Repeat".into()),
            subtitle: Some(format!("{} songs you've played most this month", items.len())),
            thumbnail: None,
            description: None,
            privacy: None,
            cover: None,
            items,
            continuation: None,
            owned: false, // nothing to rename or delete; it rebuilds itself from what you play
            collaborative: false,
            sort_menu: None, // built from local history, so YouTube has no order to give
        });
    }
    if is_local_playlist(&id) {
        return local_playlist_page(&state, &id);
    }
    let client = metadata_client(&state)?;
    let sort = sort.map(|s| (s, desc.unwrap_or(false)));
    let mut page = state.it.playlist(client, &id, sort).await.map_err(|e| e.to_string())?;
    // Alongside YouTube's own thumbnail, not over it: the dialog offers to drop the custom one.
    page.cover = custom_cover(&state, &id);
    Ok(page)
}

/// Store a sort order on a playlist, so YouTube Music and every other client show it the same way.
///
/// Only for a playlist whose `sortMenu.editable` said the options are writes. Everywhere else the
/// order is view-only and this would 400.
#[tauri::command]
pub async fn set_playlist_sort(
    state: St<'_>,
    playlist_id: String,
    sort: PlaylistSort,
) -> Result<(), String> {
    let client = editable_playlist(&state, &playlist_id)?;
    state.it.playlist_set_sort(client, &playlist_id, sort).await.map_err(|e| e.to_string())
}

/// videoId → how many times it was played, over the same trailing window On Repeat is built from
/// (the history table is pruned to it, so there is no older data to offer). Feeds the playlist
/// page's "Most played" sort; a track the map doesn't mention has not been played this month.
#[tauri::command]
pub fn play_counts(state: St<'_>) -> std::collections::HashMap<String, i64> {
    state.db.play_counts(now_secs() - ON_REPEAT_WINDOW_SECS).into_iter().collect()
}

/// The On Repeat track list: most-played first, over the trailing window. Rows whose stored JSON
/// no longer parses (a `SongItem` shape change) are dropped rather than failing the whole page.
fn on_repeat_songs(state: &Arc<AppState>) -> Vec<SongItem> {
    let since = now_secs() - ON_REPEAT_WINDOW_SECS;
    state
        .db
        .top_plays(since, ON_REPEAT_LIMIT)
        .into_iter()
        .filter_map(|(json, _plays)| serde_json::from_str(&json).ok())
        .map(shed_queue_context)
        .collect()
}

/// A play record is the whole `SongItem` as it sat in the queue, so it carries that slot's queue
/// metadata: `queued`/`queued_by` when the track was "added to queue" (in a Listen Together session,
/// stamped with who added it), `autoplay` when radio appended it, `set_video_id` from whatever
/// playlist it was played from. None of that describes the song, so On Repeat sheds it: otherwise
/// the row wears a session member's name forever, and playing On Repeat drops it into "Next in
/// queue" instead of the playlist. Strips on read so rows already stored this way are fixed too.
fn shed_queue_context(s: SongItem) -> SongItem {
    SongItem {
        queued: false,
        queued_end: false,
        queued_from: None,
        queued_by: None,
        autoplay: false,
        set_video_id: None,
        added_by: None,
        added_by_avatar: None,
        ..s
    }
}

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[tauri::command]
pub async fn get_playlist_more(
    state: St<'_>,
    token: String,
) -> Result<PlaylistContinuation, String> {
    let client = metadata_client(&state)?;
    state.it.playlist_continuation(client, &token).await.map_err(|e| e.to_string())
}

/// An album page. `id` is the album browseId (`MPRE…`).
#[tauri::command]
pub async fn get_album(state: St<'_>, id: String) -> Result<AlbumPage, String> {
    // A local album is built from SQLite, so it opens the same page while offline (local.rs).
    if let Some(key) = id.strip_prefix(crate::local::ALBUM_PREFIX) {
        return Ok(crate::local::album_page(&state.db, key));
    }
    // A local artist rides this route too: same page shape, and none of the artist route's
    // YouTube furniture applies to files on disk (see `local::artist_page`).
    if let Some(name) = id.strip_prefix(crate::local::ARTIST_PREFIX) {
        return Ok(crate::local::artist_page(&state.db, name));
    }
    let client = metadata_client(&state)?;
    state.it.album(client, &id).await.map_err(|e| e.to_string())
}

/// An artist page. `id` is the channel browseId (`UC…`).
#[tauri::command]
pub async fn get_artist(state: St<'_>, id: String) -> Result<ArtistPage, String> {
    let client = metadata_client(&state)?;
    state.it.artist(client, &id).await.map_err(|e| e.to_string())
}

/// Moods & Genres, the tiles the search page opens on.
#[tauri::command]
pub async fn get_moods(state: St<'_>) -> Result<Vec<MoodSection>, String> {
    let client = metadata_client(&state)?;
    state.it.moods(client).await.map_err(|e| e.to_string())
}

/// A cover for each Moods & Genres tile, which YouTube draws as a coloured button with no art: the
/// first playlist in that tile's category. Six categories at a time. A tile whose category fails
/// or comes back empty is left out, and the page draws it without art. The page keeps what comes
/// back, so this runs once per tile, not once per visit.
#[tauri::command]
pub async fn get_mood_art(
    state: St<'_>,
    params: Vec<String>,
) -> Result<std::collections::HashMap<String, String>, String> {
    use futures_util::StreamExt;
    let client = metadata_client(&state)?;
    let it = &state.it;
    Ok(futures_util::stream::iter(params)
        .map(|p| async move {
            let items = it.browse_grid(client, MOODS_CATEGORY_ID, Some(&p)).await.ok()?;
            Some((p, items.into_iter().find_map(|i| i.thumbnail)?))
        })
        .buffer_unordered(6)
        .filter_map(|art| async move { art })
        .collect()
        .await)
}

/// The browseId every Moods & Genres tile opens with its own `params`.
const MOODS_CATEGORY_ID: &str = "FEmusic_moods_and_genres_category";

/// A card grid reached from a carousel's "More" button (e.g. an artist's full albums list).
#[tauri::command]
pub async fn get_browse_grid(
    state: St<'_>,
    id: String,
    params: Option<String>,
) -> Result<Vec<BrowseItem>, String> {
    let client = metadata_client(&state)?;
    state.it.browse_grid(client, &id, params.as_deref()).await.map_err(|e| e.to_string())
}

/// Play a playlist/album: the given items become the queue (no radio). `start` is the clicked
/// track index; `None`/omitted means "just play it" (random opener when shuffle is on).
/// `source_id` (the page's playlist/album playlist id) makes autoplay continue with that
/// context's radio when the queue runs out. `source_name` (the page title) feeds the queue
/// panel's "Next from" header; `shuffle: true` (page Shuffle buttons) turns shuffle on for
/// this queue — pass the items in their real order, the backend shuffles. `continuation` is the
/// page's next-page token when it has one: pass the tracks that are loaded and the backend walks
/// the rest into the queue in the background, so playback starts on page 1.
#[tauri::command]
pub async fn play_playlist(
    state: St<'_>,
    items: Vec<SongItem>,
    start: Option<usize>,
    source_id: Option<String>,
    source_name: Option<String>,
    shuffle: Option<bool>,
    continuation: Option<String>,
) -> Result<(), String> {
    let state = state.inner().clone();
    state
        .play_tracks(items, start, source_id, source_name, shuffle.unwrap_or(false), continuation)
        .await;
    Ok(())
}

/// Start a radio seeded on a song, artist, album or playlist (context/08). `kind` is
/// `song` | `artist` | `album` | `playlist`; `id` is the videoId (song) or browseId/playlistId
/// (everything else) — the backend resolves it to a radio playlist. `name` titles the queue.
///
/// Starting a song radio on the track that's already playing keeps it playing and replaces only
/// what comes after it; every other case replaces the queue.
#[tauri::command]
pub async fn start_radio(
    state: St<'_>,
    kind: String,
    id: String,
    name: Option<String>,
) -> Result<(), String> {
    let state = state.inner().clone();
    state.start_radio(&kind, &id, name).await
}

// --- write actions (context/01 ✎, context/15) ----------------------------------------------

pub(crate) fn require_login(state: &Arc<AppState>) -> Result<&innertube::YouTubeClient, String> {
    if !state.it.is_logged_in() {
        return Err("Sign in first to use this.".into());
    }
    metadata_client(state)
}

/// Like, dislike, or clear a track's rating. One command for all three: YouTube's states are
/// mutually exclusive, so a dislike un-likes in the same call and the UI never has to send two.
#[tauri::command]
pub async fn rate(state: St<'_>, video_id: String, rating: Rating) -> Result<(), String> {
    let client = require_login(&state)?;
    // Before the write, not after: a `refresh_rating` round trip already in flight was asked
    // before this rating existed, so its answer is stale from here on either way (issue #93).
    state.rate_epoch.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    state.it.rate(client, &video_id, rating).await.map_err(|e| e.to_string())
}

/// Add a song to the library (Library ▸ Songs), or take it out. `token` comes from the row's own
/// menu (`SongItem.library`), which is the only handle YouTube gives on this: it is a feedback
/// action, not a rating, so it leaves Liked Music alone.
#[tauri::command]
pub async fn set_song_saved(state: St<'_>, token: String) -> Result<(), String> {
    let client = require_login(&state)?;
    state.it.feedback(client, &token).await.map_err(|e| e.to_string())
}

/// Save an album to the library, or remove it. `playlist_id` is the album's `OLAK5uy_…`
/// (`AlbumPage.playlistId`).
#[tauri::command]
pub async fn set_album_saved(
    state: St<'_>,
    playlist_id: String,
    saved: bool,
) -> Result<(), String> {
    let client = require_login(&state)?;
    state.it.like_playlist(client, &playlist_id, saved).await.map_err(|e| e.to_string())
}

/// Login, plus the guard every playlist edit needs. Two ids never reach `edit_playlist`: On Repeat
/// has no YouTube playlist behind it, and Liked Music is an auto-playlist YouTube edits through the
/// rating endpoint instead. Both answer 400 there.
pub(crate) fn editable_playlist<'a>(
    state: &'a Arc<AppState>,
    playlist_id: &str,
) -> Result<&'a innertube::YouTubeClient, String> {
    if playlist_id == ON_REPEAT_ID {
        return Err("On Repeat builds itself from what you play.".into());
    }
    if playlist_id == LIKED_MUSIC_ID {
        return Err("Liked Music follows your likes; like the song instead.".into());
    }
    // The commands that edit one dispatch before they get here. This is the net for any that
    // doesn't, so a playlist on this machine can never reach YouTube as an id it has never seen.
    if is_local_playlist(playlist_id) {
        return Err("This playlist is on this device, so that isn't available for it.".into());
    }
    require_login(state)
}

/// Liked Music. It is indexed like the rest, because a search row is the one place YouTube tells
/// us nothing: `search` responses carry no `likeStatus` at all (live-checked 2026-08-28), so
/// membership of this list is the only way a result can draw its heart filled. Not shown in the
/// "saved in" chip, though: the thumbs-up already says it (the UI filters it out there).
pub(crate) const LIKED_MUSIC_ID: &str = "VLLM";
/// When the index was last rebuilt by a full crawl (unix seconds). The key changes whenever the
/// index learns to hold something new, so an install with a fresh stamp re-crawls once: `_v2` when
/// Liked Music joined it, `_v3` when each track's metadata did (the "in your playlists" view and
/// the monitor read it).
pub(crate) const PLAYLIST_INDEX_STAMP: &str = "playlist_index_synced_at_v3";
/// How often the index is re-crawled by itself, in hours (`monitor_interval_hours`): the choices
/// the UI offers, 0 being never. Adds and removes made in this app patch the index as they happen,
/// so the interval only ever covers edits made somewhere else.
const MONITOR_INTERVALS: [i64; 6] = [0, 1, 3, 6, 12, 24];
const MONITOR_INTERVAL_DEFAULT: i64 = 6;
/// Continuation pages per playlist. YouTube hands back 100 tracks a page, so this covers 5000 of
/// them. ponytail: a hard stop, not paging state. A playlist past it marks its first 5000 tracks
/// and no more, which beats one pathological list turning a sync into hundreds of requests.
const PLAYLIST_INDEX_MAX_PAGES: usize = 50;

/// videoId → the ids of your playlists holding it, straight from SQLite with no network at all,
/// so a track list can draw the "saved" mark on its first paint. Empty until the first sync.
#[tauri::command]
pub fn playlist_index(state: St<'_>) -> std::collections::HashMap<String, Vec<String>> {
    state.db.playlist_memberships()
}

/// The re-crawl interval in seconds, `None` when it is off. Anything the UI would not have written
/// reads as the default.
fn monitor_interval_secs(db: &crate::db::Db) -> Option<i64> {
    let hours = db
        .get_setting("monitor_interval_hours")
        .and_then(|h| h.trim().parse::<i64>().ok())
        .filter(|h| MONITOR_INTERVALS.contains(h))
        .unwrap_or(MONITOR_INTERVAL_DEFAULT);
    (hours > 0).then_some(hours * 3600)
}

/// Whether a sync nobody asked for (launch, sign-in, the scheduler) should crawl now. An index that
/// was never built always should, interval off or not: the "saved" marks have nothing else to go
/// on. After that, only once the interval has passed, and never with it off.
pub(crate) fn playlist_index_due(db: &crate::db::Db, now: i64) -> bool {
    let Some(at) = db.get_setting(PLAYLIST_INDEX_STAMP).and_then(|at| at.parse::<i64>().ok())
    else {
        return true;
    };
    monitor_interval_secs(db).is_some_and(|every| now >= at + every)
}

/// When the last full sync was tried (unix seconds), and how many failed in a row before now: what
/// keeps a sync that cannot run (offline, an expired cookie) from being retried every minute.
pub(crate) const MONITOR_LAST_ATTEMPT: &str = "monitor_last_attempt_at";
pub(crate) const MONITOR_FAILURES: &str = "monitor_failures";

fn monitor_failures(db: &crate::db::Db) -> u32 {
    db.get_setting(MONITOR_FAILURES).and_then(|n| n.trim().parse().ok()).unwrap_or(0)
}

/// Write a full sync's attempt down: a success clears the failures, a failure adds one.
fn note_sync_attempt(db: &crate::db::Db, at: i64, ok: bool) {
    db.set_setting(MONITOR_LAST_ATTEMPT, &at.to_string());
    let failures = if ok { 0 } else { monitor_failures(db).saturating_add(1) };
    db.set_setting(MONITOR_FAILURES, &failures.to_string());
}

/// Whether the scheduler may try again after failed syncs ([`monitor::retry_due`]). Someone
/// asking for a sync does not wait for this.
pub(crate) fn monitor_retry_due(db: &crate::db::Db, now: i64) -> bool {
    let last = db.get_setting(MONITOR_LAST_ATTEMPT).and_then(|at| at.parse::<i64>().ok());
    monitor::retry_due(now, last, monitor_failures(db), monitor_interval_secs(db))
}

/// The `monitor_runs.trigger` for what the UI sent: the scheduler's and headless runs say so, and
/// anything else is a person in the UI.
fn sync_trigger(trigger: Option<&str>) -> &'static str {
    match trigger {
        Some("scheduler") => "scheduler",
        Some("headless") => "headless",
        _ => "manual_ui",
    }
}

/// How one playlist's read went.
enum PlaylistRead {
    /// Someone else's playlist you merely saved: not indexed, not watched.
    NotYours,
    /// Its first page could not be read.
    Failed,
    Read {
        complete: bool,
        counts: monitor::Counts,
    },
}

/// Read one playlist to the end (or the page cap) and write it down: index rows, snapshot,
/// alerts, sync record ([`monitor::record`]). The reader is the Data API when the run has one
/// (`data`, [`crate::ytdata_sync::prepare`]) and it covers this playlist, which also gives each
/// track's date added and the playlist's privacy; InnerTube otherwise, and whenever the Data API
/// read fails.
async fn sync_one(
    state: &Arc<AppState>,
    client: &innertube::YouTubeClient,
    playlist_id: &str,
    title: Option<&str>,
    account_id: Option<&str>,
    data: Option<&crate::ytdata_sync::DataApiRun>,
) -> PlaylistRead {
    if let Some(read) = sync_one_data_api(state, data, playlist_id, account_id).await {
        return read;
    }
    let Ok(page) = state.it.playlist(client, playlist_id, None).await else {
        return PlaylistRead::Failed;
    };
    // A collaborative playlist reads `owned: false` (YouTube drops the editable header on it) but
    // is one you add to and remove from, so the membership index has to cover it too. Liked Music
    // is exempt: YouTube sends it without the editable header, so it reads `owned: false` even
    // though it is yours.
    if playlist_id != LIKED_MUSIC_ID && !page.owned && !page.collaborative {
        return PlaylistRead::NotYours;
    }
    let title = page.title.clone().or_else(|| title.map(str::to_owned));
    let listed = page.subtitle.as_deref().and_then(monitor::header_track_count);
    let mut songs: Vec<SongItem> = page.items;
    let mut token = page.continuation;
    // Read to the end, not cut short by a failed page or the page cap: only then may the monitor
    // read a track missing from it as removed.
    let mut complete = token.is_none();
    for _ in 0..PLAYLIST_INDEX_MAX_PAGES {
        let Some(next) = token.take() else {
            complete = true;
            break;
        };
        let Ok(more) = state.it.playlist_continuation(client, &next).await else { break };
        songs.extend(more.items);
        token = more.continuation;
    }
    complete = complete || token.is_none();
    let read = monitor::Read {
        playlist_id,
        title: title.as_deref(),
        account_id,
        songs: &songs,
        complete,
        // Liked Music changes every time you like or unlike something anywhere: not news.
        watch: playlist_id != LIKED_MUSIC_ID,
        at: now_secs(),
        listed,
        added_at: None,
        privacy: None,
        reasons: None,
    };
    // An empty read after one that held rows is doubted, and counts as cut short.
    let complete = monitor::read_complete(&state.db, &read);
    let counts = monitor::record(&state.db, &read);
    PlaylistRead::Read { complete, counts }
}

/// [`sync_one`] through the Data API. `None` when the run has no Data API, it does not cover this
/// playlist (not one of the channel's: Liked Music, someone else's collaborative one), or the read
/// failed; the caller then reads it through InnerTube.
async fn sync_one_data_api(
    state: &Arc<AppState>,
    data: Option<&crate::ytdata_sync::DataApiRun>,
    playlist_id: &str,
    account_id: Option<&str>,
) -> Option<PlaylistRead> {
    let run = data?;
    let playlist = run.covers(playlist_id)?;
    let known = state.db.playlist_songs(playlist_id);
    let got = match run.read(playlist, &known).await {
        Ok(got) => got,
        Err(e) => {
            run.failed(&e);
            tracing::info!(error = %e, playlist_id, "Data API read failed, reading via InnerTube");
            return None;
        }
    };
    let read = monitor::Read {
        playlist_id,
        title: Some(got.title.as_str()),
        account_id,
        songs: &got.songs,
        complete: got.complete,
        watch: true,
        at: now_secs(),
        listed: Some(got.listed),
        added_at: Some(&got.added_at),
        privacy: Some(got.privacy.as_str()),
        reasons: Some(&got.reasons),
    };
    let complete = monitor::read_complete(&state.db, &read);
    let counts = monitor::record(&state.db, &read);
    Some(PlaylistRead::Read { complete, counts })
}

/// The run's Data API, when it reads through it: the reader choice for a run of `scope`, and
/// what goes in its `detail_json` under `reader`. Scheduled and headless runs are the day's backup
/// and may spend its reserve.
async fn data_api_run(
    state: &Arc<AppState>,
    scope: crate::ytdata_sync::Scope<'_>,
    trigger: &str,
) -> (Option<crate::ytdata_sync::DataApiRun>, Value) {
    let backup_run = matches!(trigger, "scheduler" | "headless");
    crate::ytdata_sync::prepare(state, scope, backup_run).await
}

/// After a run: what it spent goes on the summary (and so on `monitor_runs.units_spent`), the
/// quota widget hears of it, and the status is sent again, since the run may have marked the API
/// disabled or out of quota, or cleared that.
async fn finish_data_api_run(
    state: &Arc<AppState>,
    data: Option<&crate::ytdata_sync::DataApiRun>,
    summary: &mut SyncSummary,
    reader: &mut Value,
) {
    let Some(run) = data else { return };
    summary.units_spent = run.units();
    *reader = run.detail();
    if summary.units_spent > 0 {
        let _ = state.app.emit("quota-changed", ());
    }
    crate::ytdata_status::announce(state).await;
}

/// Log a finished run in `monitor_runs`. `playlists_failed` is every playlist not read to the
/// end; `detail` tells the unreadable ones from the cut-short ones. A scheduler failure right
/// after another folds into that one's row (`detail.repeats`, [`monitor::fold_failure`]) rather
/// than logging a row per retry.
fn record_run(state: &AppState, summary: &SyncSummary, started: i64, outcome: &str, detail: Value) {
    let run = crate::db::MonitorRun {
        id: 0,
        started_at: started,
        finished_at: now_secs(),
        trigger: summary.trigger.clone(),
        outcome: outcome.to_owned(),
        playlists_ok: i64::from(summary.complete),
        playlists_failed: i64::from(summary.playlists.saturating_sub(summary.complete)),
        alerts_new: i64::from(summary.alerts_new),
        units_spent: summary.units_spent,
        detail_json: detail.to_string(),
    };
    let prev = state.db.monitor_runs(1).into_iter().next();
    if let Some((id, detail)) = monitor::fold_failure(prev.as_ref(), &run) {
        if let Err(e) = state.db.update_monitor_run_repeat(id, run.finished_at, &detail) {
            tracing::warn!(error = %e, "could not log a monitor run");
        }
        return;
    }
    if let Err(e) = state.db.record_monitor_run(&run) {
        tracing::warn!(error = %e, "could not log a monitor run");
    }
}

/// Another run holds the monitor: log the attempt as `lock_busy` and answer `busy`.
fn monitor_busy(state: &AppState, trigger: &str) -> String {
    let now = now_secs();
    record_run(state, &SyncSummary::new(trigger, now), now, "lock_busy", json!({}));
    "busy".into()
}

/// Tell the UI a run finished: the index to re-read, and the alerts badge.
fn announce_sync(state: &AppState, summary: &SyncSummary) {
    let _ = state.app.emit("playlist-index-synced", summary);
    let unseen = state.db.unseen_alert_count();
    let _ = state.app.emit("alerts-changed", json!({ "unseen": unseen }));
}

/// The end of a monitor run: back up the newest snapshot of each playlist it read unless that file
/// is already there, then prune the database and the backups folder to `retention_keep_last`
/// (backups.rs). Off the async workers; a failure costs the backup, never the run.
async fn back_up_run(state: &Arc<AppState>, playlist_ids: Vec<String>) {
    let st = state.clone();
    let data = crate::paths::data_dir(&state.app);
    let done = tauri::async_runtime::spawn_blocking(move || {
        let dir = crate::backups::backups_dir(&st.db, &data);
        let keep = crate::backups::keep_last(&st.db);
        crate::backups::export_and_prune(&st.db, &dir, keep, &playlist_ids, true)
    })
    .await;
    if let Err(e) = done {
        tracing::warn!(error = %e, "snapshot backups did not run");
    }
}

/// Where the backups go, where they go by default, how many each playlist keeps, and whether the
/// folder picked was turned down for being another app's (`rejected`; the default is used then).
#[tauri::command]
pub fn backups_info(state: St<'_>) -> Value {
    let data = crate::paths::data_dir(&state.app);
    json!({
        "dir": crate::backups::backups_dir(&state.db, &data).to_string_lossy(),
        "default_dir": crate::backups::default_dir(&data).to_string_lossy(),
        "keep": crate::backups::keep_last(&state.db),
        "rejected": crate::backups::dir_rejected(&state.db),
    })
}

/// Back up the newest snapshot of every playlist that has one now (synced or not: a forget or a
/// sign-out keeps the snapshots), rewriting its file, then prune as a monitor run does. Answers
/// `{ written, pruned_files, pruned_rows }`; `Err("busy")` while a sync runs, which would be
/// writing the same snapshots.
#[tauri::command]
pub async fn export_backups_now(state: St<'_>) -> Result<crate::backups::Outcome, String> {
    let Some(_running) = state.begin_monitor_run() else {
        return Err("busy".into());
    };
    let st = state.inner().clone();
    let data = crate::paths::data_dir(&state.app);
    tauri::async_runtime::spawn_blocking(move || {
        let dir = crate::backups::backups_dir(&st.db, &data);
        let keep = crate::backups::keep_last(&st.db);
        // Synced playlists, and any other with snapshots kept (forgotten, or from before a
        // sign-out): their history outlives the sync record, so its backups do too.
        let mut ids: Vec<String> = st.db.playlist_syncs().into_keys().collect();
        ids.extend(st.db.snapshot_playlist_ids());
        ids.sort();
        ids.dedup();
        crate::backups::export_and_prune(&st.db, &dir, keep, &ids, false)
    })
    .await
    .map_err(|e| e.to_string())
}

/// Open the backups folder in the file manager, creating it if it is not there yet.
#[tauri::command]
pub fn open_backups_dir(state: St<'_>) -> Result<(), String> {
    let data = crate::paths::data_dir(&state.app);
    crate::backups::open_dir(&crate::backups::backups_dir(&state.db, &data))
}

// --- downloads (download/) -----------------------------------------------------------------------

type Downloads<'a> = State<'a, Arc<crate::download::runner::Runner>>;

/// yt-dlp's version (`None`: missing or broken), whether ffmpeg is there, and whether the app
/// manages them (Windows) or they come from PATH.
#[tauri::command]
pub async fn download_tools_status(
    state: St<'_>,
) -> Result<crate::download::tools::ToolsStatus, String> {
    let data = crate::paths::data_dir(&state.app);
    Ok(crate::download::tools::status(&data).await)
}

fn install_progress(
    app: &tauri::AppHandle,
) -> impl Fn(crate::download::tools::InstallProgress) + Send + Sync {
    let app = app.clone();
    move |p| {
        let _ = app.emit("tools-install-progress", p);
    }
}

/// Install or update yt-dlp from the `downloads.ytdlp_channel` releases, checked against the
/// release's own `SHA2-256SUMS`. Answers the installed version. `Err("busy")` while another
/// install runs, `Err("not_managed")` off Windows, `Err("checksum_mismatch")` when the bytes do
/// not match. Progress goes out as `tools-install-progress`.
#[tauri::command]
pub async fn install_ytdlp(state: St<'_>, downloads: Downloads<'_>) -> Result<String, String> {
    let Some(_installing) = downloads.begin_install() else { return Err("busy".into()) };
    let data = crate::paths::data_dir(&state.app);
    let repo = crate::download::settings::ytdlp_channel(&state.db).repo();
    let progress = install_progress(&state.app);
    let version = crate::download::tools::install_ytdlp(&data, repo, &progress).await?;
    // A queue that was waiting for yt-dlp can go now.
    downloads.nudge();
    Ok(version)
}

/// Install or update ffmpeg (and ffprobe) from yt-dlp's FFmpeg-Builds, checked against the
/// release's `checksums.sha256`. Errors as [`install_ytdlp`].
#[tauri::command]
pub async fn install_ffmpeg(state: St<'_>, downloads: Downloads<'_>) -> Result<(), String> {
    let Some(_installing) = downloads.begin_install() else { return Err("busy".into()) };
    let data = crate::paths::data_dir(&state.app);
    let progress = install_progress(&state.app);
    crate::download::tools::install_ffmpeg(&data, &progress).await
}

/// The download folder in use and the default one (`downloads.dir` empty).
#[tauri::command]
pub fn downloads_info(state: St<'_>) -> Value {
    let data = crate::paths::data_dir(&state.app);
    json!({
        "dir": crate::download::settings::downloads_dir(&state.db, &data).to_string_lossy(),
        "default_dir": crate::download::settings::default_dir(&data).to_string_lossy(),
    })
}

/// Open the download folder in the file manager, creating it if it is not there yet.
#[tauri::command]
pub fn open_downloads_dir(state: St<'_>) -> Result<(), String> {
    let data = crate::paths::data_dir(&state.app);
    crate::backups::open_dir(&crate::download::settings::downloads_dir(&state.db, &data))
}

/// One song to download, with what the `videos` table keeps about it. A `SongItem` from the UI
/// deserializes into this as it is.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct DownloadSong {
    pub video_id: String,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub artists: Option<String>,
    #[serde(default)]
    pub duration: Option<String>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize)]
pub struct EnqueueResult {
    /// New rows, and errored, missing or (with `redownload`) downloaded ones queued again.
    pub queued: u32,
    /// Already queued, running or downloaded: left as they were.
    pub already: u32,
    /// Not a YouTube video id (`^[A-Za-z0-9_-]{11}$`): dropped, never queued nor passed to
    /// yt-dlp. Local songs are skipped without being counted here.
    pub invalid: u32,
}

/// Queue `songs` in `format` (`audio`/`video`; the `downloads.default_format` setting when
/// absent), with today's quality, cover and folder settings. Local files are skipped.
/// `redownload` queues downloaded ones again. `Err("bad_format")` for any other format.
#[tauri::command]
pub async fn download_enqueue(
    state: St<'_>,
    downloads: Downloads<'_>,
    songs: Vec<DownloadSong>,
    format: Option<String>,
    redownload: Option<bool>,
) -> Result<EnqueueResult, String> {
    use crate::download::settings;
    let format = match format.as_deref() {
        None => settings::default_format(&state.db),
        Some(f) => settings::Format::parse(f).ok_or("bad_format")?,
    };
    let data = crate::paths::data_dir(&state.app);
    let dest = settings::downloads_dir(&state.db, &data).to_string_lossy().into_owned();
    let quality = settings::requested_quality(&state.db, format);
    let thumbnail_mode = settings::thumbnail_mode(&state.db).as_str();
    let now = crate::db::now_secs();

    let mut result = EnqueueResult::default();
    let mut ids = Vec::new();
    for song in &songs {
        if crate::local::is_local_song(&song.video_id) {
            continue;
        }
        if !crate::download::ytdlp_args::is_valid_video_id(&song.video_id) {
            result.invalid += 1;
            continue;
        }
        let duration_s = song.duration.as_deref().and_then(crate::backups::duration_secs);
        let request = crate::db::NewDownload {
            video: crate::db::VideoMeta {
                video_id: &song.video_id,
                title: song.title.as_deref().filter(|t| !t.is_empty()),
                channel: song.artists.as_deref().filter(|a| !a.is_empty()),
                duration_s,
            },
            format: format.as_str(),
            requested_quality: quality,
            thumbnail_mode,
            dest_dir: &dest,
            redownload: redownload.unwrap_or(false),
        };
        match state.db.enqueue_download(&request, now).map_err(|e| e.to_string())? {
            crate::db::EnqueueOutcome::Already => result.already += 1,
            _ => {
                result.queued += 1;
                ids.push(song.video_id.clone());
            }
        }
    }
    if !ids.is_empty() {
        crate::download::runner::emit_changed(&state.app, ids, None);
        downloads.nudge();
    }
    Ok(result)
}

/// Stop the download that is running; its row goes (the file yt-dlp had started stays for a
/// later resume). `false` when nothing runs.
#[tauri::command]
pub fn download_cancel(downloads: Downloads<'_>) -> bool {
    downloads.cancel_active()
}

/// An errored, missing or downloaded row back into the queue, at the back. `false` for one
/// already queued or running, or no row at all.
#[tauri::command]
pub fn download_retry(
    state: St<'_>,
    downloads: Downloads<'_>,
    video_id: String,
    format: String,
) -> bool {
    let requeued = state.db.requeue_download(&video_id, &format, crate::db::now_secs());
    if requeued {
        crate::download::runner::emit_changed(&state.app, vec![video_id], None);
        downloads.nudge();
    }
    requeued
}

/// Forget a download: its row, and the run if it is the one running. Never the file.
#[tauri::command]
pub fn download_remove(
    state: St<'_>,
    downloads: Downloads<'_>,
    video_id: String,
    format: String,
) -> bool {
    // The runner drops the row of the run it stops.
    if downloads.is_active(&video_id, &format) && downloads.cancel_active() {
        return true;
    }
    let removed = state.db.delete_download(&video_id, &format);
    if removed {
        crate::download::runner::emit_changed(&state.app, vec![video_id], None);
    }
    removed
}

/// The download running now, as the last `download-progress` said; `None` when idle.
#[tauri::command]
pub fn download_active(downloads: Downloads<'_>) -> Option<crate::download::runner::Progress> {
    downloads.active()
}

/// Every download row of these videos, grouped by video, after checking the files are still
/// there (a moved file is found by its `[id]` marker; a deleted one goes `missing`).
#[tauri::command]
pub async fn downloads_for(
    state: St<'_>,
    video_ids: Vec<String>,
) -> Result<std::collections::HashMap<String, Vec<crate::db::DownloadRow>>, String> {
    let db = state.db.clone();
    tauri::async_runtime::spawn_blocking(move || {
        crate::download::verify::verify_for_video_ids(&db, &video_ids, crate::db::now_secs())
    })
    .await
    .map_err(|e| e.to_string())
}

/// The newest downloads (50 by default), by completion or, until then, queueing date.
#[tauri::command]
pub fn downloads_recent(state: St<'_>, limit: Option<u32>) -> Vec<crate::db::DownloadRow> {
    state.db.recent_downloads(limit.unwrap_or(50).clamp(1, 1000) as usize)
}

/// The whole crawl: every playlist you own, then the index pruned to them, the stamp, the summary
/// (`playlist_index_last_summary`) and a `monitor_runs` row. `Err("busy")` while another run
/// holds the monitor, `Err("empty_library")` when YouTube answered with no playlists at all.
pub(crate) async fn sync_all(state: &Arc<AppState>, trigger: &str) -> Result<SyncSummary, String> {
    let Some(_running) = state.begin_monitor_run() else {
        return Err(monitor_busy(state, trigger));
    };
    let started = now_secs();
    let mut summary = SyncSummary::new(trigger, started);
    let client = match metadata_client(state) {
        Ok(client) => client,
        Err(e) => return Err(sync_failed(state, &summary, started, e)),
    };
    let library = match state.it.library_playlists(client).await {
        Ok(library) => library,
        Err(e) => return Err(sync_failed(state, &summary, started, e.to_string())),
    };
    // A degraded response that parses as an empty library would otherwise wipe every mark and
    // then call the wipe fresh for a whole interval. Nothing to index is nothing to trust: keep
    // what is stored and try again later (the scheduler backing off, `monitor_retry_due`).
    if library.is_empty() {
        return Err(sync_failed(state, &summary, started, "empty_library".into()));
    }
    let account = state.db.get_setting("active_account");
    let playlists: Vec<_> = library.into_iter().filter(|p| p.id != ON_REPEAT_ID).collect();
    let total = playlists.len();
    let mut indexed: Vec<String> = Vec::new();
    let mut failed_ids: Vec<String> = Vec::new();
    let (data, mut reader) = data_api_run(state, crate::ytdata_sync::Scope::All, trigger).await;
    let run = data.as_ref();
    for (done, item) in playlists.into_iter().enumerate() {
        let progress = json!({ "done": done, "total": total, "current": item.title });
        let _ = state.app.emit("playlist-sync-progress", progress);
        let title = Some(item.title.as_str());
        let read = sync_one(state, client, &item.id, title, account.as_deref(), run).await;
        match read {
            PlaylistRead::NotYours => continue,
            // One playlist failing (a deleted id, a hiccup) must not abandon the rest of the
            // crawl, and must not drop what is already indexed for it either: leaving it out of
            // `indexed` would have `retain_playlists` forget the tracks we do know about.
            PlaylistRead::Failed => {
                summary.failed += 1;
                failed_ids.push(item.id.clone());
            }
            PlaylistRead::Read { complete, counts } => {
                summary.complete += u32::from(complete);
                summary.add(counts);
            }
        }
        summary.playlists += 1;
        indexed.push(item.id);
    }
    let _ = state.app.emit(
        "playlist-sync-progress",
        json!({ "done": total, "total": total, "current": Value::Null }),
    );
    state.db.retain_playlists(&indexed);
    finish_data_api_run(state, data.as_ref(), &mut summary, &mut reader).await;
    back_up_run(state, indexed).await;
    state.db.set_setting(PLAYLIST_INDEX_STAMP, &now_secs().to_string());
    note_sync_attempt(&state.db, started, summary.outcome() != "failed");
    monitor::save_summary(&state.db, &summary);
    let short = summary.playlists - summary.complete - summary.failed;
    let detail =
        json!({ "scope": "all", "failed": failed_ids, "incomplete": short, "reader": reader });
    record_run(state, &summary, started, summary.outcome(), detail);
    announce_sync(state, &summary);
    Ok(summary)
}

/// A full sync that could not run at all: log the run as `failed`, count the failure for the
/// scheduler's back-off, and answer the error.
fn sync_failed(state: &AppState, summary: &SyncSummary, started: i64, error: String) -> String {
    record_run(state, summary, started, "failed", json!({ "error": error }));
    note_sync_attempt(&state.db, started, false);
    error
}

/// The scheduler's tick (lib.rs, once a minute): a full sync with trigger `scheduler` when one is
/// due, nothing otherwise. A run already in flight is no attempt, so it logs no `lock_busy`. After
/// a failure it waits out the back-off first (`monitor_retry_due`), so a sync that cannot run is
/// not retried, and logged, every minute.
pub(crate) async fn scheduled_sync(state: &Arc<AppState>) {
    if !state.it.is_logged_in() || state.monitor_running() {
        return;
    }
    let now = now_secs();
    if !playlist_index_due(&state.db, now) || !monitor_retry_due(&state.db, now) {
        return;
    }
    if let Err(e) = sync_all(state, "scheduler").await {
        tracing::info!(error = %e, "scheduled playlist sync did not run");
    }
}

/// Rebuild that index by walking the playlists you own, then answer with it.
///
/// Nothing else knows playlist membership: the library browse gives cards, a playlist browse gives
/// one list's tracks, and InnerTube's per-video add-to-playlist dialog would be a request per row.
/// So the crawl is the price, and it is paid at most once per `monitor_interval_hours`, on a
/// launch, a sign-in or the scheduler's tick, unless `force` asks for it now. Playlists you merely
/// saved are skipped: they are someone else's, so "you saved this song to it" would be a lie, and
/// their long mixes would double the walk. Each playlist read to the end is also compared with its
/// last snapshot, which is what files the monitor's alerts (playlist_tools::monitor).
///
/// `trigger` is `manual_ui` (the default), `scheduler` or `headless`, for `monitor_runs`. Answers
/// `Err("busy")` while another sync runs.
#[tauri::command]
pub async fn sync_playlist_index(
    state: St<'_>,
    force: Option<bool>,
    trigger: Option<String>,
) -> Result<std::collections::HashMap<String, Vec<String>>, String> {
    if !state.it.is_logged_in() {
        // What is left is the playlists on this machine, which no account owns.
        state.db.clear_playlist_index();
        return Ok(state.db.playlist_memberships());
    }
    let trigger = sync_trigger(trigger.as_deref());
    let now = now_secs();
    // Unasked-for (`scheduler`: a launch, a sign-in) waits out the scheduler's back-off too, except
    // with automatic checks off: then nothing else retries a failed first build, so each launch
    // still gets one try, as before the back-off.
    let backing_off = trigger == "scheduler"
        && monitor_interval_secs(&state.db).is_some()
        && !monitor_retry_due(&state.db, now);
    if !force.unwrap_or(false) && (!playlist_index_due(&state.db, now) || backing_off) {
        return Ok(state.db.playlist_memberships());
    }
    match sync_all(&state, trigger).await {
        Ok(_) => {}
        // Logged already; what is stored stands.
        Err(e) if e == "empty_library" => {}
        Err(e) => return Err(e),
    }
    Ok(state.db.playlist_memberships())
}

/// Sync one playlist now, whatever the interval says: its index rows, snapshot and alerts, as the
/// full crawl would. The rest of the index is left alone (no pruning), and so are the crawl's
/// stamp and stored summary. Answers what this one read found; `Err("busy")` while another sync
/// runs.
#[tauri::command]
pub async fn sync_playlist(state: St<'_>, playlist_id: String) -> Result<SyncSummary, String> {
    if playlist_id == ON_REPEAT_ID || is_local_playlist(&playlist_id) {
        return Err("This playlist is on this device, so there is nothing to sync.".into());
    }
    let client = require_login(&state)?;
    let Some(_running) = state.begin_monitor_run() else {
        return Err(monitor_busy(&state, "manual_ui"));
    };
    let started = now_secs();
    let mut summary = SyncSummary::new("manual_ui", started);
    let account = state.db.get_setting("active_account");
    let progress = json!({ "done": 0, "total": 1, "current": playlist_id });
    let _ = state.app.emit("playlist-sync-progress", progress);
    let scope = crate::ytdata_sync::Scope::One(&playlist_id);
    let (data, mut reader) = data_api_run(&state, scope, "manual_ui").await;
    match sync_one(&state, client, &playlist_id, None, account.as_deref(), data.as_ref()).await {
        PlaylistRead::NotYours => {}
        PlaylistRead::Failed => {
            summary.playlists = 1;
            summary.failed = 1;
        }
        PlaylistRead::Read { complete, counts } => {
            summary.playlists = 1;
            summary.complete = u32::from(complete);
            summary.add(counts);
        }
    }
    let _ = state
        .app
        .emit("playlist-sync-progress", json!({ "done": 1, "total": 1, "current": Value::Null }));
    finish_data_api_run(&state, data.as_ref(), &mut summary, &mut reader).await;
    back_up_run(&state, vec![playlist_id.clone()]).await;
    let detail = json!({ "scope": playlist_id, "reader": reader });
    record_run(&state, &summary, started, summary.outcome(), detail);
    announce_sync(&state, &summary);
    if summary.failed > 0 {
        return Err("unreadable".into());
    }
    Ok(summary)
}

/// The last full sync's summary ("+N −N ~N" and the rest), `None` before the first.
#[tauri::command]
pub fn last_sync_summary(state: St<'_>) -> Option<SyncSummary> {
    monitor::last_summary(&state.db)
}

/// Playlist id → its last complete sync (`synced_at`, `item_count`, `added`, `removed`, `moved`):
/// the Library's "2 h ago · +3 −1 ~2" line and its sort by count or by sync. SQLite only.
#[tauri::command]
pub fn playlist_sync_info(
    state: St<'_>,
) -> std::collections::HashMap<String, crate::db::PlaylistSync> {
    state.db.playlist_syncs()
}

/// `videoId` → the earliest date it was added to one of your playlists, as the Data API reported
/// it (epoch seconds). Tracks only InnerTube has read are absent: the "In your playlists" view
/// falls back to their first-seen date. SQLite only.
#[tauri::command]
pub fn playlist_added_dates(state: St<'_>) -> std::collections::HashMap<String, i64> {
    state.db.playlist_added_dates()
}

/// Whether the YouTube Data API can be used now, and why not ([`crate::ytdata_status`]). States
/// and counters only: no token or secret ever reaches the webview. Changes arrive as
/// `ytdata-status-changed`.
#[tauri::command]
pub async fn ytdata_status(state: St<'_>) -> Result<crate::ytdata_status::YtDataStatus, String> {
    Ok(crate::ytdata_status::current(&state).await)
}

// --- Settings ▸ YouTube Data API ---------------------------------------------------------------
// The imported client secret, the channels connected through OAuth, the budget. Only the masked
// client id, channel metadata and states cross into the webview: never a token, never the client
// secret. The authorization opens in the system browser, never in the webview.

/// Sent once a connection started by [`ytdata_connect_start`] ends, however it ends.
const CONNECT_FINISHED_EVENT: &str = "ytdata-connect-finished";
/// How long the browser has to come back to the loopback before the attempt gives up.
const CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5 * 60);

/// The connection in progress: its number and its cancel flag. Starting another cancels it.
type Connecting = Option<(u64, Arc<std::sync::atomic::AtomicBool>)>;
static CONNECTING: std::sync::Mutex<Connecting> = std::sync::Mutex::new(None);
static CONNECT_ATTEMPTS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn connecting() -> std::sync::MutexGuard<'static, Connecting> {
    CONNECTING.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct ClientSecretInfo {
    masked_client_id: String,
}

/// An import failure in words. serde's message can quote a piece of the file, so a file that is
/// not JSON gets a fixed sentence instead.
fn import_error(e: &ytdata::error::Error) -> String {
    match e {
        ytdata::error::Error::Serde(_) => {
            "That file is not a client_secret.json: it is not valid JSON.".to_string()
        }
        other => other.to_string(),
    }
}

/// Validates the `client_secret.json` the user picked and copies it to the data folder
/// (`ytdata_secrets::client_secret_path`). Answers only its masked client id.
#[tauri::command]
pub async fn ytdata_import_client_secret(
    state: St<'_>,
    jobs: Jobs<'_>,
    path: String,
) -> Result<ClientSecretInfo, String> {
    let dest = crate::ytdata_secrets::client_secret_path(&state.app);
    let dir = dest.parent().map(std::path::Path::to_path_buf).ok_or("no data folder")?;
    let source = std::path::PathBuf::from(path);
    let parsed = tokio::task::spawn_blocking(move || ytdata::client_secret::import(&source, &dir))
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| import_error(&e))?;
    let info = ClientSecretInfo { masked_client_id: parsed.masked_client_id() };
    if let Some(manager) = jobs.account_manager() {
        manager.set_client_secret(parsed);
    }
    tracing::info!("ytdata: client_secret.json imported");
    crate::ytdata_status::announce(&state).await;
    Ok(info)
}

/// The imported client secret's masked client id, or `None` before one is imported.
#[tauri::command]
pub async fn ytdata_client_secret_info(
    state: St<'_>,
    jobs: Jobs<'_>,
) -> Result<Option<ClientSecretInfo>, String> {
    let secret = jobs.account_manager().and_then(|m| m.client_secret()).or_else(|| {
        let path = crate::ytdata_secrets::client_secret_path(&state.app);
        path.parent().and_then(ytdata::client_secret::load_existing)
    });
    Ok(secret.map(|s| ClientSecretInfo { masked_client_id: s.masked_client_id() }))
}

#[derive(Debug, serde::Serialize)]
pub struct ConnectStarted {
    /// Google's consent page, already opened in the system browser; shown so it can be copied
    /// when no browser opened. Carries the client id, PKCE challenge and state, no secret.
    authorize_url: String,
    /// Which attempt this is: [`CONNECT_FINISHED_EVENT`] names it.
    attempt: u64,
}

/// How a connection ended, as [`CONNECT_FINISHED_EVENT`] carries it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
struct ConnectFinished {
    attempt: u64,
    ok: bool,
    /// `cancelled`, `timed_out` or `failed` when not ok.
    #[serde(skip_serializing_if = "Option::is_none")]
    code: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    channel_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    title: Option<String>,
}

impl ConnectFinished {
    fn failed(attempt: u64, e: &ytdata::error::Error) -> Self {
        use ytdata::error::{AuthError, Error};
        let (code, error) = match e {
            Error::Auth(AuthError::Cancelled) => ("cancelled", None),
            Error::Auth(AuthError::TimedOut) => ("timed_out", Some(e.to_string())),
            _ => ("failed", Some(e.to_string())),
        };
        Self { attempt, ok: false, code: Some(code), error, channel_id: None, title: None }
    }
}

/// Starts connecting a channel: binds the loopback, opens Google's consent page in the system
/// browser, and waits in the background (up to five minutes, or until cancelled) for it to come
/// back. The end arrives as `ytdata-connect-finished` (and `ytdata-status-changed`).
#[tauri::command]
pub async fn ytdata_connect_start(state: St<'_>, jobs: Jobs<'_>) -> Result<ConnectStarted, String> {
    use std::sync::atomic::{AtomicBool, Ordering};
    use ytdata::auth::flow::{PendingAuthorization, YOUTUBE_SCOPE};
    let manager =
        jobs.account_manager().ok_or("The YouTube Data API is not available in this session.")?;
    let no_secret = || ytdata::error::AuthError::NoClientSecret.to_string();
    let secret = manager.client_secret().ok_or_else(no_secret)?;
    let pending =
        PendingAuthorization::start(&secret, &[YOUTUBE_SCOPE]).map_err(|e| e.to_string())?;
    let authorize_url = pending.authorize_url.clone();
    let attempt = CONNECT_ATTEMPTS.fetch_add(1, Ordering::SeqCst) + 1;
    let cancel = Arc::new(AtomicBool::new(false));
    if let Some((_, previous)) = connecting().replace((attempt, cancel.clone())) {
        previous.store(true, Ordering::SeqCst);
    }
    // The system browser: Google refuses sign-in inside embedded webviews, and the app's own
    // webview must never hold the consent page.
    if let Err(e) = crate::lastfm::open_browser(&authorize_url) {
        tracing::warn!(error = %e, "ytdata: could not open the browser for the authorization");
    }
    let app_state = state.inner().clone();
    tauri::async_runtime::spawn(async move {
        let finished = finish_connect(&app_state, manager, pending, cancel, attempt).await;
        {
            let mut slot = connecting();
            if slot.as_ref().is_some_and(|(n, _)| *n == attempt) {
                *slot = None;
            }
        }
        if finished.ok {
            tracing::info!("ytdata: channel connected");
        } else {
            tracing::info!(code = ?finished.code, "ytdata: connection ended without a channel");
        }
        crate::ytdata_status::announce(&app_state).await;
        let _ = app_state.app.emit(CONNECT_FINISHED_EVENT, &finished);
    });
    Ok(ConnectStarted { authorize_url, attempt })
}

/// Waits for the browser, then saves the channel ([`save_connected`]) on a blocking thread: the
/// token store blocks.
async fn finish_connect(
    state: &Arc<AppState>,
    manager: Arc<ytdata::auth::accounts::AccountManager>,
    pending: ytdata::auth::flow::PendingAuthorization,
    cancel: Arc<std::sync::atomic::AtomicBool>,
    attempt: u64,
) -> ConnectFinished {
    let tokens = match pending.wait_for_tokens(cancel, CONNECT_TIMEOUT).await {
        Ok(tokens) => tokens,
        Err(e) => return ConnectFinished::failed(attempt, &e),
    };
    let db = state.db.clone();
    let active = db.get_setting("active_account");
    let joined = tokio::task::spawn_blocking(move || {
        let saving = save_connected(&manager, &db, tokens, active.as_deref(), chrono::Utc::now());
        tokio::runtime::Handle::current().block_on(saving)
    })
    .await;
    match joined {
        Ok(Ok(account)) => ConnectFinished {
            attempt,
            ok: true,
            code: None,
            error: None,
            channel_id: Some(account.channel_id),
            title: Some(account.title),
        },
        Ok(Err(e)) => ConnectFinished::failed(attempt, &e),
        Err(e) => ConnectFinished::failed(attempt, &ytdata_io(e)),
    }
}

/// Something that went wrong around a Data API call, as the crate's error type.
fn ytdata_io(e: impl std::fmt::Display) -> ytdata::error::Error {
    ytdata::error::Error::Io(std::io::Error::other(e.to_string()))
}

/// Saves a channel the browser just authorized. The account manager names it (`channels.list`),
/// puts its refresh token in the token store and its row in `ytdata_accounts` (through
/// `DbAccountsRepo`, so the manager's own list stays in step with the table). Then the call's unit
/// goes to the ledger and the channel is linked to the signed-in cookie account when that one has
/// none yet. A Data API refusal is filed with `ytdata_status` (an API turned off shows as such).
/// The token store blocks: call this off the async runtime.
async fn save_connected(
    manager: &ytdata::auth::accounts::AccountManager,
    db: &crate::db::Db,
    tokens: ytdata::auth::flow::AuthorizedTokens,
    active: Option<&str>,
    now: chrono::DateTime<chrono::Utc>,
) -> Result<crate::ytdata_accounts::YtDataAccount, ytdata::error::Error> {
    let account = match manager.add_account(tokens).await {
        Ok(account) => account,
        Err(e) => {
            crate::ytdata_status::note_error(db, &e, now);
            return Err(e);
        }
    };
    crate::ytdata_status::note_success(db);
    let (channels_list, channel) = (ytdata::quota::endpoint::CHANNELS_LIST, &account.channel_id);
    if let Err(e) = crate::quota::record(db, channels_list, Some(channel.as_str()), None, now) {
        tracing::warn!(error = %e, "ytdata: could not record the channels.list unit");
    }
    link_if_unpaired(db, &account.channel_id, active).map_err(ytdata_io)?;
    crate::ytdata_accounts::get(db, &account.channel_id)
        .map_err(ytdata_io)?
        .ok_or_else(|| ytdata_io("the connected channel was not saved"))
}

/// Links `channel_id` to the cookie account `active` when neither is linked to anything yet.
/// Answers whether it linked.
fn link_if_unpaired(
    db: &crate::db::Db,
    channel_id: &str,
    active: Option<&str>,
) -> rusqlite::Result<bool> {
    let Some(active) = active.filter(|a| !a.is_empty()) else { return Ok(false) };
    let accounts = crate::ytdata_accounts::list(db)?;
    let taken = accounts.iter().any(|a| a.linked_account.as_deref() == Some(active));
    match accounts.iter().find(|a| a.channel_id == channel_id) {
        Some(a) if a.linked_account.is_none() && !taken => {
            crate::ytdata_accounts::set_linked_account(db, channel_id, Some(active))
        }
        _ => Ok(false),
    }
}

/// Pairs `channel_id` with the cookie account `account` (or unpairs it with `None`). A cookie
/// account has one channel at most: whichever was linked to it before is unlinked. Answers
/// whether the channel exists.
fn link_account(
    db: &crate::db::Db,
    channel_id: &str,
    account: Option<&str>,
) -> rusqlite::Result<bool> {
    let accounts = crate::ytdata_accounts::list(db)?;
    if !accounts.iter().any(|a| a.channel_id == channel_id) {
        return Ok(false);
    }
    if let Some(account) = account {
        let others = accounts.iter().filter(|a| a.channel_id != channel_id);
        for other in others.filter(|a| a.linked_account.as_deref() == Some(account)) {
            crate::ytdata_accounts::set_linked_account(db, &other.channel_id, None)?;
        }
    }
    crate::ytdata_accounts::set_linked_account(db, channel_id, account)
}

/// Gives up on the connection in progress, if any. Its end still arrives as the event.
#[tauri::command]
pub fn ytdata_connect_cancel() {
    if let Some((_, cancel)) = connecting().take() {
        cancel.store(true, std::sync::atomic::Ordering::SeqCst);
    }
}

/// A connected channel as the settings list it. No token, ever.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct YtDataAccountView {
    channel_id: String,
    title: String,
    thumb: Option<String>,
    /// `connected` or `reauth_required`.
    status: ytdata::auth::accounts::AccountStatus,
    /// The cookie account (`accounts.id`) it is paired with.
    linked_account: Option<String>,
    /// Epoch seconds.
    added_at: i64,
}

#[tauri::command]
pub fn ytdata_accounts(state: St<'_>) -> Result<Vec<YtDataAccountView>, String> {
    let rows = crate::ytdata_accounts::list(&state.db).map_err(db_err)?;
    Ok(rows
        .into_iter()
        .map(|a| YtDataAccountView {
            channel_id: a.channel_id,
            title: a.title,
            thumb: a.thumb,
            status: a.status,
            linked_account: a.linked_account,
            added_at: a.added_at,
        })
        .collect())
}

/// Disconnects a channel: revokes its refresh token with Google (best effort), deletes it from the
/// token store and removes its row. Its jobs stay, unowned (`ON DELETE SET NULL`); its playlists
/// and ledger rows are not touched.
#[tauri::command]
pub async fn ytdata_disconnect(
    state: St<'_>,
    jobs: Jobs<'_>,
    channel_id: String,
) -> Result<(), String> {
    ytdata::auth::store::validate_account_id(&channel_id).map_err(|e| e.to_string())?;
    let manager = jobs.account_manager();
    let store = crate::ytdata_secrets::token_store(&state.app);
    let db = state.db.clone();
    let id = channel_id.clone();
    tokio::task::spawn_blocking(move || -> Result<(), String> {
        match manager {
            Some(m) if m.account(&id).is_some() => {
                let removing = m.revoke_and_remove_account(&id);
                tokio::runtime::Handle::current().block_on(removing).map_err(|e| e.to_string())?;
            }
            // The manager never knew it (no manager this session): the token still goes.
            _ => {
                if let Err(e) = store.delete(&id) {
                    tracing::warn!(error = %e, "ytdata: could not delete a stored token");
                }
            }
        }
        crate::ytdata_accounts::remove(&db, &id).map_err(db_err)?;
        Ok(())
    })
    .await
    .map_err(|e| e.to_string())??;
    tracing::info!("ytdata: channel disconnected");
    crate::ytdata_status::announce(&state).await;
    let _ = state.app.emit("jobs-changed", json!({}));
    Ok(())
}

/// Pairs a connected channel with a cookie account (`accounts.id`), or unpairs it with `null`:
/// the Data API acts as that channel while that account is signed in.
#[tauri::command]
pub async fn ytdata_link_account(
    state: St<'_>,
    channel_id: String,
    account: Option<String>,
) -> Result<(), String> {
    let account = account.filter(|a| !a.is_empty());
    if let Some(account) = &account {
        if !state.db.list_accounts().iter().any(|a| &a.id == account) {
            return Err("That account is not signed in on this computer.".into());
        }
    }
    if !link_account(&state.db, &channel_id, account.as_deref()).map_err(db_err)? {
        return Err("That channel is not connected.".into());
    }
    crate::ytdata_status::announce(&state).await;
    Ok(())
}

/// The budget's settings as they read, and today's partition of the quota. The settings are
/// written with `set_setting`, which validates them.
#[tauri::command]
pub async fn budget_get(state: St<'_>) -> Result<crate::jobs::budget::Snapshot, String> {
    crate::jobs::budget::snapshot(&state.db, chrono::Utc::now()).map_err(db_err)
}

/// Alerts neither seen nor dismissed: the badge's number before any `alerts-changed` arrives.
#[tauri::command]
pub fn unseen_alert_count(state: St<'_>) -> u32 {
    state.db.unseen_alert_count()
}

/// The monitor page's cards: `{ playlists, items, unavailable, duplicates_estimate }`.
#[tauri::command]
pub fn monitor_stats(state: St<'_>) -> crate::db::MonitorStats {
    state.db.monitor_stats()
}

/// The newest monitor runs, newest first: 20 unless `limit` says otherwise (at most 500).
#[tauri::command]
pub fn monitor_runs(state: St<'_>, limit: Option<u32>) -> Vec<crate::db::MonitorRun> {
    state.db.monitor_runs(limit.unwrap_or(20).min(500) as usize)
}

/// Every alert filed over the last `days` days (14 unless said, at most 366), oldest first, as
/// `{ at, kind }`. Raw rows rather than per-day sums: a day is the viewer's local day, which the
/// page knows and this does not.
#[tauri::command]
pub fn alerts_by_day(state: St<'_>, days: Option<u32>) -> Vec<crate::db::AlertStamp> {
    let days = i64::from(days.unwrap_or(14).clamp(1, 366));
    state.db.alerts_since(now_secs() - days * 86_400)
}

/// `false` means the playlist already had the track and YouTube added nothing — not an error, but
/// the UI must not draw an optimistic row for it (there is no real row to remove later).
/// With `allow_duplicates` set to `true`, `true` is returned for a duplicate that was added on purpose.
#[tauri::command]
pub async fn add_to_playlist(
    state: St<'_>,
    playlist_id: String,
    video_id: String,
    allow_duplicates: Option<bool>,
) -> Result<bool, String> {
    let client = editable_playlist(&state, &playlist_id)?;
    let added = state
        .it
        .playlist_add(client, &playlist_id, &video_id, allow_duplicates.unwrap_or(false))
        .await
        .map_err(|e| e.to_string())?;
    // Also on `false`: YouTube refusing a duplicate means the playlist holds the track, which is
    // exactly what the index should say. A stale index is how it got asked in the first place.
    state.db.add_playlist_track(&playlist_id, &video_id);
    Ok(added)
}

#[tauri::command]
pub async fn remove_from_playlist(
    state: St<'_>,
    playlist_id: String,
    video_id: String,
    set_video_id: String,
) -> Result<(), String> {
    if is_local_playlist(&playlist_id) {
        let row = set_video_id.parse().map_err(|_| GONE.to_string())?;
        return remove_local_rows(&state, &playlist_id, &[row]);
    }
    let client = editable_playlist(&state, &playlist_id)?;
    state
        .it
        .playlist_remove(client, &playlist_id, &video_id, &set_video_id)
        .await
        .map_err(|e| e.to_string())?;
    state.db.remove_playlist_track(&playlist_id, &video_id);
    Ok(())
}

/// Remove several tracks from one playlist in a single request (the bulk bar's Remove).
///
/// All or nothing: YouTube applies the whole action list or rejects it, so the UI can revert its
/// optimistic removal on an error without working out which rows made it.
#[tauri::command]
pub async fn remove_many_from_playlist(
    state: St<'_>,
    playlist_id: String,
    tracks: Vec<(String, String)>,
) -> Result<(), String> {
    if is_local_playlist(&playlist_id) {
        let rows = tracks
            .iter()
            .map(|(_, row)| row.parse().map_err(|_| GONE.to_string()))
            .collect::<Result<Vec<i64>, _>>()?;
        return remove_local_rows(&state, &playlist_id, &rows);
    }
    let client = editable_playlist(&state, &playlist_id)?;
    state
        .it
        .playlist_remove_many(client, &playlist_id, &tracks)
        .await
        .map_err(|e| e.to_string())?;
    for (video_id, _) in &tracks {
        state.db.remove_playlist_track(&playlist_id, video_id);
    }
    Ok(())
}

/// `local`: keep it on this machine instead of the account (issue #251), which is the only kind
/// there is while signed out. Answers the new playlist's id, a `LOCALPLAYLIST:` browseId for that.
#[tauri::command]
pub async fn create_playlist(
    state: St<'_>,
    title: String,
    local: Option<bool>,
) -> Result<String, String> {
    if local.unwrap_or(false) {
        let title = title.trim();
        if title.is_empty() {
            return Err("Give the playlist a name.".into());
        }
        let id = state.db.create_local_playlist(title, crate::db::now_secs()).map_err(db_err)?;
        return Ok(format!("{LOCAL_PLAYLIST_PREFIX}{id}"));
    }
    let client = require_login(&state)?;
    state.it.create_playlist(client, &title).await.map_err(|e| e.to_string())
}

/// Edit a playlist you own, from the "Edit playlist" dialog: name, description, visibility.
///
/// Each field is `None` when the user left it alone, and only what changed is sent: an edit of
/// the name must not blank a description we failed to read back off the page.
#[tauri::command]
pub async fn edit_playlist_details(
    state: St<'_>,
    playlist_id: String,
    name: Option<String>,
    description: Option<String>,
    public: Option<bool>,
) -> Result<(), String> {
    // Nobody else can see a playlist on this machine, so there is no visibility to set.
    if is_local_playlist(&playlist_id) {
        let name = name.as_deref().map(str::trim);
        if name == Some("") {
            return Err("Give the playlist a name.".into());
        }
        let key = local_key(&playlist_id)?;
        return state
            .db
            .edit_local_playlist(key, name, description.as_deref(), crate::db::now_secs())
            .map_err(db_err);
    }
    let client = editable_playlist(&state, &playlist_id)?;
    // The switch is two-state; YouTube's third value (UNLISTED) is only ever left as it was.
    let privacy = public.map(|p| if p { "PUBLIC" } else { "PRIVATE" });
    state
        .it
        .playlist_edit_details(
            client,
            &playlist_id,
            name.as_deref(),
            description.as_deref(),
            privacy,
        )
        .await
        .map_err(|e| e.to_string())
}

/// Custom playlist artwork, in both places it lives.
///
/// Setting one is local-first: the picked image is copied in beside the local-music covers and
/// answered straight back, then pushed to YouTube Music in the background (`sync_cover`), because
/// the upload is three round trips and nobody should watch a spinner for their own file.
///
/// Dropping one waits, and that is deliberate. Once a cover has been up there, YouTube's own
/// thumbnail *is* that cover, so a local-first removal would fall back to the very image being
/// removed and only reach the rebuilt collage a beat later: two swaps, the first of them pointless.
/// The clear is a single small call, so it answers with the thumbnail YouTube rebuilt and the UI
/// changes once.
#[tauri::command]
pub async fn set_playlist_cover(
    app: tauri::AppHandle,
    state: St<'_>,
    playlist_id: String,
    path: Option<String>,
) -> Result<CoverResult, String> {
    let key = cover_key(&playlist_id);
    let stored = state.db.get_setting(&key);
    let Some(src) = path else {
        // YouTube first, so the local copy is still on screen while it answers. Its refusal is
        // never fatal though: dropping the cover from this machine is what the user clicked, and
        // an account that was not allowed to set one up there has nothing to clear anyway.
        let thumbnail = match clear_cover_on_youtube(&state, &playlist_id).await {
            Ok(t) => {
                state.db.delete_setting(&synced_key(&playlist_id));
                t
            }
            Err(e) => {
                tracing::warn!(playlist_id, error = %e, "custom cover not cleared on YouTube Music");
                // Only worth saying when a cover of ours actually reached the account: otherwise
                // there was nothing up there to keep, and the warning would be a lie.
                if state.db.get_setting(&synced_key(&playlist_id)).is_some() {
                    let _ = state.app.emit(
                        "cover-error",
                        serde_json::json!({
                            "message": "Removed here, but YouTube Music kept its copy.",
                        }),
                    );
                }
                None
            }
        };
        state.db.delete_setting(&key);
        if let Some(old) = stored {
            let _ = std::fs::remove_file(old);
        }
        return Ok(CoverResult { cover: None, thumbnail });
    };
    store_cover(&app, &state, &playlist_id, std::path::Path::new(&src))
}

/// Copy an image in as a playlist's artwork, answer the local copy, and send it on to YouTube
/// Music in the background. The picker's half of [`set_playlist_cover`], and how a Spotify import
/// carries the playlist's own cover over (#375).
pub(crate) fn store_cover(
    app: &tauri::AppHandle,
    state: &Arc<AppState>,
    playlist_id: &str,
    src: &std::path::Path,
) -> Result<CoverResult, String> {
    use tauri::Manager;
    // What YouTube's uploader will take. WebP is not on the list: it answers 415 for one, and a
    // cover that only works on this machine is worse than one the picker never offered.
    const IMAGE_EXTS: [&str; 3] = ["jpg", "jpeg", "png"];

    let key = cover_key(playlist_id);
    let stored = state.db.get_setting(&key);
    let ext = src.extension().and_then(|e| e.to_str()).unwrap_or_default().to_ascii_lowercase();
    if !IMAGE_EXTS.contains(&ext.as_str()) {
        return Err("Pick a JPEG or PNG image: YouTube Music won't take anything else.".into());
    }
    // ponytail: a flat size cap instead of downscaling. It keeps a 40px sidebar thumb from
    // decoding a camera raw in the webview and the upload from swallowing one; reach for the
    // `image` crate and a real resize only if 8 MB turns out to bother anyone.
    const MAX_BYTES: u64 = 8 * 1024 * 1024;
    if src.metadata().map(|m| m.len()).unwrap_or(0) > MAX_BYTES {
        return Err("That image is over 8 MB. Pick a smaller one.".into());
    }
    let dir = crate::local::covers_dir(app).join("playlists");
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    // Timestamped, so replacing a cover can't be served out of the webview's cache under the name
    // it already has. The id is filtered to filename characters rather than trusted: it arrives
    // from the UI, and a `..` in it would write outside this directory.
    let stem: String = playlist_id
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .collect();
    let dest = dir.join(format!("{stem}-{}.{ext}", crate::db::now_secs()));
    std::fs::copy(src, &dest).map_err(|e| e.to_string())?;
    // Only now is the cover it replaces safe to unlink. Dropping it any earlier means a picked
    // file this command goes on to refuse (wrong format, too big, unreadable) takes the artwork
    // already on screen down with it, and the toast talks about the new file while the old one is
    // the thing that just disappeared.
    if let Some(old) = stored {
        let _ = std::fs::remove_file(old);
    }
    let dest = dest.to_string_lossy().to_string();
    // The covers directory is allowed recursively at startup, but the first cover on a fresh
    // install is written after that ran, so name this file explicitly too.
    let _ = app.asset_protocol_scope().allow_file(&dest);
    state.db.set_setting(&key, &dest);
    sync_cover(state, playlist_id, dest.clone());
    Ok(CoverResult { cover: Some(dest), thumbnail: None })
}

/// What the UI needs to draw after a cover changed: where the local copy is, and (on a removal)
/// the thumbnail YouTube rebuilt in its place.
#[derive(serde::Serialize)]
pub struct CoverResult {
    cover: Option<String>,
    thumbnail: Option<String>,
}

/// Send the cover on to YouTube Music behind the picker's back: the local copy is already on
/// screen, and the upload is a three-call round trip nobody should wait through.
///
/// A failure is a toast, not a rollback: the artwork is still right here, and it is still this
/// playlist's cover on this machine. Signed out (or On Repeat, which YouTube has never heard of),
/// there is nothing to sync and local is all there ever was.
fn sync_cover(state: &Arc<AppState>, playlist_id: &str, path: String) {
    if playlist_id == ON_REPEAT_ID || is_local_playlist(playlist_id) || !state.it.is_logged_in() {
        return;
    }
    let state = Arc::clone(state);
    let playlist_id = playlist_id.to_owned();
    tauri::async_runtime::spawn(async move {
        let Some(client) = state.clients.get(innertube::METADATA_CLIENT) else {
            return;
        };
        // Read here, not on the command's thread: the file was just written and the caller has its
        // answer already.
        let result = match std::fs::read(&path) {
            Ok(image) => state.it.playlist_set_cover(client, &playlist_id, image).await,
            Err(e) => Err(innertube::Error::Other(e.to_string())),
        };
        match result {
            // Remembered so a later removal knows whether YouTube has anything of ours to drop.
            Ok(()) => state.db.set_setting(&synced_key(&playlist_id), "1"),
            Err(e) => {
                tracing::warn!(playlist_id, error = %e, "playlist cover didn't reach YouTube Music");
                let message = match e {
                    // The one refusal with a known cause and no fix inside this app. Say it once,
                    // plainly, and leave the cover where it already is: on this machine.
                    innertube::Error::CoverRefused => format!("Artwork saved on this device. {e}"),
                    e => format!("Artwork saved here, but the upload to YouTube Music failed: {e}"),
                };
                let _ = state.app.emit("cover-error", serde_json::json!({ "message": message }));
            }
        }
    });
}

/// Drop the custom thumbnail from the account, answering the one YouTube rebuilt from the tracks.
/// Nothing to do (and nothing to answer with) when there is no account behind the playlist.
async fn clear_cover_on_youtube(
    state: &Arc<AppState>,
    playlist_id: &str,
) -> Result<Option<String>, String> {
    if playlist_id == ON_REPEAT_ID || is_local_playlist(playlist_id) || !state.it.is_logged_in() {
        return Ok(None);
    }
    let client = metadata_client(state)?;
    state.it.playlist_clear_cover(client, playlist_id).await.map_err(|e| e.to_string())
}

fn cover_key(playlist_id: &str) -> String {
    // Browse ids arrive `VL`-prefixed and playlist ids don't; one playlist, one key either way.
    format!("playlist_cover:{}", playlist_id.strip_prefix("VL").unwrap_or(playlist_id))
}

/// Set once a cover of ours has actually landed on the account, so a removal knows whether there
/// is anything up there to warn about failing to clear.
fn synced_key(playlist_id: &str) -> String {
    format!("{}:synced", cover_key(playlist_id))
}

/// The custom artwork stored for a playlist, if the file is still there. The user owns that
/// directory and can empty it, and a dead path renders as a broken image.
fn custom_cover(state: &Arc<AppState>, playlist_id: &str) -> Option<String> {
    let path = state.db.get_setting(&cover_key(playlist_id))?;
    std::path::Path::new(&path).is_file().then_some(path)
}

#[tauri::command]
pub async fn delete_playlist(state: St<'_>, playlist_id: String) -> Result<(), String> {
    delete_playlist_inner(&state, &playlist_id).await
}

/// `delete_playlist`, for the playlist tools' undo of a playlist they created.
pub(crate) async fn delete_playlist_inner(
    state: &Arc<AppState>,
    playlist_id: &str,
) -> Result<(), String> {
    if is_local_playlist(playlist_id) {
        state.db.delete_local_playlist(local_key(playlist_id)?).map_err(db_err)?;
        // Its artwork was a copy made for it, so it goes too.
        if let Some(cover) = state.db.get_setting(&cover_key(playlist_id)) {
            let _ = std::fs::remove_file(cover);
            state.db.delete_setting(&cover_key(playlist_id));
        }
        return Ok(());
    }
    let client = editable_playlist(state, playlist_id)?;
    state.it.delete_playlist(client, playlist_id).await.map_err(|e| e.to_string())?;
    state.db.forget_playlist(playlist_id);
    Ok(())
}

// --- playlist tools (playlist_tools/) ----------------------------------------------------------
// Each edit answers its journal entry (`OpRecord`), which the UI turns into an "Undo" toast, and
// announces the playlists it touched (`playlists-edited`) so any open page re-reads them.

/// Put a playlist in `order` (row handles: `set_video_id`s). `None` when nothing had to move.
#[tauri::command]
pub async fn reorder_playlist(
    state: St<'_>,
    playlist_id: String,
    title: String,
    order: Vec<String>,
) -> Result<Option<OpRecord>, String> {
    let (before, moved) = rows::reorder(&state, &playlist_id, &order, &|| false).await?;
    if moved == 0 {
        return Ok(None);
    }
    // Your order, not news: the next sync compares against it and files no `moved` for it.
    monitor::after_reorder(&state.db, &playlist_id, &before, &order, now_secs());
    journal::announce(&state, std::slice::from_ref(&playlist_id));
    let summary =
        Summary { playlists: vec![Named { id: playlist_id.clone(), title }], count: moved };
    Ok(journal::record(&state, "reorder", &summary, &journal::reorder_undo(&playlist_id, before)))
}

/// Take rows out of a playlist, undoably. Each row comes with the handle of the row after it that
/// stays (`before`), which is where an undo puts it back. `kind` names it in the history: a plain
/// removal, or the duplicate finder's.
///
/// With the job queue on (`jobs::engine`), an account playlist's removal runs as a job, on
/// InnerTube or the Data API; this waits for it to settle and answers its journal entry, or
/// `None` while it still waits its turn. `engine` overrides `playlist_engine` for this call.
#[tauri::command]
#[allow(clippy::too_many_arguments)]
pub async fn remove_tracks(
    state: St<'_>,
    jobs: Jobs<'_>,
    playlist_id: String,
    title: String,
    rows: Vec<Restore>,
    kind: Option<String>,
    engine: Option<String>,
) -> Result<Option<OpRecord>, String> {
    let kind = if kind.as_deref() == Some("dedupe") { "dedupe" } else { "remove" };
    let est = crate::jobs::planner::estimate_remove_units(rows.len(), rows.len());
    let touched = [playlist_id.as_str()];
    if let Some(q) = queue_target(&state, &jobs, &touched, est, engine.as_deref()) {
        let occurrences = if q.engine == crate::jobs::engine::Engine::Ytdata {
            let picked: Vec<SongItem> = rows.iter().map(|r| r.song.clone()).collect();
            let current = rows::read_all(&state, &playlist_id).await.unwrap_or_default();
            crate::jobs::planner::occurrences(&current, &picked)
        } else {
            Vec::new()
        };
        let named = Named { id: playlist_id.clone(), title };
        let Some(new) = dedup::removal_job(&named, &rows, kind, &occurrences, &q) else {
            return Ok(None);
        };
        let job = enqueue_and_wait(&state, &jobs, &new).await?;
        return Ok(job_op(&state, job.as_ref()).await);
    }
    let songs: Vec<SongItem> = rows.iter().map(|r| r.song.clone()).collect();
    rows::remove_rows(&state, &playlist_id, &songs, &|| false).await?;
    journal::announce(&state, std::slice::from_ref(&playlist_id));
    let summary =
        Summary { playlists: vec![Named { id: playlist_id.clone(), title }], count: songs.len() };
    let undo = [journal::restore_step(&playlist_id, &rows)];
    Ok(journal::record(&state, kind, &summary, &undo))
}

/// The whole playlist and the duplicate clusters in it (`playlist_tools::dedup`). Reads every page
/// of an account playlist, so the indices line up with rows the UI may not have scrolled to yet.
#[derive(serde::Serialize)]
pub struct DuplicateReport {
    rows: Vec<SongItem>,
    clusters: Vec<dedup::Cluster>,
}

#[tauri::command]
pub async fn find_duplicates(
    state: St<'_>,
    playlist_id: String,
    options: dedup::Options,
    keep: dedup::Keep,
) -> Result<DuplicateReport, String> {
    let rows = rows::read_all(&state, &playlist_id).await?;
    let clusters = dedup::find_clusters(&rows, &options, keep);
    Ok(DuplicateReport { rows, clusters })
}

/// Copy or move tracks into another playlist: a drop on a sidebar playlist, or "Move to…".
/// `source` is the playlist they came from (`None` for a list that isn't one, which can only copy).
///
/// With the job queue on (`jobs::engine`), a write to account playlists runs as a job: on
/// InnerTube in `unified` mode, or on the Data API when that is the engine (a move then copies,
/// verifies the copies landed, and only then deletes). This waits for the job to settle and
/// answers what it did, with `job_id` set; the counts are zero while it still waits its turn.
/// `engine` overrides `playlist_engine` for this call.
#[tauri::command]
#[allow(clippy::too_many_arguments)]
pub async fn transfer_tracks(
    state: St<'_>,
    jobs: Jobs<'_>,
    source: Option<String>,
    source_title: Option<String>,
    target: String,
    target_title: String,
    rows: Vec<Restore>,
    mode: transfer::Mode,
    duplicates: transfer::Duplicates,
    engine: Option<String>,
) -> Result<transfer::Transferred, String> {
    let req = transfer::Request {
        source: source.map(|id| Named { id, title: source_title.unwrap_or_default() }),
        target: Named { id: target, title: target_title },
        rows,
        mode,
        duplicates,
    };
    let moving = transfer::is_move(&req);
    let mut touched = vec![req.target.id.as_str()];
    if let (true, Some(source)) = (moving, &req.source) {
        touched.push(source.id.as_str());
    }
    let est = crate::jobs::planner::estimate_transfer_units(req.rows.len(), moving);
    let Some(q) = queue_target(&state, &jobs, &touched, est, engine.as_deref()) else {
        return transfer::run(&state, req).await;
    };
    let index = state.db.playlist_memberships();
    let (known, add) = transfer::split_known(&req.rows, &req.target.id, req.duplicates, &index);
    let occurrences = match (&req.source, q.engine) {
        (Some(source), crate::jobs::engine::Engine::Ytdata) if moving => {
            let picked: Vec<SongItem> = req.rows.iter().map(|r| r.song.clone()).collect();
            let current = rows::read_all(&state, &source.id).await.unwrap_or_default();
            crate::jobs::planner::occurrences(&current, &picked)
        }
        _ => Vec::new(),
    };
    let Some(new) = transfer::queued_job(&req, &known, &add, &occurrences, &q) else {
        // Everything was already there: nothing to write.
        let mut nothing = transfer::from_job(0, &[], known.len());
        nothing.job_id = None;
        return Ok(nothing);
    };
    let job = enqueue_and_wait(&state, &jobs, &new).await?;
    let Some(job) = job else { return Err("The queued job is gone.".into()) };
    let op = job_op(&state, Some(&job)).await;
    let items = crate::jobs::repo::list_job_items(&state.db, job.id).map_err(db_err)?;
    let mut done = transfer::from_job(job.id, &items, known.len());
    done.op = op;
    Ok(done)
}

// --- job queue dispatch (jobs/) ------------------------------------------------------------------
// The playlist tools' writes to account playlists go through the queue when `jobs::engine` says
// so, keeping their commands' signatures: the command queues, waits for the job to settle, and
// answers from it. Local playlists never queue.

type Jobs<'a> = State<'a, Arc<crate::jobs::JobsState>>;

/// How long a command waits for its job before answering "still queued".
const QUEUED_WAIT: std::time::Duration = std::time::Duration::from_secs(300);

/// Where a write to `playlists` goes: `Some` to queue it (engine, channel, account, priority),
/// `None` to run it directly as before (a local playlist, or InnerTube with the Data-API-only
/// queue). See `jobs::engine::resolve_engine`.
pub(crate) fn queue_target(
    state: &Arc<AppState>,
    jobs: &crate::jobs::JobsState,
    playlists: &[&str],
    est_units: i64,
    override_: Option<&str>,
) -> Option<crate::jobs::engine::QueueTarget> {
    use crate::jobs::engine::{self, Engine, QueueMode, QueueTarget};
    let r = route(state, jobs, playlists, est_units, override_)?;
    let db = &state.db;
    let priority = engine::default_job_priority(db);
    match r.engine {
        Engine::Ytdata => Some(QueueTarget {
            engine: r.engine,
            channel_id: r.channel_id,
            account: r.account,
            priority,
        }),
        Engine::Innertube if engine::queue_mode(db) == QueueMode::Unified => {
            Some(QueueTarget { engine: r.engine, channel_id: None, account: r.account, priority })
        }
        Engine::Innertube => None,
    }
}

/// Which engine a write to `playlists` would run on now, and what decided it. `None` for a write
/// that touches a playlist on this computer (those never queue and cost no quota).
struct Route {
    engine: crate::jobs::engine::Engine,
    /// The Data API's state for these playlists: `not_configured` when one of them is not a
    /// playlist the Data API can address (Liked Music, an album).
    state: crate::jobs::engine::DataApiState,
    channel_id: Option<String>,
    account: Option<String>,
    available: i64,
}

fn route(
    state: &Arc<AppState>,
    jobs: &crate::jobs::JobsState,
    playlists: &[&str],
    est_units: i64,
    override_: Option<&str>,
) -> Option<Route> {
    use crate::jobs::engine::{self, DataApiState, EngineSetting};
    if playlists.iter().any(|id| is_local_playlist(id)) {
        return None;
    }
    let db = &state.db;
    let now = chrono::Utc::now();
    let account = db.get_setting("active_account");
    let addressable =
        playlists.iter().all(|id| crate::jobs::planner::ytdata_playlist_id(id).is_some());
    let ctx = engine::data_api_state(db, jobs.client_secret_present(), account.as_deref(), now);
    let status = if addressable { ctx.state } else { DataApiState::NotConfigured };
    let available = crate::jobs::budget::available_for_jobs_now(db, now).unwrap_or(0);
    let chosen = engine::resolve_engine(
        engine::playlist_engine(db),
        override_.and_then(EngineSetting::parse),
        status,
        est_units,
        available,
    );
    Some(Route { engine: chosen, state: status, channel_id: ctx.channel_id, account, available })
}

/// What a copy, move or removal would cost on the Data API, what jobs may still spend today, and
/// the engine it would run on with `params.engine` as the operation's choice (`auto`, `ytdata`,
/// `innertube`; none = the `playlist_engine` setting). `engine` is `None` for a write that touches
/// a playlist on this computer. For the confirmation's "≈ N u of M available".
#[derive(serde::Deserialize)]
pub struct EstimateParams {
    #[serde(default)]
    rows: usize,
    /// Every playlist the write touches: the target, and the source too for a move.
    #[serde(default)]
    playlists: Vec<String>,
    /// How long the playlist a removal reads is, when the caller knows.
    #[serde(default)]
    playlist_len: Option<usize>,
    #[serde(default)]
    engine: Option<String>,
}

#[derive(Debug, serde::Serialize)]
pub struct OpEstimate {
    units: i64,
    available: i64,
    engine: Option<crate::jobs::engine::Engine>,
    state: crate::jobs::engine::DataApiState,
}

#[tauri::command]
pub async fn estimate_op(
    state: St<'_>,
    jobs: Jobs<'_>,
    kind: String,
    params: EstimateParams,
) -> Result<OpEstimate, String> {
    use crate::jobs::engine::{estimate_units, DataApiState, OpKind};
    let op = OpKind::parse(&kind).ok_or_else(|| format!("unknown operation: {kind}"))?;
    let units = estimate_units(op, params.rows, params.playlist_len);
    let touched: Vec<&str> = params.playlists.iter().map(String::as_str).collect();
    let Some(r) = route(&state, &jobs, &touched, units, params.engine.as_deref()) else {
        // A playlist on this computer: nothing to spend.
        let available = crate::jobs::budget::available_for_jobs_now(&state.db, chrono::Utc::now());
        let state = DataApiState::NotConfigured;
        return Ok(OpEstimate { units: 0, available: available.unwrap_or(0), engine: None, state });
    };
    Ok(OpEstimate { units, available: r.available, engine: Some(r.engine), state: r.state })
}

/// Queues `new`, wakes the runner, and waits for the job to settle (or [`QUEUED_WAIT`]).
pub(crate) async fn enqueue_and_wait(
    state: &Arc<AppState>,
    jobs: &crate::jobs::JobsState,
    new: &crate::jobs::NewJob,
) -> Result<Option<crate::jobs::Job>, String> {
    let job_id =
        crate::jobs::repo::insert_job(&state.db, new, chrono::Utc::now()).map_err(db_err)?;
    {
        use tauri::Emitter;
        let _ = state.app.emit("jobs-changed", json!({ "job_id": job_id }));
    }
    jobs.nudge();
    Ok(jobs.wait_settled(&state.db, job_id, QUEUED_WAIT).await)
}

/// The journal entry a settled job left (`jobs::control::on_job_finished`). The runner writes it
/// just after the job's status, so a job that ended without one yet gets a moment for it.
pub(crate) async fn job_op(
    state: &Arc<AppState>,
    job: Option<&crate::jobs::Job>,
) -> Option<OpRecord> {
    let id = job?.id;
    for _ in 0..20 {
        let job = crate::jobs::repo::get_job(&state.db, id).ok().flatten()?;
        if let Some(op) = job.params.get("op_id").and_then(Value::as_i64) {
            return journal::get(state, op);
        }
        let items = crate::jobs::repo::list_job_items(&state.db, id).unwrap_or_default();
        if !job.status.is_terminal() || !crate::jobs::control::should_journal(&job, &items) {
            return None;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    None
}

/// Every row of a playlist, in its order, each with its handle (`set_video_id`) when it is yours to
/// edit: an account playlist is read page by page. What Tools ▸ Extract filters and Tools ▸ Reorder
/// rearranges, which both need the whole list and the rows' handles.
#[tauri::command]
pub async fn playlist_rows(state: St<'_>, playlist_id: String) -> Result<Vec<SongItem>, String> {
    rows::read_all(&state, &playlist_id).await
}

/// A split, worked out but not written: the whole playlist and which rows go into which part.
#[derive(serde::Serialize)]
pub struct SplitPlan {
    rows: Vec<SongItem>,
    parts: Vec<split::Part>,
}

#[tauri::command]
pub async fn plan_split(
    state: St<'_>,
    playlist_id: String,
    by: split::SplitBy,
    order: split::Order,
) -> Result<SplitPlan, String> {
    let rows = rows::read_all(&state, &playlist_id).await?;
    let parts = split::split(&rows, by, order);
    Ok(SplitPlan { rows, parts })
}

/// Several playlists merged into one list, not written yet. `into`: an existing playlist the merge
/// will be appended to, whose tracks are left out when deduplicating.
#[tauri::command]
pub async fn merge_preview(
    state: St<'_>,
    ids: Vec<String>,
    how: merge::Interleave,
    dedupe: bool,
    into: Option<String>,
) -> Result<Vec<SongItem>, String> {
    let mut lists = Vec::with_capacity(ids.len());
    for id in &ids {
        lists.push(rows::read_all(&state, id).await?);
    }
    let skip = match (&into, dedupe) {
        (Some(target), true) => {
            rows::read_all(&state, target).await?.into_iter().map(|s| s.video_id).collect()
        }
        _ => Default::default(),
    };
    Ok(merge::merge(&lists, how, dedupe, &skip))
}

/// Write a split, a merge or an extract (`playlist_tools::build`): new playlists, or one existing
/// playlist appended to. `mode` only means something to an extract from one playlist: a move takes
/// the rows out of it once they are in (copy when left out). Progress arrives as
/// `playlist-op-progress`; `cancel_playlist_build` stops it.
#[tauri::command]
pub async fn build_playlists(
    state: St<'_>,
    kind: String,
    sources: Vec<Named>,
    lists: Vec<build::NewList>,
    dest: build::Dest,
    mode: Option<transfer::Mode>,
) -> Result<build::Built, String> {
    let kind = build::journal_kind(&kind);
    build::run(&state, kind, sources, lists, dest, mode.unwrap_or(transfer::Mode::Copy)).await
}

#[tauri::command]
pub fn cancel_playlist_build() {
    build::cancel();
}

/// Write the whole playlist to `path` (picked in the save dialog) as CSV, JSON or M3U8. Answers
/// how many tracks went out.
#[tauri::command]
pub async fn export_playlist(
    state: St<'_>,
    playlist_id: String,
    title: String,
    format: export::Format,
    path: String,
) -> Result<usize, String> {
    let rows = rows::read_all(&state, &playlist_id).await?;
    let text = export::render(format, &title, &rows)?;
    std::fs::write(&path, text).map_err(|e| e.to_string())?;
    Ok(rows.len())
}

/// Every song in your playlists, once each, with the playlists holding it. From the index: no
/// network, and empty until the first sync after sign-in has filled in the metadata.
#[tauri::command]
pub fn songs_everywhere(state: St<'_>) -> Vec<everywhere::Everywhere> {
    everywhere::group(state.db.indexed_songs())
}

/// Keep these songs in `target` only, or with no target, take them out of every playlist.
#[tauri::command]
pub async fn keep_only_in(
    state: St<'_>,
    songs: Vec<SongItem>,
    target: Option<Named>,
    titles: std::collections::HashMap<String, String>,
) -> Result<everywhere::Kept, String> {
    everywhere::keep_only_in(&state, songs, target, titles).await
}

/// Every copy of a song across your playlists, for the "+N" dialog. From the index and the latest
/// snapshots: no network. The edits it leads to read each playlist fresh.
#[tauri::command]
pub fn song_occurrences(state: St<'_>, video_id: String) -> Vec<everywhere::Occurrence> {
    everywhere::song_occurrences(&state.db, &video_id)
}

/// The recovery assistant's list (F4): every dead track of your account playlists worth replacing,
/// once per playlist and track, with the title the app has seen for it where its row lost it
/// (`playlist_tools::recover`). Dismissed alerts count only with `include_dismissed`. Reads the
/// database only, off the async workers, and opens or refreshes the assistant's session with a row
/// per candidate (a row already there keeps its progress).
#[tauri::command]
pub async fn recover_candidates(
    state: St<'_>,
    include_dismissed: Option<bool>,
) -> Result<Vec<crate::playlist_tools::recover::RecoverCandidate>, String> {
    let db = state.db.clone();
    let include = include_dismissed.unwrap_or(false);
    tauri::async_runtime::spawn_blocking(move || {
        crate::playlist_tools::recover::load_candidates(&db, include)
    })
    .await
    .map_err(|e| e.to_string())
}

/// The assistant session's rows: these keys (unknown ones skipped), or every row.
#[tauri::command]
pub fn recover_rows(keys: Option<Vec<String>>) -> Vec<crate::playlist_tools::recover::RecoverRow> {
    crate::playlist_tools::recover::rows(keys.as_deref())
}

/// Look up a title for each of these rows that has none: the stored lookups first, then, with
/// `wayback`, the Wayback Machine, slowly. In the background: `recover-progress` follows it.
/// `busy` while another step runs.
#[tauri::command]
pub fn recover_titles(
    state: St<'_>,
    keys: Vec<String>,
    wayback: bool,
) -> Result<crate::playlist_tools::recover::RecoverSnapshot, String> {
    crate::playlist_tools::recover::start_titles(&state, keys, wayback)
}

/// Search YouTube Music for a replacement of each of these rows that has a title and was not
/// searched yet, paced and budgeted with the Spotify import. In the background:
/// `recover-progress` follows it. `busy`, or `cooldown:<until>` while YouTube is left alone.
#[tauri::command]
pub fn recover_search(
    state: St<'_>,
    keys: Vec<String>,
) -> Result<crate::playlist_tools::recover::RecoverSnapshot, String> {
    crate::playlist_tools::recover::start_search(&state, keys)
}

/// The user's replacement for a row (`None` for none) and whether it is approved. `gone` when the
/// row is not in the session.
#[tauri::command]
pub fn recover_pick(
    state: St<'_>,
    key: String,
    song: Option<SongItem>,
    approved: bool,
) -> Result<crate::playlist_tools::recover::RecoverRow, String> {
    crate::playlist_tools::recover::pick(&state, &key, song, approved)
}

/// A title typed in for a row; its search starts over. `gone` when the row is not in the session.
#[tauri::command]
pub fn recover_set_title(
    state: St<'_>,
    key: String,
    title: String,
    artists: Option<String>,
) -> Result<crate::playlist_tools::recover::RecoverRow, String> {
    crate::playlist_tools::recover::set_title(&state, &key, &title, artists.as_deref())
}

/// The song a pasted videoId is, for a row's replacement: the first entry of its "watch next"
/// panel, which is the video itself. `None` when YouTube has nothing playable under it;
/// `invalid_id` for anything that is not an 11-character id (nothing is sent).
#[tauri::command]
pub async fn recover_song(state: St<'_>, video_id: String) -> Result<Option<SongItem>, String> {
    let video_id = video_id.trim();
    if !crate::wayback::valid_id(video_id) {
        return Err("invalid_id".into());
    }
    let client = state.clients.get(innertube::METADATA_CLIENT).ok_or("metadata client missing")?;
    let next = state.it.next(client, Some(video_id), None).await.map_err(|e| e.to_string())?;
    Ok(next.items.into_iter().find(|s| s.video_id == video_id && !s.unavailable))
}

/// Stop the assistant's running step (titles or search). What it did stays.
#[tauri::command]
pub fn recover_cancel(state: St<'_>) {
    crate::playlist_tools::recover::cancel(&state);
}

/// Forget the assistant's session: rows, picks and progress.
#[tauri::command]
pub fn recover_reset(state: St<'_>) {
    crate::playlist_tools::recover::reset(&state);
}

/// Put the approved replacement of each of these rows in (F4), one playlist at a time: `replace`
/// puts it where the dead track sits (or sat) and takes the dead one out once the new one is in;
/// `append` only adds it at the end. On InnerTube it writes directly, one journal entry per
/// playlist; on the Data API (`engine`, else `playlist_engine`) it queues an insert job and then a
/// removal job, two entries. Progress goes out as `recover-progress` (`applying`), and a cancel
/// stops it between playlists. `busy` while another step runs, `cooldown:<until>` while YouTube
/// is left alone.
#[tauri::command]
pub async fn recover_apply(
    state: St<'_>,
    jobs: Jobs<'_>,
    keys: Vec<String>,
    action: String,
    engine: Option<String>,
) -> Result<crate::playlist_tools::recover::RecoverApplied, String> {
    use crate::playlist_tools::recover;
    let action =
        recover::Action::parse(&action).ok_or_else(|| format!("unknown action: {action}"))?;
    recover::apply(&state, &jobs, keys, action, engine.as_deref()).await
}

/// A change the monitor found in one of your playlists since the sync before: a track added,
/// removed, moved, turned unavailable or restored (playlist_tools::monitor).
#[derive(serde::Serialize)]
pub struct PlaylistAlert {
    id: i64,
    playlist_id: String,
    video_id: String,
    kind: String,
    song: Option<SongItem>,
    at: i64,
    seen: bool,
    /// The positions a `moved` row went between (0-based), where known.
    #[serde(skip_serializing_if = "Option::is_none")]
    from: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    to: Option<i64>,
    /// Dismissed from Library ▸ In your playlists. Only ever true with `all`.
    dismissed: bool,
    /// An `unavailable` alert a later `restored` of the same track answers (`AlertRow::resolved`).
    resolved: bool,
}

/// `rows` (newest first) as the UI gets them. Without `all`: the ones not dismissed, one per
/// playlist, track and kind, the newest standing for its repeats. With it: every row, as filed.
fn alerts_of(rows: Vec<crate::db::AlertRow>, all: bool) -> Vec<PlaylistAlert> {
    let mut shown = std::collections::HashSet::new();
    rows.into_iter()
        .filter(|a| {
            all || (!a.dismissed
                && shown.insert((a.playlist_id.clone(), a.video_id.clone(), a.kind.clone())))
        })
        .map(|a| PlaylistAlert {
            id: a.id,
            playlist_id: a.playlist_id,
            video_id: a.video_id,
            kind: a.kind,
            song: a.song_json.and_then(|j| serde_json::from_str(&j).ok()),
            at: a.at,
            seen: a.seen,
            from: a.from_pos,
            to: a.to_pos,
            dismissed: a.dismissed,
            resolved: a.resolved,
        })
        .collect()
}

/// The monitor's alerts, newest first. By default the ones not dismissed, one per playlist, track
/// and kind: a repeated event files a row each time, but here the newest stands for the rest, and
/// dismissing it dismisses them all (`dismiss_playlist_alert`). `all` is the alerts page: every
/// row ever filed, repeats and dismissed ones included. With `all` the page loads them in pages:
/// at most `limit` rows, older than the `before` cursor (`[at, id]` of the last row it has).
/// Without `all` both are ignored.
#[tauri::command]
pub fn playlist_alerts(
    state: St<'_>,
    all: Option<bool>,
    limit: Option<u32>,
    before: Option<(i64, i64)>,
) -> Vec<PlaylistAlert> {
    let all = all.unwrap_or(false);
    if all && (limit.is_some() || before.is_some()) {
        return alerts_of(state.db.alert_rows_page(true, limit, before), true);
    }
    alerts_of(state.db.alert_rows(all), all)
}

/// Mark these alerts seen, or every one with no `ids`. Answers the unseen count after, which the
/// `alerts-changed` event carries too (the sidebar badge).
#[tauri::command]
pub fn mark_alerts_seen(state: St<'_>, ids: Option<Vec<i64>>) -> u32 {
    state.db.mark_alerts_seen(ids.as_deref());
    let unseen = state.db.unseen_alert_count();
    let _ = state.app.emit("alerts-changed", json!({ "unseen": unseen }));
    unseen
}

/// One change between two snapshots of a playlist ([`monitor::Change`], as the UI reads it).
#[derive(Debug, serde::Serialize)]
pub struct TimelineChange {
    video_id: String,
    kind: &'static str,
    song: Option<SongItem>,
    /// Where the row was in the older snapshot (0-based), and where it is in the newer one.
    #[serde(skip_serializing_if = "Option::is_none")]
    from: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    to: Option<usize>,
}

/// One snapshot of a playlist and what changed since the one before it.
#[derive(Debug, serde::Serialize)]
pub struct TimelineEntry {
    snapshot_id: i64,
    taken_at: i64,
    item_count: i64,
    title: Option<String>,
    /// The oldest snapshot kept: nothing before it to compare with, so no changes.
    baseline: bool,
    added: u32,
    removed: u32,
    moved: u32,
    unavailable: u32,
    restored: u32,
    changes: Vec<TimelineChange>,
}

/// A snapshot row as the song [`monitor::diff`] compares against.
fn snap_song(i: &crate::db::SnapItem) -> SongItem {
    SongItem {
        video_id: i.v.clone(),
        set_video_id: i.s.clone(),
        title: i.t.clone(),
        artists: i.a.clone(),
        duration: i.d.clone(),
        unavailable: i.u,
        thumbnail: i.th.clone(),
        ..Default::default()
    }
}

/// `snaps` newest first (as [`crate::db::Db::snapshots`] answers them), each with the changes
/// from the snapshot right after it in the list, the older one: the same comparison a sync makes.
fn timeline(snaps: &[crate::db::Snapshot]) -> Vec<TimelineEntry> {
    snaps
        .iter()
        .enumerate()
        .map(|(i, snap)| {
            let changes = match snaps.get(i + 1) {
                Some(older) => {
                    let now: Vec<SongItem> = snap.items.iter().map(snap_song).collect();
                    monitor::diff(&older.items, &now)
                }
                None => Vec::new(),
            };
            let mut entry = TimelineEntry {
                snapshot_id: snap.id,
                taken_at: snap.taken_at,
                item_count: snap.item_count,
                title: snap.title.clone(),
                baseline: i + 1 == snaps.len(),
                added: 0,
                removed: 0,
                moved: 0,
                unavailable: 0,
                restored: 0,
                changes: Vec::with_capacity(changes.len()),
            };
            for c in changes {
                let n = match c.kind {
                    monitor::Kind::Added => &mut entry.added,
                    monitor::Kind::Removed => &mut entry.removed,
                    monitor::Kind::Moved => &mut entry.moved,
                    monitor::Kind::Unavailable => &mut entry.unavailable,
                    monitor::Kind::Restored => &mut entry.restored,
                };
                *n += 1;
                entry.changes.push(TimelineChange {
                    video_id: c.video_id,
                    kind: c.kind.as_str(),
                    song: c.song,
                    from: c.from,
                    to: c.to,
                });
            }
            entry
        })
        .collect()
}

/// A playlist's history: every snapshot kept of it, newest first, each with what changed since
/// the one before (PlaylistForge's timeline). Empty for a playlist never synced.
#[tauri::command]
pub fn playlist_timeline(state: St<'_>, playlist_id: String) -> Vec<TimelineEntry> {
    timeline(&state.db.snapshots(&playlist_id))
}

#[tauri::command]
pub fn dismiss_playlist_alert(state: St<'_>, playlist_id: String, video_id: String, kind: String) {
    state.db.dismiss_playlist_alert(&playlist_id, &video_id, &kind);
}

/// The undo history, newest first.
#[tauri::command]
pub fn playlist_history(state: St<'_>) -> Vec<OpRecord> {
    journal::history(&state)
}

#[tauri::command]
pub async fn undo_playlist_op(state: St<'_>, id: i64) -> Result<OpRecord, String> {
    journal::undo(&state, id).await
}

// --- playlists on this machine (issue #251) --------------------------------------------------
// A `LOCALPLAYLIST:<n>` id reaches the same commands a YouTube playlist does (get, add, remove,
// edit, cover, delete), and each one answers it from SQLite before any of the YouTube path runs.
// That is what lets every playlist surface in the UI take both kinds without branching.

const GONE: &str = "This playlist is no longer on this device.";

/// `LOCALPLAYLIST:<n>` → n. An id with the prefix and no number is still not YouTube's, so it is
/// an error rather than a fall-through to a browse YouTube would 400.
pub(crate) fn local_key(id: &str) -> Result<i64, String> {
    id.strip_prefix(LOCAL_PLAYLIST_PREFIX).and_then(|n| n.parse().ok()).ok_or_else(|| GONE.into())
}

pub(crate) fn db_err(e: rusqlite::Error) -> String {
    match e {
        rusqlite::Error::QueryReturnedNoRows => GONE.into(),
        e => e.to_string(),
    }
}

/// The library card. The subtitle is English on purpose: Rust never learns the UI language, so
/// `api.ts` rewords it on the way in, the same as On Repeat's.
fn local_playlist_card(state: &Arc<AppState>, p: &crate::db::LocalPlaylist) -> BrowseItem {
    let id = format!("{LOCAL_PLAYLIST_PREFIX}{}", p.id);
    // A cover the user picked, else the first track's art, which is what YouTube shows for a
    // playlist too short for its four-track collage.
    let thumbnail = custom_cover(state, &id).or_else(|| {
        let first = p.first_song.as_deref()?;
        serde_json::from_str::<SongItem>(first).ok()?.thumbnail
    });
    BrowseItem {
        kind: "playlist",
        id,
        title: p.title.clone(),
        subtitle: Some(format!("{} songs", p.count)),
        thumbnail,
        duration: None,
        album_id: None,
        artist_runs: Vec::new(),
        play_count: None,
        is_video: false,
        is_upload: false,
        explicit: false,
    }
}

fn local_playlist_page(state: &Arc<AppState>, id: &str) -> Result<PlaylistPage, String> {
    let key = local_key(id)?;
    let p = state.db.local_playlist(key).ok_or(GONE)?;
    // The row id rides as the `set_video_id`, the handle every removal path already sends back.
    // A row whose JSON no longer parses (a `SongItem` shape change) is skipped, not fatal.
    let items: Vec<SongItem> = state
        .db
        .local_playlist_tracks(key)
        .into_iter()
        .filter_map(|(row, json)| {
            let song: SongItem = serde_json::from_str(&json).ok()?;
            Some(SongItem { set_video_id: Some(row.to_string()), ..song })
        })
        .collect();
    Ok(PlaylistPage {
        title: Some(p.title),
        subtitle: Some(format!("{} songs", items.len())), // the page words its own count
        thumbnail: items.first().and_then(|s| s.thumbnail.clone()),
        description: (!p.description.is_empty()).then_some(p.description),
        privacy: None,
        cover: custom_cover(state, id),
        items,
        continuation: None, // it is all here: nothing to page through
        owned: true,
        collaborative: false,
        sort_menu: None, // no server to keep an order, so every sort is done on the page
    })
}

/// What a track keeps once it is in a playlist: the song, none of the context it was added from
/// (`shed_queue_context`), and no snapshot of account state that would go stale behind it. The
/// rating is read live (the override map, the saved-in index), and Library ▸ Songs tokens are
/// minted per row for one account while these playlists belong to none.
pub(crate) fn playlist_row(s: SongItem) -> SongItem {
    SongItem { rating: None, library: None, ..shed_queue_context(s) }
}

fn remove_local_rows(state: &Arc<AppState>, playlist_id: &str, rows: &[i64]) -> Result<(), String> {
    let key = local_key(playlist_id)?;
    state.db.remove_local_playlist_tracks(key, rows, crate::db::now_secs()).map_err(db_err)
}

/// Just the playlists on this machine, as library cards. The UI re-reads these after every edit
/// (the count and the artwork follow the tracks), and falls back on them when the account's
/// library can't be fetched, since these need no network.
#[tauri::command]
pub async fn local_playlists(state: St<'_>) -> Result<Vec<BrowseItem>, String> {
    Ok(state.db.local_playlists().iter().map(|p| local_playlist_card(&state, p)).collect())
}

/// Add tracks to a playlist on this machine, answering per track whether it went in (`false`: it
/// was there already, refused the way YouTube refuses one). Whole `SongItem`s rather than the
/// videoIds `add_to_playlist` takes, because nothing will ever fill in a title or artwork later:
/// there is no YouTube playlist to re-read. Files on disk are welcome, unlike in a YouTube one.
#[tauri::command]
pub async fn add_to_local_playlist(
    state: St<'_>,
    playlist_id: String,
    items: Vec<SongItem>,
) -> Result<Vec<bool>, String> {
    let key = local_key(&playlist_id)?;
    let rows = items
        .into_iter()
        .map(|song| {
            let song = playlist_row(song);
            let json = serde_json::to_string(&song).map_err(|e| e.to_string())?;
            Ok((song.video_id, json))
        })
        .collect::<Result<Vec<_>, String>>()?;
    state.db.add_local_playlist_tracks(key, &rows, crate::db::now_secs()).map_err(db_err)
}

#[tauri::command]
pub async fn subscribe(state: St<'_>, channel_id: String, subscribed: bool) -> Result<(), String> {
    let client = require_login(&state)?;
    state.it.subscribe(client, &channel_id, subscribed).await.map_err(|e| e.to_string())
}

// --- blocked artists (blocked.rs, plan 046) ----------------------------------------------------

/// The blocked-artist list, for the settings pane.
#[tauri::command]
pub async fn get_blocked_artists(state: St<'_>) -> Result<Vec<BlockedArtist>, String> {
    Ok(crate::blocked::list(&state.db))
}

/// Block an artist: persist, re-arm the fetch filter, and drop them out of the live queue. Returns
/// the new list. `id` is the channel browseId when the caller has one (a track row often does not).
#[tauri::command]
pub async fn block_artist(
    state: St<'_>,
    id: Option<String>,
    name: String,
) -> Result<Vec<BlockedArtist>, String> {
    let name = name.trim().to_string();
    if name.is_empty() {
        return Err("an artist needs a name to be blocked".into());
    }
    let state = state.inner().clone();
    let list =
        crate::blocked::block(&state.db, BlockedArtist { id: id.filter(|i| !i.is_empty()), name });
    let predicate = crate::blocked::block_list(&state.db);
    state.it.set_blocked(predicate.clone());
    // The fetch filter only covers what is fetched from here on, so the queue already on screen
    // has to be cleaned out separately or the block reads as having done nothing.
    state.purge_blocked(&predicate).await;
    Ok(list)
}

#[tauri::command]
pub async fn unblock_artist(state: St<'_>, key: String) -> Result<Vec<BlockedArtist>, String> {
    let list = crate::blocked::unblock(&state.db, &key);
    state.it.set_blocked(crate::blocked::block_list(&state.db));
    Ok(list)
}

// --- Spotify import (spotify.rs, import.rs, #375) ----------------------------------------------
//
// Errors from these are short codes (`private`, `busy`, `rate_limited`, ...) that the UI words in
// the user's language; anything else is a network error passed through as text.

/// Read a pasted Spotify link, or a file picked from disk (the data export zip, one of its JSON
/// files, or a CSV). Answers what it holds; the tracks stay here until `import_start`.
#[tauri::command]
pub async fn import_read(
    link: Option<String>,
    path: Option<String>,
) -> Result<crate::import::Preview, String> {
    if let Some(link) = link {
        return crate::import::read_link(&link).await;
    }
    let path = path.ok_or("nothing_read")?;
    let bytes = std::fs::read(&path).map_err(|e| e.to_string())?;
    crate::import::read_file(&bytes, &path)
}

/// A file dropped on the window. A webview drop has the bytes but no path, so they come over as
/// the raw request body, with the name in `x-file-name` (percent-encoded: headers are ASCII).
#[tauri::command]
pub async fn import_read_file(
    request: tauri::ipc::Request<'_>,
) -> Result<crate::import::Preview, String> {
    let tauri::ipc::InvokeBody::Raw(bytes) = request.body() else {
        return Err("unreadable".into());
    };
    let name = request
        .headers()
        .get("x-file-name")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| urlencoding::decode(v).ok())
        .map(|v| v.into_owned())
        .unwrap_or_default();
    crate::import::read_file(bytes, &name)
}

/// Start matching the lists picked out of the last read (their indices in its preview).
#[tauri::command]
pub async fn import_start(
    state: St<'_>,
    lists: Vec<usize>,
) -> Result<crate::import::Snapshot, String> {
    crate::import::start(&state, lists)
}

#[tauri::command]
pub async fn import_status() -> Result<Option<crate::import::Snapshot>, String> {
    Ok(crate::import::status())
}

/// The rows of one tier, for the review step.
#[tauri::command]
pub async fn import_rows(
    tier: crate::import::Tier,
) -> Result<Vec<crate::import::ReviewRow>, String> {
    Ok(crate::import::rows(tier))
}

/// The user's pick for one row; `None` leaves the track out.
#[tauri::command]
pub async fn import_pick(
    state: St<'_>,
    key: String,
    song: Option<SongItem>,
) -> Result<crate::import::Snapshot, String> {
    crate::import::pick(&state, &key, song)
}

#[tauri::command]
pub async fn import_create(
    state: St<'_>,
    options: crate::import::CreateOptions,
) -> Result<(), String> {
    crate::import::create(&state, options)
}

/// Stop a running import, or put away a finished one.
#[tauri::command]
pub async fn import_cancel(state: St<'_>) -> Result<(), String> {
    crate::import::cancel(&state);
    Ok(())
}

/// The Spotify link a playlist was imported from, when it was imported from one.
#[tauri::command]
pub async fn import_source(state: St<'_>, playlist_id: String) -> Result<Option<String>, String> {
    Ok(crate::import::source_url(&state, &playlist_id))
}

/// "Update from Spotify": re-read the playlist's link and apply what changed.
#[tauri::command]
pub async fn import_update(
    state: St<'_>,
    playlist_id: String,
) -> Result<crate::import::Snapshot, String> {
    crate::import::update(&state, playlist_id).await
}

/// What a Spotify track, album or artist link is on YouTube Music.
#[tauri::command]
pub async fn import_resolve(
    state: St<'_>,
    link: String,
) -> Result<crate::import::Resolved, String> {
    crate::import::resolve(&state, &link).await
}

// --- local music (local.rs) ------------------------------------------------------------------

/// Rescan the watched folders and return the library. The scan is the deletion check too: its
/// `removed` list is every id that was on screen but is gone from disk, so the UI can drop those
/// tiles without waiting for anyone to click a dead one.
#[tauri::command]
pub async fn get_local_library(state: St<'_>) -> Result<crate::local::LocalLibrary, String> {
    scan_local(&state).await
}

#[tauri::command]
pub async fn add_local_folder(
    state: St<'_>,
    path: String,
) -> Result<crate::local::LocalLibrary, String> {
    crate::local::add_folder(&state.db, path);
    scan_local(&state).await
}

/// Stop watching a folder. Its tracks disappear from the library on the rescan that follows (they
/// come back untouched if the folder is added again — nothing on disk is modified).
#[tauri::command]
pub async fn remove_local_folder(
    state: St<'_>,
    path: String,
) -> Result<crate::local::LocalLibrary, String> {
    crate::local::remove_folder(&state.db, &path);
    scan_local(&state).await
}

/// Disk IO + tag parsing off the async runtime's worker threads.
async fn scan_local(state: &Arc<AppState>) -> Result<crate::local::LocalLibrary, String> {
    let app = state.app.clone();
    let state = state.clone();
    let covers = crate::local::covers_dir(&state.app);
    let lib = tauri::async_runtime::spawn_blocking(move || crate::local::scan(&state.db, &covers))
        .await
        .map_err(|e| e.to_string())?;
    // Artwork reaches the page over the asset protocol, which starts out allowing nothing.
    crate::local::allow_covers(&app, &lib.songs);
    Ok(lib)
}

// --- Listen Together (context/19) ----------------------------------------------------------

/// Current client-side LT state (status, role, room, participants, pending joins, suggestions).
#[tauri::command]
pub async fn lt_get_state(state: St<'_>) -> Result<serde_json::Value, String> {
    Ok(state.lt.snapshot().await)
}

/// Set + persist the sync server URL (e.g. the Tailscale Funnel `wss://…` address).
#[tauri::command]
pub async fn lt_set_server_url(state: St<'_>, url: String) -> Result<(), String> {
    let url = url.trim().to_string();
    state.db.set_setting("lt_server_url", &url);
    state.lt.set_server_url(url).await;
    Ok(())
}

#[tauri::command]
pub async fn lt_create_room(state: St<'_>, username: String) -> Result<(), String> {
    state.lt.create_room(username).await;
    Ok(())
}

#[tauri::command]
pub async fn lt_join_room(state: St<'_>, code: String, username: String) -> Result<(), String> {
    state.lt.join_room(code, username).await;
    Ok(())
}

#[tauri::command]
pub async fn lt_leave(state: St<'_>) -> Result<(), String> {
    state.lt.leave().await;
    Ok(())
}

#[tauri::command]
pub async fn lt_approve_join(state: St<'_>, user_id: String) -> Result<(), String> {
    state.lt.approve_join(user_id).await;
    Ok(())
}

#[tauri::command]
pub async fn lt_reject_join(state: St<'_>, user_id: String) -> Result<(), String> {
    state.lt.reject_join(user_id).await;
    Ok(())
}

#[tauri::command]
pub async fn lt_kick(state: St<'_>, user_id: String) -> Result<(), String> {
    state.lt.kick(user_id).await;
    Ok(())
}

#[tauri::command]
pub async fn lt_transfer_host(state: St<'_>, user_id: String) -> Result<(), String> {
    state.lt.transfer_host(user_id).await;
    Ok(())
}

/// Guest: send a track to the session queue (auto-approved by the host client, which stamps
/// who added it).
#[tauri::command]
pub async fn lt_suggest(state: St<'_>, item: SongItem) -> Result<(), String> {
    state.lt.suggest(crate::state::song_to_track(&item)).await;
    Ok(())
}

/// Host: approve a suggestion — add it to the real queue and notify the suggester. (Unused since
/// guest adds auto-approve, kept for a future "require approval" setting.)
#[tauri::command]
pub async fn lt_approve_suggestion(state: St<'_>, id: String) -> Result<(), String> {
    if let Some(track) = state.lt.approve_suggestion(id).await {
        state.inner().clone().lt_enqueue_track(track).await;
    }
    Ok(())
}

#[tauri::command]
pub async fn lt_reject_suggestion(state: St<'_>, id: String) -> Result<(), String> {
    state.lt.reject_suggestion(id).await;
    Ok(())
}

/// Guest: force a re-sync with the room (drift correction).
#[tauri::command]
pub async fn lt_request_sync(state: St<'_>) -> Result<(), String> {
    state.lt.request_sync().await;
    Ok(())
}

// --- lyrics ---------------------------------------------------------------------------------

/// Lyrics for a track (cached). The UI passes the metadata it already has from `now-playing`;
/// `duration` is mpv's length in seconds. `None` = no lyrics found anywhere. `source` asks one
/// provider alone, uncached: the source picker's preview.
#[tauri::command]
pub async fn get_lyrics(
    state: St<'_>,
    video_id: String,
    title: String,
    artists: String,
    album: Option<String>,
    duration: Option<f64>,
    source: Option<String>,
) -> Result<Option<crate::lyrics::Lyrics>, String> {
    let req = crate::lyrics::LyricsRequest { video_id, title, artists, album, duration };
    crate::lyrics::get_lyrics(state.inner(), req, source).await
}

/// Keep one provider's lyrics for this song (`source`), or hand it back to the provider order
/// (`None`). Returns what the song shows now.
#[tauri::command]
pub async fn choose_lyrics_source(
    state: St<'_>,
    video_id: String,
    title: String,
    artists: String,
    album: Option<String>,
    duration: Option<f64>,
    source: Option<String>,
) -> Result<Option<crate::lyrics::Lyrics>, String> {
    let req = crate::lyrics::LyricsRequest { video_id, title, artists, album, duration };
    Ok(crate::lyrics::choose_source(state.inner(), req, source).await)
}

#[tauri::command]
pub fn set_lyrics_offset(state: St<'_>, video_id: String, offset_ms: i64) {
    crate::lyrics::set_offset(state.inner(), &video_id, offset_ms);
}

/// Every lyrics provider in the user's order, for Settings and the source picker.
#[tauri::command]
pub fn lyrics_providers(state: St<'_>) -> Vec<crate::lyrics::ProviderInfo> {
    crate::lyrics::providers(state.inner())
}

// --- Changelog ------------------------------------------------------------------------------

#[derive(Clone, serde::Serialize)]
pub struct ReleaseNote {
    version: String,
    /// `YYYY-MM-DD`, or empty for an unpublished tag.
    date: String,
    /// The release description, verbatim markdown. The About tab renders it.
    body: String,
}

/// Whether a version (or a `v`-prefixed tag) is a prerelease (D1): its suffix (what follows the
/// first `-`, build metadata after `+` ignored) starts with `rc`, `beta` or `alpha`,
/// case-insensitive. The fork's own `-forge.N` suffix marks a stable release, so `1.2.0-forge.1`
/// is not one while `1.3.0-rc.1` is. Cut release candidates as `1.3.0-rc.N`, never
/// `1.2.0-forge.2-rc.1`: that one is stable by this rule and orders after `1.2.0-forge.2`. The
/// UI's twin is `isPrerelease` in ui/src/lib/version.ts; keep both rules identical.
pub(crate) fn is_prerelease(version: &str) -> bool {
    let core = version.split('+').next().unwrap_or_default();
    let Some((_, pre)) = core.split_once('-') else {
        return false;
    };
    let pre = pre.to_ascii_lowercase();
    ["rc", "beta", "alpha"].iter().any(|p| pre.starts_with(p))
}

/// What's new, read straight from the GitHub releases API so the release description is the only
/// place the changelog is written. Cached for the process: the list only changes when a release
/// is cut, and unauthenticated GitHub allows 60 requests an hour.
#[tauri::command]
pub async fn release_notes() -> Result<Vec<ReleaseNote>, String> {
    static CACHE: std::sync::OnceLock<Vec<ReleaseNote>> = std::sync::OnceLock::new();
    if let Some(cached) = CACHE.get() {
        return Ok(cached.clone());
    }
    #[derive(serde::Deserialize)]
    struct GhRelease {
        tag_name: String,
        published_at: Option<String>,
        body: Option<String>,
        draft: bool,
    }
    let releases: Vec<GhRelease> = crate::http::client()
        .get(format!(
            "https://api.github.com/repos/{}/releases?per_page=20",
            crate::brand::REPO_SLUG
        ))
        .header("User-Agent", concat!("LiMusicForge/", env!("CARGO_PKG_VERSION")))
        .header("Accept", "application/vnd.github+json")
        .timeout(std::time::Duration::from_secs(15))
        .send()
        .await
        .map_err(|e| e.to_string())?
        .error_for_status()
        .map_err(|e| e.to_string())?
        .json()
        .await
        .map_err(|e| e.to_string())?;
    // By version, not GitHub's flag: a `-forge.N` release is the fork's stable line (`is_prerelease`).
    let notes: Vec<ReleaseNote> = releases
        .into_iter()
        .filter(|r| !r.draft && !is_prerelease(&r.tag_name))
        .map(|r| ReleaseNote {
            version: r.tag_name.trim_start_matches('v').to_string(),
            date: r
                .published_at
                .and_then(|d| d.split('T').next().map(str::to_string))
                .unwrap_or_default(),
            body: r.body.unwrap_or_default(),
        })
        .collect();
    Ok(CACHE.get_or_init(|| notes).clone())
}

/// Whether this build can install an update itself, or only point the user at the download.
///
/// Tauri's Linux updater knows one trick: rewrite an AppImage in place. It takes the path from
/// `Env::appimage` and, when that is unset, falls back to `current_exe()` and writes the downloaded
/// AppImage bytes over whatever it finds there. On the `.rpm` and on distro packages (the AUR's
/// `limusic-bin`) that is a package-manager-owned `/usr/bin/limusic-forge`: it fails on permissions
/// rather than doing damage, but offering the button at all is a lie. Those users update through
/// their package manager, so the UI shows them a download link instead.
///
/// Reads the same `Env::appimage` the updater plugin decides on, so the two cannot disagree.
///
/// A build whose `plugins.updater.pubkey` is empty (the fork until its signing key exists, see
/// docs/RELEASING-FORK.md) can verify nothing it downloads, so it never self-updates either: the UI
/// falls back to the same download link.
#[tauri::command]
pub fn can_self_update(app: tauri::AppHandle) -> bool {
    if !updater_pubkey_configured(app.config().plugins.0.get("updater")) {
        return false;
    }
    // The updater would run the NSIS installer, which installs a second, non-portable copy
    // instead of replacing this one. A portable user downloads the new zip.
    if crate::paths::is_portable() {
        return false;
    }
    #[cfg(target_os = "linux")]
    {
        use tauri::Manager;
        app.env().appimage.is_some()
    }
    // Windows runs the NSIS installer and macOS swaps the .app bundle; both work however the app
    // was installed.
    #[cfg(not(target_os = "linux"))]
    {
        let _ = app;
        true
    }
}

/// How this copy keeps its data, for Settings ▸ About.
#[derive(serde::Serialize)]
pub struct InstallInfo {
    pub portable: bool,
    pub data_dir: String,
}

/// Portable or installed, and where the data lives (paths.rs).
#[tauri::command]
pub fn install_info(app: tauri::AppHandle) -> InstallInfo {
    InstallInfo {
        portable: crate::paths::is_portable(),
        data_dir: crate::paths::data_dir(&app).to_string_lossy().into_owned(),
    }
}

/// Upstream LiMusic's data as Settings ▸ Import & migrate shows it.
#[derive(serde::Serialize)]
pub struct UpstreamSource {
    pub path: String,
    pub bytes: u64,
    pub running: bool,
}

/// What there is to import on this machine. Detection only: nothing is opened.
#[derive(serde::Serialize)]
pub struct ImportSources {
    pub upstream: Option<UpstreamSource>,
    /// PlaylistForge's `forge.db`, when present. Its importer comes later.
    pub playlistforge: Option<String>,
}

#[tauri::command]
pub async fn import_sources() -> Result<ImportSources, String> {
    tauri::async_runtime::spawn_blocking(|| {
        use crate::migrate_upstream as mu;
        let upstream = mu::locate().map_err(|e| e.to_string())?.map(|up| {
            let mut bytes = mu::tree_size(&up.roaming);
            if let Some(local) = &up.local {
                bytes += mu::tree_size(&local.join("EBWebView"));
            }
            UpstreamSource {
                path: up.roaming.to_string_lossy().into_owned(),
                bytes,
                running: mu::upstream_running(),
            }
        });
        let playlistforge = mu::playlistforge_db().map(|p| p.to_string_lossy().into_owned());
        Ok(ImportSources { upstream, playlistforge })
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Phase 1 of the upstream migration: leave the marker and restart, so phase 2 runs before
/// anything holds the database or the webview profile (migrate_upstream.rs).
#[tauri::command]
pub fn migrate_upstream_request(
    app: tauri::AppHandle,
    state: St<'_>,
    include_webview: bool,
) -> Result<(), String> {
    use crate::migrate_upstream as mu;
    if mu::locate().map_err(|e| e.to_string())?.is_none() {
        return Err("no LiMusic data on this machine".into());
    }
    if mu::upstream_running() {
        return Err("upstream_running".into());
    }
    // Phase 2 resolves the directory without an AppHandle; both answers have to agree, or the
    // marker would be left where the next launch never looks.
    let data = crate::paths::data_dir(&app);
    if crate::paths::data_dir_early().as_deref() != Some(data.as_path()) {
        return Err(format!("data directory mismatch: {}", data.display()));
    }
    // Also how "Retry now" restarts a pending migration: a fresh marker, with a new window.
    let pending = mu::Pending {
        include_webview,
        prompted: state.db.get_setting(mu::PROMPTED_KEY),
        requested_at: mu::now_secs(),
        last_status: None,
    };
    mu::write_pending(&data, &pending).map_err(|e| e.to_string())?;
    app.restart()
}

/// The migration marker still waiting, if any: when it was asked for, whether it is past its
/// window (phase 2 no longer acts on it) and what phase 2 last made of it (`retry`, `expired`).
#[tauri::command]
pub fn migrate_upstream_pending(
    app: tauri::AppHandle,
) -> Option<crate::migrate_upstream::PendingInfo> {
    use crate::migrate_upstream as mu;
    mu::pending_info(&crate::paths::data_dir(&app), mu::now_secs())
}

/// Drop the pending migration: removes `<data>/migrate-upstream.pending` only (and its
/// `retry`/`expired` result). Nothing else is touched.
#[tauri::command]
pub fn migrate_upstream_cancel(app: tauri::AppHandle) -> Result<(), String> {
    let data = crate::paths::data_dir(&app);
    crate::migrate_upstream::cancel_pending(&data).map(|_| ()).map_err(|e| e.to_string())
}

/// What the last migration did, read once. Says whether upstream starts at login, so the UI can
/// offer the same for this app (upstream's own entry is never touched).
#[tauri::command]
pub fn migrate_upstream_result(app: tauri::AppHandle) -> Option<crate::migrate_upstream::Report> {
    let mut report = crate::migrate_upstream::take_result(&crate::paths::data_dir(&app))?;
    report.prompted = None;
    if report.status == "done" {
        report.upstream_autostart = crate::migrate_upstream::upstream_autostart();
    }
    Some(report)
}

/// Whether the updater plugin's config (`plugins.updater` in tauri.conf.json) carries a public key.
fn updater_pubkey_configured(updater: Option<&serde_json::Value>) -> bool {
    updater
        .and_then(|u| u.get("pubkey"))
        .and_then(serde_json::Value::as_str)
        .is_some_and(|k| !k.trim().is_empty())
}

/// The beta channel's manifest. `beta` is a permanent prerelease holding nothing but this file, and
/// the release workflows move it to the newest release candidate, or to the newest release once that
/// is ahead, so the URL never changes.
const BETA_MANIFEST: &str =
    "https://github.com/Kushro/limusic-forge/releases/download/beta/latest.json";

/// What the updater plugin's own `check` command returns, so the UI can wrap it in the plugin's
/// `Update` class and install it the usual way.
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BetaUpdate {
    rid: tauri::ResourceId,
    current_version: String,
    version: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    body: Option<String>,
    raw_json: serde_json::Value,
}

/// The updater plugin's `check()`, pointed at the beta manifest. The plugin takes its endpoint from
/// tauri.conf.json and its JS `check` has no way to pass another, so this mirrors its `check`
/// command (tauri-plugin-updater 2.10.1, `commands.rs`) with the endpoint swapped. The `Update` goes
/// into the same resource table, which is what lets the plugin's `downloadAndInstall` find it.
///
/// Any different version counts, same as `allowDowngrades` on stable: the pointer only moves forward
/// by itself, so a lower version there is a beta rollback somebody made on purpose.
#[tauri::command]
pub async fn check_beta_update(webview: tauri::Webview) -> Result<Option<BetaUpdate>, String> {
    use tauri::Manager;
    use tauri_plugin_updater::UpdaterExt;
    let url = tauri::Url::parse(BETA_MANIFEST).map_err(|e| e.to_string())?;
    let update = webview
        .updater_builder()
        .endpoints(vec![url])
        .map_err(|e| e.to_string())?
        .version_comparator(|current, remote| remote.version != current)
        .build()
        .map_err(|e| e.to_string())?
        .check()
        .await
        .map_err(|e| e.to_string())?;
    Ok(update.map(|u| BetaUpdate {
        current_version: u.current_version.clone(),
        version: u.version.clone(),
        body: u.body.clone(),
        raw_json: u.raw_json.clone(),
        rid: webview.resources_table().add(u),
    }))
}

/// Open a link from the UI in the real browser. An `<a href>` inside the webview would navigate
/// the app itself off the SPA, with no way back.
#[tauri::command]
pub async fn open_external(url: String) -> Result<(), String> {
    if !url.starts_with("https://") && !url.starts_with("http://") {
        return Err("only http(s) links".into());
    }
    crate::lastfm::open_browser(&url)
}

// --- Diagnostics ----------------------------------------------------------------------------

/// The bug-report blob for Settings ▸ About: environment header plus the redacted tail of
/// `limusic.log`. See `crate::diagnostics`.
#[tauri::command]
pub fn diagnostics(app: tauri::AppHandle, state: St<'_>) -> String {
    crate::diagnostics::report(&app, &state.db)
}

/// The environment block on its own, for prefilling the bug form's `system` field. GitHub's query
/// parameters only reach `input` and `textarea` fields, so this is how the app tells us what it is
/// running on without the user typing it.
#[tauri::command]
pub fn diagnostics_summary(app: tauri::AppHandle, state: St<'_>) -> String {
    crate::diagnostics::summary(&app, &state.db)
}

/// The same text written to a path the user picked in a save dialog, for attaching to an issue
/// when it is too long to paste comfortably.
#[tauri::command]
pub fn save_diagnostics(app: tauri::AppHandle, state: St<'_>, path: String) -> Result<(), String> {
    std::fs::write(&path, crate::diagnostics::report(&app, &state.db)).map_err(|e| e.to_string())
}

/// The webview's own errors, into the same log file as everything else. Without this a blank
/// screen or a rejected `invoke` leaves no trace at all in the log a user hands over.
#[tauri::command]
pub fn log_ui(level: String, message: String) {
    // Bounded: a throwing `$effect` re-fires every frame, and the file has no size cap.
    let message: String = message.chars().take(2000).collect();
    match level.as_str() {
        "info" => tracing::info!(target: "ui", "{message}"),
        "warn" => tracing::warn!(target: "ui", "{message}"),
        _ => tracing::error!(target: "ui", "{message}"),
    }
}

// --- Last.fm scrobbling ---------------------------------------------------------------------

/// Start the browser auth flow. Returns once the authorize page is open; the outcome (session
/// stored, or an error) arrives via the `lastfm-state` event.
#[tauri::command]
pub async fn lastfm_connect(state: St<'_>) -> Result<(), String> {
    crate::lastfm::connect(state.inner().clone()).await
}

#[tauri::command]
pub async fn lastfm_disconnect(state: St<'_>) -> Result<(), String> {
    crate::lastfm::disconnect(&state);
    Ok(())
}

/// `{ connected, username }` from the persisted session — seeds the titlebar button on mount.
#[tauri::command]
pub async fn lastfm_status(state: St<'_>) -> Result<serde_json::Value, String> {
    Ok(crate::lastfm::status(&state))
}

/// Avatar and counts for the Scrobbling tab's account card. One Last.fm call per tab open.
#[tauri::command]
pub async fn lastfm_profile(state: St<'_>) -> Result<Option<crate::lastfm::Profile>, String> {
    Ok(crate::lastfm::profile(&state).await)
}

/// What `track` would scrobble as under `config` (the Scrobbling tab's unsaved state, as JSON).
/// The tab's preview, computed by the same function the scrobbler sends from, so a pattern the
/// Rust regex engine reads differently from JavaScript's can't make the preview lie.
#[tauri::command]
pub async fn lastfm_preview(
    config: String,
    track: crate::lastfm::Track,
) -> crate::lastfm::Resolved {
    crate::lastfm::resolve(&track, &crate::lastfm::ScrobbleConfig::parse(&config))
}

/// Theater mode's fullscreen toggle (#139).
///
/// `setFullscreen` on its own is not enough on Windows. tao decides the client area in
/// WM_NCCALCSIZE: while the real Win32 placement says maximized it clamps the client to the
/// monitor's *work* area, so the "fullscreen" window sits under the taskbar with a frame-thick
/// border around it, and while the window is undecorated-with-shadow it insets the client by the
/// frame thickness. Both are decided before the fullscreen flag is, and tao's own `is_maximized`
/// reads a cached flag that can disagree with the placement, which is why unmaximizing from the
/// UI only fixed it some of the time.
///
/// So: restore from the real placement, go fullscreen, then put the window on the monitor rect
/// with SWP_FRAMECHANGED to force one recalculation with the fullscreen flag set. On the main
/// thread, where the window messages run inline and the order is guaranteed.
#[tauri::command]
pub fn theater_fullscreen(window: tauri::WebviewWindow, on: bool) -> Result<(), String> {
    #[cfg(not(target_os = "windows"))]
    {
        window.set_fullscreen(on).map_err(|e| e.to_string())
    }
    #[cfg(target_os = "windows")]
    {
        use std::sync::atomic::{AtomicBool, Ordering};
        use windows::Win32::UI::WindowsAndMessaging::{
            IsZoomed, SetWindowPos, ShowWindow, SWP_FRAMECHANGED, SWP_NOMOVE, SWP_NOSIZE,
            SWP_NOZORDER, SW_MAXIMIZE, SW_RESTORE,
        };

        /// Restoring the window is what makes fullscreen work, so theater has to put the
        /// maximized state back itself on the way out.
        static WAS_MAXIMIZED: AtomicBool = AtomicBool::new(false);

        let w = window.clone();
        window
            .run_on_main_thread(move || {
                let Ok(hwnd) = w.hwnd() else { return };
                if on {
                    let zoomed = unsafe { IsZoomed(hwnd).as_bool() };
                    WAS_MAXIMIZED.store(zoomed, Ordering::Relaxed);
                    if zoomed {
                        unsafe {
                            let _ = ShowWindow(hwnd, SW_RESTORE);
                        }
                    }
                    let _ = w.set_fullscreen(true);
                    if let Ok(Some(m)) = w.current_monitor() {
                        let (p, s) = (m.position(), m.size());
                        unsafe {
                            let _ = SetWindowPos(
                                hwnd,
                                None,
                                p.x,
                                p.y,
                                s.width as i32,
                                s.height as i32,
                                SWP_FRAMECHANGED | SWP_NOZORDER,
                            );
                        }
                    }
                } else {
                    let _ = w.set_fullscreen(false);
                    if WAS_MAXIMIZED.swap(false, Ordering::Relaxed) {
                        unsafe {
                            let _ = ShowWindow(hwnd, SW_MAXIMIZE);
                        }
                    }
                    unsafe {
                        let _ = SetWindowPos(
                            hwnd,
                            None,
                            0,
                            0,
                            0,
                            0,
                            SWP_FRAMECHANGED | SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER,
                        );
                    }
                }
            })
            .map_err(|e| e.to_string())
    }
}

// --- jobs page ---
// The queue and its history (`jobs::control`), today's quota and its last two weeks. Every control
// writes the job's row, then announces it (`jobs-changed`) and wakes the runner, which picks the
// change up on its next step.

fn jobs_changed(state: &AppState, jobs: &crate::jobs::JobsState, job_id: i64) {
    let _ = state.app.emit("jobs-changed", json!({ "job_id": job_id }));
    jobs.nudge();
}

/// The jobs page's list: `active` (the default; in the order the runner takes them), `history`
/// (ended, newest first, `limit` of them, 100 unless said) or `all`.
#[tauri::command]
pub async fn jobs_list(
    state: St<'_>,
    filter: Option<crate::jobs::control::ListFilter>,
    limit: Option<u32>,
) -> Result<Vec<crate::jobs::control::JobView>, String> {
    let limit = limit.map(|n| i64::from(n.clamp(1, 1000)));
    crate::jobs::control::list(&state.db, filter.unwrap_or_default(), limit).map_err(db_err)
}

/// One job with its items (state and error of each) and what reverting it would take. `null` for
/// a job that no longer exists.
#[tauri::command]
pub async fn job_detail(
    state: St<'_>,
    id: i64,
) -> Result<Option<crate::jobs::control::JobDetail>, String> {
    crate::jobs::control::detail(&state.db, id).map_err(db_err)
}

#[tauri::command]
pub async fn job_pause(state: St<'_>, jobs: Jobs<'_>, id: i64) -> Result<(), String> {
    crate::jobs::control::pause(&state.db, id).map_err(db_err)?;
    jobs_changed(&state, &jobs, id);
    Ok(())
}

#[tauri::command]
pub async fn job_resume(state: St<'_>, jobs: Jobs<'_>, id: i64) -> Result<(), String> {
    crate::jobs::control::resume(&state.db, id).map_err(db_err)?;
    jobs_changed(&state, &jobs, id);
    Ok(())
}

/// Cancels a job; with `revert`, what it already did is undone too (a SYSTEM job the runner queues
/// on its next step). On a job that already ended, `revert` is the same request: an undo.
#[tauri::command]
pub async fn job_cancel(
    state: St<'_>,
    jobs: Jobs<'_>,
    id: i64,
    revert: bool,
) -> Result<(), String> {
    crate::jobs::control::cancel(&state.db, id, revert, chrono::Utc::now()).map_err(db_err)?;
    jobs_changed(&state, &jobs, id);
    Ok(())
}

/// Puts the job's failed items back in the queue (an ended job with them runs again). Answers how
/// many items were retried.
#[tauri::command]
pub async fn job_retry_failed(state: St<'_>, jobs: Jobs<'_>, id: i64) -> Result<usize, String> {
    let n =
        crate::jobs::control::retry_failed(&state.db, id, chrono::Utc::now()).map_err(db_err)?;
    jobs_changed(&state, &jobs, id);
    Ok(n)
}

/// HIGH (1), NORMAL (2) or LOW (3); anything else is clamped to those.
#[tauri::command]
pub async fn job_set_priority(
    state: St<'_>,
    jobs: Jobs<'_>,
    id: i64,
    priority: i64,
) -> Result<(), String> {
    crate::jobs::control::set_priority(&state.db, id, priority).map_err(db_err)?;
    jobs_changed(&state, &jobs, id);
    Ok(())
}

/// The queue dragged into a new order: `ids` as wanted. Jobs keep their priority level; within a
/// level they run in this order. Answers how many jobs moved.
#[tauri::command]
pub async fn jobs_reorder(state: St<'_>, jobs: Jobs<'_>, ids: Vec<i64>) -> Result<usize, String> {
    let n = crate::jobs::control::reorder(&state.db, &ids).map_err(db_err)?;
    jobs_changed(&state, &jobs, 0);
    Ok(n)
}

/// Today's Data API spend, the daily quota, the next reset and the spend by endpoint.
#[tauri::command]
pub async fn quota_today(state: St<'_>) -> Result<crate::quota::QuotaToday, String> {
    crate::quota::today(&state.db, chrono::Utc::now()).map_err(db_err)
}

/// Units spent per Pacific day over the last `days` days (14 unless said, at most 90), oldest
/// first, today last.
#[tauri::command]
pub async fn quota_history(
    state: St<'_>,
    days: Option<u32>,
) -> Result<Vec<crate::quota::DailyUsage>, String> {
    let days = i64::from(days.unwrap_or(14).clamp(1, 90));
    crate::quota::daily_history(&state.db, chrono::Utc::now(), days).map_err(db_err)
}

/// Today's quota split the way the budget bar draws it (`jobs::control::budget_partition`).
#[tauri::command]
pub async fn budget_partition(
    state: St<'_>,
) -> Result<crate::jobs::control::BudgetPartition, String> {
    crate::jobs::control::budget_partition(&state.db, chrono::Utc::now()).map_err(db_err)
}

// --- headless monitor and Windows task ---

/// The Windows scheduled task that runs `--monitor --all` once a day (wintask.rs): whether it is
/// registered, when it runs next, the time in `monitor.schedule_time`, and whether it still runs
/// this exe (a portable copy that moved). `supported: false` off Windows.
#[tauri::command]
pub async fn wintask_status(state: St<'_>) -> Result<crate::wintask::WinTaskStatus, String> {
    let time = crate::wintask::schedule_time(&state.db);
    tauri::async_runtime::spawn_blocking(move || crate::wintask::status(time))
        .await
        .map_err(|e| e.to_string())
}

/// Register (or re-register) the task to run this exe daily at `time` (`HH:MM`, local), and keep
/// the time in `monitor.schedule_time`. `Err("bad_time")` for anything but `H:MM`/`HH:MM`,
/// `Err("unsupported")` off Windows; otherwise `schtasks`' own message.
#[tauri::command]
pub async fn wintask_register(
    state: St<'_>,
    time: String,
) -> Result<crate::wintask::WinTaskStatus, String> {
    let time = crate::wintask::normalize_time(&time).ok_or("bad_time")?;
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let at = time.clone();
    tauri::async_runtime::spawn_blocking(move || crate::wintask::register(&exe, &at))
        .await
        .map_err(|e| e.to_string())??;
    state.db.set_setting(crate::wintask::SCHEDULE_TIME_KEY, &time);
    tauri::async_runtime::spawn_blocking(move || crate::wintask::status(time))
        .await
        .map_err(|e| e.to_string())
}

/// Remove the task; not being registered is no error. Only "LiMusic Forge Monitor", never
/// PlaylistForge's. The time setting stays, for registering again.
#[tauri::command]
pub async fn wintask_unregister(state: St<'_>) -> Result<crate::wintask::WinTaskStatus, String> {
    tauri::async_runtime::spawn_blocking(crate::wintask::unregister)
        .await
        .map_err(|e| e.to_string())??;
    let time = crate::wintask::schedule_time(&state.db);
    tauri::async_runtime::spawn_blocking(move || crate::wintask::status(time))
        .await
        .map_err(|e| e.to_string())
}

// --- PlaylistForge import ---
// Settings ▸ Import & migrate ▸ PlaylistForge (pf_import/): detect, preview, apply, the Data API
// tokens (only with consent) and PlaylistForge's scheduled task (only once confirmed). Nothing here
// writes under PlaylistForge's folder or its keyring service, and no token or client secret
// reaches the webview: only counts, names and short codes.

/// What Settings shows before anything is read.
#[derive(Debug, serde::Serialize)]
pub struct PfDetect {
    /// PlaylistForge's folder, when it holds a database.
    path: Option<String>,
    found: bool,
    /// PlaylistForge is open: it has to be closed before its database is copied (D35).
    running: bool,
    /// Its database's schema version, read off a copy (not while it runs).
    user_version: Option<i64>,
    /// Newer than this importer knows (D34).
    too_new: bool,
    /// "PlaylistForge Monitor" is registered; `None` off Windows or when Windows did not answer.
    task: Option<bool>,
    /// Why the copy could not be read (`PfImportError::code`).
    error: Option<&'static str>,
}

/// PlaylistForge's folder: the one picked (absolute only), or `%APPDATA%\PlaylistForge`.
fn pf_dir(path: Option<String>) -> Result<std::path::PathBuf, String> {
    match path.map(|p| p.trim().to_owned()).filter(|p| !p.is_empty()) {
        Some(p) => {
            let p = std::path::PathBuf::from(p);
            if p.is_absolute() {
                Ok(p)
            } else {
                Err("not_found".into())
            }
        }
        None => crate::pf_import::pf_default_dir().map_err(|e| e.code().to_string()),
    }
}

/// `set_setting`'s gate, for a setting the import brings: a key the UI may write, with a value
/// its per-key rule accepts.
pub(crate) fn pf_settable(key: &str, value: &str) -> Result<(), String> {
    if !UI_SETTINGS.contains(&key) {
        return Err(format!("unknown setting: {key}"));
    }
    validate_setting(key, value)
}

/// The Data API's account manager reads its channels once, at startup: hand it a fresh copy after
/// the import added channels or tokens, as `lib.rs` builds it.
fn pf_reload_ytdata(state: &AppState, jobs: &crate::jobs::JobsState) {
    let store = crate::ytdata_secrets::token_store(&state.app);
    let repo = Arc::new(crate::ytdata_accounts::DbAccountsRepo::new(state.db.clone()));
    match ytdata::auth::accounts::AccountManager::new(store, repo) {
        Ok(manager) => {
            let data_dir = crate::paths::data_dir(&state.app);
            if let Some(secret) = ytdata::client_secret::load_existing(&data_dir) {
                manager.set_client_secret(secret);
            }
            jobs.set_account_manager(Some(Arc::new(manager)));
        }
        Err(e) => tracing::warn!(error = %e, "pf import: Data API account manager not reloaded"),
    }
}

/// Whether PlaylistForge is installed here, open, which schema, and whether its task is there.
#[tauri::command]
pub async fn pf_detect(state: St<'_>) -> Result<PfDetect, String> {
    let data_dir = crate::paths::data_dir(&state.app);
    tauri::async_runtime::spawn_blocking(move || {
        use crate::pf_import::{self as pf, PfImportError};
        let dir = pf::pf_default_dir().map_err(|e| e.code().to_string())?;
        let found = dir.join(pf::PF_DB_FILE).is_file();
        let running = pf::pf_running();
        let mut out = PfDetect {
            path: found.then(|| dir.to_string_lossy().into_owned()),
            found,
            running,
            user_version: None,
            too_new: false,
            task: pf::task::exists(),
            error: None,
        };
        if found && !running {
            pf::reader::sweep_stale(&data_dir);
            match pf::reader::stage(&dir, &data_dir) {
                Ok(staged) => {
                    out.user_version = Some(staged.user_version());
                    let _ = staged.finish();
                }
                Err(e) => {
                    if let PfImportError::TooNew { found, .. } = &e {
                        out.user_version = Some(*found);
                        out.too_new = true;
                    }
                    out.error = Some(e.code());
                }
            }
        }
        Ok(out)
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Read PlaylistForge (a copy of its database) and say what an import would bring: counts, the
/// playlists with D36's verdict, the accounts and how they pair. `pf_running` while it is open.
#[tauri::command]
pub async fn pf_preview(
    state: St<'_>,
    path: Option<String>,
) -> Result<crate::pf_import::apply::Preview, String> {
    let dir = pf_dir(path)?;
    if crate::pf_import::pf_running() {
        return Err("pf_running".into());
    }
    let data_dir = crate::paths::data_dir(&state.app);
    let secret = crate::ytdata_secrets::client_secret_path(&state.app);
    let db = state.db.clone();
    tauri::async_runtime::spawn_blocking(move || {
        use crate::pf_import::{apply, reader};
        reader::sweep_stale(&data_dir);
        let data = reader::read_all(&dir, &data_dir, chrono::Utc::now())
            .map_err(|e| e.code().to_string())?;
        let forge_client = ytdata::client_secret::ClientSecretFile::load(&secret)
            .ok()
            .map(|c| c.installed.client_id);
        Ok(apply::preview(&db, &data, forge_client.as_deref()))
    })
    .await
    .map_err(|e| e.to_string())?
}

#[derive(Debug, serde::Serialize)]
pub struct PfApplyResult {
    report: crate::pf_import::apply::Report,
    /// The account playlists went to the import queue (`import-progress` follows them).
    import_started: bool,
    /// Why they did not (`busy`, `cooldown:<until>`).
    import_error: Option<String>,
}

/// Import the user's selection. Progress arrives as `pf-import-progress`
/// (`{ step, done, total }`); the account playlists, if any, are then created through the
/// Spotify import's paced writer, followed by its own `import-progress`.
#[tauri::command]
pub async fn pf_import_apply(
    state: St<'_>,
    jobs: Jobs<'_>,
    selection: crate::pf_import::apply::Selection,
) -> Result<PfApplyResult, String> {
    let dir = pf_dir(selection.path.clone())?;
    if crate::pf_import::pf_running() {
        return Err("pf_running".into());
    }
    let data_dir = crate::paths::data_dir(&state.app);
    let secret_dest = crate::ytdata_secrets::client_secret_path(&state.app);
    let db = state.db.clone();
    let app = state.app.clone();
    let mut report = tauri::async_runtime::spawn_blocking(move || {
        use crate::pf_import::{apply, reader};
        let progress = |step: &'static str, done: usize, total: usize| {
            let _ = app
                .emit("pf-import-progress", json!({ "step": step, "done": done, "total": total }));
        };
        progress("read", 0, 1);
        reader::sweep_stale(&data_dir);
        let data = reader::read_all(&dir, &data_dir, chrono::Utc::now())
            .map_err(|e| e.code().to_string())?;
        let forbidden = crate::backups::forbidden_roots();
        let ctx = apply::ApplyCtx {
            now: chrono::Utc::now(),
            validate: &pf_settable,
            forbidden: &forbidden,
            client_secret_dest: Some(secret_dest.as_path()),
            progress: &progress,
        };
        apply::apply(&db, &data, &selection, &ctx).map_err(|e| e.code().to_string())
    })
    .await
    .map_err(|e| e.to_string())??;

    let mut import_started = false;
    let mut import_error = None;
    if !report.account_lists.is_empty() {
        let lists = std::mem::take(&mut report.account_lists);
        match crate::import::start_known(state.inner(), lists, false) {
            Ok(_) => {
                import_started = true;
                let ids = std::mem::take(&mut report.account_list_ids);
                if let Err(e) = crate::pf_import::apply::mark_account_lists(&state.db, &ids) {
                    tracing::warn!(error = %e, "pf import: queued playlists not remembered");
                }
            }
            Err(e) => import_error = Some(e),
        }
    }
    pf_reload_ytdata(&state, &jobs);
    if report.jobs > 0 {
        jobs.nudge();
    }
    let _ = state.app.emit("jobs-changed", json!({}));
    let _ = state.app.emit("quota-changed", ());
    let unseen = state.db.unseen_alert_count();
    let _ = state.app.emit("alerts-changed", json!({ "unseen": unseen }));
    crate::ytdata_status::announce(&state).await;
    tracing::info!(
        indexed = report.playlists_indexed,
        local = report.local_created,
        jobs = report.jobs,
        "pf import applied"
    );
    Ok(PfApplyResult { report, import_started, import_error })
}

/// Bring PlaylistForge's Data API refresh tokens for `channel_ids` into Forge's own store. Only
/// with `consent` (the user ticked the box); otherwise `consent_required` and nothing is read.
/// When Forge has no OAuth client yet, PlaylistForge's comes too: a token only works with the
/// client that minted it.
#[tauri::command]
pub async fn pf_import_credentials(
    state: St<'_>,
    jobs: Jobs<'_>,
    channel_ids: Vec<String>,
    consent: bool,
    path: Option<String>,
) -> Result<Vec<crate::pf_import::apply::TokenOutcome>, String> {
    if !consent {
        return Err("consent_required".into());
    }
    let dir = pf_dir(path)?;
    let secret_dest = crate::ytdata_secrets::client_secret_path(&state.app);
    let forge_store = crate::ytdata_secrets::token_store(&state.app);
    let db = state.db.clone();
    let outcomes = tauri::async_runtime::spawn_blocking(move || {
        use crate::pf_import::{apply, credentials, reader};
        let files = reader::read_files(&dir);
        if !secret_dest.exists() {
            if let Some(secret) = &files.client_secret {
                if let Some(parent) = secret_dest.parent() {
                    std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
                }
                std::fs::write(&secret_dest, &secret.raw).map_err(|e| e.to_string())?;
            }
        }
        let forge_client = ytdata::client_secret::ClientSecretFile::load(&secret_dest)
            .ok()
            .map(|c| c.installed.client_id);
        let pf_store = credentials::pf_token_store();
        apply::import_tokens(
            &db,
            &files,
            &pf_store,
            forge_store.as_ref(),
            &channel_ids,
            forge_client.as_deref(),
            true,
            crate::db::now_secs(),
        )
    })
    .await
    .map_err(|e| e.to_string())??;
    pf_reload_ytdata(&state, &jobs);
    for o in outcomes.iter().filter(|o| o.outcome == "imported") {
        if let Err(e) =
            crate::jobs::control::resume_waiting_auth_for_account(&state.db, &o.channel_id)
        {
            tracing::warn!(error = %e, "pf import: waiting jobs not resumed");
        }
    }
    jobs.nudge();
    let _ = state.app.emit("jobs-changed", json!({}));
    crate::ytdata_status::announce(&state).await;
    Ok(outcomes)
}

/// Remove PlaylistForge's "PlaylistForge Monitor" task (D35), only after the user confirmed it in
/// the UI (`confirmed`): `schtasks /delete /tn "PlaylistForge Monitor" /f`, nothing else.
#[tauri::command]
pub async fn pf_unregister_task(confirmed: bool) -> Result<(), String> {
    if !confirmed {
        return Err("confirmation_required".into());
    }
    tauri::async_runtime::spawn_blocking(crate::pf_import::task::unregister)
        .await
        .map_err(|e| e.to_string())?
}

#[cfg(test)]
mod tests {
    use super::*;

    fn alert_row(id: i64, video: &str, kind: &str, dismissed: bool) -> crate::db::AlertRow {
        crate::db::AlertRow {
            id,
            playlist_id: "VLPL1".into(),
            video_id: video.into(),
            kind: kind.into(),
            song_json: None,
            at: 1000 - id,
            from_pos: None,
            to_pos: None,
            seen: false,
            dismissed,
            resolved: false,
        }
    }

    #[test]
    fn alerts_collapse_repeats_unless_the_page_asks_for_all() {
        // Newest first, as `alert_rows` answers: b removed twice, a dismissed once.
        let rows = || {
            vec![
                alert_row(1, "b", "removed", false),
                alert_row(2, "b", "removed", false),
                alert_row(3, "b", "added", false),
                alert_row(4, "a", "unavailable", true),
            ]
        };
        let ids = |v: Vec<PlaylistAlert>| v.into_iter().map(|a| a.id).collect::<Vec<_>>();
        assert_eq!(ids(alerts_of(rows(), false)), [1, 3]);
        assert_eq!(ids(alerts_of(rows(), true)), [1, 2, 3, 4]);
        let all = alerts_of(rows(), true);
        assert!(all[3].dismissed && !all[0].dismissed);
    }

    #[test]
    fn timeline_diffs_each_snapshot_against_the_one_before() {
        let db = crate::db::Db::open(std::path::Path::new(":memory:")).unwrap();
        let row = |v: &str, s: &str| SongItem {
            video_id: v.into(),
            title: v.to_uppercase(),
            set_video_id: Some(s.into()),
            ..Default::default()
        };
        let read = |songs: &[SongItem], at: i64| {
            monitor::record(
                &db,
                &monitor::Read {
                    playlist_id: "VLPL1",
                    title: Some("Mix"),
                    account_id: None,
                    songs,
                    complete: true,
                    watch: true,
                    at,
                    listed: None,
                    added_at: None,
                    privacy: None,
                    reasons: None,
                },
            );
        };
        assert!(timeline(&db.snapshots("VLPL1")).is_empty(), "never synced: no history");
        read(&[row("a", "1"), row("b", "2"), row("c", "3")], 100);
        // c to the top, b gone, d new.
        read(&[row("c", "3"), row("a", "1"), row("d", "4")], 200);
        // The same content again files no snapshot, so no entry.
        read(&[row("c", "3"), row("a", "1"), row("d", "4")], 250);
        // d turns unavailable.
        let grey = SongItem { unavailable: true, ..row("d", "4") };
        read(&[row("c", "3"), row("a", "1"), grey], 300);

        let got = timeline(&db.snapshots("VLPL1"));
        let at: Vec<i64> = got.iter().map(|e| e.taken_at).collect();
        assert_eq!(at, [300, 200, 100], "newest first");
        assert!(got[2].baseline && !got[1].baseline && !got[0].baseline);
        assert!(got[2].changes.is_empty());
        assert_eq!(got[2].item_count, 3);

        let mid = &got[1];
        assert_eq!((mid.added, mid.removed, mid.moved), (1, 1, 1));
        let kinds: Vec<(&str, &str, Option<usize>, Option<usize>)> =
            mid.changes.iter().map(|c| (c.video_id.as_str(), c.kind, c.from, c.to)).collect();
        assert_eq!(
            kinds,
            [
                ("b", "removed", Some(1), None),
                ("d", "added", None, Some(2)),
                ("c", "moved", Some(2), Some(0))
            ]
        );
        assert_eq!(mid.changes[0].song.as_ref().map(|s| s.title.as_str()), Some("B"));

        let last = &got[0];
        assert_eq!((last.unavailable, last.added, last.removed, last.moved), (1, 0, 0, 0));
        assert_eq!(last.changes[0].kind, "unavailable");
        assert_eq!(last.title.as_deref(), Some("Mix"));

        // What the UI reads: kinds as strings, positions only where known.
        let json = serde_json::to_value(&got[1]).unwrap();
        assert_eq!(json["changes"][0]["kind"], "removed");
        assert!(json["changes"][0].get("to").is_none());
        assert_eq!(json["snapshot_id"], got[1].snapshot_id);
    }

    /// D1: the fork's `-forge.N` releases are stable; only rc/beta/alpha are prereleases.
    #[test]
    fn prerelease_is_rc_beta_or_alpha_never_forge() {
        // `-forge.2-rc.1` is stable by D1 (only the suffix's start counts): RCs are `x.y.z-rc.N`.
        for stable in [
            "1.2.0",
            "v1.2.0",
            "1.2.0-forge.1",
            "v1.2.0-forge.12",
            "1.2.0-forge.1+build.5",
            "1.2.0-forge.2-rc.1",
        ] {
            assert!(!is_prerelease(stable), "{stable} is stable");
        }
        for pre in ["1.2.0-rc.2", "v1.3.0-beta.1", "1.3.0-alpha", "1.2.0-RC.1", "1.3.0-rc.1+b.2"] {
            assert!(is_prerelease(pre), "{pre} is a prerelease");
        }
    }

    #[test]
    fn prerelease_self_update_needs_a_pubkey() {
        let cfg = |v: serde_json::Value| updater_pubkey_configured(Some(&v));
        assert!(!updater_pubkey_configured(None));
        assert!(!cfg(serde_json::json!({ "pubkey": "" })));
        assert!(!cfg(serde_json::json!({ "pubkey": "  " })));
        assert!(!cfg(serde_json::json!({ "endpoints": [] })));
        assert!(cfg(serde_json::json!({ "pubkey": "not-empty" })));
    }

    /// The shipped config: the fork's feed, and no key until the owner generates one.
    #[test]
    fn prerelease_updater_config_points_at_the_fork() {
        let conf: serde_json::Value =
            serde_json::from_str(include_str!("../tauri.conf.json")).expect("tauri.conf.json");
        let updater = &conf["plugins"]["updater"];
        assert_eq!(
            updater["endpoints"][0],
            "https://github.com/Kushro/limusic-forge/releases/latest/download/latest.json"
        );
        assert!(updater["pubkey"].is_string());
    }

    #[test]
    fn on_repeat_rows_shed_the_queue_slot_they_were_played_from() {
        let played = SongItem {
            video_id: "abc".into(),
            title: "Grace".into(),
            queued: true,
            queued_by: Some("simohypers".into()),
            autoplay: true,
            set_video_id: Some("SVI".into()),
            ..Default::default()
        };
        let row = shed_queue_context(played.clone());
        assert_eq!(
            row,
            SongItem { video_id: "abc".into(), title: "Grace".into(), ..Default::default() }
        );
        assert_eq!(row.title, played.title, "the song itself survives");
    }

    /// `set_setting` rejects any key outside `UI_SETTINGS`, and the UI swallows most of those
    /// errors, so a key added on the UI side alone silently never persists (`drop_mode` and
    /// `drop_dupes` did exactly that). Every literal `setSetting('<key>'` under `ui/src` has to be
    /// on the list.
    #[test]
    fn ui_settings_allow_every_key_the_ui_writes() {
        fn walk(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
            for entry in std::fs::read_dir(dir).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    walk(&path, out);
                } else if matches!(path.extension().and_then(|e| e.to_str()), Some("ts" | "svelte"))
                {
                    out.push(path);
                }
            }
        }
        let root = std::path::Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/../ui/src"));
        let mut files = Vec::new();
        walk(root, &mut files);

        let needle = "setSetting(";
        let mut seen = Vec::new();
        for file in &files {
            let src = std::fs::read_to_string(file).unwrap();
            for (at, _) in src.match_indices(needle) {
                let rest = &src[at + needle.len()..];
                let Some(quote) = rest.chars().next().filter(|c| *c == '\'' || *c == '"') else {
                    continue; // a computed key or the definition itself, nothing literal to check
                };
                let rest = &rest[1..];
                let key = &rest[..rest.find(quote).unwrap()];
                assert!(
                    UI_SETTINGS.contains(&key),
                    "{} writes setting `{key}`, which set_setting rejects",
                    file.display()
                );
                seen.push(key.to_owned());
            }
        }
        // Guards the scan itself: a moved ui/src or a renamed call would otherwise pass vacuously.
        assert!(seen.iter().any(|k| k == "drop_mode"), "scan found no setSetting calls");
        assert!(seen.iter().any(|k| k == "drop_dupes"));
        // The mini player's video toggle (SettingsDialog), written only from the UI side.
        assert!(seen.iter().any(|k| k == "mini_video"));
    }

    /// `mini_video` only switches on over `music_videos` (F3, invariant 5): refused with the code
    /// the settings dialog names while the main setting is off or was never written, and allowed
    /// to go off whatever the main setting says. Other keys never depend on anything.
    #[test]
    fn set_setting_refuses_mini_video_without_music_videos() {
        fn stored(music_videos: Option<&'static str>) -> impl Fn(&str) -> Option<String> {
            move |k: &str| if k == "music_videos" { music_videos.map(str::to_owned) } else { None }
        }
        for music_videos in [None, Some("false"), Some("")] {
            assert_eq!(
                setting_prerequisite("mini_video", "true", stored(music_videos)),
                Err("needs_music_videos".to_string()),
                "music_videos = {music_videos:?}"
            );
            assert_eq!(setting_prerequisite("mini_video", "false", stored(music_videos)), Ok(()));
            assert_eq!(setting_prerequisite("music_videos", "false", stored(music_videos)), Ok(()));
        }
        assert_eq!(setting_prerequisite("mini_video", "true", stored(Some("true"))), Ok(()));
        assert_eq!(setting_prerequisite("music_videos", "false", stored(Some("true"))), Ok(()));
        assert_eq!(setting_prerequisite("ambient_light", "true", stored(None)), Ok(()));
    }

    /// The Downloads tab writes its selects through a computed key, which the scan above skips.
    #[test]
    fn ui_settings_allow_every_download_setting() {
        for key in crate::download::settings::KEYS {
            assert!(UI_SETTINGS.contains(&key), "{key}");
        }
    }

    /// `set_setting`'s per-key rules: each refuses what its reader would misread, with the key
    /// and what it wanted in the message; a key without a rule takes anything, as before.
    #[test]
    fn set_setting_validates_values_per_key() {
        let accepted = [
            ("playlist_engine", "auto"),
            ("playlist_engine", "ytdata"),
            ("playlist_engine", "innertube"),
            ("job_queue_mode", "unified"),
            ("job_queue_mode", "ytdata_only"),
            ("budget.daily_units", "1"),
            ("budget.daily_units", "1000000"),
            ("budget.safety_margin_percent", "0"),
            ("budget.safety_margin_percent", "50"),
            ("budget.backup_reserve_units", "0"),
            ("budget.backup_reserve_units", "2500"),
            ("budget.backup_runs_per_day", "0"),
            ("budget.backup_runs_per_day", "24"),
            ("budget.opportunistic_mode", "true"),
            ("budget.opportunistic_mode", "false"),
            ("jobs.default_job_priority", "1"),
            ("jobs.default_job_priority", "3"),
            ("jobs.local_echo_max_age_s", "0"),
            ("jobs.local_echo_max_age_s", "900"),
            ("jobs.local_echo_max_age_s", "3600"),
            ("jobs.local_echo_max_age_s", "86400"),
            ("jobs.local_echo_max_age_s", "-1"),
            ("jobs.advance_jobs_headless", "true"),
            ("drop_mode", "ask"),
            ("drop_dupes", "consolidate"),
            ("tools.extract_new_mode", "create_transfer"),
            // No rule: anything goes.
            ("proxy", "socks5://whatever"),
            ("volume", "not even a number"),
        ];
        for (key, value) in accepted {
            assert_eq!(validate_setting(key, value), Ok(()), "{key} = {value:?}");
        }
        let refused = [
            ("playlist_engine", "Auto"),
            ("playlist_engine", ""),
            ("playlist_engine", "data_api"),
            ("job_queue_mode", "both"),
            ("budget.daily_units", "0"),
            ("budget.daily_units", "1000001"),
            ("budget.daily_units", "10k"),
            ("budget.daily_units", "1e4"),
            ("budget.safety_margin_percent", "51"),
            ("budget.safety_margin_percent", "-1"),
            ("budget.safety_margin_percent", "3.5"),
            ("budget.backup_reserve_units", "-500"),
            ("budget.backup_runs_per_day", "25"),
            ("budget.opportunistic_mode", "yes"),
            ("budget.opportunistic_mode", "1"),
            ("jobs.default_job_priority", "0"),
            ("jobs.default_job_priority", "4"),
            ("jobs.local_echo_max_age_s", "60"),
            ("jobs.local_echo_max_age_s", "-3600"),
            ("jobs.local_echo_max_age_s", "always"),
            ("jobs.advance_jobs_headless", "on"),
            ("drop_mode", "drop"),
            ("drop_dupes", "keep"),
            ("tools.extract_new_mode", "create"),
        ];
        for (key, value) in refused {
            let err = validate_setting(key, value).expect_err(&format!("{key} = {value:?}"));
            assert!(err.contains(key) && err.contains("expected"), "{err}");
        }
        let err = validate_setting("budget.daily_units", "0").unwrap_err();
        assert!(err.contains("a whole number from 1 to 1000000"), "{err}");
        let err = validate_setting("jobs.local_echo_max_age_s", "60").unwrap_err();
        assert!(err.contains("0, 900, 1800, 3600, 10800, 43200, 86400, -1"), "{err}");
    }

    /// A rule for a key the UI cannot write would never run.
    #[test]
    fn every_validated_setting_is_one_the_ui_may_write() {
        let keys = [
            "playlist_engine",
            "job_queue_mode",
            "budget.daily_units",
            "budget.safety_margin_percent",
            "budget.backup_reserve_units",
            "budget.backup_runs_per_day",
            "budget.opportunistic_mode",
            "jobs.default_job_priority",
            "jobs.local_echo_max_age_s",
            "jobs.advance_jobs_headless",
            "drop_mode",
            "drop_dupes",
            "tools.extract_new_mode",
        ];
        for key in keys {
            assert!(setting_rule(key).is_some(), "{key} has no rule");
            assert!(UI_SETTINGS.contains(&key), "{key}");
        }
    }

    /// A loopback server that answers every request with `body` (HTTP 200, JSON), for as many
    /// requests as `times`. Answers its base URL.
    fn fake_api(body: &'static str, times: usize) -> String {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        std::thread::spawn(move || {
            for stream in listener.incoming().take(times) {
                let Ok(mut stream) = stream else { continue };
                let mut request = Vec::new();
                let mut buf = [0u8; 1024];
                while !request.windows(4).any(|w| w == b"\r\n\r\n") {
                    match stream.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => request.extend_from_slice(&buf[..n]),
                    }
                }
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\
                     Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(response.as_bytes());
            }
        });
        base
    }

    const CHANNEL_ONE: &str = r#"{"items": [{"id": "UCfakeChannelOne", "snippet": {
        "title": "Channel One",
        "thumbnails": {"default": {"url": "https://yt3.example/1.jpg"}}}}]}"#;
    const CHANNEL_TWO: &str = r#"{"items": [{"id": "UCfakeChannelTwo", "snippet": {
        "title": "Channel Two", "thumbnails": {}}}]}"#;

    fn fake_tokens(refresh: &str) -> ytdata::auth::flow::AuthorizedTokens {
        ytdata::auth::flow::AuthorizedTokens {
            access_token: "FAKE-access-token".into(),
            refresh_token: refresh.into(),
            expires_at: std::time::Instant::now() + std::time::Duration::from_secs(3600),
        }
    }

    fn manager_for(
        db: &Arc<crate::db::Db>,
        store: Arc<ytdata::auth::store::InMemoryTokenStore>,
        base_url: String,
    ) -> ytdata::auth::accounts::AccountManager {
        let repo = Arc::new(crate::ytdata_accounts::DbAccountsRepo::new(db.clone()));
        let client = ytdata::client::YouTubeClient::new().unwrap().with_base_url(base_url);
        ytdata::auth::accounts::AccountManager::new(store, repo).unwrap().with_yt_client(client)
    }

    async fn connect(
        manager: &ytdata::auth::accounts::AccountManager,
        db: &crate::db::Db,
        refresh: &str,
        active: Option<&str>,
        now: chrono::DateTime<chrono::Utc>,
    ) -> crate::ytdata_accounts::YtDataAccount {
        save_connected(manager, db, fake_tokens(refresh), active, now).await.unwrap()
    }

    /// The part of connecting that is the app's: the refresh token lands in the token store, the
    /// channel in `ytdata_accounts` (and in the manager's own list), the `channels.list` unit in
    /// the ledger, and the channel is linked to the signed-in cookie account only while that one
    /// has none.
    #[tokio::test]
    async fn a_connected_channel_is_stored_counted_and_linked_once() {
        use ytdata::auth::accounts::AccountStatus;
        use ytdata::auth::store::TokenStore;
        let db = Arc::new(crate::db::Db::open(std::path::Path::new(":memory:")).unwrap());
        let store = Arc::new(ytdata::auth::store::InMemoryTokenStore::default());
        let now = chrono::DateTime::parse_from_rfc3339("2026-07-15T19:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);

        let one = manager_for(&db, store.clone(), fake_api(CHANNEL_ONE, 1));
        let saved = connect(&one, &db, "FAKE-refresh-1", Some("ga1"), now).await;
        assert_eq!(saved.channel_id, "UCfakeChannelOne");
        assert_eq!(saved.title, "Channel One");
        assert_eq!(saved.thumb.as_deref(), Some("https://yt3.example/1.jpg"));
        assert_eq!(saved.status, AccountStatus::Connected);
        assert_eq!(saved.linked_account.as_deref(), Some("ga1"), "ga1 had no channel");
        assert_eq!(store.load("UCfakeChannelOne").unwrap().as_deref(), Some("FAKE-refresh-1"));
        assert!(one.account("UCfakeChannelOne").is_some(), "the manager knows it too");
        assert_eq!(crate::quota::spent_today(&db, now).unwrap(), 1, "one channels.list");

        // A second channel while ga1 already has one: stored, not linked.
        let two = manager_for(&db, store.clone(), fake_api(CHANNEL_TWO, 1));
        let saved = connect(&two, &db, "FAKE-refresh-2", Some("ga1"), now).await;
        assert_eq!(saved.linked_account, None);
        assert_eq!(store.load("UCfakeChannelTwo").unwrap().as_deref(), Some("FAKE-refresh-2"));
        let rows = crate::ytdata_accounts::list(&db).unwrap();
        assert_eq!(rows.len(), 2, "the second manager's save kept the first row");

        // Reconnecting the first one (a new refresh token) keeps its link and its date.
        let again = manager_for(&db, store.clone(), fake_api(CHANNEL_ONE, 1));
        let saved = connect(&again, &db, "FAKE-refresh-3", None, now).await;
        assert_eq!(saved.linked_account.as_deref(), Some("ga1"));
        assert_eq!(store.load("UCfakeChannelOne").unwrap().as_deref(), Some("FAKE-refresh-3"));
        assert_eq!(crate::ytdata_accounts::list(&db).unwrap().len(), 2);
    }

    #[test]
    fn linking_keeps_one_channel_per_cookie_account() {
        use crate::ytdata_accounts as ya;
        let db = crate::db::Db::open(std::path::Path::new(":memory:")).unwrap();
        ya::upsert(&db, "UC1", "One", None, 1).unwrap();
        ya::upsert(&db, "UC2", "Two", None, 2).unwrap();
        let linked = |id: &str| ya::get(&db, id).unwrap().unwrap().linked_account;

        assert!(!link_if_unpaired(&db, "UC1", None).unwrap(), "nobody signed in");
        assert!(link_if_unpaired(&db, "UC1", Some("ga1")).unwrap());
        assert!(!link_if_unpaired(&db, "UC2", Some("ga1")).unwrap(), "ga1 already has UC1");
        assert!(!link_if_unpaired(&db, "UC1", Some("ga2")).unwrap(), "UC1 is already linked");
        assert!(!link_if_unpaired(&db, "UC9", Some("ga3")).unwrap(), "not connected");

        assert!(link_account(&db, "UC2", Some("ga1")).unwrap());
        assert_eq!(linked("UC2").as_deref(), Some("ga1"));
        assert_eq!(linked("UC1"), None, "the one ga1 had before is unlinked");
        assert!(link_account(&db, "UC2", None).unwrap());
        assert_eq!(linked("UC2"), None);
        assert!(!link_account(&db, "UC9", Some("ga1")).unwrap());
    }

    #[test]
    fn a_failed_connection_says_why_without_a_token() {
        use ytdata::error::AuthError;
        let cancelled = ConnectFinished::failed(3, &AuthError::Cancelled.into());
        let json = serde_json::to_value(&cancelled).unwrap();
        assert_eq!(json, json!({ "attempt": 3, "ok": false, "code": "cancelled" }));
        let timed_out = ConnectFinished::failed(4, &AuthError::TimedOut.into());
        assert_eq!(timed_out.code, Some("timed_out"));
        let failed = ConnectFinished::failed(5, &AuthError::InvalidGrant.into());
        assert_eq!(failed.code, Some("failed"));
        assert!(failed.error.unwrap().contains("invalid_grant"));
        let not_json = serde_json::from_str::<Value>("{ FAKE-file-content").unwrap_err();
        let msg = import_error(&ytdata::error::Error::Serde(not_json));
        assert!(!msg.contains("FAKE-file-content"), "{msg}");
    }

    #[test]
    fn a_song_item_from_the_ui_deserializes_into_a_download_song() {
        let song: DownloadSong = serde_json::from_value(json!({
            "video_id": "abc",
            "title": "Song",
            "artists": "Artist",
            "duration": "3:45",
            "thumbnail": "https://example.invalid/t.jpg",
            "album": "Album"
        }))
        .unwrap();
        assert_eq!(song.video_id, "abc");
        assert_eq!(song.title.as_deref(), Some("Song"));
        assert_eq!(song.artists.as_deref(), Some("Artist"));
        assert_eq!(song.duration.as_deref(), Some("3:45"));
        let bare: DownloadSong = serde_json::from_value(json!({ "video_id": "xyz" })).unwrap();
        assert_eq!(bare.title, None);
    }

    #[test]
    fn the_monitor_interval_decides_when_a_sync_is_due() {
        let db = crate::db::Db::open(std::path::Path::new(":memory:")).unwrap();
        // Never built: due, whatever the interval says.
        db.set_setting("monitor_interval_hours", "0");
        assert!(playlist_index_due(&db, 0));
        db.set_setting(PLAYLIST_INDEX_STAMP, "1000");
        assert!(!playlist_index_due(&db, 1000 + 365 * 86_400), "off means never again");
        // The default is six hours, and so is anything the UI would not have written.
        for stored in [None, Some("5"), Some("soon")] {
            match stored {
                Some(v) => db.set_setting("monitor_interval_hours", v),
                None => db.delete_setting("monitor_interval_hours"),
            }
            assert!(!playlist_index_due(&db, 1000 + 6 * 3600 - 1), "{stored:?}");
            assert!(playlist_index_due(&db, 1000 + 6 * 3600), "{stored:?}");
        }
        db.set_setting("monitor_interval_hours", "1");
        assert!(playlist_index_due(&db, 1000 + 3600));
        assert_eq!(sync_trigger(Some("scheduler")), "scheduler");
        assert_eq!(sync_trigger(Some("anything")), "manual_ui");
        assert_eq!(sync_trigger(None), "manual_ui");
    }

    #[test]
    fn failed_syncs_back_the_scheduler_off_until_one_succeeds() {
        let db = crate::db::Db::open(std::path::Path::new(":memory:")).unwrap();
        assert!(monitor_retry_due(&db, 0), "nothing failed yet");
        note_sync_attempt(&db, 1000, false);
        assert!(!monitor_retry_due(&db, 1000 + 60), "not a minute later");
        assert!(monitor_retry_due(&db, 1000 + 15 * 60));
        note_sync_attempt(&db, 2000, false);
        assert!(!monitor_retry_due(&db, 2000 + 15 * 60), "the wait doubles");
        assert!(monitor_retry_due(&db, 2000 + 30 * 60));
        note_sync_attempt(&db, 5000, true);
        assert!(monitor_retry_due(&db, 5001), "a success clears it");
        db.set_setting("monitor_interval_hours", "0");
        note_sync_attempt(&db, 6000, false);
        assert!(!monitor_retry_due(&db, 6000 + 365 * 86_400), "interval off: no retries");
    }
}
