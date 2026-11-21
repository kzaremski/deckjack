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
//! header a client can set is not an identity. Proxy headers are believed
//! from exactly two places: a request that arrived on the proxy's Unix
//! socket ([`crate::proxy`], the one `tailscale serve unix:/...` talks to),
//! and a TCP peer listed in `trusted_proxies`.

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
use crate::auth::{RevokeReason, SessionHash, SessionId, html, session};
use crate::store::Store;

/// How long a wrong code is made to wait. Long enough to make guessing over a
/// network pointless, short enough not to be noticed when you mistype once.
pub const FAILURE_DELAY: Duration = Duration::from_millis(750);

pub struct Ctx {
    /// Shared rather than owned, because lockdown's step 1 — revoke every
    /// session — has to reach the same state machine without reaching
    /// through the HTTP context to do it. See `wiring::AuthSessions`.
    state: Arc<Mutex<AuthState>>,
    /// `rusqlite::Connection` is `Send` but not `Sync`, so the store is
    /// behind its own lock. Held only across a single statement, never an
    /// await, so it never blocks the runtime.
    store: Arc<Mutex<Store>>,
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
    /// Run after a sign-in succeeds, with the address it came from.
    ///
    /// A closure for the same reason the reapers are: this is how lockdown's
    /// "a sign-in from a device that is not known" trigger is fed, and `http`
    /// has no business knowing the kill switch exists.
    sign_in_watchers: Vec<SignInWatch>,
    /// Where the audit row for a revoked session goes. Built here from
    /// `store`, not injected, and that is the point: see
    /// [`Ctx::record_revocation`].
    audit: Arc<crate::audit::StoreSink>,
    /// This node's id, for the audit row's `target_node`. Required by
    /// [`Ctx::new`] rather than registered afterwards, because an audit row
    /// that the product does not write and the test harness does is worse
    /// than no audit row at all.
    node_id: String,
}

/// Cleans up after a session that has ended; returns how many things it
/// closed, for the log.
type Reaper = Box<dyn Fn(&SessionHash) -> usize + Send + Sync>;

type SignInWatch = Box<dyn Fn(IpAddr) + Send + Sync>;

impl Ctx {
    /// `node_id` is a required argument and not a `with_node_id` builder,
    /// for the reason `wiring::BusParts` requires every field: a `mut self`
    /// builder has no getter, so a forgotten call is neither a compile error
    /// nor observable to any test, and this repo has already shipped a bus
    /// with no grant storage that way.
    pub fn new(
        state: Arc<Mutex<AuthState>>,
        store: Arc<Mutex<Store>>,
        failure_delay: Duration,
        trusted_proxies: Vec<IpAddr>,
        node_id: String,
    ) -> Ctx {
        Ctx {
            audit: crate::audit::StoreSink::new(Arc::clone(&store)),
            state,
            store,
            failure_delay,
            trusted_proxies,
            reapers: Vec::new(),
            sign_in_watchers: Vec::new(),
            node_id,
        }
    }

    /// Register something to clean up after a session that has ended.
    pub fn on_session_end(
        mut self,
        reap: impl Fn(&SessionHash) -> usize + Send + Sync + 'static,
    ) -> Ctx {
        self.reapers.push(Box::new(reap));
        self
    }

    /// Register something to run when a sign-in succeeds.
    pub fn on_sign_in(mut self, watch: impl Fn(IpAddr) + Send + Sync + 'static) -> Ctx {
        self.sign_in_watchers.push(Box::new(watch));
        self
    }

    /// Fold the WAL back into the main file. Called on graceful shutdown,
    /// before the lock is released.
    pub fn checkpoint(&self) -> Result<(), crate::store::StoreError> {
        self.store
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .checkpoint()
    }

    fn session_ended(&self, hash: &SessionHash) {
        let closed: usize = self.reapers.iter().map(|reap| reap(hash)).sum();
        if closed > 0 {
            tracing::info!(closed, "reaped what the session owned");
        }
    }

    /// Act on what a transition reported, then log it.
    ///
    /// The only interesting case is a revocation nobody asked for: the
    /// stalest session dropped for `max_sessions`, or every session belonging to an
    /// address that has just been blocked. Signing out already deletes the
    /// row and reaps the shells; these paths have to do the same, or a
    /// revoked session comes back at the next restart with its pty still
    /// running in the meantime.
    pub(crate) fn apply(&self, events: Vec<crate::auth::Event>) {
        for e in events {
            if let crate::auth::Event::SessionRevoked { hash, ip, reason } = e {
                self.forget(&hash);
                self.session_ended(&hash);
                // Written down after the row is gone and the ptys are dead,
                // so the log never claims a revocation that did not finish.
                self.record_revocation(&hash, ip, reason);
            }
            tracing::info!(event = ?e, "auth");
        }
    }

    /// The audit row for a session that was taken away: dropped to honor
    /// `max_sessions`, or taken with an address that was just blocked.
    ///
    /// Written here, from the store this context already holds, rather than
    /// by a closure `main.rs` registers. A registration is a line that can
    /// be left out of `main.rs` and put into `tests/end_to_end.rs`'s
    /// hand-copied builder chain, which would leave every test green and the
    /// row missing at runtime — the failure this codebase keeps hitting.
    /// There is nothing to register, so there is nothing to forget.
    ///
    /// NOT every revocation. `AuthState::kick` — lockdown's step 1 — drains
    /// its sessions and pushes no `Event`, so a lockdown writes no rows
    /// here; what it writes is its own engagement, in lockdown's journal.
    /// The two paths covered here are the two that take a session away while
    /// the node carries on as normal, which are the ones nothing else
    /// records.
    ///
    /// [`crate::events::Issuer::System`], because nobody asked for it — the
    /// node did it to make room. The victim is named by its session tag, the
    /// same 16 hex characters `command::SessionTag` uses, so an audit row
    /// joins to the `sessions` row it is about; the full hash is not in the
    /// row and the cookie is nowhere near it. The reason goes in `params`
    /// rather than in `detail`, because `detail` is only filled for the
    /// outcomes that failed and this revocation succeeded.
    fn record_revocation(&self, hash: &SessionHash, ip: IpAddr, reason: RevokeReason) {
        // Field names chosen around `ParamsSummary`'s redaction list: it
        // blanks any field whose name contains "session", so the tag goes in
        // one called `tag`.
        let record = crate::events::Record::new(
            crate::events::Issuer::System,
            "auth.session.revoked",
            self.node_id.clone(),
            crate::events::Outcome::Ok,
        )
        .from(ip)
        .params(crate::events::ParamsSummary::from_json(
            &serde_json::json!({ "reason": reason.as_str(), "tag": hash.tag() }),
        ));
        if let Err(e) = self.audit.append_dispatch(
            std::time::SystemTime::now(),
            &record,
            &crate::audit::DispatchFacts::none(),
        ) {
            // Logged and swallowed. The session is already gone; failing the
            // revocation because the log could not be written would leave a
            // session alive that the cap says must not be.
            tracing::warn!(error = %e, "cannot record a revoked session in the audit log");
        }
    }

    /// Persist a session so it survives a restart. A failure here is logged
    /// and swallowed: the sign-in itself succeeded, and refusing it because
    /// the database is unhappy would be a worse outcome than a session that
    /// does not outlive the process.
    fn remember(&self, hash: &SessionHash, session: &crate::auth::session::Session) {
        if let Err(e) = self
            .store
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert_session(hash, session)
        {
            tracing::warn!(error = %e, "cannot persist session");
        }
    }

    fn forget(&self, hash: &SessionHash) {
        if let Err(e) = self
            .store
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .delete_session(hash)
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
        if !self.trusts(peer) {
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
        if !self.trusts(peer) {
            return false;
        }
        headers
            .get("x-forwarded-proto")
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.eq_ignore_ascii_case("https"))
    }

    /// A request off the proxy socket is marked with
    /// [`crate::proxy::SOCKET_PEER`], an address no TCP connection can have,
    /// so it is trusted without any address being listed. Everything else is
    /// trusted only by address.
    fn trusts(&self, peer: SocketAddr) -> bool {
        peer == crate::proxy::SOCKET_PEER || self.trusted_proxies.contains(&peer.ip())
    }

    /// The session cookie this request presented, read under the name that
    /// goes with its scheme: `__Host-dj_session` over HTTPS, `dj_session`
    /// otherwise. See [`session::from_cookie_headers`].
    pub fn session_cookie(&self, peer: SocketAddr, headers: &HeaderMap) -> Option<SessionId> {
        session::from_cookie_headers(
            self.is_https(peer, headers),
            headers
                .get_all(header::COOKIE)
                .iter()
                .filter_map(|v| v.to_str().ok()),
        )
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
            (header::X_FRAME_OPTIONS, "DENY"),
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
    let id = ctx.session_cookie(peer, &headers);
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

/// The few assets that must be readable before a session exists.
///
/// iOS fetches the manifest and the home-screen icon at the moment "Add to
/// Home Screen" is pressed, and the launch image when the installed app opens.
/// At every one of those moments there is no session — the page in front of
/// the operator is `/login` — so behind the gate they 303 to the sign-in page
/// and iOS installs a screenshot icon with no standalone mode. That failure is
/// silent: nothing errors, the installed app simply comes out wrong.
///
/// Deliberately a closed list rather than a prefix like `/static/`. Each entry
/// is a generated branding file that says nothing the login page does not
/// already say out loud, and `..` is refused so the list cannot be walked out
/// of. The shell document, its JavaScript and its CSS stay behind the gate,
/// which is the property this module's doc describes.
pub(crate) fn is_public_install_asset(path: &str) -> bool {
    if path.contains("..") {
        return false;
    }
    path == "/manifest.webmanifest" || path.starts_with("/icons/") || path.starts_with("/splash/")
}

/// Everything else behind the gate: embedded assets, and client-side routes
/// that resolve to the shell document. Unauthenticated requests are sent to
/// sign in rather than served, so the build is not readable without a session
/// — except for [`is_public_install_asset`], which has to be.
async fn shell_asset(
    State(ctx): State<Arc<Ctx>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    uri: axum::http::Uri,
) -> Response {
    if is_public_install_asset(uri.path()) {
        return crate::shell::serve(uri.path());
    }
    let ip = ctx.client_ip(peer, &headers);
    let id = ctx.session_cookie(peer, &headers);
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

/// Put a router behind the session cookie.
///
/// For things that are not served out of this module and must not be
/// reachable without signing in — the app UI mount above all. An app's own
/// web build is served from the core's origin precisely so that sign-in, CSP
/// and theming stay the core's to enforce; serving it without this layer
/// would hand that away and leave an unauthenticated route into every
/// installed app.
///
/// Mounted as
/// `middleware::from_fn_with_state(Arc::clone(&ctx), require_session)`.
pub async fn require_session(
    State(ctx): State<Arc<Ctx>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    let ip = ctx.client_ip(peer, request.headers());
    let id = ctx.session_cookie(peer, request.headers());
    let signed_in = {
        let mut st = ctx.state();
        matches!(
            st.verify(ip, id.as_ref(), Now::current()),
            crate::auth::state::VerifyOutcome::Ok
        )
    };
    if !signed_in {
        // A redirect, as for any other page: these paths are opened by the
        // browser's address bar as often as by the shell.
        return redirect_to_login();
    }
    next.run(request).await
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
    ctx.apply(events);

    match outcome {
        LoginOutcome::Success { revoked } => {
            tracing::info!(%ip, revoked, "login ok");
            // Read back what `login` actually stored rather than rebuilding
            // it here, so the persisted row cannot drift from the live one.
            if let Some(s) = ctx.state().session(&new_session) {
                ctx.remember(&new_session.hash(), &s);
            }
            // After the session is persisted, because a watcher may lock the
            // node down, and a lockdown that races the row it is revoking
            // would leave it behind.
            for watch in &ctx.sign_in_watchers {
                watch(ip);
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
    if let Some(id) = ctx.session_cookie(peer, &headers)
        && ctx.state().logout(&id)
    {
        ctx.forget(&id.hash());
        ctx.session_ended(&id.hash());
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
            Arc::new(Mutex::new(AuthState::new(
                limits,
                Code::generate().unwrap(),
                Now::current(),
            ))),
            Arc::new(std::sync::Mutex::new(Store::in_memory().unwrap())),
            Duration::from_millis(0),
            trusted.iter().map(|s| s.parse().unwrap()).collect(),
            "test-node".to_string(),
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

    /// The pre-auth carve-out is a hole in the session gate, so its edges are
    /// pinned here. The half that matters is the second list: if this test is
    /// ever made to pass by widening the predicate, the shell build becomes
    /// readable without signing in.
    #[test]
    fn only_the_install_assets_are_readable_without_a_session() {
        for path in [
            "/manifest.webmanifest",
            "/icons/deckjack-180.png",
            "/icons/deckjack.svg",
            "/splash/splash-1320x2868.png",
        ] {
            assert!(is_public_install_asset(path), "iOS cannot install: {path}");
        }

        for path in [
            "/",
            "/index.html",
            "/assets/index-abc123.js",
            "/assets/index-abc123.css",
            "/sw.js",
            "/kit/bus.js",
            "/api/anything",
            "/ws",
            // A near-miss on each allowed prefix: the match is on the path
            // segment, not on the string appearing anywhere in it.
            "/not/icons/x.png",
            "/iconsy/x.png",
            "/manifest.webmanifest.map",
        ] {
            assert!(
                !is_public_install_asset(path),
                "reachable without a session: {path}"
            );
        }

        // Traversal out of an allowed prefix, which would otherwise reach the
        // whole embedded build through `/icons/`.
        for path in [
            "/icons/../index.html",
            "/icons/../assets/index-abc123.js",
            "/splash/../../etc/passwd",
        ] {
            assert!(!is_public_install_asset(path), "traversal allowed: {path}");
        }
    }
}
