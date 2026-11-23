//! The compositor under the real Deckjack sandbox (spike S1, kept as a
//! test): the real `LaunchPlan::build` with the real `OsEnv`, spawned
//! through the same `Env::spawn` the core uses, so Landlock and seccomp
//! both apply, and the app is the compositor's child under the same
//! confinement. Nothing is faked: real GTK windows draw, the pictures are
//! real H.264, and the keys typed reach the real app.
//!
//! Each test skips, saying why, on a host that cannot confine or lacks the
//! client it drives. On such a host they are evidence of nothing.

mod common;

use std::time::{Duration, Instant};

use common::{Got, Rig, have};
use deckjack_core::apps::Env as _;
use deckjack_protocol::stream::{
    Input, LaunchSpec, StreamCommand, StreamEvent, SurfaceKind, VideoHeader,
};

/// Wait for the first toplevel to map and its first picture to arrive.
fn first_window(run: &mut common::Running) -> (u32, VideoHeader) {
    let surface = std::cell::Cell::new(None);
    let ready = std::cell::Cell::new(false);
    let got = run.until(
        Duration::from_secs(30),
        |got| matches!(got, Got::Video(h, _) if Some(h.surface) == surface.get()),
        |got| match got {
            Got::Event(StreamEvent::Ready { .. }) => ready.set(true),
            Got::Event(StreamEvent::Mapped {
                surface: s,
                kind: SurfaceKind::Toplevel,
                ..
            }) => surface.set(Some(*s)),
            Got::Event(StreamEvent::Exited { status }) => panic!("the app exited: {status}"),
            _ => {}
        },
    );
    let (surface, ready) = (surface.get(), ready.get());
    let Some(Got::Video(header, h264)) = got else {
        panic!("no picture from the first window (ready: {ready}, surface: {surface:?})");
    };
    assert!(ready, "the compositor never said it was ready");
    assert!(header.is_keyframe(), "the first picture must be a keyframe");
    // Annex B with an SPS first: decodable by itself.
    assert_eq!(&h264[..4], &[0, 0, 0, 1]);
    assert_eq!(h264[4] & 0x1f, 7, "the keyframe does not start with an SPS");
    eprintln!(
        "S1: first picture {}x{} after {} ms, {} bytes",
        header.width,
        header.height,
        run.started.elapsed().as_millis(),
        h264.len()
    );
    if let Some(dump) = std::env::var_os("DJ_DUMP_H264") {
        std::fs::write(dump, &h264).unwrap();
    }
    (header.surface, header)
}

/// Read until the compositor reports the app exited; returns the status
/// and whether `surface` was unmapped first.
fn until_exit(run: &mut common::Running, surface: Option<u32>) -> (String, bool) {
    let mut unmapped = false;
    let exited = run.until(
        Duration::from_secs(15),
        |got| matches!(got, Got::Event(StreamEvent::Exited { .. })),
        |got| {
            if let Got::Event(StreamEvent::Unmapped { surface: s }) = got
                && Some(*s) == surface
            {
                unmapped = true;
            }
        },
    );
    let Some(Got::Event(StreamEvent::Exited { status })) = exited else {
        panic!("the compositor did not report the app exiting");
    };
    (status, unmapped)
}

fn wait_gone(rig: &Rig, pid: u32) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while rig.env.is_running(pid) {
        assert!(Instant::now() < deadline, "the compositor kept running");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// zenity's entry dialog, printing what was typed into a file in the
/// app's home.
fn zenity_entry(backend: &str) -> LaunchSpec {
    LaunchSpec {
        app: "native".into(),
        exec: vec![
            "/usr/bin/sh".into(),
            "-c".into(),
            format!("GDK_BACKEND={backend} /usr/bin/zenity --entry --text=Name > \"$HOME/entry\""),
        ],
        x11: backend == "x11",
    }
}

/// The whole round trip with one app: it maps, streams a keyframe first
/// and another on request, takes typed keys, and exits when Enter accepts
/// the dialog, after which the window is unmapped and the compositor
/// exits.
fn round_trip(rig: &Rig, spec: &LaunchSpec, label: &str) {
    let out = rig.home().join("entry");
    let _ = std::fs::remove_file(&out);
    let mut run = rig.launch(spec);
    let (surface, header) = first_window(&mut run);
    assert!(
        header.width > 100 && header.height > 50,
        "{label}: {header:?}"
    );

    // A keyframe on request, as a viewer attaching asks for one.
    run.send(&StreamCommand::Keyframe { surface });
    let key = run.until(
        Duration::from_secs(5),
        |got| matches!(got, Got::Video(h, _) if h.surface == surface && h.is_keyframe()),
        |_| {},
    );
    assert!(key.is_some(), "{label}: no keyframe after asking for one");

    run.send(&StreamCommand::Input {
        surface,
        input: Input::Focus,
    });
    // GTK takes keys a moment after focus arrives.
    let _ = run.next(Duration::from_millis(300));
    // A Shift left down by a device that then went away (the seat moved):
    // released by the core's ReleaseAll, so the text below is typed as
    // typed, not in capitals.
    run.send(&StreamCommand::Input {
        surface,
        input: Input::Key {
            code: 42,
            pressed: true,
        },
    });
    run.send(&StreamCommand::ReleaseAll);
    run.type_text(surface, "Deckjack 8\n");
    let (status, unmapped) = until_exit(&mut run, Some(surface));
    assert!(
        unmapped,
        "{label}: the window was not unmapped before the exit"
    );
    assert!(status.contains("exit status: 0"), "{label}: {status}");
    let typed = std::fs::read_to_string(&out).unwrap_or_default();
    assert_eq!(typed.trim(), "Deckjack 8", "{label}: what the app received");
    wait_gone(rig, run.pid);
}

/// A Wayland (GTK 4) app under the real sandbox.
#[test]
fn a_wayland_app_streams_and_takes_keys_under_the_real_launcher() {
    let Some(rig) = Rig::new() else { return };
    if !have(&["/usr/bin/zenity", "/usr/bin/sh"]) {
        return;
    }
    round_trip(&rig, &zenity_entry("wayland"), "wayland");
}

/// The same app as an X11 client, through XWayland under the same
/// sandbox: no files in /tmp (the sandbox grants none), an abstract
/// socket, a cookie.
#[test]
fn an_x11_app_streams_and_takes_keys_through_xwayland_under_the_real_launcher() {
    let Some(rig) = Rig::new() else { return };
    if !have(&["/usr/bin/zenity", "/usr/bin/Xwayland", "/usr/bin/sh"]) {
        return;
    }
    round_trip(&rig, &zenity_entry("x11"), "x11");
}

/// The X server is on an abstract socket any local process can reach, so
/// the cookie is what keeps others out: the app's own client gets in, the
/// same client with no authority file does not.
#[test]
fn only_a_client_with_the_cookie_reaches_the_x_server() {
    let Some(rig) = Rig::new() else { return };
    if !have(&["/usr/bin/xprop", "/usr/bin/Xwayland", "/usr/bin/sh"]) {
        return;
    }
    let mut run = rig.launch(&LaunchSpec {
        app: "native".into(),
        exec: vec![
            "/usr/bin/sh".into(),
            "-c".into(),
            "/usr/bin/xprop -root _NET_SUPPORTING_WM_CHECK > /dev/null 2>&1; \
             echo $? > \"$HOME/with\"; \
             XAUTHORITY=/dev/null /usr/bin/xprop -root > /dev/null 2>&1; \
             echo $? > \"$HOME/without\""
                .into(),
        ],
        x11: true,
    });
    let _ = until_exit(&mut run, None);
    let read = |name: &str| {
        std::fs::read_to_string(rig.home().join(name))
            .unwrap_or_default()
            .trim()
            .to_string()
    };
    assert_eq!(read("with"), "0", "the app's own client was refused");
    assert_ne!(read("without"), "0", "a client without the cookie got in");
    assert!(!read("without").is_empty());
}

/// The app is the compositor's child and inherits its confinement. A
/// terminal needs a pty, `/dev/pts` is granted to no app, and so foot is
/// refused one and exits without ever mapping a window.
#[test]
fn the_app_inherits_the_sandbox_so_a_terminal_gets_no_pty() {
    let Some(rig) = Rig::new() else { return };
    if !have(&["/usr/bin/foot"]) {
        return;
    }
    let mut run = rig.launch(&LaunchSpec {
        app: "native".into(),
        exec: vec!["/usr/bin/foot".into()],
        x11: false,
    });
    let mut mapped = false;
    let exited = run.until(
        Duration::from_secs(15),
        |got| matches!(got, Got::Event(StreamEvent::Exited { .. })),
        |got| {
            if matches!(got, Got::Event(StreamEvent::Mapped { .. })) {
                mapped = true;
            }
        },
    );
    let Some(Got::Event(StreamEvent::Exited { status })) = exited else {
        panic!("foot kept running under the sandbox");
    };
    assert!(!mapped, "foot mapped a window, so it got a pty");
    assert!(!status.contains("exit status: 0"), "{status}");
    wait_gone(&rig, run.pid);
}

/// Closing the window asks the app to close, and the app's exit tears the
/// whole stream down. With a right-click first, the entry's context menu
/// maps as a popup placed against its window.
#[test]
fn a_popup_is_placed_against_its_parent_and_close_ends_the_app() {
    let Some(rig) = Rig::new() else { return };
    if !have(&["/usr/bin/zenity", "/usr/bin/sh"]) {
        return;
    }
    popup_and_close(&rig, "wayland");
}

/// The same through XWayland, where GTK's menu is an override-redirect X11
/// window that the compositor has to tie to its parent itself.
#[test]
fn an_x11_popup_is_placed_against_its_parent() {
    let Some(rig) = Rig::new() else { return };
    if !have(&["/usr/bin/zenity", "/usr/bin/Xwayland", "/usr/bin/sh"]) {
        return;
    }
    popup_and_close(&rig, "x11");
}

fn popup_and_close(rig: &Rig, backend: &str) {
    let mut run = rig.launch(&zenity_entry(backend));
    let (surface, header) = first_window(&mut run);
    let (cx, cy) = (
        f64::from(header.width) / 2.0,
        f64::from(header.height) / 2.0,
    );
    for pressed in [true, false] {
        run.send(&StreamCommand::Input {
            surface,
            input: Input::PointerButton {
                x: cx,
                y: cy,
                button: 2,
                pressed,
            },
        });
    }
    let popup = run.until(
        Duration::from_secs(10),
        |got| {
            matches!(
                got,
                Got::Event(StreamEvent::Mapped {
                    kind: SurfaceKind::Popup,
                    ..
                })
            )
        },
        |_| {},
    );
    let Some(Got::Event(StreamEvent::Mapped {
        surface: menu,
        parent,
        x,
        y,
        width,
        height,
        ..
    })) = popup
    else {
        panic!("no popup after a right-click in the middle of the dialog");
    };
    eprintln!(
        "S4 {backend}: popup {menu} {width}x{height} at ({x},{y}) in {parent:?}, click at ({cx},{cy})"
    );
    assert_eq!(
        parent,
        Some(surface),
        "the popup is not placed against its window"
    );
    // GTK opens a context menu at the pointer; allow for its arrow and
    // shadow.
    assert!(
        (f64::from(x) - cx).abs() < 120.0 && (f64::from(y) - cy).abs() < 120.0,
        "the popup is at ({x},{y}), not near the click at ({cx},{cy})"
    );

    // Closing a Wayland menu's own window tells the popup it is done, and
    // it goes. (An X11 menu is left alone instead; GTK's set
    // WM_DELETE_WINDOW and ignore it either way, so this app cannot tell
    // the two apart, and that branch has no test here.)
    if backend == "wayland" {
        run.send(&StreamCommand::Close { surface: menu });
        let went = run.until(
            Duration::from_millis(1500),
            |got| matches!(got, Got::Event(StreamEvent::Unmapped { surface: s }) if *s == menu),
            |_| {},
        );
        assert!(went.is_some(), "the Wayland popup did not go when closed");
    }

    run.send(&StreamCommand::Close { surface });
    let (_, unmapped) = until_exit(&mut run, Some(surface));
    assert!(unmapped, "the window was not unmapped before the exit");
    wait_gone(rig, run.pid);
}

/// When the compositor is killed outright (a stop, a lockdown, a crash),
/// the app it ran does not live on unsupervised: it dies with its parent.
///
/// The app here is `sleep`, not a GUI app, on purpose: a GTK app exits by
/// itself when its display goes away, which would pass this test with or
/// without the parent-death signal. A process that does not care about the
/// display is the case the signal is for.
#[test]
fn killing_the_compositor_takes_the_app_with_it() {
    let Some(rig) = Rig::new() else { return };
    if !have(&["/usr/bin/sleep"]) {
        return;
    }
    let mut run = rig.launch(&LaunchSpec {
        app: "native".into(),
        exec: vec!["/usr/bin/sleep".into(), "60".into()],
        x11: false,
    });
    let ready = run.until(
        Duration::from_secs(10),
        |got| matches!(got, Got::Event(StreamEvent::Ready { .. })),
        |_| {},
    );
    assert!(ready.is_some(), "the compositor never started the app");
    let children = std::fs::read_to_string(format!("/proc/{0}/task/{0}/children", run.pid))
        .expect("the compositor's children");
    let app: i32 = children
        .split_whitespace()
        .next()
        .expect("the app is the compositor's child")
        .parse()
        .unwrap();
    // Only the pid this test spawned.
    unsafe { libc::kill(run.pid as i32, libc::SIGKILL) };
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let alive = std::fs::read_to_string(format!("/proc/{app}/stat"))
            .map(|s| {
                !s.rsplit(')')
                    .next()
                    .unwrap_or("")
                    .trim_start()
                    .starts_with('Z')
            })
            .unwrap_or(false);
        if !alive {
            break;
        }
        assert!(Instant::now() < deadline, "the app outlived its compositor");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Read a number an app wrote into its home, waiting for it.
fn read_number(rig: &Rig, name: &str) -> Option<i64> {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Ok(text) = std::fs::read_to_string(rig.home().join(name))
            && let Ok(n) = text.trim().parse()
        {
            return Some(n);
        }
        if Instant::now() > deadline {
            return None;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn alive(pid: i64) -> bool {
    std::fs::read_to_string(format!("/proc/{pid}/stat"))
        .map(|s| {
            !s.rsplit(')')
                .next()
                .unwrap_or("")
                .trim_start()
                .starts_with('Z')
        })
        .unwrap_or(false)
}

fn wait_dead(pid: i64, what: &str) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while alive(pid) {
        if Instant::now() > deadline {
            // Only the pid this test's app started.
            unsafe { libc::kill(pid as i32, libc::SIGKILL) };
            panic!("{what} outlived its compositor");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// The sandbox's edge, for a native app (Landlock ABI 9 and 6): a Unix
/// socket made outside it cannot be connected to, by path or abstract name,
/// and a process outside it cannot be signaled. The sockets here stand in
/// for the session bus, the operator's X server (`@/tmp/.X11-unix/X0`) and
/// another app's compositor; the process for the core and the desktop.
/// Inside its own sandbox the app still reaches its compositor's Wayland
/// and X11 sockets: every other test here depends on that.
#[test]
fn a_native_app_reaches_no_socket_or_process_outside_its_sandbox() {
    use std::os::linux::net::SocketAddrExt as _;
    use std::os::unix::net::{SocketAddr, UnixListener};
    let Some(rig) = Rig::new() else { return };
    if !have(&["/usr/bin/curl", "/usr/bin/sh"]) {
        return;
    }
    let path = rig.work.join("outside.sock");
    let by_path = UnixListener::bind(&path).unwrap();
    by_path.set_nonblocking(true).unwrap();
    let name = format!("deckjack-test-outside-{}", std::process::id());
    let by_name =
        UnixListener::bind_addr(&SocketAddr::from_abstract_name(name.as_bytes()).unwrap()).unwrap();
    by_name.set_nonblocking(true).unwrap();
    let me = std::process::id();
    let script = format!(
        "/usr/bin/curl -s --max-time 2 --unix-socket {path} http://x/ ; echo $? > \"$HOME/path\"; \
         /usr/bin/curl -s --max-time 2 --abstract-unix-socket {name} http://x/ ; echo $? > \"$HOME/abstract\"; \
         kill -0 {me} 2>/dev/null; echo $? > \"$HOME/signal\"",
        path = path.display()
    );
    for f in ["path", "abstract", "signal"] {
        let _ = std::fs::remove_file(rig.home().join(f));
    }
    let mut run = rig.launch(&LaunchSpec {
        app: "native".into(),
        exec: vec!["/usr/bin/sh".into(), "-c".into(), script],
        x11: false,
    });
    let _ = until_exit(&mut run, None);
    let path_status = read_number(&rig, "path");
    let abstract_status = read_number(&rig, "abstract");
    eprintln!("curl by path: {path_status:?}, by abstract name: {abstract_status:?}");
    assert!(
        by_path.accept().is_err(),
        "the app connected to a socket outside its sandbox by path"
    );
    assert!(
        by_name.accept().is_err(),
        "the app connected to an abstract socket outside its sandbox"
    );
    // curl's 7 is "could not connect"; anything else means it got further.
    assert_eq!(path_status, Some(7));
    assert_eq!(abstract_status, Some(7));
    let signal = read_number(&rig, "signal");
    assert!(
        matches!(signal, Some(n) if n != 0),
        "the app signaled a process outside its sandbox ({signal:?})"
    );
}

/// Two instances of one app share its private `TMPDIR`, but each
/// compositor's sockets and X cookie are in a runtime directory of its
/// own, which goes when the compositor does.
#[test]
fn each_instance_has_its_own_runtime_directory() {
    let Some(rig) = Rig::new() else { return };
    if !have(&["/usr/bin/sh", "/usr/bin/sleep"]) {
        return;
    }
    let spec = |file: &str| LaunchSpec {
        app: "native".into(),
        exec: vec![
            "/usr/bin/sh".into(),
            "-c".into(),
            format!("echo \"$XDG_RUNTIME_DIR\" > \"$HOME/{file}\"; exec /usr/bin/sleep 60"),
        ],
        x11: false,
    };
    for f in ["first", "second"] {
        let _ = std::fs::remove_file(rig.home().join(f));
    }
    let mut first = rig.launch(&spec("first"));
    let second = rig.launch(&spec("second"));
    let read = |name: &str| {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let text = std::fs::read_to_string(rig.home().join(name)).unwrap_or_default();
            if text.ends_with('\n') {
                return std::path::PathBuf::from(text.trim());
            }
            assert!(
                Instant::now() < deadline,
                "{name} never wrote its runtime dir"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    };
    let (a, b) = (read("first"), read("second"));
    assert_ne!(a, b, "two instances share one runtime directory");
    assert!(a.is_dir() && b.is_dir());
    // An orderly stop (what the core sends) removes it.
    unsafe { libc::kill(first.pid as i32, libc::SIGTERM) };
    let _ = until_exit(&mut first, None);
    wait_gone(&rig, first.pid);
    assert!(
        !a.exists(),
        "a stopped compositor left its runtime directory"
    );
    assert!(b.is_dir(), "stopping one instance removed the other's");
    drop(second);
}

/// The app's exit ends the compositor, and the compositor ends whatever the
/// app left running: here a daemon in a session of its own, which neither
/// the app's process group nor the parent-death signal reaches.
#[test]
fn what_the_app_left_running_goes_when_the_app_exits() {
    let Some(rig) = Rig::new() else { return };
    if !have(&["/usr/bin/sh", "/usr/bin/setsid", "/usr/bin/sleep"]) {
        return;
    }
    let _ = std::fs::remove_file(rig.home().join("orphan"));
    let mut run = rig.launch(&LaunchSpec {
        app: "native".into(),
        exec: vec![
            "/usr/bin/sh".into(),
            "-c".into(),
            "/usr/bin/setsid /usr/bin/sleep 300 < /dev/null > /dev/null 2>&1 & \
             echo $! > \"$HOME/orphan\"; exit 0"
                .into(),
        ],
        x11: false,
    });
    let orphan = read_number(&rig, "orphan").expect("the app wrote its daemon's pid");
    let (status, _) = until_exit(&mut run, None);
    assert!(status.contains("exit status: 0"), "{status}");
    wait_gone(&rig, run.pid);
    wait_dead(orphan, "the app's daemon");
}

/// The core stops a compositor with `SIGTERM` (`stream.stop`, a lockdown).
/// That ends the app and what it started, not only the compositor and its
/// direct child: a background process of the app's goes too.
#[test]
fn stopping_the_compositor_ends_what_the_app_started() {
    let Some(rig) = Rig::new() else { return };
    if !have(&["/usr/bin/sh", "/usr/bin/sleep"]) {
        return;
    }
    let _ = std::fs::remove_file(rig.home().join("background"));
    let mut run = rig.launch(&LaunchSpec {
        app: "native".into(),
        exec: vec![
            "/usr/bin/sh".into(),
            "-c".into(),
            "/usr/bin/sleep 300 & echo $! > \"$HOME/background\"; exec /usr/bin/sleep 300".into(),
        ],
        x11: false,
    });
    let background = read_number(&rig, "background").expect("the app wrote its child's pid");
    unsafe { libc::kill(run.pid as i32, libc::SIGTERM) };
    let (status, _) = until_exit(&mut run, None);
    assert!(status.contains("stopped"), "{status}");
    wait_gone(&rig, run.pid);
    wait_dead(background, "the app's background process");
}

/// One native app cannot reach another's compositor: its Wayland socket
/// was made inside the other app's sandbox, so a client there could map a
/// window the shell would show as the other app's (and take the keys typed
/// into it). Here the second app tries to connect to the first's socket.
#[test]
fn a_native_app_cannot_reach_another_apps_compositor() {
    let (Some(first), Some(second)) = (Rig::new(), Rig::new()) else {
        return;
    };
    if !have(&["/usr/bin/curl", "/usr/bin/sh", "/usr/bin/sleep"]) {
        return;
    }
    let _ = std::fs::remove_file(first.home().join("socket"));
    let _victim = first.launch(&LaunchSpec {
        app: "native".into(),
        exec: vec![
            "/usr/bin/sh".into(),
            "-c".into(),
            "echo \"$XDG_RUNTIME_DIR/$WAYLAND_DISPLAY\" > \"$HOME/socket\"; exec /usr/bin/sleep 60"
                .into(),
        ],
        x11: false,
    });
    let deadline = Instant::now() + Duration::from_secs(10);
    let socket = loop {
        let text = std::fs::read_to_string(first.home().join("socket")).unwrap_or_default();
        if text.ends_with('\n') {
            break text.trim().to_string();
        }
        assert!(
            Instant::now() < deadline,
            "the first app never said where its socket is"
        );
        std::thread::sleep(Duration::from_millis(20));
    };
    assert!(std::path::Path::new(&socket).exists(), "{socket}");
    let _ = std::fs::remove_file(second.home().join("reached"));
    let mut intruder = second.launch(&LaunchSpec {
        app: "native".into(),
        exec: vec![
            "/usr/bin/sh".into(),
            "-c".into(),
            format!(
                "/usr/bin/curl -s --max-time 2 --unix-socket {socket} http://x/ ; \
                 echo $? > \"$HOME/reached\""
            ),
        ],
        x11: false,
    });
    let _ = until_exit(&mut intruder, None);
    assert_eq!(
        read_number(&second, "reached"),
        Some(7),
        "a native app connected to another app's compositor"
    );
}
