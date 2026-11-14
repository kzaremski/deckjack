//! The command bus: every action in Deckjack is a typed command.
//!
//! The operator, the CLI and agents all dispatch through here, which is the
//! whole point: there is one place where an action is named, authorized,
//! attributed and logged, rather than a privileged path for the UI and a
//! second, politer one for agents.
//!
//! A command carries an id (`files.read`, `git.status`), a JSON Schema for
//! its parameters, a risk level and a target node. The risk level is the part
//! worth reading this module for. From the design:
//!
//! > Per-command risk levels are declared by the backend, not inferred by the
//! > UI, so an adapter bug cannot downgrade a destructive command.
//!
//! Three things make that structurally true rather than a convention:
//!
//! 1. [`Request`] — what a dispatch actually is — has no risk field, and is
//!    not `Deserialize`. What arrives from a browser or an agent is a
//!    [`Submission`], which can name a command and its parameters and nothing
//!    else. Risk is looked up from the [`Registry`] on every dispatch.
//! 2. [`CommandSpec`] has private fields, no setter, and no `Deserialize`
//!    impl. A spec can be sent *to* the browser; one can never arrive *from*
//!    it. Specs are built either from a [`Command`] implementation, where the
//!    risk is an associated const of the backend's own type, or from a
//!    [`Declaration`] read out of an app manifest on disk.
//! 3. Routing matches on [`RiskLevel`] variants exhaustively with no `_` arm,
//!    so a risk level added later cannot inherit read-only treatment by
//!    falling through a catch-all.
//!
//! The same reasoning applies to attribution: a [`Submission`] cannot say who
//! it is from. The caller pairs it with an [`Issuer`] it derived from the
//! authenticated connection, which is why a client cannot claim to be the
//! operator. For the operator that issuer also names *which* browser session
//! asked ([`BrowserSession`]), so a handler can tie what it creates to the
//! session that asked for it — a pty that dies with the browser rather than
//! outliving it — and so a timeline can tell two signed-in browsers apart.
//! The session id is a bearer credential, so it is revealed only on request
//! and serializes as a digest; see [`BrowserSession`].
//!
//! Routing, which is the design's "Agents ask, I act" rule in code:
//!
//! | Risk          | Operator / CLI     | Agent session               |
//! |---------------|--------------------|-----------------------------|
//! | read-only     | runs               | runs, if inside its grant   |
//! | write         | runs               | runs under its grant        |
//! | cross-project | confirm, then runs | waits in the Assistant Inbox|
//! | outbound      | confirm, then runs | waits in the Inbox          |
//! | destructive   | confirm, then runs | waits in the Inbox          |
//!
//! Anything outside an agent's grant is refused, never queued: queuing it
//! would turn a scope violation into a question for the operator, which is
//! how scopes stop meaning anything.
//!
//! Time, randomness, the audit log, grant storage and the Inbox are all
//! injected ([`Env`], [`Journal`], [`Grants`], [`Approvals`]), following
//! `lock.rs`: the routing table above is decidable without a clock, a
//! database or an Assistant.
//!
//! ```ignore
//! # use deckjack_core::command::*;
//! # use std::sync::Arc;
//! struct Status { repo: std::path::PathBuf }
//!
//! #[derive(serde::Deserialize, schemars::JsonSchema)]
//! struct NoParams {}
//!
//! impl Command for Status {
//!     const ID: &'static str = "git.status";
//!     const TITLE: &'static str = "Git status";
//!     // Declared here, by the backend that knows what the command does.
//!     const RISK: RiskLevel = RiskLevel::ReadOnly;
//!     type Params = NoParams;
//!     type Output = String;
//!
//!     async fn run(&self, _p: NoParams, cx: Context) -> Result<String, CommandError> {
//!         cx.progress.step(None, "running git status");
//!         Ok("clean".to_string())
//!     }
//! }
//!
//! let registry = Arc::new(Registry::new());
//! registry.register(Status { repo: ".".into() })?;
//!
//! let bus = Bus::new(registry, NodeId::new("node-a"), Arc::new(OsEnv));
//! let request = Request::from_submission(
//!     Submission { command: "git.status".to_string(), ..Default::default() },
//!     // Or `Issuer::browser(session)` where a signed-in browser asked.
//!     Issuer::operator(),
//! )?;
//!
//! match bus.dispatch(request)? {
//!     Admitted::Running(running) => {
//!         let (mut progress, finish) = running.split();
//!         // forward `progress` to the browser; await `finish` for the result
//!         let done = finish.await;
//!         let text: String = done.parse()?;
//!     }
//!     other => { /* queued in the Inbox, or needs a confirmation */ }
//! }
//! ```

use std::collections::HashMap;
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};
use std::time::SystemTime;

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use schemars::{JsonSchema, Schema, SchemaGenerator};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;
use tokio::sync::mpsc;

use crate::auth::code::os_random;
use crate::auth::session::SessionId;

/// A boxed future. The bus erases every handler behind `dyn`, so handler
/// futures cannot be `impl Future`; they are boxed once per dispatch, which
/// is nothing beside the work a command does.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// How many progress updates are buffered per dispatch before older ones are
/// dropped. Progress is advisory: a command that reports faster than the
/// browser reads must not be slowed down by it, so the sink drops rather than
/// applies back pressure. See [`ProgressSink`].
pub const PROGRESS_BUFFER: usize = 64;

// ---------------------------------------------------------------------------
// Identity
// ---------------------------------------------------------------------------

#[derive(Debug, Error)]
pub enum IdError {
    #[error(
        "command id {0:?} needs a namespace and a name, like `files.read`; \
         a bare name would collide between apps"
    )]
    NotNamespaced(String),

    #[error(
        "command id {0:?} may only contain lowercase letters, digits and \
         underscores in each dot-separated part, each starting with a letter"
    )]
    BadCharacters(String),

    #[error(
        "command id {0:?} contains a double underscore, which is reserved: \
         ids are rewritten with `__` for the dot when exposed as MCP tool names"
    )]
    DoubleUnderscore(String),

    #[error("command id {0:?} is longer than {MAX_ID_LEN} characters")]
    TooLong(String),

    #[error(
        "{0:?} is not a grant pattern: write `git.*` or `files.read.status`, \
         with `*` only as the last part"
    )]
    BadPattern(String),

    #[error(
        "a grant pattern of `*` would be a grant of the whole machine; name \
         the namespaces the session needs instead"
    )]
    PatternMatchesEverything,
}

/// Longest an id may be. Long enough for `filmcraft.timeline.clip.trim_end`,
/// short enough to stay inside the 64-character tool-name limit MCP clients
/// commonly impose once the dots are rewritten.
pub const MAX_ID_LEN: usize = 60;

/// A namespaced command id, for example `files.read` or `git.status`.
///
/// Parsed once at registration. Everything downstream — grant matching, tool
/// names, log rows — can then assume the shape instead of re-validating it.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct CommandId(String);

impl CommandId {
    pub fn new(id: impl Into<String>) -> Result<CommandId, IdError> {
        let id = id.into();
        if id.len() > MAX_ID_LEN {
            return Err(IdError::TooLong(id));
        }
        if id.contains("__") {
            return Err(IdError::DoubleUnderscore(id));
        }
        let parts: Vec<&str> = id.split('.').collect();
        if parts.len() < 2 {
            return Err(IdError::NotNamespaced(id));
        }
        for part in &parts {
            let mut chars = part.chars();
            let starts_well = chars.next().is_some_and(|c| c.is_ascii_lowercase());
            let rest_ok = chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_');
            if !starts_well || !rest_ok {
                return Err(IdError::BadCharacters(id));
            }
        }
        Ok(CommandId(id))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The first part: `files` in `files.read.status`. What grants are written
    /// against and what an app's registration owns.
    pub fn namespace(&self) -> &str {
        self.0.split('.').next().unwrap_or(&self.0)
    }

    fn parts(&self) -> impl Iterator<Item = &str> {
        self.0.split('.')
    }

    /// The id as an MCP tool name. Dots are rewritten to `__` because tool
    /// names are commonly restricted to `[A-Za-z0-9_-]`; ids reject `__`
    /// precisely so this is reversible by [`CommandId::from_tool_name`].
    pub fn to_tool_name(&self) -> String {
        self.0.replace('.', "__")
    }

    pub fn from_tool_name(name: &str) -> Result<CommandId, IdError> {
        CommandId::new(name.replace("__", "."))
    }
}

impl FromStr for CommandId {
    type Err = IdError;
    fn from_str(s: &str) -> Result<CommandId, IdError> {
        CommandId::new(s)
    }
}

impl TryFrom<String> for CommandId {
    type Error = IdError;
    fn try_from(s: String) -> Result<CommandId, IdError> {
        CommandId::new(s)
    }
}

impl From<CommandId> for String {
    fn from(id: CommandId) -> String {
        id.0
    }
}

impl fmt::Display for CommandId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A node's identity, as generated by [`crate::node::id`].
///
/// A newtype rather than a `String` so a node id and a host name cannot be
/// passed to each other's parameters; only the id decides anything.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct NodeId(String);

impl NodeId {
    pub fn new(id: impl Into<String>) -> NodeId {
        NodeId(id.into())
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for NodeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// An agent session's name, as the operator sees it in the Assistant: the
/// thing attribution points at ("Assistant", "coding-2026-11-08").
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct AgentSessionId(String);

impl AgentSessionId {
    pub fn new(name: impl Into<String>) -> AgentSessionId {
        AgentSessionId(name.into())
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for AgentSessionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// One dispatch. Random rather than sequential so it can appear in a URL, a
/// log line and a progress frame without leaking how much has been run.
///
/// Unlike [`crate::auth::SessionId`] this is not a credential — holding one
/// grants nothing — so it gets an ordinary `Display`.
#[derive(Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct DispatchId([u8; 12]);

impl DispatchId {
    pub fn generate() -> DispatchId {
        let mut bytes = [0u8; 12];
        // A failing random source is survivable here: a duplicate dispatch id
        // confuses a log reader, it does not authorize anything. The clock is
        // the fallback so the id stays unique in practice.
        if os_random(&mut bytes).is_err() {
            let secs = SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0);
            bytes[..12].copy_from_slice(&secs.to_le_bytes()[..12]);
        }
        DispatchId(bytes)
    }

    pub fn from_bytes(bytes: [u8; 12]) -> DispatchId {
        DispatchId(bytes)
    }

    pub fn encode(&self) -> String {
        URL_SAFE_NO_PAD.encode(self.0)
    }

    pub fn decode(s: &str) -> Option<DispatchId> {
        // 12 bytes encode to exactly 16 characters.
        if s.len() != 16 {
            return None;
        }
        let v = URL_SAFE_NO_PAD.decode(s).ok()?;
        Some(DispatchId(v.try_into().ok()?))
    }
}

impl fmt::Display for DispatchId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.encode())
    }
}

impl fmt::Debug for DispatchId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "DispatchId({})", self.encode())
    }
}

#[derive(Debug, Error)]
#[error("{0:?} is not a dispatch id")]
pub struct BadDispatchId(String);

impl TryFrom<String> for DispatchId {
    type Error = BadDispatchId;
    fn try_from(s: String) -> Result<DispatchId, BadDispatchId> {
        DispatchId::decode(&s).ok_or(BadDispatchId(s))
    }
}

impl From<DispatchId> for String {
    fn from(id: DispatchId) -> String {
        id.encode()
    }
}

/// An item waiting in the Assistant's Inbox. The Assistant owns the format;
/// the bus only carries it back to whoever is waiting.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PendingId(String);

impl PendingId {
    pub fn new(id: impl Into<String>) -> PendingId {
        PendingId(id.into())
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for PendingId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

// ---------------------------------------------------------------------------
// Risk
// ---------------------------------------------------------------------------

/// What a command can do, declared by the backend that implements it.
///
/// The order of the variants is the order of severity, and `Ord` follows it
/// so a grant can carry a ceiling ([`Grant::ceiling`]). Routing does **not**
/// use the ordering: it matches every variant by name, so a level added here
/// forces a decision at each decision point instead of silently landing on
/// whichever side of a `>=` it falls.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RiskLevel {
    /// Reads and nothing else. Runs immediately for everyone.
    ReadOnly,
    /// Changes something inside the project it was invoked on, reversibly
    /// enough that the operator is not asked each time.
    Write,
    /// Touches something outside the project it was invoked on. Not
    /// destructive in itself, but outside what a session was handed.
    CrossProject,
    /// Leaves the machine: sending mail, pushing a branch, calling an API.
    /// Nothing here can be taken back by undoing it locally.
    Outbound,
    /// Removes or overwrites data irreversibly, or acts in bulk.
    Destructive,
}

impl RiskLevel {
    /// Whether an agent's use of this level needs the operator's approval.
    ///
    /// Matched by name on purpose; see the type's own note.
    pub fn needs_approval_from_agents(self) -> bool {
        match self {
            RiskLevel::ReadOnly | RiskLevel::Write => false,
            RiskLevel::CrossProject | RiskLevel::Outbound | RiskLevel::Destructive => true,
        }
    }

    /// Whether the operator is asked an ordinary "are you sure" first.
    pub fn needs_confirmation_from_operator(self) -> bool {
        match self {
            RiskLevel::ReadOnly | RiskLevel::Write => false,
            RiskLevel::CrossProject | RiskLevel::Outbound | RiskLevel::Destructive => true,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            RiskLevel::ReadOnly => "read_only",
            RiskLevel::Write => "write",
            RiskLevel::CrossProject => "cross_project",
            RiskLevel::Outbound => "outbound",
            RiskLevel::Destructive => "destructive",
        }
    }
}

impl fmt::Display for RiskLevel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Where a command runs.
///
/// A panel never knows or cares which node its backend is on, so this is the
/// spec's business and not the caller's. A caller may *ask* for a node
/// ([`Request::target`]) but cannot move a command off the node its backend
/// declared.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Target {
    /// Runs wherever it was registered.
    ThisNode,
    /// Pinned to one node, whoever asks.
    Node(NodeId),
    /// Any node that registered it; the caller or the scheduler picks.
    AnyNode,
}

// ---------------------------------------------------------------------------
// Specs
// ---------------------------------------------------------------------------

/// Everything the palette, the MCP tool list and the router need to know
/// about one command.
///
/// Serializable outward only, deliberately: this type has no `Deserialize`
/// impl, so a spec — and with it a risk level — can never arrive from a
/// browser, an agent or a node that claims to be one. Build one from a
/// [`Command`] implementation or from a [`Declaration`] read off disk.
#[derive(Debug, Clone, Serialize)]
pub struct CommandSpec {
    id: CommandId,
    title: String,
    /// JSON Schema for the parameters, generated from the Rust type. The
    /// browser's TypeScript types and the MCP tool definitions come from the
    /// same schema, so there is one description of the parameters, not three.
    params: Schema,
    risk: RiskLevel,
    target: Target,
    /// Keep parameters out of the audit log for this command. Set by backends
    /// whose parameters are secrets (a passphrase, a recovery code): the log
    /// is readable from the terminal by design, so it must not become the
    /// place secrets end up.
    #[serde(skip)]
    redact_params: bool,
}

impl CommandSpec {
    /// Derive a spec from a backend's own type. The risk level is `C::RISK`,
    /// an associated const: there is no runtime value for anything to pass in
    /// or overwrite.
    pub fn of<C: Command>() -> Result<CommandSpec, IdError> {
        Ok(CommandSpec {
            id: CommandId::new(C::ID)?,
            title: C::TITLE.to_string(),
            params: SchemaGenerator::default().into_root_schema_for::<C::Params>(),
            risk: C::RISK,
            target: C::target(),
            redact_params: C::REDACT_PARAMS,
        })
    }

    /// Build a spec from an app manifest's declaration.
    ///
    /// This is how CLI adapters get on the bus: their commands are declared
    /// in `app.toml` beside the binary, which is as much "the backend" as a
    /// Rust `impl` is. The distinction that matters is on-disk-in-the-install-
    /// directory versus on-the-wire, not TOML versus Rust.
    pub fn declared(decl: Declaration) -> CommandSpec {
        CommandSpec {
            id: decl.id,
            title: decl.title,
            params: decl.params,
            risk: decl.risk,
            target: decl.target,
            redact_params: decl.redact_params,
        }
    }

    pub fn id(&self) -> &CommandId {
        &self.id
    }
    pub fn title(&self) -> &str {
        &self.title
    }
    pub fn params_schema(&self) -> &Schema {
        &self.params
    }
    /// Read-only by design. There is no `set_risk`, here or anywhere.
    pub fn risk(&self) -> RiskLevel {
        self.risk
    }
    pub fn target(&self) -> &Target {
        &self.target
    }
    pub fn redacts_params(&self) -> bool {
        self.redact_params
    }
}

/// A command declared by an app manifest rather than by a Rust type.
///
/// Deserializable because it is read from `app.toml` in the install
/// directory. Nothing accepts one of these over the network.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Declaration {
    pub id: CommandId,
    pub title: String,
    pub risk: RiskLevel,
    /// JSON Schema for the parameters, as written in the manifest.
    #[serde(default = "empty_object_schema")]
    pub params: Schema,
    #[serde(default = "default_target")]
    pub target: Target,
    #[serde(default)]
    pub redact_params: bool,
}

fn empty_object_schema() -> Schema {
    Schema::try_from(serde_json::json!({
        "type": "object",
        "properties": {},
        "additionalProperties": false
    }))
    .unwrap_or_default()
}

fn default_target() -> Target {
    Target::ThisNode
}

/// Whether a command can be invoked right now, which depends on state the
/// spec cannot describe: no selection, no repository, an app still starting.
///
/// The reason is shown to the operator, so it reads as an explanation rather
/// than a status code.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum Enablement {
    Enabled,
    Disabled { reason: String },
}

impl Enablement {
    pub fn disabled(reason: impl Into<String>) -> Enablement {
        Enablement::Disabled {
            reason: reason.into(),
        }
    }
    pub fn is_enabled(&self) -> bool {
        matches!(self, Enablement::Enabled)
    }
}

// ---------------------------------------------------------------------------
// Commands
// ---------------------------------------------------------------------------

/// A command implemented in Rust.
///
/// The three consts are the backend's declaration about itself. `RISK`
/// especially: it is a const because a const cannot be passed, mutated or
/// deserialized, which is the whole mechanism behind "an adapter bug cannot
/// downgrade a destructive command".
pub trait Command: Send + Sync + 'static {
    /// Namespaced id, validated when the command is registered.
    const ID: &'static str;
    /// Shown in the palette and in the Inbox. A sentence fragment in title
    /// case, not an id: "Delete project", not "delete_project".
    const TITLE: &'static str;
    /// What this command can do. See [`RiskLevel`].
    const RISK: RiskLevel;
    /// Keep parameters out of the audit log. See [`CommandSpec`].
    const REDACT_PARAMS: bool = false;

    /// Parameters. `JsonSchema` so the browser, the palette and the MCP tool
    /// definition all describe them the same way; `DeserializeOwned` because
    /// that deserialization *is* the validation at dispatch time.
    type Params: DeserializeOwned + JsonSchema + Send;
    /// The typed result. Serialized once, on the way out.
    type Output: Serialize + Send;

    /// Where this command runs. Defaults to the node it was registered on,
    /// which is right for everything that is not pinned to one machine.
    fn target() -> Target {
        Target::ThisNode
    }

    /// Whether it can run right now. Cheap: this is called to paint a menu.
    fn enablement(&self) -> Enablement {
        Enablement::Enabled
    }

    fn run(
        &self,
        params: Self::Params,
        cx: Context,
    ) -> impl Future<Output = Result<Self::Output, CommandError>> + Send;
}

/// A command whose parameters are only a JSON Schema, for backends that
/// cannot be Rust types: CLI adapters and app binaries across the socket.
///
/// Implementations validate their own parameters. There is no schema
/// validator in the crate yet, so a `Declaration`'s schema documents the
/// parameters for the UI and the agent, and the adapter is what enforces
/// them. A typed [`Command`] gets both from serde.
// TODO: validate `Value` parameters against the declared schema here, once a
// validator crate is picked, so an adapter cannot be the only thing standing
// between an agent's JSON and a shell invocation.
pub trait DynCommand: Send + Sync + 'static {
    fn enablement(&self) -> Enablement {
        Enablement::Enabled
    }

    fn run(&self, params: Value, cx: Context) -> BoxFuture<'static, Result<Value, CommandError>>;
}

/// What the registry stores: a spec and a way to call it, with the type
/// erased. `Arc<Self>` receiver so the returned future owns its handler and
/// is `'static`; a borrowed handler would tie every running command to the
/// lifetime of a registry lookup.
trait Erased: Send + Sync + 'static {
    fn spec(&self) -> &CommandSpec;
    fn enablement(&self) -> Enablement;
    fn call(
        self: Arc<Self>,
        params: Value,
        cx: Context,
    ) -> BoxFuture<'static, Result<Value, CommandError>>;
}

struct Typed<C: Command> {
    spec: CommandSpec,
    command: C,
}

impl<C: Command> Erased for Typed<C> {
    fn spec(&self) -> &CommandSpec {
        &self.spec
    }

    fn enablement(&self) -> Enablement {
        self.command.enablement()
    }

    fn call(
        self: Arc<Self>,
        params: Value,
        cx: Context,
    ) -> BoxFuture<'static, Result<Value, CommandError>> {
        Box::pin(async move {
            let command_id = self.spec.id.clone();
            let parsed: C::Params =
                serde_json::from_value(params).map_err(|e| CommandError::BadParams {
                    command: command_id.clone(),
                    message: e.to_string(),
                })?;
            let output = self.command.run(parsed, cx).await?;
            serde_json::to_value(output).map_err(|e| CommandError::BadOutput {
                command: command_id,
                message: e.to_string(),
            })
        })
    }
}

struct Dynamic<D: DynCommand> {
    spec: CommandSpec,
    command: D,
}

impl<D: DynCommand> Erased for Dynamic<D> {
    fn spec(&self) -> &CommandSpec {
        &self.spec
    }

    fn enablement(&self) -> Enablement {
        self.command.enablement()
    }

    fn call(
        self: Arc<Self>,
        params: Value,
        cx: Context,
    ) -> BoxFuture<'static, Result<Value, CommandError>> {
        self.command.run(params, cx)
    }
}

// ---------------------------------------------------------------------------
// Registry
// ---------------------------------------------------------------------------

#[derive(Debug, Error)]
pub enum RegistryError {
    #[error(transparent)]
    Id(#[from] IdError),

    #[error(
        "command {0} is already registered; two backends claiming one id would \
         let the second shadow the first, including its risk level"
    )]
    Duplicate(CommandId),
}

/// What a command looks like to the palette: the spec plus whether it can run
/// right now.
#[derive(Debug, Clone, Serialize)]
pub struct Listing {
    #[serde(flatten)]
    pub spec: CommandSpec,
    #[serde(flatten)]
    pub enablement: Enablement,
}

/// An MCP tool definition generated from a command spec. One tool surface for
/// agents, derived from the same schemas the UI uses, so there is nothing to
/// keep in sync by hand.
#[derive(Debug, Clone, Serialize)]
pub struct ToolDefinition {
    /// `files__read`: see [`CommandId::to_tool_name`].
    pub name: String,
    pub description: String,
    #[serde(rename = "inputSchema")]
    pub input_schema: Schema,
    /// Not part of MCP. Carried so the Assistant can tell the operator *why*
    /// something it asked for is waiting in the Inbox.
    pub risk: RiskLevel,
}

/// Every command this node can run.
///
/// Mutable behind a lock rather than frozen at startup, because apps register
/// their commands when they are launched and on-demand apps are stopped when
/// idle: a frozen registry would mean the palette advertising commands whose
/// backend is gone.
pub struct Registry {
    commands: RwLock<HashMap<CommandId, Arc<dyn Erased>>>,
}

impl Default for Registry {
    fn default() -> Registry {
        Registry::new()
    }
}

impl Registry {
    pub fn new() -> Registry {
        Registry {
            commands: RwLock::new(HashMap::new()),
        }
    }

    /// Register a typed command. Its id and risk come from its own type.
    pub fn register<C: Command>(&self, command: C) -> Result<CommandId, RegistryError> {
        let spec = CommandSpec::of::<C>()?;
        let id = spec.id.clone();
        self.insert(id.clone(), Arc::new(Typed { spec, command }))?;
        Ok(id)
    }

    /// Register a command declared by an app manifest.
    pub fn register_declared<D: DynCommand>(
        &self,
        declaration: Declaration,
        command: D,
    ) -> Result<CommandId, RegistryError> {
        let spec = CommandSpec::declared(declaration);
        let id = spec.id.clone();
        self.insert(id.clone(), Arc::new(Dynamic { spec, command }))?;
        Ok(id)
    }

    fn insert(&self, id: CommandId, entry: Arc<dyn Erased>) -> Result<(), RegistryError> {
        let mut map = self.write();
        if map.contains_key(&id) {
            return Err(RegistryError::Duplicate(id));
        }
        tracing::debug!(command = %id, risk = %entry.spec().risk(), "command registered");
        map.insert(id, entry);
        Ok(())
    }

    /// Drop every command in a namespace, for when the app that provides them
    /// stops. Returns how many went away.
    pub fn remove_namespace(&self, namespace: &str) -> usize {
        let mut map = self.write();
        let before = map.len();
        map.retain(|id, _| id.namespace() != namespace);
        before - map.len()
    }

    /// Drop exactly these commands, whoever else is in their namespace.
    ///
    /// What an app provider wants, and `remove_namespace` is not it: a
    /// namespace is not owned by one registrant. The core registers
    /// `files.list` itself, and an app named `files` unregistering "its"
    /// namespace when it exits would take the core's file API with it — a
    /// thing that happened, once, and showed up as the Files app working and
    /// then every file command disappearing.
    pub fn remove_all(&self, ids: &[CommandId]) -> usize {
        let mut map = self.write();
        let before = map.len();
        for id in ids {
            map.remove(id);
        }
        before - map.len()
    }

    pub fn contains(&self, id: &CommandId) -> bool {
        self.read().contains_key(id)
    }

    pub fn spec(&self, id: &CommandId) -> Option<CommandSpec> {
        self.read().get(id).map(|e| e.spec().clone())
    }

    fn lookup(&self, id: &CommandId) -> Option<Arc<dyn Erased>> {
        self.read().get(id).cloned()
    }

    /// Everything, in id order so the palette does not reshuffle itself
    /// between renders. Pass a grant to list what one agent session may use.
    pub fn list(&self, grant: Option<&Grant>) -> Vec<Listing> {
        let mut out: Vec<Listing> = self
            .read()
            .values()
            .filter(|e| grant.is_none_or(|g| g.allows(e.spec())))
            .map(|e| Listing {
                spec: e.spec().clone(),
                enablement: e.enablement(),
            })
            .collect();
        out.sort_by(|a, b| a.spec.id.cmp(&b.spec.id));
        out
    }

    /// MCP tool definitions for one agent session. A command outside the
    /// grant is not described at all: an agent should not be told about tools
    /// whose use would be refused.
    pub fn tools(&self, grant: &Grant) -> Vec<ToolDefinition> {
        let mut out: Vec<ToolDefinition> = self
            .read()
            .values()
            .filter(|e| grant.allows(e.spec()))
            .map(|e| {
                let spec = e.spec();
                ToolDefinition {
                    name: spec.id.to_tool_name(),
                    description: spec.title.clone(),
                    input_schema: spec.params.clone(),
                    risk: spec.risk,
                }
            })
            .collect();
        out.sort_by(|a, b| a.name.cmp(&b.name));
        out
    }

    // A registration or a lookup leaves the map consistent even if a holder
    // panicked between them, so a poisoned lock is still usable. Same
    // reasoning as `http::Ctx`.
    fn read(&self) -> std::sync::RwLockReadGuard<'_, HashMap<CommandId, Arc<dyn Erased>>> {
        self.commands
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn write(&self) -> std::sync::RwLockWriteGuard<'_, HashMap<CommandId, Arc<dyn Erased>>> {
        self.commands
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

// ---------------------------------------------------------------------------
// Grants
// ---------------------------------------------------------------------------

/// One entry in a grant: `git.*`, `files.read.*` or an exact `git.status`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Pattern {
    prefix: Vec<String>,
    /// Whether the pattern ended in `*`, matching any deeper command.
    wildcard: bool,
}

impl Pattern {
    pub fn new(pattern: &str) -> Result<Pattern, IdError> {
        if pattern == "*" {
            return Err(IdError::PatternMatchesEverything);
        }
        let mut parts: Vec<&str> = pattern.split('.').collect();
        let wildcard = parts.last() == Some(&"*");
        if wildcard {
            parts.pop();
        }
        // `git.*.push` is not supported, and silently treating it as a prefix
        // would grant more than it reads as.
        if parts.is_empty() || parts.iter().any(|p| p.contains('*')) {
            return Err(IdError::BadPattern(pattern.to_string()));
        }
        // An exact pattern must be a real command id; a wildcard one is a
        // prefix, which may legitimately be a single namespace (`git.*`).
        if !wildcard {
            CommandId::new(pattern)?;
        }
        Ok(Pattern {
            prefix: parts.into_iter().map(str::to_string).collect(),
            wildcard,
        })
    }

    pub fn matches(&self, id: &CommandId) -> bool {
        let mut parts = id.parts();
        for expected in &self.prefix {
            match parts.next() {
                Some(part) if part == expected => {}
                _ => return false,
            }
        }
        if self.wildcard {
            // `git.*` covers `git.status` but not `git` itself, which is not
            // a command id anyway.
            parts.next().is_some()
        } else {
            parts.next().is_none()
        }
    }
}

impl fmt::Display for Pattern {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.prefix.join("."))?;
        if self.wildcard {
            f.write_str(".*")?;
        }
        Ok(())
    }
}

impl FromStr for Pattern {
    type Err = IdError;
    fn from_str(s: &str) -> Result<Pattern, IdError> {
        Pattern::new(s)
    }
}

impl TryFrom<String> for Pattern {
    type Error = IdError;
    fn try_from(s: String) -> Result<Pattern, IdError> {
        Pattern::new(&s)
    }
}

impl From<Pattern> for String {
    fn from(p: Pattern) -> String {
        p.to_string()
    }
}

/// Which nodes a session may target.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NodeScope {
    /// Only the node the session is attached to. The default: a session that
    /// was handed one machine's work has no business on another.
    ThisNode,
    Nodes(Vec<NodeId>),
    AnyNode,
}

/// What one agent session may do.
///
/// Grants are the only reason an agent's command runs at all. Nothing here
/// queues: anything outside the grant is refused, because asking the operator
/// to approve a scope violation is how scopes stop meaning anything.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Grant {
    pub namespaces: Vec<Pattern>,
    pub nodes: NodeScope,
    /// The highest risk this session may even ask about. Above it, a command
    /// is refused rather than queued — which is how the design's tether mode
    /// gets "no agents with outbound permissions" without a second mechanism.
    pub ceiling: RiskLevel,
}

impl Grant {
    /// Everything this session was given, up to and including destructive
    /// commands (which still wait in the Inbox).
    pub fn new(namespaces: Vec<Pattern>, nodes: NodeScope) -> Grant {
        Grant {
            namespaces,
            nodes,
            ceiling: RiskLevel::Destructive,
        }
    }

    pub fn with_ceiling(mut self, ceiling: RiskLevel) -> Grant {
        self.ceiling = ceiling;
        self
    }

    /// Whether this session may use the command at all: inside its namespaces
    /// and not above its ceiling.
    pub fn allows(&self, spec: &CommandSpec) -> bool {
        spec.risk() <= self.ceiling && self.covers(spec.id())
    }

    /// Namespace match alone, ignoring risk.
    pub fn covers(&self, id: &CommandId) -> bool {
        self.namespaces.iter().any(|p| p.matches(id))
    }

    pub fn allows_node(&self, node: &NodeId, this_node: &NodeId) -> bool {
        match &self.nodes {
            NodeScope::ThisNode => node == this_node,
            NodeScope::Nodes(allowed) => allowed.contains(node),
            NodeScope::AnyNode => true,
        }
    }
}

/// Where grants live. A trait because the real store is `state.db` and the
/// routing rules must be decidable without one.
pub trait Grants: Send + Sync {
    /// Every grant this session holds, each to be evaluated **whole**.
    ///
    /// A list, not one merged grant, and that is the security-relevant part.
    /// Merging the fields of several grants invents combinations nobody
    /// issued: a grant of `terminal.*` on another machine, plus a grant of
    /// `notes.list` on this one, unions into `terminal.*` on this one. The
    /// bus admits a dispatch only when a SINGLE grant covers the command,
    /// its risk and the node together.
    fn grants_for(&self, session: &AgentSessionId) -> Vec<Grant>;

    /// One of this session's grants, for a caller that only needs an example
    /// — a display, a label, an attribution. Never an authorization answer:
    /// a session with several grants has more scope than this reports, and
    /// narrowing is the safe direction for a value nothing enforces with.
    fn grant(&self, session: &AgentSessionId) -> Option<Grant> {
        self.grants_for(session).into_iter().next()
    }
}

/// Grants held in memory, for a server that has not loaded them from
/// `state.db` yet and for deciding routing without a database.
#[derive(Default)]
pub struct MemoryGrants {
    grants: RwLock<HashMap<AgentSessionId, Grant>>,
}

impl MemoryGrants {
    pub fn new() -> MemoryGrants {
        MemoryGrants::default()
    }

    pub fn put(&self, session: AgentSessionId, grant: Grant) {
        self.grants
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(session, grant);
    }

    /// Withdraw a session's grant. Takes effect on the next dispatch, and on
    /// anything still waiting in the Inbox, which is re-checked when approved.
    pub fn revoke(&self, session: &AgentSessionId) -> bool {
        self.grants
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(session)
            .is_some()
    }

    pub fn revoke_all(&self) {
        self.grants
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clear();
    }
}

impl Grants for MemoryGrants {
    fn grants_for(&self, session: &AgentSessionId) -> Vec<Grant> {
        // One grant per session here: this map is keyed by session, so there
        // is never more than one to evaluate.
        self.grants
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(session)
            .cloned()
            .into_iter()
            .collect()
    }
}

/// No session has a grant. The default, and what the bus uses when the
/// Assistant is not installed: an agent that somehow dispatches is refused
/// rather than defaulted into something.
pub struct NoGrants;

impl Grants for NoGrants {
    fn grants_for(&self, _session: &AgentSessionId) -> Vec<Grant> {
        Vec::new()
    }
}

// ---------------------------------------------------------------------------
// Attribution
// ---------------------------------------------------------------------------

/// A short, non-reversible label for one browser session.
///
/// The log needs to tell two signed-in browsers apart; it does not need the
/// credential that proved either of them. This is SHA-256 over the session
/// id, truncated and hex-encoded: stable for as long as the session lives, so
/// a timeline can be grouped by browser, and useless to anyone who reads the
/// log afterwards.
///
/// Derived rather than a prefix of the id, because a prefix of a credential
/// is a piece of the credential, and shortening a secret does not stop it
/// being one.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
#[serde(transparent)]
pub struct SessionTag(String);

/// Bytes of digest kept. Eight leaves a one-in-2^64 chance that two sessions
/// share a tag, which is far more than a log holding a handful of browsers
/// needs, and short enough to read in a terminal.
///
/// A **prefix** of [`crate::auth::SessionHash`], not an independent digest —
/// see [`BrowserSession::tag`] for why that is load-bearing.
const SESSION_TAG_BYTES: usize = 8;

impl SessionTag {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for SessionTag {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// The signed-in browser a command came from.
///
/// Wraps [`SessionId`] rather than exposing it, because that id is a bearer
/// credential: holding it is being signed in. Two uses want it, and they are
/// not the same use.
///
/// Owning a resource *by* session needs the id itself — a pty has to be
/// tagged with it so [`crate::terminal::Registry::close_for_session`] can
/// match on it when the browser session ends — and asks with
/// [`BrowserSession::reveal`], named so the call site reads as the handover
/// of a secret that it is.
///
/// Writing down *which* browser acted needs only [`BrowserSession::tag`], and
/// that is what `Serialize` emits. So no serialized attribution — not a log
/// row, not an Inbox item, not a frame on its way to a client — can carry the
/// credential, even by accident. There is deliberately no `Deserialize` and no
/// `Display`: a browser session is established by the layer that
/// authenticated the connection, never parsed back out of something.
#[derive(Clone, PartialEq, Eq)]
pub struct BrowserSession(SessionId);

impl BrowserSession {
    pub fn new(session: SessionId) -> BrowserSession {
        BrowserSession(session)
    }

    /// The session id itself, for the one legitimate use: tying a resource's
    /// lifetime to this browser session.
    pub fn reveal(&self) -> &SessionId {
        &self.0
    }

    /// The label for the log and the timeline.
    ///
    /// Computed on demand rather than stored, because it is wanted once per
    /// log row and a dispatch happens on every terminal keystroke; hashing
    /// there would be work nobody asked for.
    pub fn tag(&self) -> SessionTag {
        // The first 16 hex of `SessionHash`, which is the digest the store
        // keys `sessions` by and the one `auth::Event::SessionRevoked`
        // carries. That agreement is the whole value of the tag.
        //
        // This used to be its own digest, over `SessionId::encode()` rather
        // than over the id's bytes. Both were honestly described as "SHA-256
        // of the session id" and the two were different numbers, so an audit
        // row could not be matched to the session it came from, to a live
        // row in `sessions`, or to a revocation — which is the one question
        // anybody reads an audit log to answer. Nothing failed: each side
        // was self-consistent and tested. It only came apart when the two
        // were read side by side against a running node.
        //
        // A prefix rather than a second hash, so the join is `substr(hash,
        // 1, 16) = issuer_session` and no second derivation can drift.
        let hex = self.0.hash().hex();
        SessionTag(hex[..SESSION_TAG_BYTES * 2].to_string())
    }
}

impl fmt::Debug for BrowserSession {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // The tag, not `<redacted>`: this type appears in the `Debug` of
        // `Context` and `Running`, and a debug line that names the browser is
        // useful in a way a placeholder is not.
        write!(f, "BrowserSession({})", self.tag())
    }
}

impl Serialize for BrowserSession {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.tag().serialize(serializer)
    }
}

/// Who issued a command.
///
/// Serialize only, and not part of [`Submission`]: the issuer is derived from
/// the authenticated connection by the layer that accepted it. If it were a
/// field a client could set, every rule in this module would be advisory.
///
/// The operator's variant carries which browser session asked, because
/// without it nothing downstream can tie a pty to the browser that opened it,
/// lockdown has no way to pick out what one revoked session started, and a
/// timeline cannot tell two browsers signed in at once apart. It carries it
/// as a [`BrowserSession`], whose serialized form is a digest, so the old
/// guarantee holds unchanged: the audit log is readable from the terminal by
/// design, and no credential reaches it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "issuer", rename_all = "snake_case")]
pub enum Issuer {
    /// Me, from a signed-in browser.
    Operator {
        /// Which browser. `None` where the operator's authority is right but
        /// no browser is behind the action: the CLI's in-process dispatches,
        /// a reminder firing, a command the server issues at startup.
        #[serde(skip_serializing_if = "Option::is_none")]
        session: Option<BrowserSession>,
    },
    /// A named agent session, as it appears in the Assistant.
    Agent { session: AgentSessionId },
    /// `deckjack <command>` on the host. Treated as the operator for routing,
    /// recorded separately so a timeline can tell them apart.
    Cli,
}

impl Issuer {
    /// The operator at a signed-in browser. The only way a browser session
    /// gets onto the bus, and it takes a [`SessionId`] the caller must already
    /// have authenticated — there is no path from a frame to here.
    pub fn browser(session: SessionId) -> Issuer {
        Issuer::Operator {
            session: Some(BrowserSession::new(session)),
        }
    }

    /// The operator with no browser behind the action. Named rather than
    /// written as a literal so the absence is a decision at the call site.
    pub fn operator() -> Issuer {
        Issuer::Operator { session: None }
    }

    pub fn agent(session: impl Into<String>) -> Issuer {
        Issuer::Agent {
            session: AgentSessionId::new(session),
        }
    }

    pub fn kind(&self) -> &'static str {
        match self {
            Issuer::Operator { .. } => "operator",
            Issuer::Agent { .. } => "agent",
            Issuer::Cli => "cli",
        }
    }

    /// The agent session's name, when there is one.
    pub fn name(&self) -> Option<&str> {
        match self {
            Issuer::Agent { session } => Some(session.as_str()),
            Issuer::Operator { .. } | Issuer::Cli => None,
        }
    }

    /// Which signed-in browser this came from, if any.
    ///
    /// `None` for an agent, for the CLI, and for an operator action the server
    /// issued itself — all three are "no browser to tie anything to", which is
    /// what a caller deciding a resource's owner needs to know.
    pub fn browser_session(&self) -> Option<&BrowserSession> {
        match self {
            Issuer::Operator { session } => session.as_ref(),
            Issuer::Agent { .. } | Issuer::Cli => None,
        }
    }

    pub fn is_agent(&self) -> bool {
        matches!(self, Issuer::Agent { .. })
    }
}

impl fmt::Display for Issuer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Issuer::Operator { session: None } => f.write_str("operator"),
            // The tag is what makes two browsers distinguishable in the one
            // place this `Display` is used: the journal line.
            Issuer::Operator {
                session: Some(browser),
            } => write!(f, "operator {}", browser.tag()),
            Issuer::Agent { session } => write!(f, "agent {session}"),
            Issuer::Cli => f.write_str("cli"),
        }
    }
}

// ---------------------------------------------------------------------------
// Requests
// ---------------------------------------------------------------------------

/// What arrives over the WebSocket, the HTTP route or the control socket.
///
/// Note what is absent: no risk level, no issuer, no grant. A client can name
/// a command and its parameters, ask for a node, and say the operator
/// confirmed. Everything else is looked up here.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Submission {
    pub command: String,
    #[serde(default)]
    pub params: Value,
    /// Which node to run on, when the command is not pinned to one.
    #[serde(default)]
    pub target: Option<NodeId>,
    /// The operator answered the confirmation dialog.
    ///
    /// This is the one trust the bus extends to a client, and it is bounded:
    /// it cannot change a command's risk, so the worst a buggy client can do
    /// is skip its own dialog for a command that was already destructive. It
    /// is ignored entirely for agents, who are clients too.
    #[serde(default)]
    pub confirmed: bool,
}

/// A submission plus the attribution the server derived for it.
///
/// Deliberately not `Deserialize`; [`Request::from_submission`] is the only
/// way in from the wire.
#[derive(Debug, Clone)]
pub struct Request {
    pub command: CommandId,
    pub params: Value,
    pub issuer: Issuer,
    pub target: Option<NodeId>,
    pub confirmed: bool,
}

impl Request {
    pub fn from_submission(s: Submission, issuer: Issuer) -> Result<Request, IdError> {
        Ok(Request {
            command: CommandId::new(s.command)?,
            params: s.params,
            issuer,
            target: s.target,
            confirmed: s.confirmed,
        })
    }

    /// Build one in process, for the CLI and for commands the server issues
    /// on its own behalf (startup, reminders firing).
    pub fn new(command: CommandId, params: Value, issuer: Issuer) -> Request {
        Request {
            command,
            params,
            issuer,
            target: None,
            confirmed: false,
        }
    }

    pub fn confirmed(mut self) -> Request {
        self.confirmed = true;
        self
    }

    pub fn on_node(mut self, node: NodeId) -> Request {
        self.target = Some(node);
        self
    }
}

// ---------------------------------------------------------------------------
// Progress and outcomes
// ---------------------------------------------------------------------------

/// Which of a child process's streams a log line came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Stream {
    Stdout,
    Stderr,
}

/// Something a running command has to say before it finishes.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "progress", rename_all = "snake_case")]
pub enum Progress {
    /// Where it is. `fraction` is `None` when the command genuinely does not
    /// know — better than a bar that invents its own pace.
    Step {
        dispatch: DispatchId,
        fraction: Option<f32>,
        message: String,
    },
    /// Raw output, for the panel's log view and the "show terminal" toggle.
    Log {
        dispatch: DispatchId,
        stream: Stream,
        line: String,
    },
}

/// Where a running command sends progress.
///
/// Sends never block and never fail: if nobody is draining the channel — the
/// browser closed the panel, or the caller only wants the result — updates
/// are dropped. A command must not be throttled by how fast its audience
/// reads, and reporting progress must not be a thing that can return an error
/// a backend then has to decide what to do about.
#[derive(Clone)]
pub struct ProgressSink {
    dispatch: DispatchId,
    tx: mpsc::Sender<Progress>,
}

impl fmt::Debug for ProgressSink {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ProgressSink({})", self.dispatch)
    }
}

impl ProgressSink {
    pub fn step(&self, fraction: Option<f32>, message: impl Into<String>) {
        self.send(Progress::Step {
            dispatch: self.dispatch.clone(),
            fraction: fraction.map(|f| f.clamp(0.0, 1.0)),
            message: message.into(),
        });
    }

    pub fn log(&self, stream: Stream, line: impl Into<String>) {
        self.send(Progress::Log {
            dispatch: self.dispatch.clone(),
            stream,
            line: line.into(),
        });
    }

    fn send(&self, progress: Progress) {
        let _ = self.tx.try_send(progress);
    }
}

/// What a handler is told about the dispatch it is serving.
#[derive(Debug)]
pub struct Context {
    pub dispatch: DispatchId,
    pub command: CommandId,
    /// Available so a backend can log or label its own work the same way the
    /// bus does. It is a copy of what the bus already decided with; changing
    /// it here changes nothing.
    pub risk: RiskLevel,
    pub issuer: Issuer,
    pub node: NodeId,
    pub started: SystemTime,
    pub progress: ProgressSink,
}

impl Context {
    /// Which signed-in browser session this dispatch came from, or `None` for
    /// the CLI, for an agent, and for commands the server issues itself.
    ///
    /// A handler that creates something outliving the call — a pty, a watcher,
    /// a long-running export — asks this and gives what it created an owner.
    /// `None` means there is no browser to outlive, not "unknown": the only
    /// honest owner then is the host.
    pub fn browser_session(&self) -> Option<&BrowserSession> {
        self.issuer.browser_session()
    }
}

#[derive(Debug, Error)]
pub enum CommandError {
    #[error("parameters for {command} are not valid: {message}")]
    BadParams { command: CommandId, message: String },

    #[error("{command} returned a result that could not be serialized: {message}")]
    BadOutput { command: CommandId, message: String },

    #[error("{command} cannot run on this node: {reason}")]
    Unsupported { command: CommandId, reason: String },

    #[error("{command} was cancelled")]
    Cancelled { command: CommandId },

    /// Whatever the backend failed at. Open-ended on purpose: a command is
    /// arbitrary work, and enumerating git's failures here would be a lie.
    #[error(transparent)]
    Failed(#[from] anyhow::Error),
}

/// A finished dispatch.
#[derive(Debug)]
pub struct Completion {
    pub dispatch: DispatchId,
    pub command: CommandId,
    pub issuer: Issuer,
    pub node: NodeId,
    pub started: SystemTime,
    pub finished: SystemTime,
    pub outcome: Result<Value, CommandError>,
}

impl Completion {
    pub fn succeeded(&self) -> bool {
        self.outcome.is_ok()
    }

    /// The result as the type the command declared. The value went through
    /// JSON on the way out of the handler, so this is a decode and not a cast.
    pub fn parse<T: DeserializeOwned>(&self) -> Result<T, CommandError> {
        match &self.outcome {
            Ok(value) => {
                serde_json::from_value(value.clone()).map_err(|e| CommandError::BadOutput {
                    command: self.command.clone(),
                    message: e.to_string(),
                })
            }
            Err(e) => Err(CommandError::Failed(anyhow::anyhow!("{e}"))),
        }
    }
}

/// An admitted command that has not run yet.
///
/// Returned rather than awaited inside `dispatch` so the caller can hook up
/// the progress channel *before* the work starts; otherwise the first second
/// of a long command's output would have nowhere to go.
pub struct Running {
    dispatch: DispatchId,
    command: CommandId,
    issuer: Issuer,
    node: NodeId,
    risk: RiskLevel,
    started: SystemTime,
    progress: mpsc::Receiver<Progress>,
    future: BoxFuture<'static, Result<Value, CommandError>>,
    env: Arc<dyn Env>,
    journal: Arc<dyn Journal>,
}

impl fmt::Debug for Running {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Running")
            .field("dispatch", &self.dispatch)
            .field("command", &self.command)
            .field("issuer", &self.issuer)
            .finish_non_exhaustive()
    }
}

impl Running {
    pub fn dispatch(&self) -> &DispatchId {
        &self.dispatch
    }
    pub fn command(&self) -> &CommandId {
        &self.command
    }
    pub fn risk(&self) -> RiskLevel {
        self.risk
    }

    /// Take the progress channel and the work separately, so one task can
    /// forward progress to the browser while another awaits the result.
    /// Split a running dispatch into its progress stream and its completion.
    ///
    /// **Nothing is spawned.** The command runs *inside* the returned future,
    /// so a caller that waits on the progress receiver before polling that
    /// future deadlocks: there is nothing driving the command that would
    /// produce the progress. Poll the completion concurrently — spawn it, or
    /// `select!` on both — as `wiring::BusDispatch` does.
    pub fn split(self) -> (mpsc::Receiver<Progress>, BoxFuture<'static, Completion>) {
        let Running {
            dispatch,
            command,
            issuer,
            node,
            risk,
            started,
            progress,
            future,
            env,
            journal,
        } = self;

        let finish: BoxFuture<'static, Completion> = Box::pin(async move {
            let outcome = future.await;
            let finished = env.now();
            let (decision, detail) = match &outcome {
                Ok(_) => (Decision::Completed, None),
                Err(e) => (Decision::Failed, Some(e.to_string())),
            };
            // A completion that cannot be written down is logged and let
            // through: the work already happened, and refusing to return its
            // result would not un-happen it. Admission is where an unwritable
            // log is allowed to stop something.
            if let Err(e) = journal.record(LogEntry {
                dispatch: dispatch.clone(),
                at: finished,
                node: node.clone(),
                command: command.clone(),
                risk: Some(risk),
                issuer: issuer.clone(),
                decision,
                params: None,
                detail,
            }) {
                tracing::warn!(error = %e, command = %command, "command completion not recorded");
            }
            Completion {
                dispatch,
                command,
                issuer,
                node,
                started,
                finished,
                outcome,
            }
        });

        (progress, finish)
    }

    /// Run it and wait, discarding progress. Dropping the receiver is what
    /// makes the sink's drops harmless, so this is the ordinary way to call a
    /// command from the CLI or from another command.
    pub async fn finish(self) -> Completion {
        let (progress, future) = self.split();
        drop(progress);
        future.await
    }
}

/// What a dispatch turned into.
#[derive(Debug)]
pub enum Admitted {
    /// Authorized and ready to run.
    Running(Running),
    /// Waiting for the operator in the Assistant's Inbox. Nothing has run.
    Queued {
        dispatch: DispatchId,
        pending: PendingId,
        risk: RiskLevel,
    },
    /// The operator's ordinary "are you sure" has not been answered. Nothing
    /// has run; dispatch again with [`Request::confirmed`].
    NeedsConfirmation {
        dispatch: DispatchId,
        risk: RiskLevel,
        /// What to put in the dialog, phrased for a person.
        prompt: String,
    },
}

#[derive(Debug, Error)]
pub enum Refused {
    #[error(transparent)]
    Id(#[from] IdError),

    #[error(
        "no command {0} is registered on this node; the app that provides it \
         may not be running"
    )]
    UnknownCommand(CommandId),

    #[error("{command} cannot run right now: {reason}")]
    Disabled { command: CommandId, reason: String },

    #[error(
        "{command} runs on node {wanted}, not on this node ({this}); \
         forwarding between nodes is not implemented yet"
    )]
    NotLocal {
        command: CommandId,
        wanted: NodeId,
        this: NodeId,
    },

    /// A caller asked for a node the command's own target does not leave open.
    ///
    /// Separate from [`Refused::NotLocal`] because the two say opposite
    /// things: there the command is somewhere else, here the command is right
    /// where it belongs and the *request* named the wrong machine. Reported
    /// with one variant, the message came out backwards — "runs on node
    /// node-b" about a command that runs here — and a refusal that names the
    /// wrong machine is worse than no message.
    #[error(
        "{command} runs on node {runs_on}, so it cannot be asked for on node \
         {asked}; a caller may only choose a node for a command its backend \
         left open"
    )]
    WrongNodeAsked {
        command: CommandId,
        asked: NodeId,
        runs_on: NodeId,
    },

    #[error(
        "agent session {session} has no grant, so it can run nothing; grant it \
         the namespaces it needs from the Assistant"
    )]
    NoGrant { session: AgentSessionId },

    #[error(
        "agent session {session} was not granted {command}; add its namespace \
         to the session's grant if that is what you want"
    )]
    OutsideGrant {
        session: AgentSessionId,
        command: CommandId,
    },

    #[error(
        "agent session {session} may not run {risk} commands ({command}); raise \
         the session's ceiling if that is what you want"
    )]
    AboveCeiling {
        session: AgentSessionId,
        command: CommandId,
        risk: RiskLevel,
    },

    #[error("agent session {session} may not target node {node}")]
    NodeNotAllowed {
        session: AgentSessionId,
        node: NodeId,
    },

    // Names no affordance that does not exist. The previous wording sent the
    // operator to "the menu", and there is no menu: the shell dispatches from
    // a command palette, and nothing anywhere can resume agents —
    // `Bus::resume_agents` has no non-test caller and no command id reaches
    // it. Bare `deckjack unlock` was wrong too; it bails asking for
    // `--recovery-code`.
    //
    // One variant has to cover both causes, because it carries no field saying
    // which: `agents_paused()` reads the operator's lever and the lockdown's
    // hold together.
    #[error(
        "agents are paused on this node, by the operator's lever or by a lockdown; \
         nothing an agent sends can lift it — a lockdown ends only with \
         `deckjack unlock --recovery-code CODE` on the host"
    )]
    AgentsPaused,

    #[error(
        "{command} needs the operator's approval, but no Inbox is attached: \
         the Assistant is not installed or not running, so run it yourself \
         from the palette"
    )]
    NoInbox { command: CommandId },

    #[error("could not put {command} in the Inbox: {message}")]
    InboxFailed { command: CommandId, message: String },

    #[error(
        "{command} is {risk} and the audit log could not be written ({message}); \
         refusing, because an unlogged destructive command is exactly what the \
         log exists for"
    )]
    NotRecorded {
        command: CommandId,
        risk: RiskLevel,
        message: String,
    },
}

// ---------------------------------------------------------------------------
// Audit log
// ---------------------------------------------------------------------------

/// What happened to a dispatch, as the timeline panel shows it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Decision {
    /// Admitted and started.
    Ran,
    /// Put in the Inbox.
    Queued,
    /// Waiting on the operator's confirmation dialog.
    AwaitingConfirmation,
    /// Not allowed. Counted: a burst of these from one session is one of the
    /// lockdown triggers.
    Refused,
    Completed,
    Failed,
}

impl Decision {
    pub fn as_str(self) -> &'static str {
        match self {
            Decision::Ran => "ran",
            Decision::Queued => "queued",
            Decision::AwaitingConfirmation => "awaiting_confirmation",
            Decision::Refused => "refused",
            Decision::Completed => "completed",
            Decision::Failed => "failed",
        }
    }
}

/// One row of the append-only log, per node.
#[derive(Debug, Clone, Serialize)]
pub struct LogEntry {
    pub dispatch: DispatchId,
    pub at: SystemTime,
    pub node: NodeId,
    pub command: CommandId,
    /// `None` when the command was never found, so there was no declared risk
    /// to record.
    pub risk: Option<RiskLevel>,
    pub issuer: Issuer,
    pub decision: Decision,
    /// The parameters, on admission and on a refusal, and never for a command
    /// whose spec sets `redact_params` — nor for one with no spec at all,
    /// where there is nothing left to ask about redaction.
    pub params: Option<Value>,
    /// Why it was refused, or how it failed.
    pub detail: Option<String>,
}

#[derive(Debug, Error)]
#[error("cannot write the command log: {0}")]
pub struct JournalError(pub String);

/// Where attribution is written.
///
/// A trait because the real implementation is a table in `state.db` and,
/// later, a copy shipped to the VPS as it is written — a host attacker should
/// not be able to erase what they did. Nothing in routing depends on it, so
/// the rules stay testable without a database.
pub trait Journal: Send + Sync {
    fn record(&self, entry: LogEntry) -> Result<(), JournalError>;
}

/// The log as `tracing` output. Honest default: it records the same facts, to
/// wherever the process's logs go, and survives exactly as well as they do.
pub struct TracingJournal;

impl Journal for TracingJournal {
    fn record(&self, entry: LogEntry) -> Result<(), JournalError> {
        tracing::info!(
            dispatch = %entry.dispatch,
            command = %entry.command,
            issuer = %entry.issuer,
            risk = entry.risk.map(|r| r.as_str()).unwrap_or("unknown"),
            decision = entry.decision.as_str(),
            detail = entry.detail.as_deref().unwrap_or(""),
            "command"
        );
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Approvals
// ---------------------------------------------------------------------------

/// A command waiting for the operator, with everything the Inbox needs to
/// show it and to run it afterwards.
#[derive(Debug, Clone, Serialize)]
pub struct PendingCommand {
    pub dispatch: DispatchId,
    pub command: CommandId,
    /// The spec's human title, so the Inbox does not have to look it up and
    /// cannot disagree with the palette about what the command is called.
    pub title: String,
    pub risk: RiskLevel,
    pub issuer: Issuer,
    pub node: NodeId,
    pub params: Value,
    pub requested_at: SystemTime,
}

#[derive(Debug, Error)]
pub enum ApprovalError {
    #[error("there is no Inbox attached to this bus")]
    NoInbox,
    #[error("{0}")]
    Failed(String),
}

/// The Assistant's Inbox, from the bus's side.
pub trait Approvals: Send + Sync {
    fn submit(&self, pending: &PendingCommand) -> Result<PendingId, ApprovalError>;
}

/// No Assistant installed.
///
/// With the Assistant uninstalled Deckjack is a complete desktop with no
/// agents at all, so this is not a degraded mode: there is nothing to queue
/// for, and anything that would have queued is refused with that said plainly.
pub struct NoInbox;

impl Approvals for NoInbox {
    fn submit(&self, _pending: &PendingCommand) -> Result<PendingId, ApprovalError> {
        Err(ApprovalError::NoInbox)
    }
}

/// Something outside this module that is holding agents, asked on every
/// dispatch.
///
/// The kill switch, in the product. A port rather than a dependency on
/// `crate::lockdown` so that `command` does not have to know the kill switch
/// exists, and so that the two meet in exactly one place — `wiring::bus`,
/// where every other adapter of this shape lives.
///
/// Asked, not told. A lockdown could instead push a flag in through
/// [`Bus::pause_agents`], and that is the version that was designed and never
/// wired: it needs a resume call on the unlock path, another on
/// `Lockdown::sealed`, and another wherever a restart comes back locked, and a
/// lockdown nobody can leave is worse than one that never engages. Reading
/// the state means there is nothing to remember: the hold ends when
/// `is_locked` goes false, and [`Bus::resume_agents`] cannot clear it.
pub trait Halt: Send + Sync {
    /// Whether agent-issued dispatches must be refused right now.
    fn agents_halted(&self) -> bool;
}

/// Nothing is holding agents.
///
/// The honest default for a bus with no kill switch attached, which is every
/// bus in a unit test. Deliberately NOT what [`crate::wiring::BusParts`]
/// falls back to: there the field is required, so a product bus assembled
/// without a hold is a compile error rather than an omission, and passing
/// this reads as a decision in the source — the same bargain
/// [`NoGrants`] and [`NoInbox`] make.
pub struct NoHalt;

impl Halt for NoHalt {
    fn agents_halted(&self) -> bool {
        false
    }
}

/// The operator's approval of a queued command, from the Inbox.
///
/// A separate type so releasing something from the Inbox is a distinct call
/// ([`Bus::dispatch_approved`]) and not a flag on an ordinary request that a
/// client could set. The bus trusts it because the Inbox is the operator's
/// own surface; it is not trusted to skip the grant check, which is re-run in
/// case the session's grant was withdrawn while the command waited.
#[derive(Debug, Clone)]
pub struct Approval {
    pending: PendingId,
}

impl Approval {
    pub fn granted(pending: PendingId) -> Approval {
        Approval { pending }
    }
    pub fn pending(&self) -> &PendingId {
        &self.pending
    }
}

// ---------------------------------------------------------------------------
// Injected world
// ---------------------------------------------------------------------------

/// The clock and the random source, supplied rather than read, exactly as in
/// `lock.rs`: routing and logging are then decidable in a test without a
/// clock, and two dispatches can be given the same instant on purpose.
pub trait Env: Send + Sync {
    fn now(&self) -> SystemTime;
    fn new_dispatch_id(&self) -> DispatchId;
}

/// The real one.
pub struct OsEnv;

impl Env for OsEnv {
    fn now(&self) -> SystemTime {
        SystemTime::now()
    }
    fn new_dispatch_id(&self) -> DispatchId {
        DispatchId::generate()
    }
}

// ---------------------------------------------------------------------------
// The bus
// ---------------------------------------------------------------------------

/// What routing decided, before anything is done about it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Route {
    Run,
    Confirm,
    Queue,
}

/// The command bus.
///
/// Holds the registry, the attribution log, grant storage and the Inbox, and
/// is the only thing that turns a request into work. Cheap to clone behind an
/// `Arc`; shared by the HTTP layer, the CLI's control socket and the agent
/// harness, which is the point.
pub struct Bus {
    registry: Arc<Registry>,
    node: NodeId,
    env: Arc<dyn Env>,
    journal: Arc<dyn Journal>,
    grants: Arc<dyn Grants>,
    approvals: Arc<dyn Approvals>,
    /// The operator's own lever: "pauses all agents". Separate from revoking
    /// grants so pausing is reversible without rebuilding what each session
    /// was allowed to do.
    ///
    /// NOT the lockdown's lever. A lockdown is asked about through [`Halt`]
    /// instead of being mirrored in here, so that leaving a lockdown resumes
    /// agents with nothing to remember to call, and so that
    /// [`Bus::resume_agents`] cannot clear a lockdown. See
    /// [`Bus::agents_paused`].
    agents_paused: AtomicBool,
    /// Whatever else is holding agents, asked on every dispatch. The kill
    /// switch, in the product; [`NoHalt`] by default.
    halt: Arc<dyn Halt>,
    progress_buffer: usize,
}

impl Bus {
    /// A bus with the honest defaults: the log goes to `tracing`, no session
    /// has a grant, and there is no Inbox. Add each as it exists.
    pub fn new(registry: Arc<Registry>, node: NodeId, env: Arc<dyn Env>) -> Bus {
        Bus {
            registry,
            node,
            env,
            journal: Arc::new(TracingJournal),
            grants: Arc::new(NoGrants),
            approvals: Arc::new(NoInbox),
            agents_paused: AtomicBool::new(false),
            halt: Arc::new(NoHalt),
            progress_buffer: PROGRESS_BUFFER,
        }
    }

    pub fn with_journal(mut self, journal: Arc<dyn Journal>) -> Bus {
        self.journal = journal;
        self
    }

    pub fn with_grants(mut self, grants: Arc<dyn Grants>) -> Bus {
        self.grants = grants;
        self
    }

    pub fn with_approvals(mut self, approvals: Arc<dyn Approvals>) -> Bus {
        self.approvals = approvals;
        self
    }

    /// Attach whatever may hold agents — the kill switch. See [`Halt`].
    pub fn with_halt(mut self, halt: Arc<dyn Halt>) -> Bus {
        self.halt = halt;
        self
    }

    pub fn with_progress_buffer(mut self, updates: usize) -> Bus {
        self.progress_buffer = updates.max(1);
        self
    }

    pub fn registry(&self) -> &Arc<Registry> {
        &self.registry
    }

    pub fn node(&self) -> &NodeId {
        &self.node
    }

    /// The operator's lever, on its own. A lockdown does not come through
    /// here — see [`Halt`] and [`Bus::agents_paused`].
    ///
    /// Already-running commands are not touched: this is an ADMISSION gate,
    /// so a command already past [`Bus::plan`] runs to completion. Killing
    /// child processes is lockdown's step 2 (`Lockdown::engage` reaps them
    /// through `Children`), and this module should not pretend to do it. What
    /// neither covers is an in-process command already running; that is a
    /// known gap and not one a flag can close.
    pub fn pause_agents(&self) {
        self.agents_paused.store(true, Ordering::SeqCst);
        tracing::warn!("agents paused: no agent command will be admitted");
    }

    /// Undo [`Bus::pause_agents`], and only that.
    ///
    /// It cannot release a [`Halt`], which is deliberate: an operator lever
    /// that could resume agents during a lockdown would be a way around the
    /// kill switch that needs no recovery code. `Approvals::thaw` refuses
    /// while locked for the same reason; here the refusal is structural,
    /// because [`Bus::agents_paused`] reads both.
    pub fn resume_agents(&self) {
        self.agents_paused.store(false, Ordering::SeqCst);
        tracing::info!("agents resumed");
    }

    /// Whether an agent-issued dispatch is refused right now, for either
    /// reason: the operator paused them, or something is holding them.
    ///
    /// Both, because the two must not be able to clear each other, and
    /// because the lockdown's hold has to end when the lockdown does rather
    /// than when somebody remembers to say so.
    ///
    /// Read by the two admission gates ([`Bus::plan`] and
    /// [`Bus::dispatch_approved`]) and by both tool-list paths —
    /// [`crate::agentbridge::AgentBridge::tools`], which is the live one, and
    /// [`Bus::tools`], its lower-level twin — which is what keeps the offered
    /// list and the refusal from disagreeing.
    pub fn agents_paused(&self) -> bool {
        self.agents_paused.load(Ordering::SeqCst) || self.halt.agents_halted()
    }

    /// What the palette should show. `None` for the operator, who sees
    /// everything; a grant for an agent session, which sees its own scope.
    pub fn list(&self, grant: Option<&Grant>) -> Vec<Listing> {
        self.registry.list(grant)
    }

    /// The MCP tool surface for one agent session.
    pub fn tools(&self, session: &AgentSessionId) -> Vec<ToolDefinition> {
        // A held agent is offered nothing, which is the same read `plan` and
        // `dispatch_approved` refuse by.
        //
        // `AgentBridge::tools` is the live list path and gates there already,
        // so this is not closing a reachable hole — it is closing the gap
        // between two methods that look interchangeable. This one is public on
        // a public type and reads like THE tool surface, so the first caller
        // to pick it (the Phase 2 node daemon is the obvious candidate) would
        // serve a full tool list during a pause or a lockdown without ever
        // touching this file.
        //
        // `Registry::tools` deliberately stays pause-agnostic: a registry
        // cannot see a hold, and `Bus` is the only layer that sees both.
        if self.agents_paused() {
            return Vec::new();
        }
        // Offered if ANY ONE grant allows it, which is what the dispatch
        // check will decide too. Asking per grant and merging the ANSWERS is
        // not the same as merging the grants and asking once: the second
        // offers tools no single grant covers, and every one of them is then
        // refused on use.
        //
        // No grant is no tools, rather than every tool followed by a refusal
        // for each one.
        let mut out: Vec<ToolDefinition> = Vec::new();
        for grant in self.grants.grants_for(session) {
            for tool in self.registry.tools(&grant) {
                if !out.iter().any(|t| t.name == tool.name) {
                    out.push(tool);
                }
            }
        }
        out.sort_by(|a, b| a.name.cmp(&b.name));
        out
    }

    /// Authorize a request and hand back the work.
    ///
    /// Nothing has run when this returns; see [`Admitted`].
    pub fn dispatch(&self, request: Request) -> Result<Admitted, Refused> {
        let dispatch = self.env.new_dispatch_id();
        let at = self.env.now();

        let (entry, node, route) = match self.plan(&request) {
            Ok(planned) => planned,
            Err(refusal) => {
                self.record_refusal(&dispatch, at, &request, &refusal);
                return Err(refusal);
            }
        };
        let spec = entry.spec().clone();

        match route {
            Route::Run => self.start(dispatch, at, request, entry, node, spec),
            Route::Confirm => {
                self.record_or_refuse(
                    LogEntry {
                        dispatch: dispatch.clone(),
                        at,
                        node,
                        command: spec.id().clone(),
                        risk: Some(spec.risk()),
                        issuer: request.issuer.clone(),
                        decision: Decision::AwaitingConfirmation,
                        params: logged_params(&spec, &request.params),
                        detail: None,
                    },
                    &spec,
                )?;
                Ok(Admitted::NeedsConfirmation {
                    dispatch,
                    risk: spec.risk(),
                    prompt: confirmation_prompt(&spec),
                })
            }
            Route::Queue => {
                let pending = PendingCommand {
                    dispatch: dispatch.clone(),
                    command: spec.id().clone(),
                    title: spec.title().to_string(),
                    risk: spec.risk(),
                    issuer: request.issuer.clone(),
                    node: node.clone(),
                    // The Inbox needs the real parameters to show what is
                    // being asked for and to run it afterwards, redaction or
                    // not: redaction keeps secrets out of the *log*, which
                    // outlives the decision.
                    params: request.params.clone(),
                    requested_at: at,
                };
                let pending_id = match self.approvals.submit(&pending) {
                    Ok(id) => id,
                    Err(ApprovalError::NoInbox) => {
                        let refusal = Refused::NoInbox {
                            command: spec.id().clone(),
                        };
                        self.record_refusal(&dispatch, at, &request, &refusal);
                        return Err(refusal);
                    }
                    Err(ApprovalError::Failed(message)) => {
                        let refusal = Refused::InboxFailed {
                            command: spec.id().clone(),
                            message,
                        };
                        self.record_refusal(&dispatch, at, &request, &refusal);
                        return Err(refusal);
                    }
                };
                self.record_or_refuse(
                    LogEntry {
                        dispatch: dispatch.clone(),
                        at,
                        node,
                        command: spec.id().clone(),
                        risk: Some(spec.risk()),
                        issuer: request.issuer.clone(),
                        decision: Decision::Queued,
                        params: logged_params(&spec, &request.params),
                        detail: Some(format!("inbox {pending_id}")),
                    },
                    &spec,
                )?;
                Ok(Admitted::Queued {
                    dispatch,
                    pending: pending_id,
                    risk: spec.risk(),
                })
            }
        }
    }

    /// Run a command the operator approved in the Inbox.
    ///
    /// Everything except the risk routing is checked again, because time
    /// passed while it waited: the app may have stopped, the session's grant
    /// may have been withdrawn, or the operator may have hit lockdown.
    pub fn dispatch_approved(
        &self,
        request: Request,
        approval: Approval,
    ) -> Result<Running, Refused> {
        let dispatch = self.env.new_dispatch_id();
        let at = self.env.now();

        let checked = (|| {
            // Lockdown freezes pending Inbox items, and an approval is how one
            // leaves the Inbox. Without this check, pausing agents stops new
            // dispatches while everything already waiting stays releasable,
            // which is the opposite of what the lever is for.
            if request.issuer.is_agent() && self.agents_paused() {
                return Err(Refused::AgentsPaused);
            }
            let entry = self.entry(&request)?;
            let spec = entry.spec().clone();
            let node = self.resolve_node(&spec, request.target.as_ref())?;
            if let Issuer::Agent { session } = &request.issuer {
                self.check_agent(session, &spec, &node)?;
            }
            Ok((entry, node, spec))
        })();

        let (entry, node, spec) = match checked {
            Ok(checked) => checked,
            Err(refusal) => {
                self.record_refusal(&dispatch, at, &request, &refusal);
                return Err(refusal);
            }
        };

        match self.start_inner(
            dispatch.clone(),
            at,
            request,
            entry,
            node,
            spec,
            Some(format!("approved as {}", approval.pending())),
        )? {
            Admitted::Running(running) => Ok(running),
            // `start_inner` only ever returns `Running`; this arm exists so
            // adding a variant to `Admitted` is a compile error here rather
            // than a surprise at runtime.
            _ => unreachable!("an approved dispatch always runs"),
        }
    }

    /// Everything that decides whether a request may run, and where.
    fn plan(&self, request: &Request) -> Result<(Arc<dyn Erased>, NodeId, Route), Refused> {
        // Checked before the registry is touched so a paused agent gets one
        // clear answer rather than a different error per command.
        if request.issuer.is_agent() && self.agents_paused() {
            return Err(Refused::AgentsPaused);
        }

        let entry = self.entry(request)?;
        let spec = entry.spec();
        let node = self.resolve_node(spec, request.target.as_ref())?;

        // The risk level comes from here, and only from here.
        let risk = spec.risk();

        let route = match &request.issuer {
            Issuer::Operator { .. } | Issuer::Cli => {
                if risk.needs_confirmation_from_operator() && !request.confirmed {
                    Route::Confirm
                } else {
                    Route::Run
                }
            }
            Issuer::Agent { session } => {
                self.check_agent(session, spec, &node)?;
                // `confirmed` is not consulted: an agent is a client, and a
                // flag a client sets must never be what lets it past the
                // Inbox.
                if risk.needs_approval_from_agents() {
                    Route::Queue
                } else {
                    Route::Run
                }
            }
        };

        Ok((entry, node, route))
    }

    fn entry(&self, request: &Request) -> Result<Arc<dyn Erased>, Refused> {
        let entry = self
            .registry
            .lookup(&request.command)
            .ok_or_else(|| Refused::UnknownCommand(request.command.clone()))?;
        if let Enablement::Disabled { reason } = entry.enablement() {
            return Err(Refused::Disabled {
                command: request.command.clone(),
                reason,
            });
        }
        Ok(entry)
    }

    fn check_agent(
        &self,
        session: &AgentSessionId,
        spec: &CommandSpec,
        node: &NodeId,
    ) -> Result<(), Refused> {
        let grants = self.grants.grants_for(session);
        if grants.is_empty() {
            return Err(Refused::NoGrant {
                session: session.clone(),
            });
        }

        // Each grant whole. One grant has to clear all three gates by
        // itself; clearing them between two grants is a combination the
        // operator never issued. For a session holding a single grant this
        // is exactly the old sequence.
        let mut covered = false;
        let mut under_ceiling = false;
        for grant in &grants {
            if !grant.covers(spec.id()) {
                continue;
            }
            covered = true;
            if spec.risk() > grant.ceiling {
                continue;
            }
            under_ceiling = true;
            if grant.allows_node(node, &self.node) {
                return Ok(());
            }
        }

        // The refusal names the furthest gate any one grant reached, so the
        // message tells the operator which part to widen rather than always
        // blaming the first.
        Err(if !covered {
            Refused::OutsideGrant {
                session: session.clone(),
                command: spec.id().clone(),
            }
        } else if !under_ceiling {
            Refused::AboveCeiling {
                session: session.clone(),
                command: spec.id().clone(),
                risk: spec.risk(),
            }
        } else {
            Refused::NodeNotAllowed {
                session: session.clone(),
                node: node.clone(),
            }
        })
    }

    /// Which node this dispatch lands on.
    ///
    /// A caller may ask for a node only where the spec left it open; it can
    /// never move a command off the node its backend declared.
    // TODO: forward to another node once the node daemon and signed
    // node-to-node commands exist. Until then, asking for a node that is not
    // this one is refused rather than silently run locally, which would be
    // the one failure mode that could touch the wrong machine's files.
    fn resolve_node(
        &self,
        spec: &CommandSpec,
        requested: Option<&NodeId>,
    ) -> Result<NodeId, Refused> {
        let wanted = match spec.target() {
            Target::ThisNode => self.node.clone(),
            Target::Node(pinned) => pinned.clone(),
            Target::AnyNode => requested.cloned().unwrap_or_else(|| self.node.clone()),
        };
        if let (Target::ThisNode | Target::Node(_), Some(asked)) = (spec.target(), requested)
            && asked != &wanted
        {
            return Err(Refused::WrongNodeAsked {
                command: spec.id().clone(),
                asked: asked.clone(),
                runs_on: wanted,
            });
        }
        if wanted != self.node {
            return Err(Refused::NotLocal {
                command: spec.id().clone(),
                wanted,
                this: self.node.clone(),
            });
        }
        Ok(wanted)
    }

    fn start(
        &self,
        dispatch: DispatchId,
        at: SystemTime,
        request: Request,
        entry: Arc<dyn Erased>,
        node: NodeId,
        spec: CommandSpec,
    ) -> Result<Admitted, Refused> {
        self.start_inner(dispatch, at, request, entry, node, spec, None)
    }

    // Eight arguments, and clippy is right that that is a lot. They are one
    // thing though — the context of a single dispatch — and bundling them into
    // a struct here would only move the list. Worth doing when a ninth arrives.
    #[allow(clippy::too_many_arguments)]
    fn start_inner(
        &self,
        dispatch: DispatchId,
        at: SystemTime,
        request: Request,
        entry: Arc<dyn Erased>,
        node: NodeId,
        spec: CommandSpec,
        detail: Option<String>,
    ) -> Result<Admitted, Refused> {
        self.record_or_refuse(
            LogEntry {
                dispatch: dispatch.clone(),
                at,
                node: node.clone(),
                command: spec.id().clone(),
                risk: Some(spec.risk()),
                issuer: request.issuer.clone(),
                decision: Decision::Ran,
                params: logged_params(&spec, &request.params),
                detail,
            },
            &spec,
        )?;

        let (tx, rx) = mpsc::channel(self.progress_buffer);
        let cx = Context {
            dispatch: dispatch.clone(),
            command: spec.id().clone(),
            risk: spec.risk(),
            issuer: request.issuer.clone(),
            node: node.clone(),
            started: at,
            progress: ProgressSink {
                dispatch: dispatch.clone(),
                tx,
            },
        };

        let future = entry.call(request.params, cx);

        Ok(Admitted::Running(Running {
            dispatch,
            command: spec.id().clone(),
            issuer: request.issuer,
            node,
            risk: spec.risk(),
            started: at,
            progress: rx,
            future,
            env: Arc::clone(&self.env),
            journal: Arc::clone(&self.journal),
        }))
    }

    /// Write an admission to the log, and decide what an unwritable log means.
    ///
    /// For a destructive, outbound or cross-project command it means refusal:
    /// those are exactly the actions the log exists to record, and running one
    /// unrecorded is worse than not running it. For the rest it is a warning,
    /// because making the whole desktop stop working over an audit-log write
    /// would be its own kind of outage.
    fn record_or_refuse(&self, entry: LogEntry, spec: &CommandSpec) -> Result<(), Refused> {
        let risk = spec.risk();
        match self.journal.record(entry) {
            Ok(()) => Ok(()),
            Err(e) if risk.needs_confirmation_from_operator() => Err(Refused::NotRecorded {
                command: spec.id().clone(),
                risk,
                message: e.to_string(),
            }),
            Err(e) => {
                tracing::warn!(error = %e, command = %spec.id(), "command not recorded");
                Ok(())
            }
        }
    }

    /// Refusals are logged too: "a burst of refused agent commands" is one of
    /// the lockdown triggers, and it can only be noticed if refusals are
    /// written down.
    fn record_refusal(
        &self,
        dispatch: &DispatchId,
        at: SystemTime,
        request: &Request,
        refusal: &Refused,
    ) {
        let spec = self.registry.spec(&request.command);
        // No spec means no redaction flag, so there is nothing that says these
        // parameters are safe to write down. The registry is mutable by
        // design — an on-demand app's commands go away when it stops — so the
        // keystrokes of a `terminal.write` that arrived just after the
        // terminal app was unregistered would otherwise be logged in full,
        // which is exactly what that command's `REDACT_PARAMS` exists to
        // prevent. The command id and the reason are enough to act on.
        let params = spec
            .as_ref()
            .and_then(|s| logged_params(s, &request.params));
        if let Err(e) = self.journal.record(LogEntry {
            dispatch: dispatch.clone(),
            at,
            node: self.node.clone(),
            command: request.command.clone(),
            risk: spec.as_ref().map(|s| s.risk()),
            issuer: request.issuer.clone(),
            decision: Decision::Refused,
            params,
            detail: Some(refusal.to_string()),
        }) {
            tracing::warn!(error = %e, command = %request.command, "refusal not recorded");
        }
    }
}

/// Parameters as they go into the log: nothing at all for a command whose
/// spec redacts them, rather than a shape an attacker could learn from.
fn logged_params(spec: &CommandSpec, params: &Value) -> Option<Value> {
    if spec.redacts_params() {
        Some(Value::String("<redacted>".to_string()))
    } else {
        Some(params.clone())
    }
}

/// What the confirmation dialog asks. Phrased from the risk level because the
/// backend declared that and the UI did not, so the dialog cannot
/// under-describe what it is about to do.
fn confirmation_prompt(spec: &CommandSpec) -> String {
    let what = match spec.risk() {
        RiskLevel::Destructive => "This removes or overwrites data and cannot be undone.",
        RiskLevel::Outbound => "This leaves this machine and cannot be taken back.",
        RiskLevel::CrossProject => "This acts outside the project you are in.",
        // Not reachable through routing, which only asks for a confirmation
        // above `Write`. Spelled out anyway rather than left to a `_` arm.
        RiskLevel::Write | RiskLevel::ReadOnly => "",
    };
    format!("{} {}", spec.title(), what).trim().to_string()
}
