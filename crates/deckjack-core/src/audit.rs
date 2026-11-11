//! The audit log, connected to the bus.
//!
//! [`crate::events`] is the attribution log: the table with the path, the
//! source address and the append-only triggers. It has shipped since
//! migration 2 and, until this module, nothing in `src/` referenced it —
//! every decision the bus made went to `command_log` instead, through
//! [`crate::store::StoreJournal`]. So the richer table sat empty while the
//! poorer one filled. This module is the half that was missing: a
//! [`crate::events::Sink`] over the shared [`Store`], a [`Journal`] that
//! translates the bus's [`LogEntry`] into an entry of that log, and the read
//! path the timeline panel calls.
//!
//! # Two tables, one of them authoritative
//!
//! `events` is now the audit log. `command_log` and its reader
//! ([`crate::store::Store::command_log`], [`crate::store::LoggedCommand`])
//! still exist and are deliberately left alone, but once `main.rs` passes
//! [`EventsJournal`] to the bus nothing writes to `command_log` any more: the
//! bus holds one journal. Retiring a table is a separate change from filling
//! a better one, and doing both at once is how a migration takes a log with
//! it.
//!
//! To retire `command_log` later, in this order: ship the timeline panel on
//! `system.timeline` so no reader depends on it, decide whether the existing
//! rows are worth copying into `events` (they carry no path and no source
//! address, so a backfill produces entries that are honest but thinner than
//! anything written after it), drop `Store::command_log`,
//! `Store::append_command_log`, `LoggedCommand` and `StoreJournal`, and only
//! then add a migration that drops the table and the `command_log_at`,
//! `command_log_dispatch`, `command_log_session` and `command_log_refusals`
//! indexes. Nothing reads that last index today — the lockdown watchdog
//! counts refusals in memory through [`crate::wiring::WatchJournal`], not in
//! SQL — but it is evidence of what the table was for, so it goes when the
//! table does.
//!
//! # What the bus cannot tell this log yet
//!
//! Three of `events`'s columns stay NULL on every row written here, because
//! [`LogEntry`] does not carry what they want: `path`, `issuer_window` and
//! `from_ip`. Each is named at the place it is left empty. They are stubs in
//! the honest direction — an empty column reads as "not recorded", where a
//! guess would read as evidence.

use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, named_params, params};
use serde::{Deserialize, Serialize};

use crate::command::{
    Command, CommandError, Context, Decision, Issuer as BusIssuer, Journal, JournalError, LogEntry,
    Registry, RegistryError, RiskLevel,
};
use crate::events::{
    CommandFilter, Entry, EntryId, EventsError, Issuer, IssuerFilter, Order, Outcome,
    ParamsSummary, Query, Record, Sink,
};
use crate::store::{Store, StoreError};

// ---- the four columns migration 5 added ---------------------------------

/// Which of a dispatch's rows this is.
///
/// The reason migration 5 exists. A single dispatch is logged **twice** in
/// the ordinary case — [`Decision::Ran`] on admission and
/// [`Decision::Completed`] or [`Decision::Failed`] when it finishes — and the
/// first two both map to a successful [`Outcome`]. Without this column the
/// timeline shows every command twice, identically, and "did this run or is
/// it still running" has no answer in the log at all.
///
/// The strings are [`Decision::as_str`]'s, exactly, so `events.phase` and the
/// older `command_log.decision` read the same way and a query written against
/// one works against the other.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    Ran,
    Queued,
    AwaitingConfirmation,
    Refused,
    Completed,
    Failed,
}

impl Phase {
    pub fn of(decision: Decision) -> Phase {
        match decision {
            Decision::Ran => Phase::Ran,
            Decision::Queued => Phase::Queued,
            Decision::AwaitingConfirmation => Phase::AwaitingConfirmation,
            Decision::Refused => Phase::Refused,
            Decision::Completed => Phase::Completed,
            Decision::Failed => Phase::Failed,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Phase::Ran => "ran",
            Phase::Queued => "queued",
            Phase::AwaitingConfirmation => "awaiting_confirmation",
            Phase::Refused => "refused",
            Phase::Completed => "completed",
            Phase::Failed => "failed",
        }
    }

    fn parse(text: &str) -> Option<Phase> {
        match text {
            "ran" => Some(Phase::Ran),
            "queued" => Some(Phase::Queued),
            "awaiting_confirmation" => Some(Phase::AwaitingConfirmation),
            "refused" => Some(Phase::Refused),
            "completed" => Some(Phase::Completed),
            "failed" => Some(Phase::Failed),
            _ => None,
        }
    }
}

/// What `command_log` recorded and the `events` table could not until
/// migration 5: which dispatch a row belongs to, the risk the backend
/// declared, which signed-in browser asked, and [`Phase`].
///
/// Separate from [`Record`] rather than fields on it, because [`Record`]
/// belongs to [`crate::events`] and is also what the VPS end of the shipper
/// will build; these four are what *this node's bus* knows. All four are
/// optional for the same reason the migration made the columns nullable: an
/// entry appended through the plain [`Sink`] API — a reminder firing, a sync
/// service, anything that is not a bus dispatch — has no dispatch id and no
/// phase, and `unknown` is the truth there.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DispatchFacts {
    /// [`crate::command::DispatchId`] as text, so the two rows of one
    /// dispatch can be found together.
    pub dispatch: Option<String>,
    pub risk: Option<RiskLevel>,
    /// [`crate::command::SessionTag`] as text: 16 hex characters of SHA-256
    /// over the session id. **Never** the session cookie — this file is
    /// readable from the terminal by design, and the log ships off the node.
    pub issuer_session: Option<String>,
    pub phase: Option<Phase>,
}

impl DispatchFacts {
    /// An append that is not a bus dispatch. Named rather than written as
    /// `Default::default()` at the call site so the absence is a decision.
    pub fn none() -> DispatchFacts {
        DispatchFacts::default()
    }
}

// ---- SQL ----------------------------------------------------------------
//
// These three statements have to agree with `events.rs`'s own, because
// `events::SqliteSink` writes and reads the same table through its own
// connection. They are copies rather than shared constants: `events.rs` keeps
// its SQL private, its `INSERT` predates the four columns above, and nothing
// in the trait boundary lets a caller hand it extra ones. The agreement is
// held by a test instead — `a_row_written_through_the_store_reads_back_through_the_events_sink`
// writes a row here and reads it there, so a divergence in either file fails
// a test rather than producing a log that one half of the code cannot read.

/// Fifteen columns: the eleven `events.rs` writes plus the four from
/// migration 5. `seq` and nothing else is left to SQLite.
const INSERT_SQL: &str = "INSERT INTO events (\
    at_ms, issuer_kind, issuer_name, issuer_window, command, target_node, \
    params, outcome, detail, path, from_ip, \
    dispatch, risk, issuer_session, phase) \
    VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)";

const SELECT_COLUMNS: &str = "seq, at_ms, issuer_kind, issuer_name, issuer_window, \
    command, target_node, params, outcome, detail, path, from_ip, \
    dispatch, risk, issuer_session, phase";

/// One statement with every filter written as "no filter, or it matches", so
/// the SQL is a constant and nothing is concatenated from caller input. The
/// `dispatch` clause is the one addition to `events.rs`'s version, and it is
/// what the `events_dispatch` index was added for: "show me both rows of this
/// dispatch" is the question the phase column exists to answer.
const WHERE_SQL: &str = "WHERE at_ms >= :since AND at_ms <= :until \
    AND seq > :after AND seq < :before \
    AND (:issuer_kind IS NULL OR issuer_kind = :issuer_kind) \
    AND (:issuer_name IS NULL OR issuer_name = :issuer_name) \
    AND (:dispatch IS NULL OR dispatch = :dispatch) \
    AND (:cmd_exact IS NULL \
         OR command = :cmd_exact \
         OR (:cmd_prefix IS NOT NULL \
             AND substr(command, 1, length(:cmd_prefix)) = :cmd_prefix))";

// ---- the sink over the shared store -------------------------------------

/// The audit log, written through the one connection in [`Store`].
///
/// Deliberately not [`crate::events::SqliteSink`], which opens a second
/// connection to `state.db`. One connection is what `store.rs` guarantees —
/// "a second writer would be two processes racing on one WAL" — and this
/// module's SQL lives here while the connection stays there, which is exactly
/// what [`Store::with_conn`] was added for.
pub struct StoreSink {
    store: Arc<Mutex<Store>>,
}

impl StoreSink {
    pub fn new(store: Arc<Mutex<Store>>) -> Arc<StoreSink> {
        Arc::new(StoreSink { store })
    }

    /// Held for one statement and never across an await: a `MutexGuard` alive
    /// in an async body makes the future non-`Send`, and the bus requires
    /// `Send` futures. A poisoned mutex is still usable here for the same
    /// reason as in `http::Ctx` — every write is a single statement that
    /// leaves the connection consistent however a previous holder died.
    fn locked(&self) -> MutexGuard<'_, Store> {
        self.store.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Append an entry with the four bus facts.
    ///
    /// `at` is the caller's, not a clock read here: the bus already stamped
    /// the dispatch from its own [`crate::command::Env`], and a journal that
    /// restamped would report a completion time that disagrees with the one
    /// the bus told the client and the trace.
    pub fn append_dispatch(
        &self,
        at: SystemTime,
        record: &Record,
        facts: &DispatchFacts,
    ) -> Result<EntryId, EventsError> {
        // Lossy on a path that is not UTF-8, as in `events.rs`: a mangled
        // path in the log beats not recording that a file action happened.
        let path = record
            .path
            .as_ref()
            .map(|p| p.to_string_lossy().into_owned());
        let (issuer_kind, issuer_name, issuer_window) = issuer_columns(&record.issuer);
        let (outcome, detail) = outcome_columns(&record.outcome);

        self.locked()
            .with_conn(|conn| {
                conn.execute(
                    INSERT_SQL,
                    params![
                        to_unix_ms(at),
                        issuer_kind,
                        issuer_name,
                        issuer_window,
                        record.command.as_str(),
                        record.target_node.as_str(),
                        record.params.as_str(),
                        outcome,
                        detail,
                        path,
                        record.from.map(|ip| ip.to_string()),
                        facts.dispatch.as_deref(),
                        facts.risk.map(RiskLevel::as_str),
                        facts.issuer_session.as_deref(),
                        facts.phase.map(Phase::as_str),
                    ],
                )?;
                Ok(EntryId::from_inner(conn.last_insert_rowid()))
            })
            .map_err(|e| EventsError::Append {
                source: sqlite_source(e),
            })
    }

    /// The timeline, with the four bus columns the panel needs to tell one
    /// dispatch's two rows apart.
    pub fn timeline(&self, query: &Query) -> Result<Vec<TimelineRow>, EventsError> {
        self.timeline_matching(query, None)
    }

    /// Both rows of one dispatch: admission and end. The question the
    /// `events_dispatch` index exists for.
    pub fn dispatch(&self, dispatch: &str) -> Result<Vec<TimelineRow>, EventsError> {
        self.timeline_matching(
            &Query::default().order(Order::OldestFirst),
            Some(dispatch.to_string()),
        )
    }

    fn timeline_matching(
        &self,
        query: &Query,
        dispatch: Option<String>,
    ) -> Result<Vec<TimelineRow>, EventsError> {
        let bounds = Bounds::of(query);
        let sql = format!(
            "SELECT {SELECT_COLUMNS} FROM events {WHERE_SQL} ORDER BY seq {} LIMIT :limit",
            order_sql(query.order)
        );

        let rows = self
            .locked()
            .with_conn(|conn| read_rows(conn, &sql, &bounds, dispatch.as_deref()))
            .map_err(|e| EventsError::Read {
                source: sqlite_source(e),
            })?;

        // Rows become entries out here, because deciding that a row is
        // malformed is this module's error and not rusqlite's — the same
        // split `events.rs` makes, for the same reason.
        rows.into_iter().map(RawRow::into_row).collect()
    }
}

impl Sink for StoreSink {
    /// The plain [`Sink`] append: an entry with no dispatch behind it.
    ///
    /// Leaves the four migration-5 columns NULL, which is why
    /// [`DispatchFacts::none`] is named rather than defaulted — a caller
    /// reaching this method is not a bus dispatch, and a row claiming a phase
    /// it does not have would be worse than one admitting it has none.
    fn append(&self, at: SystemTime, record: &Record) -> Result<EntryId, EventsError> {
        self.append_dispatch(at, record, &DispatchFacts::none())
    }

    fn query(&self, query: &Query) -> Result<Vec<Entry>, EventsError> {
        Ok(self
            .timeline(query)?
            .into_iter()
            .map(|row| row.entry)
            .collect())
    }
}

/// `with_conn` reports everything as a [`StoreError`], and [`EventsError`]
/// names a `rusqlite::Error` as its source. This unwraps the one variant
/// `with_conn` can produce.
fn sqlite_source(e: StoreError) -> rusqlite::Error {
    match e {
        StoreError::Sqlite(e) => e,
        // Not reachable through `with_conn`, which runs one closure against
        // an already-open connection. Carried rather than asserted with
        // `unreachable!`: this runs on the dispatch path, and a panic there
        // would take down a command over a mislabeled error.
        other => rusqlite::Error::ToSqlConversionFailure(Box::new(other)),
    }
}

// ---- encoding, both directions ------------------------------------------
//
// The strings below are the ones `events::RawRow::into_entry` matches on. A
// value this module invents and that reader does not know becomes a
// `MalformedEntry` the first time the panel asks for it, which is why these
// two functions are written as exhaustive matches rather than a `Display`.

fn issuer_columns(issuer: &Issuer) -> (&'static str, Option<&str>, Option<&str>) {
    match issuer {
        Issuer::Operator => ("operator", None, None),
        Issuer::Cli => ("cli", None, None),
        Issuer::System => ("system", None, None),
        Issuer::Agent { session, window } => ("agent", Some(session.as_str()), window.as_deref()),
    }
}

fn outcome_columns(outcome: &Outcome) -> (&'static str, Option<&str>) {
    match outcome {
        Outcome::Ok => ("ok", None),
        Outcome::Queued => ("queued", None),
        Outcome::Refused { reason } => ("refused", Some(reason.as_str())),
        Outcome::Failed { error } => ("failed", Some(error.as_str())),
    }
}

/// Milliseconds, matching the `at_ms` column and `events.rs`'s own
/// conversion. A clock behind the epoch is clamped rather than panicked on:
/// losing the attribution would be the worse failure.
fn to_unix_ms(t: SystemTime) -> i64 {
    match t.duration_since(UNIX_EPOCH) {
        Ok(d) => d.as_millis().min(i64::MAX as u128) as i64,
        Err(_) => 0,
    }
}

fn from_unix_ms(ms: i64) -> SystemTime {
    UNIX_EPOCH + Duration::from_millis(ms.max(0) as u64)
}

/// Interpolated into the SQL, which is safe because it is one of two literals
/// and never caller text.
fn order_sql(order: Order) -> &'static str {
    match order {
        Order::NewestFirst => "DESC",
        Order::OldestFirst => "ASC",
    }
}

// ---- reading ------------------------------------------------------------

/// One timeline row: an [`Entry`] plus the four columns migration 5 added.
///
/// [`Entry`] is `events.rs`'s shape and the shipper's wire format, so the
/// extra columns sit beside it rather than inside it. Flattened on the way
/// out so a panel sees one flat object.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TimelineRow {
    #[serde(flatten)]
    pub entry: Entry,
    /// `None` on an entry that was not a bus dispatch.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dispatch: Option<String>,
    /// The declared risk, as [`RiskLevel::as_str`] wrote it. Kept as text,
    /// like [`crate::store::LoggedCommand`]: a level this build does not know
    /// is a row to show, not a row to refuse.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub risk: Option<String>,
    /// Which signed-in browser, as a [`crate::command::SessionTag`]. Never a
    /// cookie.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub issuer_session: Option<String>,
    /// Which of the dispatch's rows this is. See [`Phase`].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub phase: Option<Phase>,
}

impl TimelineRow {
    /// Whether this row says the command finished, either way. What a panel
    /// needs to answer "is it still running".
    pub fn is_final(&self) -> bool {
        matches!(
            self.phase,
            Some(Phase::Completed | Phase::Failed | Phase::Refused)
        )
    }
}

/// The query's parameters, resolved to the integers SQLite wants. A struct so
/// the borrow of the `Query`'s strings outlives the statement.
struct Bounds<'q> {
    since: i64,
    until: i64,
    after: i64,
    before: i64,
    issuer_kind: Option<&'static str>,
    issuer_name: Option<&'q str>,
    cmd_exact: Option<&'q str>,
    cmd_prefix: Option<String>,
    limit: i64,
}

impl<'q> Bounds<'q> {
    fn of(query: &'q Query) -> Bounds<'q> {
        let (cmd_exact, cmd_prefix) = match &query.command {
            Some(CommandFilter::Exact(id)) => (Some(id.as_str()), None),
            // A namespace matches its own bare id too, so both are set and
            // the SQL ORs them: `files` and everything under `files.`, which
            // is how a grant is written.
            Some(CommandFilter::Namespace(ns)) => (Some(ns.as_str()), Some(format!("{ns}."))),
            None => (None, None),
        };
        let (issuer_kind, issuer_name) = match &query.issuer {
            Some(IssuerFilter::Operator) => (Some("operator"), None),
            Some(IssuerFilter::Cli) => (Some("cli"), None),
            Some(IssuerFilter::System) => (Some("system"), None),
            // `session: None` is every agent, not an agent with no name.
            Some(IssuerFilter::Agent { session }) => (Some("agent"), session.as_deref()),
            None => (None, None),
        };

        Bounds {
            since: query.since.map(to_unix_ms).unwrap_or(i64::MIN),
            until: query.until.map(to_unix_ms).unwrap_or(i64::MAX),
            after: query.after.map(EntryId::inner).unwrap_or(0),
            before: query.before.map(EntryId::inner).unwrap_or(i64::MAX),
            issuer_kind,
            issuer_name,
            cmd_exact,
            cmd_prefix,
            // Clamped, because an unbounded limit from a panel request is how
            // a log with a million rows becomes a memory incident.
            limit: query.limit.clamp(1, crate::events::MAX_LIMIT) as i64,
        }
    }
}

fn read_rows(
    conn: &Connection,
    sql: &str,
    bounds: &Bounds<'_>,
    dispatch: Option<&str>,
) -> rusqlite::Result<Vec<RawRow>> {
    let mut stmt = conn.prepare(sql)?;
    let rows = stmt.query_map(
        named_params! {
            ":since": bounds.since,
            ":until": bounds.until,
            ":after": bounds.after,
            ":before": bounds.before,
            ":issuer_kind": bounds.issuer_kind,
            ":issuer_name": bounds.issuer_name,
            ":dispatch": dispatch,
            ":cmd_exact": bounds.cmd_exact,
            ":cmd_prefix": bounds.cmd_prefix,
            ":limit": bounds.limit,
        },
        |r| {
            Ok(RawRow {
                seq: r.get(0)?,
                at_ms: r.get(1)?,
                issuer_kind: r.get(2)?,
                issuer_name: r.get(3)?,
                issuer_window: r.get(4)?,
                command: r.get(5)?,
                target_node: r.get(6)?,
                params: r.get(7)?,
                outcome: r.get(8)?,
                detail: r.get(9)?,
                path: r.get(10)?,
                from_ip: r.get(11)?,
                dispatch: r.get(12)?,
                risk: r.get(13)?,
                issuer_session: r.get(14)?,
                phase: r.get(15)?,
            })
        },
    )?;
    rows.collect()
}

struct RawRow {
    seq: i64,
    at_ms: i64,
    issuer_kind: String,
    issuer_name: Option<String>,
    issuer_window: Option<String>,
    command: String,
    target_node: String,
    params: String,
    outcome: String,
    detail: Option<String>,
    path: Option<String>,
    from_ip: Option<String>,
    dispatch: Option<String>,
    risk: Option<String>,
    issuer_session: Option<String>,
    phase: Option<String>,
}

impl RawRow {
    /// A row that cannot be understood is an error, not a skipped line: the
    /// worst case is an action that happened and is no longer attributed to
    /// anyone. Same stance as `events::RawRow::into_entry`.
    fn into_row(self) -> Result<TimelineRow, EventsError> {
        let bad = |field: &'static str, value: String| EventsError::MalformedEntry {
            seq: self.seq,
            field,
            value,
        };

        let issuer = match self.issuer_kind.as_str() {
            "operator" => Issuer::Operator,
            "cli" => Issuer::Cli,
            "system" => Issuer::System,
            "agent" => Issuer::Agent {
                session: self
                    .issuer_name
                    .clone()
                    .ok_or_else(|| bad("issuer_name", "NULL".to_string()))?,
                window: self.issuer_window.clone(),
            },
            other => return Err(bad("issuer_kind", other.to_string())),
        };

        let outcome = match self.outcome.as_str() {
            "ok" => Outcome::Ok,
            "queued" => Outcome::Queued,
            "refused" => Outcome::Refused {
                reason: self.detail.clone().unwrap_or_default(),
            },
            "failed" => Outcome::Failed {
                error: self.detail.clone().unwrap_or_default(),
            },
            other => return Err(bad("outcome", other.to_string())),
        };

        // NULL means "not a bus dispatch" and is fine. A value this build
        // does not recognize is not: it would leave the panel unable to tell
        // an admission from a completion, which is the one thing this column
        // was added to make possible, so it is an error rather than a `None`
        // that reads as "no phase recorded".
        let phase = match self.phase.as_deref() {
            None => None,
            Some(text) => Some(Phase::parse(text).ok_or_else(|| bad("phase", text.to_string()))?),
        };

        // An address that no longer parses is bad data but not worth losing
        // the entry over: the rest of the attribution is intact.
        let from = match self.from_ip.as_deref() {
            Some(text) => match text.parse() {
                Ok(ip) => Some(ip),
                Err(_) => {
                    tracing::warn!(seq = self.seq, "audit entry has an unparseable address");
                    None
                }
            },
            None => None,
        };

        Ok(TimelineRow {
            entry: Entry {
                seq: EntryId::from_inner(self.seq),
                at: from_unix_ms(self.at_ms),
                issuer,
                command: self.command,
                target_node: self.target_node,
                // Already a summary, already within `MAX_SUMMARY`: this is
                // the value the writer put in the column, not a re-render.
                params: ParamsSummary::text(self.params),
                outcome,
                path: self.path.map(std::path::PathBuf::from),
                from,
            },
            dispatch: self.dispatch,
            risk: self.risk,
            issuer_session: self.issuer_session,
            phase,
        })
    }
}

// ---- the journal --------------------------------------------------------

/// The bus's journal, writing the `events` table.
///
/// Replaces [`crate::store::StoreJournal`] as the bus's journal. It goes
/// *inside* [`crate::wiring::WatchJournal`], which wraps whatever journal it
/// is given so a burst of refused agent commands can be counted for the
/// lockdown watchdog; that wrapping is what makes the kill switch's
/// "a burst of refusals" trigger real, and nothing here may take its place.
pub struct EventsJournal {
    sink: Arc<StoreSink>,
}

impl EventsJournal {
    /// The journal over the shared store. One line in `main.rs`.
    pub fn new(store: Arc<Mutex<Store>>) -> Arc<EventsJournal> {
        EventsJournal::with_sink(StoreSink::new(store))
    }

    /// The journal over a sink the caller already holds, so the writer and
    /// the read path share one.
    pub fn with_sink(sink: Arc<StoreSink>) -> Arc<EventsJournal> {
        Arc::new(EventsJournal { sink })
    }

    /// The sink, for the read path: pass it to [`register_timeline`] so the
    /// panel reads the log this journal writes rather than a second handle to
    /// the same table.
    pub fn sink(&self) -> Arc<StoreSink> {
        Arc::clone(&self.sink)
    }
}

impl Journal for EventsJournal {
    fn record(&self, entry: LogEntry) -> Result<(), JournalError> {
        let (record, facts) = translate(&entry);
        self.sink
            .append_dispatch(entry.at, &record, &facts)
            .map(|_| ())
            // Stringified because `JournalError` is a string: the bus decides
            // what to do with a failed write (refuse an admission, let a
            // completion through) from the fact that it failed, not from
            // which error it was.
            .map_err(|e| JournalError(e.to_string()))
    }
}

/// [`LogEntry`] as an audit entry. The whole job of this module.
fn translate(entry: &LogEntry) -> (Record, DispatchFacts) {
    let detail = || entry.detail.clone().unwrap_or_default();

    // `Ran` and `Completed` both land on `Outcome::Ok`, which is correct and
    // is exactly why `phase` exists: admission is not failure, and completion
    // is not a second admission. Without the phase these two rows are
    // identical and the log shows every command twice.
    let outcome = match entry.decision {
        Decision::Ran | Decision::Completed => Outcome::Ok,
        Decision::Failed => Outcome::failed(detail()),
        Decision::Refused => Outcome::refused(detail()),
        // Both are "nothing has run and a person has to answer first". The
        // Inbox and the confirmation dialog are different people-shaped
        // gates, and `phase` is what tells them apart; `Outcome::Queued` is
        // the honest outcome for both, and it is deliberately not a failure.
        Decision::Queued | Decision::AwaitingConfirmation => Outcome::Queued,
    };

    let record = Record::new(
        issuer(&entry.issuer),
        entry.command.as_str(),
        entry.node.as_str(),
        outcome,
    )
    .params(params_summary(entry));
    // `path` is left unset. `events` has the column and `LogEntry` does not
    // carry one: the bus sees a command id and a JSON blob, not the file a
    // handler went on to touch. It waits for the file commands to report the
    // path they acted on back to the dispatch — `files.rs` knows it, the bus
    // does not. An empty column reads as "not recorded", where deriving one
    // from a `params` field named `path` would read as evidence and be wrong
    // for every command that resolves, follows or rejects the path it was
    // given.
    //
    // `from` is left unset for the same reason: the client address is on the
    // HTTP request, and `Issuer` carries a session tag rather than a
    // connection. It waits for the bus to carry the source address through a
    // dispatch.

    let facts = DispatchFacts {
        dispatch: Some(entry.dispatch.to_string()),
        risk: entry.risk,
        // The TAG, never the cookie. `SessionTag` is 16 hex characters of
        // SHA-256 over the session id and nothing about the id is recoverable
        // from it, which is what makes an audit log that is readable from the
        // terminal and shipped off the node safe to write at all.
        issuer_session: entry
            .issuer
            .browser_session()
            .map(|browser| browser.tag().as_str().to_string()),
        phase: Some(Phase::of(entry.decision)),
    };

    (record, facts)
}

/// Who issued it.
///
/// [`Issuer::System`] — Deckjack itself, a reminder firing, lockdown — is
/// left unproduced: [`crate::command::Issuer`] has no variant for it, and the
/// server's own actions reach the bus as `Issuer::operator()`, an operator
/// with no browser behind them. Inventing `System` from "operator with no
/// session" would mislabel every CLI-shaped dispatch the server makes on my
/// behalf. It waits for the bus to distinguish the two.
fn issuer(issuer: &BusIssuer) -> Issuer {
    match issuer {
        BusIssuer::Operator { .. } => Issuer::Operator,
        // `window` stays `None`: `events::Issuer::Agent` carries the invited
        // window so the log can read "Assistant via window 3", and
        // `command::Issuer::Agent` carries only the session name. It waits
        // for window invitations to reach the bus's issuer.
        BusIssuer::Agent { session } => Issuer::agent(session.as_str()),
        BusIssuer::Cli => Issuer::Cli,
    }
}

/// The parameters, as the bus handed them over.
///
/// **Not re-derived.** `LogEntry::params` is already what the bus decided to
/// write down: `Some("<redacted>")` for a command whose spec sets
/// `REDACT_PARAMS`, and `None` for a command with no spec at all, where
/// nothing says the parameters are safe to keep. Re-reading the original
/// parameters here would undo both decisions — a previous defect in this
/// project logged a password prompt's keystrokes — and this module has no
/// access to them anyway.
///
/// [`ParamsSummary::from_json`] is still what turns the value into a column,
/// for two reasons that both only narrow it: it bounds a megabyte of document
/// to [`ParamsSummary`]'s summary length, and it drops secret-looking fields
/// from a command that does *not* redact wholesale. It cannot widen anything
/// — the string `"<redacted>"` summarizes to itself.
fn params_summary(entry: &LogEntry) -> ParamsSummary {
    match &entry.params {
        Some(value) => ParamsSummary::from_json(value),
        None => ParamsSummary::empty(),
    }
}

// ---- the read path on the bus -------------------------------------------

/// `system.timeline`: the audit log back, by time range, by issuer and by
/// command.
///
/// [`RiskLevel::ReadOnly`] — it reads a table and changes nothing, so the
/// operator is not asked to confirm it and a panel can call it on every
/// repaint. That is a statement about confirmation and not about access: what
/// keeps an agent out of the audit log is its grant, which has to name
/// `system.timeline` before an agent session can call it at all.
pub struct Timeline {
    sink: Arc<StoreSink>,
}

/// What the timeline panel asks for. Every field is optional; the default is
/// the most recent page, newest first.
#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
#[serde(default)]
pub struct TimelineParams {
    /// Milliseconds since the epoch, matching the `at_ms` column and the
    /// timestamps entries serialize with, so a panel can pass back what it
    /// was given.
    pub since_ms: Option<i64>,
    pub until_ms: Option<i64>,
    /// `operator`, `agent`, `cli` or `system`.
    pub issuer: Option<String>,
    /// One agent session by name. Ignored unless `issuer` is `agent`, because
    /// only an agent has a session name in this table.
    pub session: Option<String>,
    /// A command id, or a namespace when `namespace` is set.
    pub command: Option<String>,
    /// Match `command` as a namespace: `files` covers `files` and everything
    /// under `files.`, which is how a grant is written, so the panel can show
    /// "everything this grant covered".
    pub namespace: bool,
    /// Both rows of one dispatch, by [`crate::command::DispatchId`].
    pub dispatch: Option<String>,
    /// The sequence of the oldest entry already shown: the panel's cursor.
    /// Paging on a time would either repeat or skip entries that shared a
    /// millisecond.
    pub before: Option<i64>,
    pub limit: Option<usize>,
    /// Oldest first. For reading a dispatch forwards rather than filling a
    /// panel.
    pub oldest_first: bool,
}

impl Command for Timeline {
    const ID: &'static str = "system.timeline";
    const TITLE: &'static str = "Command timeline";
    const RISK: RiskLevel = RiskLevel::ReadOnly;

    type Params = TimelineParams;
    type Output = Vec<TimelineRow>;

    async fn run(
        &self,
        params: TimelineParams,
        _cx: Context,
    ) -> Result<Vec<TimelineRow>, CommandError> {
        let mut query = Query::default();
        if let Some(ms) = params.since_ms {
            query = query.since(from_unix_ms(ms));
        }
        if let Some(ms) = params.until_ms {
            query = query.until(from_unix_ms(ms));
        }
        if let Some(kind) = params.issuer.as_deref() {
            query = query.issuer(issuer_filter(kind, params.session.clone())?);
        }
        if let Some(command) = params.command.clone() {
            query = query.command(if params.namespace {
                CommandFilter::namespace(command)
            } else {
                CommandFilter::exact(command)
            });
        }
        if let Some(seq) = params.before {
            query = query.before(EntryId::from_inner(seq));
        }
        if let Some(limit) = params.limit {
            query = query.limit(limit);
        }
        if params.oldest_first {
            query = query.order(Order::OldestFirst);
        }

        // No await between the lock and here: `timeline` takes the store's
        // mutex for one statement and the guard never crosses an await point,
        // which is what keeps this future `Send`.
        self.sink
            .timeline_matching(&query, params.dispatch.clone())
            .map_err(|e| CommandError::Failed(anyhow::anyhow!(e.to_string())))
    }
}

/// An issuer named on the wire. Rejected rather than ignored: a panel asking
/// for `agant` and silently being shown everyone's commands is a worse answer
/// than an error.
fn issuer_filter(kind: &str, session: Option<String>) -> Result<IssuerFilter, CommandError> {
    match kind {
        "operator" => Ok(IssuerFilter::Operator),
        "cli" => Ok(IssuerFilter::Cli),
        "system" => Ok(IssuerFilter::System),
        "agent" => Ok(IssuerFilter::Agent { session }),
        other => Err(CommandError::Failed(anyhow::anyhow!(
            "{other:?} is not an issuer: expected operator, agent, cli or system"
        ))),
    }
}

/// Put the timeline command on the bus. One line in `main.rs`.
pub fn register_timeline(registry: &Registry, sink: Arc<StoreSink>) -> Result<(), RegistryError> {
    registry.register(Timeline { sink })?;
    Ok(())
}
