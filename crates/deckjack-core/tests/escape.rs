//! Every app kind, confined, against the sockets and processes outside its
//! sandbox (docs/lanes/10-sandbox-all-apps.md).
//!
//! For each first-party app manifest, and for a streamed native app's
//! compositor, a probe is installed under that manifest, planned by the real
//! `LaunchPlan::build` (or `stream::native_plan`) with the real `OsEnv`, and
//! spawned by the same `Env::spawn` the core uses. The probe is this test
//! binary, re-executed: it is not the app's own code, but it runs under
//! exactly the confinement the app would, and confinement is per process and
//! inherited, so nothing the app's code does can reach more than this.
//!
//! From inside, it tries:
//!
//! - this machine's real sockets, where they exist: the session bus, the
//!   systemd user manager, the X server by path and by abstract name, the
//!   Wayland and Hyprland sockets, PipeWire and gpg-agent;
//! - a pathname and an abstract socket held by another confined app (a
//!   second probe, in its own sandbox), and a pathname socket the test makes
//!   inside the app's own writable tree;
//! - signaling that other app, and this test process;
//! - and, as the things that must keep working, a socket of its own (by path
//!   and by abstract name) and the link to the core it inherited.
//!
//! Then the same probe runs with `Ipc::Open` in place of `Ipc::Scoped` and
//! nothing else changed, and every outside target it was refused must be
//! reached. A real socket that is not reachable even then is not on this
//! machine (or not listening), and is reported as skipped rather than
//! counted: a refusal that would have happened anyway proves nothing.
//!
//! On a host that cannot scope Unix sockets (Landlock below ABI 9) these
//! print why and return; there they are evidence of nothing.

use std::collections::BTreeMap;
use std::io::{Read as _, Write as _};
use std::os::linux::net::SocketAddrExt as _;
use std::os::unix::net::{SocketAddr, UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use deckjack_core::apps::{self, Confinement, Env as _, LaunchPlan, OsEnv};
use deckjack_core::sandbox::{self, Ipc};

/// Set on a re-exec of this binary to make [`ipc_probe_child`] act.
const PROBE_ENV: &str = "DECKJACK_IPC_PROBE";
/// Where [`ipc_probe_child`] writes its report.
const PROBE_OUT_ENV: &str = "DECKJACK_IPC_PROBE_OUT";

/// The arguments that make a re-exec of this binary run only the probe.
const PROBE_ARGS: &[&str] = &[
    "--exact",
    "ipc_probe_child",
    "--nocapture",
    "--test-threads=1",
];

/// Every first-party app's manifest, as `deckjack install` lays it down.
const MANIFESTS: &[(&str, &str)] = &[
    ("editor", include_str!("../../deckjack-editor/app.toml")),
    ("files", include_str!("../../deckjack-files/app.toml")),
    ("notes", include_str!("../../deckjack-notes/app.toml")),
    ("office", include_str!("../../deckjack-office/app.toml")),
    ("photo", include_str!("../../deckjack-photo/app.toml")),
    ("tasks", include_str!("../../deckjack-tasks/app.toml")),
    ("video", include_str!("../../deckjack-video/app.toml")),
];

/// The confined half. Does nothing in an ordinary test run; re-executed
/// under a sandbox with [`PROBE_ENV`] set, it runs each operation and
/// writes `<op> ok` or `<op> errno <n>` per line.
///
/// - `path:<p>`: connect to the pathname socket `p`.
/// - `abstract:<name>`: connect to the abstract socket `name`.
/// - `signal:<pid>`: send signal 0 to `pid`.
/// - `own-path` / `own-abstract`: bind a socket of its own and connect to
///   it, the way an app talks to a helper it started.
/// - `link`: say hello on the inherited link to the core ([`apps::LINK_FD`]).
/// - `fds`: report every descriptor open in the probe, as `fds 0,1,2,…`.
/// - `serve:<path>:<abstract>:<ready>`: bind both, write `ready`, and accept
///   until killed. The other app.
#[test]
fn ipc_probe_child() {
    let Ok(ops) = std::env::var(PROBE_ENV) else {
        return;
    };
    let out = std::env::var(PROBE_OUT_ENV).expect("the probe is told where to report");
    let mut report = String::new();
    for op in ops.split_whitespace() {
        let (kind, arg) = op.split_once(':').unwrap_or((op, ""));
        if kind == "fds" {
            report.push_str(&format!("fds {}\n", open_fds()));
            continue;
        }
        let result: std::io::Result<()> = match kind {
            "path" => UnixStream::connect(arg).map(drop),
            "abstract" => SocketAddr::from_abstract_name(arg.as_bytes())
                .and_then(|a| UnixStream::connect_addr(&a))
                .map(drop),
            "signal" => {
                let pid: i32 = arg.parse().expect("a pid");
                // SAFETY: signal 0 delivers nothing; it only asks.
                if unsafe { libc::kill(pid, 0) } == 0 {
                    Ok(())
                } else {
                    Err(std::io::Error::last_os_error())
                }
            }
            "own-path" => own_path(),
            "own-abstract" => own_abstract(),
            "link" => say_on_link(),
            "serve" => serve(arg),
            other => panic!("unknown probe {other}"),
        };
        let outcome = match result {
            Ok(()) => "ok".to_string(),
            Err(e) => match e.raw_os_error() {
                Some(code) => format!("errno {code}"),
                None => format!("error {e}"),
            },
        };
        report.push_str(&format!("{op} {outcome}\n"));
    }
    std::fs::write(out, report).expect("the probe writes its report");
}

/// The descriptors open in this process, comma-separated. Asked of each
/// number with `fcntl` rather than read from `/proc/self/fd`, which the
/// sandbox may not grant and which would open a descriptor of its own.
fn open_fds() -> String {
    (0..4096)
        // SAFETY: F_GETFD on a plain integer reads a flag and nothing else.
        .filter(|&fd| unsafe { libc::fcntl(fd, libc::F_GETFD) } >= 0)
        .map(|fd: i32| fd.to_string())
        .collect::<Vec<_>>()
        .join(",")
}

/// A descriptor in this (the core's) process that is *not* close-on-exec,
/// as a pty is for a moment inside portable-pty's `openpty`. No app may
/// inherit it. Closed on drop.
struct Stray(i32);

impl Stray {
    fn open() -> Stray {
        use std::os::fd::IntoRawFd as _;
        let fd = std::fs::File::open("/dev/null").unwrap().into_raw_fd();
        // SAFETY: fcntl on a descriptor this test just opened.
        unsafe {
            let flags = libc::fcntl(fd, libc::F_GETFD);
            assert!(flags >= 0);
            assert!(libc::fcntl(fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) >= 0);
        }
        Stray(fd)
    }
}

impl Drop for Stray {
    fn drop(&mut self) {
        // SAFETY: the descriptor `open` made, closed once.
        unsafe { libc::close(self.0) };
    }
}

fn own_path() -> std::io::Result<()> {
    // Relative, from inside the app's TMPDIR: the test's TMPDIR is too deep
    // for a socket address (108 bytes), as an app's own may be too.
    let dir = std::env::var("TMPDIR").expect("an app has a TMPDIR");
    std::env::set_current_dir(&dir)?;
    let path = PathBuf::from(format!("own-{}.sock", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let listener = UnixListener::bind(&path)?;
    let result = UnixStream::connect(&path).map(drop);
    drop(listener);
    let _ = std::fs::remove_file(&path);
    result
}

fn own_abstract() -> std::io::Result<()> {
    let name = format!("deckjack-escape-own-{}", std::process::id());
    let addr = SocketAddr::from_abstract_name(name.as_bytes())?;
    let _listener = UnixListener::bind_addr(&addr)?;
    UnixStream::connect_addr(&addr).map(drop)
}

fn say_on_link() -> std::io::Result<()> {
    use std::os::fd::FromRawFd as _;
    let fd: i32 = std::env::var(apps::LINK_FD_ENV)
        .map_err(|_| std::io::Error::other("no link descriptor in the environment"))?
        .parse()
        .map_err(std::io::Error::other)?;
    // SAFETY: the launcher put the link at this descriptor and nothing else
    // in this process owns it.
    let mut link = unsafe { UnixStream::from_raw_fd(fd) };
    link.write_all(b"hello from inside\n")
}

fn serve(arg: &str) -> std::io::Result<()> {
    let mut parts = arg.splitn(3, ':');
    let (path, name, ready) = (
        parts.next().unwrap(),
        parts.next().unwrap(),
        parts.next().unwrap(),
    );
    let by_path = UnixListener::bind(path)?;
    let by_name = UnixListener::bind_addr(&SocketAddr::from_abstract_name(name.as_bytes())?)?;
    std::fs::write(ready, "ready")?;
    by_path.set_nonblocking(true)?;
    by_name.set_nonblocking(true)?;
    // Until the test kills it, or a minute at most.
    let deadline = Instant::now() + Duration::from_secs(60);
    while Instant::now() < deadline {
        let _ = by_path.accept();
        let _ = by_name.accept();
        std::thread::sleep(Duration::from_millis(5));
    }
    Ok(())
}

/// Whether this host can scope Unix sockets and signals, or why not.
fn scoping_or_skip() -> bool {
    match sandbox::availability() {
        sandbox::Availability::Enforcing {
            ipc_scoping: true, ..
        } => true,
        other => {
            eprintln!("skipped: this host cannot scope Unix sockets (Landlock ABI 9): {other:?}");
            false
        }
    }
}

/// An install with a probe in place of each app's binary.
struct Install {
    root: tempfile::TempDir,
    env: OsEnv,
    /// Held so a launch finds an attendant, as it would in the core; each
    /// app's core-side link ends arrive here.
    links: BTreeMap<String, apps::LinkReceiver>,
}

impl Install {
    fn new() -> Install {
        // Inside the target directory rather than /tmp so the test binary
        // can be hard-linked beside each app instead of copied: the launcher
        // grants an app its own directory, not the build tree.
        let root = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
        std::fs::create_dir_all(root.path().join("install/apps")).unwrap();
        std::fs::create_dir_all(root.path().join("work")).unwrap();
        Install {
            root,
            env: OsEnv::new(),
            links: BTreeMap::new(),
        }
    }

    fn install_dir(&self) -> PathBuf {
        self.root.path().join("install")
    }

    fn work(&self) -> PathBuf {
        self.root.path().join("work")
    }

    /// Put the probe in `dir` under the binary name `binary`, running
    /// `ops` and reporting to `out`.
    fn probe_binary(dir: &Path, binary: &str, ops: &str, out: &Path) {
        use std::os::unix::fs::PermissionsExt as _;
        let exe = std::env::current_exe().unwrap();
        let linked = dir.join("probe-bin");
        if !linked.exists() && std::fs::hard_link(&exe, &linked).is_err() {
            std::fs::copy(&exe, &linked).unwrap();
        }
        let script = dir.join(binary);
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\nexport {PROBE_ENV}='{ops}'\nexport {PROBE_OUT_ENV}='{}'\n\
                 exec '{}' {} >/dev/null 2>&1\n",
                out.display(),
                linked.display(),
                PROBE_ARGS.join(" ")
            ),
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    /// Install `manifest` with the probe as its binary, and plan it.
    fn plan_app(&mut self, manifest: &str, ops: &str, out: &Path) -> LaunchPlan {
        let parsed = apps::Manifest::parse(Path::new("app.toml"), manifest).unwrap();
        let name = parsed.app.name.clone();
        let dir = self.install_dir().join("apps").join(&name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("app.toml"), manifest).unwrap();
        Self::probe_binary(&dir, &format!("deckjack-{name}"), ops, out);
        let found = apps::discover(&self.install_dir(), &self.env);
        let installed = found
            .apps
            .into_iter()
            .find(|app| app.name == name)
            .unwrap_or_else(|| panic!("{name} was not discovered: {:?}", found.problems));
        self.links
            .entry(name.clone())
            .or_insert_with(|| self.env.links().attend(&name));
        LaunchPlan::build(
            &installed,
            &self.install_dir(),
            &self.work(),
            Confinement::Enforced,
            &self.env,
        )
        .expect("a plan for a real install")
    }

    /// Run a plan to completion and return its report, one line per op.
    fn run(&self, plan: &LaunchPlan, out: &Path) -> BTreeMap<String, String> {
        let _ = std::fs::remove_file(out);
        let pid = self.env.spawn(plan).expect("the confined probe starts");
        let deadline = Instant::now() + Duration::from_secs(20);
        while self.env.is_running(pid) {
            assert!(Instant::now() < deadline, "the probe hung");
            std::thread::sleep(Duration::from_millis(10));
        }
        let text = std::fs::read_to_string(out)
            .unwrap_or_else(|e| panic!("{} wrote no report ({e}); did it start at all?", plan.app));
        text.lines()
            .filter_map(|l| l.rsplit_once(' '))
            .map(|(head, tail)| {
                // "<op> ok" or "<op> errno <n>".
                match head.rsplit_once(' ') {
                    Some((op, "errno")) => (op.to_string(), format!("errno {tail}")),
                    _ => (head.to_string(), tail.to_string()),
                }
            })
            .collect()
    }
}

/// The other app: a second confined probe holding a pathname socket and an
/// abstract socket in its own sandbox. Killed (by the pid this test
/// spawned, never by name) on drop, so a failing assertion does not leave it
/// running for its full minute.
struct Peer {
    pid: u32,
    path: PathBuf,
    name: String,
}

impl Drop for Peer {
    fn drop(&mut self) {
        // SAFETY: a pid this test spawned and has not reaped yet, so it
        // cannot have been reused.
        unsafe { libc::kill(self.pid as i32, libc::SIGKILL) };
    }
}

fn start_peer(install: &mut Install) -> Peer {
    let manifest = format!(
        "[app]\nname = \"peer\"\nversion = \"1.0.0\"\nprotocol = {}\n",
        apps::PROTOCOL_VERSION
    );
    // Its own private TMPDIR, which only it may write.
    let tmp = install.install_dir().join("cache/apps/peer/tmp");
    let path = tmp.join("peer.sock");
    let name = format!("deckjack-escape-peer-{}", std::process::id());
    let ready = tmp.join("ready");
    let out = tmp.join("report");
    let ops = format!("serve:{}:{name}:{}", path.display(), ready.display());
    let plan = install.plan_app(&manifest, &ops, &out);
    let _ = std::fs::remove_file(&ready);
    let pid = install.env.spawn(&plan).expect("the peer starts");
    let deadline = Instant::now() + Duration::from_secs(20);
    while !ready.exists() {
        assert!(
            install.env.is_running(pid),
            "the peer exited before serving"
        );
        assert!(Instant::now() < deadline, "the peer never served");
        std::thread::sleep(Duration::from_millis(10));
    }
    Peer { pid, path, name }
}

/// This machine's real sockets that a confined app has no business with,
/// where they exist. Whether each is reachable at all is decided by the
/// control run, not here.
fn real_targets() -> Vec<(String, String)> {
    // SAFETY: getuid cannot fail.
    let uid = unsafe { libc::getuid() };
    let runtime = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| format!("/run/user/{uid}"));
    let runtime = PathBuf::from(runtime);
    let mut paths = vec![
        ("session bus", runtime.join("bus")),
        ("systemd user manager", runtime.join("systemd/private")),
        ("X server (path)", PathBuf::from("/tmp/.X11-unix/X0")),
        ("PipeWire", runtime.join("pipewire-0")),
        ("gpg-agent", runtime.join("gnupg/S.gpg-agent")),
    ];
    if let Ok(display) = std::env::var("WAYLAND_DISPLAY") {
        paths.push(("Wayland", runtime.join(display)));
    }
    if let Ok(sig) = std::env::var("HYPRLAND_INSTANCE_SIGNATURE") {
        paths.push((
            "Hyprland",
            runtime.join("hypr").join(sig).join(".socket.sock"),
        ));
    }
    let mut targets: Vec<(String, String)> = paths
        .into_iter()
        .filter(|(_, p)| p.exists())
        .map(|(what, p)| (what.to_string(), format!("path:{}", p.display())))
        .collect();
    targets.push((
        "X server (abstract)".to_string(),
        "abstract:/tmp/.X11-unix/X0".to_string(),
    ));
    targets
}

/// One app kind's verdict: what the scoped run reached that it should not
/// have, what it failed to do that it must, and what was skipped.
fn check_kind(
    kind: &str,
    scoped: &BTreeMap<String, String>,
    open: &BTreeMap<String, String>,
    outside: &[(String, String)],
    must_reach: &[(String, String)],
    real: &[(String, String)],
) -> (Vec<String>, Vec<String>) {
    let mut failures = Vec::new();
    let mut skipped = Vec::new();
    let refused = |r: Option<&String>| {
        r.is_some_and(|r| {
            r == &format!("errno {}", libc::EACCES) || r == &format!("errno {}", libc::EPERM)
        })
    };
    for (what, op) in outside {
        let (s, o) = (scoped.get(op), open.get(op));
        let is_real = real.iter().any(|(_, r)| r == op);
        if o.map(String::as_str) != Some("ok") {
            if is_real {
                skipped.push(format!(
                    "{what} ({op}): not reachable even unconfined ({o:?})"
                ));
                continue;
            }
            failures.push(format!(
                "{kind}: {what} ({op}) was not reached with the restriction removed ({o:?}), \
                 so its refusal would prove nothing"
            ));
        }
        if !refused(s) {
            failures.push(format!(
                "{kind}: reached {what} ({op}) from inside its sandbox ({s:?})"
            ));
        }
    }
    for (what, op) in must_reach {
        if scoped.get(op).map(String::as_str) != Some("ok") {
            failures.push(format!(
                "{kind}: could not use {what} ({op}) under the scoped sandbox ({:?})",
                scoped.get(op)
            ));
        }
    }
    (failures, skipped)
}

#[test]
fn no_app_kind_reaches_a_socket_or_process_outside_its_sandbox() {
    if !scoping_or_skip() {
        return;
    }
    let mut install = Install::new();

    // Made by this test, so outside every app's domain, in a tree each app
    // can write: a writable grant must not be a grant to its sockets.
    let planted_dir = install.install_dir().join("cache/apps");
    std::fs::create_dir_all(&planted_dir).unwrap();

    let peer = start_peer(&mut install);
    let peer_pid = peer.pid;
    let peer_path = peer.path.clone();
    let peer_name = peer.name.clone();

    let me = std::process::id();
    let real = real_targets();
    let mut outside: Vec<(String, String)> = vec![
        (
            "another app's pathname socket".to_string(),
            format!("path:{}", peer_path.display()),
        ),
        (
            "another app's abstract socket".to_string(),
            format!("abstract:{peer_name}"),
        ),
        (
            "signaling another app".to_string(),
            format!("signal:{peer_pid}"),
        ),
        (
            "signaling the core's side".to_string(),
            format!("signal:{me}"),
        ),
    ];
    outside.extend(real.iter().cloned());
    let must_reach: Vec<(String, String)> = vec![
        (
            "a socket of its own, by path".to_string(),
            "own-path".to_string(),
        ),
        (
            "a socket of its own, by abstract name".to_string(),
            "own-abstract".to_string(),
        ),
    ];

    let mut failures = Vec::new();
    let mut skipped = Vec::new();
    let mut kinds = Vec::new();

    // Inheritable in this process for the whole run, standing in for a pty
    // the terminal is opening on another thread at the moment of a launch.
    let stray = Stray::open();
    let inherited = |kind: &str, run: &str, report: &BTreeMap<String, String>, want: &[i32]| {
        let want = want
            .iter()
            .map(i32::to_string)
            .collect::<Vec<_>>()
            .join(",");
        let got = report.get("fds").cloned().unwrap_or_default();
        (got != want).then(|| {
            format!(
                "{kind} ({run}): inherited descriptors {got:?}, expected exactly {want:?} \
                 (a stray, inheritable descriptor in the core is {})",
                stray.0
            )
        })
    };

    for (crate_name, manifest) in MANIFESTS {
        let parsed = apps::Manifest::parse(Path::new("app.toml"), manifest).unwrap();
        let name = parsed.app.name.clone();
        // A socket the test makes inside this app's own writable TMPDIR.
        let tmp = install
            .install_dir()
            .join("cache/apps")
            .join(&name)
            .join("tmp");
        std::fs::create_dir_all(&tmp).unwrap();
        let planted = tmp.join("planted.sock");
        let _ = std::fs::remove_file(&planted);
        let _planted = UnixListener::bind(&planted).unwrap();
        let mut outside = outside.clone();
        outside.push((
            "a socket planted in its own writable tree".to_string(),
            format!("path:{}", planted.display()),
        ));

        // First, before anything in the probe opens a descriptor.
        let mut ops: Vec<&str> = vec!["fds"];
        ops.extend(outside.iter().map(|(_, op)| op.as_str()));
        ops.extend(must_reach.iter().map(|(_, op)| op.as_str()));
        ops.push("link");
        let out = tmp.join("report");
        let plan = install.plan_app(manifest, &ops.join(" "), &out);
        assert_eq!(
            plan.sandbox.policy.ipc,
            Ipc::Scoped {
                connect_unix: Vec::new()
            },
            "{name}'s plan is not scoped"
        );

        let scoped = install.run(&plan, &out);
        // stdin, stdout, stderr and the link, and nothing else.
        failures.extend(inherited(
            &name,
            "scoped",
            &scoped,
            &[0, 1, 2, apps::LINK_FD],
        ));
        // The link the probe said hello on reached this side.
        let mut core_end = install
            .links
            .get_mut(&name)
            .unwrap()
            .try_recv()
            .expect("the launch handed the core its end of the link");
        core_end
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut heard = String::new();
        let _ = core_end.read_to_string(&mut heard);
        if heard != "hello from inside\n" || scoped.get("link").map(String::as_str) != Some("ok") {
            failures.push(format!(
                "{name}: the inherited link to the core did not work under the scoped sandbox \
                 (probe said {:?}, core heard {heard:?})",
                scoped.get("link")
            ));
        }

        let mut control = plan.clone();
        control.sandbox.policy.ipc = Ipc::Open;
        let open = install.run(&control, &out);
        // Drain the control run's link.
        let _ = install.links.get_mut(&name).unwrap().try_recv();

        let (f, s) = check_kind(&name, &scoped, &open, &outside, &must_reach, &real);
        failures.extend(f);
        skipped.extend(s.into_iter().map(|s| format!("{name}: {s}")));
        kinds.push(format!("{name} ({crate_name})"));
        eprintln!("{name}: scoped {scoped:?}");
        eprintln!("{name}: open   {open:?}");
    }

    // A streamed native app's compositor: its own plan, its own link.
    {
        let native_dir = install
            .install_dir()
            .join(deckjack_core::stream::NATIVE_DIR);
        std::fs::create_dir_all(&native_dir).unwrap();
        let decl = format!(
            "[app]\nname = \"np\"\nversion = \"1.0.0\"\nprotocol = {}\n\
             [native]\nexec = [\"/usr/bin/true\"]\n",
            apps::PROTOCOL_VERSION
        );
        let app = deckjack_core::stream::parse_native(&native_dir.join("np.toml"), &decl).unwrap();
        let tmp = install.install_dir().join("cache/native/np/tmp");
        std::fs::create_dir_all(&tmp).unwrap();
        // A socket the test makes inside the compositor's own writable
        // TMPDIR, as for every app kind above. The app is named "np" so the
        // path fits in a socket address (108 bytes).
        let planted = tmp.join("planted.sock");
        let _ = std::fs::remove_file(&planted);
        let _planted = UnixListener::bind(&planted).unwrap();
        let mut outside = outside.clone();
        outside.push((
            "a socket planted in its own writable tree".to_string(),
            format!("path:{}", planted.display()),
        ));
        let out = tmp.join("report");
        let mut ops: Vec<&str> = vec!["fds"];
        ops.extend(outside.iter().map(|(_, op)| op.as_str()));
        ops.extend(must_reach.iter().map(|(_, op)| op.as_str()));
        Install::probe_binary(&native_dir, "native-probe-bin", &ops.join(" "), &out);
        let (_keep, link) = UnixStream::pair().unwrap();
        use std::os::fd::AsRawFd as _;
        let mut plan = deckjack_core::stream::native_plan(
            &app,
            &install.install_dir(),
            &install.work(),
            Confinement::Enforced,
            &install.env,
            link.as_raw_fd(),
        )
        .expect("a native plan");
        plan.program = native_dir.join("native-probe-bin");
        assert!(
            !plan.core_link,
            "a compositor has its own link, not the app link"
        );
        let scoped = install.run(&plan, &out);
        // stdin, stdout, stderr and its stream link, at its own number.
        failures.extend(inherited(
            "native",
            "scoped",
            &scoped,
            &[0, 1, 2, link.as_raw_fd()],
        ));
        let mut control = plan.clone();
        control.sandbox.policy.ipc = Ipc::Open;
        let open = install.run(&control, &out);
        let (f, s) = check_kind("native", &scoped, &open, &outside, &must_reach, &real);
        failures.extend(f);
        skipped.extend(s.into_iter().map(|s| format!("native: {s}")));
        kinds.push("np (streamed compositor)".to_string());
        eprintln!("native: scoped {scoped:?}");
        eprintln!("native: open   {open:?}");
    }

    let peer_pid = peer.pid;
    drop(peer);
    // Reaped, so it is not left a zombie for the rest of the test run.
    let deadline = Instant::now() + Duration::from_secs(5);
    while install.env.is_running(peer_pid) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    eprintln!("kinds probed: {}", kinds.join(", "));
    eprintln!(
        "real targets: {}",
        real.iter()
            .map(|(w, _)| w.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    );
    for s in &skipped {
        eprintln!("skipped: {s}");
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}
