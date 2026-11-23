//! Streamed native apps (docs/lanes/8-stream.md).
//!
//! A native Linux app the suite has no web UI for runs under
//! `deckjack-stream`, Deckjack's own headless Wayland compositor, which the
//! core spawns **confined**, exactly as it spawns an app: the real
//! [`LaunchPlan::build`] from the native app's manifest, Landlock and
//! seccomp applied by [`Env::spawn`]. The app is the compositor's child and
//! inherits the same confinement. Each of the app's toplevels and popups
//! becomes a Deckjack window (server state, pushed as `windows` events like
//! any other), drawn by the shell's stream panel from H.264 pictures.
//!
//! # Where the bytes go
//!
//! The compositor talks to the core over one end of a Unix socket pair it
//! inherits ([`ENV_LINK_FD`]); there is no socket path another local process
//! could connect to first. The framing is `deckjack_protocol::stream`'s.
//! Pictures do not
//! ride the command bus: each streamed window's panel opens its own
//! WebSocket at [`WS_PATH`], and the core relays that window's pictures to
//! it and its input to the compositor. Decision D6 of
//! docs/APP-UI-ISOLATION.md refused open-ended duplex streams into app
//! frames, so the panel is a shell panel, never an app iframe, and the
//! socket is the shell's.
//!
//! # Who may use a stream
//!
//! A stream socket is admitted only for a signed-in session (cookie and
//! `Origin`, as `/ws`) that **holds the single seat**, and every input
//! message is checked against the seat again before it is forwarded. When
//! the seat moves, every viewer of the old session is closed
//! ([`seat::Workspace::moved`]); the new holder's panels connect and are
//! each sent a keyframe, so the stream moves with the workspace.
//!
//! # Declaring a native app
//!
//! `<install>/native/<name>.toml`: an app manifest (`[app]`, `[needs]`,
//! validated by the same [`Manifest::parse`] every app's is) plus a
//! `[native]` table naming the program. The compositor binary sits beside
//! them as `<install>/native/deckjack-stream`. `native` is an install
//! layout entry, so no app grant can reach it.
//!
//! # How it is confined
//!
//! As an app is, plus [`sandbox::Ipc::Scoped`]: the compositor and the app
//! cannot connect to a Unix socket made outside their own sandbox (the
//! session bus, the operator's X server, another app's compositor), cannot
//! connect to an abstract socket made outside it, and cannot signal a
//! process outside it. That needs Landlock ABI 9, and a kernel without it
//! refuses native apps rather than running them less confined. Each native
//! app's private `HOME` and `TMPDIR` are under `cache/native/<name>/`, apart
//! from the apps' `cache/apps/<name>/`.
//!
//! What is not built yet, said plainly: a native app is not put through the
//! approval flow ([`crate::apps::Host`]'s grants and fingerprints), so the
//! operator placing the declaration in the install directory is the only
//! consent. A lockdown does stop compositors ([`Streams`] is one of its
//! [`crate::lockdown::Children`]).

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock, PoisonError, Weak};
use std::time::Duration;

use axum::Router;
use axum::extract::ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade};
use axum::extract::{ConnectInfo, Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use deckjack_protocol::Manifest;
use deckjack_protocol::stream::{
    BrowserMessage, ENV_LINK_FD, ENV_SPEC, KIND_CONTROL, KIND_VIDEO, LaunchSpec, StreamCommand,
    StreamEvent, SurfaceKind, VideoHeader, check_link_len, link_frame,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::sync::mpsc;

use crate::apps::{Confinement, Env, Installed, LaunchPlan};
use crate::auth::SessionHash;
use crate::auth::state::{Now, VerifyOutcome};
use crate::command::{Command, CommandError, Context, Registry, RegistryError, RiskLevel};
use crate::sandbox;
use crate::seat::Seat;
use crate::ws::{self, Hub};

/// Where native app declarations and the compositor live, under the
/// install directory.
pub const NATIVE_DIR: &str = "native";

/// The compositor's binary, in [`NATIVE_DIR`].
pub const COMPOSITOR: &str = "deckjack-stream";

/// The app id every streamed window carries in the window list. The shell
/// draws it with its stream panel.
pub const WINDOW_APP: &str = "stream";

pub use deckjack_protocol::stream::WS_PATH;

/// Where native apps' private `HOME` and `TMPDIR` live, under the install
/// directory's cache: apart from the apps' (`cache/apps/<name>/`), so a
/// native app named like an app is not handed that app's private tree.
pub const NATIVE_CACHE: &str = "native";

/// Pictures queued for one viewer before it is considered behind. Past
/// this, pictures are dropped and the viewer waits for a keyframe.
const VIEWER_QUEUE: usize = 16;

/// How often an open stream socket re-checks its session and the seat
/// even when the browser sends nothing.
const RECHECK: Duration = Duration::from_secs(2);

/// How long one message may take to go out on a stream socket. A browser
/// that has stopped reading (a phone asleep, a dead link) would otherwise
/// hold its socket, and its place in the viewer list, until TCP gave up.
const SEND_TIMEOUT: Duration = Duration::from_secs(5);

// ---------------------------------------------------------------------------
// Declarations
// ---------------------------------------------------------------------------

/// `[native]` in a native app's declaration.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NativeDecl {
    /// The program, by absolute path, and its arguments.
    pub exec: Vec<String>,
    /// Start XWayland for it (an X11 app, or one that may fall back to X11).
    #[serde(default)]
    pub x11: bool,
}

/// One declared native app.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NativeApp {
    pub manifest: Manifest,
    pub native: NativeDecl,
}

impl NativeApp {
    pub fn name(&self) -> &str {
        &self.manifest.app.name
    }
}

/// Read a declaration: the `[native]` table taken out, everything else
/// validated as an app manifest is.
pub fn parse_native(path: &Path, text: &str) -> Result<NativeApp, String> {
    let mut table: toml::Table =
        toml::from_str(text).map_err(|e| format!("{}: {e}", path.display()))?;
    let native = table
        .remove("native")
        .ok_or_else(|| format!("{}: no [native] table naming the program", path.display()))?;
    let native: NativeDecl = native
        .try_into()
        .map_err(|e| format!("{}: [native]: {e}", path.display()))?;
    let program = native
        .exec
        .first()
        .ok_or_else(|| format!("{}: [native] exec is empty", path.display()))?;
    if !Path::new(program).is_absolute() {
        return Err(format!(
            "{}: [native] exec must start with an absolute path, not {program:?}: nothing is \
             looked up on a PATH",
            path.display()
        ));
    }
    let rest = toml::to_string(&table).map_err(|e| format!("{}: {e}", path.display()))?;
    let manifest = Manifest::parse(path, &rest).map_err(|e| e.to_string())?;
    if !manifest.commands.is_empty() {
        return Err(format!(
            "{}: a native app serves no commands; remove [[commands]]",
            path.display()
        ));
    }
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or_default();
    if stem != manifest.app.name {
        return Err(format!(
            "{}: the file must be named after the app ({}.toml)",
            path.display(),
            manifest.app.name
        ));
    }
    Ok(NativeApp { manifest, native })
}

/// Every declaration under `<install>/native/`, and what was wrong with
/// the ones that are not usable.
pub fn discover(install: &Path) -> (Vec<NativeApp>, Vec<String>) {
    let dir = install.join(NATIVE_DIR);
    let mut apps = Vec::new();
    let mut problems = Vec::new();
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return (apps, problems);
    };
    let mut paths: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "toml"))
        .collect();
    paths.sort();
    for path in paths {
        match std::fs::read_to_string(&path)
            .map_err(|e| format!("{}: {e}", path.display()))
            .and_then(|text| parse_native(&path, &text))
        {
            Ok(app) => apps.push(app),
            Err(problem) => problems.push(problem),
        }
    }
    (apps, problems)
}

/// The plan the compositor for `app` is spawned with: the real
/// [`LaunchPlan::build_in`] from the declaration's manifest, with the
/// private tree under `cache/native/<name>/`, held to
/// [`sandbox::Ipc::Scoped`], handed the link at descriptor `link_fd` and the
/// app to run.
///
/// Refused, when confinement is enforced, on a kernel that cannot scope
/// Unix sockets and signals (Landlock ABI 9): a native app is a GUI app
/// with the operator's own X server and session bus in reach otherwise.
pub fn native_plan(
    app: &NativeApp,
    install: &Path,
    working_dir: &Path,
    confinement: Confinement,
    env: &dyn Env,
    link_fd: i32,
) -> Result<LaunchPlan, String> {
    if confinement.is_enforced()
        && let Err(reason) = sandbox::ipc_scoping()
    {
        return Err(format!(
            "{} cannot be streamed on this node: {reason}. A native app is not run with the \
             operator's sockets in reach.",
            app.name()
        ));
    }
    let dir = install.join(NATIVE_DIR);
    let installed = Installed {
        name: app.name().to_string(),
        dir: dir.clone(),
        binary: dir.join(COMPOSITOR),
        manifest: app.manifest.clone(),
    };
    let private_root = install.join(crate::apps::CACHE_SUBDIR).join(NATIVE_CACHE);
    let mut plan = LaunchPlan::build_in(
        &installed,
        install,
        working_dir,
        confinement,
        env,
        &private_root,
    )
    .map_err(|e| e.to_string())?;
    // Nothing outside the sandbox is a socket the compositor or the app
    // needs: the link is inherited, and the Wayland and X11 sockets are
    // made inside it.
    plan.sandbox.policy.ipc = sandbox::Ipc::Scoped {
        connect_unix: Vec::new(),
    };
    let spec = LaunchSpec {
        app: app.name().to_string(),
        exec: app.native.exec.clone(),
        x11: app.native.x11,
    };
    plan.pass_fds.push(link_fd);
    plan.env
        .push((ENV_LINK_FD.to_string(), link_fd.to_string()));
    plan.env.push((
        ENV_SPEC.to_string(),
        serde_json::to_string(&spec).map_err(|e| e.to_string())?,
    ));
    Ok(plan)
}

// ---------------------------------------------------------------------------
// The windows' side
// ---------------------------------------------------------------------------

/// What the stream registry needs from the window list. Implemented by
/// `wiring::Windows`; a trait so this module does not reach into it.
pub trait StreamWindows: Send + Sync + 'static {
    /// Open a server-made window. `parent` names a popup's parent window.
    fn open_streamed(
        &self,
        window: &str,
        title: &str,
        size: (u32, u32),
        parent: Option<(&str, (i32, i32))>,
        meta: Value,
    );
    /// A streamed window's title or picture size changed.
    fn update_streamed(&self, window: &str, title: &str, meta: Value);
    /// The app's window went away.
    fn close_streamed(&self, window: &str);
}

/// The stream registry's answer to the window list's close button.
pub trait StreamClose: Send + Sync + 'static {
    /// Ask the app to close the window, which goes away when the app
    /// unmaps it. True when `window` is a streamed window.
    fn close_requested(&self, window: &str) -> bool;
}

// ---------------------------------------------------------------------------
// The registry
// ---------------------------------------------------------------------------

/// One viewer: one stream panel's socket.
struct Viewer {
    id: u64,
    session: SessionHash,
    tx: mpsc::Sender<Message>,
    /// Pictures were dropped for this viewer; skip until a keyframe.
    needs_key: bool,
    /// A keyframe has been asked for on this viewer's behalf and has not
    /// reached it yet. Without this, every keyframe that fails to fit in a
    /// stalled viewer's queue would ask for another, forever.
    asked: bool,
    /// Set when the seat moves away from this viewer's session: its socket
    /// sends nothing more, not even what is already queued.
    revoked: Arc<AtomicBool>,
}

impl Viewer {
    /// Whether this viewer has room for a keyframe worth asking for: half
    /// its queue free, so the picture is not dropped on arrival.
    fn has_room(&self) -> bool {
        self.tx.capacity() >= VIEWER_QUEUE / 2
    }
}

struct Surface {
    window: String,
    viewers: Vec<Viewer>,
}

struct Instance {
    app: String,
    title: String,
    pid: u32,
    to_compositor: mpsc::UnboundedSender<Vec<u8>>,
    surfaces: HashMap<u32, Surface>,
}

#[derive(Default)]
struct Inner {
    next_instance: u64,
    next_viewer: u64,
    instances: HashMap<u64, Instance>,
}

/// The streamed apps running on this node.
pub struct Streams {
    install: PathBuf,
    working_dir: PathBuf,
    confinement: Confinement,
    env: Arc<dyn Env>,
    windows: Arc<dyn StreamWindows>,
    seat: OnceLock<Weak<Seat>>,
    inner: Mutex<Inner>,
}

/// Why a launch failed, in words for the operator.
fn failed(message: impl Into<String>) -> CommandError {
    CommandError::Failed(anyhow::anyhow!(message.into()))
}

/// A streamed window's client id: `stream-<instance>-<surface>`.
pub fn window_id(instance: u64, surface: u32) -> String {
    format!("stream-{instance}-{surface}")
}

/// The other way: `None` for anything that is not a streamed window's id.
pub fn parse_window_id(window: &str) -> Option<(u64, u32)> {
    let rest = window.strip_prefix("stream-")?;
    let (instance, surface) = rest.split_once('-')?;
    Some((instance.parse().ok()?, surface.parse().ok()?))
}

impl Streams {
    pub fn new(
        install: PathBuf,
        working_dir: PathBuf,
        confinement: Confinement,
        env: Arc<dyn Env>,
        windows: Arc<dyn StreamWindows>,
    ) -> Arc<Streams> {
        Arc::new(Streams {
            install,
            working_dir,
            confinement,
            env,
            windows,
            seat: OnceLock::new(),
            inner: Mutex::new(Inner::default()),
        })
    }

    fn inner(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The session holding the seat, if any.
    fn holder(&self) -> Option<SessionHash> {
        self.seat
            .get()
            .and_then(Weak::upgrade)
            .and_then(|seat| seat.holder_session())
    }

    /// The declared native apps.
    pub fn apps(&self) -> (Vec<NativeApp>, Vec<String>) {
        discover(&self.install)
    }

    /// The plan the core spawns the compositor with, for `app`, handing it
    /// the link at descriptor `link_fd`.
    pub fn plan(&self, app: &NativeApp, link_fd: i32) -> Result<LaunchPlan, String> {
        native_plan(
            app,
            &self.install,
            &self.working_dir,
            self.confinement,
            self.env.as_ref(),
            link_fd,
        )
    }

    /// Start a native app. Returns its instance number once the compositor
    /// has been spawned; its windows arrive over the link as the app maps
    /// them, and a compositor that fails to start ends the instance the
    /// same way an app's exit does.
    pub async fn launch(self: &Arc<Self>, name: &str) -> Result<u64, String> {
        let (apps, problems) = self.apps();
        let Some(app) = apps.into_iter().find(|a| a.name() == name) else {
            let hint = problems
                .iter()
                .find(|p| p.contains(&format!("{name}.toml")))
                .cloned()
                .unwrap_or_else(|| {
                    format!(
                        "no native app {name:?} is declared in {}",
                        self.install.join(NATIVE_DIR).display()
                    )
                });
            return Err(hint);
        };
        if !self.install.join(NATIVE_DIR).join(COMPOSITOR).is_file() {
            return Err(format!(
                "the compositor is not installed: {} is missing",
                self.install.join(NATIVE_DIR).join(COMPOSITOR).display()
            ));
        }
        // The link: a socket pair, ours and the compositor's. Both ends are
        // close-on-exec here; the plan opens the child's up in the child
        // alone, so nothing else spawned meanwhile inherits it.
        let (core_end, child_end) = std::os::unix::net::UnixStream::pair()
            .map_err(|e| format!("cannot make the stream link: {e}"))?;
        use std::os::fd::AsRawFd as _;
        let plan = self.plan(&app, child_end.as_raw_fd())?;
        let pid = self
            .env
            .spawn(&plan)
            .map_err(|e| format!("cannot start the compositor for {name}: {e}"))?;
        // Ours no longer: if the compositor dies, the link must read as
        // closed, which it cannot while this process holds the other end.
        drop(child_end);
        let link = core_end
            .set_nonblocking(true)
            .and_then(|()| tokio::net::UnixStream::from_std(core_end))
            .map_err(|e| {
                let _ = self.env.terminate(pid);
                format!("cannot use the stream link: {e}")
            })?;
        let instance = self.adopt(name, app.manifest.title(), pid, link);
        tracing::info!(app = %name, instance, pid, "streamed app started");
        Ok(instance)
    }

    /// Take a running compositor in: `pid` is its process, `link` the
    /// core's end of its link. Returns the instance number its windows are
    /// named by.
    pub fn adopt(
        self: &Arc<Self>,
        app: &str,
        title: &str,
        pid: u32,
        link: tokio::net::UnixStream,
    ) -> u64 {
        let (reader, writer) = link.into_split();
        let (to_compositor, rx) = mpsc::unbounded_channel::<Vec<u8>>();
        let instance = {
            let mut inner = self.inner();
            inner.next_instance += 1;
            let instance = inner.next_instance;
            inner.instances.insert(
                instance,
                Instance {
                    app: app.to_string(),
                    title: title.to_string(),
                    pid,
                    to_compositor,
                    surfaces: HashMap::new(),
                },
            );
            instance
        };
        tokio::spawn(write_link(writer, rx));
        let me = Arc::clone(self);
        tokio::spawn(async move { me.read_link(instance, reader).await });
        instance
    }

    async fn read_link(
        self: Arc<Self>,
        instance: u64,
        mut reader: tokio::net::unix::OwnedReadHalf,
    ) {
        loop {
            let mut len = [0u8; 4];
            if reader.read_exact(&mut len).await.is_err() {
                break;
            }
            let Ok(len) = check_link_len(u32::from_le_bytes(len)) else {
                tracing::warn!(
                    instance,
                    "a compositor sent an oversized frame; dropping it"
                );
                break;
            };
            let mut body = vec![0u8; len];
            if reader.read_exact(&mut body).await.is_err() {
                break;
            }
            match body[0] {
                KIND_CONTROL => match serde_json::from_slice::<StreamEvent>(&body[1..]) {
                    Ok(event) => self.on_event(instance, event),
                    Err(e) => tracing::warn!(instance, error = %e, "malformed stream event"),
                },
                KIND_VIDEO => self.on_video(instance, &body[1..]),
                other => tracing::warn!(instance, kind = other, "unknown link frame"),
            }
        }
        self.ended(instance);
    }

    fn on_event(&self, instance: u64, event: StreamEvent) {
        match event {
            StreamEvent::Ready { x11_display } => {
                tracing::info!(instance, ?x11_display, "streamed app ready");
            }
            StreamEvent::Mapped {
                surface,
                kind,
                parent,
                title,
                width,
                height,
                x,
                y,
            } => {
                let window = window_id(instance, surface);
                let (app, app_title) = {
                    let mut inner = self.inner();
                    let Some(inst) = inner.instances.get_mut(&instance) else {
                        return;
                    };
                    inst.surfaces.insert(
                        surface,
                        Surface {
                            window: window.clone(),
                            viewers: Vec::new(),
                        },
                    );
                    (inst.app.clone(), inst.title.clone())
                };
                let parent_window = parent.map(|p| window_id(instance, p));
                let shown = if title.is_empty() {
                    app_title.clone()
                } else {
                    title
                };
                let meta = meta(
                    &app,
                    &app_title,
                    kind,
                    parent_window.as_deref(),
                    (width, height),
                    (x, y),
                );
                self.windows.open_streamed(
                    &window,
                    &shown,
                    (width, height),
                    parent_window.as_deref().map(|p| (p, (x, y))),
                    meta,
                );
            }
            StreamEvent::Changed {
                surface,
                title,
                width,
                height,
                x,
                y,
            } => {
                let window = window_id(instance, surface);
                let Some(app_title) = self
                    .inner()
                    .instances
                    .get(&instance)
                    .map(|i| i.title.clone())
                else {
                    return;
                };
                let shown = if title.is_empty() { app_title } else { title };
                // Merged into what `Mapped` recorded: the kind and the
                // parent do not change.
                self.windows.update_streamed(
                    &window,
                    &shown,
                    json!({ "width": width, "height": height, "x": x, "y": y }),
                );
            }
            StreamEvent::Unmapped { surface } => {
                let removed = {
                    let mut inner = self.inner();
                    inner
                        .instances
                        .get_mut(&instance)
                        .and_then(|i| i.surfaces.remove(&surface))
                };
                if let Some(s) = removed {
                    close_viewers(&s.viewers, "the window closed");
                    self.windows.close_streamed(&s.window);
                }
            }
            StreamEvent::Exited { status } => {
                tracing::info!(instance, %status, "streamed app exited");
            }
        }
    }

    fn on_video(&self, instance: u64, payload: &[u8]) {
        let Some((header, _)) = VideoHeader::parse_link(payload) else {
            return;
        };
        let Some(browser) = VideoHeader::browser_bytes(payload) else {
            return;
        };
        let mut wants_key = false;
        {
            let mut inner = self.inner();
            let Some(inst) = inner.instances.get_mut(&instance) else {
                return;
            };
            let Some(surface) = inst.surfaces.get_mut(&header.surface) else {
                return;
            };
            surface.viewers.retain(|v| !v.tx.is_closed());
            for viewer in &mut surface.viewers {
                wants_key |= relay(viewer, header.is_keyframe(), browser);
            }
        }
        if wants_key {
            self.command(
                instance,
                &StreamCommand::Keyframe {
                    surface: header.surface,
                },
            );
        }
    }

    /// Ask for a keyframe for a viewer that is waiting for one and now has
    /// room for it, when nothing has asked yet. Called on the socket's
    /// periodic recheck, so a viewer that caught up while the window was
    /// idle (no picture to notice it by) still gets its picture.
    fn nudge(&self, instance: u64, surface: u32, viewer: u64) {
        let ask = {
            let mut inner = self.inner();
            let Some(v) = inner
                .instances
                .get_mut(&instance)
                .and_then(|i| i.surfaces.get_mut(&surface))
                .and_then(|s| s.viewers.iter_mut().find(|v| v.id == viewer))
            else {
                return;
            };
            let ask = v.needs_key && !v.asked && v.has_room();
            v.asked |= ask;
            ask
        };
        if ask {
            self.command(instance, &StreamCommand::Keyframe { surface });
        }
    }

    /// The compositor's link closed: the app and its windows are gone.
    fn ended(&self, instance: u64) {
        let Some(inst) = self.inner().instances.remove(&instance) else {
            return;
        };
        for surface in inst.surfaces.values() {
            close_viewers(&surface.viewers, "the app exited");
            self.windows.close_streamed(&surface.window);
        }
        // Reap it, or ask it to go if it is somehow still there. Never pid
        // 0: to kill(2) that is this whole process group.
        if inst.pid != 0 && self.env.is_running(inst.pid) {
            let _ = self.env.terminate(inst.pid);
        }
        tracing::info!(app = %inst.app, instance, "streamed app ended");
    }

    fn command(&self, instance: u64, command: &StreamCommand) {
        let Ok(json) = serde_json::to_vec(command) else {
            return;
        };
        if let Some(inst) = self.inner().instances.get(&instance) {
            let _ = inst.to_compositor.send(link_frame(KIND_CONTROL, &json));
        }
    }

    /// Stop an instance: the compositor is terminated, which takes the app
    /// with it (it dies with its parent), and the link's end tears the
    /// windows down.
    pub fn stop(&self, instance: u64) -> Result<(), String> {
        let pid = self
            .inner()
            .instances
            .get(&instance)
            .map(|i| i.pid)
            .filter(|&pid| pid != 0)
            .ok_or_else(|| format!("no streamed app {instance} is running"))?;
        self.env.terminate(pid).map_err(|e| e.to_string())
    }

    /// What is running, for `stream.list`.
    pub fn list(&self) -> Value {
        let inner = self.inner();
        let mut rows: Vec<Value> = inner
            .instances
            .iter()
            .map(|(id, i)| {
                let mut windows: Vec<&str> =
                    i.surfaces.values().map(|s| s.window.as_str()).collect();
                windows.sort_unstable();
                json!({ "instance": id, "app": i.app, "title": i.title, "windows": windows })
            })
            .collect();
        rows.sort_by_key(|r| r["instance"].as_u64());
        Value::Array(rows)
    }

    /// Register a viewer for a window. `None` when the window is not a
    /// streamed window that exists.
    fn attach(&self, window: &str, session: SessionHash) -> Option<Attached> {
        let (instance, surface) = parse_window_id(window)?;
        let (tx, rx) = mpsc::channel(VIEWER_QUEUE);
        let revoked = Arc::new(AtomicBool::new(false));
        let id = {
            let mut inner = self.inner();
            inner.next_viewer += 1;
            let id = inner.next_viewer;
            let s = inner
                .instances
                .get_mut(&instance)?
                .surfaces
                .get_mut(&surface)?;
            s.viewers.push(Viewer {
                id,
                session,
                tx,
                needs_key: true,
                asked: true,
                revoked: Arc::clone(&revoked),
            });
            id
        };
        // A viewer starts with a keyframe: it has no picture to apply a
        // delta to.
        self.command(instance, &StreamCommand::Keyframe { surface });
        Some(Attached {
            viewer: id,
            instance,
            surface,
            rx,
            revoked,
        })
    }

    /// A viewer's socket ended. When it was the instance's last viewer,
    /// nothing is left that could send the releases for keys or buttons it
    /// held, so the compositor is told to release them all.
    fn detach(&self, instance: u64, surface: u32, viewer: u64) {
        let last = {
            let mut inner = self.inner();
            let Some(inst) = inner.instances.get_mut(&instance) else {
                return;
            };
            let Some(s) = inst.surfaces.get_mut(&surface) else {
                return;
            };
            let before = s.viewers.len();
            s.viewers.retain(|v| v.id != viewer);
            before != s.viewers.len() && inst.surfaces.values().all(|s| s.viewers.is_empty())
        };
        if last {
            self.command(instance, &StreamCommand::ReleaseAll);
        }
    }

    /// Every running compositor's pid, for a shutdown or a lockdown.
    pub fn pids(&self) -> Vec<u32> {
        self.inner()
            .instances
            .values()
            .map(|i| i.pid)
            .filter(|&pid| pid != 0)
            .collect()
    }
}

/// What [`Streams::attach`] hands the socket that attached.
struct Attached {
    viewer: u64,
    instance: u64,
    surface: u32,
    rx: mpsc::Receiver<Message>,
    revoked: Arc<AtomicBool>,
}

/// Offer one picture to one viewer. True when a keyframe should be asked
/// for on its behalf.
///
/// A viewer that has missed a picture skips to the next keyframe, and one
/// keyframe is asked for when it starts missing them. While it is still
/// behind (a keyframe failed to fit too) nothing more is asked until its
/// queue has room again: a stalled viewer must not turn every keyframe into
/// a request for another one.
fn relay(viewer: &mut Viewer, keyframe: bool, browser: &[u8]) -> bool {
    if viewer.needs_key && !keyframe {
        let ask = !viewer.asked && viewer.has_room();
        viewer.asked |= ask;
        return ask;
    }
    match viewer.tx.try_send(Message::Binary(browser.to_vec().into())) {
        Ok(()) => {
            if keyframe {
                viewer.asked = false;
            }
            viewer.needs_key = false;
            false
        }
        Err(_) => {
            let first = !viewer.needs_key;
            viewer.needs_key = true;
            // The keyframe it was waiting for did not fit: what was asked
            // has been spent. Asked again once there is room.
            if keyframe {
                viewer.asked = false;
            }
            if first {
                viewer.asked = true;
            }
            first
        }
    }
}

fn close_viewers(viewers: &[Viewer], reason: &'static str) {
    for v in viewers {
        let _ = v.tx.try_send(Message::Close(Some(CloseFrame {
            code: 1000,
            reason: reason.into(),
        })));
    }
}

fn meta(
    app: &str,
    app_title: &str,
    kind: SurfaceKind,
    parent: Option<&str>,
    size: (u32, u32),
    offset: (i32, i32),
) -> Value {
    json!({
        "app": app,
        "appTitle": app_title,
        "kind": kind,
        "parent": parent,
        "width": size.0,
        "height": size.1,
        "x": offset.0,
        "y": offset.1,
    })
}

async fn write_link(
    mut writer: tokio::net::unix::OwnedWriteHalf,
    mut rx: mpsc::UnboundedReceiver<Vec<u8>>,
) {
    while let Some(frame) = rx.recv().await {
        if writer.write_all(&frame).await.is_err() {
            break;
        }
    }
}

impl StreamClose for Streams {
    fn close_requested(&self, window: &str) -> bool {
        let Some((instance, surface)) = parse_window_id(window) else {
            return false;
        };
        let known = self
            .inner()
            .instances
            .get(&instance)
            .is_some_and(|i| i.surfaces.contains_key(&surface));
        if known {
            self.command(instance, &StreamCommand::Close { surface });
        }
        known
    }
}

impl crate::seat::Workspace for Streams {
    fn attach(&self, _hub: &Arc<Hub>, seat: &Weak<Seat>) {
        let _ = self.seat.set(seat.clone());
    }

    /// The seat moved: the old session's viewers are taken off every
    /// window and revoked, so not one more picture reaches them (not even
    /// one already queued), and every app is told to release whatever the
    /// old device held down. The new holder's panels open their own
    /// sockets, and each is sent a keyframe on attach.
    fn moved(&self, from: &SessionHash, _to: &SessionHash) {
        let instances: Vec<u64> = {
            let mut inner = self.inner();
            for inst in inner.instances.values_mut() {
                for s in inst.surfaces.values_mut() {
                    s.viewers.retain(|v| {
                        if v.session != *from {
                            return true;
                        }
                        v.revoked.store(true, Ordering::SeqCst);
                        // Best effort: a full queue loses this frame, and
                        // the revoked flag closes the socket regardless.
                        let _ = v.tx.try_send(seat_close());
                        false
                    });
                }
            }
            inner.instances.keys().copied().collect()
        };
        for instance in instances {
            self.command(instance, &StreamCommand::ReleaseAll);
        }
    }
}

impl crate::lockdown::Children for Streams {
    /// Every running compositor, for the kill switch: each is sent
    /// `SIGTERM`, ends the app and everything it started, and then (or after
    /// the grace period's `SIGKILL`) its link closes and its windows go.
    fn running(&self) -> Vec<crate::lockdown::Child> {
        let inner = self.inner();
        let mut children: Vec<crate::lockdown::Child> = inner
            .instances
            .iter()
            .filter(|(_, i)| i.pid != 0 && self.env.is_running(i.pid))
            .map(|(id, i)| crate::lockdown::Child {
                pid: i.pid,
                kind: crate::lockdown::ChildKind::App,
                label: format!("{} (streamed, instance {id})", i.app),
            })
            .collect();
        children.sort_by_key(|c| c.pid);
        children
    }
}

/// Close code for "this session no longer holds the seat". Private range.
pub const CLOSE_SEAT: u16 = 4403;

// ---------------------------------------------------------------------------
// The socket
// ---------------------------------------------------------------------------

/// What the stream route needs.
pub struct StreamCtx {
    pub auth: Arc<crate::http::Ctx>,
    pub streams: Arc<Streams>,
    pub clock: Arc<dyn ws::Clock>,
}

impl StreamCtx {
    fn now(&self) -> Now {
        Now {
            wall: self.clock.now(),
            mono: self.clock.mono(),
        }
    }

    fn session_ok(&self, ip: std::net::IpAddr, session: &crate::auth::SessionId) -> bool {
        let now = self.now();
        matches!(
            self.auth.state().verify(ip, Some(session), now),
            VerifyOutcome::Ok
        )
    }
}

#[derive(Debug, Deserialize)]
pub struct StreamQuery {
    window: String,
}

/// The stream socket's route.
pub fn router(ctx: Arc<StreamCtx>) -> Router {
    Router::new().route(WS_PATH, get(upgrade)).with_state(ctx)
}

fn refuse(status: StatusCode, message: &'static str) -> Response {
    (
        status,
        [(header::CACHE_CONTROL, "no-store")],
        format!("{message}\n"),
    )
        .into_response()
}

/// The same rule as `/ws`: a present `Origin` must name `Host`.
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
    origin
        .strip_prefix("https://")
        .or_else(|| origin.strip_prefix("http://"))
        .is_some_and(|a| a.eq_ignore_ascii_case(host))
}

async fn upgrade(
    State(ctx): State<Arc<StreamCtx>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    Query(query): Query<StreamQuery>,
    headers: HeaderMap,
    upgrade: WebSocketUpgrade,
) -> Response {
    let ip = ctx.auth.client_ip(peer, &headers);
    if !origin_ok(&headers) {
        return refuse(StatusCode::FORBIDDEN, "cross-origin websocket refused");
    }
    let Some(session) = ctx.auth.session_cookie(peer, &headers) else {
        return refuse(StatusCode::UNAUTHORIZED, "sign in first: no valid session");
    };
    if !ctx.session_ok(ip, &session) {
        return refuse(StatusCode::UNAUTHORIZED, "sign in first: no valid session");
    }
    let hash = session.hash();
    if ctx.streams.holder() != Some(hash) {
        return refuse(
            StatusCode::CONFLICT,
            "this device does not hold the seat; take over to see streamed windows",
        );
    }
    let Some(attached) = ctx.streams.attach(&query.window, hash) else {
        return refuse(StatusCode::NOT_FOUND, "no such streamed window");
    };
    upgrade
        .max_message_size(64 << 10)
        .on_upgrade(move |socket| serve(ctx, socket, ip, session, attached))
}

/// Send one message, giving up on a peer that has stopped reading.
async fn send_or_give_up(socket: &mut WebSocket, message: Message) -> bool {
    matches!(
        tokio::time::timeout(SEND_TIMEOUT, socket.send(message)).await,
        Ok(Ok(()))
    )
}

async fn serve(
    ctx: Arc<StreamCtx>,
    mut socket: WebSocket,
    ip: std::net::IpAddr,
    session: crate::auth::SessionId,
    attached: Attached,
) {
    let Attached {
        viewer,
        instance,
        surface,
        mut rx,
        revoked,
    } = attached;
    let hash = session.hash();
    let streams = Arc::clone(&ctx.streams);
    let mut recheck = tokio::time::interval(RECHECK);
    let admitted =
        |ctx: &StreamCtx| ctx.session_ok(ip, &session) && ctx.streams.holder() == Some(hash);
    loop {
        tokio::select! {
            out = rx.recv() => {
                let Some(message) = out else { break };
                // The seat moved away: whatever is still queued is the new
                // holder's workspace, not this device's.
                if revoked.load(Ordering::SeqCst) {
                    let _ = send_or_give_up(&mut socket, seat_close()).await;
                    break;
                }
                let closing = matches!(message, Message::Close(_));
                if !send_or_give_up(&mut socket, message).await || closing {
                    break;
                }
            }
            incoming = socket.recv() => {
                let Some(Ok(message)) = incoming else { break };
                match message {
                    Message::Text(text) => {
                        // Checked on every message, not only at the upgrade:
                        // the seat can move while the socket is open.
                        if revoked.load(Ordering::SeqCst) || !admitted(&ctx) {
                            let _ = send_or_give_up(&mut socket, seat_close()).await;
                            break;
                        }
                        match serde_json::from_str::<BrowserMessage>(text.as_str()) {
                            Ok(BrowserMessage::Input { input }) => {
                                streams.command(instance, &StreamCommand::Input { surface, input });
                            }
                            Ok(BrowserMessage::Resize { width, height }) => {
                                streams.command(instance, &StreamCommand::Resize { surface, width, height });
                            }
                            Ok(BrowserMessage::Keyframe) => {
                                streams.command(instance, &StreamCommand::Keyframe { surface });
                            }
                            Err(e) => {
                                tracing::debug!(error = %e, "malformed stream message");
                            }
                        }
                    }
                    Message::Close(_) => break,
                    // Nothing binary comes from the browser.
                    Message::Binary(_) => break,
                    Message::Ping(_) | Message::Pong(_) => {}
                }
            }
            _ = recheck.tick() => {
                if revoked.load(Ordering::SeqCst) || !admitted(&ctx) {
                    let _ = send_or_give_up(&mut socket, seat_close()).await;
                    break;
                }
                streams.nudge(instance, surface, viewer);
            }
        }
    }
    streams.detach(instance, surface, viewer);
}

fn seat_close() -> Message {
    Message::Close(Some(CloseFrame {
        code: CLOSE_SEAT,
        reason: "this device no longer holds the seat, or its session ended".into(),
    }))
}

// ---------------------------------------------------------------------------
// Commands
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct NoParams {}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct LaunchParams {
    /// The native app's name, as `stream.apps` lists it.
    pub app: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct StopParams {
    /// The instance, as `stream.launch` and `stream.list` report it.
    pub instance: u64,
}

#[derive(Debug, Serialize)]
pub struct Launched {
    pub instance: u64,
}

/// `stream.apps`: the declared native apps.
pub struct ListApps {
    streams: Arc<Streams>,
}

impl Command for ListApps {
    const ID: &'static str = "stream.apps";
    const TITLE: &'static str = "List native apps";
    const RISK: RiskLevel = RiskLevel::ReadOnly;
    type Params = NoParams;
    type Output = Value;

    async fn run(&self, _p: NoParams, _cx: Context) -> Result<Value, CommandError> {
        let (apps, problems) = self.streams.apps();
        Ok(json!({
            "apps": apps.iter().map(|a| json!({
                "name": a.name(),
                "title": a.manifest.title(),
                "tag": a.manifest.app.tag,
                "x11": a.native.x11,
                "minWidth": a.manifest.app.min_width,
            })).collect::<Vec<_>>(),
            "problems": problems,
        }))
    }
}

/// `stream.launch`: start a native app. A write, like `terminal.open`: it
/// starts a process, confined to what the app's declaration asks for.
pub struct Launch {
    streams: Arc<Streams>,
}

impl Command for Launch {
    const ID: &'static str = "stream.launch";
    const TITLE: &'static str = "Open native app";
    const RISK: RiskLevel = RiskLevel::Write;
    type Params = LaunchParams;
    type Output = Launched;

    async fn run(&self, p: LaunchParams, _cx: Context) -> Result<Launched, CommandError> {
        let instance = self.streams.launch(&p.app).await.map_err(failed)?;
        Ok(Launched { instance })
    }
}

/// `stream.list`: what is running.
pub struct List {
    streams: Arc<Streams>,
}

impl Command for List {
    const ID: &'static str = "stream.list";
    const TITLE: &'static str = "List running native apps";
    const RISK: RiskLevel = RiskLevel::ReadOnly;
    type Params = NoParams;
    type Output = Value;

    async fn run(&self, _p: NoParams, _cx: Context) -> Result<Value, CommandError> {
        Ok(self.streams.list())
    }
}

/// `stream.stop`: end a native app now, without asking it. Destructive:
/// the app gets no chance to save, which is what closing its window is for.
pub struct Stop {
    streams: Arc<Streams>,
}

impl Command for Stop {
    const ID: &'static str = "stream.stop";
    const TITLE: &'static str = "Force quit native app";
    const RISK: RiskLevel = RiskLevel::Destructive;
    type Params = StopParams;
    type Output = ();

    fn subject(&self, p: &StopParams) -> Option<String> {
        let inner = self.streams.inner();
        Some(match inner.instances.get(&p.instance) {
            Some(i) => format!(
                "{} (instance {}), without letting it save",
                i.title, p.instance
            ),
            None => format!("instance {}", p.instance),
        })
    }

    async fn run(&self, p: StopParams, _cx: Context) -> Result<(), CommandError> {
        self.streams.stop(p.instance).map_err(failed)
    }
}

/// Put the `stream.*` commands on the bus.
pub fn register(registry: &Registry, streams: &Arc<Streams>) -> Result<(), RegistryError> {
    registry.register(ListApps {
        streams: Arc::clone(streams),
    })?;
    registry.register(Launch {
        streams: Arc::clone(streams),
    })?;
    registry.register(List {
        streams: Arc::clone(streams),
    })?;
    registry.register(Stop {
        streams: Arc::clone(streams),
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const DECL: &str = r#"
[app]
name = "gimp"
version = "3.2"
protocol = 1
title = "GIMP"

[needs]
working_dir = [{ path = "Pictures", write = true }]

[native]
exec = ["/usr/bin/gimp"]
x11 = false
"#;

    #[test]
    fn a_declaration_is_a_validated_manifest_plus_the_program() {
        let app = parse_native(Path::new("/i/native/gimp.toml"), DECL).unwrap();
        assert_eq!(app.name(), "gimp");
        assert_eq!(app.manifest.title(), "GIMP");
        assert_eq!(app.manifest.needs.working_dir.len(), 1);
        assert_eq!(app.native.exec, vec!["/usr/bin/gimp"]);
    }

    #[test]
    fn a_declaration_is_refused_for_what_would_make_it_unsafe_or_ambiguous() {
        let path = Path::new("/i/native/gimp.toml");
        // A program looked up on a PATH.
        let relative = DECL.replace("/usr/bin/gimp", "gimp");
        assert!(
            parse_native(path, &relative)
                .unwrap_err()
                .contains("absolute")
        );
        // No [native] at all.
        let none = DECL.split("[native]").next().unwrap();
        assert!(parse_native(path, none).unwrap_err().contains("[native]"));
        // The manifest's own rules still apply: no reaching the hidden dir.
        let hidden = DECL.replace("Pictures", ".deckjack");
        assert!(parse_native(path, &hidden).is_err());
        // Named after the file, so the dock's name and the file agree.
        assert!(parse_native(Path::new("/i/native/other.toml"), DECL).is_err());
        // Unknown keys in [native] are refused, not ignored.
        let unknown = DECL.replace("x11 = false", "x11 = false\nshell = true");
        assert!(parse_native(path, &unknown).is_err());
    }

    /// The window list's side, recorded.
    #[derive(Default)]
    struct Recorded(Mutex<Vec<String>>);

    impl StreamWindows for Recorded {
        fn open_streamed(
            &self,
            window: &str,
            _: &str,
            _: (u32, u32),
            _: Option<(&str, (i32, i32))>,
            _: Value,
        ) {
            self.0.lock().unwrap().push(format!("open {window}"));
        }
        fn update_streamed(&self, window: &str, _: &str, _: Value) {
            self.0.lock().unwrap().push(format!("update {window}"));
        }
        fn close_streamed(&self, window: &str) {
            self.0.lock().unwrap().push(format!("close {window}"));
        }
    }

    /// A registry with one running instance (no process behind it: pid 0
    /// is never signaled here) whose link to the compositor is `rx`.
    fn one_instance() -> (
        Arc<Streams>,
        Arc<Recorded>,
        mpsc::UnboundedReceiver<Vec<u8>>,
    ) {
        let windows = Arc::new(Recorded::default());
        let streams = Streams::new(
            PathBuf::from("/nonexistent"),
            PathBuf::from("/nonexistent"),
            Confinement::Enforced,
            Arc::new(crate::apps::OsEnv::new()),
            Arc::clone(&windows) as _,
        );
        let (tx, rx) = mpsc::unbounded_channel();
        streams.inner().instances.insert(
            1,
            Instance {
                app: "gimp".into(),
                title: "GIMP".into(),
                pid: 0,
                to_compositor: tx,
                surfaces: HashMap::new(),
            },
        );
        streams.on_event(
            1,
            StreamEvent::Mapped {
                surface: 4,
                kind: SurfaceKind::Toplevel,
                parent: None,
                title: "GIMP".into(),
                width: 800,
                height: 600,
                x: 0,
                y: 0,
            },
        );
        (streams, windows, rx)
    }

    fn session(byte: u8) -> SessionHash {
        crate::auth::SessionId::from_bytes([byte; 32]).hash()
    }

    fn commands(rx: &mut mpsc::UnboundedReceiver<Vec<u8>>) -> Vec<StreamCommand> {
        let mut out = Vec::new();
        while let Ok(frame) = rx.try_recv() {
            assert_eq!(frame[4], KIND_CONTROL);
            out.push(serde_json::from_slice(&frame[5..]).unwrap());
        }
        out
    }

    fn picture(keyframe: bool) -> Vec<u8> {
        VideoHeader {
            surface: 4,
            flags: u8::from(keyframe),
            width: 800,
            height: 600,
            timestamp: 0,
        }
        .link_bytes(&[0, 0, 0, 1, 0x65])
    }

    #[test]
    fn a_viewer_is_sent_a_keyframe_first_and_nothing_it_cannot_decode() {
        let (streams, windows, mut rx) = one_instance();
        assert_eq!(windows.0.lock().unwrap().as_slice(), ["open stream-1-4"]);
        let mut pictures = streams.attach("stream-1-4", session(1)).unwrap().rx;
        assert_eq!(
            commands(&mut rx),
            vec![StreamCommand::Keyframe { surface: 4 }],
            "a viewer attaching must ask for a keyframe"
        );
        // A delta before any keyframe would decode against nothing.
        streams.on_video(1, &picture(false));
        assert!(pictures.try_recv().is_err());
        streams.on_video(1, &picture(true));
        streams.on_video(1, &picture(false));
        let first = pictures.try_recv().unwrap();
        let Message::Binary(bytes) = first else {
            panic!("not a picture")
        };
        // The browser's form: the surface is left out.
        assert_eq!(bytes[0], 1, "the keyframe came first");
        assert_eq!(
            bytes.len(),
            deckjack_protocol::stream::BROWSER_HEADER_LEN + 5
        );
        assert!(pictures.try_recv().is_ok(), "the delta after it");
        // Unknown windows have no viewers to attach to.
        assert!(streams.attach("stream-1-5", session(1)).is_none());
        assert!(streams.attach("stream-2-4", session(1)).is_none());
    }

    #[test]
    fn a_viewer_that_falls_behind_skips_to_the_next_keyframe() {
        let (streams, _, mut rx) = one_instance();
        let mut pictures = streams.attach("stream-1-4", session(1)).unwrap().rx;
        streams.on_video(1, &picture(true));
        // Fill the viewer's queue (the keyframe and the deltas after it)
        // without reading it.
        for _ in 1..VIEWER_QUEUE {
            streams.on_video(1, &picture(false));
        }
        let _ = commands(&mut rx);
        streams.on_video(1, &picture(false));
        assert_eq!(
            commands(&mut rx),
            vec![StreamCommand::Keyframe { surface: 4 }],
            "a dropped picture must ask for a keyframe"
        );
        // Drain; the next delta is skipped, the next keyframe is not.
        while pictures.try_recv().is_ok() {}
        streams.on_video(1, &picture(false));
        assert!(pictures.try_recv().is_err(), "a delta after a gap was sent");
        streams.on_video(1, &picture(true));
        assert!(pictures.try_recv().is_ok());
    }

    #[test]
    fn the_seat_moving_closes_the_old_sessions_viewers_only() {
        let (streams, _, mut link) = one_instance();
        let old = streams.attach("stream-1-4", session(1)).unwrap();
        let mut new = streams.attach("stream-1-4", session(2)).unwrap().rx;
        let _ = commands(&mut link);
        crate::seat::Workspace::moved(streams.as_ref(), &session(1), &session(2));
        let mut rx = old.rx;
        match rx.try_recv() {
            Ok(Message::Close(Some(frame))) => assert_eq!(frame.code, CLOSE_SEAT),
            other => panic!("the old session's viewer was not closed: {other:?}"),
        }
        assert!(
            old.revoked.load(Ordering::SeqCst),
            "the old viewer was not revoked"
        );
        assert!(
            new.try_recv().is_err(),
            "the new session's viewer was closed"
        );
        assert!(
            !streams.inner().instances[&1].surfaces[&4]
                .viewers
                .iter()
                .any(|v| v.session == session(1))
        );
        assert_eq!(
            commands(&mut link),
            vec![StreamCommand::ReleaseAll],
            "keys the old device held must be released"
        );
    }

    /// A takeover reaches a viewer that is behind, too: its queue is full,
    /// so the close frame cannot be queued, and still no picture of the new
    /// holder's reaches it and its socket ends.
    #[test]
    fn a_takeover_cuts_off_a_viewer_whose_queue_is_full() {
        let (streams, _, _link) = one_instance();
        let old = streams.attach("stream-1-4", session(1)).unwrap();
        streams.on_video(1, &picture(true));
        for _ in 1..VIEWER_QUEUE {
            streams.on_video(1, &picture(false));
        }
        crate::seat::Workspace::moved(streams.as_ref(), &session(1), &session(2));
        assert!(old.revoked.load(Ordering::SeqCst));
        // The new holder's first picture goes nowhere near the old device.
        streams.on_video(1, &picture(true));
        let mut rx = old.rx;
        let mut queued = 0;
        while let Ok(m) = rx.try_recv() {
            assert!(matches!(m, Message::Binary(_)), "{m:?}");
            queued += 1;
        }
        assert_eq!(
            queued, VIEWER_QUEUE,
            "a picture was queued after the takeover"
        );
        assert!(
            rx.is_closed(),
            "the old viewer's queue is still open for more"
        );
    }

    /// A viewer that has stopped reading costs one keyframe request, not
    /// one per keyframe: an idle window must not be re-encoded as a
    /// keyframe sixty times a second because one phone went to sleep.
    #[test]
    fn a_stalled_viewer_does_not_turn_keyframes_into_more_requests() {
        let (streams, _, mut link) = one_instance();
        let stalled = streams.attach("stream-1-4", session(1)).unwrap();
        streams.on_video(1, &picture(true));
        for _ in 1..VIEWER_QUEUE {
            streams.on_video(1, &picture(false));
        }
        let _ = commands(&mut link);
        // It falls behind: one request.
        streams.on_video(1, &picture(false));
        assert_eq!(
            commands(&mut link),
            vec![StreamCommand::Keyframe { surface: 4 }]
        );
        // The keyframes that answer it do not fit either, and ask for
        // nothing more; nor do the deltas while its queue stays full.
        for _ in 0..20 {
            streams.on_video(1, &picture(true));
            streams.on_video(1, &picture(false));
        }
        assert_eq!(commands(&mut link), vec![], "a stalled viewer asked again");
        // The periodic nudge asks nothing while it is still full...
        streams.nudge(1, 4, stalled.viewer);
        assert_eq!(commands(&mut link), vec![]);
        // ...and once it has drained, asks exactly once.
        let mut rx = stalled.rx;
        while rx.try_recv().is_ok() {}
        streams.nudge(1, 4, stalled.viewer);
        streams.nudge(1, 4, stalled.viewer);
        assert_eq!(
            commands(&mut link),
            vec![StreamCommand::Keyframe { surface: 4 }]
        );
        // A delta arriving now does not ask a second time; the keyframe is
        // delivered and the viewer is live again.
        streams.on_video(1, &picture(false));
        assert_eq!(commands(&mut link), vec![]);
        streams.on_video(1, &picture(true));
        streams.on_video(1, &picture(false));
        assert_eq!(
            rx.try_recv()
                .map(|m| matches!(m, Message::Binary(b) if b[0] == 1)),
            Ok(true)
        );
        assert!(rx.try_recv().is_ok(), "the delta after the keyframe");
    }

    /// A viewer that drained while the window was idle is noticed by the
    /// nudge even when no picture arrives to notice it by: it asked, the
    /// keyframe did not fit, and nothing else will come.
    #[test]
    fn a_viewer_that_caught_up_on_an_idle_window_is_nudged() {
        let (streams, _, mut link) = one_instance();
        let v = streams.attach("stream-1-4", session(1)).unwrap();
        streams.on_video(1, &picture(true));
        for _ in 1..VIEWER_QUEUE {
            streams.on_video(1, &picture(false));
        }
        streams.on_video(1, &picture(false));
        streams.on_video(1, &picture(true));
        let _ = commands(&mut link);
        let mut rx = v.rx;
        while rx.try_recv().is_ok() {}
        streams.nudge(1, 4, v.viewer);
        assert_eq!(
            commands(&mut link),
            vec![StreamCommand::Keyframe { surface: 4 }]
        );
    }

    /// The instance's last viewer going away releases whatever it held:
    /// no device is left to send the releases. One of two going away does
    /// not, since the other may be holding a key right now.
    #[test]
    fn the_last_viewer_leaving_releases_what_it_held() {
        let (streams, _, mut link) = one_instance();
        let a = streams.attach("stream-1-4", session(1)).unwrap();
        let b = streams.attach("stream-1-4", session(1)).unwrap();
        let _ = commands(&mut link);
        streams.detach(1, 4, a.viewer);
        assert_eq!(commands(&mut link), vec![]);
        streams.detach(1, 4, b.viewer);
        assert_eq!(commands(&mut link), vec![StreamCommand::ReleaseAll]);
        // A viewer already gone (moved() took it) is not a second release.
        streams.detach(1, 4, b.viewer);
        assert_eq!(commands(&mut link), vec![]);
    }

    /// The kill switch sees every compositor that is still running, and
    /// never pid 0 (to kill(2), the whole process group).
    #[test]
    fn the_lockdown_sees_running_compositors_and_never_pid_zero() {
        use crate::lockdown::Children as _;
        let (streams, _, _link) = one_instance();
        assert!(
            streams.running().is_empty(),
            "pid 0 was offered to the kill switch"
        );
        assert!(streams.pids().is_empty());
        assert!(streams.stop(1).is_err(), "pid 0 would be signaled");
        let mut child = std::process::Command::new("/usr/bin/sleep")
            .arg("30")
            .spawn()
            .unwrap();
        let pid = child.id();
        streams.inner().instances.get_mut(&1).unwrap().pid = pid;
        let running = streams.running();
        assert_eq!(running.len(), 1);
        assert_eq!(running[0].pid, pid);
        assert_eq!(running[0].kind, crate::lockdown::ChildKind::App);
        assert!(running[0].label.contains("gimp"));
        let _ = child.kill();
        let _ = child.wait();
        assert!(
            streams.running().is_empty(),
            "an exited compositor is still a child"
        );
    }

    /// A native app's private tree is its own, not an app's of the same
    /// name; it is held to the scoped sandbox; and it is handed the link
    /// by descriptor, not by a path.
    #[test]
    fn a_native_plan_has_its_own_private_tree_the_scoped_sandbox_and_the_link_fd() {
        let dir = tempfile::tempdir().unwrap();
        let install = dir.path().join("install");
        let work = dir.path().join("work");
        std::fs::create_dir_all(install.join(NATIVE_DIR)).unwrap();
        std::fs::create_dir_all(&work).unwrap();
        let app = parse_native(
            Path::new("/i/native/notes.toml"),
            &DECL.replace("\"gimp\"", "\"notes\""),
        )
        .unwrap();
        let env = crate::apps::OsEnv::new();
        let plan = match native_plan(&app, &install, &work, Confinement::Enforced, &env, 7) {
            Ok(plan) => plan,
            Err(e) => {
                eprintln!("skipped: this host cannot confine a native app: {e}");
                return;
            }
        };
        let var = |k: &str| {
            plan.env
                .iter()
                .find(|(key, _)| key == k)
                .map(|(_, v)| v.clone())
        };
        let home = PathBuf::from(var("HOME").unwrap());
        let private = install.join("cache").join(NATIVE_CACHE).join("notes");
        assert_eq!(home, private.join("home"));
        assert_eq!(PathBuf::from(var("TMPDIR").unwrap()), private.join("tmp"));
        let apps_tree = install
            .join("cache")
            .join(crate::apps::APPS_SUBDIR)
            .join("notes");
        assert!(
            !plan
                .sandbox
                .policy
                .read_write
                .iter()
                .any(|p| p.starts_with(&apps_tree)),
            "the native app was granted the Notes app's private tree"
        );
        assert_eq!(
            plan.sandbox.policy.ipc,
            sandbox::Ipc::Scoped {
                connect_unix: Vec::new()
            }
        );
        assert_eq!(plan.pass_fds, vec![7]);
        assert_eq!(var(ENV_LINK_FD).as_deref(), Some("7"));
        let spec: LaunchSpec = serde_json::from_str(&var(ENV_SPEC).unwrap()).unwrap();
        assert_eq!(spec.exec, vec!["/usr/bin/gimp"]);
    }

    #[test]
    fn an_unmapped_window_closes_its_viewers_and_its_window() {
        let (streams, windows, _rx) = one_instance();
        let mut viewer = streams.attach("stream-1-4", session(1)).unwrap().rx;
        streams.on_event(1, StreamEvent::Unmapped { surface: 4 });
        assert!(matches!(viewer.try_recv(), Ok(Message::Close(_))));
        assert_eq!(
            windows.0.lock().unwrap().last().unwrap(),
            "close stream-1-4"
        );
        assert!(!streams.close_requested("stream-1-4"));
    }

    #[test]
    fn window_ids_round_trip_and_nothing_else_parses() {
        assert_eq!(window_id(3, 17), "stream-3-17");
        assert_eq!(parse_window_id("stream-3-17"), Some((3, 17)));
        assert_eq!(parse_window_id("stream-3"), None);
        assert_eq!(parse_window_id("terminal-3-17"), None);
        assert_eq!(parse_window_id("stream-x-1"), None);
    }
}
