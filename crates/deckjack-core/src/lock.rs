//! One server per working directory.
//!
//! Deckjack assumes exactly one server running out of one working directory at
//! a time. The lock file is how that is enforced across restarts, unclean
//! shutdowns, and the same directory being carried to another machine.
//!
//! It records which node holds the directory, the process id, when that
//! process started and a heartbeat it updates while running. Four situations
//! can be told apart from that, and they are the four startup rules:
//!
//! | On disk                      | Outcome                                    |
//! |------------------------------|--------------------------------------------|
//! | no lock                      | take it                                     |
//! | this node, process alive     | refuse: a server is already running here    |
//! | this node, process gone      | take it, and report the unclean shutdown    |
//! | another node                 | refuse, unless forced                       |
//!
//! The last one is a deliberate asymmetry. Liveness of a process on another
//! machine cannot be checked from here, so a lock from elsewhere is never
//! assumed stale however old its heartbeat is; it is shown to the operator
//! with the node and the heartbeat, and taking it over is an explicit act.
//!
//! The clock and the liveness check are supplied by the caller rather than
//! read here, so every rule above is tested without sleeping or spawning.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Where the lock lives, relative to the working directory.
///
/// It sits beside `state.db` in the hidden directory, which is never served
/// over the file API, WebDAV, search or agent tools.
pub const LOCK_PATH: &str = ".deckjack/lock.json";

/// What a lock file holds. Everything here exists to be shown to a person
/// when a start is refused; nothing in it is a secret.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LockInfo {
    /// The node that holds the directory. See [`crate::node::id`].
    pub node_id: String,
    /// The node's hostname, for display only.
    pub host_name: String,
    /// Process id of the server, meaningful only on the node that wrote it.
    pub pid: u32,
    /// Unix seconds when that server started.
    pub started_at: u64,
    /// Unix seconds, updated while the server runs. A stale heartbeat on this
    /// node means a crash; on another node it means nothing checkable.
    pub heartbeat: u64,
}

/// What the caller supplies instead of the lock module reading the world.
pub trait Env {
    /// Unix seconds now.
    fn now(&self) -> u64;
    /// This process's id.
    fn pid(&self) -> u32;
    /// This node's id.
    fn node_id(&self) -> String;
    /// This node's hostname, for display.
    fn host_name(&self) -> String;
    /// Whether a process with this id is running **on this node**. Never
    /// asked about another node's pid.
    fn is_running(&self, pid: u32) -> bool;
}

/// The real one.
pub struct OsEnv {
    node_id: String,
}

impl OsEnv {
    pub fn new(node_id: String) -> OsEnv {
        OsEnv { node_id }
    }
}

impl Env for OsEnv {
    fn now(&self) -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0)
    }

    fn pid(&self) -> u32 {
        std::process::id()
    }

    fn node_id(&self) -> String {
        self.node_id.clone()
    }

    fn host_name(&self) -> String {
        crate::node::host_name()
    }

    fn is_running(&self, pid: u32) -> bool {
        use nix::sys::signal::kill;
        use nix::unistd::Pid;
        // Signal 0 performs the permission and existence checks without
        // sending anything. `EPERM` means the process exists but belongs to
        // someone else, which still counts as running.
        match kill(Pid::from_raw(pid as i32), None) {
            Ok(()) => true,
            Err(nix::errno::Errno::EPERM) => true,
            Err(_) => false,
        }
    }
}

/// How a lock was obtained. The caller decides what to do about it; recovery
/// of a half-written state file belongs above this module, not in it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Acquired {
    /// No lock was present.
    Clean,
    /// This node held it and the process is gone: the last shutdown was
    /// unclean, so the caller should run recovery before trusting state.
    AfterUncleanShutdown { previous: LockInfo },
    /// Another node held it and the caller passed `force`.
    Forced { previous: LockInfo },
}

/// Why a start was refused. Both carry the lock so the caller can show it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    /// A server is already running on this node out of this directory.
    RunningHere { existing: LockInfo },
    /// Another node holds it, and liveness there cannot be checked.
    HeldByOtherNode { existing: LockInfo },
}

#[derive(Debug, Error)]
pub enum LockError {
    #[error("a Deckjack server is already running here (pid {})", .0.pid)]
    RunningHere(LockInfo),

    #[error(
        "working directory is held by {} (node {}), last heartbeat {}s ago; \
         liveness cannot be checked remotely, so pass --force to take over",
        .existing.host_name, .existing.node_id, .age
    )]
    HeldByOtherNode { existing: LockInfo, age: u64 },

    #[error("lock file at {path} is unreadable: {source}")]
    Unreadable {
        path: PathBuf,
        #[source]
        source: io::Error,
    },

    #[error("lock file at {path} is corrupt: {source}")]
    Corrupt {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },

    #[error(transparent)]
    Io(#[from] io::Error),
}

impl LockError {
    /// The refusal behind this error, if it is one.
    pub fn refusal(&self) -> Option<Refusal> {
        match self {
            LockError::RunningHere(existing) => Some(Refusal::RunningHere {
                existing: existing.clone(),
            }),
            LockError::HeldByOtherNode { existing, .. } => Some(Refusal::HeldByOtherNode {
                existing: existing.clone(),
            }),
            _ => None,
        }
    }
}

/// A held lock. Dropping it releases the directory, so a panic or an early
/// return cannot leave a lock behind that the next start would have to
/// diagnose as an unclean shutdown.
#[derive(Debug)]
pub struct Lock {
    path: PathBuf,
    info: LockInfo,
    released: bool,
}

impl Lock {
    pub fn info(&self) -> &LockInfo {
        &self.info
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Record that this server is still alive.
    pub fn heartbeat(&mut self, env: &dyn Env) -> io::Result<()> {
        self.info.heartbeat = env.now();
        write_atomic(&self.path, &self.info)
    }

    /// Release the directory. Called by `Drop`; explicit when the caller wants
    /// to see the error.
    pub fn release(&mut self) -> io::Result<()> {
        if self.released {
            return Ok(());
        }
        self.released = true;
        match fs::remove_file(&self.path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e),
        }
    }
}

impl Drop for Lock {
    fn drop(&mut self) {
        let _ = self.release();
    }
}

/// Take the lock on a working directory, applying the four startup rules.
pub fn acquire(
    working_dir: &Path,
    env: &dyn Env,
    force: bool,
) -> Result<(Lock, Acquired), LockError> {
    let path = working_dir.join(LOCK_PATH);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }

    let existing = read(&path)?;

    let outcome = match existing {
        None => Acquired::Clean,
        Some(existing) if existing.node_id == env.node_id() => {
            if env.is_running(existing.pid) {
                return Err(LockError::RunningHere(existing));
            }
            Acquired::AfterUncleanShutdown { previous: existing }
        }
        Some(existing) => {
            if !force {
                let age = env.now().saturating_sub(existing.heartbeat);
                return Err(LockError::HeldByOtherNode { existing, age });
            }
            Acquired::Forced { previous: existing }
        }
    };

    let now = env.now();
    let info = LockInfo {
        node_id: env.node_id(),
        host_name: env.host_name(),
        pid: env.pid(),
        started_at: now,
        heartbeat: now,
    };
    write_atomic(&path, &info)?;

    Ok((
        Lock {
            path,
            info,
            released: false,
        },
        outcome,
    ))
}

/// Read a lock file without taking it, for `status`-style commands.
pub fn read(path: &Path) -> Result<Option<LockInfo>, LockError> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(LockError::Unreadable {
                path: path.to_path_buf(),
                source,
            });
        }
    };
    serde_json::from_str(&text)
        .map(Some)
        .map_err(|source| LockError::Corrupt {
            path: path.to_path_buf(),
            source,
        })
}

/// Write through a temporary file in the same directory and rename over the
/// target, so a crash mid-write leaves the old lock rather than a truncated
/// one that the next start would call corrupt.
fn write_atomic(path: &Path, info: &LockInfo) -> io::Result<()> {
    let dir = path.parent().unwrap_or(Path::new("."));
    let mut tmp = tempfile::NamedTempFile::new_in(dir)?;
    serde_json::to_writer_pretty(&mut tmp, info)?;
    use std::io::Write as _;
    tmp.as_file_mut().write_all(b"\n")?;
    tmp.as_file().sync_all()?;
    tmp.persist(path).map_err(|e| e.error)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    struct TestEnv {
        now: RefCell<u64>,
        pid: u32,
        node_id: String,
        running: RefCell<Vec<u32>>,
    }

    impl TestEnv {
        fn new(node_id: &str, pid: u32) -> TestEnv {
            TestEnv {
                now: RefCell::new(1_000),
                pid,
                node_id: node_id.to_string(),
                running: RefCell::new(vec![pid]),
            }
        }
        fn kill(&self, pid: u32) {
            self.running.borrow_mut().retain(|p| *p != pid);
        }
        fn advance(&self, secs: u64) {
            *self.now.borrow_mut() += secs;
        }
    }

    impl Env for TestEnv {
        fn now(&self) -> u64 {
            *self.now.borrow()
        }
        fn pid(&self) -> u32 {
            self.pid
        }
        fn node_id(&self) -> String {
            self.node_id.clone()
        }
        fn host_name(&self) -> String {
            format!("host-{}", self.node_id)
        }
        fn is_running(&self, pid: u32) -> bool {
            self.running.borrow().contains(&pid)
        }
    }

    #[test]
    fn no_lock_is_taken_cleanly() {
        let dir = tempfile::tempdir().unwrap();
        let env = TestEnv::new("node-a", 100);
        let (lock, how) = acquire(dir.path(), &env, false).unwrap();
        assert_eq!(how, Acquired::Clean);
        assert_eq!(lock.info().pid, 100);
        assert!(lock.path().exists());
    }

    #[test]
    fn second_server_on_this_node_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let env = TestEnv::new("node-a", 100);
        let (_held, _) = acquire(dir.path(), &env, false).unwrap();

        // A different process on the same node, while the first still runs.
        let other = TestEnv {
            now: RefCell::new(1_000),
            pid: 101,
            node_id: "node-a".to_string(),
            running: RefCell::new(vec![100, 101]),
        };
        let err = acquire(dir.path(), &other, false).unwrap_err();
        assert!(matches!(
            err.refusal(),
            Some(Refusal::RunningHere { existing }) if existing.pid == 100
        ));
    }

    #[test]
    fn dead_process_on_this_node_is_an_unclean_shutdown() {
        let dir = tempfile::tempdir().unwrap();
        let env = TestEnv::new("node-a", 100);
        let (lock, _) = acquire(dir.path(), &env, false).unwrap();
        // Leak it: the file stays, as it would after a crash.
        std::mem::forget(lock);
        env.kill(100);

        let restarted = TestEnv::new("node-a", 102);
        let (_lock, how) = acquire(dir.path(), &restarted, false).unwrap();
        match how {
            Acquired::AfterUncleanShutdown { previous } => assert_eq!(previous.pid, 100),
            other => panic!("expected unclean shutdown, got {other:?}"),
        }
    }

    #[test]
    fn another_node_is_refused_however_stale_the_heartbeat() {
        let dir = tempfile::tempdir().unwrap();
        let a = TestEnv::new("node-a", 100);
        let (lock, _) = acquire(dir.path(), &a, false).unwrap();
        std::mem::forget(lock);

        let b = TestEnv::new("node-b", 200);
        b.advance(60 * 60 * 24 * 30);

        let err = acquire(dir.path(), &b, false).unwrap_err();
        match err {
            LockError::HeldByOtherNode { existing, age } => {
                assert_eq!(existing.node_id, "node-a");
                assert_eq!(age, 60 * 60 * 24 * 30);
            }
            other => panic!("expected refusal, got {other:?}"),
        }
    }

    #[test]
    fn force_takes_over_another_nodes_lock() {
        let dir = tempfile::tempdir().unwrap();
        let a = TestEnv::new("node-a", 100);
        let (lock, _) = acquire(dir.path(), &a, false).unwrap();
        std::mem::forget(lock);

        let b = TestEnv::new("node-b", 200);
        let (lock, how) = acquire(dir.path(), &b, true).unwrap();
        match how {
            Acquired::Forced { previous } => assert_eq!(previous.node_id, "node-a"),
            other => panic!("expected forced, got {other:?}"),
        }
        assert_eq!(lock.info().node_id, "node-b");
    }

    #[test]
    fn dropping_the_lock_releases_the_directory() {
        let dir = tempfile::tempdir().unwrap();
        let env = TestEnv::new("node-a", 100);
        let path = {
            let (lock, _) = acquire(dir.path(), &env, false).unwrap();
            lock.path().to_path_buf()
        };
        assert!(!path.exists(), "drop should have removed the lock");

        // And the next start is clean, not an unclean shutdown.
        let (_lock, how) = acquire(dir.path(), &env, false).unwrap();
        assert_eq!(how, Acquired::Clean);
    }

    #[test]
    fn heartbeat_is_written_through() {
        let dir = tempfile::tempdir().unwrap();
        let env = TestEnv::new("node-a", 100);
        let (mut lock, _) = acquire(dir.path(), &env, false).unwrap();
        let first = lock.info().heartbeat;

        env.advance(30);
        lock.heartbeat(&env).unwrap();

        let on_disk = read(lock.path()).unwrap().unwrap();
        assert_eq!(on_disk.heartbeat, first + 30);
        assert_eq!(on_disk.started_at, first, "start time must not move");
    }

    #[test]
    fn corrupt_lock_is_reported_not_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(LOCK_PATH);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "{not json").unwrap();

        let env = TestEnv::new("node-a", 100);
        let err = acquire(dir.path(), &env, false).unwrap_err();
        assert!(matches!(err, LockError::Corrupt { .. }), "got {err:?}");
    }

    #[test]
    fn lock_lives_in_the_hidden_directory() {
        let dir = tempfile::tempdir().unwrap();
        let env = TestEnv::new("node-a", 100);
        let (lock, _) = acquire(dir.path(), &env, false).unwrap();
        let rel = lock.path().strip_prefix(dir.path()).unwrap();
        assert_eq!(rel, Path::new(LOCK_PATH));
        assert!(rel.starts_with(".deckjack"));
    }
}
