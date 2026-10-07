//! OAuth: installed-app flow (loopback redirect + PKCE), pluggable refresh-token storage and
//! multi-account management with automatic refresh and `invalid_grant` detection.
//!
//! - [`flow`]: PKCE + loopback + code exchange / refresh / revoke against Google's endpoints.
//! - [`loopback`]: the `tiny_http` server on `127.0.0.1:0` that receives the browser redirect.
//! - [`store`]: the [`store::TokenStore`] trait (refresh tokens only; access tokens never touch
//!   disk), [`store::KeyringTokenStore`] (OS keyring) and [`store::InMemoryTokenStore`].
//! - `dpapi` (Windows only): `dpapi::DpapiFileStore`, DPAPI-encrypted files for portable builds.
//! - [`accounts`]: [`accounts::AccountManager`], the multi-account façade the app holds.

pub mod accounts;
#[cfg(windows)]
pub mod dpapi;
pub mod flow;
pub mod loopback;
pub mod store;
