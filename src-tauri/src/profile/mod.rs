//! Export the whole profile to one `.lmfprofile` file and import it on another machine or OS
//! (Windows → Linux dual boot being the case it was built for).
//!
//! The file is a zip:
//!
//! ```text
//! manifest.json     Manifest: versions, source OS and data dir, counts. Never a secret.
//! profile.sqlite    VACUUM INTO snapshot, scrubbed (scrub.rs): no secret is ever left in it.
//! webstorage.json   the webview's localStorage, WEBSTORAGE_KEYS only.
//! files/…           covers/, app-icon.png
//! secrets.bin       only when the user opted in: a SecretsBundle sealed with `age` (crypto.rs).
//! ```
//!
//! - [`scrub`]: the database snapshot, what is dropped from it, the secrets it holds.
//! - [`crypto`]: sealing the [`SecretsBundle`] with a passphrase.
//! - [`credentials`]: Data API refresh tokens in and out of the token store.
//! - [`archive`]: writing and reading the zip.
//! - [`remap`]: absolute paths from the source machine, regrouped by root and rewritten.
//! - [`export`] and [`import`]: the two flows; import is two-phase like migrate_upstream.rs.
//! - [`commands`]: the Tauri commands.

use std::collections::BTreeMap;
use std::fmt;

use serde::{Deserialize, Serialize};
use zeroize::Zeroize;

pub mod archive;
pub mod commands;
pub mod credentials;
pub mod crypto;
pub mod export;
pub mod import;
pub mod remap;
pub mod scrub;

/// Bumped when a reader of an older version could no longer import the file.
pub const FORMAT_VERSION: u32 = 1;
pub const EXTENSION: &str = "lmfprofile";
pub const MANIFEST_ENTRY: &str = "manifest.json";
pub const DB_ENTRY: &str = "profile.sqlite";
pub const WEBSTORAGE_ENTRY: &str = "webstorage.json";
pub const SECRETS_ENTRY: &str = "secrets.bin";
/// Prefix of every copied file in the zip (`files/covers/…`, `files/app-icon.png`), relative to
/// the data dir it came from.
pub const FILES_PREFIX: &str = "files/";
/// Shortest passphrase accepted for `secrets.bin`, in chars.
pub const MIN_PASSPHRASE_CHARS: usize = 10;

/// The localStorage keys carried over. Everything else in the webview profile stays behind.
pub const WEBSTORAGE_KEYS: &[&str] = &[
    "limusic:personal",
    "primary-theme",
    "custom-theme",
    "appearance",
    "limusic_locale",
    "limusic:effect-presets",
    "zoom",
    "sidebar_collapsed",
    "track_columns",
    "playlist_sort",
    "lyrics_size",
    "lyrics_romanized",
    "lyrics_translated",
    "new_playlist_where",
    "playlist_suggestions_hidden",
    "lt_name",
];

/// localStorage, key → raw string value, [`WEBSTORAGE_KEYS`] only.
pub type WebStorage = BTreeMap<String, String>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SourceOs {
    Windows,
    Linux,
    Macos,
}

impl SourceOs {
    pub fn current() -> Self {
        if cfg!(windows) {
            SourceOs::Windows
        } else if cfg!(target_os = "macos") {
            SourceOs::Macos
        } else {
            SourceOs::Linux
        }
    }
}

/// `manifest.json`. Readable without the passphrase, so it holds counts, never values.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Manifest {
    pub format_version: u32,
    pub app_version: String,
    pub source_os: SourceOs,
    /// The source's data dir, as it printed there: paths under it are rewritten to the
    /// destination's data dir without asking (remap.rs).
    pub source_data_dir: String,
    /// RFC 3339, UTC.
    pub created_at: String,
    pub includes_secrets: bool,
    pub includes_caches: bool,
    pub counts: Counts,
    /// What `secrets.bin` holds, `None` when there is none.
    pub secret_counts: Option<SecretCounts>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Counts {
    pub settings: u64,
    pub local_tracks: u64,
    pub local_playlists: u64,
    pub listens: u64,
    pub accounts: u64,
    pub ytdata_accounts: u64,
    pub snapshots: u64,
    pub downloads: u64,
    /// Files under `files/`.
    pub files: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SecretCounts {
    pub google_sessions: u64,
    pub ytdata_tokens: u64,
    pub client_secret: bool,
    pub lastfm: bool,
    pub listenbrainz: bool,
    pub proxy: bool,
}

/// One cookie account (`accounts` row, keyed by its `id`).
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GoogleSession {
    pub account_id: String,
    pub session_cookie: String,
}

/// Every secret the profile has. Lives in memory only, then sealed into `secrets.bin`. `Debug`
/// shows what is present, never a value, and dropping it wipes the strings.
#[derive(Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SecretsBundle {
    /// `settings.session_cookie`: the active session.
    pub active_session_cookie: Option<String>,
    /// `accounts.session_cookie`, per account.
    pub google_sessions: Vec<GoogleSession>,
    /// Data API refresh tokens, channel id → token.
    pub ytdata_refresh_tokens: BTreeMap<String, String>,
    /// `<data>/client_secret.json`, verbatim.
    pub client_secret_json: Option<String>,
    pub lastfm_session_key: Option<String>,
    pub listenbrainz_token: Option<String>,
    /// The whole `proxy` setting, when it carries credentials (`scheme://user:pass@host`).
    pub proxy: Option<String>,
}

impl SecretsBundle {
    pub fn is_empty(&self) -> bool {
        self.counts() == SecretCounts::default()
    }

    pub fn counts(&self) -> SecretCounts {
        SecretCounts {
            google_sessions: self.google_sessions.len() as u64
                + u64::from(self.active_session_cookie.is_some()),
            ytdata_tokens: self.ytdata_refresh_tokens.len() as u64,
            client_secret: self.client_secret_json.is_some(),
            lastfm: self.lastfm_session_key.is_some(),
            listenbrainz: self.listenbrainz_token.is_some(),
            proxy: self.proxy.is_some(),
        }
    }
}

impl fmt::Debug for SecretsBundle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SecretsBundle").field("counts", &self.counts()).finish_non_exhaustive()
    }
}

impl Drop for SecretsBundle {
    fn drop(&mut self) {
        self.active_session_cookie.zeroize();
        for s in &mut self.google_sessions {
            s.session_cookie.zeroize();
        }
        for v in self.ytdata_refresh_tokens.values_mut() {
            v.zeroize();
        }
        self.client_secret_json.zeroize();
        self.lastfm_session_key.zeroize();
        self.listenbrainz_token.zeroize();
        self.proxy.zeroize();
    }
}

/// An absolute-path root found in the profile (remap.rs), for the import dialog.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PathRoot {
    /// As the source printed it (`E:\Música`).
    pub root: String,
    /// What lives under it: `local_folders`, `local_tracks`, `downloads_dir`, `downloads`,
    /// `backups_dir`, `fonts`, …
    pub kinds: Vec<String>,
    /// How many references point under it.
    pub count: u64,
    /// The proposed destination, `None` when nothing sensible was found.
    pub suggestion: Option<String>,
}

/// What the user chose for one root.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum RootAction {
    /// Rewrite the prefix to `to` (an absolute destination path).
    Map { to: String },
    /// Leave the references as they are; they show as missing until a rescan.
    Keep,
    /// Delete what points under it (local tracks, their playlist entries, queue items, …).
    Drop,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RootDecision {
    pub root: String,
    #[serde(flatten)]
    pub action: RootAction,
}

/// Why an export or import failed. [`ProfileError::code`] is what the UI translates.
#[derive(Debug, thiserror::Error)]
pub enum ProfileError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("database: {0}")]
    Db(#[from] rusqlite::Error),
    #[error("zip: {0}")]
    Zip(String),
    #[error("not a LiMusic Forge profile: {0}")]
    BadFormat(String),
    #[error("made by a newer version (format {0})")]
    TooNew(u32),
    #[error("the passphrase is shorter than {MIN_PASSPHRASE_CHARS} characters")]
    PassphraseTooShort,
    #[error("wrong passphrase")]
    WrongPassphrase,
    #[error("secrets.bin is damaged")]
    CorruptSecrets,
    #[error("the file has secrets; a passphrase is needed")]
    PassphraseRequired,
    #[error("an import is already pending")]
    AlreadyPending,
    #[error("{0}")]
    Other(String),
}

impl ProfileError {
    pub fn code(&self) -> &'static str {
        match self {
            ProfileError::Io(_) => "io",
            ProfileError::Db(_) => "db",
            ProfileError::Zip(_) => "zip",
            ProfileError::BadFormat(_) => "bad_format",
            ProfileError::TooNew(_) => "too_new",
            ProfileError::PassphraseTooShort => "passphrase_too_short",
            ProfileError::WrongPassphrase => "wrong_passphrase",
            ProfileError::CorruptSecrets => "corrupt_secrets",
            ProfileError::PassphraseRequired => "passphrase_required",
            ProfileError::AlreadyPending => "already_pending",
            ProfileError::Other(_) => "other",
        }
    }
}

pub type Result<T, E = ProfileError> = std::result::Result<T, E>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profile_secrets_debug_hides_values() {
        let mut b = SecretsBundle::default();
        b.active_session_cookie = Some("SAPISID=fake-secret".into());
        b.lastfm_session_key = Some("fake-lastfm".into());
        let shown = format!("{b:?}");
        assert!(!shown.contains("fake-secret") && !shown.contains("fake-lastfm"), "{shown}");
        assert_eq!(b.counts().google_sessions, 1);
        assert!(!b.is_empty());
        assert!(SecretsBundle::default().is_empty());
    }

    #[test]
    fn profile_root_decision_json_shape() {
        let d = RootDecision {
            root: r"E:\Música".into(),
            action: RootAction::Map { to: "/mnt/e/Música".into() },
        };
        let v = serde_json::to_value(&d).unwrap();
        assert_eq!(
            v,
            serde_json::json!({"root": r"E:\Música", "action": "map", "to": "/mnt/e/Música"})
        );
        let k: RootDecision =
            serde_json::from_value(serde_json::json!({"root": "/x", "action": "drop"})).unwrap();
        assert_eq!(k.action, RootAction::Drop);
    }
}
