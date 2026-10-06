//! The fork's identity in one place, so the tray, notifications, OS media controls and window
//! titles can't drift apart. Keep in sync with `productName`/`mainBinaryName` in
//! `tauri.conf.json` (the `brand_identity_is_forge` test in lib.rs checks the config side).
//!
//! Deliberately NOT renamed here: the account/identity salts, `LIMUSIC_*` env vars, URL schemes
//! and storage keys keep their upstream names so existing data and tooling keep working.

/// Human-facing name, shown in the tray, toasts and OS media overlays.
pub const APP_NAME: &str = "LiMusic Forge";

/// Machine-facing short name: the binary (`mainBinaryName`), and the id the desktop sees for us
/// (tray item id, MPRIS bus suffix).
pub const APP_SLUG: &str = "limusic-forge";

/// The fork's GitHub repository, `owner/name`: releases, the updater feed and issue reports.
pub const REPO_SLUG: &str = "Kushro/limusic-forge";

/// The fork's GitHub repository page.
pub const REPO_URL: &str = "https://github.com/Kushro/limusic-forge";
