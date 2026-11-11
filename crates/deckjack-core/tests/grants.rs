//! Scoped grants and the approval Inbox, driven through the public API only.
//!
//! The rules under test are the ones the design is explicit about:
//!
//! - outside the grant is **refused**, never queued, whatever the risk;
//! - default-deny on namespaces, on nodes and on expiry;
//! - bulk and destructive items show a count before approval, and an
//!   approval that does not echo that count is refused;
//! - an approval happens once.
//!
//! The clock is injected, so every expiry rule here is exercised without
//! sleeping. Nothing below reaches into the module's internals: if a test
//! compiles, the surface it tests is the surface a caller has.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use deckjack_core::auth::code::RandomError;
use deckjack_core::grants::{
    CommandFacts, Confirmation, Decision, Env, Grant, GrantId, GrantSpec, Grants, Inbox,
    InboxError, IssueError, Issuer, ItemStatus, MemoryGrants, MemoryInbox, NodeScope, Pattern,
    PatternError, Policy, QueueReason, QueueRequest, Risk, Scale, decide, decide_any,
};

// ---------------------------------------------------------------------------
// Doubles
// ---------------------------------------------------------------------------

/// An injected clock and id source. Cloneable so a handle can advance time
/// after the other handle has been moved into `Grants` or `Inbox`.
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
                // should not be masked by the clock starting at epoch.
                now: AtomicU64::new(1_000_000),
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

/// A command as the bus would describe it to this module.
struct Cmd {
    id: String,
    risk: Risk,
    node: String,
    scale: Scale,
}

impl Cmd {
    fn new(id: &str, risk: Risk) -> Cmd {
        Cmd {
            id: id.to_string(),
            risk,
            node: "r620".to_string(),
            scale: Scale::Single,
        }
    }

    fn on(mut self, node: &str) -> Cmd {
        self.node = node.to_string();
        self
    }

    fn touching(mut self, scale: Scale) -> Cmd {
        self.scale = scale;
        self
    }
}

impl CommandFacts for Cmd {
    fn id(&self) -> &str {
        &self.id
    }
    fn risk(&self) -> Risk {
        self.risk
    }
    fn target_node(&self) -> &str {
        &self.node
    }
    fn scale(&self) -> Scale {
        self.scale
    }
}

fn session_grants(policy: Policy) -> (Grants<Arc<MemoryGrants>, Clock>, Arc<MemoryGrants>, Clock) {
    let store = Arc::new(MemoryGrants::new());
    let clock = Clock::new();
    let grants = Grants::new(store.clone(), clock.clone(), policy);
    (grants, store, clock)
}

fn inbox(policy: Policy) -> (Inbox<Arc<MemoryInbox>, Clock>, Clock) {
    let clock = Clock::new();
    let inbox = Inbox::new(Arc::new(MemoryInbox::new()), clock.clone(), policy);
    (inbox, clock)
}

fn spec(label: &str, namespaces: &[&str]) -> GrantSpec {
    GrantSpec {
        label: label.to_string(),
        namespaces: namespaces.iter().map(|s| s.to_string()).collect(),
        nodes: Some(NodeScope::Only(vec!["r620".to_string()])),
        ttl_secs: Some(3_600),
        ..GrantSpec::default()
    }
}

fn agent(session: &str) -> Issuer {
    Issuer::Session {
        label: "Assistant".to_string(),
        session: Some(session.to_string()),
    }
}

/// A grant built by hand, for the cases issuance refuses to create but a
/// database row or an older schema could still hold.
fn raw_grant(namespaces: Vec<Pattern>, nodes: NodeScope) -> Grant {
    Grant {
        id: GrantId::from_text("g1"),
        label: "hand-built".to_string(),
        session: Some("s1".to_string()),
        owner: None,
        project: None,
        namespaces,
        nodes,
        issued_at: 1_000_000,
        expires_at: None,
        revoked_at: None,
    }
}

fn queued(cmd: &Cmd, reason: QueueReason, scale: Scale, session: &str) -> QueueRequest {
    QueueRequest::from_command(
        None,
        agent(session),
        cmd,
        reason,
        scale,
        "a thing the operator must read",
        serde_json::json!({ "path": "~/Footage" }),
    )
}

const EVERY_RISK: [Risk; 5] = [
    Risk::ReadOnly,
    Risk::Write,
    Risk::Destructive,
    Risk::Outbound,
    Risk::CrossProject,
];

// ---------------------------------------------------------------------------
// Namespace matching
// ---------------------------------------------------------------------------

#[test]
fn a_namespace_grant_does_not_leak_into_a_neighboring_namespace() {
    let p = Pattern::parse("git.*").expect("git.* is a pattern");

    assert!(p.matches("git.status"));
    assert!(p.matches("git.commit.amend"), "deeper ids are inside git.*");

    // The whole point: a prefix match on the *string* would let every one of
    // these through.
    for outside in ["github.x", "gitx.y", "mygit.status", "git", "", "gitstatus"] {
        assert!(
            !p.matches(outside),
            "git.* must not match {outside:?}; segments are matched, not characters"
        );
    }
}

#[test]
fn a_wildcard_must_be_a_whole_segment_or_the_pattern_is_refused() {
    // A half-working pattern is the dangerous outcome: "git.pu*" reads like
    // it grants git.push, and a string-prefix implementation would agree.
    assert!(matches!(
        Pattern::parse("git.pu*"),
        Err(PatternError::PartialWildcard { .. })
    ));
    assert!(matches!(
        Pattern::parse("*s.read"),
        Err(PatternError::PartialWildcard { .. })
    ));

    // And a middle wildcard stands for exactly one segment, so it cannot be
    // used to reach deeper than it reads.
    let p = Pattern::parse("files.*.read").expect("a middle wildcard is a pattern");
    assert!(p.matches("files.local.read"));
    assert!(!p.matches("files.local.deep.read"));
    assert!(!p.matches("files.read"));
}

#[test]
fn a_malformed_pattern_is_refused_rather_than_narrowed() {
    assert!(matches!(Pattern::parse(""), Err(PatternError::Empty)));
    assert!(matches!(
        Pattern::parse("git..*"),
        Err(PatternError::EmptySegment { .. })
    ));
    assert!(matches!(
        Pattern::parse("git."),
        Err(PatternError::EmptySegment { .. })
    ));
    // Case-folding a permission check is how the UI and the bus come to
    // disagree about what was granted.
    assert!(matches!(
        Pattern::parse("Git.*"),
        Err(PatternError::BadCharacter { .. })
    ));
    assert!(matches!(
        Pattern::parse("git.sta tus"),
        Err(PatternError::BadCharacter { .. })
    ));
}

#[test]
fn a_grant_of_the_whole_bus_is_refused_and_not_stored() {
    let (grants, store, _clock) = session_grants(Policy::default());

    let err = grants
        .issue(None, spec("too much", &["*"]))
        .expect_err("a bare * leaves nothing outside the grant");
    assert!(matches!(err, IssueError::Unrestricted { .. }), "{err}");

    use deckjack_core::grants::GrantStore as _;
    assert!(
        store.grants().expect("store readable").is_empty(),
        "a refused grant must not be in the store; a stored grant is already being routed against"
    );
}

#[test]
fn an_all_wildcard_pattern_is_the_whole_bus_however_it_is_spelled() {
    // Every command id on the bus is dotted, so "*.*" grants all of them.
    // Recognizing only a literal "*" let an unrestricted grant through both
    // the issuance check and the UI flag.
    let star_star = Pattern::parse("*.*").expect("*.* parses");
    assert!(star_star.matches("terminal.write"));
    assert!(star_star.matches("git.status"));
    assert!(
        star_star.is_everything(),
        "*.* grants every dotted command id, so it is not a restricted grant"
    );

    let (grants, _store, _clock) = session_grants(Policy::default());
    assert!(matches!(
        grants.issue(None, spec("sneaky", &["*.*"])),
        Err(IssueError::Unrestricted { .. })
    ));

    // The operator's own tooling can still ask for it, but only by saying so.
    let policy = Policy {
        allow_unrestricted_grants: true,
        ..Policy::default()
    };
    let (grants, _store, _clock) = session_grants(policy);
    let grant = grants
        .issue(None, spec("operator tooling", &["*.*"]))
        .expect("an explicit opt-in may hold the bus");
    assert!(
        grant.is_unrestricted(),
        "the UI flag must still show this grant for what it is"
    );
}

// ---------------------------------------------------------------------------
// Default-deny
// ---------------------------------------------------------------------------

#[test]
fn an_empty_grant_permits_nothing() {
    let (grants, _store, _clock) = session_grants(Policy::default());
    assert!(matches!(
        grants.issue(None, spec("blank", &[])),
        Err(IssueError::NoNamespaces { .. })
    ));

    // And if one exists anyway — an older row, a hand-edited database — it
    // must refuse rather than fall open.
    let grant = raw_grant(vec![], NodeScope::All);
    assert!(!grant.allows_command("git.status"));
    let cmd = Cmd::new("git.status", Risk::ReadOnly);
    assert!(matches!(
        decide(&grant, &cmd, &Policy::default(), 1_000_000),
        Decision::Refuse(_)
    ));
}

#[test]
fn a_command_outside_the_grant_is_refused_at_every_risk_level_and_never_queued() {
    // The single most important behavior in the module: an ungranted command
    // must not become a question for the operator. If it could, the Inbox
    // would be a channel for asking for anything, and an agent with no grant
    // would still get a prompt in front of a tired human.
    let grant = raw_grant(
        vec![Pattern::parse("git.*").unwrap()],
        NodeScope::Only(vec!["r620".to_string()]),
    );

    for risk in EVERY_RISK {
        for scale in [Scale::Single, Scale::Count(10_000), Scale::Unknown] {
            let cmd = Cmd::new("mail.send", risk).touching(scale);
            match decide(&grant, &cmd, &Policy::default(), 1_000_000) {
                Decision::Refuse(_) => {}
                other => panic!("{risk} at {scale} outside the grant became {other:?}"),
            }
        }
    }
}

#[test]
fn a_grant_refuses_a_command_aimed_at_another_node() {
    let grant = raw_grant(
        vec![Pattern::parse("git.*").unwrap()],
        NodeScope::Only(vec!["r620".to_string()]),
    );

    let here = Cmd::new("git.status", Risk::ReadOnly);
    assert!(decide(&grant, &here, &Policy::default(), 1_000_000).is_run());

    let elsewhere = Cmd::new("git.status", Risk::ReadOnly).on("fold");
    assert!(
        matches!(
            decide(&grant, &elsewhere, &Policy::default(), 1_000_000),
            Decision::Refuse(_)
        ),
        "node scope is part of the grant, not a hint"
    );

    // "No nodes granted" reads as nothing, not everything.
    let none = raw_grant(
        vec![Pattern::parse("git.*").unwrap()],
        NodeScope::Only(vec![]),
    );
    assert!(!none.allows_node("r620"));
}

#[test]
fn a_grant_with_no_node_scope_is_refused_rather_than_defaulted_to_every_node() {
    let (grants, _store, _clock) = session_grants(Policy::default());

    assert!(matches!(
        grants.issue(
            None,
            GrantSpec {
                nodes: None,
                ..spec("nodeless", &["git.*"])
            }
        ),
        Err(IssueError::NoNodes { .. })
    ));
    assert!(matches!(
        grants.issue(
            None,
            GrantSpec {
                nodes: Some(NodeScope::Only(vec![])),
                ..spec("nodeless", &["git.*"])
            }
        ),
        Err(IssueError::NoNodes { .. })
    ));
}

#[test]
fn an_unreadable_stored_pattern_fails_closed() {
    // A line that no longer parses is dropped. The invariant that matters is
    // the direction of the failure: the grant gets narrower, never wider.
    let patterns = Grant::parse_namespaces("git.*\nfiles.pu*\n\n  mail.send  ");
    let grant = raw_grant(patterns, NodeScope::All);

    assert!(grant.allows_command("git.status"));
    assert!(grant.allows_command("mail.send"));
    assert!(
        !grant.allows_command("files.push"),
        "a pattern that could not be read must not grant anything"
    );
}

// ---------------------------------------------------------------------------
// Expiry, revocation, scoping between sessions
// ---------------------------------------------------------------------------

#[test]
fn an_expired_grant_permits_nothing_not_even_a_read() {
    let (grants, _store, clock) = session_grants(Policy::default());
    let grant = grants
        .issue(
            None,
            GrantSpec {
                session: Some("s1".to_string()),
                ttl_secs: Some(100),
                ..spec("short-lived", &["git.*"])
            },
        )
        .expect("issued");

    let read = Cmd::new("git.status", Risk::ReadOnly);
    assert!(decide(&grant, &read, &Policy::default(), clock.now()).is_run());

    clock.advance(100); // exactly at the expiry: already gone, not still good.
    assert!(!grant.is_active(clock.now()));
    assert!(matches!(
        decide(&grant, &read, &Policy::default(), clock.now()),
        Decision::Refuse(_)
    ));
    assert!(
        grants
            .active_for_session("s1")
            .expect("readable")
            .is_empty(),
        "routing must not be handed an expired grant at all"
    );
}

#[test]
fn an_expired_or_revoked_grant_refuses_a_destructive_command_instead_of_queueing_it() {
    // Queueing it would let a session that has lost its grant keep putting
    // questions in front of the operator.
    let (grants, _store, clock) = session_grants(Policy::default());
    let grant = grants
        .issue(
            None,
            GrantSpec {
                session: Some("s1".to_string()),
                ttl_secs: Some(100),
                ..spec("short-lived", &["files.*"])
            },
        )
        .expect("issued");
    let destructive = Cmd::new("files.delete", Risk::Destructive).touching(Scale::Count(412));

    clock.advance(500);
    assert!(matches!(
        decide(&grant, &destructive, &Policy::default(), clock.now()),
        Decision::Refuse(_)
    ));

    let (grants, _store, clock) = session_grants(Policy::default());
    let grant = grants
        .issue(
            None,
            GrantSpec {
                session: Some("s1".to_string()),
                ..spec("revoked", &["files.*"])
            },
        )
        .expect("issued");
    assert!(grants.revoke(&grant.id).expect("revocable"));
    let grant = grants
        .get(&grant.id)
        .expect("readable")
        .expect("still on record");
    match decide(&grant, &destructive, &Policy::default(), clock.now()) {
        Decision::Refuse(_) => {}
        other => panic!("a revoked grant produced {other:?}"),
    }
}

#[test]
fn the_kill_switch_refuses_every_command_under_every_grant() {
    let (grants, _store, _clock) = session_grants(Policy::default());
    grants
        .issue(
            None,
            GrantSpec {
                session: Some("s1".to_string()),
                ..spec("one", &["git.*"])
            },
        )
        .expect("issued");
    grants
        .issue(
            None,
            GrantSpec {
                session: Some("s2".to_string()),
                ..spec("two", &["files.*"])
            },
        )
        .expect("issued");

    assert_eq!(grants.revoke_all().expect("revocable"), 2);

    for (session, id) in [("s1", "git.status"), ("s2", "files.read")] {
        let cmd = Cmd::new(id, Risk::ReadOnly);
        match grants.decide_for(&agent(session), &cmd).expect("decidable") {
            Decision::Refuse(_) => {}
            other => panic!("{id} still {other:?} after the kill switch"),
        }
    }
}

#[test]
fn one_sessions_grant_never_authorizes_another_session() {
    let (grants, _store, _clock) = session_grants(Policy::default());
    grants
        .issue(
            None,
            GrantSpec {
                session: Some("s1".to_string()),
                ..spec("session one", &["git.*"])
            },
        )
        .expect("issued");

    let cmd = Cmd::new("git.status", Risk::ReadOnly);
    assert!(
        grants
            .decide_for(&agent("s1"), &cmd)
            .expect("decidable")
            .is_run()
    );
    match grants.decide_for(&agent("s2"), &cmd).expect("decidable") {
        Decision::Refuse(_) => {}
        other => panic!("a grant for another session produced {other:?}"),
    }
}

#[test]
fn holding_two_grants_grants_their_union() {
    let read = raw_grant(
        vec![Pattern::parse("files.read.*").unwrap()],
        NodeScope::All,
    );
    let git = raw_grant(vec![Pattern::parse("git.*").unwrap()], NodeScope::All);
    let cmd = Cmd::new("git.status", Risk::ReadOnly);

    // Order must not matter: a refusal from the first grant consulted cannot
    // shadow a permission from the second.
    for pair in [
        vec![read.clone(), git.clone()],
        vec![git.clone(), read.clone()],
    ] {
        assert!(
            decide_any(&pair, &agent("s1"), &cmd, &Policy::default(), 1_000_000).is_run(),
            "two grants mean the union of what they allow"
        );
    }
}

#[test]
fn a_session_with_no_grant_at_all_is_refused_by_name() {
    let cmd = Cmd::new("git.status", Risk::ReadOnly);
    match decide_any(&[], &agent("s1"), &cmd, &Policy::default(), 1_000_000) {
        Decision::Refuse(reason) => {
            let text = reason.to_string();
            assert!(
                text.contains("Assistant") && text.contains("git.status"),
                "the log line must name the issuer and the command: {text}"
            );
        }
        other => panic!("no grants produced {other:?}"),
    }
}

#[test]
fn a_grant_cannot_outlive_the_policy_maximum_or_stand_forever_against_policy() {
    let (grants, _store, _clock) = session_grants(Policy::default());
    assert!(matches!(
        grants.issue(
            None,
            GrantSpec {
                ttl_secs: Some(Policy::default().max_grant_ttl_secs + 1),
                ..spec("too long", &["git.*"])
            }
        ),
        Err(IssueError::TtlTooLong { .. })
    ));

    let policy = Policy {
        allow_standing_grants: false,
        ..Policy::default()
    };
    let (grants, _store, _clock) = session_grants(policy);
    assert!(matches!(
        grants.issue(
            None,
            GrantSpec {
                ttl_secs: None,
                ..spec("forever", &["git.*"])
            }
        ),
        Err(IssueError::StandingNotAllowed { .. })
    ));
}

#[test]
fn a_standing_grant_for_one_agent_does_not_authorize_another_sessionless_issuer() {
    let policy = Policy {
        allow_unrestricted_grants: true,
        ..Policy::default()
    };
    let (grants, _store, _clock) = session_grants(policy);

    // The operator's own tooling holds the bus, as the module's comments
    // anticipate. The always-on assistant holds one narrow namespace.
    grants
        .issue(
            Some("operator tooling"),
            GrantSpec {
                session: None,
                ttl_secs: None,
                ..spec("operator tooling", &["*"])
            },
        )
        .expect("issued");
    grants
        .issue(
            Some("Assistant"),
            GrantSpec {
                session: None,
                ttl_secs: None,
                ..spec("Assistant", &["git.status"])
            },
        )
        .expect("issued");

    let assistant = Issuer::Session {
        label: "Assistant".to_string(),
        session: None,
    };
    let cmd = Cmd::new("mail.send", Risk::Outbound);
    match grants.decide_for(&assistant, &cmd).expect("decidable") {
        Decision::Refuse(_) => {}
        other => panic!("the Assistant inherited another standing grant: {other:?}"),
    }
}

/// A standing grant held by nobody authorizes nobody.
///
/// A row written before migration 6 has `session` NULL and `owner` NULL. It
/// is the case that looks permissive, because "held by nobody in particular"
/// reads like "held by anyone", and reading it that way re-creates the defect
/// the owner column was added for. Written straight through `GrantStore`
/// because `Grants::issue` refuses `*` under the default policy — which is
/// exactly how such a row gets into a real database: an older schema, not an
/// issuance.
#[test]
fn a_standing_grant_with_no_owner_authorizes_nobody() {
    use deckjack_core::grants::GrantStore as _;

    let orphan = Grant {
        id: GrantId::from_text("pre-migration-6"),
        label: "an old standing grant".to_string(),
        session: None,
        owner: None,
        project: None,
        namespaces: vec![Pattern::parse("*").expect("a pattern")],
        nodes: NodeScope::All,
        issued_at: 1_000_000,
        expires_at: None,
        revoked_at: None,
    };

    let store = Arc::new(MemoryGrants::new());
    store.put_grant(&orphan).expect("written");
    let grants = Grants::new(Arc::clone(&store), Clock::new(), Policy::default());

    let cmd = Cmd::new("mail.send", Risk::Outbound);
    for issuer in [
        Issuer::Session {
            label: "Assistant".to_string(),
            session: None,
        },
        Issuer::Operator,
        Issuer::Cli,
    ] {
        match grants.decide_for(&issuer, &cmd).expect("decidable") {
            Decision::Refuse(_) => {}
            other => panic!(
                "{} inherited a standing grant nobody holds: {other:?}",
                issuer.label()
            ),
        }
    }
}

/// Neither the operator nor the CLI holds a standing grant, even one labelled
/// with their own name.
///
/// `Issuer::owner` answers `None` for both, and `decide_for` gives them an
/// empty slice rather than looking a grant up by their display name. The
/// grants below are owned by the exact strings `Issuer::label` returns, so a
/// version of `owner()` that answered `Some(self.label())` would find them —
/// which is the regression this pins, and which a NULL-owner fixture cannot
/// see at all. Neither issuer routes through this module: the operator's own
/// commands are gated by `operator_only`, not by a grant, so a grant found
/// here is authority nobody issued.
#[test]
fn neither_the_operator_nor_the_cli_holds_a_standing_grant() {
    use deckjack_core::grants::GrantStore as _;

    let store = Arc::new(MemoryGrants::new());
    for (id, owner) in [("g-op", "the operator"), ("g-cli", "the CLI")] {
        store
            .put_grant(&Grant {
                id: GrantId::from_text(id),
                label: owner.to_string(),
                session: None,
                owner: Some(owner.to_string()),
                project: None,
                namespaces: vec![Pattern::parse("*").expect("a pattern")],
                nodes: NodeScope::All,
                issued_at: 1_000_000,
                expires_at: None,
                revoked_at: None,
            })
            .expect("written");
    }
    let grants = Grants::new(Arc::clone(&store), Clock::new(), Policy::default());

    let cmd = Cmd::new("mail.send", Risk::Outbound);
    for issuer in [Issuer::Operator, Issuer::Cli] {
        match grants.decide_for(&issuer, &cmd).expect("decidable") {
            Decision::Refuse(_) => {}
            other => panic!(
                "{} was handed a standing grant by its own display name: {other:?}",
                issuer.label()
            ),
        }
    }
}

// ---------------------------------------------------------------------------
// Risk routing inside the grant
// ---------------------------------------------------------------------------

#[test]
fn a_destructive_outbound_or_cross_project_command_waits_instead_of_running() {
    let grant = raw_grant(vec![Pattern::parse("files.*").unwrap()], NodeScope::All);

    for (risk, expected) in [
        (Risk::Destructive, QueueReason::Destructive),
        (Risk::Outbound, QueueReason::Outbound),
        (Risk::CrossProject, QueueReason::CrossProject),
    ] {
        let cmd = Cmd::new("files.delete", risk);
        match decide(&grant, &cmd, &Policy::default(), 1_000_000) {
            Decision::Queue { reason, scale } => {
                assert_eq!(reason, expected, "the operator reads this reason");
                assert_eq!(scale, Scale::Single);
            }
            other => panic!("{risk} ran straight through: {other:?}"),
        }
    }

    // The declared risk is the only input. A read stays a read.
    let read = Cmd::new("files.read", Risk::ReadOnly);
    assert!(decide(&grant, &read, &Policy::default(), 1_000_000).is_run());
}

#[test]
fn a_bulk_write_waits_while_a_small_one_runs() {
    // An agent deleting a researcher's mail one message at a time is the
    // reason this rule exists: each step looked ordinary.
    let grant = raw_grant(vec![Pattern::parse("files.*").unwrap()], NodeScope::All);
    let policy = Policy::default();

    let small =
        Cmd::new("files.write", Risk::Write).touching(Scale::Count(policy.bulk_threshold - 1));
    assert!(decide(&grant, &small, &policy, 1_000_000).is_run());

    let sweep = Cmd::new("files.write", Risk::Write).touching(Scale::Count(policy.bulk_threshold));
    match decide(&grant, &sweep, &policy, 1_000_000) {
        Decision::Queue { reason, scale } => {
            assert_eq!(
                reason,
                QueueReason::Bulk {
                    count: policy.bulk_threshold
                }
            );
            assert_eq!(
                scale.count(),
                Some(policy.bulk_threshold),
                "the count must travel with the item or the operator cannot be shown it"
            );
        }
        other => panic!("a bulk write produced {other:?}"),
    }
}

#[test]
fn a_write_that_will_not_say_its_size_waits_but_a_wide_read_still_runs() {
    let grant = raw_grant(vec![Pattern::parse("files.*").unwrap()], NodeScope::All);
    let policy = Policy::default();

    let unmeasured = Cmd::new("files.write", Risk::Write).touching(Scale::Unknown);
    assert!(matches!(
        decide(&grant, &unmeasured, &policy, 1_000_000),
        Decision::Queue {
            reason: QueueReason::ScaleUnknown,
            ..
        }
    ));

    // Reading ten thousand files is not the hazard the rule guards against.
    let wide_read = Cmd::new("files.read", Risk::ReadOnly).touching(Scale::Unknown);
    assert!(decide(&grant, &wide_read, &policy, 1_000_000).is_run());

    let lenient = Policy {
        queue_unknown_scale: false,
        ..Policy::default()
    };
    assert!(decide(&grant, &unmeasured, &lenient, 1_000_000).is_run());
}

// ---------------------------------------------------------------------------
// The Inbox: counts before approval
// ---------------------------------------------------------------------------

#[test]
fn a_bulk_item_cannot_be_approved_without_echoing_the_count_it_showed() {
    let (inbox, _clock) = inbox(Policy::default());
    let cmd = Cmd::new("files.delete", Risk::Write).touching(Scale::Count(412));
    let item = inbox
        .queue(queued(
            &cmd,
            QueueReason::Bulk { count: 412 },
            Scale::Count(412),
            "s1",
        ))
        .expect("queued");

    assert!(
        item.needs_count_confirmation(),
        "a client that never renders the number must not be able to approve this"
    );
    assert!(matches!(
        inbox.approve(&item.id, Confirmation::Simple),
        Err(InboxError::CountRequired { .. })
    ));
    assert!(matches!(
        inbox.approve(&item.id, Confirmation::CountSeen(1)),
        Err(InboxError::CountMismatch { .. })
    ));
    assert!(matches!(
        inbox.approve(&item.id, Confirmation::UnknownAccepted),
        Err(InboxError::CountRequired { .. })
    ));

    let approved = inbox
        .approve(&item.id, Confirmation::CountSeen(412))
        .expect("the stated count matches what will run");
    assert_eq!(approved.item.status, ItemStatus::Approved);
}

#[test]
fn an_unbounded_item_cannot_be_approved_with_a_number() {
    let (inbox, _clock) = inbox(Policy::default());
    let cmd = Cmd::new("files.delete", Risk::Write).touching(Scale::Unknown);
    let item = inbox
        .queue(queued(
            &cmd,
            QueueReason::ScaleUnknown,
            Scale::Unknown,
            "s1",
        ))
        .expect("queued");

    // A number would be a fiction: nothing knows how many this touches.
    assert!(matches!(
        inbox.approve(&item.id, Confirmation::CountSeen(10)),
        Err(InboxError::CountRequired { .. })
    ));
    assert!(matches!(
        inbox.approve(&item.id, Confirmation::Simple),
        Err(InboxError::CountRequired { .. })
    ));
    assert!(
        inbox
            .approve(&item.id, Confirmation::UnknownAccepted)
            .is_ok(),
        "the operator may accept the unbounded case, but only explicitly"
    );
}

#[test]
fn a_destructive_item_states_its_count_even_when_it_is_one_thing() {
    let (inbox, _clock) = inbox(Policy::default());
    let cmd = Cmd::new("files.delete", Risk::Destructive);
    let item = inbox
        .queue(queued(&cmd, QueueReason::Destructive, Scale::Single, "s1"))
        .expect("queued");

    assert!(item.needs_count_confirmation());
    assert!(matches!(
        inbox.approve(&item.id, Confirmation::Simple),
        Err(InboxError::CountRequired { .. })
    ));
    assert!(inbox.approve(&item.id, Confirmation::CountSeen(1)).is_ok());
}

#[test]
fn a_single_outbound_item_approves_without_a_count() {
    // One message leaving the machine is a question about the act, not about
    // a number; demanding "1" everywhere would train the operator to tap.
    let (inbox, _clock) = inbox(Policy::default());
    let cmd = Cmd::new("mail.send", Risk::Outbound);
    let item = inbox
        .queue(queued(&cmd, QueueReason::Outbound, Scale::Single, "s1"))
        .expect("queued");

    assert!(!item.needs_count_confirmation());
    assert!(inbox.approve(&item.id, Confirmation::Simple).is_ok());
}

#[test]
fn approving_an_item_runs_it_once_and_only_once() {
    let (inbox, _clock) = inbox(Policy::default());
    let cmd = Cmd::new("mail.send", Risk::Outbound);
    let item = inbox
        .queue(queued(&cmd, QueueReason::Outbound, Scale::Single, "s1"))
        .expect("queued");

    assert!(inbox.approve(&item.id, Confirmation::Simple).is_ok());

    // A second tap — a retried request, a double click, two phones — must not
    // dispatch the command again.
    assert!(matches!(
        inbox.approve(&item.id, Confirmation::Simple),
        Err(InboxError::NotPending { .. })
    ));
    assert!(matches!(
        inbox.deny(&item.id, None),
        Err(InboxError::NotPending { .. })
    ));
    assert!(matches!(
        inbox.withdraw(&item.id),
        Err(InboxError::NotPending { .. })
    ));
    assert!(
        inbox.pending().expect("readable").is_empty(),
        "a decided item must leave the queue, or it will be offered again"
    );
}

#[test]
fn a_refused_approval_is_not_a_decision() {
    // If a bad confirmation decided the item, a mistyped count would destroy
    // the operator's chance to approve the thing they meant to approve.
    let (inbox, _clock) = inbox(Policy::default());
    let cmd = Cmd::new("files.delete", Risk::Write).touching(Scale::Count(7));
    let item = inbox
        .queue(queued(
            &cmd,
            QueueReason::Bulk { count: 7 },
            Scale::Count(7),
            "s1",
        ))
        .expect("queued");

    assert!(inbox.approve(&item.id, Confirmation::CountSeen(6)).is_err());
    let after = inbox
        .item(&item.id)
        .expect("readable")
        .expect("still there");
    assert_eq!(after.status, ItemStatus::Pending);
    assert_eq!(inbox.pending_count().expect("readable"), 1);
    assert!(inbox.approve(&item.id, Confirmation::CountSeen(7)).is_ok());
}

#[test]
fn lockdown_freezes_approval_but_never_denial() {
    let (mut inbox, _clock) = inbox(Policy::default());
    let cmd = Cmd::new("mail.send", Risk::Outbound);
    let a = inbox
        .queue(queued(&cmd, QueueReason::Outbound, Scale::Single, "s1"))
        .expect("queued");
    let b = inbox
        .queue(queued(&cmd, QueueReason::Outbound, Scale::Single, "s1"))
        .expect("queued");

    inbox.set_frozen(true);
    assert!(matches!(
        inbox.approve(&a.id, Confirmation::Simple),
        Err(InboxError::Frozen)
    ));
    assert_eq!(
        inbox.item(&a.id).expect("readable").expect("there").status,
        ItemStatus::Pending,
        "a frozen approval leaves the item for later, it does not lose it"
    );
    // Stopping a queued action is never the dangerous direction.
    assert!(inbox.deny(&b.id, Some("no".to_string())).is_ok());

    inbox.set_frozen(false);
    assert!(inbox.approve(&a.id, Confirmation::Simple).is_ok());
}

// ---------------------------------------------------------------------------
// The Inbox: item lifetime
// ---------------------------------------------------------------------------

#[test]
fn an_item_is_not_offered_as_pending_at_the_instant_it_expires() {
    // Listing and approving must agree: an item the Inbox still shows, and
    // then refuses when tapped, reads to the operator as a broken button.
    let policy = Policy {
        pending_ttl_secs: Some(100),
        ..Policy::default()
    };
    let (inbox, clock) = inbox(policy);
    let cmd = Cmd::new("mail.send", Risk::Outbound);
    let item = inbox
        .queue(queued(&cmd, QueueReason::Outbound, Scale::Single, "s1"))
        .expect("queued");

    clock.advance(99);
    assert_eq!(inbox.pending().expect("readable").len(), 1);

    clock.advance(1); // exactly at the expiry
    assert!(item.is_expired(clock.now()));
    assert!(
        inbox.pending().expect("readable").is_empty(),
        "an item that approve() would refuse must not be listed as approvable"
    );
    assert_eq!(
        inbox
            .item(&item.id)
            .expect("readable")
            .expect("still on record")
            .status,
        ItemStatus::Expired,
        "the expiry is recorded, so the same item cannot be presented as pending again"
    );
}

#[test]
fn an_expired_item_cannot_be_approved_and_the_expiry_is_recorded() {
    let policy = Policy {
        pending_ttl_secs: Some(100),
        ..Policy::default()
    };
    let (inbox, clock) = inbox(policy);
    let cmd = Cmd::new("files.delete", Risk::Destructive).touching(Scale::Count(412));
    let item = inbox
        .queue(queued(
            &cmd,
            QueueReason::Destructive,
            Scale::Count(412),
            "s1",
        ))
        .expect("queued");

    clock.advance(1_000);
    // Approving straight from a page left open all week, without listing.
    assert!(matches!(
        inbox.approve(&item.id, Confirmation::CountSeen(412)),
        Err(InboxError::Expired { .. })
    ));
    assert_eq!(
        inbox
            .item(&item.id)
            .expect("readable")
            .expect("there")
            .status,
        ItemStatus::Expired
    );
}

#[test]
fn an_item_without_a_policy_lifetime_keeps_waiting() {
    let policy = Policy {
        pending_ttl_secs: None,
        ..Policy::default()
    };
    let (inbox, clock) = inbox(policy);
    let cmd = Cmd::new("mail.send", Risk::Outbound);
    let item = inbox
        .queue(queued(&cmd, QueueReason::Outbound, Scale::Single, "s1"))
        .expect("queued");

    clock.advance(400 * 24 * 3_600);
    assert_eq!(inbox.pending().expect("readable").len(), 1);
    assert!(inbox.approve(&item.id, Confirmation::Simple).is_ok());
}

#[test]
fn ending_a_session_withdraws_its_waiting_items_even_when_one_has_lapsed() {
    // A session's items must not outlive it. One stale item used to abort the
    // whole sweep, leaving the rest of that session's commands waiting for an
    // answer nobody would ever give.
    let policy = Policy {
        pending_ttl_secs: Some(100),
        ..Policy::default()
    };
    let (inbox, clock) = inbox(policy);
    let cmd = Cmd::new("mail.send", Risk::Outbound);

    let stale = inbox
        .queue(queued(&cmd, QueueReason::Outbound, Scale::Single, "s1"))
        .expect("queued");
    clock.advance(60);
    let fresh = inbox
        .queue(queued(&cmd, QueueReason::Outbound, Scale::Single, "s1"))
        .expect("queued");
    let other_session = inbox
        .queue(queued(&cmd, QueueReason::Outbound, Scale::Single, "s2"))
        .expect("queued");
    clock.advance(50); // stale is past its expiry, fresh is not

    let withdrawn = inbox
        .withdraw_for_session("s1")
        .expect("ending a session is cleanup and must not fail over a lapsed item");
    assert_eq!(withdrawn, 1, "one item was still waiting for this session");

    let status = |id: &_| inbox.item(id).expect("readable").expect("on record").status;
    assert_eq!(status(&fresh.id), ItemStatus::Withdrawn);
    assert_ne!(
        status(&stale.id),
        ItemStatus::Pending,
        "the lapsed item must not be left pending either"
    );
    assert_eq!(
        status(&other_session.id),
        ItemStatus::Pending,
        "another session's items are not this session's to withdraw"
    );
}

// ---------------------------------------------------------------------------
// Wire and storage shapes
// ---------------------------------------------------------------------------

#[test]
fn a_pending_item_keeps_its_wire_shape_through_json() {
    // This is what the browser renders the approval from. A tag renamed on
    // one side only is how the operator ends up approving a blank card.
    let (inbox, _clock) = inbox(Policy::default());
    let cmd = Cmd::new("files.delete", Risk::Destructive).touching(Scale::Count(412));
    let item = inbox
        .queue(queued(
            &cmd,
            QueueReason::Destructive,
            Scale::Count(412),
            "s1",
        ))
        .expect("queued");

    let json = serde_json::to_value(&item).expect("serializable");
    assert_eq!(json["risk"], "destructive");
    assert_eq!(json["status"], "pending");
    assert_eq!(json["reason"], "destructive");
    assert_eq!(
        json["scale"]["count"], 412,
        "the count the operator must be shown has to survive the wire"
    );
    assert_eq!(json["issuer"]["kind"], "session");

    let back: deckjack_core::grants::PendingItem =
        serde_json::from_value(json).expect("deserializable");
    assert_eq!(back, item);
}

#[test]
fn a_grant_carrying_an_unreadable_pattern_does_not_deserialize() {
    let grant = raw_grant(vec![Pattern::parse("git.*").unwrap()], NodeScope::All);
    let mut json = serde_json::to_value(&grant).expect("serializable");
    assert_eq!(json["namespaces"][0], "git.*", "patterns travel as text");

    json["namespaces"] = serde_json::json!(["git.pu*"]);
    assert!(
        serde_json::from_value::<Grant>(json).is_err(),
        "a grant whose pattern cannot be read must not load as a grant that allows something else"
    );
}

#[test]
fn grants_round_trip_through_their_storage_columns() {
    let grant = raw_grant(
        vec![
            Pattern::parse("git.*").unwrap(),
            Pattern::parse("files.*.read").unwrap(),
            Pattern::parse("mail.send").unwrap(),
        ],
        NodeScope::Only(vec!["r620".to_string(), "fold".to_string()]),
    );

    assert_eq!(
        Grant::parse_namespaces(&grant.namespaces_text()),
        grant.namespaces,
        "a middle wildcard must not come back as a trailing one"
    );
    assert_eq!(NodeScope::from_text(&grant.nodes.as_text()), grant.nodes);
    assert_eq!(
        NodeScope::from_text(&NodeScope::All.as_text()),
        NodeScope::All
    );

    for risk in EVERY_RISK {
        assert_eq!(Risk::parse(risk.as_str()), Some(risk));
    }
    assert_eq!(
        Risk::parse("safe"),
        None,
        "an unreadable risk column must not read as the harmless level"
    );

    for scale in [Scale::Single, Scale::Count(412), Scale::Unknown] {
        let (kind, count) = scale.as_parts();
        assert_eq!(Scale::from_parts(kind, count), Some(scale));
    }
    for status in [
        ItemStatus::Pending,
        ItemStatus::Approved,
        ItemStatus::Denied,
        ItemStatus::Expired,
        ItemStatus::Withdrawn,
    ] {
        assert_eq!(ItemStatus::parse(status.as_str()), Some(status));
    }
}
