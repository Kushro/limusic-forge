//! Where the YouTube Data API credentials live.
//!
//! - Refresh tokens: the OS keyring under [`ytdata::KEYRING_SERVICE`] (account = channel id).
//!   Portable on Windows: DPAPI-encrypted files in `<data>\secrets`, so nothing is left in the
//!   user's Credential Manager. Linux without a Secret Service gets no fallback: the keyring store
//!   fails with `AuthError::KeyringUnavailable`, which the app reports as
//!   `reason: keyring_unavailable` (D28). The token is never written in clear.
//! - `client_secret.json`: `<data>/client_secret.json`.
//! - Access tokens: memory only (`ytdata::auth::accounts::AccountManager`).
//!
//! Every path comes from [`paths::data_dir`], so portable mode keeps all of it beside the exe.

// Used by commit 26/27 (sync reader and the Data API settings tab).
#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::sync::Arc;

use tauri::AppHandle;
use ytdata::auth::store::{KeyringTokenStore, TokenStore};

use crate::paths;

/// Directory under the data dir holding the DPAPI token files (portable Windows only).
const SECRETS_DIR: &str = "secrets";
const CLIENT_SECRET_FILE: &str = "client_secret.json";

/// The refresh-token store for this run. Construction never fails: a missing keyring surfaces on
/// first use as `AuthError::KeyringUnavailable`. Its calls block; run them off the async runtime.
pub fn token_store(app: &AppHandle) -> Arc<dyn TokenStore> {
    #[cfg(windows)]
    {
        if uses_dpapi(paths::is_portable()) {
            let dir = secrets_dir_in(&paths::data_dir(app));
            return Arc::new(ytdata::auth::dpapi::DpapiFileStore::new(dir));
        }
    }
    #[cfg(not(windows))]
    let _ = app;
    Arc::new(KeyringTokenStore::new(ytdata::KEYRING_SERVICE))
}

/// Where the imported `client_secret.json` is kept.
pub fn client_secret_path(app: &AppHandle) -> PathBuf {
    client_secret_path_in(&paths::data_dir(app))
}

/// DPAPI files only for portable builds on Windows; everything else uses the OS keyring.
fn uses_dpapi(portable: bool) -> bool {
    portable && cfg!(windows)
}

fn secrets_dir_in(data_dir: &Path) -> PathBuf {
    data_dir.join(SECRETS_DIR)
}

fn client_secret_path_in(data_dir: &Path) -> PathBuf {
    data_dir.join(CLIENT_SECRET_FILE)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ytdata_secrets_dpapi_only_when_portable_on_windows() {
        assert!(!uses_dpapi(false));
        assert_eq!(uses_dpapi(true), cfg!(windows));
    }

    #[test]
    fn ytdata_secrets_paths_live_under_the_data_dir() {
        let data = Path::new("/portable/data");
        assert_eq!(secrets_dir_in(data), data.join("secrets"));
        assert_eq!(client_secret_path_in(data), data.join("client_secret.json"));
    }

    #[test]
    fn ytdata_secrets_keyring_service_is_forge_not_playlistforge() {
        assert_eq!(ytdata::KEYRING_SERVICE, "LiMusicForge");
        assert!(!KeyringTokenStore::new(ytdata::KEYRING_SERVICE).is_read_only());
        assert!(KeyringTokenStore::new(ytdata::PF_KEYRING_SERVICE).is_read_only());
    }
}
