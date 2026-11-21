//! Installing, updating and removing apps: `deckjack install`, `deckjack
//! uninstall` and `deckjack apps`.
//!
//! An app is a directory `<install>/apps/<name>/` holding its `app.toml` and
//! its `deckjack-<name>` executable (see [`crate::apps`]). Installing one is
//! three promises, and this module exists to keep them:
//!
//! 1. **Nothing lands that the core would refuse to run.** The manifest goes
//!    through the protocol crate's own validation and protocol-range check,
//!    and the binary must be an ELF executable for this machine's
//!    architecture. Both checks run on the *staged copies*, the bytes that
//!    will actually be renamed into place, so a source file changed between
//!    the check and the copy cannot slip through.
//!
//! 2. **The swap is atomic and safe while the old version runs.** The new
//!    version is staged in full under `apps/.staging/`, then exchanged with
//!    the old directory in one `renameat2(RENAME_EXCHANGE)`. There is no
//!    instant at which `apps/<name>/` holds a new manifest beside an old
//!    binary, or no app at all. A running process keeps its own inode, so
//!    removing the old directory afterwards does not disturb it.
//!
//! 3. **An install never approves anything.** This module never writes an
//!    approval. The approval model lives in [`crate::apps`]: a grant records
//!    the manifest's fingerprint, and a launch whose manifest has a different
//!    fingerprint is refused as stale. So an update that asks for more is
//!    re-approved by the operator before it runs, whatever this module does.
//!    What it can do is *say so*, which is [`Approval`] — worked out against
//!    the approval the server has actually stored, read (read-only) from the
//!    working directory's state database, because that is what the launch
//!    compares against. Comparing the previous manifest with the new one
//!    instead promised approvals that did not exist: a reinstall of an app
//!    never approved, and every app approved before the fingerprint moved
//!    from fnv1a64 to SHA-256.
//!
//! Signing is out of scope: an installed binary is trusted because the
//! operator, on the host, chose to install it. Nothing here checks who built
//! it.

use std::fs;
use std::io::{self, Read as _, Write as _};
use std::path::{Path, PathBuf};

use thiserror::Error;

use crate::apps::{
    self, APPS_SUBDIR, BINARY_PREFIX, Discovery, MANIFEST_FILE, Manifest, ManifestError,
    ProtocolError,
};

/// Where an install is staged, inside `apps/` so that the final rename never
/// crosses a filesystem. Discovery only claims a directory with an `app.toml`
/// directly inside it, and this directory holds subdirectories, so nothing
/// staged here is ever mistaken for an app.
pub const STAGING_SUBDIR: &str = ".staging";

// ---------------------------------------------------------------------------
// What an install reads
// ---------------------------------------------------------------------------

/// The two files an install copies. Built with [`Source::from_dir`] for a
/// built app directory, or by hand when the binary lives elsewhere — in this
/// repository the manifest is `crates/deckjack-<name>/app.toml` and the binary
/// is under `target/`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Source {
    pub manifest: PathBuf,
    pub binary: PathBuf,
}

impl Source {
    /// `<dir>/app.toml` and `<dir>/deckjack-<name>`, the name read from the
    /// manifest. With `binary`, that path is used instead.
    pub fn from_dir(dir: &Path, binary: Option<PathBuf>) -> Result<Source, InstallError> {
        let manifest = dir.join(MANIFEST_FILE);
        let binary = match binary {
            Some(binary) => binary,
            None => {
                let (parsed, _) = read_manifest(&manifest)?;
                dir.join(format!("{BINARY_PREFIX}{}", parsed.app.name))
            }
        };
        Ok(Source { manifest, binary })
    }
}

// ---------------------------------------------------------------------------
// What an install reports
// ---------------------------------------------------------------------------

/// Whether the installed version can run on an approval the operator has
/// already given: the stored grant's fingerprint against this manifest's,
/// the same comparison the server makes before it launches the app.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Approval {
    /// The app asks for nothing in `[needs]`, so it runs without one.
    NotNeeded,
    /// The stored approval was given for exactly what this version asks.
    Approved,
    /// The operator must approve this version before it runs.
    Required { because: Because, summary: String },
    /// The working directory's approvals could not be read, so whether this
    /// version may run is not known here. The server decides at launch.
    Unknown { reason: String, summary: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Because {
    /// Nothing by this name was installed, and nothing was approved for it.
    NewApp,
    /// It was installed before, but never approved (or the approval was
    /// revoked).
    NotApproved,
    /// The stored approval covers different needs or commands. Narrowing
    /// counts too: the approval is for exactly what was shown.
    Changed,
    /// The stored approval was recorded under the fnv1a64 fingerprint, which
    /// SHA-256 replaced. Every app approved before that change is asked once
    /// more, whatever its manifest says.
    OlderFingerprint,
    /// A stored approval exists but its recorded needs cannot be read. The
    /// server treats that row as no approval (see
    /// [`crate::store::Store::app_grant`]) and asks again.
    Unreadable,
}

/// The prefix the approval fingerprint carried before it was SHA-256.
pub const OLD_FINGERPRINT_PREFIX: &str = "fnv1a64:";

/// What the operator does to approve an app, said once so the install
/// report, the server's refusals and the shell agree. The shell lists one
/// palette entry per app awaiting approval, titled `Approve <title>`, which
/// runs `system.app.approve`.
pub fn how_to_approve(title: &str) -> String {
    format!(
        "Approve it from the shell: open the command palette and choose \"Approve {title}\" \
         (it runs system.app.approve, which asks you to confirm)"
    )
}

/// What an install did. Every field is required, so a caller that renders
/// it cannot forget to say whether approval is needed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    pub name: String,
    pub version: String,
    /// `apps/<name>/`, where it now lives.
    pub dir: PathBuf,
    /// The version it replaced, if one was installed and its manifest read.
    pub previous_version: Option<String>,
    pub fingerprint: String,
    pub approval: Approval,
    /// What the shell calls the app: its manifest title, else its name.
    pub title: String,
}

impl Report {
    /// What `deckjack install` prints.
    pub fn render(&self) -> String {
        let what = match &self.previous_version {
            Some(old) if old == &self.version => {
                format!("reinstalled {} {}", self.name, self.version)
            }
            Some(old) => format!("updated {} from {old} to {}", self.name, self.version),
            None => format!("installed {} {}", self.name, self.version),
        };
        let approval = match &self.approval {
            Approval::NotNeeded => {
                "It asks for nothing beyond the command bus, so it needs no approval.".to_string()
            }
            Approval::Approved => "The approval recorded for it covers exactly what this \
                                   version asks for, so it runs without asking again."
                .to_string(),
            Approval::Required { because, summary } => {
                let why = match because {
                    Because::NewApp => "It is new",
                    Because::NotApproved => "It has never been approved",
                    Because::Changed => {
                        "Its needs or commands changed, so the recorded approval no longer covers it"
                    }
                    Because::Unreadable => {
                        "Its recorded approval could not be read, so the server does not honor \
                         it and asks again"
                    }
                    Because::OlderFingerprint => {
                        "Its approval was recorded under the fingerprint this version of \
                         Deckjack replaced (fnv1a64, now SHA-256), so it is asked once more"
                    }
                };
                format!(
                    "{why}: the operator must approve it before it runs. It asks for {summary}. \
                     {}.",
                    how_to_approve(&self.title)
                )
            }
            Approval::Unknown { reason, summary } => format!(
                "Whether it is approved is unknown: {reason}. It asks for {summary}. If the \
                 server refuses to run it: {}.",
                how_to_approve(&self.title)
            ),
        };
        format!(
            "{what} into {}\n{approval}\nA server already running picks this up when it \
             restarts.",
            self.dir.display()
        )
    }
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

#[derive(Debug, Error)]
pub enum InstallError {
    #[error(transparent)]
    Manifest(#[from] ManifestError),

    #[error(transparent)]
    Protocol(#[from] ProtocolError),

    #[error(transparent)]
    Binary(#[from] ElfError),

    #[error("{path} is not a file; give the {BINARY_PREFIX}<name> executable the build produced")]
    BinaryNotAFile { path: PathBuf },

    #[error(
        "{path} does not exist; build the app first, or name its binary with --binary \
         (cargo puts it under target/)"
    )]
    BinaryMissing { path: PathBuf },

    #[error(
        "{name:?} is not an app name: a name is 1-32 characters of a-z, 0-9 and '-', \
         starting with a letter, so it cannot name anything outside {APPS_SUBDIR}/"
    )]
    BadName { name: String },

    #[error("no app named {name:?} is installed in {dir}")]
    NotInstalled { name: String, dir: PathBuf },

    #[error(
        "{path} exists but is not an app directory; move it aside, because \
         {APPS_SUBDIR}/<name>/ is where the app {name:?} has to live"
    )]
    InTheWay { name: String, path: PathBuf },

    #[error("cannot {doing} {path}: {source}")]
    Io {
        doing: &'static str,
        path: PathBuf,
        #[source]
        source: io::Error,
    },
}

fn io_err<'a>(doing: &'static str, path: &'a Path) -> impl FnOnce(io::Error) -> InstallError + 'a {
    move |source| InstallError::Io {
        doing,
        path: path.to_path_buf(),
        source,
    }
}

// ---------------------------------------------------------------------------
// The ELF check
// ---------------------------------------------------------------------------

#[derive(Debug, Error, PartialEq, Eq)]
pub enum ElfError {
    #[error(
        "{path} is not an ELF executable (a script, or a file built for another operating \
         system?); install the binary `cargo build` produced"
    )]
    NotElf { path: PathBuf },

    #[error("{path} is an ELF {kind}, not an executable; install the linked binary")]
    NotExecutable { path: PathBuf, kind: &'static str },

    #[error(
        "{path} is built for {found}, but this machine is {expected}; build it for {expected} \
         and install that"
    )]
    WrongArch {
        path: PathBuf,
        found: String,
        expected: String,
    },
}

/// The ELF identity of the machine this core was built for, which is the
/// machine it is running on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Machine {
    /// `e_machine`.
    pub machine: u16,
    /// `EI_CLASS`: 1 for 32-bit, 2 for 64-bit.
    pub class: u8,
    /// `EI_DATA`: 1 for little-endian, 2 for big-endian.
    pub data: u8,
}

const EM_386: u16 = 3;
const EM_ARM: u16 = 40;
const EM_X86_64: u16 = 62;
const EM_AARCH64: u16 = 183;
const EM_RISCV: u16 = 243;

impl Machine {
    /// This build's target.
    pub fn this() -> Machine {
        let machine = if cfg!(target_arch = "x86_64") {
            EM_X86_64
        } else if cfg!(target_arch = "aarch64") {
            EM_AARCH64
        } else if cfg!(target_arch = "x86") {
            EM_386
        } else if cfg!(target_arch = "arm") {
            EM_ARM
        } else if cfg!(any(target_arch = "riscv64", target_arch = "riscv32")) {
            EM_RISCV
        } else {
            // An architecture this table does not know refuses everything,
            // which is the safe failure: the error names both sides.
            0
        };
        Machine {
            machine,
            class: if cfg!(target_pointer_width = "64") {
                2
            } else {
                1
            },
            data: if cfg!(target_endian = "little") { 1 } else { 2 },
        }
    }

    /// A name an operator recognizes: `x86_64`, `aarch64`, and so on.
    pub fn describe(&self) -> String {
        let arch = match (self.machine, self.class) {
            (EM_X86_64, 2) => "x86_64".to_string(),
            (EM_X86_64, _) => "x86_64 (32-bit x32)".to_string(),
            (EM_AARCH64, 2) => "aarch64".to_string(),
            (EM_AARCH64, _) => "aarch64 (32-bit ILP32)".to_string(),
            (EM_386, _) => "x86 (32-bit)".to_string(),
            (EM_ARM, _) => "arm (32-bit)".to_string(),
            (EM_RISCV, 2) => "riscv64".to_string(),
            (EM_RISCV, _) => "riscv32".to_string(),
            (8, _) => "mips".to_string(),
            (20, _) => "powerpc".to_string(),
            (21, _) => "powerpc64".to_string(),
            (22, _) => "s390x".to_string(),
            (258, _) => "loongarch".to_string(),
            (other, _) => format!("an unknown architecture (ELF machine {other})"),
        };
        if self.data == 2 {
            format!("{arch}, big-endian")
        } else {
            arch
        }
    }
}

/// Check that `header` (the first bytes of `path`) starts an ELF executable
/// for `expected`. Reads only the identification bytes, `e_type` and
/// `e_machine`, which sit at the same offsets in 32- and 64-bit files.
pub fn check_elf(path: &Path, header: &[u8], expected: Machine) -> Result<(), ElfError> {
    if header.len() < 20 || header[..4] != [0x7f, b'E', b'L', b'F'] {
        return Err(ElfError::NotElf {
            path: path.to_path_buf(),
        });
    }
    let class = header[4];
    let data = header[5];
    let half = |at: usize| {
        let bytes = [header[at], header[at + 1]];
        match data {
            2 => u16::from_be_bytes(bytes),
            _ => u16::from_le_bytes(bytes),
        }
    };
    if !matches!(class, 1 | 2) || !matches!(data, 1 | 2) {
        return Err(ElfError::NotElf {
            path: path.to_path_buf(),
        });
    }
    let found = Machine {
        machine: half(18),
        class,
        data,
    };
    if found != expected {
        return Err(ElfError::WrongArch {
            path: path.to_path_buf(),
            found: found.describe(),
            expected: expected.describe(),
        });
    }
    // ET_EXEC, or ET_DYN for a position-independent executable, which is what
    // a modern toolchain links by default.
    match half(16) {
        2 | 3 => Ok(()),
        1 => Err(ElfError::NotExecutable {
            path: path.to_path_buf(),
            kind: "object file",
        }),
        4 => Err(ElfError::NotExecutable {
            path: path.to_path_buf(),
            kind: "core dump",
        }),
        _ => Err(ElfError::NotExecutable {
            path: path.to_path_buf(),
            kind: "file of an unknown type",
        }),
    }
}

// ---------------------------------------------------------------------------
// Install
// ---------------------------------------------------------------------------

fn read_manifest(path: &Path) -> Result<(Manifest, String), InstallError> {
    let text = fs::read_to_string(path).map_err(|source| ManifestError::Read {
        path: path.to_path_buf(),
        source,
    })?;
    let manifest = Manifest::parse(path, &text)?;
    apps::check_protocol(&manifest.app.name, manifest.app.protocol)?;
    Ok((manifest, text))
}

/// `apps/<name>`, refusing any name that is not a valid app name. A valid
/// name is a single path component of `[a-z0-9-]`, so it cannot be `..`,
/// cannot hold a `/`, and cannot start with the `.` of the staging directory.
pub fn app_dir(install_dir: &Path, name: &str) -> Result<PathBuf, InstallError> {
    if !apps::valid_name(name) {
        return Err(InstallError::BadName {
            name: name.to_string(),
        });
    }
    Ok(apps::apps_root(install_dir).join(name))
}

/// A staging directory, removed on drop with whatever it then holds: the
/// half-written new version when an install fails, the old version after a
/// swap. The shared `.staging` directory goes too once nothing else is in it,
/// so `apps/` is left as the operator had it.
struct Staged {
    path: PathBuf,
}

impl Drop for Staged {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
        if let Some(root) = self.path.parent() {
            // Fails, harmlessly, while another install is staging. One that
            // is about to stage retries if this wins (see `staging_dir`).
            let _ = fs::remove_dir(root);
        }
    }
}

fn staging_dir(install_dir: &Path, name: &str) -> Result<Staged, InstallError> {
    let root = apps::apps_root(install_dir).join(STAGING_SUBDIR);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    let path = root.join(format!("{name}.{}.{nanos}", std::process::id()));
    let apps_root = apps::apps_root(install_dir);
    fs::create_dir_all(&apps_root).map_err(io_err("create", &apps_root))?;
    // Another install finishing can remove the shared root (see `Staged`'s
    // drop) at any moment: between creating it and creating this directory
    // inside it, or even inside `create_dir_all`, which reports "exists" for
    // a directory that was there at mkdir and gone at its second look. That
    // is a lost race, not a failure, so create the root again and retry.
    let mut attempts = 0;
    loop {
        match fs::create_dir(&root) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(io_err("create", &root)(e)),
        }
        match fs::create_dir(&path) {
            Ok(()) => return Ok(Staged { path }),
            Err(e) if e.kind() == io::ErrorKind::NotFound && attempts < 100 => attempts += 1,
            Err(e) => return Err(io_err("create", &path)(e)),
        }
    }
}

fn write_file(path: &Path, bytes: &[u8], mode: u32) -> Result<(), InstallError> {
    use std::os::unix::fs::OpenOptionsExt as _;
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(mode)
        .open(path)
        .map_err(io_err("create", path))?;
    file.write_all(bytes).map_err(io_err("write", path))?;
    file.sync_all().map_err(io_err("flush", path))
}

fn copy_file(from: &Path, to: &Path, mode: u32) -> Result<(), InstallError> {
    use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};
    let mut src = fs::File::open(from).map_err(io_err("read", from))?;
    let mut dst = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(mode)
        .open(to)
        .map_err(io_err("create", to))?;
    io::copy(&mut src, &mut dst).map_err(io_err("copy", from))?;
    // `mode` above is filtered by the umask; an executable the core cannot
    // execute is a "not executable" problem at discovery, so set it plainly.
    dst.set_permissions(fs::Permissions::from_mode(mode))
        .map_err(io_err("set permissions on", to))?;
    dst.sync_all().map_err(io_err("flush", to))
}

const RENAME_NOREPLACE: libc::c_uint = 1;
const RENAME_EXCHANGE: libc::c_uint = 2;

/// `renameat2(2)`, called through `syscall` because libc's wrapper is not
/// present on every Linux libc this may be built against.
fn renameat2(from: &Path, to: &Path, flags: libc::c_uint) -> io::Result<()> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt as _;
    let from = CString::new(from.as_os_str().as_bytes())?;
    let to = CString::new(to.as_os_str().as_bytes())?;
    // SAFETY: both pointers are to NUL-terminated strings that outlive the
    // call; the syscall reads them and retains nothing.
    let rc = unsafe {
        libc::syscall(
            libc::SYS_renameat2,
            libc::AT_FDCWD,
            from.as_ptr(),
            libc::AT_FDCWD,
            to.as_ptr(),
            flags,
        )
    };
    if rc == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

fn sync_dir(dir: &Path) {
    // Best effort: the rename has happened either way, and a filesystem that
    // will not fsync a directory is not a reason to report a failed install.
    if let Ok(dir) = fs::File::open(dir) {
        let _ = dir.sync_all();
    }
}

/// Put `staged` at `target`, atomically. Returns where the replaced version
/// went (inside staging), for the caller to delete.
fn swap_into_place(staged: &Path, target: &Path) -> Result<Option<PathBuf>, InstallError> {
    // Fresh install. NOREPLACE, so an install racing this one is not
    // silently overwritten; if it loses, it falls through to the exchange.
    if fs::symlink_metadata(target).is_err() {
        match renameat2(staged, target, RENAME_NOREPLACE) {
            Ok(()) => return Ok(None),
            Err(e) if e.raw_os_error() == Some(libc::EEXIST) => {}
            Err(e) if e.raw_os_error() == Some(libc::EINVAL) => {
                // The filesystem has no renameat2 flags; a plain rename onto
                // a missing path is just as atomic.
                fs::rename(staged, target).map_err(io_err("move into place", target))?;
                return Ok(None);
            }
            Err(e) => return Err(io_err("move into place", target)(e)),
        }
    }

    match renameat2(staged, target, RENAME_EXCHANGE) {
        // The old version is now at `staged`.
        Ok(()) => Ok(Some(staged.to_path_buf())),
        Err(e) if e.raw_os_error() == Some(libc::EINVAL) => {
            // A filesystem without RENAME_EXCHANGE. Two renames, and between
            // them the app is briefly absent; never a mix of old and new.
            let mut old = staged.as_os_str().to_owned();
            old.push(".old");
            let old = PathBuf::from(old);
            fs::rename(target, &old).map_err(io_err("move aside", target))?;
            if let Err(e) = fs::rename(staged, target) {
                let _ = fs::rename(&old, target);
                return Err(io_err("move into place", target)(e));
            }
            Ok(Some(old))
        }
        Err(e) => Err(io_err("swap into place", target)(e)),
    }
}

/// Install (or update) the app at `source` into `install_dir`, reporting
/// against the approvals of a server whose working directory is the install
/// directory — the default, and how `deckjack install` runs without `--dir`.
///
/// That working directory is a guess: the server may run with `--dir`
/// elsewhere. Where the guess holds no state database at all, the report
/// says the approval is unknown rather than that the app was never approved.
pub fn install(install_dir: &Path, source: &Source) -> Result<Report, InstallError> {
    install_reporting(install_dir, install_dir, Named::Guessed, source)
}

/// Whether the working directory the report is about was named by the
/// operator (`--dir`) or guessed (the server's default).
#[derive(Clone, Copy, PartialEq, Eq)]
enum Named {
    Explicitly,
    Guessed,
}

/// What the working directory's state database holds for one app.
enum Stored {
    /// No state database: no server has run with this working directory.
    NoDatabase,
    /// A database with no approval for this app.
    Nothing,
    /// A row the server would not honor: its needs do not parse.
    Unreadable,
    /// The fingerprint of an approval the server would read.
    Fingerprint(String),
}

/// What the working directory's state database holds for `app`. Opened
/// read-only and never written: an install does not approve. A row is
/// judged as [`crate::store::Store::app_grant`] judges it, so the report and
/// the server's launch check cannot disagree about whether it counts.
fn stored_grant(working_dir: &Path, app: &str) -> Result<Stored, String> {
    use rusqlite::{OpenFlags, OptionalExtension as _};

    let path = working_dir.join(crate::store::DB_PATH);
    if !path.is_file() {
        return Ok(Stored::NoDatabase);
    }
    let describe = |e: rusqlite::Error| format!("cannot read approvals in {}: {e}", path.display());
    let conn = rusqlite::Connection::open_with_flags(
        &path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(describe)?;
    // A server may be writing; wait for it briefly rather than misreport.
    conn.busy_timeout(std::time::Duration::from_secs(2))
        .map_err(describe)?;
    let has_table: bool = conn
        .query_row(
            "SELECT EXISTS (SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'app_grants')",
            [],
            |r| r.get(0),
        )
        .map_err(describe)?;
    if !has_table {
        return Ok(Stored::Nothing);
    }
    let row: Option<(String, String)> = conn
        .query_row(
            "SELECT fingerprint, needs FROM app_grants WHERE app = ?1",
            [app],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()
        .map_err(describe)?;
    Ok(match row {
        None => Stored::Nothing,
        Some((_, needs)) if serde_json::from_str::<apps::Needs>(&needs).is_err() => {
            Stored::Unreadable
        }
        Some((fingerprint, _)) => Stored::Fingerprint(fingerprint),
    })
}

/// [`install`], reporting against the approvals of the server serving
/// `working_dir` (`deckjack install --dir`).
pub fn install_in(
    install_dir: &Path,
    working_dir: &Path,
    source: &Source,
) -> Result<Report, InstallError> {
    install_reporting(install_dir, working_dir, Named::Explicitly, source)
}

fn install_reporting(
    install_dir: &Path,
    working_dir: &Path,
    named: Named,
    source: &Source,
) -> Result<Report, InstallError> {
    // Read and validate the manifest once; the text that was validated is
    // the text written, never a second read of a file that may have changed.
    let (manifest, text) = read_manifest(&source.manifest)?;
    let name = manifest.app.name.clone();
    let target = app_dir(install_dir, &name)?;

    let meta = match fs::metadata(&source.binary) {
        Ok(meta) => meta,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            return Err(InstallError::BinaryMissing {
                path: source.binary.clone(),
            });
        }
        Err(e) => return Err(io_err("read", &source.binary)(e)),
    };
    if !meta.is_file() {
        return Err(InstallError::BinaryNotAFile {
            path: source.binary.clone(),
        });
    }

    // Whatever is there now must be an app directory (or a link to one) to be
    // replaced: a directory with an `app.toml` in it, readable or not. A stray
    // file of that name, or a directory without a manifest, is the
    // operator's, and the swap would delete it. `uninstall` draws the same
    // line.
    match fs::metadata(&target) {
        Ok(m) if !m.is_dir() || !target.join(MANIFEST_FILE).is_file() => {
            return Err(InstallError::InTheWay { name, path: target });
        }
        _ => {}
    }

    let staged = staging_dir(install_dir, &name)?;
    let binary_name = format!("{BINARY_PREFIX}{name}");
    let staged_binary = staged.path.join(&binary_name);
    write_file(&staged.path.join(MANIFEST_FILE), text.as_bytes(), 0o644)?;
    copy_file(&source.binary, &staged_binary, 0o755)?;

    // Checked on the copy that will be renamed into place.
    let mut header = [0u8; 64];
    let read = {
        let mut file = fs::File::open(&staged_binary).map_err(io_err("read", &staged_binary))?;
        let mut n = 0;
        while n < header.len() {
            match file.read(&mut header[n..]) {
                Ok(0) => break,
                Ok(k) => n += k,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(io_err("read", &staged_binary)(e)),
            }
        }
        n
    };
    check_elf(&source.binary, &header[..read], Machine::this())?;
    sync_dir(&staged.path);

    // The previous version, for the report. Read just before the swap so it
    // describes what is being replaced.
    let previous = if fs::symlink_metadata(&target).is_ok() {
        Some(read_manifest(&target.join(MANIFEST_FILE)).ok())
    } else {
        None
    };

    let replaced = swap_into_place(&staged.path, &target)?;
    sync_dir(&apps::apps_root(install_dir));
    if let Some(old) = replaced {
        // The old binary may be running. Unlinking it is safe: the process
        // holds its inode, not its name.
        let _ = fs::remove_dir_all(&old);
    }
    drop(staged);

    let fingerprint = manifest.fingerprint();
    let summary = apps::summarize(&manifest.needs);
    // Against the stored approval, exactly as `apps::Host` checks it at
    // launch: equal fingerprints run, anything else is asked again.
    let approval = if manifest.needs.is_empty() {
        Approval::NotNeeded
    } else {
        let unapproved = |because| Approval::Required {
            because,
            summary: summary.clone(),
        };
        let never = if previous.is_some() {
            Because::NotApproved
        } else {
            Because::NewApp
        };
        match stored_grant(working_dir, &name) {
            Err(reason) => Approval::Unknown {
                reason,
                summary: summary.clone(),
            },
            Ok(Stored::Fingerprint(stored)) if stored == fingerprint => Approval::Approved,
            Ok(Stored::Fingerprint(stored)) => {
                unapproved(if stored.starts_with(OLD_FINGERPRINT_PREFIX) {
                    Because::OlderFingerprint
                } else {
                    Because::Changed
                })
            }
            Ok(Stored::Unreadable) => unapproved(Because::Unreadable),
            Ok(Stored::Nothing) => unapproved(never),
            // A directory the operator named, where no server has run yet:
            // the server that does will start with no approvals.
            Ok(Stored::NoDatabase) if named == Named::Explicitly => unapproved(never),
            Ok(Stored::NoDatabase) => Approval::Unknown {
                reason: format!(
                    "no server has recorded approvals in {}, the default working \
                     directory; if the server runs with --dir, run deckjack install \
                     again with the same --dir to check that directory's approvals",
                    working_dir.display()
                ),
                summary: summary.clone(),
            },
        }
    };
    let title = manifest
        .app
        .title
        .clone()
        .unwrap_or_else(|| manifest.app.name.clone());

    Ok(Report {
        name,
        version: manifest.app.version.clone(),
        dir: target,
        previous_version: previous.flatten().map(|(old, _)| old.app.version),
        fingerprint,
        approval,
        title,
    })
}

// ---------------------------------------------------------------------------
// Uninstall and list
// ---------------------------------------------------------------------------

/// Remove `apps/<name>/`. The directory is first renamed into staging, so
/// discovery never sees a half-deleted app, then removed.
///
/// An approval the operator gave stays in the working directory's database.
/// It is keyed by the manifest fingerprint, so it only ever applies again to
/// a reinstall asking for exactly what was approved.
pub fn uninstall(install_dir: &Path, name: &str) -> Result<PathBuf, InstallError> {
    let target = app_dir(install_dir, name)?;
    if fs::symlink_metadata(&target).is_err() {
        return Err(InstallError::NotInstalled {
            name: name.to_string(),
            dir: apps::apps_root(install_dir),
        });
    }
    if !target.join(MANIFEST_FILE).is_file() {
        return Err(InstallError::InTheWay {
            name: name.to_string(),
            path: target,
        });
    }
    let staged = staging_dir(install_dir, name)?;
    let gone = staged.path.join("removed");
    fs::rename(&target, &gone).map_err(io_err("remove", &target))?;
    sync_dir(&apps::apps_root(install_dir));
    drop(staged);
    Ok(target)
}

/// What `deckjack apps` prints: every app discovery finds, and every
/// directory it will not run with the reason.
pub fn render_listing(found: &Discovery) -> String {
    let mut out = String::new();
    if found.apps.is_empty() && found.problems.is_empty() {
        out.push_str("no apps installed\n");
    }
    for app in &found.apps {
        out.push_str(&format!(
            "{} {}  needs {}\n",
            app.name,
            app.manifest.app.version,
            apps::summarize(&app.manifest.needs)
        ));
    }
    for problem in &found.problems {
        let who = problem
            .name
            .clone()
            .unwrap_or_else(|| problem.dir.display().to_string());
        out.push_str(&format!("{who}  NOT RUNNABLE: {}\n", problem.error));
    }
    out
}
