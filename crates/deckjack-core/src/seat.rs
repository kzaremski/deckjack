//! The single seat: one workspace, one connection using it at a time.
//!
//! Apps, windows and terminals live in one workspace on the node. Several
//! devices may stay signed in, but only one shell connection is actively
//! using Deckjack at any moment — it **holds the seat** — and any signed-in
//! device can take it over. The others see who has it and since when, and a
//! button.
//!
//! # Why the server enforces it
//!
//! A seat the shell merely agreed to respect would be a seat any script on a
//! second socket could ignore, and two devices driving one set of windows and
//! one shell at the same time is exactly the confusion this exists to stop.
//! So the bus adapter (`wiring::BusDispatch`) refuses every call that arrives
//! over a shell socket that does not hold the seat, with [`NOT_HELD`], except
//! the `seat.*` commands themselves and the kill switch. What is not a shell
//! socket is not touched: the host's own CLI, agents over the agent bridge,
//! an approval released from the Inbox and app backends over their own
//! sockets are not "a device using the workspace", and a lockdown has to be
//! one click from any signed-in device whoever is seated.
//!
//! # What the seat is
//!
//! A node-wide lease held by exactly one connection, identified by the
//! connection id the transport assigned and the session behind it, with the
//! device label the shell sent and when it was taken.
//!
//! - **Vacant**: nobody holds it. The next connection to join, or to ask for
//!   anything, gets it without a prompt.
//! - **Held** by one connection.
//! - **Reserved**: the holder's connection dropped. For [`GRACE`] the seat
//!   stays with its *session* — a phone changing networks reconnects as a new
//!   connection on the same cookie, and that reconnect takes the seat back
//!   silently. After the grace it is vacant.
//!
//! A new connection from the session that holds the seat takes it at once,
//! even if the old connection has not been noticed dropping yet: a socket
//! whose peer vanished without a FIN looks open for as long as the keepalive
//! takes, and a reload must not have to wait that out. The same rule makes a
//! second tab of the same browser take the seat from the first, which then
//! shows the takeover screen like any other device.
//!
//! # Devices and attendants
//!
//! A connection whose hello names a `device` is a device: the shell. It is
//! what takes the seat, and what the rule above about a new connection of the
//! holding session applies to. A connection that names none is an
//! **attendant**: an app's own UI talking to the bus over its own socket
//! (`web/public/kit/bus.js`), on the same browser and cookie as the shell
//! that framed it. An attendant never takes the seat from its session's
//! holder — if it did, every app window that connected would take the seat
//! from the shell around it — and its calls are admitted exactly while its
//! session holds the seat (or holds the reservation during the grace). On a
//! vacant seat an attendant gets it like anyone else, so a client that names
//! no device still works alone. An attendant may not take, claim or release
//! the seat by command: the seat's commands are exempt from the gate, and an
//! app's own socket is a plain browser connection that the bus's hard-deny
//! list (which applies to `as_app` calls) never sees, so the refusal is here.
//!
//! # Calls that span a move
//!
//! The gate decides once, when a call is admitted, and a call can still be
//! running when the seat moves. [`Seat::admits_now`] answers the question
//! again at the moment a part of the workspace changes something; the
//! windows ask it. The terminals instead follow the seat for a pty that
//! registers after the move (`terminal::Registry::transfer_owner` records
//! the move), and `terminal.read` checks ownership again when its long poll
//! wakes.
//!
//! # The workspace follows the seat
//!
//! [`Workspace`] is the hook. When the seat moves to another session the
//! terminals the previous holder's session owned are handed to the new one
//! (`terminal::Registry::transfer_owner`), so the new device can reattach and
//! the old one cannot. When a connection acquires the seat it is sent a
//! snapshot of the open windows, so it shows exactly what was open.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, Weak};
use std::time::{Duration, Instant, UNIX_EPOCH};

use serde::Deserialize;
use serde_json::{Value, json};

use crate::auth::{SessionHash, SessionId};
use crate::command::{Command, CommandError, Context, Registry, RegistryError, RiskLevel};
use crate::ws::{self, ConnectionId, Hub};

/// How long a dropped holder's session keeps the seat.
///
/// Long enough for a phone to hand off between networks and reconnect, short
/// enough that a device that really went away does not keep the workspace
/// from the next one for long. Not configurable: the feature is always on,
/// and [`Seat::with_grace`] is the one way to change it, for tests.
pub const GRACE: Duration = Duration::from_secs(30);

/// The error code for a call refused because its connection does not hold
/// the seat. Its own code, so the shell can tell "another device has the
/// workspace" from every other refusal and show the takeover screen rather
/// than a toast.
pub const NOT_HELD: &str = "seat_not_held";

/// The event topic the seat is pushed under, to every connection.
pub const TOPIC: &str = "seat";

/// The most characters a device label keeps.
pub const DEVICE_MAX_CHARS: usize = 40;

/// What a connection that sent no usable label is called.
pub const UNNAMED_DEVICE: &str = "another device";

/// Commands a connection that does not hold the seat may still run.
///
/// The seat's own commands, which are how a device sees who has it and takes
/// it; and the kill switch, which in an emergency must be one click from any
/// signed-in device whoever is seated (`wiring::Lockdown` argues why it does
/// not even confirm).
pub fn exempt(command: &str) -> bool {
    command == "seat" || command.starts_with("seat.") || command == crate::lockdown::CMD_ENGAGE
}

/// A device label as the shell sent it, made safe to show on another device.
///
/// Trimmed, control and bidi-formatting characters removed (a label is shown
/// to *other* devices, so a right-to-left override in one could reorder the
/// sentence around it there), runs of whitespace collapsed, and cut to
/// [`DEVICE_MAX_CHARS`] characters. Nothing left means [`UNNAMED_DEVICE`].
pub fn sanitize_device(raw: Option<&str>) -> String {
    let Some(raw) = raw else {
        return UNNAMED_DEVICE.to_string();
    };
    let mut out = String::new();
    let mut gap = false;
    for c in raw.chars() {
        if c.is_whitespace() {
            gap = !out.is_empty();
            continue;
        }
        if c.is_control() || is_bidi_or_invisible(c) {
            continue;
        }
        if gap {
            out.push(' ');
            gap = false;
        }
        out.push(c);
    }
    let cut: String = out.chars().take(DEVICE_MAX_CHARS).collect();
    let cut = cut.trim_end().to_string();
    if cut.is_empty() {
        UNNAMED_DEVICE.to_string()
    } else {
        cut
    }
}

/// Characters that change how surrounding text is shown without being
/// visible themselves: every format character (Unicode general category
/// `Cf`, as of Unicode 16) — the bidi marks, embeddings, overrides and
/// isolates, the zero-width characters, the soft hyphen, the interlinear
/// annotation controls and the tag characters. The standard library has no
/// category lookup, so the class is listed here; it changes rarely, and a
/// character missing from it is shown, not run.
fn is_bidi_or_invisible(c: char) -> bool {
    matches!(
        c,
        '\u{00AD}'
            | '\u{0600}'..='\u{0605}'
            | '\u{061C}'
            | '\u{06DD}'
            | '\u{070F}'
            | '\u{0890}'..='\u{0891}'
            | '\u{08E2}'
            | '\u{180E}'
            | '\u{200B}'..='\u{200F}'
            | '\u{202A}'..='\u{202E}'
            | '\u{2060}'..='\u{2064}'
            | '\u{2066}'..='\u{206F}'
            | '\u{FEFF}'
            | '\u{FFF9}'..='\u{FFFB}'
            | '\u{110BD}'
            | '\u{110CD}'
            | '\u{13430}'..='\u{1343F}'
            | '\u{1BCA0}'..='\u{1BCA3}'
            | '\u{1D173}'..='\u{1D17A}'
            | '\u{E0001}'
            | '\u{E0020}'..='\u{E007F}'
    )
}

/// A part of the workspace that follows the seat.
///
/// Every method has a default, so a part implements only what it has.
pub trait Workspace: Send + Sync + 'static {
    /// Called once, when the seat is built, with the hub the seat pushes to,
    /// for a part that pushes its own changes (the windows do), and the seat
    /// itself, for a part that has to ask whether a call that was admitted a
    /// moment ago still may change it ([`Seat::admits_now`]).
    fn attach(&self, _hub: &Arc<Hub>, _seat: &Weak<Seat>) {}
    /// The seat moved from one browser session to another.
    fn moved(&self, _from: &SessionHash, _to: &SessionHash) {}
    /// An event to send the connection that has just acquired the seat, so it
    /// shows the workspace as it is: `(topic, payload)`.
    fn snapshot(&self) -> Option<(String, Value)> {
        None
    }
}

impl Workspace for crate::terminal::Registry {
    fn moved(&self, from: &SessionHash, to: &SessionHash) {
        self.transfer_owner(from, to);
    }
}

/// One joined connection.
struct Peer {
    session: SessionHash,
    device: String,
    /// The hello named a device. Only such a connection — the shell — takes
    /// the seat from its own session's holder; see "Devices and attendants".
    names_device: bool,
}

/// Who has the seat, when someone does.
#[derive(Clone)]
struct Lease {
    session: SessionHash,
    device: String,
    /// Wall clock, milliseconds since the epoch: shown to a person.
    since_ms: u64,
}

enum Holder {
    Vacant,
    Held {
        connection: ConnectionId,
        lease: Lease,
    },
    Reserved {
        lease: Lease,
        until: Instant,
    },
}

struct State {
    peers: HashMap<ConnectionId, Peer>,
    holder: Holder,
    /// The last session to hold the seat, whether or not anyone holds it
    /// now. Its terminals go to whoever acquires the seat next.
    previous: Option<SessionHash>,
    /// Bumped on every change of holder, so a grace timer armed for one
    /// reservation does nothing to a later one.
    generation: u64,
}

/// What the seat looks like to one connection. See [`Seat::status`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Status {
    /// This connection holds the seat.
    pub held: bool,
    /// This connection would get the seat without taking it from anyone: it
    /// is vacant, or reserved for this connection's own session.
    pub claimable: bool,
    /// Who has it, when anyone does.
    pub holder: Option<HolderView>,
}

/// The holder as the other devices see it. No session, no connection id:
/// the label and the time are what a person needs, and a connection id would
/// tell another browser nothing it should act on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HolderView {
    pub device: String,
    pub since_ms: u64,
    /// False while the seat is reserved for a holder whose connection
    /// dropped and may yet come back.
    pub connected: bool,
}

impl Status {
    /// The wire shape: the `seat` event's payload and every seat command's
    /// answer. Mirrored in `web/src/stores/seat.ts`.
    pub fn to_json(&self) -> Value {
        json!({
            "held": self.held,
            "claimable": self.claimable,
            "holder": self.holder.as_ref().map(|h| json!({
                "device": h.device,
                "since_ms": h.since_ms,
                "connected": h.connected,
            })),
        })
    }
}

/// The seat. One per node.
pub struct Seat {
    state: Mutex<State>,
    /// The connections whose calls the seat admits right now, kept in step
    /// with `state` on every change, behind a lock of its own that is never
    /// held while taking another. It is what [`Seat::admits_now`] reads, so a
    /// part of the workspace can ask while holding its own lock — the one it
    /// also takes for the snapshot, which `acquire` takes under `state` —
    /// without the two locks ever being taken in opposite orders.
    admitted: Mutex<HashSet<ConnectionId>>,
    hub: Arc<Hub>,
    clock: Arc<dyn ws::Clock>,
    grace: Duration,
    workspace: Vec<Arc<dyn Workspace>>,
    me: Weak<Seat>,
}

impl Seat {
    /// The seat, with the parts of the workspace that follow it. `workspace`
    /// is required rather than added later for the reason `wiring::BusParts`
    /// gives: a seat built without the terminals would move the seat and
    /// leave the shells with a session that can no longer reach them, and
    /// nothing about that is a compile error or a panic.
    pub fn new(
        hub: Arc<Hub>,
        clock: Arc<dyn ws::Clock>,
        workspace: Vec<Arc<dyn Workspace>>,
    ) -> Arc<Seat> {
        Seat::with_grace(hub, clock, workspace, GRACE)
    }

    /// [`Seat::new`] with another grace period, so a test does not wait
    /// thirty seconds to see one expire.
    pub fn with_grace(
        hub: Arc<Hub>,
        clock: Arc<dyn ws::Clock>,
        workspace: Vec<Arc<dyn Workspace>>,
        grace: Duration,
    ) -> Arc<Seat> {
        Arc::new_cyclic(|me| {
            for part in &workspace {
                part.attach(&hub, me);
            }
            Seat {
                state: Mutex::new(State {
                    peers: HashMap::new(),
                    holder: Holder::Vacant,
                    previous: None,
                    generation: 0,
                }),
                admitted: Mutex::new(HashSet::new()),
                hub,
                clock,
                grace,
                workspace,
                me: me.clone(),
            }
        })
    }

    /// The lock is taken for one decision and its side effects, never across
    /// an await, and nothing called under it calls back into the seat.
    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Bring [`Seat::admits_now`]'s answer in line with `st`. Called after
    /// every change to who is joined or who holds the seat, before anything
    /// that change triggers.
    fn sync(&self, st: &State) {
        let now: HashSet<ConnectionId> = st
            .peers
            .keys()
            .copied()
            .filter(|c| Seat::status_in(st, Some(*c)).held)
            .collect();
        *self.admitted.lock().unwrap_or_else(PoisonError::into_inner) = now;
    }

    /// Whether a call on `connection` may change the workspace at this
    /// instant: it holds the seat, or rides on its session's lease.
    ///
    /// The gate in `wiring::BusDispatch` decides once, when a call is
    /// admitted; a call can still be running when the seat moves. A part of
    /// the workspace whose change must not land after the move — a window
    /// opened by the old holder after the new one was handed the snapshot
    /// would be a window the new holder never hears of — asks again here, at
    /// the moment it changes anything. Never claims the seat, and never takes
    /// the seat's main lock.
    pub fn admits_now(&self, connection: ConnectionId) -> bool {
        self.admitted
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .contains(&connection)
    }

    fn now_ms(&self) -> u64 {
        self.clock
            .now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0)
    }

    /// A reservation whose grace is over is a vacant seat. Checked on every
    /// entry rather than trusted to the timer, which only exists inside a
    /// runtime and may fire a moment late.
    fn expire_if_due(&self, st: &mut State) -> bool {
        if let Holder::Reserved { until, .. } = st.holder
            && self.clock.mono() >= until
        {
            st.holder = Holder::Vacant;
            st.generation += 1;
            self.sync(st);
            return true;
        }
        false
    }

    fn status_in(st: &State, connection: Option<ConnectionId>) -> Status {
        let peer = connection.and_then(|c| st.peers.get(&c));
        let session = peer.map(|p| p.session);
        // An attendant rides on its session's lease: see the module note.
        let attends =
            |lease: &Lease| peer.is_some_and(|p| !p.names_device && p.session == lease.session);
        match &st.holder {
            Holder::Vacant => Status {
                held: false,
                // Only a connection that has joined: one whose session just
                // ended has been dropped from the peers and gets nothing.
                claimable: peer.is_some(),
                holder: None,
            },
            Holder::Held {
                connection: holder,
                lease,
            } => Status {
                held: Some(*holder) == connection || attends(lease),
                claimable: false,
                holder: Some(HolderView {
                    device: lease.device.clone(),
                    since_ms: lease.since_ms,
                    connected: true,
                }),
            },
            Holder::Reserved { lease, .. } => Status {
                held: attends(lease),
                claimable: session == Some(lease.session) && !attends(lease),
                holder: Some(HolderView {
                    device: lease.device.clone(),
                    since_ms: lease.since_ms,
                    connected: false,
                }),
            },
        }
    }

    /// Tell every joined connection what the seat looks like to it.
    fn broadcast(&self, st: &State) {
        for connection in st.peers.keys() {
            let status = Seat::status_in(st, Some(*connection));
            self.hub.send_to(*connection, TOPIC, status.to_json());
        }
    }

    /// Give the seat to `connection`, which must have joined. Moves the
    /// previous holder's workspace, sends the newcomer the snapshot, and
    /// tells everyone.
    fn acquire(&self, st: &mut State, connection: ConnectionId) {
        let Some(peer) = st.peers.get(&connection) else {
            return;
        };
        let (session, device) = (peer.session, peer.device.clone());
        // The same session carrying on — a reconnect, a reload — keeps its
        // `since`: it is the same device that has had the workspace all along.
        let kept_since = match &st.holder {
            Holder::Held { lease, .. } | Holder::Reserved { lease, .. }
                if lease.session == session =>
            {
                Some(lease.since_ms)
            }
            _ => None,
        };
        if let Some(from) = st.previous
            && from != session
        {
            for part in &self.workspace {
                part.moved(&from, &session);
            }
        }
        st.previous = Some(session);
        st.holder = Holder::Held {
            connection,
            lease: Lease {
                session,
                device,
                since_ms: kept_since.unwrap_or_else(|| self.now_ms()),
            },
        };
        st.generation += 1;
        // Before the snapshot: from here on a call from the old holder that
        // was admitted earlier and is still running is refused when it
        // tries to change a window (`admits_now`), so nothing lands that the
        // snapshot does not show.
        self.sync(st);
        // The snapshot before the seat event, so a shell that renders the
        // workspace on the seat event already has the windows to render.
        for part in &self.workspace {
            if let Some((topic, payload)) = part.snapshot() {
                self.hub.send_to(connection, topic, payload);
            }
        }
        self.broadcast(st);
        tracing::info!(%connection, "seat taken");
    }

    /// What the seat looks like to one connection, or to no connection
    /// (`None`: the CLI asking).
    pub fn status(&self, connection: Option<ConnectionId>) -> Status {
        let mut st = self.state();
        if self.expire_if_due(&mut st) {
            self.broadcast(&st);
        }
        Seat::status_in(&st, connection)
    }

    /// Whether a call on `connection` may run. A connection that holds the
    /// seat may; one that would get it without taking it from anyone (vacant,
    /// or reserved for its own session) gets it now and may. Anything else is
    /// refused with the status, so the refusal can say who has it.
    pub fn admit(&self, connection: ConnectionId) -> Result<(), Status> {
        let mut st = self.state();
        let expired = self.expire_if_due(&mut st);
        let status = Seat::status_in(&st, Some(connection));
        if status.held {
            return Ok(());
        }
        if status.claimable {
            self.acquire(&mut st, connection);
            return Ok(());
        }
        if expired {
            self.broadcast(&st);
        }
        Err(status)
    }

    /// Who may move the seat: a joined device. An attendant — an app's own
    /// socket, which names no device — may not, whatever it asks: the seat's
    /// commands are exempt from the gate so a device without the seat can
    /// reach them, and an app's socket is a plain browser connection to the
    /// bus (the hard-deny list only sees `as_app` calls), so this is the one
    /// place that keeps an app from taking the seat from the shell around it.
    fn mover(st: &State, connection: ConnectionId) -> Result<(), Refusal> {
        match st.peers.get(&connection) {
            None => Err(Refusal::NotJoined),
            Some(peer) if !peer.names_device => Err(Refusal::Attendant),
            Some(_) => Ok(()),
        }
    }

    /// Take the seat if it is free: vacant, or reserved for this connection's
    /// own session. Never takes it from another device. What the shell calls
    /// when it is told the seat is vacant, so two waiting devices cannot take
    /// it from each other in turn.
    pub fn claim(&self, connection: ConnectionId) -> Result<Status, Refusal> {
        let mut st = self.state();
        Seat::mover(&st, connection)?;
        self.expire_if_due(&mut st);
        let status = Seat::status_in(&st, Some(connection));
        if !status.held && status.claimable {
            self.acquire(&mut st, connection);
        }
        Ok(Seat::status_in(&st, Some(connection)))
    }

    /// Take the seat from whoever has it. `device`, when given, relabels this
    /// connection first, so the others are told the name the operator just
    /// chose.
    pub fn take(&self, connection: ConnectionId, device: Option<&str>) -> Result<Status, Refusal> {
        let mut st = self.state();
        Seat::mover(&st, connection)?;
        let Some(peer) = st.peers.get_mut(&connection) else {
            return Err(Refusal::NotJoined);
        };
        if let Some(device) = device {
            peer.device = sanitize_device(Some(device));
        }
        self.expire_if_due(&mut st);
        let held = matches!(st.holder, Holder::Held { connection: c, .. } if c == connection);
        if held {
            // Already seated; a relabel is still news to the others.
            if let (Some(device), Holder::Held { lease, .. }) = (device, &mut st.holder) {
                lease.device = sanitize_device(Some(device));
                self.broadcast(&st);
            }
        } else {
            self.acquire(&mut st, connection);
        }
        Ok(Seat::status_in(&st, Some(connection)))
    }

    /// Give the seat up. Vacant at once, with no grace: the holder said it
    /// was done. A connection that does not hold the seat changes nothing.
    pub fn release(&self, connection: ConnectionId) -> Result<Status, Refusal> {
        let mut st = self.state();
        Seat::mover(&st, connection)?;
        self.expire_if_due(&mut st);
        if matches!(st.holder, Holder::Held { connection: c, .. } if c == connection) {
            st.holder = Holder::Vacant;
            st.generation += 1;
            self.sync(&st);
            self.broadcast(&st);
            tracing::info!(%connection, "seat released");
        }
        Ok(Seat::status_in(&st, Some(connection)))
    }

    /// A browser session ended: signed out, revoked or expired. Its
    /// connections are forgotten at once — they are being closed, and must
    /// not be handed the seat on the way out — and a seat it held or had
    /// reserved is vacant now, with no grace: the grace is for a device that
    /// may come back, and an ended session cannot. Returns 1 when that freed
    /// the seat, for the reapers' count.
    ///
    /// Registered with `http::Ctx::on_session_end`, next to the terminals'
    /// reaper. A lockdown drains every session without that hook; it ends
    /// the reservation by the grace, which is shorter than any unlock.
    pub fn session_ended(&self, session: &SessionHash) -> usize {
        let mut st = self.state();
        st.peers.retain(|_, peer| peer.session != *session);
        let freed = match &st.holder {
            Holder::Held { lease, .. } | Holder::Reserved { lease, .. } => {
                lease.session == *session
            }
            Holder::Vacant => false,
        };
        if freed {
            st.holder = Holder::Vacant;
            st.generation += 1;
            tracing::info!("seat vacant: the session holding it ended");
        }
        self.sync(&st);
        if freed {
            self.broadcast(&st);
        }
        usize::from(freed)
    }

    /// The grace for one reservation is over.
    fn grace_over(&self, generation: u64) {
        let mut st = self.state();
        if st.generation == generation && matches!(st.holder, Holder::Reserved { .. }) {
            st.holder = Holder::Vacant;
            st.generation += 1;
            self.sync(&st);
            self.broadcast(&st);
            tracing::info!("seat vacant: the holder did not come back in time");
        }
    }

    fn arm_grace(&self, generation: u64) {
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            // No runtime, no timer: `expire_if_due` still ends the
            // reservation on the next thing anyone asks.
            return;
        };
        let seat = self.me.clone();
        let grace = self.grace;
        runtime.spawn(async move {
            tokio::time::sleep(grace).await;
            if let Some(seat) = seat.upgrade() {
                seat.grace_over(generation);
            }
        });
    }
}

impl ws::Presence for Seat {
    fn joined(&self, connection: ConnectionId, session: &SessionId, device: Option<&str>) {
        let mut st = self.state();
        let session = session.hash();
        st.peers.insert(
            connection,
            Peer {
                session,
                device: sanitize_device(device),
                names_device: device.is_some(),
            },
        );
        self.expire_if_due(&mut st);
        let mine = match &st.holder {
            Holder::Vacant => true,
            // The holder's own session: a reconnect inside the grace, a
            // reload whose old socket is not yet noticed gone, or a second
            // tab. See the module note. Never for an attendant, which rides
            // on that lease instead of taking it.
            Holder::Held { lease, .. } | Holder::Reserved { lease, .. } => {
                device.is_some() && lease.session == session
            }
        };
        if mine {
            self.acquire(&mut st, connection);
        } else {
            // An attendant of the holding session is admitted from here on.
            self.sync(&st);
            let status = Seat::status_in(&st, Some(connection));
            self.hub.send_to(connection, TOPIC, status.to_json());
        }
    }

    fn left(&self, connection: ConnectionId) {
        let mut st = self.state();
        st.peers.remove(&connection);
        let lease = match &st.holder {
            Holder::Held {
                connection: c,
                lease,
            } if *c == connection => lease.clone(),
            _ => {
                self.sync(&st);
                return;
            }
        };
        st.holder = Holder::Reserved {
            lease,
            until: self.clock.mono() + self.grace,
        };
        st.generation += 1;
        let generation = st.generation;
        self.sync(&st);
        self.broadcast(&st);
        drop(st);
        self.arm_grace(generation);
        tracing::info!(%connection, grace = ?self.grace, "seat holder left; reserved for its session");
    }
}

// ---------------------------------------------------------------------------
// the seat commands
// ---------------------------------------------------------------------------

/// The connection a seat command is about. Only a shell socket has one: the
/// CLI, an agent and the Inbox do not, and none of them can hold the seat.
fn connection_of(cx: &Context) -> Result<ConnectionId, CommandError> {
    cx.connection.map(ConnectionId).ok_or_else(|| {
        CommandError::Failed(anyhow::anyhow!(
            "the seat is held by a shell connection, and this call did not come over one"
        ))
    })
}

/// Why a connection may not move the seat.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum Refusal {
    #[error("this connection has not finished its handshake, so it cannot hold the seat")]
    NotJoined,
    #[error(
        "this connection names no device (an app's own socket), so it cannot take, claim or \
         release the seat; the shell around it does that"
    )]
    Attendant,
}

impl From<Refusal> for CommandError {
    fn from(refusal: Refusal) -> CommandError {
        CommandError::Failed(anyhow::Error::new(refusal))
    }
}

/// A change refused because its connection lost the seat after the call was
/// admitted ([`Seat::admits_now`]). A command returns it inside
/// [`CommandError::Failed`], and `wiring::BusDispatch` answers it with
/// [`NOT_HELD`] like a refusal at the gate, so the shell treats both alike.
#[derive(Debug, Clone, thiserror::Error)]
#[error("this connection no longer holds the seat; nothing was changed")]
pub struct LostSeat {
    pub status: Status,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct NoParams {}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct TakeParams {
    /// What to call this device on the others' takeover screen. Optional; the
    /// label the connection sent in its hello is used otherwise.
    #[serde(default)]
    pub device: Option<String>,
}

/// `seat.status`: who has the seat, and whether it is this connection.
pub struct SeatStatus {
    seat: Arc<Seat>,
}

impl Command for SeatStatus {
    const ID: &'static str = "seat.status";
    const TITLE: &'static str = "Who is using Deckjack";
    const RISK: RiskLevel = RiskLevel::ReadOnly;

    type Params = NoParams;
    type Output = Value;

    async fn run(&self, _p: NoParams, cx: Context) -> Result<Value, CommandError> {
        Ok(self.seat.status(cx.connection.map(ConnectionId)).to_json())
    }
}

/// `seat.take`: take the seat from whoever has it.
///
/// [`RiskLevel::Write`]: it changes which device is using the workspace and
/// nothing else, and any signed-in device may do it — that is the feature.
/// The shell asks "take over from <device>?" itself before calling, because
/// the question is about another device rather than about data.
pub struct SeatTake {
    seat: Arc<Seat>,
}

impl Command for SeatTake {
    const ID: &'static str = "seat.take";
    const TITLE: &'static str = "Use Deckjack on this device";
    const RISK: RiskLevel = RiskLevel::Write;

    type Params = TakeParams;
    type Output = Value;

    async fn run(&self, p: TakeParams, cx: Context) -> Result<Value, CommandError> {
        let connection = connection_of(&cx)?;
        Ok(self.seat.take(connection, p.device.as_deref())?.to_json())
    }
}

/// `seat.claim`: take the seat only if nobody else has it.
pub struct SeatClaim {
    seat: Arc<Seat>,
}

impl Command for SeatClaim {
    const ID: &'static str = "seat.claim";
    const TITLE: &'static str = "Use Deckjack here if it is free";
    const RISK: RiskLevel = RiskLevel::Write;

    type Params = NoParams;
    type Output = Value;

    async fn run(&self, _p: NoParams, cx: Context) -> Result<Value, CommandError> {
        let connection = connection_of(&cx)?;
        Ok(self.seat.claim(connection)?.to_json())
    }
}

/// `seat.release`: give the seat up, so the next device gets it at once.
pub struct SeatRelease {
    seat: Arc<Seat>,
}

impl Command for SeatRelease {
    const ID: &'static str = "seat.release";
    const TITLE: &'static str = "Stop using Deckjack on this device";
    const RISK: RiskLevel = RiskLevel::Write;

    type Params = NoParams;
    type Output = Value;

    async fn run(&self, _p: NoParams, cx: Context) -> Result<Value, CommandError> {
        let connection = connection_of(&cx)?;
        Ok(self.seat.release(connection)?.to_json())
    }
}

/// Put the seat commands on the bus.
pub fn register(registry: &Registry, seat: &Arc<Seat>) -> Result<(), RegistryError> {
    registry.register(SeatStatus {
        seat: Arc::clone(seat),
    })?;
    registry.register(SeatTake {
        seat: Arc::clone(seat),
    })?;
    registry.register(SeatClaim {
        seat: Arc::clone(seat),
    })?;
    registry.register(SeatRelease {
        seat: Arc::clone(seat),
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_label_is_trimmed_collapsed_stripped_and_capped() {
        assert_eq!(
            sanitize_device(Some("  Chrome   on\tPixel  ")),
            "Chrome on Pixel"
        );
        assert_eq!(
            sanitize_device(Some("evil\u{202E}lexip\u{0007}")),
            "evillexip"
        );
        assert_eq!(sanitize_device(Some("\u{200B}\n ")), UNNAMED_DEVICE);
        assert_eq!(sanitize_device(None), UNNAMED_DEVICE);
        let long = "x".repeat(DEVICE_MAX_CHARS + 10);
        assert_eq!(
            sanitize_device(Some(&long)).chars().count(),
            DEVICE_MAX_CHARS
        );
        // Cut on characters, not bytes: a multi-byte label is not split
        // inside a character.
        let wide = "é".repeat(DEVICE_MAX_CHARS + 1);
        assert_eq!(sanitize_device(Some(&wide)), "é".repeat(DEVICE_MAX_CHARS));
    }

    #[test]
    fn every_kind_of_format_character_is_stripped() {
        // One from each range of the list: bidi marks, the soft hyphen, the
        // Mongolian vowel separator, the annotation controls, the tags.
        for c in [
            '\u{061C}',
            '\u{00AD}',
            '\u{0600}',
            '\u{06DD}',
            '\u{070F}',
            '\u{0890}',
            '\u{08E2}',
            '\u{180E}',
            '\u{200E}',
            '\u{202D}',
            '\u{2062}',
            '\u{2067}',
            '\u{206F}',
            '\u{FEFF}',
            '\u{FFFA}',
            '\u{110BD}',
            '\u{110CD}',
            '\u{13435}',
            '\u{1BCA1}',
            '\u{1D175}',
            '\u{E0001}',
            '\u{E0041}',
        ] {
            assert_eq!(
                sanitize_device(Some(&format!("Pi{c}xel"))),
                "Pixel",
                "U+{:04X} survived",
                c as u32
            );
        }
        // Visible neighbors of the ranges are kept.
        assert_eq!(
            sanitize_device(Some("\u{00AC}\u{2065}\u{FFFC}")),
            "\u{00AC}\u{2065}\u{FFFC}"
        );
    }

    /// A clock a test moves by hand.
    struct Manual {
        at: Mutex<(std::time::SystemTime, Instant)>,
    }

    impl Manual {
        fn new() -> Arc<Manual> {
            Arc::new(Manual {
                at: Mutex::new((
                    UNIX_EPOCH + Duration::from_secs(1_800_000_000),
                    Instant::now(),
                )),
            })
        }

        fn advance(&self, by: Duration) {
            let mut at = self.at.lock().unwrap();
            at.0 += by;
            at.1 += by;
        }
    }

    impl ws::Clock for Manual {
        fn now(&self) -> std::time::SystemTime {
            self.at.lock().unwrap().0
        }
        fn mono(&self) -> Instant {
            self.at.lock().unwrap().1
        }
    }

    /// A seat with no runtime, so no grace timer: whatever ends a
    /// reservation here is the deadline check on the way in.
    fn seat(clock: &Arc<Manual>) -> Arc<Seat> {
        let hub = Hub::new(Arc::clone(clock) as Arc<dyn ws::Clock>);
        Seat::new(hub, Arc::clone(clock) as Arc<dyn ws::Clock>, Vec::new())
    }

    #[test]
    fn a_reservation_ends_at_its_deadline_without_a_timer() {
        use ws::Presence as _;
        let clock = Manual::new();
        let seat = seat(&clock);
        let session = SessionId::generate().unwrap();
        seat.joined(ConnectionId(1), &session, Some("phone"));
        seat.left(ConnectionId(1));

        clock.advance(GRACE - Duration::from_secs(1));
        let reserved = seat.status(None).holder.expect("still reserved");
        assert!(!reserved.connected);

        clock.advance(Duration::from_secs(2));
        assert_eq!(
            seat.status(None).holder,
            None,
            "the grace is over and the seat is vacant"
        );
    }

    #[test]
    fn the_same_session_coming_back_keeps_its_since() {
        use ws::Presence as _;
        let clock = Manual::new();
        let seat = seat(&clock);
        let session = SessionId::generate().unwrap();
        seat.joined(ConnectionId(1), &session, Some("phone"));
        let since = seat.status(None).holder.expect("held").since_ms;

        clock.advance(Duration::from_secs(10));
        seat.left(ConnectionId(1));
        clock.advance(Duration::from_secs(5));
        seat.joined(ConnectionId(2), &session, Some("phone"));
        let back = seat.status(Some(ConnectionId(2)));
        assert!(back.held);
        assert_eq!(back.holder.expect("held").since_ms, since);

        // Another session taking it starts a new `since`.
        let other = SessionId::generate().unwrap();
        seat.joined(ConnectionId(3), &other, Some("laptop"));
        seat.take(ConnectionId(3), None).unwrap();
        assert_eq!(
            seat.status(None).holder.expect("held").since_ms,
            since + 15_000
        );
    }

    #[test]
    fn only_the_seat_and_the_kill_switch_are_exempt() {
        assert!(exempt("seat.take"));
        assert!(exempt("seat.status"));
        assert!(exempt("system.lockdown"));
        assert!(!exempt("seats.take"));
        assert!(!exempt("system.status"));
        assert!(!exempt("terminal.open"));
        assert!(!exempt("shell.window.open"));
    }
}
