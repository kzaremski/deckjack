//! The compositor: one headless Wayland display, optionally XWayland, one
//! native app, and one picture stream per app window.
//!
//! There is no screen. Every toplevel and every popup is a window of its
//! own, rendered on its own into an offscreen image the size of the window
//! (its xdg window geometry, so client-side shadows are left out), encoded
//! on its own thread, and sent to the core tagged with its surface number.
//! The core turns each into a Deckjack window. Positions between windows
//! mean nothing here, with one exception: a popup's offset from its parent,
//! which the core uses to place the popup's window next to its parent's.
//!
//! The app sees one 1920x1080 output at scale 1. It never learns how big or
//! how dense the screen looking at it is: the browser scales the picture.
//!
//! Rendering is the pixman (CPU) renderer, because the sandbox denies the
//! GPU unless a manifest asks for it. Each window has its own damage
//! tracker over its own image, so a window whose app did not commit
//! anything costs nothing, and one that did is re-encoded only where the
//! damage tracker says pixels changed (none changed: no picture at all).

use std::collections::{BTreeMap, BTreeSet};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result};
use deckjack_protocol::stream::{
    FLAG_KEYFRAME, Input, LaunchSpec, OUTPUT_HEIGHT, OUTPUT_WIDTH, StreamCommand, StreamEvent,
    SurfaceKind, VideoHeader,
};
use smithay::backend::allocator::Fourcc;
use smithay::backend::renderer::damage::OutputDamageTracker;
use smithay::backend::renderer::element::Kind;
use smithay::backend::renderer::element::surface::{
    WaylandSurfaceRenderElement, render_elements_from_surface_tree,
};
use smithay::backend::renderer::pixman::PixmanRenderer;
use smithay::backend::renderer::utils::on_commit_buffer_handler;
use smithay::backend::renderer::{Bind as _, Offscreen as _};
use smithay::desktop::utils::{bbox_from_surface_tree, send_frames_surface_tree, under_from_surface_tree};
use smithay::desktop::WindowSurfaceType;
use smithay::input::keyboard::{FilterResult, KeyboardTarget, KeysymHandle, ModifiersState, XkbConfig};
use smithay::input::pointer::{AxisFrame, ButtonEvent, CursorImageStatus, MotionEvent};
use smithay::input::{Seat, SeatHandler, SeatState};
use smithay::output::{Mode as OutputMode, Output, PhysicalProperties, Scale, Subpixel};
use smithay::reexports::calloop::channel::{Event as ChannelEvent, channel};
use smithay::reexports::calloop::generic::Generic;
use smithay::reexports::calloop::timer::{TimeoutAction, Timer};
use smithay::reexports::calloop::{EventLoop, Interest, LoopHandle, LoopSignal, Mode, PostAction};
use smithay::reexports::pixman;
use smithay::reexports::wayland_protocols::xdg::decoration::zv1::server::zxdg_toplevel_decoration_v1::Mode as DecorationMode;
use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel;
use smithay::reexports::wayland_server::backend::{ClientData, ClientId, DisconnectReason};
use smithay::reexports::wayland_server::protocol::wl_buffer::WlBuffer;
use smithay::reexports::wayland_server::protocol::wl_seat::WlSeat;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::reexports::wayland_server::{Client, Display, DisplayHandle};
use smithay::utils::{IsAlive, Logical, Point, Rectangle, SERIAL_COUNTER, Serial, Size, Transform};
use smithay::wayland::buffer::BufferHandler;
use smithay::wayland::compositor::{
    CompositorClientState, CompositorHandler, CompositorState, SurfaceAttributes, with_states,
};
use smithay::wayland::output::{OutputHandler, OutputManagerState};
use smithay::wayland::selection::SelectionHandler;
use smithay::wayland::selection::data_device::{
    ClientDndGrabHandler, DataDeviceHandler, DataDeviceState, ServerDndGrabHandler,
};
use smithay::wayland::shell::xdg::decoration::{XdgDecorationHandler, XdgDecorationState};
use smithay::wayland::shell::xdg::{
    PopupSurface, PositionerState, SurfaceCachedState, ToplevelSurface, XdgShellHandler,
    XdgShellState, XdgToplevelSurfaceData,
};
use smithay::wayland::shm::{ShmHandler, ShmState};
use smithay::wayland::socket::ListeningSocketSource;
use smithay::wayland::xwayland_shell::{XWaylandShellHandler, XWaylandShellState};
use smithay::xwayland::xwm::{Reorder, ResizeEdge as X11ResizeEdge, XwmId};
use smithay::xwayland::{X11Surface, X11Wm, XWayland, XWaylandClientData, XWaylandEvent, XwmHandler};
use smithay::{
    delegate_compositor, delegate_data_device, delegate_output, delegate_seat, delegate_shm,
    delegate_xdg_decoration, delegate_xdg_shell, delegate_xwayland_shell,
};

use crate::encode::{EncoderHandle, Picture, load_api};
use crate::input;
use crate::link::{self, LinkSender};
use crate::xauth;

/// The first X display number tried. High, so the abstract socket does not
/// shadow a real X server's display on this machine (`:0`, `:1`).
pub const FIRST_X_DISPLAY: u32 = 64;
/// How many numbers after it are tried (the patched smithay tries 64).
const X_DISPLAYS: u32 = 64;

/// How often windows with new commits are rendered and frame callbacks
/// sent: 60 Hz at most.
const FRAME: Duration = Duration::from_millis(16);

/// One app window, as the compositor tracks it.
struct Win {
    id: u32,
    shell: Shell,
    announced: bool,
    title: String,
    /// The visible size and (a popup) the offset from the parent last told
    /// to the core.
    size: (u32, u32),
    offset: (i32, i32),
    canvas: Option<Canvas>,
    encoder: Option<EncoderHandle>,
    /// A keyframe is owed: a viewer attached, the seat moved, or a picture
    /// was dropped on the link.
    owes_keyframe: Arc<AtomicBool>,
    /// Something committed since the last render.
    dirty: bool,
}

#[derive(Clone)]
enum Shell {
    Toplevel(ToplevelSurface),
    Popup(PopupSurface),
    X11(X11Surface),
}

impl Shell {
    fn wl_surface(&self) -> Option<WlSurface> {
        match self {
            Shell::Toplevel(t) => Some(t.wl_surface().clone()),
            Shell::Popup(p) => Some(p.wl_surface().clone()),
            Shell::X11(x) => x.wl_surface(),
        }
    }

    fn kind(&self) -> SurfaceKind {
        match self {
            Shell::Toplevel(_) => SurfaceKind::Toplevel,
            Shell::Popup(_) => SurfaceKind::Popup,
            Shell::X11(x) if x.is_override_redirect() => SurfaceKind::Popup,
            Shell::X11(_) => SurfaceKind::Toplevel,
        }
    }

    fn alive(&self) -> bool {
        match self {
            Shell::Toplevel(t) => t.alive(),
            Shell::Popup(p) => p.alive(),
            Shell::X11(x) => x.alive(),
        }
    }
}

/// One window's offscreen image and its damage tracker.
struct Canvas {
    image: pixman::Image<'static, 'static>,
    damage: OutputDamageTracker,
    /// The image's size: the visible size rounded up to even.
    size: (u32, u32),
}

/// Who has the keyboard: a Wayland surface, or an X11 window (whose input
/// focus has to be set through the X server as well).
#[derive(Debug, Clone, PartialEq)]
pub enum KeyboardFocus {
    Wl(WlSurface),
    X11(X11Surface),
}

impl IsAlive for KeyboardFocus {
    fn alive(&self) -> bool {
        match self {
            KeyboardFocus::Wl(s) => s.alive(),
            KeyboardFocus::X11(x) => x.alive(),
        }
    }
}

impl smithay::wayland::seat::WaylandFocus for KeyboardFocus {
    fn wl_surface(&self) -> Option<std::borrow::Cow<'_, WlSurface>> {
        match self {
            KeyboardFocus::Wl(s) => Some(std::borrow::Cow::Borrowed(s)),
            KeyboardFocus::X11(x) => x.wl_surface().map(std::borrow::Cow::Owned),
        }
    }
}

impl KeyboardTarget<State> for KeyboardFocus {
    fn enter(
        &self,
        seat: &Seat<State>,
        data: &mut State,
        keys: Vec<KeysymHandle<'_>>,
        serial: Serial,
    ) {
        match self {
            KeyboardFocus::Wl(s) => KeyboardTarget::enter(s, seat, data, keys, serial),
            KeyboardFocus::X11(x) => KeyboardTarget::enter(x, seat, data, keys, serial),
        }
    }

    fn leave(&self, seat: &Seat<State>, data: &mut State, serial: Serial) {
        match self {
            KeyboardFocus::Wl(s) => KeyboardTarget::leave(s, seat, data, serial),
            KeyboardFocus::X11(x) => KeyboardTarget::leave(x, seat, data, serial),
        }
    }

    fn key(
        &self,
        seat: &Seat<State>,
        data: &mut State,
        key: KeysymHandle<'_>,
        state: smithay::backend::input::KeyState,
        serial: Serial,
        time: u32,
    ) {
        match self {
            KeyboardFocus::Wl(s) => KeyboardTarget::key(s, seat, data, key, state, serial, time),
            KeyboardFocus::X11(x) => KeyboardTarget::key(x, seat, data, key, state, serial, time),
        }
    }

    fn modifiers(
        &self,
        seat: &Seat<State>,
        data: &mut State,
        modifiers: ModifiersState,
        serial: Serial,
    ) {
        match self {
            KeyboardFocus::Wl(s) => KeyboardTarget::modifiers(s, seat, data, modifiers, serial),
            KeyboardFocus::X11(x) => KeyboardTarget::modifiers(x, seat, data, modifiers, serial),
        }
    }
}

/// Per-client state for the app's own Wayland clients.
#[derive(Default)]
struct ClientState {
    compositor: CompositorClientState,
}

impl ClientData for ClientState {
    fn initialized(&self, _client_id: ClientId) {}
    fn disconnected(&self, _client_id: ClientId, _reason: DisconnectReason) {}
}

/// Everything the compositor holds.
pub struct State {
    dh: DisplayHandle,
    handle: LoopHandle<'static, State>,
    signal: LoopSignal,
    compositor: CompositorState,
    xdg_shell: XdgShellState,
    shm: ShmState,
    seat_state: SeatState<State>,
    seat: Seat<State>,
    _output_manager: OutputManagerState,
    data_device: DataDeviceState,
    _decoration: XdgDecorationState,
    xwayland_shell: XWaylandShellState,
    output: Output,
    renderer: PixmanRenderer,
    windows: BTreeMap<u32, Win>,
    next_id: u32,
    xwm: Option<X11Wm>,
    link: LinkSender,
    started: Instant,
    spec: LaunchSpec,
    child: Option<Child>,
    x11_display: Option<u32>,
    xauthority: Option<PathBuf>,
    wayland_display: String,
    runtime_dir: PathBuf,
    /// Set when the app has exited or the core has gone: the loop stops.
    exiting: Option<String>,
    /// Pointer buttons the app has been told are down, so they can all be
    /// released when the device that pressed them is gone.
    buttons: BTreeSet<u32>,
    /// The toplevel told it is activated, so it can be told it no longer
    /// is when another takes the keyboard.
    activated: Option<u32>,
    /// The app's process group (it is started as its own group's leader).
    app_group: Option<i32>,
}

/// Where this compositor keeps its sockets and the X authority file: a
/// fresh owner-only directory, `$TMPDIR/runtime-XXXXXX`, of its own. The
/// plan's TMPDIR is the app's private scratch directory, which every
/// instance of the same app shares, so a fixed name there would let a
/// second instance overwrite the first's X cookie and share its sockets.
/// Removed by `main` when the compositor ends.
pub fn runtime_dir() -> Result<PathBuf> {
    use std::os::unix::ffi::OsStringExt as _;
    let tmp = std::env::var_os("TMPDIR")
        .map(PathBuf::from)
        .context("TMPDIR is not set; the core sets it on every launch plan")?;
    let template = tmp.join("runtime-XXXXXX");
    let mut bytes = template.into_os_string().into_vec();
    bytes.push(0);
    // SAFETY: a NUL-terminated buffer mkdtemp rewrites in place.
    let made = unsafe { libc::mkdtemp(bytes.as_mut_ptr().cast()) };
    if made.is_null() {
        return Err(std::io::Error::last_os_error())
            .with_context(|| format!("cannot create a runtime directory in {}", tmp.display()));
    }
    bytes.pop();
    // mkdtemp makes it 0700.
    Ok(PathBuf::from(std::ffi::OsString::from_vec(bytes)))
}

/// Set by `SIGTERM`: the core asked this compositor to stop (a force quit,
/// a lockdown).
static STOP: AtomicBool = AtomicBool::new(false);

extern "C" fn on_stop(_: libc::c_int) {
    STOP.store(true, Ordering::SeqCst);
}

/// Make this process the reaper of everything the app starts, and turn
/// `SIGTERM` into an orderly stop, so that stopping the compositor stops
/// the app and every process it started, not only its direct child.
///
/// Called by `main` before any thread exists.
pub fn become_supervisor() -> Result<()> {
    // SAFETY: plain prctl and sigaction calls on this process; the handler
    // only stores to an atomic.
    unsafe {
        if libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1) != 0 {
            return Err(std::io::Error::last_os_error()).context("cannot become a subreaper");
        }
        let mut action: libc::sigaction = std::mem::zeroed();
        action.sa_sigaction = on_stop as extern "C" fn(libc::c_int) as usize;
        libc::sigemptyset(&mut action.sa_mask);
        if libc::sigaction(libc::SIGTERM, &action, std::ptr::null_mut()) != 0 {
            return Err(std::io::Error::last_os_error()).context("cannot handle SIGTERM");
        }
    }
    Ok(())
}

/// This process's children, from every thread's `children` list (an
/// orphan taken in by a subreaper may be adopted by any of its threads).
fn children() -> Vec<i32> {
    let mut out = BTreeSet::new();
    if let Ok(tasks) = std::fs::read_dir("/proc/self/task") {
        for task in tasks.flatten() {
            if let Ok(list) = std::fs::read_to_string(task.path().join("children")) {
                out.extend(
                    list.split_whitespace()
                        .filter_map(|p| p.parse::<i32>().ok()),
                );
            }
        }
    }
    out.into_iter().collect()
}

/// Reap every child that has exited, without waiting.
fn reap_exited() {
    loop {
        let mut status = 0;
        // SAFETY: waitpid on our own children only.
        let pid = unsafe { libc::waitpid(-1, &mut status, libc::WNOHANG) };
        if pid <= 0 {
            return;
        }
    }
}

/// End everything this compositor has running: the app's process group,
/// and every child, including orphans of the app's that were handed to
/// this process as their subreaper. `SIGTERM` first, `SIGKILL` for what is
/// left after a second, and reaped, for at most two seconds in all.
fn end_descendants(app_group: Option<i32>) {
    let started = Instant::now();
    loop {
        reap_exited();
        let kids = children();
        if kids.is_empty() {
            return;
        }
        let elapsed = started.elapsed();
        if elapsed > Duration::from_secs(2) {
            tracing::warn!(left = ?kids, "processes the app started outlived the compositor's stop");
            return;
        }
        let signal = if elapsed < Duration::from_secs(1) {
            libc::SIGTERM
        } else {
            libc::SIGKILL
        };
        // SAFETY: signals to our own process group and our own children.
        unsafe {
            if let Some(group) = app_group {
                libc::killpg(group, signal);
            }
            for pid in kids {
                libc::kill(pid, signal);
            }
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Run the compositor until the app exits, the core goes away or the core
/// sends `SIGTERM`; then end the app and everything it started.
///
/// `runtime` must already be `XDG_RUNTIME_DIR` in this process's
/// environment (set by `main`, before any thread exists), because the
/// listening socket is created there.
pub fn run(spec: LaunchSpec, core: UnixStream, runtime: PathBuf) -> Result<()> {
    let mut event_loop: EventLoop<'static, State> = EventLoop::try_new()?;
    let display: Display<State> = Display::new()?;
    let dh = display.handle();

    let compositor = CompositorState::new::<State>(&dh);
    let xdg_shell = XdgShellState::new::<State>(&dh);
    let shm = ShmState::new::<State>(&dh, vec![]);
    let mut seat_state = SeatState::new();
    let mut seat = seat_state.new_wl_seat(&dh, "seat0");
    seat.add_keyboard(XkbConfig::default(), 400, 30)
        .context("cannot load the US keymap")?;
    seat.add_pointer();
    let output_manager = OutputManagerState::new_with_xdg_output::<State>(&dh);
    let data_device = DataDeviceState::new::<State>(&dh);
    let decoration = XdgDecorationState::new::<State>(&dh);
    let xwayland_shell = XWaylandShellState::new::<State>(&dh);

    // One output like a 1080p monitor at 96 dpi: 1920 px over 508 mm.
    let output = Output::new(
        "deckjack".into(),
        PhysicalProperties {
            size: (508, 286).into(),
            subpixel: Subpixel::Unknown,
            make: "Deckjack".into(),
            model: "Stream".into(),
        },
    );
    let mode = OutputMode {
        size: (OUTPUT_WIDTH as i32, OUTPUT_HEIGHT as i32).into(),
        refresh: 60_000,
    };
    output.change_current_state(
        Some(mode),
        Some(Transform::Normal),
        Some(Scale::Integer(1)),
        Some((0, 0).into()),
    );
    output.set_preferred(mode);
    let _global = output.create_global::<State>(&dh);

    let socket = ListeningSocketSource::new_auto().context("cannot open the Wayland socket")?;
    let wayland_display = socket.socket_name().to_string_lossy().into_owned();
    event_loop
        .handle()
        .insert_source(socket, |stream, _, state: &mut State| {
            if let Err(e) = state
                .dh
                .insert_client(stream, Arc::new(ClientState::default()))
            {
                tracing::warn!(error = %e, "cannot add a Wayland client");
            }
        })
        .map_err(|e| anyhow::anyhow!("cannot watch the Wayland socket: {e}"))?;
    event_loop
        .handle()
        .insert_source(
            Generic::new(display, Interest::READ, Mode::Level),
            |_, display, state: &mut State| {
                // SAFETY: the display is not dropped while the loop runs.
                unsafe {
                    if let Err(e) = display.get_mut().dispatch_clients(state) {
                        tracing::warn!(error = %e, "dispatching Wayland clients failed");
                    }
                }
                Ok(PostAction::Continue)
            },
        )
        .map_err(|e| anyhow::anyhow!("cannot watch the display: {e}"))?;

    // The link: a reader thread feeding the loop, a writer thread draining
    // what the loop and the encoders queue.
    let gone = Arc::new(AtomicBool::new(false));
    let sender = link::spawn_writer(core.try_clone()?, Arc::clone(&gone))?;
    let (commands, command_rx) = channel::<Option<StreamCommand>>();
    link::spawn_reader(core, move |command| {
        let _ = commands.send(command);
    })?;
    event_loop
        .handle()
        .insert_source(command_rx, |event, _, state: &mut State| match event {
            ChannelEvent::Msg(Some(command)) => state.on_command(command),
            ChannelEvent::Msg(None) | ChannelEvent::Closed => {
                state.exit("the core closed the link");
            }
        })
        .map_err(|e| anyhow::anyhow!("cannot watch the link: {e}"))?;

    let mut state = State {
        dh: dh.clone(),
        handle: event_loop.handle(),
        signal: event_loop.get_signal(),
        compositor,
        xdg_shell,
        shm,
        seat_state,
        seat,
        _output_manager: output_manager,
        data_device,
        _decoration: decoration,
        xwayland_shell,
        output,
        renderer: PixmanRenderer::new().context("cannot start the pixman renderer")?,
        windows: BTreeMap::new(),
        next_id: 1,
        xwm: None,
        link: sender,
        started: Instant::now(),
        spec,
        child: None,
        x11_display: None,
        xauthority: None,
        wayland_display,
        runtime_dir: runtime,
        exiting: None,
        buttons: BTreeSet::new(),
        activated: None,
        app_group: None,
    };

    // Fail at startup, not at the first window, when there is no encoder.
    load_api()?;

    event_loop
        .handle()
        .insert_source(Timer::from_duration(FRAME), |_, _, state: &mut State| {
            state.tick();
            TimeoutAction::ToDuration(FRAME)
        })
        .map_err(|e| anyhow::anyhow!("cannot start the frame timer: {e}"))?;

    if state.spec.x11 {
        state.start_xwayland()?;
    } else {
        state.launch()?;
    }

    while state.exiting.is_none() {
        event_loop.dispatch(Some(FRAME), &mut state)?;
        state.dh.flush_clients()?;
        if gone.load(Ordering::SeqCst) {
            state.exit("the link to the core failed");
        }
        if STOP.load(Ordering::SeqCst) {
            state.exit("stopped by the core");
        }
    }
    let status = state.exiting.clone().unwrap_or_default();
    tracing::info!(%status, "compositor exiting");
    state.link.event(&StreamEvent::Exited { status });
    // The app, and everything it started: its direct child dies with this
    // process anyway (PR_SET_PDEATHSIG), the rest only if it is ended here.
    drop(state.child.take());
    end_descendants(state.app_group);
    // Give the writer a moment to put the last event on the wire.
    std::thread::sleep(Duration::from_millis(50));
    Ok(())
}

/// Environment that keeps an app (and Xwayland) on CPU rendering.
///
/// The sandbox denies `/dev/dri` unless a manifest asks for the GPU, so a
/// toolkit that probes for GL finds none, and on a machine with NVIDIA's
/// driver libglvnd tries NVIDIA's EGL first, which crashed Xwayland during
/// the spike. Pointing libglvnd at Mesa (llvmpipe, no device needed) and
/// asking GTK and Qt for their software paths avoids both.
fn software_rendering_env() -> Vec<(String, String)> {
    let mut env = vec![
        ("LIBGL_ALWAYS_SOFTWARE".to_string(), "1".to_string()),
        ("GSK_RENDERER".to_string(), "cairo".to_string()),
        ("QT_QUICK_BACKEND".to_string(), "software".to_string()),
    ];
    let mesa = "/usr/share/glvnd/egl_vendor.d/50_mesa.json";
    if std::path::Path::new(mesa).exists() {
        env.push((
            "__EGL_VENDOR_LIBRARY_FILENAMES".to_string(),
            mesa.to_string(),
        ));
    }
    env
}

impl State {
    fn exit(&mut self, why: &str) {
        if self.exiting.is_none() {
            self.exiting = Some(why.to_string());
            self.signal.stop();
        }
    }

    /// Start XWayland on an abstract socket with a cookie; the app is
    /// launched once it is ready.
    fn start_xwayland(&mut self) -> Result<()> {
        let auth = self.runtime_dir.join("xauthority");
        let cookie = xauth::cookie()?;
        xauth::write(
            &auth,
            FIRST_X_DISPLAY..FIRST_X_DISPLAY + X_DISPLAYS,
            &cookie,
        )?;
        let mut envs: Vec<(String, String)> = Vec::new();
        for key in ["HOME", "TMPDIR", "XDG_RUNTIME_DIR"] {
            if let Ok(value) = std::env::var(key) {
                envs.push((key.to_string(), value));
            }
        }
        envs.extend(software_rendering_env());
        let (xwayland, client) = XWayland::spawn_abstract(
            &self.dh,
            FIRST_X_DISPLAY,
            envs,
            // `-shm`: buffers in shared memory, no glamor. There is no GPU
            // in the sandbox, and probing for one crashed Xwayland inside
            // NVIDIA's EGL library on the development machine (S1).
            [
                "-shm".to_string(),
                "-auth".to_string(),
                auth.to_string_lossy().into_owned(),
            ],
            Stdio::null(),
            Stdio::inherit(),
            |_| {},
        )
        .context("cannot start Xwayland")?;
        self.xauthority = Some(auth);
        self.handle
            .insert_source(xwayland, move |event, _, state: &mut State| match event {
                XWaylandEvent::Ready {
                    x11_socket,
                    display_number,
                } => {
                    match X11Wm::start_wm(state.handle.clone(), x11_socket, client.clone()) {
                        Ok(wm) => state.xwm = Some(wm),
                        Err(e) => {
                            tracing::error!(error = %e, "cannot start the X11 window manager");
                            state.exit("the X11 window manager failed to start");
                            return;
                        }
                    }
                    state.x11_display = Some(display_number);
                    if let Err(e) = state.launch() {
                        tracing::error!(error = %e, "cannot launch the app");
                        state.exit("the app could not be launched");
                    }
                }
                XWaylandEvent::Error => state.exit("Xwayland failed to start"),
            })
            .map_err(|e| anyhow::anyhow!("cannot watch Xwayland: {e}"))?;
        Ok(())
    }

    /// Start the app, as this process's child: it inherits the Landlock
    /// domain and the seccomp filter the core put this process under.
    fn launch(&mut self) -> Result<()> {
        let (program, args) = self
            .spec
            .exec
            .split_first()
            .context("the launch spec names no program")?;
        let mut command = Command::new(program);
        command.args(args).env_clear();
        for key in ["HOME", "TMPDIR", "LANG"] {
            if let Ok(value) = std::env::var(key) {
                command.env(key, value);
            }
        }
        command
            .env("PATH", "/usr/local/bin:/usr/bin:/bin")
            .env("XDG_RUNTIME_DIR", &self.runtime_dir)
            .env("WAYLAND_DISPLAY", &self.wayland_display)
            .env("XDG_SESSION_TYPE", "wayland")
            // Scale 1, everywhere: the app never learns client density.
            .env("GDK_SCALE", "1")
            .env("QT_SCALE_FACTOR", "1")
            .env("QT_QPA_PLATFORM", "wayland;xcb")
            .envs(software_rendering_env());
        if let (Some(display), Some(auth)) = (self.x11_display, self.xauthority.as_ref()) {
            command
                .env("DISPLAY", format!(":{display}"))
                .env("XAUTHORITY", auth);
        }
        if self.spec.x11
            && self.x11_display.is_some()
            && std::env::var_os("DECKJACK_STREAM_PREFER_X11").is_some()
        {
            command.env_remove("WAYLAND_DISPLAY");
        }
        command.stdin(Stdio::null());
        // The app dies with the compositor even when the compositor is
        // killed outright (PR_SET_PDEATHSIG). That covers the app only, not
        // what the app starts, so the app also leads a process group of its
        // own, and the compositor is the subreaper of anything orphaned
        // under it (`become_supervisor`): an orderly stop ends them all.
        // SAFETY: prctl and setpgid are async-signal-safe and touch only
        // this child.
        unsafe {
            use std::os::unix::process::CommandExt as _;
            command.pre_exec(|| {
                if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                if libc::setpgid(0, 0) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let child = command
            .spawn()
            .with_context(|| format!("cannot start {program}"))?;
        tracing::info!(app = %self.spec.app, pid = child.id(), "app started");
        self.app_group = Some(child.id() as i32);
        self.child = Some(child);
        self.link.event(&StreamEvent::Ready {
            x11_display: self.x11_display,
        });
        Ok(())
    }

    // ---- windows ----------------------------------------------------

    fn add_window(&mut self, shell: Shell) -> u32 {
        let id = self.next_id;
        self.next_id += 1;
        self.windows.insert(
            id,
            Win {
                id,
                shell,
                announced: false,
                title: String::new(),
                size: (0, 0),
                offset: (0, 0),
                canvas: None,
                encoder: None,
                owes_keyframe: Arc::new(AtomicBool::new(true)),
                dirty: true,
            },
        );
        id
    }

    fn window_for_surface(&self, surface: &WlSurface) -> Option<u32> {
        // The root of a surface tree: a subsurface commit marks its window.
        let mut root = surface.clone();
        while let Some(parent) = smithay::wayland::compositor::get_parent(&root) {
            root = parent;
        }
        self.windows
            .values()
            .find(|w| w.shell.wl_surface().as_ref() == Some(&root))
            .map(|w| w.id)
    }

    fn window_for_x11(&self, x11: &X11Surface) -> Option<u32> {
        self.windows
            .values()
            .find(|w| matches!(&w.shell, Shell::X11(x) if x == x11))
            .map(|w| w.id)
    }

    /// Who each window's parent is, right now.
    fn parents(&self) -> Vec<(u32, Option<u32>)> {
        self.windows
            .values()
            .map(|w| (w.id, self.parent_of(w)))
            .collect()
    }

    /// A window goes, and its popups go with it. Which windows those are
    /// is worked out first, while the parent can still be looked up.
    fn remove_window(&mut self, id: u32) {
        let doomed = input::with_descendants(id, &self.parents());
        for gone in doomed {
            let Some(w) = self.windows.remove(&gone) else {
                continue;
            };
            if gone != id
                && let Shell::Popup(p) = &w.shell
            {
                // Tell the client its menu is done, rather than leave it
                // mapped against a window that is not there.
                p.send_popup_done();
            }
            if w.announced {
                self.link.event(&StreamEvent::Unmapped { surface: gone });
            }
        }
        if self.activated == Some(id) {
            self.activated = None;
        }
    }

    /// A window stops being shown (its buffer went, or an X11 window was
    /// unmapped), and so do the popups placed against it.
    fn unannounce(&mut self, id: u32) {
        let hidden = input::with_descendants(id, &self.parents());
        for gone in hidden {
            let Some(w) = self.windows.get_mut(&gone) else {
                continue;
            };
            if gone != id
                && let Shell::Popup(p) = &w.shell
            {
                p.send_popup_done();
            }
            if w.announced {
                w.announced = false;
                w.canvas = None;
                w.encoder = None;
                self.link.event(&StreamEvent::Unmapped { surface: gone });
            }
        }
    }

    fn parent_of(&self, w: &Win) -> Option<u32> {
        match &w.shell {
            Shell::Popup(p) => p
                .get_parent_surface()
                .and_then(|parent| self.window_for_surface(&parent)),
            Shell::X11(x) if x.is_override_redirect() => self.x11_popup_parent(x),
            _ => None,
        }
    }

    /// The window an override-redirect X11 window (a menu) belongs to: the
    /// one it is transient for, else the X11 toplevel that last had the
    /// keyboard, else any X11 toplevel.
    fn x11_popup_parent(&self, x: &X11Surface) -> Option<u32> {
        if let Some(parent) = x.is_transient_for()
            && let Some(w) = self
                .windows
                .values()
                .find(|w| matches!(&w.shell, Shell::X11(p) if p.window_id() == parent))
        {
            return Some(w.id);
        }
        let focused = self.seat.get_keyboard().and_then(|k| k.current_focus());
        if let Some(KeyboardFocus::X11(f)) = focused
            && let Some(id) = self.window_for_x11(&f)
        {
            return Some(id);
        }
        self.windows
            .values()
            .find(|w| matches!(&w.shell, Shell::X11(p) if !p.is_override_redirect()))
            .map(|w| w.id)
    }

    /// A window's geometry within its own surface tree: the xdg window
    /// geometry when the client set one (it leaves out shadows), clipped to
    /// the bounding box of the tree as xdg-shell requires, else that
    /// bounding box. Its size is then held to the output
    /// ([`input::visible_size`]) by everything that sizes a canvas from it.
    fn geometry(&self, w: &Win) -> Option<Rectangle<i32, Logical>> {
        let surface = w.shell.wl_surface()?;
        if let Shell::X11(x) = &w.shell {
            let g = x.geometry();
            return Some(Rectangle::new((0, 0).into(), g.size));
        }
        let set = with_states(&surface, |states| {
            states
                .cached_state
                .get::<SurfaceCachedState>()
                .current()
                .geometry
        });
        let bbox = bbox_from_surface_tree(&surface, (0, 0));
        Some(
            set.filter(|g| g.size.w > 0 && g.size.h > 0)
                .and_then(|g| g.intersection(bbox))
                .unwrap_or(bbox),
        )
    }

    fn has_buffer(surface: &WlSurface) -> bool {
        with_states(surface, |states| {
            states
                .data_map
                .get::<smithay::backend::renderer::utils::RendererSurfaceStateUserData>()
                .is_some_and(|s| s.lock().unwrap().buffer().is_some())
        })
    }

    fn title_of(&self, w: &Win) -> String {
        match &w.shell {
            Shell::Toplevel(t) => with_states(t.wl_surface(), |states| {
                states
                    .data_map
                    .get::<XdgToplevelSurfaceData>()
                    .and_then(|d| d.lock().unwrap().title.clone())
            })
            .unwrap_or_else(|| self.spec.app.clone()),
            Shell::X11(x) => {
                let t = x.title();
                if t.is_empty() {
                    self.spec.app.clone()
                } else {
                    t
                }
            }
            Shell::Popup(_) => String::new(),
        }
    }

    /// Tell the core about windows that appeared, changed or went.
    fn announce(&mut self) {
        let ids: Vec<u32> = self.windows.keys().copied().collect();
        for id in ids {
            let Some(w) = self.windows.get(&id) else {
                continue;
            };
            if !w.shell.alive() {
                self.remove_window(id);
                continue;
            }
            let Some(surface) = w.shell.wl_surface() else {
                continue;
            };
            let mapped = Self::has_buffer(&surface)
                && match &w.shell {
                    Shell::X11(x) => x.is_mapped() || x.is_override_redirect(),
                    _ => true,
                };
            if !mapped {
                if w.announced {
                    self.unannounce(id);
                }
                continue;
            }
            let Some(geometry) = self.geometry(w) else {
                continue;
            };
            let size = input::visible_size(geometry.size.w, geometry.size.h);
            let parent = self.parent_of(w);
            let offset = self.popup_offset(w, parent);
            let title = self.title_of(w);
            let kind = w.shell.kind();
            if kind == SurfaceKind::Popup && parent.is_none() {
                // A popup whose parent is not a window (yet): nothing to
                // place it against.
                continue;
            }
            let w = self.windows.get_mut(&id).unwrap();
            if !w.announced {
                w.announced = true;
                w.size = size;
                w.offset = offset;
                w.title = title.clone();
                w.owes_keyframe.store(true, Ordering::SeqCst);
                self.output.enter(&surface);
                self.link.event(&StreamEvent::Mapped {
                    surface: id,
                    kind,
                    parent,
                    title,
                    width: size.0,
                    height: size.1,
                    x: offset.0,
                    y: offset.1,
                });
            } else if w.size != size || w.offset != offset || w.title != title {
                w.size = size;
                w.offset = offset;
                w.title = title.clone();
                self.link.event(&StreamEvent::Changed {
                    surface: id,
                    title,
                    width: size.0,
                    height: size.1,
                    x: offset.0,
                    y: offset.1,
                });
            }
        }
    }

    /// Where a popup sits relative to its parent's top-left corner.
    fn popup_offset(&self, w: &Win, parent: Option<u32>) -> (i32, i32) {
        match &w.shell {
            Shell::Popup(p) => {
                let g = p.with_pending_state(|s| s.geometry);
                (g.loc.x, g.loc.y)
            }
            Shell::X11(x) if x.is_override_redirect() => {
                let mine = x.geometry().loc;
                let theirs = parent
                    .and_then(|id| self.windows.get(&id))
                    .and_then(|pw| match &pw.shell {
                        Shell::X11(px) => Some(px.geometry().loc),
                        _ => None,
                    })
                    .unwrap_or_default();
                (mine.x - theirs.x, mine.y - theirs.y)
            }
            _ => (0, 0),
        }
    }

    // ---- rendering ----------------------------------------------------

    /// Render every window that committed, encode what changed, and send
    /// frame callbacks.
    fn tick(&mut self) {
        self.reap_child();
        self.announce();
        let elapsed = self.started.elapsed();
        let ids: Vec<u32> = self.windows.keys().copied().collect();
        for id in ids {
            let force = self
                .windows
                .get(&id)
                .is_some_and(|w| w.owes_keyframe.load(Ordering::SeqCst));
            let dirty = self.windows.get(&id).is_some_and(|w| w.dirty);
            if (dirty || force)
                && let Err(e) = self.render(id, force)
            {
                tracing::warn!(surface = id, error = %e, "cannot render a window");
            }
            if let Some(surface) = self.windows.get(&id).and_then(|w| w.shell.wl_surface()) {
                let output = self.output.clone();
                send_frames_surface_tree(
                    &surface,
                    &self.output,
                    elapsed,
                    Some(Duration::ZERO),
                    |_, _| Some(output.clone()),
                );
            }
        }
    }

    fn render(&mut self, id: u32, force: bool) -> Result<()> {
        let Some(w) = self.windows.get(&id) else {
            return Ok(());
        };
        if !w.announced {
            return Ok(());
        }
        let Some(surface) = w.shell.wl_surface() else {
            return Ok(());
        };
        let Some(geometry) = self.geometry(w) else {
            return Ok(());
        };
        let visible = input::visible_size(geometry.size.w, geometry.size.h);
        let size = input::encoded_size(visible);

        let elements: Vec<WaylandSurfaceRenderElement<PixmanRenderer>> =
            render_elements_from_surface_tree(
                &mut self.renderer,
                &surface,
                (-geometry.loc.x, -geometry.loc.y),
                1.0,
                1.0,
                Kind::Unspecified,
            );

        let w = self.windows.get_mut(&id).unwrap();
        w.dirty = false;
        if w.canvas.as_ref().map(|c| c.size) != Some(size) {
            let image = self
                .renderer
                .create_buffer(Fourcc::Argb8888, (size.0 as i32, size.1 as i32).into())
                .map_err(|e| anyhow::anyhow!("cannot create an image: {e:?}"))?;
            w.canvas = Some(Canvas {
                image,
                damage: OutputDamageTracker::new(
                    Size::from((size.0 as i32, size.1 as i32)),
                    1.0,
                    Transform::Normal,
                ),
                size,
            });
        }
        let canvas = w.canvas.as_mut().unwrap();
        let damaged = {
            let mut target = self
                .renderer
                .bind(&mut canvas.image)
                .map_err(|e| anyhow::anyhow!("cannot bind an image: {e:?}"))?;
            let result = canvas
                .damage
                .render_output(
                    &mut self.renderer,
                    &mut target,
                    1,
                    &elements,
                    [0.0, 0.0, 0.0, 1.0],
                )
                .map_err(|e| anyhow::anyhow!("rendering failed: {e:?}"))?;
            result.damage.is_some_and(|d| !d.is_empty())
        };
        if !damaged && !force {
            return Ok(());
        }
        let keyframe = w.owes_keyframe.swap(false, Ordering::SeqCst);
        // Copy out of the image: the encoder thread converts and encodes
        // while the loop goes on drawing into the same image.
        let bytes = size.0 as usize * size.1 as usize * 4;
        let stride = canvas.image.stride();
        let mut bgra = vec![0u8; bytes];
        // SAFETY: the image is `size` pixels of 4 bytes with `stride` bytes
        // per row, alive for this borrow, and nothing writes to it here.
        unsafe {
            let base = canvas.image.data() as *const u8;
            for row in 0..size.1 as usize {
                let src = std::slice::from_raw_parts(base.add(row * stride), size.0 as usize * 4);
                bgra[row * size.0 as usize * 4..(row + 1) * size.0 as usize * 4]
                    .copy_from_slice(src);
            }
        }
        if w.encoder.is_none() {
            let link = self.link.clone();
            let owes = Arc::clone(&w.owes_keyframe);
            let started = self.started;
            // Set when a picture could not be queued: every delta after it
            // references a picture the viewer never got, so none goes out
            // until a keyframe does.
            let mut broken = false;
            w.encoder = Some(EncoderHandle::spawn(
                format!("encode-{id}"),
                load_api,
                move |encoded| {
                    if broken && !encoded.keyframe {
                        owes.store(true, Ordering::SeqCst);
                        return;
                    }
                    let header = VideoHeader {
                        surface: id,
                        flags: if encoded.keyframe { FLAG_KEYFRAME } else { 0 },
                        width: encoded.visible.0.min(u16::MAX as u32) as u16,
                        height: encoded.visible.1.min(u16::MAX as u32) as u16,
                        timestamp: started.elapsed().as_millis() as u32,
                    };
                    if crate::trace_timing() {
                        tracing::info!(
                            surface = id,
                            keyframe = encoded.keyframe,
                            bytes = encoded.annex_b.len(),
                            millis = encoded.millis,
                            "encoded"
                        );
                    }
                    if link.video(&header.link_bytes(&encoded.annex_b)) {
                        broken = false;
                    } else {
                        broken = true;
                        owes.store(true, Ordering::SeqCst);
                    }
                },
            )?);
        }
        if let Some(encoder) = &w.encoder {
            encoder.submit(Picture {
                bgra,
                width: size.0,
                height: size.1,
                visible,
                keyframe,
            });
        }
        Ok(())
    }

    fn reap_child(&mut self) {
        if let Some(child) = self.child.as_mut() {
            match child.try_wait() {
                Ok(Some(status)) => {
                    let status = status.to_string();
                    self.child = None;
                    self.exit(&format!("the app exited: {status}"));
                }
                Ok(None) => {}
                Err(e) => tracing::warn!(error = %e, "cannot check the app"),
            }
        }
    }

    // ---- commands from the core -----------------------------------------

    fn on_command(&mut self, command: StreamCommand) {
        match command {
            StreamCommand::Keyframe { surface } => {
                if let Some(w) = self.windows.get(&surface) {
                    w.owes_keyframe.store(true, Ordering::SeqCst);
                }
            }
            StreamCommand::Close { surface } => {
                if let Some(w) = self.windows.get(&surface) {
                    match &w.shell {
                        Shell::Toplevel(t) => t.send_close(),
                        Shell::Popup(p) => p.send_popup_done(),
                        // An override-redirect window (a menu, a tooltip)
                        // has no WM_DELETE_WINDOW to ask with, and
                        // `X11Surface::close` would then destroy the window
                        // under the client, which an Xlib app can die of.
                        // Its toplevel dismisses it (a press there).
                        Shell::X11(x) if x.is_override_redirect() => {
                            tracing::debug!(surface, "not closing an X11 menu or tooltip");
                        }
                        Shell::X11(x) => {
                            let _ = x.close();
                        }
                    }
                }
            }
            StreamCommand::ReleaseAll => self.release_all(),
            StreamCommand::Resize {
                surface,
                width,
                height,
            } => {
                let (width, height) = input::clamp_size(width, height);
                if let Some(w) = self.windows.get(&surface) {
                    match &w.shell {
                        Shell::Toplevel(t) => {
                            t.with_pending_state(|s| {
                                s.size = Some((width as i32, height as i32).into());
                            });
                            t.send_pending_configure();
                        }
                        Shell::X11(x) if !x.is_override_redirect() => {
                            let mut g = x.geometry();
                            g.size = (width as i32, height as i32).into();
                            let _ = x.configure(g);
                        }
                        _ => {}
                    }
                }
            }
            StreamCommand::Input { surface, input } => self.on_input(surface, input),
        }
    }

    /// Release every key and button the app sees held. The keyboard and
    /// pointer are the seat's, not a window's, so this is too.
    fn release_all(&mut self) {
        let serial = SERIAL_COUNTER.next_serial();
        let time = self.started.elapsed().as_millis() as u32;
        if let Some(k) = self.seat.get_keyboard() {
            for code in k.pressed_keys() {
                k.input::<(), _>(
                    self,
                    code,
                    smithay::backend::input::KeyState::Released,
                    serial,
                    time,
                    |_, _, _| FilterResult::Forward,
                );
            }
        }
        let buttons = std::mem::take(&mut self.buttons);
        if let Some(p) = self.seat.get_pointer()
            && !buttons.is_empty()
        {
            for button in buttons {
                p.button(
                    self,
                    &ButtonEvent {
                        serial,
                        time,
                        button,
                        state: smithay::backend::input::ButtonState::Released,
                    },
                );
            }
            p.frame(self);
        }
    }

    fn on_input(&mut self, id: u32, event: Input) {
        // A key's release is the seat's business, whichever window it was
        // sent for: the window may be gone (Ctrl+W closed it on the press),
        // and the key must not stay down for the rest of the app.
        if let Input::Key {
            code,
            pressed: false,
        } = event
        {
            if let (Some(code), Some(k)) = (input::xkb_keycode(code), self.seat.get_keyboard()) {
                k.input::<(), _>(
                    self,
                    code.into(),
                    smithay::backend::input::KeyState::Released,
                    SERIAL_COUNTER.next_serial(),
                    self.started.elapsed().as_millis() as u32,
                    |_, _, _| FilterResult::Forward,
                );
            }
            return;
        }
        let Some(w) = self.windows.get(&id) else {
            return;
        };
        let Some(root) = w.shell.wl_surface() else {
            return;
        };
        let Some(geometry) = self.geometry(w) else {
            return;
        };
        let shell = w.shell.clone();
        let serial = SERIAL_COUNTER.next_serial();
        let time = self.started.elapsed().as_millis() as u32;
        let pointer = self.seat.get_pointer();
        let keyboard = self.seat.get_keyboard();
        // Window-local logical pixels, from the window geometry's corner,
        // to the surface under them.
        let under = |x: f64, y: f64| {
            let (x, y) = input::clamp_point(x, y, geometry.size.w, geometry.size.h);
            let point = Point::<f64, Logical>::from((x, y));
            let found = under_from_surface_tree(
                &root,
                point,
                (-geometry.loc.x, -geometry.loc.y),
                WindowSurfaceType::ALL,
            )
            .map(|(s, loc)| (s, loc.to_f64()));
            (point, found)
        };
        match event {
            Input::PointerMove { x, y } => {
                let (location, focus) = under(x, y);
                if let Some(p) = pointer {
                    p.motion(
                        self,
                        focus,
                        &MotionEvent {
                            location,
                            serial,
                            time,
                        },
                    );
                    p.frame(self);
                }
            }
            Input::PointerButton {
                x,
                y,
                button,
                pressed,
            } => {
                let (location, focus) = under(x, y);
                if pressed && matches!(shell.kind(), SurfaceKind::Toplevel) {
                    self.focus_window(id, serial);
                    // A press on a toplevel dismisses its open menus, which
                    // is what a popup grab would have done.
                    self.dismiss_popups_of(id);
                }
                let Some(code) = input::evdev_button(button) else {
                    return;
                };
                if pressed {
                    self.buttons.insert(code);
                } else {
                    self.buttons.remove(&code);
                }
                if let Some(p) = pointer {
                    p.motion(
                        self,
                        focus,
                        &MotionEvent {
                            location,
                            serial,
                            time,
                        },
                    );
                    p.button(
                        self,
                        &ButtonEvent {
                            serial,
                            time,
                            button: code,
                            state: if pressed {
                                smithay::backend::input::ButtonState::Pressed
                            } else {
                                smithay::backend::input::ButtonState::Released
                            },
                        },
                    );
                    p.frame(self);
                }
            }
            Input::Scroll { x, y, dx, dy } => {
                let (location, focus) = under(x, y);
                if let Some(p) = pointer {
                    p.motion(
                        self,
                        focus,
                        &MotionEvent {
                            location,
                            serial,
                            time,
                        },
                    );
                    let mut frame =
                        AxisFrame::new(time).source(smithay::backend::input::AxisSource::Wheel);
                    for (axis, delta) in [
                        (smithay::backend::input::Axis::Vertical, dy),
                        (smithay::backend::input::Axis::Horizontal, dx),
                    ] {
                        if delta != 0.0 {
                            let (value, v120) = input::wheel(delta);
                            frame = frame.value(axis, value).v120(axis, v120);
                        }
                    }
                    p.axis(self, frame);
                    p.frame(self);
                }
            }
            Input::PointerLeave => {
                if let Some(p) = pointer {
                    p.motion(
                        self,
                        None,
                        &MotionEvent {
                            location: (-1.0, -1.0).into(),
                            serial,
                            time,
                        },
                    );
                    p.frame(self);
                }
            }
            Input::Key { code, pressed } => {
                let Some(code) = input::xkb_keycode(code) else {
                    return;
                };
                if let Some(k) = keyboard {
                    if k.current_focus().is_none() {
                        self.focus_window(id, serial);
                    }
                    k.input::<(), _>(
                        self,
                        code.into(),
                        if pressed {
                            smithay::backend::input::KeyState::Pressed
                        } else {
                            smithay::backend::input::KeyState::Released
                        },
                        serial,
                        time,
                        |_, _, _| FilterResult::Forward,
                    );
                }
            }
            Input::Focus => self.focus_window(id, serial),
        }
    }

    fn focus_window(&mut self, id: u32, serial: Serial) {
        let Some(w) = self.windows.get(&id) else {
            return;
        };
        let target = match &w.shell {
            Shell::Toplevel(t) => {
                t.with_pending_state(|s| {
                    s.states.set(xdg_toplevel::State::Activated);
                });
                t.send_pending_configure();
                KeyboardFocus::Wl(t.wl_surface().clone())
            }
            Shell::X11(x) if !x.is_override_redirect() => {
                let _ = x.set_activated(true);
                if let Some(wm) = self.xwm.as_mut() {
                    let _ = wm.raise_window(x);
                }
                KeyboardFocus::X11(x.clone())
            }
            // A popup's keyboard goes to its toplevel.
            _ => return,
        };
        self.deactivate_other_than(id);
        self.activated = Some(id);
        if let Some(k) = self.seat.get_keyboard() {
            k.set_focus(self, Some(target), serial);
        }
    }

    /// The toplevel that was activated, if it is not `id`, is told it no
    /// longer is: one window draws itself focused at a time.
    fn deactivate_other_than(&mut self, id: u32) {
        let Some(old) = self.activated.filter(|&old| old != id) else {
            return;
        };
        match self.windows.get(&old).map(|w| &w.shell) {
            Some(Shell::Toplevel(t)) => {
                t.with_pending_state(|s| {
                    s.states.unset(xdg_toplevel::State::Activated);
                });
                if t.is_initial_configure_sent() {
                    t.send_pending_configure();
                }
            }
            Some(Shell::X11(x)) => {
                let _ = x.set_activated(false);
            }
            _ => {}
        }
        self.activated = None;
    }

    fn dismiss_popups_of(&mut self, id: u32) {
        let popups: Vec<PopupSurface> = self
            .windows
            .values()
            .filter(|w| self.parent_of(w) == Some(id))
            .filter_map(|w| match &w.shell {
                Shell::Popup(p) => Some(p.clone()),
                _ => None,
            })
            .collect();
        for p in popups {
            p.send_popup_done();
        }
    }
}

// ---- protocol handlers ------------------------------------------------

impl CompositorHandler for State {
    fn compositor_state(&mut self) -> &mut CompositorState {
        &mut self.compositor
    }

    fn client_compositor_state<'a>(&self, client: &'a Client) -> &'a CompositorClientState {
        if let Some(state) = client.get_data::<XWaylandClientData>() {
            return &state.compositor_state;
        }
        &client
            .get_data::<ClientState>()
            .expect("every client is inserted with ClientState")
            .compositor
    }

    fn commit(&mut self, surface: &WlSurface) {
        on_commit_buffer_handler::<Self>(surface);
        // The first commit of an xdg surface asks for its first configure.
        if let Some(id) = self.window_for_surface(surface)
            && let Some(w) = self.windows.get_mut(&id)
        {
            w.dirty = true;
            match &w.shell {
                Shell::Toplevel(t) => {
                    if !t.is_initial_configure_sent() {
                        t.send_configure();
                    }
                }
                Shell::Popup(p) => {
                    if !p.is_initial_configure_sent() {
                        let _ = p.send_configure();
                    }
                }
                Shell::X11(_) => {}
            }
        }
        // Frame callbacks ride the next tick; drop any that piled up on a
        // surface that is not a window's, so a client is not left waiting.
        if self.window_for_surface(surface).is_none() {
            with_states(surface, |states| {
                for callback in states
                    .cached_state
                    .get::<SurfaceAttributes>()
                    .current()
                    .frame_callbacks
                    .drain(..)
                {
                    callback.done(self.started.elapsed().as_millis() as u32);
                }
            });
        }
    }
}

impl BufferHandler for State {
    fn buffer_destroyed(&mut self, _buffer: &WlBuffer) {}
}

impl ShmHandler for State {
    fn shm_state(&self) -> &ShmState {
        &self.shm
    }
}

impl SeatHandler for State {
    type KeyboardFocus = KeyboardFocus;
    type PointerFocus = WlSurface;
    type TouchFocus = WlSurface;

    fn seat_state(&mut self) -> &mut SeatState<State> {
        &mut self.seat_state
    }

    fn cursor_image(&mut self, _seat: &Seat<Self>, _image: CursorImageStatus) {
        // The browser draws its own cursor over the picture. Forwarding
        // the app's cursor shape is deferred (docs/lanes/8-stream.md).
    }
}

impl SelectionHandler for State {
    type SelectionUserData = ();
}

impl ClientDndGrabHandler for State {}
impl ServerDndGrabHandler for State {}

impl DataDeviceHandler for State {
    fn data_device_state(&self) -> &DataDeviceState {
        &self.data_device
    }
}

impl OutputHandler for State {}

impl XdgShellHandler for State {
    fn xdg_shell_state(&mut self) -> &mut XdgShellState {
        &mut self.xdg_shell
    }

    fn new_toplevel(&mut self, surface: ToplevelSurface) {
        surface.with_pending_state(|s| {
            s.states.set(xdg_toplevel::State::Activated);
            // Deckjack draws the chrome; ask for no client decorations
            // where the client honors xdg-decoration.
            s.decoration_mode = Some(DecorationMode::ServerSide);
            s.bounds = Some((OUTPUT_WIDTH as i32, OUTPUT_HEIGHT as i32).into());
        });
        let wl = surface.wl_surface().clone();
        let id = self.add_window(Shell::Toplevel(surface));
        self.deactivate_other_than(id);
        self.activated = Some(id);
        if let Some(k) = self.seat.get_keyboard() {
            k.set_focus(
                self,
                Some(KeyboardFocus::Wl(wl)),
                SERIAL_COUNTER.next_serial(),
            );
        }
    }

    fn new_popup(&mut self, surface: PopupSurface, positioner: PositionerState) {
        surface.with_pending_state(|s| {
            s.geometry = positioner.get_geometry();
        });
        self.add_window(Shell::Popup(surface));
    }

    fn reposition_request(
        &mut self,
        surface: PopupSurface,
        positioner: PositionerState,
        token: u32,
    ) {
        surface.with_pending_state(|s| {
            s.geometry = positioner.get_geometry();
            s.positioner = positioner;
        });
        surface.send_repositioned(token);
    }

    fn grab(&mut self, _surface: PopupSurface, _seat: WlSeat, _serial: Serial) {
        // No grab: each popup is a window of its own, and a press on its
        // toplevel dismisses it (`dismiss_popups_of`). Keyboard navigation
        // inside menus is deferred (docs/lanes/8-stream.md).
    }

    fn toplevel_destroyed(&mut self, surface: ToplevelSurface) {
        if let Some(id) = self.window_for_surface(surface.wl_surface()) {
            self.remove_window(id);
        }
    }

    fn popup_destroyed(&mut self, surface: PopupSurface) {
        if let Some(id) = self.window_for_surface(surface.wl_surface()) {
            self.remove_window(id);
        }
    }

    fn title_changed(&mut self, _surface: ToplevelSurface) {
        // Picked up by the next `announce`.
    }
}

impl XdgDecorationHandler for State {
    fn new_decoration(&mut self, toplevel: ToplevelSurface) {
        toplevel.with_pending_state(|s| s.decoration_mode = Some(DecorationMode::ServerSide));
    }

    fn request_mode(&mut self, toplevel: ToplevelSurface, _mode: DecorationMode) {
        toplevel.with_pending_state(|s| s.decoration_mode = Some(DecorationMode::ServerSide));
        if toplevel.is_initial_configure_sent() {
            toplevel.send_pending_configure();
        }
    }

    fn unset_mode(&mut self, toplevel: ToplevelSurface) {
        toplevel.with_pending_state(|s| s.decoration_mode = Some(DecorationMode::ServerSide));
        if toplevel.is_initial_configure_sent() {
            toplevel.send_pending_configure();
        }
    }
}

impl XWaylandShellHandler for State {
    fn xwayland_shell_state(&mut self) -> &mut XWaylandShellState {
        &mut self.xwayland_shell
    }
}

impl XwmHandler for State {
    fn xwm_state(&mut self, _xwm: XwmId) -> &mut X11Wm {
        self.xwm
            .as_mut()
            .expect("X11 events arrive only once the WM is running")
    }

    fn new_window(&mut self, _xwm: XwmId, _window: X11Surface) {}

    fn new_override_redirect_window(&mut self, _xwm: XwmId, _window: X11Surface) {}

    fn map_window_request(&mut self, _xwm: XwmId, window: X11Surface) {
        let _ = window.set_mapped(true);
        // Every X11 toplevel at the output's origin: X positions mean
        // nothing between Deckjack windows, and a menu is placed against
        // its parent by offset.
        let mut g = window.geometry();
        g.loc = (0, 0).into();
        let (w, h) = input::clamp_size(g.size.w.max(1) as u32, g.size.h.max(1) as u32);
        g.size = (w as i32, h as i32).into();
        let _ = window.configure(g);
        if self.window_for_x11(&window).is_none() {
            self.add_window(Shell::X11(window));
        }
    }

    fn mapped_override_redirect_window(&mut self, _xwm: XwmId, window: X11Surface) {
        if self.window_for_x11(&window).is_none() {
            self.add_window(Shell::X11(window));
        }
    }

    fn unmapped_window(&mut self, _xwm: XwmId, window: X11Surface) {
        if let Some(id) = self.window_for_x11(&window) {
            self.remove_window(id);
        }
        if !window.is_override_redirect() {
            let _ = window.set_mapped(false);
        }
    }

    fn destroyed_window(&mut self, _xwm: XwmId, window: X11Surface) {
        if let Some(id) = self.window_for_x11(&window) {
            self.remove_window(id);
        }
    }

    fn configure_request(
        &mut self,
        _xwm: XwmId,
        window: X11Surface,
        _x: Option<i32>,
        _y: Option<i32>,
        w: Option<u32>,
        h: Option<u32>,
        _reorder: Option<Reorder>,
    ) {
        let mut g = window.geometry();
        if let Some(w) = w {
            g.size.w = w as i32;
        }
        if let Some(h) = h {
            g.size.h = h as i32;
        }
        let (cw, ch) = input::clamp_size(g.size.w.max(1) as u32, g.size.h.max(1) as u32);
        g.size = (cw as i32, ch as i32).into();
        g.loc = (0, 0).into();
        let _ = window.configure(g);
    }

    fn configure_notify(
        &mut self,
        _xwm: XwmId,
        _window: X11Surface,
        _geometry: Rectangle<i32, Logical>,
        _above: Option<u32>,
    ) {
    }

    fn resize_request(
        &mut self,
        _xwm: XwmId,
        _window: X11Surface,
        _button: u32,
        _edge: X11ResizeEdge,
    ) {
    }

    fn move_request(&mut self, _xwm: XwmId, _window: X11Surface, _button: u32) {}
}

delegate_compositor!(State);
delegate_shm!(State);
delegate_seat!(State);
delegate_data_device!(State);
delegate_output!(State);
delegate_xdg_shell!(State);
delegate_xdg_decoration!(State);
delegate_xwayland_shell!(State);
