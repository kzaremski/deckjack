//! Grants and the Inbox as they are actually wired.
//!
//! `tests/grants.rs` already drives `grants.rs` through its own doubles, and
//! every rule in it passed while the feature could not happen at runtime:
//! `Bus::new` fills its grant and Inbox slots with refusing stubs, so every
//! agent dispatch was refused before routing looked at the command. A test
//! that builds its own objects and attaches its own doubles cannot see that.
//!
//! So every test below goes through a real [`Bus`] with the real facades
//! attached, over a real migrated `state.db`, exactly as [`Harness::new`]
//! assembles it — which is what `main.rs` assembles. Delete
//! `with_grants(...)`, `with_approvals(...)` or `register_approvals(...)` from
//! that assembly and these fail; that is the point of them.
//!
//! Nothing here sleeps. The clock is injected, so expiry is driven by
//! advancing it.

use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use deckjack_core::approvals::{Approvals, Facts, GrantRows, InboxRows, register_approvals};
use deckjack_core::auth::code::RandomError;
use deckjack_core::command::{
    Admitted, AgentSessionId, Bus, Command, CommandError, CommandId, Context, Grants as BusGrants,
    Issuer, NodeId, OsEnv, PendingId, Registry, Request, RiskLevel, Running,
};
use deckjack_core::grants::{
    CommandFacts, Env, Grant, GrantId, GrantSpec, GrantStore, InboxStore, Issuer as GrantIssuer,
    ItemId, ItemStatus, NodeScope, Pattern, PendingItem, Policy, QueueReason, Risk, Scale,
};
use deckjack_core::lockdown::{self, Identity, Lockdown, SecondFactor, Trigger, UnlockRequest};
use deckjack_core::store::Store;
use deckjack_core::wiring::NoParams;

const NODE: &str = "node-a";
const AGENT: &str = "Assistant";

// ---------------------------------------------------------------------------
// The injected clock
// ---------------------------------------------------------------------------

/// Time and ids, supplied. Cloneable so the test keeps a handle after one has
/// been moved into the subsystem.
#[derive(Clone)]
struct Clock {
    inner: Arc<ClockState>,
}

struct ClockState {
    now: AtomicU64,
    ids: AtomicU64,
}

impl Clock {
    fn new() -> Clock {
        Clock {
            inner: Arc::new(ClockState {
                // Not zero: a bug that confuses "no timestamp" with "epoch"
                // must not be masked by the clock starting at the epoch.
                now: AtomicU64::new(1_700_000_000),
                ids: AtomicU64::new(1),
            }),
        }
    }

    fn advance(&self, secs: u64) {
        self.inner.now.fetch_add(secs, Ordering::SeqCst);
    }
}

impl Env for Clock {
    fn now(&self) -> u64 {
        self.inner.now.load(Ordering::SeqCst)
    }

    fn new_id(&self) -> Result<String, RandomError> {
        Ok(format!(
            "id-{}",
            self.inner.ids.fetch_add(1, Ordering::SeqCst)
        ))
    }
}

// ---------------------------------------------------------------------------
// Commands to dispatch
// ---------------------------------------------------------------------------

/// A destructive command, so an agent's dispatch of it has to wait. Counts its
/// own runs: "was it queued" and "did it nevertheless run" are different
/// questions, and a test that only read the bus's answer could not tell them
/// apart.
struct Wipe {
    ran: Arc<AtomicUsize>,
}

impl Command for Wipe {
    const ID: &'static str = "probe.wipe";
    const TITLE: &'static str = "Wipe the probe";
    const RISK: RiskLevel = RiskLevel::Destructive;

    type Params = NoParams;
    type Output = bool;

    async fn run(&self, _p: NoParams, _cx: Context) -> Result<bool, CommandError> {
        self.ran.fetch_add(1, Ordering::SeqCst);
        Ok(true)
    }
}

/// An ordinary read, which a granted agent may run without asking.
struct Peek;

impl Command for Peek {
    const ID: &'static str = "probe.read";
    const TITLE: &'static str = "Read the probe";
    const RISK: RiskLevel = RiskLevel::ReadOnly;

    type Params = NoParams;
    type Output = bool;

    async fn run(&self, _p: NoParams, _cx: Context) -> Result<bool, CommandError> {
        Ok(true)
    }
}

// ---------------------------------------------------------------------------
// Lockdown's other dependencies
// ---------------------------------------------------------------------------

/// The lockdown journal in memory. Only `outbound` is under test here; a real
/// `FileJournal` would add a file to prove nothing this file claims.
struct MemJournal {
    record: Mutex<Option<lockdown::Record>>,
}

impl lockdown::Journal for MemJournal {
    fn load(&self) -> Result<Option<lockdown::Record>, lockdown::JournalError> {
        Ok(self.record.lock().expect("the journal").clone())
    }

    fn save(&self, record: &lockdown::Record) -> Result<(), lockdown::JournalError> {
        *self.record.lock().expect("the journal") = Some(record.clone());
        Ok(())
    }
}

struct NoSessions;

impl lockdown::Sessions for NoSessions {
    fn revoke_all(&self) -> usize {
        0
    }
    fn adopt_signing_key(&self, _key: lockdown::SessionKey) {}
}

// ---------------------------------------------------------------------------
// The harness: the assembly under test
// ---------------------------------------------------------------------------

/// A migrated `state.db`, the subsystem over it, and a bus with both facades
/// attached and the commands registered.
///
/// This is `main.rs`'s assembly with the parts that do not decide anything
/// here left out. What it must keep is exactly the three lines the operator
/// adds: both facades on the bus, and the registration.
struct Harness {
    /// Held so the working directory, and the database in it, outlive the
    /// harness.
    _dir: tempfile::TempDir,
    approvals: Arc<Approvals>,
    bus: Arc<Bus>,
    registry: Arc<Registry>,
    clock: Clock,
    ran: Arc<AtomicUsize>,
}

impl Harness {
    fn new() -> Harness {
        Harness::with_ceiling(RiskLevel::Destructive)
    }

    fn with_ceiling(ceiling: RiskLevel) -> Harness {
        let dir = tempfile::tempdir().expect("a working directory");
        // Through `Store::open`, as the server does: a fixture that created
        // the `grants` and `inbox` tables itself would hide a schema the
        // migrations do not actually produce, which is half of what these
        // tests are for.
        let store = Arc::new(Mutex::new(
            Store::open(dir.path()).expect("the state database"),
        ));
        let clock = Clock::new();
        let ran = Arc::new(AtomicUsize::new(0));

        let registry = Arc::new(Registry::new());
        registry
            .register(Wipe {
                ran: Arc::clone(&ran),
            })
            .expect("probe.wipe registers");
        registry.register(Peek).expect("probe.read registers");

        let approvals = Approvals::open_with(
            Arc::clone(&store),
            ceiling,
            Policy::default(),
            Arc::new(clock.clone()),
        );
        let bus = Arc::new(
            Bus::new(Arc::clone(&registry), NodeId::new(NODE), Arc::new(OsEnv))
                .with_grants(approvals.grants())
                .with_approvals(approvals.inbox()),
        );
        register_approvals(&registry, &approvals, &bus).expect("the approval commands register");

        Harness {
            _dir: dir,
            approvals,
            bus,
            registry,
            clock,
            ran,
        }
    }

    /// A grant held by `owner`, over `namespaces`, on every node.
    fn grant(&self, owner: &str, namespaces: &[&str], ttl_secs: Option<u64>) -> GrantId {
        self.approvals
            .issue(
                owner,
                GrantSpec {
                    label: format!("{owner}'s grant"),
                    session: None,
                    project: None,
                    namespaces: namespaces.iter().map(|s| s.to_string()).collect(),
                    nodes: Some(NodeScope::All),
                    ttl_secs,
                },
            )
            .expect("the grant issues")
            .grant
            .id
    }

    fn dispatch(&self, command: &str, params: serde_json::Value, issuer: Issuer) -> Admitted {
        self.bus
            .dispatch(Request::new(
                CommandId::new(command).expect("a valid command id"),
                params,
                issuer,
            ))
            .unwrap_or_else(|e| panic!("{command} was refused: {e}"))
    }

    fn try_dispatch(
        &self,
        command: &str,
        params: serde_json::Value,
        issuer: Issuer,
    ) -> Result<Admitted, deckjack_core::command::Refused> {
        self.bus.dispatch(Request::new(
            CommandId::new(command).expect("a valid command id"),
            params,
            issuer,
        ))
    }

    /// An agent's `probe.wipe`, which the bus must queue rather than run.
    fn queue_a_wipe(&self) -> PendingId {
        match self.dispatch("probe.wipe", serde_json::json!({}), Issuer::agent(AGENT)) {
            Admitted::Queued { pending, risk, .. } => {
                assert_eq!(
                    risk,
                    RiskLevel::Destructive,
                    "the risk the operator will be shown comes from the backend, not the caller"
                );
                pending
            }
            other => panic!("a destructive agent command did not wait: {other:?}"),
        }
    }

    /// Approve as the operator, through the bus, and return what the command
    /// answered. `accept_unknown` because every item a bus dispatch produces
    /// has an unknown scale; see `approvals::Facts::scale`.
    async fn approve(&self, item: &PendingId) -> Result<serde_json::Value, String> {
        let admitted = self
            .try_dispatch(
                "system.inbox.approve",
                serde_json::json!({ "item": item.to_string(), "accept_unknown": true }),
                Issuer::operator(),
            )
            .map_err(|e| e.to_string())?;
        let completion = expect_running(admitted).finish().await;
        completion.outcome.map_err(|e| e.to_string())
    }

    fn runs(&self) -> usize {
        self.ran.load(Ordering::SeqCst)
    }

    /// Deny as the operator, through the bus.
    async fn deny(&self, item: &PendingId) -> Result<serde_json::Value, String> {
        let admitted = self
            .try_dispatch(
                "system.inbox.deny",
                serde_json::json!({ "item": item.to_string(), "note": "not today" }),
                Issuer::operator(),
            )
            .map_err(|e| e.to_string())?;
        expect_running(admitted)
            .finish()
            .await
            .outcome
            .map_err(|e| e.to_string())
    }

    /// A real `Lockdown` with this subsystem as its step 3, followed so that
    /// clearing it clears the freeze — the assembly `main.rs` builds.
    fn lockdown(&self) -> Arc<Lockdown> {
        let deps = lockdown::Deps {
            clock: Box::new(lockdown::OsClock),
            random: Box::new(lockdown::OsRandom),
            sessions: Box::new(NoSessions),
            // No children, so the grace period is never waited out: `reap`
            // returns on an empty list.
            children: Box::new(lockdown::NoChildren),
            signals: Box::new(lockdown::OsSignals::new()),
            outbound: self.approvals.outbound(),
            passkeys: Box::new(lockdown::PasskeysUnavailable),
            journal: Box::new(MemJournal {
                record: Mutex::new(None),
            }),
        };
        let lockdown = Arc::new(
            Lockdown::restore(deps, lockdown::Policy::default(), NODE.to_string())
                .expect("the journal is readable"),
        );
        self.approvals.follow_lockdown(Arc::downgrade(&lockdown));
        lockdown
    }
}

fn expect_running(admitted: Admitted) -> Running {
    match admitted {
        Admitted::Running(running) => running,
        other => panic!("expected the command to run, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Storage
// ---------------------------------------------------------------------------

/// The one test that proves the schema really matches the types. Everything
/// else here would still pass against an in-memory store; this would not.
///
/// If it fails, a grant or an approval does not survive a restart: the
/// operator comes back to agents whose scope the server has forgotten, or to
/// an approval nobody can see they were asked for.
#[test]
fn a_grant_and_an_inbox_item_survive_a_restart_with_every_field_intact() {
    let dir = tempfile::tempdir().expect("a working directory");

    let grant = Grant {
        id: GrantId::from_text("grant-1"),
        label: "Assistant on the R620".to_string(),
        session: Some("session-abc".to_string()),
        owner: Some(AGENT.to_string()),
        project: Some("/srv/footage".to_string()),
        namespaces: vec![
            Pattern::parse("files.read.*").expect("a pattern"),
            Pattern::parse("git.status").expect("a pattern"),
        ],
        nodes: NodeScope::Only(vec!["r620".to_string(), "node-a".to_string()]),
        issued_at: 1_700_000_000,
        expires_at: Some(1_700_003_600),
        revoked_at: Some(1_700_001_000),
    };

    // Two items, because the `scale_kind`/`scale_count` split has a NULL case
    // and a counted case and they are stored in different columns.
    let counted = PendingItem {
        id: ItemId::from_text("item-1"),
        grant: Some(grant.id.clone()),
        issuer: GrantIssuer::Session {
            label: AGENT.to_string(),
            session: Some("session-abc".to_string()),
        },
        command: "files.delete".to_string(),
        target_node: "r620".to_string(),
        risk: Risk::Destructive,
        reason: QueueReason::Bulk { count: 412 },
        scale: Scale::Count(412),
        title: "Delete 412 files under ~/Footage".to_string(),
        detail: Some("everything older than a year".to_string()),
        params: serde_json::json!({ "path": "~/Footage", "older_than_days": 365 }),
        created: 1_700_000_500,
        expires: Some(1_700_086_900),
        status: ItemStatus::Denied,
        decided_at: Some(1_700_000_600),
        decided_note: Some("not this week".to_string()),
    };
    let unmeasured = PendingItem {
        id: ItemId::from_text("item-2"),
        grant: None,
        issuer: GrantIssuer::Cli,
        command: "mail.send".to_string(),
        target_node: "node-a".to_string(),
        risk: Risk::Outbound,
        reason: QueueReason::ScaleUnknown,
        scale: Scale::Unknown,
        title: "Send the week's digest".to_string(),
        detail: None,
        params: serde_json::json!(null),
        created: 1_700_000_700,
        expires: None,
        status: ItemStatus::Pending,
        decided_at: None,
        decided_note: None,
    };

    {
        let store = Arc::new(Mutex::new(Store::open(dir.path()).expect("the database")));
        let rows = GrantRows::new(Arc::clone(&store));
        let items = InboxRows::new(Arc::clone(&store));
        rows.put_grant(&grant).expect("the grant is written");
        items.put_item(&counted).expect("the item is written");
        items.put_item(&unmeasured).expect("the item is written");
    }
    // Dropped, so the next `Store::open` is a genuinely new connection to the
    // file rather than the same one in memory.

    let store = Arc::new(Mutex::new(Store::open(dir.path()).expect("the database")));
    let rows = GrantRows::new(Arc::clone(&store));
    let items = InboxRows::new(Arc::clone(&store));

    let read = rows
        .grant(&grant.id)
        .expect("readable")
        .expect("still there");
    assert_eq!(read, grant, "every grant column round-trips through SQLite");

    let owned = rows.all_owned().expect("readable");
    assert_eq!(owned.len(), 1);
    assert_eq!(
        owned[0].owner.as_deref(),
        Some(AGENT),
        "the owner column is what a standing grant is looked up by; losing it \
         would hand the grant to nobody"
    );

    assert_eq!(
        items.item(&counted.id).expect("readable"),
        Some(counted.clone()),
        "a denied item keeps its count, its note and its parameters: it is the \
         record of what was asked and refused"
    );
    assert_eq!(
        items.item(&unmeasured.id).expect("readable"),
        Some(unmeasured.clone()),
        "an unmeasured item round-trips with no count at all, which is the \
         column's NULL case"
    );

    assert_eq!(
        items
            .items_with_status(ItemStatus::Pending)
            .expect("readable"),
        vec![unmeasured],
        "the Inbox lists only what is waiting, so a decided item cannot be \
         tapped a second time"
    );
    assert_eq!(
        items.recent_items(10).expect("readable").len(),
        2,
        "history shows decided items too"
    );
}

// ---------------------------------------------------------------------------
// The owner column
// ---------------------------------------------------------------------------

/// The invariant the ignored test in `tests/grants.rs` names, on the path the
/// bus actually uses.
///
/// If it fails, one sessionless issuer inherits the union of every standing
/// grant: the always-on assistant's narrow grant would carry whatever the
/// operator's own tooling holds, and an agent would run commands nobody
/// granted it.
#[tokio::test]
async fn a_standing_grant_for_one_owner_does_not_authorize_a_different_issuer() {
    let h = Harness::new();

    // The operator's own tooling holds the wide grant; the Assistant holds one
    // narrow namespace. Both are standing: no expiry, no session.
    h.grant("operator tooling", &["probe.*"], None);
    h.grant(AGENT, &["probe.read"], None);

    let grant = BusGrants::grant(h.approvals.as_ref(), &AgentSessionId::new(AGENT))
        .expect("the Assistant holds a grant");
    assert!(
        grant.covers(&CommandId::new("probe.read").expect("id")),
        "its own namespace is granted"
    );
    assert!(
        !grant.covers(&CommandId::new("probe.wipe").expect("id")),
        "a standing grant held by someone else is not the Assistant's to use"
    );

    let refused = h
        .try_dispatch("probe.wipe", serde_json::json!({}), Issuer::agent(AGENT))
        .expect_err("the Assistant may not wipe");
    assert!(
        refused.to_string().contains("was not granted"),
        "the refusal names the scope, not the risk: {refused}"
    );
    assert_eq!(h.runs(), 0);

    // And an issuer with no row at all holds nothing, rather than inheriting
    // the standing grants that exist.
    assert!(
        BusGrants::grant(h.approvals.as_ref(), &AgentSessionId::new("stranger")).is_none(),
        "an unknown agent holds no grant"
    );
}

/// If this fails, a grant outlives its own expiry and an agent keeps acting
/// after the operator expected it to stop.
#[tokio::test]
async fn an_expired_grant_authorizes_nothing() {
    let h = Harness::new();
    h.grant(AGENT, &["probe.*"], Some(600));

    // Inside the lifetime, an ordinary read runs.
    let admitted = h.dispatch("probe.read", serde_json::json!({}), Issuer::agent(AGENT));
    assert!(expect_running(admitted).finish().await.succeeded());

    h.clock.advance(601);

    let refused = h
        .try_dispatch("probe.read", serde_json::json!({}), Issuer::agent(AGENT))
        .expect_err("an expired grant is not a narrower grant");
    assert!(
        refused.to_string().contains("has no grant"),
        "an expired grant is gone, not narrowed: {refused}"
    );

    // And nothing destructive sneaks into the Inbox on the way out: an expired
    // session must not be able to keep asking.
    let refused = h
        .try_dispatch("probe.wipe", serde_json::json!({}), Issuer::agent(AGENT))
        .expect_err("no queueing under an expired grant");
    assert!(refused.to_string().contains("has no grant"), "{refused}");
    assert!(
        h.approvals.pending().expect("the Inbox reads").is_empty(),
        "an expired grant cannot put anything in front of the operator"
    );
}

// ---------------------------------------------------------------------------
// Queue, approve, run
// ---------------------------------------------------------------------------

/// The refusal this whole module exists to undo.
///
/// If it fails, a granted agent's destructive command is refused instead of
/// queued, which is the state the server shipped in: the Inbox is reachable
/// only through code nothing constructs, so the operator is never asked and
/// the agent can never act.
#[tokio::test]
async fn a_destructive_agent_dispatch_is_queued_rather_than_refused() {
    let h = Harness::new();
    h.grant(AGENT, &["probe.*"], Some(3600));

    let pending = h.queue_a_wipe();
    assert_eq!(h.runs(), 0, "queued means nothing has run");

    let waiting = h.approvals.pending().expect("the Inbox reads");
    assert_eq!(
        waiting.len(),
        1,
        "the operator has exactly one thing to answer"
    );
    let item = &waiting[0];
    assert_eq!(item.id.to_string(), pending.to_string());
    assert_eq!(item.command, "probe.wipe");
    assert_eq!(item.target_node, NODE);
    assert_eq!(item.risk, Risk::Destructive);
    assert_eq!(item.reason, QueueReason::Destructive);
    assert_eq!(
        item.scale,
        Scale::Unknown,
        "the bus cannot say how many things a dispatch touches, and claiming \
         one item would disarm the bulk-approval rule"
    );
    assert_eq!(
        item.issuer,
        GrantIssuer::Session {
            label: AGENT.to_string(),
            session: None
        },
        "the item records which agent asked, so the operator knows who they \
         are answering"
    );
    assert!(
        item.grant.is_some(),
        "the item names the grant it was issued under, which is what makes a \
         later revocation auditable"
    );
}

/// The keystone. `Bus::dispatch_approved` re-runs the agent check, so an Inbox
/// attached without grant storage would take the operator's approval and then
/// refuse the command.
///
/// If this fails, approving in the Inbox does nothing except mark the item
/// approved: the operator taps yes and the work never happens.
#[tokio::test]
async fn approving_a_queued_item_actually_runs_it() {
    let h = Harness::new();
    h.grant(AGENT, &["probe.*"], Some(3600));
    let pending = h.queue_a_wipe();

    let result = h.approve(&pending).await.expect("the approval releases it");
    assert_eq!(result["command"], "probe.wipe");
    assert_eq!(result["result"], serde_json::json!(true));
    assert_eq!(h.runs(), 1, "the approved command ran exactly once");

    assert!(
        h.approvals.pending().expect("the Inbox reads").is_empty(),
        "an approved item leaves the queue"
    );
    let again = h.approve(&pending).await.expect_err("approved once");
    assert!(
        again.contains("already approved"),
        "a second approval is refused rather than dispatching twice: {again}"
    );
    assert_eq!(h.runs(), 1);
}

/// An approval that cannot state the count is refused. The guardrail from the
/// module docs: a client that never renders the number physically cannot
/// approve a sweep.
///
/// If this fails, an operator can approve an unbounded destructive action
/// without ever being shown what it touches.
#[tokio::test]
async fn an_approval_that_does_not_face_the_unknown_count_is_refused() {
    let h = Harness::new();
    h.grant(AGENT, &["probe.*"], Some(3600));
    let pending = h.queue_a_wipe();

    let admitted = h
        .try_dispatch(
            "system.inbox.approve",
            // No count, and no explicit acceptance of an unbounded action.
            serde_json::json!({ "item": pending.to_string() }),
            Issuer::operator(),
        )
        .expect("the approve command itself is not refused");
    let failure = expect_running(admitted)
        .finish()
        .await
        .outcome
        .expect_err("the Inbox refuses a bare yes");
    assert!(
        failure.to_string().contains("must state the count"),
        "{failure}"
    );
    assert_eq!(h.runs(), 0);

    // The item is still waiting: a refused approval is not a decision.
    assert_eq!(h.approvals.pending().expect("reads").len(), 1);
}

/// If this fails, revoking a grant stops new dispatches while everything
/// already queued under it stays runnable — so the operator revokes, sees the
/// agent stop, and then releases its destructive command by hand.
#[tokio::test]
async fn a_revoked_grant_does_not_authorize_an_item_approved_afterwards() {
    let h = Harness::new();
    let grant = h.grant(AGENT, &["probe.*"], Some(3600));
    let pending = h.queue_a_wipe();

    // Through the bus command, so the operator's real path is what revokes.
    let admitted = h.dispatch(
        "system.grant.revoke",
        serde_json::json!({ "grant": grant.to_string() }),
        Issuer::operator(),
    );
    let revoked = expect_running(admitted)
        .finish()
        .await
        .outcome
        .expect("the revocation runs");
    assert_eq!(revoked["revoked"], serde_json::json!(true));

    let refused = h.approve(&pending).await.expect_err("nothing to run it as");
    assert!(
        refused.contains("has no grant"),
        "the release is refused by the grant check, not by the Inbox: {refused}"
    );
    assert_eq!(h.runs(), 0, "a revoked agent's queued command never ran");
}

/// The containment that does not depend on nobody ever writing `system.*` into
/// a grant.
///
/// If this fails, an agent queues its own destructive command and then approves
/// it, and the Inbox is decoration.
#[tokio::test]
async fn an_agent_cannot_approve_its_own_queued_item() {
    let h = Harness::new();
    // Deliberately wide: the agent is granted the very command it would use.
    h.grant(AGENT, &["probe.*", "system.*"], Some(3600));
    let pending = h.queue_a_wipe();

    let admitted = h.dispatch(
        "system.inbox.approve",
        serde_json::json!({ "item": pending.to_string(), "accept_unknown": true }),
        Issuer::agent(AGENT),
    );
    let failure = expect_running(admitted)
        .finish()
        .await
        .outcome
        .expect_err("an agent may not approve");
    assert!(
        failure.to_string().contains("operator's decision"),
        "{failure}"
    );
    assert_eq!(h.runs(), 0);
    assert_eq!(
        h.approvals.pending().expect("reads").len(),
        1,
        "the item is untouched, so the operator still gets to answer it"
    );
}

/// The adapter the bus is described through, asserted on the real spec.
///
/// If this fails — if `scale()` ever answers `Single` — the bulk rule disarms
/// itself: every dispatch would claim to touch one item, so nothing would
/// cross `bulk_threshold` and a sweep would run under an ordinary write.
#[test]
fn a_dispatch_describes_its_scale_as_unknown_rather_than_claiming_one_item() {
    let h = Harness::new();
    let spec = h
        .registry
        .spec(&CommandId::new("probe.wipe").expect("id"))
        .expect("probe.wipe is registered");
    let facts = Facts::of_spec(&spec, &NodeId::new(NODE));

    assert_eq!(facts.id(), "probe.wipe");
    assert_eq!(
        facts.risk(),
        Risk::Destructive,
        "the risk comes from the backend's own const, not from this translation"
    );
    assert_eq!(facts.target_node(), NODE);
    assert_eq!(facts.scale(), Scale::Unknown);
}

// ---------------------------------------------------------------------------
// Lockdown step 3
// ---------------------------------------------------------------------------

/// Freezing has to reach the queue, not just the door.
///
/// The known defect this defends: pausing agents stopped new dispatches while
/// items already waiting stayed releasable, so an agent's destructive command
/// queued before lockdown ran after it. If this fails, the kill switch leaves
/// exactly that hole.
#[tokio::test]
async fn lockdown_holds_an_item_that_was_already_pending() {
    let h = Harness::new();
    h.grant(AGENT, &["probe.*"], Some(3600));
    let pending = h.queue_a_wipe();

    let lockdown = h.lockdown();
    let report = lockdown.engage(Trigger::HostCommand);
    assert_eq!(
        report.frozen.map(|f| f.inbox_pending),
        Some(1),
        "step 3 reports what it held, and it is no longer entitled to report \
         zero now that there is an Inbox"
    );
    assert!(h.approvals.is_frozen());

    let refused = h.approve(&pending).await.expect_err("frozen");
    assert!(refused.contains("frozen"), "{refused}");
    assert_eq!(h.runs(), 0);

    // Held, not discarded: after an incident the queue is evidence.
    assert_eq!(
        h.approvals.pending().expect("reads").len(),
        1,
        "freezing holds items in place rather than throwing them away"
    );

    // Denying still works, because refusing a queued action is never the
    // dangerous direction, and a frozen Inbox the operator cannot empty is
    // one they will clear the lockdown to get at.
    let denied = h
        .deny(&pending)
        .await
        .expect("denial is allowed while frozen");
    assert_eq!(denied["status"], serde_json::json!("denied"));
    assert_eq!(h.runs(), 0);
}

/// The explicit thaw must not be a way around the kill switch.
///
/// If this fails, anything in the process that can reach this subsystem can
/// unfreeze approvals during a lockdown, and the freeze is advisory.
#[tokio::test]
async fn thawing_is_refused_while_the_lockdown_is_still_engaged() {
    let h = Harness::new();
    h.grant(AGENT, &["probe.*"], Some(3600));
    let pending = h.queue_a_wipe();

    let lockdown = h.lockdown();
    lockdown.engage(Trigger::HostCommand);

    assert!(
        !h.approvals.thaw(),
        "a locked node does not thaw on request"
    );
    assert!(h.approvals.is_frozen());
    assert!(h.approve(&pending).await.is_err(), "still frozen");
    assert_eq!(h.runs(), 0);
}

/// If this fails, the operator ends a lockdown and the Inbox is still frozen:
/// the kill switch works once and the approval queue never comes back without
/// a restart.
#[tokio::test]
async fn clearing_the_lockdown_makes_a_held_item_approvable_again() {
    let h = Harness::new();
    h.grant(AGENT, &["probe.*"], Some(3600));
    let pending = h.queue_a_wipe();

    let lockdown = h.lockdown();
    // Minted before the lockdown on purpose: issuing a code is an unlock path
    // and is refused from the locked state.
    let codes = lockdown
        .issue_recovery_codes(2)
        .expect("a fresh node can issue codes");

    lockdown.engage(Trigger::HostCommand);
    assert!(h.approve(&pending).await.is_err(), "frozen while locked");

    lockdown
        .unlock(UnlockRequest {
            // The usable path while the passkey verifier fails closed: the
            // host console plus an offline code.
            identity: Identity::HostConsole,
            second: SecondFactor::RecoveryCode(codes[0].reveal()),
        })
        .expect("the unlock is accepted");
    assert!(!lockdown.is_locked());

    h.approve(&pending)
        .await
        .expect("the item the lockdown held is approvable again");
    assert_eq!(h.runs(), 1);
}

// ---------------------------------------------------------------------------
// Registration
// ---------------------------------------------------------------------------

/// The wiring itself, asserted. Deleting `register_approvals` from startup
/// leaves the Inbox invisible: no page can list it, and no approval can be
/// tapped.
#[test]
fn the_grant_and_inbox_commands_are_on_the_bus() {
    let h = Harness::new();
    let ids: Vec<String> = h
        .registry
        .list(None)
        .into_iter()
        .map(|l| l.spec.id().to_string())
        .collect();
    for expected in [
        "system.inbox",
        "system.inbox.approve",
        "system.inbox.deny",
        "system.grants",
        "system.grant.issue",
        "system.grant.revoke",
    ] {
        assert!(
            ids.contains(&expected.to_string()),
            "{expected} is not registered; {ids:?}"
        );
    }
}

/// The owner column reaches the field `active_standing` filters on.
///
/// `grants::Grants::active_standing` compares `Grant::owner`. A store that
/// writes the column but reads it back only into `OwnedGrant` would leave
/// every standing grant owner-blind on the SQL path while passing against
/// `MemoryGrants`: green unit tests, no authority at runtime. This is the
/// same string on both paths, so the bus's lookup and the domain query agree
/// on what a holder is.
#[test]
fn the_owner_column_round_trips_into_the_grant_itself() {
    let dir = tempfile::tempdir().expect("a working directory");
    let store = Arc::new(Mutex::new(Store::open(dir.path()).expect("the database")));
    let rows = GrantRows::new(Arc::clone(&store));

    let grant = Grant {
        id: GrantId::from_text("standing-1"),
        label: "Assistant".to_string(),
        session: None,
        owner: Some(AGENT.to_string()),
        project: None,
        namespaces: vec![Pattern::parse("git.status").expect("a pattern")],
        nodes: NodeScope::All,
        issued_at: 1_700_000_000,
        expires_at: None,
        revoked_at: None,
    };
    rows.put_grant(&grant).expect("the grant is written");

    let read = rows
        .grant(&grant.id)
        .expect("readable")
        .expect("still there");
    assert_eq!(
        read.owner.as_deref(),
        Some(AGENT),
        "put_grant must write the owner column and grant() must read it back \
         into the field active_standing filters on"
    );

    let held = rows.held_by(AGENT).expect("readable");
    assert_eq!(
        held.len(),
        1,
        "the same string finds the standing grant through held_by, so the bus \
         path and the domain path agree on what a holder is"
    );
}

/// Two grants are two grants, never their union.
///
/// The scenario is the one that was actually broken: the operator issues
/// `probe.wipe` scoped to a DIFFERENT node, and separately `probe.read` on
/// this one. Each grant is innocuous. Unioning them field-wise — namespaces
/// together, node lists together — produces `probe.wipe` on this node,
/// which neither grant authorized and which the operator was never shown.
///
/// `grants::decide_any` always evaluated each grant whole; the bus facade
/// was the thing that merged them. This is the test that keeps the two
/// agreeing, and it is a privilege-escalation regression test, not a
/// refactoring one.
#[test]
fn a_grant_scoped_to_another_node_does_not_combine_with_one_scoped_to_this_one() {
    let h = Harness::new();

    // Destructive on another machine only.
    h.approvals
        .issue(
            AGENT,
            GrantSpec {
                label: "wipe, but only on the R620".to_string(),
                session: None,
                project: None,
                namespaces: vec!["probe.wipe".to_string()],
                nodes: Some(NodeScope::Only(vec!["r620".to_string()])),
                ttl_secs: None,
            },
        )
        .expect("the remote grant issues");

    // Harmless, here.
    h.approvals
        .issue(
            AGENT,
            GrantSpec {
                label: "read, on this node".to_string(),
                session: None,
                project: None,
                namespaces: vec!["probe.read".to_string()],
                nodes: Some(NodeScope::Only(vec![NODE.to_string()])),
                ttl_secs: None,
            },
        )
        .expect("the local grant issues");

    // The grant each one really is, kept apart.
    let grants = deckjack_core::command::Grants::grants_for(
        h.approvals.grants().as_ref(),
        &AgentSessionId::new(AGENT.to_string()),
    );
    assert_eq!(grants.len(), 2, "two grants must not arrive as one");

    // The premise, asserted rather than assumed: the union of these two
    // grants really does authorize what neither does. Built here the way the
    // old `translate_grant` built it — namespaces concatenated, node lists
    // concatenated — so this test carries the proof that it is defending
    // against something, and keeps carrying it after everyone has forgotten
    // what the bug was.
    let spec = h
        .registry
        .spec(&CommandId::new("probe.wipe").expect("id"))
        .expect("probe.wipe is registered");
    let this_node = NodeId::new(NODE);
    let union = deckjack_core::command::Grant {
        namespaces: grants.iter().flat_map(|g| g.namespaces.clone()).collect(),
        nodes: deckjack_core::command::NodeScope::Nodes(
            grants
                .iter()
                .flat_map(|g| match &g.nodes {
                    deckjack_core::command::NodeScope::Nodes(ids) => ids.clone(),
                    _ => Vec::new(),
                })
                .collect(),
        ),
        ceiling: RiskLevel::Destructive,
    };
    assert!(
        union.allows(&spec) && union.allows_node(&this_node, &this_node),
        "the union does not escalate, so this test proves nothing: fix the test"
    );
    assert!(
        !grants
            .iter()
            .any(|g| g.allows(&spec) && g.allows_node(&this_node, &this_node)),
        "no single grant may authorize probe.wipe on this node"
    );

    // What the operator did grant: reading, here.
    match h.try_dispatch("probe.read", serde_json::json!({}), Issuer::agent(AGENT)) {
        Ok(_) => {}
        Err(e) => panic!("the local read grant should authorize probe.read: {e}"),
    }

    // What the union would have granted and nobody did.
    let refused = h
        .try_dispatch("probe.wipe", serde_json::json!({}), Issuer::agent(AGENT))
        .expect_err("probe.wipe is granted only on r620 and must not run here");
    let message = refused.to_string();
    assert!(
        message.contains("node") || message.contains("r620") || message.contains(NODE),
        "the refusal should name the node that stopped it, not the namespace: {message}"
    );
    assert_eq!(
        h.ran.load(Ordering::SeqCst),
        0,
        "probe.wipe must not have run"
    );
}
