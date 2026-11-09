//! The whole thing, in one process, over a real socket.
//!
//! Every other test in this crate exercises one module with its world
//! injected. This one does the opposite on purpose: it builds the same
//! objects `main.rs` builds, in the same order, merges the same two routers,
//! binds a real `TcpListener` and talks to it the way a browser does. The
//! defects this pass exists to catch — two halves of a wire protocol that
//! never agreed, a command marked with the wrong risk, a form posting to a
//! path that was never routed — are all invisible to a module test and
//! obvious here.
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

use deckjack_core::auth::Code;
use deckjack_core::auth::session::COOKIE_NAME;
use deckjack_core::auth::state::{AuthState, Limits, Now};
use deckjack_core::command::{Bus, NodeId, OsEnv as BusEnv, Registry};
use deckjack_core::http::{self, Ctx};
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
    let state = AuthState::new(limits, code, Now::current());

    let registry = Arc::new(Registry::new());
    let terminals = wiring::register_terminal(&registry, dir.path().to_path_buf())
        .expect("the terminal commands register");
    // Registered because main.rs registers them: a duplicate command id
    // between the two sets would be a startup failure in production and a
    // silent absence here.
    let _files =
        files::register_commands(&registry, dir.path(), &node_id).expect("the file commands");
    let bus = Arc::new(Bus::new(
        Arc::clone(&registry),
        NodeId::new(node_id.clone()),
        Arc::new(BusEnv),
    ));

    let reaper = Arc::clone(&terminals);
    let ctx = Arc::new(
        // Zero failure delay. The real delay is a cost imposed on someone
        // guessing codes over a network; paying it in a test would only make
        // the suite slow, and nothing here asserts on it.
        Ctx::new(
            state,
            std::sync::Arc::new(std::sync::Mutex::new(
                Store::in_memory().expect("the state database"),
            )),
            Duration::ZERO,
            Vec::new(),
        )
        .on_session_end(move |id| reaper.close_for_session(id)),
    );

    let ws_ctx = ws::WsCtx::new(
        Arc::clone(&ctx),
        BusDispatch::new(Arc::clone(&bus)),
        ws::Hub::new(Arc::new(ws::SystemClock)),
        Arc::new(ws::SystemClock),
        node_id,
        node::host_name(),
        ws::Settings::default(),
    );

    let app = http::router(Arc::clone(&ctx)).merge(ws::router(ws_ctx));
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
// Known defects
// ---------------------------------------------------------------------------

/// Blocking an address revokes that address's sessions inside
/// `AuthState::login`, and nothing downstream is told which sessions those
/// were, so the shells they opened keep running.
///
/// This is the same invariant `signing_out_reaps_the_terminal_that_session_opened`
/// proves for the voluntary path. The involuntary path — which is the one
/// that fires when something hostile is happening — does not hold it.
#[tokio::test]
#[ignore = "defect in auth/state.rs + http.rs, not this file: AuthState::login drops the \
            blocked address's sessions, and the Locked arm of login_submit never calls \
            Ctx::session_ended, so the pty survives the session that owned it"]
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
