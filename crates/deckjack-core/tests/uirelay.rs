//! The UI protocol relay over a real app socket: a real `AppHost`, a real
//! Unix socket and handshake (the app is a task speaking the protocol, as in
//! `tests/appui.rs`), and the relay between it and a recording browser side.
//!
//! What the unit tests in `uirelay.rs` cannot show: that the host sends an
//! app's window frames over its socket in the protocol's own shape, carries
//! the app's patches back to the relay, reopens a window when the app
//! reconnects, and never sends a UI frame to an app that serves a page.

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

use deckjack_core::apphost::AppHost;
use deckjack_core::apps;
use deckjack_core::command::Registry;
use deckjack_core::store::{Store, StoreGrants};
use deckjack_core::uirelay::{UiChannel, UiGate, UiSink};
use deckjack_core::ws::{ConnectionId, UiRelay};

#[derive(Default)]
struct Browser(Mutex<Vec<(ConnectionId, String, Value)>>);

impl UiSink for Browser {
    fn deliver(&self, connection: ConnectionId, window: &str, body: Value) -> Option<bool> {
        self.0
            .lock()
            .unwrap()
            .push((connection, window.to_string(), body));
        Some(true)
    }
}

struct Seated;

impl UiGate for Seated {
    fn admit(&self, _: ConnectionId) -> Result<(), Value> {
        Ok(())
    }
    fn admits_now(&self, _: ConnectionId) -> bool {
        true
    }
    fn is_device(&self, _: ConnectionId) -> bool {
        true
    }
}

fn install_app(install: &Path, name: &str, tier: &str) {
    let dir = install.join("apps").join(name);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("app.toml"),
        format!(
            "[app]\nname = \"{name}\"\nversion = \"1.0.0\"\nprotocol = 1\n{tier}\n\
             [[commands]]\nid = \"{name}.ping\"\nsummary = \"Answer\"\nrisk = \"read\"\n"
        ),
    )
    .unwrap();
    let binary = dir.join(format!("deckjack-{name}"));
    std::fs::write(&binary, "#!/bin/sh\nexit 0\n").unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o755)).unwrap();
}

struct Rig {
    install: std::path::PathBuf,
    registry: Arc<Registry>,
    relay: Arc<UiChannel>,
    browser: Arc<Browser>,
    _host: Arc<AppHost>,
    _dir: tempfile::TempDir,
}

async fn rig() -> Rig {
    let dir = tempfile::tempdir().unwrap();
    let work = dir.path().join("work");
    std::fs::create_dir_all(&work).unwrap();
    let install = dir.path().join("install");
    install_app(&install, "tasks", "tier = 1");
    install_app(&install, "notes", "");
    let store = Arc::new(Mutex::new(Store::open(&work).unwrap()));
    let registry = Arc::new(Registry::new());
    let host = AppHost::new(
        apps::Host::new(
            install.clone(),
            work,
            apps::CoreDoor::new("127.0.0.1:8822".parse().unwrap(), vec![]),
            apps::OsEnv::new(),
            apps::Policy::default(),
            Box::new(StoreGrants::new(Arc::clone(&store))),
        ),
        Arc::clone(&registry),
        install.clone(),
        Duration::from_millis(80),
    );
    let browser = Arc::new(Browser::default());
    let relay = UiChannel::new(Arc::clone(&browser) as _, Arc::new(Seated));
    host.attach_ui(Arc::clone(&relay));
    host.start().await;
    Rig {
        install,
        registry,
        relay,
        browser,
        _host: host,
        _dir: dir,
    }
}

struct FakeApp {
    stream: BufReader<UnixStream>,
}

impl FakeApp {
    async fn send(&mut self, frame: Value) {
        let mut line = frame.to_string();
        line.push('\n');
        self.stream
            .get_mut()
            .write_all(line.as_bytes())
            .await
            .unwrap();
    }

    async fn recv(&mut self) -> Value {
        let mut line = String::new();
        tokio::time::timeout(Duration::from_secs(5), self.stream.read_line(&mut line))
            .await
            .expect("the core sent nothing within 5 s")
            .unwrap();
        serde_json::from_str(&line).unwrap()
    }

    /// Nothing arrives for a while.
    async fn quiet(&mut self) -> bool {
        let mut line = String::new();
        tokio::time::timeout(Duration::from_millis(300), self.stream.read_line(&mut line))
            .await
            .is_err()
    }
}

impl Rig {
    async fn connect(&self, name: &str) -> FakeApp {
        let stream = UnixStream::connect(apps::socket_path(&self.install, name))
            .await
            .unwrap();
        let mut app = FakeApp {
            stream: BufReader::new(stream),
        };
        assert_eq!(app.recv().await["frame"], "hello");
        app.send(json!({
            "frame": "register", "protocol": 1, "app": name, "version": "1.0.0",
            "commands": [{
                "id": format!("{name}.ping"), "summary": "Answer", "risk": "read",
                "params": { "type": "object", "properties": {} },
            }],
        }))
        .await;
        let id = deckjack_core::command::CommandId::new(format!("{name}.ping")).unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !self.registry.contains(&id) {
            assert!(
                std::time::Instant::now() < deadline,
                "{name} never registered"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        app
    }

    async fn browser_frame(&self) -> (ConnectionId, String, Value) {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(frame) = {
                let mut sent = self.browser.0.lock().unwrap();
                (!sent.is_empty()).then(|| sent.remove(0))
            } {
                return frame;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "nothing reached the browser"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }
}

fn tree(text: &str) -> Value {
    json!({ "key": "root", "widget": { "kind": "text", "text": text } })
}

#[tokio::test]
async fn a_window_round_trips_through_the_apps_socket() {
    let r = rig().await;
    let mut app = r.connect("tasks").await;
    let browser = ConnectionId(7);

    r.relay.upstream(
        browser,
        "w1".into(),
        json!({ "op": "open", "app": "tasks" }),
    );
    let open = app.recv().await;
    assert_eq!(open, json!({ "frame": "ui-open", "window": "w1" }));

    app.send(json!({
        "frame": "ui-patch", "window": "w1", "rev": 1,
        "patches": [{ "op": "replace", "path": [], "node": tree("hello") }],
    }))
    .await;
    let (to, window, body) = r.browser_frame().await;
    assert_eq!((to, window.as_str()), (browser, "w1"));
    assert_eq!(body["rev"], 1);
    assert_eq!(body["patches"][0]["node"]["widget"]["text"], "hello");

    // An event goes to the app in the protocol's shape, and its answer comes
    // back with the ack.
    r.relay.upstream(
        browser,
        "w1".into(),
        json!({ "op": "event", "event": { "path": [], "kind": "activated", "seq": 9 } }),
    );
    let event = app.recv().await;
    assert_eq!(event["frame"], "ui-event");
    assert_eq!(event["event"]["seq"], 9);
    app.send(json!({
        "frame": "ui-patch", "window": "w1", "rev": 2, "ack": 9,
        "patches": [{ "op": "props", "path": [], "widget": { "kind": "text", "text": "again" } }],
    }))
    .await;
    let (_, _, body) = r.browser_frame().await;
    assert_eq!(body["ack"], 9);
    assert_eq!(
        r.relay.snapshot("w1").unwrap()["tree"]["widget"]["text"],
        "again"
    );
}

#[tokio::test]
async fn a_reconnecting_app_is_asked_for_its_open_windows_again() {
    let r = rig().await;
    let mut app = r.connect("tasks").await;
    r.relay.upstream(
        ConnectionId(1),
        "w1".into(),
        json!({ "op": "open", "app": "tasks" }),
    );
    assert_eq!(app.recv().await["frame"], "ui-open");
    drop(app);
    // The shell hears the app went, and the new process is asked to draw
    // the window it has never seen.
    let (_, _, body) = r.browser_frame().await;
    assert_eq!(body["status"], "lost");
    let mut again = r.connect("tasks").await;
    assert_eq!(
        again.recv().await,
        json!({ "frame": "ui-open", "window": "w1" })
    );
}

#[tokio::test]
async fn an_app_that_serves_a_page_is_never_sent_a_ui_frame() {
    // A protocol-1 app built before the UI protocol has no variant for these
    // frames and would drop its connection on one.
    let r = rig().await;
    let mut notes = r.connect("notes").await;
    r.relay.upstream(
        ConnectionId(1),
        "w2".into(),
        json!({ "op": "open", "app": "notes" }),
    );
    assert!(notes.quiet().await, "notes was sent a frame");
    let (_, _, body) = r.browser_frame().await;
    assert_eq!(body["error"]["code"], "not_described");
}
