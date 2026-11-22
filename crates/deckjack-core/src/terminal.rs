//! PTY-backed terminal sessions.
//!
//! The terminal is the documented escape hatch: a real shell running as the
//! host user, not an emulation of one. Everything a shell expects is
//! therefore real too — a controlling terminal, job control, `SIGWINCH` on
//! resize — which is why this goes through a pseudo-terminal rather than
//! piped stdio. See docs/DESIGN.md, Host access.
//!
//! **Why `portable-pty` and not `nix::pty`.** Deckjack hosts on Linux, macOS
//! and `aarch64-linux-android` (Termux). Each needs different ceremony around
//! `openpty`, `setsid`, `TIOCSCTTY` and the login-shell `argv[0]`, and
//! `portable-pty` is the one the design names for exactly that reason. It
//! also resolves the user's shell the way a terminal emulator does ($SHELL,
//! then the password database), which matters on Termux where there is no
//! `/bin/bash`.
//!
//! **Sessions outlive browsers.** A client holds no state: it reconnects and
//! renders whatever the server holds. So a session lives in [`Registry`],
//! keyed by [`TerminalId`], until the shell exits or someone closes it. A
//! reconnecting browser reattaches and replays [`SCROLLBACK_BYTES`] of recent
//! output instead of staring at a blank screen.
//!
//! **This module does not authorize anything.** Remote sessions get a
//! terminal only after step-up sign-in, and tether sessions not at all; that
//! decision belongs to the command bus above. What is recorded here is
//! [`Opener`], so lockdown can kill the terminals belonging to a revoked
//! session.
//!
//! ```no_run
//! use std::sync::Arc;
//! use deckjack_core::terminal::{NativeBackend, OsEnv, Registry, SessionSpec};
//!
//! # async fn example() -> Result<(), Box<dyn std::error::Error>> {
//! let registry = Registry::new(
//!     Arc::new(OsEnv),
//!     Arc::new(NativeBackend),
//!     Default::default(),
//! );
//!
//! let term = registry.open(SessionSpec::login_shell("/srv/deckjack"))?;
//! term.write(b"uname -a\r")?;
//!
//! // What a reattaching browser does: take the scrollback, then follow.
//! let mut at = {
//!     let seen = term.snapshot();
//!     // feed `seen.bytes` to xterm.js
//!     seen.cursor
//! };
//! loop {
//!     let out = term.output_after(at).await;
//!     at = out.cursor;
//!     if out.exit.is_some() {
//!         break;
//!     }
//! }
//! # Ok(())
//! # }
//! ```

use std::collections::{HashMap, VecDeque};
use std::ffi::{OsStr, OsString};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{SyncSender, TrySendError, sync_channel};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, Weak};
use std::time::{SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use portable_pty::{
    Child, ChildKiller, CommandBuilder, MasterPty, PtyPair, PtySize, native_pty_system,
};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::auth::code::os_random;
use crate::auth::session::SessionHash;

/// How much recent output is kept per session so a reconnecting browser has
/// something to render.
///
/// 256 KiB is roughly 3,000 lines of 80-column text, which is more than
/// xterm.js shows with its default 1,000-line scrollback, so a reattach looks
/// like the terminal was never away. The cost is bounded and known: the
/// default session limit times this is a few megabytes, which a phone hosting
/// in Termux can afford. Output older than this is gone on purpose — a
/// terminal is not a transcript, and anything worth keeping belongs in a
/// file.
pub const SCROLLBACK_BYTES: usize = 256 * 1024;

/// Read size for the output pump. One pty buffer's worth on Linux, so a busy
/// shell is drained in as few syscalls as possible without holding the
/// scrollback lock for long.
const READ_CHUNK: usize = 8 * 1024;

/// Keystrokes and pastes queued for a shell that is not reading its input.
///
/// Bounded on purpose: a shell blocked on something slow must not let the
/// browser grow the server's memory. 256 chunks is more typing than anyone
/// does between reads, and hitting the limit is reported rather than hidden.
const INPUT_QUEUE: usize = 256;

/// What xterm.js advertises itself as, so programs pick the right terminfo.
const DEFAULT_TERM: &str = "xterm-256color";

/// A window size the browser cannot turn into a resource problem. A size is
/// untrusted input, and programs routinely allocate `rows * cols` cells.
const MAX_ROWS: u16 = 1_000;
const MAX_COLS: u16 = 2_000;

/// Bytes of randomness behind a terminal id.
const ID_BYTES: usize = 12;

// ---------------------------------------------------------------------------
// Identity and value types
// ---------------------------------------------------------------------------

/// A terminal's handle. Not a secret: it appears in audit records and in the
/// window state, and access is gated by the signed-in session, not by
/// knowing this. It is still random rather than a counter, so one client
/// cannot guess at another's terminals by incrementing.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct TerminalId(String);

impl TerminalId {
    pub fn generate() -> Result<TerminalId, TerminalError> {
        let mut bytes = [0u8; ID_BYTES];
        os_random(&mut bytes).map_err(|e| TerminalError::Random(e.to_string()))?;
        Ok(TerminalId(URL_SAFE_NO_PAD.encode(bytes)))
    }

    /// Accept an id that came in over the wire. Rejecting the shape here
    /// keeps an id out of log lines and thread names where it would need
    /// escaping.
    pub fn parse(s: &str) -> Option<TerminalId> {
        let ok = !s.is_empty()
            && s.len() <= 32
            && s.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
        ok.then(|| TerminalId(s.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for TerminalId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Who opened a terminal.
///
/// Kept so lockdown and sign-out can kill what a revoked session started:
/// "revoke every session" and "kill all child processes" are one act, and
/// without this the second half would have to kill every terminal including
/// the one the operator is sitting at on the host.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Opener {
    /// The host's own CLI or the server itself. Never revoked.
    Host,
    /// A signed-in browser session, identified by the hash of its id rather
    /// than the id itself. The registry outlives individual requests and is
    /// dumped into logs and reports, and a session id is a bearer credential;
    /// a digest identifies the session just as well for the one thing this is
    /// for, which is matching it against a revocation.
    Client(SessionHash),
}

/// Visible window size. Pixel dimensions are only consulted by programs that
/// draw images (sixel, kitty graphics); zero means "don't know", which is
/// what every terminal that does not report them sends.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct WinSize {
    pub rows: u16,
    pub cols: u16,
    #[serde(default)]
    pub pixel_width: u16,
    #[serde(default)]
    pub pixel_height: u16,
}

impl Default for WinSize {
    fn default() -> WinSize {
        // The size every terminal has always been until told otherwise.
        WinSize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        }
    }
}

impl WinSize {
    pub fn new(rows: u16, cols: u16) -> WinSize {
        WinSize {
            rows,
            cols,
            ..WinSize::default()
        }
    }

    /// A size that cannot hurt the host. A browser mid-layout reports zero
    /// rows, and a zero-row terminal makes `less` and `vim` misbehave rather
    /// than fail, which is much harder to diagnose than being clamped.
    fn clamped(self) -> WinSize {
        WinSize {
            rows: self.rows.clamp(1, MAX_ROWS),
            cols: self.cols.clamp(1, MAX_COLS),
            pixel_width: self.pixel_width,
            pixel_height: self.pixel_height,
        }
    }

    fn to_pty_size(self) -> PtySize {
        PtySize {
            rows: self.rows,
            cols: self.cols,
            pixel_width: self.pixel_width,
            pixel_height: self.pixel_height,
        }
    }
}

/// How a shell ended.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Exit {
    pub code: u32,
    /// Name of the signal that killed it, when one did. `portable-pty`
    /// reports this as text because the names are not portable numbers.
    pub signal: Option<String>,
    /// Unix seconds when the child was reaped.
    pub at: u64,
}

/// What to run in the terminal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Program {
    /// The user's login shell, found the way a terminal emulator finds it.
    /// `argv[0]` is prefixed with `-` so profile files are sourced; on macOS
    /// that is how `PATH` gets built at all, and on Termux it is how the
    /// Termux `PATH` arrives.
    LoginShell,
    /// A specific command, for a "show terminal" view of an adapter run.
    /// `argv[0]` is resolved against the child's `PATH`.
    Command(Vec<OsString>),
}

/// Everything needed to start a session.
#[derive(Debug, Clone)]
pub struct SessionSpec {
    pub program: Program,
    /// Starting directory. The working directory is the terminal's home, like
    /// any other app's view of the filesystem; a directory that does not
    /// exist falls back to the user's home rather than failing the spawn.
    pub cwd: PathBuf,
    pub size: WinSize,
    /// Extra environment for the child, applied over the server's own.
    pub env: Vec<(OsString, OsString)>,
    /// Variables to drop before spawning.
    ///
    /// The child inherits the server's environment, which is what makes the
    /// terminal feel like a normal login. Anything the server was handed as a
    /// secret in its environment must be named here, because a shell is the
    /// one place that environment becomes readable by anything the operator
    /// runs. Empty today: the server reads its secrets from the config file
    /// and the state database, not from the environment.
    pub env_remove: Vec<OsString>,
    pub opened_by: Opener,
}

impl SessionSpec {
    /// A plain shell in `cwd`, opened by the host.
    pub fn login_shell(cwd: impl Into<PathBuf>) -> SessionSpec {
        SessionSpec {
            program: Program::LoginShell,
            cwd: cwd.into(),
            size: WinSize::default(),
            env: Vec::new(),
            env_remove: Vec::new(),
            opened_by: Opener::Host,
        }
    }

    pub fn with_size(mut self, size: WinSize) -> SessionSpec {
        self.size = size;
        self
    }

    pub fn opened_by(mut self, who: Opener) -> SessionSpec {
        self.opened_by = who;
        self
    }

    /// A short label for the window title and the audit record.
    fn label(&self) -> String {
        match &self.program {
            Program::LoginShell => "shell".to_string(),
            Program::Command(argv) => argv
                .first()
                .map(|a| Path::new(a).file_name().unwrap_or(a.as_os_str()))
                .unwrap_or(OsStr::new("command"))
                .to_string_lossy()
                .into_owned(),
        }
    }
}

/// A terminal as the UI and the command bus see it, with no byte stream.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Info {
    pub id: TerminalId,
    pub label: String,
    pub size: WinSize,
    pub opened_at: u64,
    /// Process id of the shell on the hosting node. Meaningless anywhere
    /// else, shown so the operator can find it with the tools they know.
    pub pid: Option<u32>,
    pub exit: Option<Exit>,
}

/// Output handed to a client, with the cursor to ask from next time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Output {
    pub bytes: Vec<u8>,
    /// Absolute count of bytes this terminal has produced, up to the end of
    /// `bytes`. Pass it back to continue where this left off.
    pub cursor: u64,
    /// Output that fell out of the scrollback before this client asked for
    /// it. Non-zero means the stream has a hole in it: the client must reset
    /// its emulator before writing `bytes`, because the first byte may land
    /// in the middle of an escape sequence.
    pub skipped: u64,
    /// Set once the shell has exited and been reaped. The bytes before it are
    /// still delivered, so the client sees the last line.
    pub exit: Option<Exit>,
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

#[derive(Debug, Error)]
pub enum TerminalError {
    #[error(
        "{live} terminals are already open and the limit is {max}; \
         close one before opening another, or raise the limit in config"
    )]
    TooMany { live: usize, max: usize },

    #[error("cannot open a pseudo-terminal: {source}")]
    OpenPty {
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },

    #[error(
        "cannot start {program} in a pseudo-terminal: {source}; \
         check that it exists on this node and is executable"
    )]
    Spawn {
        program: String,
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },

    #[error(
        "terminal {id} does not exist here; it may have been closed, or belong to another node"
    )]
    NoSuchTerminal { id: TerminalId },

    #[error("terminal {id} has exited; open a new one")]
    Exited { id: TerminalId },

    #[error(
        "terminal {id} has {queued} chunks of input waiting and is not reading any of it; \
         the program in it is stuck, so the input was dropped rather than queued further"
    )]
    InputBacklog { id: TerminalId, queued: usize },

    #[error("terminal {id}: {action} failed: {source}")]
    Pty {
        id: TerminalId,
        action: &'static str,
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },

    #[error("could not signal the shell of terminal {id}: {source}")]
    Signal {
        id: TerminalId,
        #[source]
        source: nix::Error,
    },

    #[error("the OS random source failed: {0}; a terminal id cannot be generated")]
    Random(String),
}

// ---------------------------------------------------------------------------
// Injected world
// ---------------------------------------------------------------------------

/// The clock, so exit and open times are not read from under the module and
/// a test can place a session in the past without waiting.
pub trait Env: Send + Sync {
    /// Unix seconds now.
    fn now(&self) -> u64;
}

/// The real clock.
pub struct OsEnv;

impl Env for OsEnv {
    fn now(&self) -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0)
    }
}

/// Which signal to send a shell. Named rather than numeric so callers say
/// what they mean.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Hangup {
    /// `SIGHUP`: the window closed. A shell runs its exit traps and hangs up
    /// its jobs, which is what closing a terminal has always meant.
    Polite,
    /// `SIGKILL`: lockdown. Nothing is given a chance to ignore it.
    Now,
}

/// One running pseudo-terminal and the child in it.
///
/// A trait, not a struct, for two reasons: a test can drive a session over a
/// pipe pair with no shell and no timing, and a node that is not a Unix host
/// (a remote PTY forwarded from another node) can supply its own without this
/// module knowing.
pub trait PtyProcess: Send + Sync {
    /// Tell the kernel the window changed, which is what raises `SIGWINCH`
    /// in the child. The `TIOCSWINSZ` equivalent for whatever host this is.
    fn resize(&self, size: WinSize) -> Result<(), Box<dyn std::error::Error + Send + Sync>>;
    /// The output stream. Taken once, by the pump.
    fn reader(&self) -> Result<Box<dyn Read + Send>, Box<dyn std::error::Error + Send + Sync>>;
    /// The input stream. Taken once, by the writer thread.
    fn writer(&self) -> Result<Box<dyn Write + Send>, Box<dyn std::error::Error + Send + Sync>>;
    /// Process id of the child on this node, if it has one.
    fn pid(&self) -> Option<u32>;
    /// Signal the child's process group.
    fn hangup(&self, how: Hangup) -> Result<(), nix::Error>;
    /// Block until the child exits and reap it. Called once, by the pump,
    /// after the output stream ends.
    fn wait(&self) -> Option<(u32, Option<String>)>;
    /// Whether anything this terminal started may still be running once the
    /// pump is done with it: a job the shell left behind in its session.
    /// The registry keeps a closed terminal within the kill switch's reach
    /// until this says no. A pty with no processes behind it says no.
    fn lingers(&self) -> bool {
        false
    }
}

/// Opens pseudo-terminals.
pub trait PtyBackend: Send + Sync {
    fn open(
        &self,
        spec: &SessionSpec,
        id: &TerminalId,
    ) -> Result<Box<dyn PtyProcess>, TerminalError>;
}

/// The host's own pseudo-terminals, through `portable-pty`.
pub struct NativeBackend;

impl PtyBackend for NativeBackend {
    fn open(
        &self,
        spec: &SessionSpec,
        id: &TerminalId,
    ) -> Result<Box<dyn PtyProcess>, TerminalError> {
        let pair = native_pty_system()
            .openpty(spec.size.clamped().to_pty_size())
            .map_err(|e| TerminalError::OpenPty { source: e.into() })?;
        let PtyPair { slave, master } = pair;

        let mut cmd = match &spec.program {
            Program::LoginShell => CommandBuilder::new_default_prog(),
            Program::Command(argv) => CommandBuilder::from_argv(argv.clone()),
        };
        cmd.cwd(&spec.cwd);
        for key in &spec.env_remove {
            cmd.env_remove(key);
        }
        // Set before the caller's own entries so a spec can override them.
        cmd.env("TERM", DEFAULT_TERM);
        cmd.env("COLORTERM", "truecolor");
        cmd.env("TERM_PROGRAM", "deckjack");
        // So a script can tell it is running in a Deckjack terminal, and so
        // the audit log can be tied back to a stray process by its
        // environment.
        cmd.env("DECKJACK_TERMINAL", id.as_str());
        for (key, value) in &spec.env {
            cmd.env(key, value);
        }

        let child = slave.spawn_command(cmd).map_err(|e| TerminalError::Spawn {
            program: spec.label(),
            source: e.into(),
        })?;

        // The master only reports end-of-file once every slave handle is
        // closed. Holding this one would make the pump block forever after
        // the shell exits, and the session would never report its status.
        drop(slave);

        let killer = child.clone_killer();
        let pid = child.process_id();

        Ok(Box::new(NativePty {
            // `UnixMasterPty` keeps its "writer already taken" flag in a
            // `RefCell`, so the master is `Send` but not `Sync` and cannot be
            // shared behind a plain reference however read-only the call
            // looks.
            master: Mutex::new(master),
            child: Mutex::new(child),
            killer: Mutex::new(killer),
            pid,
            reaped: AtomicBool::new(false),
        }))
    }
}

struct NativePty {
    master: Mutex<Box<dyn MasterPty + Send>>,
    child: Mutex<Box<dyn Child + Send + Sync>>,
    killer: Mutex<Box<dyn ChildKiller + Send + Sync>>,
    pid: Option<u32>,
    /// Set once `wait` has collected the child. From then on the pid is the
    /// kernel's to hand to someone else, so signaling it — or the group or
    /// session it names — could hit an unrelated process. Every hangup after
    /// this is a no-op.
    reaped: AtomicBool,
}

impl PtyProcess for NativePty {
    fn resize(&self, size: WinSize) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        lock(&self.master)
            .resize(size.to_pty_size())
            .map_err(Into::into)
    }

    fn reader(&self) -> Result<Box<dyn Read + Send>, Box<dyn std::error::Error + Send + Sync>> {
        lock(&self.master).try_clone_reader().map_err(Into::into)
    }

    fn writer(&self) -> Result<Box<dyn Write + Send>, Box<dyn std::error::Error + Send + Sync>> {
        lock(&self.master).take_writer().map_err(Into::into)
    }

    fn pid(&self) -> Option<u32> {
        self.pid
    }

    fn hangup(&self, how: Hangup) -> Result<(), nix::Error> {
        use nix::sys::signal::{Signal, killpg};
        use nix::unistd::Pid;

        let Some(pid) = self.pid else {
            return Ok(());
        };
        if self.reaped.load(Ordering::Acquire) {
            // The shell is gone, but a job it left behind — `nohup cmd &`
            // and `exit` — is still in its session. The kernel does not hand
            // out a pid while any process still carries it as a session id,
            // so while such a job lives, the session id still means this
            // terminal; `left_behind` also checks no new process has taken
            // the pid. The polite hangup is the shell's to pass on, and the
            // shell is gone, so only the kill switch reaches this far.
            if how == Hangup::Now && left_behind(pid) {
                kill_session(pid);
            }
            return Ok(());
        }
        match how {
            // `clone_killer` exists so a kill can happen while another thread
            // is blocked in `wait`, which is exactly the pump's situation.
            // It signals the one process, so the group call below is what
            // actually takes the shell's children with it.
            //
            // The group is not the whole of it. An interactive shell puts
            // every job in a process group of its own, so `killpg` on the
            // shell's group leaves `make &` running as the host user — and
            // holding the pty open, so the output stream never ends and the
            // dead shell is never reaped. The shell is a session leader
            // (`portable-pty` calls `setsid`), so its session is exactly the
            // set of processes this terminal started, and that is what a
            // kill switch kills.
            //
            // Members first, leader last: while the leader is alive and
            // unreaped its pid cannot be reused, so the session id the scan
            // matches on still means this terminal.
            Hangup::Now => {
                kill_session(pid);
                let _ = lock(&self.killer).kill();
                killpg(Pid::from_raw(pid as i32), Signal::SIGKILL)
            }
            Hangup::Polite => killpg(Pid::from_raw(pid as i32), Signal::SIGHUP),
        }
        .or_else(|e| {
            // The group is already gone, which is the outcome asked for.
            if e == nix::errno::Errno::ESRCH {
                Ok(())
            } else {
                Err(e)
            }
        })
    }

    fn wait(&self) -> Option<(u32, Option<String>)> {
        let status = lock(&self.child).wait().ok()?;
        self.reaped.store(true, Ordering::Release);
        Some((status.exit_code(), status.signal().map(str::to_string)))
    }

    fn lingers(&self) -> bool {
        match self.pid {
            Some(pid) => !self.reaped.load(Ordering::Acquire) || left_behind(pid),
            None => false,
        }
    }
}

/// Whether processes remain in the session a reaped shell `sid` led.
///
/// Only while the pid itself is free: once a new process holds it, that
/// process may have started a session of its own under the same id, and its
/// members are not this terminal's to kill.
#[cfg(any(target_os = "linux", target_os = "android"))]
fn left_behind(sid: u32) -> bool {
    if std::path::Path::new(&format!("/proc/{sid}")).exists() {
        return false;
    }
    !session_members(sid).is_empty()
}

#[cfg(not(any(target_os = "linux", target_os = "android")))]
fn left_behind(_sid: u32) -> bool {
    false
}

/// `SIGKILL` every process in the session `sid` leads, other than the leader
/// itself (which the caller signals).
///
/// Read from `/proc`, the only interface Linux offers for "which processes
/// are in this session". A process that exits between the listing and the
/// kill is `ESRCH`, which is the outcome asked for. A process that forks
/// between the two is the one race this cannot close; it is a kill switch for
/// shells an operator was using, not a defense against a program written to
/// outrun it.
#[cfg(any(target_os = "linux", target_os = "android"))]
fn kill_session(sid: u32) {
    use nix::sys::signal::{Signal, kill};
    use nix::unistd::Pid;

    for pid in session_members(sid) {
        let _ = kill(Pid::from_raw(pid as i32), Signal::SIGKILL);
    }
}

/// Every process in session `sid` other than its leader, read from `/proc`.
#[cfg(any(target_os = "linux", target_os = "android"))]
fn session_members(sid: u32) -> Vec<u32> {
    let mut members = Vec::new();
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return members;
    };
    for entry in entries.flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|n| n.parse::<u32>().ok())
        else {
            continue;
        };
        if pid == sid {
            continue;
        }
        let Ok(stat) = std::fs::read_to_string(entry.path().join("stat")) else {
            continue;
        };
        if session_of_stat(&stat) == Some(sid) {
            members.push(pid);
        }
    }
    members
}

/// Elsewhere there is no portable way to list a session's members, so the
/// kill stays with the shell's process group, as it always was.
#[cfg(not(any(target_os = "linux", target_os = "android")))]
fn kill_session(_sid: u32) {}

/// The session id field of a `/proc/<pid>/stat` line.
///
/// The command name in field 2 is parenthesized and may itself contain spaces
/// and parentheses, so the fields are counted from the *last* `)`: state,
/// ppid, pgrp, session.
#[cfg_attr(not(any(target_os = "linux", target_os = "android")), allow(dead_code))]
fn session_of_stat(stat: &str) -> Option<u32> {
    let rest = &stat[stat.rfind(')')? + 1..];
    rest.split_whitespace().nth(3)?.parse().ok()
}

impl Drop for NativePty {
    fn drop(&mut self) {
        // A dropped session must not leave a shell attached to a pty nobody
        // holds. Closing the master would make the kernel hang up the
        // foreground process group, but background jobs in their own groups
        // would survive, so the group is signaled explicitly first.
        let _ = self.hangup(Hangup::Polite);
    }
}

// ---------------------------------------------------------------------------
// Scrollback
// ---------------------------------------------------------------------------

/// A bounded ring of recent output, addressed by absolute byte offset so a
/// client's cursor stays meaningful after old bytes are dropped.
struct Scrollback {
    buf: VecDeque<u8>,
    cap: usize,
    /// Absolute offset of `buf[0]`: how many bytes have fallen off the front.
    base: u64,
}

impl Scrollback {
    fn new(cap: usize) -> Scrollback {
        Scrollback {
            buf: VecDeque::new(),
            cap: cap.max(1),
            base: 0,
        }
    }

    fn total(&self) -> u64 {
        self.base + self.buf.len() as u64
    }

    fn push(&mut self, bytes: &[u8]) {
        if bytes.len() >= self.cap {
            // A single write bigger than the whole ring: only its tail can
            // survive, and everything before it is a hole.
            self.base = self.total() + (bytes.len() - self.cap) as u64;
            self.buf.clear();
            self.buf.extend(&bytes[bytes.len() - self.cap..]);
            return;
        }
        self.buf.extend(bytes);
        let over = self.buf.len().saturating_sub(self.cap);
        if over > 0 {
            self.buf.drain(..over);
            self.base += over as u64;
        }
    }

    /// Everything held from `cursor` onward, and how much was missed. A
    /// cursor beyond the end means the client is ahead of the stream, which
    /// is treated as caught up rather than as an error: it happens whenever a
    /// client's state outlives a restart.
    fn from(&self, cursor: u64) -> (Vec<u8>, u64) {
        let skipped = self.base.saturating_sub(cursor);
        let start = (cursor.max(self.base) - self.base) as usize;
        let start = start.min(self.buf.len());
        (self.buf.range(start..).copied().collect(), skipped)
    }
}

// ---------------------------------------------------------------------------
// Session
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
enum Life {
    Running,
    Exited(Exit),
}

/// One terminal: a pty, the shell in it, and the recent output.
pub struct TerminalSession {
    id: TerminalId,
    label: String,
    /// Behind a lock because it can change exactly one way: the single seat
    /// moving to another browser session hands that session the previous
    /// holder's terminals ([`Registry::transfer_owner`]). Nothing else
    /// writes it.
    opened_by: Mutex<Opener>,
    opened_at: u64,
    env: Arc<dyn Env>,
    /// Shared with the output pump, which must be able to reap the child
    /// after this session is gone: see [`pump`].
    pty: Arc<dyn PtyProcess>,
    /// Input goes through a thread so that a shell which has stopped reading
    /// cannot block an async handler on a pty write. `SyncSender` is both
    /// bounded and `Sync`, so no lock is needed around it.
    input: SyncSender<Vec<u8>>,
    out: Mutex<Scrollback>,
    size: Mutex<WinSize>,
    life: Mutex<Life>,
    /// Woken on every chunk of output and once on exit.
    wake: tokio::sync::Notify,
}

impl std::fmt::Debug for TerminalSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TerminalSession")
            .field("id", &self.id)
            .field("label", &self.label)
            .field("opened_by", &self.opened_by())
            .field("pid", &self.pty.pid())
            .finish_non_exhaustive()
    }
}

impl TerminalSession {
    pub fn id(&self) -> &TerminalId {
        &self.id
    }

    /// Who owns this terminal now: whoever opened it, or the session the
    /// seat moved to since.
    pub fn opened_by(&self) -> Opener {
        *lock(&self.opened_by)
    }

    pub fn size(&self) -> WinSize {
        *lock(&self.size)
    }

    pub fn exit(&self) -> Option<Exit> {
        match &*lock(&self.life) {
            Life::Running => None,
            Life::Exited(e) => Some(e.clone()),
        }
    }

    pub fn info(&self) -> Info {
        Info {
            id: self.id.clone(),
            label: self.label.clone(),
            size: self.size(),
            opened_at: self.opened_at,
            pid: self.pty.pid(),
            exit: self.exit(),
        }
    }

    /// Tell the shell the window changed. Idempotent, and cheap enough to
    /// call on every drag frame.
    pub fn resize(&self, size: WinSize) -> Result<(), TerminalError> {
        let size = size.clamped();
        let mut held = lock(&self.size);
        if *held == size {
            return Ok(());
        }
        self.pty.resize(size).map_err(|source| TerminalError::Pty {
            id: self.id.clone(),
            action: "resize",
            source,
        })?;
        *held = size;
        Ok(())
    }

    /// Queue bytes for the shell's input. Returns without waiting for them to
    /// be consumed, so a wedged program cannot stall the caller.
    pub fn write(&self, bytes: &[u8]) -> Result<(), TerminalError> {
        if bytes.is_empty() {
            return Ok(());
        }
        // Asked here rather than left to the pty. A write to a master whose
        // child is gone is accepted by the queue and then fails inside the
        // input thread, so without this check a client would type into a dead
        // terminal and never be told.
        if self.exit().is_some() {
            return Err(TerminalError::Exited {
                id: self.id.clone(),
            });
        }
        match self.input.try_send(bytes.to_vec()) {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(_)) => Err(TerminalError::InputBacklog {
                id: self.id.clone(),
                queued: INPUT_QUEUE,
            }),
            // The input thread ended because the pty rejected a write, which
            // means the shell is on its way out even if it has not been
            // reaped yet.
            Err(TrySendError::Disconnected(_)) => Err(TerminalError::Exited {
                id: self.id.clone(),
            }),
        }
    }

    /// All the output still held, for a browser that has just attached.
    ///
    /// `skipped` is zero here by definition: a fresh client has missed
    /// nothing it ever had. What fell out of the ring before it attached is
    /// simply not part of this terminal's history any more.
    pub fn snapshot(&self) -> Output {
        // Sampled before the bytes, for the reason given in `read_from`.
        let exit = self.exit();
        let held = lock(&self.out);
        let (bytes, _) = held.from(0);
        let cursor = held.total();
        drop(held);
        Output {
            bytes,
            cursor,
            skipped: 0,
            exit,
        }
    }

    /// Output from `cursor` onward, without waiting.
    pub fn read_from(&self, cursor: u64) -> Output {
        // The exit is sampled *before* the scrollback, because the pump
        // pushes every byte it read and only then records the exit. In this
        // order, an exit seen here means the read below already holds the
        // shell's last line. The other order loses it: the scrollback can be
        // read, then the final chunk pushed and the exit recorded, and the
        // result would report the exit with the last line missing — which a
        // client that stops following on `exit` would never come back for.
        let exit = self.exit();
        let held = lock(&self.out);
        let (bytes, skipped) = held.from(cursor);
        let cursor = held.total();
        drop(held);
        Output {
            bytes,
            cursor,
            skipped,
            exit,
        }
    }

    /// Output from `cursor` onward, waiting until there is some or the shell
    /// exits. This is the follow loop a WebSocket handler runs.
    pub async fn output_after(&self, cursor: u64) -> Output {
        loop {
            // The waiter is registered *before* the buffer is read.
            // `notify_waiters` leaves no permit behind, so output that
            // arrived between the read and the await would be lost otherwise,
            // and the client would hang with bytes sitting in the ring.
            let waiting = self.wake.notified();
            let mut waiting = std::pin::pin!(waiting);
            waiting.as_mut().enable();

            let out = self.read_from(cursor);
            if !out.bytes.is_empty() || out.skipped > 0 || out.exit.is_some() {
                return out;
            }
            waiting.await;
        }
    }

    /// Wait for the shell to exit.
    pub async fn wait_for_exit(&self) -> Exit {
        loop {
            let waiting = self.wake.notified();
            let mut waiting = std::pin::pin!(waiting);
            waiting.as_mut().enable();
            if let Some(exit) = self.exit() {
                return exit;
            }
            waiting.await;
        }
    }

    /// Signal the shell and everything in its process group.
    ///
    /// Returns once the signal is delivered, not once the shell is gone;
    /// [`wait_for_exit`](Self::wait_for_exit) is how a caller learns it died.
    pub fn hangup(&self, how: Hangup) -> Result<(), TerminalError> {
        self.pty
            .hangup(how)
            .map_err(|source| TerminalError::Signal {
                id: self.id.clone(),
                source,
            })
    }

    /// Record the exit and wake everyone following. Called by the pump once
    /// the output stream ends and the child has been waited on.
    fn record_exit(&self, status: Option<(u32, Option<String>)>) {
        let (code, signal) = status.unwrap_or((
            // The child could not be reaped, which on Unix means something
            // else already did. Its output stream has ended either way, so
            // the honest report is an unknown non-zero status rather than
            // leaving the session looking alive forever.
            1, None,
        ));
        let exit = Exit {
            code,
            signal,
            at: self.env.now(),
        };
        *lock(&self.life) = Life::Exited(exit);
        self.wake.notify_waiters();
    }
}

/// Drain queued input into the pty. One thread per session, because a pty
/// write blocks when the child is not reading and nothing above may block.
fn spawn_input_thread(
    id: &TerminalId,
    mut writer: Box<dyn Write + Send>,
) -> io::Result<SyncSender<Vec<u8>>> {
    let (tx, rx) = sync_channel::<Vec<u8>>(INPUT_QUEUE);
    let id = id.clone();
    std::thread::Builder::new()
        .name(format!("dj-pty-in-{id}"))
        .spawn(move || {
            for chunk in rx {
                if let Err(e) = writer.write_all(&chunk).and_then(|()| writer.flush()) {
                    tracing::debug!(terminal = %id, error = %e, "pty input closed");
                    break;
                }
            }
        })?;
    Ok(tx)
}

/// Read the pty until it ends, appending to the scrollback, then reap the
/// child.
///
/// Holds a [`Weak`] to the session rather than an [`Arc`]: a shell may sit
/// idle for days, and a strong reference here would keep a closed session
/// and its scrollback alive for as long as it did.
///
/// It holds the *pty* strongly, though, and that is the half that matters.
/// The pump is the one thing that waits on the child, and it must do so
/// whether or not anyone still holds the session: `close` and `kill_all`
/// forget the session first and the shell dies afterwards, so a pump that
/// reaped through the session found it gone and skipped the `wait`, leaving a
/// zombie for every terminal a lockdown or a close ever took.
fn spawn_output_pump(
    session: &Arc<TerminalSession>,
    mut reader: Box<dyn Read + Send>,
) -> io::Result<()> {
    let weak = Arc::downgrade(session);
    let pty = Arc::clone(&session.pty);
    let id = session.id.clone();
    std::thread::Builder::new()
        .name(format!("dj-pty-out-{id}"))
        .spawn(move || pump(&weak, pty, &id, reader.as_mut()))?;
    Ok(())
}

fn pump(
    weak: &Weak<TerminalSession>,
    pty: Arc<dyn PtyProcess>,
    id: &TerminalId,
    reader: &mut (dyn Read + Send),
) {
    let mut buf = [0u8; READ_CHUNK];
    loop {
        let n = match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            // Linux reports the last slave closing as `EIO` on the master
            // rather than as end-of-file. That is the normal end of a
            // session, not a fault, so it is logged at debug and the loop
            // ends the same way.
            Err(e) => {
                tracing::debug!(terminal = %id, error = %e, "pty output ended");
                break;
            }
        };
        // A session closed while the shell is still printing has nowhere to
        // put the output, but the stream is still drained to its end: a
        // shell blocked writing to a full pty never gets as far as exiting,
        // and the end of the stream is what says it is safe to wait.
        if let Some(session) = weak.upgrade() {
            lock(&session.out).push(&buf[..n]);
            session.wake.notify_waiters();
        }
    }

    // Reaped unconditionally, before the session is even looked for.
    let status = pty.wait();
    // Let go of the pty before the exit is announced, so that once a session
    // reads as exited the pump holds nothing of it: pruning the session is
    // then what closes the master, not a race with this thread's last line.
    drop(pty);
    if let Some(session) = weak.upgrade() {
        session.record_exit(status);
    }
}

// ---------------------------------------------------------------------------
// Registry
// ---------------------------------------------------------------------------

/// Bounds on what the registry will hold.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// Running sessions allowed at once. A limit exists because each one
    /// costs a pty, two threads and a scrollback, and because a client bug
    /// that opens terminals in a loop should hit a wall on the host rather
    /// than take it down.
    pub max_sessions: usize,
    pub scrollback_bytes: usize,
    /// How long an exited terminal is kept, in seconds, so a browser that
    /// reconnects can still read its last output and how it ended.
    ///
    /// An exited terminal is not free: until it is forgotten it holds its pty
    /// master, the thread that feeds its input and its scrollback. So it is
    /// kept for this long and no longer.
    pub exited_grace_secs: u64,
    /// The most exited terminals kept at once, whatever their age. Exited
    /// terminals do not count against [`Limits::max_sessions`], so without
    /// this a loop of open-and-exit would hold a pty per pass for the whole
    /// grace period. The oldest exit goes first.
    pub max_exited: usize,
}

impl Default for Limits {
    fn default() -> Limits {
        Limits {
            max_sessions: 16,
            scrollback_bytes: SCROLLBACK_BYTES,
            exited_grace_secs: 15 * 60,
            max_exited: 8,
        }
    }
}

/// Every terminal this server holds.
///
/// Sessions live here and not in the client, which is what makes a reconnect
/// a reattach. They do not survive a restart: a pty master is a file
/// descriptor in this process, so when the server goes the shells go with it.
pub struct Registry {
    env: Arc<dyn Env>,
    backend: Arc<dyn PtyBackend>,
    limits: Limits,
    live: Mutex<HashMap<TerminalId, Arc<TerminalSession>>>,
    /// Terminals closed but possibly not ended: a job that ignored the
    /// hangup, or one the shell left behind, runs on after `close` has
    /// forgotten the session. Kept so the kill switch still reaches them;
    /// dropped once the pump is done and nothing lingers.
    closed: Mutex<Vec<Arc<dyn PtyProcess>>>,
    /// Where a browser session's terminals went: `from -> to` for every
    /// session whose terminals [`Registry::transfer_owner`] handed on, to
    /// whoever has them now. A terminal that `from` asked for before the
    /// move but that registers after it — `terminal.open` was already
    /// running when the seat changed hands — goes the same way, instead of
    /// landing with a session that can no longer use it. Taken after `live`,
    /// never before.
    moved: Mutex<HashMap<SessionHash, SessionHash>>,
}

impl Registry {
    pub fn new(env: Arc<dyn Env>, backend: Arc<dyn PtyBackend>, limits: Limits) -> Registry {
        Registry {
            env,
            backend,
            limits,
            live: Mutex::new(HashMap::new()),
            closed: Mutex::new(Vec::new()),
            moved: Mutex::new(HashMap::new()),
        }
    }

    /// Keep a closed terminal's pty within the kill switch's reach, and let
    /// go of the ones that are over: only this list still holds them (the
    /// pump has reaped and returned) and nothing they started is running.
    ///
    /// The new ones are judged with the rest: one that nothing else holds and
    /// that left nothing running is let go now, so its master closes now. A
    /// caller that still holds the session holds the pty too, so a terminal
    /// just closed is always kept until a later call.
    fn keep_closed(&self, ptys: impl IntoIterator<Item = Arc<dyn PtyProcess>>) {
        let mut closed = lock(&self.closed);
        closed.extend(ptys);
        closed.retain(|p| Arc::strong_count(p) > 1 || p.lingers());
    }

    /// Exited terminals past their grace, and those beyond
    /// [`Limits::max_exited`], taken out of `live`.
    ///
    /// The cap is node-wide, so it has to choose whose terminals go. Each
    /// eviction takes the oldest exit of whichever opener holds the most
    /// exited terminals: a session that opens and exits shells in a loop
    /// pays for its own loop, and another session's recently exited
    /// terminal stays reattachable for its grace.
    fn take_expired(
        &self,
        live: &mut HashMap<TerminalId, Arc<TerminalSession>>,
    ) -> Vec<Arc<TerminalSession>> {
        let cutoff = self.env.now().saturating_sub(self.limits.exited_grace_secs);
        let mut exited: Vec<(u64, u64, TerminalId, Opener)> = live
            .values()
            .filter_map(|s| {
                s.exit()
                    .map(|e| (e.at, s.opened_at, s.id.clone(), s.opened_by()))
            })
            .collect();
        // Oldest exit first, so within one opener the cap takes the ones
        // least likely to be looked at again.
        exited.sort_by(|a, b| (a.0, a.1, a.2.as_str()).cmp(&(b.0, b.1, b.2.as_str())));
        let (mut doomed, mut kept): (Vec<_>, Vec<_>) =
            exited.into_iter().partition(|(at, ..)| *at < cutoff);
        while kept.len() > self.limits.max_exited {
            let held = |o: &Opener| kept.iter().filter(|k| &k.3 == o).count();
            // The first opener (in exit order) with the largest share; its
            // first entry in `kept` is its oldest exit.
            let most = kept.iter().map(|k| held(&k.3)).max().unwrap_or(0);
            let Some(i) = kept.iter().position(|k| held(&k.3) == most) else {
                break;
            };
            doomed.push(kept.remove(i));
        }
        doomed
            .into_iter()
            .filter_map(|(_, _, id, _)| live.remove(&id))
            .collect()
    }

    /// Drop sessions already out of `live`, keeping their ptys within the
    /// kill switch's reach for as long as something they started lingers.
    fn let_go(&self, sessions: Vec<Arc<TerminalSession>>) -> usize {
        let n = sessions.len();
        let ptys: Vec<Arc<dyn PtyProcess>> = sessions.iter().map(|s| Arc::clone(&s.pty)).collect();
        // The sessions first: each one holds its pty and its input thread's
        // sender, and the pty can only be let go once nothing else holds it.
        drop(sessions);
        self.keep_closed(ptys);
        n
    }

    /// Forget exited terminals past [`Limits::exited_grace_secs`], and the
    /// oldest beyond [`Limits::max_exited`]. Returns how many went.
    ///
    /// Run on every open and list, and on a timer by the wiring, so the bound
    /// holds whether or not anyone is using the terminal app.
    pub fn reap_exited(&self) -> usize {
        let expired = {
            let mut live = lock(&self.live);
            self.take_expired(&mut live)
        };
        let n = self.let_go(expired);
        if n > 0 {
            tracing::debug!(count = n, "exited terminals forgotten");
        }
        n
    }

    pub fn limits(&self) -> Limits {
        self.limits
    }

    /// Start a shell. The caller has already decided this is allowed.
    pub fn open(&self, spec: SessionSpec) -> Result<Arc<TerminalSession>, TerminalError> {
        // Clamped here rather than inside each backend. The size is
        // untrusted input, and a backend that is not the host's own ptys — a
        // pty forwarded from another node, say — would otherwise be handed a
        // zero-row or 65535-row window while this registry recorded the
        // clamped one, leaving the server and the terminal disagreeing about
        // a size with nothing to resync them. Clamping twice is harmless.
        let mut spec = spec;
        spec.size = spec.size.clamped();
        self.reap_exited();
        let mut live = lock(&self.live);

        // Exited sessions are kept so their last output can still be read.
        // They are not counted here — a few crashed shells must not wedge the
        // terminal app — because they are bounded on their own, by
        // `exited_grace_secs` and `max_exited`, and `reap_exited` above has
        // just applied both.
        let running = live.values().filter(|s| s.exit().is_none()).count();
        if running >= self.limits.max_sessions {
            return Err(TerminalError::TooMany {
                live: running,
                max: self.limits.max_sessions,
            });
        }

        let id = TerminalId::generate()?;
        if live.contains_key(&id) {
            // 96 random bits collided. That is not luck, it is a broken
            // random source, and quietly reusing the id would hand one
            // client another's terminal.
            return Err(TerminalError::Random(
                "generated a terminal id that is already in use".to_string(),
            ));
        }

        let size = spec.size;
        let pty: Arc<dyn PtyProcess> = Arc::from(self.backend.open(&spec, &id)?);

        let reader = pty.reader().map_err(|source| TerminalError::Pty {
            id: id.clone(),
            action: "opening the output stream",
            source,
        })?;
        let writer = pty.writer().map_err(|source| TerminalError::Pty {
            id: id.clone(),
            action: "opening the input stream",
            source,
        })?;
        let input = spawn_input_thread(&id, writer).map_err(|e| TerminalError::Pty {
            id: id.clone(),
            action: "starting the input thread",
            source: Box::new(e),
        })?;

        // Under `live`, the lock `transfer_owner` holds while it moves
        // terminals and records the move: either the move already happened
        // and is applied here, or it happens after this pty is in `live` and
        // moves it.
        let opened_by = match spec.opened_by {
            Opener::Client(asked) => {
                Opener::Client(lock(&self.moved).get(&asked).copied().unwrap_or(asked))
            }
            Opener::Host => Opener::Host,
        };
        let session = Arc::new(TerminalSession {
            id: id.clone(),
            label: spec.label(),
            opened_by: Mutex::new(opened_by),
            opened_at: self.env.now(),
            env: Arc::clone(&self.env),
            pty,
            input,
            out: Mutex::new(Scrollback::new(self.limits.scrollback_bytes)),
            size: Mutex::new(size),
            life: Mutex::new(Life::Running),
            wake: tokio::sync::Notify::new(),
        });

        spawn_output_pump(&session, reader).map_err(|e| TerminalError::Pty {
            id: id.clone(),
            action: "starting the output thread",
            source: Box::new(e),
        })?;

        live.insert(id, Arc::clone(&session));
        tracing::info!(
            terminal = %session.id,
            label = %session.label,
            pid = ?session.pty.pid(),
            opened_by = ?session.opened_by(),
            "terminal opened",
        );
        Ok(session)
    }

    /// Reattach to a terminal the client already knows about.
    pub fn attach(&self, id: &TerminalId) -> Result<Arc<TerminalSession>, TerminalError> {
        lock(&self.live)
            .get(id)
            .map(Arc::clone)
            .ok_or_else(|| TerminalError::NoSuchTerminal { id: id.clone() })
    }

    /// Everything held, oldest first, so a client can rebuild its tab strip.
    pub fn list(&self) -> Vec<Info> {
        self.reap_exited();
        let mut out: Vec<Info> = lock(&self.live).values().map(|s| s.info()).collect();
        out.sort_by_key(|i| (i.opened_at, i.id.0.clone()));
        out
    }

    /// The terminals one opener owns.
    ///
    /// [`Self::list`] reports every terminal on the node, which is right for
    /// an operator-facing report and wrong for answering a session that asked
    /// what *it* has open: an [`Info`] carries the [`TerminalId`], and that
    /// id is all it takes to reach a terminal through [`Self::attach`]. So
    /// the unfiltered list handed any signed-in session the handle to every
    /// other session's shell. Callers serving a session use this.
    pub fn list_for(&self, owner: &Opener) -> Vec<Info> {
        self.reap_exited();
        let mut out: Vec<Info> = lock(&self.live)
            .values()
            .filter(|s| &s.opened_by() == owner)
            .map(|s| s.info())
            .collect();
        out.sort_by_key(|i| (i.opened_at, i.id.0.clone()));
        out
    }

    /// Close one terminal: hang up its shell and forget it.
    ///
    /// The shell's own hangup handling is given a chance to run, and the pty
    /// closes when the last reference to the session drops, which is itself a
    /// hangup for anything still attached to it.
    pub fn close(&self, id: &TerminalId) -> Result<(), TerminalError> {
        let session = lock(&self.live)
            .remove(id)
            .ok_or_else(|| TerminalError::NoSuchTerminal { id: id.clone() })?;
        tracing::info!(terminal = %id, "terminal closed");
        let hung = session.hangup(Hangup::Polite);
        self.keep_closed([Arc::clone(&session.pty)]);
        hung
    }

    /// Lockdown: kill every shell, now, and forget them all.
    ///
    /// Returns how many were killed. `SIGKILL` and not `SIGHUP` because a
    /// kill switch that a shell can trap is not a kill switch.
    pub fn kill_all(&self) -> usize {
        let sessions: Vec<Arc<TerminalSession>> =
            lock(&self.live).drain().map(|(_, s)| s).collect();
        let mut killed = 0;
        for session in &sessions {
            // An exited shell is signaled too, for the jobs it left behind
            // (see `NativePty::hangup`); it is just not counted as killed.
            let running = session.exit().is_none();
            match session.hangup(Hangup::Now) {
                Ok(()) if running => killed += 1,
                Ok(()) => {}
                Err(e) => {
                    tracing::warn!(terminal = %session.id, error = %e, "could not kill shell")
                }
            }
        }
        // Terminals already closed, whose jobs may have ignored the hangup.
        let closed: Vec<Arc<dyn PtyProcess>> = lock(&self.closed).drain(..).collect();
        for pty in &closed {
            if let Err(e) = pty.hangup(Hangup::Now) {
                tracing::warn!(error = %e, "could not kill a closed terminal's processes");
            }
        }
        if killed > 0 {
            tracing::warn!(count = killed, "all terminals killed");
        }
        killed
    }

    /// Close the terminals a session opened, for sign-out and for revocation.
    /// Terminals opened on the host are left alone: the operator at the
    /// keyboard did not sign out.
    pub fn close_for_session(&self, session: &SessionHash) -> usize {
        let doomed: Vec<Arc<TerminalSession>> = {
            let mut live = lock(&self.live);
            let ids: Vec<TerminalId> = live
                .values()
                .filter(|s| s.opened_by() == Opener::Client(*session))
                .map(|s| s.id.clone())
                .collect();
            let removed = ids.iter().filter_map(|id| live.remove(id)).collect();
            // An ended session neither forwards nor is forwarded to.
            lock(&self.moved).retain(|from, to| from != session && to != session);
            removed
        };
        for session in &doomed {
            if let Err(e) = session.hangup(Hangup::Polite) {
                tracing::warn!(terminal = %session.id, error = %e, "could not hang up shell");
            }
        }
        self.keep_closed(doomed.iter().map(|s| Arc::clone(&s.pty)));
        doomed.len()
    }

    /// Hand every terminal `from` owns to `to`. Returns how many moved.
    ///
    /// For the single seat (`crate::seat`): the workspace follows whoever
    /// holds it, so when another browser session takes the seat it takes the
    /// previous holder's shells with it. After this the new session can
    /// reattach and the old one cannot — the ownership check in the terminal
    /// commands is unchanged, it simply answers for the new owner — and the
    /// shells now die with the new session rather than the old one, which is
    /// the session that is actually using them.
    ///
    /// Host terminals are never moved, and moving a session's terminals to
    /// itself moves nothing.
    pub fn transfer_owner(&self, from: &SessionHash, to: &SessionHash) -> usize {
        if from == to {
            return 0;
        }
        let live = lock(&self.live);
        let mut moved = 0;
        for session in live.values() {
            let mut owner = lock(&session.opened_by);
            if *owner == Opener::Client(*from) {
                *owner = Opener::Client(*to);
                moved += 1;
            }
        }
        // Record it, for a terminal `from` (or a session that forwarded to
        // it) asked for that is not in `live` yet. `to` owns its terminals
        // now, so it forwards nowhere.
        let mut forwards = lock(&self.moved);
        for dest in forwards.values_mut() {
            if dest == from {
                *dest = *to;
            }
        }
        forwards.insert(*from, *to);
        forwards.remove(to);
        drop(forwards);
        if moved > 0 {
            tracing::info!(
                count = moved,
                "terminals moved to the session that took the seat"
            );
        }
        moved
    }

    /// Forget sessions that exited before `cutoff` (unix seconds).
    ///
    /// Exited sessions are kept on purpose so the browser can read the last
    /// output and the exit status after a reconnect. They are not kept
    /// forever, or a long-running server accumulates every shell it ever ran.
    ///
    /// [`Self::reap_exited`] is the policy the server runs; this is the same
    /// release with a cutoff the caller picks.
    pub fn prune_exited(&self, cutoff: u64) -> usize {
        let gone: Vec<Arc<TerminalSession>> = {
            let mut live = lock(&self.live);
            let ids: Vec<TerminalId> = live
                .values()
                .filter(|s| s.exit().is_some_and(|exit| exit.at < cutoff))
                .map(|s| s.id.clone())
                .collect();
            ids.iter().filter_map(|id| live.remove(id)).collect()
        };
        self.let_go(gone)
    }
}

impl Drop for Registry {
    fn drop(&mut self) {
        // A server shutting down must not leave shells attached to ptys that
        // no longer have a reader. Polite, because this is also the path a
        // graceful stop takes, and a shell deserves to run its exit traps.
        if let Ok(live) = self.live.lock() {
            for session in live.values() {
                let _ = session.hangup(Hangup::Polite);
            }
        }
    }
}

/// A poisoned lock here means another thread panicked while holding it. The
/// data behind it is a byte buffer, a window size or an exit status, none of
/// which can be left half-written in a way that matters, so recovering is
/// better than taking the whole server down with it.
fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::session_of_stat;

    /// A program can name itself anything, including text that looks like
    /// the fields after it. Counting from the first `)` would read this
    /// one's session as `5`, and the kill switch would sweep session 5.
    #[test]
    fn the_session_is_read_past_a_command_name_that_contains_parentheses() {
        let stat = "4242 (evil) S 1 5 5) S 1 777 4242 34816 4242 4194560 0 0";
        assert_eq!(session_of_stat(stat), Some(4242));
    }

    #[test]
    fn a_truncated_stat_line_has_no_session() {
        assert_eq!(session_of_stat("4242 (sh) S 1"), None);
        assert_eq!(session_of_stat(""), None);
    }
}
