//! The attribution log.
//!
//! Every command the bus runs lands here: when it ran, who issued it (me, a
//! named agent session, the CLI, the server itself), which command, on which
//! node, a redacted summary of its parameters, how it ended, and for file
//! actions the path. The timeline panel reads it back by time range, by
//! issuer or by command. See docs/DESIGN.md, "Agents" (Attribution), "Host
//! access" (Audit log) and "Lockdown".
//!
//! **What "append-only" means here, and what it does not.**
//! The guarantee this module can actually make is about the API: [`Log`] has
//! one mutating method, [`Log::append`], and no caller can update a row,
//! delete a row or choose an entry's timestamp — the log stamps the time
//! itself from its injected [`Clock`]. The migration adds `BEFORE UPDATE` and
//! `BEFORE DELETE` triggers that abort, so even hand-written SQL through
//! another connection is refused, and `AUTOINCREMENT` means a removed row
//! leaves a visible gap in `seq` rather than being papered over by a reused
//! rowid.
//!
//! None of that is tamper-proofing. `state.db` is a file owned by my uid, and
//! the terminal is a documented escape hatch: anyone with a shell as me can
//! `DROP TRIGGER`, rewrite rows or delete the file. Append-only in SQLite is
//! a convention that protects against accident and against bugs in Deckjack
//! itself, not against an attacker who already owns the host. The design's
//! answer to that attacker is to get the entries off the machine — "the audit
//! log ships to the VPS as written so a host attacker can't erase it" — which
//! is a shipper on top of this module, not something the module can do alone.
//! [`Log::after`] exists to feed it: a watermark plus everything since.
//!
//! ```ignore
//! use deckjack_core::events::{Issuer, Log, Outcome, ParamsSummary, Query, Record};
//!
//! let log = Log::sqlite(store.path())?;
//!
//! log.append(
//!     Record::new(
//!         Issuer::Operator,
//!         "files.delete",
//!         node_id.clone(),
//!         Outcome::Ok,
//!     )
//!     .params(ParamsSummary::from_json(&params))
//!     .path("/home/me/notes/old.md")
//!     .from(client_ip),
//! )?;
//!
//! // What the timeline panel asks for: the last hour, newest first.
//! let recent = log.timeline(&Query::default().since(an_hour_ago))?;
//! ```

use std::fmt;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, named_params, params};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use thiserror::Error;

/// Most entries one query may return. A timeline panel scrolls; it does not
/// need the whole log in one response, and an unbounded limit from a panel
/// request is how a log with a million rows becomes a memory incident.
pub const MAX_LIMIT: usize = 1_000;

/// What [`Query::default`] asks for: enough to fill a panel.
pub const DEFAULT_LIMIT: usize = 200;

// ---- time ---------------------------------------------------------------

/// The clock, supplied rather than read, so the log can be driven to a known
/// time in a test without sleeping. Same reason as [`crate::lock::Env`].
pub trait Clock: Send + Sync {
    fn now(&self) -> SystemTime;
}

/// The real one.
pub struct OsClock;

impl Clock for OsClock {
    fn now(&self) -> SystemTime {
        SystemTime::now()
    }
}

/// Milliseconds, not the seconds the rest of the state file uses. A command
/// log routinely records several entries inside one second, and a timeline
/// that shows them all at the same instant is a worse log. The column is
/// named `at_ms` so the unit cannot be misread at the SQL prompt.
fn to_unix_ms(t: SystemTime) -> i64 {
    match t.duration_since(UNIX_EPOCH) {
        Ok(d) => d.as_millis().min(i64::MAX as u128) as i64,
        // A clock behind the epoch is not a time we expect. Clamping keeps
        // the entry rather than losing the attribution to a panic.
        Err(_) => 0,
    }
}

fn from_unix_ms(ms: i64) -> SystemTime {
    UNIX_EPOCH + Duration::from_millis(ms.max(0) as u64)
}

/// Entries cross to the browser and to the VPS as JSON, where a millisecond
/// integer is what every client already understands. `SystemTime`'s own serde
/// shape is a two-field struct, which no panel wants to parse.
mod unix_ms {
    use super::{Deserialize, Deserializer, Serializer, SystemTime, from_unix_ms, to_unix_ms};

    pub fn serialize<S: Serializer>(t: &SystemTime, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_i64(to_unix_ms(*t))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<SystemTime, D::Error> {
        Ok(from_unix_ms(i64::deserialize(d)?))
    }
}

// ---- who ----------------------------------------------------------------

/// Who issued a command.
///
/// Deliberately not a session id. The log ships off the node, so anything in
/// it must be safe to hold somewhere less trusted than the host; a session id
/// is a bearer credential and belongs only in the `sessions` table. The
/// client address is recorded separately ([`Record::from`]) because it is
/// evidence rather than a key.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Issuer {
    /// Me, acting directly from a signed-in client.
    Operator,
    /// A named agent session. `window` carries the invited window when the
    /// agent is working inside one, so the log reads "Assistant via window
    /// 3" exactly as the design requires.
    Agent {
        session: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        window: Option<String>,
    },
    /// The local CLI, over the control socket.
    Cli,
    /// Deckjack itself: a reminder firing, a sync service, lockdown.
    System,
    /// An installed app's window, calling through the shell on the
    /// operator's browser. Recorded as `app-ui:<app>` so a timeline can tell
    /// an app's frame from the shell (docs/APP-UI-ISOLATION.md §4.6); which
    /// browser is in the row's session tag, as for the operator.
    AppUi { app: String },
}

impl Issuer {
    /// A named agent session, by name.
    pub fn agent(session: impl Into<String>) -> Issuer {
        Issuer::Agent {
            session: session.into(),
            window: None,
        }
    }

    /// An agent session acting inside a window I invited it into.
    pub fn agent_via_window(session: impl Into<String>, window: impl Into<String>) -> Issuer {
        Issuer::Agent {
            session: session.into(),
            window: Some(window.into()),
        }
    }

    /// How the timeline panel names this issuer. The design's own words.
    pub fn label(&self) -> String {
        match self {
            Issuer::Operator => "me".to_string(),
            Issuer::Agent {
                session,
                window: None,
            } => session.clone(),
            Issuer::Agent {
                session,
                window: Some(w),
            } => format!("{session} via window {w}"),
            Issuer::Cli => "the CLI".to_string(),
            Issuer::System => "Deckjack".to_string(),
            Issuer::AppUi { app } => format!("app-ui:{app}"),
        }
    }

    fn kind_str(&self) -> &'static str {
        match self {
            Issuer::Operator => "operator",
            Issuer::Agent { .. } => "agent",
            Issuer::Cli => "cli",
            Issuer::System => "system",
            Issuer::AppUi { .. } => "app_ui",
        }
    }

    fn name(&self) -> Option<&str> {
        match self {
            Issuer::Agent { session, .. } => Some(session.as_str()),
            Issuer::AppUi { app } => Some(app.as_str()),
            _ => None,
        }
    }

    fn window(&self) -> Option<&str> {
        match self {
            Issuer::Agent { window, .. } => window.as_deref(),
            _ => None,
        }
    }
}

// ---- how it ended -------------------------------------------------------

/// How a command ended. `Queued` is not a failure: it is the Inbox gate doing
/// its job, and it is recorded so the timeline shows the gap between an agent
/// asking and me approving.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum Outcome {
    Ok,
    /// Outside the session's grant, or refused by a risk rule.
    Refused {
        reason: String,
    },
    /// Waiting for approval in the Assistant's Inbox.
    Queued,
    /// Ran and failed.
    Failed {
        error: String,
    },
}

impl Outcome {
    pub fn refused(reason: impl Into<String>) -> Outcome {
        Outcome::Refused {
            reason: reason.into(),
        }
    }

    pub fn failed(error: impl Into<String>) -> Outcome {
        Outcome::Failed {
            error: error.into(),
        }
    }

    fn kind_str(&self) -> &'static str {
        match self {
            Outcome::Ok => "ok",
            Outcome::Refused { .. } => "refused",
            Outcome::Queued => "queued",
            Outcome::Failed { .. } => "failed",
        }
    }

    /// The one free-text column, shared by the two outcomes that carry a
    /// message, so the schema does not grow a column per variant.
    fn detail(&self) -> Option<&str> {
        match self {
            Outcome::Refused { reason } => Some(reason.as_str()),
            Outcome::Failed { error } => Some(error.as_str()),
            Outcome::Ok | Outcome::Queued => None,
        }
    }
}

// ---- parameters ---------------------------------------------------------

/// A short, redacted rendering of a command's parameters.
///
/// A summary rather than the parameters themselves for two reasons: entries
/// leave the node, and a command's parameters can be a megabyte of document.
/// [`ParamsSummary::from_json`] is the path every bus command should take,
/// because it is the one that redacts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ParamsSummary(String);

/// Longest summary kept. Past this the tail is dropped with an ellipsis: a
/// truncated summary still attributes the action, which is the job.
const MAX_SUMMARY: usize = 400;

/// Longest single string value kept inside a summary.
const MAX_VALUE: usize = 80;

/// Array elements kept inside a summary. A bulk delete's interesting fact is
/// the count, not the three-thousandth path.
const MAX_ITEMS: usize = 8;

/// Substrings in a parameter name that make its value a secret.
///
/// Matched as substrings on the lowercased name, which over-redacts: a
/// parameter called `keyboard_layout` loses its value. That is the right
/// direction to be wrong in, since the alternative is a passphrase sitting in
/// a log file that gets copied to a VPS.
const SECRET_HINTS: &[&str] = &[
    "password",
    "passphrase",
    "secret",
    "token",
    "code",
    "key",
    "cookie",
    "credential",
    "authorization",
    "session",
];

const REDACTED: &str = "<redacted>";

impl ParamsSummary {
    /// No parameters worth recording.
    pub fn empty() -> ParamsSummary {
        ParamsSummary(String::new())
    }

    /// A summary the caller has already rendered and already redacted. The
    /// caller owns that promise; prefer [`ParamsSummary::from_json`].
    pub fn text(text: impl Into<String>) -> ParamsSummary {
        ParamsSummary(truncate(&text.into(), MAX_SUMMARY))
    }

    /// Summarize a command's JSON parameters, dropping secret-looking values
    /// and trimming anything long.
    pub fn from_json(value: &serde_json::Value) -> ParamsSummary {
        let redacted = redact(value, 0);
        ParamsSummary(truncate(&redacted.to_string(), MAX_SUMMARY))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl fmt::Display for ParamsSummary {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// How deep into nested parameters the summary goes before collapsing an
/// object to its shape. Depth, not size, is what makes a summary unreadable.
const MAX_DEPTH: usize = 3;

fn redact(value: &serde_json::Value, depth: usize) -> serde_json::Value {
    use serde_json::Value;

    match value {
        Value::String(s) => Value::String(truncate(s, MAX_VALUE)),
        Value::Array(items) => {
            if depth >= MAX_DEPTH {
                return Value::String(format!("[{} items]", items.len()));
            }
            let mut out: Vec<Value> = items
                .iter()
                .take(MAX_ITEMS)
                .map(|v| redact(v, depth + 1))
                .collect();
            if items.len() > MAX_ITEMS {
                out.push(Value::String(format!("+{} more", items.len() - MAX_ITEMS)));
            }
            Value::Array(out)
        }
        Value::Object(fields) => {
            if depth >= MAX_DEPTH {
                return Value::String(format!("{{{} fields}}", fields.len()));
            }
            let mut out = serde_json::Map::new();
            for (name, v) in fields {
                let lower = name.to_lowercase();
                if SECRET_HINTS.iter().any(|hint| lower.contains(hint)) {
                    out.insert(name.clone(), Value::String(REDACTED.to_string()));
                } else {
                    out.insert(name.clone(), redact(v, depth + 1));
                }
            }
            Value::Object(out)
        }
        other => other.clone(),
    }
}

/// Truncate on a character boundary, because cutting a multi-byte character
/// in half would panic on a path with an accent in it.
fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let kept: String = s.chars().take(max).collect();
    format!("{kept}…")
}

// ---- entries ------------------------------------------------------------

/// A log entry's place in the log. Monotonic and never reused, so it doubles
/// as the watermark a shipper keeps and as evidence: a gap means a row went
/// missing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct EntryId(i64);

impl EntryId {
    /// Before the first entry. `seq` starts at 1, so nothing is below this.
    pub const START: EntryId = EntryId(0);

    pub fn inner(self) -> i64 {
        self.0
    }

    pub fn from_inner(v: i64) -> EntryId {
        EntryId(v)
    }
}

impl fmt::Display for EntryId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// What a caller hands the log. There is no `at` field: the log stamps the
/// time from its own clock, so no caller can backdate an entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Record {
    pub issuer: Issuer,
    /// The command's id on the bus, for example `files.delete`.
    ///
    /// A `String` because the bus's own `CommandId` type does not exist yet.
    /// TODO: take the bus's id type once `commands`/`bus` lands, so a typo in
    /// a command id is a compile error rather than an unqueryable log row.
    pub command: String,
    /// The node that ran it. See [`crate::node::id`].
    pub target_node: String,
    pub params: ParamsSummary,
    pub outcome: Outcome,
    /// Set for file actions, which the design calls out specifically: paths
    /// touched outside the working directory are the ones worth auditing.
    pub path: Option<PathBuf>,
    /// Where the command came in from, when it came over the network. `None`
    /// for the CLI and for the server's own actions.
    pub from: Option<IpAddr>,
}

impl Record {
    pub fn new(
        issuer: Issuer,
        command: impl Into<String>,
        target_node: impl Into<String>,
        outcome: Outcome,
    ) -> Record {
        Record {
            issuer,
            command: command.into(),
            target_node: target_node.into(),
            params: ParamsSummary::empty(),
            // Taken by value rather than defaulted to `Ok` and set later: a
            // forgotten builder call must not be able to log a failed
            // command as a successful one.
            outcome,
            path: None,
            from: None,
        }
    }

    pub fn params(mut self, params: ParamsSummary) -> Record {
        self.params = params;
        self
    }

    pub fn path(mut self, path: impl Into<PathBuf>) -> Record {
        self.path = Some(path.into());
        self
    }

    pub fn from(mut self, ip: IpAddr) -> Record {
        self.from = Some(ip);
        self
    }
}

/// A record as the log holds it: stamped and numbered.
///
/// `Deserialize` as well as `Serialize`, because the VPS end of the shipper
/// has to read entries back out of JSON to store them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    pub seq: EntryId,
    #[serde(with = "unix_ms")]
    pub at: SystemTime,
    pub issuer: Issuer,
    pub command: String,
    pub target_node: String,
    pub params: ParamsSummary,
    #[serde(flatten)]
    pub outcome: Outcome,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<PathBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub from: Option<IpAddr>,
}

// ---- queries ------------------------------------------------------------

/// Which issuers a query wants. `Agent { session: None }` is every agent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IssuerFilter {
    Operator,
    Agent {
        session: Option<String>,
    },
    Cli,
    System,
    /// App windows: one app's when `app` is set, every app's otherwise.
    AppUi {
        app: Option<String>,
    },
}

impl IssuerFilter {
    /// Every agent session, rather than one named one.
    pub fn any_agent() -> IssuerFilter {
        IssuerFilter::Agent { session: None }
    }

    pub fn agent(session: impl Into<String>) -> IssuerFilter {
        IssuerFilter::Agent {
            session: Some(session.into()),
        }
    }

    fn kind_str(&self) -> &'static str {
        match self {
            IssuerFilter::Operator => "operator",
            IssuerFilter::Agent { .. } => "agent",
            IssuerFilter::Cli => "cli",
            IssuerFilter::System => "system",
            IssuerFilter::AppUi { .. } => "app_ui",
        }
    }

    fn name(&self) -> Option<&str> {
        match self {
            IssuerFilter::Agent { session } => session.as_deref(),
            IssuerFilter::AppUi { app } => app.as_deref(),
            _ => None,
        }
    }
}

/// One command, or a namespace of them. `Namespace("files")` matches `files`
/// and everything under `files.`, which is how grants are written, so the
/// panel can show "everything this grant covered".
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommandFilter {
    Exact(String),
    Namespace(String),
}

impl CommandFilter {
    pub fn exact(id: impl Into<String>) -> CommandFilter {
        CommandFilter::Exact(id.into())
    }

    pub fn namespace(ns: impl Into<String>) -> CommandFilter {
        CommandFilter::Namespace(ns.into())
    }

    /// `(exact id, prefix)`. A namespace matches its own bare id too, so both
    /// are set and the SQL ORs them.
    fn sql_parts(&self) -> (&str, Option<String>) {
        match self {
            CommandFilter::Exact(id) => (id.as_str(), None),
            CommandFilter::Namespace(ns) => (ns.as_str(), Some(format!("{ns}."))),
        }
    }
}

/// Newest first for a panel; oldest first for a shipper working through a
/// backlog.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Order {
    NewestFirst,
    OldestFirst,
}

impl Order {
    /// Interpolated into the SQL, which is safe because it is one of two
    /// literals and never caller text.
    fn sql(self) -> &'static str {
        match self {
            Order::NewestFirst => "DESC",
            Order::OldestFirst => "ASC",
        }
    }
}

/// What the timeline panel asks for. Every field is optional; the defaults
/// are "the most recent [`DEFAULT_LIMIT`] entries".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Query {
    pub since: Option<SystemTime>,
    pub until: Option<SystemTime>,
    pub issuer: Option<IssuerFilter>,
    pub command: Option<CommandFilter>,
    /// Only entries with a higher sequence than this one: the shipper's
    /// watermark, read oldest first.
    pub after: Option<EntryId>,
    /// Only entries with a lower sequence than this one: the timeline
    /// panel's cursor, read newest first.
    ///
    /// Both ends exist because a cursor has to be a sequence number rather
    /// than a time. A panel scrolling back cannot page on [`Query::until`]:
    /// several commands land in the same millisecond, so a timestamp cursor
    /// either shows the same entries again or steps over the ones that
    /// shared the instant. And it cannot page on [`Query::after`] either,
    /// which moves toward the newest entry while the panel is moving away
    /// from it.
    pub before: Option<EntryId>,
    pub limit: usize,
    pub order: Order,
}

impl Default for Query {
    fn default() -> Query {
        Query {
            since: None,
            until: None,
            issuer: None,
            command: None,
            after: None,
            before: None,
            limit: DEFAULT_LIMIT,
            order: Order::NewestFirst,
        }
    }
}

impl Query {
    pub fn since(mut self, t: SystemTime) -> Query {
        self.since = Some(t);
        self
    }

    pub fn until(mut self, t: SystemTime) -> Query {
        self.until = Some(t);
        self
    }

    pub fn range(self, from: SystemTime, to: SystemTime) -> Query {
        self.since(from).until(to)
    }

    pub fn issuer(mut self, f: IssuerFilter) -> Query {
        self.issuer = Some(f);
        self
    }

    pub fn command(mut self, f: CommandFilter) -> Query {
        self.command = Some(f);
        self
    }

    pub fn after(mut self, seq: EntryId) -> Query {
        self.after = Some(seq);
        self
    }

    /// The page of entries older than `seq`. What the panel passes to scroll
    /// back: the sequence of the oldest entry it is already showing.
    pub fn before(mut self, seq: EntryId) -> Query {
        self.before = Some(seq);
        self
    }

    pub fn limit(mut self, n: usize) -> Query {
        self.limit = n;
        self
    }

    pub fn order(mut self, order: Order) -> Query {
        self.order = order;
        self
    }

    fn capped_limit(&self) -> i64 {
        self.limit.clamp(1, MAX_LIMIT) as i64
    }
}

// ---- errors -------------------------------------------------------------

#[derive(Debug, Error)]
pub enum EventsError {
    #[error("cannot open the audit log in {path}: {source}")]
    Open {
        path: PathBuf,
        #[source]
        source: rusqlite::Error,
    },

    #[error(
        "{path} has no `events` table: this build's audit log migration has not been applied; \
         start the server once so migrations run, and if that does not create it the migration \
         is missing from store.rs"
    )]
    NotMigrated { path: PathBuf },

    #[error(
        "cannot write the audit log ({source}); a command that cannot be attributed should be \
         refused rather than run unlogged, so treat this as fatal for anything above read-only"
    )]
    Append {
        #[source]
        source: rusqlite::Error,
    },

    #[error("cannot read the audit log: {source}")]
    Read {
        #[source]
        source: rusqlite::Error,
    },

    #[error(
        "audit log entry {seq} is unreadable ({field} = {value:?}); it was written by another \
         build or edited by hand — preserve the database and investigate before writing more"
    )]
    MalformedEntry {
        seq: i64,
        field: &'static str,
        value: String,
    },
}

// ---- storage boundary ---------------------------------------------------

/// Where entries actually go.
///
/// A trait so the log can be pointed at something other than this node's
/// `state.db` — the VPS side of the shipper is the obvious second
/// implementation — and so the rest of the system holds a [`Log`] rather than
/// a database handle.
pub trait Sink: Send + Sync {
    /// Store one entry and return its number. Implementations must never
    /// modify an existing row.
    fn append(&self, at: SystemTime, record: &Record) -> Result<EntryId, EventsError>;

    fn query(&self, query: &Query) -> Result<Vec<Entry>, EventsError>;
}

const INSERT_SQL: &str = "INSERT INTO events (\
    at_ms, issuer_kind, issuer_name, issuer_window, command, target_node, \
    params, outcome, detail, path, from_ip) \
    VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)";

const SELECT_COLUMNS: &str = "seq, at_ms, issuer_kind, issuer_name, issuer_window, \
    command, target_node, params, outcome, detail, path, from_ip";

/// One statement with every filter written as "no filter, or it matches", so
/// the SQL is a constant and nothing is ever concatenated from caller input.
/// The cost is that SQLite cannot always pick the narrowest index; at the size
/// of one person's command log that is not worth a query builder.
const WHERE_SQL: &str = "WHERE at_ms >= :since AND at_ms <= :until \
    AND seq > :after AND seq < :before \
    AND (:issuer_kind IS NULL OR issuer_kind = :issuer_kind) \
    AND (:issuer_name IS NULL OR issuer_name = :issuer_name) \
    AND (:cmd_exact IS NULL \
         OR command = :cmd_exact \
         OR (:cmd_prefix IS NOT NULL \
             AND substr(command, 1, length(:cmd_prefix)) = :cmd_prefix))";

/// The audit log in this node's `state.db`.
///
/// It opens its own connection to the same file rather than borrowing the
/// one inside [`crate::store::Store`]. Two reasons: appending an entry must
/// not queue behind whatever else holds the store's mutex, and WAL mode
/// already supports many connections to one database. The writer is still
/// serialized by SQLite itself, which is what `busy_timeout` is for.
pub struct SqliteSink {
    conn: Mutex<Connection>,
    path: PathBuf,
}

/// How long a writer waits for another connection's transaction before giving
/// up. Appends are single statements, so contention is brief; the timeout only
/// has to outlast a checkpoint.
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

impl SqliteSink {
    /// Open the log in an existing state database. Pass [`crate::store::Store::path`].
    ///
    /// The table itself is created by the store's migrations, not here: one
    /// place owns the schema, and a log that quietly created its own table
    /// would hide a missing migration.
    pub fn open(db_path: &Path) -> Result<SqliteSink, EventsError> {
        let conn = Connection::open(db_path).map_err(|source| EventsError::Open {
            path: db_path.to_path_buf(),
            source,
        })?;

        let open_err = |source: rusqlite::Error| EventsError::Open {
            path: db_path.to_path_buf(),
            source,
        };
        conn.pragma_update(None, "journal_mode", "WAL")
            .map_err(open_err)?;
        // The entry must survive the crash that the entry is about.
        conn.pragma_update(None, "synchronous", "FULL")
            .map_err(open_err)?;
        conn.busy_timeout(BUSY_TIMEOUT).map_err(open_err)?;

        let exists: bool = conn
            .query_row(
                "SELECT count(*) > 0 FROM sqlite_schema WHERE type = 'table' AND name = 'events'",
                [],
                |r| r.get(0),
            )
            .map_err(open_err)?;
        if !exists {
            return Err(EventsError::NotMigrated {
                path: db_path.to_path_buf(),
            });
        }

        Ok(SqliteSink {
            conn: Mutex::new(conn),
            path: db_path.to_path_buf(),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn with_conn<T>(&self, f: impl FnOnce(&Connection) -> T) -> T {
        // Appends are single statements that leave the connection consistent
        // however a previous holder died, so a poisoned mutex is still usable
        // — the same reasoning as `http::Ctx`.
        f(&self.conn.lock().unwrap_or_else(PoisonError::into_inner))
    }
}

impl Sink for SqliteSink {
    fn append(&self, at: SystemTime, record: &Record) -> Result<EntryId, EventsError> {
        // Lossy on a path that is not UTF-8. A mangled path in the log beats
        // refusing to record that a file action happened at all.
        let path = record
            .path
            .as_ref()
            .map(|p| p.to_string_lossy().into_owned());

        self.with_conn(|conn| {
            conn.execute(
                INSERT_SQL,
                params![
                    to_unix_ms(at),
                    record.issuer.kind_str(),
                    record.issuer.name(),
                    record.issuer.window(),
                    record.command.as_str(),
                    record.target_node.as_str(),
                    record.params.as_str(),
                    record.outcome.kind_str(),
                    record.outcome.detail(),
                    path,
                    record.from.map(|ip| ip.to_string()),
                ],
            )
            .map_err(|source| EventsError::Append { source })?;
            Ok(EntryId(conn.last_insert_rowid()))
        })
    }

    fn query(&self, query: &Query) -> Result<Vec<Entry>, EventsError> {
        let since = query.since.map(to_unix_ms).unwrap_or(i64::MIN);
        let until = query.until.map(to_unix_ms).unwrap_or(i64::MAX);
        let after = query.after.map(EntryId::inner).unwrap_or(0);
        let before = query.before.map(EntryId::inner).unwrap_or(i64::MAX);
        let issuer_kind = query.issuer.as_ref().map(IssuerFilter::kind_str);
        let issuer_name = query.issuer.as_ref().and_then(IssuerFilter::name);
        let (cmd_exact, cmd_prefix) = match &query.command {
            Some(f) => {
                let (exact, prefix) = f.sql_parts();
                (Some(exact), prefix)
            }
            None => (None, None),
        };
        let limit = query.capped_limit();

        let sql = format!(
            "SELECT {SELECT_COLUMNS} FROM events {WHERE_SQL} ORDER BY seq {} LIMIT :limit",
            query.order.sql()
        );

        self.with_conn(|conn| {
            let mut stmt = conn
                .prepare(&sql)
                .map_err(|source| EventsError::Read { source })?;

            // Rows come back as plain columns and are turned into entries
            // outside the closure, because deciding that a row is malformed
            // is this module's error, not rusqlite's.
            let rows = stmt
                .query_map(
                    named_params! {
                        ":since": since,
                        ":until": until,
                        ":after": after,
                        ":before": before,
                        ":issuer_kind": issuer_kind,
                        ":issuer_name": issuer_name,
                        ":cmd_exact": cmd_exact,
                        ":cmd_prefix": cmd_prefix,
                        ":limit": limit,
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
                        })
                    },
                )
                .map_err(|source| EventsError::Read { source })?;

            let mut out = Vec::new();
            for row in rows {
                let row = row.map_err(|source| EventsError::Read { source })?;
                out.push(row.into_entry()?);
            }
            Ok(out)
        })
    }
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
}

impl RawRow {
    /// A row that cannot be understood is an error, not a skipped line.
    /// `store::live_sessions` drops unreadable rows because the worst case
    /// there is signing in again; here the worst case is an action that
    /// happened and is no longer attributed to anyone.
    fn into_entry(self) -> Result<Entry, EventsError> {
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
            "app_ui" => Issuer::AppUi {
                app: self
                    .issuer_name
                    .clone()
                    .ok_or_else(|| bad("issuer_name", "NULL".to_string()))?,
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

        // A client address that no longer parses is bad data, but it is not
        // worth losing the entry over: the rest of the attribution is intact.
        let from = match self.from_ip.as_deref() {
            Some(text) => match text.parse::<IpAddr>() {
                Ok(ip) => Some(ip),
                Err(_) => {
                    tracing::warn!(seq = self.seq, "audit entry has an unparseable address");
                    None
                }
            },
            None => None,
        };

        Ok(Entry {
            seq: EntryId(self.seq),
            at: from_unix_ms(self.at_ms),
            issuer,
            command: self.command,
            target_node: self.target_node,
            params: ParamsSummary(self.params),
            outcome,
            path: self.path.map(PathBuf::from),
            from,
        })
    }
}

// ---- the log ------------------------------------------------------------

/// The append-only log.
///
/// The whole point of the type is what is missing from it: no `update`, no
/// `delete`, no way to set an entry's time. Handlers hold a clone of this and
/// so cannot rewrite history even by mistake. Cloning is cheap; both halves
/// share one sink.
#[derive(Clone)]
pub struct Log {
    sink: Arc<dyn Sink>,
    clock: Arc<dyn Clock>,
}

impl fmt::Debug for Log {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Neither field says anything useful, and the entries are not the
        // log's business to dump into a trace line.
        f.write_str("Log(append-only)")
    }
}

impl Log {
    pub fn new(sink: Arc<dyn Sink>, clock: Arc<dyn Clock>) -> Log {
        Log { sink, clock }
    }

    /// The log in this node's state database, on the real clock.
    pub fn sqlite(db_path: &Path) -> Result<Log, EventsError> {
        Ok(Log::new(
            Arc::new(SqliteSink::open(db_path)?),
            Arc::new(OsClock),
        ))
    }

    /// Record one command. The only way to change the log.
    pub fn append(&self, record: Record) -> Result<EntryId, EventsError> {
        let at = self.clock.now();
        let seq = self.sink.append(at, &record)?;
        // Mirrored into the trace so an operator tailing the server sees the
        // same attribution they would see in the panel. The log, not this
        // line, is the record.
        tracing::info!(
            seq = seq.inner(),
            issuer = %record.issuer.label(),
            command = %record.command,
            node = %record.target_node,
            outcome = record.outcome.kind_str(),
            "command"
        );
        Ok(seq)
    }

    /// What the timeline panel calls: by time range, by issuer, by command.
    pub fn timeline(&self, query: &Query) -> Result<Vec<Entry>, EventsError> {
        self.sink.query(query)
    }

    /// Everything after `seq`, oldest first. The shipper's read: keep the
    /// last entry id the VPS acknowledged and ask for what followed it.
    pub fn after(&self, seq: EntryId, limit: usize) -> Result<Vec<Entry>, EventsError> {
        self.sink.query(
            &Query::default()
                .after(seq)
                .limit(limit)
                .order(Order::OldestFirst),
        )
    }

    /// The newest entry, or `None` on an empty log. Used to show a shipper's
    /// backlog and to seed a watermark.
    pub fn newest(&self) -> Result<Option<Entry>, EventsError> {
        Ok(self.sink.query(&Query::default().limit(1))?.pop())
    }
}
