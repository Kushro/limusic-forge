//! Local echo: a Data API write that just succeeded is mirrored into the local playlist index right
//! away, instead of waiting for (and paying for) the next sync to rediscover it. Port of
//! PlaylistForge's `pf-app/src/jobs/local_echo.rs`.
//!
//! An echo edits a copy that may be stale: it assumes the index still matches YouTube apart from
//! this one change. So it is gated on how fresh the playlist's last sync is
//! (`playlist_sync.synced_at`), with a window the user picks on an eight-position slider, from
//! "never" to "always". Nothing is at risk either way: the next real sync replaces the index
//! wholesale, and an echo never writes to YouTube. It never fails the job: every error is logged
//! and dropped.
//!
//! InnerTube jobs need none of this: `playlist_tools::rows` already updates the index as it writes.
//!
//! Stored as one integer in `jobs.local_echo_max_age_s`: `0` off, `-1` (any negative) always,
//! otherwise seconds. Missing or unparseable reads as the one-hour default.

use chrono::{DateTime, TimeZone, Utc};
use rusqlite::OptionalExtension;

use super::planner::PlannedAction;
use crate::db::Db;

pub const LOCAL_ECHO_MAX_AGE_KEY: &str = "jobs.local_echo_max_age_s";
pub const DEFAULT_LOCAL_ECHO_MAX_AGE_S: i64 = 3600;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EchoWindow {
    Off,
    /// Echo if the playlist synced at most this many seconds ago. Always positive.
    MaxAge(i64),
    Always,
}

impl EchoWindow {
    /// The settings slider, least to most risky (left to right).
    pub const SLIDER: [EchoWindow; 8] = [
        EchoWindow::Off,
        EchoWindow::MaxAge(15 * 60),
        EchoWindow::MaxAge(30 * 60),
        EchoWindow::MaxAge(3600),
        EchoWindow::MaxAge(3 * 3600),
        EchoWindow::MaxAge(12 * 3600),
        EchoWindow::MaxAge(24 * 3600),
        EchoWindow::Always,
    ];

    pub fn parse(raw: Option<&str>) -> EchoWindow {
        match raw.map(str::trim).filter(|s| !s.is_empty()).and_then(|s| s.parse::<i64>().ok()) {
            Some(0) => EchoWindow::Off,
            Some(n) if n < 0 => EchoWindow::Always,
            Some(n) => EchoWindow::MaxAge(n),
            None => EchoWindow::MaxAge(DEFAULT_LOCAL_ECHO_MAX_AGE_S),
        }
    }

    pub fn to_setting_value(self) -> String {
        match self {
            EchoWindow::Off => "0".to_string(),
            EchoWindow::Always => "-1".to_string(),
            EchoWindow::MaxAge(seconds) => seconds.to_string(),
        }
    }

    /// May an echo go in now, given when the playlist last synced? A playlist never synced has
    /// nothing worth echoing into (only "always" ignores that); a sync date in the future (clock
    /// skew) reads as "just synced".
    pub fn allows(self, last_sync_at: Option<DateTime<Utc>>, now: DateTime<Utc>) -> bool {
        match self {
            EchoWindow::Off => false,
            EchoWindow::Always => true,
            EchoWindow::MaxAge(max_age_s) => {
                last_sync_at.is_some_and(|at| (now - at).num_seconds() <= max_age_s)
            }
        }
    }

    /// Position on [`Self::SLIDER`]. A stored duration between two positions snaps to the safer
    /// one, so the slider never shows a window shorter than the one in force.
    pub fn slider_index(self) -> usize {
        match self {
            EchoWindow::Off => 0,
            EchoWindow::Always => EchoWindow::SLIDER.len() - 1,
            EchoWindow::MaxAge(seconds) => EchoWindow::SLIDER
                .iter()
                .rposition(|w| matches!(w, EchoWindow::MaxAge(s) if *s <= seconds))
                .unwrap_or(1),
        }
    }
}

pub fn window(db: &Db) -> EchoWindow {
    EchoWindow::parse(db.get_setting(LOCAL_ECHO_MAX_AGE_KEY).as_deref())
}

pub fn set_window(db: &Db, window: EchoWindow) {
    db.set_setting(LOCAL_ECHO_MAX_AGE_KEY, &window.to_setting_value());
}

/// The index's id for a Data API playlist id.
fn index_id(playlist_id: &str) -> String {
    if playlist_id.starts_with("VL") {
        playlist_id.to_string()
    } else {
        format!("VL{playlist_id}")
    }
}

fn last_sync(db: &Db, index_id: &str) -> Option<DateTime<Utc>> {
    let secs: Option<i64> = db
        .conn()
        .query_row("SELECT synced_at FROM playlist_sync WHERE playlist_id = ?1", [index_id], |r| {
            r.get(0)
        })
        .optional()
        .ok()
        .flatten();
    Utc.timestamp_opt(secs?, 0).single()
}

/// The runner's hook, right after an item is recorded done. Mirrors an insert or a delete of a
/// playlist row when the freshness window allows; anything else, or any failure, is a no-op.
/// Returns whether it echoed (for the tests).
pub fn echo_completed_action(db: &Db, action: &PlannedAction, now: DateTime<Utc>) -> bool {
    let (playlist_id, video_id) = match action {
        PlannedAction::PlaylistItemInsert { playlist_id, video_id, .. }
        | PlannedAction::PlaylistItemDelete { playlist_id, video_id, .. } => {
            (playlist_id, video_id)
        }
        _ => return false,
    };
    let window = window(db);
    if window == EchoWindow::Off {
        return false;
    }
    let key = index_id(playlist_id);
    if !window.allows(last_sync(db, &key), now) {
        tracing::debug!(playlist_id, "local echo skipped: the index is older than the window");
        return false;
    }
    match action {
        PlannedAction::PlaylistItemInsert { .. } => db.add_playlist_track(&key, video_id),
        // The index holds a video once per playlist: taking out one of two copies takes the
        // video out until the next sync puts it back, which only ever under-reports.
        _ => db.remove_playlist_track(&key, video_id),
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn now() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-07-15T12:00:00Z").unwrap().with_timezone(&Utc)
    }

    #[test]
    fn parse_maps_the_sentinels_seconds_and_garbage() {
        let default = EchoWindow::MaxAge(DEFAULT_LOCAL_ECHO_MAX_AGE_S);
        assert_eq!(EchoWindow::parse(Some("0")), EchoWindow::Off);
        assert_eq!(EchoWindow::parse(Some("-1")), EchoWindow::Always);
        assert_eq!(EchoWindow::parse(Some("-3600")), EchoWindow::Always);
        assert_eq!(EchoWindow::parse(Some(" 900 ")), EchoWindow::MaxAge(900));
        for raw in [None, Some(""), Some("  "), Some("banana"), Some("3.5")] {
            assert_eq!(EchoWindow::parse(raw), default, "{raw:?}");
        }
        for w in EchoWindow::SLIDER {
            assert_eq!(EchoWindow::parse(Some(&w.to_setting_value())), w);
        }
    }

    #[test]
    fn allows_up_to_and_including_the_boundary() {
        let w = EchoWindow::MaxAge(3600);
        assert!(w.allows(Some(now() - chrono::Duration::seconds(3600)), now()));
        assert!(!w.allows(Some(now() - chrono::Duration::seconds(3601)), now()));
        assert!(!w.allows(None, now()), "never synced");
        assert!(w.allows(Some(now() + chrono::Duration::minutes(5)), now()), "clock skew");
        assert!(!EchoWindow::Off.allows(Some(now()), now()));
        assert!(EchoWindow::Always.allows(None, now()));
    }

    #[test]
    fn slider_positions_map_to_themselves_and_off_scale_values_snap_down() {
        for (i, w) in EchoWindow::SLIDER.into_iter().enumerate() {
            assert_eq!(w.slider_index(), i, "{w:?}");
        }
        assert_eq!(EchoWindow::MaxAge(45 * 60).slider_index(), 2);
        assert_eq!(EchoWindow::MaxAge(30).slider_index(), 1);
        assert_eq!(EchoWindow::MaxAge(72 * 3600).slider_index(), 6);
    }

    fn synced(db: &Db, playlist: &str, at: DateTime<Utc>) {
        db.conn()
            .execute(
                "INSERT INTO playlist_sync(playlist_id, synced_at, item_count) VALUES(?1, ?2, 0)",
                rusqlite::params![playlist, at.timestamp()],
            )
            .unwrap();
    }

    fn indexed(db: &Db, playlist: &str) -> Vec<String> {
        db.playlist_memberships()
            .into_iter()
            .filter(|(_, ids)| ids.iter().any(|id| id == playlist))
            .map(|(v, _)| v)
            .collect()
    }

    #[test]
    fn a_fresh_index_gets_inserts_and_deletes_and_a_stale_one_is_left_alone() {
        let db = Db::open(std::path::Path::new(":memory:")).unwrap();
        assert_eq!(window(&db), EchoWindow::MaxAge(3600));
        synced(&db, "VLPLfresh", now() - chrono::Duration::minutes(5));
        synced(&db, "VLPLstale", now() - chrono::Duration::hours(9));
        let insert = |pl: &str| PlannedAction::PlaylistItemInsert {
            playlist_id: pl.into(),
            video_id: "v1".into(),
            position: None,
        };
        assert!(echo_completed_action(&db, &insert("PLfresh"), now()));
        assert_eq!(indexed(&db, "VLPLfresh"), ["v1"]);
        assert!(!echo_completed_action(&db, &insert("PLstale"), now()));
        assert!(indexed(&db, "VLPLstale").is_empty());

        let delete = PlannedAction::PlaylistItemDelete {
            playlist_item_id: "pi".into(),
            video_id: "v1".into(),
            playlist_id: "PLfresh".into(),
            prev_position: None,
        };
        assert!(echo_completed_action(&db, &delete, now()));
        assert!(indexed(&db, "VLPLfresh").is_empty());

        set_window(&db, EchoWindow::Off);
        assert!(!echo_completed_action(&db, &insert("PLfresh"), now()));
        set_window(&db, EchoWindow::Always);
        assert!(echo_completed_action(&db, &insert("PLnever"), now()), "always: even unsynced");
        let rename = PlannedAction::PlaylistDelete { playlist_id: "PLfresh".into() };
        assert!(!echo_completed_action(&db, &rename, now()), "not a row change");
    }
}
