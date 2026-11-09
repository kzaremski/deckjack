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
/// refused rather than queued. Remote sessions are gated again by step-up
/// sign-in, which the design specifies and which does not exist yet. Those
/// are the right instruments; risk level is not.
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
            Some(browser) => Opener::Client(browser.reveal().clone()),
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

    async fn run(&self, params: ReadParams, _cx: Context) -> Result<Chunk, CommandError> {
        let session = self.session(&params.id)?;
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

impl Read {
    fn session(&self, id: &str) -> Result<Arc<terminal::TerminalSession>, CommandError> {
        let id =
            TerminalId::parse(id).ok_or_else(|| failed(format!("{id} is not a terminal id")))?;
        self.terminals
            .attach(&id)
            .map_err(|e| failed(e.to_string()))
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

    async fn run(&self, params: WriteParams, _cx: Context) -> Result<(), CommandError> {
        let id = TerminalId::parse(&params.id)
            .ok_or_else(|| failed(format!("{} is not a terminal id", params.id)))?;
        let session = self
            .terminals
            .attach(&id)
            .map_err(|e| failed(e.to_string()))?;
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

    async fn run(&self, params: ResizeParams, _cx: Context) -> Result<(), CommandError> {
        let id = TerminalId::parse(&params.id)
            .ok_or_else(|| failed(format!("{} is not a terminal id", params.id)))?;
        let session = self
            .terminals
            .attach(&id)
            .map_err(|e| failed(e.to_string()))?;
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

    async fn run(&self, params: CloseParams, _cx: Context) -> Result<(), CommandError> {
        let id = TerminalId::parse(&params.id)
            .ok_or_else(|| failed(format!("{} is not a terminal id", params.id)))?;
        self.terminals.close(&id).map_err(|e| failed(e.to_string()))
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

    async fn run(&self, _p: NoParams, _cx: Context) -> Result<Value, CommandError> {
        let list = self.terminals.list();
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
    clock: Arc<dyn window::Clock + Send + Sync>,
    node_id: String,
}

impl Windows {
    pub fn new(node_id: String) -> Arc<Windows> {
        Arc::new(Windows {
            set: std::sync::Mutex::new(window::WindowSet::new()),
            ids: std::sync::Mutex::new(std::collections::HashMap::new()),
            clock: Arc::new(window::SystemClock),
            node_id,
        })
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
        Ok(serde_json::json!({ "id": p.id, "server_id": id.as_u64() }))
    }

    fn do_close(&self, p: WindowRef) -> Result<Value, CommandError> {
        let id = self.resolve(&p.id)?;
        self.set().close(id).map_err(|e| failed(e.to_string()))?;
        self.ids
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&p.id);
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
