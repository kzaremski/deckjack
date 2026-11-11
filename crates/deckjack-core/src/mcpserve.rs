//! Serving the generated tools to a real agent.
//!
//! [`crate::mcp`] turns registered commands into MCP tool definitions, and
//! [`crate::agentbridge`] backs those definitions with the real bus. Both
//! were complete and tested and had no user at all: `agentbridge`'s own
//! module doc said so in as many words — "there is no such transport in this
//! build and nothing issues agent sessions". A node therefore generated a
//! correct, grant-filtered tool surface that no agent could reach, which is
//! this project's recurring defect shape (built, unit-tested, never
//! constructed) wearing a protocol hat.
//!
//! This module is that transport. It is deliberately the thinnest possible
//! layer: it owns no authority, makes no permission decision, and holds no
//! grant. Everything it serves comes from [`AgentBridge`], which is the one
//! path app invocations already take.
//!
//! # The pipe is the credential
//!
//! An agent does not say who it is. A [`Handler`] is constructed around one
//! [`AgentIdentity`] and serves exactly one byte stream, so the session an
//! incoming `tools/call` is attributed to is fixed by whoever built the
//! handler — never by anything the agent sends. There is no field in the
//! protocol it could set to become another session, because the session is
//! not read from the protocol at all.
//!
//! That is the whole authentication story, and it is on purpose. The
//! alternative shapes are worse:
//!
//! * **A bearer token.** A token lives in an agent's config file, which gets
//!   copied between machines, committed by accident and pasted into chats. A
//!   pipe cannot be copied anywhere.
//! * **The operator's session cookie over HTTP.** This is the tempting one
//!   and it is wrong on the axis this codebase keeps separate: the cookie is
//!   the *operator's* credential. An agent presenting it would act with
//!   operator authority, and grants — the thing that keeps agents inside a
//!   scope — are keyed by agent session, not by operator. Risk level decides
//!   whether the operator confirms; grants decide what an agent may ask for.
//!   Conflating them hands an agent the one identity that is never refused.
//!
//! # What a session with no grant gets
//!
//! An empty tool list, and a refusal for anything it calls anyway. Not an
//! error, not a 403, and nothing that reads as "ask differently". This falls
//! out of [`AgentBridge::tools`] rather than being implemented here, and it
//! means mounting a transport for a session the operator never granted
//! anything is inert rather than dangerous.
//!
//! # Lockdown
//!
//! The lockdown gate in [`crate::wiring::app`] is an axum layer, so it covers
//! HTTP routes and nothing else. This transport is not an HTTP route, so
//! during a lockdown it would otherwise keep serving — which is exactly the
//! sort of gap that makes a security control a decoration. [`Handler`]
//! therefore consults [`Lockdown::is_locked`] itself, on every list and every
//! call.
//!
//! Two honest limits on that:
//!
//! 1. It is checked **per request**, so a dispatch already in flight when the
//!    lockdown engages still runs to completion. Nothing stops those: the bus
//!    holds agents at ADMISSION (`BusParts.halt`, which is the same lockdown
//!    asked on every dispatch, so this check is belt and braces rather than
//!    the only one), and a command already past admission runs to completion.
//!    Killing child processes is lockdown's step 2; an in-process command is
//!    a known gap that needs a cancellation token, not a flag.
//! 2. A locked node reports an **empty tool list** rather than an error.
//!    That follows [`crate::mcp`]'s own rule: never offer a tool that would
//!    be refused, because offering and then refusing teaches a model the
//!    boundary is negotiable and worth rewording. During a lockdown every
//!    tool would be refused, so the honest list is the empty one.

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};

use rmcp::model::{
    CallToolRequestParams, CallToolResult, Content, Implementation, ListToolsResult,
    PaginatedRequestParams, ProtocolVersion, ServerCapabilities, ServerInfo,
};
use rmcp::service::{RequestContext, RoleServer};
use rmcp::{ErrorData as McpError, ServerHandler, ServiceExt};
use tokio::io::{AsyncRead, AsyncWrite};

use crate::agentbridge::AgentBridge;
use crate::command::AgentSessionId;
use crate::lockdown::Lockdown;
use crate::mcp::{AgentIdentity, Skipped};

/// What the Assistant is called in the log and the Inbox when the operator
/// has not named it something else.
pub const DEFAULT_LABEL: &str = "Assistant";

/// The instructions sent to the agent at `initialize`.
///
/// Worth writing down rather than leaving empty: a model that understands a
/// refusal is final stops burning turns rephrasing one. This text is the only
/// thing in this module addressed to the agent rather than the operator.
const INSTRUCTIONS: &str = "\
Every tool here is a Deckjack command on this node, and Deckjack commands are the \
only tools you have: there is no shell, no filesystem tool and no way to reach \
anything that is not listed. The list you were given is already filtered to what \
this session is allowed to ask for, so a tool you cannot see is not one to look \
for another name for.\n\n\
A refused call is final. It means the command is outside this session's grant, and \
rewording the request, trying a neighbouring command or calling again will produce \
the same refusal. Only the operator can widen a grant.\n\n\
A call may instead come back as waiting for approval. That is not a failure and not \
something to retry: the operator has it in their Inbox, and the work happens if they \
approve it.";

/// One agent session's view of this node's commands.
///
/// Serves exactly one byte stream. The identity is fixed at construction, so
/// nothing an agent sends can change who its calls are attributed to.
pub struct Handler {
    bridge: Arc<AgentBridge>,
    lockdown: Arc<Lockdown>,
    identity: AgentIdentity,
    /// Registration bugs already reported, so a long-lived agent polling
    /// `tools/list` does not reprint the same operator-facing error every
    /// time. Keyed by command id because that is what the operator has to go
    /// and fix.
    reported: Mutex<BTreeSet<String>>,
}

impl Handler {
    /// `identity.session` must be the agent session id the operator issued a
    /// grant to. Nothing here can check that — the grant store is keyed by
    /// this string and a typo simply matches no grant — so the failure mode
    /// is an agent that can see and do nothing, which is the right direction
    /// to fail in.
    pub fn new(
        bridge: Arc<AgentBridge>,
        lockdown: Arc<Lockdown>,
        identity: AgentIdentity,
    ) -> Handler {
        Handler {
            bridge,
            lockdown,
            identity,
            reported: Mutex::new(BTreeSet::new()),
        }
    }

    /// Who this handler's calls are recorded as.
    pub fn identity(&self) -> &AgentIdentity {
        &self.identity
    }

    fn session(&self) -> AgentSessionId {
        AgentSessionId::new(self.identity.session.clone())
    }

    /// Tell the operator about commands that cannot become tools.
    ///
    /// [`crate::mcp`]'s module doc asks for exactly this and nothing did it,
    /// so [`crate::mcp::ToolList::skipped`] was computed on every list and
    /// read by nobody. Every entry is a registration bug — a command id with
    /// a character that cannot be encoded, a duplicate registration — not a
    /// permission decision, so it is the operator's to fix and the agent
    /// never sees it.
    fn report_skipped(&self, skipped: &[Skipped]) {
        if skipped.is_empty() {
            return;
        }
        let mut reported = match self.reported.lock() {
            Ok(guard) => guard,
            // A poisoned mutex here means a previous log attempt panicked.
            // Reporting a registration bug is not worth taking the agent's
            // connection down over, so recover and carry on.
            Err(poisoned) => poisoned.into_inner(),
        };
        for entry in skipped {
            if reported.insert(entry.command_id.clone()) {
                tracing::error!(
                    target: "deckjack::agent",
                    command = %entry.command_id,
                    "registered command cannot be an agent tool: {}",
                    entry.why
                );
            }
        }
    }
}

impl ServerHandler for Handler {
    fn get_info(&self) -> ServerInfo {
        // `ServerInfo` is `#[non_exhaustive]`, so it is built through its
        // own chain rather than a struct literal. Named "deckjack" rather
        // than `Implementation::from_build_env()`, which would report the
        // crate name (`deckjack-core`) — an agent's client lists this string,
        // and the crate layout is not the agent's business.
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_protocol_version(ProtocolVersion::LATEST)
            .with_server_info(Implementation::new("deckjack", env!("CARGO_PKG_VERSION")))
            .with_instructions(INSTRUCTIONS)
    }

    /// Pagination is accepted and ignored: the tool count is bounded by what
    /// is registered on one node — dozens, not thousands — so there is one
    /// page. Returning everything with no cursor is a truthful answer to any
    /// cursor a client sends, rather than a second code path that could
    /// disagree with the filtered list.
    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        if self.lockdown.is_locked() {
            tracing::info!(
                target: "deckjack::agent",
                issuer = %self.identity.attribution(),
                "lockdown: serving an empty tool list"
            );
            return Ok(ListToolsResult::with_all_items(Vec::new()));
        }

        let listing = self.bridge.tools(&self.session());
        self.report_skipped(&listing.skipped);
        Ok(ListToolsResult::with_all_items(
            listing.tools.iter().map(|tool| tool.to_rmcp()).collect(),
        ))
    }

    /// A refusal comes back as a tool *result*, never a protocol error.
    ///
    /// Same reasoning as [`crate::mcp::ToolSurface::call_tool`], which this
    /// delegates to: a protocol error is the kind of failure a model retries
    /// blindly, whereas a result is something it can read and stop on.
    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        if self.lockdown.is_locked() {
            tracing::warn!(
                target: "deckjack::agent",
                tool = %request.name,
                issuer = %self.identity.attribution(),
                "lockdown: refused an agent tool call"
            );
            return Ok(CallToolResult::error(vec![Content::text(
                "This node is in lockdown. Every command is refused until the operator \
                 unlocks it at the console, and nothing you can send from here changes \
                 that. Stop and report it.",
            )]));
        }

        Ok(self
            .bridge
            .call_tool(&self.identity, request.name.as_ref(), request.arguments)
            .await)
    }
}

/// Why serving one agent ended.
#[derive(Debug, thiserror::Error)]
pub enum ServeError {
    // Boxed because `ServerInitializeError` carries a whole rejected message
    // and is several hundred bytes, which would otherwise make every `Ok` on
    // this path pay for the unhappy one.
    #[error("the agent never completed the MCP handshake: {0}")]
    Handshake(Box<rmcp::service::ServerInitializeError>),
    #[error("serving the agent panicked or was cancelled: {0}")]
    Join(#[from] tokio::task::JoinError),
}

/// Serve one agent over one byte stream until it disconnects.
///
/// `read` carries what the agent sends and `write` what it is sent. Any pair
/// of async streams works, which is the point: the pair this is called with
/// is the credential, so the caller decides what counts as proof of identity
/// by deciding what it hands over. A spawned child's pipes, a unix socket the
/// operator alone can open, or an in-memory duplex in a test are all the same
/// to this function.
///
/// Returns when the agent goes away. Tests use [`tokio::io::duplex`].
pub async fn serve<R, W>(handler: Handler, read: R, write: W) -> Result<(), ServeError>
where
    R: AsyncRead + Send + Unpin + 'static,
    W: AsyncWrite + Send + Unpin + 'static,
{
    let who = handler.identity().attribution();
    let running = handler
        .serve((read, write))
        .await
        .map_err(|e| ServeError::Handshake(Box::new(e)))?;
    tracing::info!(target: "deckjack::agent", issuer = %who, "an agent attached");
    let reason = running.waiting().await?;
    tracing::info!(
        target: "deckjack::agent",
        issuer = %who,
        "an agent detached: {reason:?}"
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// Where an agent attaches
// ---------------------------------------------------------------------------

/// Agent sockets live under the *install* directory's run directory, beside
/// the app sockets in `run/` rather than in it: an app socket is named for an
/// installed app and an agent socket for a granted session, and one directory
/// holding both would make a name collision between the two into a silent
/// authority swap.
///
/// The install directory, not the working directory, for two reasons that
/// point the same way. The design's own rule is that `~/.deckjack/` is built
/// for this machine and never travels while the working directory is "the
/// only thing synced or carried" — and a live socket is machine-local
/// runtime state, so syncing one between machines is at best noise. And a
/// unix socket path has to fit in `sun_path`, about 108 bytes: a working
/// directory nested a few levels deep blows that, which is not a theoretical
/// limit but the thing that actually happened the first time this was run.
pub const AGENTS_SUBDIR: &str = "agents";

/// A session id that cannot be a filename.
#[derive(Debug, thiserror::Error)]
pub enum BadSession {
    #[error("an agent session id cannot be empty")]
    Empty,
    #[error(
        "an agent session id may contain only letters, digits, '.', '-' and '_', \
         so {character:?} cannot be used"
    )]
    Character { character: char },
    #[error("an agent session id cannot be \".\" or \"..\"")]
    Dots,
}

/// Check that a session id can safely become a socket filename.
///
/// The id comes from the operator's config, not from the network, so this is
/// not defending against an attacker. It is defending against a typo
/// producing a socket somewhere other than where the operator will look for
/// it: `../../tmp/x` would bind a perfectly working agent door outside the
/// hidden directory, with none of the `0700` protection that directory has,
/// and nothing would say so. Refusing at startup is how that becomes a
/// legible error instead of a quiet hole.
pub fn check_session_name(session: &str) -> Result<(), BadSession> {
    if session.is_empty() {
        return Err(BadSession::Empty);
    }
    if session == "." || session == ".." {
        return Err(BadSession::Dots);
    }
    for ch in session.chars() {
        if !matches!(ch, 'a'..='z' | 'A'..='Z' | '0'..='9' | '.' | '-' | '_') {
            return Err(BadSession::Character { character: ch });
        }
    }
    Ok(())
}

/// Hex characters of working-directory digest in a socket name. Eight is
/// plenty to keep the handful of working directories on one machine apart,
/// and every character spent here is one `sun_path` does not have.
const TAG_HEX: usize = 8;

/// A short, stable tag for one working directory.
///
/// One machine can serve several working directories, one node each, and
/// every one of them may have a session called `assistant`. The sockets all
/// live in the one install directory, so the name has to say which node it
/// belongs to — and it cannot say it by spelling the path out, because that
/// is exactly what does not fit in `sun_path`.
///
/// Canonicalized first, so `deckjack agent --dir .` and a node started with
/// an absolute path agree. A directory that cannot be canonicalized (it does
/// not exist yet) falls back to the path as given: the two sides then still
/// agree whenever they were given the same path, which is the case that has
/// to work.
pub fn workspace_tag(working_dir: &std::path::Path) -> String {
    use sha2::{Digest, Sha256};
    use std::fmt::Write as _;

    let canonical =
        std::fs::canonicalize(working_dir).unwrap_or_else(|_| working_dir.to_path_buf());
    // The encoded bytes rather than a lossy string: two paths that differ
    // only outside UTF-8 are different directories and must not collide.
    let digest = Sha256::digest(canonical.as_os_str().as_encoded_bytes());
    let mut tag = String::with_capacity(TAG_HEX);
    for byte in &digest[..TAG_HEX / 2] {
        let _ = write!(tag, "{byte:02x}");
    }
    tag
}

/// Where one agent session's socket lives.
///
/// Call [`check_session_name`] first: this does no validation, because a
/// function that silently repaired a bad name would hand back a path the
/// operator did not ask for.
pub fn socket_path(
    install_dir: &std::path::Path,
    working_dir: &std::path::Path,
    session: &str,
) -> std::path::PathBuf {
    install_dir
        .join(crate::apps::RUN_SUBDIR)
        .join(AGENTS_SUBDIR)
        .join(format!("{}-{session}.sock", workspace_tag(working_dir)))
}

/// Serve one agent session's socket until the task is dropped.
///
/// # The socket is the credential
///
/// [`crate::apps::bind_socket`] sets the containing directory to `0700`, so
/// opening this socket already means being the user that runs the node. That
/// is the whole check, and it is why no token is exchanged after connecting:
/// the session a connection speaks for is fixed by *which socket it is*, and
/// the identity below is the one this listener was mounted with.
///
/// Several clients may connect at once and all are served. That is not an
/// escalation: they are all the same session with the same grant, recorded
/// under the same attribution, so N connections are N views of one authority
/// rather than a way to assemble more of it.
///
/// A connection that fails is logged and dropped; the listener keeps
/// accepting. An agent crashing must not take the node's agent door with it.
pub async fn mount(
    listener: tokio::net::UnixListener,
    bridge: Arc<AgentBridge>,
    lockdown: Arc<Lockdown>,
    identity: AgentIdentity,
) {
    loop {
        let (stream, _) = match listener.accept().await {
            Ok(accepted) => accepted,
            Err(e) => {
                // Not fatal and not retried in a tight loop: `accept` failing
                // repeatedly means the socket is gone, and the next error
                // arrives when a client next tries, not immediately.
                tracing::error!(
                    target: "deckjack::agent",
                    issuer = %identity.attribution(),
                    "accepting an agent connection failed: {e}"
                );
                continue;
            }
        };

        let handler = Handler::new(Arc::clone(&bridge), Arc::clone(&lockdown), identity.clone());
        let who = identity.attribution();
        tokio::spawn(async move {
            let (read, write) = tokio::io::split(stream);
            if let Err(e) = serve(handler, read, write).await {
                tracing::warn!(
                    target: "deckjack::agent",
                    issuer = %who,
                    "serving an agent ended: {e}"
                );
            }
        });
    }
}
