//! PlaylistForge's vocabulary in Forge's terms: alert kinds, settings keys, theme, language,
//! dates. Pure functions over the [`super::reader`] model; commit 31 writes the results.

use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};

use super::reader::{PfAlert, PfConfig};
use crate::db::{parse_rfc3339, rfc3339_text};

// --- dates --------------------------------------------------------------------------------------

/// A PlaylistForge date (`to_rfc3339()`: `+00:00`, maybe fractional seconds) in Forge's text form,
/// `YYYY-MM-DDTHH:MM:SSZ` (what `jobs`, `job_items`, `quota_ledger` and `downloads` store). An
/// unreadable value reads as the epoch, as [`parse_rfc3339`] does.
pub fn forge_date(raw: &str) -> String {
    rfc3339_text(parse_rfc3339(raw))
}

/// [`forge_date`] for a nullable column. Blank stays `None`.
pub fn forge_date_opt(raw: Option<&str>) -> Option<String> {
    raw.map(str::trim).filter(|s| !s.is_empty()).map(forge_date)
}

/// A PlaylistForge date as Unix seconds (Forge's INTEGER date columns: `added_at`, `synced_at`,
/// `playlist_alert.at`...).
pub fn epoch_secs(raw: &str) -> i64 {
    parse_rfc3339(raw).timestamp()
}

/// The instant of a date already in Forge's text form.
pub fn instant(forge_text: &str) -> DateTime<Utc> {
    parse_rfc3339(forge_text)
}

// --- alerts -------------------------------------------------------------------------------------

/// PlaylistForge's `alerts.kind` as Forge's `playlist_alert.kind`. `None` for a kind Forge has no
/// equivalent of (the row is skipped).
pub fn alert_kind(pf_kind: &str) -> Option<&'static str> {
    Some(match pf_kind {
        "video_deleted" | "video_private" => "unavailable",
        "video_restored" => "restored",
        "item_added" => "added",
        "item_removed" => "removed",
        "item_moved" => "moved",
        _ => return None,
    })
}

/// One PlaylistForge alert ready for Forge's `playlist_alert` (`db::NewAlert` plus `seen`).
#[derive(Debug, Clone, PartialEq)]
pub struct MappedAlert {
    pub playlist_id: String,
    pub video_id: String,
    pub kind: &'static str,
    /// `created_at` in Unix seconds.
    pub at: i64,
    /// Kept as PlaylistForge had it.
    pub seen: bool,
    /// The positions a `moved` row went between; `removed` has only `from_pos`, `added` only
    /// `to_pos` (0-based, as PlaylistForge wrote them).
    pub from_pos: Option<i64>,
    pub to_pos: Option<i64>,
    /// From the alert's payload, for building `song_json`.
    pub title: Option<String>,
    pub channel: Option<String>,
    /// Forge's dedupe key, scoped by PlaylistForge's own key (or row id) so each PF event stays a
    /// row of its own and importing twice files nothing new.
    pub dedupe_key: String,
}

/// A PlaylistForge alert in Forge's model, or `None` when Forge cannot hold it: an unknown kind,
/// or no playlist or video to hang it on.
pub fn alert(a: &PfAlert) -> Option<MappedAlert> {
    let kind = alert_kind(&a.kind)?;
    let playlist_id = a.playlist_id.clone().filter(|s| !s.is_empty())?;
    let payload: serde_json::Value =
        serde_json::from_str(&a.payload_json).unwrap_or(serde_json::Value::Null);
    let video_id = a
        .video_id
        .clone()
        .or_else(|| payload.get("video_id").and_then(|v| v.as_str()).map(str::to_owned))
        .filter(|s| !s.is_empty())?;
    let int = |k: &str| payload.get(k).and_then(serde_json::Value::as_i64);
    let text = |k: &str| payload.get(k).and_then(|v| v.as_str()).map(str::to_owned);
    let (from_pos, to_pos) = match kind {
        "moved" => (int("from"), int("to")),
        "removed" => (int("position"), None),
        "added" => (None, int("position")),
        _ => (None, None),
    };
    let scope = if a.dedupe_key.is_empty() {
        format!("pf:{}", a.id)
    } else {
        format!("pf:{}", a.dedupe_key)
    };
    Some(MappedAlert {
        dedupe_key: crate::db::alert_dedupe_key(&playlist_id, &video_id, kind, Some(&scope)),
        playlist_id,
        video_id,
        kind,
        at: epoch_secs(&a.created_at),
        seen: a.seen,
        from_pos,
        to_pos,
        title: text("title"),
        channel: text("channel"),
    })
}

// --- jobs ---------------------------------------------------------------------------------------

/// Job states that still have work to do: what commit 31 brings over to the queue (D-F7).
pub fn job_is_pending(status: &str) -> bool {
    matches!(
        status,
        "queued"
            | "running"
            | "verifying"
            | "paused_user"
            | "paused_network"
            | "waiting_quota"
            | "waiting_auth"
    )
}

/// The state a pending job enters Forge's queue in. A job PlaylistForge left `running` (it was
/// closed mid-run) goes back to `queued`, as Forge's own runner does on start-up; everything else
/// keeps its state (the vocabularies are the same, Forge's queue is a port of PlaylistForge's).
pub fn job_status_on_import(status: &str) -> &str {
    match status {
        "running" => "queued",
        other => other,
    }
}

// --- config.json --------------------------------------------------------------------------------

/// Light or dark, as Forge's appearance toggle (mode-watcher) has it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ThemeMode {
    Dark,
    Light,
}

/// A Forge theme preset (`ui/src/lib/theme.svelte.ts` `ThemeId`, kept in localStorage, so the UI
/// applies it) and, when PlaylistForge's theme implies one, the light/dark mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub struct ThemeChoice {
    pub id: &'static str,
    pub mode: Option<ThemeMode>,
}

/// PlaylistForge's `theme`: `dark`/`light` are Forge's default palette in that mode; `catppuccin`,
/// `nord` and `dracula` are the presets of the same name. Anything else is PlaylistForge's own
/// fallback, dark.
pub fn theme(pf_theme: &str) -> ThemeChoice {
    match pf_theme.trim() {
        "light" => ThemeChoice { id: "default", mode: Some(ThemeMode::Light) },
        "catppuccin" => ThemeChoice { id: "catppuccin", mode: None },
        "nord" => ThemeChoice { id: "nord", mode: None },
        "dracula" => ThemeChoice { id: "dracula", mode: None },
        _ => ThemeChoice { id: "default", mode: Some(ThemeMode::Dark) },
    }
}

/// PlaylistForge's `lang` as Forge's `locale` setting. PlaylistForge had `es` and `en` and read
/// anything else as `es`.
pub fn locale(pf_lang: &str) -> &'static str {
    match pf_lang.trim() {
        "en" => "en",
        _ => "es",
    }
}

/// The Forge settings `config.json` carries: `locale` only (the theme is a UI preference, see
/// [`theme`]).
pub fn config_settings(config: &PfConfig) -> Vec<(String, String)> {
    vec![("locale".to_string(), locale(&config.lang).to_string())]
}

// --- settings -----------------------------------------------------------------------------------

/// Why a PlaylistForge setting was not brought over.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SkipReason {
    /// Forge has no such setting (`monitor.schedule_time`, `ui.recent_drop_dests`...).
    Unknown,
    /// `downloads.cookies_browser`: Forge takes cookies from its own session.
    NotCarried,
    /// A value the Forge setting does not accept.
    BadValue,
    /// `monitor.backups_dir` inside PlaylistForge's (or upstream's) folders, or not absolute:
    /// Forge would write and prune there (R2 I3). Forge keeps its default.
    ForbiddenDir,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct SkippedSetting {
    pub key: String,
    pub reason: SkipReason,
}

/// PlaylistForge's `settings` rows in Forge's keys.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MappedSettings {
    /// `(forge key, value)`, ready for `Db::set_setting`. Budget and jobs values are passed as
    /// PlaylistForge stored them; the caller runs them through the same per-key validation as the
    /// settings dialog.
    pub settings: Vec<(String, String)>,
    pub skipped: Vec<SkippedSetting>,
}

const DOWNLOAD_KEYS: [&str; 5] = [
    "downloads.dir",
    "downloads.default_format",
    "downloads.audio_quality",
    "downloads.video_quality",
    "downloads.thumbnail_mode",
];
const BUDGET_KEYS: [&str; 4] = [
    "budget.safety_margin_percent",
    "budget.backup_reserve_units",
    "budget.opportunistic_mode",
    "budget.backup_runs_per_day",
];
const JOBS_KEYS: [&str; 3] =
    ["jobs.default_job_priority", "jobs.local_echo_max_age_s", "jobs.advance_jobs_headless"];

/// Map PlaylistForge's settings. `forbidden` is [`crate::backups::forbidden_roots`] (injected so
/// tests never look at the real per-user folders).
pub fn settings(pf: &[(String, String)], forbidden: &[PathBuf]) -> MappedSettings {
    let mut out = MappedSettings::default();
    for (key, value) in pf {
        let value = value.trim();
        let k = key.as_str();
        let mapped: Result<(&str, String), SkipReason> = match k {
            "ui.drop_mode" => one_of(value, &["ask", "copy", "move"]).map(|v| ("drop_mode", v)),
            "ui.drop_dup_policy" => {
                one_of(value, &["skip", "allow", "consolidate"]).map(|v| ("drop_dupes", v))
            }
            "ui.playlists_view" => {
                one_of(value, &["grid", "list"]).map(|v| ("library_playlists_view", v))
            }
            "retention_keep_last" => match value.parse::<u32>() {
                Ok(n) if n >= 1 => Ok((crate::backups::KEEP_KEY, n.to_string())),
                _ => Err(SkipReason::BadValue),
            },
            "monitor.backups_dir" => {
                backups_dir(value, forbidden).map(|v| (crate::backups::DIR_KEY, v))
            }
            "downloads.cookies_browser" => Err(SkipReason::NotCarried),
            _ if DOWNLOAD_KEYS.contains(&k)
                || BUDGET_KEYS.contains(&k)
                || JOBS_KEYS.contains(&k) =>
            {
                if value.is_empty() {
                    Err(SkipReason::BadValue)
                } else {
                    Ok((k, value.to_string()))
                }
            }
            _ => Err(SkipReason::Unknown),
        };
        match mapped {
            Ok((k, v)) => out.settings.push((k.to_string(), v)),
            Err(reason) => out.skipped.push(SkippedSetting { key: key.clone(), reason }),
        }
    }
    out
}

fn one_of(value: &str, allowed: &[&str]) -> Result<String, SkipReason> {
    if allowed.contains(&value) {
        Ok(value.to_string())
    } else {
        Err(SkipReason::BadValue)
    }
}

/// `monitor.backups_dir` only when it is an absolute folder outside every forbidden root.
fn backups_dir(value: &str, forbidden: &[PathBuf]) -> Result<String, SkipReason> {
    let dir = Path::new(value);
    if value.is_empty() || !dir.is_absolute() || crate::backups::is_forbidden(dir, forbidden) {
        Err(SkipReason::ForbiddenDir)
    } else {
        Ok(value.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kv(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    fn pf_alert(kind: &str, video: Option<&str>, payload: &str, seen: bool) -> PfAlert {
        PfAlert {
            id: 7,
            account_id: "UCa".into(),
            playlist_id: Some("PL1".into()),
            kind: kind.into(),
            video_id: video.map(str::to_owned),
            payload_json: payload.into(),
            created_at: "2026-10-05T10:00:01+00:00".into(),
            seen,
            dedupe_key: format!("PL1|2|{kind}|x"),
        }
    }

    #[test]
    fn pf_import_alert_kinds_map_to_forge() {
        assert_eq!(alert_kind("video_deleted"), Some("unavailable"));
        assert_eq!(alert_kind("video_private"), Some("unavailable"));
        assert_eq!(alert_kind("video_restored"), Some("restored"));
        assert_eq!(alert_kind("item_added"), Some("added"));
        assert_eq!(alert_kind("item_removed"), Some("removed"));
        assert_eq!(alert_kind("item_moved"), Some("moved"));
        assert_eq!(alert_kind("gone"), None);
        assert_eq!(alert_kind(""), None);
    }

    #[test]
    fn pf_import_alert_keeps_seen_positions_and_a_stable_scoped_key() {
        let moved =
            alert(&pf_alert("item_moved", Some("vid1"), r#"{"from":0,"to":3}"#, true)).unwrap();
        assert_eq!(moved.kind, "moved");
        assert!(moved.seen);
        assert_eq!((moved.from_pos, moved.to_pos), (Some(0), Some(3)));
        assert_eq!(moved.at, 1_791_194_401);
        assert_eq!(
            moved.dedupe_key,
            crate::db::alert_dedupe_key("PL1", "vid1", "moved", Some("pf:PL1|2|item_moved|x"))
        );
        // Same row, same key: importing twice files nothing new.
        let again =
            alert(&pf_alert("item_moved", Some("vid1"), r#"{"from":0,"to":3}"#, true)).unwrap();
        assert_eq!(again.dedupe_key, moved.dedupe_key);

        let removed = alert(&pf_alert(
            "item_removed",
            Some("vid1"),
            r#"{"position":4,"title":"T","channel":"C"}"#,
            false,
        ))
        .unwrap();
        assert_eq!((removed.from_pos, removed.to_pos), (Some(4), None));
        assert_eq!((removed.title.as_deref(), removed.channel.as_deref()), (Some("T"), Some("C")));
        assert!(!removed.seen);

        let added =
            alert(&pf_alert("item_added", Some("vid1"), r#"{"position":2}"#, false)).unwrap();
        assert_eq!((added.from_pos, added.to_pos), (None, Some(2)));
    }

    #[test]
    fn pf_import_alert_without_a_place_is_skipped() {
        assert!(alert(&pf_alert("mystery", Some("v"), "{}", false)).is_none());
        let mut no_playlist = pf_alert("item_added", Some("v"), "{}", false);
        no_playlist.playlist_id = None;
        assert!(alert(&no_playlist).is_none());
        // The video can come from the payload when the column was nulled.
        let from_payload =
            alert(&pf_alert("video_deleted", None, r#"{"video_id":"vidP"}"#, false)).unwrap();
        assert_eq!(from_payload.video_id, "vidP");
        assert!(alert(&pf_alert("video_deleted", None, "{}", false)).is_none());
        // Without PF's key the row id scopes it.
        let mut keyless = pf_alert("item_added", Some("v"), "{}", false);
        keyless.dedupe_key.clear();
        assert!(alert(&keyless).unwrap().dedupe_key.ends_with("pf:7"));
    }

    #[test]
    fn pf_import_dates_become_forge_text() {
        assert_eq!(forge_date("2026-10-05T10:00:00.250+00:00"), "2026-10-05T10:00:00Z");
        assert_eq!(forge_date("2026-10-05T12:00:00+02:00"), "2026-10-05T10:00:00Z");
        assert_eq!(forge_date("garbage"), "1970-01-01T00:00:00Z");
        assert_eq!(forge_date_opt(None), None);
        assert_eq!(forge_date_opt(Some("  ")), None);
        assert_eq!(
            forge_date_opt(Some("2026-10-05T10:00:00+00:00")).as_deref(),
            Some("2026-10-05T10:00:00Z")
        );
        assert_eq!(epoch_secs("1970-01-01T00:01:40+00:00"), 100);
    }

    #[test]
    fn pf_import_theme_and_locale() {
        assert_eq!(theme("dark"), ThemeChoice { id: "default", mode: Some(ThemeMode::Dark) });
        assert_eq!(theme("light"), ThemeChoice { id: "default", mode: Some(ThemeMode::Light) });
        for same in ["catppuccin", "nord", "dracula"] {
            assert_eq!(theme(same), ThemeChoice { id: same, mode: None });
        }
        assert_eq!(theme("neon-pink"), ThemeChoice { id: "default", mode: Some(ThemeMode::Dark) });
        assert_eq!(locale("en"), "en");
        assert_eq!(locale("es"), "es");
        assert_eq!(locale("fr"), "es");
        let cfg = PfConfig { theme: "nord".into(), lang: "en".into(), active_account_id: None };
        assert_eq!(config_settings(&cfg), kv(&[("locale", "en")]));
    }

    #[test]
    fn pf_import_job_states() {
        for pending in ["queued", "running", "verifying", "paused_user", "waiting_quota"] {
            assert!(job_is_pending(pending), "{pending}");
        }
        for done in ["completed", "completed_with_errors", "failed", "cancelled"] {
            assert!(!job_is_pending(done), "{done}");
        }
        assert_eq!(job_status_on_import("running"), "queued");
        assert_eq!(job_status_on_import("waiting_quota"), "waiting_quota");
    }

    #[test]
    fn pf_import_settings_map_to_forge_keys() {
        let tmp = tempfile::tempdir().unwrap();
        let pf_root = tmp.path().join("PlaylistForge");
        let outside = tmp.path().join("MyBackups");
        let pf = kv(&[
            ("ui.drop_mode", "move"),
            ("ui.drop_dup_policy", "consolidate"),
            ("ui.playlists_view", "list"),
            ("retention_keep_last", "12"),
            ("downloads.dir", "D:/Music"),
            ("downloads.thumbnail_mode", "embed"),
            ("downloads.cookies_browser", "firefox"),
            ("budget.safety_margin_percent", "5"),
            ("jobs.default_job_priority", "1"),
            ("monitor.schedule_time", "09:00"),
            ("ui.recent_drop_dests", "[]"),
            ("monitor.backups_dir", outside.to_str().unwrap()),
        ]);
        let m = settings(&pf, std::slice::from_ref(&pf_root));
        let got: std::collections::HashMap<_, _> = m.settings.iter().cloned().collect();
        assert_eq!(got["drop_mode"], "move");
        assert_eq!(got["drop_dupes"], "consolidate");
        assert_eq!(got["library_playlists_view"], "list");
        assert_eq!(got["retention_keep_last"], "12");
        assert_eq!(got["downloads.dir"], "D:/Music");
        assert_eq!(got["downloads.thumbnail_mode"], "embed");
        assert_eq!(got["budget.safety_margin_percent"], "5");
        assert_eq!(got["jobs.default_job_priority"], "1");
        assert_eq!(got["monitor.backups_dir"], outside.to_str().unwrap());
        assert!(!got.contains_key("downloads.cookies_browser"));
        let skipped: std::collections::HashMap<_, _> =
            m.skipped.iter().map(|s| (s.key.as_str(), s.reason)).collect();
        assert_eq!(skipped["downloads.cookies_browser"], SkipReason::NotCarried);
        assert_eq!(skipped["monitor.schedule_time"], SkipReason::Unknown);
        assert_eq!(skipped["ui.recent_drop_dests"], SkipReason::Unknown);
    }

    #[test]
    fn pf_import_backups_dir_inside_playlistforge_is_not_carried() {
        let tmp = tempfile::tempdir().unwrap();
        let pf_root = tmp.path().join("PlaylistForge");
        let inside = pf_root.join("backups");
        for value in [inside.to_str().unwrap(), pf_root.to_str().unwrap(), "relative/backups", ""] {
            let m =
                settings(&kv(&[("monitor.backups_dir", value)]), std::slice::from_ref(&pf_root));
            assert!(m.settings.is_empty(), "{value:?} carried");
            assert_eq!(m.skipped[0].reason, SkipReason::ForbiddenDir, "{value:?}");
        }
    }

    #[test]
    fn pf_import_settings_with_bad_values_are_skipped() {
        let pf = kv(&[
            ("ui.drop_mode", "teleport"),
            ("ui.drop_dup_policy", ""),
            ("ui.playlists_view", "cards"),
            ("retention_keep_last", "0"),
            ("budget.daily_units", "9000"),
        ]);
        let m = settings(&pf, &[]);
        assert!(m.settings.is_empty(), "{:?}", m.settings);
        let reasons: Vec<_> = m.skipped.iter().map(|s| s.reason).collect();
        assert_eq!(
            reasons,
            [
                SkipReason::BadValue,
                SkipReason::BadValue,
                SkipReason::BadValue,
                SkipReason::BadValue,
                SkipReason::Unknown
            ]
        );
    }
}
