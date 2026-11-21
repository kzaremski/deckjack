//! The sandbox against the real kernel.
//!
//! Everything else that touches the launcher runs against a fake `Env`, which
//! can say what *would* be granted but not whether Landlock accepts it. That
//! gap is exactly where the file-grant defect lived: a file in a directory
//! list fails the whole ruleset, and only the kernel says so. So these confine
//! a real `/bin/sh` and look at what it could and could not open.
//!
//! On a host that cannot confine (no Landlock, or a kernel below ABI 4) they
//! print why and return, because there is nothing to observe. They are not
//! evidence of anything on such a host.

use std::net::TcpListener;
use std::path::PathBuf;
use std::process::Output;

use deckjack_core::sandbox::{self, Network, Policy};

/// What `/bin/sh` and `head` need to load at all, and nothing else.
fn loadable() -> Policy {
    let mut policy = Policy::nothing();
    for dir in ["/usr", "/bin", "/lib", "/lib64", "/etc"] {
        if std::path::Path::new(dir).is_dir() {
            policy.read.push(PathBuf::from(dir));
        }
    }
    policy
}

fn confined_sh(policy: Policy, script: &str) -> Option<Output> {
    if !sandbox::availability().is_enforcing() {
        eprintln!(
            "skipped: this host cannot confine ({:?})",
            sandbox::availability()
        );
        return None;
    }
    let mut cmd = sandbox::command("/bin/sh", policy).expect("an enforcing host builds a command");
    cmd.arg("-c").arg(script).env_clear();
    Some(cmd.output().expect("the confined shell runs"))
}

#[test]
fn a_confined_process_can_use_the_files_it_was_granted() {
    let mut policy = loadable();
    policy.read_files.push("/dev/urandom".into());
    policy.read_write_files.push("/dev/null".into());

    let Some(out) = confined_sh(policy, "head -c 16 /dev/urandom > /dev/null") else {
        return;
    };
    assert!(
        out.status.success(),
        "a file in the file lists must be usable, and must not fail the ruleset: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn without_the_file_grants_the_same_process_is_refused() {
    // The control for the test above: the script only succeeds *because* of
    // the grants, not because the sandbox lets everything through.
    let Some(out) = confined_sh(loadable(), "head -c 16 /dev/urandom > /dev/null") else {
        return;
    };
    assert!(
        !out.status.success(),
        "with no grant on /dev/urandom or /dev/null the read must be refused"
    );
}

#[test]
fn a_granted_file_does_not_open_its_parent_directory() {
    // A file grant is the file. `/dev/null` granted must not mean `/dev`
    // listable, which on a systemd seat would include `/dev/input`.
    let mut policy = loadable();
    policy.read_write_files.push("/dev/null".into());

    let Some(out) = confined_sh(policy, "ls /dev > /dev/null") else {
        return;
    };
    assert!(
        !out.status.success(),
        "listing /dev must still be refused when only /dev/null was granted"
    );
}

#[test]
fn the_gpu_compute_device_opens_read_write_when_granted() {
    let kfd = std::path::Path::new("/dev/kfd");
    if !kfd.exists() {
        eprintln!("skipped: no /dev/kfd on this machine");
        return;
    }
    // `exec 3<>` opens for read and write, which is how ROCm opens it.
    let script = "exec 3<> /dev/kfd";

    let mut granted = loadable();
    granted.read_write_files.push(kfd.into());
    let Some(out) = confined_sh(granted, script) else {
        return;
    };
    assert!(
        out.status.success(),
        "a granted /dev/kfd must open read-write: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let Some(out) = confined_sh(loadable(), script) else {
        return;
    };
    assert!(
        !out.status.success(),
        "an ungranted /dev/kfd must be refused"
    );
}

/// The planner and the launcher together, on this machine, with nothing
/// faked: discover an installed app from disk, build its plan with the real
/// `OsEnv`, spawn it through the same `Env::spawn` the core uses, and see what
/// the confined process managed to do.
///
/// This is the test that catches a system path the planner chose and the
/// kernel refuses — which would fail *every* app's launch, and which no test
/// against a fake `Env` can see.
#[test]
fn a_real_app_launched_by_the_core_can_use_its_system_files() {
    use deckjack_core::apps::{self, Confinement, Env as _, LaunchPlan, OsEnv};
    use std::os::unix::fs::PermissionsExt as _;

    if !sandbox::availability().is_enforcing() {
        eprintln!("skipped: this host cannot confine");
        return;
    }

    let root = tempfile::tempdir().unwrap();
    let install = root.path().join("install");
    let work = root.path().join("work");
    std::fs::create_dir_all(work.join("Out")).unwrap();
    let app_dir = install.join("apps").join("probe");
    std::fs::create_dir_all(&app_dir).unwrap();
    std::fs::write(
        app_dir.join("app.toml"),
        format!(
            "[app]\nname = \"probe\"\nversion = \"1.0.0\"\nprotocol = {}\n\
             [needs]\ngpu = true\nworking_dir = [{{ path = \"Out\", write = true }}]\n",
            apps::PROTOCOL_VERSION
        ),
    )
    .unwrap();
    // Each step writes its name only if it succeeded, so a failure says which
    // grant is missing rather than only that something was.
    let binary = app_dir.join("deckjack-probe");
    std::fs::write(
        &binary,
        "#!/bin/sh\n\
         head -c 16 /dev/urandom > /dev/null && echo urandom-null >> Out/log\n\
         cat /proc/cpuinfo > /dev/null && echo cpuinfo >> Out/log\n\
         if [ -e /dev/kfd ]; then exec 3<> /dev/kfd && echo kfd >> Out/log; else echo kfd >> Out/log; fi\n\
         echo done >> Out/log\n",
    )
    .unwrap();
    std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o755)).unwrap();

    let env = OsEnv::new();
    let found = apps::discover(&install, &env);
    let installed = found
        .apps
        .into_iter()
        .find(|app| app.name == "probe")
        .unwrap_or_else(|| panic!("probe was not discovered: {:?}", found.problems));
    let plan = LaunchPlan::build(&installed, &install, &work, Confinement::Enforced, &env)
        .expect("a plan for a real install");

    let pid = env.spawn(&plan).expect("the confined app starts");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while env.is_running(pid) {
        assert!(std::time::Instant::now() < deadline, "the probe app hung");
        std::thread::sleep(std::time::Duration::from_millis(20));
    }

    let log = std::fs::read_to_string(work.join("Out/log")).unwrap_or_default();
    assert_eq!(
        log.lines().collect::<Vec<_>>(),
        ["urandom-null", "cpuinfo", "kfd", "done"],
        "every system file the plan granted must be usable by the launched app"
    );
}

// ---------------------------------------------------------------------------
// TCP ports
// ---------------------------------------------------------------------------

/// Set on a re-exec of this test binary to make [`net_probe_child`] act.
const PROBE_ENV: &str = "DECKJACK_NET_PROBE";
/// Where [`net_probe_child`] appends its results, when stdout is not
/// collected (an app the core spawned inherits the core's stdout).
const PROBE_OUT_ENV: &str = "DECKJACK_NET_PROBE_OUT";

/// The confined half of the TCP tests. It does nothing at all in an ordinary
/// test run; the tests below re-exec this binary under a sandbox with
/// [`PROBE_ENV`] set, and then it tries each `connect:<port>` or
/// `bind:<port>` on 127.0.0.1 and reports `ok` or the errno.
///
/// The test binary itself rather than a shell, because no shell can `bind`,
/// and a helper that needs installing is a helper that is missing on the
/// next machine.
#[test]
fn net_probe_child() {
    use std::io::Write as _;
    use std::net::{Ipv4Addr, TcpListener, TcpStream};

    let Ok(ops) = std::env::var(PROBE_ENV) else {
        return;
    };
    let mut report = String::new();
    for op in ops.split_whitespace() {
        let (kind, port) = op.split_once(':').expect("op is kind:port");
        let port: u16 = port.parse().expect("a port number");
        let addr = (Ipv4Addr::LOCALHOST, port);
        let result = match kind {
            "connect" => TcpStream::connect(addr).map(drop),
            "bind" => TcpListener::bind(addr).map(drop),
            "mptcp" => mptcp_connect(port),
            "uring" => uring_setup(),
            other => panic!("unknown probe {other}"),
        };
        let outcome = match result {
            Ok(()) => "ok".to_string(),
            Err(e) => match e.raw_os_error() {
                Some(code) => format!("errno {code}"),
                None => format!("error {e}"),
            },
        };
        report.push_str(&format!("probe: {kind} {port} {outcome}\n"));
    }
    match std::env::var(PROBE_OUT_ENV) {
        Ok(path) => std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .and_then(|mut f| f.write_all(report.as_bytes()))
            .expect("the probe writes its report"),
        Err(_) => print!("{report}"),
    }
}

/// A TCP connection opened the way `mptcpize` opens one: `socket(AF_INET,
/// SOCK_STREAM, IPPROTO_MPTCP)`. Landlock's TCP rules do not mediate an
/// MPTCP socket, and against a plain TCP listener it falls back to TCP, so
/// this is a TCP connection Landlock never saw.
fn mptcp_connect(port: u16) -> std::io::Result<()> {
    // SAFETY: plain syscalls on a descriptor this function owns and closes.
    unsafe {
        let fd = libc::socket(libc::AF_INET, libc::SOCK_STREAM, libc::IPPROTO_MPTCP);
        if fd < 0 {
            return Err(std::io::Error::last_os_error());
        }
        let addr = libc::sockaddr_in {
            sin_family: libc::AF_INET as libc::sa_family_t,
            sin_port: port.to_be(),
            sin_addr: libc::in_addr {
                s_addr: u32::from(std::net::Ipv4Addr::LOCALHOST).to_be(),
            },
            sin_zero: [0; 8],
        };
        let r = libc::connect(
            fd,
            (&raw const addr).cast(),
            std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t,
        );
        let result = if r == 0 {
            Ok(())
        } else {
            Err(std::io::Error::last_os_error())
        };
        libc::close(fd);
        result
    }
}

/// `io_uring_setup(1, ..)`, closing the ring if one is made. io_uring opens
/// sockets without passing through seccomp, so it is a way around the
/// MPTCP filter. (The probe's port is ignored.)
fn uring_setup() -> std::io::Result<()> {
    // io_uring_params is 120 bytes; zeroed asks for nothing special.
    let mut params = [0u8; 120];
    // SAFETY: the kernel writes at most sizeof(io_uring_params) into params.
    let fd = unsafe { libc::syscall(libc::SYS_io_uring_setup, 1u32, params.as_mut_ptr()) };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: closing the descriptor just returned.
    unsafe { libc::close(fd as libc::c_int) };
    Ok(())
}

/// The arguments that make a re-exec of this binary run only the probe.
const PROBE_ARGS: &[&str] = &[
    "--exact",
    "net_probe_child",
    "--nocapture",
    "--test-threads=1",
];

/// Whether this host can enforce Landlock's TCP rules, or why not.
fn network_rules_or_skip() -> bool {
    match sandbox::availability() {
        sandbox::Availability::Enforcing {
            network_rules: true,
            ..
        } => true,
        other => {
            eprintln!(
                "skipped: this host has no Landlock network rules (ABI 4, Linux 6.7): {other:?}"
            );
            false
        }
    }
}

/// Run the probe confined to `network`, returning its report lines.
fn probe(network: Network, ops: &[(&str, u16)]) -> Option<Vec<String>> {
    if !network_rules_or_skip() {
        return None;
    }
    let exe = std::env::current_exe().expect("the test binary has a path");
    let mut policy = loadable();
    policy
        .read
        .push(exe.parent().expect("the binary is in a directory").into());
    policy.network = network;

    let ops: Vec<String> = ops.iter().map(|(k, p)| format!("{k}:{p}")).collect();
    let mut cmd = sandbox::command(&exe, policy).expect("an enforcing host builds a command");
    cmd.args(PROBE_ARGS)
        .env_clear()
        .env(PROBE_ENV, ops.join(" "));
    let out = cmd.output().expect("the confined probe runs");
    assert!(
        out.status.success(),
        "the confined probe failed to run at all: {}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    Some(probe_lines(&String::from_utf8_lossy(&out.stdout)))
}

/// The probe's report out of the test harness's chatter, which shares the
/// first line with it (`test net_probe_child ... probe: ...`).
fn probe_lines(text: &str) -> Vec<String> {
    text.lines()
        .filter_map(|l| l.split_once("probe: ").map(|(_, report)| report))
        .map(str::to_string)
        .collect()
}

/// Two live listeners on loopback, so a refused connect can only be the
/// sandbox: with no listener the kernel would refuse it too.
fn two_listeners() -> (TcpListener, u16, TcpListener, u16) {
    let a = TcpListener::bind("127.0.0.1:0").unwrap();
    let b = TcpListener::bind("127.0.0.1:0").unwrap();
    let pa = a.local_addr().unwrap().port();
    let pb = b.local_addr().unwrap().port();
    (a, pa, b, pb)
}

/// Two ports nothing is listening on right now. Bound and released, so there
/// is a window in which something else could take one; on a test machine
/// that is a flake measured in never.
fn two_free_ports() -> (u16, u16) {
    let (a, pa, b, pb) = two_listeners();
    drop((a, b));
    (pa, pb)
}

/// What Landlock answers a refused `connect` or `bind` with.
fn eacces() -> String {
    format!("errno {}", libc::EACCES)
}

#[test]
fn a_confined_process_connects_only_to_the_tcp_ports_it_was_granted() {
    let (_a, allowed, _b, other) = two_listeners();
    let Some(lines) = probe(
        Network::Ports {
            connect_tcp: vec![allowed],
            bind_tcp: vec![],
        },
        &[("connect", allowed), ("connect", other)],
    ) else {
        return;
    };
    assert_eq!(
        lines,
        [
            format!("connect {allowed} ok"),
            format!("connect {other} {}", eacces()),
        ],
        "the granted port connects and the other, with a live listener, is refused by Landlock"
    );
}

#[test]
fn a_confined_process_binds_only_the_tcp_ports_it_was_granted() {
    let (allowed, other) = two_free_ports();
    let Some(lines) = probe(
        Network::Ports {
            connect_tcp: vec![],
            bind_tcp: vec![allowed],
        },
        &[("bind", allowed), ("bind", other)],
    ) else {
        return;
    };
    assert_eq!(
        lines,
        [
            format!("bind {allowed} ok"),
            format!("bind {other} {}", eacces()),
        ]
    );
}

#[test]
fn a_connect_grant_is_not_a_bind_grant() {
    // Landlock keeps the two rights apart; a port granted to connect to must
    // not become a port the app can listen on.
    let (port, _) = two_free_ports();
    let Some(lines) = probe(
        Network::Ports {
            connect_tcp: vec![port],
            bind_tcp: vec![],
        },
        &[("bind", port)],
    ) else {
        return;
    };
    assert_eq!(lines, [format!("bind {port} {}", eacces())]);
}

#[test]
fn the_tcp_probe_reaches_everything_with_the_whole_network_and_nothing_without() {
    // The controls for the tests above: the probe can connect when nothing
    // filters it, so a refusal there is the port rule and not the probe; and
    // a denied network refuses even a port with a live listener.
    let (_a, pa, _b, pb) = two_listeners();
    let ops = [("connect", pa), ("connect", pb)];
    let Some(whole) = probe(Network::Whole, &ops) else {
        return;
    };
    assert_eq!(
        whole,
        [format!("connect {pa} ok"), format!("connect {pb} ok")]
    );

    let Some(denied) = probe(Network::denied(), &ops) else {
        return;
    };
    assert_eq!(
        denied,
        [
            format!("connect {pa} {}", eacces()),
            format!("connect {pb} {}", eacces()),
        ]
    );
}

#[test]
fn an_mptcp_socket_cannot_carry_a_confined_app_past_its_port_rules() {
    // D2 (docs/APP-UI-ISOLATION.md): "no app can reach the core's port"
    // rests on Landlock's TCP rules, and Landlock does not mediate MPTCP. A
    // process allowed no TCP port at all could open an MPTCP socket to the
    // core on loopback and pose as its proxy. The sandbox refuses the socket
    // itself, with the errno of a kernel built without MPTCP, so a program
    // that tries MPTCP first falls back to TCP — and meets Landlock.
    let (_a, pa, _b, _pb) = two_listeners();
    // The control: unconfined, the probe really does connect, so a refusal
    // below is the sandbox's. A kernel without MPTCP (or with
    // net.mptcp.enabled=0) refuses it on its own and has nothing to show.
    if let Err(e) = mptcp_connect(pa) {
        eprintln!("skipped: this kernel offers no MPTCP ({e})");
        return;
    }
    for network in [
        Network::denied(),
        Network::Ports {
            connect_tcp: vec![],
            bind_tcp: vec![],
        },
    ] {
        let Some(lines) = probe(network, &[("mptcp", pa), ("connect", pa)]) else {
            return;
        };
        assert_eq!(
            lines,
            [
                format!("mptcp {pa} errno {}", libc::EPROTONOSUPPORT),
                format!("connect {pa} {}", eacces()),
            ]
        );
    }
}

#[test]
fn a_confined_app_cannot_set_up_io_uring() {
    // io_uring's IORING_OP_SOCKET makes a socket without a socket(2) call,
    // so the MPTCP filter above would be a filter on one of two doors.
    let (_a, pa, _b, _pb) = two_listeners();
    // The control: unconfined, this host makes a ring (unless an
    // administrator disabled io_uring, which leaves nothing to show).
    if let Err(e) = uring_setup() {
        eprintln!("skipped: io_uring is unavailable here ({e})");
        return;
    }
    let Some(lines) = probe(Network::Whole, &[("uring", pa)]) else {
        return;
    };
    assert_eq!(lines, [format!("uring {pa} errno {}", libc::EPERM)]);
}

/// The manifest keys through the real launcher: `app.toml` on disk,
/// discovered, planned with `OsEnv` and spawned by `Env::spawn`, as the core
/// does it. This is what fails if the ports are parsed and validated but
/// never reach the ruleset.
#[test]
fn an_app_launched_by_the_core_gets_exactly_the_tcp_ports_its_manifest_names() {
    use deckjack_core::apps::{self, Confinement, Env as _, LaunchPlan, OsEnv};
    use std::os::unix::fs::PermissionsExt as _;

    if !network_rules_or_skip() {
        return;
    }
    let (_a, allowed, _b, other) = two_listeners();

    // Inside the target directory rather than /tmp so the test binary can be
    // hard-linked into the app's directory instead of copied: the launcher
    // grants an app its own directory and not the build tree.
    let root = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let install = root.path().join("install");
    let work = root.path().join("work");
    std::fs::create_dir_all(work.join("Out")).unwrap();
    let app_dir = install.join("apps").join("probe");
    std::fs::create_dir_all(&app_dir).unwrap();
    std::fs::write(
        app_dir.join("app.toml"),
        format!(
            "[app]\nname = \"probe\"\nversion = \"1.0.0\"\nprotocol = {}\n\
             [needs]\nnetwork = [\"127.0.0.1\"]\nconnect_tcp = [{allowed}]\n\
             working_dir = [{{ path = \"Out\", write = true }}]\n",
            apps::PROTOCOL_VERSION
        ),
    )
    .unwrap();
    let exe = std::env::current_exe().unwrap();
    let linked = app_dir.join("probe-bin");
    if std::fs::hard_link(&exe, &linked).is_err() {
        std::fs::copy(&exe, &linked).unwrap();
    }
    let binary = app_dir.join("deckjack-probe");
    std::fs::write(
        &binary,
        format!(
            "#!/bin/sh\n\
             export {PROBE_ENV}='connect:{allowed} connect:{other}'\n\
             export {PROBE_OUT_ENV}=Out/log\n\
             exec '{}' {} >/dev/null\n",
            linked.display(),
            PROBE_ARGS.join(" ")
        ),
    )
    .unwrap();
    std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o755)).unwrap();

    let env = OsEnv::new();
    let found = apps::discover(&install, &env);
    let installed = found
        .apps
        .into_iter()
        .find(|app| app.name == "probe")
        .unwrap_or_else(|| panic!("probe was not discovered: {:?}", found.problems));
    let plan = LaunchPlan::build(&installed, &install, &work, Confinement::Enforced, &env)
        .expect("a plan for a real install");

    let pid = env.spawn(&plan).expect("the confined app starts");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    while env.is_running(pid) {
        assert!(std::time::Instant::now() < deadline, "the probe app hung");
        std::thread::sleep(std::time::Duration::from_millis(20));
    }

    let log = std::fs::read_to_string(work.join("Out/log")).unwrap_or_default();
    assert_eq!(
        probe_lines(&log),
        [
            format!("connect {allowed} ok"),
            format!("connect {other} {}", eacces()),
        ],
        "the launched app reaches the manifest's port and is refused on another"
    );
}
