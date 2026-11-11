//! The bridge between [`crate::mcp`]'s injected traits and the real bus.
//!
//! `mcp.rs` is written against three traits — [`mcp::Catalog`],
//! [`mcp::Dispatch`] and [`mcp::Grant`] — and says plainly that the
//! integrator provides them. Nobody did, so a module whose whole argument is
//! "every tool an agent is sent is a registered command" had no registry
//! behind it and nothing was generated from anything. This file is those
//! three implementations and the two callers that use them.
//!
//! # Two callers, one path
//!
//! 1. **An app invoking a command.** [`crate::apphost`] serves
//!    `AppFrame::Invoke` through [`AgentBridge::invoke_for_app`]. This is the
//!    path that exists today and that `tests/agentbridge.rs` drives over a
//!    real socket.
//! 2. **An agent calling an MCP tool.** [`AgentBridge::tools`] and
//!    [`AgentBridge::call_tool`] are the surface an MCP transport would
//!    serve. There is no such transport in this build and nothing issues
//!    agent sessions, so this half is constructed and tested but has no user
//!    yet. Said here rather than discovered later.
//!
//! Both go through one [`BusInvoker`], so there is no second dispatch path
//! with weaker rules — which was the point of `mcp.rs` having a `Dispatch`
//! trait rather than a `Bus` field.
//!
//! # Where authority lives
//!
//! Three checks, each in exactly one place, and this module owns none of
//! them:
//!
//! * **May this app call the bus at all?** The manifest capability
//!   `needs.invoke_commands`, checked by [`crate::apphost`], which is the
//!   half that holds the manifest.
//! * **May this session run this command?** The agent grant, checked by
//!   [`Bus::dispatch`] on every single dispatch. This module deliberately
//!   does not pre-check it: see [`AgentBridge::invoke_for_app`].
//! * **What risk is this command?** The registered [`CommandSpec`], and
//!   nowhere else. [`tool_risk`] is the one translation.
//!
//! The grant snapshot in [`SessionGrant`] is *not* a fourth check. It only
//! narrows what a session is shown, and it comes from the same [`Grants`] the
//! bus was built with — passed in rather than built here, precisely so there
//! is no second grant source. If the two ever did disagree, the disagreement
//! could only make the *list* wider than the truth, because every call is
//! re-checked by the bus; that asymmetry is what makes one of them safe to be
//! stale and the other not.

use std::sync::Arc;

use serde_json::Value;

use deckjack_protocol::{AvailableCommand, CallRisk, InvokeOutcome};

use crate::command::{
    Admitted, AgentSessionId, Bus, CommandError, CommandId, CommandSpec, Grants, Issuer, NodeId,
    Refused, Registry, Request, RiskLevel, Target,
};
use crate::mcp::{
    self, AgentIdentity, BusCall, Dispatched, GateReason, JsonObject, McpTool, ToolList, ToolRisk,
    ToolSurface,
};

/// Prefix on the agent session id an app's dispatches are attributed to.
///
/// An app's calls are not the operator's, and the bus has exactly one issuer
/// kind that is neither the operator nor the CLI: an agent session. So an app
/// dispatches as the agent session `app:<name>`, which is also the name of
/// the grant the operator has to hand it. Reserved: whatever eventually
/// issues agent sessions must refuse this prefix, or a session could name
/// itself `app:notes` and inherit the Notes app's grant.
pub const APP_SESSION_PREFIX: &str = "app:";

/// The agent session one app's invocations are recorded as.
pub fn app_session(app: &str) -> AgentSessionId {
    AgentSessionId::new(format!("{APP_SESSION_PREFIX}{app}"))
}

/// How one app appears in the log and the Inbox.
pub fn app_identity(app: &str) -> AgentIdentity {
    AgentIdentity {
        session: app_session(app).as_str().to_string(),
        label: format!("app {app}"),
        // An app is not acting through a window even when it has one open:
        // the invocation came from its process, not from something the
        // operator clicked. Claiming a window would put a surface the
        // operator was looking at next to a command they did not issue.
        via_window: None,
    }
}

// ---------------------------------------------------------------------------
// The risk mapping
// ---------------------------------------------------------------------------

/// `command::RiskLevel` to `mcp::ToolRisk`. **The** one place.
///
/// `mcp.rs` asks for this by name: "The integrator maps `crate::command`'s
/// risk enum onto this in exactly one place, and a new risk level there is a
/// compile error here rather than a quiet default." Hence no `_` arm. A sixth
/// risk level would stop this file compiling, which is the intended cost: the
/// alternative is a wildcard that lands a new level on whichever side of the
/// Inbox gate the author of the arm happened to pick.
///
/// Note what this decides and what it does not. It decides what an agent is
/// *told* and what MCP's annotations claim. Routing is not here:
/// [`Bus::dispatch`] reads the risk from the registered spec itself, so a
/// mistake in this function cannot let a destructive command skip the Inbox.
pub fn tool_risk(risk: RiskLevel) -> ToolRisk {
    match risk {
        RiskLevel::ReadOnly => ToolRisk::ReadOnly,
        RiskLevel::Write => ToolRisk::Write,
        RiskLevel::CrossProject => ToolRisk::Gated(GateReason::CrossProject),
        RiskLevel::Outbound => ToolRisk::Gated(GateReason::Outbound),
        RiskLevel::Destructive => ToolRisk::Gated(GateReason::Destructive),
    }
}

/// `mcp::ToolRisk` to the protocol's [`CallRisk`], for an app's command list.
///
/// Exhaustive for the same reason as [`tool_risk`], and separate from it
/// because the two destinations are not one vocabulary: MCP carries a gate
/// reason as a behavior hint, an app carries it as the reason its own call
/// will wait.
pub fn call_risk(risk: ToolRisk) -> CallRisk {
    match risk {
        ToolRisk::ReadOnly => CallRisk::ReadOnly,
        ToolRisk::Write => CallRisk::Write,
        ToolRisk::Gated(GateReason::Destructive) => CallRisk::GatedDestructive,
        ToolRisk::Gated(GateReason::Outbound) => CallRisk::GatedOutbound,
        ToolRisk::Gated(GateReason::CrossProject) => CallRisk::GatedCrossProject,
    }
}

// ---------------------------------------------------------------------------
// Catalog
// ---------------------------------------------------------------------------

/// [`mcp::Catalog`] over the live [`Registry`].
///
/// Reads the registry on every call rather than caching, because apps
/// register their commands at handshake and lose them the moment their socket
/// ends: a cached catalog is a tool list offering commands whose backend is
/// gone, and that failure looks like Deckjack's rather than the app's.
pub struct RegistryCatalog {
    registry: Arc<Registry>,
}

impl RegistryCatalog {
    pub fn new(registry: Arc<Registry>) -> RegistryCatalog {
        RegistryCatalog { registry }
    }
}

impl mcp::Catalog for RegistryCatalog {
    fn commands(&self) -> Vec<mcp::CommandSpec> {
        self.registry
            .list(None)
            .into_iter()
            .map(|listing| mcp::CommandSpec {
                id: listing.spec.id().as_str().to_string(),
                title: listing.spec.title().to_string(),
                // The bus has one human string per command and it is the
                // title. There is no second, longer description to carry yet;
                // `None` says so rather than printing the title twice into
                // the text an agent reads as documentation.
                summary: None,
                parameters: schema_object(&listing.spec),
                // The registry records no result schema. `None` is honest: an
                // invented `outputSchema` is worse than none, because a
                // client that validates `structuredContent` against it would
                // throw away results the command really produced.
                result: None,
                risk: tool_risk(listing.spec.risk()),
                node: node_of(listing.spec.target()),
                enabled: listing.enablement.is_enabled(),
            })
            .collect()
    }
}

/// A command's parameter schema as MCP wants it.
///
/// A `Schema` that does not serialize to a JSON object becomes an empty one,
/// which `mcp`'s own translation reads as "no parameters". Safe rather than
/// lax: a schema only shapes what a caller sends, while the check that
/// matters is the bus deserializing the parameters into the command's own
/// type. The case is close to unreachable anyway — core commands get their
/// schema from `schemars` on a real type, and `apps::accept_registration`
/// refuses a registration whose schema is not an object.
fn schema_object(spec: &CommandSpec) -> JsonObject {
    match serde_json::to_value(spec.params_schema()) {
        Ok(Value::Object(map)) => map,
        _ => JsonObject::new(),
    }
}

/// Which node a command belongs to, as `mcp` names nodes.
///
/// `None` means "the node this core is", which is what both `ThisNode` and
/// `AnyNode` mean from here: the first is where it was registered and the
/// second includes it. Only a command pinned elsewhere names a node, and that
/// is the only case a grant's node scope has to tell apart.
fn node_of(target: &Target) -> Option<String> {
    match target {
        Target::ThisNode | Target::AnyNode => None,
        Target::Node(node) => Some(node.as_str().to_string()),
    }
}

// ---------------------------------------------------------------------------
// Grant
// ---------------------------------------------------------------------------

/// One session's grant, as [`mcp::Grant`] asks about it.
///
/// Holds the registry as well as the grant because the bus's own
/// [`crate::command::Grant::allows`] needs a spec: it checks the risk ceiling
/// as well as the namespace. Answering on the namespace alone would offer an
/// agent every tool up to `destructive` in a granted namespace while the bus
/// refused them for being above the session's ceiling — exactly the "offered
/// and then refused" that `mcp.rs` says teaches a model the boundary is
/// negotiable and worth another try.
///
/// `None` is no grant, which allows nothing. Not "unknown": a session the
/// grant store has never heard of is a session with no scope, and defaulting
/// the other way is how a missing row becomes an authorization.
pub struct SessionGrant {
    /// Every grant the session holds, each answered **whole**. A merged
    /// grant would offer tools no single grant covers, and the bus would
    /// then refuse every one of them on use — the "offered and then refused"
    /// failure `mcp.rs` warns about.
    grants: Vec<crate::command::Grant>,
    registry: Arc<Registry>,
    this_node: NodeId,
}

impl mcp::Grant for SessionGrant {
    /// Allowed when ONE grant covers the command, its risk and this node
    /// together.
    ///
    /// `mcp::Grant` asks about the command and the node through two separate
    /// methods, so answering them from two different grants would rebuild
    /// the cross-product this type exists to avoid. The node is folded in
    /// here, against the node a tool call would actually land on, and
    /// [`SessionGrant::allows_node`] only widens that for an explicitly
    /// named node.
    fn allows_command(&self, command_id: &str) -> bool {
        let Ok(id) = CommandId::new(command_id.to_string()) else {
            return false;
        };
        // A command that is not registered is not allowed, rather than
        // allowed-but-missing. On the tool path the id came from the catalog,
        // so this is only reachable when the backend went away between the
        // listing and the question.
        let Some(spec) = self.registry.spec(&id) else {
            return false;
        };
        self.grants
            .iter()
            .any(|g| g.allows(&spec) && g.allows_node(&self.this_node, &self.this_node))
    }

    fn allows_node(&self, node_id: Option<&str>) -> bool {
        let node = node_id
            .map(NodeId::new)
            .unwrap_or_else(|| self.this_node.clone());
        self.grants
            .iter()
            .any(|g| g.allows_node(&node, &self.this_node))
    }
}

// ---------------------------------------------------------------------------
// Dispatch
// ---------------------------------------------------------------------------

/// [`mcp::Dispatch`] over the real [`Bus`].
///
/// `mcp::Dispatch` returns a return-position `impl Future`, so it is **not**
/// object-safe: there is no `Box<dyn Dispatch>` to be had, and
/// [`ToolSurface`] is generic over this type rather than holding a trait
/// object. That is the trait's own decision ("the MCP server holds one
/// concrete bus, so there is nothing to gain from object safety and an
/// allocation per tool call to lose"), and this type is that one concrete
/// bus.
#[derive(Clone)]
pub struct BusInvoker {
    bus: Arc<Bus>,
}

impl BusInvoker {
    pub fn new(bus: Arc<Bus>) -> BusInvoker {
        BusInvoker { bus }
    }
}

impl mcp::Dispatch for BusInvoker {
    async fn dispatch(&self, call: BusCall) -> Result<Dispatched, mcp::DispatchError> {
        let command_id = call.command_id.clone();
        let id = match CommandId::new(call.command_id) {
            Ok(id) => id,
            Err(e) => {
                // On the tool path the id came from the catalog, so this is
                // only reachable from `invoke_for_app`, where the app named
                // the command itself. A refusal, not a failure: nothing ran.
                return Ok(Dispatched::Refused {
                    reason: e.to_string(),
                });
            }
        };

        // The issuer is where attribution stops being decorative: the bus
        // writes it into the append-only log, sends an agent's destructive
        // command to the Inbox instead of running it, and refuses the lot
        // while agents are paused. Dispatching this as `Issuer::operator()`
        // would have made all three wrong at once, and the log would have
        // said the operator did it.
        let issuer = Issuer::Agent {
            session: AgentSessionId::new(call.issuer.session.clone()),
        };
        let mut request = Request::new(id, Value::Object(call.arguments), issuer);
        if let Some(node) = &call.node {
            request = request.on_node(NodeId::new(node.clone()));
        }

        // No grant is consulted here. The bus holds the one grant source and
        // checks it inside `dispatch`, which is also where the risk routing
        // and the audit entry happen; asking a grant of our own first would
        // be a second opinion able to differ from the one that decides.
        match self.bus.dispatch(request) {
            Ok(Admitted::Running(running)) => match running.finish().await.outcome {
                Ok(result) => Ok(Dispatched::Done { result }),
                Err(CommandError::BadParams { message, .. }) => {
                    Err(mcp::DispatchError::BadArguments {
                        command_id,
                        message,
                    })
                }
                Err(other) => Err(mcp::DispatchError::Failed {
                    command_id,
                    message: other.to_string(),
                }),
            },
            Ok(Admitted::Queued { pending, .. }) => Ok(Dispatched::Queued {
                approval_id: pending.as_str().to_string(),
            }),
            // Routing asks for the confirmation dialog only for the operator
            // and the CLI, and every dispatch from here carries an agent
            // issuer, so this is unreachable. Refused rather than
            // `unreachable!()`: if it ever is reached the issuer was built
            // wrong, and the safe reading of "a person was supposed to be
            // asked" is that nothing runs.
            Ok(Admitted::NeedsConfirmation { risk, .. }) => Ok(Dispatched::Refused {
                reason: format!(
                    "{command_id} is {risk} and came back needing the operator's own \
                     confirmation, which is not something a program can answer; it was not run"
                ),
            }),
            Err(Refused::NotLocal { wanted, .. }) => Err(mcp::DispatchError::NodeUnreachable {
                node: wanted.as_str().to_string(),
            }),
            // Every remaining refusal is a "no" whose message is already
            // written for whoever has to read it: no grant, outside the
            // grant, above the ceiling, agents paused, no Inbox, nothing
            // recorded. A wildcard is safe here and only here, because its
            // default is refusal — a new `Refused` variant lands on "it did
            // not run", which is what every refusal means.
            Err(refusal) => Ok(Dispatched::Refused {
                reason: refusal.to_string(),
            }),
        }
    }
}

// ---------------------------------------------------------------------------
// The bridge
// ---------------------------------------------------------------------------

/// The MCP tool surface over this node's bus, plus the app-facing half.
///
/// One per server. A caller's grant and identity are passed per call and
/// never held, so one bridge cannot leak one caller's scope to another.
pub struct AgentBridge {
    surface: ToolSurface<RegistryCatalog, BusInvoker>,
    /// The same dispatch the surface holds, reachable directly for the app
    /// path, which names a dotted command id rather than an encoded tool
    /// name. Two callers, one implementation.
    invoker: BusInvoker,
    /// The grant store **the bus was built with**. Injected rather than
    /// constructed here: a second grant source is how `ToolSurface::list` and
    /// `Bus::dispatch` come to tell one caller two different things.
    grants: Arc<dyn Grants>,
    registry: Arc<Registry>,
    node: NodeId,
    /// Asked one question only, in [`AgentBridge::tools`]: is anything
    /// holding agents right now. Not a second dispatch path — every call
    /// still goes through [`BusInvoker`].
    bus: Arc<Bus>,
}

impl AgentBridge {
    /// `grants` must be the very same `Arc` passed to [`Bus::with_grants`].
    /// Nothing can check that from here — the bus does not expose its grant
    /// source — so it is written down as the contract it is. Passing a
    /// different one opens no hole, because every call is re-checked by the
    /// bus, but it does produce a tool list that disagrees with what calling
    /// those tools does.
    pub fn new(bus: Arc<Bus>, grants: Arc<dyn Grants>) -> Arc<AgentBridge> {
        let registry = Arc::clone(bus.registry());
        let node = bus.node().clone();
        let invoker = BusInvoker::new(Arc::clone(&bus));
        Arc::new(AgentBridge {
            surface: ToolSurface::new(RegistryCatalog::new(Arc::clone(&registry)), invoker.clone()),
            invoker,
            grants,
            registry,
            node,
            bus,
        })
    }

    /// One session's grant, as the tool surface asks about it.
    pub fn grant_for(&self, session: &AgentSessionId) -> SessionGrant {
        SessionGrant {
            grants: self.grants.grants_for(session),
            registry: Arc::clone(&self.registry),
            this_node: self.node.clone(),
        }
    }

    /// `tools/list` for one agent session.
    ///
    /// [`ToolList::skipped`] is the operator's to read, not the agent's:
    /// every entry in it is a command registered in a way that cannot become
    /// a tool, which is a bug to fix and not a permission to widen.
    ///
    /// Empty while agents are held — a lockdown, or the operator's pause.
    /// `mcp.rs`'s rule, which `mcpserve` quotes: never offer a tool that
    /// would be refused, because offering and then refusing teaches a model
    /// the boundary is negotiable and worth rewording. Every dispatch from
    /// here is about to be refused with `Refused::AgentsPaused`, so the
    /// honest list is the empty one.
    ///
    /// Asked of the same [`Bus::agents_paused`] the refusal is decided by,
    /// not of a lockdown of our own, so the list and the dispatch cannot
    /// disagree — the same reason `grants` is the bus's own `Arc`.
    /// `skipped` is empty too: nothing was listed, so there is nothing that
    /// failed to list. It also covers [`AgentBridge::commands_for_app`],
    /// which generates an app's `AppFrame::Commands` from this.
    pub fn tools(&self, session: &AgentSessionId) -> ToolList {
        if self.bus.agents_paused() {
            return ToolList {
                tools: Vec::new(),
                skipped: Vec::new(),
            };
        }
        self.surface.list(&self.grant_for(session))
    }

    /// `tools/call` for one agent session, folded into the single value MCP
    /// returns.
    pub async fn call_tool(
        &self,
        agent: &AgentIdentity,
        name: &str,
        arguments: Option<JsonObject>,
    ) -> rmcp::model::CallToolResult {
        let grant = self.grant_for(&AgentSessionId::new(agent.session.clone()));
        self.surface.call_tool(agent, &grant, name, arguments).await
    }

    /// What one app may call, for `AppFrame::Commands`.
    ///
    /// Generated from the live registry through the same [`ToolSurface`] an
    /// agent's `tools/list` goes through, so there is one answer to "what may
    /// this non-operator caller use" rather than two that drift.
    pub fn commands_for_app(&self, app: &str) -> Vec<AvailableCommand> {
        self.tools(&app_session(app))
            .tools
            .iter()
            .map(available)
            .collect()
    }

    /// Run a bus command on one app's behalf, for `AppFrame::Invoke`.
    ///
    /// The app's grant is **not** checked here. It is checked by the bus,
    /// inside the dispatch, along with the risk routing and the audit entry —
    /// one decision in one place. Pre-checking would mean a second reading of
    /// the grant that could say yes while the bus said no, or worse, the
    /// reverse.
    ///
    /// What is checked before this is called is the manifest capability, by
    /// [`crate::apphost`], which is the half that has the manifest.
    pub async fn invoke_for_app(&self, app: &str, command: &str, params: Value) -> InvokeOutcome {
        use mcp::Dispatch as _;

        let call = BusCall {
            command_id: command.to_string(),
            // Parameters that are not an object become an empty map, and the
            // command's own deserialization then refuses them naming the
            // field it wanted. Wrapping them in an invented object here would
            // make that message describe a shape the app never sent.
            arguments: match params {
                Value::Object(map) => map,
                _ => JsonObject::new(),
            },
            // An app's invocation is for the node the app runs on, which is
            // this one. An app has no way to name another node and should not
            // have: it is confined to a folder on this machine.
            node: None,
            // Nothing on this path reads it — the bus takes the risk from the
            // registered spec, which is the only place risk may come from.
            // Filled from the registry anyway so a `BusCall` is never a lie
            // about the command it names, and gated-destructive for a command
            // nobody registered, because an unknown command must not be
            // described as read-only.
            risk: self.risk_of(command),
            issuer: app_identity(app),
        };

        match self.invoker.dispatch(call).await {
            Ok(Dispatched::Done { result }) => InvokeOutcome::Ok { value: result },
            Ok(Dispatched::Queued { approval_id }) => InvokeOutcome::Queued {
                approval: approval_id,
            },
            Ok(Dispatched::Refused { reason }) => InvokeOutcome::Refused { message: reason },
            // Everything left is "it was allowed and did not work", which an
            // app may sensibly report or retry. Kept apart from `Refused`,
            // which it never should.
            Err(e) => InvokeOutcome::Failed {
                message: e.to_string(),
            },
        }
    }

    fn risk_of(&self, command: &str) -> ToolRisk {
        CommandId::new(command.to_string())
            .ok()
            .and_then(|id| self.registry.spec(&id))
            .map(|spec| tool_risk(spec.risk()))
            .unwrap_or(ToolRisk::Gated(GateReason::Destructive))
    }
}

/// One tool, as an app's command list carries it.
fn available(tool: &McpTool) -> AvailableCommand {
    AvailableCommand {
        command: tool.command_id().to_string(),
        title: tool.title().to_string(),
        // The same text an agent gets, including the sentence explaining a
        // gate. An app about to wait for a person should be told why in the
        // same words.
        description: tool.description().to_string(),
        params: Value::Object(tool.input_schema().clone()),
        risk: call_risk(tool.risk()),
    }
}
