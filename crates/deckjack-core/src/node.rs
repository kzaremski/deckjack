//! This machine's identity.
//!
//! A node id is generated once and kept in the install directory, which never
//! travels, rather than derived from something like `/etc/machine-id`. Two
//! reasons: the working directory is carried between machines and may be
//! synced, so a stable hardware identifier written into it would follow the
//! files around; and a node that is restored from a backup onto different
//! hardware should keep its identity rather than silently become a new node.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;

// The one place in the crate that touches the OS random source. It lives in
// `auth::code` because that is where it was first needed; if a third caller
// appears it should move somewhere neutral rather than be duplicated.
use crate::auth::code::os_random;

/// Where the id lives, relative to the install directory.
pub const ID_FILE: &str = "keys/node-id";

/// Bytes of randomness behind a node id. 16 is far more than enough to keep
/// the handful of machines one person owns apart, and keeps the id short
/// enough to read out of a lock file by eye.
const ID_BYTES: usize = 16;

/// The install directory: `$DECKJACK_HOME`, else `~/.deckjack`.
///
/// This holds binaries, caches and this node's identity, all built for this
/// machine. It is not the working directory and is never synced.
pub fn install_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("DECKJACK_HOME") {
        return PathBuf::from(dir);
    }
    match std::env::var_os("HOME") {
        Some(home) => PathBuf::from(home).join(".deckjack"),
        None => PathBuf::from(".deckjack"),
    }
}

/// Read this node's id, generating and storing one the first time.
///
/// The file is created with `create_new`, so two processes racing on first run
/// cannot both win: the loser sees `AlreadyExists` and reads what the winner
/// wrote.
pub fn id(install_dir: &Path) -> io::Result<String> {
    let path = install_dir.join(ID_FILE);

    match fs::read_to_string(&path) {
        Ok(existing) => {
            let trimmed = existing.trim();
            if !trimmed.is_empty() {
                return Ok(trimmed.to_string());
            }
            // An empty file is a half-finished write from a crash, not an id.
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }

    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }

    let mut bytes = [0u8; ID_BYTES];
    os_random(&mut bytes).map_err(|e| io::Error::other(e.to_string()))?;
    let generated = URL_SAFE_NO_PAD.encode(bytes);

    match fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
    {
        Ok(mut file) => {
            use std::io::Write as _;
            writeln!(file, "{generated}")?;
            Ok(generated)
        }
        // Lost the race, or an empty file was already there. Either way the
        // other writer's id is authoritative.
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
            let existing = fs::read_to_string(&path)?;
            let trimmed = existing.trim();
            if trimmed.is_empty() {
                fs::write(&path, format!("{generated}\n"))?;
                Ok(generated)
            } else {
                Ok(trimmed.to_string())
            }
        }
        Err(e) => Err(e),
    }
}

/// This machine's hostname, for showing a person which node holds a lock.
/// Never used to decide anything; [`id`] is the identity.
pub fn host_name() -> String {
    nix::unistd::gethostname()
        .ok()
        .and_then(|n| n.into_string().ok())
        .unwrap_or_else(|| "unknown".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn id_is_generated_once_and_then_reused() {
        let dir = tempfile::tempdir().unwrap();
        let first = id(dir.path()).unwrap();
        let second = id(dir.path()).unwrap();
        assert_eq!(first, second);
        assert!(!first.is_empty());
    }

    #[test]
    fn different_install_dirs_are_different_nodes() {
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        assert_ne!(id(a.path()).unwrap(), id(b.path()).unwrap());
    }

    #[test]
    fn empty_id_file_is_replaced_rather_than_returned() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(ID_FILE);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "   \n").unwrap();

        let generated = id(dir.path()).unwrap();
        assert!(!generated.is_empty());
        assert_eq!(id(dir.path()).unwrap(), generated);
    }

    #[test]
    fn install_dir_follows_deckjack_home() {
        // Not asserted against the environment, which tests must not mutate:
        // this only checks the shape of the fallback.
        let dir = install_dir();
        assert!(dir.is_absolute() || dir == *Path::new(".deckjack"));
    }
}
