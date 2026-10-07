//! Reading a PlaylistForge installation (its `forge.db`, `config.json`, `accounts.json`,
//! `client_secret.json`, JSON backups and keyring tokens) so Forge can take over from it.
//!
//! PlaylistForge is discontinued; Forge replaces it. This module only **reads**:
//!
//! - Nothing is ever written under PlaylistForge's folder ([`pf_default_dir`]) nor under its
//!   keyring service (`ytdata::PF_KEYRING_SERVICE`, opened read-only in [`credentials`]).
//! - The database is not opened in place: `forge.db` and its `-wal`/`-shm` are copied to
//!   `<data>/tmp/pf-import-<ts>/` and the copy is opened read-only ([`reader::stage`]). When done,
//!   only that folder is deleted, after checking it really is ours ([`reader::Staged::finish`]).
//! - PlaylistForge should be closed before copying (D35): [`pf_running`] tells the command to ask.
//!
//! [`reader`] produces the model ([`reader::PfData`]), [`mapping`] translates it to Forge's
//! vocabulary (alert kinds, settings keys, theme, dates), [`credentials`] reads refresh tokens.
//! [`apply`] writes the user's selection into Forge's database (in foreign-key order: accounts,
//! then jobs, then their items), and [`task`] removes PlaylistForge's scheduled task once the
//! user confirms (D35).

// Some of the reader's model is only read by the tests and the preview.
#![allow(dead_code)]

pub mod apply;
pub mod credentials;
pub mod mapping;
pub mod reader;
pub mod task;

use std::path::PathBuf;

/// PlaylistForge's folder name under the per-user config directory (`%APPDATA%` on Windows):
/// pf-app `config::app_data_dir`.
pub const PF_DIR_NAME: &str = "PlaylistForge";
/// Its SQLite file, inside [`PF_DIR_NAME`].
pub const PF_DB_FILE: &str = "forge.db";
/// `config.json`: theme, language, active account.
pub const PF_CONFIG_FILE: &str = "config.json";
/// `accounts.json`: non-secret Data API account metadata (pf-yt `ACCOUNTS_FILE_NAME`).
pub const PF_ACCOUNTS_FILE: &str = "accounts.json";
/// `client_secret.json`: the OAuth client PlaylistForge was set up with.
pub const PF_CLIENT_SECRET_FILE: &str = "client_secret.json";
/// Default backups folder name inside PlaylistForge's folder (when `monitor.backups_dir` is unset).
pub const PF_BACKUPS_DIR: &str = "backups";
/// The newest `forge.db` schema this importer knows (PlaylistForge migrations 0001-0005). A newer
/// one is refused (D34): its tables may mean something this code does not know.
pub const PF_SCHEMA_VERSION: i64 = 5;
/// The executable PlaylistForge ships as (`[[bin]] name = "playlist-forge"` in pf-app).
pub const PF_EXE_STEM: &str = "playlist-forge";
/// Prefix of the scratch folder under `<data>/tmp`. The only thing this module ever deletes.
pub const WORK_PREFIX: &str = "pf-import-";

/// Why an import could not read PlaylistForge's data. [`Self::code`] is the short code the
/// commands hand the UI.
#[derive(Debug, thiserror::Error)]
pub enum PfImportError {
    #[error("the per-user config directory could not be resolved")]
    NoConfigDir,
    #[error("no PlaylistForge database at {0}")]
    NotFound(PathBuf),
    #[error("this is not a PlaylistForge database")]
    NotPlaylistForge,
    #[error("PlaylistForge database version {found} is newer than this importer knows ({known})")]
    TooNew { found: i64, known: i64 },
    #[error("the PlaylistForge database copy failed its check: {0}")]
    Corrupt(String),
    #[error("PlaylistForge is running; close it first")]
    Running,
    #[error("refusing to work in {0}")]
    UnsafePath(PathBuf),
    #[error("I/O error on {path}: {message}")]
    Io { path: PathBuf, message: String },
    #[error("SQLite error: {0}")]
    Sqlite(String),
}

impl PfImportError {
    pub fn code(&self) -> &'static str {
        match self {
            PfImportError::NoConfigDir => "no_config_dir",
            PfImportError::NotFound(_) => "not_found",
            PfImportError::NotPlaylistForge => "not_playlistforge",
            PfImportError::TooNew { .. } => "too_new",
            PfImportError::Corrupt(_) => "corrupt",
            PfImportError::Running => "pf_running",
            PfImportError::UnsafePath(_) => "unsafe_path",
            PfImportError::Io { .. } => "io",
            PfImportError::Sqlite(_) => "sqlite",
        }
    }

    pub(crate) fn io(path: &std::path::Path, e: std::io::Error) -> Self {
        PfImportError::Io { path: path.to_path_buf(), message: e.to_string() }
    }
}

impl From<rusqlite::Error> for PfImportError {
    fn from(e: rusqlite::Error) -> Self {
        PfImportError::Sqlite(e.to_string())
    }
}

/// PlaylistForge's folder: `dirs::config_dir()/PlaylistForge` (`%APPDATA%\PlaylistForge`). An
/// error when the config directory cannot be resolved. Whether it exists is the caller's question.
pub fn pf_default_dir() -> Result<PathBuf, PfImportError> {
    dirs::config_dir()
        .map(|d| d.join(PF_DIR_NAME))
        .filter(|p| p.is_absolute())
        .ok_or(PfImportError::NoConfigDir)
}

/// Whether a process name is PlaylistForge's executable (`playlist-forge.exe` on Windows,
/// `playlist-forge` elsewhere), case-insensitively.
pub fn is_pf_exe(name: &str) -> bool {
    let name = name.trim().to_ascii_lowercase();
    name == PF_EXE_STEM || name.strip_suffix(".exe") == Some(PF_EXE_STEM)
}

/// Whether PlaylistForge is running (D35): the import asks to close it before copying its
/// database, so the copy is not taken mid-write. Windows walks the process list (Toolhelp32);
/// Linux reads `/proc/*/comm`; elsewhere it answers `false`.
pub fn pf_running() -> bool {
    running_impl()
}

#[cfg(windows)]
fn running_impl() -> bool {
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W,
        TH32CS_SNAPPROCESS,
    };
    // SAFETY: a process snapshot handle we own and close; `entry` is a properly sized
    // PROCESSENTRY32W (dwSize set) the API fills in place.
    unsafe {
        let Ok(snapshot) = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) else {
            return false;
        };
        let mut entry = PROCESSENTRY32W {
            dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
            ..Default::default()
        };
        let mut found = false;
        if Process32FirstW(snapshot, &mut entry).is_ok() {
            loop {
                let len =
                    entry.szExeFile.iter().position(|&c| c == 0).unwrap_or(entry.szExeFile.len());
                if is_pf_exe(&String::from_utf16_lossy(&entry.szExeFile[..len])) {
                    found = true;
                    break;
                }
                if Process32NextW(snapshot, &mut entry).is_err() {
                    break;
                }
            }
        }
        let _ = CloseHandle(snapshot);
        found
    }
}

#[cfg(target_os = "linux")]
fn running_impl() -> bool {
    let Ok(procs) = std::fs::read_dir("/proc") else { return false };
    procs.flatten().any(|p| {
        p.file_name().to_string_lossy().bytes().all(|b| b.is_ascii_digit())
            && std::fs::read_to_string(p.path().join("comm")).is_ok_and(|c| is_pf_exe(&c))
    })
}

#[cfg(not(any(windows, target_os = "linux")))]
fn running_impl() -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pf_import_default_dir_is_playlistforge_under_config() {
        // Only resolves the path; nothing is read or written there.
        if let Some(config) = dirs::config_dir() {
            assert_eq!(pf_default_dir().unwrap(), config.join("PlaylistForge"));
        }
    }

    #[test]
    fn pf_import_recognizes_the_pf_executable() {
        for yes in
            ["playlist-forge.exe", "PLAYLIST-FORGE.EXE", "playlist-forge", "playlist-forge\n"]
        {
            assert!(is_pf_exe(yes), "{yes:?}");
        }
        for no in ["playlist-forge2.exe", "limusic-forge.exe", "forge.exe", "", ".exe"] {
            assert!(!is_pf_exe(no), "{no:?}");
        }
    }

    #[test]
    fn pf_import_running_check_does_not_panic() {
        // The test runner is not PlaylistForge; whatever else runs, the walk completes.
        let _ = pf_running();
    }

    #[test]
    fn pf_import_error_codes_are_short() {
        assert_eq!(PfImportError::TooNew { found: 6, known: 5 }.code(), "too_new");
        assert_eq!(PfImportError::NotPlaylistForge.code(), "not_playlistforge");
        assert_eq!(PfImportError::Running.code(), "pf_running");
    }
}
