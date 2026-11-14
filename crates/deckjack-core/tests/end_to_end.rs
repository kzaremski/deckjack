//! The whole thing, in one process, over a real socket.
//!
//! Every other test in this crate exercises one module with its world
//! injected. This one does the opposite on purpose: it builds the same
//! objects `main.rs` builds, in the same order, assembles the router through
//! the same `wiring::app`, binds a real `TcpListener` and talks to it the way
//! a browser does. The
//! defects this pass exists to catch — two halves of a wire protocol that
//! never agreed, a command marked with the wrong risk, a form posting to a
//! path that was never routed, a kill switch that was fully implemented and
//! never constructed — are all invisible to a module test and obvious here.
//!
//! # Why a hand-written client
//!
//! The crate has no `tower` and no `tokio-tungstenite`, and this file may not
//! add a dependency. That turns out to be an advantage rather than a cost:
//! `tower::ServiceExt::oneshot` would call the router as a function and never
//! prove that a request off a socket reaches it, and a WebSocket library
//! would hide the handshake, which is exactly where an upgrade gets refused.
//! So the HTTP requests are written as bytes and the WebSocket frames are
//! masked by hand. It is about a hundred lines, and it is the only part of
//! this file that is not the real system.
//!
//! # No sleeping
//!
//! Nothing here waits for a duration to decide anything. The sign-in failure
//! delay is configured to zero, every assertion is on state that is settled
//! by the time the response arrives, and the one place that genuinely has to
//! wait for another process — a shell starting up and printing — polls the
//! bus with `terminal.read`, which is a long poll: it parks until the shell
//! speaks. The timeouts around it are failure deadlines, not pacing.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use deckjack_core::apphost::AppHost;
use deckjack_core::apps;
use deckjack_core::auth::Code;
use deckjack_core::auth::session::COOKIE_NAME;
use deckjack_core::auth::state::{AuthState, Limits, Now};
use deckjack_core::command::Registry;
use deckjack_core::http::Ctx;
use deckjack_core::lockdown::{self, HostToken, Lockdown, Watchdog};
use deckjack_core::store::Store;
use deckjack_core::wiring::{self, BusDispatch};
use deckjack_core::{files, node, terminal, ws};

// ---------------------------------------------------------------------------
// The server, assembled the way main.rs assembles it
// ---------------------------------------------------------------------------

struct Harness {
    addr: SocketAddr,
    /// The one-time code, as the banner prints it on the host.
    code: String,
    /// Held so a test can ask what pty sessions actually exist, which is the
    /// only honest way to prove something was reaped: the browser's view of
    /// its own terminals is gone the moment its session is.
    terminals: Arc<terminal::Registry>,
    /// The real kill switch, built over the real auth state, store and
    /// terminal registry.
    lockdown: Arc<Lockdown>,
    /// What `deckjack unlock` reads off the host's filesystem.
    host_token: String,
    /// So a test can ask whether a revoked session is still on disk.
    store: Arc<std::sync::Mutex<Store>>,
    /// Two recovery codes, as the operator wrote them down.
    recovery: Vec<String>,
    /// The product's own bus, built by `wiring::bus`. Held so a test can ask
    /// whether a lockdown holds agents on it — the half `lockdown::gate`
    /// cannot cover, because an app invokes over its own socket and never
    /// reaches a router.
    bus: Arc<deckjack_core::command::Bus>,
    /// Keeps the working directory alive for as long as the server runs.
    _dir: tempfile::TempDir,
}

async fn start() -> Harness {
    let dir = tempfile::tempdir().expect("a working directory");
    let node_id = "e2e-node".to_string();

    let code = Code::generate().expect("a sign-in code");
    // `reveal` is the only way to read the text, and it has to happen before
    // the code moves into the state machine.
    let code_text = code.reveal();

    let limits = Limits {
        per_ip_attempts: 3,
        global_attempts: 10,
        session_max: Duration::from_secs(3600),
        max_sessions: 4,
    };
    // Shared, not owned by the HTTP context, because lockdown's step 1 has to
    // reach the same state machine. Same shape as main.rs.
    let state = Arc::new(std::sync::Mutex::new(AuthState::new(
        limits,
        code,
        Now::current(),
    )));
    let store = Arc::new(std::sync::Mutex::new(
        Store::open(dir.path()).expect("the state database"),
    ));

    let registry = Arc::new(Registry::new());
    let terminals = wiring::register_terminal(&registry, dir.path().to_path_buf())
        .expect("the terminal commands register");
    // Registered because main.rs registers them: a duplicate command id
    // between the sets would be a startup failure in production and a silent
    // absence here.
    let _files =
        files::register_commands(&registry, dir.path(), &node_id).expect("the file commands");
    let windows =
        wiring::register_windows(&registry, node_id.clone()).expect("the window commands register");
    let host_token =
        HostToken::load_or_create(dir.path(), &lockdown::OsRandom).expect("the host unlock token");
    let host_token_text = host_token.reveal().to_string();

    // The real `Deps`, over the real auth state, store and terminal registry
    // — the production wiring, not a set of fakes. The one thing that differs
    // is the grace period: `Policy::default` gives children two seconds to
    // exit, and this suite does not wait for durations to decide anything.
    let policy = lockdown::Policy {
        grace: Duration::from_millis(10),
        grace_checks: 2,
        ..lockdown::Policy::default()
    };
    // The real grants and the real Inbox, with main.rs's ceiling. Not fakes:
    // the whole point of this harness is that a defect in the production
    // assembly fails here rather than shipping, and `NoGrants` would make
    // every agent dispatch refuse for a reason the product does not have.
    let approvals = deckjack_core::approvals::Approvals::open(
        Arc::clone(&store),
        deckjack_core::command::RiskLevel::Destructive,
    );

    let lockdown_deps = || lockdown::Deps {
        clock: Box::new(lockdown::OsClock),
        random: Box::new(lockdown::OsRandom),
        sessions: Box::new(wiring::AuthSessions::new(
            Arc::clone(&state),
            Arc::clone(&store),
            Arc::clone(&terminals),
            node_id.clone(),
        )),
        children: Box::new(wiring::TerminalChildren::new(Arc::clone(&terminals))),
        signals: Box::new(lockdown::OsSignals::new()),
        outbound: approvals.outbound(),
        passkeys: Box::new(lockdown::PasskeysUnavailable),
        journal: Box::new(lockdown::FileJournal::new(dir.path())),
    };
    let lockdown = Arc::new(
        Lockdown::restore(lockdown_deps(), policy, node_id.clone()).expect("a fresh journal"),
    );
    // Clearing a lockdown has to clear the Inbox freeze: `Outbound` has a
    // freeze and no thaw, and `unlock` has no hook to call one from.
    approvals.follow_lockdown(Arc::downgrade(&lockdown));
    let recovery = lockdown
        .issue_recovery_codes(2)
        .expect("recovery codes on a node that is not locked")
        .iter()
        .map(lockdown::RecoveryCode::reveal)
        .collect();
    let watchdog = Arc::new(Watchdog::new(lockdown.policy().auto));
    wiring::register_lockdown(&registry, Arc::clone(&lockdown))
        .expect("the lockdown commands register");

    let audit = deckjack_core::audit::EventsJournal::new(Arc::clone(&store));
    deckjack_core::audit::register_timeline(&registry, audit.sink())
        .expect("the timeline command registers");

    // One grant source for the bus and the agent bridge alike, as in
    // main.rs. This harness is the only thing that can notice them drifting
    // apart, because `Bus` exposes no way to ask what its grants are.
    let agent_grants = approvals.grants();

    // `wiring::bus`, NOT a second copy of the builder chain. A hand-copied
    // assembly here is how a bus shipped with no grant storage while this
    // suite stayed green: `with_grants` is a `mut self` builder with no
    // getter, so the omission was neither a compile error nor observable.
    let bus = wiring::bus(wiring::BusParts {
        registry: Arc::clone(&registry),
        node_id: node_id.clone(),
        journal: audit.clone(),
        watchdog: Arc::clone(&watchdog),
        lockdown: Arc::clone(&lockdown),
        grants: Arc::clone(&agent_grants),
        approvals: approvals.inbox(),
        // As in main.rs: the lockdown holds agents on the bus, because the
        // HTTP gate cannot see an app's own socket.
        halt: Arc::new(wiring::LockdownHalt(Arc::clone(&lockdown))),
    });

    deckjack_core::approvals::register_approvals(&registry, &approvals, &bus)
        .expect("the grant and Inbox commands register");

    let reaper = Arc::clone(&terminals);
    let sign_in_store = Arc::clone(&store);
    let sign_in_watchdog = Arc::clone(&watchdog);
    let sign_in_lockdown = Arc::clone(&lockdown);
    let ctx = Arc::new(
        // Zero failure delay. The real delay is a cost imposed on someone
        // guessing codes over a network; paying it in a test would only make
        // the suite slow, and nothing here asserts on it.
        Ctx::new(
            Arc::clone(&state),
            Arc::clone(&store),
            Duration::ZERO,
            Vec::new(),
            node_id.clone(),
        )
        .on_session_end(move |hash| reaper.close_for_session(hash))
        .on_sign_in(move |ip| {
            let now = std::time::SystemTime::now();
            let known = {
                let st = sign_in_store
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                let known = st.is_known_address(ip).unwrap_or(true);
                let _ = st.remember_address(ip, now);
                known
            };
            wiring::act_on(
                &sign_in_lockdown,
                sign_in_watchdog.observe(
                    lockdown::Observation::SignIn {
                        ip,
                        known_device: known,
                    },
                    now,
                ),
            );
        }),
    );

    // The app host, over an install directory with nothing in it. Discovery
    // finding no apps is the ordinary state of a fresh install, and it is
    // what every test here wants: the point of building it is that the UI
    // mount, the app commands and the sweep are all really present.
    let install = dir.path().join("install");
    std::fs::create_dir_all(&install).expect("an install directory");
    let app_host = AppHost::new(
        apps::Host::new(
            install.clone(),
            dir.path().to_path_buf(),
            apps::OsEnv::new(),
            apps::Policy::default(),
            Box::new(deckjack_core::store::StoreGrants::new(Arc::clone(&store))),
        ),
        Arc::clone(&registry),
        install,
        Duration::from_millis(50),
    );
    windows.attach_app_host(Arc::clone(&app_host));
    // Before `start()`, which binds the sockets: an app that connects
    // before the bridge is attached refuses invocations for its whole life.
    app_host.attach_bridge(deckjack_core::agentbridge::AgentBridge::new(
        Arc::clone(&bus),
        Arc::clone(&agent_grants),
    ));
    wiring::register_apps(&registry, Arc::clone(&app_host)).expect("the app commands register");
    app_host.start().await;

    let ws_ctx = ws::WsCtx::new(
        Arc::clone(&ctx),
        BusDispatch::new(Arc::clone(&bus)),
        ws::Hub::new(Arc::new(ws::SystemClock)),
        Arc::new(ws::SystemClock),
        node_id,
        node::host_name(),
        ws::Settings::default(),
    );

    let passkeys = Arc::new(
        deckjack_core::passkeyhttp::PasskeyCtx::start(
            deckjack_core::passkeyhttp::settings_for(
                "127.0.0.1:7777".parse().unwrap(),
                &deckjack_core::config::Passkey::default(),
            )
            .expect("loopback always has usable passkey settings"),
            Arc::clone(&ctx),
            Arc::clone(&store),
            deckjack_core::passkeyhttp::os_env(),
        )
        .expect("the passkey registry starts"),
    );
    let app = wiring::app(
        Arc::clone(&ctx),
        ws_ctx,
        lockdown::UnlockCtx::new(Arc::clone(&lockdown), host_token),
        Arc::clone(&lockdown),
        Some(Arc::clone(&app_host)),
        Some(passkeys),
    );
    let tcp = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("a loopback port");
    let addr = tcp.local_addr().expect("the bound address");
    tokio::spawn(async move {
        let _ = axum::serve(tcp, app.into_make_service_with_connect_info::<SocketAddr>()).await;
    });

    Harness {
        addr,
        code: code_text,
        terminals,
        lockdown,
        host_token: host_token_text,
        recovery,
        store,
        bus,
        _dir: dir,
    }
}

impl Harness {
    async fn get(&self, path: &str, headers: &[(&str, &str)]) -> Response {
        request(self.addr, "GET", path, headers, None).await
    }

    async fn post(&self, path: &str, headers: &[(&str, &str)], body: &str) -> Response {
        request(self.addr, "POST", path, headers, Some(body)).await
    }

    /// Sign in with the real code and return the cookie, as a browser holds it.
    async fn sign_in(&self) -> String {
        let r = self
            .post("/login", &[], &format!("code={}", self.code))
            .await;
        assert_eq!(r.status, 303, "the real code should sign in: {}", r.body);
        session_cookie(&r)
    }
}

/// A well-formed code of the right shape that is not the real one, so a
/// failure is a wrong *code* and not malformed input taking a different path.
fn wrong_code(real: &str) -> String {
    let mut symbols: Vec<char> = real.chars().filter(|c| *c != '-').collect();
    symbols[0] = if symbols[0] == 'Z' { 'Y' } else { 'Z' };
    symbols.into_iter().collect()
}

// ---------------------------------------------------------------------------
// A hand-written HTTP client
// ---------------------------------------------------------------------------

struct Response {
    status: u16,
    headers: Vec<(String, String)>,
    body: String,
}

impl Response {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

async fn request(
    addr: SocketAddr,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: Option<&str>,
) -> Response {
    // `Connection: close` makes the response end at EOF, so the reply can be
    // read to the end without implementing chunked framing.
    let mut req = format!("{method} {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n");
    for (name, value) in headers {
        req.push_str(&format!("{name}: {value}\r\n"));
    }
    match body {
        Some(b) => {
            req.push_str("Content-Type: application/x-www-form-urlencoded\r\n");
            req.push_str(&format!("Content-Length: {}\r\n\r\n", b.len()));
            req.push_str(b);
        }
        None => req.push_str("\r\n"),
    }

    let mut stream = TcpStream::connect(addr).await.expect("connect");
    stream
        .write_all(req.as_bytes())
        .await
        .expect("send request");
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).await.expect("read response");

    let split = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .expect("a response with a header block");
    let head = String::from_utf8_lossy(&raw[..split]).into_owned();
    let body = String::from_utf8_lossy(&raw[split + 4..]).into_owned();

    let mut lines = head.split("\r\n");
    let status = lines
        .next()
        .and_then(|l| l.split_whitespace().nth(1).map(str::to_string))
        .and_then(|s| s.parse().ok())
        .expect("a status code");
    let headers = lines
        .filter_map(|l| l.split_once(':'))
        .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
        .collect();

    Response {
        status,
        headers,
        body,
    }
}

/// The `dj_session=...` pair out of a `Set-Cookie`, ready to send back.
fn session_cookie(r: &Response) -> String {
    let raw = r.header("set-cookie").expect("a Set-Cookie header");
    let pair = raw.split(';').next().expect("a cookie value").to_string();
    assert!(
        pair.starts_with(&format!("{COOKIE_NAME}=")),
        "expected the session cookie, got {pair}"
    );
    pair
}

// ---------------------------------------------------------------------------
// A hand-written WebSocket client
// ---------------------------------------------------------------------------

enum Frame {
    Text(String),
    Close { code: u16, reason: String },
}

struct Ws {
    stream: TcpStream,
}

impl Ws {
    /// Open the bus socket. `Err` carries the HTTP status when the server
    /// refused before upgrading, which is the case the bus cares about most:
    /// an unauthenticated peer must never get an upgraded socket at all.
    async fn connect(
        addr: SocketAddr,
        cookie: Option<&str>,
        origin: Option<&str>,
    ) -> Result<Ws, u16> {
        let mut req = format!(
            "GET {} HTTP/1.1\r\nHost: {addr}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\
             Sec-WebSocket-Version: 13\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n",
            ws::PATH
        );
        if let Some(c) = cookie {
            req.push_str(&format!("Cookie: {c}\r\n"));
        }
        if let Some(o) = origin {
            req.push_str(&format!("Origin: {o}\r\n"));
        }
        req.push_str("\r\n");

        let mut stream = TcpStream::connect(addr).await.expect("connect");
        stream
            .write_all(req.as_bytes())
            .await
            .expect("send upgrade");

        // One byte at a time, so none of the first server frame is swallowed
        // into a read buffer this client does not keep.
        let mut head = Vec::new();
        loop {
            let mut byte = [0u8; 1];
            let n = stream.read(&mut byte).await.expect("read the handshake");
            assert_eq!(n, 1, "the server closed mid-handshake");
            head.push(byte[0]);
            if head.ends_with(b"\r\n\r\n") {
                break;
            }
        }
        let text = String::from_utf8_lossy(&head).into_owned();
        let status: u16 = text
            .split_whitespace()
            .nth(1)
            .and_then(|s| s.parse().ok())
            .expect("a status code on the handshake");
        if status != 101 {
            return Err(status);
        }
        Ok(Ws { stream })
    }

    async fn send_frame(&mut self, opcode: u8, payload: &[u8]) {
        // A client frame must be masked; the mask itself is arbitrary.
        const MASK: [u8; 4] = [0x37, 0xfa, 0x21, 0x3d];
        let mut frame = vec![0x80 | opcode];
        if payload.len() < 126 {
            frame.push(0x80 | payload.len() as u8);
        } else {
            assert!(
                payload.len() <= u16::MAX as usize,
                "test frames stay under 64KiB"
            );
            frame.push(0x80 | 126);
            frame.extend_from_slice(&(payload.len() as u16).to_be_bytes());
        }
        frame.extend_from_slice(&MASK);
        frame.extend(
            payload
                .iter()
                .enumerate()
                .map(|(i, b)| b ^ MASK[i % MASK.len()]),
        );
        self.stream.write_all(&frame).await.expect("send a frame");
    }

    async fn send_text(&mut self, text: &str) {
        self.send_frame(0x1, text.as_bytes()).await;
    }

    async fn read_exact(&mut self, n: usize) -> Vec<u8> {
        let mut buf = vec![0u8; n];
        self.stream
            .read_exact(&mut buf)
            .await
            .expect("read frame bytes");
        buf
    }

    /// One text or close frame. Pings are answered on the way through, so a
    /// test that is between assertions does not look dead to the keepalive.
    async fn recv(&mut self) -> Frame {
        loop {
            let head = self.read_exact(2).await;
            let opcode = head[0] & 0x0f;
            let len = match head[1] & 0x7f {
                126 => {
                    let b = self.read_exact(2).await;
                    usize::from(u16::from_be_bytes([b[0], b[1]]))
                }
                127 => {
                    let b = self.read_exact(8).await;
                    u64::from_be_bytes(b.try_into().expect("eight bytes")) as usize
                }
                n => usize::from(n),
            };
            let payload = self.read_exact(len).await;
            match opcode {
                0x1 => return Frame::Text(String::from_utf8_lossy(&payload).into_owned()),
                0x8 => {
                    let code = if payload.len() >= 2 {
                        u16::from_be_bytes([payload[0], payload[1]])
                    } else {
                        0
                    };
                    return Frame::Close {
                        code,
                        reason: String::from_utf8_lossy(payload.get(2..).unwrap_or(&[]))
                            .into_owned(),
                    };
                }
                0x9 => self.send_frame(0xa, &payload).await,
                0xa => {}
                other => panic!("unexpected websocket opcode {other}"),
            }
        }
    }

    async fn expect_json(&mut self) -> Value {
        match self.recv().await {
            Frame::Text(t) => serde_json::from_str(&t).expect("a JSON frame"),
            Frame::Close { code, reason } => {
                panic!("the bus closed instead of answering: {code} {reason}")
            }
        }
    }

    async fn expect_close(&mut self) -> (u16, String) {
        match self.recv().await {
            Frame::Close { code, reason } => (code, reason),
            Frame::Text(t) => panic!("expected a close, got a frame: {t}"),
        }
    }

    /// The handshake. Returns the welcome frame.
    async fn hello(&mut self) -> Value {
        self.send_text(&json!({ "type": "hello", "protocol": ws::PROTOCOL_VERSION }).to_string())
            .await;
        self.expect_json().await
    }

    /// Invoke a command and wait for the response for that correlation id,
    /// stepping over progress and server-pushed events on the way.
    async fn invoke(&mut self, id: &str, command: &str, params: Value) -> Value {
        self.send_text(
            &json!({ "type": "invoke", "id": id, "command": command, "params": params })
                .to_string(),
        )
        .await;
        loop {
            let frame = self.expect_json().await;
            if frame["type"] == "response" && frame["id"] == id {
                return frame;
            }
        }
    }
}

/// The value out of a successful response, or a failure that says why not.
fn ok_value(response: &Value) -> &Value {
    assert_eq!(
        response["outcome"]["status"], "ok",
        "the command did not succeed: {response}"
    );
    &response["outcome"]["value"]
}

// ---------------------------------------------------------------------------
// Sign-in
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_code_signs_in_and_that_cookie_reaches_the_desktop() {
    let h = start().await;

    let login = h.post("/login", &[], &format!("code={}", h.code)).await;
    assert_eq!(login.status, 303, "sign-in should redirect: {}", login.body);
    assert_eq!(login.header("location"), Some("/"));
    let cookie = session_cookie(&login);

    let desktop = h.get("/", &[("cookie", cookie.as_str())]).await;
    assert_eq!(
        desktop.status, 200,
        "a signed-in browser must reach the desktop, not a 404 or a blank page"
    );
    assert!(
        desktop.body.to_ascii_lowercase().contains("<!doctype html"),
        "the desktop should be the shell document, got: {:?}",
        desktop.body.chars().take(120).collect::<String>()
    );
    assert_eq!(
        desktop.header("cache-control"),
        Some("no-store"),
        "the entry document must never be cached; a stale one asks for assets that are gone"
    );
}

#[tokio::test]
async fn the_desktop_is_not_reachable_without_a_cookie() {
    let h = start().await;

    let desktop = h.get("/", &[]).await;
    assert_eq!(desktop.status, 303);
    assert_eq!(desktop.header("location"), Some("/login"));
    assert!(
        desktop.body.is_empty(),
        "a redirect must not carry the document it refused to serve"
    );
}

#[tokio::test]
async fn the_build_is_not_readable_without_a_cookie() {
    let h = start().await;

    // Signed in first, so the asset is known to exist: otherwise a 303 could
    // just as well mean "no such file", and this test would prove nothing.
    let cookie = h.sign_in().await;
    let served = h.get("/sw.js", &[("cookie", cookie.as_str())]).await;
    assert_eq!(
        served.status, 200,
        "/sw.js should be part of the embedded build"
    );

    let refused = h.get("/sw.js", &[]).await;
    assert_eq!(
        refused.status, 303,
        "an asset must be gated like the document; the build is not public"
    );
    assert_eq!(refused.header("location"), Some("/login"));
    assert!(refused.body.is_empty());
}

#[tokio::test]
async fn three_wrong_codes_block_the_address_and_the_right_code_no_longer_works() {
    let h = start().await;
    let wrong = wrong_code(&h.code);

    for attempt in 1..=2 {
        let r = h.post("/login", &[], &format!("code={wrong}")).await;
        assert_eq!(
            r.status, 401,
            "attempt {attempt} is wrong, not yet a lock: {}",
            r.body
        );
        assert!(r.header("set-cookie").is_none());
    }

    let third = h.post("/login", &[], &format!("code={wrong}")).await;
    assert_eq!(
        third.status, 403,
        "the third wrong code should block the address"
    );

    // The invariant: once the address is blocked the gate is shut, so the
    // fourth attempt is refused *without the code being checked at all*. The
    // observable form of "without running" is that the real code — which
    // would otherwise work — does not.
    let fourth = h.post("/login", &[], &format!("code={}", h.code)).await;
    assert_eq!(
        fourth.status, 403,
        "a blocked address must be refused even with the right code"
    );
    assert!(
        fourth.header("set-cookie").is_none(),
        "a refused login must not hand out a session"
    );

    // And the gate is shut for reads too, not just for the form.
    let desktop = h.get("/", &[]).await;
    assert_eq!(desktop.status, 303);
}

#[tokio::test]
async fn the_login_form_posts_to_a_path_that_exists() {
    let h = start().await;

    // The form's own action, read off the page it was rendered on. A page
    // that posts somewhere unrouted fails as an empty 404 body, which in a
    // browser is indistinguishable from a rendering bug.
    let page = h.get("/login", &[]).await;
    assert_eq!(page.status, 200);
    assert!(
        page.body.contains("action=\"/login\""),
        "the login form should post to /login: {}",
        page.body
    );

    let posted = h.post("/login", &[], "code=").await;
    assert_ne!(
        posted.status, 404,
        "the path the form posts to is not routed"
    );
    assert!(
        !posted.body.is_empty(),
        "an empty body renders as a blank white page and reads as a rendering bug"
    );
}

// ---------------------------------------------------------------------------
// The asset fallback
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_missing_asset_404s_while_a_client_route_resolves_to_the_document() {
    let h = start().await;
    let cookie = h.sign_in().await;

    let missing = h
        .get(
            "/assets/not-in-this-build-abc123.js",
            &[("cookie", cookie.as_str())],
        )
        .await;
    assert_eq!(
        missing.status, 404,
        "a missing .js must 404; HTML handed back for a script is a syntax error \
         pointing at '<' and half an hour of confusion"
    );
    let lower = missing.body.to_ascii_lowercase();
    assert!(
        !lower.contains("<!doctype") && !lower.contains("<html"),
        "a missing asset was answered with the HTML document: {}",
        missing.body
    );

    // The contrast is the point: the same fallback must still resolve a
    // client-side route, which has no extension, to the shell document.
    let route = h.get("/files/notes", &[("cookie", cookie.as_str())]).await;
    assert_eq!(
        route.status, 200,
        "a client-side route is a view the browser resolves, not a missing file"
    );
}

#[tokio::test]
async fn nothing_outside_the_embedded_build_can_be_fetched() {
    let h = start().await;
    let cookie = h.sign_in().await;

    // A browser would normalize these away; a socket will not, and the
    // terminal being the documented escape hatch is not a reason for the
    // asset route to be a second one.
    for path in [
        "/../Cargo.toml",
        "/../../Cargo.toml",
        "/assets/../../Cargo.toml",
        "/..%2fCargo.toml",
    ] {
        let r = h.get(path, &[("cookie", cookie.as_str())]).await;
        assert_eq!(r.status, 404, "{path} was not refused");
        assert!(
            !r.body.contains("[package]"),
            "{path} served a file from outside the build: {}",
            r.body
        );
    }
}

// ---------------------------------------------------------------------------
// The bus socket
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_bus_refuses_to_upgrade_without_a_session() {
    let h = start().await;

    let status = Ws::connect(h.addr, None, None)
        .await
        .err()
        .expect("an unauthenticated peer must not get an upgraded socket");
    assert_eq!(
        status, 401,
        "a WebSocket client has nowhere to follow a redirect to, so the refusal is a 401"
    );
}

#[tokio::test]
async fn a_cross_origin_socket_is_refused_even_with_a_valid_cookie() {
    let h = start().await;
    let cookie = h.sign_in().await;

    // The same-origin policy does not apply to WebSocket, so this check is
    // the whole defense against another open page riding the session cookie.
    let status = Ws::connect(h.addr, Some(&cookie), Some("http://evil.example"))
        .await
        .err()
        .expect("a cross-origin upgrade must be refused");
    assert_eq!(status, 403);

    // And the same cookie from the right origin still works, so the refusal
    // was about the origin and not about the cookie.
    let origin = format!("http://{}", h.addr);
    Ws::connect(h.addr, Some(&cookie), Some(&origin))
        .await
        .expect("the same cookie from this host should be allowed");
}

#[tokio::test]
async fn nothing_is_accepted_before_the_protocol_version_is_agreed() {
    let h = start().await;
    let cookie = h.sign_in().await;
    let mut ws = Ws::connect(h.addr, Some(&cookie), None)
        .await
        .expect("upgrade");

    // An invoke as the first frame. Accepting it would mean running a command
    // for a client whose wire format has not been checked.
    ws.send_text(
        &json!({ "type": "invoke", "id": "1", "command": "terminal.list", "params": {} })
            .to_string(),
    )
    .await;
    let (code, reason) = ws.expect_close().await;
    assert_eq!(code, 4400, "closed with {code}: {reason}");
    assert!(
        reason.contains("hello"),
        "the reason should say what was expected: {reason}"
    );
}

#[tokio::test]
async fn a_protocol_mismatch_is_refused_at_the_handshake_and_names_both_numbers() {
    let h = start().await;
    let cookie = h.sign_in().await;
    let mut ws = Ws::connect(h.addr, Some(&cookie), None)
        .await
        .expect("upgrade");

    let offered = ws::PROTOCOL_VERSION + 7;
    ws.send_text(&json!({ "type": "hello", "protocol": offered }).to_string())
        .await;
    let (code, reason) = ws.expect_close().await;
    assert_eq!(code, 4426, "closed with {code}: {reason}");
    assert!(
        reason.contains(&offered.to_string()) && reason.contains(&ws::PROTOCOL_VERSION.to_string()),
        "a version mismatch has to name both numbers or it cannot be acted on: {reason}"
    );
}

#[tokio::test]
async fn the_welcome_frame_agrees_with_the_protocol_the_client_offered() {
    let h = start().await;
    let cookie = h.sign_in().await;
    let mut ws = Ws::connect(h.addr, Some(&cookie), None)
        .await
        .expect("upgrade");

    let welcome = ws.hello().await;
    // The shape of this frame is the contract the browser's generated client
    // is built against: a different tag field or a different field name here
    // means nothing can talk to anything.
    assert_eq!(welcome["type"], "welcome", "got {welcome}");
    assert_eq!(welcome["protocol"], ws::PROTOCOL_VERSION);
    assert!(
        welcome["connection"].is_u64(),
        "the connection id should be a number: {welcome}"
    );
    assert_eq!(welcome["node_id"], "e2e-node");
    assert!(welcome["host_name"].is_string());
}

#[tokio::test]
async fn an_unknown_command_is_answered_rather_than_closing_the_socket() {
    let h = start().await;
    let cookie = h.sign_in().await;
    let mut ws = Ws::connect(h.addr, Some(&cookie), None)
        .await
        .expect("upgrade");
    ws.hello().await;

    let r = ws.invoke("1", "nothing.registered.here", json!({})).await;
    assert_eq!(
        r["outcome"]["status"], "error",
        "an unknown command is an error, not a protocol violation: {r}"
    );

    // The connection has to survive it, or one stale button in the shell
    // takes the whole desktop's socket down.
    let after = ws.invoke("2", "terminal.list", json!({})).await;
    ok_value(&after);
}

// ---------------------------------------------------------------------------
// The terminal, end to end
// ---------------------------------------------------------------------------

/// Open a terminal over the bus and return its id.
async fn open_terminal(ws: &mut Ws) -> String {
    let opened = ws
        .invoke("open", "terminal.open", json!({ "rows": 24, "cols": 80 }))
        .await;
    let value = ok_value(&opened);
    assert_eq!(
        value["cursor"], 0,
        "a new terminal starts at cursor zero: {value}"
    );
    value["id"]
        .as_str()
        .expect("a terminal id in the response")
        .to_string()
}

#[tokio::test]
async fn a_terminal_runs_what_is_written_to_it_and_the_cursor_advances() {
    let h = start().await;
    let cookie = h.sign_in().await;
    let mut ws = Ws::connect(h.addr, Some(&cookie), None)
        .await
        .expect("upgrade");
    ws.hello().await;

    let id = open_terminal(&mut ws).await;

    // The marker only ever appears in what the shell *prints*. The pty echoes
    // the keystrokes back too, but those carry the format string, so a test
    // that found "ready-e2e-done" has found shell output and not its own
    // input coming back off a pty with nothing behind it.
    ws.invoke(
        "write",
        "terminal.write",
        json!({ "id": id, "data": "printf 'ready-%s-done\\n' e2e\r" }),
    )
    .await;

    let mut cursor = 0u64;
    let mut seen = String::new();
    let mut n = 0;
    // `terminal.read` is a long poll: it parks until the shell speaks, so
    // this loop waits on output rather than on the clock. The deadline is
    // only here so a shell that never starts fails the test instead of
    // hanging the suite.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    while !seen.contains("ready-e2e-done") {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the shell never printed the marker; read {} bytes: {seen:?}",
            seen.len()
        );
        n += 1;
        let chunk = tokio::time::timeout(
            Duration::from_secs(10),
            ws.invoke(
                &format!("read{n}"),
                "terminal.read",
                json!({ "id": id, "cursor": cursor }),
            ),
        )
        .await
        .expect("terminal.read answered");
        let value = ok_value(&chunk).clone();

        let next = value["cursor"].as_u64().expect("a cursor in the chunk");
        assert!(
            next >= cursor,
            "the cursor went backwards ({cursor} -> {next}); a client that \
             re-read from it would lose output"
        );
        cursor = next;
        seen.push_str(value["data"].as_str().expect("data in the chunk"));

        assert!(
            value["exited"] != true || seen.contains("ready-e2e-done"),
            "the shell exited before running the command; output was {seen:?}"
        );
    }

    assert!(
        cursor > 0,
        "output was read but the cursor did not advance, so a reattach would \
         replay forever"
    );

    // Reading from the cursor the client now holds must not hand back what it
    // has already seen: that is the whole contract reattach rests on.
    ws.invoke(
        "write2",
        "terminal.write",
        json!({ "id": id, "data": "printf 'second-%s-line\\n' e2e\r" }),
    )
    .await;
    let mut tail = String::new();
    let mut m = 0;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    while !tail.contains("second-e2e-line") {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the second command never printed; tail was {tail:?}"
        );
        m += 1;
        let chunk = tokio::time::timeout(
            Duration::from_secs(10),
            ws.invoke(
                &format!("tail{m}"),
                "terminal.read",
                json!({ "id": id, "cursor": cursor }),
            ),
        )
        .await
        .expect("terminal.read answered");
        let value = ok_value(&chunk).clone();
        cursor = value["cursor"].as_u64().expect("a cursor");
        tail.push_str(value["data"].as_str().expect("data"));
    }
    assert!(
        !tail.contains("ready-e2e-done"),
        "a read from the client's cursor replayed output it had already seen: {tail:?}"
    );
}

#[tokio::test]
async fn writing_to_a_terminal_never_asks_the_operator_to_confirm() {
    let h = start().await;
    let cookie = h.sign_in().await;
    let mut ws = Ws::connect(h.addr, Some(&cookie), None)
        .await
        .expect("upgrade");
    ws.hello().await;
    let id = open_terminal(&mut ws).await;

    // This command fires on every keystroke. Any risk level above Write puts
    // a confirmation dialog in front of each keypress, which is the exact
    // regression that shipped once already — and it is invisible to a unit
    // test of the command, because the routing decision is the bus's.
    let wrote = ws
        .invoke("write", "terminal.write", json!({ "id": id, "data": "x" }))
        .await;
    let value = ok_value(&wrote);
    assert!(
        value.is_null(),
        "terminal.write must simply run; it came back as {value}"
    );

    // Opening a terminal is the most consequential thing on this bus, and it
    // is deliberately not destructive either: the grant keeps agents out, not
    // the risk level. Asserted because a well-meaning change to either would
    // make the terminal unusable rather than insecure, and nothing else says so.
    let opened = ws.invoke("open2", "terminal.open", json!({})).await;
    let value = ok_value(&opened);
    assert!(
        value["id"].is_string(),
        "terminal.open must run for the operator at their own browser: {value}"
    );
}

#[tokio::test]
async fn signing_out_reaps_the_terminal_that_session_opened() {
    let h = start().await;
    let cookie = h.sign_in().await;
    let mut ws = Ws::connect(h.addr, Some(&cookie), None)
        .await
        .expect("upgrade");
    ws.hello().await;

    open_terminal(&mut ws).await;
    assert_eq!(
        h.terminals.list().len(),
        1,
        "the terminal the browser opened should be on the host"
    );

    let out = h.post("/logout", &[("cookie", cookie.as_str())], "").await;
    assert_eq!(out.status, 303);
    assert_eq!(out.header("location"), Some("/login"));

    // A pty is a process running as the host user. One left behind after the
    // browser that asked for it has gone is a shell nobody is watching.
    assert!(
        h.terminals.list().is_empty(),
        "sign-out left a shell running: {:?}",
        h.terminals.list()
    );

    // And the cookie it signed out with is dead, so the socket it would open
    // next is refused rather than reattaching to a session that ended.
    let status = Ws::connect(h.addr, Some(&cookie), None)
        .await
        .err()
        .expect("a signed-out cookie must not open a socket");
    assert_eq!(status, 401);
}

#[tokio::test]
async fn a_terminal_opened_by_one_session_is_not_reaped_by_another_signing_out() {
    let h = start().await;
    let cookie = h.sign_in().await;
    let mut ws = Ws::connect(h.addr, Some(&cookie), None)
        .await
        .expect("upgrade");
    ws.hello().await;
    open_terminal(&mut ws).await;

    // A logout carrying no session at all, which is what a stale tab sends.
    // It must not be a way to kill the shells of whoever is still signed in.
    let out = h.post("/logout", &[], "").await;
    assert_eq!(out.status, 303);
    assert_eq!(
        h.terminals.list().len(),
        1,
        "a sign-out with no session reaped someone else's terminal"
    );
}

// ---------------------------------------------------------------------------
// Revocation that nobody asked for
// ---------------------------------------------------------------------------

/// Blocking an address revokes that address's sessions, and the shells they
/// opened go with them.
///
/// This is the same invariant `signing_out_reaps_the_terminal_that_session_opened`
/// proves for the voluntary path. The involuntary path is the one that fires
/// when something hostile is happening, so it is the one that matters:
/// `AuthState::login` reports each revocation as `Event::SessionRevoked` and
/// `Ctx::apply` deletes the persisted row and runs the reapers for it.
#[tokio::test]
async fn blocking_an_address_reaps_the_terminals_of_the_sessions_it_revokes() {
    let h = start().await;
    let cookie = h.sign_in().await;
    let mut ws = Ws::connect(h.addr, Some(&cookie), None)
        .await
        .expect("upgrade");
    ws.hello().await;
    open_terminal(&mut ws).await;
    assert_eq!(h.terminals.list().len(), 1);

    let wrong = wrong_code(&h.code);
    for _ in 0..3 {
        h.post("/login", &[], &format!("code={wrong}")).await;
    }

    // The session is gone: the cookie no longer opens a socket.
    let status = Ws::connect(h.addr, Some(&cookie), None)
        .await
        .err()
        .expect("the revoked session must not open a socket");
    assert_eq!(status, 403);

    assert!(
        h.terminals.list().is_empty(),
        "a revoked session left a shell running: {:?}",
        h.terminals.list()
    );
}

/// A session taken away leaves a row in the audit log.
///
/// Destroying a signed-in browser's session used to be the one thing the node
/// did to a user that it wrote nowhere: the row was deleted and the ptys were
/// reaped in silence. The row is written by `http::Ctx` itself, from the
/// store it already holds, rather than by a closure `main.rs` registers —
/// which is why this test means something. A registration is a line that can
/// be present in this harness's hand-copied builder chain and missing from
/// `main.rs`, and then every test here is green and the product writes
/// nothing. There is nothing to register, so there is nothing to forget, and
/// `node_id` is a required argument of `Ctx::new` rather than a builder call,
/// so a node cannot be assembled without one.
#[tokio::test]
async fn a_revoked_session_leaves_an_audit_row() {
    let h = start().await;
    let cookie = h.sign_in().await;
    assert!(!cookie.is_empty(), "the sign-in produced no cookie");

    let wrong = wrong_code(&h.code);
    for _ in 0..3 {
        h.post("/login", &[], &format!("code={wrong}")).await;
    }

    let rows = deckjack_core::audit::StoreSink::new(Arc::clone(&h.store))
        .timeline(&deckjack_core::events::Query::default())
        .expect("the timeline reads");
    let revoked: Vec<_> = rows
        .iter()
        .filter(|r| r.entry.command == "auth.session.revoked")
        .collect();

    assert_eq!(
        revoked.len(),
        1,
        "a session was revoked and the audit log does not say so"
    );
    assert_eq!(
        revoked[0].entry.target_node, "e2e-node",
        "the row names the wrong node"
    );
    assert!(
        revoked[0].entry.params.as_str().contains("address_blocked"),
        "the row has to say why the session went: {}",
        revoked[0].entry.params
    );
}

/// The kill switch has to leave a trace of whose sessions it took.
///
/// Lockdown revoked sessions, deleted their rows and killed their shells while
/// writing nothing at all to the audit log, so afterwards there was no record
/// that anyone had been signed in — the one event most worth being able to
/// reconstruct. The row is written by `wiring::AuthSessions::revoke_all`, not
/// by `AuthState::kick`, which is a pure state machine with no journal and no
/// business acquiring one.
#[tokio::test]
async fn a_lockdown_leaves_an_audit_row_for_every_session_it_revoked() {
    let h = start().await;
    let cookie = h.sign_in().await;
    assert!(!cookie.is_empty(), "the sign-in produced no cookie");

    h.lockdown.engage(lockdown::Trigger::Operator {
        surface: lockdown::Surface::Palette,
    });

    let rows = deckjack_core::audit::StoreSink::new(Arc::clone(&h.store))
        .timeline(&deckjack_core::events::Query::default())
        .expect("the timeline reads");
    let revoked: Vec<_> = rows
        .iter()
        .filter(|r| r.entry.command == "auth.session.revoked")
        .collect();

    assert_eq!(
        revoked.len(),
        1,
        "a lockdown revoked a session and the audit log does not say so"
    );
    assert_eq!(
        revoked[0].entry.target_node, "e2e-node",
        "the row names the wrong node"
    );
    // Distinct from the blocked-address path's own reason, which the test
    // above pins. If the two ever collapse into one string, a lockdown and a
    // lockout become indistinguishable afterwards.
    assert!(
        revoked[0].entry.params.as_str().contains("lockdown"),
        "the row has to say the lockdown took it: {}",
        revoked[0].entry.params
    );
}

// ---------------------------------------------------------------------------
// The kill switch, in the assembled server
// ---------------------------------------------------------------------------
//
// `tests/lockdown.rs` proves every rule in the module with its world
// injected, and passed all of them while the kill switch could not fire at
// all: nothing constructed a `Lockdown`, the gate was never mounted, and its
// commands were never registered. Those are assembly facts, so they are
// checked here, against the router `main.rs` actually serves.

#[tokio::test]
async fn the_gate_is_mounted_so_a_locked_node_serves_nothing_but_the_locked_page() {
    let h = start().await;
    let cookie = h.sign_in().await;

    // Signed in and served before the lockdown.
    assert_eq!(h.get("/", &[("Cookie", &cookie)]).await.status, 200);

    h.lockdown.engage(lockdown::Trigger::Operator {
        surface: lockdown::Surface::Palette,
    });

    // Every route, including the one that was working a line ago and the
    // sign-in page an attacker would try next.
    for path in ["/", "/login", "/assets/anything.js", "/not-a-route"] {
        let r = h.get(path, &[("Cookie", &cookie)]).await;
        assert_eq!(r.status, 503, "{path} was served while locked down");
        assert!(
            r.body.contains("Locked down"),
            "{path} did not serve the locked page: {}",
            r.body
        );
    }

    // And the bus socket, which is the one that would let a held session keep
    // working without ever asking for a page again.
    let status = Ws::connect(h.addr, Some(&cookie), None)
        .await
        .err()
        .expect("the bus socket must not upgrade while locked down");
    assert_eq!(status, 503);
}

#[tokio::test]
async fn locking_down_revokes_the_sessions_on_disk_as_well_as_in_memory() {
    let h = start().await;
    h.sign_in().await;

    assert_eq!(
        h.store
            .lock()
            .unwrap()
            .live_sessions(std::time::SystemTime::now())
            .unwrap()
            .len(),
        1,
        "the sign-in was not persisted, so this test would pass for the wrong reason"
    );

    let report = h.lockdown.engage(lockdown::Trigger::HostCommand);
    assert_eq!(report.sessions_revoked, 1);

    // The rows too. A count-only revocation leaves them, and the next start
    // restores every session the lockdown just revoked.
    assert!(
        h.store
            .lock()
            .unwrap()
            .live_sessions(std::time::SystemTime::now())
            .unwrap()
            .is_empty(),
        "a revoked session is still in the state database and would come back"
    );
}

#[tokio::test]
async fn locking_down_kills_the_shells_it_finds() {
    let h = start().await;
    let cookie = h.sign_in().await;
    let mut ws = Ws::connect(h.addr, Some(&cookie), None)
        .await
        .expect("upgrade");
    ws.hello().await;
    open_terminal(&mut ws).await;
    assert_eq!(h.terminals.list().len(), 1);

    let report = h.lockdown.engage(lockdown::Trigger::HostCommand);

    // Gone, and gone from the registry: `AuthSessions::revoke_all` kills the
    // process group and drops the session, which closes the pty master too.
    assert!(
        h.terminals.list().is_empty(),
        "a shell outlived the lockdown: {:?}",
        h.terminals.list()
    );

    // So step 2 has nothing left to find, which is the honest report and not
    // a `NoChildren` stub: nothing survived, and nothing had to be chased.
    assert_eq!(report.children_seen, 0, "{report:?}");
    assert!(report.survived.is_empty(), "{report:?}");
    assert!(report.errors.is_empty(), "{report:?}");
}

#[tokio::test]
async fn a_shell_the_session_sweep_missed_is_still_reported_as_a_child() {
    // The other half of the ordering above: `TerminalChildren` is what makes
    // a process nobody killed something the operator is told about, rather
    // than a shell left running under a locked-down node.
    let h = start().await;
    let cookie = h.sign_in().await;
    let mut ws = Ws::connect(h.addr, Some(&cookie), None)
        .await
        .expect("upgrade");
    ws.hello().await;
    open_terminal(&mut ws).await;

    let children = wiring::TerminalChildren::new(Arc::clone(&h.terminals));
    let seen = deckjack_core::lockdown::Children::running(&children);
    assert_eq!(seen.len(), 1, "a live shell was not reported: {seen:?}");
    assert_eq!(seen[0].kind, deckjack_core::lockdown::ChildKind::Terminal);
    assert!(seen[0].pid > 1, "a pid lockdown would refuse to signal");

    h.terminals.kill_all();
    assert!(
        deckjack_core::lockdown::Children::running(&children).is_empty(),
        "a killed shell is still reported as running"
    );
}

#[tokio::test]
async fn the_lockdown_command_is_on_the_bus_and_does_not_ask_the_operator_to_confirm() {
    let h = start().await;
    let cookie = h.sign_in().await;
    let mut ws = Ws::connect(h.addr, Some(&cookie), None)
        .await
        .expect("upgrade");
    ws.hello().await;

    let response = ws
        .invoke(
            "lock",
            lockdown::CMD_ENGAGE,
            json!({ "surface": "palette" }),
        )
        .await;

    // Not `needs_confirmation`. Triggering is one click with no step-up by
    // design — "in an emergency friction is the enemy" — so a `Destructive`
    // risk level here would put a dialog in front of the kill switch.
    let value = ok_value(&response);
    assert_eq!(value["status"], Value::Null, "got {value}");
    assert_eq!(value["key_rotated"], true, "got {value}");
    assert!(h.lockdown.is_locked());
}

#[tokio::test]
async fn the_status_command_is_on_the_bus() {
    let h = start().await;
    let cookie = h.sign_in().await;
    let mut ws = Ws::connect(h.addr, Some(&cookie), None)
        .await
        .expect("upgrade");
    ws.hello().await;

    // The socket is the operator by construction, so this is the closest a
    // test gets from outside: the status command is read-only and must work,
    // which proves the registration is real and not the reason above.
    let status = ws.invoke("st", "system.status", json!({})).await;
    let value = ok_value(&status);
    assert_eq!(value["locked"], false, "got {value}");
    assert_eq!(value["recovery_codes_left"], 2, "got {value}");
}

#[tokio::test]
async fn unlocking_needs_the_host_token_and_a_recovery_code() {
    let h = start().await;
    h.lockdown.engage(lockdown::Trigger::HostCommand);
    assert!(h.lockdown.is_locked());

    let good_code = h.recovery[0].clone();

    // /unlock is reachable while locked; everything else is not.
    assert_eq!(h.get("/unlock", &[]).await.status, 503);

    // No token: refused before a code is spent, so guessing codes against a
    // locked node is not something that can be done over the network at all.
    let r = h
        .post("/unlock", &[], &format!("recovery_code={good_code}"))
        .await;
    assert_eq!(r.status, 403, "{}", r.body);
    assert_eq!(
        h.lockdown.recovery_codes_left(),
        2,
        "a code was spent by a request that could not prove host access"
    );

    // The token alone is not enough either.
    let r = h
        .post(
            "/unlock",
            &[],
            &format!(
                "token={}&recovery_code=AAAA-AAAA-AAAA-AAAA-AAAA",
                h.host_token
            ),
        )
        .await;
    assert_eq!(r.status, 403, "{}", r.body);
    assert!(h.lockdown.is_locked());

    // Both.
    let r = h
        .post(
            "/unlock",
            &[],
            &format!("token={}&recovery_code={good_code}", h.host_token),
        )
        .await;
    assert_eq!(r.status, 200, "{}", r.body);
    assert!(!h.lockdown.is_locked());
    assert_eq!(
        h.lockdown.recovery_codes_left(),
        1,
        "the code was not spent"
    );

    // The same code does not work twice.
    h.lockdown.engage(lockdown::Trigger::HostCommand);
    let r = h
        .post(
            "/unlock",
            &[],
            &format!("token={}&recovery_code={good_code}", h.host_token),
        )
        .await;
    assert_eq!(r.status, 403, "a spent recovery code unlocked again");
    assert!(h.lockdown.is_locked());
}

#[tokio::test]
async fn after_an_unlock_the_node_serves_again() {
    let h = start().await;
    let cookie = h.sign_in().await;
    h.lockdown.engage(lockdown::Trigger::HostCommand);

    // The session is gone with the lockdown, which is step 1, so signing in
    // again is the way back — not the old cookie.
    assert_eq!(h.get("/", &[("Cookie", &cookie)]).await.status, 503);

    let r = h
        .post(
            "/unlock",
            &[],
            &format!("token={}&recovery_code={}", h.host_token, h.recovery[0]),
        )
        .await;
    assert_eq!(r.status, 200, "{}", r.body);

    // Served again, and the revoked cookie is still revoked: the gate opening
    // must not hand back the sessions the lockdown took.
    let r = h.get("/", &[("Cookie", &cookie)]).await;
    assert_eq!(r.status, 303, "a revoked cookie worked after the unlock");
    assert_eq!(r.header("location"), Some("/login"));
    assert_eq!(h.get("/login", &[]).await.status, 200);
}

/// The lockdown holds agents on the bus, and the `/unlock` ROUTE releases it.
///
/// Through the real product assembly: `wiring::bus` with `LockdownHalt`
/// attached, and the unlock going over HTTP exactly as `deckjack unlock`
/// sends it, rather than by calling `Lockdown::unlock` directly. The hold is
/// the half `lockdown::gate` cannot cover — it is axum middleware, so it sees
/// HTTP and ws and not an app's own Unix socket.
///
/// No command is registered under this id on purpose: `Bus::plan` checks the
/// hold BEFORE it touches the registry, so a locked node answers
/// `AgentsPaused` where an unlocked one answers `UnknownCommand`. Two
/// different refusals, no probe command needed, and nothing leaked about
/// which commands exist to a caller that is locked out.
#[tokio::test]
async fn a_lockdown_holds_agents_on_the_real_bus_and_the_unlock_route_releases_them() {
    use deckjack_core::command::{CommandId, Issuer, Refused, Request};

    let h = start().await;

    // An app's invocation, as `agentbridge::app_identity` shapes it: an
    // `Issuer::Agent` on the reserved `app:<name>` session.
    let from_an_app = || {
        Request::new(
            CommandId::new("probe.read").expect("a valid command id"),
            json!({}),
            Issuer::agent("app:notes"),
        )
    };

    assert!(
        matches!(
            h.bus
                .dispatch(from_an_app())
                .expect_err("no command is registered under that id"),
            Refused::UnknownCommand(_)
        ),
        "unlocked, an app's dispatch should get as far as the registry; if it \
         does not, the rest of this test proves nothing"
    );

    h.lockdown.engage(lockdown::Trigger::HostCommand);
    assert!(
        matches!(
            h.bus.dispatch(from_an_app()).expect_err("locked"),
            Refused::AgentsPaused
        ),
        "a lockdown left an app's dispatches running: `lockdown::gate` never \
         sees the socket they arrive on"
    );

    // The operator's own lever cannot let agents back in during a lockdown.
    h.bus.resume_agents();
    assert!(matches!(
        h.bus.dispatch(from_an_app()).expect_err("still locked"),
        Refused::AgentsPaused
    ));

    let r = h
        .post(
            "/unlock",
            &[],
            &format!("token={}&recovery_code={}", h.host_token, h.recovery[0]),
        )
        .await;
    assert_eq!(r.status, 200, "{}", r.body);

    assert!(
        matches!(
            h.bus
                .dispatch(from_an_app())
                .expect_err("no command is registered under that id"),
            Refused::UnknownCommand(_)
        ),
        "the unlock route reopened the router but not the bus: a lockdown you \
         cannot leave is worse than one that does not engage"
    );
}

#[tokio::test]
async fn the_host_token_is_owner_only_and_stable_across_reads() {
    let dir = tempfile::tempdir().unwrap();
    let first = HostToken::load_or_create(dir.path(), &lockdown::OsRandom).unwrap();
    let again = HostToken::load_or_create(dir.path(), &lockdown::OsRandom).unwrap();
    assert_eq!(
        first.reveal(),
        again.reveal(),
        "a token that changed every start would invalidate an unlock midway through"
    );
    assert!(first.matches(again.reveal()));
    assert!(!first.matches("not the token"));
    assert!(!first.matches(""));

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.path().join(lockdown::HOST_TOKEN_PATH);
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(
            mode & 0o077,
            0,
            "{} is mode {mode:o}: group or other can read the host unlock token",
            path.display()
        );
    }

    // Never printed, in a module whose whole job is to be read during an
    // incident.
    assert_eq!(format!("{first:?}"), "HostToken(<redacted>)");
}

// ---------------------------------------------------------------------------
// The automatic triggers, through the real adapters
// ---------------------------------------------------------------------------

/// A lockdown over the same real `Deps` the harness builds, with an
/// `AutoPolicy` the test chooses. Separate from `start()` because the point
/// is the policy: the shipped default for most automatic triggers is `Alert`,
/// which by design produces a log line and nothing a test can assert on.
fn with_auto_policy(
    auto: lockdown::AutoPolicy,
) -> (Arc<Lockdown>, Arc<Watchdog>, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("a working directory");
    let state = Arc::new(std::sync::Mutex::new(AuthState::new(
        Limits {
            per_ip_attempts: 3,
            global_attempts: 10,
            session_max: Duration::from_secs(3600),
            max_sessions: 4,
        },
        Code::generate().expect("a sign-in code"),
        Now::current(),
    )));
    let store = Arc::new(std::sync::Mutex::new(
        Store::open(dir.path()).expect("the state database"),
    ));
    let registry = Arc::new(Registry::new());
    let terminals = wiring::register_terminal(&registry, dir.path().to_path_buf())
        .expect("the terminal commands register");

    let policy = lockdown::Policy {
        grace: Duration::from_millis(10),
        grace_checks: 2,
        auto,
        ..lockdown::Policy::default()
    };
    let lockdown = Arc::new(
        Lockdown::restore(
            lockdown::Deps {
                clock: Box::new(lockdown::OsClock),
                random: Box::new(lockdown::OsRandom),
                sessions: Box::new(wiring::AuthSessions::new(
                    state,
                    store,
                    terminals,
                    "auto-node".to_string(),
                )),
                children: Box::new(lockdown::NoChildren),
                signals: Box::new(lockdown::OsSignals::new()),
                outbound: Box::new(lockdown::NoOutbound),
                passkeys: Box::new(lockdown::PasskeysUnavailable),
                journal: Box::new(lockdown::FileJournal::new(dir.path())),
            },
            policy,
            "auto-node".to_string(),
        )
        .expect("a fresh journal"),
    );
    let watchdog = Arc::new(Watchdog::new(lockdown.policy().auto));
    (lockdown, watchdog, dir)
}

#[tokio::test]
async fn a_burst_of_refused_agent_commands_reaches_the_kill_switch_through_the_journal() {
    use deckjack_core::command::{Decision, DispatchId, Issuer, Journal, LogEntry, NodeId};

    let (lockdown, watchdog, _dir) = with_auto_policy(lockdown::AutoPolicy {
        refused_agent_commands: lockdown::Burst {
            count: 2,
            within: Duration::from_secs(60),
            response: lockdown::Response::Engage,
        },
        ..lockdown::AutoPolicy::default()
    });

    // The journal is the hook because it is the one place every decision
    // already passes through. `TracingJournal` stands in for the store's,
    // which is not what this is about.
    let journal = wiring::WatchJournal::new(
        Arc::new(deckjack_core::command::TracingJournal),
        Arc::clone(&watchdog),
        Arc::clone(&lockdown),
    );

    let entry = |command: &str, issuer: Issuer, decision: Decision| LogEntry {
        dispatch: DispatchId::generate(),
        at: std::time::SystemTime::now(),
        node: NodeId::new("auto-node"),
        command: deckjack_core::command::CommandId::new(command.to_string()).unwrap(),
        risk: None,
        issuer,
        decision,
        params: None,
        detail: None,
    };

    // The operator's own refusals do not count. A burst of them is a confused
    // UI, not an agent probing the gate, and counting them would let the
    // shell lock the workspace down by misbehaving.
    for _ in 0..5 {
        journal
            .record(entry("files.delete", Issuer::operator(), Decision::Refused))
            .unwrap();
    }
    // Nor does an agent command that ran.
    for _ in 0..5 {
        journal
            .record(entry("files.read", Issuer::agent("helper"), Decision::Ran))
            .unwrap();
    }
    assert!(!lockdown.is_locked(), "the wrong entries were counted");

    journal
        .record(entry(
            "files.delete",
            Issuer::agent("helper"),
            Decision::Refused,
        ))
        .unwrap();
    assert!(!lockdown.is_locked(), "one refusal is not a burst");

    journal
        .record(entry(
            "files.delete",
            Issuer::agent("helper"),
            Decision::Refused,
        ))
        .unwrap();

    // `engage` blocks for the grace period, so `act_on` runs it on a thread.
    // Waiting on the flag rather than on a duration: the deadline is a
    // failure deadline, not pacing.
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while !lockdown.is_locked() && std::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert!(
        lockdown.is_locked(),
        "a burst of refused agent commands did not reach the kill switch"
    );
    let status = lockdown.status();
    assert_eq!(
        status.engaged.expect("an engagement").trigger,
        lockdown::TriggerKind::RefusedAgentCommands
    );
}

/// A locked node must not manufacture its own trigger burst.
///
/// The lockdown now holds agents on the bus, so every agent dispatch during
/// one is refused with `AgentsPaused` — and a refused agent command is
/// exactly what the watchdog counts. Counting them would make the kill switch
/// working into an incident: harmless under the shipped `Response::Alert`,
/// but under `Response::Engage` a thread spawned moments before an unlock
/// re-locks the node straight after it, burning a recovery code for nothing.
///
/// Asserted on the watchdog's own window rather than on whether a re-engage
/// happened, because `act_on` runs `engage` on a thread and this file does
/// not wait on durations to decide anything. `observe` is synchronous and
/// returns the verdict, so "was that counted" is one call with one answer:
/// the burst threshold is 3 and exactly 2 refusals are fed while locked, so
/// a window that counted them answers `Engage` to the next observation and an
/// empty one answers `Nothing`.
#[tokio::test]
async fn refusals_while_already_locked_are_not_counted_toward_the_kill_switch() {
    use deckjack_core::command::{Decision, DispatchId, Issuer, Journal, LogEntry, NodeId};

    let (lockdown, watchdog, _dir) = with_auto_policy(lockdown::AutoPolicy {
        refused_agent_commands: lockdown::Burst {
            count: 3,
            within: Duration::from_secs(60),
            response: lockdown::Response::Engage,
        },
        ..lockdown::AutoPolicy::default()
    });
    // Minted first: `issue_recovery_codes` refuses from the locked state, and
    // this test has to be able to leave the lockdown it engages.
    let code = lockdown
        .issue_recovery_codes(1)
        .expect("codes issue while unlocked")[0]
        .reveal();

    let journal = wiring::WatchJournal::new(
        Arc::new(deckjack_core::command::TracingJournal),
        Arc::clone(&watchdog),
        Arc::clone(&lockdown),
    );

    // An app's refusal, as the bus records one once the lockdown is holding
    // agents: `Issuer::Agent` on the reserved `app:` session.
    let refusal = || LogEntry {
        dispatch: DispatchId::generate(),
        at: std::time::SystemTime::now(),
        node: NodeId::new("auto-node"),
        command: deckjack_core::command::CommandId::new("files.delete".to_string()).unwrap(),
        risk: None,
        issuer: Issuer::agent("app:notes"),
        decision: Decision::Refused,
        params: None,
        detail: Some("agents are paused; resume them from the menu".to_string()),
    };

    lockdown.engage(lockdown::Trigger::HostCommand);
    // One short of the threshold, so nothing fires either way and the only
    // difference between counted and not is what the window holds.
    for _ in 0..2 {
        journal.record(refusal()).unwrap();
    }

    assert!(
        matches!(
            watchdog.observe(
                lockdown::Observation::RefusedAgentCommand {
                    command: "files.delete".to_string(),
                },
                std::time::SystemTime::now(),
            ),
            lockdown::Verdict::Nothing
        ),
        "a locked node counted the refusals its own lockdown caused, so it is \
         one observation away from re-engaging on them"
    );

    // Not counting while locked must not mean never counting again. After the
    // unlock the window arms exactly as the burst test pins it: the manual
    // observation above left one in the window, so the third refusal fires.
    lockdown
        .unlock(lockdown::UnlockRequest {
            identity: lockdown::Identity::HostConsole,
            second: lockdown::SecondFactor::RecoveryCode(code),
        })
        .expect("the host console plus a recovery code unlocks");
    assert!(!lockdown.is_locked());

    for _ in 0..2 {
        journal.record(refusal()).unwrap();
    }

    // `engage` blocks for the grace period, so `act_on` runs it on a thread.
    // Waiting on the flag rather than on a duration: the deadline is a
    // failure deadline, not pacing.
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while !lockdown.is_locked() && std::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert!(
        lockdown.is_locked(),
        "the watchdog stopped counting refusals for good after one lockdown"
    );
}

#[tokio::test]
async fn a_sign_in_from_an_unknown_address_can_engage_and_a_known_one_cannot() {
    let (lockdown, watchdog, _dir) = with_auto_policy(lockdown::AutoPolicy {
        new_device: lockdown::Response::Engage,
        ..lockdown::AutoPolicy::default()
    });
    let ip: std::net::IpAddr = "100.69.236.100".parse().unwrap();
    let now = std::time::SystemTime::now();

    // A device that is known is not an incident, whatever the policy says.
    wiring::act_on(
        &lockdown,
        watchdog.observe(
            lockdown::Observation::SignIn {
                ip,
                known_device: true,
            },
            now,
        ),
    );
    assert!(!lockdown.is_locked(), "a known device locked the node down");

    wiring::act_on(
        &lockdown,
        watchdog.observe(
            lockdown::Observation::SignIn {
                ip,
                known_device: false,
            },
            now,
        ),
    );
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while !lockdown.is_locked() && std::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert!(lockdown.is_locked(), "an unknown device did not engage");

    // The address is in the log line and in the audit trail; the journal,
    // which is written during an incident and read by whoever then holds the
    // machine, does not keep it.
    let reason = lockdown.status().engaged.expect("an engagement").reason;
    assert!(
        !reason.contains("100.69.236.100"),
        "the journal kept the address: {reason}"
    );
}

#[tokio::test]
async fn signing_in_records_the_address_so_the_second_time_is_not_a_new_device() {
    let h = start().await;
    let known = || {
        h.store
            .lock()
            .unwrap()
            .is_known_address("127.0.0.1".parse().unwrap())
            .unwrap()
    };
    assert!(!known(), "a fresh node should not know any address yet");
    h.sign_in().await;
    assert!(
        known(),
        "the sign-in was not recorded, so every sign-in would look like a new device"
    );
}

#[tokio::test]
async fn the_passkey_routes_are_mounted_on_the_real_front_door() {
    let h = start().await;
    let r = h.post("/passkey/sign-in/begin", &[], "").await;
    assert_ne!(
        r.status, 404,
        "the passkey routes are not mounted: {}",
        r.body
    );
}
