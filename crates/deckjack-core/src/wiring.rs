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
pub struct BusDispatch {
    bus: Arc<Bus>,
}

impl BusDispatch {
    pub fn new(bus: Arc<Bus>) -> Arc<BusDispatch> {
        Arc::new(BusDispatch { bus })
    }
}

impl ws::Dispatch for BusDispatch {
    fn dispatch(&self, call: ws::Call, progress: ws::ProgressSink) -> ws::Dispatched {
        let bus = Arc::clone(&self.bus);

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
            let mut request = Request::new(
                id,
                call.params,
                Issuer::browser(call.issuer.session().clone()),
            );
            if call.confirmed {
                request = request.confirmed();
            }

            let admitted = match bus.dispatch(request) {
                Ok(a) => a,
                Err(refused) => {
                    return ws::Outcome::error(ws::CommandError::new(
                        "refused",
                        refused.to_string(),
                    ));
                }
            };

            match admitted {
                Admitted::Running(running) => {
                    let (mut rx, done) = running.split();

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
                        Err(e) => {
                            ws::Outcome::error(ws::CommandError::new("failed", e.to_string()))
                        }
                    }
                }
                // Both of these are answers, not failures: the command has
                // not run and the shell has something to show the operator.
                Admitted::Queued { pending, risk, .. } => ws::Outcome::ok(serde_json::json!({
                    "status": "queued",
                    "pending": pending.to_string(),
                    "risk": risk.as_str(),
                })),
                // Not a failure: the operator has not been asked yet. The
                // prompt is the bus's, so the client shows that text rather
                // than inventing its own, then re-invokes with `confirmed`.
                Admitted::NeedsConfirmation { risk, prompt, .. } => {
                    ws::Outcome::ok(serde_json::json!({
                        "status": "needs_confirmation",
                        "prompt": prompt,
                        "risk": risk.as_str(),
                    }))
                }
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
    if session.opened_by() != &opener_of(cx) {
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

/// Put the terminal commands on the bus.
///
/// Returns the terminal registry so the caller can close sessions when a
/// browser session ends or a lockdown fires — a pty outliving the session
/// that opened it is a shell nobody is watching.
pub fn register_terminal(
    registry: &Registry,
    cwd: std::path::PathBuf,
) -> Result<Arc<terminal::Registry>, crate::command::RegistryError> {
    let terminals = Arc::new(terminal::Registry::new(
        Arc::new(terminal::OsEnv),
        Arc::new(terminal::NativeBackend),
        terminal::Limits::default(),
    ));

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
/// server and adopted by the client on the response. That needs the server to
/// push `windows` events so a second device learns about a window it did not
/// open — which does not exist yet, so neither does the simplification.
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
}

impl Windows {
    pub fn new(node_id: String) -> Arc<Windows> {
        Arc::new(Windows {
            set: std::sync::Mutex::new(window::WindowSet::new()),
            ids: std::sync::Mutex::new(std::collections::HashMap::new()),
            apps_by_window: std::sync::Mutex::new(std::collections::HashMap::new()),
            clock: Arc::new(window::SystemClock),
            node_id,
            host: std::sync::OnceLock::new(),
        })
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

            async fn run(&self, params: $params, _cx: Context) -> Result<Value, CommandError> {
                let f: fn(&Windows, $params) -> Result<Value, CommandError> = $body;
                f(&self.windows, params)
            }
        }
    };
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct OpenWindow {
    pub id: String,
    pub app: String,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub width: Option<u32>,
    #[serde(default)]
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

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct Place {
    pub id: String,
    #[serde(default)]
    pub x: i32,
    #[serde(default)]
    pub y: i32,
    #[serde(default)]
    pub width: u32,
    #[serde(default)]
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
) -> axum::Router {
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

    crate::http::router(Arc::clone(&ctx))
        .merge(ws::router(ws_ctx))
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
        let apps: Vec<Value> = self
            .host
            .installed()
            .into_iter()
            .map(|info| {
                let status = self.host.window_status(&info.name);
                serde_json::json!({
                    "name": info.name,
                    "title": info.title.clone().unwrap_or_else(|| info.name.clone()),
                    "version": info.version,
                    "lifecycle": match info.lifecycle {
                        crate::apps::Lifecycle::Service => "service",
                        crate::apps::Lifecycle::OnDemand => "on-demand",
                    },
                    "min_width": info.min_width,
                    "status": match status {
                        crate::apps::WindowStatus::Live => "live",
                        crate::apps::WindowStatus::Reconnecting => "reconnecting",
                        crate::apps::WindowStatus::Unavailable => "unavailable",
                    },
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
