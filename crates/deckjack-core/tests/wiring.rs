//! What the assembly owes, asked at the seams it assembles.
//!
//! Every other module in this crate can be tested with its world injected,
//! and `wiring.rs` is the one that cannot: its whole job is to introduce
//! modules to each other, so there is nothing left to inject. That is also
//! why it exists. This project's recurring defect has been a subsystem built
//! complete, unit-tested in isolation and then never constructed — the kill
//! switch that passed every rule in `tests/lockdown.rs` while nothing in the
//! program could fire it is the loudest example — and `wiring.rs` is where
//! that class of defect is supposed to become impossible.
//!
//! Its structural answer is required struct fields: omit one of `BusParts`
//! and the build fails. What a compile error cannot catch is the rest:
//! whether a `register_*` call was made at all, whether a route was merged
//! above the gate or below it, whether the hold the bus was handed is read by
//! everything that offers an agent a tool, and whether a layer that must sit
//! outside every route really does.
//!
//! So nothing here reads a field or counts a call. Each test asks the
//! question through the surface the operator (or their phone, or an agent)
//! actually touches: an HTTP request off a real socket, a frame on the bus
//! socket, a palette listing, a tool list, an audit row. A rewrite of
//! `wiring.rs` that kept the promises keeps these green; one that forgets a
//! registration or moves a `.merge` below a `.layer` does not.
//!
//! # What is borrowed from `tests/end_to_end.rs`
//!
//! The harness is that file's: the same objects in the same order through
//! `wiring::bus` and `wiring::app`, a real `TcpListener`, and a hand-written
//! HTTP and WebSocket client because the crate has no `tower` and no
//! `tokio-tungstenite` and a test file may not add a dependency. Integration
//! tests are separate binaries with nothing to share, so the client is
//! copied rather than invented: a second style of client here would be a
//! second thing to trust.
//!
//! Two deviations from that harness, both deliberate:
//!
//!  - **Grants are a `MemoryGrants`** rather than the store's. The product's
//!    grant store starts empty, so an agent would be offered nothing for want
//!    of a grant and the tool-list test would pass while reading nothing. The
//!    promise under test is that the bus's hold reaches both tool lists, not
//!    where grants are kept — and the one wiring rule that matters here is
//!    kept: the bus and the bridge are handed the *same* `Arc`.
//!  - **Nothing waits on a duration to decide anything.** The deadlines in
//!    this file are failure deadlines. The lockdown's grace period is ten
//!    milliseconds, and the one test that could hang forever — a re-entrant
//!    lock on the store — runs the call on a thread and fails it rather than
//!    taking the suite down with it.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use deckjack_core::agentbridge::AgentBridge;
use deckjack_core::apphost::AppHost;
use deckjack_core::apps;
use deckjack_core::auth::Code;
use deckjack_core::auth::session::{SessionHash, SessionId};
use deckjack_core::auth::state::{AuthState, Limits, Now};
use deckjack_core::command::{
    AgentSessionId, Bus, Grant, Grants, MemoryGrants, NodeScope, Pattern, Registry, RiskLevel,
};
use deckjack_core::http::Ctx;
use deckjack_core::lockdown::{self, HostToken, Lockdown, Watchdog};
use deckjack_core::store::Store;
use deckjack_core::wiring::{self, BusDispatch};
use deckjack_core::{node, ws};

const NODE: &str = "wiring-node";

/// An agent session with a grant, shaped the way `agentbridge` shapes an
/// app's: the reserved `app:<name>` identity.
const AGENT: &str = "app:notes";

// ---------------------------------------------------------------------------
// The node, assembled through `wiring::bus` and `wiring::app`
// ---------------------------------------------------------------------------

struct Node {
    addr: SocketAddr,
    /// The one-time code, as the banner prints it on the host.
    code: String,
    /// The product's own bus, from `wiring::bus` — not a hand-copied builder
    /// chain, which is how a bus with no grant storage shipped green.
    bus: Arc<Bus>,
    /// The live tool-list path, over the same bus and the same grants.
    bridge: Arc<AgentBridge>,
    lockdown: Arc<Lockdown>,
    /// Keeps the working directory alive for as long as the server runs.
    _dir: tempfile::TempDir,
}

async fn start() -> Node {
    let dir = tempfile::tempdir().expect("a working directory");
    let node_id = NODE.to_string();

    let code = Code::generate().expect("a sign-in code");
    let code_text = code.reveal();

    let state = Arc::new(std::sync::Mutex::new(AuthState::new(
        Limits {
            per_ip_attempts: 3,
            global_attempts: 10,
            session_max: Duration::from_secs(3600),
            max_sessions: 4,
        },
        code,
        Now::current(),
    )));
    let store = Arc::new(std::sync::Mutex::new(
        Store::open(dir.path()).expect("the state database"),
    ));

    let registry = Arc::new(Registry::new());
    let terminals = wiring::register_terminal(&registry, dir.path().to_path_buf())
        .expect("the terminal commands register");
    let windows =
        wiring::register_windows(&registry, node_id.clone()).expect("the window commands register");

    let approvals = deckjack_core::approvals::Approvals::open(
        Arc::clone(&store),
        deckjack_core::command::RiskLevel::Destructive,
    );
    // The real kill switch over the real auth state, store and terminal
    // registry — `wiring::AuthSessions` and `wiring::TerminalChildren`, which
    // are the implementations `lockdown`'s traits went without for as long as
    // the kill switch could not fire.
    let lockdown = Arc::new(
        Lockdown::restore(
            lockdown::Deps {
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
            },
            lockdown::Policy {
                grace: Duration::from_millis(10),
                grace_checks: 2,
                ..lockdown::Policy::default()
            },
            node_id.clone(),
        )
        .expect("a fresh journal"),
    );
    approvals.follow_lockdown(Arc::downgrade(&lockdown));
    let watchdog = Arc::new(Watchdog::new(lockdown.policy().auto));
    wiring::register_lockdown(&registry, Arc::clone(&lockdown))
        .expect("the lockdown commands register");

    let audit = deckjack_core::audit::EventsJournal::new(Arc::clone(&store));

    // One grant source, handed to the bus and to the bridge. Two sources is
    // how a tool list and a dispatch come to tell one caller different
    // things, which is the thing `BusParts::grants` documents.
    let grants = Arc::new(MemoryGrants::new());
    grants.put(
        AgentSessionId::new(AGENT),
        Grant::new(
            vec![Pattern::new("terminal.*").expect("a valid pattern")],
            NodeScope::ThisNode,
        ),
    );

    let bus = wiring::bus(wiring::BusParts {
        registry: Arc::clone(&registry),
        node_id: node_id.clone(),
        journal: audit.clone(),
        watchdog,
        lockdown: Arc::clone(&lockdown),
        grants: Arc::clone(&grants) as Arc<dyn Grants>,
        approvals: approvals.inbox(),
        halt: Arc::new(wiring::LockdownHalt(Arc::clone(&lockdown))),
    });

    let reaper = Arc::clone(&terminals);
    let ctx = Arc::new(
        // Zero failure delay: the real delay is a cost imposed on someone
        // guessing codes over a network, and nothing here asserts on it.
        Ctx::new(
            Arc::clone(&state),
            Arc::clone(&store),
            Duration::ZERO,
            Vec::new(),
            node_id.clone(),
        )
        .on_session_end(move |hash| reaper.close_for_session(hash)),
    );

    // Over an install directory with nothing in it. Discovery finding no apps
    // is the ordinary state of a fresh install; what matters is that the UI
    // mount and the app commands are really present.
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
    let bridge = AgentBridge::new(Arc::clone(&bus), Arc::clone(&grants) as Arc<dyn Grants>);
    app_host.attach_bridge(Arc::clone(&bridge));
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
                "127.0.0.1:7777".parse().expect("a socket address"),
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
        lockdown::UnlockCtx::new(
            Arc::clone(&lockdown),
            HostToken::load_or_create(dir.path(), &lockdown::OsRandom)
                .expect("the host unlock token"),
        ),
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

    Node {
        addr,
        code: code_text,
        bus,
        bridge,
        lockdown,
        _dir: dir,
    }
}

impl Node {
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
        let raw = r.header("set-cookie").expect("a Set-Cookie header");
        raw.split(';').next().expect("a cookie value").to_string()
    }

    /// A signed-in bus socket, past the protocol handshake.
    async fn socket(&self) -> Ws {
        let cookie = self.sign_in().await;
        let mut ws = Ws::connect(self.addr, Some(&cookie))
            .await
            .expect("a signed-in browser should get an upgraded socket");
        let welcome = ws.hello().await;
        assert_eq!(welcome["type"], "welcome", "got {welcome}");
        ws
    }

    fn engage(&self) {
        self.lockdown.engage(lockdown::Trigger::Operator {
            surface: lockdown::Surface::Palette,
        });
        assert!(self.lockdown.is_locked(), "the lockdown did not engage");
    }

    /// Every command id the palette would show the operator.
    fn palette(&self) -> Vec<String> {
        self.bus
            .list(None)
            .into_iter()
            .map(|listing| listing.spec.id().as_str().to_string())
            .collect()
    }
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

// ---------------------------------------------------------------------------
// A hand-written WebSocket client
// ---------------------------------------------------------------------------

struct Ws {
    stream: TcpStream,
}

impl Ws {
    /// Open the bus socket. `Err` carries the HTTP status when the server
    /// refused before upgrading.
    async fn connect(addr: SocketAddr, cookie: Option<&str>) -> Result<Ws, u16> {
        let mut req = format!(
            "GET {} HTTP/1.1\r\nHost: {addr}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\
             Sec-WebSocket-Version: 13\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n",
            ws::PATH
        );
        if let Some(c) = cookie {
            req.push_str(&format!("Cookie: {c}\r\n"));
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
        let status: u16 = String::from_utf8_lossy(&head)
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
        assert!(payload.len() < 126, "test frames stay small");
        frame.push(0x80 | payload.len() as u8);
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
        // Longer than a small frame's length field allows, which this client
        // does not implement: the two-byte form is enough for every frame
        // here and a silent truncation would be worse than a failure.
        const MASK: [u8; 4] = [0x37, 0xfa, 0x21, 0x3d];
        let payload = text.as_bytes();
        if payload.len() < 126 {
            self.send_frame(0x1, payload).await;
            return;
        }
        let mut frame = vec![0x80 | 0x1u8, 0x80 | 126];
        assert!(
            payload.len() <= u16::MAX as usize,
            "test frames stay under 64KiB"
        );
        frame.extend_from_slice(&(payload.len() as u16).to_be_bytes());
        frame.extend_from_slice(&MASK);
        frame.extend(
            payload
                .iter()
                .enumerate()
                .map(|(i, b)| b ^ MASK[i % MASK.len()]),
        );
        self.stream.write_all(&frame).await.expect("send a frame");
    }

    async fn read_exact(&mut self, n: usize) -> Vec<u8> {
        let mut buf = vec![0u8; n];
        self.stream
            .read_exact(&mut buf)
            .await
            .expect("read frame bytes");
        buf
    }

    /// One text frame. Pings are answered on the way through, so a test that
    /// is between assertions does not look dead to the keepalive.
    async fn expect_json(&mut self) -> Value {
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
                0x1 => {
                    return serde_json::from_str(&String::from_utf8_lossy(&payload))
                        .expect("a JSON frame");
                }
                0x8 => panic!(
                    "the bus closed instead of answering: {}",
                    String::from_utf8_lossy(payload.get(2..).unwrap_or(&[]))
                ),
                0x9 => self.send_frame(0xa, &payload).await,
                0xa => {}
                other => panic!("unexpected websocket opcode {other}"),
            }
        }
    }

    async fn hello(&mut self) -> Value {
        self.send_text(&json!({ "type": "hello", "protocol": ws::PROTOCOL_VERSION }).to_string())
            .await;
        self.expect_json().await
    }

    /// Invoke a command and wait for the response for that correlation id,
    /// stepping over progress and server-pushed events on the way.
    async fn invoke(&mut self, id: &str, command: &str, params: Value, confirmed: bool) -> Value {
        self.send_text(
            &json!({
                "type": "invoke",
                "id": id,
                "command": command,
                "params": params,
                "confirmed": confirmed,
            })
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
// Nothing is built and left unconstructed
// ---------------------------------------------------------------------------

/// Every `register_*` the wiring offers was actually called, and what it
/// registered answers on the socket the shell talks to.
///
/// The palette listing and a dispatch, not a field read: a registration that
/// happened is a command the operator can see and run, and that is the only
/// form of "constructed" worth asserting. Each id below comes from a
/// different `register_*` function, so dropping any one of them — the exact
/// defect this module exists to prevent — fails here rather than shipping as
/// a button that does nothing.
#[tokio::test]
async fn every_command_the_wiring_registers_is_on_the_bus_and_answers_over_the_socket() {
    let h = start().await;
    let palette = h.palette();

    for id in [
        // register_terminal
        "terminal.open",
        "terminal.read",
        "terminal.write",
        "terminal.resize",
        "terminal.close",
        "terminal.list",
        // register_windows
        "shell.window.open",
        "shell.window.close",
        "shell.window.minimize",
        "shell.window.restore",
        "shell.window.maximize",
        "shell.window.place",
        "shell.window.snap",
        "shell.windows.tile",
        "shell.split.set",
        "shell.split.clear",
        // register_lockdown
        lockdown::CMD_ENGAGE,
        "system.status",
        "system.recovery_codes",
        // register_apps
        "system.apps",
        "system.app.approve",
        "system.app.revoke",
    ] {
        assert!(
            palette.iter().any(|known| known == id),
            "{id} is implemented and nothing registered it, so the palette cannot \
             offer it; the palette has {palette:?}"
        );
    }

    let mut ws = h.socket().await;

    // The app host, reached through the command that reads it. An empty list
    // is the right answer for an install directory with nothing in it — what
    // would be wrong is an error, which is what an unconstructed host gives.
    let apps = ws.invoke("apps", "system.apps", json!({}), false).await;
    let value = ok_value(&apps);
    assert_eq!(
        value["apps"],
        json!([]),
        "system.apps should report an empty install, not fail: {value}"
    );

    // The window set, reached through the command that opens one. `app_status`
    // is null because `shell.terminal` is a window with no binary behind it,
    // which is the ordinary case the app host is asked about and declines.
    let opened = ws
        .invoke(
            "win",
            "shell.window.open",
            json!({ "id": "client-1", "app": "shell.terminal" }),
            false,
        )
        .await;
    let value = ok_value(&opened);
    assert_eq!(value["id"], "client-1", "got {value}");
    assert!(
        value["server_id"].is_u64(),
        "the server has to mint its own id for the window it is now holding: {value}"
    );
    assert_eq!(
        value["app_status"],
        Value::Null,
        "a window with no app behind it must not claim an app status: {value}"
    );

    // And closing it resolves through the same id bridge, which is the part
    // of `Windows` that has no meaning until both commands are registered.
    let closed = ws
        .invoke(
            "close",
            "shell.window.close",
            json!({ "id": "client-1" }),
            false,
        )
        .await;
    assert_eq!(ok_value(&closed), &Value::Null, "got {closed}");
}

// ---------------------------------------------------------------------------
// The pre-auth carve-out, at the composed router
// ---------------------------------------------------------------------------

/// A phone can install this node without signing in first, and that is the
/// only thing it can read.
///
/// `http.rs` pins the predicate; this pins the composed router, which is the
/// surface iOS actually fetches. The two are not the same claim: the carve-out
/// lives on a fallback that `wiring::app` merges under several other routers
/// and then wraps in a layer, and any of that could shadow it. The second
/// list is the half that matters — if this test is ever made to pass by
/// widening the carve-out, the shell build becomes readable with no session
/// at all.
#[tokio::test]
async fn the_install_assets_are_readable_before_sign_in_and_the_shell_build_is_not() {
    let h = start().await;

    for path in [
        "/manifest.webmanifest",
        "/icons/deckjack-180.png",
        "/splash/splash-1125x2436.png",
    ] {
        let r = h.get(path, &[]).await;
        assert_eq!(
            r.status, 200,
            "{path} is fetched at the moment 'Add to Home Screen' is pressed, when \
             there is no session; a redirect there installs a screenshot icon with \
             no standalone mode and nothing errors"
        );
        assert!(!r.body.is_empty(), "{path} was answered with an empty body");
    }

    // Signed in first, so each path below is known to exist: otherwise a
    // redirect could just as well mean "no such file" and this would prove
    // nothing.
    let cookie = h.sign_in().await;
    for path in ["/", "/sw.js", "/kit/bus.js"] {
        let served = h.get(path, &[("cookie", cookie.as_str())]).await;
        assert_eq!(served.status, 200, "{path} should be part of the build");

        let refused = h.get(path, &[]).await;
        assert_eq!(
            refused.status, 303,
            "{path} is readable without a session; the build is not public"
        );
        assert_eq!(refused.header("location"), Some("/login"));
        assert!(
            refused.body.is_empty(),
            "a redirect must not carry what it refused to serve: {path}"
        );
    }

    // The carve-out is inside the gate, not outside it: a locked node hands
    // out nothing, branding included.
    h.engage();
    let r = h.get("/manifest.webmanifest", &[]).await;
    assert_eq!(
        r.status, 503,
        "a locked node served an install asset: the lockdown gate has to sit \
         outside the carve-out, not beside it"
    );
}

// ---------------------------------------------------------------------------
// Above the gate, and behind it
// ---------------------------------------------------------------------------

/// A locked node offers no way to sign in, passkeys included.
///
/// This is the whole reason the ceremony is merged inside `wiring::app`
/// rather than by the caller afterwards: `axum`'s `.layer()` applies only to
/// the routes already added, so a `.merge` after `app` returns would leave
/// the passkey routes answering normally while the node is locked down. A
/// working sign-in next to `/unlock` is a hole in the kill switch, and
/// nothing about the passkey module itself would show it.
#[tokio::test]
async fn the_passkey_sign_in_routes_are_mounted_above_the_lockdown_gate() {
    let h = start().await;

    // Mounted at all — otherwise "refused while locked" would be true of a
    // path that is refused always, and the test would be vacuous.
    for (method, path) in [
        ("POST", "/passkey/sign-in/begin"),
        ("GET", "/passkey"),
        ("GET", "/passkey/ceremony.js"),
    ] {
        let r = match method {
            "POST" => h.post(path, &[], "").await,
            _ => h.get(path, &[]).await,
        };
        assert_ne!(r.status, 404, "{path} is not mounted: {}", r.body);
    }

    h.engage();

    for (method, path) in [
        ("POST", "/passkey/sign-in/begin"),
        ("POST", "/passkey/sign-in/finish"),
        ("POST", "/passkey/step-up/begin"),
        ("GET", "/passkey"),
        ("GET", "/passkey/ceremony.js"),
        ("GET", "/login"),
    ] {
        let r = match method {
            "POST" => h.post(path, &[], "").await,
            _ => h.get(path, &[]).await,
        };
        assert_eq!(
            r.status, 503,
            "{path} answered while the node was locked down"
        );
        assert!(
            r.body.contains("Locked down"),
            "{path} did not serve the locked page: {}",
            r.body
        );
    }
}

/// The way out keeps working when nothing else does, and reaches its own
/// handler rather than the locked page.
///
/// `/unlock` returning 503 is not enough to prove the carve-out: the gate's
/// own locked page is a 503 too. So the assertion is on the body only
/// `unlock_page` can produce — the instructions naming `deckjack unlock` —
/// which is what distinguishes "the gate let this through" from "the gate
/// answered for it".
#[tokio::test]
async fn the_unlock_route_still_answers_while_everything_else_is_locked_out() {
    let h = start().await;
    h.engage();

    let r = h.get("/unlock", &[]).await;
    assert_eq!(
        r.status, 503,
        "a locked node's /unlock is a 503: {}",
        r.body
    );
    assert!(
        r.body.contains("deckjack unlock --recovery-code"),
        "the gate answered for /unlock instead of letting it through, so the \
         only way out of a lockdown is unreachable: {}",
        r.body
    );

    // `ALLOWED_WHILE_LOCKED` is `/unlock` and `/healthz`, and the allow-list
    // is live in the composed router: `/healthz` is not answered with the
    // locked page. Nothing in this build mounts a health endpoint, so what it
    // reaches is the shell fallback, which sends an unauthenticated caller to
    // sign in — hence the assertion is that the gate did not claim it.
    let health = h.get("/healthz", &[]).await;
    assert_ne!(
        health.status, 503,
        "the gate's allow-list is not in effect in the composed router: {}",
        health.body
    );
    assert!(
        !health.body.contains("Locked down"),
        "/healthz was answered with the locked page: {}",
        health.body
    );
}

/// An app's own web build is not reachable without signing in.
///
/// Serving an app's UI from the core's origin is deliberate — it keeps
/// sign-in, CSP and theming the core's to enforce — and the entire value of
/// that depends on one `.layer(require_session)` in `wiring::app`. Without it
/// the mount answers for anybody who can reach the port. The two statuses
/// below are what separates the two worlds: a gated mount redirects, an
/// ungated one looks the app up and says it does not exist.
#[tokio::test]
async fn an_apps_own_ui_is_not_reachable_without_signing_in() {
    let h = start().await;

    let refused = h.get("/apps/notes/", &[]).await;
    assert_eq!(
        refused.status, 303,
        "the app UI mount answered without a session: {}",
        refused.body
    );
    assert_eq!(refused.header("location"), Some("/login"));
    assert!(
        !refused.body.contains("no such app asset"),
        "the request reached the app mount before the session check: {}",
        refused.body
    );

    // Signed in, the mount is really there and answers for itself: no app
    // called `notes` is installed, so it says so. That is what proves the
    // redirect above was the session gate and not an absent mount.
    let cookie = h.sign_in().await;
    let served = h.get("/apps/notes/", &[("cookie", cookie.as_str())]).await;
    assert_eq!(
        served.status, 404,
        "a signed-in request should reach the app mount: {}",
        served.body
    );
    assert!(
        served.body.contains("no such app asset"),
        "the app mount is not nested where the wiring says it is: {}",
        served.body
    );

    // And it is inside the lockdown gate like everything else.
    h.engage();
    let locked = h.get("/apps/notes/", &[("cookie", cookie.as_str())]).await;
    assert_eq!(locked.status, 503, "{}", locked.body);
}

// ---------------------------------------------------------------------------
// The bus answer the shell has to be able to show
// ---------------------------------------------------------------------------

/// A command that needs confirmation comes back as something to show the
/// operator, not as a failure — and the operator's answer is carried back.
///
/// Both halves are `BusDispatch`'s and only `BusDispatch`'s. The bus says
/// `Admitted::NeedsConfirmation` with a prompt; whether that becomes an
/// answer the shell can render or an error a toast swallows is decided here,
/// as is whether the client's `confirmed` flag ever reaches the bus. A
/// confirmation that cannot be answered is a command the operator can never
/// run, and it would look exactly like a backend bug.
#[tokio::test]
async fn a_destructive_command_asks_the_operator_first_and_runs_once_confirmed() {
    let h = start().await;
    let before = h.lockdown.recovery_codes_left();
    let mut ws = h.socket().await;

    let asked = ws
        .invoke("codes", "system.recovery_codes", json!({}), false)
        .await;
    let value = ok_value(&asked);
    assert_eq!(
        value["status"], "needs_confirmation",
        "a destructive command must come back as a question, not an error: {value}"
    );
    assert_eq!(
        value["risk"],
        RiskLevel::Destructive.as_str(),
        "got {value}"
    );
    let prompt = value["prompt"].as_str().expect("a prompt to show");
    assert!(
        prompt.contains("Issue new recovery codes"),
        "the prompt is the bus's so the dialog shows that text rather than \
         inventing its own: {prompt}"
    );
    assert_eq!(
        h.lockdown.recovery_codes_left(),
        before,
        "the command ran while it was still being asked about; issuing replaces \
         the codes in the operator's wallet"
    );

    // The same call with the dialog answered. If `confirmed` is not carried
    // through, this asks again forever.
    let ran = ws
        .invoke("codes2", "system.recovery_codes", json!({}), true)
        .await;
    let value = ok_value(&ran);
    let codes = value["codes"]
        .as_array()
        .unwrap_or_else(|| panic!("a confirmed call should issue codes: {value}"));
    assert_eq!(codes.len(), lockdown::RECOVERY_CODES_ISSUED, "got {value}");
    assert_eq!(
        h.lockdown.recovery_codes_left(),
        lockdown::RECOVERY_CODES_ISSUED,
        "the codes the operator was just shown are not the ones the node will accept"
    );
}

// ---------------------------------------------------------------------------
// The hold, read by everything that offers a tool
// ---------------------------------------------------------------------------

/// While the node holds agents, both tool lists go empty and the dispatch
/// agrees with them.
///
/// `LockdownHalt` is the only thing that makes a lockdown visible to the bus:
/// `lockdown::gate` is axum middleware, so it sees HTTP and never the Unix
/// socket an app invokes over. The rule the modules state is that an offered
/// tool is never a tool that would be refused — offering and then refusing
/// teaches a model the boundary is negotiable — so the list and the refusal
/// have to be read off the same hold.
///
/// Both readers, because they look interchangeable and are not:
/// `AgentBridge::tools` is the live path, and `Bus::tools` is the public
/// lower-level twin the next caller is likely to reach for.
#[tokio::test]
async fn holding_agents_empties_both_tool_lists_and_the_dispatch_that_follows() {
    use deckjack_core::command::{CommandId, Issuer, Refused, Request};

    let h = start().await;
    let session = AgentSessionId::new(AGENT);
    let probe = || {
        Request::new(
            CommandId::new("terminal.list".to_string()).expect("a valid command id"),
            json!({}),
            Issuer::agent(AGENT),
        )
    };

    // Unlocked, the grant is real and the surface is not empty — otherwise
    // everything below would be true of a session that was never offered
    // anything.
    assert!(
        !h.bridge.tools(&session).tools.is_empty(),
        "the agent's grant offers nothing, so this test cannot tell a hold from \
         a missing grant"
    );
    assert!(!h.bus.tools(&session).is_empty(), "the bus offers nothing");
    assert!(
        h.bus.dispatch(probe()).is_ok(),
        "an unlocked node should admit a granted read-only command"
    );

    h.engage();

    assert!(
        h.bridge.tools(&session).tools.is_empty(),
        "a held agent was still offered tools by the live list path: every call \
         it makes from that list is about to be refused"
    );
    assert!(
        h.bus.tools(&session).is_empty(),
        "the bus's own tool list ignores the hold the bus refuses by, so the \
         first caller to pick it serves a full surface during a lockdown"
    );
    assert!(
        matches!(
            h.bus.dispatch(probe()).expect_err("locked"),
            Refused::AgentsPaused
        ),
        "the lockdown did not reach the bus at all: an app's dispatches keep \
         running while sessions are revoked and children are killed"
    );
}

// ---------------------------------------------------------------------------
// Step 1, over the real auth state and the real store
// ---------------------------------------------------------------------------

/// Lockdown's step 1 over the production implementation, with no HTTP in the
/// way.
///
/// Sessions are seated through `AuthState::admit`, which is the passkey
/// sign-in path, because the one-time code is consumed by the first login and
/// these tests need more than one session: a kill switch that writes one row
/// for two sessions is the defect worth catching, and a single-session test
/// cannot see it.
struct Revoking {
    sessions: Arc<wiring::AuthSessions>,
    auth: Arc<std::sync::Mutex<AuthState>>,
    store: Arc<std::sync::Mutex<Store>>,
    _dir: tempfile::TempDir,
}

fn revoking() -> Revoking {
    let dir = tempfile::tempdir().expect("a working directory");
    let auth = Arc::new(std::sync::Mutex::new(AuthState::new(
        Limits {
            per_ip_attempts: 3,
            global_attempts: 10,
            session_max: Duration::from_secs(3600),
            max_sessions: 8,
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

    Revoking {
        sessions: wiring::AuthSessions::new(
            Arc::clone(&auth),
            Arc::clone(&store),
            terminals,
            NODE.to_string(),
        ),
        auth,
        store,
        _dir: dir,
    }
}

impl Revoking {
    /// Seat a signed-in session, in memory and on disk, as the sign-in paths
    /// leave it.
    fn seat(&self, ip: &str) -> SessionHash {
        let id = SessionId::generate().expect("a session id");
        let mut events = Vec::new();
        let session = self
            .auth
            .lock()
            .expect("the auth state")
            .admit(
                ip.parse().expect("an address"),
                &id,
                Now::current(),
                &mut events,
            )
            .expect("a fresh node admits a session");
        self.store
            .lock()
            .expect("the store")
            .insert_session(&id.hash(), &session)
            .expect("the session persists");
        id.hash()
    }

    /// `revoke_all` on a thread, so a re-entrant lock on the store fails this
    /// test instead of hanging the suite. Returns how many sessions went.
    fn revoke_all_before(&self, deadline: Duration) -> usize {
        let sessions = Arc::clone(&self.sessions);
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(deckjack_core::lockdown::Sessions::revoke_all(&sessions));
        });
        rx.recv_timeout(deadline).unwrap_or_else(|_| {
            panic!(
                "revoke_all did not finish within {deadline:?}: the audit rows are \
                 written through a sink that locks the same store mutex step 1 \
                 holds, so writing them inside that guard deadlocks on a \
                 re-entrant lock rather than failing"
            )
        })
    }

    fn revocation_rows(&self) -> Vec<deckjack_core::events::Entry> {
        deckjack_core::audit::StoreSink::new(Arc::clone(&self.store))
            .timeline(&deckjack_core::events::Query::default())
            .expect("the timeline reads")
            .into_iter()
            .map(|row| row.entry)
            .filter(|entry| entry.command == "auth.session.revoked")
            .collect()
    }
}

/// The kill switch leaves a trace of whose sessions it took — one per session.
///
/// A lockdown revoking sessions in silence was the one thing the node did to
/// its user that it wrote nowhere, which is the single event most worth being
/// able to reconstruct afterwards. One row per session, not one per lockdown:
/// the rows are what say how many devices were signed in and which, and a
/// loop that wrote once outside it would read as a single revocation forever.
#[tokio::test]
async fn the_kill_switch_leaves_one_audit_row_for_every_session_it_took() {
    let r = revoking();
    let first = r.seat("127.0.0.1");
    let second = r.seat("100.69.236.100");
    assert_ne!(first.tag(), second.tag(), "two sessions, two tags");

    let revoked = r.revoke_all_before(Duration::from_secs(20));
    assert_eq!(revoked, 2, "step 1 did not report both sessions");

    let rows = r.revocation_rows();
    assert_eq!(
        rows.len(),
        2,
        "two sessions were revoked and the audit log has {} row(s) for it",
        rows.len()
    );
    for row in &rows {
        assert_eq!(row.target_node, NODE, "the row names the wrong node");
        assert!(
            row.params.as_str().contains("lockdown"),
            "the row has to say the lockdown took it, or a lockdown and a lockout \
             are indistinguishable afterwards: {}",
            row.params
        );
    }
    // Each session named once, by the tag `ParamsSummary` is allowed to keep.
    for tag in [first.tag(), second.tag()] {
        assert_eq!(
            rows.iter()
                .filter(|row| row.params.as_str().contains(&tag))
                .count(),
            1,
            "the log does not name the session {tag} exactly once: {:?}",
            rows.iter().map(|r| r.params.as_str()).collect::<Vec<_>>()
        );
    }

    // And the persisted rows are gone: a count-only revocation leaves them,
    // and the next start restores every session the lockdown just revoked.
    assert!(
        r.store
            .lock()
            .expect("the store")
            .live_sessions(std::time::SystemTime::now())
            .expect("the session rows read")
            .is_empty(),
        "a revoked session is still in the state database and would come back"
    );
}

/// Step 1 finishes. The ordering inside it is load-bearing, not tidiness.
///
/// `AuthSessions::revoke_all` holds the store mutex to delete the session
/// rows, and then writes the audit rows through `StoreSink`, which locks that
/// same mutex. The second must happen after the guard is dropped: a
/// `std::sync::Mutex` is not re-entrant, so writing the log inside the guard
/// does not fail — it deadlocks, with the node half locked down, sessions
/// dropped from memory and the shells still running.
///
/// Asserted as completion within a deadline rather than by inspecting the
/// order, because the order is implementation and the promise is that the
/// kill switch returns.
#[tokio::test]
async fn revoking_every_session_finishes_rather_than_deadlocking_on_its_own_store() {
    let r = revoking();
    for ip in ["127.0.0.1", "127.0.0.2", "127.0.0.3"] {
        r.seat(ip);
    }

    // Generous: this is a failure deadline, not pacing. Three sqlite writes
    // and three log appends on a loopback-only temp directory are microseconds
    // of work, so the only way to spend twenty seconds here is to be blocked
    // forever.
    assert_eq!(
        r.revoke_all_before(Duration::from_secs(20)),
        3,
        "step 1 did not report every session it dropped"
    );

    // Still usable afterwards, which is the other half of "no deadlock": a
    // lock held by a thread that gave up would make every later read hang.
    assert!(
        r.store
            .lock()
            .expect("the store")
            .live_sessions(std::time::SystemTime::now())
            .expect("the session rows read")
            .is_empty()
    );
    assert_eq!(
        r.revocation_rows().len(),
        3,
        "the audit rows were not written, so nothing records whose sessions went"
    );
}
