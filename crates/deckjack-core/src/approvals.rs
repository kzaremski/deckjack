//! Grants and the approval Inbox, attached to the bus.
//!
//! [`crate::grants`] has held the whole of this subsystem — scoped grants,
//! routing, the Inbox, the count-before-approval rule — and until this module
//! nothing in `src/` named it. The consequence was not a missing feature but
//! an inverted one. [`crate::command::Bus::new`] fills both of its slots with
//! refusing stubs ([`crate::command::NoGrants`] and
//! [`crate::command::NoInbox`]), and the bus's agent check starts with
//! `self.grants.grant(session).ok_or(Refused::NoGrant)`, so every conceivable
//! agent dispatch was refused before routing ever looked at the command. The
//! Inbox that was supposed to hold the dangerous ones was never reached,
//! because nothing got far enough to be queued.
//!
//! # Why both halves land together
//!
//! They are one wiring, not two. [`crate::command::Bus::dispatch_approved`]
//! re-runs the grant check on purpose — time passed while the item waited and
//! the grant may be gone — so an Inbox attached without grant storage would
//! queue an agent's command, show it to the operator, take their approval and
//! then refuse it for want of a grant. The operator would have tapped approve
//! on something that could never run. Hence one type, [`Approvals`], that
//! implements both bus traits, and one call that registers the commands.
//!
//! # Two vocabularies
//!
//! `command` and `grants` each have their own `Risk`, `Issuer`, `Grant`,
//! `Pattern` and `NodeScope`. That is the right shape — routing is decidable
//! without the bus, and the bus does not depend on the Assistant — and it
//! makes this module a translator. Each edge of the translation is lossy in
//! one direction, and each one is commented where it happens, because a
//! translation that rounds in the permissive direction is a privilege
//! escalation written as a convenience:
//!
//! * [`command::Grant`] carries a `ceiling` that [`grants::Grant`] does not,
//!   so the ceiling is stated by the caller at wiring time rather than
//!   defaulted here. See [`Approvals::open`].
//! * A `grants` pattern covering the whole bus (`*`, `*.*`) has no
//!   [`command::Pattern`] at all — `Pattern::new` refuses it — so it is
//!   dropped rather than approximated, and a grant made only of such patterns
//!   authorizes nothing on the bus.
//! * [`grants::CommandFacts::scale`] cannot be answered honestly from a bus
//!   dispatch, so it answers [`grants::Scale::Unknown`]. See [`Facts`].
//!
//! # The owner column
//!
//! Migration 6 added `grants.owner`, for a defect in both grant lookups: a
//! standing grant has `session = NULL`, and filtering on nothing else let one
//! sessionless issuer inherit the union of every standing grant. Both paths
//! now filter on the holder. [`GrantRows::held_by`] matches
//! `session = ?1 OR owner = ?1` — the path the bus uses — and
//! [`grants::Grants::active_standing`] takes the owner as an argument and
//! compares it to [`grants::Grant::owner`], which `grant_from_row` reads out
//! of the same column.
//!
//! The column has exactly one writer, [`GrantStore::put_grant`], because
//! [`grants::Grants::issue`] takes the holder as an argument and stores it
//! with the rest of the grant. It used to be a second `UPDATE` after
//! issuance, which left a window where a crash stored a grant nobody held.
//!
//! A row written before the migration has an unknown owner and matches
//! nobody: in SQL because `NULL = x` is never true, in Rust because the
//! comparison is against `Some`. That is the safe reading of such a row;
//! inventing an owner for it would be claiming the operator approved
//! something they did not.

use std::sync::{Arc, Mutex, MutexGuard, PoisonError, Weak};

use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::command::{
    self, AgentSessionId, Approval, Bus, Command, CommandError, CommandId, Context, NodeId,
    PendingCommand, PendingId, Registry, RegistryError, Request, RiskLevel,
};
use crate::grants::{
    self, CommandFacts, Confirmation, Env, Grant, GrantId, GrantSpec, GrantStore, Grants, Inbox,
    InboxError, InboxStore, ItemId, ItemStatus, PendingItem, Policy, QueueReason, QueueRequest,
    Risk, Scale, StorageError, SystemEnv,
};
use crate::lockdown::{self, Frozen, OutboundError};
use crate::store::Store;

/// `CommandError::Failed` wraps an `anyhow::Error`; this is the shorthand, as
/// in `wiring.rs`.
fn failed(message: impl Into<String>) -> CommandError {
    CommandError::Failed(anyhow::anyhow!(message.into()))
}

// ---------------------------------------------------------------------------
// The injected world, shared
// ---------------------------------------------------------------------------

/// One clock and id source, handed to both the [`Grants`] and the [`Inbox`].
///
/// [`grants::Env`] is implemented on concrete types and both `Grants` and
/// `Inbox` are generic over it, but [`Approvals`] must not be generic: it is
/// held as `Arc<dyn command::Grants>` and `Arc<dyn command::Approvals>`. This
/// is the adapter that squares the two, and it is what lets a test drive
/// expiry without sleeping while the server passes [`grants::SystemEnv`].
#[derive(Clone)]
pub struct SharedEnv(Arc<dyn Env>);

impl SharedEnv {
    pub fn new(env: Arc<dyn Env>) -> SharedEnv {
        SharedEnv(env)
    }
}

impl Env for SharedEnv {
    fn now(&self) -> u64 {
        self.0.now()
    }

    fn new_id(&self) -> Result<String, crate::auth::code::RandomError> {
        self.0.new_id()
    }
}

// ---------------------------------------------------------------------------
// Storage: the grants table
// ---------------------------------------------------------------------------

/// Every column of `grants`, in the order [`grant_from_row`] reads them.
const GRANT_COLUMNS: &str =
    "id, label, session, project, namespaces, nodes, issued_at, expires_at, revoked_at, owner";

/// A stored grant and the holder it is looked up by.
///
/// `owner` is a field on [`grants::Grant`] as well, because
/// [`grants::Grants::active_standing`] has to filter on it and could not see a
/// column held outside the type. This copy stays because it is what
/// [`GrantListing`] serializes — [`grants::Grant::owner`] is `#[serde(skip)]`,
/// so the operator's listing carries the key once — and both are assigned from
/// one read in `grant_from_row`. Collapsing the two is a separate change.
#[derive(Debug, Clone, Serialize)]
pub struct OwnedGrant {
    #[serde(flatten)]
    pub grant: Grant,
    /// Who holds the grant. `None` on a row written before migration 6, which
    /// therefore matches nobody.
    pub owner: Option<String>,
}

/// Grants in the state database.
pub struct GrantRows {
    store: Arc<Mutex<Store>>,
}

impl GrantRows {
    pub fn new(store: Arc<Mutex<Store>>) -> Arc<GrantRows> {
        Arc::new(GrantRows { store })
    }

    /// Held for one statement and never across an await: a `MutexGuard` alive
    /// in an async body makes the future non-`Send`, and the bus requires
    /// `Send` futures. A poisoned mutex is still usable, as in `audit.rs` and
    /// `http::Ctx` — every write below is one statement, or one small group
    /// that leaves the row set consistent however a previous holder died.
    fn locked(&self) -> MutexGuard<'_, Store> {
        self.store.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Every grant held by one issuer, owner-aware.
    ///
    /// `session = ?1 OR owner = ?1` is the whole fix for the standing-grant
    /// defect. The `session` arm is kept because a grant issued through
    /// [`grants::Grants::issue`] with a session id records it there, and an
    /// agent harness that names its bus session by that id should still find
    /// it; the `owner` arm is what [`Approvals::issue`] writes. Both are
    /// equality tests, so a row with NULL in both columns is returned for
    /// nobody.
    ///
    /// Whether a grant is *active* is deliberately not in this SQL:
    /// [`Grant::is_active`] is `grants.rs`'s rule, and restating it here would
    /// be two definitions of expiry with nothing keeping them in step.
    pub fn held_by(&self, issuer: &str) -> Result<Vec<OwnedGrant>, StorageError> {
        let sql = format!("SELECT {GRANT_COLUMNS} FROM grants WHERE session = ?1 OR owner = ?1");
        self.locked()
            .with_conn(|conn| read_grants(conn, &sql, params![issuer]))
            .map_err(StorageError::new)
    }

    /// Every grant, for the operator's listing.
    pub fn all_owned(&self) -> Result<Vec<OwnedGrant>, StorageError> {
        let sql = format!("SELECT {GRANT_COLUMNS} FROM grants ORDER BY issued_at, id");
        self.locked()
            .with_conn(|conn| read_grants(conn, &sql, params![]))
            .map_err(StorageError::new)
    }
}

impl GrantStore for GrantRows {
    /// Upsert by id, owner included.
    ///
    /// The only writer of the `owner` column. `INSERT OR REPLACE` would
    /// delete the row and insert a new one; the conflict clause names its
    /// columns instead, which mattered when `owner` was a column this port
    /// could not see — a grant rewritten by REPLACE lost its holder and
    /// stopped matching anybody. [`grants::Grant`] now carries `owner`, read
    /// back by `grant_from_row`, so writing it here is writing what was read
    /// rather than clearing it.
    fn put_grant(&self, grant: &Grant) -> Result<(), StorageError> {
        let namespaces = grant.namespaces_text();
        let nodes = grant.nodes.as_text();
        self.locked()
            .with_conn(|conn| {
                conn.execute(
                    "INSERT INTO grants \
                     (id, label, session, project, namespaces, nodes, issued_at, expires_at, \
                      revoked_at, owner) \
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10) \
                     ON CONFLICT(id) DO UPDATE SET \
                       label = ?2, session = ?3, project = ?4, namespaces = ?5, nodes = ?6, \
                       issued_at = ?7, expires_at = ?8, revoked_at = ?9, owner = ?10",
                    params![
                        grant.id.as_str(),
                        grant.label,
                        grant.session,
                        grant.project,
                        namespaces,
                        nodes,
                        secs(grant.issued_at),
                        grant.expires_at.map(secs),
                        grant.revoked_at.map(secs),
                        grant.owner,
                    ],
                )?;
                Ok(())
            })
            .map_err(StorageError::new)
    }

    fn grant(&self, id: &GrantId) -> Result<Option<Grant>, StorageError> {
        let sql = format!("SELECT {GRANT_COLUMNS} FROM grants WHERE id = ?1");
        self.locked()
            .with_conn(|conn| {
                conn.query_row(&sql, params![id.as_str()], grant_from_row)
                    .optional()
            })
            .map_err(StorageError::new)
            .map(|row| row.flatten().map(|owned| owned.grant))
    }

    fn grants(&self) -> Result<Vec<Grant>, StorageError> {
        Ok(self
            .all_owned()?
            .into_iter()
            .map(|owned| owned.grant)
            .collect())
    }

    fn revoke_grant(&self, id: &GrantId, at: u64) -> Result<bool, StorageError> {
        self.locked()
            .with_conn(|conn| {
                // Two statements, because the answer and the write are
                // different questions: the caller is told whether there was a
                // grant at all, while `revoked_at IS NULL` keeps the first
                // revocation's timestamp — the one that mattered.
                let present: Option<i64> = conn
                    .query_row(
                        "SELECT 1 FROM grants WHERE id = ?1",
                        params![id.as_str()],
                        |r| r.get(0),
                    )
                    .optional()?;
                if present.is_none() {
                    return Ok(false);
                }
                conn.execute(
                    "UPDATE grants SET revoked_at = ?2 WHERE id = ?1 AND revoked_at IS NULL",
                    params![id.as_str(), secs(at)],
                )?;
                Ok(true)
            })
            .map_err(StorageError::new)
    }

    fn revoke_all(&self, at: u64) -> Result<usize, StorageError> {
        self.locked()
            .with_conn(|conn| {
                conn.execute(
                    "UPDATE grants SET revoked_at = ?1 WHERE revoked_at IS NULL",
                    params![secs(at)],
                )
            })
            .map_err(StorageError::new)
    }
}

fn read_grants(
    conn: &Connection,
    sql: &str,
    args: &[&dyn rusqlite::ToSql],
) -> rusqlite::Result<Vec<OwnedGrant>> {
    let mut stmt = conn.prepare(sql)?;
    let rows = stmt.query_map(args, grant_from_row)?;
    let mut out = Vec::new();
    for row in rows {
        // A row this build cannot read is a row it cannot honor, and honoring
        // it wrongly would mean authorizing a command under a grant nobody can
        // see. Dropped with a warning, so the failure is a refused command the
        // operator can issue a fresh grant for.
        match row? {
            Some(owned) => out.push(owned),
            None => tracing::warn!("skipping an unreadable grant row"),
        }
    }
    Ok(out)
}

/// `Ok(None)` is a row whose text this build does not understand; see
/// [`read_grants`].
fn grant_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Option<OwnedGrant>> {
    let id: String = row.get(0)?;
    let label: String = row.get(1)?;
    let session: Option<String> = row.get(2)?;
    let project: Option<String> = row.get(3)?;
    let namespaces: String = row.get(4)?;
    let nodes: String = row.get(5)?;
    let issued_at: i64 = row.get(6)?;
    let expires_at: Option<i64> = row.get(7)?;
    let revoked_at: Option<i64> = row.get(8)?;
    let owner: Option<String> = row.get(9)?;

    let namespaces = Grant::parse_namespaces(&namespaces);
    // An empty namespace list allows nothing, and `Grants::issue` refuses to
    // create one. Finding one here means every pattern in the row failed to
    // parse, so the row is reported unreadable rather than returned as a grant
    // that covers nothing under a label saying otherwise.
    if namespaces.is_empty() {
        return Ok(None);
    }

    Ok(Some(OwnedGrant {
        grant: Grant {
            id: GrantId::from_text(id),
            label,
            session,
            // One read of the column, assigned twice at one site, so
            // `Grant::owner` and `OwnedGrant::owner` cannot drift. The
            // duplication survives only because `OwnedGrant` is what the
            // operator's listing is built from; collapsing the two is a
            // separate change.
            owner: owner.clone(),
            project,
            namespaces,
            nodes: grants::NodeScope::from_text(&nodes),
            issued_at: from_secs(issued_at),
            expires_at: expires_at.map(from_secs),
            revoked_at: revoked_at.map(from_secs),
        },
        owner,
    }))
}

// ---------------------------------------------------------------------------
// Storage: the inbox table
// ---------------------------------------------------------------------------

/// Every column of `inbox`, in the order [`item_from_row`] reads them.
const ITEM_COLUMNS: &str = "id, grant_id, issuer, command, target_node, risk, reason, \
     scale_kind, scale_count, title, detail, params, created, expires, status, decided_at, \
     decided_note";

/// Inbox items in the state database.
pub struct InboxRows {
    store: Arc<Mutex<Store>>,
}

impl InboxRows {
    pub fn new(store: Arc<Mutex<Store>>) -> Arc<InboxRows> {
        Arc::new(InboxRows { store })
    }

    /// See [`GrantRows::locked`] for why this is never held across an await.
    fn locked(&self) -> MutexGuard<'_, Store> {
        self.store.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl InboxStore for InboxRows {
    fn put_item(&self, item: &PendingItem) -> Result<(), StorageError> {
        // The three JSON columns. `params` is data: it is written, shown to
        // the operator and handed back to the bus, and nothing here reads any
        // of it as instructions.
        let issuer = serde_json::to_string(&item.issuer).map_err(StorageError::new)?;
        let reason = serde_json::to_string(&item.reason).map_err(StorageError::new)?;
        let item_params = serde_json::to_string(&item.params).map_err(StorageError::new)?;
        let (scale_kind, scale_count) = item.scale.as_parts();

        self.locked()
            .with_conn(|conn| {
                conn.execute(
                    "INSERT OR REPLACE INTO inbox \
                     (id, grant_id, issuer, command, target_node, risk, reason, scale_kind, \
                      scale_count, title, detail, params, created, expires, status, decided_at, \
                      decided_note) \
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, \
                             ?16, ?17)",
                    params![
                        item.id.as_str(),
                        item.grant.as_ref().map(|g| g.as_str()),
                        issuer,
                        item.command,
                        item.target_node,
                        item.risk.as_str(),
                        reason,
                        scale_kind,
                        scale_count.map(secs),
                        item.title,
                        item.detail,
                        item_params,
                        secs(item.created),
                        item.expires.map(secs),
                        item.status.as_str(),
                        item.decided_at.map(secs),
                        item.decided_note,
                    ],
                )?;
                Ok(())
            })
            .map_err(StorageError::new)
    }

    fn item(&self, id: &ItemId) -> Result<Option<PendingItem>, StorageError> {
        let sql = format!("SELECT {ITEM_COLUMNS} FROM inbox WHERE id = ?1");
        self.locked()
            .with_conn(|conn| {
                conn.query_row(&sql, params![id.as_str()], item_from_row)
                    .optional()
            })
            .map_err(StorageError::new)
            .map(Option::flatten)
    }

    fn items_with_status(&self, status: ItemStatus) -> Result<Vec<PendingItem>, StorageError> {
        // Oldest first, which is the order the Inbox reads in and the order
        // the `inbox_status` index is built for. The id breaks ties so two
        // items queued in the same second have a stable order rather than
        // whichever SQLite happens to return.
        let sql =
            format!("SELECT {ITEM_COLUMNS} FROM inbox WHERE status = ?1 ORDER BY created, id");
        self.locked()
            .with_conn(|conn| read_items(conn, &sql, params![status.as_str()]))
            .map_err(StorageError::new)
    }

    fn recent_items(&self, limit: u32) -> Result<Vec<PendingItem>, StorageError> {
        let sql =
            format!("SELECT {ITEM_COLUMNS} FROM inbox ORDER BY created DESC, id DESC LIMIT ?1");
        self.locked()
            .with_conn(|conn| read_items(conn, &sql, params![limit]))
            .map_err(StorageError::new)
    }

    fn expire_pending_before(&self, cutoff: u64, at: u64) -> Result<usize, StorageError> {
        self.locked()
            .with_conn(|conn| {
                conn.execute(
                    "UPDATE inbox SET status = ?1, decided_at = ?2 \
                     WHERE status = ?3 AND created < ?4",
                    params![
                        ItemStatus::Expired.as_str(),
                        secs(at),
                        ItemStatus::Pending.as_str(),
                        secs(cutoff)
                    ],
                )
            })
            .map_err(StorageError::new)
    }
}

fn read_items(
    conn: &Connection,
    sql: &str,
    args: &[&dyn rusqlite::ToSql],
) -> rusqlite::Result<Vec<PendingItem>> {
    let mut stmt = conn.prepare(sql)?;
    let rows = stmt.query_map(args, item_from_row)?;
    let mut out = Vec::new();
    for row in rows {
        // An item this build cannot read is left out of the listing. That
        // hides a request from the operator, which is bad; returning a
        // half-understood one would let them approve a command whose risk,
        // scale or parameters are not what the row says, which is worse. An
        // item that is never listed is also never approved.
        match row? {
            Some(item) => out.push(item),
            None => tracing::warn!("skipping an unreadable Inbox row"),
        }
    }
    Ok(out)
}

/// `Ok(None)` is a row whose encodings this build does not understand; see
/// [`read_items`].
fn item_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Option<PendingItem>> {
    let id: String = row.get(0)?;
    let grant: Option<String> = row.get(1)?;
    let issuer: String = row.get(2)?;
    let command: String = row.get(3)?;
    let target_node: String = row.get(4)?;
    let risk: String = row.get(5)?;
    let reason: String = row.get(6)?;
    let scale_kind: String = row.get(7)?;
    let scale_count: Option<i64> = row.get(8)?;
    let title: String = row.get(9)?;
    let detail: Option<String> = row.get(10)?;
    let item_params: String = row.get(11)?;
    let created: i64 = row.get(12)?;
    let expires: Option<i64> = row.get(13)?;
    let status: String = row.get(14)?;
    let decided_at: Option<i64> = row.get(15)?;
    let decided_note: Option<String> = row.get(16)?;

    // Each of these is a value some other build may have written with a name
    // this one does not know. All of them decide what the operator would be
    // approving, so none of them gets a default.
    let (Some(risk), Some(status)) = (Risk::parse(&risk), ItemStatus::parse(&status)) else {
        return Ok(None);
    };
    let Some(scale) = Scale::from_parts(&scale_kind, scale_count.map(from_secs)) else {
        return Ok(None);
    };
    let (Ok(issuer), Ok(reason), Ok(item_params)) = (
        serde_json::from_str(&issuer),
        serde_json::from_str(&reason),
        serde_json::from_str::<Value>(&item_params),
    ) else {
        return Ok(None);
    };

    Ok(Some(PendingItem {
        id: ItemId::from_text(id),
        grant: grant.map(GrantId::from_text),
        issuer,
        command,
        target_node,
        risk,
        reason,
        scale,
        title,
        detail,
        params: item_params,
        created: from_secs(created),
        expires: expires.map(from_secs),
        status,
        decided_at: decided_at.map(from_secs),
        decided_note,
    }))
}

/// Unix seconds as the column wants them. A value beyond `i64` is clamped
/// rather than wrapped: a grant that appears to expire in the year 292 billion
/// is a bug worth reading in the file, and one that wrapped to the past would
/// silently revoke itself.
fn secs(v: u64) -> i64 {
    i64::try_from(v).unwrap_or(i64::MAX)
}

fn from_secs(v: i64) -> u64 {
    v.max(0) as u64
}

// ---------------------------------------------------------------------------
// The seam to the bus: CommandFacts
// ---------------------------------------------------------------------------

/// What [`crate::grants`] needs to know about a bus dispatch.
///
/// Built from a [`command::CommandSpec`] plus the node the bus resolved, or
/// from the [`PendingCommand`] the bus hands the Inbox — which carries the
/// same facts and is what actually arrives on the queueing path.
pub struct Facts {
    id: String,
    risk: Risk,
    node: String,
}

impl Facts {
    pub fn of_spec(spec: &command::CommandSpec, node: &NodeId) -> Facts {
        Facts {
            id: spec.id().to_string(),
            risk: translate_risk(spec.risk()),
            node: node.as_str().to_string(),
        }
    }

    pub fn of_pending(pending: &PendingCommand) -> Facts {
        Facts {
            id: pending.command.to_string(),
            risk: translate_risk(pending.risk),
            node: pending.node.as_str().to_string(),
        }
    }
}

impl CommandFacts for Facts {
    fn id(&self) -> &str {
        &self.id
    }

    fn risk(&self) -> Risk {
        self.risk
    }

    fn target_node(&self) -> &str {
        &self.node
    }

    /// Always [`Scale::Unknown`], and that is not a shortcut.
    ///
    /// The bus has no concept of a per-dispatch item count anywhere: a command
    /// declares an id, a risk and a target, and nothing in
    /// [`command::Request`] or [`command::CommandSpec`] says how many things
    /// an invocation touches. There is nothing here that could supply a real
    /// number.
    ///
    /// The trait's default is [`Scale::Single`], and taking it would be a
    /// quiet lie in the exact shape the bulk rule exists to catch: an agent
    /// deleting most of a mailbox one message at a time, each delete looking
    /// small. `Single` reports one item, which is below any
    /// [`Policy::bulk_threshold`], so the bulk-approval rule would disarm
    /// itself. `Unknown` is both honest and conservative —
    /// [`Policy::queue_unknown_scale`] defaults true, so a write that will not
    /// say how much it touches waits — and it is what makes the operator
    /// accept an unbounded action explicitly
    /// ([`Confirmation::UnknownAccepted`]) instead of echoing a count that was
    /// invented here.
    fn scale(&self) -> Scale {
        Scale::Unknown
    }
}

/// `command::RiskLevel` to `grants::Risk`, by name and exhaustively.
///
/// No catch-all arm, so a level added to either enum is a compile error here.
/// A `_ =>` would map an unknown future level onto whichever existing one
/// looked closest, and the two enums already disagree about ordering —
/// `grants::Risk` lists `Destructive` before `Outbound`, `command::RiskLevel`
/// the other way — so "closest" is not something this translation can guess.
fn translate_risk(risk: RiskLevel) -> Risk {
    match risk {
        RiskLevel::ReadOnly => Risk::ReadOnly,
        RiskLevel::Write => Risk::Write,
        RiskLevel::CrossProject => Risk::CrossProject,
        RiskLevel::Outbound => Risk::Outbound,
        RiskLevel::Destructive => Risk::Destructive,
    }
}

/// Why an item waits, when there is no grant to run [`grants::decide`]
/// against — the operator's own deferred confirmations, and the CLI's.
///
/// Derived from the declared risk, never from the caller. `Write` and
/// `ReadOnly` cannot reach the queue through the bus at all
/// ([`RiskLevel::needs_approval_from_agents`] is false for both); if one ever
/// does, [`QueueReason::ScaleUnknown`] is the true reason, because that is
/// what [`Facts::scale`] reports.
fn reason_for(risk: Risk) -> QueueReason {
    match risk {
        Risk::Destructive => QueueReason::Destructive,
        Risk::Outbound => QueueReason::Outbound,
        Risk::CrossProject => QueueReason::CrossProject,
        Risk::Write | Risk::ReadOnly => QueueReason::ScaleUnknown,
    }
}

/// `command::Issuer` to `grants::Issuer`.
///
/// An agent's bus identity is a *name* ([`AgentSessionId`]) and never a
/// session cookie, so `session` stays `None` and the name travels as the
/// label. That name is also the key [`GrantRows::held_by`] matches on, which
/// is what keeps the Inbox item's recorded issuer and the grant lookup talking
/// about the same thing.
///
/// The operator's browser session is dropped rather than carried: the bus
/// holds it as a [`command::BrowserSession`] whose serialized form is a
/// digest, and `grants::Issuer::Operator` has nowhere to put one. An Inbox row
/// saying which browser deferred a confirmation would be a nice timeline
/// detail and is not worth inventing a field for.
fn translate_issuer(issuer: &command::Issuer) -> grants::Issuer {
    match issuer {
        command::Issuer::Operator { .. } => grants::Issuer::Operator,
        command::Issuer::Cli => grants::Issuer::Cli,
        command::Issuer::Agent { session } => grants::Issuer::Session {
            label: session.as_str().to_string(),
            session: None,
        },
    }
}

/// `grants::Issuer` back to `command::Issuer`, for releasing an approved item.
///
/// An operator item comes back with no browser session. That is honest: the
/// browser that deferred the confirmation is not necessarily the one that
/// approved it, and the bus uses that field to decide what a pty outlives.
fn restore_issuer(issuer: &grants::Issuer) -> command::Issuer {
    match issuer {
        grants::Issuer::Operator => command::Issuer::operator(),
        grants::Issuer::Cli => command::Issuer::Cli,
        grants::Issuer::Session { label, .. } => command::Issuer::agent(label.clone()),
    }
}

// ---------------------------------------------------------------------------
// The subsystem
// ---------------------------------------------------------------------------

/// Grants and the Inbox, assembled over the shared [`Store`] and attachable to
/// the bus.
///
/// Implements [`command::Grants`] and [`command::Approvals`] on one type, so
/// the two cannot be attached apart; see the module docs for why that matters.
pub struct Approvals {
    grants: Grants<Arc<GrantRows>, SharedEnv>,
    rows: Arc<GrantRows>,
    items: Arc<InboxRows>,
    /// Behind a mutex because [`Inbox::set_frozen`] takes `&mut self` while
    /// [`lockdown::Lockdown::engage`] has only `&self`. That is the reason it
    /// is here, and it buys something else worth having: every decision about
    /// an item — load, check, write — happens under one lock. See
    /// [`Approvals::approve`].
    inbox: Mutex<Inbox<Arc<InboxRows>, SharedEnv>>,
    /// Set by [`Approvals::follow_lockdown`]. See it for why the freeze needs
    /// a way back.
    lockdown: Mutex<Option<Weak<lockdown::Lockdown>>>,
    env: SharedEnv,
    policy: Policy,
    ceiling: RiskLevel,
}

impl Approvals {
    /// The server's assembly: the real clock, the default policy.
    ///
    /// `ceiling` has no default on purpose. [`command::Grant`] carries the
    /// highest risk a session may *ask* about, [`grants::Grant`] has no such
    /// field, and nothing in the database says what it should be. The
    /// permissive end ([`RiskLevel::Destructive`]) is what
    /// [`command::Grant::new`] uses, and defaulting to it here would widen
    /// every grant the operator writes by filling in a field they never wrote.
    /// The restrictive end ([`RiskLevel::Write`]) refuses destructive,
    /// outbound and cross-project agent commands outright instead of queueing
    /// them — the conservative direction, and also the direction that leaves
    /// the Inbox permanently empty.
    ///
    /// Neither is this module's to pick silently, so the caller states it in
    /// the line that attaches this subsystem, where it shows up in a diff.
    /// Until there is a column for it — tether mode is the design's own reason
    /// to want one per session — it is one value for the node.
    pub fn open(store: Arc<Mutex<Store>>, ceiling: RiskLevel) -> Arc<Approvals> {
        Approvals::open_with(store, ceiling, Policy::default(), Arc::new(SystemEnv))
    }

    /// The same, with the policy and the clock supplied.
    pub fn open_with(
        store: Arc<Mutex<Store>>,
        ceiling: RiskLevel,
        policy: Policy,
        env: Arc<dyn Env>,
    ) -> Arc<Approvals> {
        let env = SharedEnv::new(env);
        let rows = GrantRows::new(Arc::clone(&store));
        let items = InboxRows::new(store);
        Arc::new(Approvals {
            // `Arc<T>` satisfies both storage ports through the blanket impls
            // in `grants.rs`, so the same handle is held here and by the
            // owner-aware queries beside it.
            grants: Grants::new(Arc::clone(&rows), env.clone(), policy.clone()),
            rows,
            items: Arc::clone(&items),
            inbox: Mutex::new(Inbox::new(items, env.clone(), policy.clone())),
            lockdown: Mutex::new(None),
            env,
            policy,
            ceiling,
        })
    }

    /// Attach as the bus's grant storage: `bus.with_grants(approvals.grants())`.
    pub fn grants(self: &Arc<Self>) -> Arc<dyn command::Grants> {
        Arc::clone(self) as Arc<dyn command::Grants>
    }

    /// Attach as the bus's Inbox: `bus.with_approvals(approvals.inbox())`.
    pub fn inbox(self: &Arc<Self>) -> Arc<dyn command::Approvals> {
        Arc::clone(self) as Arc<dyn command::Approvals>
    }

    /// Attach as lockdown's step 3: `outbound: approvals.outbound()`.
    pub fn outbound(self: &Arc<Self>) -> Box<dyn lockdown::Outbound + Send + Sync> {
        Box::new(InboxFreeze(Arc::clone(self)))
    }

    /// Follow lockdown's own state, so that clearing a lockdown clears the
    /// freeze: `approvals.follow_lockdown(Arc::downgrade(&lockdown))`.
    ///
    /// [`lockdown::Outbound`] has a `freeze` and no thaw, and
    /// [`lockdown::Lockdown::unlock`] has no hook to call one from. Without
    /// this, one lockdown leaves the Inbox frozen until the server restarts —
    /// a feature that works once and then silently does not, which is the
    /// shape of defect this whole module exists to undo.
    ///
    /// A [`Weak`] handle, because the lockdown holds this subsystem as its
    /// step 3 and a strong one would be a cycle neither end ever leaves.
    ///
    /// It only ever thaws. Freezing stays the kill switch's own path: a gate
    /// that could also freeze from here would be two authorities deciding
    /// when approvals stop, and a kill switch must not have anything to
    /// disagree with.
    pub fn follow_lockdown(&self, lockdown: Weak<lockdown::Lockdown>) {
        *self.lockdown.lock().unwrap_or_else(PoisonError::into_inner) = Some(lockdown);
    }

    /// Clear a freeze whose lockdown is over.
    ///
    /// Takes the guard the caller already holds, so the check and the
    /// approval that follows it cannot be separated by another thread
    /// freezing in between.
    fn thaw_if_unlocked(&self, inbox: &mut Inbox<Arc<InboxRows>, SharedEnv>) {
        if !inbox.is_frozen() {
            return;
        }
        let followed = self
            .lockdown
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .as_ref()
            .and_then(Weak::upgrade);
        // Nothing followed, or the lockdown is gone: the freeze stands.
        // Staying frozen is the safe direction, and `thaw` is the way out by
        // hand.
        let Some(lockdown) = followed else {
            return;
        };
        if !lockdown.is_locked() {
            inbox.set_frozen(false);
            tracing::info!("lockdown is over; Inbox approvals unfrozen");
        }
    }

    pub fn policy(&self) -> &Policy {
        &self.policy
    }

    /// The injected clock. Unix seconds.
    pub fn now(&self) -> u64 {
        self.env.now()
    }

    /// See [`GrantRows::locked`]: taken for one call, never across an await.
    fn locked_inbox(&self) -> MutexGuard<'_, Inbox<Arc<InboxRows>, SharedEnv>> {
        self.inbox.lock().unwrap_or_else(PoisonError::into_inner)
    }

    // ---- grants ---------------------------------------------------------

    /// Issue a grant to a named holder.
    ///
    /// Validation — the patterns, the node scope, the TTL ceiling, the refusal
    /// of a grant covering the whole bus — is [`grants::Grants::issue`]'s,
    /// which is why this is a wrapper and not its own write. The holder is
    /// handed to it and written with the rest of the grant, so there is one
    /// statement and one source for it: no window where the grant is on disk
    /// owned by nobody, and no spec field that could disagree with this
    /// argument.
    pub fn issue(&self, owner: &str, spec: GrantSpec) -> Result<OwnedGrant, grants::IssueError> {
        let grant = self.grants.issue(Some(owner), spec)?;
        Ok(OwnedGrant {
            grant,
            owner: Some(owner.to_string()),
        })
    }

    pub fn revoke(&self, id: &GrantId) -> Result<bool, StorageError> {
        self.grants.revoke(id)
    }

    /// The kill switch's half of grants: revoke everything, pausing every
    /// agent. Not reachable from lockdown today — [`lockdown::Deps`] has no
    /// slot for grant storage — and exposed so the path that gains one has it.
    pub fn revoke_all(&self) -> Result<usize, StorageError> {
        self.grants.revoke_all()
    }

    pub fn all_grants(&self) -> Result<Vec<OwnedGrant>, StorageError> {
        self.rows.all_owned()
    }

    /// Active grants held by one issuer, owner-aware. What
    /// [`command::Grants::grant`] answers from.
    pub fn held_by(&self, issuer: &str) -> Result<Vec<Grant>, StorageError> {
        let now = self.env.now();
        Ok(self
            .rows
            .held_by(issuer)?
            .into_iter()
            .map(|owned| owned.grant)
            .filter(|g| g.is_active(now))
            .collect())
    }

    // ---- the Inbox ------------------------------------------------------

    pub fn pending(&self) -> Result<Vec<PendingItem>, InboxError> {
        self.locked_inbox().pending()
    }

    /// Recent items, decided ones included, for the history pane.
    ///
    /// Straight to the port: [`Inbox`] has no reader for decided items, and
    /// history wants rows rather than routing.
    pub fn recent(&self, limit: u32) -> Result<Vec<PendingItem>, StorageError> {
        self.items.recent_items(limit)
    }

    /// Approve an item and hand it back for dispatch.
    ///
    /// The lock is held across the whole approval, which is what makes it
    /// atomic. [`Inbox::approve`] reads the item, checks that it is pending
    /// and writes it back as approved with no compare-and-set in between, so
    /// two approvals arriving together could both read `pending` and both
    /// return an approved item — one queued command dispatched twice. Closing
    /// that properly needs a conditional write, which means a new method on
    /// [`InboxStore`], and `grants.rs` is not this module's to change.
    /// Serializing every decision through this mutex closes it for this
    /// process, and there is exactly one process: the store holds the single
    /// connection to the file.
    pub fn approve(&self, id: &ItemId, confirm: Confirmation) -> Result<PendingItem, InboxError> {
        let mut inbox = self.locked_inbox();
        self.thaw_if_unlocked(&mut inbox);
        Ok(inbox.approve(id, confirm)?.item)
    }

    pub fn deny(&self, id: &ItemId, note: Option<String>) -> Result<PendingItem, InboxError> {
        self.locked_inbox().deny(id, note)
    }

    pub fn is_frozen(&self) -> bool {
        self.locked_inbox().is_frozen()
    }

    /// End the freeze lockdown's step 3 began.
    ///
    /// [`lockdown::Outbound`] has a `freeze` and no thaw, and
    /// [`lockdown::Lockdown::unlock`] therefore cannot undo step 3. It does
    /// not undo step 1 or step 2 either, because a revoked session and a
    /// killed child are not things an unlock puts back. A frozen Inbox is.
    ///
    /// [`Approvals::follow_lockdown`] does this automatically on the next
    /// approval; this is the explicit lever, for a caller that has cleared a
    /// lockdown and wants the Inbox usable before anyone taps anything.
    ///
    /// Refused, and `false`, while a followed lockdown is still engaged. A
    /// method that unfroze approvals during a lockdown would be a way around
    /// the kill switch, and the caller this exists for — the unlock path — is
    /// by definition not locked any more by the time it runs. With nothing
    /// followed there is nothing to consult and the thaw goes ahead; that is
    /// the deliberate gap, and it is why following is one of the wiring lines.
    pub fn thaw(&self) -> bool {
        if self.locked_down() {
            tracing::warn!(
                "refusing to unfreeze the Inbox: this node is still locked down, and \
                 approvals are part of what that holds"
            );
            return false;
        }
        self.locked_inbox().set_frozen(false);
        tracing::info!("Inbox approvals unfrozen");
        true
    }

    /// Whether the followed lockdown says this node is locked. `false` when
    /// nothing is followed: this cannot report a lockdown it was never told
    /// about.
    fn locked_down(&self) -> bool {
        self.lockdown
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .as_ref()
            .and_then(Weak::upgrade)
            .is_some_and(|ld| ld.is_locked())
    }
}

// ---------------------------------------------------------------------------
// The bus facades
// ---------------------------------------------------------------------------

impl command::Grants for Approvals {
    /// The grant the bus checks every agent dispatch against.
    ///
    /// A storage failure answers `None`, which the bus turns into its
    /// "holds no grant" refusal. That is the safe direction: a database this
    /// subsystem cannot read must not be a database that authorizes anything.
    /// It is logged at error level because it is not a refusal the operator
    /// can fix by widening a grant.
    fn grants_for(&self, session: &AgentSessionId) -> Vec<command::Grant> {
        let held = match self.held_by(session.as_str()) {
            Ok(held) => held,
            Err(e) => {
                tracing::error!(
                    session = %session,
                    error = %e,
                    "grants could not be read; refusing the dispatch"
                );
                // An empty list, so the bus says "holds no grant". A
                // database this subsystem cannot read must not be a database
                // that authorizes anything.
                return Vec::new();
            }
        };
        // One translation per grant, kept separate. Empty means "holds no
        // grant", which is the true answer and a different thing from "was
        // not granted this command".
        held.iter()
            .map(|g| translate_grant(g, self.ceiling))
            .collect()
    }
}

/// ONE grant, in the bus's vocabulary.
///
/// One at a time, deliberately. This used to take the whole slice and union
/// it field-wise, on the reasoning that holding two grants means holding
/// their union — which is true of `grants::decide_any`, but `decide_any`
/// evaluates each grant WHOLE. Unioning the fields instead produced
/// combinations nobody issued: a grant of `terminal.*` scoped to node
/// `r620`, plus a grant of `notes.list` on this node, came out as namespaces
/// `[terminal.*, notes.list]` over nodes `[r620, this]` — so `terminal.open`
/// ran here, authorized by neither grant. The module header calls that out
/// as the thing not to do ("a translation that rounds in the permissive
/// direction is a privilege escalation written as a convenience") and it is
/// what the code did anyway.
///
/// The ceiling is not part of the stored grant: it is the value the caller
/// stated once, at wiring, so every translated grant carries the same one.
fn translate_grant(grant: &Grant, ceiling: RiskLevel) -> command::Grant {
    let mut namespaces: Vec<command::Pattern> = Vec::new();

    for pattern in &grant.namespaces {
        let text = pattern.to_string();
        match command::Pattern::new(&text) {
            Ok(p) => {
                if !namespaces.contains(&p) {
                    namespaces.push(p);
                }
            }
            // Dropped, not approximated. What lands here are the patterns
            // with no bus equivalent: `*` and `*.*`, which
            // `command::Pattern::new` refuses because a grant of the whole
            // machine is not a grant, and a middle wildcard such as
            // `git.*.push`, which the bus does not implement. Widening
            // either into something that compiles would grant more than
            // the operator wrote; the refusal it causes instead is visible
            // and the grant can be reissued as namespaces.
            Err(e) => tracing::warn!(
                grant = %grant.label,
                pattern = %text,
                error = %e,
                "a grant pattern has no bus equivalent and was dropped"
            ),
        }
    }

    command::Grant {
        namespaces,
        // `ThisNode` is never produced: `grants::NodeScope` has no such
        // variant, so every node a grant names is named explicitly, and
        // translating an explicit list into "wherever this happens to run"
        // would silently follow the server onto another machine.
        nodes: match &grant.nodes {
            grants::NodeScope::All => command::NodeScope::AnyNode,
            grants::NodeScope::Only(ids) => {
                command::NodeScope::Nodes(ids.iter().cloned().map(NodeId::new).collect())
            }
        },
        ceiling,
    }
}

impl command::Approvals for Approvals {
    /// Put a dispatch the bus declined to run straight into the Inbox.
    ///
    /// Everything the operator is shown comes from the bus's own
    /// [`PendingCommand`] — the id, the declared risk, the node, the title the
    /// spec wrote and the real parameters — except the queue reason and the
    /// scale, which are this subsystem's to decide. Both are derived from the
    /// declared risk and [`Facts`], never from anything a client sent.
    fn submit(&self, pending: &PendingCommand) -> Result<PendingId, command::ApprovalError> {
        let facts = Facts::of_pending(pending);
        let issuer = translate_issuer(&pending.issuer);

        // Which grant it was issued under, for the Inbox's own attribution.
        // Best effort: the bus has already decided this dispatch is inside the
        // grant, so a lookup that comes back empty means the item is recorded
        // with no grant id, not that it should be refused.
        let under = self.grant_behind(&issuer, &facts);
        let (reason, scale) = self.reason_and_scale(under.as_ref(), &facts);

        let request = QueueRequest::from_command(
            under.as_ref(),
            issuer,
            &facts,
            reason,
            scale,
            pending.title.clone(),
            pending.params.clone(),
        );

        let item = self
            .locked_inbox()
            .queue(request)
            // Every `InboxError`, with its own message attached.
            //
            // `Frozen` is NOT among them in practice, and the comment here
            // used to claim it was: `Inbox::queue` has no frozen check — only
            // `Inbox::approve` does. So a dispatch that would queue during a
            // lockdown still queues. That fails closed rather than open (the
            // item cannot be approved until `thaw`, and the kill switch has
            // already revoked every session that could ask), and it keeps the
            // request recorded instead of losing it, which is why it is left
            // this way. It is simply not what this comment said.
            .map_err(|e| command::ApprovalError::Failed(e.to_string()))?;
        Ok(PendingId::new(item.id.to_string()))
    }
}

impl Approvals {
    /// The active grant covering this command, if the issuer holds one.
    fn grant_behind(&self, issuer: &grants::Issuer, facts: &Facts) -> Option<Grant> {
        let label = match issuer {
            grants::Issuer::Session { label, .. } => label.as_str(),
            // The operator and the CLI do not route through grants at all (see
            // `grants::Issuer`), so an item from either has no grant behind it
            // and says so.
            grants::Issuer::Operator | grants::Issuer::Cli => return None,
        };
        let held = self.held_by(label).unwrap_or_else(|e| {
            tracing::warn!(
                issuer = label,
                error = %e,
                "grants could not be read while queueing; the item is recorded without one"
            );
            Vec::new()
        });
        held.into_iter()
            .find(|g| g.allows_command(facts.id()) && g.allows_node(facts.target_node()))
    }

    /// Why this item waits, and how much it touches.
    ///
    /// Asked of [`grants::decide`] when there is a grant, because that
    /// function is the module's own rule and may have a better reason than the
    /// risk level alone — bulk, or an unmeasured write. Its `Run` and `Refuse`
    /// answers are not contradictions to resolve here: the bus has already
    /// decided this dispatch waits, and the two modules weigh different things
    /// (the bus has no scale, this one has no ceiling). The risk-derived
    /// reason is the honest fallback either way.
    fn reason_and_scale(&self, under: Option<&Grant>, facts: &Facts) -> (QueueReason, Scale) {
        let scale = facts.scale();
        let risk = facts.risk();
        let Some(grant) = under else {
            return (reason_for(risk), scale);
        };
        match grants::decide(grant, facts, &self.policy, self.env.now()) {
            grants::Decision::Queue { reason, scale } => (reason, scale),
            grants::Decision::Run | grants::Decision::Refuse(_) => (reason_for(risk), scale),
        }
    }
}

// ---------------------------------------------------------------------------
// lockdown step 3
// ---------------------------------------------------------------------------

/// Lockdown's step 3 over the Inbox, replacing [`lockdown::NoOutbound`].
///
/// Only the Inbox: there is still no mail queue and no scheduler, so those two
/// counts stay zero and stay honest. [`Frozen`] is shared with subsystems that
/// do not exist yet, and reporting a number for one of them would be the lie
/// `NoOutbound` was named to avoid.
pub struct InboxFreeze(Arc<Approvals>);

impl lockdown::Outbound for InboxFreeze {
    fn freeze(&self) -> Result<Frozen, OutboundError> {
        // One lock for both, and the flag first: counting first would leave a
        // window in which an item is counted as held and then approved before
        // the freeze lands. Freezing is what the operator asked for; the
        // number is only what they are told about it.
        //
        // This is what holds items that were already queued: without it, an
        // agent's destructive command queued before a lockdown could be
        // approved and run after it. `Inbox::approve` refuses while frozen,
        // and that is the half this provides.
        //
        // The other half is on the bus, and is wired: `BusParts.halt`
        // carries the lockdown itself as the bus's `command::Halt`, so
        // `Bus::agents_paused` reports true for the whole lockdown and every
        // agent-issued dispatch is refused with `Refused::AgentsPaused`. An
        // app's dispatch is covered by that too, because `app_identity`
        // makes an app an `Issuer::Agent`. It is pulled from
        // `Lockdown::is_locked` rather than pushed into a flag, so `unlock`
        // releases agents without a thaw hook of its own — the thing this
        // freeze needed and had to grow `follow_lockdown` to get.
        //
        // What neither half does is stop a command that is ALREADY running.
        // Step 2 kills child processes; an in-process command already past
        // admission runs to completion. Admission is the line these two
        // hold.
        let mut inbox = self.0.locked_inbox();
        inbox.set_frozen(true);
        let pending = inbox.pending().map_err(|e| OutboundError(e.to_string()))?;
        let inbox_pending = u32::try_from(pending.len()).unwrap_or(u32::MAX);
        drop(inbox);

        tracing::warn!(
            pending = inbox_pending,
            "Inbox frozen: items already queued cannot be approved until lockdown is cleared"
        );
        Ok(Frozen {
            mail_queued: 0,
            inbox_pending,
            tasks_scheduled: 0,
        })
    }
}

// ---------------------------------------------------------------------------
// Bus commands
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct InboxParams {
    /// Absent lists what is waiting. A number lists that many recent items,
    /// decided ones included, for the history pane.
    #[serde(default)]
    pub history: Option<u32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ApproveParams {
    /// The Inbox item id.
    pub item: String,
    /// The count the operator was shown. Required for anything bulk or
    /// destructive; see [`Confirmation`].
    #[serde(default)]
    pub count: Option<u64>,
    /// "I am approving something that will not say how many items it touches."
    /// The only way to approve an item of unknown scale, which — see
    /// [`Facts::scale`] — is every item a bus dispatch produces today.
    #[serde(default)]
    pub accept_unknown: bool,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct DenyParams {
    pub item: String,
    /// Why, when the operator says. Kept with the item and shown in history.
    #[serde(default)]
    pub note: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct IssueParams {
    /// Operator-facing name for the grant: "Assistant", "coding on r620".
    pub label: String,
    /// Who holds it: the agent session's name, exactly as the bus knows it
    /// ([`AgentSessionId`]). This is what the grant is looked up by, so a typo
    /// here is a grant that authorizes nobody rather than one that authorizes
    /// everybody.
    pub agent: String,
    /// Patterns, as written: `git.*`, `files.read.status`.
    pub namespaces: Vec<String>,
    /// Node ids, or `["*"]` for every node. An empty list is refused: default
    /// deny applies to nodes exactly as it does to namespaces.
    #[serde(default)]
    pub nodes: Vec<String>,
    #[serde(default)]
    pub project: Option<String>,
    /// Lifetime in seconds. Absent asks for a grant that ends only when
    /// revoked, which [`Policy::allow_standing_grants`] may refuse.
    #[serde(default)]
    pub ttl_secs: Option<u64>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct RevokeParams {
    /// The grant id, as `system.grants` lists it.
    pub grant: String,
}

/// One grant as the operator reads it.
#[derive(Debug, Serialize)]
pub struct GrantListing {
    #[serde(flatten)]
    pub grant: Grant,
    pub owner: Option<String>,
    /// Computed rather than stored: a grant is active until it expires or is
    /// revoked, and a listing that showed a lapsed grant as live would be the
    /// one place the operator could misread what an agent can do.
    pub active: bool,
}

impl GrantListing {
    fn of(owned: OwnedGrant, now: u64) -> GrantListing {
        GrantListing {
            active: owned.grant.is_active(now),
            owner: owned.owner,
            grant: owned.grant,
        }
    }
}

/// An agent must not be able to decide its own queued command.
///
/// This is a grant question, not a risk question — they are different levers,
/// and conflating them is how `terminal.write` ended up with a confirmation
/// dialog on every keystroke — so it is enforced structurally here rather than
/// by hoping nobody ever writes `system.*` into a grant. Without it, an agent
/// holding `system.inbox.approve` could queue a destructive command and then
/// approve it, which is the whole design defeated in two dispatches.
fn operator_only(cx: &Context, what: &str) -> Result<(), CommandError> {
    if cx.issuer.is_agent() {
        return Err(failed(format!(
            "{what} is the operator's decision; an agent may not make it, whatever its grant says"
        )));
    }
    Ok(())
}

/// `system.inbox`: what is waiting for the operator.
///
/// [`RiskLevel::ReadOnly`]: it reads rows and changes nothing the operator
/// chose. Listing does sweep items past their TTL to `expired` on the way
/// through, which is [`Inbox::pending`]'s rule — the alternative is a stale
/// item still approvable from a page left open overnight.
pub struct ListInbox {
    approvals: Arc<Approvals>,
}

impl Command for ListInbox {
    const ID: &'static str = "system.inbox";
    const TITLE: &'static str = "Approval Inbox";
    const RISK: RiskLevel = RiskLevel::ReadOnly;

    type Params = InboxParams;
    type Output = Vec<PendingItem>;

    async fn run(&self, p: InboxParams, _cx: Context) -> Result<Vec<PendingItem>, CommandError> {
        // No await between taking the store's mutex and returning: the guard
        // never crosses an await point, which is what keeps this future `Send`.
        match p.history {
            Some(limit) => self
                .approvals
                .recent(limit.clamp(1, 500))
                .map_err(|e| failed(e.to_string())),
            None => self.approvals.pending().map_err(|e| failed(e.to_string())),
        }
    }
}

/// `system.inbox.approve`: release a queued command.
///
/// [`RiskLevel::Write`], and the level is the interesting decision in this
/// module.
///
/// Approving is how a destructive agent command actually runs, so
/// [`RiskLevel::Destructive`] is the tempting answer. It is the wrong one. The
/// risk level decides whether the *operator* is asked an ordinary "are you
/// sure" before their own action, and approving in the Inbox already is that
/// question: the operator is looking at a screen naming the command, the
/// issuer, the node, the reason it waited and its parameters, and
/// [`Confirmation`] makes them echo the count or accept an unbounded action
/// explicitly before anything moves. A second yes/no dialog in front of that
/// is the `terminal.write` mistake — ceremony that teaches the operator to
/// click through dialogs.
///
/// What the level must not do is let an agent approve its own item. That is
/// handled where it belongs, by [`operator_only`].
///
/// The dangerous act keeps its own risk: the released command goes through
/// [`Bus::dispatch_approved`], which re-checks the registry, the node, the
/// grant and the agent pause, and logs it under the agent that asked.
pub struct ApproveItem {
    approvals: Arc<Approvals>,
    /// Weak on purpose. The bus holds the registry, the registry holds this
    /// command, and this command needs the bus: a strong handle closes that
    /// cycle and nothing in it is ever dropped. A bus that has gone is a
    /// shutting-down server, and the approval then fails rather than running
    /// something nobody can log.
    bus: Weak<Bus>,
}

impl Command for ApproveItem {
    const ID: &'static str = "system.inbox.approve";
    const TITLE: &'static str = "Approve a waiting command";
    const RISK: RiskLevel = RiskLevel::Write;

    type Params = ApproveParams;
    type Output = Value;

    async fn run(&self, p: ApproveParams, cx: Context) -> Result<Value, CommandError> {
        operator_only(&cx, "approving a queued command")?;
        let bus = self
            .bus
            .upgrade()
            .ok_or_else(|| failed("the command bus is gone; nothing can be released now"))?;

        let confirm = match (p.count, p.accept_unknown) {
            (Some(count), _) => Confirmation::CountSeen(count),
            (None, true) => Confirmation::UnknownAccepted,
            (None, false) => Confirmation::Simple,
        };

        // Scoped so the Inbox's mutex is released before the await below: a
        // guard alive across an await point makes this future non-`Send`, and
        // it would hold every other approval behind a command that is running.
        let item = {
            let id = ItemId::from_text(p.item);
            self.approvals
                .approve(&id, confirm)
                .map_err(|e| failed(e.to_string()))?
        };

        let command = CommandId::new(item.command.clone()).map_err(|e| failed(e.to_string()))?;
        // The node the operator was shown is the node it runs on. Passing it
        // means a command whose backend has moved since it was queued is
        // refused by name rather than quietly running here.
        let request = Request::new(command, item.params.clone(), restore_issuer(&item.issuer))
            .on_node(NodeId::new(item.target_node.clone()));

        // The item is already recorded as approved, because the operator's
        // decision happened. A refusal from here is the grant check doing its
        // job — the session may have been revoked while the item waited — and
        // the bus writes that refusal to the audit log. The item cannot be
        // approved a second time, which is the right direction: nothing runs
        // twice because a dispatch failed once.
        let running = bus
            .dispatch_approved(
                request,
                Approval::granted(PendingId::new(item.id.to_string())),
            )
            .map_err(|e| failed(e.to_string()))?;

        let completion = running.finish().await;
        match completion.outcome {
            Ok(value) => Ok(serde_json::json!({
                "item": item.id.to_string(),
                "command": item.command,
                "dispatch": completion.dispatch.to_string(),
                "result": value,
            })),
            Err(e) => Err(failed(e.to_string())),
        }
    }
}

/// `system.inbox.deny`: refuse a queued command.
///
/// [`RiskLevel::Write`] rather than anything higher, for the same reason
/// [`Inbox::deny`] works during lockdown: stopping a queued action is never
/// the dangerous direction, and a confirmation dialog in front of the thing
/// the operator reaches for when something looks wrong is friction in exactly
/// the wrong place.
pub struct DenyItem {
    approvals: Arc<Approvals>,
}

impl Command for DenyItem {
    const ID: &'static str = "system.inbox.deny";
    const TITLE: &'static str = "Deny a waiting command";
    const RISK: RiskLevel = RiskLevel::Write;

    type Params = DenyParams;
    type Output = PendingItem;

    async fn run(&self, p: DenyParams, cx: Context) -> Result<PendingItem, CommandError> {
        operator_only(&cx, "denying a queued command")?;
        self.approvals
            .deny(&ItemId::from_text(p.item), p.note)
            .map_err(|e| failed(e.to_string()))
    }
}

/// `system.grants`: every grant, and whether it is still live.
///
/// [`RiskLevel::ReadOnly`]. It is the page the operator answers "what can the
/// agents do right now" from, so revoked and expired grants are listed too,
/// marked by the `active` flag rather than left out.
pub struct ListGrants {
    approvals: Arc<Approvals>,
}

impl Command for ListGrants {
    const ID: &'static str = "system.grants";
    const TITLE: &'static str = "Agent grants";
    const RISK: RiskLevel = RiskLevel::ReadOnly;

    type Params = crate::wiring::NoParams;
    type Output = Vec<GrantListing>;

    async fn run(
        &self,
        _p: crate::wiring::NoParams,
        _cx: Context,
    ) -> Result<Vec<GrantListing>, CommandError> {
        let now = self.approvals.now();
        let rows = self
            .approvals
            .all_grants()
            .map_err(|e| failed(e.to_string()))?;
        Ok(rows
            .into_iter()
            .map(|owned| GrantListing::of(owned, now))
            .collect())
    }
}

/// `system.grant.issue`: hand an agent session a scope.
///
/// [`RiskLevel::CrossProject`], so the operator confirms, matching
/// `system.app.approve` for the same reason: this is the moment authority is
/// handed to something that will then act on its own, and it is the only
/// moment the operator is asked. Not [`RiskLevel::Destructive`], which would
/// be a claim about what issuing destroys; what it does is widen.
pub struct IssueGrant {
    approvals: Arc<Approvals>,
}

impl Command for IssueGrant {
    const ID: &'static str = "system.grant.issue";
    const TITLE: &'static str = "Issue an agent grant";
    const RISK: RiskLevel = RiskLevel::CrossProject;

    type Params = IssueParams;
    type Output = GrantListing;

    async fn run(&self, p: IssueParams, cx: Context) -> Result<GrantListing, CommandError> {
        operator_only(&cx, "issuing a grant")?;
        let nodes = match p.nodes.as_slice() {
            // `["*"]` and nothing else: a list with `*` beside real node ids
            // is ambiguous about which the operator meant, and reading it as
            // "all" would be the permissive guess.
            [one] if one == "*" => Some(grants::NodeScope::All),
            [] => None,
            ids => Some(grants::NodeScope::Only(ids.to_vec())),
        };

        let owner = p.agent;
        let issued = self
            .approvals
            .issue(
                &owner,
                GrantSpec {
                    label: p.label,
                    // Not the agent's name: that column is a browser or agent
                    // *session id*, and this is a name. The holder goes in
                    // `owner`, the column this subsystem looks grants up by.
                    session: None,
                    project: p.project,
                    namespaces: p.namespaces,
                    nodes,
                    ttl_secs: p.ttl_secs,
                },
            )
            .map_err(|e| failed(e.to_string()))?;

        let now = self.approvals.now();
        Ok(GrantListing::of(issued, now))
    }
}

/// `system.grant.revoke`: take a scope back.
///
/// [`RiskLevel::Write`]. Revoking is the safe direction — it is what the
/// operator reaches for when an agent is doing something they did not
/// expect — and asking "are you sure" before pausing an agent is friction on
/// the brake pedal. Anything already queued under the grant is re-checked when
/// it is approved, so a revocation reaches the Inbox as well as the next
/// dispatch.
pub struct RevokeGrant {
    approvals: Arc<Approvals>,
}

impl Command for RevokeGrant {
    const ID: &'static str = "system.grant.revoke";
    const TITLE: &'static str = "Revoke an agent grant";
    const RISK: RiskLevel = RiskLevel::Write;

    type Params = RevokeParams;
    type Output = Value;

    async fn run(&self, p: RevokeParams, cx: Context) -> Result<Value, CommandError> {
        operator_only(&cx, "revoking a grant")?;
        let id = GrantId::from_text(p.grant);
        let revoked = self
            .approvals
            .revoke(&id)
            .map_err(|e| failed(e.to_string()))?;
        Ok(serde_json::json!({ "grant": id.to_string(), "revoked": revoked }))
    }
}

/// Put the grant and Inbox commands on the bus. One line in `main.rs`.
///
/// Takes the bus because the approval command dispatches what it releases; it
/// keeps a [`Weak`] handle, so call this after the bus exists. The two facades
/// are attached to the bus separately, with [`Approvals::grants`] and
/// [`Approvals::inbox`] — both of them, or an approved item is refused for
/// want of a grant.
pub fn register_approvals(
    registry: &Registry,
    approvals: &Arc<Approvals>,
    bus: &Arc<Bus>,
) -> Result<(), RegistryError> {
    registry.register(ListInbox {
        approvals: Arc::clone(approvals),
    })?;
    registry.register(ApproveItem {
        approvals: Arc::clone(approvals),
        bus: Arc::downgrade(bus),
    })?;
    registry.register(DenyItem {
        approvals: Arc::clone(approvals),
    })?;
    registry.register(ListGrants {
        approvals: Arc::clone(approvals),
    })?;
    registry.register(IssueGrant {
        approvals: Arc::clone(approvals),
    })?;
    registry.register(RevokeGrant {
        approvals: Arc::clone(approvals),
    })?;
    Ok(())
}
