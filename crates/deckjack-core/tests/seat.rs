//! The single seat, over real sockets against the real dispatcher.
//!
//! One workspace, one connection using it: these tests open two (or three)
//! signed-in browser sessions against the bus route and check what each one
//! is allowed to do and is told, the way the shell would see it. The gate is
//! `wiring::BusDispatch` and the lease is `seat::Seat`, both as `main.rs`
//! builds them; the terminals and windows are the product's registries.
//!
//! Sessions are admitted through `AuthState::admit` (the passkey path),
//! because the one-time code signs in once and these tests need several
//! sessions. Nothing sleeps to decide anything: the grace tests run with a
//! short grace and wait for the event that says it ended, under a deadline.

use std::collections::VecDeque;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use deckjack_core::agentbridge::AgentBridge;
use deckjack_core::auth::session::COOKIE_NAME;
use deckjack_core::auth::state::{AuthState, Limits, Now};
use deckjack_core::auth::{Code, SessionHash, SessionId};
use deckjack_core::command::{
    Admitted, AgentSessionId, Bus, CommandId, Grant, Grants, Issuer, Journal, JournalError,
    LogEntry, MemoryGrants, NodeId, NodeScope, OsEnv as BusEnv, Pattern, Registry, Request,
};
use deckjack_core::http::Ctx;
use deckjack_core::mcp::AgentIdentity;
use deckjack_core::seat::{self, Seat};
use deckjack_core::store::Store;
use deckjack_core::terminal::{self, Opener};
use deckjack_core::wiring::{self, BusDispatch};
use deckjack_core::ws;

const NODE: &str = "seat-node";
const AGENT: &str = "helper";

struct Discard;

impl Journal for Discard {
    fn record(&self, _entry: LogEntry) -> Result<(), JournalError> {
        Ok(())
    }
}

struct Node {
    addr: SocketAddr,
    auth: Arc<std::sync::Mutex<AuthState>>,
    bus: Arc<Bus>,
    bridge: Arc<AgentBridge>,
    terminals: Arc<terminal::Registry>,
    hub: Arc<ws::Hub>,
    _dir: tempfile::TempDir,
}

async fn start(grace: Duration) -> Node {
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
        .expect("the terminal commands");
    let windows = wiring::register_windows(&registry, NODE.to_string()).expect("window commands");

    // One agent with a grant over the window commands, so the agent path can
    // be shown to be untouched by who holds the seat.
    let grants = Arc::new(MemoryGrants::new());
    grants.put(
        AgentSessionId::new(AGENT),
        Grant::new(
            vec![Pattern::new("shell.*").expect("a valid pattern")],
            NodeScope::ThisNode,
        ),
    );
    let bus = Arc::new(
        Bus::new(Arc::clone(&registry), NodeId::new(NODE), Arc::new(BusEnv))
            .with_journal(Arc::new(Discard) as Arc<dyn Journal>)
            .with_grants(Arc::clone(&grants) as Arc<dyn Grants>),
    );
    let bridge = AgentBridge::new(Arc::clone(&bus), Arc::clone(&grants) as Arc<dyn Grants>);

    let hub = ws::Hub::new(Arc::new(ws::SystemClock));
    let seat = Seat::with_grace(
        Arc::clone(&hub),
        Arc::new(ws::SystemClock),
        vec![Arc::clone(&terminals) as _, Arc::clone(&windows) as _],
        grace,
    );
    seat::register(&registry, &seat).expect("the seat commands");

    // Ending a session reaps its shells and frees the seat, as main.rs
    // registers both.
    let reaper = Arc::clone(&terminals);
    let seat_reaper = Arc::clone(&seat);
    let ctx = Arc::new(
        Ctx::new(
            Arc::clone(&auth),
            Arc::clone(&store),
            Duration::ZERO,
            Vec::new(),
            NODE.to_string(),
        )
        .on_session_end(move |hash| reaper.close_for_session(hash))
        .on_session_end(move |hash| seat_reaper.session_ended(hash)),
    );
    let ws_ctx = ws::WsCtx::new(
        Arc::clone(&ctx),
        BusDispatch::new(
            Arc::clone(&bus),
            deckjack_core::ticket::Tickets::new(Arc::new(ws::SystemClock)),
            Arc::clone(&seat),
        ),
        seat,
        Arc::clone(&hub),
        Arc::new(ws::SystemClock),
        NODE.to_string(),
        "seat-host".to_string(),
        ws::Settings::default(),
    );
    let tcp = TcpListener::bind("127.0.0.1:0").await.expect("a port");
    let addr = tcp.local_addr().expect("the address");
    let app = ws::router(ws_ctx).merge(deckjack_core::http::router(ctx));
    tokio::spawn(async move {
        let _ = axum::serve(tcp, app.into_make_service_with_connect_info::<SocketAddr>()).await;
    });

    Node {
        addr,
        auth,
        bus,
        bridge,
        terminals,
        hub,
        _dir: dir,
    }
}

/// A signed-in browser session: its id (for asking the bus directly what it
/// may reach) and the cookie its sockets carry.
struct Session {
    id: SessionId,
    cookie: String,
}

impl Session {
    fn hash(&self) -> SessionHash {
        self.id.hash()
    }
}

impl Node {
    fn sign_in(&self) -> Session {
        let id = SessionId::generate().expect("a session id");
        let mut events = Vec::new();
        self.auth
            .lock()
            .expect("the auth state")
            .admit(
                "127.0.0.1".parse().expect("an address"),
                &id,
                Now::current(),
                &mut events,
            )
            .expect("the node admits another session");
        let cookie = format!("{COOKIE_NAME}={}", id.encode());
        Session { id, cookie }
    }

    /// Open a socket for `session`, say hello as `device`, and return it
    /// with the welcome consumed.
    async fn connect(&self, session: &Session, device: &str) -> Ws {
        let mut ws = Ws::connect(self.addr, &session.cookie).await;
        ws.send(json!({
            "type": "hello",
            "protocol": ws::PROTOCOL_VERSION,
            "client": "seat-test",
            "device": device,
        }))
        .await;
        let welcome = ws.next_frame().await;
        assert_eq!(welcome["type"], "welcome", "got {welcome}");
        ws
    }

    /// Open a socket the way an app's own UI does (`web/public/kit/bus.js`):
    /// a hello that names no device.
    async fn attendant(&self, session: &Session) -> Ws {
        let mut ws = Ws::connect(self.addr, &session.cookie).await;
        ws.send(json!({ "type": "hello", "protocol": ws::PROTOCOL_VERSION }))
            .await;
        let welcome = ws.next_frame().await;
        assert_eq!(welcome["type"], "welcome", "got {welcome}");
        ws
    }

    /// Dispatch on the bus as a browser session, past the socket and so past
    /// the seat: what a session's *ownership* reaches, whatever the seat
    /// says.
    /// Sign `session` out the way the shell's menu does: `POST /logout`.
    async fn sign_out(&self, session: &Session) {
        let mut stream = TcpStream::connect(self.addr).await.expect("connect");
        let req = format!(
            "POST /logout HTTP/1.1\r\nHost: {}\r\nCookie: {}\r\nContent-Length: 0\r\n\
             Connection: close\r\n\r\n",
            self.addr, session.cookie
        );
        stream.write_all(req.as_bytes()).await.expect("send");
        let mut answer = Vec::new();
        stream.read_to_end(&mut answer).await.expect("the answer");
        let text = String::from_utf8_lossy(&answer);
        assert!(text.starts_with("HTTP/1.1 303"), "not signed out: {text}");
    }

    async fn as_session(
        &self,
        session: &Session,
        command: &str,
        params: Value,
    ) -> Result<Value, String> {
        let mut request = Request::new(
            CommandId::new(command.to_string()).expect("a command id"),
            params,
            Issuer::browser(session.id.clone()),
        );
        request.confirmed = true;
        run(&self.bus, request).await
    }
}

async fn run(bus: &Bus, request: Request) -> Result<Value, String> {
    match bus.dispatch(request) {
        Ok(Admitted::Running(running)) => {
            let (_rx, done) = running.split();
            done.await.outcome.map_err(|e| e.to_string())
        }
        Ok(other) => Err(format!("did not run: {other:?}")),
        Err(refused) => Err(refused.to_string()),
    }
}

// ---------------------------------------------------------------------------
// A minimal WebSocket client (the same shape as end_to_end.rs's)
// ---------------------------------------------------------------------------

struct Ws {
    stream: TcpStream,
    /// Frames read while waiting for something else, in order.
    held: VecDeque<Value>,
    next_id: u64,
}

const DEADLINE: Duration = Duration::from_secs(10);

impl Ws {
    async fn connect(addr: SocketAddr, cookie: &str) -> Ws {
        let req = format!(
            "GET {} HTTP/1.1\r\nHost: {addr}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\
             Sec-WebSocket-Version: 13\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\
             Cookie: {cookie}\r\n\r\n",
            ws::PATH
        );
        let mut stream = TcpStream::connect(addr).await.expect("connect");
        stream.write_all(req.as_bytes()).await.expect("upgrade");
        let mut head = Vec::new();
        loop {
            let mut byte = [0u8; 1];
            let n = stream.read(&mut byte).await.expect("the handshake");
            assert_eq!(n, 1, "the server closed mid-handshake");
            head.push(byte[0]);
            if head.ends_with(b"\r\n\r\n") {
                break;
            }
        }
        let text = String::from_utf8_lossy(&head).into_owned();
        assert!(text.starts_with("HTTP/1.1 101"), "not upgraded: {text}");
        Ws {
            stream,
            held: VecDeque::new(),
            next_id: 0,
        }
    }

    async fn send_frame(&mut self, opcode: u8, payload: &[u8]) {
        const MASK: [u8; 4] = [0x37, 0xfa, 0x21, 0x3d];
        let mut frame = vec![0x80 | opcode];
        if payload.len() < 126 {
            frame.push(0x80 | payload.len() as u8);
        } else {
            frame.push(0x80 | 126);
            frame.extend_from_slice(&(payload.len() as u16).to_be_bytes());
        }
        frame.extend_from_slice(&MASK);
        frame.extend(payload.iter().enumerate().map(|(i, b)| b ^ MASK[i % 4]));
        self.stream.write_all(&frame).await.expect("send a frame");
    }

    async fn send(&mut self, frame: Value) {
        self.send_frame(0x1, frame.to_string().as_bytes()).await;
    }

    async fn read_exact(&mut self, n: usize) -> Vec<u8> {
        let mut buf = vec![0u8; n];
        self.stream.read_exact(&mut buf).await.expect("frame bytes");
        buf
    }

    /// The next text frame off the wire, answering pings on the way.
    async fn wire_frame(&mut self) -> Value {
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
                0x1 => return serde_json::from_slice(&payload).expect("a JSON frame"),
                0x8 => panic!("the socket closed: {}", String::from_utf8_lossy(&payload)),
                0x9 => self.send_frame(0xa, &payload).await,
                _ => {}
            }
        }
    }

    async fn next_frame(&mut self) -> Value {
        if let Some(frame) = self.held.pop_front() {
            return frame;
        }
        tokio::time::timeout(DEADLINE, self.wire_frame())
            .await
            .expect("a frame within the deadline")
    }

    /// The first frame matching `want`, keeping every other frame for later.
    async fn find(&mut self, what: &str, want: impl Fn(&Value) -> bool) -> Value {
        if let Some(i) = self.held.iter().position(&want) {
            return self.held.remove(i).expect("the frame just found");
        }
        let found = tokio::time::timeout(DEADLINE, async {
            loop {
                let frame = self.wire_frame().await;
                if want(&frame) {
                    return frame;
                }
                self.held.push_back(frame);
            }
        })
        .await;
        found.unwrap_or_else(|_| panic!("no {what} within the deadline; held: {:?}", self.held))
    }

    /// The next `seat` event's payload.
    async fn seat(&mut self) -> Value {
        self.find("seat event", |f| {
            f["type"] == "event" && f["topic"] == "seat"
        })
        .await["payload"]
            .clone()
    }

    /// The next `seat` event that satisfies `want`, skipping earlier ones.
    async fn seat_until(&mut self, want: impl Fn(&Value) -> bool) -> Value {
        self.find("matching seat event", |f| {
            f["type"] == "event" && f["topic"] == "seat" && want(&f["payload"])
        })
        .await["payload"]
            .clone()
    }

    async fn windows(&mut self) -> Value {
        self.find("windows event", |f| {
            f["type"] == "event" && f["topic"] == "windows"
        })
        .await["payload"]
            .clone()
    }

    /// Invoke and wait for its response, keeping events for later.
    async fn invoke(&mut self, command: &str, params: Value) -> Value {
        self.next_id += 1;
        let id = format!("c{}", self.next_id);
        self.send(json!({ "type": "invoke", "id": id, "command": command, "params": params }))
            .await;
        self.find("response", |f| f["type"] == "response" && f["id"] == id)
            .await["outcome"]
            .clone()
    }

    async fn invoke_as_app(&mut self, app: &str, command: &str, params: Value) -> Value {
        self.next_id += 1;
        let id = format!("c{}", self.next_id);
        self.send(json!({
            "type": "invoke", "id": id, "command": command, "params": params, "as_app": app,
        }))
        .await;
        self.find("response", |f| f["type"] == "response" && f["id"] == id)
            .await["outcome"]
            .clone()
    }

    /// Close the socket the way a browser tab going away does.
    async fn close(mut self) {
        self.send_frame(0x8, &1000u16.to_be_bytes()).await;
        let _ = self.stream.shutdown().await;
    }
}

fn ok(outcome: &Value) -> &Value {
    assert_eq!(
        outcome["status"], "ok",
        "the call did not succeed: {outcome}"
    );
    &outcome["value"]
}

fn error_code(outcome: &Value) -> &str {
    assert_eq!(
        outcome["status"], "error",
        "the call was not refused: {outcome}"
    );
    outcome["error"]["code"].as_str().unwrap_or_default()
}

impl Node {
    /// The ids of the connections open right now.
    fn connection_ids(&self) -> Vec<u64> {
        let mut ids: Vec<u64> = self.hub.summary().iter().map(|c| c.connection.0).collect();
        ids.sort_unstable();
        ids
    }

    /// [`Node::connect`], also returning the connection id the node gave it.
    async fn connect_with_id(&self, session: &Session, device: &str) -> (Ws, u64) {
        let before = self.connection_ids();
        let ws = self.connect(session, device).await;
        let id = self
            .connection_ids()
            .into_iter()
            .find(|id| !before.contains(id))
            .expect("the new connection is registered");
        (ws, id)
    }
}

/// A write that needs nothing to exist: tile no windows.
const WRITE: &str = "shell.windows.tile";

// ---------------------------------------------------------------------------
// Who gets the seat
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_first_connection_gets_the_seat_and_a_second_device_is_refused() {
    let node = start(seat::GRACE).await;
    let (a, b) = (node.sign_in(), node.sign_in());

    let mut laptop = node.connect(&a, "Firefox on Linux").await;
    let mine = laptop.seat().await;
    assert_eq!(
        mine["held"], true,
        "the first connection was not seated: {mine}"
    );
    assert_eq!(mine["holder"]["device"], "Firefox on Linux");
    ok(&laptop.invoke(WRITE, json!({})).await);

    let mut phone = node.connect(&b, "Chrome on Pixel").await;
    let theirs = phone.seat().await;
    assert_eq!(theirs["held"], false, "{theirs}");
    assert_eq!(theirs["claimable"], false, "{theirs}");
    assert_eq!(theirs["holder"]["device"], "Firefox on Linux");
    assert_eq!(theirs["holder"]["connected"], true);

    let refused = phone.invoke(WRITE, json!({})).await;
    assert_eq!(error_code(&refused), seat::NOT_HELD, "{refused}");
    assert!(
        refused["error"]["message"]
            .as_str()
            .is_some_and(|m| m.contains("Firefox on Linux")),
        "the refusal names who has it: {refused}"
    );
    assert_eq!(
        refused["error"]["detail"]["holder"]["device"], "Firefox on Linux",
        "the refusal carries the status: {refused}"
    );
    // A read is refused the same way: the line is the connection, not the
    // risk of the call.
    let read = phone.invoke("terminal.list", json!({})).await;
    assert_eq!(error_code(&read), seat::NOT_HELD, "{read}");

    // What the takeover screen needs is still answered.
    let status = ok(&phone.invoke("seat.status", json!({})).await).clone();
    assert_eq!(status["held"], false);
    assert_eq!(status["holder"]["device"], "Firefox on Linux");

    // And the holder was not disturbed by any of it.
    ok(&laptop.invoke(WRITE, json!({})).await);
}

#[tokio::test]
async fn a_device_label_is_sanitized_before_another_device_sees_it() {
    let node = start(seat::GRACE).await;
    let (a, b) = (node.sign_in(), node.sign_in());
    let mut first = node
        .connect(
            &a,
            "  Pixel\u{202E}  7\u{0007}   with a name far, far too long to keep whole  ",
        )
        .await;
    first.seat().await;
    let mut second = node.connect(&b, "other").await;
    let seen = second.seat().await;
    let device = seen["holder"]["device"].as_str().expect("a label");
    assert!(device.starts_with("Pixel 7 with a name"), "got {device:?}");
    assert_eq!(
        device.chars().count(),
        seat::DEVICE_MAX_CHARS,
        "got {device:?}"
    );
    assert!(!device.contains('\u{202E}') && !device.contains('\u{0007}'));
}

#[tokio::test]
async fn taking_the_seat_moves_it_and_tells_both_devices() {
    let node = start(seat::GRACE).await;
    let (a, b) = (node.sign_in(), node.sign_in());
    let mut laptop = node.connect(&a, "Firefox on Linux").await;
    assert_eq!(laptop.seat().await["held"], true);
    let mut phone = node.connect(&b, "Chrome on Pixel").await;
    assert_eq!(phone.seat().await["held"], false);

    let took = phone
        .invoke("seat.take", json!({ "device": "Pixel 7" }))
        .await;
    assert_eq!(ok(&took)["held"], true, "{took}");

    let now_phone = phone.seat().await;
    assert_eq!(now_phone["held"], true, "{now_phone}");
    assert_eq!(
        now_phone["holder"]["device"], "Pixel 7",
        "the take relabeled it"
    );

    let now_laptop = laptop.seat().await;
    assert_eq!(now_laptop["held"], false, "{now_laptop}");
    assert_eq!(now_laptop["holder"]["device"], "Pixel 7");
    assert_eq!(now_laptop["holder"]["connected"], true);

    ok(&phone.invoke(WRITE, json!({})).await);
    let refused = laptop.invoke(WRITE, json!({})).await;
    assert_eq!(error_code(&refused), seat::NOT_HELD, "{refused}");

    // Taking it back is the same command the other way.
    ok(&laptop.invoke("seat.take", json!({})).await);
    assert_eq!(laptop.seat_until(|s| s["held"] == true).await["held"], true);
    assert_eq!(
        phone.seat_until(|s| s["held"] == false).await["holder"]["device"],
        "Firefox on Linux"
    );
}

#[tokio::test]
async fn releasing_the_seat_leaves_it_free_and_claim_never_steals() {
    let node = start(seat::GRACE).await;
    let (a, b) = (node.sign_in(), node.sign_in());
    let mut laptop = node.connect(&a, "laptop").await;
    laptop.seat().await;
    let mut phone = node.connect(&b, "phone").await;
    phone.seat().await;

    // A claim while someone holds it changes nothing.
    let claimed = ok(&phone.invoke("seat.claim", json!({})).await).clone();
    assert_eq!(claimed["held"], false, "a claim took the seat: {claimed}");
    ok(&laptop.invoke(WRITE, json!({})).await);

    ok(&laptop.invoke("seat.release", json!({})).await);
    let vacant = phone.seat_until(|s| s["holder"].is_null()).await;
    assert_eq!(vacant["claimable"], true, "{vacant}");

    let claimed = ok(&phone.invoke("seat.claim", json!({})).await).clone();
    assert_eq!(
        claimed["held"], true,
        "a free seat was not claimed: {claimed}"
    );
    let refused = laptop.invoke(WRITE, json!({})).await;
    assert_eq!(error_code(&refused), seat::NOT_HELD, "{refused}");
}

// ---------------------------------------------------------------------------
// The workspace follows the seat
// ---------------------------------------------------------------------------

#[tokio::test]
async fn taking_over_hands_the_terminals_to_the_new_session_and_not_back() {
    let node = start(seat::GRACE).await;
    let (a, b) = (node.sign_in(), node.sign_in());
    let mut laptop = node.connect(&a, "laptop").await;
    laptop.seat().await;
    let opened = ok(&laptop
        .invoke("terminal.open", json!({ "rows": 24, "cols": 80 }))
        .await)
    .clone();
    let id = opened["id"].as_str().expect("a terminal id").to_string();

    let mut phone = node.connect(&b, "phone").await;
    phone.seat().await;
    ok(&phone.invoke("seat.take", json!({})).await);

    // The new device finds the shell by asking what it owns, and can use it.
    let listed = ok(&phone.invoke("terminal.list", json!({})).await).clone();
    let ids: Vec<&str> = listed
        .as_array()
        .expect("a list")
        .iter()
        .filter_map(|t| t["id"].as_str())
        .collect();
    assert_eq!(ids, vec![id.as_str()], "the shell did not follow the seat");
    ok(&phone
        .invoke("terminal.write", json!({ "id": id, "data": "true\r" }))
        .await);

    // The old session owns nothing now, whatever the seat says: asked past the
    // socket, it cannot reach the shell it opened.
    assert_eq!(node.terminals.list_for(&Opener::Client(a.hash())).len(), 0);
    assert_eq!(node.terminals.list_for(&Opener::Client(b.hash())).len(), 1);
    let reattach = node
        .as_session(&a, "terminal.write", json!({ "id": id, "data": "x" }))
        .await;
    assert!(
        reattach
            .as_ref()
            .is_err_and(|e| e.contains(&format!("there is no terminal {id}"))),
        "the old session reached the shell: {reattach:?}"
    );
    node.as_session(&b, "terminal.write", json!({ "id": id, "data": "true\r" }))
        .await
        .expect("the control: the new owner reaches it the same way");
}

#[tokio::test]
async fn window_changes_are_pushed_and_a_takeover_is_handed_the_open_windows() {
    let node = start(seat::GRACE).await;
    let (a, b) = (node.sign_in(), node.sign_in());
    let mut laptop = node.connect(&a, "laptop").await;
    laptop.seat().await;
    // Acquiring the seat hands over the windows even when there are none.
    assert_eq!(laptop.windows().await, json!([]));
    let mut phone = node.connect(&b, "phone").await;
    phone.seat().await;

    ok(&laptop
        .invoke(
            "shell.window.open",
            json!({ "id": "w-notes", "app": "notes", "title": "Notes", "x": 40, "y": 60, "w": 700, "h": 500 }),
        )
        .await);
    // Every connection is told, the holder included.
    for ws in [&mut laptop, &mut phone] {
        let pushed = ws.windows().await;
        assert_eq!(pushed[0]["id"], "w-notes", "{pushed}");
    }
    ok(&laptop
        .invoke("shell.window.minimize", json!({ "id": "w-notes" }))
        .await);
    let pushed = phone.windows().await;
    assert_eq!(pushed[0]["desktop"]["minimized"], true, "{pushed}");
    laptop.windows().await;

    // A third device takes over and is handed the snapshot, before the seat
    // event that tells it to show the workspace.
    let c = node.sign_in();
    let mut tablet = node.connect(&c, "tablet").await;
    tablet.seat().await;
    tablet
        .send(json!({ "type": "invoke", "id": "take", "command": "seat.take", "params": {} }))
        .await;
    let mut snapshot = None;
    loop {
        let frame = tablet.next_frame().await;
        if frame["type"] == "event" && frame["topic"] == "windows" {
            snapshot = Some(frame["payload"].clone());
        }
        if frame["type"] == "event" && frame["topic"] == "seat" && frame["payload"]["held"] == true
        {
            break;
        }
    }
    let snapshot = snapshot.expect("the windows snapshot came before the seat");
    assert_eq!(
        snapshot,
        json!([{
            "id": "w-notes",
            "app": "notes",
            "title": "Notes",
            "minWidth": 320,
            "minHeight": 0,
            "desktop": { "x": 40, "y": 60, "w": 700, "h": 500, "z": 0, "minimized": true, "maximized": false },
        }]),
        "the device that took over sees exactly what was open, where it was"
    );
}

// ---------------------------------------------------------------------------
// Grace and auto-claim
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_dropped_holder_gets_the_seat_back_inside_the_grace() {
    let node = start(seat::GRACE).await;
    let (a, b) = (node.sign_in(), node.sign_in());
    let laptop = node.connect(&a, "phone of a").await;
    let mut watcher = node.connect(&b, "watcher").await;
    watcher.seat().await;

    laptop.close().await;
    let away = watcher
        .seat_until(|s| s["holder"]["connected"] == false)
        .await;
    assert_eq!(away["holder"]["device"], "phone of a", "{away}");
    assert_eq!(
        away["claimable"], false,
        "another session may not claim a reserved seat"
    );
    let refused = watcher.invoke(WRITE, json!({})).await;
    assert_eq!(
        error_code(&refused),
        seat::NOT_HELD,
        "the reservation held: {refused}"
    );

    // The same session, on a new connection, is seated without asking.
    let mut back = node.connect(&a, "phone of a").await;
    let reclaimed = back.seat().await;
    assert_eq!(reclaimed["held"], true, "{reclaimed}");
    ok(&back.invoke(WRITE, json!({})).await);
    let seen = watcher
        .seat_until(|s| s["holder"]["connected"] == true)
        .await;
    assert_eq!(seen["held"], false);
}

#[tokio::test]
async fn after_the_grace_the_next_device_to_ask_gets_the_seat_without_a_prompt() {
    let node = start(Duration::from_millis(300)).await;
    let (a, b) = (node.sign_in(), node.sign_in());
    let laptop = node.connect(&a, "laptop").await;
    let mut phone = node.connect(&b, "phone").await;
    phone.seat().await;
    // The seat is laptop's for its connection's life.
    assert_eq!(
        error_code(&phone.invoke(WRITE, json!({})).await),
        seat::NOT_HELD
    );

    laptop.close().await;
    let vacant = phone.seat_until(|s| s["holder"].is_null()).await;
    assert_eq!(vacant["claimable"], true, "{vacant}");

    // Asking for anything is enough.
    ok(&phone.invoke(WRITE, json!({})).await);
    assert_eq!(phone.seat_until(|s| s["held"] == true).await["held"], true);

    // And a seat that went vacant with nobody waiting goes to whoever
    // connects next.
    ok(&phone.invoke("seat.release", json!({})).await);
    let c = node.sign_in();
    let mut tablet = node.connect(&c, "tablet").await;
    assert_eq!(tablet.seat().await["held"], true);
}

#[tokio::test]
async fn a_second_connection_of_the_holding_session_takes_the_seat_at_once() {
    let node = start(seat::GRACE).await;
    let a = node.sign_in();
    let mut old = node.connect(&a, "laptop").await;
    assert_eq!(old.seat().await["held"], true);
    // The old socket is still open: a reload whose old connection the node
    // has not noticed dropping yet.
    let mut new = node.connect(&a, "laptop").await;
    assert_eq!(new.seat().await["held"], true);
    assert_eq!(
        old.seat_until(|s| s["held"] == false).await["holder"]["device"],
        "laptop"
    );
    ok(&new.invoke(WRITE, json!({})).await);
}

#[tokio::test]
async fn an_app_ui_socket_rides_on_its_sessions_seat_and_never_takes_it() {
    let node = start(seat::GRACE).await;
    let (a, b) = (node.sign_in(), node.sign_in());
    let mut shell = node.connect(&a, "laptop").await;
    assert_eq!(shell.seat().await["held"], true);

    // An app window on the same browser opens its own socket: it may act,
    // and the shell around it keeps the seat.
    let mut app = node.attendant(&a).await;
    ok(&app.invoke(WRITE, json!({})).await);
    ok(&shell.invoke(WRITE, json!({})).await);

    // Another device's app socket may not.
    let mut stranger = node.attendant(&b).await;
    assert_eq!(
        error_code(&stranger.invoke(WRITE, json!({})).await),
        seat::NOT_HELD
    );

    // Once another device takes the seat, the first device's app socket goes
    // with its shell.
    let mut phone = node.connect(&b, "phone").await;
    phone.seat().await;
    ok(&phone.invoke("seat.take", json!({})).await);
    assert_eq!(
        error_code(&app.invoke(WRITE, json!({})).await),
        seat::NOT_HELD
    );
    ok(&stranger.invoke(WRITE, json!({})).await);
}

// ---------------------------------------------------------------------------
// Who the seat does not apply to, and who may never touch it
// ---------------------------------------------------------------------------

#[tokio::test]
async fn an_apps_window_cannot_call_the_seat_commands() {
    let node = start(seat::GRACE).await;
    let (a, b) = (node.sign_in(), node.sign_in());
    let mut holder = node.connect(&a, "laptop").await;
    holder.seat().await;
    let mut other = node.connect(&b, "phone").await;
    other.seat().await;

    for command in ["seat.take", "seat.claim", "seat.release", "seat.status"] {
        let refused = other.invoke_as_app("filer", command, json!({})).await;
        assert_eq!(
            error_code(&refused),
            wiring::NOT_PERMITTED,
            "{command}: {refused}"
        );
        assert!(
            refused["error"]["message"]
                .as_str()
                .is_some_and(|m| m.contains("never available to an app's window")),
            "{command} was refused, but not by the hard-deny list: {refused}"
        );
    }
    // The seat did not move.
    ok(&holder.invoke(WRITE, json!({})).await);
}

#[tokio::test]
async fn the_host_and_agents_are_not_held_to_the_seat() {
    let node = start(seat::GRACE).await;
    let (a, b) = (node.sign_in(), node.sign_in());
    let mut holder = node.connect(&a, "laptop").await;
    holder.seat().await;
    let mut other = node.connect(&b, "phone").await;
    other.seat().await;
    // The control: a browser socket without the seat is refused.
    assert_eq!(
        error_code(&other.invoke(WRITE, json!({})).await),
        seat::NOT_HELD
    );

    // The host's own command, while a device holds the seat.
    let cli = Request::new(
        CommandId::new(WRITE.to_string()).expect("an id"),
        json!({}),
        Issuer::Cli,
    );
    run(&node.bus, cli)
        .await
        .expect("the CLI is not held to the seat");

    // An agent, through the bridge every agent goes through.
    let agent = AgentIdentity {
        session: AGENT.to_string(),
        label: "helper".to_string(),
        via_window: None,
    };
    let result = node
        .bridge
        .call_tool(&agent, "shell__windows__tile", None)
        .await;
    assert_ne!(
        result.is_error,
        Some(true),
        "the agent was refused: {:?}",
        result.structured_content
    );

    // Neither took the seat from the device holding it.
    ok(&holder.invoke(WRITE, json!({})).await);
}

// ---------------------------------------------------------------------------
// Review follow-ups: the edges where the seat moves while something is
// running, and the paths that are not the shell's own calls
// ---------------------------------------------------------------------------

/// An app's own socket (`web/public/kit/bus.js`) names no device. The seat's
/// commands are exempt from the gate and such a socket is not an `as_app`
/// call, so the hard-deny list never sees it; the seat itself refuses it.
#[tokio::test]
async fn an_app_ui_socket_cannot_take_claim_or_release_the_seat() {
    let node = start(seat::GRACE).await;
    let (a, b) = (node.sign_in(), node.sign_in());
    let mut laptop = node.connect(&a, "laptop").await;
    assert_eq!(laptop.seat().await["held"], true);
    let mut phone = node.connect(&b, "phone").await;
    phone.seat().await;

    // Another device's app socket, and the holder's own app socket.
    let mut stranger = node.attendant(&b).await;
    let mut own = node.attendant(&a).await;
    for (who, ws) in [
        ("another device's", &mut stranger),
        ("the holder's", &mut own),
    ] {
        for command in ["seat.take", "seat.claim", "seat.release"] {
            let refused = ws.invoke(command, json!({})).await;
            assert_eq!(error_code(&refused), "failed", "{who} {command}: {refused}");
            assert!(
                refused["error"]["message"]
                    .as_str()
                    .is_some_and(|m| m.contains("names no device")),
                "{who} {command} was refused for the wrong reason: {refused}"
            );
        }
        // Reading the seat is still fine.
        ok(&ws.invoke("seat.status", json!({})).await);
    }
    // The seat never moved, and was never released.
    ok(&laptop.invoke(WRITE, json!({})).await);
    assert_eq!(
        error_code(&phone.invoke(WRITE, json!({})).await),
        seat::NOT_HELD
    );
}

/// An app window's calls through the shell's frame bridge (`as_app`) are
/// held to the seat like the shell's own: a device without the seat cannot
/// run what the window's scope would allow.
#[tokio::test]
async fn an_apps_window_on_a_device_without_the_seat_is_refused_at_the_gate() {
    let node = start(seat::GRACE).await;
    let (a, b) = (node.sign_in(), node.sign_in());
    let mut holder = node.connect(&a, "laptop").await;
    holder.seat().await;
    let mut other = node.connect(&b, "phone").await;
    other.seat().await;

    let refused = other
        .invoke_as_app("filer", "files.list", json!({ "path": "/" }))
        .await;
    assert_eq!(error_code(&refused), seat::NOT_HELD, "{refused}");
    // The control: on the holder, the same call gets past the seat and is
    // decided by the app's scope instead.
    let past = holder
        .invoke_as_app("filer", "files.list", json!({ "path": "/" }))
        .await;
    assert_ne!(
        past["error"]["code"].as_str(),
        Some(seat::NOT_HELD),
        "{past}"
    );
}

#[tokio::test]
async fn the_snapshot_keeps_the_stacking_order() {
    let node = start(seat::GRACE).await;
    let (a, b) = (node.sign_in(), node.sign_in());
    let mut laptop = node.connect(&a, "laptop").await;
    laptop.seat().await;
    for id in ["w-back", "w-front"] {
        ok(&laptop
            .invoke(
                "shell.window.open",
                json!({ "id": id, "app": "notes", "x": 10, "y": 10, "w": 400, "h": 300 }),
            )
            .await);
    }
    let mut phone = node.connect(&b, "phone").await;
    phone.seat().await;
    phone
        .send(json!({ "type": "invoke", "id": "take", "command": "seat.take", "params": {} }))
        .await;
    let mut snapshot = None;
    loop {
        let frame = phone.next_frame().await;
        if frame["type"] == "event" && frame["topic"] == "windows" {
            snapshot = Some(frame["payload"].clone());
        }
        if frame["type"] == "event" && frame["topic"] == "seat" && frame["payload"]["held"] == true
        {
            break;
        }
    }
    let snapshot = snapshot.expect("a snapshot");
    let order: Vec<(&str, u64)> = snapshot
        .as_array()
        .expect("a list")
        .iter()
        .map(|w| {
            (
                w["id"].as_str().expect("an id"),
                w["desktop"]["z"].as_u64().expect("a z"),
            )
        })
        .collect();
    assert_eq!(
        order,
        vec![("w-back", 0), ("w-front", 1)],
        "back to front, the last opened on top: {snapshot}"
    );
}

async fn open_terminal(ws: &mut Ws) -> String {
    let opened = ok(&ws
        .invoke("terminal.open", json!({ "rows": 24, "cols": 80 }))
        .await)
    .clone();
    opened["id"].as_str().expect("a terminal id").to_string()
}

fn owned_by(node: &Node, session: &Session) -> Vec<String> {
    node.terminals
        .list_for(&Opener::Client(session.hash()))
        .into_iter()
        .map(|t| t.id.as_str().to_string())
        .collect()
}

#[tokio::test]
async fn terminals_follow_a_release_and_the_next_claim() {
    let node = start(seat::GRACE).await;
    let (a, b) = (node.sign_in(), node.sign_in());
    let mut laptop = node.connect(&a, "laptop").await;
    laptop.seat().await;
    let id = open_terminal(&mut laptop).await;
    let mut phone = node.connect(&b, "phone").await;
    phone.seat().await;

    ok(&laptop.invoke("seat.release", json!({})).await);
    phone.seat_until(|s| s["holder"].is_null()).await;
    // Still the laptop's while nobody holds the seat.
    assert_eq!(owned_by(&node, &a), vec![id.clone()]);
    assert_eq!(
        ok(&phone.invoke("seat.claim", json!({})).await)["held"],
        true
    );
    assert_eq!(owned_by(&node, &b), vec![id], "the shell did not follow");
    assert!(owned_by(&node, &a).is_empty());
}

#[tokio::test]
async fn terminals_follow_a_grace_that_ran_out_to_the_next_device() {
    let node = start(Duration::from_millis(300)).await;
    let (a, b) = (node.sign_in(), node.sign_in());
    let mut laptop = node.connect(&a, "laptop").await;
    laptop.seat().await;
    let id = open_terminal(&mut laptop).await;
    let mut phone = node.connect(&b, "phone").await;
    phone.seat().await;

    laptop.close().await;
    phone.seat_until(|s| s["holder"].is_null()).await;
    // Asking for anything claims the vacant seat, and the shell comes along.
    let listed = ok(&phone.invoke("terminal.list", json!({})).await).clone();
    assert_eq!(listed[0]["id"], id.as_str(), "{listed}");
    assert_eq!(owned_by(&node, &b), vec![id]);
}

/// Signing out closes the session's sockets; the seat must not then sit
/// reserved for a session that can never come back.
#[tokio::test]
async fn a_session_that_ends_frees_the_seat_without_a_grace() {
    // The full grace: only the session's end can free the seat in time.
    let node = start(seat::GRACE).await;
    let (a, b) = (node.sign_in(), node.sign_in());
    let mut phone = node.connect(&a, "phone").await;
    phone.seat().await;
    let mut laptop = node.connect(&b, "laptop").await;
    assert_eq!(laptop.seat().await["held"], false);

    node.sign_out(&a).await;
    let freed = laptop.seat_until(|s| s["holder"].is_null()).await;
    assert_eq!(freed["claimable"], true, "{freed}");
    ok(&laptop.invoke(WRITE, json!({})).await);
}

/// A `terminal.read` parked by the holder before a takeover must not hand
/// it the first output the new holder causes.
#[tokio::test]
async fn a_read_parked_before_a_takeover_does_not_return_the_new_holders_output() {
    let node = start(seat::GRACE).await;
    let (a, b) = (node.sign_in(), node.sign_in());
    let mut laptop = node.connect(&a, "laptop").await;
    laptop.seat().await;
    let id = open_terminal(&mut laptop).await;

    // Park a read on a quiet shell, as the old owner. Past the socket: what
    // is under test is the read's own ownership check, which outlives the
    // gate that admitted it.
    let parked = {
        let mut cursor = 0;
        loop {
            let read = Request::new(
                CommandId::new("terminal.read".to_string()).expect("an id"),
                json!({ "id": id, "cursor": cursor }),
                Issuer::browser(a.id.clone()),
            );
            let bus = Arc::clone(&node.bus);
            let mut task = tokio::spawn(async move { run(&bus, read).await });
            // The shell is quiet once a read has waited a while for output;
            // until then, follow it.
            match tokio::time::timeout(Duration::from_millis(400), &mut task).await {
                Err(_) => break task,
                Ok(done) => {
                    let chunk = done.expect("the read task").expect("a chunk");
                    cursor = chunk["cursor"].as_u64().expect("a cursor");
                }
            }
        }
    };

    let mut phone = node.connect(&b, "phone").await;
    phone.seat().await;
    ok(&phone.invoke("seat.take", json!({})).await);
    ok(&phone
        .invoke(
            "terminal.write",
            json!({ "id": id, "data": "echo seat-marker-$((6*7))\r" }),
        )
        .await);

    let answer = tokio::time::timeout(DEADLINE, parked)
        .await
        .expect("the parked read answered")
        .expect("the read task");
    assert!(
        answer
            .as_ref()
            .is_err_and(|e| e.contains(&format!("there is no terminal {id}"))),
        "the old owner's parked read was answered: {answer:?}"
    );
}

/// A `terminal.open` the old holder sent before a takeover, that registers
/// its pty after the takeover moved the terminals, still follows the seat.
#[tokio::test]
async fn a_terminal_opened_across_a_takeover_lands_with_the_new_holder() {
    let node = start(seat::GRACE).await;
    let (a, b) = (node.sign_in(), node.sign_in());
    let mut laptop = node.connect(&a, "laptop").await;
    laptop.seat().await;
    let mut phone = node.connect(&b, "phone").await;
    phone.seat().await;
    ok(&phone.invoke("seat.take", json!({})).await);

    // The open as it runs after the gate admitted it: as the old session.
    let opened = node
        .as_session(&a, "terminal.open", json!({ "rows": 24, "cols": 80 }))
        .await
        .expect("the open runs");
    let id = opened["id"].as_str().expect("an id").to_string();
    assert_eq!(owned_by(&node, &b), vec![id.clone()], "it did not follow");
    assert!(owned_by(&node, &a).is_empty());
    let listed = ok(&phone.invoke("terminal.list", json!({})).await).clone();
    assert_eq!(listed[0]["id"], id.as_str(), "{listed}");

    // Taken back, the old session owns it again and the forward is gone:
    // what it opens now is its own.
    ok(&laptop.invoke("seat.take", json!({})).await);
    assert_eq!(owned_by(&node, &a), vec![id]);
    let mine = node
        .as_session(&a, "terminal.open", json!({ "rows": 24, "cols": 80 }))
        .await
        .expect("the open runs");
    assert!(owned_by(&node, &a).contains(&mine["id"].as_str().expect("an id").to_string()));
    assert!(owned_by(&node, &b).is_empty());
}

/// A window command the old holder sent before a takeover, still running
/// when the seat moved, is refused when it comes to change anything: the
/// new holder was handed a snapshot that does not have it, and ignores
/// `windows` events while it holds the seat.
#[tokio::test]
async fn a_window_change_from_a_connection_that_lost_the_seat_is_refused() {
    let node = start(seat::GRACE).await;
    let (a, b) = (node.sign_in(), node.sign_in());
    let (mut laptop, laptop_id) = node.connect_with_id(&a, "laptop").await;
    laptop.seat().await;
    let mut phone = node.connect(&b, "phone").await;
    phone.seat().await;

    let open = |id: &str| {
        Request::new(
            CommandId::new("shell.window.open".to_string()).expect("an id"),
            json!({ "id": id, "app": "notes" }),
            Issuer::browser(a.id.clone()),
        )
        .via_connection(laptop_id)
    };
    // The control: while the laptop holds the seat, its change lands.
    run(&node.bus, open("w-before"))
        .await
        .expect("the holder's change lands");

    ok(&phone.invoke("seat.take", json!({})).await);
    let late = run(&node.bus, open("w-late")).await;
    assert!(
        late.as_ref()
            .is_err_and(|e| e.contains("no longer holds the seat")),
        "a change from a connection without the seat landed: {late:?}"
    );
    // The CLI is not held to it.
    let cli = Request::new(
        CommandId::new("shell.window.open".to_string()).expect("an id"),
        json!({ "id": "w-cli", "app": "notes" }),
        Issuer::Cli,
    );
    run(&node.bus, cli).await.expect("the CLI's change lands");
}
