//! Where the modules meet.
//!
//! `ws` and `command` were built not to know about each other: `ws` declares
//! the narrowest trait it needs ([`ws::Dispatch`], one method) and `command`
//! knows nothing about sockets at all. That is the right shape — it is also
//! why something has to introduce them, and this is that something. Keeping
//! the introduction in one file means the coupling is visible rather than
//! smeared through both.
//!
//! It also registers the first real commands: the terminal.
//!
//! # Why terminal output is not progress
//!
//! The obvious wiring is to stream pty bytes as [`ws::Progress`] frames. It
//! is wrong. `ProgressSink::report` deliberately **drops** frames when the
//! client is behind, which is correct for a progress bar — a stale percentage
//! helps nobody — and corrupting for a terminal, where a dropped chunk is
//! missing output that never comes back.
//!
//! So output rides the reliable request path instead, with a cursor.
//! `terminal.read` takes the cursor the client last saw and returns what came
//! after it, awaiting new output rather than spinning. If a response is lost,
//! the client asks again from the same cursor and loses nothing. That is also
//! what makes reattach work after a browser reload: the cursor is the whole
//! state the client needs to hold.

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::command::{
    Admitted, Bus, Command, CommandError, Context, Enablement, Issuer, Registry, Request, RiskLevel,
};
use crate::terminal::{self, Opener, SessionSpec, TerminalId, WinSize};

/// `CommandError::Failed` wraps an `anyhow::Error`; this is the shorthand.
fn failed(message: impl Into<String>) -> CommandError {
    CommandError::Failed(anyhow::anyhow!(message.into()))
}
use crate::window;
use crate::ws;

// ---------------------------------------------------------------------------
// ws -> command
// ---------------------------------------------------------------------------

/// Carries WebSocket calls to the command bus.
///
/// It also holds the operator's confirmations: the bus says a call needs
/// one, this issues the ticket, and only a `confirm` frame presenting that
/// ticket on the same socket runs the call. See [`crate::ticket`].
///
/// And it holds the single seat's line (`crate::seat`): a call on a socket
/// that does not hold the seat is refused here, before the bus, with
/// [`crate::seat::NOT_HELD`]. Here because this is the one door a browser's
/// session reaches the bus through — the shell's own calls and its app
/// windows' (`as_app`) alike — and the paths that are not a browser using the
/// workspace (the CLI, agents, the Inbox, app backends) never pass it.
pub struct BusDispatch {
    bus: Arc<Bus>,
    tickets: Arc<crate::ticket::Tickets>,
    seat: Arc<crate::seat::Seat>,
}

impl BusDispatch {
    /// `tickets` is required, not defaulted: a dispatcher built without the
    /// table could not confirm anything, and one that built its own would
    /// not share the node's clock. `seat` likewise: the same seat the
    /// transport reports connections to, or nobody would ever hold it.
    pub fn new(
        bus: Arc<Bus>,
        tickets: Arc<crate::ticket::Tickets>,
        seat: Arc<crate::seat::Seat>,
    ) -> Arc<BusDispatch> {
        Arc::new(BusDispatch { bus, tickets, seat })
    }
}

/// The refusal for a call on a socket that does not hold the seat. Says who
/// does, and carries the seat's status as `detail` so the shell can draw its
/// takeover screen from the refusal alone.
fn seat_refusal(status: &crate::seat::Status) -> ws::CommandError {
    let message = match &status.holder {
        Some(holder) => format!(
            "Deckjack is in use on {}. Take over on this device to use it here; nothing was run",
            holder.device
        ),
        None => "this connection does not hold the seat; nothing was run".to_string(),
    };
    ws::CommandError::new(crate::seat::NOT_HELD, message).with_detail(status.to_json())
}

/// What the client is told about a command that ran and failed.
///
/// A change refused because the seat moved while the call ran
/// ([`crate::seat::LostSeat`]) gets the gate's answer, [`crate::seat::NOT_HELD`]
/// with the seat's status, so the shell shows the takeover screen rather than
/// a failure. Everything else is `failed`.
fn failure(e: crate::command::CommandError) -> ws::CommandError {
    if let crate::command::CommandError::Failed(inner) = &e
        && let Some(lost) = inner.downcast_ref::<crate::seat::LostSeat>()
    {
        return seat_refusal(&lost.status);
    }
    ws::CommandError::new("failed", e.to_string())
}

/// The longest ticket text looked up. A real one is 32 characters; anything
/// longer is refused without touching the table.
const TICKET_MAX_LEN: usize = 64;

/// Commands a client's cancel may stop by dropping their future.
///
/// Only work that is a pure wait with nothing behind it: dropping a future
/// does not stop a `spawn_blocking` task under it, so a file write or a
/// lockdown "cancelled" this way would still land, after the audit row and
/// the client had both been told it did not — and its blocking thread would
/// outlive the in-flight slot that was meant to bound it. `terminal.read`
/// only parks on the session's output notifier, so dropping it is complete.
const DROPPED_ON_CANCEL: &[&str] = &[<Read as Command>::ID];

fn drops_on_cancel(command: &crate::command::CommandId) -> bool {
    DROPPED_ON_CANCEL.contains(&command.as_str())
}

/// The bus issuer for a socket call: the operator at this browser, or, when
/// the frame names `as_app`, that app's window on this browser.
///
/// A name that could not be an app's is refused here with `not_permitted`
/// rather than handed to the bus, so the audit log and the journal never
/// carry a string a frame chose and no app could have.
fn issuer_for(call: &ws::Call) -> Result<Issuer, ws::CommandError> {
    let session = call.issuer.session().clone();
    match &call.as_app {
        None => Ok(Issuer::browser(session)),
        Some(app) if deckjack_protocol::valid_name(app) => Ok(Issuer::app_ui(app.clone(), session)),
        Some(_) => Err(ws::CommandError::new(
            NOT_PERMITTED,
            "as_app does not name an app: an app name is 1-32 characters of a-z, 0-9 and '-'",
        )),
    }
}

/// The socket's code for a call refused by an app window's scope. Its own
/// code, so the shell and the tests can tell "this window may not" from a
/// malformed call or any other refusal.
pub const NOT_PERMITTED: &str = "not_permitted";

/// The stable code a refusal goes out under.
fn refusal_code(refused: &crate::command::Refused) -> &'static str {
    match refused {
        crate::command::Refused::NotPermitted { .. } => NOT_PERMITTED,
        _ => "refused",
    }
}

impl ws::Dispatch for BusDispatch {
    fn dispatch(&self, call: ws::Call, progress: ws::ProgressSink) -> ws::Dispatched {
        let bus = Arc::clone(&self.bus);
        let tickets = Arc::clone(&self.tickets);
        let seat = Arc::clone(&self.seat);

        Box::pin(async move {
            let id = match crate::command::CommandId::new(call.command.to_string()) {
                Ok(id) => id,
                Err(e) => {
                    return ws::Outcome::error(ws::CommandError::new("bad_command", e.to_string()));
                }
            };

            // A socket authenticated by the session cookie is the operator at
            // their own browser. An agent reaches the bus through the MCP
            // path with its own session identity, never through here, so
            // there is no way for a browser to claim to be one.
            //
            // `as_app` is the one thing a frame may say about who is asking,
            // and it can only make the answer smaller: the call becomes that
            // app's window, held to the app's approved scope. The shell is
            // trusted to name the app truthfully (it is the trust root,
            // docs/APP-UI-ISOLATION.md §7.2); nothing it names here can reach
            // further than the operator already could. Decided before any
            // ticket is looked at, so a name no app could have never spends
            // one, and the issuer it yields is the one the ticket is bound to.
            let issuer = match issuer_for(&call) {
                Ok(issuer) => issuer,
                Err(e) => return ws::Outcome::error(e),
            };
            let connection = call.issuer.connection();

            // The seat, before any ticket is looked at, so a socket that may
            // not run anything spends nothing. An app's window is held to the
            // same line as the shell it runs in: it is that device using the
            // workspace. `seat.*` passes here and is refused to an app window
            // by the bus's hard-deny list instead.
            if !crate::seat::exempt(id.as_str())
                && let Err(status) = seat.admit(connection)
            {
                tracing::debug!(%connection, command = %id, "refused: the seat is held elsewhere");
                return ws::Outcome::error(seat_refusal(&status));
            }

            let digest = crate::ticket::params_digest(&call.params);
            let bind = |target: Option<&str>| {
                crate::ticket::Binding::new(connection, issuer.clone(), id.clone(), digest, target)
            };

            // The only way a browser's call is ever confirmed: a ticket this
            // dispatcher issued, for this socket, this issuer (the shell, or
            // one named app's window: a ticket issued to one is refused when
            // presented as another), this command, these exact parameters and
            // the target it showed, presented once within its minute.
            let redeeming = call.ticket.is_some();
            let spent = call.ticket.map(|ticket| {
                if ticket.len() > TICKET_MAX_LEN {
                    Err(crate::ticket::Refusal::Unknown)
                } else {
                    let now = bus.target(&id, &call.params);
                    tickets.redeem(&ticket, &bind(now.as_deref()))
                }
            });
            let mut request =
                Request::new(id.clone(), call.params, issuer.clone()).via_connection(connection.0);
            match spent {
                None => {}
                Some(Ok(())) => request = request.confirmed(),
                Some(Err(refusal)) => {
                    tracing::info!(%connection, command = %request.command, %refusal, "confirmation refused");
                    return ws::Outcome::error(ws::CommandError::new(
                        ws::CONFIRMATION_REFUSED,
                        refusal.to_string(),
                    ));
                }
            }

            let admitted = match bus.dispatch(request) {
                Ok(a) => a,
                Err(refused) => {
                    return ws::Outcome::error(ws::CommandError::new(
                        refusal_code(&refused),
                        refused.to_string(),
                    ));
                }
            };

            match admitted {
                Admitted::Running(running) => {
                    // The client's cancel, carried through — but only to a
                    // command whose work can be dropped mid-flight. `ws`
                    // flips this signal on a `cancel` frame and when the
                    // socket goes away; ignoring it left a cancelled
                    // `terminal.read` parked until the shell next printed
                    // (forever, for a quiet one), holding an in-flight slot.
                    // Every other command runs to its end: most do their
                    // work on the blocking pool, which dropping the future
                    // does not stop, so the change would land while the
                    // journal and the client were told it was cancelled.
                    let (mut rx, done) = if drops_on_cancel(running.command()) {
                        let cancel = call.cancel.clone();
                        running.split_until(async move { cancel.canceled().await })
                    } else {
                        running.split()
                    };

                    // Forward whatever the command reports while it runs.
                    // Lossy on purpose; see the module note.
                    let forward = tokio::spawn(async move {
                        while let Some(p) = rx.recv().await {
                            progress.report(translate_progress(p));
                        }
                    });

                    let completion = done.await;
                    forward.abort();

                    match completion.outcome {
                        Ok(value) => ws::Outcome::ok(value),
                        // Its own code: the client asked for this, and a
                        // panel that cancelled its own read must be able to
                        // tell that apart from the command failing.
                        Err(e @ crate::command::CommandError::Cancelled { .. }) => {
                            ws::Outcome::error(ws::CommandError::new("cancelled", e.to_string()))
                        }
                        Err(e) => ws::Outcome::error(failure(e)),
                    }
                }
                // Both of these are answers, not failures: the command has
                // not run and the shell has something to show the operator.
                Admitted::Queued { pending, risk, .. } => ws::Outcome::ok(serde_json::json!({
                    "status": "queued",
                    "pending": pending.to_string(),
                    "risk": risk.as_str(),
                })),
                // A redeemed ticket marks the request confirmed, so the bus
                // asking again would be a bug here or there. Issuing a second
                // ticket would ask the operator forever; refuse instead.
                Admitted::NeedsConfirmation { .. } if redeeming => {
                    ws::Outcome::error(ws::CommandError::new(
                        ws::CONFIRMATION_REFUSED,
                        "the node asked for a confirmation it had just been given; \
                         nothing was run. This is a fault on the node, not something \
                         to retry",
                    ))
                }
                // Not a failure: the operator has not been asked yet. The
                // prompt and target are the bus's, so the client shows that
                // text rather than inventing its own, then answers with a
                // `confirm` frame carrying the ticket.
                Admitted::NeedsConfirmation {
                    risk,
                    prompt,
                    target,
                    ..
                } => match tickets.issue(bind(target.as_deref())) {
                    Ok(ticket) => ws::Outcome::ok(serde_json::json!({
                        "status": "needs_confirmation",
                        "prompt": prompt,
                        "target": target,
                        "risk": risk.as_str(),
                        "ticket": ticket.reveal(),
                    })),
                    Err(e) => ws::Outcome::error(ws::CommandError::new(
                        "failed",
                        format!("could not issue a confirmation, so nothing was run: {e}"),
                    )),
                },
            }
        })
    }
}

fn translate_progress(p: crate::command::Progress) -> ws::Progress {
    use crate::command::Progress as P;
    match p {
        P::Step {
            fraction, message, ..
        } => ws::Progress {
            fraction,
            message: Some(message),
            log: Vec::new(),
        },
        P::Log { line, .. } => ws::Progress::line(line),
    }
}

// ---------------------------------------------------------------------------
// the terminal commands
// ---------------------------------------------------------------------------

/// Opening a shell is the most consequential thing on this bus: arbitrary
/// code as the host user, and the design calls the terminal "the documented
/// escape hatch".
///
/// It is nonetheless [`RiskLevel::Write`], not `Destructive`. Risk level
/// decides whether *the operator* is asked "are you sure" — and the design is
/// clear that my own actions run directly. Making it destructive meant a
/// confirmation every time a terminal opened, which is friction nothing asked
/// for.
///
/// What keeps an agent out is not this field. It is the **grant**: an agent
/// is simply never given `terminal.*`, and anything outside a grant is
/// refused rather than queued.
///
/// The design also gates a remote session with step-up sign-in. That is now
/// built — `passkey.rs` has `start_step_up`, `finish_step_up` and
/// `has_step_up`, and `passkeyhttp` serves the routes — but **nothing here
/// consults it**: this command does not ask whether the session stepped up,
/// so a stolen cookie still opens a shell. Saying "does not exist yet" was
/// true when it was written and is now the wrong excuse for the same gap.
/// Closing it means a check in `Open::run` against the session's step-up
/// state, which needs the session id on `Context` and a handle to the
/// passkey registry.
/// Which terminals this caller owns.
///
/// The same rule [`Open::run`] assigns ownership by, written once so the two
/// cannot drift apart: a pty belongs to the browser session that asked for
/// it, and a dispatch with no browser behind it owns the host's own
/// terminals.
fn opener_of(cx: &Context) -> Opener {
    match cx.browser_session() {
        Some(browser) => Opener::Client(browser.reveal().hash()),
        None => Opener::Host,
    }
}

/// Attach to a terminal this caller owns.
///
/// Every one of `terminal.read`, `write`, `resize` and `close` used to take
/// `_cx` and throw it away, resolving the id through
/// [`terminal::Registry::attach`], which looks a terminal up by id and
/// nothing else. Ownership was already recorded — `close_for_session` has
/// always keyed on it — just never checked, so any signed-in session that
/// learned an id could type into another session's shell. `terminal.list`
/// handed out every id, so learning one took one read-only call.
///
/// A terminal the caller does not own is reported as **not existing**, with
/// the same error a genuinely unknown id gets. Saying "not yours" instead
/// would turn this into an oracle: a caller could sweep ids and learn
/// exactly which ones are live on the node, which is most of what it wanted
/// the list for.
fn attach_owned(
    terminals: &terminal::Registry,
    id: &str,
    cx: &Context,
) -> Result<Arc<terminal::TerminalSession>, CommandError> {
    let parsed =
        TerminalId::parse(id).ok_or_else(|| failed(format!("{id} is not a terminal id")))?;
    let unknown = || failed(format!("there is no terminal {id}"));
    let session = terminals.attach(&parsed).map_err(|_| unknown())?;
    if session.opened_by() != opener_of(cx) {
        return Err(unknown());
    }
    Ok(session)
}

pub struct Open {
    terminals: Arc<terminal::Registry>,
    cwd: std::path::PathBuf,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct OpenParams {
    /// Rows and columns the browser is showing. The pty is sized to this so
    /// the first frame is not reflowed.
    #[serde(default = "default_rows")]
    pub rows: u16,
    #[serde(default = "default_cols")]
    pub cols: u16,
}

fn default_rows() -> u16 {
    24
}
fn default_cols() -> u16 {
    80
}

#[derive(Debug, Serialize)]
pub struct Opened {
    pub id: String,
    pub rows: u16,
    pub cols: u16,
    /// Where to start reading. Zero for a new terminal.
    pub cursor: u64,
}

impl Command for Open {
    const ID: &'static str = "terminal.open";
    const TITLE: &'static str = "Open terminal";
    const RISK: RiskLevel = RiskLevel::Write;

    type Params = OpenParams;
    type Output = Opened;

    async fn run(&self, params: OpenParams, cx: Context) -> Result<Opened, CommandError> {
        // A pty belongs to the browser session that asked for it, so that
        // ending that session can reap it. `Opener::Host` is for terminals
        // the server opened on its own behalf, which nothing revokes.
        let opened_by = match cx.browser_session() {
            Some(browser) => Opener::Client(browser.reveal().hash()),
            None => Opener::Host,
        };
        let spec = SessionSpec::login_shell(self.cwd.clone())
            .with_size(WinSize::new(params.rows, params.cols))
            .opened_by(opened_by);

        let session = self
            .terminals
            .open(spec)
            .map_err(|e| failed(e.to_string()))?;

        let size = session.size();
        Ok(Opened {
            id: session.id().as_str().to_string(),
            rows: size.rows,
            cols: size.cols,
            cursor: 0,
        })
    }
}

/// Read output after a cursor, waiting for more rather than returning empty.
///
/// This is a long poll: the client calls it, it parks until the shell says
/// something, and the client immediately calls again with the new cursor. No
/// busy loop, no dropped bytes.
pub struct Read {
    terminals: Arc<terminal::Registry>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ReadParams {
    pub id: String,
    /// The cursor from the last read, or from `terminal.open`.
    #[serde(default)]
    pub cursor: u64,
}

#[derive(Debug, Serialize)]
pub struct Chunk {
    /// Bytes as UTF-8, lossy. A pty emits whatever the program writes, which
    /// is not guaranteed to be valid UTF-8 mid-sequence; the terminal
    /// emulator in the browser handles the replacement characters better than
    /// a failed response would.
    pub data: String,
    pub cursor: u64,
    pub exited: bool,
    /// The read began before the oldest byte the scrollback still holds, so
    /// `data` starts somewhere in the middle of the stream — possibly inside
    /// an escape sequence. The client clears its emulator before writing it.
    /// A reattach from cursor zero after a busy shell is the usual case.
    pub reset: bool,
}

impl Command for Read {
    const ID: &'static str = "terminal.read";
    const TITLE: &'static str = "Read terminal output";
    const RISK: RiskLevel = RiskLevel::ReadOnly;

    type Params = ReadParams;
    type Output = Chunk;

    async fn run(&self, params: ReadParams, cx: Context) -> Result<Chunk, CommandError> {
        let session = attach_owned(&self.terminals, &params.id, &cx)?;
        let out = session.output_after(params.cursor).await;
        // Ownership again, after the wait. The check above was at the start
        // of a long poll, and the terminal can change owner while it is
        // parked (the single seat moving to another session hands it over),
        // so the output that wakes it may be the new owner's. A read parked
        // by the previous owner is answered as if the terminal were gone,
        // exactly as a fresh one from that session would be.
        if session.opened_by() != opener_of(&cx) {
            return Err(failed(format!("there is no terminal {}", params.id)));
        }
        // Sample the exit status BEFORE reading it off the session again. A
        // shell that exits between `output_after` returning and a re-sample
        // would otherwise be reported as exited on a chunk that does not
        // carry its final bytes, and the panel stops polling on `exited` —
        // so the last line a shell ever printed would never be shown.
        let exited = session.exit().is_some() && out.bytes.is_empty();
        Ok(Chunk {
            data: String::from_utf8_lossy(&out.bytes).into_owned(),
            cursor: out.cursor,
            exited,
            reset: out.skipped > 0,
        })
    }
}

pub struct Write {
    terminals: Arc<terminal::Registry>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct WriteParams {
    pub id: String,
    /// Keystrokes, as the browser sends them.
    pub data: String,
}

impl Command for Write {
    const ID: &'static str = "terminal.write";
    const TITLE: &'static str = "Send to terminal";
    // Called on every keystroke. Anything above Write would ask the
    // operator to confirm each keypress.
    const RISK: RiskLevel = RiskLevel::Write;
    /// Keystrokes go to a shell. Recording them would put whatever is typed
    /// at a password prompt into the audit log.
    const REDACT_PARAMS: bool = true;

    type Params = WriteParams;
    type Output = ();

    async fn run(&self, params: WriteParams, cx: Context) -> Result<(), CommandError> {
        let session = attach_owned(&self.terminals, &params.id, &cx)?;
        session
            .write(params.data.as_bytes())
            .map_err(|e| failed(e.to_string()))
    }
}

pub struct Resize {
    terminals: Arc<terminal::Registry>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ResizeParams {
    pub id: String,
    pub rows: u16,
    pub cols: u16,
}

impl Command for Resize {
    const ID: &'static str = "terminal.resize";
    const TITLE: &'static str = "Resize terminal";
    const RISK: RiskLevel = RiskLevel::Write;

    type Params = ResizeParams;
    type Output = ();

    async fn run(&self, params: ResizeParams, cx: Context) -> Result<(), CommandError> {
        let session = attach_owned(&self.terminals, &params.id, &cx)?;
        session
            .resize(WinSize::new(params.rows, params.cols))
            .map_err(|e| failed(e.to_string()))
    }
}

pub struct Close {
    terminals: Arc<terminal::Registry>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct CloseParams {
    pub id: String,
}

impl Command for Close {
    const ID: &'static str = "terminal.close";
    const TITLE: &'static str = "Close terminal";
    const RISK: RiskLevel = RiskLevel::Write;

    type Params = CloseParams;
    type Output = ();

    async fn run(&self, params: CloseParams, cx: Context) -> Result<(), CommandError> {
        // Resolved through the ownership check first, so one session cannot
        // kill another's shell by id — which was the loudest version of this
        // hole, because a close is not recoverable.
        let session = attach_owned(&self.terminals, &params.id, &cx)?;
        self.terminals
            .close(session.id())
            .map_err(|e| failed(e.to_string()))
    }
}

pub struct List {
    terminals: Arc<terminal::Registry>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct NoParams {}

impl Command for List {
    const ID: &'static str = "terminal.list";
    const TITLE: &'static str = "List terminals";
    const RISK: RiskLevel = RiskLevel::ReadOnly;

    type Params = NoParams;
    type Output = Value;

    fn enablement(&self) -> Enablement {
        Enablement::Enabled
    }

    async fn run(&self, _p: NoParams, cx: Context) -> Result<Value, CommandError> {
        // The caller's own terminals, not the node's. An `Info` carries the
        // `TerminalId`, and that id is the whole handle to a shell, so the
        // unfiltered list was itself the disclosure.
        let list = self.terminals.list_for(&opener_of(&cx));
        serde_json::to_value(&list).map_err(|e| failed(e.to_string()))
    }
}

/// How often expired browser sessions are looked for, so their shells are
/// reaped on time rather than at the next sign-in.
pub const SESSION_SWEEP: std::time::Duration = std::time::Duration::from_secs(5);

/// How often exited terminals past their grace are let go when nobody is
/// opening or listing terminals to do it in passing.
pub const TERMINAL_SWEEP: std::time::Duration = std::time::Duration::from_secs(30);

/// Revoke every expired browser session the way any other revocation is
/// handled: row deleted, the session's terminals closed, an audit row
/// written. Returns how many expired.
///
/// The sign-in paths already expire sessions as they go, but a node nobody
/// signs in to would otherwise keep an expired session's shells running for
/// as long as nobody came back — reachable by no one, since the cookie that
/// owned them no longer verifies and a new sign-in is a new owner.
pub fn expire_sessions(ctx: &crate::http::Ctx) -> usize {
    let mut events = Vec::new();
    // The guard is dropped before `apply`, which takes the store and runs the
    // reapers; neither belongs under the auth lock.
    let expired = ctx
        .state()
        .expire(std::time::SystemTime::now(), &mut events);
    if expired > 0 {
        ctx.apply(events);
    }
    expired
}

/// Run [`expire_sessions`] every `every`, for as long as `ctx` lives.
///
/// Holds a `Weak`, so a dropped context (a test's, or a server shutting
/// down) ends the loop. Outside a Tokio runtime there is nothing to spawn on
/// and nothing to do.
pub fn spawn_session_expiry(ctx: &Arc<crate::http::Ctx>, every: std::time::Duration) {
    let Ok(runtime) = tokio::runtime::Handle::try_current() else {
        return;
    };
    let weak = Arc::downgrade(ctx);
    runtime.spawn(async move {
        let mut tick = tokio::time::interval(every);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tick.tick().await;
            let Some(ctx) = weak.upgrade() else { break };
            // A store write and pty hangups: blocking work, kept off the
            // runtime's workers.
            if tokio::task::spawn_blocking(move || expire_sessions(&ctx))
                .await
                .is_err()
            {
                break;
            }
        }
    });
}

/// Run [`terminal::Registry::reap_exited`] every `every`, for as long as the
/// registry lives. Opening and listing terminals reap in passing; this is for
/// the node nobody is using, whose exited shells would otherwise keep their
/// pty masters until someone came back.
fn spawn_terminal_reaper(terminals: &Arc<terminal::Registry>, every: std::time::Duration) {
    let Ok(runtime) = tokio::runtime::Handle::try_current() else {
        return;
    };
    let weak = Arc::downgrade(terminals);
    runtime.spawn(async move {
        let mut tick = tokio::time::interval(every);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tick.tick().await;
            let Some(terminals) = weak.upgrade() else {
                break;
            };
            if tokio::task::spawn_blocking(move || terminals.reap_exited())
                .await
                .is_err()
            {
                break;
            }
        }
    });
}

/// Put the terminal commands on the bus.
///
/// Returns the terminal registry so the caller can close sessions when a
/// browser session ends or a lockdown fires — a pty outliving the session
/// that opened it is a shell nobody is watching.
pub fn register_terminal(
    registry: &Registry,
    cwd: std::path::PathBuf,
) -> Result<Arc<terminal::Registry>, crate::command::RegistryError> {
    register_terminal_with(
        registry,
        cwd,
        terminal::Registry::new(
            Arc::new(terminal::OsEnv),
            Arc::new(terminal::NativeBackend),
            terminal::Limits::default(),
        ),
        TERMINAL_SWEEP,
    )
}

/// [`register_terminal`] around a terminal registry the caller built, with
/// exited terminals swept every `sweep`. The server uses the host's ptys,
/// the wall clock and [`TERMINAL_SWEEP`]; a test passes its own.
pub fn register_terminal_with(
    registry: &Registry,
    cwd: std::path::PathBuf,
    terminals: terminal::Registry,
    sweep: std::time::Duration,
) -> Result<Arc<terminal::Registry>, crate::command::RegistryError> {
    let terminals = Arc::new(terminals);
    spawn_terminal_reaper(&terminals, sweep);

    registry.register(Open {
        terminals: Arc::clone(&terminals),
        cwd,
    })?;
    registry.register(Read {
        terminals: Arc::clone(&terminals),
    })?;
    registry.register(Write {
        terminals: Arc::clone(&terminals),
    })?;
    registry.register(Resize {
        terminals: Arc::clone(&terminals),
    })?;
    registry.register(Close {
        terminals: Arc::clone(&terminals),
    })?;
    registry.register(List {
        terminals: Arc::clone(&terminals),
    })?;

    Ok(terminals)
}

// ---------------------------------------------------------------------------
// the window commands
// ---------------------------------------------------------------------------

/// Server-side window state, behind the `shell.window.*` commands.
///
/// The design is explicit that windows are server state: "each window is an
/// app instance held by the server, so every device sees the same open
/// windows." The browser holds a projection, not the truth.
///
/// # The id bridge
///
/// The browser mints its own id when it opens a window, optimistically, so
/// the window appears before any round trip. [`window::WindowSet`] mints its
/// own too. Neither is wrong and they do not agree, so this keeps a map from
/// the client's string to the server's [`window::WindowId`]. Every other
/// command looks up through it.
///
/// This is the seam where the two halves were written apart, and it is worth
/// naming rather than hiding: the honest end state is one id, minted by the
/// server and adopted by the client on the response. The server now pushes a
/// `windows` event on every change, and hands a snapshot to the connection
/// that takes the single seat (`crate::seat`), both keyed by the client's id
/// — so the map stays, and is what lets the device that takes over adopt
/// the windows another device opened under the ids it chose.
///
/// # What is pushed
///
/// The `windows` event's payload is the whole list, in the shape
/// `web/src/protocol.ts` calls `WireWindow`: the client's id, the app, the
/// title and the desktop placement. The whole list rather than a delta, so a
/// connection that missed one (events are droppable) is corrected by the
/// next.
pub struct Windows {
    set: std::sync::Mutex<window::WindowSet>,
    ids: std::sync::Mutex<std::collections::HashMap<String, window::WindowId>>,
    /// Which app each open window belongs to, so closing one can tell the app
    /// host. The window set knows, but `do_close` removes the window before
    /// it could ask.
    apps_by_window: std::sync::Mutex<std::collections::HashMap<String, String>>,
    clock: Arc<dyn window::Clock + Send + Sync>,
    node_id: String,
    /// The app host, once there is one.
    ///
    /// Optional because a window is not only ever an app: the terminal and
    /// the shell's own panels are windows too, and this module is used in
    /// tests that have no install directory. When it is absent, opening a
    /// window for an app simply does not start anything — which is what the
    /// server did for as long as the app host was never constructed.
    host: std::sync::OnceLock<Arc<crate::apphost::AppHost>>,
    /// Where changes are pushed. Attached by the seat, which is handed these
    /// windows as part of the workspace (`seat::Workspace::attach`); until
    /// then a change is simply not announced.
    hub: std::sync::OnceLock<Arc<ws::Hub>>,
    /// The single seat, attached with the hub. Asked, at the moment a window
    /// command changes anything, whether the connection it came from still
    /// holds the seat ([`crate::seat::Seat::admits_now`]).
    seat: std::sync::OnceLock<std::sync::Weak<crate::seat::Seat>>,
    /// Held across a window command's check, change and announcement, and
    /// across taking the snapshot handed to a connection that acquires the
    /// seat. With the seat's own bookkeeping updated before the snapshot is
    /// taken, a change either lands before the snapshot (and is in it, and
    /// was pushed before the new holder held the seat) or is refused — never
    /// after the snapshot, unseen by the device that took over.
    op: std::sync::Mutex<()>,
    /// Streamed windows (`crate::stream`), by client id, with what the
    /// shell needs to draw one: its app, the picture's size and, for a
    /// popup, its parent and offset. Carried in the list as `stream`.
    streamed: std::sync::Mutex<std::collections::HashMap<String, Value>>,
    /// Where a close of a streamed window goes: to the app, which may ask
    /// to save first. The window goes when the app unmaps it.
    stream_close: std::sync::OnceLock<Arc<dyn crate::stream::StreamClose>>,
}

/// The shell's title bar, in CSS pixels, so a streamed window opened by the
/// server is tall enough for its picture. Advisory, as every server-side
/// frame is: the shell sizes it for real.
const STREAM_TITLEBAR: u32 = 34;

/// The topic window changes and snapshots go out under. The shell's bus
/// already maps it onto its window store (`web/src/stores/bus.ts`).
pub const WINDOWS_TOPIC: &str = "windows";

impl Windows {
    pub fn new(node_id: String) -> Arc<Windows> {
        Arc::new(Windows {
            set: std::sync::Mutex::new(window::WindowSet::new()),
            ids: std::sync::Mutex::new(std::collections::HashMap::new()),
            apps_by_window: std::sync::Mutex::new(std::collections::HashMap::new()),
            clock: Arc::new(window::SystemClock),
            node_id,
            host: std::sync::OnceLock::new(),
            hub: std::sync::OnceLock::new(),
            seat: std::sync::OnceLock::new(),
            op: std::sync::Mutex::new(()),
            streamed: std::sync::Mutex::new(std::collections::HashMap::new()),
            stream_close: std::sync::OnceLock::new(),
        })
    }

    /// Give the window commands the stream registry, so closing a streamed
    /// window asks its app. Set once.
    pub fn attach_streams(&self, streams: Arc<dyn crate::stream::StreamClose>) {
        let _ = self.stream_close.set(streams);
    }

    fn streamed(&self) -> std::sync::MutexGuard<'_, std::collections::HashMap<String, Value>> {
        self.streamed
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Every open window, as the shell's `WireWindow`, back to front.
    ///
    /// Windows the client never named (none today: every window is opened by
    /// `shell.window.open`, which names one) are left out, because the shell
    /// could not address them.
    pub fn wire(&self) -> Value {
        let ids: Vec<(String, window::WindowId)> = self
            .ids
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .map(|(client, server)| (client.clone(), *server))
            .collect();
        let streamed = self.streamed().clone();
        let set = self.set();
        let stacking = set.desktop().stacking();
        let mut rows: Vec<(usize, Value)> = ids
            .into_iter()
            .filter_map(|(client, server)| {
                let w = set.window(server)?;
                let place = set.desktop().placement(server)?;
                let z = stacking.iter().position(|s| *s == server).unwrap_or(0);
                let mut row = serde_json::json!({
                        "id": client,
                        "app": w.app.app,
                        "title": w.title,
                        "minWidth": w.min_width,
                        "minHeight": 0,
                        "desktop": {
                            "x": place.frame.x,
                            "y": place.frame.y,
                            "w": place.frame.width,
                            "h": place.frame.height,
                            "z": z,
                            "minimized": place.minimized,
                            "maximized": place.snap == window::Snap::Maximized,
                        },
                });
                if let Some(meta) = streamed.get(&client) {
                    row["stream"] = meta.clone();
                }
                Some((z, row))
            })
            .collect();
        rows.sort_by_key(|(z, _)| *z);
        Value::Array(rows.into_iter().map(|(_, row)| row).collect())
    }

    /// Run one window command: check the seat, change, announce, all under
    /// [`Windows::op`]. `connection` is the shell socket the call came over,
    /// `None` for the CLI and agents, which the seat does not apply to.
    fn run_change(
        &self,
        connection: Option<u64>,
        change: impl FnOnce(&Windows) -> Result<Value, CommandError>,
    ) -> Result<Value, CommandError> {
        let seat = self.seat.get().and_then(std::sync::Weak::upgrade);
        {
            let _op = self
                .op
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let seated = match (&seat, connection) {
                (Some(seat), Some(c)) => seat.admits_now(ws::ConnectionId(c)),
                _ => true,
            };
            if seated {
                let out = change(self);
                // Every window command changes something, and a refused one
                // changed nothing, so this is the one place to announce.
                if out.is_ok() {
                    self.changed();
                }
                return out;
            }
        }
        // Refused, and the status asked for only after `op` is let go: the
        // seat takes its own lock first and `op` second, so asking it under
        // `op` would take them in the other order.
        let status = match (seat, connection) {
            (Some(seat), Some(c)) => seat.status(Some(ws::ConnectionId(c))),
            _ => unreachable!("only a seated check refuses"),
        };
        Err(CommandError::Failed(anyhow::Error::new(
            crate::seat::LostSeat { status },
        )))
    }

    /// Tell every open shell the windows changed.
    fn changed(&self) {
        if let Some(hub) = self.hub.get() {
            hub.publish(WINDOWS_TOPIC, self.wire());
        }
    }

    /// Give the window commands the app host.
    ///
    /// Late-bound rather than a constructor argument because the app host
    /// needs the command registry and the window commands are registered on
    /// it: one of the two has to be attached after the other exists. Set
    /// once; a second call is ignored.
    pub fn attach_app_host(&self, host: Arc<crate::apphost::AppHost>) {
        let _ = self.host.set(host);
    }

    /// Whether this window's app is one the app host knows about, and if so
    /// make sure it is running. An app that is not installed is not an error
    /// here: `shell.terminal`, `shell.settings` and the rest are windows with
    /// no binary behind them.
    fn starting(&self, app: &str) -> Option<crate::apps::WindowStatus> {
        let host = self.host.get()?;
        if !host.installed().iter().any(|a| a.name == app) {
            return None;
        }
        Some(host.open_window(app))
    }

    fn resolve(&self, client_id: &str) -> Result<window::WindowId, CommandError> {
        self.ids
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(client_id)
            .copied()
            .ok_or_else(|| failed(format!("no window {client_id}")))
    }

    fn set(&self) -> std::sync::MutexGuard<'_, window::WindowSet> {
        self.set
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    // Each of these takes the lock, does its work and drops the guard before
    // returning. That is not a style choice: a `MutexGuard` alive inside an
    // `async fn` body makes the whole future non-Send, and the bus requires
    // Send futures. Keeping the lock inside a plain method is what keeps the
    // two compatible.

    fn do_open(&self, p: OpenWindow) -> Result<Value, CommandError> {
        // Before the window exists, so a window is never shown for an app
        // that could not be launched at all.
        let status = self.starting(&p.app);
        if status == Some(crate::apps::WindowStatus::Unavailable) {
            // An app awaiting the operator's approval is the common case, and
            // the one the operator can fix from here, so it says how instead
            // of pointing at a log.
            if let Some(host) = self.host.get()
                && let Some((Ok(state), asks)) = host.approval(&p.app)
                && state.awaits_operator()
            {
                let title = host
                    .installed()
                    .into_iter()
                    .find(|a| a.name == p.app)
                    .and_then(|a| a.title)
                    .unwrap_or_else(|| p.app.clone());
                let why = if state == crate::apps::ApprovalState::Stale {
                    "its approval no longer covers what it asks for"
                } else {
                    "it has not been approved"
                };
                return Err(failed(format!(
                    "{} cannot run yet: {why}. It asks for {asks}. {}",
                    p.app,
                    crate::install::how_to_approve(&title)
                )));
            }
            // A node that launches no app at all says why in words: the
            // operator can do nothing from a log line they cannot see.
            if let Some(why) = self.host.get().and_then(|host| host.launch_refusal()) {
                return Err(failed(format!("{} cannot run: {why}", p.app)));
            }
            return Err(failed(format!(
                "{} is installed but cannot run on this node; see the log for why",
                p.app
            )));
        }

        let spec = window::OpenSpec {
            app: window::AppInstance::new(p.app.clone(), p.id.clone()),
            title: p.title.unwrap_or_else(|| p.app.clone()),
            node_id: self.node_id.clone(),
            min_width: 0,
            size: window::Size {
                width: p.width.unwrap_or(880),
                height: p.height.unwrap_or(560),
            },
        };
        let id = self
            .set()
            .open(spec, assumed_work_area(), self.clock.as_ref());
        self.ids
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(p.id.clone(), id);
        // Where the shell put it, when it said: the cascade above is the
        // server's own guess, and the device that takes the seat next should
        // see the window where the operator had it.
        if let (Some(x), Some(y), Some(width), Some(height)) = (p.x, p.y, p.width, p.height) {
            let frame = window::Rect::new(x, y, width.max(1), height.max(1));
            let _ = self.set().place(id, frame, assumed_work_area());
        }
        if status.is_some() {
            self.apps_by_window
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .insert(p.id.clone(), p.app.clone());
        }
        Ok(serde_json::json!({
            "id": p.id,
            "server_id": id.as_u64(),
            // `reconnecting` when the app is restarting: the panel shows that
            // rather than a blank frame, which is the whole reason
            // `HostEvent::Reconnecting` exists.
            "app_status": status.map(|s| match s {
                crate::apps::WindowStatus::Live => "live",
                crate::apps::WindowStatus::Reconnecting => "reconnecting",
                crate::apps::WindowStatus::Unavailable => "unavailable",
            }),
        }))
    }

    fn do_close(&self, p: WindowRef) -> Result<Value, CommandError> {
        let id = self.resolve(&p.id)?;
        // A streamed window is the app's: closing it asks the app, which may
        // ask to save first, and the window goes when the app unmaps it.
        // Only a window whose stream is gone (the app died without saying)
        // is removed here.
        if self.streamed().contains_key(&p.id) {
            if self
                .stream_close
                .get()
                .is_some_and(|streams| streams.close_requested(&p.id))
            {
                return Ok(Value::Null);
            }
            self.streamed().remove(&p.id);
        }
        self.set().close(id).map_err(|e| failed(e.to_string()))?;
        self.ids
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&p.id);
        // The app host counts windows, and an on-demand app with none is what
        // the idle sweep stops. Without this an app stays warm forever after
        // its last window closed.
        let app = self
            .apps_by_window
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&p.id);
        if let (Some(app), Some(host)) = (app, self.host.get()) {
            host.close_window(&app);
        }
        Ok(Value::Null)
    }

    fn do_minimize(&self, p: WindowRef) -> Result<Value, CommandError> {
        let id = self.resolve(&p.id)?;
        self.set().minimize(id).map_err(|e| failed(e.to_string()))?;
        Ok(Value::Null)
    }

    fn do_restore(&self, p: WindowRef) -> Result<Value, CommandError> {
        let id = self.resolve(&p.id)?;
        self.set()
            .unminimize(id)
            .map_err(|e| failed(e.to_string()))?;
        Ok(Value::Null)
    }

    fn do_maximize(&self, p: Maximize) -> Result<Value, CommandError> {
        let id = self.resolve(&p.id)?;
        let zone = if p.maximized {
            window::Snap::Maximized
        } else {
            window::Snap::Floating
        };
        self.set()
            .snap(id, zone, assumed_work_area())
            .map_err(|e| failed(e.to_string()))?;
        Ok(Value::Null)
    }

    fn do_place(&self, p: Place) -> Result<Value, CommandError> {
        let id = self.resolve(&p.id)?;
        let frame = window::Rect::new(p.x, p.y, p.width.max(1), p.height.max(1));
        self.set()
            .place(id, frame, assumed_work_area())
            .map_err(|e| failed(e.to_string()))?;
        Ok(Value::Null)
    }

    fn do_snap(&self, p: SnapTo) -> Result<Value, CommandError> {
        let id = self.resolve(&p.id)?;
        let zone = window::Snap::parse(&p.zone)
            .ok_or_else(|| failed(format!("{} is not a snap zone", p.zone)))?;
        self.set()
            .snap(id, zone, assumed_work_area())
            .map_err(|e| failed(e.to_string()))?;
        Ok(Value::Null)
    }

    fn do_tile(&self) -> Result<Value, CommandError> {
        self.set().tile(assumed_work_area());
        Ok(Value::Null)
    }

    fn do_split(&self, p: SetSplit) -> Result<Value, CommandError> {
        let left = self.resolve(&p.left)?;
        let right = self.resolve(&p.right)?;
        self.set()
            .split(left, right, window::Divider::new(0.5))
            .map_err(|e| failed(e.to_string()))?;
        Ok(Value::Null)
    }

    fn do_unsplit(&self) -> Result<Value, CommandError> {
        self.set().unsplit();
        Ok(Value::Null)
    }
}

/// The desktop the server lays windows out in when the client does not say.
///
/// A placeholder: the browser knows its own viewport and the server does not,
/// so until the client sends it, server-side geometry is advisory. It is
/// still recorded, because the layout has to survive a reload even if it is
/// later corrected by the device that can actually measure itself.
fn assumed_work_area() -> window::Rect {
    window::Rect::new(0, 0, 1920, 1080)
}

macro_rules! window_command {
    ($name:ident, $id:literal, $title:literal, $risk:expr, $params:ty, $body:expr) => {
        pub struct $name {
            windows: Arc<Windows>,
        }

        impl Command for $name {
            const ID: &'static str = $id;
            const TITLE: &'static str = $title;
            const RISK: RiskLevel = $risk;
            type Params = $params;
            type Output = Value;

            async fn run(&self, params: $params, cx: Context) -> Result<Value, CommandError> {
                let f: fn(&Windows, $params) -> Result<Value, CommandError> = $body;
                self.windows
                    .run_change(cx.connection, move |w| f(w, params))
            }
        }
    };
}

/// What the shell sends to open a window. The shell sends its rect as
/// `x`, `y`, `w`, `h` (`web/src/stores/windows.ts`); `w` and `h` were never
/// read under the names this declared, so every window was recorded at the
/// default size. Both spellings are accepted.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct OpenWindow {
    pub id: String,
    pub app: String,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub x: Option<i32>,
    #[serde(default)]
    pub y: Option<i32>,
    #[serde(default, alias = "w")]
    pub width: Option<u32>,
    #[serde(default, alias = "h")]
    pub height: Option<u32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct WindowRef {
    pub id: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct Maximize {
    pub id: String,
    #[serde(default)]
    pub maximized: bool,
}

/// A drag's end. `w`/`h` accepted for the reason [`OpenWindow`] gives:
/// without them every drag the shell recorded was a 1x1 frame.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct Place {
    pub id: String,
    #[serde(default)]
    pub x: i32,
    #[serde(default)]
    pub y: i32,
    #[serde(default, alias = "w")]
    pub width: u32,
    #[serde(default, alias = "h")]
    pub height: u32,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SnapTo {
    pub id: String,
    pub zone: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct Tile {
    #[serde(default)]
    pub ids: Vec<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SetSplit {
    pub left: String,
    pub right: String,
}

window_command!(
    WindowOpen,
    "shell.window.open",
    "Open window",
    RiskLevel::Write,
    OpenWindow,
    |w, p| w.do_open(p)
);

window_command!(
    WindowClose,
    "shell.window.close",
    "Close window",
    RiskLevel::Write,
    WindowRef,
    |w, p| w.do_close(p)
);

window_command!(
    WindowMinimize,
    "shell.window.minimize",
    "Minimize window",
    RiskLevel::Write,
    WindowRef,
    |w, p| w.do_minimize(p)
);

window_command!(
    WindowRestore,
    "shell.window.restore",
    "Restore window",
    RiskLevel::Write,
    WindowRef,
    |w, p| w.do_restore(p)
);

window_command!(
    WindowMaximize,
    "shell.window.maximize",
    "Maximize window",
    RiskLevel::Write,
    Maximize,
    |w, p| w.do_maximize(p)
);

window_command!(
    WindowPlace,
    "shell.window.place",
    "Move window",
    RiskLevel::Write,
    Place,
    |w, p| w.do_place(p)
);

window_command!(
    WindowSnap,
    "shell.window.snap",
    "Snap window",
    RiskLevel::Write,
    SnapTo,
    |w, p| w.do_snap(p)
);

window_command!(
    WindowsTile,
    "shell.windows.tile",
    "Tile windows",
    RiskLevel::Write,
    Tile,
    |w, _p| w.do_tile()
);

window_command!(
    SplitSet,
    "shell.split.set",
    "Split the screen",
    RiskLevel::Write,
    SetSplit,
    |w, p| w.do_split(p)
);

window_command!(
    SplitClear,
    "shell.split.clear",
    "Clear the split",
    RiskLevel::Write,
    NoParams,
    |w, _p| w.do_unsplit()
);

/// Windows the server opens: a streamed app mapped a toplevel or a popup.
///
/// Under [`Windows::op`] like every change, and announced the same way, but
/// with no seat check: nobody's call made them, the app did. The shell that
/// holds the seat does not adopt the server's echo of its own window
/// changes, so it takes these from the `stream` field instead
/// (`web/src/stores/windows.ts`, `syncStreamed`).
impl crate::stream::StreamWindows for Windows {
    fn open_streamed(
        &self,
        window: &str,
        title: &str,
        size: (u32, u32),
        parent: Option<(&str, (i32, i32))>,
        meta: Value,
    ) {
        let _op = self
            .op
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let popup = parent.is_some();
        let height = if popup {
            size.1
        } else {
            size.1 + STREAM_TITLEBAR
        };
        let spec = window::OpenSpec {
            app: window::AppInstance::new(crate::stream::WINDOW_APP, window),
            title: title.to_string(),
            node_id: self.node_id.clone(),
            min_width: 0,
            size: window::Size {
                width: size.0.max(1),
                height: height.max(1),
            },
        };
        let id = self
            .set()
            .open(spec, assumed_work_area(), self.clock.as_ref());
        self.ids
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(window.to_string(), id);
        // A popup goes where the app put it: its parent's picture's corner
        // plus the offset. The shell places it again from the same numbers
        // against the parent as it is drawn.
        if let Some((parent, (x, y))) = parent {
            let frame = self
                .resolve(parent)
                .ok()
                .and_then(|p| self.set().placement_of(p).ok())
                .map(|p| p.frame);
            if let Some(frame) = frame {
                let rect = window::Rect::new(
                    frame.x + x,
                    frame.y + STREAM_TITLEBAR as i32 + y,
                    size.0.max(1),
                    size.1.max(1),
                );
                let _ = self.set().place(id, rect, assumed_work_area());
            }
        }
        self.streamed().insert(window.to_string(), meta);
        self.changed();
    }

    fn update_streamed(&self, window: &str, title: &str, meta: Value) {
        let _op = self
            .op
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Ok(id) = self.resolve(window) else { return };
        let _ = self.set().set_title(id, title);
        if let (Some(Value::Object(old)), Value::Object(new)) =
            (self.streamed().get_mut(window), meta)
        {
            for (key, value) in new {
                old.insert(key, value);
            }
        }
        self.changed();
    }

    fn close_streamed(&self, window: &str) {
        let _op = self
            .op
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.streamed().remove(window);
        let Ok(id) = self.resolve(window) else { return };
        let _ = self.set().close(id);
        self.ids
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(window);
        self.changed();
    }
}

impl crate::seat::Workspace for Windows {
    fn attach(&self, hub: &Arc<ws::Hub>, seat: &std::sync::Weak<crate::seat::Seat>) {
        let _ = self.hub.set(Arc::clone(hub));
        let _ = self.seat.set(seat.clone());
    }

    fn snapshot(&self) -> Option<(String, Value)> {
        // Under `op`, so no window command is halfway through a change.
        let _op = self
            .op
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        Some((WINDOWS_TOPIC.to_string(), self.wire()))
    }
}

/// Put the window commands on the bus.
pub fn register_windows(
    registry: &Registry,
    node_id: String,
) -> Result<Arc<Windows>, crate::command::RegistryError> {
    let windows = Windows::new(node_id);
    macro_rules! reg {
        ($t:ident) => {
            registry.register($t {
                windows: Arc::clone(&windows),
            })?;
        };
    }
    reg!(WindowOpen);
    reg!(WindowClose);
    reg!(WindowMinimize);
    reg!(WindowRestore);
    reg!(WindowMaximize);
    reg!(WindowPlace);
    reg!(WindowSnap);
    reg!(WindowsTile);
    reg!(SplitSet);
    reg!(SplitClear);
    Ok(windows)
}

// ---------------------------------------------------------------------------
// lockdown
// ---------------------------------------------------------------------------
//
// `lockdown` takes everything it touches through a trait, which is what makes
// every rule in it testable without revoking, killing or freezing anything
// real. The other side of that is that the traits have to be implemented over
// the real thing somewhere, and nothing did — so the kill switch existed,
// passed its tests, and could not fire. These are the implementations.

/// Step 1: revoke every session and rotate the signing key.
///
/// Three things, in this order, because each one is a way the revocation
/// could be undone:
///
///  1. drop them from [`AuthState`], which is what the request path checks;
///  2. delete the persisted rows, or the next start restores every session
///     the lockdown just revoked;
///  3. kill the shells, because a pty is a process running as the host user
///     and the session that asked for it is gone.
///  4. write one audit row per revoked session, because a kill switch that
///     leaves no trace of whose sessions it took is a kill switch nobody can
///     audit afterwards.
pub struct AuthSessions {
    auth: Arc<std::sync::Mutex<crate::auth::state::AuthState>>,
    store: Arc<std::sync::Mutex<crate::store::Store>>,
    terminals: Arc<terminal::Registry>,
    /// Which node the revocation rows name. A required field rather than a
    /// builder call, for the reason `http.rs` gives about registrations that
    /// can be left out of `main.rs`: an unread field compiles, so a `with_`
    /// setter here would let the audit row silently lose its node.
    node_id: String,
    /// Built inside [`AuthSessions::new`] from the store it is already handed,
    /// so there is nothing for `main.rs` to pass and nothing to forget.
    audit: Arc<crate::audit::StoreSink>,
}

impl AuthSessions {
    pub fn new(
        auth: Arc<std::sync::Mutex<crate::auth::state::AuthState>>,
        store: Arc<std::sync::Mutex<crate::store::Store>>,
        terminals: Arc<terminal::Registry>,
        node_id: String,
    ) -> Arc<AuthSessions> {
        let audit = crate::audit::StoreSink::new(Arc::clone(&store));
        Arc::new(AuthSessions {
            auth,
            store,
            terminals,
            node_id,
            audit,
        })
    }
}

impl crate::lockdown::Sessions for Arc<AuthSessions> {
    fn revoke_all(&self) -> usize {
        let hashes = self
            .auth
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .kick();

        {
            let store = self
                .store
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            // One by one rather than `delete_all_sessions`, so a row written
            // by something else between the kick and here is not swept up
            // silently. There are at most `max_sessions` of them.
            for hash in &hashes {
                if let Err(e) = store.delete_session(hash) {
                    tracing::error!(
                        error = %e,
                        "a revoked session could not be removed from the state database; \
                         a restart would bring it back"
                    );
                }
            }
        }

        // Every terminal, not only the revoked sessions': the host's own
        // shells go too. That is the design's step 2 — "kill all child
        // processes" — and a kill switch that spares the shell someone is
        // already sitting at spares the one that matters most.
        //
        // Here rather than left to lockdown's own step 2, which signals bare
        // pids, because `terminal::Registry::kill_all` does three things a
        // pid cannot: it `SIGKILL`s the process *group*, so a build the shell
        // started dies with it; it drops the sessions, which closes the pty
        // masters and so hangs up anything still attached; and it is what
        // terminal.rs already calls the lockdown path. Step 2 still runs
        // after this and still reports — see `TerminalChildren` — so a shell
        // this missed is caught there rather than silently left behind.
        let killed = self.terminals.kill_all();
        if killed > 0 {
            tracing::warn!(killed, "shells killed by lockdown");
        }

        // Last, and deliberately outside the `store` guard above: this writes
        // through `StoreSink`, which locks the same mutex, so doing it inside
        // that block deadlocks on a re-entrant lock rather than failing.
        //
        // Written after the rows are gone and the ptys are dead, by the rule
        // in `http.rs`: the log must never claim a revocation that did not
        // finish.
        //
        // Same command id and same field names as `http::Ctx::record_revocation`,
        // so one query answers "when did this session end" however it ended.
        // `tag` rather than anything containing "session", because
        // `ParamsSummary` blanks those. No `.from(ip)`: `kick` returns hashes
        // only, and under lockdown every session goes, so no single address is
        // evidence — an unset column reads as "not recorded" rather than as a
        // claim about where it came from.
        for hash in &hashes {
            let record = crate::events::Record::new(
                crate::events::Issuer::System,
                "auth.session.revoked",
                self.node_id.clone(),
                crate::events::Outcome::Ok,
            )
            .params(crate::events::ParamsSummary::from_json(
                &serde_json::json!({ "reason": "lockdown", "tag": hash.tag() }),
            ));
            if let Err(e) = self.audit.append_dispatch(
                std::time::SystemTime::now(),
                &record,
                &crate::audit::DispatchFacts::none(),
            ) {
                // Logged and swallowed, as in `http.rs`: the session is
                // already gone, and failing a lockdown because its log could
                // not be written would leave sessions alive that must not be.
                tracing::warn!(error = %e, "cannot record a lockdown revocation in the audit log");
            }
        }

        hashes.len()
    }

    fn adopt_signing_key(&self, _key: crate::lockdown::SessionKey) {
        // Nothing signs session cookies yet; see the TODO(session-signing)
        // note on `lockdown::SessionKey`. Holding the key here would be
        // theater, so it is dropped — what defeats the stolen cookie today is
        // `revoke_all` above, and the generation counter in the journal
        // records that a rotation happened.
    }
}

/// Step 2, the half that knows what we started.
///
/// Terminals are the only children Deckjack spawns today. Agents, app
/// binaries and sync services will register here too; until they exist,
/// reporting the terminals is the whole truth rather than
/// [`crate::lockdown::NoChildren`]'s honest nothing.
///
/// In a lockdown this normally reports **nothing**, and that is not a bug:
/// step 1 has already run, and [`AuthSessions::revoke_all`] kills the shells
/// there because it can kill their process groups and close their pty masters,
/// which signaling a bare pid cannot. What is left for this to report is a
/// shell that step 1 failed to kill — and that is the case worth having, since
/// otherwise it would be a process nobody mentioned. A `Report` with
/// `children_seen: 0` after a lockdown means step 1 got them all.
pub struct TerminalChildren {
    terminals: Arc<terminal::Registry>,
}

impl TerminalChildren {
    pub fn new(terminals: Arc<terminal::Registry>) -> TerminalChildren {
        TerminalChildren { terminals }
    }
}

impl crate::lockdown::Children for TerminalChildren {
    fn running(&self) -> Vec<crate::lockdown::Child> {
        self.terminals
            .list()
            .into_iter()
            // An exited shell is not a child to kill, and a session with no
            // pid is one whose backend never gave us one — signaling a pid we
            // do not have is not something to guess at.
            .filter(|i| i.exit.is_none())
            .filter_map(|i| {
                i.pid.map(|pid| crate::lockdown::Child {
                    pid,
                    kind: crate::lockdown::ChildKind::Terminal,
                    label: i.label.clone(),
                })
            })
            .collect()
    }
}

/// Feeds the bus's refusals to the lockdown watchdog on their way to the real
/// journal.
///
/// `Decision::Refused` already says in `command.rs` that it is "counted: a
/// burst of these from one session is one of the lockdown triggers". Nothing
/// was counting. A journal wrapper is the hook because the journal is the one
/// place every decision already passes through, so neither the bus nor the
/// kill switch has to learn about the other.
pub struct WatchJournal {
    inner: Arc<dyn crate::command::Journal>,
    watchdog: Arc<crate::lockdown::Watchdog>,
    lockdown: Arc<crate::lockdown::Lockdown>,
}

impl WatchJournal {
    pub fn new(
        inner: Arc<dyn crate::command::Journal>,
        watchdog: Arc<crate::lockdown::Watchdog>,
        lockdown: Arc<crate::lockdown::Lockdown>,
    ) -> Arc<WatchJournal> {
        Arc::new(WatchJournal {
            inner,
            watchdog,
            lockdown,
        })
    }
}

impl crate::command::Journal for WatchJournal {
    fn record(&self, entry: crate::command::LogEntry) -> Result<(), crate::command::JournalError> {
        // Not counted while this node is already locked, and that is not an
        // optimization. A lockdown now holds agents on the bus
        // (`LockdownHalt`), so every agent dispatch during one is refused
        // with `AgentsPaused` — which is precisely the entry shape this
        // counts. Counting them would make a locked node manufacture its own
        // trigger burst out of the kill switch working: harmless under the
        // shipped `Response::Alert`, but under `Response::Engage` a thread
        // spawned moments before an unlock re-locks the node straight after
        // it, burning a recovery code for nothing. A refusal while locked
        // tells nobody anything anyway: the answer was never going to be
        // yes.
        let watched = !self.lockdown.is_locked()
            && matches!(entry.decision, crate::command::Decision::Refused)
            && matches!(entry.issuer, crate::command::Issuer::Agent { .. });
        let (command, at) = (entry.command.clone(), entry.at);

        // The real journal first, and its result is what the bus sees. A
        // refusal that was not recorded is a refusal the bus must still be
        // able to reject the dispatch over; the watchdog does not get to
        // change that outcome.
        let recorded = self.inner.record(entry);

        if watched {
            act_on(
                &self.lockdown,
                self.watchdog.observe(
                    crate::lockdown::Observation::RefusedAgentCommand {
                        command: command.to_string(),
                    },
                    at,
                ),
            );
        }
        recorded
    }
}

/// Carry out a watchdog verdict.
///
/// `Engage` blocks for up to `Policy::grace` while children exit, and this is
/// called from a journal write on the dispatch path, so it goes to a thread.
/// `Alert` is a log line and nothing more until there is somewhere to send a
/// notification; saying so here is better than pretending the alert arrived.
pub fn act_on(lockdown: &Arc<crate::lockdown::Lockdown>, verdict: crate::lockdown::Verdict) {
    use crate::lockdown::Verdict;
    match verdict {
        Verdict::Nothing => {}
        Verdict::Alert(trigger) => {
            tracing::warn!(
                trigger = ?trigger.kind(),
                reason = %trigger.describe(),
                "lockdown ALERT: policy says alert rather than engage, and there is no \
                 notification channel yet, so this log line is the alert"
            );
        }
        Verdict::Engage(trigger) => {
            let ld = Arc::clone(lockdown);
            std::thread::spawn(move || {
                ld.engage(trigger);
            });
        }
    }
}

/// `system.lockdown`.
///
/// [`RiskLevel::Write`], which looks wrong for something this consequential
/// and is not. Risk level decides whether *the operator* is asked to
/// confirm, and the design is explicit that triggering takes one click with
/// no step-up: "in an emergency friction is the enemy and the cost of a false
/// positive is an inconvenience". A `Destructive` level here would put a
/// confirmation dialog in front of the kill switch.
///
/// What keeps an agent from firing it is the grant — an agent is never given
/// `system.*` — and the check below, which refuses any issuer that is not a
/// signed-in browser. [`crate::lockdown::may_engage`] is the authorization
/// rule and it says "any signed-in session"; an agent is not one.
pub struct Lockdown {
    lockdown: Arc<crate::lockdown::Lockdown>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct LockdownParams {
    /// Where the operator triggered it: `menu`, `palette`, `hotkey`, `link`.
    /// Recorded because "I hit the hotkey by accident" and "the phone link
    /// was opened" are different incidents.
    #[serde(default)]
    pub surface: Option<String>,
}

impl Command for Lockdown {
    const ID: &'static str = crate::lockdown::CMD_ENGAGE;
    const TITLE: &'static str = "Lock down this node";
    const RISK: RiskLevel = RiskLevel::Write;

    type Params = LockdownParams;
    type Output = Value;

    async fn run(&self, params: LockdownParams, cx: Context) -> Result<Value, CommandError> {
        use crate::lockdown::Surface;

        let signed_in = match &cx.issuer {
            Issuer::Operator { session } => session.is_some(),
            // The CLI is the operator's authority, but it has its own
            // trigger: `Trigger::HostCommand`, not `Operator`.
            Issuer::Cli => false,
            Issuer::Agent { .. } => false,
            // An app's window runs on a signed-in session but is not the
            // operator. `system.*` is hard-denied to it before dispatch; this
            // keeps the answer right even without that.
            Issuer::AppUi { .. } => false,
        };
        let trigger = if signed_in {
            let surface = match params.surface.as_deref() {
                Some("menu") => Surface::Menu,
                Some("hotkey") => Surface::Hotkey,
                Some("link") => Surface::Link,
                // The palette is where a command with no stated surface came
                // from: it is the only surface that dispatches by command id.
                _ => Surface::Palette,
            };
            crate::lockdown::Trigger::Operator { surface }
        } else if matches!(cx.issuer, Issuer::Cli) {
            crate::lockdown::Trigger::HostCommand
        } else {
            return Err(failed(
                "only a signed-in session or the host command may lock this node down",
            ));
        };

        // `engage` blocks for up to the grace period while children exit, and
        // is deliberately not cancellable: a dropped future halfway through
        // step 2 would leave the workspace locked with an agent still
        // running. So it goes to the blocking pool rather than being awaited
        // on a runtime worker.
        let ld = Arc::clone(&self.lockdown);
        let report = tokio::task::spawn_blocking(move || ld.engage(trigger))
            .await
            .map_err(|e| failed(format!("lockdown task did not finish: {e}")))?;

        serde_json::to_value(&report).map_err(|e| failed(e.to_string()))
    }
}

/// `system.status`: is this node locked down, since when and why.
pub struct LockdownStatus {
    lockdown: Arc<crate::lockdown::Lockdown>,
}

impl Command for LockdownStatus {
    const ID: &'static str = "system.status";
    const TITLE: &'static str = "Lockdown status";
    const RISK: RiskLevel = RiskLevel::ReadOnly;

    type Params = NoParams;
    type Output = Value;

    fn enablement(&self) -> Enablement {
        Enablement::Enabled
    }

    async fn run(&self, _p: NoParams, _cx: Context) -> Result<Value, CommandError> {
        serde_json::to_value(self.lockdown.status()).map_err(|e| failed(e.to_string()))
    }
}

/// `system.recovery_codes`: issue a fresh set of offline recovery codes.
///
/// [`RiskLevel::Destructive`], so the operator confirms — unlike the kill
/// switch, and for the opposite reason. Issuing replaces the existing set,
/// which means the codes in a wallet or a safe stop working. Doing that by
/// accident is how a node becomes unlockable.
///
/// The plaintext comes back exactly once; only digests are kept.
pub struct RecoveryCodes {
    lockdown: Arc<crate::lockdown::Lockdown>,
}

impl Command for RecoveryCodes {
    const ID: &'static str = "system.recovery_codes";
    const TITLE: &'static str = "Issue new recovery codes";
    const RISK: RiskLevel = RiskLevel::Destructive;

    type Params = NoParams;
    type Output = Value;

    async fn run(&self, _p: NoParams, _cx: Context) -> Result<Value, CommandError> {
        let codes = self
            .lockdown
            .issue_recovery_codes(crate::lockdown::RECOVERY_CODES_ISSUED)
            .map_err(|e| failed(e.to_string()))?;
        Ok(serde_json::json!({
            "codes": codes.iter().map(crate::lockdown::RecoveryCode::reveal).collect::<Vec<_>>(),
            "note": "Written down now or not at all: only digests are kept, and issuing \
                     a new set stops the previous one working.",
        }))
    }
}

/// Put the lockdown commands on the bus.
pub fn register_lockdown(
    registry: &Registry,
    lockdown: Arc<crate::lockdown::Lockdown>,
) -> Result<(), crate::command::RegistryError> {
    registry.register(Lockdown {
        lockdown: Arc::clone(&lockdown),
    })?;
    registry.register(LockdownStatus {
        lockdown: Arc::clone(&lockdown),
    })?;
    registry.register(RecoveryCodes { lockdown })?;
    Ok(())
}

// ---------------------------------------------------------------------------
// the bus
// ---------------------------------------------------------------------------

/// Everything the bus needs, named.
///
/// A struct rather than seven arguments, and every field required rather
/// than `Option`, for one reason: [`Bus::with_journal`], [`Bus::with_grants`]
/// and [`Bus::with_approvals`] are `mut self` builders with no setter to
/// read back and no accessor to ask. A `Bus` assembled without grants is a
/// `Bus` that refuses every agent command, and a `Bus` assembled without an
/// Inbox takes an approval and then refuses the command it approved —
/// neither of which is a compile error, a panic, or anything a test of the
/// bus can notice. Omitting a field here IS a compile error.
///
/// A caller that genuinely wants none of a thing passes the explicit
/// refusal — [`crate::command::NoGrants`], [`crate::command::NoInbox`] —
/// which reads as a decision in the source instead of an absence.
pub struct BusParts {
    pub registry: Arc<Registry>,
    pub node_id: String,
    /// Wrapped in [`WatchJournal`] here, not by the caller: the journal is
    /// the one place every decision passes through, so it is where a burst
    /// of refusals is counted for the kill switch.
    pub journal: Arc<dyn crate::command::Journal>,
    pub watchdog: Arc<crate::lockdown::Watchdog>,
    pub lockdown: Arc<crate::lockdown::Lockdown>,
    /// The same `Arc` the agent bridge reads. Two grant sources that
    /// disagree is how a tool list and a dispatch tell one caller different
    /// things.
    pub grants: Arc<dyn crate::command::Grants>,
    pub approvals: Arc<dyn crate::command::Approvals>,
    /// What may hold agents — the kill switch. [`LockdownHalt`] in the
    /// product; [`crate::command::NoHalt`] only where there is genuinely no
    /// lockdown to ask.
    ///
    /// Required, and that is the whole point of it being here rather than a
    /// builder default. `Bus::with_halt` is a fourth `mut self` builder with
    /// no setter to read back, so a bus assembled without a hold is a bus
    /// whose lockdown freezes the Inbox and revokes sessions while an app's
    /// already-permitted dispatches keep running — not a compile error, not a
    /// panic, and not visible to any test of the bus. Omitting it here IS a
    /// compile error.
    pub halt: Arc<dyn crate::command::Halt>,
}

/// Assemble the command bus.
///
/// Here rather than in `main`, so that "is this bus wired the way the
/// product's bus is wired" is a question a test can ask. `main` is a binary
/// and cannot be called from one, which is how a bus with no grant storage
/// shipped while every test stayed green.
pub fn bus(parts: BusParts) -> Arc<Bus> {
    Arc::new(
        Bus::new(
            Arc::clone(&parts.registry),
            crate::command::NodeId::new(parts.node_id),
            Arc::new(crate::command::OsEnv),
        )
        .with_journal(WatchJournal::new(
            parts.journal,
            parts.watchdog,
            parts.lockdown,
        ))
        // Lockdown's step 4, over the bus.
        //
        // `lockdown::gate` is axum middleware and so covers HTTP only. An
        // app invokes bus commands over its own Unix socket — `apphost`'s
        // `Invocations::begin` into `AgentBridge::invoke_for_app` — and never
        // passes through that gate at all. Without this a lockdown revokes
        // sessions, kills children and freezes the Inbox while an app's
        // already-permitted dispatches keep running, which is the hole
        // `InboxFreeze` was designed assuming somebody else closed.
        //
        // The hold is ASKED for on each dispatch rather than pushed in once,
        // so `Lockdown::unlock` resumes agents by flipping `is_locked` and
        // has no thaw hook to forget — and a node that `restore`s locked
        // after a restart, or comes up `sealed`, is holding agents from its
        // first dispatch with nothing extra called.
        .with_halt(parts.halt)
        .with_grants(parts.grants)
        .with_approvals(parts.approvals),
    )
}

/// `lockdown::Lockdown` as the bus's [`crate::command::Halt`].
///
/// A newtype here rather than an impl on `Lockdown`, so that `command` never
/// names `lockdown` and the two modules meet only in this assembly — where
/// `AuthSessions` and `TerminalChildren` already do the same job for
/// lockdown's other ports.
///
/// A strong `Arc`, and that is checked rather than assumed: the bus already
/// holds this same `Arc<Lockdown>` through [`WatchJournal`], and nothing
/// reachable from a `Lockdown` reaches the bus — `AuthSessions` holds auth,
/// store and the terminal registry, `TerminalChildren` holds the terminal
/// registry, and `InboxFreeze`'s only way back is a `Weak`. So this is not a
/// cycle. Hand `Lockdown` anything that owns the bus and it becomes one, and
/// this field has to become a `Weak`.
pub struct LockdownHalt(pub Arc<crate::lockdown::Lockdown>);

impl crate::command::Halt for LockdownHalt {
    fn agents_halted(&self) -> bool {
        self.0.is_locked()
    }
}

/// The whole HTTP surface, assembled.
///
/// This is a function rather than ten lines in `main` because of what it is
/// holding together: the lockdown gate has to sit outside every route there
/// is, and "did anyone remember to mount it" is exactly the question that was
/// answered wrong for as long as the kill switch existed and could not fire.
/// A function can be called from a test; `main` cannot.
// One argument per part of the node it routes to, as `serve` in main.rs has.
#[allow(clippy::too_many_arguments)]
pub fn app(
    ctx: Arc<crate::http::Ctx>,
    ws_ctx: Arc<ws::WsCtx>,
    unlock_ctx: Arc<crate::lockdown::UnlockCtx>,
    lockdown: Arc<crate::lockdown::Lockdown>,
    apps: Option<Arc<crate::apphost::AppHost>>,
    // `None` when no `rp_id` a browser would accept could be determined —
    // see `crate::passkeyhttp::settings_for`. The ceremony page is still
    // mounted and explains, because `/login` links to it either way.
    passkeys: Option<Arc<crate::passkeyhttp::PasskeyCtx>>,
    // The one `files::Namespace` the command bus already uses. Passed in
    // rather than built here, and that is the point: the content route,
    // WebDAV, previews and downloads must all cross the same boundary as
    // `files.read`. A second `Root` built for HTTP would be a second place for
    // the working-directory containment rules to drift, which is how the
    // predecessor project ended up with a root that was decorative.
    files: Arc<crate::files::Namespace>,
    // The streamed native apps (`crate::stream`), whose windows' pictures
    // and input go over their own socket. Here, inside the lockdown gate and
    // the cookie-route gate like every other route; `None` where a harness
    // has no install directory to find native apps in.
    streams: Option<Arc<crate::stream::Streams>>,
) -> axum::Router {
    // Here, in the one assembly `main` and the test harnesses share, so a
    // node cannot be built that forgets to reap expired sessions.
    spawn_session_expiry(&ctx, SESSION_SWEEP);

    // Each app's own web build, served from the core's origin at
    // `/apps/<name>/`. Behind `require_session`, which is the point: an app's
    // UI must not be reachable without signing in, and serving it here rather
    // than from the app's own port is what keeps sign-in, CSP and theming the
    // core's to enforce.
    let app_ui = match apps {
        Some(host) => axum::Router::new()
            .nest(
                crate::apps::UI_MOUNT,
                crate::apps::ui_router(host as Arc<dyn crate::apps::UiHost>),
            )
            .layer(axum::middleware::from_fn_with_state(
                Arc::clone(&ctx),
                crate::http::require_session,
            )),
        None => axum::Router::new(),
    };

    // `/healthz`, the one route `ALLOWED_WHILE_LOCKED` names besides `/unlock`.
    //
    // It was carved out of the lockdown gate and listed in the allow-list, but
    // nothing ever mounted it, so it fell through to the shell-asset fallback
    // and an unauthenticated health check was answered with a redirect to
    // sign in -- locked or not. `tests/lockdown.rs` mounts a stand-in
    // `/healthz` in its own router to exercise the gate, which is how a
    // carve-out with no handler behind it stayed invisible.
    //
    // The payload is deliberately narrower than `Status`, which also carries
    // how many recovery codes are left and why the node locked itself. None of
    // that belongs on a route that answers before sign-in. `locked` does: the
    // locked page is already served to anyone who asks, and a load balancer
    // has to know not to send work to a node that cannot do any. See
    // docs/DESIGN.md -- the multi-node routing this feeds is a later phase, so
    // this is the health check and not yet the lock-generation handshake.
    let health = {
        let lockdown = Arc::clone(&lockdown);
        axum::Router::new().route(
            "/healthz",
            axum::routing::get(move || {
                let lockdown = Arc::clone(&lockdown);
                async move {
                    let status = lockdown.status();
                    axum::Json(serde_json::json!({
                        "node": status.node_id,
                        "locked": status.locked,
                        "key_generation": status.key_generation,
                    }))
                }
            }),
        )
    };

    let stream = match streams {
        Some(streams) => crate::stream::router(Arc::new(crate::stream::StreamCtx {
            auth: Arc::clone(&ctx),
            streams,
            clock: Arc::new(ws::SystemClock),
        })),
        None => axum::Router::new(),
    };

    crate::http::router(Arc::clone(&ctx))
        .merge(ws::router(ws_ctx))
        .merge(stream)
        .merge(health)
        // Bytes out of the working directory, for the previews and downloads
        // that `files.read` cannot carry: it answers inside a JSON command
        // response, capped, with no way to seek. The route applies
        // `require_session` itself -- see `content::router`, which argues why
        // that layer belongs inside the function rather than at this call
        // site.
        .merge(crate::content::router(Arc::clone(&ctx), files))
        .merge(crate::lockdown::unlock_router(unlock_ctx))
        // Merged HERE, inside this function and above the gate, and that
        // placement is the whole point. `axum`'s `.layer()` applies only to
        // routes already added, so merging the ceremony after `app` returns
        // would leave a sign-in route answering normally while the node is
        // locked down. `ALLOWED_WHILE_LOCKED` is `/unlock` and `/healthz`;
        // a working sign-in alongside them is a hole in the kill switch.
        .merge(match &passkeys {
            Some(ctx) => crate::passkeyhttp::passkey_router(Arc::clone(ctx)),
            None => axum::Router::new(),
        })
        .merge(crate::passkeypage::page_router(passkeys.is_some()))
        .merge(app_ui)
        // The cookie-route gate (docs/APP-UI-ISOLATION.md §4.9): `Origin:
        // null`, another site's subresource, and a missing `Sec-Fetch-Site`
        // on the routes that act are refused before any handler runs. A
        // layer over everything, with its exemptions a closed list inside
        // `fetchgate`, so a route merged later is gated by default. Inside
        // the lockdown gate, which stays outermost.
        .layer(axum::middleware::from_fn(crate::fetchgate::gate))
        // A `layer`, not a check inside each handler: the one handler that
        // forgot it would be the hole. `/unlock` and `/healthz` are what it
        // lets through — see `lockdown::ALLOWED_WHILE_LOCKED`.
        .layer(axum::middleware::from_fn_with_state(
            lockdown,
            crate::lockdown::gate,
        ))
}

// ---------------------------------------------------------------------------
// the app commands
// ---------------------------------------------------------------------------

/// `system.apps`: what is installed, and whether each one can run.
pub struct ListApps {
    host: Arc<crate::apphost::AppHost>,
}

impl Command for ListApps {
    const ID: &'static str = "system.apps";
    const TITLE: &'static str = "Installed apps";
    const RISK: RiskLevel = RiskLevel::ReadOnly;

    type Params = NoParams;
    type Output = Value;

    fn enablement(&self) -> Enablement {
        Enablement::Enabled
    }

    async fn run(&self, _p: NoParams, _cx: Context) -> Result<Value, CommandError> {
        // Why no app can launch on this node at all (a kernel whose Landlock
        // cannot scope Unix sockets, say), or null. On every entry, so the
        // shell can say it where the app is offered.
        let refusal = self.host.launch_refusal();
        let apps: Vec<Value> = self
            .host
            .installed()
            .into_iter()
            .map(|info| {
                let status = self.host.window_status(&info.name);
                // Whether the operator still has to approve it, so the shell
                // can offer "Approve <app>" before a window fails to open.
                // `unknown` when the approvals could not be read; the launch
                // refuses in that case, and says why.
                let (approval, asks) = match self.host.approval(&info.name) {
                    Some((Ok(state), asks)) => (state.as_str(), Some(asks)),
                    Some((Err(_), asks)) => ("unknown", Some(asks)),
                    None => ("unknown", None),
                };
                serde_json::json!({
                    "name": info.name,
                    "title": info.title.clone().unwrap_or_else(|| info.name.clone()),
                    "version": info.version,
                    "lifecycle": match info.lifecycle {
                        crate::apps::Lifecycle::Service => "service",
                        crate::apps::Lifecycle::OnDemand => "on-demand",
                    },
                    "min_width": info.min_width,
                    // The rest of the app contract, so the shell's dock is
                    // built from what is installed rather than from a list
                    // compiled into the bundle. Absent stays null: the shell
                    // owns the defaults, and a core inventing them here would
                    // be a second place to change them.
                    "tag": info.tag,
                    "summary": info.summary,
                    "default_width": info.default_width,
                    "default_height": info.default_height,
                    "compact": info.compact,
                    "wide": info.wide,
                    // How the shell draws its window: absent frames the
                    // app's own page, 1 renders the tree it describes.
                    "tier": info.tier,
                    "status": match status {
                        crate::apps::WindowStatus::Live => "live",
                        crate::apps::WindowStatus::Reconnecting => "reconnecting",
                        crate::apps::WindowStatus::Unavailable => "unavailable",
                    },
                    "approval": approval,
                    "asks": asks,
                    "refusal": refusal,
                })
            })
            .collect();
        Ok(serde_json::json!({ "apps": apps }))
    }
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct AppRef {
    pub app: String,
}

/// `system.app.approve`: grant an app the `[needs]` its manifest asks for.
///
/// [`RiskLevel::CrossProject`], so the operator confirms. That is not
/// ceremony: approving is the moment a program is given the network, the GPU
/// or parts of the working directory, and it is the only moment the operator
/// is asked. An app whose manifest later asks for more has a different
/// fingerprint and lands here again rather than inheriting consent.
pub struct ApproveApp {
    host: Arc<crate::apphost::AppHost>,
}

impl Command for ApproveApp {
    const ID: &'static str = "system.app.approve";
    const TITLE: &'static str = "Approve an app's permissions";
    const RISK: RiskLevel = RiskLevel::CrossProject;

    type Params = AppRef;
    type Output = Value;

    fn subject(&self, p: &AppRef) -> Option<String> {
        Some(p.app.clone())
    }

    async fn run(&self, p: AppRef, _cx: Context) -> Result<Value, CommandError> {
        let grant = self
            .host
            .approve(&p.app)
            .map_err(|e| failed(e.to_string()))?;
        Ok(serde_json::json!({
            "app": grant.app,
            "fingerprint": grant.fingerprint,
            "needs": grant.needs,
            "granted_at": grant.granted_at,
        }))
    }
}

/// `system.app.revoke`: take an approval back.
pub struct RevokeApp {
    host: Arc<crate::apphost::AppHost>,
}

impl Command for RevokeApp {
    const ID: &'static str = "system.app.revoke";
    const TITLE: &'static str = "Revoke an app's permissions";
    const RISK: RiskLevel = RiskLevel::Destructive;

    type Params = AppRef;
    type Output = ();

    fn subject(&self, p: &AppRef) -> Option<String> {
        Some(p.app.clone())
    }

    async fn run(&self, p: AppRef, _cx: Context) -> Result<(), CommandError> {
        self.host.revoke(&p.app).map_err(|e| failed(e.to_string()))
    }
}

/// Put the app-management commands on the bus.
pub fn register_apps(
    registry: &Registry,
    host: Arc<crate::apphost::AppHost>,
) -> Result<(), crate::command::RegistryError> {
    registry.register(ListApps {
        host: Arc::clone(&host),
    })?;
    registry.register(ApproveApp {
        host: Arc::clone(&host),
    })?;
    registry.register(RevokeApp { host })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_change_refused_for_a_lost_seat_is_answered_like_the_gate() {
        let status = crate::seat::Status {
            held: false,
            claimable: false,
            holder: Some(crate::seat::HolderView {
                device: "Pixel 7".to_string(),
                since_ms: 1,
                connected: true,
            }),
        };
        let lost = failure(CommandError::Failed(anyhow::Error::new(
            crate::seat::LostSeat {
                status: status.clone(),
            },
        )));
        assert_eq!(lost.code, crate::seat::NOT_HELD);
        assert!(lost.message.contains("Pixel 7"), "{}", lost.message);
        assert_eq!(lost.detail, Some(status.to_json()));

        let other = failure(failed("the disk is full"));
        assert_eq!(other.code, "failed");
        assert_eq!(other.message, "the disk is full");
    }
}
