//! The app host's runtime half: a real socket, a real handshake, real frames.
//!
//! `tests/apps.rs` proves the host's *decisions* — discovery, backoff, idle
//! stops, what a manifest becomes as a sandbox policy — with its world
//! injected, and it passed all of them while no `Host` was ever constructed
//! and no socket ever existed. What is exercised here is the half that was
//! missing: accept, handshake, the frame pump, and the bridge that puts an
//! app's commands on the bus and takes them away again.
//!
//! The "app" is a task in this process that speaks the protocol over the same
//! Unix socket a real one would. Nothing is spawned, so nothing depends on a
//! binary existing, a kernel that can confine it, or a timeout: every app in
//! here declares `lifecycle = "on-demand"` and no window is opened, so the
//! host never tries to launch it and the attendant is simply waiting when the
//! fake app connects.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

use deckjack_core::apphost::AppHost;
use deckjack_core::apps;
use deckjack_core::command::{
    Admitted, Bus, CommandId, Issuer, NodeId, OsEnv as BusEnv, Progress, Registry, Request,
    RiskLevel,
};
use deckjack_core::store::{Store, StoreGrants};
use deckjack_core::wiring;

// ---------------------------------------------------------------------------
// A node with one app installed
// ---------------------------------------------------------------------------

struct Rig {
    host: Arc<AppHost>,
    registry: Arc<Registry>,
    bus: Arc<Bus>,
    install: std::path::PathBuf,
    _dir: tempfile::TempDir,
}

/// `app.toml` for an app declaring one read command and one destructive one.
fn manifest(name: &str) -> String {
    format!(
        r#"[app]
name = "{name}"
version = "1.0.0"
protocol = {}
lifecycle = "on-demand"

[[commands]]
id = "{name}.echo"
summary = "Echo the parameters back"
risk = "read"

[[commands]]
id = "{name}.wipe"
summary = "Delete everything"
risk = "destructive"
"#,
        apps::PROTOCOL_VERSION
    )
}

fn install_app(install: &Path, name: &str, manifest_text: &str) {
    let dir = install.join("apps").join(name);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("app.toml"), manifest_text).unwrap();

    // Discovery refuses an app with no executable, and rightly: a manifest
    // with nothing behind it is a broken install. It is never spawned here.
    let binary = dir.join(format!("deckjack-{name}"));
    std::fs::write(&binary, "#!/bin/sh\nexit 0\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
}

async fn rig(apps_to_install: &[&str]) -> Rig {
    let manifests: Vec<(&str, String)> = apps_to_install
        .iter()
        .map(|name| (*name, manifest(name)))
        .collect();
    rig_with(&manifests).await
}

/// A rig over apps whose manifests the test writes itself.
async fn rig_with(apps_to_install: &[(&str, String)]) -> Rig {
    let dir = tempfile::tempdir().unwrap();
    let install = dir.path().join("install");
    std::fs::create_dir_all(&install).unwrap();
    for (name, text) in apps_to_install {
        install_app(&install, name, text);
    }

    let store = Arc::new(std::sync::Mutex::new(Store::open(dir.path()).unwrap()));
    let registry = Arc::new(Registry::new());
    let bus = Arc::new(Bus::new(
        Arc::clone(&registry),
        NodeId::new("apphost-node"),
        Arc::new(BusEnv),
    ));

    let host = AppHost::new(
        apps::Host::new(
            install.clone(),
            dir.path().to_path_buf(),
            apps::CoreDoor::new("127.0.0.1:7878".parse().unwrap(), vec![]),
            apps::OsEnv::new(),
            apps::Policy::default(),
            Box::new(StoreGrants::new(store)),
        ),
        Arc::clone(&registry),
        install.clone(),
        // Short, because every iteration of an attendant's accept loop costs
        // one of these when no app is connected.
        Duration::from_millis(80),
    );
    host.start().await;

    Rig {
        host,
        registry,
        bus,
        install,
        _dir: dir,
    }
}

impl Rig {
    /// Connect as an app and complete the handshake.
    async fn connect(&self, name: &str) -> FakeApp {
        let path = apps::socket_path(&self.install, name);
        // The attendant binds before anything is spawned, so the socket is
        // there as soon as `start` returned. A retry loop would hide exactly
        // the race this ordering exists to prevent.
        let stream = UnixStream::connect(&path)
            .await
            .unwrap_or_else(|e| panic!("connect to {}: {e}", path.display()));
        let mut app = FakeApp {
            name: name.to_string(),
            stream: BufReader::new(stream),
        };

        let hello = app.recv().await;
        assert_eq!(hello["frame"], "hello", "got {hello}");
        assert_eq!(hello["protocol"], apps::PROTOCOL_VERSION);

        app.send(json!({
            "frame": "register",
            "protocol": apps::PROTOCOL_VERSION,
            "app": name,
            "version": "1.0.0",
            "commands": [
                {
                    "id": format!("{name}.echo"),
                    "summary": "Echo the parameters back",
                    "risk": "read",
                    "params": { "type": "object", "properties": {} },
                },
                {
                    "id": format!("{name}.wipe"),
                    "summary": "Delete everything",
                    "risk": "destructive",
                    "params": { "type": "object", "properties": {} },
                },
            ],
        }))
        .await;
        app
    }

    /// Wait until the registry holds (or no longer holds) a command.
    ///
    /// The handshake finishes on the attendant's task, so there is a real
    /// happens-before to wait on rather than a duration to guess at. The
    /// deadline is a failure deadline.
    async fn until(&self, command: &str, present: bool) {
        let id = CommandId::new(command.to_string()).unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while self.registry.contains(&id) != present {
            assert!(
                std::time::Instant::now() < deadline,
                "{command} was {} on the bus after 5s",
                if present { "never" } else { "still" }
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    /// Dispatch a command and return (outcome, progress it reported).
    async fn call(&self, command: &str, params: Value) -> (Result<Value, String>, Vec<Progress>) {
        let request = Request::new(
            CommandId::new(command.to_string()).unwrap(),
            params,
            Issuer::operator(),
        );
        let Ok(Admitted::Running(running)) = self.bus.dispatch(request) else {
            panic!("{command} was not admitted");
        };
        let (mut rx, done) = running.split();
        let collect = tokio::spawn(async move {
            let mut seen = Vec::new();
            while let Some(p) = rx.recv().await {
                seen.push(p);
            }
            seen
        });
        let completion = done.await;
        let progress = collect.await.unwrap();
        (completion.outcome.map_err(|e| e.to_string()), progress)
    }
}

struct FakeApp {
    name: String,
    stream: BufReader<UnixStream>,
}

impl FakeApp {
    async fn send(&mut self, frame: Value) {
        let line = format!("{frame}\n");
        self.stream
            .get_mut()
            .write_all(line.as_bytes())
            .await
            .unwrap();
        self.stream.get_mut().flush().await.unwrap();
    }

    async fn recv(&mut self) -> Value {
        let mut line = String::new();
        let n = self.stream.read_line(&mut line).await.unwrap();
        assert!(n > 0, "the core closed the connection to {}", self.name);
        serde_json::from_str(&line).unwrap_or_else(|e| panic!("bad frame {line:?}: {e}"))
    }
}

// ---------------------------------------------------------------------------
// The bridge onto the bus
// ---------------------------------------------------------------------------

#[tokio::test]
async fn an_app_that_registers_puts_its_commands_on_the_bus() {
    let rig = rig(&["demo"]).await;
    assert_eq!(rig.host.installed().len(), 1, "discovery found no app");

    let _app = rig.connect("demo").await;
    rig.until("demo.echo", true).await;
    rig.until("demo.wipe", true).await;
}

#[tokio::test]
async fn the_risk_an_app_declared_is_the_risk_the_bus_enforces() {
    let rig = rig(&["demo"]).await;
    let _app = rig.connect("demo").await;
    rig.until("demo.wipe", true).await;

    let echo = rig
        .registry
        .spec(&CommandId::new("demo.echo".to_string()).unwrap())
        .unwrap();
    let wipe = rig
        .registry
        .spec(&CommandId::new("demo.wipe".to_string()).unwrap())
        .unwrap();

    // An app cannot be quietly downgraded on the way to the bus: a
    // destructive command that arrived as `Write` would run without the
    // operator being asked and without an agent's call waiting in the Inbox.
    assert_eq!(echo.risk(), RiskLevel::ReadOnly);
    assert_eq!(wipe.risk(), RiskLevel::Destructive);
}

#[tokio::test]
async fn an_app_cannot_hide_its_parameters_from_the_audit_log() {
    // `redact_params` exists for a password prompt's keystrokes, decided by
    // the core for its own commands. An app asking for it would be a program
    // choosing what the operator's record of their own machine may contain.
    let rig = rig(&["demo"]).await;
    let _app = rig.connect("demo").await;
    rig.until("demo.echo", true).await;

    let spec = rig
        .registry
        .spec(&CommandId::new("demo.echo".to_string()).unwrap())
        .unwrap();
    assert!(!spec.redacts_params());
}

#[tokio::test]
async fn a_dispatch_reaches_the_app_and_its_answer_comes_back() {
    let rig = rig(&["demo"]).await;
    let mut app = rig.connect("demo").await;
    rig.until("demo.echo", true).await;

    let caller = tokio::spawn({
        let bus = Arc::clone(&rig.bus);
        async move {
            let request = Request::new(
                CommandId::new("demo.echo".to_string()).unwrap(),
                json!({ "say": "hello" }),
                Issuer::operator(),
            );
            let Ok(Admitted::Running(running)) = bus.dispatch(request) else {
                panic!("not admitted");
            };
            let (_rx, done) = running.split();
            done.await.outcome.map_err(|e| e.to_string())
        }
    });

    let call = app.recv().await;
    assert_eq!(call["frame"], "call");
    assert_eq!(call["command"], "demo.echo");
    assert_eq!(call["params"]["say"], "hello");
    let id = call["id"].as_u64().unwrap();

    app.send(json!({ "frame": "result", "id": id, "result": { "heard": "hello" } }))
        .await;

    let outcome = caller.await.unwrap();
    assert_eq!(outcome.unwrap()["heard"], "hello");
}

#[tokio::test]
async fn an_apps_error_is_the_commands_error_and_not_a_success() {
    let rig = rig(&["demo"]).await;
    let mut app = rig.connect("demo").await;
    rig.until("demo.echo", true).await;

    let caller = tokio::spawn({
        let bus = Arc::clone(&rig.bus);
        async move {
            let request = Request::new(
                CommandId::new("demo.echo".to_string()).unwrap(),
                json!({}),
                Issuer::operator(),
            );
            let Ok(Admitted::Running(running)) = bus.dispatch(request) else {
                panic!("not admitted");
            };
            let (_rx, done) = running.split();
            done.await.outcome.map_err(|e| e.to_string())
        }
    });

    let id = app.recv().await["id"].as_u64().unwrap();
    app.send(json!({ "frame": "result", "id": id, "error": "the disk is full" }))
        .await;

    let err = caller.await.unwrap().expect_err("an error, not a value");
    assert!(err.contains("the disk is full"), "got {err}");
}

#[tokio::test]
async fn an_answer_with_neither_a_result_nor_an_error_is_null_not_a_failure() {
    let rig = rig(&["demo"]).await;
    let mut app = rig.connect("demo").await;
    rig.until("demo.echo", true).await;

    let caller = tokio::spawn({
        let bus = Arc::clone(&rig.bus);
        async move {
            let request = Request::new(
                CommandId::new("demo.echo".to_string()).unwrap(),
                json!({}),
                Issuer::operator(),
            );
            let Ok(Admitted::Running(running)) = bus.dispatch(request) else {
                panic!("not admitted");
            };
            let (_rx, done) = running.split();
            done.await.outcome.map_err(|e| e.to_string())
        }
    });

    let id = app.recv().await["id"].as_u64().unwrap();
    app.send(json!({ "frame": "result", "id": id })).await;
    assert_eq!(caller.await.unwrap().unwrap(), Value::Null);
}

#[tokio::test]
async fn progress_from_an_app_reaches_the_dispatch_that_asked_for_it() {
    let rig = rig(&["demo"]).await;
    let mut app = rig.connect("demo").await;
    rig.until("demo.echo", true).await;

    let request = Request::new(
        CommandId::new("demo.echo".to_string()).unwrap(),
        json!({}),
        Issuer::operator(),
    );
    let Ok(Admitted::Running(running)) = rig.bus.dispatch(request) else {
        panic!("not admitted");
    };
    let (mut rx, done) = running.split();

    // `split` does not spawn: the command runs *inside* the completion
    // future, so a caller that waits on progress before polling that future
    // deadlocks — nothing is driving the command that would produce the
    // progress. `ws.rs` gets this right by spawning the forwarder and
    // awaiting the completion; this is the same shape the other way round.
    let finished = tokio::spawn(done);

    let id = app.recv().await["id"].as_u64().unwrap();
    app.send(json!({ "frame": "progress", "id": id, "fraction": 0.5, "message": "halfway" }))
        .await;

    let first = rx.recv().await.expect("a progress update");
    match first {
        Progress::Step {
            fraction, message, ..
        } => {
            assert_eq!(fraction, Some(0.5));
            assert_eq!(message, "halfway");
        }
        other => panic!("expected a step, got {other:?}"),
    }

    app.send(json!({ "frame": "result", "id": id, "result": null }))
        .await;
    assert!(finished.await.unwrap().succeeded());
}

#[tokio::test]
async fn progress_for_an_unknown_id_is_ignored_rather_than_fatal() {
    // A buggy app must not be able to take down its own connection and so
    // lose the commands still in flight on it.
    let rig = rig(&["demo"]).await;
    let mut app = rig.connect("demo").await;
    rig.until("demo.echo", true).await;

    app.send(json!({ "frame": "progress", "id": 9999, "message": "nobody asked" }))
        .await;
    app.send(json!({ "frame": "result", "id": 9999, "result": null }))
        .await;

    // Still connected, still on the bus, and still able to answer a real call.
    let (outcome, _) = tokio::join!(rig.call("demo.echo", json!({})), async {
        let id = app.recv().await["id"].as_u64().unwrap();
        app.send(json!({ "frame": "result", "id": id, "result": "fine" }))
            .await;
    })
    .0;
    assert_eq!(outcome.unwrap(), json!("fine"));
}

// ---------------------------------------------------------------------------
// The lifecycle
// ---------------------------------------------------------------------------

#[tokio::test]
async fn an_apps_commands_leave_the_bus_the_moment_it_disconnects() {
    // The invariant: a palette entry for a dead app is a command that fails
    // when clicked, and the failure looks like Deckjack's rather than the
    // app's.
    let rig = rig(&["demo"]).await;
    let app = rig.connect("demo").await;
    rig.until("demo.echo", true).await;

    drop(app);
    rig.until("demo.echo", false).await;
    rig.until("demo.wipe", false).await;
}

#[tokio::test]
async fn an_app_that_reconnects_gets_its_commands_back() {
    let rig = rig(&["demo"]).await;
    let first = rig.connect("demo").await;
    rig.until("demo.echo", true).await;
    drop(first);
    rig.until("demo.echo", false).await;

    // The listener outlives any one connection, which is what makes a restart
    // a reconnect rather than a permanently unlaunchable app.
    let _second = rig.connect("demo").await;
    rig.until("demo.echo", true).await;
}

#[tokio::test]
async fn registering_twice_on_one_connection_is_refused() {
    // Registration happens once, inside the handshake, where the manifest is
    // checked against it. A second one is an app trying to change what it is
    // after that check.
    let rig = rig(&["demo"]).await;
    let mut app = rig.connect("demo").await;
    rig.until("demo.echo", true).await;

    app.send(json!({
        "frame": "register",
        "protocol": apps::PROTOCOL_VERSION,
        "app": "demo",
        "version": "1.0.0",
        "commands": [],
    }))
    .await;

    rig.until("demo.echo", false).await;
}

#[tokio::test]
async fn two_apps_do_not_share_a_socket_or_a_namespace() {
    let rig = rig(&["alpha", "beta"]).await;
    let _a = rig.connect("alpha").await;
    let _b = rig.connect("beta").await;
    rig.until("alpha.echo", true).await;
    rig.until("beta.echo", true).await;

    drop(_a);
    rig.until("alpha.echo", false).await;
    // Beta is untouched: `remove_namespace` is per app, and one app crashing
    // must not clear another's palette entries.
    assert!(
        rig.registry
            .contains(&CommandId::new("beta.echo".to_string()).unwrap()),
        "one app disconnecting took another's commands away"
    );
}

#[tokio::test]
async fn a_command_dispatched_to_a_disconnected_app_is_refused_not_hung() {
    let rig = rig(&["demo"]).await;
    let app = rig.connect("demo").await;
    rig.until("demo.echo", true).await;

    // Registered-but-gone is reachable: the connection can end between the
    // registry lookup and the send. Forced here by dropping the app and
    // dispatching before the attendant has cleared the namespace.
    let id = CommandId::new("demo.echo".to_string()).unwrap();
    drop(app);

    // Whichever side wins the race, the answer must arrive. Hanging is the
    // one outcome that is never acceptable: `terminal.read` taught that a
    // dispatch with no answer is a client that waits forever.
    let deadline = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if !rig.registry.contains(&id) {
                return;
            }
            let (outcome, _) = rig.call("demo.echo", json!({})).await;
            if outcome.is_err() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await;
    assert!(deadline.is_ok(), "a dispatch to a gone app never answered");
}

// ---------------------------------------------------------------------------
// The UI mount
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_ui_asset_is_fetched_from_the_app_itself() {
    use base64::Engine as _;

    let rig = rig(&["demo"]).await;
    let mut app = rig.connect("demo").await;
    rig.until("demo.echo", true).await;

    // `ui_asset` blocks its thread on the app's reply, as its own contract
    // says it may, so it runs on the blocking pool here exactly as the UI
    // router runs it.
    let host = Arc::clone(&rig.host);
    let fetch = tokio::task::spawn_blocking(move || {
        use deckjack_core::apps::UiHost as _;
        host.ui_asset("demo", "index.html")
    });

    let request = app.recv().await;
    assert_eq!(request["frame"], "ui");
    assert_eq!(request["path"], "index.html");
    let id = request["id"].as_u64().unwrap();

    app.send(json!({
        "frame": "ui",
        "id": id,
        "asset": {
            "content_type": "text/html; charset=utf-8",
            "body": base64::engine::general_purpose::STANDARD.encode("<h1>demo</h1>"),
            "immutable": false,
        },
    }))
    .await;

    let asset = fetch.await.unwrap().unwrap().expect("an asset");
    assert_eq!(asset.content_type, "text/html; charset=utf-8");
    assert_eq!(String::from_utf8(asset.bytes).unwrap(), "<h1>demo</h1>");
}

#[tokio::test]
async fn an_app_with_no_such_asset_is_a_404_and_not_a_failure() {
    let rig = rig(&["demo"]).await;
    let mut app = rig.connect("demo").await;
    rig.until("demo.echo", true).await;

    let host = Arc::clone(&rig.host);
    let fetch = tokio::task::spawn_blocking(move || {
        use deckjack_core::apps::UiHost as _;
        host.ui_asset("demo", "nope.js")
    });

    let id = app.recv().await["id"].as_u64().unwrap();
    app.send(json!({ "frame": "ui", "id": id })).await;

    assert!(
        fetch.await.unwrap().unwrap().is_none(),
        "a missing asset should be None, not an error"
    );
}

#[tokio::test]
async fn an_asset_request_to_an_app_that_is_not_connected_says_so() {
    use deckjack_core::apps::UiHost as _;
    let rig = rig(&["demo"]).await;
    let host = Arc::clone(&rig.host);
    let answer = tokio::task::spawn_blocking(move || host.ui_asset("demo", "index.html"))
        .await
        .unwrap();
    assert!(
        matches!(answer, Err(apps::UiError::Unreachable { .. })),
        "expected an unreachable error"
    );
}

// ---------------------------------------------------------------------------
// A namespace is not owned by one registrant
// ---------------------------------------------------------------------------

#[tokio::test]
async fn an_app_losing_its_connection_does_not_take_the_cores_commands_with_it() {
    // The defect this is here for: the core registers `files.list` itself,
    // the Files *app* is called `files`, and clearing "the app's namespace"
    // on disconnect took the core's whole file API with it. The Files app
    // worked, and then every file command vanished from the bus.
    let rig = rig(&["demo"]).await;

    // Stand in for the core's own commands in the app's namespace. Registered
    // before the app connects, exactly as `main.rs` registers files.*.
    let core_command = CommandId::new("demo.core_owned".to_string()).unwrap();
    rig.registry
        .register_declared(
            deckjack_core::command::Declaration {
                id: core_command.clone(),
                title: "Something the core provides".to_string(),
                risk: RiskLevel::ReadOnly,
                params: schemars::Schema::try_from(json!({
                    "type": "object", "properties": {}
                }))
                .unwrap(),
                target: deckjack_core::command::Target::ThisNode,
                redact_params: false,
            },
            CoreOwned,
        )
        .expect("the core's own command registers");

    let app = rig.connect("demo").await;
    rig.until("demo.echo", true).await;
    assert!(rig.registry.contains(&core_command));

    drop(app);
    rig.until("demo.echo", false).await;

    assert!(
        rig.registry.contains(&core_command),
        "an app disconnecting removed a command the core had registered in the same namespace"
    );
}

/// A command the core owns, for the test above.
struct CoreOwned;

impl deckjack_core::command::DynCommand for CoreOwned {
    fn run(
        &self,
        _params: Value,
        _cx: deckjack_core::command::Context,
    ) -> deckjack_core::command::BoxFuture<
        'static,
        Result<Value, deckjack_core::command::CommandError>,
    > {
        Box::pin(async { Ok(Value::Null) })
    }
}

// ---------------------------------------------------------------------------
// What the shell's dock is built from
// ---------------------------------------------------------------------------

/// Run `system.apps` through the bus, as the shell does.
async fn system_apps(rig: &Rig) -> Value {
    wiring::register_apps(&rig.registry, Arc::clone(&rig.host)).expect("app commands register");
    let request = Request::new(
        CommandId::new("system.apps".to_string()).unwrap(),
        json!({}),
        Issuer::operator(),
    );
    let Ok(Admitted::Running(running)) = rig.bus.dispatch(request) else {
        panic!("system.apps was not admitted");
    };
    let (_rx, done) = running.split();
    done.await.outcome.expect("system.apps answers")
}

#[tokio::test]
async fn system_apps_hands_the_shell_each_apps_dock_fields() {
    // The shell used to read its dock from a list compiled into the bundle,
    // so an app installed after the build could never reach it. What it
    // reads now is this answer, built from the manifest on disk.
    let rig = rig_with(&[
        (
            "photo",
            format!(
                "[app]\nname = \"photo\"\nversion = \"2.0.0\"\nprotocol = {}\n\
             title = \"Photos\"\ntag = \"Ph\"\nsummary = \"Edit photographs\"\n\
             min_width = 360\ndefault_width = 900\ndefault_height = 600\nwide = false\n",
                apps::PROTOCOL_VERSION
            ),
        ),
        ("plain", manifest("plain")),
    ])
    .await;

    let value = system_apps(&rig).await;
    let list = value["apps"].as_array().expect("an apps list");
    let photo = list
        .iter()
        .find(|a| a["name"] == "photo")
        .unwrap_or_else(|| panic!("photo is not in {value}"));
    assert_eq!(photo["title"], "Photos");
    assert_eq!(photo["tag"], "Ph");
    assert_eq!(photo["summary"], "Edit photographs");
    assert_eq!(photo["min_width"], 360);
    assert_eq!(photo["default_width"], 900);
    assert_eq!(photo["default_height"], 600);
    assert_eq!(photo["compact"], true);
    assert_eq!(photo["wide"], false);

    // Absent fields arrive as null, not as a default the core made up: the
    // shell owns the defaults, and two places holding them would drift.
    let plain = list
        .iter()
        .find(|a| a["name"] == "plain")
        .unwrap_or_else(|| panic!("plain is not in {value}"));
    assert_eq!(plain["tag"], Value::Null);
    assert_eq!(plain["summary"], Value::Null);
    assert_eq!(plain["default_width"], Value::Null);
    assert_eq!(plain["compact"], true);
}

// ---------------------------------------------------------------------------
// An app awaiting approval is discoverable, and says how to approve it
// ---------------------------------------------------------------------------

/// An app that needs approving. Its network grant names a port: a host list
/// with no ports opens every port, the core's own included, and is refused
/// at approval while the core listens on loopback (`apps::reaches_core`).
fn needy(name: &str) -> String {
    format!(
        "[app]\nname = \"{name}\"\nversion = \"1.0.0\"\nprotocol = {}\ntitle = \"Notes\"\n\n\
         [needs]\nnetwork = [\"cloud.example.net\"]\nconnect_tcp = [443]\n",
        apps::PROTOCOL_VERSION
    )
}

#[tokio::test]
async fn system_apps_says_which_apps_await_approval_and_what_they_ask() {
    // After the fingerprint moved to SHA-256 every approved app went stale,
    // and nothing the shell could see said so: the window failed to open and
    // the log named a CLI command that does not exist. The shell builds its
    // "Approve <app>" entries from this answer.
    let rig = rig_with(&[("notes", needy("notes")), ("plain", manifest("plain"))]).await;

    let value = system_apps(&rig).await;
    let list = value["apps"].as_array().expect("an apps list");
    let find = |name: &str| {
        list.iter()
            .find(|a| a["name"] == name)
            .unwrap_or_else(|| panic!("{name} is not in {value}"))
            .clone()
    };
    let notes = find("notes");
    assert_eq!(notes["approval"], "required", "{notes}");
    assert!(
        notes["asks"]
            .as_str()
            .is_some_and(|a| a.contains("cloud.example.net")),
        "{notes}"
    );
    assert_eq!(find("plain")["approval"], "not-needed");

    rig.host.approve("notes").expect("the operator approves");
    // Registered by the first call; asked again directly.
    let request = Request::new(
        CommandId::new("system.apps".to_string()).unwrap(),
        json!({}),
        Issuer::operator(),
    );
    let Ok(Admitted::Running(running)) = rig.bus.dispatch(request) else {
        panic!("system.apps was not admitted");
    };
    let (_rx, done) = running.split();
    let value = done.await.outcome.expect("system.apps answers");
    let notes = value["apps"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["name"] == "notes")
        .cloned()
        .unwrap();
    assert_eq!(notes["approval"], "approved", "{notes}");
}

#[tokio::test]
async fn opening_an_app_awaiting_approval_says_how_to_approve_it() {
    let rig = rig_with(&[("notes", needy("notes"))]).await;
    let windows = wiring::register_windows(&rig.registry, "apphost-node".to_string())
        .expect("the window commands register");
    windows.attach_app_host(Arc::clone(&rig.host));

    let request = Request::new(
        CommandId::new("shell.window.open".to_string()).unwrap(),
        json!({ "id": "w1", "app": "notes" }),
        Issuer::operator(),
    );
    let Ok(Admitted::Running(running)) = rig.bus.dispatch(request) else {
        panic!("shell.window.open was not admitted");
    };
    let (_rx, done) = running.split();
    let why = done
        .await
        .outcome
        .expect_err("an unapproved app must not open")
        .to_string();
    assert!(why.contains("Approve Notes"), "{why}");
    assert!(why.contains("cloud.example.net"), "{why}");
    assert!(!why.contains("see the log"), "{why}");
}
