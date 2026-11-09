//! Scoped grants and the approval Inbox.
//!
//! Two rules from the design meet here, and the difference between them is
//! the whole point of this module:
//!
//! - **Outside the grant is refused, never queued.** A session holds a set of
//!   command namespaces (`git.*`, `files.read.*`, `filmcraft.timeline.*`) and
//!   the nodes it may target. A command it was not granted does not become a
//!   question for the operator; it is denied. Queueing it would turn the
//!   Inbox into a channel an ungranted agent could use to ask for anything,
//!   and the operator into a rubber stamp.
//! - **Inside the grant, risk decides.** Read-only commands run. Writes
//!   inside a project run under the grant. Destructive, outbound and
//!   cross-project commands go to the Inbox and wait.
//!
//! The risk level comes from the backend that declared the command, never
//! from the caller or the UI, so an adapter bug cannot downgrade a
//! destructive command. This module only reads it, through [`CommandFacts`] —
//! the one narrow seam to the command bus.
//!
//! **Counts before approval.** An agent elsewhere deleted most of a
//! researcher's mail one message at a time; each delete looked small. So a
//! write whose blast radius is more than a handful of items is queued even
//! when its own risk level is ordinary, the count travels with the pending
//! item, and [`Inbox::approve`] refuses an approval that does not echo the
//! count the operator was shown. A UI that forgets to display the number
//! cannot approve a sweep.
//!
//! Time, randomness and storage are injected ([`Env`], [`GrantStore`],
//! [`InboxStore`]) so every rule above is exercised without a clock or a
//! database.
//!
//! ```text
//! let grants = Grants::new(store.clone(), SystemEnv, Policy::default());
//! let grant = grants.issue(GrantSpec {
//!     label: "Assistant".into(),
//!     session: Some(session_id.encode()),
//!     namespaces: vec!["git.*".into(), "files.read.*".into()],
//!     nodes: NodeScope::Only(vec!["r620".into()]),
//!     ttl_secs: Some(4 * 3600),
//!     ..GrantSpec::default()
//! })?;
//!
//! match decide(&grant, &cmd, &Policy::default(), now) {
//!     Decision::Run => bus.dispatch(cmd),
//!     Decision::Queue { reason, scale } => {
//!         inbox.queue(QueueRequest::from_command(&grant, &cmd, reason, scale))?;
//!     }
//!     Decision::Refuse(reason) => return Err(reason.into()),
//! }
//! ```

use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::auth::code::{RandomError, os_random};

// ---------------------------------------------------------------------------
// Injected world
// ---------------------------------------------------------------------------

/// The clock and the random source, supplied rather than read, so expiry and
/// id generation are testable without sleeping.
pub trait Env: Send + Sync {
    /// Unix seconds now.
    fn now(&self) -> u64;

    /// A fresh id for a grant or an Inbox item.
    ///
    /// Unguessable on purpose. Ids are not secrets — approval always requires
    /// an operator session — but an id an agent can predict is an id it can
    /// name in a request, and there is no reason to hand it that.
    fn new_id(&self) -> Result<String, RandomError>;
}

/// The real one.
pub struct SystemEnv;

impl Env for SystemEnv {
    fn now(&self) -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0)
    }

    fn new_id(&self) -> Result<String, RandomError> {
        use base64::Engine as _;
        use base64::engine::general_purpose::URL_SAFE_NO_PAD;
        let mut b = [0u8; 16];
        os_random(&mut b)?;
        Ok(URL_SAFE_NO_PAD.encode(b))
    }
}

// ---------------------------------------------------------------------------
// The seam to the command bus
// ---------------------------------------------------------------------------

/// What this module needs to know about a command it is deciding on.
///
/// Deliberately the whole coupling to `crate::command`: the bus owns command
/// ids, parameter schemas and risk levels, and routing owns none of them. A
/// command type implements this, or the bus hands over a small adapter that
/// does.
pub trait CommandFacts {
    /// The command's dotted id, as the bus declares it: `git.commit`,
    /// `mail.send`, `filmcraft.timeline.ripple_delete`. This is what grant
    /// namespaces match against.
    fn id(&self) -> &str;

    /// The risk level **the backend declared**. Never inferred here.
    fn risk(&self) -> Risk;

    /// The node this invocation targets.
    fn target_node(&self) -> &str;

    /// How many things this invocation touches, if the command can say.
    /// Defaults to one, which is what most commands are.
    fn scale(&self) -> Scale {
        Scale::Single
    }
}

/// Risk level of a command, as declared by its backend.
///
/// The four routed-to-Inbox levels are kept apart rather than collapsed into
/// one "risky", because the operator reads them: "sends mail off this machine"
/// and "deletes files" are different questions even though both wait.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Risk {
    /// Reads state and changes nothing.
    ReadOnly,
    /// Changes state inside the project the grant covers.
    Write,
    /// Removes or overwrites something that is not trivially recoverable.
    Destructive,
    /// Leaves this machine: mail, invites, posts, uploads.
    Outbound,
    /// Touches state outside the grant's project.
    CrossProject,
}

impl Risk {
    /// Column text for storage, and the wire name.
    pub fn as_str(self) -> &'static str {
        match self {
            Risk::ReadOnly => "read_only",
            Risk::Write => "write",
            Risk::Destructive => "destructive",
            Risk::Outbound => "outbound",
            Risk::CrossProject => "cross_project",
        }
    }

    pub fn parse(s: &str) -> Option<Risk> {
        match s {
            "read_only" => Some(Risk::ReadOnly),
            "write" => Some(Risk::Write),
            "destructive" => Some(Risk::Destructive),
            "outbound" => Some(Risk::Outbound),
            "cross_project" => Some(Risk::CrossProject),
            _ => None,
        }
    }

    /// Whether this level always waits for approval, whatever its scale.
    pub fn always_waits(self) -> bool {
        matches!(
            self,
            Risk::Destructive | Risk::Outbound | Risk::CrossProject
        )
    }

    /// Whether this level changes anything. Scale only matters for these: a
    /// command that reads ten thousand files is not the failure the design
    /// is guarding against, one that deletes ten thousand is.
    pub fn mutates(self) -> bool {
        !matches!(self, Risk::ReadOnly)
    }

    /// The queue reason this level implies, if it always waits.
    fn queue_reason(self) -> Option<QueueReason> {
        match self {
            Risk::Destructive => Some(QueueReason::Destructive),
            Risk::Outbound => Some(QueueReason::Outbound),
            Risk::CrossProject => Some(QueueReason::CrossProject),
            Risk::ReadOnly | Risk::Write => None,
        }
    }
}

impl fmt::Display for Risk {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// How many items an invocation affects.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind", content = "count")]
pub enum Scale {
    /// One thing: this file, this commit, this message.
    Single,
    /// A known number of things.
    Count(u64),
    /// The command cannot say in advance — a glob, a query, a recursive
    /// sweep. Treated as the dangerous case, not the harmless one.
    Unknown,
}

impl Scale {
    /// The number to show a human, when there is one.
    pub fn count(self) -> Option<u64> {
        match self {
            Scale::Single => Some(1),
            Scale::Count(n) => Some(n),
            Scale::Unknown => None,
        }
    }

    /// Storage split: a kind column and a nullable count column.
    pub fn as_parts(self) -> (&'static str, Option<u64>) {
        match self {
            Scale::Single => ("single", Some(1)),
            Scale::Count(n) => ("count", Some(n)),
            Scale::Unknown => ("unknown", None),
        }
    }

    pub fn from_parts(kind: &str, count: Option<u64>) -> Option<Scale> {
        match (kind, count) {
            ("single", _) => Some(Scale::Single),
            ("count", Some(n)) => Some(Scale::Count(n)),
            ("unknown", _) => Some(Scale::Unknown),
            _ => None,
        }
    }
}

impl fmt::Display for Scale {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Scale::Single => f.write_str("1 item"),
            Scale::Count(n) => write!(f, "{n} items"),
            Scale::Unknown => f.write_str("an unknown number of items"),
        }
    }
}

// ---------------------------------------------------------------------------
// Namespace patterns
// ---------------------------------------------------------------------------

/// A command-namespace pattern: `git.*`, `files.read.*`, `mail.send`.
///
/// Dot-separated segments. `*` is a whole segment, never part of one, so
/// `git.pu*` is rejected rather than quietly matching `git.push` — a pattern
/// that half-works is a permission bug waiting to happen.
///
/// A trailing `*` matches one or more remaining segments. It does **not**
/// match zero: `git.*` grants everything inside the `git` namespace, and a
/// command whose whole id is `git` has to be granted as `git`. Prefix
/// patterns describe what is inside a namespace, and an operator reading
/// `files.read.*` does not expect it to include a bare `files.read` command.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Pattern {
    segments: Vec<Segment>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Segment {
    Literal(String),
    /// `*` in a middle position: exactly one segment.
    One,
    /// `*` in the final position: one or more segments.
    Rest,
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum PatternError {
    #[error("a namespace pattern cannot be empty; write something like \"git.*\"")]
    Empty,

    #[error("\"{pattern}\" has an empty segment; write \"git.*\", not \"git..*\" or \"git.\"")]
    EmptySegment { pattern: String },

    #[error(
        "\"{pattern}\" mixes \"*\" with other characters in one segment; \
         \"*\" must be a whole segment, so write \"git.*\" rather than \"git.pu*\""
    )]
    PartialWildcard { pattern: String },

    #[error(
        "\"{pattern}\" contains {bad:?}; command namespaces use lowercase letters, \
         digits, \"_\", \"-\" and \".\""
    )]
    BadCharacter { pattern: String, bad: char },
}

impl Pattern {
    pub fn parse(pattern: &str) -> Result<Pattern, PatternError> {
        if pattern.is_empty() {
            return Err(PatternError::Empty);
        }
        let raw: Vec<&str> = pattern.split('.').collect();
        let last = raw.len() - 1;
        let mut segments = Vec::with_capacity(raw.len());

        for (i, part) in raw.into_iter().enumerate() {
            if part.is_empty() {
                return Err(PatternError::EmptySegment {
                    pattern: pattern.to_string(),
                });
            }
            if part.contains('*') {
                if part != "*" {
                    return Err(PatternError::PartialWildcard {
                        pattern: pattern.to_string(),
                    });
                }
                segments.push(if i == last {
                    Segment::Rest
                } else {
                    Segment::One
                });
                continue;
            }
            if let Some(bad) = part
                .chars()
                .find(|c| !(c.is_ascii_lowercase() || c.is_ascii_digit() || *c == '_' || *c == '-'))
            {
                return Err(PatternError::BadCharacter {
                    pattern: pattern.to_string(),
                    bad,
                });
            }
            segments.push(Segment::Literal(part.to_string()));
        }

        Ok(Pattern { segments })
    }

    /// Whether this pattern grants `command_id`.
    ///
    /// Matching is exact on literals and case-sensitive: command ids are
    /// lowercase by convention, and case-folding a permission check invites
    /// the UI and the bus to disagree about what was granted.
    pub fn matches(&self, command_id: &str) -> bool {
        let parts: Vec<&str> = command_id.split('.').collect();
        if parts.iter().any(|p| p.is_empty()) {
            // Not a command id the bus can have issued; refuse by default.
            return false;
        }

        let mut i = 0;
        for (n, segment) in self.segments.iter().enumerate() {
            match segment {
                Segment::Rest => {
                    // Last segment by construction: everything left matches,
                    // as long as there is at least one segment left.
                    debug_assert_eq!(n, self.segments.len() - 1);
                    return i < parts.len();
                }
                Segment::One => {
                    if i >= parts.len() {
                        return false;
                    }
                    i += 1;
                }
                Segment::Literal(lit) => {
                    if parts.get(i) != Some(&lit.as_str()) {
                        return false;
                    }
                    i += 1;
                }
            }
        }
        i == parts.len()
    }

    /// Whether this pattern grants the entire bus: `*`, and equally `*.*` or
    /// any other pattern with no literal segment in it.
    ///
    /// Every command id the bus declares is dotted (`terminal.write`,
    /// `git.status`), so `*.*` is the whole bus written a second way. Reading
    /// only a bare `*` as unrestricted hid such a grant from both the UI flag
    /// and [`Grants::issue`], which is the one place that can refuse it.
    pub fn is_everything(&self) -> bool {
        !self
            .segments
            .iter()
            .any(|s| matches!(s, Segment::Literal(_)))
    }
}

impl fmt::Display for Pattern {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (i, segment) in self.segments.iter().enumerate() {
            if i > 0 {
                f.write_str(".")?;
            }
            match segment {
                Segment::Literal(lit) => f.write_str(lit)?,
                Segment::One | Segment::Rest => f.write_str("*")?,
            }
        }
        Ok(())
    }
}

impl TryFrom<String> for Pattern {
    type Error = PatternError;

    fn try_from(s: String) -> Result<Pattern, PatternError> {
        Pattern::parse(&s)
    }
}

impl From<Pattern> for String {
    fn from(p: Pattern) -> String {
        p.to_string()
    }
}

/// Which nodes a grant may target.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NodeScope {
    /// Every node the shell knows about. Explicit, so it can be shown as
    /// what it is.
    All,
    /// Only these node ids. An empty list allows nothing, which is the right
    /// reading of "no nodes granted".
    Only(Vec<String>),
}

impl NodeScope {
    pub fn allows(&self, node_id: &str) -> bool {
        match self {
            NodeScope::All => true,
            NodeScope::Only(ids) => ids.iter().any(|id| id == node_id),
        }
    }

    /// Single-column storage: `*`, or newline-separated node ids. Node ids
    /// never contain a newline (see [`crate::node::id`]), so this round-trips.
    pub fn as_text(&self) -> String {
        match self {
            NodeScope::All => "*".to_string(),
            NodeScope::Only(ids) => ids.join("\n"),
        }
    }

    pub fn from_text(text: &str) -> NodeScope {
        if text == "*" {
            return NodeScope::All;
        }
        NodeScope::Only(
            text.lines()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect(),
        )
    }
}

// ---------------------------------------------------------------------------
// Grants
// ---------------------------------------------------------------------------

/// A grant's id. Not a secret, so it gets a plain `Display`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct GrantId(String);

impl GrantId {
    pub fn from_text(s: impl Into<String>) -> GrantId {
        GrantId(s.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for GrantId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// What one session is allowed to do.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Grant {
    pub id: GrantId,
    /// Shown wherever this grant is named: "Assistant", "coding session on
    /// r620". Operator-facing, not an identifier.
    pub label: String,
    /// The signed-in session or agent session this grant belongs to, if it
    /// dies with one. `None` is a standing grant, such as the always-on
    /// assistant's.
    pub session: Option<String>,
    /// The project root the grant is scoped to, for display and for the
    /// operator's own audit. Cross-project detection happens in the backend
    /// that declares the command, which is the only place that knows what a
    /// project means for it.
    pub project: Option<String>,
    pub namespaces: Vec<Pattern>,
    pub nodes: NodeScope,
    pub issued_at: u64,
    /// Unix seconds. `None` is a grant that only ends when revoked; the
    /// always-on assistant has one, agent sessions should not.
    pub expires_at: Option<u64>,
    pub revoked_at: Option<u64>,
}

impl Grant {
    /// Why this grant cannot be used, if it cannot.
    pub fn inactive_reason(&self, now: u64) -> Option<RefuseReason> {
        if let Some(at) = self.revoked_at {
            return Some(RefuseReason::Revoked {
                grant: self.label.clone(),
                at,
            });
        }
        match self.expires_at {
            Some(at) if now >= at => Some(RefuseReason::Expired {
                grant: self.label.clone(),
                at,
            }),
            _ => None,
        }
    }

    pub fn is_active(&self, now: u64) -> bool {
        self.inactive_reason(now).is_none()
    }

    /// Prefix/glob matching, refusing by default: a command matches only if
    /// some pattern says so. An empty namespace list therefore allows
    /// nothing, which is why [`Grants::issue`] refuses to create one.
    pub fn allows_command(&self, command_id: &str) -> bool {
        self.namespaces.iter().any(|p| p.matches(command_id))
    }

    pub fn allows_node(&self, node_id: &str) -> bool {
        self.nodes.allows(node_id)
    }

    /// Whether this grant covers the whole bus, so the UI can mark it.
    pub fn is_unrestricted(&self) -> bool {
        self.namespaces.iter().any(Pattern::is_everything)
    }

    /// The namespace column: one pattern per line, as written.
    pub fn namespaces_text(&self) -> String {
        self.namespaces
            .iter()
            .map(Pattern::to_string)
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Read back [`Grant::namespaces_text`]. A line that no longer parses is
    /// dropped with a warning rather than failing the whole grant: the
    /// failure mode of dropping a pattern is a refused command the operator
    /// can widen, and the failure mode of keeping it is unclear.
    pub fn parse_namespaces(text: &str) -> Vec<Pattern> {
        text.lines()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .filter_map(|line| match Pattern::parse(line) {
                Ok(p) => Some(p),
                Err(e) => {
                    tracing::warn!(pattern = line, error = %e, "dropping unreadable grant pattern");
                    None
                }
            })
            .collect()
    }
}

/// What the caller asks for when issuing a grant.
#[derive(Debug, Clone, Default)]
pub struct GrantSpec {
    pub label: String,
    pub session: Option<String>,
    pub project: Option<String>,
    /// Patterns as text; parsed and validated by [`Grants::issue`] so a typo
    /// is an error at issuance rather than a silently narrower grant.
    pub namespaces: Vec<String>,
    pub nodes: Option<NodeScope>,
    /// Lifetime in seconds. `None` asks for a standing grant, which
    /// [`Policy::allow_standing_grants`] may refuse.
    pub ttl_secs: Option<u64>,
}

// ---------------------------------------------------------------------------
// Routing
// ---------------------------------------------------------------------------

/// Tunables that are policy rather than mechanism.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Policy {
    /// A write touching at least this many items waits for approval even when
    /// its declared risk is ordinary. A handful of files is an edit; a sweep
    /// is a different act, and the design asks for a count before one.
    pub bulk_threshold: u64,
    /// Whether a write that cannot say how much it touches waits. Default
    /// yes: "I don't know how many" is the shape the researcher's mail
    /// deletion had.
    pub queue_unknown_scale: bool,
    /// How long a pending item stays approvable. An approval tapped a week
    /// late applies to a project that has moved on, so the item expires
    /// instead of executing.
    pub pending_ttl_secs: Option<u64>,
    /// Longest lifetime a grant may be issued for.
    pub max_grant_ttl_secs: u64,
    /// Whether a grant with no expiry may be issued. The always-on assistant
    /// needs one; agent sessions should not have one, so callers that issue
    /// session grants pass a policy with this off.
    pub allow_standing_grants: bool,
    /// Whether a grant may name a pattern covering the whole bus (`*`, `*.*`).
    ///
    /// Default no. "Anything outside the grant is refused" is the rule this
    /// module exists for, and a grant of everything puts nothing outside it,
    /// so the rule stops meaning anything. The operator's own actions do not
    /// route through this module at all (see [`Issuer`]), so the tooling that
    /// genuinely wants the whole bus opts in here, deliberately, instead of
    /// every caller getting it by typing one character.
    pub allow_unrestricted_grants: bool,
}

impl Default for Policy {
    fn default() -> Policy {
        Policy {
            // Five is a judgment call, not a number from the design: enough
            // that ordinary multi-file edits are not constant interruptions,
            // small enough that nothing resembling a sweep slips past.
            bulk_threshold: 5,
            queue_unknown_scale: true,
            pending_ttl_secs: Some(24 * 3600),
            max_grant_ttl_secs: 12 * 3600,
            allow_standing_grants: true,
            allow_unrestricted_grants: false,
        }
    }
}

/// What to do with a command, given its risk and the issuer's grant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// Dispatch now, attributed to the grant.
    Run,
    /// Put it in the Inbox and tell the agent it is waiting.
    Queue { reason: QueueReason, scale: Scale },
    /// Deny it. Not queued: see the module docs.
    Refuse(RefuseReason),
}

impl Decision {
    pub fn is_run(&self) -> bool {
        matches!(self, Decision::Run)
    }

    /// Ordering used when a session holds more than one grant: the most
    /// permissive outcome wins, because holding two grants means holding
    /// their union.
    fn rank(&self) -> u8 {
        match self {
            Decision::Run => 2,
            Decision::Queue { .. } => 1,
            Decision::Refuse(_) => 0,
        }
    }
}

/// Why an in-grant command is waiting. The operator reads these.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QueueReason {
    Destructive,
    Outbound,
    CrossProject,
    /// A write over [`Policy::bulk_threshold`] items.
    Bulk {
        count: u64,
    },
    /// A write that would not say how much it touches.
    ScaleUnknown,
}

impl QueueReason {
    pub fn as_str(&self) -> &'static str {
        match self {
            QueueReason::Destructive => "destructive",
            QueueReason::Outbound => "outbound",
            QueueReason::CrossProject => "cross_project",
            QueueReason::Bulk { .. } => "bulk",
            QueueReason::ScaleUnknown => "scale_unknown",
        }
    }
}

impl fmt::Display for QueueReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            QueueReason::Destructive => f.write_str("destroys something"),
            QueueReason::Outbound => f.write_str("leaves this machine"),
            QueueReason::CrossProject => f.write_str("reaches outside this project"),
            QueueReason::Bulk { count } => write!(f, "changes {count} items at once"),
            QueueReason::ScaleUnknown => f.write_str("will not say how many items it changes"),
        }
    }
}

/// Why a command was denied outright. Every message says what to do about it,
/// because the operator reading it in the log is deciding whether to widen a
/// grant or to go look at what the agent is doing.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum RefuseReason {
    #[error(
        "{issuer} holds no active grant, so \"{command}\" was refused; \
         issue a grant for the namespaces it needs"
    )]
    NoGrant { issuer: String, command: String },

    #[error(
        "\"{command}\" is outside the grant \"{grant}\" ({granted}), so it was refused, \
         not queued; widen the grant if this session should have it"
    )]
    OutsideNamespaces {
        grant: String,
        command: String,
        granted: String,
    },

    #[error(
        "the grant \"{grant}\" may not target node {node}, so \"{command}\" was refused; \
         add the node to the grant if this session should reach it"
    )]
    NodeNotGranted {
        grant: String,
        command: String,
        node: String,
    },

    #[error("the grant \"{grant}\" expired at {at} (unix); issue a new one to continue")]
    Expired { grant: String, at: u64 },

    #[error(
        "the grant \"{grant}\" was revoked at {at} (unix); \
         if this was the kill switch, clear lockdown before reissuing"
    )]
    Revoked { grant: String, at: u64 },
}

/// Route one command against one grant.
pub fn decide(grant: &Grant, cmd: &dyn CommandFacts, policy: &Policy, now: u64) -> Decision {
    // Grant validity first. An expired grant is not a narrower grant, and a
    // destructive command under one must not reach the Inbox: queueing it
    // would let a revoked session keep asking.
    if let Some(reason) = grant.inactive_reason(now) {
        return Decision::Refuse(reason);
    }

    if !grant.allows_command(cmd.id()) {
        return Decision::Refuse(RefuseReason::OutsideNamespaces {
            grant: grant.label.clone(),
            command: cmd.id().to_string(),
            granted: grant.namespaces_text().replace('\n', ", "),
        });
    }

    if !grant.allows_node(cmd.target_node()) {
        return Decision::Refuse(RefuseReason::NodeNotGranted {
            grant: grant.label.clone(),
            command: cmd.id().to_string(),
            node: cmd.target_node().to_string(),
        });
    }

    let risk = cmd.risk();
    let scale = cmd.scale();

    if let Some(reason) = risk.queue_reason() {
        return Decision::Queue { reason, scale };
    }

    // Scale only escalates writes. Reading widely is not the hazard here.
    if risk.mutates() {
        match scale {
            Scale::Count(n) if n >= policy.bulk_threshold => {
                return Decision::Queue {
                    reason: QueueReason::Bulk { count: n },
                    scale,
                };
            }
            Scale::Unknown if policy.queue_unknown_scale => {
                return Decision::Queue {
                    reason: QueueReason::ScaleUnknown,
                    scale,
                };
            }
            _ => {}
        }
    }

    Decision::Run
}

/// Route against every grant a session holds, most permissive outcome wins.
/// With no grants at all the answer is a refusal naming the issuer, which is
/// what the log should show.
pub fn decide_any(
    grants: &[Grant],
    issuer: &Issuer,
    cmd: &dyn CommandFacts,
    policy: &Policy,
    now: u64,
) -> Decision {
    let mut best: Option<Decision> = None;
    for grant in grants {
        let d = decide(grant, cmd, policy, now);
        if d.is_run() {
            return d;
        }
        if best.as_ref().is_none_or(|b| d.rank() > b.rank()) {
            best = Some(d);
        }
    }
    best.unwrap_or_else(|| {
        Decision::Refuse(RefuseReason::NoGrant {
            issuer: issuer.label().to_string(),
            command: cmd.id().to_string(),
        })
    })
}

// ---------------------------------------------------------------------------
// Attribution
// ---------------------------------------------------------------------------

/// Who issued a command. The operator's own path does not come through this
/// module at all — my actions run directly, with ordinary confirmations — but
/// [`Issuer::Operator`] exists because an Inbox item records who it came
/// from, and an operator-initiated item (a confirmation deferred from the
/// phone, say) is a real case.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum Issuer {
    Operator,
    Cli,
    /// A named agent session. `session` is the session id it signed in with,
    /// when there is one.
    Session {
        label: String,
        session: Option<String>,
    },
}

impl Issuer {
    pub fn label(&self) -> &str {
        match self {
            Issuer::Operator => "the operator",
            Issuer::Cli => "the CLI",
            Issuer::Session { label, .. } => label,
        }
    }

    /// The session id this issuer's grants hang off, if any.
    pub fn session(&self) -> Option<&str> {
        match self {
            Issuer::Session { session, .. } => session.as_deref(),
            Issuer::Operator | Issuer::Cli => None,
        }
    }
}

// ---------------------------------------------------------------------------
// The Inbox
// ---------------------------------------------------------------------------

/// An Inbox item's id.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ItemId(String);

impl ItemId {
    pub fn from_text(s: impl Into<String>) -> ItemId {
        ItemId(s.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ItemId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ItemStatus {
    Pending,
    Approved,
    Denied,
    /// Sat too long to be meaningful; see [`Policy::pending_ttl_secs`].
    Expired,
    /// The issuer gave up or its session ended before a decision.
    Withdrawn,
}

impl ItemStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            ItemStatus::Pending => "pending",
            ItemStatus::Approved => "approved",
            ItemStatus::Denied => "denied",
            ItemStatus::Expired => "expired",
            ItemStatus::Withdrawn => "withdrawn",
        }
    }

    pub fn parse(s: &str) -> Option<ItemStatus> {
        match s {
            "pending" => Some(ItemStatus::Pending),
            "approved" => Some(ItemStatus::Approved),
            "denied" => Some(ItemStatus::Denied),
            "expired" => Some(ItemStatus::Expired),
            "withdrawn" => Some(ItemStatus::Withdrawn),
            _ => None,
        }
    }
}

/// A command waiting for approval, carrying everything needed to show a human
/// what they are about to allow: who asked, what command, on which node, why
/// it is waiting, how much it touches, and the parameters themselves.
///
/// `params` is **data**. It is rendered for the operator and handed back to
/// the bus on approval; nothing in this module interprets it, and nothing in
/// it may become a command of its own.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingItem {
    pub id: ItemId,
    /// The grant the command was issued under, if it had one.
    pub grant: Option<GrantId>,
    pub issuer: Issuer,
    /// The bus command id, e.g. `mail.send`.
    pub command: String,
    pub target_node: String,
    pub risk: Risk,
    pub reason: QueueReason,
    /// The count the operator must be shown before approving.
    pub scale: Scale,
    /// One line, written by the backend that declared the command:
    /// "Send 3 messages", "Delete 412 files under ~/Footage".
    pub title: String,
    /// Optional longer explanation, already human-readable.
    pub detail: Option<String>,
    pub params: serde_json::Value,
    pub created: u64,
    /// When this stops being approvable.
    pub expires: Option<u64>,
    pub status: ItemStatus,
    pub decided_at: Option<u64>,
    /// Why the operator denied it, when they said.
    pub decided_note: Option<String>,
}

impl PendingItem {
    /// Whether approving this needs the operator to echo a count. True for
    /// anything bulk, destructive or unmeasured — the three shapes the
    /// design says must show a number first.
    pub fn needs_count_confirmation(&self) -> bool {
        matches!(
            self.reason,
            QueueReason::Bulk { .. } | QueueReason::ScaleUnknown
        ) || self.risk == Risk::Destructive
            || self.scale == Scale::Unknown
            || self.scale.count().is_some_and(|n| n > 1)
    }

    pub fn is_expired(&self, now: u64) -> bool {
        self.expires.is_some_and(|at| now >= at)
    }
}

/// What a caller passes to put something in the Inbox. Built from a
/// [`Decision::Queue`] plus the command's own description of itself.
#[derive(Debug, Clone)]
pub struct QueueRequest {
    pub grant: Option<GrantId>,
    pub issuer: Issuer,
    pub command: String,
    pub target_node: String,
    pub risk: Risk,
    pub reason: QueueReason,
    pub scale: Scale,
    pub title: String,
    pub detail: Option<String>,
    pub params: serde_json::Value,
}

impl QueueRequest {
    /// The usual construction: the command describes itself, the decision
    /// supplies the reason and the scale.
    pub fn from_command(
        grant: Option<&Grant>,
        issuer: Issuer,
        cmd: &dyn CommandFacts,
        reason: QueueReason,
        scale: Scale,
        title: impl Into<String>,
        params: serde_json::Value,
    ) -> QueueRequest {
        QueueRequest {
            grant: grant.map(|g| g.id.clone()),
            issuer,
            command: cmd.id().to_string(),
            target_node: cmd.target_node().to_string(),
            risk: cmd.risk(),
            reason,
            scale,
            title: title.into(),
            detail: None,
            params,
        }
    }
}

/// How the operator confirms an approval.
///
/// [`Confirmation::CountSeen`] is not bookkeeping: the Inbox refuses to
/// approve a bulk or destructive item unless the approver states the number
/// they were shown and it matches the item. A client that never renders the
/// count physically cannot approve such an item, which is the guardrail the
/// design asks for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Confirmation {
    /// For items that affect one known thing.
    Simple,
    /// "I saw that this affects N items."
    CountSeen(u64),
    /// For an item whose scale is unknown: the operator is approving
    /// something unbounded and says so explicitly.
    UnknownAccepted,
}

/// An approved item, handed back to the bus to dispatch. The bus re-validates
/// the parameters against the command's schema; nothing here does.
#[derive(Debug, Clone)]
pub struct Approved {
    pub item: PendingItem,
}

#[derive(Debug, Error)]
pub enum InboxError {
    #[error("no Inbox item {id}")]
    NotFound { id: ItemId },

    #[error("Inbox item {id} is already {status}, so it cannot be decided again")]
    NotPending { id: ItemId, status: &'static str },

    #[error(
        "Inbox item {id} expired after waiting {waited}s; \
         the agent must ask again against current state"
    )]
    Expired { id: ItemId, waited: u64 },

    #[error(
        "approving \"{command}\" affects {scale}, so the approval must state the count \
         the operator was shown; the client did not"
    )]
    CountRequired { command: String, scale: Scale },

    #[error(
        "the approval says {shown} items but \"{command}\" now affects {actual}; \
         refused, because what was approved is not what would run"
    )]
    CountMismatch {
        command: String,
        shown: u64,
        actual: u64,
    },

    #[error(
        "approvals are frozen: the system is in lockdown, which freezes pending \
         Inbox items; clear lockdown first"
    )]
    Frozen,

    #[error(transparent)]
    Random(#[from] RandomError),

    #[error(transparent)]
    Storage(#[from] StorageError),
}

/// The approval queue.
///
/// Generic over its storage and its clock so the routing rules can be driven
/// without a database or a wall clock. `S` is typically an
/// `Arc<dyn InboxStore>` held in the HTTP context.
pub struct Inbox<S, E> {
    store: S,
    env: E,
    policy: Policy,
    /// Lockdown freezes outbound actions and pending items. Held here rather
    /// than read from global state so the freeze is explicit at the call
    /// site that set it.
    frozen: bool,
}

impl<S: InboxStore, E: Env> Inbox<S, E> {
    pub fn new(store: S, env: E, policy: Policy) -> Inbox<S, E> {
        Inbox {
            store,
            env,
            policy,
            frozen: false,
        }
    }

    /// Lockdown: pending items stay visible but nothing can be approved.
    /// Denial still works, because stopping a queued action is never the
    /// dangerous direction.
    pub fn set_frozen(&mut self, frozen: bool) {
        self.frozen = frozen;
    }

    pub fn is_frozen(&self) -> bool {
        self.frozen
    }

    pub fn policy(&self) -> &Policy {
        &self.policy
    }

    /// Queue a command for approval.
    pub fn queue(&self, req: QueueRequest) -> Result<PendingItem, InboxError> {
        let now = self.env.now();
        let item = PendingItem {
            id: ItemId(self.env.new_id()?),
            grant: req.grant,
            issuer: req.issuer,
            command: req.command,
            target_node: req.target_node,
            risk: req.risk,
            reason: req.reason,
            scale: req.scale,
            title: req.title,
            detail: req.detail,
            params: req.params,
            created: now,
            expires: self.policy.pending_ttl_secs.map(|ttl| now + ttl),
            status: ItemStatus::Pending,
            decided_at: None,
            decided_note: None,
        };
        self.store.put_item(&item)?;
        tracing::info!(
            id = %item.id,
            command = %item.command,
            risk = %item.risk,
            reason = item.reason.as_str(),
            scale = %item.scale,
            issuer = item.issuer.label(),
            "queued for approval"
        );
        Ok(item)
    }

    /// Everything still waiting, newest last. Items past their TTL are swept
    /// to [`ItemStatus::Expired`] on the way through, so a stale approval can
    /// never be tapped from a page that was left open.
    pub fn pending(&self) -> Result<Vec<PendingItem>, InboxError> {
        let now = self.env.now();
        let mut live = Vec::new();
        let mut swept = 0usize;

        // The expiry recorded on the item is the only authority here, because
        // it is the one [`Inbox::approve`] reads and the one the operator was
        // shown. Sweeping by a cutoff derived from the policy in force now
        // disagreed with it at exactly `created + ttl`: the item was still
        // listed as pending and then refused as expired when it was tapped.
        for mut item in self.store.items_with_status(ItemStatus::Pending)? {
            if item.is_expired(now) {
                item.status = ItemStatus::Expired;
                item.decided_at = Some(now);
                self.store.put_item(&item)?;
                swept += 1;
                continue;
            }
            live.push(item);
        }

        if swept > 0 {
            tracing::info!(count = swept, "expired stale Inbox items");
        }
        Ok(live)
    }

    pub fn item(&self, id: &ItemId) -> Result<Option<PendingItem>, InboxError> {
        Ok(self.store.item(id)?)
    }

    /// How many items are waiting, for the dock badge.
    pub fn pending_count(&self) -> Result<usize, InboxError> {
        Ok(self.pending()?.len())
    }

    /// Approve an item. Returns it for the bus to dispatch.
    pub fn approve(&self, id: &ItemId, confirm: Confirmation) -> Result<Approved, InboxError> {
        if self.frozen {
            return Err(InboxError::Frozen);
        }
        let now = self.env.now();
        let mut item = self.load_pending(id, now)?;

        if item.needs_count_confirmation() {
            match (&confirm, item.scale) {
                (Confirmation::CountSeen(shown), _) => {
                    let actual = item.scale.count();
                    match actual {
                        Some(actual) if actual != *shown => {
                            return Err(InboxError::CountMismatch {
                                command: item.command,
                                shown: *shown,
                                actual,
                            });
                        }
                        // An unknown scale cannot be confirmed by a number;
                        // the operator has to accept the unbounded case.
                        None => {
                            return Err(InboxError::CountRequired {
                                command: item.command,
                                scale: item.scale,
                            });
                        }
                        Some(_) => {}
                    }
                }
                (Confirmation::UnknownAccepted, Scale::Unknown) => {}
                _ => {
                    return Err(InboxError::CountRequired {
                        command: item.command,
                        scale: item.scale,
                    });
                }
            }
        }

        item.status = ItemStatus::Approved;
        item.decided_at = Some(now);
        self.store.put_item(&item)?;
        tracing::info!(
            id = %item.id,
            command = %item.command,
            scale = %item.scale,
            "approved"
        );
        Ok(Approved { item })
    }

    /// Deny an item. Allowed in lockdown: refusing a queued action is always
    /// safe.
    pub fn deny(&self, id: &ItemId, note: Option<String>) -> Result<PendingItem, InboxError> {
        let now = self.env.now();
        let mut item = self.load_pending(id, now)?;
        item.status = ItemStatus::Denied;
        item.decided_at = Some(now);
        item.decided_note = note;
        self.store.put_item(&item)?;
        tracing::info!(id = %item.id, command = %item.command, "denied");
        Ok(item)
    }

    /// The issuer gave up, or its session ended. Not a denial: nobody
    /// decided, and the audit trail should say so.
    pub fn withdraw(&self, id: &ItemId) -> Result<PendingItem, InboxError> {
        let now = self.env.now();
        let mut item = self.load_pending(id, now)?;
        item.status = ItemStatus::Withdrawn;
        item.decided_at = Some(now);
        self.store.put_item(&item)?;
        Ok(item)
    }

    /// Withdraw everything a session left waiting, when it ends or is killed.
    pub fn withdraw_for_session(&self, session: &str) -> Result<usize, InboxError> {
        let mut n = 0;
        for item in self.store.items_with_status(ItemStatus::Pending)? {
            if item.issuer.session() != Some(session) {
                continue;
            }
            match self.withdraw(&item.id) {
                Ok(_) => n += 1,
                // Ending a session is cleanup, not a transaction. An item
                // that lapsed or was decided between the listing and this
                // call is already not waiting, and failing the whole sweep
                // over it used to leave the session's remaining items
                // pending forever, with nobody left to answer them.
                Err(InboxError::Expired { .. })
                | Err(InboxError::NotPending { .. })
                | Err(InboxError::NotFound { .. }) => {}
                Err(e) => return Err(e),
            }
        }
        Ok(n)
    }

    fn load_pending(&self, id: &ItemId, now: u64) -> Result<PendingItem, InboxError> {
        let mut item = self
            .store
            .item(id)?
            .ok_or_else(|| InboxError::NotFound { id: id.clone() })?;

        if item.status != ItemStatus::Pending {
            return Err(InboxError::NotPending {
                id: id.clone(),
                status: item.status.as_str(),
            });
        }
        if item.is_expired(now) {
            // Record the expiry rather than only reporting it, so the same
            // item cannot be presented as pending again.
            item.status = ItemStatus::Expired;
            item.decided_at = Some(now);
            self.store.put_item(&item)?;
            return Err(InboxError::Expired {
                id: id.clone(),
                waited: now.saturating_sub(item.created),
            });
        }
        Ok(item)
    }
}

// ---------------------------------------------------------------------------
// Grant administration
// ---------------------------------------------------------------------------

#[derive(Debug, Error)]
pub enum IssueError {
    #[error("grant \"{label}\" lists no namespaces, so it would refuse everything")]
    NoNamespaces { label: String },

    #[error("grant \"{label}\" lists no nodes, so it could not target anything")]
    NoNodes { label: String },

    #[error(transparent)]
    BadPattern(#[from] PatternError),

    #[error(
        "grant \"{label}\" asks for {asked}s, longer than the {max}s maximum; \
         issue a shorter one and reissue when it lapses"
    )]
    TtlTooLong { label: String, asked: u64, max: u64 },

    #[error(
        "grant \"{label}\" asks for no expiry, which this policy does not allow; \
         give an agent session a lifetime"
    )]
    StandingNotAllowed { label: String },

    #[error(
        "grant \"{label}\" asks for \"{pattern}\", which covers the whole command bus, \
         so nothing would be outside it; list the namespaces the session needs, or \
         set Policy::allow_unrestricted_grants for the operator's own tooling"
    )]
    Unrestricted { label: String, pattern: String },

    #[error(transparent)]
    Random(#[from] RandomError),

    #[error(transparent)]
    Storage(#[from] StorageError),
}

/// Issuing, revoking and consulting grants.
pub struct Grants<S, E> {
    store: S,
    env: E,
    policy: Policy,
}

impl<S: GrantStore, E: Env> Grants<S, E> {
    pub fn new(store: S, env: E, policy: Policy) -> Grants<S, E> {
        Grants { store, env, policy }
    }

    pub fn issue(&self, spec: GrantSpec) -> Result<Grant, IssueError> {
        if spec.namespaces.is_empty() {
            return Err(IssueError::NoNamespaces { label: spec.label });
        }
        let mut namespaces = Vec::with_capacity(spec.namespaces.len());
        for p in &spec.namespaces {
            namespaces.push(Pattern::parse(p)?);
        }

        // Refused at issuance, not flagged after the fact: a grant that is
        // already in the store is already being routed against.
        if let Some(everything) = namespaces
            .iter()
            .find(|p| p.is_everything())
            .filter(|_| !self.policy.allow_unrestricted_grants)
        {
            return Err(IssueError::Unrestricted {
                label: spec.label,
                pattern: everything.to_string(),
            });
        }

        let nodes = match spec.nodes {
            // No node scope stated is not "every node": default refuse applies
            // to nodes exactly as it does to namespaces.
            None => return Err(IssueError::NoNodes { label: spec.label }),
            Some(NodeScope::Only(ids)) if ids.is_empty() => {
                return Err(IssueError::NoNodes { label: spec.label });
            }
            Some(nodes) => nodes,
        };

        let now = self.env.now();
        let expires_at = match spec.ttl_secs {
            Some(ttl) if ttl > self.policy.max_grant_ttl_secs => {
                return Err(IssueError::TtlTooLong {
                    label: spec.label,
                    asked: ttl,
                    max: self.policy.max_grant_ttl_secs,
                });
            }
            Some(ttl) => Some(now + ttl),
            None if !self.policy.allow_standing_grants => {
                return Err(IssueError::StandingNotAllowed { label: spec.label });
            }
            None => None,
        };

        let grant = Grant {
            id: GrantId(self.env.new_id()?),
            label: spec.label,
            session: spec.session,
            project: spec.project,
            namespaces,
            nodes,
            issued_at: now,
            expires_at,
            revoked_at: None,
        };
        self.store.put_grant(&grant)?;
        tracing::info!(
            id = %grant.id,
            label = %grant.label,
            namespaces = %grant.namespaces_text().replace('\n', ", "),
            unrestricted = grant.is_unrestricted(),
            "grant issued"
        );
        Ok(grant)
    }

    pub fn get(&self, id: &GrantId) -> Result<Option<Grant>, StorageError> {
        self.store.grant(id)
    }

    pub fn all(&self) -> Result<Vec<Grant>, StorageError> {
        self.store.grants()
    }

    /// Active grants for one session, which is what routing consults.
    pub fn active_for_session(&self, session: &str) -> Result<Vec<Grant>, StorageError> {
        let now = self.env.now();
        Ok(self
            .store
            .grants()?
            .into_iter()
            .filter(|g| g.session.as_deref() == Some(session) && g.is_active(now))
            .collect())
    }

    /// Standing grants, which belong to no session (the always-on assistant).
    pub fn active_standing(&self) -> Result<Vec<Grant>, StorageError> {
        let now = self.env.now();
        Ok(self
            .store
            .grants()?
            .into_iter()
            .filter(|g| g.session.is_none() && g.is_active(now))
            .collect())
    }

    pub fn revoke(&self, id: &GrantId) -> Result<bool, StorageError> {
        let revoked = self.store.revoke_grant(id, self.env.now())?;
        if revoked {
            tracing::info!(id = %id, "grant revoked");
        }
        Ok(revoked)
    }

    /// The kill switch: revoke every grant, pausing all agents.
    pub fn revoke_all(&self) -> Result<usize, StorageError> {
        let n = self.store.revoke_all(self.env.now())?;
        tracing::warn!(count = n, "all grants revoked");
        Ok(n)
    }

    /// Route a command for an issuer, consulting the grants it holds.
    pub fn decide_for(
        &self,
        issuer: &Issuer,
        cmd: &dyn CommandFacts,
    ) -> Result<Decision, StorageError> {
        let grants = match issuer.session() {
            Some(session) => self.active_for_session(session)?,
            None => self.active_standing()?,
        };
        Ok(decide_any(
            &grants,
            issuer,
            cmd,
            &self.policy,
            self.env.now(),
        ))
    }
}

// ---------------------------------------------------------------------------
// Storage ports
// ---------------------------------------------------------------------------

/// A storage failure, kept opaque so this module does not depend on SQLite.
/// The Store implementation wraps its own error in here.
#[derive(Debug, Error)]
#[error("grants storage failed: {0}")]
pub struct StorageError(#[source] Box<dyn std::error::Error + Send + Sync>);

impl StorageError {
    pub fn new(source: impl std::error::Error + Send + Sync + 'static) -> StorageError {
        StorageError(Box::new(source))
    }
}

/// Where grants live. Implemented on [`crate::store::Store`]; see this
/// module's integration notes for the SQL.
pub trait GrantStore: Send + Sync {
    /// Insert or replace, keyed by id.
    fn put_grant(&self, grant: &Grant) -> Result<(), StorageError>;
    fn grant(&self, id: &GrantId) -> Result<Option<Grant>, StorageError>;
    fn grants(&self) -> Result<Vec<Grant>, StorageError>;
    /// Returns whether a grant was there to revoke. Already-revoked grants
    /// keep their original timestamp: the first revocation is the one that
    /// mattered.
    fn revoke_grant(&self, id: &GrantId, at: u64) -> Result<bool, StorageError>;
    /// Revoke everything not already revoked; returns how many.
    fn revoke_all(&self, at: u64) -> Result<usize, StorageError>;
}

/// Where Inbox items live.
pub trait InboxStore: Send + Sync {
    /// Insert or replace, keyed by id. Items are few and small, so a whole-row
    /// write keeps the port narrow instead of one method per field.
    fn put_item(&self, item: &PendingItem) -> Result<(), StorageError>;
    fn item(&self, id: &ItemId) -> Result<Option<PendingItem>, StorageError>;
    /// Oldest first, so the Inbox reads in the order things were asked.
    fn items_with_status(&self, status: ItemStatus) -> Result<Vec<PendingItem>, StorageError>;
    /// Most recent decisions first, for the history pane.
    fn recent_items(&self, limit: u32) -> Result<Vec<PendingItem>, StorageError>;
    /// Mark pending items created before `cutoff` as expired, decided at `at`.
    ///
    /// Maintenance only — a vacuum for rows nobody will ever look at again.
    /// [`Inbox::pending`] does not route through it: an item's approvability
    /// is decided by the `expires` recorded on the item, so that listing and
    /// [`Inbox::approve`] can never disagree about whether it has lapsed.
    fn expire_pending_before(&self, cutoff: u64, at: u64) -> Result<usize, StorageError>;
}

// Shared ownership is the normal case: the HTTP context holds one store and
// hands it to both facades.
impl<T: GrantStore + ?Sized> GrantStore for Arc<T> {
    fn put_grant(&self, grant: &Grant) -> Result<(), StorageError> {
        (**self).put_grant(grant)
    }
    fn grant(&self, id: &GrantId) -> Result<Option<Grant>, StorageError> {
        (**self).grant(id)
    }
    fn grants(&self) -> Result<Vec<Grant>, StorageError> {
        (**self).grants()
    }
    fn revoke_grant(&self, id: &GrantId, at: u64) -> Result<bool, StorageError> {
        (**self).revoke_grant(id, at)
    }
    fn revoke_all(&self, at: u64) -> Result<usize, StorageError> {
        (**self).revoke_all(at)
    }
}

impl<T: InboxStore + ?Sized> InboxStore for Arc<T> {
    fn put_item(&self, item: &PendingItem) -> Result<(), StorageError> {
        (**self).put_item(item)
    }
    fn item(&self, id: &ItemId) -> Result<Option<PendingItem>, StorageError> {
        (**self).item(id)
    }
    fn items_with_status(&self, status: ItemStatus) -> Result<Vec<PendingItem>, StorageError> {
        (**self).items_with_status(status)
    }
    fn recent_items(&self, limit: u32) -> Result<Vec<PendingItem>, StorageError> {
        (**self).recent_items(limit)
    }
    fn expire_pending_before(&self, cutoff: u64, at: u64) -> Result<usize, StorageError> {
        (**self).expire_pending_before(cutoff, at)
    }
}

// ---------------------------------------------------------------------------
// In-process storage
// ---------------------------------------------------------------------------

/// Grants held in memory.
///
/// Real deployments use the SQLite implementation: grants must survive a
/// restart, or a restart would silently widen what agents can do by losing
/// the record of what they were granted. This exists for the pre-persistence
/// path and as the obvious double when driving the routing rules.
#[derive(Default)]
pub struct MemoryGrants {
    rows: Mutex<HashMap<String, Grant>>,
}

impl MemoryGrants {
    pub fn new() -> MemoryGrants {
        MemoryGrants::default()
    }
}

impl GrantStore for MemoryGrants {
    fn put_grant(&self, grant: &Grant) -> Result<(), StorageError> {
        let mut rows = self.rows.lock().expect("grants lock poisoned");
        rows.insert(grant.id.0.clone(), grant.clone());
        Ok(())
    }

    fn grant(&self, id: &GrantId) -> Result<Option<Grant>, StorageError> {
        let rows = self.rows.lock().expect("grants lock poisoned");
        Ok(rows.get(&id.0).cloned())
    }

    fn grants(&self) -> Result<Vec<Grant>, StorageError> {
        let rows = self.rows.lock().expect("grants lock poisoned");
        let mut out: Vec<Grant> = rows.values().cloned().collect();
        out.sort_by_key(|g| g.issued_at);
        Ok(out)
    }

    fn revoke_grant(&self, id: &GrantId, at: u64) -> Result<bool, StorageError> {
        let mut rows = self.rows.lock().expect("grants lock poisoned");
        match rows.get_mut(&id.0) {
            Some(g) => {
                g.revoked_at.get_or_insert(at);
                Ok(true)
            }
            None => Ok(false),
        }
    }

    fn revoke_all(&self, at: u64) -> Result<usize, StorageError> {
        let mut rows = self.rows.lock().expect("grants lock poisoned");
        let mut n = 0;
        for g in rows.values_mut() {
            if g.revoked_at.is_none() {
                g.revoked_at = Some(at);
                n += 1;
            }
        }
        Ok(n)
    }
}

/// Inbox items held in memory. Same caveat as [`MemoryGrants`]: a pending
/// approval that disappears on restart is a command the operator never
/// answered and nobody can see they were asked.
#[derive(Default)]
pub struct MemoryInbox {
    rows: Mutex<HashMap<String, PendingItem>>,
}

impl MemoryInbox {
    pub fn new() -> MemoryInbox {
        MemoryInbox::default()
    }
}

impl InboxStore for MemoryInbox {
    fn put_item(&self, item: &PendingItem) -> Result<(), StorageError> {
        let mut rows = self.rows.lock().expect("inbox lock poisoned");
        rows.insert(item.id.0.clone(), item.clone());
        Ok(())
    }

    fn item(&self, id: &ItemId) -> Result<Option<PendingItem>, StorageError> {
        let rows = self.rows.lock().expect("inbox lock poisoned");
        Ok(rows.get(&id.0).cloned())
    }

    fn items_with_status(&self, status: ItemStatus) -> Result<Vec<PendingItem>, StorageError> {
        let rows = self.rows.lock().expect("inbox lock poisoned");
        let mut out: Vec<PendingItem> = rows
            .values()
            .filter(|i| i.status == status)
            .cloned()
            .collect();
        out.sort_by_key(|i| i.created);
        Ok(out)
    }

    fn recent_items(&self, limit: u32) -> Result<Vec<PendingItem>, StorageError> {
        let rows = self.rows.lock().expect("inbox lock poisoned");
        let mut out: Vec<PendingItem> = rows.values().cloned().collect();
        out.sort_by_key(|i| std::cmp::Reverse(i.created));
        out.truncate(limit as usize);
        Ok(out)
    }

    fn expire_pending_before(&self, cutoff: u64, at: u64) -> Result<usize, StorageError> {
        let mut rows = self.rows.lock().expect("inbox lock poisoned");
        let mut n = 0;
        for item in rows.values_mut() {
            if item.status == ItemStatus::Pending && item.created < cutoff {
                item.status = ItemStatus::Expired;
                item.decided_at = Some(at);
                n += 1;
            }
        }
        Ok(n)
    }
}
