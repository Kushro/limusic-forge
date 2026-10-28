//! Tauri commands for exporting and importing a profile (Settings ▸ Import & migrate).
//!
//! Errors reach the UI as strings: just the [`ProfileError::code`] for the ones the UI translates
//! on their own (`wrong_passphrase`, `passphrase_required`, …), `code: detail` for the ones that
//! carry something worth showing.

use std::path::PathBuf;

use tauri::AppHandle;

use super::export::{self, ExportCtx, ExportReport, ExportRequest};
use super::import::{self, ImportCtx, ImportReport, Preview};
use super::{ProfileError, RootDecision, WebStorage, EXTENSION};

fn err(e: ProfileError) -> String {
    match e {
        ProfileError::PassphraseTooShort
        | ProfileError::WrongPassphrase
        | ProfileError::CorruptSecrets
        | ProfileError::PassphraseRequired
        | ProfileError::AlreadyPending => e.code().to_string(),
        other => format!("{}: {other}", other.code()),
    }
}

fn joined<T>(r: Result<super::Result<T>, tauri::Error>) -> Result<T, String> {
    r.map_err(|e| e.to_string())?.map_err(err)
}

/// Write the profile to `dest`. `webstorage` is the webview's localStorage, which only the
/// frontend can read.
#[tauri::command]
pub async fn profile_export(
    app: AppHandle,
    dest: String,
    include_secrets: bool,
    include_caches: bool,
    passphrase: Option<String>,
    webstorage: WebStorage,
) -> Result<ExportReport, String> {
    let ctx = ExportCtx {
        data_dir: crate::paths::data_dir(&app),
        app_version: app.package_info().version.to_string(),
        token_store: crate::ytdata_secrets::token_store(&app),
    };
    let req = ExportRequest {
        dest: PathBuf::from(dest),
        include_secrets,
        include_caches,
        passphrase,
        webstorage,
    };
    joined(tauri::async_runtime::spawn_blocking(move || export::export(&ctx, req)).await)
}

/// `LiMusicForge-perfil-YYYY-MM-DD.lmfprofile`, with the local date.
#[tauri::command]
pub fn profile_default_file_name() -> String {
    format!("LiMusicForge-perfil-{}.{EXTENSION}", chrono::Local::now().format("%Y-%m-%d"))
}

/// What a profile file holds, whether the passphrase opens its secrets, and the path roots it
/// references. Nothing is applied.
#[tauri::command]
pub async fn profile_preview(
    app: AppHandle,
    path: String,
    passphrase: Option<String>,
) -> Result<Preview, String> {
    let data = crate::paths::data_dir(&app);
    joined(
        tauri::async_runtime::spawn_blocking(move || {
            import::preview(&PathBuf::from(path), passphrase.as_deref(), &data)
        })
        .await,
    )
}

/// Phase 1 of the import: stage the file, leave the marker, restart. Phase 2 runs on the next
/// launch, before anything opens the database (profile/import.rs).
#[tauri::command]
pub async fn profile_import_request(
    app: AppHandle,
    path: String,
    passphrase: Option<String>,
    decisions: Vec<RootDecision>,
) -> Result<(), String> {
    // Phase 2 resolves the directory without an AppHandle; both answers have to agree, or the
    // marker would be left where the next launch never looks.
    let data = crate::paths::data_dir(&app);
    if crate::paths::data_dir_early().as_deref() != Some(data.as_path()) {
        return Err(format!("data directory mismatch: {}", data.display()));
    }
    let ctx = ImportCtx { data_dir: data, token_store: crate::ytdata_secrets::token_store(&app) };
    joined(
        tauri::async_runtime::spawn_blocking(move || {
            import::prepare(&PathBuf::from(path), passphrase.as_deref(), &decisions, &ctx)
        })
        .await,
    )?;
    app.restart()
}

/// What the last import did, read once.
#[tauri::command]
pub fn profile_import_result(app: AppHandle) -> Option<ImportReport> {
    import::take_result(&crate::paths::data_dir(&app))
}

/// The remapped localStorage the import left, for the UI to apply on first boot. Read once.
#[tauri::command]
pub fn profile_take_webstorage(app: AppHandle) -> Option<WebStorage> {
    import::take_webstorage(&crate::paths::data_dir(&app))
}

/// Whether an import marker is waiting (including an expired one, which `profile_import_cancel`
/// clears).
#[tauri::command]
pub fn profile_import_pending(app: AppHandle) -> bool {
    import::pending(&crate::paths::data_dir(&app), crate::migrate_upstream::now_secs()).is_some()
}

/// Drop the pending import and its staging directory.
#[tauri::command]
pub fn profile_import_cancel(app: AppHandle) -> Result<(), String> {
    import::cancel(&crate::paths::data_dir(&app)).map(|_| ()).map_err(|e| e.to_string())
}
