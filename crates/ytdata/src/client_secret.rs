//! `client_secret.json` — the OAuth client Google generates for a "Desktop app" credential
//! (Google Cloud Console → APIs & Services → Credentials → OAuth client ID → Desktop app).
//!
//! The imported file is copied verbatim to `<app data>/client_secret.json`; the app never ships
//! or invents a client id/secret of its own.

use std::fmt;
use std::fs;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::error::{AuthError, Error};

/// Leaf file name under the app data directory (the app owns the directory itself).
pub const CLIENT_SECRET_FILE_NAME: &str = "client_secret.json";

/// The subset of Google's JSON this crate needs. Tolerant of extra fields; [`import`] persists
/// the original bytes rather than round-tripping through this struct.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClientSecretFile {
    pub installed: InstalledSecret,
}

/// `Debug` is hand-written so `client_secret` never reaches a log line.
#[derive(Clone, Serialize, Deserialize)]
pub struct InstalledSecret {
    pub client_id: String,
    pub client_secret: String,
    #[serde(default = "default_auth_uri")]
    pub auth_uri: String,
    #[serde(default = "default_token_uri")]
    pub token_uri: String,
    #[serde(default)]
    pub redirect_uris: Vec<String>,
}

impl fmt::Debug for InstalledSecret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("InstalledSecret")
            .field("client_id", &self.client_id)
            .field("client_secret", &"***")
            .field("auth_uri", &self.auth_uri)
            .field("token_uri", &self.token_uri)
            .field("redirect_uris", &self.redirect_uris)
            .finish()
    }
}

fn default_auth_uri() -> String {
    "https://accounts.google.com/o/oauth2/auth".to_string()
}

fn default_token_uri() -> String {
    "https://oauth2.googleapis.com/token".to_string()
}

impl ClientSecretFile {
    /// Parses and validates raw JSON. Fails when the top-level `"installed"` key is missing —
    /// the usual mistake is downloading a "Web application" client (`"web"`), which cannot use
    /// the loopback flow — or when `client_id`/`client_secret` are blank.
    pub fn parse(raw: &str) -> Result<Self, Error> {
        let value: serde_json::Value = serde_json::from_str(raw)?;
        if value.get("installed").is_none() {
            let hint = if value.get("web").is_some() {
                "found a \"web\" client instead — in Google Cloud Console, create an OAuth client of type \"Desktop app\", not \"Web application\""
            } else {
                "missing the \"installed\" key — make sure you downloaded the JSON for an OAuth client of type \"Desktop app\""
            };
            return Err(AuthError::InvalidClientSecret(hint.to_string()).into());
        }

        let file: ClientSecretFile = serde_json::from_value(value)?;
        if file.installed.client_id.trim().is_empty()
            || file.installed.client_secret.trim().is_empty()
        {
            return Err(AuthError::InvalidClientSecret(
                "\"installed.client_id\" or \"installed.client_secret\" is empty".to_string(),
            )
            .into());
        }
        Ok(file)
    }

    /// Loads and validates a `client_secret.json` already on disk.
    pub fn load(path: &Path) -> Result<Self, Error> {
        let raw = fs::read_to_string(path)?;
        Self::parse(&raw)
    }

    /// The client id with its middle hidden, for the UI (`1234…ent.com`). The secret is never
    /// exposed by this crate in any form.
    pub fn masked_client_id(&self) -> String {
        let id = &self.installed.client_id;
        let chars: Vec<char> = id.chars().collect();
        if chars.len() <= 10 {
            return "***".to_string();
        }
        let head: String = chars[..4].iter().collect();
        let tail: String = chars[chars.len() - 7..].iter().collect();
        format!("{head}…{tail}")
    }
}

/// Validates `source` (a file the user picked) and, if valid, copies it **verbatim** to
/// `<app_data_dir>/client_secret.json`, creating the directory if needed. Nothing is written
/// when validation fails.
pub fn import(source: &Path, app_data_dir: &Path) -> Result<ClientSecretFile, Error> {
    let raw = fs::read_to_string(source)?;
    let parsed = ClientSecretFile::parse(&raw)?;
    fs::create_dir_all(app_data_dir)?;
    fs::write(app_data_dir.join(CLIENT_SECRET_FILE_NAME), &raw)?;
    Ok(parsed)
}

/// The already-imported `client_secret.json` in `app_data_dir`, if any. Never errors: a
/// missing, corrupt or wrong-type file just means "not configured".
pub fn load_existing(app_data_dir: &Path) -> Option<ClientSecretFile> {
    ClientSecretFile::load(&app_data_dir.join(CLIENT_SECRET_FILE_NAME)).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    const VALID: &str = r#"{
        "installed": {
            "client_id": "FAKE-test-client-id.apps.googleusercontent.com",
            "project_id": "fake-test-project",
            "auth_uri": "https://accounts.google.com/o/oauth2/auth",
            "token_uri": "https://oauth2.googleapis.com/token",
            "client_secret": "FAKE-test-client-secret",
            "redirect_uris": ["http://localhost"]
        }
    }"#;

    #[test]
    fn parses_valid_installed_secret() {
        let parsed = ClientSecretFile::parse(VALID).unwrap();
        assert_eq!(parsed.installed.client_id, "FAKE-test-client-id.apps.googleusercontent.com");
        assert_eq!(parsed.installed.client_secret, "FAKE-test-client-secret");
        assert_eq!(parsed.installed.token_uri, "https://oauth2.googleapis.com/token");
    }

    #[test]
    fn defaults_auth_and_token_uri_when_absent() {
        let raw = r#"{"installed": {"client_id": "id", "client_secret": "secret"}}"#;
        let parsed = ClientSecretFile::parse(raw).unwrap();
        assert_eq!(parsed.installed.auth_uri, default_auth_uri());
        assert_eq!(parsed.installed.token_uri, default_token_uri());
    }

    #[test]
    fn rejects_malformed_json() {
        let err = ClientSecretFile::parse("{ not json").unwrap_err();
        assert!(matches!(err, Error::Serde(_)));
    }

    #[test]
    fn rejects_web_client_type_with_a_helpful_hint() {
        let raw = r#"{"web": {"client_id": "id", "client_secret": "secret"}}"#;
        match ClientSecretFile::parse(raw).unwrap_err() {
            Error::Auth(AuthError::InvalidClientSecret(msg)) => {
                assert!(msg.contains("Desktop app"), "message was: {msg}");
                assert!(msg.contains("\"web\""), "message was: {msg}");
            }
            other => panic!("expected InvalidClientSecret, got {other:?}"),
        }
    }

    #[test]
    fn rejects_json_without_installed_key() {
        let raw = r#"{"something_else": {}}"#;
        let err = ClientSecretFile::parse(raw).unwrap_err();
        assert!(matches!(err, Error::Auth(AuthError::InvalidClientSecret(_))));
    }

    #[test]
    fn rejects_empty_client_id_or_secret() {
        let raw = r#"{"installed": {"client_id": "  ", "client_secret": "secret"}}"#;
        let err = ClientSecretFile::parse(raw).unwrap_err();
        assert!(matches!(err, Error::Auth(AuthError::InvalidClientSecret(_))));
    }

    #[test]
    fn debug_never_prints_the_client_secret() {
        let parsed = ClientSecretFile::parse(VALID).unwrap();
        let debug = format!("{parsed:?}");
        assert!(!debug.contains("FAKE-test-client-secret"), "{debug}");
        assert!(debug.contains("***"), "{debug}");
    }

    #[test]
    fn masked_client_id_hides_the_middle() {
        let parsed = ClientSecretFile::parse(VALID).unwrap();
        let masked = parsed.masked_client_id();
        assert!(masked.starts_with("FAKE"), "{masked}");
        assert!(masked.ends_with("ent.com"), "{masked}");
        assert!(!masked.contains("test-client-id"), "{masked}");
    }

    #[test]
    fn import_copies_bytes_verbatim_and_load_existing_finds_it() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("downloaded.json");
        fs::write(&source, VALID).unwrap();

        let app_data_dir = dir.path().join("app_data");
        let imported = import(&source, &app_data_dir).unwrap();
        assert_eq!(imported.installed.client_id, "FAKE-test-client-id.apps.googleusercontent.com");

        let dest = app_data_dir.join(CLIENT_SECRET_FILE_NAME);
        assert_eq!(fs::read_to_string(&dest).unwrap(), VALID);

        let existing = load_existing(&app_data_dir).unwrap();
        assert_eq!(existing.installed.client_id, "FAKE-test-client-id.apps.googleusercontent.com");
    }

    #[test]
    fn load_existing_returns_none_when_absent() {
        let dir = tempfile::tempdir().unwrap();
        assert!(load_existing(dir.path()).is_none());
    }

    #[test]
    fn import_rejects_invalid_source_without_writing_dest() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("bad.json");
        fs::write(&source, "{ not json").unwrap();

        let app_data_dir = dir.path().join("app_data");
        assert!(import(&source, &app_data_dir).is_err());
        assert!(!app_data_dir.join(CLIENT_SECRET_FILE_NAME).exists());
    }
}
