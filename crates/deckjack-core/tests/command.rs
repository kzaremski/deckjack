//! The command bus, tested through its public surface only.
//!
//! What is asserted here is the design's routing table and the three
//! structural claims the module's own docs make: risk is declared by the
//! backend and cannot be named from the wire, anything outside an agent's
//! grant is refused rather than queued, and nothing a redacting command
//! carries reaches the journal.
//!
//! Following `lock.rs`: the clock, the dispatch-id source, the journal, the
//! grant store and the Inbox are all injected, so every rule below is decided
//! without a database, a timer or a sleep.

use std::collections::HashMap;
use std::future::Future;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use deckjack_core::auth::SessionId;
use deckjack_core::command::*;
use deckjack_core::mcp::JsonObject;

// ---------------------------------------------------------------------------
// Scaffolding
// ---------------------------------------------------------------------------

/// `serde_json::Value`, named without naming `serde_json`.
///
/// WHY: the crate declares no dev-dependencies, so this test target links
/// against `deckjack_core` and `std` alone — `serde_json` is a dependency of
/// the library, not of the tests, so it has no extern-prelude entry here even
/// though `Value` is all over the bus's public API. Every other test below
/// gets its `Value`s by type inference (a struct field, a function argument).
/// Implementing [`DynCommand`] is the one place a signature must spell the
/// type out, so it is projected out of the public `mcp::JsonObject` alias.
trait PairValue {
    type Value;
}

impl<I, K, V> PairValue for I
where
    I: IntoIterator<Item = (K, V)>,
{
    type Value = V;
}

type Value = <JsonObject as PairValue>::Value;

const NODE: &str = "node-a";
const OTHER_NODE: &str = "node-b";

/// A fixed clock and a counting dispatch-id source.
///
/// Dispatch ids are sequential here on purpose: the real ones are random so a
/// log reader cannot count dispatches, but a test that wants to say "this row
/// is about that dispatch" needs them predictable.
struct TestEnv {
    ids: AtomicU64,
}

impl TestEnv {
    fn new() -> Arc<TestEnv> {
        Arc::new(TestEnv {
            ids: AtomicU64::new(1),
        })
    }
}

impl Env for TestEnv {
    fn now(&self) -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000)
    }

    fn new_dispatch_id(&self) -> DispatchId {
        let n = self.ids.fetch_add(1, Ordering::SeqCst);
        let mut bytes = [0u8; 12];
        bytes[..8].copy_from_slice(&n.to_le_bytes());
        DispatchId::from_bytes(bytes)
    }
}

/// A journal that keeps what it was told, or refuses to write at all.
struct Log {
    entries: Mutex<Vec<LogEntry>>,
    broken: bool,
}

impl Log {
    fn new() -> Arc<Log> {
        Arc::new(Log {
            entries: Mutex::new(Vec::new()),
            broken: false,
        })
    }

    /// A log that cannot be written: a full disk, a read-only data directory.
    fn broken() -> Arc<Log> {
        Arc::new(Log {
            entries: Mutex::new(Vec::new()),
            broken: true,
        })
    }

    fn entries(&self) -> Vec<LogEntry> {
        self.entries
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    fn decisions(&self) -> Vec<Decision> {
        self.entries().iter().map(|e| e.decision).collect()
    }

    /// Everything the log would show a reader: the row's own fields plus its
    /// detail. Used to assert a secret is in none of them.
    fn text(&self) -> String {
        self.entries()
            .iter()
            .map(|e| {
                format!(
                    "{} {} {} {} {} {}\n",
                    e.dispatch,
                    e.command,
                    e.issuer,
                    e.decision.as_str(),
                    e.params.as_ref().map(|p| p.to_string()).unwrap_or_default(),
                    e.detail.clone().unwrap_or_default(),
                )
            })
            .collect()
    }
}

impl Journal for Log {
    fn record(&self, entry: LogEntry) -> Result<(), JournalError> {
        if self.broken {
            return Err(JournalError("no space left on device".to_string()));
        }
        self.entries
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(entry);
        Ok(())
    }
}

/// The Assistant's Inbox, or a broken one.
struct Inbox {
    items: Mutex<Vec<PendingCommand>>,
    fails: bool,
}

impl Inbox {
    fn new() -> Arc<Inbox> {
        Arc::new(Inbox {
            items: Mutex::new(Vec::new()),
            fails: false,
        })
    }

    fn failing() -> Arc<Inbox> {
        Arc::new(Inbox {
            items: Mutex::new(Vec::new()),
            fails: true,
        })
    }

    fn items(&self) -> Vec<PendingCommand> {
        self.items.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
}

impl Approvals for Inbox {
    fn submit(&self, pending: &PendingCommand) -> Result<PendingId, ApprovalError> {
        if self.fails {
            return Err(ApprovalError::Failed("inbox table is locked".to_string()));
        }
        let mut items = self.items.lock().unwrap_or_else(|e| e.into_inner());
        items.push(pending.clone());
        Ok(PendingId::new(format!("inbox-{}", items.len())))
    }
}

/// What a probe command was actually called with.
///
/// The `Context` is recorded rather than the request, so a test can assert on
/// the risk and the issuer the *bus* decided with instead of the ones the
/// caller asked for.
struct Call {
    risk: RiskLevel,
    issuer: Issuer,
}

#[derive(Default)]
struct Seen {
    calls: Mutex<Vec<Call>>,
}

impl Seen {
    fn new() -> Arc<Seen> {
        Arc::new(Seen::default())
    }

    fn count(&self) -> usize {
        self.calls.lock().unwrap_or_else(|e| e.into_inner()).len()
    }

    fn risk_of_last_call(&self) -> RiskLevel {
        self.calls
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .last()
            .expect("the command ran")
            .risk
    }

    fn issuer_of_last_call(&self) -> Issuer {
        self.calls
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .last()
            .expect("the command ran")
            .issuer
            .clone()
    }
}

/// A command that records the context it was handed and nothing else.
macro_rules! probe {
    ($name:ident, $id:literal, $risk:expr $(, redact = $redact:expr)?) => {
        struct $name(Arc<Seen>);

        impl Command for $name {
            const ID: &'static str = $id;
            const TITLE: &'static str = concat!("Probe ", $id);
            const RISK: RiskLevel = $risk;
            $(const REDACT_PARAMS: bool = $redact;)?

            type Params = HashMap<String, String>;
            type Output = String;

            async fn run(
                &self,
                _params: HashMap<String, String>,
                cx: Context,
            ) -> Result<String, CommandError> {
                // Nothing is awaited while the lock is held: a `MutexGuard`
                // alive across an `await` makes the future non-`Send`, and the
                // bus requires `Send`.
                self.0
                    .calls
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push(Call {
                        risk: cx.risk,
                        issuer: cx.issuer.clone(),
                    });
                Ok(format!("{} ran", cx.command))
            }
        }
    };
}

probe!(ProbeRead, "probe.read", RiskLevel::ReadOnly);
probe!(ProbeWrite, "probe.write", RiskLevel::Write);
probe!(ProbeCross, "probe.cross", RiskLevel::CrossProject);
probe!(ProbeOutbound, "probe.outbound", RiskLevel::Outbound);
probe!(ProbeDestroy, "probe.destroy", RiskLevel::Destructive);
probe!(ProbeSecret, "probe.secret", RiskLevel::Write, redact = true);
// A second namespace, so a grant's edges can be tested.
probe!(OtherRead, "other.read", RiskLevel::ReadOnly);
// A namespace whose name starts with another one's, which is where a prefix
// match instead of a part match would leak a grant.
probe!(ProbesRead, "probes.read", RiskLevel::ReadOnly);

/// A command whose backend says it cannot run right now.
struct ProbeAsleep;

impl Command for ProbeAsleep {
    const ID: &'static str = "probe.asleep";
    const TITLE: &'static str = "Probe asleep";
    const RISK: RiskLevel = RiskLevel::ReadOnly;

    type Params = HashMap<String, String>;
    type Output = String;

    fn enablement(&self) -> Enablement {
        Enablement::disabled("no project is open")
    }

    async fn run(&self, _p: HashMap<String, String>, _cx: Context) -> Result<String, CommandError> {
        panic!("a disabled command must never be called");
    }
}

/// A command pinned to a node this bus is not.
struct ProbeElsewhere;

impl Command for ProbeElsewhere {
    const ID: &'static str = "probe.elsewhere";
    const TITLE: &'static str = "Probe elsewhere";
    const RISK: RiskLevel = RiskLevel::ReadOnly;

    type Params = HashMap<String, String>;
    type Output = String;

    fn target() -> Target {
        Target::Node(NodeId::new(OTHER_NODE))
    }

    async fn run(&self, _p: HashMap<String, String>, _cx: Context) -> Result<String, CommandError> {
        panic!("a command pinned to another node must never run here");
    }
}

/// A command any node may run, so a caller is allowed to ask for one.
struct ProbeAnywhere;

impl Command for ProbeAnywhere {
    const ID: &'static str = "probe.anywhere";
    const TITLE: &'static str = "Probe anywhere";
    const RISK: RiskLevel = RiskLevel::ReadOnly;

    type Params = HashMap<String, String>;
    type Output = String;

    fn target() -> Target {
        Target::AnyNode
    }

    async fn run(&self, _p: HashMap<String, String>, cx: Context) -> Result<String, CommandError> {
        Ok(cx.node.to_string())
    }
}

/// Hands back its own attribution, so the serialized form of an issuer can be
/// inspected through the bus's own serializer rather than a hand-rolled one.
struct WhoAmI;

impl Command for WhoAmI {
    const ID: &'static str = "probe.whoami";
    const TITLE: &'static str = "Who am I";
    const RISK: RiskLevel = RiskLevel::ReadOnly;

    type Params = HashMap<String, String>;
    type Output = Issuer;

    async fn run(&self, _p: HashMap<String, String>, cx: Context) -> Result<Issuer, CommandError> {
        Ok(cx.issuer.clone())
    }
}

/// Reports far more progress than any buffer holds, and then finishes.
struct Chatty;

impl Command for Chatty {
    const ID: &'static str = "probe.chatty";
    const TITLE: &'static str = "Chatty probe";
    const RISK: RiskLevel = RiskLevel::ReadOnly;

    type Params = HashMap<String, String>;
    type Output = String;

    async fn run(&self, _p: HashMap<String, String>, cx: Context) -> Result<String, CommandError> {
        for i in 0..500 {
            cx.progress.step(None, format!("step {i}"));
        }
        // Out of range on purpose: a backend that divides by a total of zero
        // must not be able to paint a bar past its end.
        cx.progress.step(Some(9.0), "overshoot");
        cx.progress.log(Stream::Stderr, "a line");
        Ok("done".to_string())
    }
}

/// A command declared by an app manifest rather than a Rust type: the CLI
/// adapter path. Its risk comes from the [`Declaration`], not from here.
struct Adapter;

impl DynCommand for Adapter {
    fn run(&self, _params: Value, _cx: Context) -> BoxFuture<'static, Result<Value, CommandError>> {
        Box::pin(async move { panic!("the declared probe must never run in these tests") })
    }
}

struct Harness {
    bus: Bus,
    log: Arc<Log>,
    inbox: Arc<Inbox>,
    grants: Arc<MemoryGrants>,
    seen: Arc<Seen>,
    registry: Arc<Registry>,
}

/// A bus with every probe registered, a recording journal, an empty grant
/// store and a working Inbox.
fn harness() -> Harness {
    let seen = Seen::new();
    let registry = Arc::new(Registry::new());
    registry.register(ProbeRead(Arc::clone(&seen))).expect("id");
    registry
        .register(ProbeWrite(Arc::clone(&seen)))
        .expect("id");
    registry
        .register(ProbeCross(Arc::clone(&seen)))
        .expect("id");
    registry
        .register(ProbeOutbound(Arc::clone(&seen)))
        .expect("id");
    registry
        .register(ProbeDestroy(Arc::clone(&seen)))
        .expect("id");
    registry
        .register(ProbeSecret(Arc::clone(&seen)))
        .expect("id");
    registry.register(OtherRead(Arc::clone(&seen))).expect("id");
    registry
        .register(ProbesRead(Arc::clone(&seen)))
        .expect("id");
    registry.register(ProbeAsleep).expect("id");
    registry.register(ProbeElsewhere).expect("id");
    registry.register(ProbeAnywhere).expect("id");
    registry.register(WhoAmI).expect("id");
    registry.register(Chatty).expect("id");

    let log = Log::new();
    let inbox = Inbox::new();
    let grants = Arc::new(MemoryGrants::new());
    let bus = Bus::new(Arc::clone(&registry), NodeId::new(NODE), TestEnv::new())
        .with_journal(Arc::clone(&log) as Arc<dyn Journal>)
        .with_grants(Arc::clone(&grants) as Arc<dyn Grants>)
        .with_approvals(Arc::clone(&inbox) as Arc<dyn Approvals>);

    Harness {
        bus,
        log,
        inbox,
        grants,
        seen,
        registry,
    }
}

/// What a client can say, with everything it is allowed to set filled in.
fn submission(command: &str) -> Submission {
    Submission {
        command: command.to_string(),
        params: r#"{"text":"hello"}"#.parse().expect("valid json"),
        target: None,
        confirmed: false,
    }
}

fn from(command: &str, issuer: Issuer) -> Request {
    Request::from_submission(submission(command), issuer).expect("a valid command id")
}

fn pattern(p: &str) -> Pattern {
    Pattern::new(p).expect("a valid grant pattern")
}

fn grant(patterns: &[&str], ceiling: RiskLevel) -> Grant {
    Grant::new(
        patterns.iter().map(|p| pattern(p)).collect(),
        NodeScope::ThisNode,
    )
    .with_ceiling(ceiling)
}

/// Drive a future to completion without a runtime.
///
/// WHY no runtime: `tokio` is a dependency of the library, not a
/// dev-dependency, so this test target cannot name it. Every handler here is
/// ready on its first poll, so a no-op waker is enough — and a handler that
/// parked would fail loudly instead of sleeping, which is what this suite
/// wants anyway.
fn block_on<F: Future>(future: F) -> F::Output {
    let mut future = Box::pin(future);
    let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
    match future.as_mut().poll(&mut cx) {
        std::task::Poll::Ready(value) => value,
        std::task::Poll::Pending => panic!("a test handler parked; nothing here awaits"),
    }
}

fn expect_running(admitted: Admitted) -> Running {
    match admitted {
        Admitted::Running(running) => running,
        other => panic!("expected the command to run, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Routing: the design's table, row by row
// ---------------------------------------------------------------------------

#[test]
fn a_read_only_command_runs_for_the_operator_without_a_confirmation() {
    let h = harness();
    let running = expect_running(
        h.bus
            .dispatch(from("probe.read", Issuer::operator()))
            .expect("read-only runs"),
    );
    let done = block_on(running.finish());

    assert!(
        done.succeeded(),
        "a read-only command runs for the operator"
    );
    assert_eq!(
        done.parse::<String>().expect("the declared output type"),
        "probe.read ran"
    );
    assert_eq!(h.log.decisions(), vec![Decision::Ran, Decision::Completed]);
}

#[test]
fn a_destructive_command_is_confirmed_before_the_operator_runs_it() {
    let h = harness();
    let admitted = h
        .bus
        .dispatch(from("probe.destroy", Issuer::operator()))
        .expect("the operator may ask");

    match admitted {
        Admitted::NeedsConfirmation { risk, prompt, .. } => {
            assert_eq!(risk, RiskLevel::Destructive);
            assert!(
                prompt.contains("Probe probe.destroy") && prompt.contains("cannot be undone"),
                "the dialog names the command and what it does: {prompt}"
            );
        }
        other => panic!("a destructive command must be confirmed, got {other:?}"),
    }
    assert_eq!(
        h.seen.count(),
        0,
        "nothing ran before the operator answered"
    );
    assert_eq!(h.log.decisions(), vec![Decision::AwaitingConfirmation]);
}

#[test]
fn the_operators_answer_lets_the_same_destructive_command_run() {
    let h = harness();
    let running = expect_running(
        h.bus
            .dispatch(from("probe.destroy", Issuer::operator()).confirmed())
            .expect("a confirmed destructive command runs"),
    );
    assert_eq!(running.risk(), RiskLevel::Destructive);
    block_on(running.finish());

    assert_eq!(h.seen.count(), 1);
    assert_eq!(
        h.seen.risk_of_last_call(),
        RiskLevel::Destructive,
        "the handler is told the risk the registry declared"
    );
    assert!(h.inbox.items().is_empty(), "the operator does not queue");
}

#[test]
fn the_cli_is_the_operator_for_routing_and_a_separate_issuer_in_the_log() {
    let h = harness();
    // Same routing as the operator: confirmation above `Write`...
    assert!(matches!(
        h.bus
            .dispatch(from("probe.outbound", Issuer::Cli))
            .expect("the CLI may ask"),
        Admitted::NeedsConfirmation { .. }
    ));
    // ...and it runs once confirmed, still recorded as the CLI.
    block_on(
        expect_running(
            h.bus
                .dispatch(from("probe.outbound", Issuer::Cli).confirmed())
                .expect("confirmed"),
        )
        .finish(),
    );
    assert_eq!(h.seen.issuer_of_last_call().kind(), "cli");
    assert!(
        h.log.text().contains("cli"),
        "the log tells the CLI apart from a browser: {}",
        h.log.text()
    );
}

#[test]
fn an_agent_runs_a_write_inside_its_grant_and_queues_anything_riskier() {
    let h = harness();
    h.grants.put(
        AgentSessionId::new("coding"),
        grant(&["probe.*"], RiskLevel::Destructive),
    );

    // Read-only and write run.
    for id in ["probe.read", "probe.write"] {
        let admitted = h
            .bus
            .dispatch(from(id, Issuer::agent("coding")))
            .unwrap_or_else(|e| panic!("{id} is inside the grant: {e}"));
        block_on(expect_running(admitted).finish());
    }
    assert_eq!(h.seen.count(), 2);

    // Cross-project, outbound and destructive wait in the Inbox.
    for id in ["probe.cross", "probe.outbound", "probe.destroy"] {
        match h
            .bus
            .dispatch(from(id, Issuer::agent("coding")))
            .unwrap_or_else(|e| panic!("{id} is inside the grant: {e}"))
        {
            Admitted::Queued { .. } => {}
            other => panic!("{id} must wait in the Inbox, got {other:?}"),
        }
    }
    assert_eq!(h.seen.count(), 2, "nothing above a write ran");
    assert_eq!(h.inbox.items().len(), 3);
}

#[test]
fn an_agent_cannot_confirm_its_own_destructive_command() {
    let h = harness();
    h.grants.put(
        AgentSessionId::new("coding"),
        grant(&["probe.*"], RiskLevel::Destructive),
    );

    // `confirmed` is the one thing a client is trusted about, and an agent is
    // a client: setting it must not be what gets past the Inbox.
    let request = from("probe.destroy", Issuer::agent("coding")).confirmed();
    match h.bus.dispatch(request).expect("inside the grant") {
        Admitted::Queued { risk, .. } => assert_eq!(risk, RiskLevel::Destructive),
        other => panic!("an agent's own confirmation must not run it, got {other:?}"),
    }
    assert_eq!(h.seen.count(), 0);
}

#[test]
fn a_command_outside_an_agents_grant_is_refused_and_never_queued() {
    let h = harness();
    h.grants.put(
        AgentSessionId::new("coding"),
        grant(&["probe.*"], RiskLevel::Destructive),
    );

    let refusal = h
        .bus
        .dispatch(from("other.read", Issuer::agent("coding")))
        .expect_err("outside the grant");

    match &refusal {
        Refused::OutsideGrant { session, command } => {
            assert_eq!(session.as_str(), "coding");
            assert_eq!(command.as_str(), "other.read");
        }
        other => panic!("expected a scope refusal, got {other:?}"),
    }
    assert!(
        h.inbox.items().is_empty(),
        "queuing a scope violation would turn it into a question for the operator"
    );
    assert_eq!(h.log.decisions(), vec![Decision::Refused]);
}

#[test]
fn a_command_above_an_agents_ceiling_is_refused_and_never_queued() {
    let h = harness();
    // Tether mode: a session that may not even ask about outbound work.
    h.grants.put(
        AgentSessionId::new("tethered"),
        grant(&["probe.*"], RiskLevel::Write),
    );

    let refusal = h
        .bus
        .dispatch(from("probe.outbound", Issuer::agent("tethered")))
        .expect_err("above the ceiling");

    match &refusal {
        Refused::AboveCeiling { risk, command, .. } => {
            assert_eq!(*risk, RiskLevel::Outbound);
            assert_eq!(command.as_str(), "probe.outbound");
        }
        other => panic!("expected a ceiling refusal, got {other:?}"),
    }
    assert!(
        h.inbox.items().is_empty(),
        "a ceiling that queues is not a ceiling"
    );
}

#[test]
fn an_agent_session_with_no_grant_can_run_nothing_at_all() {
    let h = harness();
    // No grant was put in the store, which is also the state of a bus with no
    // Assistant installed.
    let refusal = h
        .bus
        .dispatch(from("probe.read", Issuer::agent("stranger")))
        .expect_err("no grant, nothing runs");

    assert!(matches!(refusal, Refused::NoGrant { .. }));
    assert_eq!(h.seen.count(), 0, "not even a read-only command");
    assert!(h.bus.tools(&AgentSessionId::new("stranger")).is_empty());
}

#[test]
fn pausing_agents_stops_them_without_touching_the_operator() {
    let h = harness();
    h.grants.put(
        AgentSessionId::new("coding"),
        grant(&["probe.*"], RiskLevel::Destructive),
    );
    h.bus.pause_agents();

    let refusal = h
        .bus
        .dispatch(from("probe.read", Issuer::agent("coding")))
        .expect_err("agents are paused");
    assert!(matches!(refusal, Refused::AgentsPaused));

    // The operator's own desktop keeps working: lockdown pauses agents, and
    // revoking the operator's sessions is a different lever.
    block_on(
        expect_running(
            h.bus
                .dispatch(from("probe.read", Issuer::operator()))
                .expect("the operator is not an agent"),
        )
        .finish(),
    );

    h.bus.resume_agents();
    assert!(
        h.bus
            .dispatch(from("probe.read", Issuer::agent("coding")))
            .is_ok(),
        "resuming does not need the grants rebuilt"
    );
}

#[test]
fn queuing_with_no_inbox_attached_is_refused_rather_than_run() {
    // A bus with the honest defaults: no Assistant, so nothing to queue for.
    let seen = Seen::new();
    let registry = Arc::new(Registry::new());
    registry
        .register(ProbeDestroy(Arc::clone(&seen)))
        .expect("id");
    let grants = Arc::new(MemoryGrants::new());
    grants.put(
        AgentSessionId::new("coding"),
        grant(&["probe.*"], RiskLevel::Destructive),
    );
    let bus = Bus::new(registry, NodeId::new(NODE), TestEnv::new())
        .with_grants(Arc::clone(&grants) as Arc<dyn Grants>);

    let refusal = bus
        .dispatch(from("probe.destroy", Issuer::agent("coding")))
        .expect_err("there is no Inbox to wait in");

    assert!(matches!(refusal, Refused::NoInbox { .. }));
    assert_eq!(seen.count(), 0, "no Inbox must not mean it just runs");
}

#[test]
fn an_inbox_that_cannot_accept_a_command_does_not_run_it() {
    let seen = Seen::new();
    let registry = Arc::new(Registry::new());
    registry
        .register(ProbeDestroy(Arc::clone(&seen)))
        .expect("id");
    let grants = Arc::new(MemoryGrants::new());
    grants.put(
        AgentSessionId::new("coding"),
        grant(&["probe.*"], RiskLevel::Destructive),
    );
    let log = Log::new();
    let bus = Bus::new(registry, NodeId::new(NODE), TestEnv::new())
        .with_grants(Arc::clone(&grants) as Arc<dyn Grants>)
        .with_journal(Arc::clone(&log) as Arc<dyn Journal>)
        .with_approvals(Inbox::failing() as Arc<dyn Approvals>);

    let refusal = bus
        .dispatch(from("probe.destroy", Issuer::agent("coding")))
        .expect_err("the Inbox failed");

    assert!(matches!(refusal, Refused::InboxFailed { .. }));
    assert_eq!(seen.count(), 0);
    assert_eq!(log.decisions(), vec![Decision::Refused]);
}

// ---------------------------------------------------------------------------
// Approval: what is re-checked when the operator releases a queued command
// ---------------------------------------------------------------------------

#[test]
fn an_approved_command_runs_once_without_being_queued_again() {
    let h = harness();
    h.grants.put(
        AgentSessionId::new("coding"),
        grant(&["probe.*"], RiskLevel::Destructive),
    );
    let queued = h
        .bus
        .dispatch(from("probe.destroy", Issuer::agent("coding")))
        .expect("queued");
    let pending = match queued {
        Admitted::Queued { pending, .. } => pending,
        other => panic!("expected a queued command, got {other:?}"),
    };

    let running = h
        .bus
        .dispatch_approved(
            from("probe.destroy", Issuer::agent("coding")),
            Approval::granted(pending.clone()),
        )
        .expect("the operator approved it");
    block_on(running.finish());

    assert_eq!(h.seen.count(), 1);
    assert_eq!(h.inbox.items().len(), 1, "approving does not re-queue");
    assert!(
        h.log.text().contains(&format!("approved as {pending}")),
        "the log says which Inbox item released it: {}",
        h.log.text()
    );
}

#[test]
fn an_approved_command_is_refused_if_the_grant_went_away_while_it_waited() {
    let h = harness();
    let session = AgentSessionId::new("coding");
    h.grants
        .put(session.clone(), grant(&["probe.*"], RiskLevel::Destructive));
    let pending = match h
        .bus
        .dispatch(from("probe.destroy", Issuer::agent("coding")))
        .expect("queued")
    {
        Admitted::Queued { pending, .. } => pending,
        other => panic!("expected a queued command, got {other:?}"),
    };

    assert!(h.grants.revoke(&session));

    let refusal = h
        .bus
        .dispatch_approved(
            from("probe.destroy", Issuer::agent("coding")),
            Approval::granted(pending),
        )
        .expect_err("the grant was withdrawn while it waited");

    assert!(matches!(refusal, Refused::NoGrant { .. }));
    assert_eq!(h.seen.count(), 0);
}

#[test]
fn an_approved_command_is_refused_while_agents_are_paused() {
    let h = harness();
    h.grants.put(
        AgentSessionId::new("coding"),
        grant(&["probe.*"], RiskLevel::Destructive),
    );
    let pending = match h
        .bus
        .dispatch(from("probe.destroy", Issuer::agent("coding")))
        .expect("queued")
    {
        Admitted::Queued { pending, .. } => pending,
        other => panic!("expected a queued command, got {other:?}"),
    };

    // Lockdown: "freeze outbound actions: queued mail, pending Inbox items
    // and scheduled tasks". An Inbox item from before the pause is exactly a
    // pending item, so releasing it must not be a way around the pause.
    h.bus.pause_agents();

    let refusal = h
        .bus
        .dispatch_approved(
            from("probe.destroy", Issuer::agent("coding")),
            Approval::granted(pending),
        )
        .expect_err("agents are paused");

    assert!(matches!(refusal, Refused::AgentsPaused));
    assert_eq!(h.seen.count(), 0, "a paused agent's command does not run");
}

#[test]
fn an_approved_command_is_refused_if_its_app_stopped_while_it_waited() {
    let h = harness();
    h.grants.put(
        AgentSessionId::new("coding"),
        grant(&["probe.*"], RiskLevel::Destructive),
    );
    let pending = match h
        .bus
        .dispatch(from("probe.destroy", Issuer::agent("coding")))
        .expect("queued")
    {
        Admitted::Queued { pending, .. } => pending,
        other => panic!("expected a queued command, got {other:?}"),
    };

    // The app that provides the command was stopped while the operator
    // thought about it.
    assert!(h.registry.remove_namespace("probe") > 0);

    let refusal = h
        .bus
        .dispatch_approved(
            from("probe.destroy", Issuer::agent("coding")),
            Approval::granted(pending),
        )
        .expect_err("the command is gone");

    assert!(matches!(refusal, Refused::UnknownCommand(_)));
}

// ---------------------------------------------------------------------------
// Risk is unforgeable
// ---------------------------------------------------------------------------

#[test]
fn nothing_a_client_can_set_turns_a_destructive_command_into_a_read_only_one() {
    let h = harness();
    h.grants.put(
        AgentSessionId::new("coding"),
        grant(&["probe.*"], RiskLevel::Destructive),
    );

    // Every field a `Submission` has, in every combination a client could
    // send. There is no risk field and no issuer field to try, which is the
    // point: the only knobs are these.
    for confirmed in [false, true] {
        for target in [None, Some(NodeId::new(NODE))] {
            let submitted = Submission {
                command: "probe.destroy".to_string(),
                params: r#"{"text":"rm -rf"}"#.parse().expect("valid json"),
                target,
                confirmed,
            };
            let request = Request::from_submission(submitted, Issuer::agent("coding"))
                .expect("a valid command id");
            match h.bus.dispatch(request).expect("inside the grant") {
                Admitted::Queued { risk, .. } => assert_eq!(risk, RiskLevel::Destructive),
                other => panic!("confirmed={confirmed}: expected the Inbox, got {other:?}"),
            }
        }
    }
    assert_eq!(h.seen.count(), 0);
}

#[test]
fn the_risk_a_handler_is_told_is_the_one_the_registry_declared() {
    let h = harness();
    block_on(
        expect_running(
            h.bus
                .dispatch(from("probe.destroy", Issuer::operator()).confirmed())
                .expect("confirmed"),
        )
        .finish(),
    );
    assert_eq!(h.seen.risk_of_last_call(), RiskLevel::Destructive);
    assert_eq!(
        h.registry
            .spec(&CommandId::new("probe.destroy").expect("id"))
            .expect("registered")
            .risk(),
        RiskLevel::Destructive,
        "and the spec the UI is shown says the same thing"
    );
}

#[test]
fn two_backends_cannot_claim_one_command_id() {
    // The shadowing path: register a second `probe.destroy` with a gentler
    // risk and the first one's risk is what routing would stop using.
    let seen = Seen::new();
    let registry = Registry::new();
    registry
        .register(ProbeDestroy(Arc::clone(&seen)))
        .expect("the first registration");
    let refusal = registry
        .register(ProbeDestroy(Arc::clone(&seen)))
        .expect_err("the second must be refused");

    assert!(matches!(refusal, RegistryError::Duplicate(_)));
    assert_eq!(
        registry
            .spec(&CommandId::new("probe.destroy").expect("id"))
            .expect("still registered")
            .risk(),
        RiskLevel::Destructive
    );
}

#[test]
fn a_manifest_declared_risk_is_the_one_routing_uses() {
    // A CLI adapter's risk comes from `app.toml` in the install directory,
    // which is the backend as much as a Rust `impl` is. It must route the
    // same way: the operator is asked first.
    let registry = Arc::new(Registry::new());
    let declaration = Declaration {
        id: CommandId::new("adapter.purge").expect("id"),
        title: "Purge the cache".to_string(),
        risk: RiskLevel::Destructive,
        params: Default::default(),
        target: Target::ThisNode,
        redact_params: false,
    };
    registry
        .register_declared(declaration, Adapter)
        .expect("declared");

    let log = Log::new();
    let bus = Bus::new(Arc::clone(&registry), NodeId::new(NODE), TestEnv::new())
        .with_journal(Arc::clone(&log) as Arc<dyn Journal>);

    match bus
        .dispatch(from("adapter.purge", Issuer::operator()))
        .expect("the operator may ask")
    {
        Admitted::NeedsConfirmation { risk, .. } => assert_eq!(risk, RiskLevel::Destructive),
        other => panic!("a declared destructive command must be confirmed, got {other:?}"),
    }
    assert_eq!(log.decisions(), vec![Decision::AwaitingConfirmation]);
}

// ---------------------------------------------------------------------------
// The journal
// ---------------------------------------------------------------------------

#[test]
fn a_refused_dispatch_is_recorded_exactly_once() {
    let h = harness();
    let refusal = h
        .bus
        .dispatch(from("probe.gone", Issuer::operator()))
        .expect_err("unknown");

    let entries = h.log.entries();
    assert_eq!(
        entries.len(),
        1,
        "one refusal, one row: a burst of refusals is a lockdown trigger, and \
         double counting would fire it at half the rate"
    );
    assert_eq!(entries[0].decision, Decision::Refused);
    assert_eq!(entries[0].command.as_str(), "probe.gone");
    assert_eq!(
        entries[0].detail.as_deref(),
        Some(refusal.to_string().as_str()),
        "the row says why"
    );
    assert_eq!(
        entries[0].risk, None,
        "an unregistered command has no declared risk to record"
    );
}

#[test]
fn an_unknown_command_is_refused_with_its_name_in_the_message() {
    let h = harness();
    let refusal = h
        .bus
        .dispatch(from("probe.typo", Issuer::operator()))
        .expect_err("unknown");

    assert!(matches!(refusal, Refused::UnknownCommand(_)));
    assert!(
        refusal.to_string().contains("probe.typo"),
        "the operator has to know which command: {refusal}"
    );
}

#[test]
fn a_redacting_commands_parameters_never_reach_the_journal() {
    let h = harness();
    let secret = "correct horse battery staple";
    let submitted = Submission {
        command: "probe.secret".to_string(),
        params: format!(r#"{{"data":"{secret}"}}"#)
            .parse()
            .expect("valid json"),
        target: None,
        confirmed: false,
    };
    let request =
        Request::from_submission(submitted, Issuer::operator()).expect("a valid command id");

    block_on(expect_running(h.bus.dispatch(request).expect("write runs")).finish());

    assert_eq!(h.seen.count(), 1, "the handler still gets the real thing");
    assert!(
        !h.log.text().contains(secret),
        "keystrokes at a password prompt must not land in a log that is \
         readable from the terminal by design: {}",
        h.log.text()
    );
    assert!(
        h.log.entries().iter().all(|e| e
            .params
            .as_ref()
            .is_none_or(|p| !p.to_string().contains("data"))),
        "not even the shape of the parameters"
    );
}

#[test]
fn a_redacting_commands_parameters_are_not_logged_when_it_is_refused_either() {
    let h = harness();
    h.grants.put(
        AgentSessionId::new("coding"),
        // `probe.secret` is outside this grant, so the dispatch is refused —
        // on the refusal path, where the row is written by a different branch.
        grant(&["other.*"], RiskLevel::Destructive),
    );
    let secret = "hunter2";
    let submitted = Submission {
        command: "probe.secret".to_string(),
        params: format!(r#"{{"data":"{secret}"}}"#)
            .parse()
            .expect("valid json"),
        target: None,
        confirmed: false,
    };
    let request =
        Request::from_submission(submitted, Issuer::agent("coding")).expect("a valid command id");

    h.bus.dispatch(request).expect_err("outside the grant");

    assert_eq!(h.log.decisions(), vec![Decision::Refused]);
    assert!(
        !h.log.text().contains(secret),
        "a refusal is still a log row: {}",
        h.log.text()
    );
}

#[test]
fn an_unregistered_commands_parameters_are_not_recorded() {
    let h = harness();
    // The scenario: the terminal app's namespace was removed while a browser
    // was typing, so the keystrokes of `terminal.write` arrive for a command
    // that no longer has a spec — and so no longer has a redaction flag.
    let secret = "s3cret-passphrase";
    let submitted = Submission {
        command: "probe.vanished".to_string(),
        params: format!(r#"{{"data":"{secret}"}}"#)
            .parse()
            .expect("valid json"),
        target: None,
        confirmed: false,
    };
    let request =
        Request::from_submission(submitted, Issuer::operator()).expect("a valid command id");

    h.bus.dispatch(request).expect_err("unknown command");

    assert_eq!(h.log.decisions(), vec![Decision::Refused]);
    assert!(
        !h.log.text().contains(secret),
        "parameters with no spec behind them cannot be known safe to log: {}",
        h.log.text()
    );
}

#[test]
fn a_destructive_command_is_refused_when_its_admission_cannot_be_recorded() {
    let seen = Seen::new();
    let registry = Arc::new(Registry::new());
    registry
        .register(ProbeDestroy(Arc::clone(&seen)))
        .expect("id");
    registry.register(ProbeRead(Arc::clone(&seen))).expect("id");
    let bus = Bus::new(registry, NodeId::new(NODE), TestEnv::new())
        .with_journal(Log::broken() as Arc<dyn Journal>);

    let refusal = bus
        .dispatch(from("probe.destroy", Issuer::operator()).confirmed())
        .expect_err("an unlogged destructive command is what the log exists for");
    match &refusal {
        Refused::NotRecorded { risk, .. } => assert_eq!(*risk, RiskLevel::Destructive),
        other => panic!("expected a recording refusal, got {other:?}"),
    }
    assert_eq!(seen.count(), 0, "it must not have started");

    // A read-only command is not worth taking the desktop down over.
    block_on(
        expect_running(
            bus.dispatch(from("probe.read", Issuer::operator()))
                .expect("a read still runs"),
        )
        .finish(),
    );
    assert_eq!(seen.count(), 1);
}

#[test]
fn a_running_command_is_recorded_on_admission_and_again_on_its_outcome() {
    let h = harness();
    let running = expect_running(
        h.bus
            .dispatch(from("probe.read", Issuer::operator()))
            .expect("runs"),
    );
    let dispatch = running.dispatch().clone();
    block_on(running.finish());

    let entries = h.log.entries();
    assert_eq!(entries.len(), 2);
    assert!(
        entries.iter().all(|e| e.dispatch == dispatch),
        "both rows are about the one dispatch"
    );
    assert_eq!(entries[0].decision, Decision::Ran);
    assert_eq!(entries[1].decision, Decision::Completed);
    assert!(
        entries[1].at >= entries[0].at,
        "the injected clock orders them"
    );
}

#[test]
fn a_failing_command_is_recorded_as_failed_with_its_reason() {
    // `probe.secret` takes a map of strings; an integer makes the bus's own
    // parameter check fail, which is the failure path a handler never sees.
    let h = harness();
    let submitted = Submission {
        command: "probe.secret".to_string(),
        params: r#"{"data":7}"#.parse().expect("valid json"),
        target: None,
        confirmed: false,
    };
    let request =
        Request::from_submission(submitted, Issuer::operator()).expect("a valid command id");
    let done = block_on(expect_running(h.bus.dispatch(request).expect("admitted")).finish());

    assert!(!done.succeeded());
    assert!(matches!(done.outcome, Err(CommandError::BadParams { .. })));
    assert_eq!(h.seen.count(), 0, "the handler never ran");
    assert_eq!(h.log.decisions(), vec![Decision::Ran, Decision::Failed]);
    let text = h.log.text();
    assert!(
        text.contains("not valid"),
        "the row says what went wrong: {text}"
    );
}

// ---------------------------------------------------------------------------
// Attribution
// ---------------------------------------------------------------------------

#[test]
fn a_serialized_issuer_carries_a_session_digest_and_never_the_session_id() {
    let h = harness();
    // 43 base64url characters is one session id. `decode` is the only public
    // way in, which is itself the guarantee: a browser session is established
    // by the layer that authenticated it.
    let credential = "A".repeat(43);
    let session = SessionId::decode(&credential).expect("a session id");
    let issuer = Issuer::browser(session);
    let tag = issuer
        .browser_session()
        .expect("the operator's browser")
        .tag()
        .to_string();

    let done = block_on(
        expect_running(
            h.bus
                .dispatch(from("probe.whoami", issuer.clone()))
                .expect("runs"),
        )
        .finish(),
    );

    // Serialized by the bus's own serializer, on the way to a client.
    let wire = done.outcome.expect("ran").to_string();
    assert!(
        !wire.contains(&credential),
        "no frame on its way to a client may carry the credential: {wire}"
    );
    assert!(
        wire.contains(&tag),
        "but it must still say which browser: {wire}"
    );
    assert!(
        !h.log.text().contains(&credential),
        "nor may the log: {}",
        h.log.text()
    );
    assert!(h.log.text().contains(&tag));
    assert!(
        !format!("{issuer:?} {issuer}").contains(&credential),
        "nor a debug line"
    );
    assert!(
        !credential.starts_with(&tag),
        "the tag is a digest, not a prefix of the credential: shortening a \
         secret does not stop it being one"
    );
}

#[test]
fn an_operator_action_with_no_browser_behind_it_has_no_session_to_own_things() {
    // What a handler asks before it ties a pty's life to a browser: `None`
    // means the host owns it, not "unknown".
    assert!(Issuer::operator().browser_session().is_none());
    assert!(Issuer::Cli.browser_session().is_none());
    assert!(Issuer::agent("coding").browser_session().is_none());

    // 43 base64url characters, the last one carrying no bits that would be
    // dropped — `decode` rejects a non-canonical id.
    let session = SessionId::decode(&format!("{}A", "B".repeat(42))).expect("a session id");
    assert!(Issuer::browser(session).browser_session().is_some());
}

// ---------------------------------------------------------------------------
// Grants, ids and the tool surface
// ---------------------------------------------------------------------------

#[test]
fn a_namespace_grant_does_not_reach_a_similarly_named_namespace() {
    let h = harness();
    // `probes.read` is not inside `probe.*`, however much it looks like it.
    h.grants.put(
        AgentSessionId::new("coding"),
        grant(&["probe.*"], RiskLevel::Destructive),
    );

    let refusal = h
        .bus
        .dispatch(from("probes.read", Issuer::agent("coding")))
        .expect_err("a part boundary is not a string prefix");
    assert!(matches!(refusal, Refused::OutsideGrant { .. }));
}

#[test]
fn an_exact_grant_does_not_cover_the_commands_below_it() {
    let exact = pattern("probe.read");
    assert!(exact.matches(&CommandId::new("probe.read").expect("id")));
    assert!(
        !exact.matches(&CommandId::new("probe.read.all").expect("id")),
        "`probe.read` is one command, not a prefix"
    );

    let wildcard = pattern("probe.read.*");
    assert!(wildcard.matches(&CommandId::new("probe.read.all").expect("id")));
    assert!(
        !wildcard.matches(&CommandId::new("probe.read").expect("id")),
        "`probe.read.*` is what is below `probe.read`, not `probe.read` itself"
    );
}

#[test]
fn a_grant_of_the_whole_machine_cannot_be_written() {
    assert!(matches!(
        Pattern::new("*"),
        Err(IdError::PatternMatchesEverything)
    ));
    // A wildcard anywhere but the end would grant more than it reads as.
    assert!(matches!(
        Pattern::new("probe.*.read"),
        Err(IdError::BadPattern(_))
    ));
    assert!(matches!(
        Pattern::new("*.read"),
        Err(IdError::BadPattern(_))
    ));
}

#[test]
fn an_agent_is_told_about_no_tool_whose_use_would_be_refused() {
    let h = harness();
    let session = AgentSessionId::new("tethered");
    h.grants
        .put(session.clone(), grant(&["probe.*"], RiskLevel::Write));

    let tools: Vec<String> = h
        .bus
        .tools(&session)
        .iter()
        .map(|t| t.name.clone())
        .collect();

    assert!(tools.contains(&"probe__read".to_string()));
    assert!(tools.contains(&"probe__write".to_string()));
    assert!(
        !tools
            .iter()
            .any(|t| t == "probe__outbound" || t == "probe__destroy"),
        "above the ceiling, so describing it would be an offer the bus refuses: {tools:?}"
    );
    assert!(
        !tools.iter().any(|t| t.starts_with("other__")),
        "outside the namespaces: {tools:?}"
    );
    assert!(
        tools.windows(2).all(|w| w[0] <= w[1]),
        "stable order so the tool list does not reshuffle itself"
    );
}

#[test]
fn a_command_id_survives_the_trip_through_an_mcp_tool_name() {
    let id = CommandId::new("filmcraft.timeline.trim_end").expect("id");
    assert_eq!(id.to_tool_name(), "filmcraft__timeline__trim_end");
    assert_eq!(
        CommandId::from_tool_name(&id.to_tool_name()).expect("round trip"),
        id,
        "the rewrite is only reversible because ids reject `__`"
    );
    assert!(matches!(
        CommandId::new("probe__read"),
        Err(IdError::DoubleUnderscore(_))
    ));
}

#[test]
fn an_id_that_would_collide_between_apps_is_rejected() {
    assert!(matches!(
        CommandId::new("status"),
        Err(IdError::NotNamespaced(_))
    ));
    assert!(matches!(
        CommandId::new("Probe.Read"),
        Err(IdError::BadCharacters(_))
    ));
    assert!(matches!(
        CommandId::new("probe.2read"),
        Err(IdError::BadCharacters(_))
    ));
    assert!(matches!(
        CommandId::new("probe."),
        Err(IdError::BadCharacters(_))
    ));
    assert!(matches!(
        CommandId::new(format!("probe.{}", "x".repeat(MAX_ID_LEN))),
        Err(IdError::TooLong(_))
    ));
    // And a bad id never reaches the bus at all: `from_submission` is the only
    // way in from the wire.
    assert!(Request::from_submission(submission("status"), Issuer::operator()).is_err());
}

#[test]
fn removing_an_apps_namespace_takes_only_its_own_commands() {
    let h = harness();
    let before = h.bus.list(None).len();
    let removed = h.registry.remove_namespace("probe");

    assert!(removed > 0);
    assert_eq!(h.bus.list(None).len(), before - removed);
    assert!(
        h.registry
            .contains(&CommandId::new("other.read").expect("id")),
        "another app's commands are not collateral"
    );
    assert!(
        h.registry
            .contains(&CommandId::new("probes.read").expect("id")),
        "nor is a namespace that merely starts the same way"
    );
}

// ---------------------------------------------------------------------------
// Enablement, nodes and progress
// ---------------------------------------------------------------------------

#[test]
fn a_command_whose_backend_is_not_ready_is_refused_with_the_reason_shown() {
    let h = harness();
    let refusal = h
        .bus
        .dispatch(from("probe.asleep", Issuer::operator()))
        .expect_err("disabled");

    match &refusal {
        Refused::Disabled { reason, .. } => assert_eq!(reason, "no project is open"),
        other => panic!("expected a disabled refusal, got {other:?}"),
    }
    assert!(refusal.to_string().contains("no project is open"));
}

#[test]
fn a_caller_cannot_move_a_command_off_the_node_its_backend_declared() {
    let h = harness();
    // `probe.read` is `Target::ThisNode`; asking for another node must not
    // quietly run it here.
    let request = Request::from_submission(
        Submission {
            command: "probe.read".to_string(),
            params: r#"{}"#.parse().expect("valid json"),
            target: Some(NodeId::new(OTHER_NODE)),
            confirmed: false,
        },
        Issuer::operator(),
    )
    .expect("a valid command id");

    let refusal = h.bus.dispatch(request).expect_err("not this node");
    match &refusal {
        Refused::WrongNodeAsked { asked, runs_on, .. } => {
            // The refusal has to name the two machines the right way round: a
            // message claiming a local command "runs on node-b" sends whoever
            // reads it looking at the wrong host.
            assert_eq!(asked.as_str(), OTHER_NODE);
            assert_eq!(runs_on.as_str(), NODE);
        }
        other => panic!("expected a wrong-node refusal, got {other:?}"),
    }
    let message = refusal.to_string();
    assert!(
        message.contains(&format!("runs on node {NODE}"))
            && message.contains(&format!("asked for on node {OTHER_NODE}")),
        "the message says where it runs and what was asked for: {message}"
    );
    assert_eq!(
        h.seen.count(),
        0,
        "the wrong machine's files are the one thing worth refusing over"
    );

    // A command pinned elsewhere is refused even with no node asked for, and
    // that one really is somewhere else.
    match h
        .bus
        .dispatch(from("probe.elsewhere", Issuer::operator()))
        .expect_err("pinned elsewhere")
    {
        Refused::NotLocal { wanted, this, .. } => {
            assert_eq!(wanted.as_str(), OTHER_NODE);
            assert_eq!(this.as_str(), NODE);
        }
        other => panic!("expected a not-local refusal, got {other:?}"),
    }
}

#[test]
fn an_any_node_command_may_be_asked_for_by_name_on_the_node_running_it() {
    let h = harness();
    let request = Request::from_submission(
        Submission {
            command: "probe.anywhere".to_string(),
            params: r#"{}"#.parse().expect("valid json"),
            target: Some(NodeId::new(NODE)),
            confirmed: false,
        },
        Issuer::operator(),
    )
    .expect("a valid command id");

    let done = block_on(expect_running(h.bus.dispatch(request).expect("runs")).finish());
    assert_eq!(done.parse::<String>().expect("output"), NODE);
    assert_eq!(done.node.as_str(), NODE);
}

#[test]
fn an_agent_scoped_to_other_nodes_may_not_run_anything_here() {
    let h = harness();
    h.grants.put(
        AgentSessionId::new("coding"),
        Grant::new(
            vec![pattern("probe.*")],
            NodeScope::Nodes(vec![NodeId::new(OTHER_NODE)]),
        ),
    );

    let refusal = h
        .bus
        .dispatch(from("probe.read", Issuer::agent("coding")))
        .expect_err("not this node's session");
    match &refusal {
        Refused::NodeNotAllowed { node, .. } => assert_eq!(node.as_str(), NODE),
        other => panic!("expected a node refusal, got {other:?}"),
    }
}

#[test]
fn progress_is_dropped_rather_than_slowing_a_command_down() {
    let h = harness();
    let bus = Bus::new(Arc::clone(&h.registry), NodeId::new(NODE), TestEnv::new())
        .with_journal(Arc::clone(&h.log) as Arc<dyn Journal>)
        .with_progress_buffer(4);

    let running = expect_running(
        bus.dispatch(from("probe.chatty", Issuer::operator()))
            .expect("runs"),
    );
    let (mut progress, finish) = running.split();

    // Nobody is draining the channel while the command reports 500 steps. It
    // finishes anyway: a command must not be throttled by how fast its
    // audience reads.
    let done = block_on(finish);
    assert!(done.succeeded());

    let mut received = Vec::new();
    while let Ok(update) = progress.try_recv() {
        received.push(update);
    }
    assert!(
        !received.is_empty() && received.len() <= 4,
        "the buffer holds what it says and drops the rest, got {}",
        received.len()
    );
}

#[test]
fn a_progress_fraction_outside_zero_to_one_is_clamped_rather_than_reported() {
    let h = harness();
    let bus = Bus::new(Arc::clone(&h.registry), NodeId::new(NODE), TestEnv::new())
        .with_journal(Arc::clone(&h.log) as Arc<dyn Journal>)
        // Room for every update the chatty probe sends, so the overshoot at
        // the end is not one of the dropped ones.
        .with_progress_buffer(1024);

    let running = expect_running(
        bus.dispatch(from("probe.chatty", Issuer::operator()))
            .expect("runs"),
    );
    let (mut progress, finish) = running.split();
    block_on(finish);

    let mut fractions = Vec::new();
    while let Ok(update) = progress.try_recv() {
        if let Progress::Step { fraction, .. } = update {
            fractions.extend(fraction);
        }
    }
    assert!(
        fractions.iter().all(|f| (0.0..=1.0).contains(f)),
        "a bar cannot be painted past its end: {fractions:?}"
    );
    assert!(
        fractions.contains(&1.0),
        "the overshoot arrives clamped, not dropped"
    );
}

// ---------------------------------------------------------------------------
// The palette
// ---------------------------------------------------------------------------

#[test]
fn the_palette_lists_what_an_agent_may_use_and_the_operator_everything() {
    let h = harness();
    let everything = h.bus.list(None);
    assert!(everything.len() > 3);
    assert!(
        everything
            .windows(2)
            .all(|w| w[0].spec.id() <= w[1].spec.id()),
        "id order, so the palette does not reshuffle between renders"
    );
    assert!(
        everything
            .iter()
            .any(|l| l.spec.id().as_str() == "probe.asleep" && !l.enablement.is_enabled()),
        "a command that cannot run now is still listed, with the reason"
    );

    let scoped = h
        .bus
        .list(Some(&grant(&["other.*"], RiskLevel::Destructive)));
    assert_eq!(scoped.len(), 1);
    assert_eq!(scoped[0].spec.id().as_str(), "other.read");
}
