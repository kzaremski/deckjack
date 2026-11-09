//! The HTTP front door.
//!
//! Only sign-in so far: there is no shell, no command bus and no apps, so a
//! signed-in browser gets a placeholder. What is real is the gate — the
//! one-time code, the per-address and global lockout, and the session cookie
//! — because that is the part worth getting right before anything sits
//! behind it.
//!
//! **Client address.** pocketdock took it from `X-Forwarded-For`, because
//! Caddy always sat in front and overwrote whatever the client sent.
//! Deckjack serves directly here, so the address comes from the socket
//! instead. That matters: it is what the lockout counts against, and a
//! header a client can set is not an identity. No proxy header is trusted,
//! which means running this behind a reverse proxy would attribute every
//! request to the proxy until that is made explicit.

use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use axum::extract::{ConnectInfo, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Form, Router};
use serde::Deserialize;

use crate::auth::state::{AuthState, LoginOutcome, Now, Refusal};
use crate::auth::{SessionId, html, session};
use crate::store::Store;

/// How long a wrong code is made to wait. Long enough to make guessing over a
/// network pointless, short enough not to be noticed when you mistype once.
pub const FAILURE_DELAY: Duration = Duration::from_millis(750);

pub struct Ctx {
    state: Mutex<AuthState>,
    /// `rusqlite::Connection` is `Send` but not `Sync`, so the store is
    /// behind its own lock. Held only across a single statement, never an
    /// await, so it never blocks the runtime.
    store: Mutex<Store>,
    failure_delay: Duration,
    trusted_proxies: Vec<IpAddr>,
    /// Run when a browser session genuinely ends — signed out, expired,
    /// revoked — so whatever it owns is reaped with it.
    ///
    /// Closures rather than a trait object over a concrete type: a pty is
    /// the first thing that outlives its session, it will not be the last,
    /// and `http` should not learn about `terminal` to say so. Each returns
    /// how many things it closed, for the log.
    ///
    /// NOT run when a socket closes. A reload drops every connection and
    /// must not kill the shell — that is the whole point of reattach.
    reapers: Vec<Reaper>,
}

/// Cleans up after a session that has ended; returns how many things it
/// closed, for the log.
type Reaper = Box<dyn Fn(&SessionId) -> usize + Send + Sync>;

impl Ctx {
    pub fn new(
        state: AuthState,
        store: Store,
        failure_delay: Duration,
        trusted_proxies: Vec<IpAddr>,
    ) -> Ctx {
        Ctx {
            state: Mutex::new(state),
            store: Mutex::new(store),
            failure_delay,
            trusted_proxies,
            reapers: Vec::new(),
        }
    }

    /// Register something to clean up after a session that has ended.
    pub fn on_session_end(
        mut self,
        reap: impl Fn(&SessionId) -> usize + Send + Sync + 'static,
    ) -> Ctx {
        self.reapers.push(Box::new(reap));
        self
    }

    fn session_ended(&self, id: &SessionId) {
        let closed: usize = self.reapers.iter().map(|reap| reap(id)).sum();
        if closed > 0 {
            tracing::info!(closed, "reaped what the session owned");
        }
    }

    /// Persist a session so it survives a restart. A failure here is logged
    /// and swallowed: the sign-in itself succeeded, and refusing it because
    /// the database is unhappy would be a worse outcome than a session that
    /// does not outlive the process.
    fn remember(&self, id: &SessionId, session: &crate::auth::session::Session) {
        if let Err(e) = self
            .store
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert_session(id, session)
        {
            tracing::warn!(error = %e, "cannot persist session");
        }
    }

    fn forget(&self, id: &SessionId) {
        if let Err(e) = self
            .store
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .delete_session(id)
        {
            tracing::warn!(error = %e, "cannot remove persisted session");
        }
    }

    /// Who a request is from.
    ///
    /// The socket, unless it arrived from a proxy we were told to believe.
    /// `X-Forwarded-For` is `client, proxy1, proxy2`: each hop appends the
    /// peer it saw, so only the **rightmost** entry was written by the
    /// trusted hop. Everything to its left could have been sent by the
    /// client, and taking the leftmost — the usual mistake — would let anyone
    /// choose their own identity and so dodge the per-address lockout.
    pub fn client_ip(&self, peer: SocketAddr, headers: &HeaderMap) -> IpAddr {
        if !self.trusts(peer.ip()) {
            return peer.ip();
        }
        headers
            .get_all("x-forwarded-for")
            .iter()
            .filter_map(|v| v.to_str().ok())
            .flat_map(|v| v.split(','))
            .filter_map(|v| v.trim().parse::<IpAddr>().ok())
            .next_back()
            .unwrap_or_else(|| peer.ip())
    }

    /// Whether the browser reached us over HTTPS, which decides if the
    /// session cookie may be marked `Secure`. Only a trusted proxy gets to
    /// say so; otherwise it is whatever we are actually serving, which here
    /// is plain HTTP.
    pub fn is_https(&self, peer: SocketAddr, headers: &HeaderMap) -> bool {
        if !self.trusts(peer.ip()) {
            return false;
        }
        headers
            .get("x-forwarded-proto")
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.eq_ignore_ascii_case("https"))
    }

    fn trusts(&self, ip: IpAddr) -> bool {
        self.trusted_proxies.contains(&ip)
    }

    /// Transitions are small and leave the state consistent even if a holder
    /// panicked, so a poisoned lock is still usable.
    pub fn state(&self) -> MutexGuard<'_, AuthState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

pub fn router(ctx: Arc<Ctx>) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/login", get(login_page).post(login_submit))
        .route("/logout", post(logout))
        .fallback(shell_asset)
        .with_state(ctx)
}

fn page(status: StatusCode, body: String) -> Response {
    (
        status,
        [
            (header::CACHE_CONTROL, "no-store"),
            (header::CONTENT_SECURITY_POLICY, html::CSP),
        ],
        Html(body),
    )
        .into_response()
}

/// A present `Origin` must name the same host as `Host`, which is what stops
/// another page in the browser posting the form on your behalf.
fn origin_ok(headers: &HeaderMap) -> bool {
    let Some(origin) = headers.get(header::ORIGIN) else {
        return true;
    };
    let (Ok(origin), Some(Ok(host))) = (
        origin.to_str(),
        headers.get(header::HOST).map(|h| h.to_str()),
    ) else {
        return false;
    };
    let authority = origin
        .strip_prefix("https://")
        .or_else(|| origin.strip_prefix("http://"));
    authority.is_some_and(|a| a.eq_ignore_ascii_case(host))
}

fn cookie_session(headers: &HeaderMap) -> Option<SessionId> {
    session::from_cookie_headers(
        headers
            .get_all(header::COOKIE)
            .iter()
            .filter_map(|v| v.to_str().ok()),
    )
}

fn redirect_to_login() -> Response {
    (
        StatusCode::SEE_OTHER,
        [
            (header::LOCATION, "/login"),
            (header::CACHE_CONTROL, "no-store"),
        ],
    )
        .into_response()
}

async fn index(
    State(ctx): State<Arc<Ctx>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
) -> Response {
    let ip = ctx.client_ip(peer, &headers);
    let id = cookie_session(&headers);
    let signed_in = {
        let mut st = ctx.state();
        matches!(
            st.verify(ip, id.as_ref(), Now::current()),
            crate::auth::state::VerifyOutcome::Ok
        )
    };
    if !signed_in {
        return redirect_to_login();
    }
    crate::shell::index()
}

/// Everything else behind the gate: embedded assets, and client-side routes
/// that resolve to the shell document. Unauthenticated requests are sent to
/// sign in rather than served, so the build is not readable without a session.
async fn shell_asset(
    State(ctx): State<Arc<Ctx>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    uri: axum::http::Uri,
) -> Response {
    let ip = ctx.client_ip(peer, &headers);
    let id = cookie_session(&headers);
    let signed_in = {
        let mut st = ctx.state();
        matches!(
            st.verify(ip, id.as_ref(), Now::current()),
            crate::auth::state::VerifyOutcome::Ok
        )
    };
    if !signed_in {
        return redirect_to_login();
    }
    crate::shell::serve(uri.path())
}

async fn login_page(
    State(ctx): State<Arc<Ctx>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
) -> Response {
    if ctx.state().refusal(ctx.client_ip(peer, &headers)).is_some() {
        return page(StatusCode::FORBIDDEN, html::locked());
    }
    page(StatusCode::OK, html::login(None))
}

#[derive(Deserialize)]
struct LoginForm {
    #[serde(default)]
    code: String,
}

async fn login_submit(
    State(ctx): State<Arc<Ctx>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Form(form): Form<LoginForm>,
) -> Response {
    let ip = ctx.client_ip(peer, &headers);

    if !origin_ok(&headers) {
        tracing::warn!(%ip, "login rejected: Origin does not match Host");
        return (StatusCode::FORBIDDEN, "cross-origin request refused\n").into_response();
    }

    let new_session = match SessionId::generate() {
        Ok(id) => id,
        Err(e) => {
            tracing::error!(error = %e, "cannot create session id");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };

    let mut events = Vec::new();
    let outcome = {
        let mut st = ctx.state();
        st.login(
            ip,
            &form.code,
            new_session.clone(),
            Now::current(),
            &mut events,
        )
    };
    for e in events {
        tracing::info!(event = ?e, "auth");
    }

    match outcome {
        LoginOutcome::Success { revoked } => {
            tracing::info!(%ip, revoked, "login ok");
            // Read back what `login` actually stored rather than rebuilding
            // it here, so the persisted row cannot drift from the live one.
            if let Some(s) = ctx.state().session(&new_session) {
                ctx.remember(&new_session, &s);
            }
            (
                StatusCode::SEE_OTHER,
                [
                    (header::LOCATION, "/".to_string()),
                    (
                        header::SET_COOKIE,
                        session::set_cookie(&new_session, ctx.is_https(peer, &headers)),
                    ),
                    (header::CACHE_CONTROL, "no-store".to_string()),
                ],
            )
                .into_response()
        }
        LoginOutcome::Refused(r) => {
            let why = match r {
                Refusal::IpBlocked => "ip blocked",
                Refusal::GlobalLock => "global lock",
            };
            tracing::info!(%ip, reason = why, "login refused");
            page(StatusCode::FORBIDDEN, html::locked())
        }
        LoginOutcome::Failed { left } => {
            tracing::info!(%ip, left, "login failed ({left} left)");
            tokio::time::sleep(ctx.failure_delay).await;
            page(StatusCode::UNAUTHORIZED, html::wrong_code(left))
        }
        LoginOutcome::Locked {
            ip_blocked,
            global_lock,
        } => {
            tracing::info!(%ip, "login failed (0 left)");
            if ip_blocked {
                tracing::warn!(%ip, "address blocked");
            }
            if global_lock {
                tracing::warn!("global lock: all sign-ins refused until reset");
            }
            tokio::time::sleep(ctx.failure_delay).await;
            page(StatusCode::FORBIDDEN, html::locked())
        }
    }
}

async fn logout(
    State(ctx): State<Arc<Ctx>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
) -> Response {
    if !origin_ok(&headers) {
        return (StatusCode::FORBIDDEN, "cross-origin request refused\n").into_response();
    }
    if let Some(id) = cookie_session(&headers)
        && ctx.state().logout(&id)
    {
        ctx.forget(&id);
        ctx.session_ended(&id);
    }
    (
        StatusCode::SEE_OTHER,
        [
            (header::LOCATION, "/login".to_string()),
            (
                header::SET_COOKIE,
                session::clear_cookie(ctx.is_https(peer, &headers)),
            ),
            (header::CACHE_CONTROL, "no-store".to_string()),
        ],
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::Code;
    use crate::auth::state::Limits;

    fn ctx(trusted: &[&str]) -> Ctx {
        let limits = Limits {
            per_ip_attempts: 3,
            global_attempts: 10,
            session_max: Duration::from_secs(3600),
            max_sessions: 4,
        };
        Ctx::new(
            AuthState::new(limits, Code::generate().unwrap(), Now::current()),
            Store::in_memory().unwrap(),
            Duration::from_millis(0),
            trusted.iter().map(|s| s.parse().unwrap()).collect(),
        )
    }

    fn peer(ip: &str) -> SocketAddr {
        SocketAddr::new(ip.parse().unwrap(), 40000)
    }

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.append(
                axum::http::HeaderName::from_bytes(k.as_bytes()).unwrap(),
                v.parse().unwrap(),
            );
        }
        h
    }

    #[test]
    fn an_untrusted_peer_cannot_claim_an_address() {
        // The whole point: without this, anyone could set the header and so
        // never be the address the lockout counts against.
        let c = ctx(&[]);
        let h = headers(&[("x-forwarded-for", "9.9.9.9")]);
        assert_eq!(
            c.client_ip(peer("203.0.113.7"), &h),
            "203.0.113.7".parse::<IpAddr>().unwrap()
        );
    }

    #[test]
    fn a_trusted_proxy_is_believed() {
        let c = ctx(&["127.0.0.1"]);
        let h = headers(&[("x-forwarded-for", "100.69.236.100")]);
        assert_eq!(
            c.client_ip(peer("127.0.0.1"), &h),
            "100.69.236.100".parse::<IpAddr>().unwrap()
        );
    }

    #[test]
    fn the_rightmost_entry_wins_not_the_leftmost() {
        // "client, proxy1" — a client that sends its own X-Forwarded-For has
        // its value pushed left when the trusted hop appends what it saw.
        // Taking the leftmost would take the forged one.
        let c = ctx(&["127.0.0.1"]);
        let h = headers(&[("x-forwarded-for", "9.9.9.9, 100.69.236.100")]);
        assert_eq!(
            c.client_ip(peer("127.0.0.1"), &h),
            "100.69.236.100".parse::<IpAddr>().unwrap()
        );
    }

    #[test]
    fn rightmost_across_several_header_lines() {
        let c = ctx(&["127.0.0.1"]);
        let h = headers(&[
            ("x-forwarded-for", "9.9.9.9"),
            ("x-forwarded-for", "100.69.236.100"),
        ]);
        assert_eq!(
            c.client_ip(peer("127.0.0.1"), &h),
            "100.69.236.100".parse::<IpAddr>().unwrap()
        );
    }

    #[test]
    fn a_trusted_proxy_that_sends_nothing_falls_back_to_the_socket() {
        let c = ctx(&["127.0.0.1"]);
        assert_eq!(
            c.client_ip(peer("127.0.0.1"), &HeaderMap::new()),
            "127.0.0.1".parse::<IpAddr>().unwrap()
        );
    }

    #[test]
    fn garbage_in_the_header_falls_back_rather_than_failing_open() {
        let c = ctx(&["127.0.0.1"]);
        let h = headers(&[("x-forwarded-for", "not-an-address")]);
        assert_eq!(
            c.client_ip(peer("127.0.0.1"), &h),
            "127.0.0.1".parse::<IpAddr>().unwrap()
        );
    }

    #[test]
    fn scheme_is_only_taken_from_a_trusted_proxy() {
        let h = headers(&[("x-forwarded-proto", "https")]);

        // Untrusted: we serve plain HTTP, so the cookie must not be Secure or
        // the browser drops it and sign-in silently never sticks.
        assert!(!ctx(&[]).is_https(peer("203.0.113.7"), &h));

        // Trusted: Tailscale Serve really did terminate TLS.
        assert!(ctx(&["127.0.0.1"]).is_https(peer("127.0.0.1"), &h));
    }

    #[test]
    fn origin_must_match_host_when_present() {
        assert!(origin_ok(&HeaderMap::new()));
        assert!(origin_ok(&headers(&[
            ("origin", "https://pocket.example.ts.net"),
            ("host", "pocket.example.ts.net"),
        ])));
        assert!(!origin_ok(&headers(&[
            ("origin", "https://evil.example"),
            ("host", "pocket.example.ts.net"),
        ])));
    }
}
