//! MCP tools, generated from the command bus.
//!
//! An agent gets no shell, no file tool and no hook to talk its way past: the
//! only tools it is ever sent are Deckjack commands. This module is where
//! that claim is either structurally true or a comfortable lie. See
//! docs/DESIGN.md, "Agents".
//!
//! It does three things.
//!
//! 1. **Translate.** One registered command — id, title, parameter JSON
//!    Schema, risk level, owning node — becomes one MCP tool definition.
//! 2. **Filter.** A session's grant decides which tools it is *shown*. An
//!    agent is never offered a tool it would be refused for calling, because
//!    offering and then refusing teaches a model that the boundary is
//!    negotiable and worth another attempt with different wording.
//! 3. **Dispatch.** A `tools/call` becomes one bus dispatch carrying the
//!    agent's identity, so attribution, risk routing and the Inbox gate apply
//!    exactly as they do to the same command run from the palette. There is
//!    no second code path with weaker rules.
//!
//! # Why this is a guarantee and not a convention
//!
//! [`McpTool`] has private fields, no public constructor and no
//! `Deserialize`. The only code in existence that can build one is the loop
//! inside [`ToolSurface::emit`], and that loop can only iterate what the
//! [`Catalog`] reports. So "every tool is backed by a registered command" is
//! a fact the compiler enforces, not a rule a future contributor has to
//! remember while adding a feature at 1am.
//!
//! # Names are looked up, never decoded
//!
//! Command ids are dotted (`files.read.text`). MCP tool names are restricted
//! to `[A-Za-z0-9_-]`, so the encoding (`.` becomes `__`) cannot be reversed
//! without ambiguity. The call path therefore never reverses it: it resolves
//! an incoming name by matching it against names *generated from the
//! catalog*, so a name either matches a registered command or matches
//! nothing. No amount of cleverness in a tool name reaches a command id that
//! was never registered.
//!
//! # The grant is checked again at call time
//!
//! A tool list an agent is holding may be minutes old, and a grant can be
//! narrowed or revoked between list and call — the kill switch does exactly
//! that. Filtering the list is a courtesy to the model; the call-time check
//! is the control.
//!
//! # Intended use
//!
//! ```text
//! let surface = ToolSurface::new(bus_catalog, bus_dispatch);
//!
//! // tools/list for one agent session
//! let listing = surface.list(&session_grant);
//! for skipped in &listing.skipped {
//!     // A registration bug, not a permission decision: tell the operator.
//!     tracing::error!(command = %skipped.command_id, "command cannot be a tool: {}", skipped.why);
//! }
//! let tools: Vec<rmcp::model::Tool> = listing.tools.iter().map(McpTool::to_rmcp).collect();
//!
//! // tools/call
//! let result = surface.call_tool(&agent, &session_grant, &name, args).await;
//! ```

use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::sync::Arc;

use serde::Serialize;
use serde_json::{Map, Value};
use thiserror::Error;

/// A JSON Schema, or any other JSON object. Same shape rmcp uses, so the
/// mapping at the bottom of this file is a move rather than a rebuild.
pub type JsonObject = Map<String, Value>;

/// Longest tool name MCP clients accept. Claude's tool interface uses the
/// same limit, so a name that fits here fits everywhere Deckjack sends it.
pub const MAX_TOOL_NAME: usize = 128;

/// What a `.` in a command id becomes. Two characters rather than one so
/// `files.read` and `files_read` cannot collide by accident — and when two
/// ids do collide anyway, [`SkipReason::NameCollision`] reports it instead of
/// letting one silently shadow the other.
pub const NAMESPACE_SEPARATOR: &str = "__";

// ---------------------------------------------------------------------------
// What this module needs to know about a command
// ---------------------------------------------------------------------------

/// Where a command sits in the Agents design's risk routing.
///
/// This is the MCP-facing projection of the bus's own risk level, not a second
/// opinion about it. The backend declares risk; nothing here infers it, so an
/// adapter bug cannot downgrade a destructive command by omission. The
/// integrator maps `crate::command`'s risk enum onto this in exactly one
/// place, and a new risk level there is a compile error here rather than a
/// quiet default.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolRisk {
    /// Runs immediately. Reads state and changes nothing.
    ReadOnly,
    /// Runs immediately under the session's grant. A write inside the project
    /// the session was given.
    Write,
    /// Goes to the Assistant's Inbox and waits for the operator.
    Gated(GateReason),
}

/// Why a command waits for approval. Told to the agent in the tool
/// description, so it can plan around the wait instead of discovering it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GateReason {
    /// Deletes or overwrites; a bulk change counts.
    Destructive,
    /// Leaves the machine: mail, messages, uploads, posts.
    Outbound,
    /// Touches something outside the project the session was granted.
    CrossProject,
}

impl GateReason {
    /// The sentence appended to the tool description. Written at the agent,
    /// in the second person, because that is who reads it.
    fn explanation(self) -> &'static str {
        match self {
            GateReason::Destructive => {
                "This command is destructive, so it will not run when you call it: \
                 it waits in the operator's Inbox for approval."
            }
            GateReason::Outbound => {
                "This command sends something outside this machine, so it will not run \
                 when you call it: it waits in the operator's Inbox for approval."
            }
            GateReason::CrossProject => {
                "This command reaches outside the project you were given, so it will not \
                 run when you call it: it waits in the operator's Inbox for approval."
            }
        }
    }
}

/// One registered command, as the bus describes it to this module.
///
/// A narrow, owned snapshot rather than a borrow of the bus's own type: the
/// registry changes while apps start, stop and crash, and copying a few
/// hundred small structs per `tools/list` is cheaper than holding a lock
/// across the translation.
#[derive(Debug, Clone, PartialEq)]
pub struct CommandSpec {
    /// Dotted bus id, for example `files.read.text`.
    pub id: String,
    /// Short human title, as the command palette shows it.
    pub title: String,
    /// Longer description. Worth writing: for an agent this is the entire
    /// documentation of the command.
    pub summary: Option<String>,
    /// JSON Schema for the parameters, generated from the Rust type.
    pub parameters: JsonObject,
    /// JSON Schema for the result, when the command has one.
    pub result: Option<JsonObject>,
    /// Declared by the backend. See [`ToolRisk`].
    pub risk: ToolRisk,
    /// The node that owns the backend, or `None` for the shell node.
    pub node: Option<String>,
    /// Commands disable themselves when they make no sense right now (no
    /// selection, no document open). A disabled command is not offered,
    /// because a tool that always fails is worse than a tool that is absent.
    pub enabled: bool,
}

/// The authoritative list of registered commands.
///
/// Deliberately the only way tools can come into existence. Implemented by
/// the command bus registry; nothing else should implement it, and in
/// particular nothing that reads a list of tool names from a file or a
/// request body.
pub trait Catalog {
    /// Every command registered right now, in any order. Order does not
    /// matter: [`ToolSurface::emit`] sorts before resolving collisions so the
    /// surface does not depend on registration timing.
    fn commands(&self) -> Vec<CommandSpec>;
}

/// What one agent session is allowed to do.
///
/// A narrow view of `crate::grants`: namespace patterns, expiry and the
/// grant's own storage are that module's business. All this module needs is a
/// yes or no, which keeps the two testable apart.
pub trait Grant {
    /// Whether this session may call the command with this dotted id.
    fn allows_command(&self, command_id: &str) -> bool;
    /// Whether this session may target this node. `None` is the shell node.
    fn allows_node(&self, node_id: Option<&str>) -> bool;
}

// ---------------------------------------------------------------------------
// Attribution
// ---------------------------------------------------------------------------

/// Who an agent's command is recorded as. Every command records its issuer in
/// the append-only log, and an agent's commands are not exempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentIdentity {
    /// The agent session's id, as the Assistant knows it. Stable across the
    /// session so the timeline panel can group by it.
    pub session: String,
    /// What to call it in the log and the Inbox: "Assistant", "coding
    /// session: deckjack".
    pub label: String,
    /// Set when the agent was invited into one window as a co-pilot, so the
    /// log can say "Assistant via window X" and the operator can see which
    /// surface it was acting through.
    pub via_window: Option<String>,
}

impl AgentIdentity {
    /// The attribution line for the log and the Inbox.
    pub fn attribution(&self) -> String {
        match &self.via_window {
            Some(window) => format!("{} via window {window}", self.label),
            None => self.label.clone(),
        }
    }
}

// ---------------------------------------------------------------------------
// The call path
// ---------------------------------------------------------------------------

/// One dispatch onto the bus, built from a tool call.
///
/// The risk level travels with it, taken from the registered command rather
/// than from anything the agent said, so the bus routes on the backend's
/// declaration even if a caller of this module got careless.
#[derive(Debug, Clone, PartialEq)]
pub struct BusCall {
    pub command_id: String,
    pub arguments: JsonObject,
    pub node: Option<String>,
    pub risk: ToolRisk,
    pub issuer: AgentIdentity,
}

/// What the bus did with a dispatch.
#[derive(Debug, Clone, PartialEq)]
pub enum Dispatched {
    /// Ran. `result` is whatever the command returned, `Value::Null` for a
    /// command with no result.
    Done { result: Value },
    /// Queued in the Assistant's Inbox. Nothing has run.
    Queued { approval_id: String },
    /// The bus itself said no — the command disabled itself between listing
    /// and calling, or a precondition failed. Distinct from
    /// [`CallError::OutsideGrant`], which is this module's decision.
    Refused { reason: String },
}

/// Why a dispatch could not be completed. The bus has richer errors; the
/// integrator maps them onto these three, which are the three an agent can
/// act on differently.
#[derive(Debug, Error)]
pub enum DispatchError {
    #[error("{command_id} rejected the arguments: {message}")]
    BadArguments { command_id: String, message: String },

    #[error("{command_id} failed: {message}")]
    Failed { command_id: String, message: String },

    #[error("node {node} is not reachable; the command was not run")]
    NodeUnreachable { node: String },
}

/// The bus, as this module needs it.
///
/// Return-position `impl Future` rather than a boxed one: the MCP server
/// holds one concrete bus, so there is nothing to gain from object safety and
/// an allocation per tool call to lose.
pub trait Dispatch {
    fn dispatch(
        &self,
        call: BusCall,
    ) -> impl Future<Output = Result<Dispatched, DispatchError>> + Send;
}

// ---------------------------------------------------------------------------
// The tool definition
// ---------------------------------------------------------------------------

/// One MCP tool, generated from one registered command.
///
/// Fields are private and there is no constructor. That is the whole security
/// argument of this module: see the module docs.
#[derive(Debug, Clone, PartialEq)]
pub struct McpTool {
    name: String,
    title: String,
    description: String,
    input_schema: JsonObject,
    output_schema: Option<JsonObject>,
    command_id: String,
    risk: ToolRisk,
    node: Option<String>,
}

impl McpTool {
    /// The MCP tool name, derived from the command id.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The command this tool is backed by. Non-optional by construction.
    pub fn command_id(&self) -> &str {
        &self.command_id
    }

    pub fn title(&self) -> &str {
        &self.title
    }

    pub fn description(&self) -> &str {
        &self.description
    }

    pub fn input_schema(&self) -> &JsonObject {
        &self.input_schema
    }

    pub fn output_schema(&self) -> Option<&JsonObject> {
        self.output_schema.as_ref()
    }

    pub fn risk(&self) -> ToolRisk {
        self.risk
    }

    pub fn node(&self) -> Option<&str> {
        self.node.as_deref()
    }
}

/// A registered command that could not become a tool. Every variant is a bug
/// in how a command was registered, so the message says what to change.
///
/// A command withheld by a grant is **not** one of these. It is a permission
/// decision, not a defect, and it is deliberately absent from
/// [`ToolList::skipped`] so an operator reading the surface of one session
/// does not mistake normal scoping for breakage.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum SkipReason {
    #[error(
        "its id contains {character:?}, which MCP does not allow in a tool name; \
         rename the command to letters, digits, '-', '_' and '.'"
    )]
    UnusableName { character: char },

    #[error(
        "its id makes a tool name {length} characters long, over the {} MCP allows; \
         shorten the command id",
        MAX_TOOL_NAME
    )]
    NameTooLong { length: usize },

    #[error("its id makes the same tool name as {other}; rename one of the two")]
    NameCollision { other: String },

    #[error(
        "its parameter schema is not an object schema, and MCP requires one; \
         give the command a parameters struct, or no parameters at all"
    )]
    NotAnObjectSchema,

    #[error(
        "its result schema is not an object schema, and MCP requires one for a tool's \
         output schema; give the command a result struct, or no result at all"
    )]
    ResultNotAnObjectSchema,

    #[error(
        "it is registered twice, so there is no telling which registration a call would \
         reach; unregister the duplicate"
    )]
    RegisteredTwice,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skipped {
    pub command_id: String,
    pub why: SkipReason,
}

/// The answer to `tools/list` for one session, plus the registration problems
/// found on the way. The operator sees `skipped`; the agent never does.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolList {
    pub tools: Vec<McpTool>,
    pub skipped: Vec<Skipped>,
}

/// Why a tool call did not reach the bus.
#[derive(Debug, Error)]
pub enum CallError {
    #[error("there is no tool named {name}; call tools/list for the tools this session has")]
    NoSuchTool { name: String },

    #[error(
        "{name} is outside this session's grant and was not run; ask the operator to widen \
         the grant rather than calling it again"
    )]
    OutsideGrant { name: String },

    #[error(transparent)]
    Dispatch(#[from] DispatchError),
}

// ---------------------------------------------------------------------------
// The surface
// ---------------------------------------------------------------------------

/// The MCP tool surface over the command bus.
///
/// One of these per server, shared by every agent session; the session's
/// grant and identity are passed per call rather than held, so one surface
/// cannot leak another session's scope.
pub struct ToolSurface<C, D> {
    catalog: C,
    dispatch: D,
}

impl<C, D> ToolSurface<C, D>
where
    C: Catalog,
    D: Dispatch,
{
    pub fn new(catalog: C, dispatch: D) -> ToolSurface<C, D> {
        ToolSurface { catalog, dispatch }
    }

    /// Every tool the whole bus can express, keyed by tool name, plus the
    /// commands that could not be expressed.
    ///
    /// Grant-independent on purpose. Name collisions have to be resolved
    /// across *all* registered commands, because if the loser were chosen
    /// from only the granted subset, which of two colliding commands a
    /// session reached would depend on what some other session was allowed to
    /// see. Sorting by command id first makes the choice stable across
    /// restarts and registration order; two registrations of the *same* id
    /// cannot be ordered that way, so neither is offered at all.
    ///
    /// This is the only function in the program that constructs an
    /// [`McpTool`].
    fn emit(&self) -> (BTreeMap<String, McpTool>, Vec<Skipped>) {
        let mut specs = self.catalog.commands();
        specs.sort_by(|a, b| a.id.cmp(&b.id));
        // A disabled command is neither a defect nor a permission decision,
        // so it is reported as neither: it is simply not applicable right now
        // and will reappear in a later list. Dropped before anything else so
        // it neither claims a name nor counts as a duplicate.
        specs.retain(|spec| spec.enabled);

        // The bus dispatches by command id, so if one id is registered twice
        // there is no way to tell from here which registration a call would
        // reach — and the tool would advertise one registration's risk level,
        // node and schema for behavior that might come from the other. Risk
        // routing is the Inbox gate, so guessing is not an option: neither is
        // offered, and both are reported. Picking a winner here would also
        // make the surface depend on registration order, which is whatever
        // apps happened to start first.
        let registered_twice: BTreeSet<String> = {
            let mut once = BTreeSet::new();
            let mut twice = BTreeSet::new();
            for spec in &specs {
                if !once.insert(spec.id.as_str()) {
                    twice.insert(spec.id.clone());
                }
            }
            twice
        };

        let mut tools: BTreeMap<String, McpTool> = BTreeMap::new();
        let mut skipped = Vec::new();

        for spec in specs {
            if registered_twice.contains(&spec.id) {
                skipped.push(Skipped {
                    command_id: spec.id,
                    why: SkipReason::RegisteredTwice,
                });
                continue;
            }

            let name = match tool_name(&spec.id) {
                Ok(name) => name,
                Err(why) => {
                    skipped.push(Skipped {
                        command_id: spec.id,
                        why,
                    });
                    continue;
                }
            };

            if let Some(existing) = tools.get(&name) {
                skipped.push(Skipped {
                    command_id: spec.id,
                    why: SkipReason::NameCollision {
                        other: existing.command_id.clone(),
                    },
                });
                continue;
            }

            let input_schema = match object_schema(&spec.parameters) {
                Some(schema) => schema,
                None => {
                    skipped.push(Skipped {
                        command_id: spec.id,
                        why: SkipReason::NotAnObjectSchema,
                    });
                    continue;
                }
            };

            let output_schema = match result_schema(spec.result.as_ref()) {
                Ok(schema) => schema,
                Err(why) => {
                    skipped.push(Skipped {
                        command_id: spec.id,
                        why,
                    });
                    continue;
                }
            };

            tools.insert(
                name.clone(),
                McpTool {
                    name,
                    title: spec.title.clone(),
                    description: agent_description(&spec),
                    input_schema,
                    output_schema,
                    command_id: spec.id,
                    risk: spec.risk,
                    node: spec.node,
                },
            );
        }

        (tools, skipped)
    }

    /// The tools one session may use. Nothing outside its grant appears.
    pub fn list(&self, grant: &impl Grant) -> ToolList {
        let (tools, skipped) = self.emit();
        let tools = tools
            .into_values()
            .filter(|tool| grant.allows_command(&tool.command_id) && grant.allows_node(tool.node()))
            .collect();
        ToolList { tools, skipped }
    }

    /// One tool by name, if this session has it. Same filter as [`list`], so
    /// a name cannot be probed for existence through the back door.
    ///
    /// [`list`]: ToolSurface::list
    pub fn describe(&self, grant: &impl Grant, name: &str) -> Option<McpTool> {
        let (mut tools, _) = self.emit();
        let tool = tools.remove(name)?;
        (grant.allows_command(&tool.command_id) && grant.allows_node(tool.node())).then_some(tool)
    }

    /// Turn a `tools/call` into one bus dispatch.
    ///
    /// The name is resolved against the catalog, never decoded; the grant is
    /// rechecked here because the agent's tool list may be stale; and the
    /// risk level travels from the registered command, so routing to the
    /// Inbox cannot be skipped by anything in the request.
    ///
    /// Arguments are passed through unvalidated on purpose. The bus
    /// deserializes them into the command's own Rust type, which is the real
    /// check; a JSON Schema validator here would be a second, slightly
    /// different opinion about what is valid, and the gap between two
    /// validators is where parser-differential bugs live.
    pub async fn call(
        &self,
        agent: &AgentIdentity,
        grant: &impl Grant,
        name: &str,
        arguments: Option<JsonObject>,
    ) -> Result<Dispatched, CallError> {
        let (mut tools, _) = self.emit();

        let Some(tool) = tools.remove(name) else {
            // Also the answer for a command that exists but is disabled, and
            // for one whose registration is broken. The agent has one thing
            // to do about all three, which is call tools/list again.
            return Err(CallError::NoSuchTool {
                name: name.to_string(),
            });
        };

        if !grant.allows_command(&tool.command_id) || !grant.allows_node(tool.node()) {
            // Refused, never queued: the Inbox is for commands a session is
            // allowed to ask for, not a place to appeal its scope. Logged at
            // warn because a burst of these is one of the lockdown triggers.
            tracing::warn!(
                target: "deckjack::agent",
                command = %tool.command_id,
                issuer = %agent.attribution(),
                "refused an agent command outside its grant"
            );
            return Err(CallError::OutsideGrant {
                name: name.to_string(),
            });
        }

        let call = BusCall {
            command_id: tool.command_id.clone(),
            arguments: arguments.unwrap_or_default(),
            node: tool.node.clone(),
            risk: tool.risk,
            issuer: agent.clone(),
        };

        tracing::info!(
            target: "deckjack::agent",
            command = %call.command_id,
            issuer = %agent.attribution(),
            "dispatching an agent command"
        );

        Ok(self.dispatch.dispatch(call).await?)
    }

    /// [`call`], folded into the single value an MCP `tools/call` has to
    /// return. A refusal is a tool result the agent can read and reason
    /// about, not a protocol error, because a protocol error is the kind of
    /// failure a model retries blindly.
    ///
    /// [`call`]: ToolSurface::call
    pub async fn call_tool(
        &self,
        agent: &AgentIdentity,
        grant: &impl Grant,
        name: &str,
        arguments: Option<JsonObject>,
    ) -> rmcp::model::CallToolResult {
        match self.call(agent, grant, name, arguments).await {
            Ok(dispatched) => dispatched.to_rmcp(),
            Err(err) => err.to_rmcp(),
        }
    }
}

// ---------------------------------------------------------------------------
// Translation details
// ---------------------------------------------------------------------------

/// Encode a dotted command id as an MCP tool name.
///
/// Only the three transformations a command id can need: `.` becomes
/// [`NAMESPACE_SEPARATOR`], allowed characters pass through, and anything
/// else is a registration bug rather than something to quietly strip.
/// Stripping would make two different ids produce one name, which is the
/// ambiguity this whole scheme exists to avoid.
fn tool_name(command_id: &str) -> Result<String, SkipReason> {
    let mut name = String::with_capacity(command_id.len());
    for ch in command_id.chars() {
        match ch {
            '.' => name.push_str(NAMESPACE_SEPARATOR),
            'a'..='z' | 'A'..='Z' | '0'..='9' | '_' | '-' => name.push(ch),
            other => return Err(SkipReason::UnusableName { character: other }),
        }
    }
    if name.is_empty() {
        // An empty id is unnamable, and reporting it as an unusable NUL is
        // closer to the truth than inventing a name for it.
        return Err(SkipReason::UnusableName { character: '\0' });
    }
    if name.len() > MAX_TOOL_NAME {
        return Err(SkipReason::NameTooLong { length: name.len() });
    }
    Ok(name)
}

/// Accept a parameter schema MCP can carry, or say no.
///
/// MCP requires the input schema's root to be an object schema. A command
/// with no parameters often generates `{"type": "null"}` or nothing at all,
/// which is the same statement in a different dialect, so it is translated
/// rather than refused. A schema that genuinely describes a non-object — an
/// array or a bare string — is a registration mistake: the agent would have
/// nowhere to put the value.
fn object_schema(parameters: &JsonObject) -> Option<JsonObject> {
    let empty = || {
        let mut schema = JsonObject::new();
        schema.insert("type".to_string(), Value::String("object".to_string()));
        schema.insert("properties".to_string(), Value::Object(JsonObject::new()));
        schema
    };

    match parameters.get("type") {
        None if parameters.is_empty() => Some(empty()),
        Some(Value::String(t)) if t == "null" => Some(empty()),
        Some(Value::String(t)) if t == "object" => Some(parameters.clone()),
        // A `$ref` to a definition, as schemars emits for a named struct.
        // Carried through: the client resolves it against `$defs` in the same
        // document, and rewriting someone else's schema here would be a
        // second schema compiler to keep correct.
        None if parameters.contains_key("$ref") => Some(parameters.clone()),
        _ => None,
    }
}

/// Accept a result schema MCP can carry, or say no.
///
/// MCP requires the root of `outputSchema` to be an object schema, exactly as
/// it does for the input, and a client that cannot validate `structuredContent`
/// against it may reject the tool or the whole `tools/list` response — so one
/// command registered with an array result would take the entire surface down
/// with it. Checking here turns that into one reported registration bug.
///
/// There is nothing to synthesize, unlike [`object_schema`]: a command with no
/// result simply has no output schema. `null` and an empty schema are the same
/// statement in a different dialect and are translated to that rather than
/// refused.
fn result_schema(result: Option<&JsonObject>) -> Result<Option<JsonObject>, SkipReason> {
    let Some(schema) = result else {
        return Ok(None);
    };
    match schema.get("type") {
        None if schema.is_empty() => Ok(None),
        Some(Value::String(t)) if t == "null" => Ok(None),
        Some(Value::String(t)) if t == "object" => Ok(Some(schema.clone())),
        // A `$ref` into `$defs`, as schemars emits for a named struct. Carried
        // through for the same reason the input path carries it: resolving
        // someone else's schema here would be a second schema compiler.
        None if schema.contains_key("$ref") => Ok(Some(schema.clone())),
        _ => Err(SkipReason::ResultNotAnObjectSchema),
    }
}

/// The description the agent reads.
///
/// Title, then the command's own summary, then the risk sentence. The risk
/// sentence is appended here rather than left to each command's author
/// because it has to be present on every gated command without exception,
/// and a convention that depends on remembering is a convention that is
/// eventually forgotten.
fn agent_description(spec: &CommandSpec) -> String {
    let mut out = String::new();
    out.push_str(&spec.title);
    if let Some(summary) = &spec.summary {
        let summary = summary.trim();
        if !summary.is_empty() {
            out.push_str("\n\n");
            out.push_str(summary);
        }
    }
    if let ToolRisk::Gated(reason) = spec.risk {
        out.push_str("\n\n");
        out.push_str(reason.explanation());
    }
    out
}

// ---------------------------------------------------------------------------
// rmcp mapping
// ---------------------------------------------------------------------------
//
// Everything above is plain data and knows nothing about rmcp. This section
// is the only place that does, so an rmcp upgrade is read and fixed here and
// nowhere else. Its types are `#[non_exhaustive]`, which is why these go
// through the constructors and then assign fields rather than using struct
// literals.

/// What goes in the `structuredContent` of a result the agent must not retry.
#[derive(Debug, Serialize)]
struct Status<'a> {
    status: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    approval_id: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<&'a str>,
}

impl McpTool {
    /// The rmcp tool definition to put in `tools/list`.
    pub fn to_rmcp(&self) -> rmcp::model::Tool {
        let mut tool = rmcp::model::Tool::new(
            self.name.clone(),
            self.description.clone(),
            Arc::new(self.input_schema.clone()),
        );
        tool.title = Some(self.title.clone());
        tool.output_schema = self.output_schema.clone().map(Arc::new);
        tool.annotations = Some(self.annotations());
        tool
    }

    /// MCP's behavior hints, from the backend's declared risk level.
    ///
    /// `idempotent_hint` is left unset for anything that writes: whether a
    /// write can be repeated safely is a property of the command that the
    /// risk level does not record, and claiming it is safe would be a guess
    /// the agent would act on.
    fn annotations(&self) -> rmcp::model::ToolAnnotations {
        let mut a = rmcp::model::ToolAnnotations::default();
        a.title = Some(self.title.clone());
        match self.risk {
            ToolRisk::ReadOnly => {
                a.read_only_hint = Some(true);
                a.destructive_hint = Some(false);
                a.idempotent_hint = Some(true);
                a.open_world_hint = Some(false);
            }
            ToolRisk::Write => {
                a.read_only_hint = Some(false);
                a.destructive_hint = Some(false);
                a.open_world_hint = Some(false);
            }
            ToolRisk::Gated(reason) => {
                a.read_only_hint = Some(false);
                a.destructive_hint = Some(matches!(reason, GateReason::Destructive));
                a.open_world_hint = Some(matches!(reason, GateReason::Outbound));
            }
        }
        a
    }
}

impl Dispatched {
    /// The MCP result for what the bus did.
    pub fn to_rmcp(&self) -> rmcp::model::CallToolResult {
        use rmcp::model::{CallToolResult, Content};

        match self {
            Dispatched::Done { result } if result.is_null() => {
                CallToolResult::success(vec![Content::text("Done.")])
            }
            Dispatched::Done { result } => {
                // Both forms: `structuredContent` for anything parsing the
                // result, and text because that is what a model actually
                // reads. The text is the command's output as data; it is
                // never treated as instructions, here or further up.
                let text =
                    serde_json::to_string_pretty(result).unwrap_or_else(|_| result.to_string());
                let mut out = CallToolResult::success(vec![Content::text(text)]);
                // `structuredContent` is an object in the protocol, not any
                // JSON value, so a command that returns an array or a bare
                // string gets the text form only. Sending the value there
                // anyway would make the whole result off-spec, and a client
                // that validates it would throw away a result the command
                // actually produced. Nothing is lost: the text above is that
                // same value, pretty-printed.
                if result.is_object() {
                    out.structured_content = Some(result.clone());
                }
                out
            }
            Dispatched::Queued { approval_id } => {
                // Success, not an error: nothing went wrong. The wording is
                // doing real work — told only that it was "queued", a model
                // will call again to check, and a retry loop against the
                // Inbox is both a denial-of-service on the operator's
                // attention and a way to wear down a human's judgment.
                let text = format!(
                    "Not run. This command needs the operator's approval and is waiting in \
                     their Inbox as approval {approval_id}. Do not call it again and do not \
                     look for another way to do it; continue with other work, or stop and \
                     say what you are waiting for."
                );
                let mut out = CallToolResult::success(vec![Content::text(text)]);
                out.structured_content = serde_json::to_value(Status {
                    status: "awaiting_approval",
                    approval_id: Some(approval_id.as_str()),
                    reason: None,
                })
                .ok();
                out
            }
            Dispatched::Refused { reason } => {
                let mut out =
                    CallToolResult::error(vec![Content::text(format!("Refused: {reason}"))]);
                out.structured_content = serde_json::to_value(Status {
                    status: "refused",
                    approval_id: None,
                    reason: Some(reason.as_str()),
                })
                .ok();
                out
            }
        }
    }
}

impl CallError {
    /// The MCP result for a call that never reached the bus.
    pub fn to_rmcp(&self) -> rmcp::model::CallToolResult {
        use rmcp::model::{CallToolResult, Content};

        let status = match self {
            CallError::NoSuchTool { .. } => "no_such_tool",
            CallError::OutsideGrant { .. } => "outside_grant",
            CallError::Dispatch(_) => "failed",
        };
        let message = self.to_string();
        let mut out = CallToolResult::error(vec![Content::text(message.clone())]);
        out.structured_content = serde_json::to_value(Status {
            status,
            approval_id: None,
            reason: Some(message.as_str()),
        })
        .ok();
        out
    }
}
