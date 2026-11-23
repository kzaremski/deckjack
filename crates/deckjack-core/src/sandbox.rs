//! The permission launcher: Landlock plus seccomp.
//!
//! Every app runs with only what its manifest asked for and the operator
//! approved. This module is what makes that true rather than decorative.
//!
//! # Fail closed
//!
//! The rule this module exists to enforce: **a node that cannot enforce a
//! manifest's permissions must refuse to launch the app, not launch it
//! unconfined.** Every path here returns an error instead of silently
//! degrading, and [`CompatLevel::HardRequirement`] makes the kernel's missing
//! features an error too rather than a best-effort shrug. A half-applied
//! sandbox is worse than none, because it invites trust it has not earned.
//!
//! That makes a platform limit visible: on a host without Landlock the app
//! does not start, and the operator sees why. The design already has the
//! answer for that case — run the work on a node that can.
//!
//! # What this does and does not buy
//!
//! Landlock carries the filesystem restriction and it is the main event: it is
//! unprivileged, needs no namespaces and no root, and works inside the R620's
//! LXC. Note that this is *not* what the design doc assumed — it reasoned from
//! bubblewrap, which does need namespaces, and concluded a phone could not be
//! confined. Landlock needs none, so the live question for a phone is only
//! whether its kernel was built with `CONFIG_SECURITY_LANDLOCK=y`.
//!
//! The seccomp layer is a **denylist** of syscalls that exist to escape a
//! sandbox or reconfigure the machine. A denylist is weaker than an allowlist
//! and that is a deliberate trade: an allowlist that is subtly wrong breaks
//! every app in ways that look like bugs, while a denylist of
//! `ptrace`/`mount`/`init_module` and friends is small enough to be read and
//! believed. Landlock is doing the load-bearing work; this closes the doors
//! that would let a process climb out around it.
//!
//! Neither layer constrains CPU, memory or what the app does with data it is
//! legitimately allowed to read. This is containment, not a hypervisor.

use std::path::{Path, PathBuf};

use thiserror::Error;

/// What a launched process may touch. Built from an app manifest; everything
/// not named here is denied.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Policy {
    /// Directories readable but not writable.
    pub read: Vec<PathBuf>,
    /// Directories both readable and writable.
    pub read_write: Vec<PathBuf>,
    /// Single files readable but not writable: `/dev/urandom`,
    /// `/proc/cpuinfo`.
    ///
    /// Separate from [`Policy::read`] because Landlock refuses a
    /// directory-only right such as `ReadDir` on anything that is not a
    /// directory, and refuses it by failing the whole ruleset. A file here is
    /// granted only the rights that apply to a file.
    pub read_files: Vec<PathBuf>,
    /// Single files both readable and writable: `/dev/null`, `/dev/kfd`.
    pub read_write_files: Vec<PathBuf>,
    /// What the process may do on the network. Denied unless said otherwise.
    pub network: Network,
    /// Unix sockets and signals across the sandbox's edge. Open unless said
    /// otherwise; see [`Ipc`] for why, and for what `Scoped` costs.
    pub ipc: Ipc,
}

/// Unix sockets and signals that cross the edge of the sandbox.
///
/// Landlock ABI 4, which every app is held to, has no rule for either: a
/// confined process can `connect()` to any pathname Unix socket its uid can
/// reach (the session D-Bus, the systemd user manager, the operator's own X
/// server, the desktop compositor's IPC), connect to any abstract socket, and
/// signal any process of the same uid. Through the session bus that is a way
/// out of the sandbox altogether (`systemd-run --user`).
///
/// [`Ipc::Scoped`] closes all three, and needs Landlock ABI 9
/// (`LANDLOCK_ACCESS_FS_RESOLVE_UNIX`) for pathname sockets and ABI 6 for the
/// abstract-socket and signal scopes. Everything inside the sandbox's own
/// domain keeps working: a server socket created by the confined process or
/// any of its children stays reachable by the others, and they may signal
/// each other. A kernel without ABI 9 refuses a `Scoped` policy rather than
/// running it as `Open` ([`CompatLevel::HardRequirement`]).
///
/// Streamed native apps are held to `Scoped` (docs/lanes/8-stream.md).
/// Other apps are still `Open`: requiring ABI 9 of them would refuse every
/// app on any kernel older than this one, which is a platform decision of
/// its own, recorded in the lane brief.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum Ipc {
    /// No Unix-socket or signal rule is installed (Landlock ABI 4).
    #[default]
    Open,
    /// Pathname sockets created outside the sandbox only beneath
    /// `connect_unix`; abstract sockets and signals only within it.
    Scoped { connect_unix: Vec<PathBuf> },
}

/// The network half of a [`Policy`].
///
/// Landlock's network rules (ABI 4) cover TCP `connect` and `bind`, by port.
/// They say nothing about which host a connection goes to, and nothing about
/// UDP, so neither does this type: [`Network::Ports`] is "TCP only on these
/// port numbers, to anyone", and UDP is untouched by every variant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Network {
    /// TCP connect only to `connect_tcp` and bind only on `bind_tcp`; every
    /// other TCP port is refused. Both empty is no TCP at all, which is the
    /// default.
    Ports {
        connect_tcp: Vec<u16>,
        bind_tcp: Vec<u16>,
    },
    /// Landlock's network rules are not installed at all.
    Whole,
}

impl Network {
    /// No TCP connect or bind on any port.
    pub fn denied() -> Network {
        Network::Ports {
            connect_tcp: Vec::new(),
            bind_tcp: Vec::new(),
        }
    }

    /// True when nothing on the network is filtered.
    pub fn is_whole(&self) -> bool {
        matches!(self, Network::Whole)
    }

    /// True when no TCP port at all is allowed.
    pub fn is_denied(&self) -> bool {
        matches!(self, Network::Ports { connect_tcp, bind_tcp }
            if connect_tcp.is_empty() && bind_tcp.is_empty())
    }
}

impl Default for Network {
    fn default() -> Network {
        Network::denied()
    }
}

impl Policy {
    /// A policy that allows nothing. Every grant is then explicit.
    pub fn nothing() -> Policy {
        Policy::default()
    }

    pub fn reading(mut self, path: impl Into<PathBuf>) -> Policy {
        self.read.push(path.into());
        self
    }

    pub fn writing(mut self, path: impl Into<PathBuf>) -> Policy {
        self.read_write.push(path.into());
        self
    }

    pub fn with_network(mut self, network: Network) -> Policy {
        self.network = network;
        self
    }

    pub fn with_ipc(mut self, ipc: Ipc) -> Policy {
        self.ipc = ipc;
        self
    }
}

/// Whether this host can enforce a policy, decided before anything is spawned
/// so the refusal happens at launch rather than halfway through.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Availability {
    /// Landlock is present at this ABI and seccomp is compiled in.
    Enforcing {
        landlock_abi: i32,
        /// Landlock gained network rules at ABI 4. Below that a policy that
        /// denies or narrows the network cannot be honored and is refused
        /// rather than quietly granted whole.
        network_rules: bool,
    },
    /// Nothing here can be confined. Carries the reason so the operator is
    /// told why an app will not start.
    Unavailable { reason: String },
}

impl Availability {
    pub fn is_enforcing(&self) -> bool {
        matches!(self, Availability::Enforcing { .. })
    }
}

#[derive(Debug, Error)]
pub enum SandboxError {
    #[error(
        "this host cannot confine a process ({reason}); refusing to launch unconfined. \
         Run this app on a node that can enforce its manifest."
    )]
    Unavailable { reason: String },

    #[error(
        "the manifest asks for the network to be denied or limited to some TCP ports, but this \
         kernel's Landlock is ABI {abi} \
         and network rules arrived in ABI 4; refusing rather than granting network access the \
         manifest did not ask for"
    )]
    NoNetworkRules { abi: i32 },

    #[error("{path} is named in the manifest but cannot be opened: {source}")]
    UnreadablePath {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("building the Landlock ruleset failed: {0}")]
    Ruleset(String),

    #[error(
        "Landlock reported the ruleset only {status}, not fully enforced; \
         refusing to continue with a partial sandbox"
    )]
    NotFullyEnforced { status: String },

    #[error("seccomp filter could not be built: {0}")]
    SeccompBuild(String),

    #[error("seccomp filter could not be installed: {0}")]
    SeccompApply(String),

    #[error("this architecture has no seccomp profile here, so the process cannot be confined")]
    UnsupportedArch,
}

/// Syscalls denied to a sandboxed process.
///
/// Each one is here because it is a way out of the box or a way to reconfigure
/// the machine, not because it is rare. Grouped by why, so the list can be
/// argued with:
///
/// - `ptrace`, `process_vm_readv`, `process_vm_writev`: read or write another
///   process's memory, which is a straight path to code that is not confined.
/// - `mount`, `umount2`, `pivot_root`, `chroot`: rearrange the filesystem
///   underneath the Landlock rules that were just applied.
/// - `init_module`, `finit_module`, `delete_module`, `kexec_load`,
///   `kexec_file_load`: load code into the kernel or replace it.
/// - `bpf`: load programs the kernel runs, and a long history of escapes.
/// - `perf_event_open`: broad observation of other processes.
/// - `unshare`, `setns`: enter or create namespaces, escaping the view the
///   launcher set up.
/// - `reboot`, `swapon`, `swapoff`, `settimeofday`, `clock_settime`: change
///   the machine out from under everything else on it.
/// - `add_key`, `request_key`, `keyctl`: the kernel keyring, which is shared.
/// - `io_uring_setup`, `io_uring_enter`, `io_uring_register`: io_uring
///   creates sockets (`IORING_OP_SOCKET`) and makes other calls without
///   passing through seccomp, so with it every argument filter below — the
///   MPTCP one in particular — is a filter on one of two doors. Denied the
///   way Docker's default profile denies it; programs fall back to syscalls.
///
/// `fork`/`clone` are deliberately NOT denied: an app spawning a helper is
/// ordinary, and a child inherits both Landlock and seccomp, so it is confined
/// too. `no_new_privs` is what stops it regaining anything through setuid.
fn denied_syscalls() -> Vec<i64> {
    vec![
        libc::SYS_ptrace,
        libc::SYS_process_vm_readv,
        libc::SYS_process_vm_writev,
        libc::SYS_mount,
        libc::SYS_umount2,
        libc::SYS_pivot_root,
        libc::SYS_init_module,
        libc::SYS_finit_module,
        libc::SYS_delete_module,
        libc::SYS_kexec_load,
        libc::SYS_kexec_file_load,
        libc::SYS_bpf,
        libc::SYS_perf_event_open,
        libc::SYS_unshare,
        libc::SYS_setns,
        libc::SYS_reboot,
        libc::SYS_swapon,
        libc::SYS_swapoff,
        libc::SYS_settimeofday,
        libc::SYS_clock_settime,
        libc::SYS_add_key,
        libc::SYS_request_key,
        libc::SYS_keyctl,
        libc::SYS_io_uring_setup,
        libc::SYS_io_uring_enter,
        libc::SYS_io_uring_register,
        #[cfg(target_arch = "x86_64")]
        libc::SYS_chroot,
        #[cfg(target_arch = "aarch64")]
        libc::SYS_chroot,
    ]
}

#[cfg(target_os = "linux")]
mod linux {
    use super::*;
    use landlock::{
        ABI, Access, AccessFs, AccessNet, CompatLevel, Compatible, NetPort, PathBeneath, PathFd,
        Ruleset, RulesetAttr, RulesetCreatedAttr, RulesetStatus, Scope,
    };
    use seccompiler::{SeccompAction, SeccompFilter, TargetArch, apply_filter};
    use std::collections::BTreeMap;

    /// The Landlock ABI this build asks for. v4 (Linux 6.7) is the floor
    /// because it is where network rules arrive, and a manifest that denies
    /// the network has to be honorable.
    ///
    /// This is the one knob. Raising it buys newer restrictions at the cost of
    /// refusing older kernels; lowering it does the reverse. It is deliberately
    /// a single constant so that trade is made in one visible place.
    const WANTED: ABI = ABI::V4;

    /// The crate keeps runtime ABI detection private on purpose — a value read
    /// at runtime invites code that behaves differently per kernel. So probe
    /// instead: build the ruleset we would really use and see whether the
    /// kernel accepts it. Authoritative, and it exercises the same path as
    /// `apply`.
    pub fn availability() -> Availability {
        match probe() {
            Ok(()) => Availability::Enforcing {
                landlock_abi: WANTED as i32,
                network_rules: true,
            },
            Err(reason) => Availability::Unavailable { reason },
        }
    }

    /// The ABI an [`Ipc::Scoped`] policy needs: 9, for
    /// `LANDLOCK_ACCESS_FS_RESOLVE_UNIX`. The scopes arrived in 6.
    const IPC: ABI = ABI::V9;

    /// Whether this kernel can hold a process to [`Ipc::Scoped`], probed
    /// the same way [`availability`] is: by building the ruleset.
    pub fn ipc_scoping() -> Result<(), String> {
        static ANSWER: std::sync::OnceLock<Result<(), String>> = std::sync::OnceLock::new();
        ANSWER
            .get_or_init(|| {
                Ruleset::default()
                    .set_compatibility(CompatLevel::HardRequirement)
                    .handle_access(AccessFs::from_all(WANTED) | AccessFs::ResolveUnix)
                    .and_then(|r| r.scope(Scope::from_all(IPC)))
                    .and_then(|r| r.create())
                    .map(|_| ())
                    .map_err(|e| {
                        format!(
                            "this kernel's Landlock cannot keep Unix sockets and signals inside \
                             a sandbox (that needs ABI 9): {e}"
                        )
                    })
            })
            .clone()
    }

    fn probe() -> Result<(), String> {
        let ruleset = Ruleset::default()
            .set_compatibility(CompatLevel::HardRequirement)
            .handle_access(AccessFs::from_all(WANTED))
            .map_err(|e| format!("Landlock filesystem rules unavailable: {e}"))?
            .handle_access(AccessNet::from_all(WANTED))
            .map_err(|e| format!("Landlock network rules unavailable (needs Linux 6.7+): {e}"))?;
        ruleset
            .create()
            .map(|_| ())
            .map_err(|e| format!("the kernel refused a Landlock ruleset: {e}"))
    }

    /// Apply the policy to the **calling process**. Only ever called between
    /// fork and exec, where the child is single-threaded.
    ///
    /// Landlock's `restrict_self` restricts the calling thread by default, so
    /// applying it in a multithreaded parent would leave sibling threads
    /// unconfined. In the forked child there is exactly one thread, which is
    /// why this is the right place and the only place.
    pub fn apply(policy: &Policy) -> Result<(), SandboxError> {
        let read_only = AccessFs::from_read(WANTED);
        let read_write = AccessFs::from_all(WANTED);
        // The subset of each that Landlock accepts on a non-directory:
        // execute, read, write and truncate, with every directory-only right
        // (`ReadDir`, `MakeReg`, `RemoveFile`, ...) masked off.
        let file = AccessFs::from_file(WANTED);

        let scoped = matches!(policy.ipc, Ipc::Scoped { .. });
        // Handled, so denied wherever it is not granted: connecting to a
        // pathname Unix socket made outside this domain. Granted only by the
        // `connect_unix` rules below, never as part of `read_write` (whose
        // rights stay ABI 4's), so a writable tree is not also a tree whose
        // sockets the process may reach.
        let handled = if scoped {
            read_write | AccessFs::ResolveUnix
        } else {
            read_write
        };

        let mut ruleset = Ruleset::default()
            // Missing kernel features become errors here rather than a
            // silently weaker sandbox. This is the fail-closed lever.
            .set_compatibility(CompatLevel::HardRequirement)
            .handle_access(handled)
            .map_err(|e| SandboxError::Ruleset(e.to_string()))?;
        if scoped {
            ruleset = ruleset
                .scope(Scope::from_all(IPC))
                .map_err(|e| SandboxError::Ruleset(e.to_string()))?;
        }

        if !policy.network.is_whole() {
            ruleset = ruleset
                .handle_access(AccessNet::from_all(WANTED))
                .map_err(|e| SandboxError::Ruleset(e.to_string()))?;
        }

        let mut created = ruleset
            .create()
            .map_err(|e| SandboxError::Ruleset(e.to_string()))?;

        for path in &policy.read {
            created = add_path(created, path, read_only)?;
        }
        for path in &policy.read_write {
            created = add_path(created, path, read_write)?;
        }
        for path in &policy.read_files {
            created = add_path(created, path, read_only & file)?;
        }
        for path in &policy.read_write_files {
            created = add_path(created, path, read_write & file)?;
        }
        if let Ipc::Scoped { connect_unix } = &policy.ipc {
            for path in connect_unix {
                created = add_path(created, path, AccessFs::ResolveUnix.into())?;
            }
        }

        // Denying the network is the ABSENCE of any NetPort rule after having
        // handled AccessNet — Landlock grants nothing it was not told to, so
        // each port below is the whole of what TCP may do. When the manifest
        // allows the whole network we never handle AccessNet at all, so it
        // stays untouched rather than being re-granted port by port.
        if let Network::Ports {
            connect_tcp,
            bind_tcp,
        } = &policy.network
        {
            for &port in connect_tcp {
                created = add_port(created, port, AccessNet::ConnectTcp)?;
            }
            for &port in bind_tcp {
                created = add_port(created, port, AccessNet::BindTcp)?;
            }
        }

        let status = created
            .restrict_self()
            .map_err(|e| SandboxError::Ruleset(e.to_string()))?;

        match status.ruleset {
            RulesetStatus::FullyEnforced => {}
            other => {
                return Err(SandboxError::NotFullyEnforced {
                    status: format!("{other:?}"),
                });
            }
        }
        if !status.no_new_privs {
            return Err(SandboxError::NotFullyEnforced {
                status: "no_new_privs was not set, so a setuid binary could regain privilege"
                    .to_string(),
            });
        }

        seccomp()?;
        Ok(())
    }

    fn add_port(
        created: landlock::RulesetCreated,
        port: u16,
        access: AccessNet,
    ) -> Result<landlock::RulesetCreated, SandboxError> {
        created
            .add_rule(NetPort::new(port, access))
            .map_err(|e| SandboxError::Ruleset(e.to_string()))
    }

    fn add_path(
        created: landlock::RulesetCreated,
        path: &Path,
        access: landlock::BitFlags<AccessFs>,
    ) -> Result<landlock::RulesetCreated, SandboxError> {
        let fd = PathFd::new(path).map_err(|e| SandboxError::UnreadablePath {
            path: path.to_path_buf(),
            source: std::io::Error::other(e.to_string()),
        })?;
        created
            .add_rule(PathBeneath::new(fd, access))
            .map_err(|e| SandboxError::Ruleset(e.to_string()))
    }

    fn target_arch() -> Result<TargetArch, SandboxError> {
        #[cfg(target_arch = "x86_64")]
        {
            Ok(TargetArch::x86_64)
        }
        #[cfg(target_arch = "aarch64")]
        {
            Ok(TargetArch::aarch64)
        }
        #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
        {
            Err(SandboxError::UnsupportedArch)
        }
    }

    /// Refuse `socket(_, _, IPPROTO_MPTCP)` with `EPROTONOSUPPORT`.
    ///
    /// Landlock's TCP rules do not mediate MPTCP (upstream treats it as a
    /// separate protocol, by design), and an MPTCP socket aimed at a plain
    /// TCP listener falls back to TCP. So without this, an app allowed no
    /// TCP port at all can still connect to the core on loopback, which is
    /// exactly what D2 in docs/APP-UI-ISOLATION.md promises it cannot.
    /// `EPROTONOSUPPORT` is what a kernel built without MPTCP answers, so a
    /// program that tries MPTCP first (Go's listeners, `mptcpize`) falls back
    /// to TCP and meets Landlock. Its own filter because seccompiler gives a
    /// filter one errno; the kernel applies the stricter of stacked filters.
    fn mptcp_filter(arch: TargetArch) -> Result<seccompiler::BpfProgram, SandboxError> {
        use seccompiler::{SeccompCmpArgLen, SeccompCmpOp, SeccompCondition, SeccompRule};
        let build = |e: seccompiler::BackendError| SandboxError::SeccompBuild(e.to_string());
        let protocol = SeccompCondition::new(
            2,
            SeccompCmpArgLen::Dword,
            SeccompCmpOp::Eq,
            libc::IPPROTO_MPTCP as u64,
        )
        .map_err(build)?;
        let mut rules: BTreeMap<i64, Vec<SeccompRule>> = BTreeMap::new();
        rules.insert(
            libc::SYS_socket,
            vec![SeccompRule::new(vec![protocol]).map_err(build)?],
        );
        SeccompFilter::new(
            rules,
            SeccompAction::Allow,
            SeccompAction::Errno(libc::EPROTONOSUPPORT as u32),
            arch,
        )
        .map_err(build)?
        .try_into()
        .map_err(build)
    }

    fn seccomp() -> Result<(), SandboxError> {
        let arch = target_arch()?;
        apply_filter(&mptcp_filter(arch)?)
            .map_err(|e| SandboxError::SeccompApply(e.to_string()))?;
        let mut rules: BTreeMap<i64, Vec<seccompiler::SeccompRule>> = BTreeMap::new();
        for nr in denied_syscalls() {
            // An empty rule vector means "match this syscall unconditionally".
            rules.insert(nr, Vec::new());
        }

        // EPERM rather than killing the process: an app that probes for a
        // capability and handles its absence should degrade, not die, and a
        // killed process is harder to diagnose than a refused syscall.
        let filter = SeccompFilter::new(
            rules,
            SeccompAction::Allow,
            SeccompAction::Errno(libc::EPERM as u32),
            arch,
        )
        .map_err(|e| SandboxError::SeccompBuild(e.to_string()))?;

        let program: seccompiler::BpfProgram = filter
            .try_into()
            .map_err(|e: seccompiler::BackendError| SandboxError::SeccompBuild(e.to_string()))?;

        apply_filter(&program).map_err(|e| SandboxError::SeccompApply(e.to_string()))
    }
}

#[cfg(not(target_os = "linux"))]
mod linux {
    use super::*;

    pub fn availability() -> Availability {
        Availability::Unavailable {
            reason: "Landlock and seccomp are Linux-only; macOS needs Seatbelt or a Linux VM"
                .to_string(),
        }
    }

    pub fn apply(_policy: &Policy) -> Result<(), SandboxError> {
        Err(SandboxError::Unavailable {
            reason: "not Linux".to_string(),
        })
    }

    pub fn ipc_scoping() -> Result<(), String> {
        Err("Landlock is Linux-only".to_string())
    }
}

/// Whether this host can confine a process. Check before launching, so the
/// refusal is a clear message rather than a failure part-way in.
pub fn availability() -> Availability {
    linux::availability()
}

/// Whether this host can hold a process to [`Ipc::Scoped`] (Landlock ABI
/// 9). Asked before a launch that needs it, so the refusal is a sentence
/// rather than an `EINVAL` from the forked child; [`apply`] refuses on its
/// own either way.
pub fn ipc_scoping() -> Result<(), String> {
    linux::ipc_scoping()
}

/// Confine the calling process to `policy`.
///
/// # Safety-adjacent note
///
/// This is intended to run between `fork` and `exec` via
/// [`std::os::unix::process::CommandExt::pre_exec`]. That context is
/// nominally async-signal-safe only, and the Landlock bindings allocate. The
/// child does nothing but this and then `exec`s, which is the same bargain
/// every Rust sandbox launcher makes, but it is a bargain and not a free
/// lunch — worth knowing if a deadlock ever appears here.
pub fn apply(policy: &Policy) -> Result<(), SandboxError> {
    linux::apply(policy)
}

/// Build a [`std::process::Command`] that confines itself before `exec`.
///
/// Refuses up front when the host cannot enforce, so a caller never gets a
/// Command that would silently run unconfined.
pub fn command(
    program: impl AsRef<std::ffi::OsStr>,
    policy: Policy,
) -> Result<std::process::Command, SandboxError> {
    if let Availability::Unavailable { reason } = availability() {
        return Err(SandboxError::Unavailable { reason });
    }

    let mut cmd = std::process::Command::new(program);
    // SAFETY: runs in the forked child before exec. See the note on `apply`.
    unsafe {
        use std::os::unix::process::CommandExt as _;
        cmd.pre_exec(move || {
            apply(&policy).map_err(|e| std::io::Error::other(e.to_string()))?;
            Ok(())
        });
    }
    Ok(cmd)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_host_that_cannot_enforce_refuses_to_launch() {
        // The rule this module exists for. Whatever this host is, `command`
        // must agree with `availability`: never hand back a Command that
        // would run unconfined.
        let can = availability().is_enforcing();
        let got = command("/bin/true", Policy::nothing());
        assert_eq!(can, got.is_ok(), "availability and command disagree");
        if !can {
            assert!(matches!(got.unwrap_err(), SandboxError::Unavailable { .. }));
        }
    }

    #[test]
    fn a_policy_grants_nothing_by_default() {
        let p = Policy::nothing();
        assert!(p.read.is_empty());
        assert!(p.read_write.is_empty());
        assert!(p.read_files.is_empty());
        assert!(p.read_write_files.is_empty());
        assert!(p.network.is_denied(), "network must be opt-in, not opt-out");
        assert_eq!(p.ipc, Ipc::Open);
    }

    #[test]
    fn the_denylist_covers_the_escape_routes() {
        let denied = denied_syscalls();
        for required in [
            libc::SYS_ptrace,
            libc::SYS_mount,
            libc::SYS_bpf,
            libc::SYS_setns,
        ] {
            assert!(
                denied.contains(&required),
                "syscall {required} must be denied"
            );
        }
    }

    #[test]
    fn this_host_reports_what_it_can_do() {
        // Not an assertion about the result — it is a fact about the kernel,
        // and the point is that asking is cheap and never panics.
        match availability() {
            Availability::Enforcing {
                landlock_abi,
                network_rules,
            } => {
                assert!(landlock_abi >= 1);
                println!("enforcing: landlock ABI {landlock_abi}, network_rules={network_rules}");
            }
            Availability::Unavailable { reason } => println!("unavailable: {reason}"),
        }
    }
}
