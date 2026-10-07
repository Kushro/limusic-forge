//! Loopback HTTP server for the OAuth installed-app redirect (`http://127.0.0.1:<ephemeral>`),
//! alive only for one interactive authorization.
//!
//! Bound to `127.0.0.1:0` only — never `0.0.0.0`, never `localhost` (which could resolve to a
//! non-loopback or IPv6 address). `tiny_http` has no async API, so the poll loop runs on a
//! blocking thread and uses `recv_timeout` in short slices to notice cancellation and the
//! deadline promptly. Requests that carry neither `code` nor `error` (a browser's
//! `/favicon.ico`, a stray probe) get a 404 and the server keeps waiting.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::error::{AuthError, Error};

/// The address the loopback server binds. Fixed: loopback IPv4, OS-assigned port.
pub const LOOPBACK_BIND: &str = "127.0.0.1:0";

/// Outcome of the redirect the loopback server waits for.
pub enum CallbackOutcome {
    /// `?code=...&state=...` — authorization granted.
    Code { code: String, state: Option<String> },
    /// `?error=...` — e.g. `access_denied` when the user cancels on Google's consent screen.
    ProviderError(String),
}

/// A bound-but-not-yet-serving loopback listener.
pub struct LoopbackServer {
    server: tiny_http::Server,
    addr: SocketAddr,
}

impl LoopbackServer {
    /// Binds an ephemeral port on `127.0.0.1`.
    pub fn start() -> Result<Self, Error> {
        let server = tiny_http::Server::http(LOOPBACK_BIND)
            .map_err(|err| AuthError::Loopback(err.to_string()))?;
        let addr = server
            .server_addr()
            .to_ip()
            .ok_or_else(|| AuthError::Loopback("loopback server has no IP address".to_string()))?;
        if !addr.ip().is_loopback() {
            return Err(AuthError::Loopback(format!("refusing non-loopback bind {addr}")).into());
        }
        Ok(Self { server, addr })
    }

    /// `http://127.0.0.1:<port>` — the `redirect_uri` handed to Google.
    pub fn redirect_uri(&self) -> String {
        format!("http://127.0.0.1:{}", self.addr.port())
    }

    pub fn port(&self) -> u16 {
        self.addr.port()
    }

    pub fn local_addr(&self) -> SocketAddr {
        self.addr
    }

    /// Waits (on a blocking thread) for a redirect with `code` or `error`, until `cancel` is
    /// set or `timeout` elapses.
    pub async fn wait_for_callback(
        self,
        cancel: Arc<AtomicBool>,
        timeout: Duration,
    ) -> Result<CallbackOutcome, Error> {
        let deadline = Instant::now() + timeout;
        match tokio::task::spawn_blocking(move || Self::poll_loop(&self.server, &cancel, deadline))
            .await
        {
            Ok(result) => result,
            Err(join_err) => {
                Err(AuthError::Loopback(format!("loopback server task panicked: {join_err}"))
                    .into())
            }
        }
    }

    fn poll_loop(
        server: &tiny_http::Server,
        cancel: &AtomicBool,
        deadline: Instant,
    ) -> Result<CallbackOutcome, Error> {
        loop {
            if cancel.load(Ordering::SeqCst) {
                return Err(AuthError::Cancelled.into());
            }
            let now = Instant::now();
            if now >= deadline {
                return Err(AuthError::TimedOut.into());
            }
            let poll = (deadline - now).min(Duration::from_millis(400));
            match server.recv_timeout(poll) {
                Ok(Some(request)) => {
                    let outcome = parse_callback(request.url());
                    let (status, body) = match &outcome {
                        Some(CallbackOutcome::Code { .. }) => (200u16, SUCCESS_HTML),
                        Some(CallbackOutcome::ProviderError(_)) => (200u16, ERROR_HTML),
                        None => (404u16, NOT_FOUND_HTML),
                    };
                    let response = tiny_http::Response::from_string(body)
                        .with_status_code(status)
                        .with_header(
                            "Content-Type: text/html; charset=utf-8"
                                .parse::<tiny_http::Header>()
                                .expect("static header is well-formed"),
                        );
                    // Best-effort: a tab closed early must not fail the authorization.
                    let _ = request.respond(response);
                    if let Some(outcome) = outcome {
                        return Ok(outcome);
                    }
                }
                Ok(None) => continue,
                Err(err) => return Err(AuthError::Loopback(err.to_string()).into()),
            }
        }
    }
}

/// `raw_url` is the request's path + query (`/?code=...&state=...`). `None` when it carries
/// neither `code` nor `error` (or does not parse at all).
pub(crate) fn parse_callback(raw_url: &str) -> Option<CallbackOutcome> {
    let parsed = url::Url::parse(&format!("http://127.0.0.1{raw_url}")).ok()?;

    let mut code = None;
    let mut state = None;
    let mut provider_error = None;
    for (key, value) in parsed.query_pairs() {
        match key.as_ref() {
            "code" => code = Some(value.into_owned()),
            "state" => state = Some(value.into_owned()),
            "error" => provider_error = Some(value.into_owned()),
            _ => {}
        }
    }

    if let Some(code) = code {
        return Some(CallbackOutcome::Code { code, state });
    }
    provider_error.map(CallbackOutcome::ProviderError)
}

const SUCCESS_HTML: &str = "<!doctype html><html><head><meta charset=\"utf-8\"><title>LiMusic Forge</title></head><body style=\"font-family:sans-serif;text-align:center;padding:4rem;\"><h1>LiMusic Forge</h1><p>Authorization complete. You can close this window.</p><p>Autorizaci&oacute;n completada. Pod&eacute;s cerrar esta ventana.</p></body></html>";
const ERROR_HTML: &str = "<!doctype html><html><head><meta charset=\"utf-8\"><title>LiMusic Forge</title></head><body style=\"font-family:sans-serif;text-align:center;padding:4rem;\"><h1>LiMusic Forge</h1><p>Authorization could not be completed. Return to LiMusic Forge and try again.</p><p>La autorizaci&oacute;n no se pudo completar. Volv&eacute; a LiMusic Forge e intent&aacute; de nuevo.</p></body></html>";
const NOT_FOUND_HTML: &str = "<!doctype html><html><body>Not found</body></html>";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_callback_extracts_code_and_state() {
        match parse_callback("/?state=xyz&code=abc123") {
            Some(CallbackOutcome::Code { code, state }) => {
                assert_eq!(code, "abc123");
                assert_eq!(state.as_deref(), Some("xyz"));
            }
            _ => panic!("expected Code outcome"),
        }
    }

    #[test]
    fn parse_callback_extracts_provider_error() {
        match parse_callback("/?error=access_denied") {
            Some(CallbackOutcome::ProviderError(err)) => assert_eq!(err, "access_denied"),
            _ => panic!("expected ProviderError outcome"),
        }
    }

    #[test]
    fn parse_callback_ignores_requests_without_code_or_error() {
        assert!(parse_callback("/?foo=bar").is_none());
        assert!(parse_callback("/favicon.ico").is_none());
    }

    #[test]
    fn binds_loopback_only() {
        let server = LoopbackServer::start().unwrap();
        assert!(server.local_addr().ip().is_loopback());
        assert_eq!(server.local_addr().ip().to_string(), "127.0.0.1");
        assert_ne!(server.port(), 0);
        assert_eq!(server.redirect_uri(), format!("http://127.0.0.1:{}", server.port()));
    }

    #[tokio::test]
    async fn loopback_server_round_trips_a_real_http_request() {
        let server = LoopbackServer::start().unwrap();
        let redirect_uri = server.redirect_uri();

        let cancel = Arc::new(AtomicBool::new(false));
        let waiting = tokio::spawn(server.wait_for_callback(cancel, Duration::from_secs(10)));

        tokio::time::sleep(Duration::from_millis(100)).await;
        let http = reqwest::Client::new();
        // A favicon probe first: answered 404, the server keeps waiting.
        let probe = http.get(format!("{redirect_uri}/favicon.ico")).send().await.unwrap();
        assert_eq!(probe.status().as_u16(), 404);
        let resp = http
            .get(format!("{redirect_uri}/?code=test-auth-code&state=test-state"))
            .send()
            .await
            .unwrap();
        assert!(resp.status().is_success());

        match waiting.await.unwrap().unwrap() {
            CallbackOutcome::Code { code, state } => {
                assert_eq!(code, "test-auth-code");
                assert_eq!(state.as_deref(), Some("test-state"));
            }
            _ => panic!("expected Code outcome"),
        }
    }

    #[tokio::test]
    async fn loopback_server_honors_cancellation() {
        let server = LoopbackServer::start().unwrap();
        let cancel = Arc::new(AtomicBool::new(false));
        let cancel_clone = cancel.clone();

        let waiting = tokio::spawn(server.wait_for_callback(cancel, Duration::from_secs(30)));
        tokio::time::sleep(Duration::from_millis(50)).await;
        cancel_clone.store(true, Ordering::SeqCst);

        let result = waiting.await.unwrap();
        assert!(matches!(result, Err(Error::Auth(AuthError::Cancelled))));
    }

    #[tokio::test]
    async fn loopback_server_times_out() {
        let server = LoopbackServer::start().unwrap();
        let cancel = Arc::new(AtomicBool::new(false));
        let result = server.wait_for_callback(cancel, Duration::from_millis(200)).await;
        assert!(matches!(result, Err(Error::Auth(AuthError::TimedOut))));
    }
}
