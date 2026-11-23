//! An app's window on the bus: `Issuer::AppUi`, held to its own scope.
//!
//! docs/APP-UI-ISOLATION.md §4.6, stage 3. Nothing sends `as_app` yet, so
//! the rules are asked here directly of the bus, over the real pieces that
//! decide them: a real `AppHost`, a real Unix socket and handshake (the fake
//! app is a task speaking the protocol, as in `tests/apphost.rs`), the real
//! approval store, and the core's real file commands. An app's own scope is
//! whatever its process registered at that handshake; anything beyond it is
//! the `[ui] invoke` list the operator approved; `HARD_DENY` is never
//! reachable.
//!
//! Every refusal has a control in the same fixture that passes first, and
//! every refusal asserts `Refused::NotPermitted` and the effect (the command
//! never ran, the file was not returned), not only that something failed.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

use deckjack_core::apphost::AppHost;
use deckjack_core::apps;
use deckjack_core::audit::{EventsJournal, StoreSink};
use deckjack_core::auth::SessionId;
use deckjack_core::command::{
    Admitted, Approval, Bus, Command, CommandError, CommandId, Context, Issuer, NodeId,
    OsEnv as BusEnv, PendingId, Refused, Registry, Request, RiskLevel, Submission,
};
use deckjack_core::events::{Issuer as LogIssuer, Query};
use deckjack_core::store::{Store, StoreGrants};
use deckjack_core::wiring::NoParams;
use deckjack_core::ws::ClientFrame;

// ---------------------------------------------------------------------------
// A node with apps installed and the core's file commands on the bus
// ---------------------------------------------------------------------------

struct Rig {
    host: Arc<AppHost>,
    registry: Arc<Registry>,
    bus: Arc<Bus>,
    store: Arc<std::sync::Mutex<Store>>,
    _dir: tempfile::TempDir,
}

/// An app with one read command of its own, `<name>.ping`, and a
/// destructive one, `<name>.wipe`, plus whatever `[ui]` the test gives it.
fn manifest(name: &str, ui: &str) -> String {
    format!(
        r#"[app]
name = "{name}"
version = "1.0.0"
protocol = 1
lifecycle = "on-demand"

[[commands]]
id = "{name}.ping"
summary = "Answer"
risk = "read"

[[commands]]
id = "{name}.wipe"
summary = "Delete everything"
risk = "destructive"
{ui}"#
    )
}

fn install_app(install: &Path, name: &str, text: &str) {
    let dir = install.join("apps").join(name);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("app.toml"), text).unwrap();
    // Discovery wants an executable; it is never spawned here.
    let binary = dir.join(format!("deckjack-{name}"));
    std::fs::write(&binary, "#!/bin/sh\nexit 0\n").unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o755)).unwrap();
}

async fn rig(apps_to_install: &[(&str, &str)]) -> Rig {
    let dir = tempfile::tempdir().unwrap();
    let work = dir.path().join("work");
    std::fs::create_dir_all(&work).unwrap();
    std::fs::write(work.join("hello.txt"), "the file's own words").unwrap();
    let install = dir.path().join("install");
    std::fs::create_dir_all(&install).unwrap();
    for (name, ui) in apps_to_install {
        install_app(&install, name, &manifest(name, ui));
    }

    let store = Arc::new(std::sync::Mutex::new(Store::open(&work).unwrap()));
    let registry = Arc::new(Registry::new());
    deckjack_core::files::register_commands(&registry, &work, "appui-node", &install)
        .expect("the core's file commands");
    let bus = Arc::new(
        Bus::new(
            Arc::clone(&registry),
            NodeId::new("appui-node"),
            Arc::new(BusEnv),
        )
        .with_journal(EventsJournal::new(Arc::clone(&store))),
    );

    let host = AppHost::new(
        apps::Host::new(
            install.clone(),
            work.clone(),
            // Where the core listens, for the D2 grant rule; no app here
            // asks for TCP, so the address is never reached.
            apps::CoreDoor::new("127.0.0.1:8821".parse().unwrap(), vec![]),
            apps::OsEnv::new(),
            apps::Policy::default(),
            Box::new(StoreGrants::new(Arc::clone(&store))),
        ),
        Arc::clone(&registry),
        Duration::from_millis(80),
    );
    host.start().await;

    Rig {
        host,
        registry,
        bus,
        store,
        _dir: dir,
    }
}

/// A browser session for the window to be running in.
fn session() -> SessionId {
    SessionId::generate().expect("a session id")
}

fn window(app: &str) -> Issuer {
    Issuer::app_ui(app, session())
}

fn id(s: &str) -> CommandId {
    CommandId::new(s.to_string()).unwrap()
}

impl Rig {
    /// Connect as `name`'s process and register its two commands.
    async fn connect(&self, name: &str) -> FakeApp {
        // A link made the way a launch makes one: the other end is
        // already with this app's attendant.
        let link = self
            .host
            .link(name)
            .unwrap_or_else(|e| panic!("a link for {name}: {e}"));
        link.set_nonblocking(true).unwrap();
        let stream = UnixStream::from_std(link).unwrap();
        let mut app = FakeApp {
            name: name.to_string(),
            stream: BufReader::new(stream),
        };
        let hello = app.recv().await;
        assert_eq!(hello["frame"], "hello", "got {hello}");
        app.send(json!({
            "frame": "register",
            "protocol": 1,
            "app": name,
            "version": "1.0.0",
            "commands": [
                {
                    "id": format!("{name}.ping"),
                    "summary": "Answer",
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
        self.until(&format!("{name}.ping"), true).await;
        app
    }

    /// Wait for the handshake to land (or the disconnect to). A failure
    /// deadline, not pacing.
    async fn until(&self, command: &str, present: bool) {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while self.registry.contains(&id(command)) != present {
            assert!(
                std::time::Instant::now() < deadline,
                "{command} was {} on the bus after 5s",
                if present { "never" } else { "still" }
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    /// Run a core command (one no fake app has to answer) and return its
    /// value, or the refusal.
    async fn run(&self, command: &str, params: Value, issuer: Issuer) -> Result<Value, Refused> {
        self.run_request(Request::new(id(command), params, issuer))
            .await
    }

    /// [`Rig::run`] for a request built by the caller (one already confirmed,
    /// say).
    async fn run_request(&self, request: Request) -> Result<Value, Refused> {
        let command = request.command.to_string();
        match self.bus.dispatch(request)? {
            Admitted::Running(running) => {
                let (_rx, done) = running.split();
                // A failure deadline: a call that wrongly reaches a fake app
                // nobody is answering would otherwise hang the suite.
                let completion = tokio::time::timeout(Duration::from_secs(10), done)
                    .await
                    .unwrap_or_else(|_| panic!("{command} was admitted and never finished"));
                Ok(completion.outcome.expect("the command itself succeeds"))
            }
            other => panic!("{command} was admitted as {other:?}, not run"),
        }
    }

    fn read_hello(&self, issuer: Issuer) -> impl Future<Output = Result<Value, Refused>> + '_ {
        self.run("files.read", json!({ "path": "/hello.txt" }), issuer)
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
        // A failure deadline: a call the bus wrongly refused never arrives.
        let n = tokio::time::timeout(Duration::from_secs(10), self.stream.read_line(&mut line))
            .await
            .unwrap_or_else(|_| panic!("{} was sent nothing for 10s", self.name))
            .unwrap();
        assert!(n > 0, "the core closed the connection to {}", self.name);
        serde_json::from_str(&line).unwrap_or_else(|e| panic!("bad frame {line:?}: {e}"))
    }

    /// Answer the next call with `{"pong": <command>}`.
    async fn answer(&mut self) {
        let call = self.recv().await;
        assert_eq!(call["frame"], "call", "got {call}");
        let id = call["id"].as_u64().unwrap();
        let command = call["command"].clone();
        self.send(json!({ "frame": "result", "id": id, "result": { "pong": command } }))
            .await;
    }
}

fn not_permitted(result: Result<Value, Refused>, what: &str) -> String {
    match result {
        Err(refused @ Refused::NotPermitted { .. }) => refused.to_string(),
        other => panic!("{what}: expected not_permitted, got {other:?}"),
    }
}

/// A core command that counts its runs, for proving a refused call did not
/// run rather than only that the bus said no.
struct Probe {
    runs: Arc<AtomicUsize>,
}

macro_rules! probe {
    ($name:ident, $id:literal) => {
        struct $name(Probe);
        impl Command for $name {
            const ID: &'static str = $id;
            const TITLE: &'static str = "Probe";
            const RISK: RiskLevel = RiskLevel::ReadOnly;
            type Params = NoParams;
            type Output = Value;
            async fn run(&self, _p: NoParams, _cx: Context) -> Result<Value, CommandError> {
                self.0.runs.fetch_add(1, Ordering::SeqCst);
                Ok(json!({ "ran": true }))
            }
        }
    };
}

probe!(SystemProbe, "system.probe");
probe!(NotesSneak, "notes.sneak");

// ---------------------------------------------------------------------------
// Its own scope
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_window_runs_the_commands_its_own_process_registered() {
    let rig = rig(&[("notes", "")]).await;
    let mut app = rig.connect("notes").await;

    let caller = tokio::spawn({
        let bus = Arc::clone(&rig.bus);
        async move {
            let Ok(Admitted::Running(running)) =
                bus.dispatch(Request::new(id("notes.ping"), json!({}), window("notes")))
            else {
                panic!("the window's own command was not admitted");
            };
            let (_rx, done) = running.split();
            done.await.outcome.map_err(|e| e.to_string())
        }
    });
    app.answer().await;
    assert_eq!(caller.await.unwrap().unwrap()["pong"], "notes.ping");
}

#[tokio::test]
async fn another_apps_commands_are_not_in_a_windows_scope() {
    let rig = rig(&[("notes", ""), ("tasks", "")]).await;
    let _notes = rig.connect("notes").await;
    let mut tasks = rig.connect("tasks").await;

    // The control: the operator reaches tasks.ping.
    let caller = tokio::spawn({
        let bus = Arc::clone(&rig.bus);
        async move {
            let Ok(Admitted::Running(running)) = bus.dispatch(Request::new(
                id("tasks.ping"),
                json!({}),
                Issuer::operator(),
            )) else {
                panic!("the operator's call was not admitted");
            };
            running.finish().await.outcome.is_ok()
        }
    });
    tasks.answer().await;
    assert!(caller.await.unwrap());

    let message = not_permitted(
        rig.run("tasks.ping", json!({}), window("notes")).await,
        "notes' window calling tasks' command",
    );
    assert!(message.contains("tasks.ping"), "{message}");
}

#[tokio::test]
async fn a_destructive_command_in_scope_is_asked_about_rather_than_run() {
    // Inside its scope a window is routed as the operator whose browser it is
    // in: a destructive call gets a confirmation, never a silent run and
    // never an agent's Inbox.
    let rig = rig(&[("notes", "")]).await;
    let _app = rig.connect("notes").await;
    let admitted = rig
        .bus
        .dispatch(Request::new(id("notes.wipe"), json!({}), window("notes")))
        .expect("in scope");
    assert!(
        matches!(admitted, Admitted::NeedsConfirmation { .. }),
        "got {admitted:?}"
    );
}

// ---------------------------------------------------------------------------
// Beyond it: only the approved `[ui] invoke`
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_core_command_is_refused_without_ui_and_runs_with_it_once_approved() {
    let rig = rig(&[
        ("plain", ""),
        ("viewer", "\n[ui]\ninvoke = [\"files.read\"]\n"),
    ])
    .await;
    rig.host
        .approve("viewer")
        .expect("the operator approves the viewer");
    let _plain = rig.connect("plain").await;
    let _viewer = rig.connect("viewer").await;

    let read = rig
        .read_hello(window("viewer"))
        .await
        .expect("declared and approved");
    assert_eq!(read["content"], "the file's own words");

    let message = not_permitted(rig.read_hello(window("plain")).await, "no [ui] at all");
    assert!(message.contains("files.read"), "{message}");
}

#[tokio::test]
async fn a_declared_but_unapproved_ui_list_reaches_nothing() {
    // The list comes from the operator's stored approval, not the manifest on
    // disk: an app that wrote `files.read` into its manifest and was never
    // approved gets nothing from writing it.
    let rig = rig(&[("viewer", "\n[ui]\ninvoke = [\"files.read\"]\n")]).await;
    let _viewer = rig.connect("viewer").await;
    not_permitted(rig.read_hello(window("viewer")).await, "unapproved [ui]");

    // The control, in the same fixture: approve, reconnect, and it runs.
    rig.host.approve("viewer").unwrap();
    drop(_viewer);
    rig.until("viewer.ping", false).await;
    let _viewer = rig.connect("viewer").await;
    rig.read_hello(window("viewer"))
        .await
        .expect("approved and reconnected");
}

#[tokio::test]
async fn a_wildcard_does_not_pick_up_a_core_command_registered_later() {
    let rig = rig(&[("notes", "\n[ui]\ninvoke = [\"notes.*\"]\n")]).await;
    rig.host.approve("notes").unwrap();
    let mut app = rig.connect("notes").await;

    // The core puts a command in the app's namespace after the app came up.
    // It shares the prefix `notes.` and the wildcard, and it is not the app's.
    let runs = Arc::new(AtomicUsize::new(0));
    rig.registry
        .register(NotesSneak(Probe {
            runs: Arc::clone(&runs),
        }))
        .unwrap();

    // The control: its own registered command runs.
    let caller = tokio::spawn({
        let bus = Arc::clone(&rig.bus);
        async move {
            let Ok(Admitted::Running(running)) =
                bus.dispatch(Request::new(id("notes.ping"), json!({}), window("notes")))
            else {
                panic!("not admitted");
            };
            running.finish().await.outcome.is_ok()
        }
    });
    app.answer().await;
    assert!(caller.await.unwrap());

    not_permitted(
        rig.run("notes.sneak", json!({}), window("notes")).await,
        "a core command under the app's prefix",
    );
    assert_eq!(runs.load(Ordering::SeqCst), 0, "it ran anyway");
    rig.run("notes.sneak", json!({}), Issuer::operator())
        .await
        .expect("the control: the operator runs it");
    assert_eq!(runs.load(Ordering::SeqCst), 1);
}

// ---------------------------------------------------------------------------
// Never: HARD_DENY
// ---------------------------------------------------------------------------

#[tokio::test]
async fn hard_denied_namespaces_are_refused_whatever_is_registered() {
    let rig = rig(&[("viewer", "\n[ui]\ninvoke = [\"files.read\"]\n")]).await;
    rig.host.approve("viewer").unwrap();
    let _viewer = rig.connect("viewer").await;
    let runs = Arc::new(AtomicUsize::new(0));
    rig.registry
        .register(SystemProbe(Probe {
            runs: Arc::clone(&runs),
        }))
        .unwrap();

    rig.read_hello(window("viewer"))
        .await
        .expect("the control: the same window's approved call");

    let message = not_permitted(
        rig.run("system.probe", json!({}), window("viewer")).await,
        "system.*",
    );
    assert!(message.contains("never available"), "{message}");
    assert_eq!(runs.load(Ordering::SeqCst), 0);

    // Refused as not permitted even where nothing is registered, so a window
    // learns nothing by probing the hard-denied namespaces.
    for command in [
        "terminal.open",
        "system.recovery_codes",
        "shell.app.open",
        "auth.logout",
    ] {
        not_permitted(rig.run(command, json!({}), window("viewer")).await, command);
    }

    rig.run("system.probe", json!({}), Issuer::operator())
        .await
        .expect("the control: the operator runs it");
    assert_eq!(runs.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn the_inbox_door_holds_a_window_to_its_scope_too() {
    // `dispatch_approved` is the bus's other way in. A window's call is never
    // queued, but a scope that held at one public door and not the other
    // would not hold.
    let rig = rig(&[
        ("plain", ""),
        ("viewer", "\n[ui]\ninvoke = [\"files.read\"]\n"),
    ])
    .await;
    rig.host.approve("viewer").unwrap();
    let _plain = rig.connect("plain").await;
    let _viewer = rig.connect("viewer").await;
    let approved = |issuer| {
        rig.bus.dispatch_approved(
            Request::new(id("files.read"), json!({ "path": "/hello.txt" }), issuer),
            Approval::granted(PendingId::new("p1")),
        )
    };

    let running = approved(window("viewer")).expect("the control: in scope");
    assert!(running.finish().await.succeeded());
    assert!(matches!(
        approved(window("plain")),
        Err(Refused::NotPermitted { .. })
    ));
}

#[tokio::test]
async fn a_confirmation_does_not_widen_a_windows_scope() {
    // A window's call that needs confirming is answered with a ticket, and
    // redeeming it reaches the bus as a confirmed request. The operator's
    // "yes" covers that one call; it must not carry it past the scope. A
    // ticket issued while the app was approved and running is redeemed up
    // to a minute later, by which time the app may be revoked.
    let rig = rig(&[
        ("plain", ""),
        ("viewer", "\n[ui]\ninvoke = [\"files.read\"]\n"),
    ])
    .await;
    rig.host.approve("viewer").unwrap();
    let _plain = rig.connect("plain").await;
    let _viewer = rig.connect("viewer").await;
    let runs = Arc::new(AtomicUsize::new(0));
    rig.registry
        .register(SystemProbe(Probe {
            runs: Arc::clone(&runs),
        }))
        .unwrap();
    let confirmed = |command: &str, params: Value, issuer| {
        rig.run_request(Request::new(id(command), params, issuer).confirmed())
    };

    let read = confirmed(
        "files.read",
        json!({ "path": "/hello.txt" }),
        window("viewer"),
    )
    .await
    .expect("the control: confirmed and in scope");
    assert_eq!(read["content"], "the file's own words");

    let message = not_permitted(
        confirmed("system.probe", json!({}), window("viewer")).await,
        "a confirmed call into HARD_DENY",
    );
    assert!(message.contains("never available"), "{message}");
    assert_eq!(runs.load(Ordering::SeqCst), 0, "system.probe ran");
    not_permitted(
        confirmed("system.recovery_codes", json!({}), window("viewer")).await,
        "a confirmed system.recovery_codes",
    );
    not_permitted(
        confirmed(
            "files.read",
            json!({ "path": "/hello.txt" }),
            window("plain"),
        )
        .await,
        "a confirmed call outside the [ui] invoke list",
    );

    rig.host.revoke("viewer").expect("the operator revokes it");
    not_permitted(
        confirmed(
            "files.read",
            json!({ "path": "/hello.txt" }),
            window("viewer"),
        )
        .await,
        "a confirmed call from a revoked app's window",
    );
}

// ---------------------------------------------------------------------------
// A window with no process behind it
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_window_whose_app_has_stopped_may_call_nothing() {
    let rig = rig(&[("viewer", "\n[ui]\ninvoke = [\"files.read\"]\n")]).await;
    rig.host.approve("viewer").unwrap();
    let app = rig.connect("viewer").await;
    rig.read_hello(window("viewer"))
        .await
        .expect("the control: while the app is up");

    drop(app);
    rig.until("viewer.ping", false).await;
    // The registry drops the commands and the scope on the same disconnect;
    // wait out the scope too, with the same failure deadline.
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while rig.registry.app_ui_invoke("viewer").is_some() {
        assert!(
            std::time::Instant::now() < deadline,
            "the scope outlived the app"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let message = not_permitted(rig.read_hello(window("viewer")).await, "a stopped app");
    assert!(message.contains("running"), "{message}");

    not_permitted(
        rig.read_hello(window("ghost")).await,
        "an app never installed",
    );
    rig.read_hello(Issuer::operator())
        .await
        .expect("the control: the operator still reads it");
}

#[tokio::test]
async fn revoking_an_app_takes_its_windows_scope_even_while_its_process_holds_on() {
    // The fake app never exits on its own: it is the process that traps the
    // stop signal and keeps its socket open. Revoking the approval must take
    // the window's scope there and then, not when the socket happens to close.
    let rig = rig(&[("viewer", "\n[ui]\ninvoke = [\"files.read\"]\n")]).await;
    rig.host.approve("viewer").unwrap();
    let _viewer = rig.connect("viewer").await;
    rig.read_hello(window("viewer"))
        .await
        .expect("the control: approved and running");

    rig.host.revoke("viewer").expect("the operator revokes it");

    not_permitted(rig.read_hello(window("viewer")).await, "a revoked app");
    assert!(
        !rig.registry.contains(&id("viewer.ping")),
        "a revoked app's commands stayed on the bus"
    );
    rig.read_hello(Issuer::operator())
        .await
        .expect("the control: the operator still reads it");
}

// ---------------------------------------------------------------------------
// Attribution
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_audit_log_records_the_apps_window_and_the_browser_it_ran_in() {
    let rig = rig(&[("viewer", "\n[ui]\ninvoke = [\"files.read\"]\n")]).await;
    rig.host.approve("viewer").unwrap();
    let _viewer = rig.connect("viewer").await;
    let browser = session();

    rig.read_hello(Issuer::app_ui("viewer", browser.clone()))
        .await
        .unwrap();
    not_permitted(
        rig.run(
            "terminal.open",
            json!({}),
            Issuer::app_ui("viewer", browser.clone()),
        )
        .await,
        "terminal",
    );
    rig.read_hello(Issuer::operator()).await.unwrap();

    let rows = StoreSink::new(Arc::clone(&rig.store))
        .timeline(&Query::default())
        .expect("the timeline reads");
    let windows: Vec<_> = rows
        .iter()
        .filter(|row| {
            row.entry.issuer
                == LogIssuer::AppUi {
                    app: "viewer".to_string(),
                }
        })
        .collect();
    let commands: Vec<&str> = windows.iter().map(|r| r.entry.command.as_str()).collect();
    assert!(commands.contains(&"files.read"), "{commands:?}");
    assert!(
        commands.contains(&"terminal.open"),
        "the refusal is attributed too: {commands:?}"
    );
    let tag = Issuer::browser(browser).browser_session().unwrap().tag();
    for row in &windows {
        assert_eq!(row.entry.issuer.label(), "app-ui:viewer");
        assert_eq!(row.issuer_session.as_deref(), Some(tag.as_str()));
    }
    assert!(
        rows.iter()
            .any(|row| row.entry.issuer == LogIssuer::Operator && row.entry.command == "files.read"),
        "the control: the operator's read is the operator's"
    );

    // And the timeline command can be asked for only the app windows' rows,
    // or one app's, the way a panel would ask.
    deckjack_core::audit::register_timeline(&rig.registry, StoreSink::new(Arc::clone(&rig.store)))
        .unwrap();
    let issuers = |rows: Value| -> Vec<String> {
        rows.as_array()
            .expect("rows")
            .iter()
            .map(|row| row["issuer"].to_string())
            .collect()
    };
    let only_windows = rig
        .run(
            "system.timeline",
            json!({ "issuer": "app_ui" }),
            Issuer::operator(),
        )
        .await
        .expect("app_ui is an issuer the timeline filters by");
    let only_windows = issuers(only_windows);
    assert!(only_windows.len() >= 2, "{only_windows:?}");
    assert!(
        only_windows.iter().all(|i| i.contains("viewer")),
        "{only_windows:?}"
    );
    let one_app = rig
        .run(
            "system.timeline",
            json!({ "issuer": "app_ui", "session": "viewer" }),
            Issuer::operator(),
        )
        .await
        .unwrap();
    assert_eq!(issuers(one_app).len(), only_windows.len());
    let other_app = rig
        .run(
            "system.timeline",
            json!({ "issuer": "app_ui", "session": "ghost" }),
            Issuer::operator(),
        )
        .await
        .unwrap();
    assert!(issuers(other_app).is_empty(), "the filter names one app");
    let operator = rig
        .run(
            "system.timeline",
            json!({ "issuer": "operator" }),
            Issuer::operator(),
        )
        .await
        .unwrap();
    let operator = issuers(operator);
    assert!(!operator.is_empty(), "the control: the operator's rows");
    assert!(
        operator.iter().all(|i| !i.contains("viewer")),
        "{operator:?}"
    );
}

// ---------------------------------------------------------------------------
// Where `as_app` may come from
// ---------------------------------------------------------------------------

#[test]
fn only_the_browser_socket_frame_can_name_an_app() {
    // The socket's invoke frame carries it...
    let frame: ClientFrame = serde_json::from_value(json!({
        "type": "invoke", "id": "c1", "command": "files.list", "as_app": "filer",
    }))
    .unwrap();
    let ClientFrame::Invoke { as_app, .. } = frame else {
        panic!("not an invoke");
    };
    assert_eq!(as_app.as_deref(), Some("filer"));
    let frame: ClientFrame =
        serde_json::from_value(json!({ "type": "invoke", "id": "c1", "command": "files.list" }))
            .unwrap();
    let ClientFrame::Invoke { as_app, .. } = frame else {
        panic!("not an invoke");
    };
    assert_eq!(as_app, None, "absent means the shell itself");

    // ...and the submission every other door parses (CLI, control socket)
    // refuses it, so no other caller can make itself an app's window.
    serde_json::from_value::<Submission>(json!({ "command": "files.list" }))
        .expect("the control: a plain submission");
    assert!(
        serde_json::from_value::<Submission>(json!({ "command": "files.list", "as_app": "filer" }))
            .is_err()
    );
}
