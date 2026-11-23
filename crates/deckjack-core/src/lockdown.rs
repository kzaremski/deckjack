//! Lockdown: the kill switch.
//!
//! Locking everything down rather than killing the process, because a
//! supervisor or an attacker could simply restart it. Four steps, in order:
//!
//! 1. **Revoke** every session and rotate the session signing key.
//! 2. **Kill** all child processes: terminals, agents, app binaries, sync
//!    services.
//! 3. **Freeze** outbound actions: queued mail, pending Inbox items and
//!    scheduled tasks.
//! 4. **Serve only a "locked down" page** until unlocked.
//!
//! **Deliberately asymmetric.** Triggering takes one click from any
//! signed-in session with no step-up, because in an emergency friction is
//! the enemy and the cost of a false positive is an inconvenience.
//! Unlocking takes the passkey *plus* either `deckjack unlock` on the host
//! or an offline recovery code, because the whole point is that whoever
//! holds a stolen session cannot undo it.
//!
//! **The limit, stated plainly: it defeats a stolen session, not a
//! compromised host.** If someone has a shell as my user they can read this
//! file, delete the journal, patch the binary or simply not run it. The
//! levers for that case are outside Deckjack — revoke the node in the
//! Tailscale admin console, stop Caddy on the VPS, stop the LXC in Proxmox,
//! or pull power. Nothing in this module should be mistaken for a defense
//! against local root, or against the host user at all.
//!
//! **Why the state is persisted.** A process restart is the exact case this
//! exists for, so the locked state lives on disk and is read back at
//! startup. It is a small JSON file beside `lock.json` rather than a row in
//! `state.db`: the kill switch must work when SQLite is wedged, mid-WAL or
//! half-migrated, and the first thing engaging does is write that file.
//!
//! Everything that touches the clock, randomness, processes, sessions or the
//! filesystem arrives through a trait, so every rule here is exercised
//! without sleeping, spawning or revoking anything real.
//!
//! ```no_run
//! use std::sync::Arc;
//! use deckjack_core::lockdown::{self, Lockdown, Policy, Surface, Trigger};
//!
//! # fn demo(deps: lockdown::Deps, node_id: String) -> Result<(), lockdown::StartupError> {
//! // At startup: whatever the journal says wins, including "still locked".
//! let ld = Arc::new(Lockdown::restore(deps, Policy::default(), node_id)?);
//!
//! // One click from a signed-in session. No step-up, no confirmation.
//! if lockdown::may_engage(true) {
//!     let report = ld.engage(Trigger::Operator { surface: Surface::Palette });
//!     tracing::warn!(?report, "lockdown engaged");
//! }
//! # Ok(())
//! # }
//! ```

use std::collections::VecDeque;
use std::fmt;
use std::fs;
use std::io;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use thiserror::Error;

use crate::auth::code::{ALPHABET, RandomError, os_random};

/// Where the locked state lives, relative to the working directory. Beside
/// `lock.json` and `state.db`, inside the hidden directory that is never
/// served over the file API, WebDAV, search or agent tools.
pub const JOURNAL_PATH: &str = ".deckjack/lockdown.json";

/// Bumped if [`Record`] ever changes shape. A journal from the future is
/// treated as "locked", never as "unreadable, so presumably fine".
pub const RECORD_SCHEMA: u32 = 1;

/// Symbols per recovery code. Twenty Crockford base32 symbols is 100 bits,
/// which is why these are hashed with a plain SHA-256 below rather than a
/// password KDF: there is nothing to grind through.
pub const RECOVERY_CODE_LEN: usize = 20;

/// How many codes `recovery-codes --new` issues. Enough to put one in a
/// wallet, one in a safe and one in a password manager on another device.
pub const RECOVERY_CODES_ISSUED: usize = 10;

/// Longer than this is not a mistyped code, so it is rejected before any
/// work is done on it.
const MAX_CODE_INPUT: usize = 128;

/// Command ids on the bus, so the palette, the CLI and the MCP tool surface
/// all name the same two commands.
///
/// `wiring::Lockdown` registers [`CMD_ENGAGE`] and declares its risk there,
/// deliberately as `Write` rather than `Destructive`: risk level decides
/// whether the *operator* is asked to confirm, and triggering is one click by
/// design. What keeps an agent out is the grant.
///
/// [`CMD_UNLOCK`] is deliberately **not** on the bus. Every bus route is
/// behind the gate, which refuses everything while locked, so a command there
/// could never be reached in the one state it exists for. The unlock surface
/// is `POST /unlock` — see [`unlock_router`] — which is in
/// [`ALLOWED_WHILE_LOCKED`] and needs the host token plus a recovery code.
/// The constant stays because the CLI and the MCP tool surface name it.
pub const CMD_ENGAGE: &str = "system.lockdown";
pub const CMD_UNLOCK: &str = "system.unlock";

/// Whether a caller may engage lockdown.
///
/// This is the whole authorization rule, and it is thin on purpose: any
/// signed-in session, no step-up, no re-prompt for the passkey, no
/// confirmation dialog. A stolen session that panics the owner into locking
/// their own workspace has cost nothing; hesitating during a real incident
/// costs everything.
pub fn may_engage(signed_in: bool) -> bool {
    signed_in
}

// ---- secrets ------------------------------------------------------------

/// The key sessions are signed with. Rotating it is step 1, alongside
/// revoking the sessions themselves, so that a cookie minted before the
/// incident cannot be replayed even if the revocation list is lost.
///
/// TODO(session-signing): nothing signs with this yet. `auth::session`
/// currently issues 256 bits of raw randomness looked up in a map, which
/// needs no key. The type and the rotation step exist now so that when
/// signed cookies arrive the kill switch does not have to be reopened; until
/// then [`Sessions::adopt_signing_key`] is handed a key nobody uses, and
/// revocation alone is what defeats the stolen cookie.
#[derive(Clone)]
pub struct SessionKey([u8; 32]);

impl fmt::Debug for SessionKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SessionKey(<redacted>)")
    }
}

impl SessionKey {
    pub fn generate(random: &dyn Random) -> Result<SessionKey, RandomError> {
        let mut b = [0u8; 32];
        random.fill(&mut b)?;
        Ok(SessionKey(b))
    }

    /// The raw key. Named to stand out in a diff: everything else about this
    /// type exists to keep the bytes out of a log line.
    pub fn reveal(&self) -> &[u8; 32] {
        &self.0
    }
}

/// An offline recovery code: one of the two things that, with the passkey,
/// can end a lockdown.
///
/// Printed once when issued and never recoverable afterwards — only the
/// digest is kept — so there is no path from a stolen session to a usable
/// code.
#[derive(Clone)]
pub struct RecoveryCode([u8; RECOVERY_CODE_LEN]);

impl fmt::Debug for RecoveryCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("RecoveryCode(<redacted>)")
    }
}

impl RecoveryCode {
    pub fn generate(random: &dyn Random) -> Result<RecoveryCode, RandomError> {
        let mut bytes = [0u8; RECOVERY_CODE_LEN];
        random.fill(&mut bytes)?;
        // The alphabet is 32 symbols, a power of two, so taking the low five
        // bits of each byte is unbiased.
        Ok(RecoveryCode(bytes.map(|b| ALPHABET[usize::from(b & 0x1F)])))
    }

    /// The code as `XXXX-XXXX-XXXX-XXXX-XXXX`, for writing down. The only
    /// way to see it, and only at the moment it is issued.
    pub fn reveal(&self) -> String {
        let s = String::from_utf8_lossy(&self.0);
        s.as_bytes()
            .chunks(4)
            .map(|c| String::from_utf8_lossy(c).into_owned())
            .collect::<Vec<_>>()
            .join("-")
    }

    pub fn digest(&self) -> CodeDigest {
        CodeDigest(hex(Sha256::digest(self.0).as_slice()))
    }
}

/// SHA-256 of a recovery code, hex encoded. This is what the journal holds,
/// so reading the journal does not hand anyone an unlock.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodeDigest(String);

impl CodeDigest {
    /// Constant time, so the number of leading bytes that matched is not
    /// observable in the response time of a failed unlock.
    fn matches(&self, other: &CodeDigest) -> bool {
        self.0.len() == other.0.len() && bool::from(self.0.as_bytes().ct_eq(other.0.as_bytes()))
    }
}

/// Strip spaces and dashes, uppercase, map O→0 and I/L→1, and hash. Mirrors
/// `auth::code::normalize` so a code reads the same way whichever prompt it
/// is typed into. Malformed input yields `None`, which the caller treats as
/// a mismatch rather than a distinct error — telling an attacker that their
/// guess was at least well-formed is free information.
fn digest_typed_code(input: &str) -> Option<CodeDigest> {
    if input.len() > MAX_CODE_INPUT {
        return None;
    }
    let mut out = [0u8; RECOVERY_CODE_LEN];
    let mut n = 0;
    for c in input.bytes() {
        let c = match c.to_ascii_uppercase() {
            b' ' | b'-' => continue,
            b'O' => b'0',
            b'I' | b'L' => b'1',
            c => c,
        };
        if !ALPHABET.contains(&c) || n == RECOVERY_CODE_LEN {
            return None;
        }
        out[n] = c;
        n += 1;
    }
    (n == RECOVERY_CODE_LEN).then(|| CodeDigest(hex(Sha256::digest(out).as_slice())))
}

fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        use fmt::Write as _;
        // Infallible for a String; the result is discarded rather than
        // unwrapped so a formatting bug cannot panic the kill switch.
        let _ = write!(s, "{b:02x}");
    }
    s
}

/// A WebAuthn assertion, carried opaquely.
///
/// TODO(passkeys): there is no WebAuthn implementation in the crate yet, so
/// this is a byte string this module never interprets. It is defined here so
/// the unlock rule can be written honestly today; see
/// [`PasskeysUnavailable`].
pub struct PasskeyAssertion(Vec<u8>);

impl fmt::Debug for PasskeyAssertion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("PasskeyAssertion(<redacted>)")
    }
}

impl PasskeyAssertion {
    pub fn new(bytes: Vec<u8>) -> PasskeyAssertion {
        PasskeyAssertion(bytes)
    }

    pub fn reveal(&self) -> &[u8] {
        &self.0
    }
}

// ---- injected dependencies ---------------------------------------------

/// Wall clock, plus the one place lockdown waits: the grace period between
/// `SIGTERM` and `SIGKILL`.
pub trait Clock {
    fn now(&self) -> SystemTime;
    /// Blocking on purpose. A fake implementation records the request and
    /// returns, which is how the kill sequence is tested in milliseconds.
    fn sleep(&self, d: Duration);
}

pub struct OsClock;

impl Clock for OsClock {
    fn now(&self) -> SystemTime {
        SystemTime::now()
    }

    fn sleep(&self, d: Duration) {
        std::thread::sleep(d);
    }
}

pub trait Random {
    fn fill(&self, buf: &mut [u8]) -> Result<(), RandomError>;
}

pub struct OsRandom;

impl Random for OsRandom {
    fn fill(&self, buf: &mut [u8]) -> Result<(), RandomError> {
        os_random(buf)
    }
}

/// Step 1. Implemented over `auth::state::AuthState` (`kick` revokes every
/// session) plus the store, so the persisted rows go too.
pub trait Sessions {
    /// Revoke every session, returning how many there were.
    fn revoke_all(&self) -> usize;
    /// Take a freshly generated signing key.
    fn adopt_signing_key(&self, key: SessionKey);
}

/// What kind of child a pid belongs to. Only for the operator's benefit:
/// every kind is killed, and none is spared.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChildKind {
    Terminal,
    Agent,
    App,
    Sync,
    Other,
}

/// One process Deckjack started and believes is still running.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Child {
    pub pid: u32,
    pub kind: ChildKind,
    /// For the log and the report; never parsed.
    pub label: String,
}

/// Step 2, the half that knows what we started.
pub trait Children {
    /// A snapshot of the live children. Called repeatedly during the grace
    /// period, so it must reflect exits rather than return a cached list.
    fn running(&self) -> Vec<Child>;
}

impl<T: Children + ?Sized> Children for std::sync::Arc<T> {
    fn running(&self) -> Vec<Child> {
        (**self).running()
    }
}

/// Several child tables as one: terminals and streamed apps today, each
/// kept by its own registry.
pub struct AllChildren(pub Vec<Box<dyn Children + Send + Sync>>);

impl Children for AllChildren {
    fn running(&self) -> Vec<Child> {
        self.0.iter().flat_map(|c| c.running()).collect()
    }
}

/// No process supervisor exists yet, so nothing is registered and nothing is
/// killed. Honest rather than convenient: lockdown works today, and step 2
/// reports zero instead of pretending.
///
/// TODO(supervisor): replace with the real child table once terminals,
/// agents, app binaries and sync services are spawned through one place.
pub struct NoChildren;

impl Children for NoChildren {
    fn running(&self) -> Vec<Child> {
        Vec::new()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Signal {
    /// Ask it to exit, so a terminal flushes its scrollback and an agent
    /// checkpoints its session.
    Term,
    /// Take it out.
    Kill,
}

#[derive(Debug, Error)]
pub enum SignalError {
    #[error("refusing to signal pid {pid}: {why}")]
    Refused { pid: u32, why: &'static str },

    #[error("cannot signal pid {pid}: {source}")]
    Failed {
        pid: u32,
        #[source]
        source: io::Error,
    },
}

/// Step 2, the half that does the killing.
pub trait Signals {
    /// Send `signal`. `Ok(false)` means the process was already gone, which
    /// is a success for our purposes.
    fn send(&self, pid: u32, signal: Signal) -> Result<bool, SignalError>;
}

pub struct OsSignals {
    /// Our own pid, so the kill sequence cannot include us.
    own: u32,
}

impl OsSignals {
    pub fn new() -> OsSignals {
        OsSignals {
            own: std::process::id(),
        }
    }
}

impl Default for OsSignals {
    fn default() -> OsSignals {
        OsSignals::new()
    }
}

impl Signals for OsSignals {
    fn send(&self, pid: u32, signal: Signal) -> Result<bool, SignalError> {
        use nix::sys::signal::{Signal as Nix, kill};
        use nix::unistd::Pid;

        // A bug in the child table must not be able to turn the kill switch
        // into a way to kill init, the whole process group, or the server
        // that is trying to lock itself down. These three cases are
        // unrepresentable rather than merely unlikely.
        if pid == 0 {
            return Err(SignalError::Refused {
                pid,
                why: "pid 0 means the entire process group",
            });
        }
        if pid == 1 {
            return Err(SignalError::Refused {
                pid,
                why: "pid 1 is init",
            });
        }
        if pid == self.own {
            return Err(SignalError::Refused {
                pid,
                why: "that is this server; lockdown keeps running to serve the locked page",
            });
        }

        let sig = match signal {
            Signal::Term => Nix::SIGTERM,
            Signal::Kill => Nix::SIGKILL,
        };
        match kill(Pid::from_raw(pid as i32), sig) {
            Ok(()) => Ok(true),
            Err(nix::errno::Errno::ESRCH) => Ok(false),
            Err(e) => Err(SignalError::Failed {
                pid,
                source: io::Error::from_raw_os_error(e as i32),
            }),
        }
    }
}

/// What step 3 held back.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Frozen {
    pub mail_queued: u32,
    pub inbox_pending: u32,
    pub tasks_scheduled: u32,
}

#[derive(Debug, Error)]
#[error("cannot freeze outbound actions: {0}")]
pub struct OutboundError(pub String);

/// Step 3. Freezing holds items in place rather than discarding them: after
/// an incident the queue is evidence, and the design says to preserve
/// evidence before wiping anything.
pub trait Outbound {
    fn freeze(&self) -> Result<Frozen, OutboundError>;
}

/// Nothing sends mail, holds an Inbox or runs scheduled tasks yet.
///
/// TODO(outbound): implement over the mail queue, the Assistant's Inbox and
/// the scheduler when they exist. Reporting zeroes is correct today — there
/// is genuinely nothing queued — and becomes a lie the moment any of those
/// three lands, which is why this type is named for what it does not do.
pub struct NoOutbound;

impl Outbound for NoOutbound {
    fn freeze(&self) -> Result<Frozen, OutboundError> {
        Ok(Frozen::default())
    }
}

/// Half of the unlock rule.
pub trait PasskeyVerifier {
    fn verify(&self, assertion: &PasskeyAssertion) -> Result<(), UnlockError>;
}

/// Refuses every assertion, because there is no WebAuthn implementation to
/// check one against.
///
/// Failing closed is the only safe stub for this particular trait: a
/// verifier that returned `Ok` while unimplemented would reduce the unlock
/// rule to "a recovery code", and a stub that silently weakens a kill switch
/// is worse than no kill switch. With this verifier the usable path is
/// [`Identity::HostConsole`] plus a recovery code — physical access to the
/// machine and a code from a wallet — which is two real factors.
pub struct PasskeysUnavailable;

impl PasskeyVerifier for PasskeysUnavailable {
    fn verify(&self, _assertion: &PasskeyAssertion) -> Result<(), UnlockError> {
        Err(UnlockError::PasskeysUnavailable)
    }
}

// ---- the journal --------------------------------------------------------

/// Everything persisted about lockdown. Small, flat, and written in full
/// each time: there is no partial update worth the risk of a half-applied
/// one in the file that decides whether the machine is locked.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Record {
    pub schema: u32,
    /// `Some` means locked. Cleared by a successful unlock.
    pub engaged: Option<Engagement>,
    /// Incremented on every rotation, so a stale signed cookie can be told
    /// apart from a current one by generation alone.
    pub key_generation: u64,
    /// Digests of the codes still unused.
    pub recovery: Vec<CodeDigest>,
    /// How many have been spent. Kept because "you have three left of ten"
    /// is the number an operator actually needs.
    pub recovery_spent: u32,
}

impl Default for Record {
    fn default() -> Record {
        Record {
            schema: RECORD_SCHEMA,
            engaged: None,
            key_generation: 0,
            recovery: Vec::new(),
            recovery_spent: 0,
        }
    }
}

/// One lockdown, as it is remembered.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Engagement {
    /// Unix seconds.
    pub at: u64,
    pub trigger: TriggerKind,
    /// A sentence for the operator. Never a session id, an address that
    /// identifies a person, or anything else that is a credential: this file
    /// is written during an incident and read afterwards by whoever is
    /// holding the machine.
    pub reason: String,
    pub node_id: String,
    /// How many times lockdown was triggered again while already locked.
    pub reengaged: u32,
    /// `None` until the four steps finish, which is also the signature of a
    /// crash partway through them.
    pub report: Option<Report>,
}

#[derive(Debug, Error)]
pub enum JournalError {
    #[error("cannot read {path}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: io::Error,
    },

    #[error(
        "lockdown journal at {path} is unreadable: {source}; treat the system as locked and \
         unlock with a recovery code once the file is restored or removed by hand"
    )]
    Corrupt {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },

    #[error(
        "lockdown journal at {path} was written by a newer Deckjack (schema {found}, this build \
         understands {supported}); upgrade rather than guess at it"
    )]
    FromTheFuture {
        path: PathBuf,
        found: u32,
        supported: u32,
    },

    #[error("cannot write {path}: {source}")]
    Write {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
}

pub trait Journal {
    /// `Ok(None)` means there has never been a lockdown here. An `Err` means
    /// we do not know, which is not the same thing — see
    /// [`Lockdown::restore`].
    fn load(&self) -> Result<Option<Record>, JournalError>;
    fn save(&self, record: &Record) -> Result<(), JournalError>;
}

/// The real journal: one JSON file in the hidden directory, replaced
/// atomically.
pub struct FileJournal {
    path: PathBuf,
}

impl FileJournal {
    pub fn new(working_dir: &Path) -> FileJournal {
        FileJournal {
            path: working_dir.join(JOURNAL_PATH),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Journal for FileJournal {
    fn load(&self) -> Result<Option<Record>, JournalError> {
        let text = match fs::read_to_string(&self.path) {
            Ok(t) => t,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(source) => {
                return Err(JournalError::Read {
                    path: self.path.clone(),
                    source,
                });
            }
        };
        let record: Record =
            serde_json::from_str(&text).map_err(|source| JournalError::Corrupt {
                path: self.path.clone(),
                source,
            })?;
        if record.schema > RECORD_SCHEMA {
            return Err(JournalError::FromTheFuture {
                path: self.path.clone(),
                found: record.schema,
                supported: RECORD_SCHEMA,
            });
        }
        Ok(Some(record))
    }

    fn save(&self, record: &Record) -> Result<(), JournalError> {
        let write = |source| JournalError::Write {
            path: self.path.clone(),
            source,
        };
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent).map_err(write)?;
        }
        let dir = self.path.parent().unwrap_or(Path::new("."));

        // Temp file in the same directory, then rename over the target, as
        // the lock file does: a crash mid-write must leave the previous
        // record rather than a truncated one that reads as corrupt and so
        // locks the machine for a reason nobody can explain.
        let mut tmp = tempfile::NamedTempFile::new_in(dir).map_err(write)?;
        serde_json::to_writer_pretty(&mut tmp, record)
            .map_err(|e| write(io::Error::other(e.to_string())))?;
        use std::io::Write as _;
        tmp.as_file_mut().write_all(b"\n").map_err(write)?;

        // The digests are not secrets, but the file says when the machine
        // was locked and why, which is nobody else's business on a shared
        // host.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            tmp.as_file()
                .set_permissions(fs::Permissions::from_mode(0o600))
                .map_err(write)?;
        }

        // Durability matters more here than anywhere else in the system: the
        // whole design assumes a restart cannot clear the locked state.
        tmp.as_file().sync_all().map_err(write)?;
        tmp.persist(&self.path).map_err(|e| write(e.error))?;
        Ok(())
    }
}

/// A journal that forgets on exit.
///
/// Only for bring-up and for tests: a restart clears the locked state, which
/// is precisely the attack lockdown exists to stop. Never hand this to a
/// running server.
pub struct MemoryJournal {
    record: Mutex<Option<Record>>,
}

impl MemoryJournal {
    pub fn new() -> MemoryJournal {
        MemoryJournal {
            record: Mutex::new(None),
        }
    }
}

impl Default for MemoryJournal {
    fn default() -> MemoryJournal {
        MemoryJournal::new()
    }
}

impl Journal for MemoryJournal {
    fn load(&self) -> Result<Option<Record>, JournalError> {
        Ok(self
            .record
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone())
    }

    fn save(&self, record: &Record) -> Result<(), JournalError> {
        *self.record.lock().unwrap_or_else(PoisonError::into_inner) = Some(record.clone());
        Ok(())
    }
}

// ---- triggers -----------------------------------------------------------

/// Where an operator clicked. Recorded because "I hit the hotkey by
/// accident" and "the phone link was opened" are different incidents.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Surface {
    Menu,
    Palette,
    Hotkey,
    /// The separate lockdown link, which is a bare URL precisely so it works
    /// from a phone's lock screen without signing in first.
    Link,
}

/// The persisted shape of a trigger: a tag, nothing more. [`Trigger`] itself
/// carries addresses and remote source names that have no business in a file
/// that outlives the incident; see [`Trigger::reason_for_journal`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TriggerKind {
    Operator,
    HostCommand,
    RemoteCommand,
    NewDevice,
    FailedPasskeys,
    TetherTerminal,
    RefusedAgentCommands,
}

impl TriggerKind {
    pub fn label(self) -> &'static str {
        match self {
            TriggerKind::Operator => "triggered from a signed-in session",
            TriggerKind::HostCommand => "triggered by deckjack lockdown on the host",
            TriggerKind::RemoteCommand => "triggered by a signed remote command",
            TriggerKind::NewDevice => "a sign-in from a device that is not known",
            TriggerKind::FailedPasskeys => "repeated failed passkey attempts",
            TriggerKind::TetherTerminal => "a terminal opened from a tether session",
            TriggerKind::RefusedAgentCommands => "a burst of refused agent commands",
        }
    }

    /// The last four are automatic; the first three are somebody deciding.
    pub fn is_automatic(self) -> bool {
        matches!(
            self,
            TriggerKind::NewDevice
                | TriggerKind::FailedPasskeys
                | TriggerKind::TetherTerminal
                | TriggerKind::RefusedAgentCommands
        )
    }
}

/// Why lockdown engaged, with the detail the log wants.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Trigger {
    /// One click in the menu, the palette, a hotkey or the lockdown link.
    /// Which session is deliberately not recorded: a session id is a bearer
    /// credential and this one goes in a file.
    Operator {
        surface: Surface,
    },
    /// `deckjack lockdown`, over the control socket.
    HostCommand,
    /// A signed ntfy or Matrix command from the phone.
    RemoteCommand {
        source: String,
    },
    NewDevice {
        ip: IpAddr,
    },
    FailedPasskeys {
        count: u32,
        within: Duration,
    },
    TetherTerminal,
    RefusedAgentCommands {
        count: u32,
        within: Duration,
    },
}

impl Trigger {
    pub fn kind(&self) -> TriggerKind {
        match self {
            Trigger::Operator { .. } => TriggerKind::Operator,
            Trigger::HostCommand => TriggerKind::HostCommand,
            Trigger::RemoteCommand { .. } => TriggerKind::RemoteCommand,
            Trigger::NewDevice { .. } => TriggerKind::NewDevice,
            Trigger::FailedPasskeys { .. } => TriggerKind::FailedPasskeys,
            Trigger::TetherTerminal => TriggerKind::TetherTerminal,
            Trigger::RefusedAgentCommands { .. } => TriggerKind::RefusedAgentCommands,
        }
    }

    /// A sentence for the log, with the full detail: the audit log ships to
    /// the VPS as written, and that is where incident evidence belongs. What
    /// the journal keeps is [`Self::reason_for_journal`].
    pub fn describe(&self) -> String {
        match self {
            Trigger::Operator { surface } => {
                format!("{} ({surface:?})", TriggerKind::Operator.label())
            }
            Trigger::HostCommand => TriggerKind::HostCommand.label().to_string(),
            Trigger::RemoteCommand { source } => {
                format!("{} via {source}", TriggerKind::RemoteCommand.label())
            }
            Trigger::NewDevice { ip } => {
                format!("{} ({ip})", TriggerKind::NewDevice.label())
            }
            Trigger::FailedPasskeys { count, within } => format!(
                "{count} failed passkey attempts within {}s",
                within.as_secs()
            ),
            Trigger::TetherTerminal => TriggerKind::TetherTerminal.label().to_string(),
            Trigger::RefusedAgentCommands { count, within } => format!(
                "{count} refused agent commands within {}s",
                within.as_secs()
            ),
        }
    }

    /// The sentence the journal keeps, which is not the same sentence the log
    /// gets.
    ///
    /// Counts stay: "5 failed passkey attempts within 300s" is what makes an
    /// entry worth reading afterwards, and a count identifies nobody.
    /// Addresses and remote source names do not stay, because
    /// [`Engagement::reason`] is written during an incident into a file that
    /// outlives it and is read by whoever is then holding the machine — and
    /// the most common cause of a `NewDevice` trigger is the operator's own
    /// new laptop. The address is in the log line and in the audit trail; it
    /// does not also need to be on disk here.
    pub fn reason_for_journal(&self) -> String {
        match self {
            Trigger::NewDevice { .. } => TriggerKind::NewDevice.label().to_string(),
            Trigger::RemoteCommand { .. } => TriggerKind::RemoteCommand.label().to_string(),
            other => other.describe(),
        }
    }
}

// ---- policy -------------------------------------------------------------

/// What an automatic trigger should do. The design offers both: lock down,
/// or raise an alert asking me to confirm. Which one is right depends on how
/// much a false positive costs, so it is a setting, not a decision baked in
/// here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Response {
    /// Watch and record, do nothing.
    Ignore,
    /// Ask me. The caller decides how: a notification, a banner, ntfy.
    Alert,
    /// Lock down now.
    Engage,
}

/// Thresholds for the counting triggers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Burst {
    pub count: u32,
    pub within: Duration,
    pub response: Response,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AutoPolicy {
    /// A sign-in from a device that is not known. Alert by default: a new
    /// device is also what happens when I buy a laptop, and a sign-in
    /// already needed approval from an existing session.
    pub new_device: Response,
    /// Repeated failed passkey attempts. Engage: nothing legitimate fails a
    /// passkey five times, and the attempts themselves prove someone is
    /// trying.
    pub failed_passkeys: Burst,
    /// A terminal opened from a tether session. Engage: tether mode has no
    /// terminal by design, so a terminal from one means the scope check was
    /// bypassed.
    ///
    /// Inert today, and left at `Engage` rather than softened: nothing
    /// produces [`Observation::TetherTerminal`], so this value decides
    /// nothing. Note that the reasoning above assumes the scope check
    /// exists. It does not — `wiring::Open::run` consults neither the
    /// session's kind nor its step-up state — so `Engage` becomes the right
    /// default only once a refused tether terminal is what reaches here.
    pub tether_terminal: Response,
    /// A burst of refused agent commands. Alert by default: a confused agent
    /// retrying a command it has no grant for is common, and the refusals
    /// mean the gate is already working.
    pub refused_agent_commands: Burst,
}

impl Default for AutoPolicy {
    fn default() -> AutoPolicy {
        AutoPolicy {
            new_device: Response::Alert,
            failed_passkeys: Burst {
                count: 5,
                within: Duration::from_secs(300),
                response: Response::Engage,
            },
            tether_terminal: Response::Engage,
            refused_agent_commands: Burst {
                count: 10,
                within: Duration::from_secs(60),
                response: Response::Alert,
            },
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Policy {
    /// How long children get between `SIGTERM` and `SIGKILL`. Short: a
    /// terminal flushing scrollback or an agent checkpointing takes
    /// milliseconds, and anything slower is not cooperating.
    pub grace: Duration,
    /// How many times to check for exits during the grace period. More
    /// checks mean the common case (everything exits at once) finishes
    /// sooner.
    pub grace_checks: u32,
    /// Whether being on the host console may stand in for the passkey.
    ///
    /// True while WebAuthn is unimplemented, because otherwise a lockdown
    /// could never be undone. It does not weaken the threat model this
    /// module defends: whoever is at the host console already has a shell as
    /// my user, which lockdown never claimed to stop. The second factor is
    /// still required, and when the identity is the host console that second
    /// factor must be a recovery code rather than the host command — see
    /// [`Lockdown::unlock`].
    ///
    /// TODO(passkeys): flip the default to false once a real verifier exists.
    pub allow_host_console_identity: bool,
    pub auto: AutoPolicy,
}

impl Default for Policy {
    fn default() -> Policy {
        Policy {
            grace: Duration::from_secs(2),
            grace_checks: 8,
            allow_host_console_identity: true,
            auto: AutoPolicy::default(),
        }
    }
}

// ---- reports and status -------------------------------------------------

/// What the four steps actually managed to do.
///
/// Every field can be disappointing without the lockdown having failed.
/// Nothing in [`Lockdown::engage`] returns `Err`, because a kill switch that
/// stops at the first problem is a kill switch that an attacker can stop by
/// causing one problem.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Report {
    pub sessions_revoked: usize,
    pub key_rotated: bool,
    pub children_seen: usize,
    pub terminated: usize,
    pub killed: usize,
    /// Pids still alive after `SIGKILL`: uninterruptible sleep, a zombie
    /// whose parent is not us, or a pid we are not allowed to signal. The
    /// operator needs to know, because these are the processes still holding
    /// whatever they held.
    pub survived: Vec<u32>,
    pub frozen: Option<Frozen>,
    /// One line per thing that went wrong, in the order it went wrong.
    pub errors: Vec<String>,
}

/// Lockdown as something to render or serialize.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Status {
    pub locked: bool,
    pub node_id: String,
    pub key_generation: u64,
    pub recovery_codes_left: usize,
    pub recovery_codes_spent: u32,
    pub engaged: Option<Engagement>,
}

// ---- errors -------------------------------------------------------------

#[derive(Debug, Error)]
pub enum StartupError {
    #[error(
        "the lockdown journal cannot be read, so this node serves nothing but the locked \
         page; the recovery digests were in that file, so a code cannot unlock it from here: \
         restore {path}, or move it aside by hand on the host: {source}"
    )]
    JournalUnreadable {
        path: PathBuf,
        #[source]
        source: JournalError,
    },
}

#[derive(Debug, Error)]
pub enum UnlockError {
    #[error("not locked down; nothing to unlock")]
    NotLocked,

    #[error(
        "passkey verification is not implemented in this build; unlock from the host console \
         with a recovery code (deckjack unlock --recovery-code)"
    )]
    PasskeysUnavailable,

    #[error("the passkey was not accepted")]
    BadPasskey,

    #[error(
        "the host console cannot stand in for the passkey on this node; \
         set allow_host_console_identity if that is what you want"
    )]
    HostConsoleNotAllowed,

    #[error(
        "the host console plus the host command is one factor, not two; \
         add a recovery code, or present the passkey"
    )]
    OneFactor,

    #[error("that recovery code is not valid")]
    BadRecoveryCode,

    #[error("there are no recovery codes left; issue a new set from the host once unlocked")]
    NoRecoveryCodes,

    #[error(
        "unlocked in memory but the journal could not be written, so a restart would come back \
         locked; fix the working directory and unlock again: {source}"
    )]
    JournalWrite {
        #[source]
        source: JournalError,
    },
}

#[derive(Debug, Error)]
pub enum RecoveryError {
    #[error(
        "recovery codes cannot be issued while locked down, because issuing them is an unlock \
         path; unlock first"
    )]
    Locked,

    #[error("the OS random source failed: {0}")]
    Random(#[from] RandomError),

    #[error("the new codes were not written, so the old ones are still the live set: {source}")]
    Journal {
        #[source]
        source: JournalError,
    },
}

// ---- unlocking ----------------------------------------------------------

/// Who is asking.
pub enum Identity {
    Passkey(PasskeyAssertion),
    /// The unlock arrived over the local control socket, from a shell on the
    /// host. See [`Policy::allow_host_console_identity`].
    HostConsole,
}

impl fmt::Debug for Identity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Identity::Passkey(_) => f.write_str("Identity::Passkey(<redacted>)"),
            Identity::HostConsole => f.write_str("Identity::HostConsole"),
        }
    }
}

/// The second thing the design requires, on top of the passkey.
pub enum SecondFactor {
    /// `deckjack unlock`, run on the host.
    HostCommand,
    /// One of the offline codes. Spent only when the unlock succeeds, so a
    /// rejected passkey does not burn a code.
    RecoveryCode(String),
}

impl fmt::Debug for SecondFactor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SecondFactor::HostCommand => f.write_str("SecondFactor::HostCommand"),
            SecondFactor::RecoveryCode(_) => f.write_str("SecondFactor::RecoveryCode(<redacted>)"),
        }
    }
}

#[derive(Debug)]
pub struct UnlockRequest {
    pub identity: Identity,
    pub second: SecondFactor,
}

// ---- the kill switch ----------------------------------------------------

/// Everything lockdown is allowed to touch. Assembled once at startup.
pub struct Deps {
    pub clock: Box<dyn Clock + Send + Sync>,
    pub random: Box<dyn Random + Send + Sync>,
    pub sessions: Box<dyn Sessions + Send + Sync>,
    pub children: Box<dyn Children + Send + Sync>,
    pub signals: Box<dyn Signals + Send + Sync>,
    pub outbound: Box<dyn Outbound + Send + Sync>,
    pub passkeys: Box<dyn PasskeyVerifier + Send + Sync>,
    pub journal: Box<dyn Journal + Send + Sync>,
}

pub struct Lockdown {
    deps: Deps,
    policy: Policy,
    node_id: String,
    /// Read on every request by the gate, and set before any of the slow
    /// steps run. It is separate from the mutex below so that a request
    /// arriving while children are being killed is refused immediately
    /// rather than waiting on a lock held across the grace period.
    locked: AtomicBool,
    record: Mutex<Record>,
}

impl Lockdown {
    /// Read the journal and come back in whatever state it describes.
    ///
    /// An unreadable or future journal refuses the start, and the caller
    /// serves [`Self::sealed`] rather than coming up open: an attacker who
    /// can scribble over one small file must not thereby unlock the machine.
    /// Failing closed has a cost worth stating plainly, though, because the
    /// recovery digests lived in that same file: no recovery code can unlock
    /// a sealed node. The way out is on the host — restore the file, or move
    /// it aside by hand. See [`StartupError`].
    pub fn restore(deps: Deps, policy: Policy, node_id: String) -> Result<Lockdown, StartupError> {
        match deps.journal.load() {
            Ok(Some(record)) => {
                let locked = record.engaged.is_some();
                if locked {
                    tracing::warn!(
                        node = %node_id,
                        "starting locked down: the journal says so, and a restart is exactly \
                         what this is here to survive"
                    );
                }
                Ok(Lockdown {
                    deps,
                    policy,
                    node_id,
                    locked: AtomicBool::new(locked),
                    record: Mutex::new(record),
                })
            }
            Ok(None) => Ok(Lockdown {
                deps,
                policy,
                node_id,
                locked: AtomicBool::new(false),
                record: Mutex::new(Record::default()),
            }),
            Err(source) => Err(StartupError::JournalUnreadable {
                path: PathBuf::from(JOURNAL_PATH),
                source,
            }),
        }
    }

    /// The state [`Self::restore`] describes but cannot return: locked, with
    /// no engagement to show.
    ///
    /// A caller holding a [`StartupError`] has two other options and both are
    /// worse. Aborting serves nothing, so a phone on the far side of the
    /// tailnet learns only that the connection failed. Coming up open is the
    /// hole the journal exists to close. A sealed node serves the locked
    /// page, says on it that the journal could not be read, and cannot be
    /// unlocked by a recovery code, because there are no digests left to
    /// match — [`Self::unlock`] refuses with
    /// [`UnlockError::NoRecoveryCodes`]. A passkey plus the host command
    /// still unlocks it, and that unlock replaces the unreadable file.
    ///
    /// It is a separate constructor so that nothing reaches this state by
    /// accident: every other path into `Lockdown` goes through the journal.
    pub fn sealed(deps: Deps, policy: Policy, node_id: String) -> Lockdown {
        tracing::error!(
            node = %node_id,
            "sealed: the lockdown journal could not be read, so this node serves only the \
             locked page"
        );
        Lockdown {
            deps,
            policy,
            node_id,
            locked: AtomicBool::new(true),
            // Deliberately no `Engagement`: inventing one would put a cause
            // in the journal that no trigger ever produced. `locked_page`
            // has a branch for exactly this state.
            record: Mutex::new(Record::default()),
        }
    }

    /// Transitions are short and leave the record consistent even if a holder
    /// panicked, so a poisoned lock is still usable — as in `http::Ctx`.
    fn record(&self) -> MutexGuard<'_, Record> {
        self.record.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub fn is_locked(&self) -> bool {
        self.locked.load(Ordering::SeqCst)
    }

    pub fn policy(&self) -> &Policy {
        &self.policy
    }

    pub fn status(&self) -> Status {
        let r = self.record();
        Status {
            locked: self.is_locked(),
            node_id: self.node_id.clone(),
            key_generation: r.key_generation,
            recovery_codes_left: r.recovery.len(),
            recovery_codes_spent: r.recovery_spent,
            engaged: r.engaged.clone(),
        }
    }

    /// Lock everything down. Never fails: see [`Report`].
    ///
    /// **Blocks** for up to [`Policy::grace`] while children exit, so an
    /// async caller runs this on `tokio::task::spawn_blocking`. It is not
    /// `async` because nothing here should be cancellable — a dropped future
    /// halfway through step 2 would leave the workspace locked with an agent
    /// still running.
    pub fn engage(&self, trigger: Trigger) -> Report {
        // Step 4 first. Flipping the gate is the one action that cannot
        // fail, it takes effect immediately, and it is what stops new
        // requests from starting work while steps 1 to 3 undo the old work.
        // The design lists it last because it is what the user sees last.
        let was_locked = self.locked.swap(true, Ordering::SeqCst);

        let kind = trigger.kind();
        // Two sentences on purpose: the log line carries the address, the
        // journal does not. See [`Trigger::reason_for_journal`].
        let detail = trigger.describe();
        let reason = trigger.reason_for_journal();
        tracing::warn!(
            trigger = ?kind,
            reason = %detail,
            already_locked = was_locked,
            "LOCKDOWN"
        );

        // Then the journal, before any of the work: if the process dies
        // during step 2, the next start must still come back locked. The
        // record is written again at the end with the report, so an
        // engagement with `report: None` on disk is the fingerprint of a
        // crash partway through.
        {
            let mut r = self.record();
            match r.engaged.as_mut() {
                // Already locked. Keep the original cause — the first
                // trigger is the interesting one — and just count this.
                Some(existing) => existing.reengaged = existing.reengaged.saturating_add(1),
                None => {
                    r.engaged = Some(Engagement {
                        at: unix(self.deps.clock.now()),
                        trigger: kind,
                        reason,
                        node_id: self.node_id.clone(),
                        reengaged: 0,
                        report: None,
                    });
                }
            }
            let snapshot = r.clone();
            drop(r);
            if let Err(e) = self.deps.journal.save(&snapshot) {
                // Nothing to do but say so loudly and carry on: an
                // in-memory lockdown that a restart would clear is still
                // better than no lockdown.
                tracing::error!(
                    error = %e,
                    "lockdown state could not be persisted; a restart would clear it"
                );
            }
        }

        // Step 1: revoke every session, rotate the signing key.
        let mut report = Report {
            sessions_revoked: self.deps.sessions.revoke_all(),
            ..Report::default()
        };
        match SessionKey::generate(self.deps.random.as_ref()) {
            Ok(key) => {
                self.deps.sessions.adopt_signing_key(key);
                report.key_rotated = true;
                let mut r = self.record();
                r.key_generation = r.key_generation.saturating_add(1);
            }
            Err(e) => {
                // The sessions are already gone, which is the part that
                // matters; a key that could not be rotated is recorded and
                // the sequence continues.
                report
                    .errors
                    .push(format!("session signing key not rotated: {e}"));
            }
        }

        // Step 2: kill the children.
        self.reap(&mut report);

        // Step 3: freeze outbound actions.
        match self.deps.outbound.freeze() {
            Ok(frozen) => report.frozen = Some(frozen),
            Err(e) => report.errors.push(e.to_string()),
        }

        // Record what happened, so the locked page and the audit trail can
        // say more than "locked".
        {
            let mut r = self.record();
            if let Some(engaged) = r.engaged.as_mut() {
                engaged.report = Some(report.clone());
            }
            let snapshot = r.clone();
            drop(r);
            if let Err(e) = self.deps.journal.save(&snapshot) {
                tracing::error!(error = %e, "lockdown report not persisted");
            }
        }

        tracing::warn!(?report, "lockdown complete");
        report
    }

    /// `SIGTERM` everything, wait out the grace period checking for exits,
    /// then `SIGKILL` whatever is left.
    fn reap(&self, report: &mut Report) {
        let children = self.deps.children.running();
        report.children_seen = children.len();
        if children.is_empty() {
            return;
        }

        for child in &children {
            match self.deps.signals.send(child.pid, Signal::Term) {
                Ok(true) => {
                    report.terminated += 1;
                    tracing::info!(pid = child.pid, kind = ?child.kind, label = %child.label, "SIGTERM");
                }
                // Already gone. Nothing to report: the goal was for it not
                // to be running.
                Ok(false) => {}
                Err(e) => report.errors.push(e.to_string()),
            }
        }

        // Poll rather than sleep the whole grace period: the common case is
        // that everything exits at once, and a kill switch should not stand
        // around for two seconds to confirm it.
        let checks = self.policy.grace_checks.max(1);
        let step = self.policy.grace / checks;
        let mut left = self.deps.children.running();
        for _ in 0..checks {
            if left.is_empty() {
                return;
            }
            self.deps.clock.sleep(step);
            left = self.deps.children.running();
        }

        for child in &left {
            match self.deps.signals.send(child.pid, Signal::Kill) {
                Ok(true) => {
                    report.killed += 1;
                    tracing::warn!(pid = child.pid, label = %child.label, "SIGKILL after grace");
                }
                Ok(false) => {}
                Err(e) => report.errors.push(e.to_string()),
            }
        }

        // One last look. A process still here after SIGKILL is stuck in the
        // kernel or beyond our permissions, and the operator has to be told
        // rather than left to assume the sweep was clean.
        self.deps.clock.sleep(step);
        report.survived = self
            .deps
            .children
            .running()
            .into_iter()
            .map(|c| c.pid)
            .collect();
        if !report.survived.is_empty() {
            tracing::error!(pids = ?report.survived, "processes survived SIGKILL");
        }
    }

    /// End a lockdown. The passkey **plus** a second factor, always.
    ///
    /// The two factors must be independent, which is the one rule that is
    /// not obvious from the design's sentence: the host console standing in
    /// for the passkey ([`Policy::allow_host_console_identity`]) and the
    /// host command as the second factor are the same factor twice, so that
    /// combination is refused. Physical access plus an offline code, or a
    /// passkey plus either, is two.
    pub fn unlock(&self, request: UnlockRequest) -> Result<(), UnlockError> {
        if !self.is_locked() {
            return Err(UnlockError::NotLocked);
        }

        match &request.identity {
            Identity::Passkey(assertion) => self.deps.passkeys.verify(assertion)?,
            Identity::HostConsole => {
                if !self.policy.allow_host_console_identity {
                    return Err(UnlockError::HostConsoleNotAllowed);
                }
                if matches!(request.second, SecondFactor::HostCommand) {
                    return Err(UnlockError::OneFactor);
                }
            }
        }

        // The code is checked, and spent, only after the identity passes, so
        // guessing codes against a lockdown is not a thing that can be done
        // at all without first getting past the passkey.
        let mut r = self.record();
        if let SecondFactor::RecoveryCode(typed) = &request.second {
            if r.recovery.is_empty() {
                return Err(UnlockError::NoRecoveryCodes);
            }
            let Some(digest) = digest_typed_code(typed) else {
                return Err(UnlockError::BadRecoveryCode);
            };
            let Some(at) = r.recovery.iter().position(|d| d.matches(&digest)) else {
                return Err(UnlockError::BadRecoveryCode);
            };
            r.recovery.remove(at);
            r.recovery_spent = r.recovery_spent.saturating_add(1);
        }

        let was = r.engaged.take();
        let snapshot = r.clone();
        drop(r);

        // Persist first, flip the gate second. If the write fails the node
        // stays locked, which is the honest outcome: a process that believes
        // it is unlocked while the disk says otherwise would unlock itself
        // and then re-lock on the next restart, with nobody able to say why.
        if let Err(source) = self.deps.journal.save(&snapshot) {
            let mut r = self.record();
            r.engaged = was;
            // The spent code is not put back. It was revealed to whoever
            // typed it, and reusing it later would mean a code that unlocks
            // twice.
            return Err(UnlockError::JournalWrite { source });
        }

        self.locked.store(false, Ordering::SeqCst);
        tracing::warn!(node = %self.node_id, "lockdown lifted");
        Ok(())
    }

    /// Issue a fresh set of offline recovery codes, replacing any that
    /// exist. The plaintext is returned once; only digests are kept.
    ///
    /// Refused while locked down, because minting a code is an unlock path
    /// and must not be reachable from the locked state.
    pub fn issue_recovery_codes(&self, count: usize) -> Result<Vec<RecoveryCode>, RecoveryError> {
        if self.is_locked() {
            return Err(RecoveryError::Locked);
        }

        let mut codes = Vec::with_capacity(count);
        for _ in 0..count {
            codes.push(RecoveryCode::generate(self.deps.random.as_ref())?);
        }

        let mut r = self.record();
        let previous = std::mem::replace(
            &mut r.recovery,
            codes.iter().map(RecoveryCode::digest).collect(),
        );
        r.recovery_spent = 0;
        let snapshot = r.clone();
        drop(r);

        if let Err(source) = self.deps.journal.save(&snapshot) {
            let mut r = self.record();
            r.recovery = previous;
            return Err(RecoveryError::Journal { source });
        }

        tracing::info!(
            count,
            "recovery codes issued; the previous set no longer works"
        );
        Ok(codes)
    }

    pub fn recovery_codes_left(&self) -> usize {
        self.record().recovery.len()
    }
}

/// Time as the journal stores it. Before the epoch is not a time we expect;
/// clamping beats panicking in the one code path that must not panic.
fn unix(t: SystemTime) -> u64 {
    t.duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

// ---- automatic triggers -------------------------------------------------

/// Something worth watching, as the rest of the system sees it.
///
/// `SignIn` carries whether the device is already known rather than a device
/// id, because there is no device registry yet and inventing one here would
/// be a second source of truth for the thing sign-in already decides.
#[derive(Debug, Clone)]
pub enum Observation {
    SignIn {
        ip: IpAddr,
        known_device: bool,
    },
    FailedPasskey {
        ip: IpAddr,
    },
    /// A terminal was opened from a tether session, which tether mode's own
    /// scope rules should already have refused.
    ///
    /// **Produced by nobody, and not for want of a call site.** The only
    /// place a terminal is opened is `wiring::Open::run`, which holds the
    /// browser session and could read `sessions.kind` through the store. What
    /// is missing is the fact, not the wiring.
    ///
    /// `sessions.kind` records which *credential* proved a session —
    /// `'passkey'`, or `'tether'` by migration 7's DEFAULT — and the only
    /// writer of `'passkey'` is `passkeyhttp::PasskeyCtx::admit`. So every
    /// one-time-code sign-in reads as `'tether'`, including the operator's
    /// own machine on the tailnet, which docs/DESIGN.md ("Scope by session
    /// type") gives full terminal access. Reading that column as "this is a
    /// tether session" and acting on [`AutoPolicy::tether_terminal`]'s
    /// `Engage` would lock the node down the first time the operator opened a
    /// shell.
    ///
    /// Tether mode as the design means it — served over the cable, dying with
    /// it — is not implemented, so the fact this observation needs does not
    /// exist. It wants its own marker, written only by that sign-in path. The
    /// credential axis cannot stand in for the mode axis, any more than risk
    /// level can stand in for a grant.
    ///
    /// Note that `wiring::Open::run`'s own ownership check is a third axis
    /// again: it answers *which caller* a pty belongs to, not which mode or
    /// credential opened it.
    TetherTerminal,
    /// An agent command was refused by the risk gate.
    RefusedAgentCommand {
        command: String,
    },
}

/// What the watchdog decided.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    Nothing,
    /// Ask the operator to confirm. The caller sends the notification; the
    /// trigger is carried so that confirming can call
    /// [`Lockdown::engage`] with the real cause rather than a generic one.
    Alert(Trigger),
    Engage(Trigger),
}

/// The automatic triggers: a sign-in from a new device, repeated failed
/// passkey attempts, a terminal opened from a tether session, and a burst of
/// refused agent commands.
///
/// Counting happens here rather than in the auth or agent modules so that
/// the thresholds live next to the response, and so that neither of those
/// modules needs to know the kill switch exists.
pub struct Watchdog {
    policy: AutoPolicy,
    failed_passkeys: Mutex<VecDeque<SystemTime>>,
    refused_commands: Mutex<VecDeque<SystemTime>>,
}

impl Watchdog {
    pub fn new(policy: AutoPolicy) -> Watchdog {
        Watchdog {
            policy,
            failed_passkeys: Mutex::new(VecDeque::new()),
            refused_commands: Mutex::new(VecDeque::new()),
        }
    }

    pub fn policy(&self) -> &AutoPolicy {
        &self.policy
    }

    /// Record an observation and say what should happen.
    ///
    /// Takes `now` rather than reading a clock, so the burst windows are
    /// exercised without waiting them out.
    pub fn observe(&self, observation: Observation, now: SystemTime) -> Verdict {
        match observation {
            Observation::SignIn { ip, known_device } => {
                if known_device {
                    return Verdict::Nothing;
                }
                decide(self.policy.new_device, Trigger::NewDevice { ip })
            }

            Observation::FailedPasskey { ip } => {
                let burst = self.policy.failed_passkeys;
                let count = self.count(&self.failed_passkeys, now, burst.within);
                if count < burst.count {
                    tracing::info!(%ip, count, threshold = burst.count, "failed passkey");
                    return Verdict::Nothing;
                }
                // Counted attempts are cleared once the threshold fires, so
                // one burst produces one verdict instead of one per further
                // attempt while the operator is deciding.
                self.clear(&self.failed_passkeys);
                decide(
                    burst.response,
                    Trigger::FailedPasskeys {
                        count,
                        within: burst.within,
                    },
                )
            }

            Observation::TetherTerminal => {
                decide(self.policy.tether_terminal, Trigger::TetherTerminal)
            }

            Observation::RefusedAgentCommand { command } => {
                let burst = self.policy.refused_agent_commands;
                let count = self.count(&self.refused_commands, now, burst.within);
                if count < burst.count {
                    tracing::info!(%command, count, threshold = burst.count, "agent command refused");
                    return Verdict::Nothing;
                }
                self.clear(&self.refused_commands);
                decide(
                    burst.response,
                    Trigger::RefusedAgentCommands {
                        count,
                        within: burst.within,
                    },
                )
            }
        }
    }

    /// Add `now` to a window and return how many events it holds. Events
    /// older than the window are dropped from the front, so the queue cannot
    /// grow without bound on a long-running node.
    fn count(
        &self,
        window: &Mutex<VecDeque<SystemTime>>,
        now: SystemTime,
        within: Duration,
    ) -> u32 {
        let mut q = window.lock().unwrap_or_else(PoisonError::into_inner);
        q.push_back(now);
        let cutoff = now.checked_sub(within).unwrap_or(UNIX_EPOCH);
        while q.front().is_some_and(|t| *t < cutoff) {
            q.pop_front();
        }
        // A window longer than u32 of events is not a real scenario, and
        // saturating here is better than a cast that wraps to a small number
        // and so silently stops triggering.
        u32::try_from(q.len()).unwrap_or(u32::MAX)
    }

    fn clear(&self, window: &Mutex<VecDeque<SystemTime>>) {
        window
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clear();
    }
}

fn decide(response: Response, trigger: Trigger) -> Verdict {
    match response {
        Response::Ignore => Verdict::Nothing,
        Response::Alert => Verdict::Alert(trigger),
        Response::Engage => Verdict::Engage(trigger),
    }
}

// ---- step 4: the locked page -------------------------------------------

/// Paths that keep working while locked down.
///
/// Deliberately tiny. `/unlock` is the way out, and `/healthz` stays up so
/// the VPS keeps routing to this node: if health checks failed here, Caddy
/// would fail over to another node and the operator would find themselves
/// talking to an unlocked machine instead of seeing the locked page.
pub const ALLOWED_WHILE_LOCKED: &[&str] = &["/unlock", "/healthz"];

pub fn allowed_while_locked(path: &str) -> bool {
    ALLOWED_WHILE_LOCKED
        .iter()
        .any(|p| path == *p || path.starts_with(&format!("{p}/")))
}

/// Minimal escaping for the few values interpolated below. `auth::html` can
/// promise that nothing user-supplied is ever interpolated; this page shows
/// a node name and a trigger description, so it cannot.
fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
    out
}

const LOCKED_STYLE: &str = "\
*{box-sizing:border-box}\
body{margin:0;min-height:100vh;display:flex;align-items:center;justify-content:center;\
font:16px/1.5 system-ui,sans-serif;background:#1b1210;color:#f5e9e6}\
main{width:min(30rem,100% - 2rem);padding:2rem;background:#2a1a17;border-radius:12px;\
border:1px solid #5a2b22}\
h1{margin:0 0 1rem;font-size:1.25rem}\
p{margin:0 0 1rem}\
dl{margin:0;display:grid;grid-template-columns:auto 1fr;gap:.25rem 1rem;font-size:.9rem}\
dt{color:#c9a89f}\
dd{margin:0}\
code{font-family:ui-monospace,monospace}";

/// The only page a locked-down node serves.
///
/// No JavaScript, no external assets, no form. Unlocking is not something
/// that happens in this page: it needs the passkey plus the host or an
/// offline code, so the page's job is to say what happened and what to do.
pub fn locked_page(status: &Status) -> String {
    let detail = match &status.engaged {
        Some(e) => format!(
            "<dl><dt>Since</dt><dd>{} (unix)</dd>\
<dt>Trigger</dt><dd>{}</dd><dt>Node</dt><dd><code>{}</code></dd>\
<dt>Recovery codes</dt><dd>{} left</dd></dl>",
            e.at,
            escape(&e.reason),
            escape(&status.node_id),
            status.recovery_codes_left,
        ),
        // Locked with no engagement recorded means the journal was
        // unreadable at startup, which is itself worth telling the operator.
        None => format!(
            "<dl><dt>Node</dt><dd><code>{}</code></dd>\
<dt>Reason</dt><dd>the lockdown journal could not be read</dd></dl>",
            escape(&status.node_id),
        ),
    };

    format!(
        "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\">\
<meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">\
<meta name=\"robots\" content=\"noindex\">\
<title>Locked down</title><style>{LOCKED_STYLE}</style></head><body><main>\
<h1>Locked down</h1>\
<p>Every session has been revoked, child processes were stopped and outbound \
actions are frozen. Nothing here will work until this node is unlocked.</p>\
<p>Unlocking needs the passkey, plus either <code>deckjack unlock</code> on \
the host or an offline recovery code.</p>\
{detail}</main></body></html>"
    )
}

/// The HTTP response for a locked node: `503`, never cached, with the same
/// CSP the sign-in pages use.
pub fn locked_response(status: &Status) -> axum::response::Response {
    use axum::http::{StatusCode, header};
    use axum::response::{Html, IntoResponse as _};

    (
        StatusCode::SERVICE_UNAVAILABLE,
        [
            (header::CACHE_CONTROL, "no-store"),
            (header::CONTENT_SECURITY_POLICY, crate::auth::html::CSP),
            // No number of seconds would be honest: this ends when a person
            // unlocks it, so tell intermediaries not to guess.
            (header::RETRY_AFTER, "3600"),
        ],
        Html(locked_page(status)),
    )
        .into_response()
}

/// Axum middleware for step 4. Mounted with
/// `middleware::from_fn_with_state(lockdown.clone(), gate)` so it carries
/// its own state and does not force the router's state type to change.
///
/// It sits outside every other route on purpose: if the gate were a check
/// inside each handler, the one handler that forgot it would be the hole.
pub async fn gate(
    axum::extract::State(lockdown): axum::extract::State<std::sync::Arc<Lockdown>>,
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    if !lockdown.is_locked() {
        return next.run(request).await;
    }
    let path = request.uri().path().to_string();
    if allowed_while_locked(&path) {
        return next.run(request).await;
    }
    tracing::info!(%path, "refused: locked down");
    locked_response(&lockdown.status())
}

// ---- the way out: the host token and the /unlock route -----------------

/// Where the host token lives, relative to the working directory.
pub const HOST_TOKEN_PATH: &str = ".deckjack/unlock.token";

/// Proof that a request came from something with access to the working
/// directory, which is what [`Identity::HostConsole`] means in practice.
///
/// Lockdown's unlock rule is the passkey plus a second factor, and until
/// WebAuthn exists [`Policy::allow_host_console_identity`] lets the host
/// console stand in for the passkey. That raised a question the module left
/// open: over HTTP, what *is* the host console?
///
/// Not "the request arrived on loopback". `tailscale serve` proxies from
/// loopback, so every remote request looks local. Not "loopback and no
/// `X-Forwarded-For`" either — hanging an authentication decision on a
/// header being absent is the kind of thing that is true until some
/// intermediary changes.
///
/// So: a random value written to a file inside the working directory, mode
/// 0600. Reading it needs filesystem access on the host as the user running
/// the server, which is exactly the access `Identity::HostConsole` is
/// claiming. `deckjack unlock` reads the file and sends it; nothing reachable
/// over the network can.
///
/// It is not a second factor by itself, and the code never treats it as one.
/// The recovery code is still required, and it exists only on paper — the
/// journal holds digests — so host access alone does not unlock anything.
#[derive(Clone)]
pub struct HostToken(String);

impl fmt::Debug for HostToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("HostToken(<redacted>)")
    }
}

impl HostToken {
    /// Read the token for a working directory, creating it if it is not
    /// there. Stable across restarts on purpose: a token that changed every
    /// start would invalidate a `deckjack unlock` the operator was midway
    /// through typing.
    pub fn load_or_create(working_dir: &Path, random: &dyn Random) -> io::Result<HostToken> {
        let path = working_dir.join(HOST_TOKEN_PATH);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        if let Ok(text) = fs::read_to_string(&path) {
            let trimmed = text.trim();
            if trimmed.len() == 64 {
                return Ok(HostToken(trimmed.to_string()));
            }
            tracing::warn!(
                path = %path.display(),
                "the host unlock token is not a 64-character value; replacing it"
            );
        }

        let mut bytes = [0u8; 32];
        random
            .fill(&mut bytes)
            .map_err(|e| io::Error::other(e.to_string()))?;
        let token = HostToken(hex(&bytes));

        // Owner-only before the bytes go in, not after: a token that spends a
        // moment at 0644 is a token that may have been read.
        #[cfg(unix)]
        {
            use std::io::Write as _;
            use std::os::unix::fs::OpenOptionsExt as _;
            let mut f = fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
                .open(&path)?;
            f.write_all(token.0.as_bytes())?;
            f.write_all(b"\n")?;
        }
        #[cfg(not(unix))]
        fs::write(&path, format!("{}\n", token.0))?;

        Ok(token)
    }

    /// Constant time, so a wrong token does not leak how much of it was
    /// right to something that can time the response.
    pub fn matches(&self, typed: &str) -> bool {
        let typed = typed.trim();
        self.0.len() == typed.len() && bool::from(self.0.as_bytes().ct_eq(typed.as_bytes()))
    }

    pub fn reveal(&self) -> &str {
        &self.0
    }
}

/// What the `/unlock` route needs. Held together so the route's state is one
/// type and the router does not force the rest of the app's state on it.
pub struct UnlockCtx {
    lockdown: std::sync::Arc<Lockdown>,
    token: HostToken,
}

impl UnlockCtx {
    pub fn new(lockdown: std::sync::Arc<Lockdown>, token: HostToken) -> std::sync::Arc<UnlockCtx> {
        std::sync::Arc::new(UnlockCtx { lockdown, token })
    }
}

/// `GET /unlock` and `POST /unlock`, the only mutating route that keeps
/// working while locked down (see [`ALLOWED_WHILE_LOCKED`]).
///
/// `GET` says what to do and nothing else: no form, because the thing that
/// has to be submitted is a file only the host can read, and a browser form
/// that asked for it would invite pasting it into one on a borrowed machine.
/// `POST` takes the token and a recovery code as form fields, which is what
/// `deckjack unlock --recovery-code` sends.
pub fn unlock_router(ctx: std::sync::Arc<UnlockCtx>) -> axum::Router {
    use axum::routing::get;
    axum::Router::new()
        .route("/unlock", get(unlock_page).post(unlock_submit))
        .with_state(ctx)
}

const UNLOCK_INSTRUCTIONS: &str = "\
<p>Unlocking needs two things: access to this node's working directory on the \
host, and one of the offline recovery codes printed when they were issued.</p>\
<p>On the host, run:</p>\
<pre><code>deckjack unlock --recovery-code XXXX-XXXX-XXXX-XXXX-XXXX</code></pre>\
<p>There is no form here on purpose. The host half of the proof is a file \
inside the working directory, and a page that asked you to paste it would be \
a page asking you to carry it onto whatever machine you are reading this on.</p>";

async fn unlock_page(
    axum::extract::State(ctx): axum::extract::State<std::sync::Arc<UnlockCtx>>,
) -> axum::response::Response {
    use axum::http::{StatusCode, header};
    use axum::response::{Html, IntoResponse as _};

    let status = ctx.lockdown.status();
    let (code, heading) = if status.locked {
        (StatusCode::SERVICE_UNAVAILABLE, "Locked down")
    } else {
        (StatusCode::OK, "Not locked")
    };
    let body = if status.locked {
        format!(
            "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\">\
<meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">\
<meta name=\"robots\" content=\"noindex\"><title>Unlock</title>\
<style>{LOCKED_STYLE}</style></head><body><main><h1>{heading}</h1>\
{UNLOCK_INSTRUCTIONS}<dl><dt>Node</dt><dd><code>{}</code></dd>\
<dt>Recovery codes</dt><dd>{} left</dd></dl></main></body></html>",
            escape(&status.node_id),
            status.recovery_codes_left,
        )
    } else {
        format!(
            "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\">\
<meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">\
<meta name=\"robots\" content=\"noindex\"><title>Unlock</title>\
<style>{LOCKED_STYLE}</style></head><body><main><h1>{heading}</h1>\
<p>This node is not locked down. There is nothing to unlock.</p>\
<dl><dt>Node</dt><dd><code>{}</code></dd></dl></main></body></html>",
            escape(&status.node_id),
        )
    };
    (
        code,
        [
            (header::CACHE_CONTROL, "no-store"),
            (header::CONTENT_SECURITY_POLICY, crate::auth::html::CSP),
        ],
        Html(body),
    )
        .into_response()
}

#[derive(Debug, Deserialize)]
pub struct UnlockForm {
    #[serde(default)]
    pub token: String,
    #[serde(default)]
    pub recovery_code: String,
}

async fn unlock_submit(
    axum::extract::State(ctx): axum::extract::State<std::sync::Arc<UnlockCtx>>,
    axum::Form(form): axum::Form<UnlockForm>,
) -> axum::response::Response {
    use axum::http::StatusCode;
    use axum::response::IntoResponse as _;

    // The token is checked first and on its own, so a caller that cannot
    // prove host access never gets to spend a recovery code — not even a
    // wrong one, which `Lockdown::unlock` would otherwise count as a failed
    // attempt against the set.
    if !ctx.token.matches(&form.token) {
        tracing::warn!("unlock refused: the host token did not match");
        return (
            StatusCode::FORBIDDEN,
            "the host token did not match; run this from the host\n",
        )
            .into_response();
    }

    let request = UnlockRequest {
        identity: Identity::HostConsole,
        second: SecondFactor::RecoveryCode(form.recovery_code),
    };
    match ctx.lockdown.unlock(request) {
        Ok(()) => (StatusCode::OK, "unlocked\n").into_response(),
        Err(e) => {
            tracing::warn!(error = %e, "unlock refused");
            let code = match e {
                UnlockError::NotLocked => StatusCode::CONFLICT,
                UnlockError::JournalWrite { .. } => StatusCode::INTERNAL_SERVER_ERROR,
                _ => StatusCode::FORBIDDEN,
            };
            (code, format!("{e}\n")).into_response()
        }
    }
}
