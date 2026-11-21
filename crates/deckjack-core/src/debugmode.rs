//! `deckjack --debug`: a node set up to be driven by tools, not people.
//!
//! What it trades, said plainly because every item is a cost:
//!
//! - **A session nobody signed in for.** At startup the node mints one
//!   session bound to `127.0.0.1` and writes its cookie to
//!   `<install>/debug-session.cookie`, mode 0600. A script, a test harness or
//!   an agent working on Deckjack can then call the node without spending
//!   the operator's one-time code, which a restart rotates. Anyone who can
//!   read that file is signed in. It is never persisted to `state.db`, so it
//!   dies with the process, and a lockdown revokes it like any other session.
//! - **Loopback only.** `--debug` refuses any listen address that is not
//!   loopback. That is what keeps the session above from being a remote
//!   credential — with one hole worth knowing: `tailscale serve` and any other
//!   local reverse proxy forward to loopback, and the session is bound to
//!   `127.0.0.1`, so a debug node on a port such a proxy exposes is reachable
//!   through it by whoever holds the cookie. `cargo xtask dev` uses its own
//!   port, which nothing exposes.
//! - **Verbose logs.** `debug` level by default rather than `info`, unless
//!   `RUST_LOG` says otherwise. Slower and noisier; that is the point.
//!
//! A normal start removes a leftover cookie file, so a file on disk never
//! outlives the mode that made it.

use std::io::{self, Write as _};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};

use thiserror::Error;

use crate::auth::Event;
use crate::auth::session::{COOKIE_NAME, SessionId};
use crate::auth::state::{AuthState, Now, Refusal};

/// Where the debug session's cookie is written, inside the install directory.
pub const COOKIE_FILE: &str = "debug-session.cookie";

/// The address the debug session is bound to.
pub const DEBUG_IP: IpAddr = IpAddr::V4(Ipv4Addr::LOCALHOST);

/// The default log filter in debug mode.
pub const LOG_FILTER: &str = "deckjack=debug,deckjack_core=debug";

#[derive(Debug, Error)]
pub enum DebugError {
    #[error(
        "--debug only listens on loopback, and {0} is not: debug mode mints a session \
         without a sign-in, which must not be reachable from another machine. Use \
         --listen 127.0.0.1:PORT"
    )]
    NotLoopback(SocketAddr),

    #[error("the debug session was refused: {0:?}")]
    Refused(Refusal),

    #[error("cannot generate the debug session: {0}")]
    Random(String),

    #[error("cannot write {path}: {source}")]
    Write {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
}

/// Refuse a debug node on anything but loopback.
pub fn check_listen(listen: SocketAddr) -> Result<(), DebugError> {
    if listen.ip().is_loopback() {
        Ok(())
    } else {
        Err(DebugError::NotLoopback(listen))
    }
}

/// Mint the debug session in the node's own state machine.
///
/// Through [`AuthState::admit`], the path a passkey sign-in takes, so the
/// session cap and the address refusals apply to it like to any other. Any
/// session that admitting it evicted is returned for the caller to log.
pub fn mint(state: &mut AuthState, now: Now) -> Result<(SessionId, Vec<Event>), DebugError> {
    let id = SessionId::generate().map_err(|e| DebugError::Random(e.to_string()))?;
    let mut events = Vec::new();
    state
        .admit(DEBUG_IP, &id, now, &mut events)
        .map_err(DebugError::Refused)?;
    Ok((id, events))
}

/// Write `name=value`, ready for a `Cookie:` header, readable by the owner
/// only. Created with that mode rather than chmodded after, so there is no
/// moment when it is readable by anyone else.
pub fn write_cookie(install_dir: &Path, id: &SessionId) -> Result<PathBuf, DebugError> {
    let path = install_dir.join(COOKIE_FILE);
    let write = || -> io::Result<()> {
        // Removed first: `mode` applies only when a file is created, so an
        // existing file with looser permissions would keep them.
        match std::fs::remove_file(&path) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(0o600);
        }
        let mut file = options.open(&path)?;
        writeln!(file, "{COOKIE_NAME}={}", id.encode())?;
        file.sync_all()
    };
    write().map_err(|source| DebugError::Write {
        path: path.clone(),
        source,
    })?;
    Ok(path)
}

/// A normal start removes a debug cookie left by an earlier debug start.
pub fn remove_cookie(install_dir: &Path) -> io::Result<()> {
    match std::fs::remove_file(install_dir.join(COOKIE_FILE)) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}
