//! An agent on the other end of a real pipe, talking to a real bus.
//!
//! `tests/mcp.rs` proves the translation rules against its own doubles and
//! `tests/agentbridge.rs` proves the bridge against a real bus — and both
//! passed for as long as no transport existed, which is to say for as long as
//! no agent could reach any of it. What is exercised here is the byte stream:
//! newline-delimited JSON-RPC in one end, a dispatch onto the real bus and a
//! real result out the other.
//!
//! Nothing is spawned and nothing is mocked. The "agent" is this test writing
//! protocol frames into a [`tokio::io::duplex`], which is the same thing a
//! child's pipe or a unix socket would be to `mcpserve::serve`.

use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, DuplexStream, ReadHalf, WriteHalf};

use deckjack_core::agentbridge::AgentBridge;
use deckjack_core::command::{
    Bus, Command, CommandError, Context, Grant, Grants, Journal, JournalError, LogEntry,
    MemoryGrants, NodeId, NodeScope, OsEnv as BusEnv, Pattern, Registry, RiskLevel,
};
use deckjack_core::lockdown::{self, Lockdown};
use deckjack_core::mcp::AgentIdentity;
use deckjack_core::mcpserve;

const NODE: &str = "mcpserve-node";
const SESSION: &str = "assistant-1";

// ---------------------------------------------------------------------------
// Commands to call
// ---------------------------------------------------------------------------

#[derive(serde::Deserialize, schemars::JsonSchema)]
struct EchoParams {
    say: String,
    /// Present so a test can try to pass a session id in the arguments. The
    /// command reports who the bus said was calling, never this.
    #[serde(default)]
    session: Option<String>,
}

/// Reports who the bus says is calling, so attribution is something a test
/// can assert on rather than a log line nobody reads.
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
            "claimed_session": params.session,
            "issuer_kind": cx.issuer.kind(),
            "issuer_name": cx.issuer.name(),
        }))
    }
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
struct NoParams {}

/// Registered, and outside every grant in this file.
struct Secret;

impl Command for Secret {
    const ID: &'static str = "vault.secret";
    const TITLE: &'static str = "Read the vault";
    const RISK: RiskLevel = RiskLevel::ReadOnly;
    type Params = NoParams;
    type Output = Value;

    async fn run(&self, _params: NoParams, _cx: Context) -> Result<Value, CommandError> {
        Err(CommandError::Failed(anyhow::anyhow!(
            "vault.secret ran, which means a command outside the grant reached a backend"
        )))
    }
}

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

struct Discard;

impl Journal for Discard {
    fn record(&self, _entry: LogEntry) -> Result<(), JournalError> {
        Ok(())
    }
}

/// `lockdown::Deps` wants a session store; nothing here has sessions.
struct NoSessions;

impl lockdown::Sessions for NoSessions {
    fn revoke_all(&self) -> usize {
        0
    }
    fn adopt_signing_key(&self, _key: lockdown::SessionKey) {}
}

fn lockdown_for(dir: &std::path::Path) -> Arc<Lockdown> {
    let policy = lockdown::Policy {
        grace: Duration::from_millis(10),
        grace_checks: 2,
        ..lockdown::Policy::default()
    };
    Arc::new(
        Lockdown::restore(
            lockdown::Deps {
                clock: Box::new(lockdown::OsClock),
                random: Box::new(lockdown::OsRandom),
                sessions: Box::new(NoSessions),
                children: Box::new(lockdown::NoChildren),
                signals: Box::new(lockdown::OsSignals::new()),
                outbound: Box::new(lockdown::NoOutbound),
                passkeys: Box::new(lockdown::PasskeysUnavailable),
                journal: Box::new(lockdown::FileJournal::new(dir)),
            },
            policy,
            NODE.to_string(),
        )
        .expect("a fresh journal"),
    )
}

/// The agent's end of the pipe.
///
/// Generic over the stream because the transport is: an in-memory duplex and
/// a unix socket are the same thing to `mcpserve::serve`, and this type being
/// tied to one of them would have quietly limited what could be tested.
struct Wire<R, W> {
    read: BufReader<R>,
    write: W,
    next_id: i64,
}

impl<R: tokio::io::AsyncRead + Unpin, W: tokio::io::AsyncWrite + Unpin> Wire<R, W> {
    async fn send(&mut self, frame: Value) {
        let mut line = serde_json::to_string(&frame).unwrap();
        line.push('\n');
        self.write.write_all(line.as_bytes()).await.unwrap();
        self.write.flush().await.unwrap();
    }

    async fn recv(&mut self) -> Value {
        let mut line = String::new();
        let n = self.read.read_line(&mut line).await.unwrap();
        assert!(n > 0, "the server closed the pipe instead of answering");
        serde_json::from_str(&line).unwrap_or_else(|e| panic!("not JSON: {line:?}: {e}"))
    }

    /// One request, one response. Asserts the id echoes, because a client that
    /// matches responses by arrival order rather than id is a client that
    /// works until it doesn't.
    async fn call(&mut self, method: &str, params: Value) -> Value {
        let id = self.next_id;
        self.next_id += 1;
        self.send(json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        }))
        .await;
        let reply = self.recv().await;
        assert_eq!(reply["id"], json!(id), "response id did not echo: {reply}");
        reply
    }

    /// The handshake every MCP client performs before anything else.
    async fn initialize(&mut self) -> Value {
        let reply = self
            .call(
                "initialize",
                json!({
                    "protocolVersion": "2025-11-25",
                    "capabilities": {},
                    "clientInfo": { "name": "test-agent", "version": "0" },
                }),
            )
            .await;
        self.send(json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }))
            .await;
        reply
    }

    async fn tools(&mut self) -> Vec<String> {
        let reply = self.call("tools/list", json!({})).await;
        reply["result"]["tools"]
            .as_array()
            .unwrap_or_else(|| panic!("no tools array in {reply}"))
            .iter()
            .map(|t| t["name"].as_str().unwrap().to_string())
            .collect()
    }

    async fn call_tool(&mut self, name: &str, arguments: Value) -> Value {
        self.call(
            "tools/call",
            json!({ "name": name, "arguments": arguments }),
        )
        .await
    }
}

struct Rig {
    wire: Wire<ReadHalf<DuplexStream>, WriteHalf<DuplexStream>>,
    lockdown: Arc<Lockdown>,
    _dir: tempfile::TempDir,
}

/// Build the real assembly and attach one agent to it.
///
/// `session` is the agent session id the handler is built around. Passing one
/// the grant store knows nothing about is how the no-grant case is tested,
/// which is the whole reason it is a parameter.
fn rig(session: &str) -> Rig {
    let dir = tempfile::tempdir().unwrap();
    let (bridge, lockdown) = assembly(dir.path());

    let handler = mcpserve::Handler::new(bridge, Arc::clone(&lockdown), identity(session));

    let (agent, server) = tokio::io::duplex(64 * 1024);
    let (server_read, server_write) = tokio::io::split(server);
    tokio::spawn(async move {
        // The pipe going away is how an agent detaches, so an error here is
        // not a test failure by itself; a test that depended on an answer has
        // already failed on the closed pipe.
        let _ = mcpserve::serve(handler, server_read, server_write).await;
    });

    let (agent_read, agent_write) = tokio::io::split(agent);
    Rig {
        wire: Wire {
            read: BufReader::new(agent_read),
            write: agent_write,
            next_id: 1,
        },
        lockdown,
        _dir: dir,
    }
}

fn identity(session: &str) -> AgentIdentity {
    AgentIdentity {
        session: session.to_string(),
        label: mcpserve::DEFAULT_LABEL.to_string(),
        via_window: None,
    }
}

/// A real registry, a real bus with a real grant store, and a real bridge.
fn assembly(dir: &std::path::Path) -> (Arc<AgentBridge>, Arc<Lockdown>) {
    let registry = Arc::new(Registry::new());
    registry.register(Echo).unwrap();
    registry.register(Secret).unwrap();

    // Exact patterns, so `vault.secret` is outside the grant for a reason a
    // reader can see rather than because a wildcard happened not to match.
    let grants = Arc::new(MemoryGrants::new());
    grants.put(
        deckjack_core::command::AgentSessionId::new(SESSION.to_string()),
        Grant::new(
            vec![Pattern::new("probe.echo").unwrap()],
            NodeScope::ThisNode,
        ),
    );

    let bus = Arc::new(
        Bus::new(Arc::clone(&registry), NodeId::new(NODE), Arc::new(BusEnv))
            .with_journal(Arc::new(Discard) as Arc<dyn Journal>)
            .with_grants(Arc::clone(&grants) as Arc<dyn Grants>),
    );

    // The same `Arc<dyn Grants>` the bus holds. Two grant sources is how a
    // tool list and a dispatch come to tell one caller different things.
    let bridge = AgentBridge::new(Arc::clone(&bus), Arc::clone(&grants) as Arc<dyn Grants>);
    (bridge, lockdown_for(dir))
}

fn is_error(reply: &Value) -> bool {
    reply["result"]["isError"].as_bool().unwrap_or(false)
}

fn text_of(reply: &Value) -> String {
    reply["result"]["content"]
        .as_array()
        .unwrap_or_else(|| panic!("no content in {reply}"))
        .iter()
        .filter_map(|c| c["text"].as_str())
        .collect::<Vec<_>>()
        .join("\n")
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn an_agent_completes_the_handshake_and_is_told_what_this_server_is() {
    let mut rig = rig(SESSION);
    let reply = rig.wire.initialize().await;

    assert_eq!(reply["result"]["serverInfo"]["name"], json!("deckjack"));
    // Tools, and nothing else. A capability advertised and not implemented is
    // a client spending a round trip to find that out.
    assert!(
        reply["result"]["capabilities"]["tools"].is_object(),
        "tools not advertised: {reply}"
    );
    assert!(
        reply["result"]["capabilities"]["resources"].is_null(),
        "advertised resources, which this server does not serve: {reply}"
    );
    // The agent is told a refusal is final, because a model that does not
    // know that spends its turns rewording one.
    let instructions = reply["result"]["instructions"].as_str().unwrap();
    assert!(
        instructions.contains("refused") && instructions.contains("final"),
        "instructions do not say a refusal is final: {instructions:?}"
    );
}

#[tokio::test]
async fn the_tool_list_is_the_registry_filtered_by_this_sessions_grant() {
    let mut rig = rig(SESSION);
    rig.wire.initialize().await;

    let tools = rig.wire.tools().await;

    assert!(
        tools.contains(&"probe__echo".to_string()),
        "a granted command was not offered: {tools:?}"
    );
    assert!(
        !tools.iter().any(|t| t.starts_with("vault")),
        "offered a tool outside the grant: {tools:?}"
    );
}

#[tokio::test]
async fn a_call_runs_the_real_command_and_is_attributed_to_the_agent() {
    let mut rig = rig(SESSION);
    rig.wire.initialize().await;

    let reply = rig
        .wire
        .call_tool("probe__echo", json!({ "say": "hello" }))
        .await;

    assert!(!is_error(&reply), "the call failed: {reply}");
    let text = text_of(&reply);
    assert!(text.contains("hello"), "the command did not run: {text}");
    // Attribution is the point: an agent's command is recorded as the agent's,
    // not as the operator's, and under the session id the operator granted —
    // which is what an audit row can be joined back to.
    assert!(
        text.contains("\"issuer_kind\": \"agent\""),
        "the command was not attributed to an agent: {text}"
    );
    assert!(
        text.contains(SESSION),
        "the agent session id did not reach the command: {text}"
    );
    // Deliberately NOT asserting the label arrives. `Issuer::Agent` carries
    // the session id and nothing else, so of `AgentIdentity`'s three fields
    // only `session` survives a dispatch: `label` reaches tracing through
    // `attribution()` and `via_window` is dropped outright. That is a defensible
    // split — an id is a fact and a label is renameable presentation — but it
    // means the timeline shows `assistant-1` rather than "Assistant", and this
    // is where that is written down.
}

#[tokio::test]
async fn an_agent_cannot_claim_a_different_session_in_its_arguments() {
    let mut rig = rig(SESSION);
    rig.wire.initialize().await;

    // There is no session field in the protocol, so the only thing an agent
    // can do is put one in the arguments and hope. The identity is fixed when
    // the handler is built, so this changes nothing — and that is the property
    // being pinned, because the day someone adds a "session" parameter to the
    // call path this test is what notices.
    let reply = rig
        .wire
        .call_tool("probe__echo", json!({ "say": "hi", "session": "operator" }))
        .await;

    assert!(!is_error(&reply), "the call failed: {reply}");
    let text = text_of(&reply);
    assert!(
        text.contains("agent"),
        "claiming a session changed the issuer kind: {text}"
    );
    assert!(
        !text.contains("\"issuer_name\": \"operator\""),
        "the claimed session became the attribution: {text}"
    );
}

#[tokio::test]
async fn a_tool_outside_the_grant_is_refused_rather_than_erroring_the_protocol() {
    let mut rig = rig(SESSION);
    rig.wire.initialize().await;

    // Not offered, called anyway. A protocol-level error is what a model
    // retries blindly, so this has to come back as a readable result.
    let reply = rig.wire.call_tool("vault__secret", json!({})).await;

    assert!(
        reply["error"].is_null(),
        "a refusal arrived as a protocol error, which a model will retry: {reply}"
    );
    assert!(
        is_error(&reply),
        "an ungranted call was not refused: {reply}"
    );
}

#[tokio::test]
async fn a_session_the_operator_granted_nothing_is_offered_nothing_and_refused_everything() {
    // A handler mounted for a session id with no grant. Mounting a transport
    // for a session nobody granted anything has to be inert, not dangerous.
    let mut rig = rig("never-granted");
    rig.wire.initialize().await;

    assert!(
        rig.wire.tools().await.is_empty(),
        "a session with no grant was offered tools"
    );

    let reply = rig
        .wire
        .call_tool("probe__echo", json!({ "say": "hi" }))
        .await;
    assert!(
        is_error(&reply),
        "a session with no grant was served: {reply}"
    );
}

#[tokio::test]
async fn a_locked_node_offers_no_tools_and_refuses_every_call() {
    let mut rig = rig(SESSION);
    rig.wire.initialize().await;

    // Granted before the lockdown, so what changes below is the lockdown and
    // nothing else.
    assert!(!rig.wire.tools().await.is_empty());

    rig.lockdown.engage(lockdown::Trigger::HostCommand);

    // The axum gate layer does not cover this transport: it is not an HTTP
    // route. If the handler did not check for itself, an agent would keep
    // working through a lockdown.
    assert!(
        rig.wire.tools().await.is_empty(),
        "a locked node still offered tools"
    );

    let reply = rig
        .wire
        .call_tool("probe__echo", json!({ "say": "hi" }))
        .await;
    assert!(
        is_error(&reply),
        "a locked node ran an agent's command: {reply}"
    );
    assert!(
        text_of(&reply).to_lowercase().contains("lockdown"),
        "the refusal did not say why: {}",
        text_of(&reply)
    );
}

// ---------------------------------------------------------------------------
// The socket an agent attaches to
// ---------------------------------------------------------------------------

#[test]
fn a_session_socket_lands_in_the_install_directory_not_the_synced_one() {
    let install = std::path::Path::new("/home/me/.deckjack");
    let working = std::path::Path::new("/home/me/work");
    let path = mcpserve::socket_path(install, working, "assistant-1");

    assert!(
        path.starts_with(install),
        "an agent socket escaped the install directory: {}",
        path.display()
    );
    // The working directory is the one thing that gets synced or carried
    // between machines, and a live socket is machine-local runtime state.
    assert!(
        !path.starts_with(working),
        "a socket was placed in the synced working directory: {}",
        path.display()
    );
    assert!(
        path.to_str().unwrap().ends_with("-assistant-1.sock"),
        "unexpected socket name: {}",
        path.display()
    );
}

#[test]
fn two_working_directories_do_not_share_one_sessions_socket() {
    // One machine can serve several working directories, each its own node,
    // and every one of them may have a session called `assistant`. They all
    // live in the one install directory, so the names have to differ.
    let install = std::path::Path::new("/home/me/.deckjack");
    let a = mcpserve::socket_path(
        install,
        std::path::Path::new("/home/me/work-a"),
        "assistant",
    );
    let b = mcpserve::socket_path(
        install,
        std::path::Path::new("/home/me/work-b"),
        "assistant",
    );
    assert_ne!(a, b, "two nodes would bind the same agent socket");
}

#[test]
fn a_socket_path_fits_in_sun_path() {
    // The first live run of this failed with "path must be shorter than
    // SUN_LEN" because the sockets were under a deeply nested working
    // directory. `sun_path` is 108 bytes on Linux, including the NUL.
    let install = std::path::Path::new("/home/some-longer-username/.deckjack");
    let working = std::path::Path::new(
        "/home/some-longer-username/Projects/nested/rather/deeply/indeed/workspace",
    );
    let path = mcpserve::socket_path(install, working, "assistant-with-a-longish-name");
    assert!(
        path.as_os_str().len() < 100,
        "socket path is {} bytes, which will not bind: {}",
        path.as_os_str().len(),
        path.display()
    );
}

#[test]
fn the_same_directory_spelled_two_ways_gets_the_same_tag() {
    // `deckjack agent --dir .` and a node started with an absolute path have
    // to agree, or the proxy connects to a socket nothing is serving.
    let dir = tempfile::tempdir().unwrap();
    let absolute = mcpserve::workspace_tag(dir.path());
    let indirect = mcpserve::workspace_tag(&dir.path().join("."));
    assert_eq!(absolute, indirect);
}

#[test]
fn a_session_id_that_would_escape_the_hidden_directory_is_refused() {
    // The id comes from the operator's config rather than the network, so
    // this is a typo guard — but a typo here binds a working agent door
    // outside the 0700 directory, with nothing to say it happened.
    for bad in ["../../tmp/x", "a/b", "", ".", "..", "has space", "nul\0"] {
        assert!(
            mcpserve::check_session_name(bad).is_err(),
            "accepted a session id that cannot be a filename: {bad:?}"
        );
    }
    for good in ["assistant-1", "coding.session", "a_b", "A1"] {
        assert!(
            mcpserve::check_session_name(good).is_ok(),
            "refused a usable session id: {good:?}"
        );
    }
}

#[tokio::test]
async fn an_agent_attaches_over_the_real_socket_and_gets_its_granted_tools() {
    let dir = tempfile::tempdir().unwrap();
    let (bridge, lockdown) = assembly(dir.path());

    let install = tempfile::tempdir().unwrap();
    let path = mcpserve::socket_path(install.path(), dir.path(), SESSION);
    let listener = deckjack_core::apps::bind_socket(&path).await.unwrap();

    // The directory the socket sits in is the credential, so assert it is
    // actually owner-only rather than trusting that `bind_socket` did it.
    use std::os::unix::fs::PermissionsExt as _;
    let mode = std::fs::metadata(path.parent().unwrap())
        .unwrap()
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o700, "the agent socket directory is not owner-only");

    tokio::spawn(mcpserve::mount(
        listener,
        bridge,
        lockdown,
        identity(SESSION),
    ));

    let stream = tokio::net::UnixStream::connect(&path).await.unwrap();
    let (read, write) = tokio::io::split(stream);
    let mut wire = Wire {
        read: BufReader::new(read),
        write,
        next_id: 1,
    };

    wire.initialize().await;
    let tools = wire.tools().await;
    assert!(
        tools.contains(&"probe__echo".to_string()),
        "attaching over the socket did not produce the granted tools: {tools:?}"
    );

    let reply = wire
        .call_tool("probe__echo", json!({ "say": "over a socket" }))
        .await;
    assert!(!is_error(&reply), "the call failed: {reply}");
    assert!(text_of(&reply).contains("over a socket"));
}

#[tokio::test]
async fn two_clients_on_one_session_socket_are_both_served() {
    // Documented behavior, pinned so it is a decision rather than an
    // accident: both are the same session with the same grant and the same
    // attribution, so this is two views of one authority, not a way to
    // assemble more of it.
    let dir = tempfile::tempdir().unwrap();
    let (bridge, lockdown) = assembly(dir.path());

    let install = tempfile::tempdir().unwrap();
    let path = mcpserve::socket_path(install.path(), dir.path(), SESSION);
    let listener = deckjack_core::apps::bind_socket(&path).await.unwrap();
    tokio::spawn(mcpserve::mount(
        listener,
        bridge,
        lockdown,
        identity(SESSION),
    ));

    let mut wires = Vec::new();
    for _ in 0..2 {
        let stream = tokio::net::UnixStream::connect(&path).await.unwrap();
        let (read, write) = tokio::io::split(stream);
        let mut wire = Wire {
            read: BufReader::new(read),
            write,
            next_id: 1,
        };
        wire.initialize().await;
        wires.push(wire);
    }

    for wire in wires.iter_mut() {
        let reply = wire
            .call_tool("probe__echo", json!({ "say": "both" }))
            .await;
        assert!(
            !is_error(&reply),
            "one of two clients was not served: {reply}"
        );
    }
}
