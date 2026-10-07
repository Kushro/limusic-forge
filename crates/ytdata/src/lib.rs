//! `ytdata` — thin YouTube Data API v3 client + OAuth for LiMusic Forge, ported from
//! PlaylistForge's `pf-yt`.
//!
//! Hand-rolled on `reqwest` + `serde` instead of the generated `google-youtube3` crate: only a
//! handful of endpoints are needed, and a thin client gives full control over `part`/`fields`,
//! per-reason error handling (`quotaExceeded`, `manualSortRequired`, `accessNotConfigured`,
//! `invalid_grant`...), retries and quota accounting.
//!
//! This crate knows nothing about the app's database or Tauri: quota spend leaves through the
//! [`quota::QuotaSink`] trait, refresh tokens through [`auth::store::TokenStore`] and account
//! metadata through [`auth::accounts::AccountsRepo`]. The app wires the real implementations.

/// HTTP client for the Data API endpoints (read + write), with Google's error envelope mapped
/// to [`error::Error::Api`].
pub mod client;

/// `client_secret.json` parsing, validation and import ("Desktop app" OAuth clients only).
pub mod client_secret;

/// OAuth: installed-app flow (loopback + PKCE), pluggable refresh-token storage and the
/// multi-account manager.
pub mod auth;

/// Typed errors, including the [`error::ApiErrorKind`] classification callers branch on.
pub mod error;

/// Typed request/response shapes; callers only see the public `*Summary` types.
pub mod types;

/// [`quota::QuotaSink`] and the per-endpoint unit cost table (read 1, write 50).
pub mod quota;

/// Keyring service under which LiMusic Forge stores refresh tokens (account = `channel_id`).
pub const KEYRING_SERVICE: &str = "LiMusicForge";

/// Keyring service PlaylistForge used. Only ever read (the PlaylistForge importer copies its
/// tokens over); this crate never writes under it.
pub const PF_KEYRING_SERVICE: &str = "PlaylistForge";
