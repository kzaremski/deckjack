//! The WebSocket the command bus rides on.
//!
//! One socket per browser tab carries everything: invocations up, results,
//! incremental progress and server-pushed events down. There is no second
//! channel and no polling endpoint, because a result and the progress that
//! preceded it have to arrive in order, and two transports cannot promise
//! that.
//!
//! **Versioned at the handshake, not per frame.** The first frame a client
//! sends must be [`ClientFrame::Hello`] carrying the protocol it was built
//! against. A mismatch is refused with a close code and a sentence naming
//! both numbers. The alternative — accepting frames and discovering the
//! disagreement field by field — is how an old client ends up silently
//! running a command it did not mean. Once the handshake agrees, unknown
//! fields inside frames are ignored rather than fatal: the version is the
//! compatibility gate, so a newer client talking to this server should fail
//! at the gate or not at all.
//!
//! **Authenticated before the upgrade.** The session cookie is checked on
//! the HTTP request, exactly as [`crate::http`] checks it, and an
//! unauthenticated peer never gets an upgraded socket. That is deliberately
//! *not* "upgrade then close with 4401": a browser cannot read the close
//! code of a socket it was never allowed to open either way, and
//! [`crate::auth::state::verify_response`] already decided that upgrade
//! requests get a plain 401 because a redirect is meaningless to them. Close
//! codes carry the reasons that can only be discovered *after* the upgrade:
//! wrong protocol, malformed frame, a session that expired while the socket
//! was open.
//!
//! **The command bus is behind one trait.** This module knows how to carry a
//! call and route its progress back to the right correlation id, and nothing
//! else. [`Dispatch`] is the whole contract; `crate::command::Registry`
//! implements it and neither side sees the other's internals.
//!
//! **Who asked crosses the boundary.** Every [`Call`] carries an [`Issuer`]
//! built from the peer the upgrade authenticated: the connection, the IP and
//! the session. The session is the part the bus cannot do without — a command
//! that opens a pty has to tag it with the session so the pty dies with the
//! browser that asked for it, and lockdown needs the same tag to kill what one
//! revoked session started. It is also a bearer credential, so it leaves this
//! module through exactly one accessor ([`Issuer::session`]) and never through
//! a frame.
//!
//! ```no_run
//! # use std::sync::Arc;
//! # use deckjack_core::ws;
//! # fn wire(
//! #     auth: Arc<deckjack_core::http::Ctx>,
//! #     bus: Arc<dyn ws::Dispatch>,
//! #     node_id: String,
//! # ) -> axum::Router {
//! let clock = Arc::new(ws::SystemClock);
//! let hub = ws::Hub::new(clock.clone());
//! let ctx = ws::WsCtx::new(
//!     auth,
//!     bus,
//!     Arc::clone(&hub),
//!     clock,
//!     node_id,
//!     deckjack_core::node::host_name(),
//!     ws::Settings::default(),
//! );
//! // Anything on the server can now push an event to every open desktop:
//! hub.publish("windows.changed", serde_json::json!({ "count": 3 }));
//! ws::router(ctx)
//! # }
//! ```

use std::collections::HashMap;
use std::fmt;
use std::future::Future;
use std::net::{IpAddr, SocketAddr};
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use axum::Router;
use axum::body::Bytes;
use axum::extract::ws::{CloseCode, CloseFrame, Message, Utf8Bytes, WebSocket, WebSocketUpgrade};
use axum::extract::{ConnectInfo, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;
use tokio::sync::{mpsc, watch};

use crate::auth::SessionId;
use crate::auth::state::{Now, VerifyOutcome, VerifyResponse, verify_response};
use crate::http::Ctx;

/// Where the bus listens. One path, one protocol; apps are multiplexed by
/// command namespace rather than by a socket each, so a desktop with twelve
/// windows still holds one connection.
pub const PATH: &str = "/ws";

/// The wire protocol this build speaks.
///
/// Bumped whenever a frame's meaning changes in a way an older peer would
/// misread. Adding an optional field, or a new frame variant that older
/// peers simply never send, does not need a bump — those are covered by
/// ignoring unknown fields and unknown variants being a `BadFrame` only when
/// actually sent.
pub const PROTOCOL_VERSION: u32 = 1;

/// Error code for an invoke that still carries the removed `confirmed`
/// field. Not a protocol bump: the frame still parses, and the answer says
/// exactly what changed, which a handshake refusal could not.
pub const CONFIRMED_REMOVED: &str = "confirmed_removed";

/// Error code for a `confirm` frame whose ticket did not run anything:
/// unknown, used, expired, or for another socket, issuer, command or
/// parameters.
pub const CONFIRMATION_REFUSED: &str = "confirmation_refused";

/// Close codes.
///
/// 4000-4999 is the range reserved for application use. These mirror the
/// HTTP status with the same meaning (`4401` for unauthenticated, `4426` for
/// "upgrade required") so a close code in a log line reads without a lookup
/// table.
pub mod close {
    use axum::extract::ws::CloseCode;

    /// A frame was not JSON, not a known frame, or broke a field's rule.
    pub const BAD_FRAME: CloseCode = 4400;
    /// The session expired or was revoked while the socket was open.
    pub const UNAUTHENTICATED: CloseCode = 4401;
    /// Nothing arrived in time: no `hello`, or no traffic at all.
    pub const TIMEOUT: CloseCode = 4408;
    /// The client speaks a protocol version this server does not.
    pub const PROTOCOL: CloseCode = 4426;
    /// The client is not draining what it asked for.
    pub const BACKPRESSURE: CloseCode = 4429;
    /// A fault on this side, not the client's.
    pub const INTERNAL: CloseCode = 4500;
}

// ---------------------------------------------------------------------------
// Identifiers
// ---------------------------------------------------------------------------

/// Why an identifier off the wire was refused.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum IdError {
    #[error("{kind} is empty; send the id the request was made with")]
    Empty { kind: &'static str },

    #[error(
        "{kind} is {len} bytes, over the {max} byte limit; use a short opaque id, not a payload"
    )]
    TooLong {
        kind: &'static str,
        len: usize,
        max: usize,
    },

    #[error("{kind} contains a character that is not allowed ({found:?}); use {allowed}")]
    BadChar {
        kind: &'static str,
        found: char,
        allowed: &'static str,
    },
}

/// A client-chosen tag that ties a response and every progress frame back to
/// the request that caused them.
///
/// Opaque to the server and never interpreted. It is validated anyway: it is
/// a map key held for as long as the call runs, and it reaches log lines, so
/// an unbounded or control-character-bearing value would be a memory and a
/// log-injection problem rather than a client's own business.
#[derive(Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String")]
pub struct CorrelationId(String);

impl CorrelationId {
    pub const MAX_LEN: usize = 64;

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for CorrelationId {
    type Error = IdError;

    fn try_from(s: String) -> Result<CorrelationId, IdError> {
        const KIND: &str = "correlation id";
        if s.is_empty() {
            return Err(IdError::Empty { kind: KIND });
        }
        if s.len() > CorrelationId::MAX_LEN {
            return Err(IdError::TooLong {
                kind: KIND,
                len: s.len(),
                max: CorrelationId::MAX_LEN,
            });
        }
        if let Some(found) = s.chars().find(|c| !c.is_ascii_graphic() && *c != ' ') {
            return Err(IdError::BadChar {
                kind: KIND,
                found,
                allowed: "printable ASCII",
            });
        }
        Ok(CorrelationId(s))
    }
}

impl fmt::Display for CorrelationId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl fmt::Debug for CorrelationId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}", self.0)
    }
}

/// Which command is being invoked, as a dotted namespace (`files.move`).
///
// TODO: when `crate::command` lands it will have the authoritative id type
// with the namespace rules the grant system matches against (`files.read.*`).
// This stays the *wire* shape — validated, cheap, no knowledge of what is
// registered — and `crate::command` converts from it, so neither module has
// to depend on the other's internals.
#[derive(Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String")]
pub struct CommandId(String);

impl CommandId {
    pub const MAX_LEN: usize = 128;

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for CommandId {
    type Error = IdError;

    fn try_from(s: String) -> Result<CommandId, IdError> {
        const KIND: &str = "command id";
        if s.is_empty() {
            return Err(IdError::Empty { kind: KIND });
        }
        if s.len() > CommandId::MAX_LEN {
            return Err(IdError::TooLong {
                kind: KIND,
                len: s.len(),
                max: CommandId::MAX_LEN,
            });
        }
        if let Some(found) = s.chars().find(|c| {
            !(c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '_' | '-'))
        }) {
            return Err(IdError::BadChar {
                kind: KIND,
                found,
                allowed: "lowercase letters, digits, '.', '_' and '-'",
            });
        }
        Ok(CommandId(s))
    }
}

impl fmt::Display for CommandId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl fmt::Debug for CommandId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}", self.0)
    }
}

/// A connection's handle for the lifetime of one socket. Server-assigned, so
/// unlike a correlation id it can be trusted in attribution.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Serialize, Deserialize)]
pub struct ConnectionId(pub u64);

impl fmt::Display for ConnectionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "conn-{}", self.0)
    }
}

// ---------------------------------------------------------------------------
// Wire frames
// ---------------------------------------------------------------------------

/// `Some` for a field that is in the frame, whatever its value, `null`
/// included. Used with `#[serde(default)]`, so an absent field stays `None`.
fn present<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<Value>, D::Error> {
    Value::deserialize(d).map(Some)
}

/// Browser to server.
///
/// Internally tagged so the TypeScript side is a discriminated union on
/// `type`, which is what the generated client wants.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientFrame {
    /// Must be the first frame. Anything else closes the socket.
    Hello {
        protocol: u32,
        /// Free-form build string, logged so a misbehaving client can be
        /// identified. Never trusted for a decision.
        #[serde(default)]
        client: Option<String>,
    },
    /// Run a command. `id` is echoed on every frame that answers it.
    Invoke {
        id: CorrelationId,
        command: CommandId,
        #[serde(default)]
        params: Value,
        /// **Removed.** This was the client saying "the operator answered the
        /// dialog", and the bus believed it, so any script on the socket
        /// could skip the dialog. A confirmation is now a server-issued
        /// ticket redeemed with [`ClientFrame::Confirm`].
        ///
        /// Still parsed, only so that a frame carrying it is answered with an
        /// error naming the change rather than run as if unconfirmed or
        /// silently ignored: a stale client that sends it learns why.
        /// Present at all is enough, `null` included: serde would otherwise
        /// read `"confirmed": null` as absent.
        #[serde(
            default,
            deserialize_with = "present",
            skip_serializing_if = "Option::is_none"
        )]
        confirmed: Option<Value>,
        /// The call is made on behalf of this installed app's window, not
        /// by the shell itself (docs/APP-UI-ISOLATION.md §4.6).
        ///
        /// It can only narrow what the call may do: the bus then treats it
        /// as that app's window and holds it to the app's approved scope,
        /// never to the operator's. Accepted here, on a socket whose session
        /// cookie the upgrade verified, and nowhere else.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        as_app: Option<String>,
    },
    /// Run a command the operator has just confirmed.
    ///
    /// `ticket` is what the `needs_confirmation` answer to an earlier invoke
    /// carried. `command` and `params` are that same call again: the server
    /// keeps only a digest of what it asked about, and runs this only if the
    /// two agree. Valid once, for a minute, on the socket that was asked.
    /// `id` is a fresh correlation id, answered like an invoke's.
    Confirm {
        id: CorrelationId,
        ticket: String,
        command: CommandId,
        #[serde(default)]
        params: Value,
        /// The `as_app` the invoke that was asked about carried, again. The
        /// ticket is bound to the issuer it named, so a ticket issued to an
        /// app's window is refused on a confirm that names another app or
        /// none, and one issued to the shell is refused on a confirm that
        /// names any app.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        as_app: Option<String>,
    },
    /// Ask a running command to stop. Cooperative: the command sees the
    /// signal and unwinds, so a `response` still arrives for `id`.
    Cancel { id: CorrelationId },

    /// Liveness probe, answered immediately with [`ServerFrame::Pong`].
    ///
    /// This exists because the WebSocket-level ping this server already sends
    /// is useless to the party that needs it most. A browser answers those
    /// pings itself, in the network stack, and never tells the page — so the
    /// server can tell a dead client, and the client cannot tell a dead
    /// server. A socket whose peer vanished without a FIN stays `open` to
    /// JavaScript until TCP gives up, which is minutes, and the shell spends
    /// those minutes claiming it is connected.
    ///
    /// `token` is echoed back so the client can measure a round trip and
    /// ignore a stale answer, rather than merely knowing that *something*
    /// arrived.
    Ping {
        #[serde(default)]
        token: Option<String>,
    },
}

/// Server to browser.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerFrame {
    /// The handshake was accepted. Carries the node's identity so the shell
    /// can show which machine is hosting without a second request.
    Welcome {
        protocol: u32,
        connection: ConnectionId,
        node_id: String,
        host_name: String,
    },
    /// An incremental update for a call still in flight.
    ///
    /// `seq` counts from 0 per correlation id and is the point of the frame:
    /// progress is droppable (see [`ProgressSink::report`]), so a gap in
    /// `seq` is how a client learns it missed some rather than silently
    /// rendering a stale bar.
    Progress {
        id: CorrelationId,
        seq: u64,
        progress: Progress,
    },
    /// The call finished. Exactly one per accepted `invoke`.
    Response {
        id: CorrelationId,
        outcome: Outcome,
        elapsed_ms: u64,
    },
    /// Something happened that no one asked about: a window moved, an export
    /// finished, an approval is waiting.
    Event {
        topic: String,
        payload: Value,
        at_ms: u64,
    },
    /// The answer to [`ClientFrame::Ping`], echoing its `token`.
    ///
    /// Deliberately carries no state. Its only job is to prove that a whole
    /// round trip still completes — socket, task, and this process — which is
    /// strictly more than TCP staying up tells anyone.
    Pong {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        token: Option<String>,
    },
}

/// What a command reports while it runs. Every field optional on purpose: a
/// command that only knows it is alive sends a message, one that counts
/// sends a fraction, one that shells out forwards lines.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Progress {
    /// 0.0 to 1.0 where the command can say; `None` where it cannot. A
    /// made-up fraction is worse than a spinner.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fraction: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    /// Output lines, in order. Data, never instructions: the shell renders
    /// these as text and nothing else.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub log: Vec<String>,
}

impl Progress {
    pub fn message(text: impl Into<String>) -> Progress {
        Progress {
            message: Some(text.into()),
            ..Progress::default()
        }
    }

    pub fn fraction(done: u64, total: u64) -> Progress {
        Progress {
            // A total of zero means "nothing to do", which is complete, not
            // a division by zero.
            fraction: Some(if total == 0 {
                1.0
            } else {
                // Clamped because the shell renders this as a bar width and
                // the field is documented as 0.0 to 1.0. A count that
                // overshoots its total — a resumed transfer, a directory
                // that grew during the walk — is a plausible mistake for a
                // command to make, and "more than everything" is not
                // something the client has any way to draw.
                ((done as f64 / total as f64) as f32).clamp(0.0, 1.0)
            }),
            ..Progress::default()
        }
    }

    pub fn line(text: impl Into<String>) -> Progress {
        Progress {
            log: vec![text.into()],
            ..Progress::default()
        }
    }
}

/// How a call ended.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum Outcome {
    Ok { value: Value },
    Error { error: CommandError },
}

impl Outcome {
    pub fn ok(value: Value) -> Outcome {
        Outcome::Ok { value }
    }

    pub fn error(error: CommandError) -> Outcome {
        Outcome::Error { error }
    }
}

/// A failure the client is expected to render.
///
/// `code` is the stable machine-readable part; `message` is written for the
/// operator reading it in a toast, so it says what to do about it.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CommandError {
    pub code: String,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<Value>,
}

impl CommandError {
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> CommandError {
        CommandError {
            code: code.into(),
            message: message.into(),
            detail: None,
        }
    }

    pub fn with_detail(mut self, detail: Value) -> CommandError {
        self.detail = Some(detail);
        self
    }

    /// The connection is already running as many commands as it may.
    fn too_many(max: usize) -> CommandError {
        CommandError::new(
            "ws.too_many_in_flight",
            format!(
                "this connection already has {max} commands running; \
                 wait for one to finish or cancel it"
            ),
        )
    }

    /// An invoke still carrying the client-asserted `confirmed` field.
    fn confirmed_removed() -> CommandError {
        CommandError::new(
            CONFIRMED_REMOVED,
            "`confirmed` is no longer accepted on an invoke: the server issues a \
             confirmation ticket and a `confirm` frame redeems it. Nothing was run. \
             Reload the desktop to pick up the current client",
        )
    }

    /// Cancel arrived for a correlation id nothing is running under. Not an
    /// error worth closing over: a cancel racing a response is normal.
    fn not_running() -> CommandError {
        CommandError::new(
            "ws.not_running",
            "nothing is running under that id; it had already finished",
        )
    }
}

// ---------------------------------------------------------------------------
// Coupling to the command bus
// ---------------------------------------------------------------------------

/// A boxed future, because [`Dispatch`] has to be a trait object: the bus is
/// chosen at startup and `async fn` in a trait is not dyn-compatible.
pub type Dispatched = Pin<Box<dyn Future<Output = Outcome> + Send + 'static>>;

/// Everything this module needs from the command bus.
///
/// Deliberately one method. Schemas, risk levels, grants, node targeting and
/// the attribution log all live in `crate::command`; none of them change how
/// a frame is carried, so none of them appear here. An unknown command is
/// the bus's answer to give, not this module's to guess, so there is no
/// lookup method to drift out of step with what is registered.
pub trait Dispatch: Send + Sync + 'static {
    fn dispatch(&self, call: Call, progress: ProgressSink) -> Dispatched;
}

/// One invocation, as the bus receives it.
pub struct Call {
    pub command: CommandId,
    pub params: Value,
    pub correlation: CorrelationId,
    pub issuer: Issuer,
    /// Watch this while doing anything slow. Cancellation is cooperative:
    /// nothing aborts the future, because a command killed between two
    /// writes leaves worse state than one that unwinds.
    pub cancel: CancelSignal,
    /// The confirmation ticket, when this call came in as a `confirm` frame.
    /// Carried, not interpreted: whether it is good is the dispatcher's
    /// decision, which holds the tickets.
    pub ticket: Option<String>,
    /// The installed app whose window this call is made for, from the
    /// frame's `as_app`. Carried, not interpreted, like `ticket`.
    pub as_app: Option<String>,
}

/// Who issued a call, for the attribution log and for ownership.
///
/// Facts this module actually knows, nothing inferred. No `Display` and no
/// `Serialize`, and the session id's own `Debug` redacts, so an attribution
/// line cannot leak a bearer token.
///
/// Fields are private with one constructor, which is called in exactly one
/// place — the `invoke` arm, from the peer the upgrade authenticated. Same
/// reasoning as `command::CommandSpec`: a type whose only value is being
/// trustworthy should not be assemblable from parts a caller chose.
#[derive(Debug, Clone)]
pub struct Issuer {
    connection: ConnectionId,
    session: SessionId,
    ip: IpAddr,
}

impl Issuer {
    pub fn new(connection: ConnectionId, session: SessionId, ip: IpAddr) -> Issuer {
        Issuer {
            connection,
            session,
            ip,
        }
    }

    /// Which socket. Server-assigned, not a secret, and the right thing to put
    /// in a log line about one tab.
    pub fn connection(&self) -> ConnectionId {
        self.connection
    }

    pub fn ip(&self) -> IpAddr {
        self.ip
    }

    /// The signed-in session behind the socket.
    ///
    /// This is the one part of a call that is a bearer credential, and it is
    /// carried because the bus needs it: a command that opens a pty has to tag
    /// it with the session so the pty dies when that browser session does. The
    /// adapter hands it to `command::Issuer::browser`, which wraps it in a type
    /// that reveals it only on request and serializes as a digest.
    ///
    /// It must not go anywhere else. Not into a frame — every client already
    /// holds its own cookie and none should learn another's — and not into a
    /// log line or an error string, where it would outlive the session in
    /// whatever backs the logs up.
    pub fn session(&self) -> &SessionId {
        &self.session
    }
}

/// The cancel side of a running call, held by the command.
#[derive(Clone)]
pub struct CancelSignal(watch::Receiver<bool>);

impl CancelSignal {
    pub fn is_canceled(&self) -> bool {
        *self.0.borrow()
    }

    /// Resolves once the call is canceled. Pair it with the real work in a
    /// `select!`.
    pub async fn canceled(&self) {
        let mut rx = self.0.clone();
        while !*rx.borrow_and_update() {
            // The sender is dropped when the registry stops tracking the
            // call, which means nothing is left to deliver a result to.
            // Treating that as a cancel is the same thing from the command's
            // point of view.
            if rx.changed().await.is_err() {
                return;
            }
        }
    }
}

impl fmt::Debug for CancelSignal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "CancelSignal(canceled: {})", self.is_canceled())
    }
}

/// Where a command sends progress. Knows its own correlation id, so a
/// command cannot route an update to someone else's call by mistake.
pub struct ProgressSink {
    id: CorrelationId,
    registry: Arc<Registry>,
    out: mpsc::Sender<Outbound>,
}

impl ProgressSink {
    pub fn id(&self) -> &CorrelationId {
        &self.id
    }

    /// Report progress, dropping it if the connection is behind.
    ///
    /// Never blocks and never fails loudly. Progress is advisory and a
    /// result is not: if a command could be stalled by a client that is slow
    /// to read its own progress bar, a chatty command would hold the bus.
    /// The dropped frame shows up as a gap in `seq`, which the client can
    /// see.
    pub fn report(&self, progress: Progress) {
        // No sequence number means the call is no longer tracked — finished,
        // or the connection went away. Either way there is nothing to say.
        let Some(seq) = self.registry.next_seq(&self.id) else {
            return;
        };
        let frame = ServerFrame::Progress {
            id: self.id.clone(),
            seq,
            progress,
        };
        if self.out.try_send(Outbound::Frame(frame)).is_err() {
            tracing::trace!(id = %self.id, seq, "progress dropped: send queue full or closed");
        }
    }

    /// Shorthand for the common case.
    pub fn message(&self, text: impl Into<String>) {
        self.report(Progress::message(text));
    }
}

// ---------------------------------------------------------------------------
// Injected world
// ---------------------------------------------------------------------------

/// Time, supplied rather than read, so session expiry and elapsed timings
/// are exercisable without waiting. Timers in the connection loop use
/// tokio's clock, which `tokio::time::pause` already controls.
pub trait Clock: Send + Sync + 'static {
    /// Wall time, for stamps that are shown to a person.
    fn now(&self) -> SystemTime;
    /// Monotonic time, for durations. Wall time can step backwards.
    fn mono(&self) -> Instant;
}

pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> SystemTime {
        SystemTime::now()
    }

    fn mono(&self) -> Instant {
        Instant::now()
    }
}

fn millis_since_epoch(clock: &dyn Clock) -> u64 {
    clock
        .now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Transport limits. Not in `config.toml` yet; see the integration notes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Settings {
    /// Largest frame accepted. Params are JSON, not payloads: file contents
    /// move over the file API, which is why this can stay small enough that
    /// one connection cannot cost megabytes of buffer.
    pub max_frame_bytes: usize,
    /// Frames buffered toward one client before progress starts being
    /// dropped.
    pub send_queue: usize,
    /// Commands one connection may have running at once.
    pub max_in_flight: usize,
    /// How long a client has to send `hello`.
    pub handshake_timeout: Duration,
    /// Ping period. Two periods of silence closes the socket, so a laptop
    /// that slept does not leave a connection holding cancel handles.
    pub keepalive: Duration,
    /// How often an open socket's session is checked again. A socket opened
    /// with a valid cookie must not outlive the session behind it.
    pub session_recheck: Duration,
}

impl Default for Settings {
    fn default() -> Settings {
        Settings {
            max_frame_bytes: 256 * 1024,
            send_queue: 256,
            max_in_flight: 32,
            handshake_timeout: Duration::from_secs(10),
            keepalive: Duration::from_secs(20),
            session_recheck: Duration::from_secs(60),
        }
    }
}

// ---------------------------------------------------------------------------
// Per-connection registry
// ---------------------------------------------------------------------------

struct InFlight {
    command: CommandId,
    started: Instant,
    next_seq: u64,
    cancel: watch::Sender<bool>,
}

/// Why a call could not be started.
#[derive(Debug, Error, PartialEq, Eq)]
enum BeginError {
    #[error("correlation id {0} is already running")]
    Duplicate(CorrelationId),

    #[error("already running {max} commands")]
    TooMany { max: usize },
}

/// What one connection is currently running.
///
/// Per connection, not global: a correlation id is chosen by the client, so
/// two tabs will reuse the same ones and a shared table would collide.
pub struct Registry {
    calls: Mutex<HashMap<CorrelationId, InFlight>>,
    max: usize,
}

/// A call that has just finished, for the response frame.
struct Finished {
    command: CommandId,
    elapsed: Duration,
}

impl Registry {
    fn new(max: usize) -> Registry {
        Registry {
            calls: Mutex::new(HashMap::new()),
            max,
        }
    }

    /// Every method takes the lock for one map operation and never across an
    /// await, so a panic mid-operation cannot leave the map half-updated and
    /// a poisoned lock is still usable.
    fn calls(&self) -> std::sync::MutexGuard<'_, HashMap<CorrelationId, InFlight>> {
        self.calls.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn begin(
        &self,
        id: &CorrelationId,
        command: CommandId,
        started: Instant,
    ) -> Result<CancelSignal, BeginError> {
        let mut calls = self.calls();
        if calls.contains_key(id) {
            return Err(BeginError::Duplicate(id.clone()));
        }
        if calls.len() >= self.max {
            return Err(BeginError::TooMany { max: self.max });
        }
        let (cancel, rx) = watch::channel(false);
        calls.insert(
            id.clone(),
            InFlight {
                command,
                started,
                next_seq: 0,
                cancel,
            },
        );
        Ok(CancelSignal(rx))
    }

    /// The next progress sequence number, or `None` if the call is no longer
    /// tracked.
    fn next_seq(&self, id: &CorrelationId) -> Option<u64> {
        let mut calls = self.calls();
        let call = calls.get_mut(id)?;
        let seq = call.next_seq;
        call.next_seq += 1;
        Some(seq)
    }

    fn finish(&self, id: &CorrelationId, now: Instant) -> Option<Finished> {
        let call = self.calls().remove(id)?;
        Some(Finished {
            command: call.command,
            elapsed: now.saturating_duration_since(call.started),
        })
    }

    fn cancel(&self, id: &CorrelationId) -> bool {
        match self.calls().get(id) {
            // `send` fails only when every receiver is gone, which here
            // means the command dropped its signal; the call is ending
            // anyway, so the cancel still counts as delivered.
            Some(call) => {
                let _ = call.cancel.send(true);
                true
            }
            None => false,
        }
    }

    /// Cancel everything, for a connection that is going away.
    ///
    /// Work whose only consumer has disconnected is work no one will see the
    /// result of. A command that genuinely must outlive its connection
    /// belongs in a service the bus owns, not in a socket handler.
    fn cancel_all(&self) {
        for call in self.calls().values() {
            let _ = call.cancel.send(true);
        }
    }

    pub fn in_flight(&self) -> usize {
        self.calls().len()
    }

    /// What is running, for an operator panel.
    pub fn running(&self, now: Instant) -> Vec<(String, Duration)> {
        self.calls()
            .iter()
            .map(|(id, call)| {
                (
                    format!("{id} {}", call.command),
                    now.saturating_duration_since(call.started),
                )
            })
            .collect()
    }
}

// ---------------------------------------------------------------------------
// The hub
// ---------------------------------------------------------------------------

/// Anything queued toward one client.
enum Outbound {
    Frame(ServerFrame),
    /// Close the socket. Lets the hub end a connection without reaching into
    /// the socket itself, which belongs to the connection's own task.
    Close {
        code: CloseCode,
        reason: String,
    },
}

struct Connection {
    session: SessionId,
    out: mpsc::Sender<Outbound>,
    registry: Arc<Registry>,
}

/// Every open connection, so the server can push.
///
/// This is what makes events and the kill switch possible: without it the
/// only frames a client gets are answers to its own requests.
pub struct Hub {
    next_id: AtomicU64,
    connections: Mutex<HashMap<ConnectionId, Connection>>,
    /// Events carry a timestamp the shell shows, so the hub is given a clock
    /// rather than reading one.
    clock: Arc<dyn Clock>,
}

/// One connection, summarized for an operator panel.
///
/// No session id: that is a bearer token and never leaves the server, not
/// even to the signed-in browser that holds it.
#[derive(Debug, Clone, Serialize)]
pub struct ConnectionSummary {
    pub connection: ConnectionId,
    pub in_flight: usize,
}

impl Hub {
    pub fn new(clock: Arc<dyn Clock>) -> Arc<Hub> {
        Arc::new(Hub {
            next_id: AtomicU64::new(1),
            connections: Mutex::new(HashMap::new()),
            clock,
        })
    }

    fn connections(&self) -> std::sync::MutexGuard<'_, HashMap<ConnectionId, Connection>> {
        self.connections
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    fn register(
        &self,
        session: SessionId,
        out: mpsc::Sender<Outbound>,
        registry: Arc<Registry>,
    ) -> ConnectionId {
        let id = ConnectionId(self.next_id.fetch_add(1, Ordering::Relaxed));
        self.connections().insert(
            id,
            Connection {
                session,
                out,
                registry,
            },
        );
        id
    }

    fn unregister(&self, id: ConnectionId) {
        self.connections().remove(&id);
    }

    /// Push an event to every open connection. Returns how many it reached.
    ///
    /// An event is a notification, so a connection whose queue is full is
    /// skipped rather than waited for: one stalled browser must not hold up
    /// a server-wide broadcast.
    pub fn publish(&self, topic: impl Into<String>, payload: Value) -> usize {
        let topic = topic.into();
        let frame = ServerFrame::Event {
            topic: topic.clone(),
            payload,
            at_ms: millis_since_epoch(self.clock.as_ref()),
        };
        let mut sent = 0;
        for (id, conn) in self.connections().iter() {
            if conn.out.try_send(Outbound::Frame(frame.clone())).is_ok() {
                sent += 1;
            } else {
                tracing::debug!(%id, %topic, "event dropped: client is not keeping up");
            }
        }
        sent
    }

    /// Push an event only to the connections belonging to one session.
    pub fn publish_to_session(
        &self,
        session: &SessionId,
        topic: impl Into<String>,
        payload: Value,
    ) -> usize {
        let topic = topic.into();
        let frame = ServerFrame::Event {
            topic,
            payload,
            at_ms: millis_since_epoch(self.clock.as_ref()),
        };
        self.connections()
            .iter()
            .filter(|(_, conn)| &conn.session == session)
            .filter(|(_, conn)| conn.out.try_send(Outbound::Frame(frame.clone())).is_ok())
            .count()
    }

    /// Drop every connection on a session. This is the transport half of the
    /// kill switch: revoking a session in `AuthState` stops the *next*
    /// request, and this stops the socket that is already open.
    pub fn revoke_session(&self, session: &SessionId, reason: &str) -> usize {
        let mut closed = 0;
        for conn in self.connections().values() {
            if &conn.session == session {
                // A close is not droppable the way an event is, but a
                // connection too full to accept one is a connection that is
                // about to be closed by backpressure anyway.
                if conn
                    .out
                    .try_send(Outbound::Close {
                        code: close::UNAUTHENTICATED,
                        reason: reason.to_string(),
                    })
                    .is_ok()
                {
                    closed += 1;
                }
            }
        }
        closed
    }

    /// Close every connection, for shutdown.
    pub fn close_all(&self, reason: &str) -> usize {
        self.connections()
            .iter()
            .filter(|(_, conn)| {
                conn.out
                    .try_send(Outbound::Close {
                        code: axum::extract::ws::close_code::AWAY,
                        reason: reason.to_string(),
                    })
                    .is_ok()
            })
            .count()
    }

    pub fn summary(&self) -> Vec<ConnectionSummary> {
        self.connections()
            .iter()
            .map(|(id, conn)| ConnectionSummary {
                connection: *id,
                in_flight: conn.registry.in_flight(),
            })
            .collect()
    }
}

// ---------------------------------------------------------------------------
// Context and routing
// ---------------------------------------------------------------------------

/// What the handler needs. Built once at startup.
pub struct WsCtx {
    auth: Arc<Ctx>,
    dispatch: Arc<dyn Dispatch>,
    hub: Arc<Hub>,
    clock: Arc<dyn Clock>,
    node_id: String,
    host_name: String,
    settings: Settings,
}

impl WsCtx {
    pub fn new(
        auth: Arc<Ctx>,
        dispatch: Arc<dyn Dispatch>,
        hub: Arc<Hub>,
        clock: Arc<dyn Clock>,
        node_id: String,
        host_name: String,
        settings: Settings,
    ) -> Arc<WsCtx> {
        Arc::new(WsCtx {
            auth,
            dispatch,
            hub,
            clock,
            node_id,
            host_name,
            settings,
        })
    }

    pub fn hub(&self) -> &Arc<Hub> {
        &self.hub
    }

    fn now(&self) -> Now {
        Now {
            wall: self.clock.now(),
            mono: self.clock.mono(),
        }
    }

    /// Whether the session behind an open socket is still good.
    fn session_ok(&self, peer: &Peer) -> bool {
        let now = self.now();
        let mut state = self.auth.state();
        matches!(
            state.verify(peer.ip, Some(&peer.session), now),
            VerifyOutcome::Ok
        )
    }
}

/// Who is on the other end of an upgraded socket.
struct Peer {
    ip: IpAddr,
    session: SessionId,
}

/// The bus route, with its own state so it can be merged into the HTTP
/// router without either router's state type infecting the other.
pub fn router(ctx: Arc<WsCtx>) -> Router {
    Router::new().route(PATH, get(upgrade)).with_state(ctx)
}

/// A present `Origin` must name the same host as `Host`.
///
/// Without this, any page the browser happens to have open could open a
/// socket to the bus and ride the session cookie — the same-origin policy
/// does not apply to WebSocket, so this check is the whole defense. A
/// missing `Origin` is allowed because browsers always send one on an
/// upgrade; its absence means a non-browser client (the CLI over loopback),
/// which has nothing to forge on anyone's behalf.
///
// NOTE: this duplicates `http::origin_ok`, which is private. Worth making
// that one `pub(crate)` and deleting this when a shared file can be touched.
fn origin_ok(headers: &HeaderMap) -> bool {
    let Some(origin) = headers.get(header::ORIGIN) else {
        return true;
    };
    let (Ok(origin), Some(Ok(host))) = (
        origin.to_str(),
        headers.get(header::HOST).map(|h| h.to_str()),
    ) else {
        return false;
    };
    let authority = origin
        .strip_prefix("https://")
        .or_else(|| origin.strip_prefix("http://"));
    authority.is_some_and(|a| a.eq_ignore_ascii_case(host))
}

/// Refuse before upgrading. The body is for `curl` and the log; a browser
/// never shows it, which is exactly why the close codes below exist for
/// everything discovered after the upgrade.
fn refuse(status: StatusCode, message: &'static str) -> Response {
    (
        status,
        [(header::CACHE_CONTROL, "no-store")],
        format!("{message}\n"),
    )
        .into_response()
}

async fn upgrade(
    State(ctx): State<Arc<WsCtx>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    upgrade: WebSocketUpgrade,
) -> Response {
    let ip = ctx.auth.client_ip(peer, &headers);

    if !origin_ok(&headers) {
        tracing::warn!(%ip, "ws refused: Origin does not match Host");
        return refuse(StatusCode::FORBIDDEN, "cross-origin websocket refused");
    }

    let session = ctx.auth.session_cookie(peer, &headers);

    let decision = {
        let now = ctx.now();
        let mut state = ctx.auth.state();
        verify_response(state.verify(ip, session.as_ref(), now), PATH, true)
    };

    match decision {
        VerifyResponse::Allow => {}
        VerifyResponse::Forbidden => {
            tracing::info!(%ip, "ws refused: address blocked");
            return refuse(StatusCode::FORBIDDEN, "address is blocked");
        }
        // `verify_response` maps an upgrade to 401 rather than a redirect,
        // because a WebSocket client has nowhere to follow one to.
        VerifyResponse::Unauthorized | VerifyResponse::RedirectToLogin => {
            tracing::info!(%ip, "ws refused: no valid session");
            return refuse(StatusCode::UNAUTHORIZED, "sign in first: no valid session");
        }
    }

    let Some(session) = session else {
        // `verify` returns Ok only for a live session, so this is a bug here
        // rather than anything a client did.
        tracing::error!(%ip, "ws allowed with no session id; refusing anyway");
        return refuse(
            StatusCode::INTERNAL_SERVER_ERROR,
            "session state is inconsistent",
        );
    };

    let who = Peer { ip, session };
    let max = ctx.settings.max_frame_bytes;
    upgrade
        .max_message_size(max)
        .on_upgrade(move |socket| serve(ctx, socket, who))
}

// ---------------------------------------------------------------------------
// The connection
// ---------------------------------------------------------------------------

/// Why a connection ended, and what the client is told.
///
/// The `Display` text becomes the close reason, so each one is written for
/// whoever reads it in a console: it names the problem and what to do.
#[derive(Debug, Error)]
enum Ending {
    #[error("client closed the connection")]
    ClientGone,

    #[error("transport failed: {0}")]
    Transport(String),

    #[error(
        "no hello within {0:?}; the first frame must be \
         {{\"type\":\"hello\",\"protocol\":1}}"
    )]
    NoHello(Duration),

    #[error(
        "protocol {offered} is not supported; this node speaks {PROTOCOL_VERSION}. \
         Reload the desktop to pick up the matching client; if that does not help, \
         the client is newer than this node and the node needs updating."
    )]
    Protocol { offered: u32 },

    #[error("frame refused: {0}")]
    BadFrame(String),

    #[error("binary frames are not part of this protocol; frames are JSON text")]
    Binary,

    #[error("session expired or was revoked; sign in again")]
    SessionGone,

    #[error("no traffic for {0:?}; closing an idle connection")]
    Idle(Duration),

    #[error("{reason}")]
    AsAsked { code: CloseCode, reason: String },
}

impl Ending {
    fn code(&self) -> CloseCode {
        use axum::extract::ws::close_code;
        match self {
            Ending::ClientGone => close_code::NORMAL,
            Ending::Transport(_) => close_code::ABNORMAL,
            Ending::NoHello(_) | Ending::Idle(_) => close::TIMEOUT,
            Ending::Protocol { .. } => close::PROTOCOL,
            Ending::BadFrame(_) | Ending::Binary => close::BAD_FRAME,
            Ending::SessionGone => close::UNAUTHENTICATED,
            Ending::AsAsked { code, .. } => *code,
        }
    }

    /// Whether to bother sending a close frame. A client that already closed
    /// or whose transport failed will not read one.
    fn worth_announcing(&self) -> bool {
        !matches!(self, Ending::ClientGone | Ending::Transport(_))
    }
}

async fn serve(ctx: Arc<WsCtx>, mut socket: WebSocket, peer: Peer) {
    let (out, mut rx) = mpsc::channel::<Outbound>(ctx.settings.send_queue);
    let registry = Arc::new(Registry::new(ctx.settings.max_in_flight));
    let conn = ctx
        .hub
        .register(peer.session.clone(), out.clone(), Arc::clone(&registry));
    tracing::info!(%conn, ip = %peer.ip, "ws open");

    let ending = match handshake(&ctx, &mut socket, conn).await {
        Ok(()) => pump(&ctx, &mut socket, &mut rx, &out, &registry, &peer, conn).await,
        Err(e) => e,
    };

    // Order matters: stop accepting work and cancel what is running before
    // dropping the registry, so commands see the cancel rather than a
    // silently closed channel.
    registry.cancel_all();
    ctx.hub.unregister(conn);

    tracing::info!(%conn, code = ending.code(), reason = %ending, "ws closed");
    if ending.worth_announcing() {
        let frame = CloseFrame {
            code: ending.code(),
            reason: fits_in_close_frame(&ending.to_string()),
        };
        if let Err(e) = socket.send(Message::Close(Some(frame))).await {
            tracing::debug!(%conn, error = %e, "could not send close frame");
        }
    }
}

/// A close frame is a control frame: 125 bytes of payload, 2 of which are
/// the code. A longer reason makes the frame invalid, so it is cut on a
/// character boundary rather than rejected.
fn fits_in_close_frame(reason: &str) -> Utf8Bytes {
    const MAX: usize = 123;
    if reason.len() <= MAX {
        return Utf8Bytes::from(reason.to_string());
    }
    let mut end = MAX;
    while !reason.is_char_boundary(end) {
        end -= 1;
    }
    Utf8Bytes::from(reason[..end].to_string())
}

async fn handshake(
    ctx: &Arc<WsCtx>,
    socket: &mut WebSocket,
    conn: ConnectionId,
) -> Result<(), Ending> {
    let timeout = ctx.settings.handshake_timeout;
    let first = tokio::time::timeout(timeout, socket.recv())
        .await
        .map_err(|_| Ending::NoHello(timeout))?
        .ok_or(Ending::ClientGone)?
        .map_err(|e| Ending::Transport(e.to_string()))?;

    let text = match first {
        Message::Text(text) => text,
        Message::Binary(_) => return Err(Ending::Binary),
        Message::Close(_) => return Err(Ending::ClientGone),
        // Control frames before the handshake are harmless but say nothing,
        // and waiting again would need the timeout re-armed; one hello
        // first, always, is the simpler contract.
        Message::Ping(_) | Message::Pong(_) => {
            return Err(Ending::BadFrame(
                "expected hello, got a control frame".into(),
            ));
        }
    };

    let frame: ClientFrame =
        serde_json::from_str(&text).map_err(|e| Ending::BadFrame(e.to_string()))?;
    let ClientFrame::Hello { protocol, client } = frame else {
        return Err(Ending::BadFrame(
            "first frame must be hello; nothing is accepted before the version is agreed".into(),
        ));
    };
    if protocol != PROTOCOL_VERSION {
        return Err(Ending::Protocol { offered: protocol });
    }
    tracing::info!(%conn, protocol, client = client.as_deref().unwrap_or("unknown"), "ws hello");

    let welcome = ServerFrame::Welcome {
        protocol: PROTOCOL_VERSION,
        connection: conn,
        node_id: ctx.node_id.clone(),
        host_name: ctx.host_name.clone(),
    };
    send_now(socket, &welcome).await
}

/// Write one frame straight to the socket, bypassing the queue. Only used
/// before the queue is being drained.
async fn send_now(socket: &mut WebSocket, frame: &ServerFrame) -> Result<(), Ending> {
    let text = serde_json::to_string(frame).map_err(|e| Ending::Transport(e.to_string()))?;
    socket
        .send(Message::Text(text.into()))
        .await
        .map_err(|e| Ending::Transport(e.to_string()))
}

async fn pump(
    ctx: &Arc<WsCtx>,
    socket: &mut WebSocket,
    rx: &mut mpsc::Receiver<Outbound>,
    out: &mpsc::Sender<Outbound>,
    registry: &Arc<Registry>,
    peer: &Peer,
    conn: ConnectionId,
) -> Ending {
    let keepalive = ctx.settings.keepalive;
    let mut ping = tokio::time::interval_at(tokio::time::Instant::now() + keepalive, keepalive);
    let mut recheck = tokio::time::interval_at(
        tokio::time::Instant::now() + ctx.settings.session_recheck,
        ctx.settings.session_recheck,
    );
    let mut last_heard = tokio::time::Instant::now();

    loop {
        tokio::select! {
            incoming = socket.recv() => {
                let Some(message) = incoming else {
                    return Ending::ClientGone;
                };
                let message = match message {
                    Ok(m) => m,
                    Err(e) => return Ending::Transport(e.to_string()),
                };
                last_heard = tokio::time::Instant::now();
                match message {
                    Message::Text(text) => {
                        let frame = match serde_json::from_str::<ClientFrame>(&text) {
                            Ok(frame) => frame,
                            Err(e) => return Ending::BadFrame(e.to_string()),
                        };
                        if let Some(ending) = handle(ctx, frame, registry, out, peer, conn) {
                            return ending;
                        }
                    }
                    Message::Binary(_) => return Ending::Binary,
                    Message::Close(_) => return Ending::ClientGone,
                    // tungstenite answers pings itself; a pong is just proof
                    // of life, which `last_heard` already recorded.
                    Message::Ping(_) | Message::Pong(_) => {}
                }
            }

            Some(outbound) = rx.recv() => {
                match outbound {
                    Outbound::Frame(frame) => {
                        // Writing here, in the same task that reads, is what
                        // gives the client real backpressure: a client that
                        // will not read stalls this loop, fills the queue,
                        // and is eventually closed, rather than growing a
                        // buffer on the server.
                        if let Err(e) = send_now(socket, &frame).await {
                            return e;
                        }
                    }
                    Outbound::Close { code, reason } => {
                        return Ending::AsAsked { code, reason };
                    }
                }
            }

            _ = ping.tick() => {
                if last_heard.elapsed() > keepalive * 2 {
                    return Ending::Idle(last_heard.elapsed());
                }
                if socket.send(Message::Ping(Bytes::new())).await.is_err() {
                    return Ending::ClientGone;
                }
            }

            _ = recheck.tick() => {
                // A cookie checked once at the upgrade would let a socket
                // outlive the session that opened it, which for a long-lived
                // desktop connection means expiry never happens at all.
                if !ctx.session_ok(peer) {
                    return Ending::SessionGone;
                }
            }
        }
    }
}

/// Act on one client frame.
///
/// Returns `Some` only when the connection has to end. Synchronous on
/// purpose: it must never await a queue that only [`pump`] drains, or a full
/// queue would deadlock the loop that empties it. Hence `try_send` here and
/// `send().await` in the spawned task, which is a different task and may
/// safely wait.
fn handle(
    ctx: &Arc<WsCtx>,
    frame: ClientFrame,
    registry: &Arc<Registry>,
    out: &mpsc::Sender<Outbound>,
    peer: &Peer,
    conn: ConnectionId,
) -> Option<Ending> {
    match frame {
        // A second hello is a confused client, not a renegotiation.
        ClientFrame::Hello { .. } => Some(Ending::BadFrame(
            "hello was already exchanged; open a new connection to change protocol".into(),
        )),

        // Answered inline rather than queued behind command work, because a
        // pong that waits for a slow command would report the connection dead
        // exactly when the node is busiest. `try_send` for the same reason: if
        // the outbound queue is full the connection has a real problem and a
        // dropped pong is the correct signal, not something to block on.
        ClientFrame::Ping { token } => {
            let _ = out.try_send(Outbound::Frame(ServerFrame::Pong { token }));
            None
        }

        ClientFrame::Cancel { id } => {
            if registry.cancel(&id) {
                tracing::debug!(%conn, %id, "cancel requested");
                None
            } else {
                // No response frame is owed for an id nothing is running:
                // one would be indistinguishable from the real response to
                // the call that just finished.
                tracing::debug!(%conn, %id, error = %CommandError::not_running().message, "cancel ignored");
                None
            }
        }

        // A confirm is an invoke that carries a ticket; past this point the
        // two are one path, so neither can grow a check the other lacks.
        frame @ (ClientFrame::Invoke { .. } | ClientFrame::Confirm { .. }) => {
            let (id, command, params, ticket, as_app) = match frame {
                ClientFrame::Invoke {
                    id,
                    command,
                    params,
                    confirmed,
                    as_app,
                } => {
                    // Answered rather than closed: the call is refused, and
                    // the socket is fine. Before anything is registered, so
                    // it costs no in-flight slot and reaches no command.
                    if confirmed.is_some() {
                        return queue_or_close(
                            out,
                            ServerFrame::Response {
                                id,
                                outcome: Outcome::error(CommandError::confirmed_removed()),
                                elapsed_ms: 0,
                            },
                        );
                    }
                    (id, command, params, None, as_app)
                }
                ClientFrame::Confirm {
                    id,
                    ticket,
                    command,
                    params,
                    as_app,
                } => (id, command, params, Some(ticket), as_app),
                _ => unreachable!("only invoke and confirm reach this arm"),
            };
            let started = ctx.clock.mono();
            let cancel = match registry.begin(&id, command.clone(), started) {
                Ok(cancel) => cancel,
                Err(BeginError::Duplicate(id)) => {
                    // Answering with an error would be ambiguous: the client
                    // could not tell which of the two calls under this id it
                    // refers to. A client reusing a live correlation id has a
                    // bug, and its results are already uninterpretable.
                    return Some(Ending::BadFrame(format!(
                        "correlation id {id} is already running; ids must be unique \
                         while a call is in flight"
                    )));
                }
                Err(BeginError::TooMany { max }) => {
                    // Load, not a bug: refuse this one call and keep going.
                    return queue_or_close(
                        out,
                        ServerFrame::Response {
                            id,
                            outcome: Outcome::error(CommandError::too_many(max)),
                            elapsed_ms: 0,
                        },
                    );
                }
            };

            let call = Call {
                command: command.clone(),
                params,
                correlation: id.clone(),
                // The session comes from `peer`, which the upgrade verified
                // before the socket existed, and is never re-read from a
                // frame: a client cannot name the session it is acting as.
                issuer: Issuer::new(conn, peer.session.clone(), peer.ip),
                cancel,
                ticket,
                as_app,
            };
            let sink = ProgressSink {
                id: id.clone(),
                registry: Arc::clone(registry),
                out: out.clone(),
            };

            let dispatched = ctx.dispatch.dispatch(call, sink);
            let registry = Arc::clone(registry);
            let out = out.clone();
            let clock = Arc::clone(&ctx.clock);
            tokio::spawn(async move {
                // The command runs in a task of its own so that a panic
                // inside it comes back as a `JoinError` instead of taking
                // this task with it. A panic that killed this task would
                // leave the call in the registry forever: no response for
                // the client to stop waiting on, and one in-flight slot
                // gone for the life of the connection, so a command that
                // panics reliably would wedge the socket.
                let outcome = match tokio::spawn(dispatched).await {
                    Ok(outcome) => outcome,
                    Err(e) => {
                        tracing::error!(%conn, %id, error = %e, "command panicked");
                        Outcome::error(CommandError::new(
                            "ws.command_panicked",
                            "the command failed with an internal error; \
                             the node's log has the details",
                        ))
                    }
                };
                // The registry is the authority on elapsed time; a call the
                // registry has already forgotten belongs to a connection
                // that is gone, so there is nobody to answer.
                let Some(finished) = registry.finish(&id, clock.mono()) else {
                    tracing::debug!(%id, "result discarded: connection closed first");
                    return;
                };
                tracing::debug!(
                    %conn,
                    %id,
                    command = %finished.command,
                    elapsed_ms = finished.elapsed.as_millis() as u64,
                    "command finished"
                );
                let response = ServerFrame::Response {
                    id,
                    outcome,
                    elapsed_ms: finished.elapsed.as_millis() as u64,
                };
                // Unlike progress, a result waits its turn rather than being
                // dropped: a client that never hears how its call ended can
                // only guess.
                if out.send(Outbound::Frame(response)).await.is_err() {
                    tracing::debug!("result undelivered: connection closed");
                }
            });
            None
        }
    }
}

/// Queue a frame from inside [`handle`], or end the connection.
///
/// A full queue here means the client is not reading what it asked for.
/// Dropping the frame would leave a request unanswered forever, so the
/// connection is closed instead and the client can retry on a fresh one.
fn queue_or_close(out: &mpsc::Sender<Outbound>, frame: ServerFrame) -> Option<Ending> {
    match out.try_send(Outbound::Frame(frame)) {
        Ok(()) => None,
        Err(mpsc::error::TrySendError::Full(_)) => Some(Ending::AsAsked {
            code: close::BACKPRESSURE,
            reason: "send queue is full; the client is not reading its frames".into(),
        }),
        Err(mpsc::error::TrySendError::Closed(_)) => Some(Ending::ClientGone),
    }
}
