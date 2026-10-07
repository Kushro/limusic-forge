//! OAuth: installed-app flow (loopback redirect + PKCE), pluggable refresh-token storage and
//! multi-account management with automatic refresh and `invalid_grant` detection.
//!
//! - [`flow`]: PKCE + loopback + code exchange / refresh / revoke against Google's endpoints.
//! - [`loopback`]: the `tiny_http` server on `127.0.0.1:0` that receives the browser redirect.
//! - [`store`]: the [`store::TokenStore`] trait (refresh tokens only; access tokens never touch
//!   disk) and [`store::InMemoryTokenStore`]. The keyring/DPAPI stores implement the same trait.
//! - [`accounts`]: [`accounts::AccountManager`], the multi-account façade the app holds.

pub mod accounts;
pub mod flow;
pub mod loopback;
pub mod store;
