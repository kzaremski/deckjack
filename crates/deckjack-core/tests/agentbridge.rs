//! An app calling the bus, and the MCP tool surface over the same bus.
//!
//! `tests/mcp.rs` proves the translation rules with its own `Catalog`,
//! `Dispatch` and `Grant` doubles, and every one of them passed while nothing
//! in the program implemented any of the three: a module whose whole argument
//! is "every tool an agent is sent is a registered command" had no registry
//! behind it. What is exercised here is the real assembly — a real
//! `Registry`, a real `Bus` with a real grant store, a real `AppHost` with a
//! real `AgentBridge` attached, and an "app" that is a task in this process
//! speaking the protocol over the same Unix socket a real one would.
//!
//! The harness follows `tests/apphost.rs`: nothing is spawned, so nothing
//! depends on a binary existing or a kernel that can confine it. Every app
//! here is `on-demand` and no window is opened, so the host never launches
//! anything and the attendant is simply waiting when the fake app connects.

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

use deckjack_core::agentbridge::{AgentBridge, app_identity, app_session, call_risk, tool_risk};
use deckjack_core::apphost::AppHost;
use deckjack_core::apps;
use deckjack_core::command::{
    Admitted, Bus, Command, CommandError, CommandId, Context, Enablement, Grant, Grants, Issuer,
    Journal, JournalError, LogEntry, MemoryGrants, NodeId, NodeScope, OsEnv as BusEnv, Pattern,
    Registry, Request, RiskLevel,
};
use deckjack_core::mcp::{GateReason, ToolRisk};
use deckjack_core::store::{Store, StoreGrants};
use deckjack_protocol::CallRisk;

const NODE: &str = "agentbridge-node";

// ---------------------------------------------------------------------------
// Commands to call
// ---------------------------------------------------------------------------

#[derive(serde::Deserialize, schemars::JsonSchema)]
struct EchoParams {
    say: String,
}

/// A read-only command that reports who called it.
///
/// The issuer is part of its result on purpose: attribution that only exists
/// in a log line is attribution nothing can assert on from inside a command,
/// and "the bus told the command who asked" is the property the Inbox and the
/// timeline both rest on.
struct Echo;

impl Command for Echo {
    const ID: &'static str = "probe.echo";
    const TITLE: &'static str = "Echo what you are given";
    const RISK: RiskLevel = RiskLevel::ReadOnly;
    type Params = EchoParams;
    type Output = Value;

    async fn run(&self, params: EchoParams, cx: Context) -> Result<Value, CommandError> {
        Ok(json!({
            "heard": params.say,
            "issuer_kind": cx.issuer.kind(),
            "issuer_name": cx.issuer.name(),
        }))
    }
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
struct NoParams {}

/// Destructive, so an agent's call — an app's included — must not run on the
/// strength of asking.
struct Wipe;

impl Command for Wipe {
    const ID: &'static str = "probe.wipe";
    const TITLE: &'static str = "Delete everything";
    const RISK: RiskLevel = RiskLevel::Destructive;
    type Params = NoParams;
    type Output = Value;

    async fn run(&self, _params: NoParams, _cx: Context) -> Result<Value, CommandError> {
        // Reached only if the Inbox gate let it through, which is what the
        // test asserting a refusal is about. Fails loudly rather than
        // quietly doing nothing, so a hole cannot pass as a pass.
        Err(CommandError::Failed(anyhow::anyhow!(
            "probe.wipe ran, which means a destructive command reached a backend \
             without the operator approving it"
        )))
    }
}

/// Registered but outside every grant below. The one a session is neither
/// offered nor allowed to call.
struct Secret;

impl Command for Secret {
    const ID: &'static str = "vault.secret";
    const TITLE: &'static str = "Read the vault";
    const RISK: RiskLevel = RiskLevel::ReadOnly;
    type Params = NoParams;
    type Output = Value;

    async fn run(&self, _params: NoParams, _cx: Context) -> Result<Value, CommandError> {
        Ok(json!({ "secret": "should never be reached from a grant that omits it" }))
    }
}

/// Disabled right now, which is neither a defect nor a permission decision.
struct Asleep;

impl Command for Asleep {
    const ID: &'static str = "probe.asleep";
    const TITLE: &'static str = "Nothing to do";
    const RISK: RiskLevel = RiskLevel::ReadOnly;
    type Params = NoParams;
    type Output = Value;

    fn enablement(&self) -> Enablement {
        Enablement::disabled("there is no selection")
    }

    async fn run(&self, _params: NoParams, _cx: Context) -> Result<Value, CommandError> {
        Ok(Value::Null)
    }
}

// ---------------------------------------------------------------------------
// A node with apps installed, a bus, and the bridge between them
// ---------------------------------------------------------------------------

/// The journal, kept so a test can read what the bus wrote down about who
/// asked for what.
struct Log {
    entries: Mutex<Vec<LogEntry>>,
}

impl Log {
    fn new() -> Arc<Log> {
        Arc::new(Log {
            entries: Mutex::new(Vec::new()),
        })
    }

    /// Everything recorded about one command, as (issuer kind, issuer name).
    fn issuers_of(&self, command: &str) -> Vec<(String, Option<String>)> {
        self.entries
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .filter(|entry| entry.command.as_str() == command)
            .map(|entry| {
                (
                    entry.issuer.kind().to_string(),
                    entry.issuer.name().map(str::to_string),
                )
            })
            .collect()
    }
}

impl Journal for Log {
    fn record(&self, entry: LogEntry) -> Result<(), JournalError> {
        self.entries
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(entry);
        Ok(())
    }
}

struct Rig {
    host: Arc<AppHost>,
    bridge: Arc<AgentBridge>,
    bus: Arc<Bus>,
    registry: Arc<Registry>,
    grants: Arc<MemoryGrants>,
    log: Arc<Log>,
    install: std::path::PathBuf,
    _dir: tempfile::TempDir,
}

/// An app that may call the bus: protocol 2 and the capability declared.
fn calling_manifest(name: &str) -> String {
    format!(
        r#"[app]
name = "{name}"
version = "1.0.0"
protocol = 2
lifecycle = "on-demand"

[needs]
invoke_commands = true

[[commands]]
id = "{name}.ping"
summary = "Answer with a pong"
risk = "read"
"#
    )
}

/// An app built against protocol 1: no capability, and no key in `[needs]`
/// that protocol 1 did not have.
fn old_manifest(name: &str) -> String {
    format!(
        r#"[app]
name = "{name}"
version = "1.0.0"
protocol = 1
lifecycle = "on-demand"

[[commands]]
id = "{name}.ping"
summary = "Answer with a pong"
risk = "read"
"#
    )
}

fn install_app(install: &std::path::Path, name: &str, manifest_text: &str) {
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

/// `caller` declares the capability, `quiet` is a protocol-1 app that does
/// not. Both are granted `probe.*` so the capability is the only difference
/// between them: a test that refuses `quiet` has to be refusing it for the
/// capability and not for a missing grant.
async fn rig() -> Rig {
    let dir = tempfile::tempdir().unwrap();
    let install = dir.path().join("install");
    std::fs::create_dir_all(&install).unwrap();
    install_app(&install, "caller", &calling_manifest("caller"));
    install_app(&install, "quiet", &old_manifest("quiet"));

    let store = Arc::new(std::sync::Mutex::new(Store::open(dir.path()).unwrap()));
    let registry = Arc::new(Registry::new());
    registry.register(Echo).unwrap();
    registry.register(Wipe).unwrap();
    registry.register(Secret).unwrap();
    registry.register(Asleep).unwrap();

    // Exact patterns rather than `probe.*`, so `vault.secret` is outside the
    // grant and `probe.asleep` is inside it: one of the two assertions below
    // is about a grant and the other is about a disabled command, and a
    // single wildcard would have conflated them.
    let granted = Grant::new(
        vec![
            Pattern::new("probe.echo").unwrap(),
            Pattern::new("probe.wipe").unwrap(),
            Pattern::new("probe.asleep").unwrap(),
        ],
        NodeScope::ThisNode,
    );
    let grants = Arc::new(MemoryGrants::new());
    grants.put(app_session("caller"), granted.clone());
    grants.put(app_session("quiet"), granted);

    let log = Log::new();
    let bus = Arc::new(
        Bus::new(Arc::clone(&registry), NodeId::new(NODE), Arc::new(BusEnv))
            .with_journal(Arc::clone(&log) as Arc<dyn Journal>)
            .with_grants(Arc::clone(&grants) as Arc<dyn Grants>),
    );

    let host = AppHost::new(
        apps::Host::new(
            install.clone(),
            dir.path().to_path_buf(),
            apps::OsEnv::new(),
            apps::Policy::default(),
            Box::new(StoreGrants::new(Arc::clone(&store))),
        ),
        Arc::clone(&registry),
        install.clone(),
        // Short, because every iteration of an attendant's accept loop costs
        // one of these while no app is connected.
        Duration::from_millis(80),
    );

    // The one line that makes any of this reachable at runtime, and the same
    // line `main.rs` needs. The same `Arc<dyn Grants>` the bus was built
    // with: two grant sources is how a tool list and a dispatch come to
    // disagree.
    let bridge = AgentBridge::new(Arc::clone(&bus), Arc::clone(&grants) as Arc<dyn Grants>);
    host.attach_bridge(Arc::clone(&bridge));

    host.start().await;

    Rig {
        host,
        bridge,
        bus,
        registry,
        grants,
        log,
        install,
        _dir: dir,
    }
}

impl Rig {
    /// Connect as an app and complete the handshake.
    async fn connect(&self, name: &str, protocol: u32) -> FakeApp {
        let path = apps::socket_path(&self.install, name);
        let stream = UnixStream::connect(&path)
            .await
            .unwrap_or_else(|e| panic!("connect to {}: {e}", path.display()));
        let mut app = FakeApp {
            name: name.to_string(),
            calls: 0,
            stream: BufReader::new(stream),
        };

        let hello = app.recv().await;
        assert_eq!(hello["frame"], "hello", "got {hello}");

        app.send(json!({
            "frame": "register",
            // What the app binary speaks, which is the number
            // `accept_registration` checks. A protocol-1 app says 1 here.
            "protocol": protocol,
            "app": name,
            "version": "1.0.0",
            "commands": [{
                "id": format!("{name}.ping"),
                "summary": "Answer with a pong",
                "risk": "read",
                "params": { "type": "object", "properties": {} },
            }],
        }))
        .await;
        app
    }

    /// Wait until the registry holds a command. The handshake finishes on the
    /// attendant's task, so there is a real happens-before to wait on rather
    /// than a duration to guess at; the deadline is a failure deadline.
    async fn until_registered(&self, command: &str) {
        let id = CommandId::new(command.to_string()).unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !self.registry.contains(&id) {
            assert!(
                std::time::Instant::now() < deadline,
                "{command} never reached the bus"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }
}

struct FakeApp {
    name: String,
    /// The app's own call numbers. Separate from the core's `id` space, which
    /// is the whole point of the two fields being named differently.
    calls: u64,
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

    /// Invoke a bus command and read the answer.
    async fn invoke(&mut self, command: &str, params: Value) -> Value {
        self.calls += 1;
        let call = self.calls;
        self.send(json!({
            "frame": "invoke",
            "call": call,
            "command": command,
            "params": params,
        }))
        .await;
        let answer = self.recv().await;
        assert_eq!(answer["frame"], "invoked", "got {answer}");
        assert_eq!(
            answer["call"], call,
            "the core answered a different call number: {answer}"
        );
        answer["outcome"].clone()
    }

    /// Ask what this app may call.
    async fn commands(&mut self) -> Value {
        self.calls += 1;
        let call = self.calls;
        self.send(json!({ "frame": "commands", "call": call }))
            .await;
        let answer = self.recv().await;
        assert_eq!(answer["call"], call, "got {answer}");
        answer
    }
}

// ---------------------------------------------------------------------------
// The capability
// ---------------------------------------------------------------------------

#[tokio::test]
async fn an_app_without_the_capability_is_refused_and_told_which_key_is_missing() {
    // The reason the capability and the frames had to land together. Notes,
    // Tasks and Files are each confined to one folder and approved for
    // exactly that; if an invocation frame worked without a declared
    // capability, any of them could call `terminal.open` and have a shell on
    // the strength of a manifest that asked to read a Notes folder.
    let rig = rig().await;
    let mut app = rig.connect("quiet", 1).await;
    rig.until_registered("quiet.ping").await;

    let outcome = app.invoke("probe.echo", json!({ "say": "hello" })).await;

    assert_eq!(outcome["outcome"], "refused", "got {outcome}");
    let message = outcome["message"].as_str().unwrap_or_default();
    // Named, not merely implied: the reader is an app author or an operator
    // looking at app.toml, and "permission denied" would send them to the
    // sandbox instead of to the one line they have to add.
    assert!(
        message.contains("invoke_commands"),
        "the refusal does not name the capability: {message}"
    );
}

#[tokio::test]
async fn an_app_without_the_capability_cannot_even_enumerate_the_bus() {
    // An empty list would read as "the bus has nothing for you" and send an
    // app author hunting for missing commands rather than a missing
    // capability. It is also the surface of the whole machine, which an app
    // with no business calling anything has no business reading.
    let rig = rig().await;
    let mut app = rig.connect("quiet", 1).await;
    rig.until_registered("quiet.ping").await;

    let answer = app.commands().await;

    assert_eq!(answer["frame"], "invoked", "got {answer}");
    assert_eq!(answer["outcome"]["outcome"], "refused");
    assert!(
        answer["outcome"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("invoke_commands")
    );
}

#[tokio::test]
async fn an_app_with_the_capability_invokes_a_command_and_gets_the_value_back() {
    // The path itself. If it does not work the Assistant cannot exist: an app
    // that can only serve commands has no way to dispatch one, which is why
    // `mcp.rs` sat unreferenced.
    let rig = rig().await;
    let mut app = rig.connect("caller", 2).await;
    rig.until_registered("caller.ping").await;

    let outcome = app.invoke("probe.echo", json!({ "say": "hello" })).await;

    assert_eq!(outcome["outcome"], "ok", "got {outcome}");
    assert_eq!(outcome["value"]["heard"], "hello");
}

#[tokio::test]
async fn an_apps_invocation_is_attributed_to_the_app_and_not_to_the_operator() {
    // Attribution is what makes the timeline, the Inbox and the kill switch
    // mean anything. An app's dispatch recorded as the operator's would put a
    // command the operator never issued under their name, and would route a
    // destructive one to a confirmation dialog instead of the Inbox.
    let rig = rig().await;
    let mut app = rig.connect("caller", 2).await;
    rig.until_registered("caller.ping").await;

    let outcome = app.invoke("probe.echo", json!({ "say": "hello" })).await;

    // What the command itself was told.
    assert_eq!(outcome["value"]["issuer_kind"], "agent");
    assert_eq!(outcome["value"]["issuer_name"], "app:caller");

    // And what the append-only log recorded, which is the copy that outlives
    // the call.
    let issuers = rig.log.issuers_of("probe.echo");
    assert!(!issuers.is_empty(), "the bus recorded nothing");
    for (kind, name) in &issuers {
        assert_eq!(kind, "agent", "recorded as {kind}");
        assert_eq!(name.as_deref(), Some("app:caller"));
    }
}

#[tokio::test]
async fn an_app_is_refused_a_command_outside_its_grant_by_the_bus_itself() {
    // The capability lets an app ask; the grant decides what it gets. If
    // these were one thing, approving an app for bus access would be
    // approving it for everything on the bus.
    let rig = rig().await;
    let mut app = rig.connect("caller", 2).await;
    rig.until_registered("caller.ping").await;

    let outcome = app.invoke("vault.secret", json!({})).await;

    assert_eq!(outcome["outcome"], "refused", "got {outcome}");
    let message = outcome["message"].as_str().unwrap_or_default();
    assert!(
        message.contains("grant"),
        "the refusal does not mention the grant: {message}"
    );
}

#[tokio::test]
async fn a_destructive_command_an_app_asks_for_does_not_run() {
    // An app is an agent as far as routing is concerned, so a destructive
    // command waits for the operator. There is no Inbox in this build, so the
    // honest answer is a refusal — `probe.wipe` fails loudly if it is ever
    // reached, so a hole here cannot pass as a pass.
    let rig = rig().await;
    let mut app = rig.connect("caller", 2).await;
    rig.until_registered("caller.ping").await;

    let outcome = app.invoke("probe.wipe", json!({})).await;

    assert_eq!(outcome["outcome"], "refused", "got {outcome}");
    let message = outcome["message"].as_str().unwrap_or_default();
    assert!(
        message.contains("approval") || message.contains("Inbox"),
        "the refusal does not say a person has to decide: {message}"
    );
}

#[tokio::test]
async fn an_app_can_list_what_it_may_call_and_is_not_offered_the_rest() {
    // The list is generated from the live registry through the same surface
    // an agent's tools/list goes through. A hand-maintained second list is
    // how an app comes to be offered a command the bus will refuse.
    let rig = rig().await;
    let mut app = rig.connect("caller", 2).await;
    rig.until_registered("caller.ping").await;

    let answer = app.commands().await;

    assert_eq!(answer["frame"], "available", "got {answer}");
    let offered: BTreeSet<String> = answer["commands"]
        .as_array()
        .expect("a list of commands")
        .iter()
        .map(|c| c["command"].as_str().unwrap_or_default().to_string())
        .collect();

    assert!(offered.contains("probe.echo"));
    assert!(
        !offered.contains("vault.secret"),
        "offered a command outside the grant: {offered:?}"
    );
    assert!(
        !offered.contains("probe.asleep"),
        "offered a command that cannot run right now: {offered:?}"
    );

    // The gate reason travels with it, so an app can plan around the wait
    // instead of discovering it.
    let wipe = answer["commands"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["command"] == "probe.wipe")
        .expect("probe.wipe is in the grant");
    assert_eq!(wipe["risk"], "gated-destructive");
}

#[tokio::test]
async fn an_app_built_against_protocol_1_still_handshakes_and_still_serves_commands() {
    // The compatibility promise. Protocol 2 added frames and a manifest key;
    // an app that knows neither must keep working, because the alternative is
    // that adding a capability breaks every installed app.
    assert_eq!(
        deckjack_protocol::MIN_PROTOCOL_VERSION,
        1,
        "protocol 1 is no longer accepted"
    );
    // What a core must put in `Hello` for such an app: its own number, not
    // the core's, because the app validates it against the window it was
    // compiled with.
    assert_eq!(deckjack_protocol::negotiated(1), 1);

    let rig = rig().await;
    let mut app = rig.connect("quiet", 1).await;
    rig.until_registered("quiet.ping").await;

    // Serving still works: the operator dispatches, the frame reaches the
    // app, the answer comes back.
    let caller = tokio::spawn({
        let bus = Arc::clone(&rig.bus);
        async move {
            let request = Request::new(
                CommandId::new("quiet.ping".to_string()).unwrap(),
                json!({}),
                Issuer::operator(),
            );
            let Ok(Admitted::Running(running)) = bus.dispatch(request) else {
                panic!("quiet.ping was not admitted");
            };
            running.finish().await.outcome.map_err(|e| e.to_string())
        }
    });

    let call = app.recv().await;
    assert_eq!(call["frame"], "call");
    assert_eq!(call["command"], "quiet.ping");
    app.send(json!({ "frame": "result", "id": call["id"], "result": { "pong": true } }))
        .await;

    assert_eq!(caller.await.unwrap().unwrap()["pong"], true);
}

#[tokio::test]
async fn declaring_the_capability_changes_the_manifest_fingerprint() {
    // The fingerprint is what makes an app re-ask for approval when it
    // changes what it wants. A capability outside it would be a permission
    // the operator granted once for a manifest that then quietly grew.
    let path = std::path::Path::new("app.toml");
    let without = apps::Manifest::parse(path, &old_manifest("caller")).unwrap();
    let with = apps::Manifest::parse(path, &calling_manifest("caller")).unwrap();

    assert_ne!(
        without.fingerprint(),
        with.fingerprint(),
        "asking for the bus does not change the fingerprint, so an app could add it \
         without being approved again"
    );

    // And an app with nothing else in `[needs]` is no longer "needs nothing":
    // that emptiness is what `Host::check_grant` reads to launch an app with
    // no approval at all, so a capability that left it empty would be a
    // capability nobody was ever asked about.
    assert!(
        !with.needs.is_empty(),
        "an app asking for the bus would launch unapproved"
    );
    assert!(without.needs.is_empty());

    // The protocol-1 manifest's fingerprint is unchanged by protocol 2
    // existing: `invoke_commands` is skipped when false, so the three
    // installed apps were not all made to re-ask for approval by this
    // change. A fixed digest, because the whole value of a fingerprint is
    // that it does not move on its own.
    assert_eq!(
        without.fingerprint(),
        apps::Manifest::parse(path, &old_manifest("caller"))
            .unwrap()
            .fingerprint()
    );
}

#[tokio::test]
async fn a_manifest_asking_for_the_bus_on_protocol_1_is_refused_while_it_is_read() {
    // The capability needs the frames that carry it. Declared without them it
    // would be a permission the operator granted and the app could never use,
    // and the app author would find out from a refusal at runtime.
    let text = calling_manifest("caller").replace("protocol = 2", "protocol = 1");
    let error = apps::Manifest::parse(std::path::Path::new("app.toml"), &text)
        .expect_err("protocol 1 accepted a protocol 2 capability");
    let message = error.to_string();
    assert!(message.contains("invoke_commands"), "{message}");
    assert!(message.contains("protocol"), "{message}");
}

#[tokio::test]
async fn an_app_declaring_the_capability_cannot_launch_until_it_is_approved() {
    // `Needs::is_empty` decides whether an app needs approving at all, so
    // this is the assertion that the capability actually reaches the
    // operator: `caller` has no stored grant, so the host refuses to start it
    // rather than launching it with bus access nobody agreed to.
    let rig = rig().await;
    assert_eq!(
        rig.host.open_window("caller"),
        apps::WindowStatus::Unavailable,
        "an app asking for the bus launched without an approval"
    );
}

// ---------------------------------------------------------------------------
// The MCP tool surface, over the real registry and the real bus
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_tool_surface_offers_one_tool_per_registered_command_in_the_grant() {
    let rig = rig().await;
    // Held, not dropped: its connection ending would take `caller.ping` off
    // the registry and with it the assertion that the filter is the grant.
    let _app = rig.connect("caller", 2).await;
    rig.until_registered("caller.ping").await;

    let listing = rig.bridge.tools(&app_session("caller"));
    let offered: BTreeSet<&str> = listing.tools.iter().map(|tool| tool.command_id()).collect();

    // Generated from the registry, not from a list anyone maintains: these
    // are exactly the granted commands that can run right now.
    assert!(offered.contains("probe.echo"));
    assert!(offered.contains("probe.wipe"));
    assert!(!offered.contains("vault.secret"), "{offered:?}");
    assert!(!offered.contains("probe.asleep"), "{offered:?}");
    // An app's own command is in the registry and outside this grant, which
    // is the case that proves the filter is the grant and not a hard-coded
    // namespace.
    assert!(!offered.contains("caller.ping"), "{offered:?}");

    // A registration bug is reported to the operator and never to the agent.
    // Nothing here is registered badly, so the list is empty.
    assert!(listing.skipped.is_empty(), "{:?}", listing.skipped);

    // The tool name is the encoded command id, and the risk travelled from
    // the backend's declaration rather than being inferred here.
    let wipe = listing
        .tools
        .iter()
        .find(|tool| tool.command_id() == "probe.wipe")
        .unwrap();
    assert_eq!(wipe.name(), "probe__wipe");
    assert_eq!(wipe.risk(), ToolRisk::Gated(GateReason::Destructive));
}

#[tokio::test]
async fn a_command_outside_the_grant_is_refused_even_when_it_is_called_anyway() {
    // A tool list an agent holds may be minutes old, and a grant can be
    // narrowed or revoked between the list and the call — the kill switch
    // does exactly that. Filtering the list is a courtesy; this is the
    // control.
    let rig = rig().await;
    let agent = app_identity("caller");

    let result = rig.bridge.call_tool(&agent, "vault__secret", None).await;

    assert_eq!(result.is_error, Some(true));
    assert_eq!(
        result
            .structured_content
            .expect("a machine-readable status")["status"],
        "outside_grant"
    );
    // Nothing ran, so nothing was recorded as having run.
    assert!(
        rig.log.issuers_of("vault.secret").is_empty(),
        "a refused call reached the bus"
    );
}

/// Held agents are offered nothing, which is the same rule `mcpserve` keeps.
///
/// `mcp.rs`: never offer a tool that would be refused, because offering and
/// then refusing teaches a model the boundary is negotiable and worth
/// rewording. While agents are held every dispatch comes back
/// `Refused::AgentsPaused`, so a full tool list is an invitation to probe a
/// gate that is shut. The app-facing half is generated from the same call, so
/// `AppFrame::Commands` goes empty with it.
///
/// Asked of `Bus::agents_paused`, which is the same read the refusal is
/// decided by — a lockdown of the bridge's own would be a second opinion able
/// to offer what the bus refuses. The hold here is the operator's lever
/// because it is the one a unit test can pull; in the product the other term
/// is the lockdown, attached in `wiring::bus`.
#[tokio::test]
async fn nothing_is_offered_while_agents_are_held() {
    let rig = rig().await;
    let session = app_session("caller");

    assert!(
        !rig.bridge.tools(&session).tools.is_empty(),
        "nothing was offered even before the hold; this proves nothing"
    );
    assert!(!rig.bridge.commands_for_app("caller").is_empty());

    rig.bus.pause_agents();

    let listing = rig.bridge.tools(&session);
    assert!(
        listing.tools.is_empty(),
        "a held session was offered tools it cannot call: {:?}",
        listing
            .tools
            .iter()
            .map(|t| t.command_id())
            .collect::<Vec<_>>()
    );
    assert!(
        listing.skipped.is_empty(),
        "nothing was listed, so nothing failed to list: {:?}",
        listing.skipped
    );
    assert!(
        rig.bridge.commands_for_app("caller").is_empty(),
        "an app was sent a command list it cannot invoke"
    );

    // And calling anyway is refused, which is the control the list is only a
    // courtesy for.
    let result = rig
        .bridge
        .call_tool(&app_identity("caller"), "probe__echo", None)
        .await;
    assert_eq!(result.is_error, Some(true));

    // Released, and the offer comes back with nothing rebuilt.
    rig.bus.resume_agents();
    assert!(!rig.bridge.tools(&session).tools.is_empty());
}

#[tokio::test]
async fn a_revoked_grant_stops_being_offered_and_stops_being_callable() {
    // One grant store, read by both halves. Two would mean a revocation that
    // took effect in the list and not at the call, or the reverse.
    let rig = rig().await;
    let session = app_session("caller");

    assert!(!rig.bridge.tools(&session).tools.is_empty());
    rig.grants.revoke(&session);

    assert!(
        rig.bridge.tools(&session).tools.is_empty(),
        "a revoked session is still offered tools"
    );
    let result = rig
        .bridge
        .call_tool(&app_identity("caller"), "probe__echo", None)
        .await;
    assert_eq!(result.is_error, Some(true));
}

// ---------------------------------------------------------------------------
// The risk mapping
// ---------------------------------------------------------------------------

/// The expected mapping, written out a second time with no wildcard arm.
///
/// Adding a risk level to the bus stops this function compiling, which is the
/// point: the mapping it checks must then be given a decision rather than
/// landing on whichever side of the Inbox gate a `_` arm happened to pick.
fn expected_tool_risk(level: RiskLevel) -> ToolRisk {
    match level {
        RiskLevel::ReadOnly => ToolRisk::ReadOnly,
        RiskLevel::Write => ToolRisk::Write,
        RiskLevel::CrossProject => ToolRisk::Gated(GateReason::CrossProject),
        RiskLevel::Outbound => ToolRisk::Gated(GateReason::Outbound),
        RiskLevel::Destructive => ToolRisk::Gated(GateReason::Destructive),
    }
}

#[test]
fn the_risk_mapping_covers_every_level_the_bus_has() {
    // If a level ever mapped to the wrong side of the gate, a destructive
    // command would be described to an agent as an ordinary write and the
    // agent would plan around it running immediately.
    for level in [
        RiskLevel::ReadOnly,
        RiskLevel::Write,
        RiskLevel::CrossProject,
        RiskLevel::Outbound,
        RiskLevel::Destructive,
    ] {
        assert_eq!(tool_risk(level), expected_tool_risk(level), "{level}");
    }
}

#[test]
fn every_tool_risk_has_a_call_risk_an_app_can_read() {
    // Same argument one step further out: an app is told whether its call
    // will wait, and "gated" with no reason is a wait it cannot explain to
    // anyone.
    let pairs = [
        (ToolRisk::ReadOnly, CallRisk::ReadOnly, false),
        (ToolRisk::Write, CallRisk::Write, false),
        (
            ToolRisk::Gated(GateReason::Destructive),
            CallRisk::GatedDestructive,
            true,
        ),
        (
            ToolRisk::Gated(GateReason::Outbound),
            CallRisk::GatedOutbound,
            true,
        ),
        (
            ToolRisk::Gated(GateReason::CrossProject),
            CallRisk::GatedCrossProject,
            true,
        ),
    ];
    for (tool, call, waits) in pairs {
        assert_eq!(call_risk(tool), call);
        assert_eq!(call.waits_for_the_operator(), waits);
    }
}
