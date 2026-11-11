//! The audit log as it is actually wired: the bus's journal, the `events`
//! table, and the read path the timeline panel calls.
//!
//! `tests/events.rs` already covers `events.rs` through its own sink. What is
//! asserted here is the thing that was missing and that no module test could
//! see: that a dispatch on a real `Bus` lands in `events`, that the two rows
//! one dispatch produces can be told apart, that nothing the bus withheld —
//! a redacting command's parameters, a session cookie — reaches the table,
//! and that what was written reads back by issuer, by time range and by
//! dispatch.
//!
//! Two tests build the journal sandwich `main.rs` builds, `WatchJournal`
//! wrapping `EventsJournal`, over a real `Lockdown`. That is deliberate: this
//! subsystem's whole defect class is a module that is complete, tested and
//! never constructed, and a test that attaches its own double to its own
//! objects would reproduce it exactly.
//!
//! Nothing here sleeps. Every timestamp is either chosen by the test or taken
//! from the bus's own clock, and every handler returns without awaiting, so
//! the one-poll `block_on` below is enough.

use std::future::Future;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use deckjack_core::audit::{EventsJournal, Phase, StoreSink, TimelineRow, register_timeline};
use deckjack_core::auth::SessionId;
use deckjack_core::command::{
    Admitted, BrowserSession, Bus, Command, CommandError, CommandId, Context, Decision, DispatchId,
    Issuer, Journal, LogEntry, NodeId, OsEnv, Registry, Request, RiskLevel, Running,
};
use deckjack_core::events::{CommandFilter, IssuerFilter, Order, Outcome, Query, Record, Sink};
use deckjack_core::lockdown::{self, Lockdown, Watchdog};
use deckjack_core::store::{DB_PATH, Store};
use deckjack_core::wiring::{self, NoParams};

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

/// A migrated state database and the shared store over it.
///
/// Through `Store::open`, exactly as the server does: the store owns the
/// schema, and a fixture that created the `events` table itself would hide a
/// missing migration — which is the defect this whole subsystem exists to
/// undo.
struct Db {
    dir: tempfile::TempDir,
    store: Arc<Mutex<Store>>,
}

impl Db {
    fn new() -> Db {
        let dir = tempfile::tempdir().expect("a working directory");
        let store = Arc::new(Mutex::new(
            Store::open(dir.path()).expect("the state database"),
        ));
        Db { dir, store }
    }

    fn path(&self) -> PathBuf {
        self.dir.path().join(DB_PATH)
    }

    fn sink(&self) -> Arc<StoreSink> {
        StoreSink::new(Arc::clone(&self.store))
    }

    fn journal(&self) -> Arc<EventsJournal> {
        EventsJournal::new(Arc::clone(&self.store))
    }

    fn rows(&self) -> Vec<TimelineRow> {
        self.sink()
            .timeline(&Query::default().order(Order::OldestFirst))
            .expect("the log reads back")
    }

    /// Every value in every column of every row, as a reader of the file
    /// would see them.
    ///
    /// Deliberately not "the columns I expect a secret to be in": the claims
    /// under test are that a cookie and a redacted parameter are in the table
    /// *nowhere*, and a grep that names the columns would pass while a secret
    /// sat in `detail`.
    fn all_text(&self) -> String {
        let store = self.store.lock().expect("the store");
        store
            .with_conn(|conn| {
                let mut stmt = conn.prepare("SELECT * FROM events")?;
                let columns = stmt.column_count();
                let mut out = String::new();
                let mut rows = stmt.query([])?;
                while let Some(row) = rows.next()? {
                    for i in 0..columns {
                        let value: rusqlite::types::Value = row.get(i)?;
                        out.push_str(&format!("{value:?}\n"));
                    }
                }
                Ok(out)
            })
            .expect("the events table is readable")
    }
}

/// A fixed millisecond, so an expected timestamp is exact: the column is
/// milliseconds, and a test using `SystemTime::now()` could not tell
/// truncation from correctness.
fn at_ms(ms: u64) -> SystemTime {
    UNIX_EPOCH + Duration::from_millis(ms)
}

fn dispatch_id(n: u8) -> DispatchId {
    DispatchId::from_bytes([n; 12])
}

/// A log entry as the bus hands one over.
fn log_entry(dispatch: DispatchId, decision: Decision, issuer: Issuer, ms: u64) -> LogEntry {
    LogEntry {
        dispatch,
        at: at_ms(ms),
        node: NodeId::new("node-a"),
        command: CommandId::new("files.delete").expect("a valid command id"),
        risk: Some(RiskLevel::Destructive),
        issuer,
        decision,
        params: None,
        detail: None,
    }
}

/// The one-poll runtime from `tests/command.rs`: no handler below awaits, so
/// a dispatch is settled by the time it returns, and a test that waited on a
/// duration would be testing the duration.
fn block_on<F: Future>(future: F) -> F::Output {
    let mut future = Box::pin(future);
    let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
    match future.as_mut().poll(&mut cx) {
        std::task::Poll::Ready(value) => value,
        std::task::Poll::Pending => panic!("a handler parked; nothing here awaits"),
    }
}

fn expect_running(admitted: Admitted) -> Running {
    match admitted {
        Admitted::Running(running) => running,
        other => panic!("expected the command to run, got {other:?}"),
    }
}

// ---- commands to dispatch -----------------------------------------------

struct Probe;

impl Command for Probe {
    const ID: &'static str = "probe.read";
    const TITLE: &'static str = "Probe";
    const RISK: RiskLevel = RiskLevel::ReadOnly;

    type Params = NoParams;
    type Output = bool;

    async fn run(&self, _p: NoParams, _cx: Context) -> Result<bool, CommandError> {
        Ok(true)
    }
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
struct SecretParams {
    password: String,
}

/// A command whose spec redacts its parameters: the shape `terminal.write`
/// and a password prompt have.
struct Secret;

impl Command for Secret {
    const ID: &'static str = "probe.secret";
    const TITLE: &'static str = "Probe with a password";
    const RISK: RiskLevel = RiskLevel::Write;
    const REDACT_PARAMS: bool = true;

    type Params = SecretParams;
    type Output = bool;

    async fn run(&self, p: SecretParams, _cx: Context) -> Result<bool, CommandError> {
        Ok(!p.password.is_empty())
    }
}

/// A command that fails, so a `Failed` row is written by the bus rather than
/// by the test.
struct Broken;

impl Command for Broken {
    const ID: &'static str = "probe.broken";
    const TITLE: &'static str = "Probe that fails";
    const RISK: RiskLevel = RiskLevel::Write;

    type Params = NoParams;
    type Output = bool;

    async fn run(&self, _p: NoParams, _cx: Context) -> Result<bool, CommandError> {
        Err(CommandError::Failed(anyhow::anyhow!("the probe broke")))
    }
}

// ---- the real journal sandwich ------------------------------------------

/// Sessions, for a `Lockdown` that this test never engages. The trait is two
/// methods; a real `AuthSessions` would pull in the auth state machine and a
/// terminal registry to prove nothing this file claims.
struct NoSessions;

impl lockdown::Sessions for NoSessions {
    fn revoke_all(&self) -> usize {
        0
    }
    fn adopt_signing_key(&self, _key: lockdown::SessionKey) {}
}

/// A bus with the journal `main.rs` gives it: `EventsJournal` *inside*
/// `WatchJournal`.
///
/// The wrapping is load-bearing in both directions. `WatchJournal` is what
/// counts a burst of refused agent commands for the kill switch, and it only
/// sees them because every decision passes through the journal; the inner
/// journal is what makes the decision durable. A test that passed
/// `EventsJournal` to the bus directly would still pass if someone dropped
/// the wrapper and silently disarmed the watchdog.
fn bus_with_watched_events_journal(db: &Db, registry: &Arc<Registry>) -> Arc<Bus> {
    let lockdown = Arc::new(
        Lockdown::restore(
            lockdown::Deps {
                clock: Box::new(lockdown::OsClock),
                random: Box::new(lockdown::OsRandom),
                sessions: Box::new(NoSessions),
                children: Box::new(lockdown::NoChildren),
                signals: Box::new(lockdown::OsSignals::new()),
                outbound: Box::new(lockdown::NoOutbound),
                passkeys: Box::new(lockdown::PasskeysUnavailable),
                journal: Box::new(lockdown::MemoryJournal::new()),
            },
            lockdown::Policy::default(),
            "node-a".to_string(),
        )
        .expect("a fresh lockdown journal"),
    );
    let watchdog = Arc::new(Watchdog::new(lockdown.policy().auto));

    Arc::new(
        Bus::new(Arc::clone(registry), NodeId::new("node-a"), Arc::new(OsEnv))
            .with_journal(wiring::WatchJournal::new(db.journal(), watchdog, lockdown)),
    )
}

// ---------------------------------------------------------------------------
// Phase: telling one dispatch's two rows apart
// ---------------------------------------------------------------------------

#[test]
fn a_dispatch_that_ran_and_then_completed_produces_two_rows_that_can_be_told_apart() {
    let db = Db::new();
    let journal = db.journal();
    let dispatch = dispatch_id(7);

    journal
        .record(log_entry(
            dispatch.clone(),
            Decision::Ran,
            Issuer::operator(),
            1_000,
        ))
        .expect("the admission is recorded");
    journal
        .record(log_entry(
            dispatch.clone(),
            Decision::Completed,
            Issuer::operator(),
            1_250,
        ))
        .expect("the completion is recorded");

    let rows = db.rows();
    assert_eq!(rows.len(), 2, "both halves of one dispatch are logged");

    // If this fails the timeline shows every command twice, identically, and
    // "did this run or is it still running" has no answer in the log — which
    // is the question anyone asks it first.
    assert_eq!(rows[0].phase, Some(Phase::Ran));
    assert_eq!(rows[1].phase, Some(Phase::Completed));
    assert!(!rows[0].is_final(), "admission is not an ending");
    assert!(rows[1].is_final(), "completion is");

    // Both are successes, which is correct and is exactly why the phase has
    // to carry the difference.
    assert_eq!(rows[0].entry.outcome, Outcome::Ok);
    assert_eq!(rows[1].entry.outcome, Outcome::Ok);

    // And both are findable as one dispatch, which is what migration 5's
    // `events_dispatch` index was added for.
    let together = db.sink().dispatch(&dispatch.to_string()).expect("the pair");
    assert_eq!(together.len(), 2);
    assert_eq!(
        together.iter().map(|r| r.phase).collect::<Vec<_>>(),
        vec![Some(Phase::Ran), Some(Phase::Completed)]
    );
}

#[test]
fn every_decision_the_bus_can_make_is_a_distinct_phase() {
    let db = Db::new();
    let journal = db.journal();

    let decisions = [
        Decision::Ran,
        Decision::Queued,
        Decision::AwaitingConfirmation,
        Decision::Refused,
        Decision::Completed,
        Decision::Failed,
    ];
    for (n, decision) in decisions.iter().enumerate() {
        journal
            .record(log_entry(
                dispatch_id(n as u8),
                *decision,
                Issuer::operator(),
                1_000 + n as u64,
            ))
            .expect("recorded");
    }

    let phases: Vec<Option<Phase>> = db.rows().iter().map(|r| r.phase).collect();
    assert_eq!(
        phases,
        vec![
            Some(Phase::Ran),
            Some(Phase::Queued),
            Some(Phase::AwaitingConfirmation),
            Some(Phase::Refused),
            Some(Phase::Completed),
            Some(Phase::Failed),
        ],
        "a decision that collapses into another one is a decision the log cannot report"
    );

    // The Inbox and the operator's confirmation dialog are different gates
    // and both are "nothing ran, a person has to answer". They share an
    // outcome on purpose; losing the phase is what would make them the same
    // row, and a queued agent request would become indistinguishable from a
    // dialog the operator has not clicked.
    let rows = db.rows();
    assert_eq!(rows[1].entry.outcome, Outcome::Queued);
    assert_eq!(rows[2].entry.outcome, Outcome::Queued);
    assert_ne!(rows[1].phase, rows[2].phase);
}

#[test]
fn an_entry_that_is_not_a_bus_dispatch_claims_no_phase() {
    let db = Db::new();
    let sink = db.sink();

    // Through the plain `Sink` API: the shipper, a reminder, anything that is
    // not a dispatch. A row that claimed a phase it does not have would tell
    // a panel there is a matching completion row somewhere, and there is not.
    sink.append(
        at_ms(500),
        &Record::new(
            deckjack_core::events::Issuer::System,
            "system.reminder",
            "node-a",
            Outcome::Ok,
        ),
    )
    .expect("the append succeeds");

    let rows = db.rows();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].phase, None);
    assert_eq!(rows[0].dispatch, None);
    assert_eq!(rows[0].risk, None);
    assert!(!rows[0].is_final());
}

// ---------------------------------------------------------------------------
// What must never reach the table
// ---------------------------------------------------------------------------

#[test]
fn a_redacted_commands_parameters_do_not_appear_in_the_events_table() {
    let db = Db::new();
    let registry = Arc::new(Registry::new());
    registry.register(Secret).expect("probe.secret registers");
    let bus = bus_with_watched_events_journal(&db, &registry);

    let running = expect_running(
        bus.dispatch(Request::new(
            CommandId::new("probe.secret").unwrap(),
            serde_json::json!({ "password": "hunter2-correct-horse" }),
            Issuer::operator(),
        ))
        .expect("a write command runs for the operator"),
    );
    block_on(running.finish());

    let text = db.all_text();
    // A previous defect in this project logged a password prompt's
    // keystrokes. The bus already decided these parameters are not to be
    // written down; this journal's job is not to second-guess that decision
    // in either direction.
    assert!(
        !text.contains("hunter2-correct-horse"),
        "a redacting command's parameters reached the audit log:\n{text}"
    );
    assert!(
        text.contains("<redacted>"),
        "the row should say the parameters were withheld rather than look empty:\n{text}"
    );
}

#[test]
fn parameters_the_bus_withheld_entirely_are_not_invented() {
    let db = Db::new();
    // `params: None` is what the bus hands over for a completion, and for a
    // refusal of a command that has no spec. An empty summary is the truth;
    // anything else would be this module making up a row the bus declined to
    // describe.
    db.journal()
        .record(log_entry(
            dispatch_id(1),
            Decision::Completed,
            Issuer::operator(),
            1_000,
        ))
        .expect("recorded");

    let rows = db.rows();
    assert!(rows[0].entry.params.is_empty());
}

#[test]
fn a_session_cookie_never_appears_in_the_events_table() {
    let db = Db::new();
    let registry = Arc::new(Registry::new());
    registry.register(Probe).expect("probe.read registers");
    let bus = bus_with_watched_events_journal(&db, &registry);

    let session = SessionId::generate().expect("a session id");
    let cookie = session.encode();
    let tag = BrowserSession::new(session.clone())
        .tag()
        .as_str()
        .to_string();

    let running = expect_running(
        bus.dispatch(Request::new(
            CommandId::new("probe.read").unwrap(),
            serde_json::json!({}),
            Issuer::browser(session),
        ))
        .expect("a read-only command runs"),
    );
    block_on(running.finish());

    let text = db.all_text();
    // `state.db` is readable from the terminal by design and the design
    // ships this log off the node, so a cookie in it is a bearer credential
    // sitting somewhere less trusted than the host.
    assert!(
        !text.contains(&cookie),
        "the session cookie reached the audit log:\n{text}"
    );
    // The tag still has to be there, or the log cannot tell two signed-in
    // browsers apart, which is the whole reason the column exists.
    assert!(
        text.contains(&tag),
        "the browser session's tag is missing, so the row names no browser:\n{text}"
    );
    assert_eq!(
        db.rows()[0].issuer_session.as_deref(),
        Some(tag.as_str()),
        "the tag belongs in issuer_session, not wherever it happened to land"
    );
}

#[test]
fn the_append_only_triggers_refuse_an_update_and_a_delete_through_this_path() {
    let db = Db::new();
    db.journal()
        .record(log_entry(
            dispatch_id(3),
            Decision::Ran,
            Issuer::operator(),
            1_000,
        ))
        .expect("recorded");

    // Through the store's own connection — the one this module writes with,
    // not a second handle to the file. "Append-only" has to hold against the
    // connection that does the appending, or a bug in Deckjack is enough to
    // rewrite history.
    let store = db.store.lock().expect("the store");
    let updated = store.with_conn(|conn| {
        conn.execute("UPDATE events SET command = 'files.read' WHERE seq = 1", [])
    });
    let deleted = store.with_conn(|conn| conn.execute("DELETE FROM events WHERE seq = 1", []));
    drop(store);

    let update_error = updated.expect_err("an UPDATE must be refused").to_string();
    let delete_error = deleted.expect_err("a DELETE must be refused").to_string();
    assert!(
        update_error.contains("append-only"),
        "the trigger's own message should say why: {update_error}"
    );
    assert!(
        delete_error.contains("append-only"),
        "the trigger's own message should say why: {delete_error}"
    );

    let rows = db.rows();
    assert_eq!(rows.len(), 1, "the row survived both attempts");
    assert_eq!(rows[0].entry.command, "files.delete");
}

// ---------------------------------------------------------------------------
// Attribution
// ---------------------------------------------------------------------------

#[test]
fn an_agents_command_is_attributed_to_its_session_name() {
    let db = Db::new();
    db.journal()
        .record(log_entry(
            dispatch_id(4),
            Decision::Refused,
            Issuer::agent("coding-2026-11-08"),
            2_000,
        ))
        .expect("recorded");

    let rows = db.rows();
    assert_eq!(
        rows[0].entry.issuer,
        deckjack_core::events::Issuer::Agent {
            session: "coding-2026-11-08".to_string(),
            // Not yet available: nothing carries the invited window to the
            // bus's issuer. If this ever becomes `Some`, the module's comment
            // about what `window` waits for is stale.
            window: None,
        },
        "an agent's actions have to be attributable to the session, or a grant \
         cannot be audited after the fact"
    );
    assert_eq!(
        rows[0].entry.issuer.label(),
        "coding-2026-11-08",
        "the timeline names the agent the way the Assistant does"
    );
    // An agent has no browser, so there is nothing to tag.
    assert_eq!(rows[0].issuer_session, None);
}

#[test]
fn the_operator_the_cli_and_an_agent_are_three_different_issuers_in_the_log() {
    let db = Db::new();
    let journal = db.journal();
    for (n, issuer) in [Issuer::operator(), Issuer::Cli, Issuer::agent("coding")]
        .into_iter()
        .enumerate()
    {
        journal
            .record(log_entry(
                dispatch_id(10 + n as u8),
                Decision::Ran,
                issuer,
                3_000 + n as u64,
            ))
            .expect("recorded");
    }

    // Three rows that read as one issuer would make "which of these was me"
    // unanswerable, and that question is the point of the log.
    let labels: Vec<String> = db.rows().iter().map(|r| r.entry.issuer.label()).collect();
    assert_eq!(labels, vec!["me", "the CLI", "coding"]);
}

// ---------------------------------------------------------------------------
// The read path
// ---------------------------------------------------------------------------

#[test]
fn reading_back_by_issuer_returns_only_that_issuers_commands() {
    let db = Db::new();
    let journal = db.journal();
    journal
        .record(log_entry(
            dispatch_id(1),
            Decision::Ran,
            Issuer::operator(),
            1_000,
        ))
        .unwrap();
    journal
        .record(log_entry(
            dispatch_id(2),
            Decision::Ran,
            Issuer::agent("coding"),
            1_100,
        ))
        .unwrap();
    journal
        .record(log_entry(
            dispatch_id(3),
            Decision::Ran,
            Issuer::agent("research"),
            1_200,
        ))
        .unwrap();

    let sink = db.sink();
    let one = sink
        .timeline(&Query::default().issuer(IssuerFilter::agent("coding")))
        .expect("the query runs");
    assert_eq!(one.len(), 1);
    assert_eq!(one[0].entry.issuer.label(), "coding");

    // `Agent { session: None }` is every agent, not an agent with no name —
    // the filter the panel uses for "what have the agents been doing".
    let any_agent = sink
        .timeline(&Query::default().issuer(IssuerFilter::any_agent()))
        .expect("the query runs");
    assert_eq!(any_agent.len(), 2);

    let mine = sink
        .timeline(&Query::default().issuer(IssuerFilter::Operator))
        .expect("the query runs");
    assert_eq!(mine.len(), 1);
    assert_eq!(mine[0].entry.issuer.label(), "me");
}

#[test]
fn reading_back_by_time_range_returns_what_was_written_inside_it() {
    let db = Db::new();
    let journal = db.journal();
    for (n, ms) in [1_000u64, 2_000, 3_000, 4_000].iter().enumerate() {
        journal
            .record(log_entry(
                dispatch_id(n as u8),
                Decision::Ran,
                Issuer::operator(),
                *ms,
            ))
            .unwrap();
    }

    let inside = db
        .sink()
        .timeline(
            &Query::default()
                .range(at_ms(2_000), at_ms(3_000))
                .order(Order::OldestFirst),
        )
        .expect("the query runs");

    // Inclusive at both ends, and in milliseconds: a range that silently
    // dropped the entries on its boundary would make "what happened between
    // 09:00 and 09:05" answer with something else.
    assert_eq!(inside.len(), 2);
    assert_eq!(inside[0].entry.at, at_ms(2_000));
    assert_eq!(inside[1].entry.at, at_ms(3_000));
}

#[test]
fn reading_back_by_command_covers_a_namespace_the_way_a_grant_is_written() {
    let db = Db::new();
    let journal = db.journal();
    for (n, command) in ["files.delete", "files.read", "git.status"]
        .iter()
        .enumerate()
    {
        let mut entry = log_entry(
            dispatch_id(n as u8),
            Decision::Ran,
            Issuer::operator(),
            1_000 + n as u64,
        );
        entry.command = CommandId::new(*command).unwrap();
        journal.record(entry).unwrap();
    }

    let sink = db.sink();
    let namespace = sink
        .timeline(&Query::default().command(CommandFilter::namespace("files")))
        .expect("the query runs");
    assert_eq!(
        namespace.len(),
        2,
        "a grant is written against a namespace, so the panel has to be able to \
         show everything one covered"
    );

    let exact = sink
        .timeline(&Query::default().command(CommandFilter::exact("files.read")))
        .expect("the query runs");
    assert_eq!(exact.len(), 1);
    assert_eq!(exact[0].entry.command, "files.read");
}

#[test]
fn a_row_written_through_the_store_reads_back_through_the_events_sink() {
    let db = Db::new();
    db.journal()
        .record(log_entry(
            dispatch_id(9),
            Decision::Failed,
            Issuer::agent("coding"),
            5_000,
        ))
        .expect("recorded");

    // `audit.rs` keeps its own copy of the column encoding, because
    // `events.rs` keeps its SQL private and its INSERT predates migration 5's
    // four columns. This is what holds the two copies together: the row is
    // written here and read by `events.rs`'s own sink, so a divergence in
    // either file fails a test instead of producing a log that one half of
    // the code cannot read.
    let theirs = deckjack_core::events::SqliteSink::open(&db.path()).expect("their sink opens");
    let entries = theirs
        .query(&Query::default())
        .expect("their reader understands our row");

    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0], db.rows()[0].entry);
    assert_eq!(
        entries[0].outcome,
        Outcome::Failed {
            error: String::new()
        }
    );
    assert_eq!(entries[0].at, at_ms(5_000));
}

// ---------------------------------------------------------------------------
// The wiring itself
// ---------------------------------------------------------------------------

#[test]
fn a_dispatch_on_a_bus_wired_the_way_main_wires_it_lands_in_the_events_table() {
    let db = Db::new();
    let registry = Arc::new(Registry::new());
    registry.register(Broken).expect("probe.broken registers");
    let bus = bus_with_watched_events_journal(&db, &registry);

    let running = expect_running(
        bus.dispatch(Request::new(
            CommandId::new("probe.broken").unwrap(),
            serde_json::json!({}),
            Issuer::operator(),
        ))
        .expect("a write command runs for the operator"),
    );
    let completion = block_on(running.finish());
    assert!(completion.outcome.is_err());

    // The claim: the audit log is not a module that compiles, it is a table
    // with rows in it after a command runs. Delete the journal from the bus
    // and this is the test that fails.
    let rows = db.rows();
    assert_eq!(rows.len(), 2, "admission and failure, through WatchJournal");
    assert_eq!(rows[0].phase, Some(Phase::Ran));
    assert_eq!(rows[1].phase, Some(Phase::Failed));
    assert_eq!(rows[1].entry.command, "probe.broken");
    assert_eq!(rows[1].risk.as_deref(), Some("write"));
    assert_eq!(
        rows[1].entry.outcome,
        Outcome::Failed {
            error: "the probe broke".to_string()
        },
        "a failure that does not say how it failed is a row nobody can act on"
    );
    assert_eq!(
        rows[0].dispatch, rows[1].dispatch,
        "both rows name the same dispatch, or they cannot be paired"
    );
    assert_eq!(rows[0].entry.target_node, "node-a");

    // Not recorded, and honestly so: the bus does not carry the path a
    // handler touched or the address a request came from.
    assert_eq!(rows[1].entry.path, None);
    assert_eq!(rows[1].entry.from, None);
}

#[test]
fn a_refused_agent_command_is_written_with_its_reason_through_the_watchdog_wrapper() {
    let db = Db::new();
    let registry = Arc::new(Registry::new());
    registry.register(Probe).expect("probe.read registers");
    let bus = bus_with_watched_events_journal(&db, &registry);

    // No grant, so the bus refuses. `WatchJournal` counts this on its way
    // past for the kill switch's "a burst of refused agent commands" trigger,
    // and must still hand it to the inner journal: a refusal nobody wrote
    // down is a refusal nobody can notice a burst of.
    let refused = bus
        .dispatch(Request::new(
            CommandId::new("probe.read").unwrap(),
            serde_json::json!({}),
            Issuer::agent("coding"),
        ))
        .expect_err("an agent with no grant is refused");

    let rows = db.rows();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].phase, Some(Phase::Refused));
    assert_eq!(rows[0].entry.issuer.label(), "coding");
    match &rows[0].entry.outcome {
        Outcome::Refused { reason } => assert_eq!(reason, &refused.to_string()),
        other => panic!("a refusal must be logged as one, got {other:?}"),
    }
}

#[test]
fn system_timeline_reads_the_log_back_over_the_bus() {
    let db = Db::new();
    let registry = Arc::new(Registry::new());
    register_timeline(&registry, db.journal().sink()).expect("system.timeline registers");
    let bus = bus_with_watched_events_journal(&db, &registry);

    let running = expect_running(
        bus.dispatch(Request::new(
            CommandId::new("system.timeline").unwrap(),
            serde_json::json!({ "oldest_first": true }),
            Issuer::operator(),
        ))
        .expect("ReadOnly runs for the operator with no confirmation dialog"),
    );
    let value = block_on(running.finish())
        .outcome
        .expect("the timeline reads back");

    // The log is written on admission, so the first thing the timeline ever
    // returns is its own `ran` row. That is not a quirk to work around: it is
    // the proof that the read path and the write path are looking at one
    // table.
    let rows = value.as_array().expect("an array of rows");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["command"], "system.timeline");
    assert_eq!(rows[0]["phase"], "ran");
    assert_eq!(rows[0]["issuer"]["kind"], "operator");
    // Nothing about a browser: this dispatch came from `Issuer::operator()`,
    // which is the operator with no browser behind the action.
    assert!(rows[0].get("issuer_session").is_none());
}

#[test]
fn system_timeline_is_read_only_so_it_never_asks_the_operator_to_confirm() {
    let db = Db::new();
    let registry = Registry::new();
    register_timeline(&registry, db.journal().sink()).expect("system.timeline registers");

    let spec = registry
        .spec(&CommandId::new("system.timeline").unwrap())
        .expect("the command is on the bus");
    // Reading the log changes nothing, and a confirmation dialog in front of
    // a panel repaint is how a risk level gets quietly downgraded later to
    // make the dialog go away. The thing that keeps an agent out of the audit
    // log is its grant, not this level.
    assert_eq!(spec.risk(), RiskLevel::ReadOnly);
    assert!(!spec.risk().needs_confirmation_from_operator());
    assert!(!spec.risk().needs_approval_from_agents());
}
