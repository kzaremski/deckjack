//! Working-directory addresses, confined to `Photos/`.
//!
//! The app's grant is `Photos/` and nothing else (app.toml). Landlock
//! enforces that when the app runs confined; this module enforces it in the
//! app too, so an unconfined debug run behaves the same and an agent's bad
//! path gets a sentence instead of a permission error.

use std::path::{Path, PathBuf};

/// The one folder this app reads and writes, relative to the working
/// directory.
pub const ROOT: &str = "Photos";

#[derive(Debug, thiserror::Error, PartialEq)]
pub enum PathError {
    #[error("{0:?} is not under Photos/, the only folder this app can open")]
    OutsideRoot(String),
    #[error("{0:?} is not a plain path: no '..', '.' or empty parts")]
    NotPlain(String),
}

/// An address the app may touch: its path on disk, and its canonical
/// working-directory form (`Photos/a/b.jpg`) for answers and sidecars.
#[derive(Debug, Clone, PartialEq)]
pub struct Address {
    pub disk: PathBuf,
    pub relative: String,
}

/// Resolve a working-directory address: `/Photos/x.jpg`, `Photos/x.jpg` or
/// `node:/Photos/x.jpg` (the shell's `?path=` form). `Photos` itself is
/// allowed, so a folder listing can start there.
pub fn resolve(working: &Path, address: &str) -> Result<Address, PathError> {
    // `node:/a/b`: drop a node prefix, which names where, not what.
    let path = match address.split_once(':') {
        Some((node, rest)) if !node.contains('/') && rest.starts_with('/') => rest,
        _ => address,
    };
    let trimmed = path.trim_start_matches('/');
    let mut parts = Vec::new();
    for part in trimmed.split('/') {
        if part.is_empty() && parts.is_empty() {
            continue;
        }
        if part.is_empty() || part == "." || part == ".." || part.contains('\0') {
            return Err(PathError::NotPlain(address.to_string()));
        }
        parts.push(part);
    }
    if parts.first() != Some(&ROOT) {
        return Err(PathError::OutsideRoot(address.to_string()));
    }
    let relative = parts.join("/");
    let disk = working.join(&relative);
    Ok(Address { disk, relative })
}

/// A symlink inside `Photos/` that points out of it is refused: the address
/// says Photos, the bytes would come from elsewhere. Checked on the
/// canonical path of something that exists.
pub fn contained(working: &Path, address: &Address) -> Result<(), PathError> {
    let root = working.join(ROOT).canonicalize();
    let real = address.disk.canonicalize();
    match (root, real) {
        (Ok(root), Ok(real)) if !real.starts_with(&root) => {
            Err(PathError::OutsideRoot(address.relative.clone()))
        }
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_spelling_of_a_photos_path_resolves_to_one_address() {
        let w = Path::new("/w");
        for a in [
            "Photos/a/b.jpg",
            "/Photos/a/b.jpg",
            "laptop:/Photos/a/b.jpg",
        ] {
            let r = resolve(w, a).unwrap();
            assert_eq!(r.relative, "Photos/a/b.jpg", "{a}");
            assert_eq!(r.disk, Path::new("/w/Photos/a/b.jpg"));
        }
        assert_eq!(resolve(w, "/Photos").unwrap().relative, "Photos");
    }

    #[test]
    fn anything_outside_photos_or_climbing_is_refused() {
        let w = Path::new("/w");
        assert!(matches!(
            resolve(w, "Notes/x.md"),
            Err(PathError::OutsideRoot(_))
        ));
        assert!(matches!(resolve(w, "/"), Err(PathError::OutsideRoot(_))));
        assert!(matches!(
            resolve(w, "Photoshop/x.jpg"),
            Err(PathError::OutsideRoot(_))
        ));
        assert!(matches!(
            resolve(w, "Photos/../.deckjack/x"),
            Err(PathError::NotPlain(_))
        ));
        assert!(matches!(
            resolve(w, "Photos/./x.jpg"),
            Err(PathError::NotPlain(_))
        ));
        assert!(matches!(
            resolve(w, "Photos//x.jpg"),
            Err(PathError::NotPlain(_))
        ));
    }

    #[test]
    fn a_symlink_out_of_photos_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("Photos")).unwrap();
        std::fs::write(dir.path().join("secret.jpg"), b"x").unwrap();
        std::fs::write(dir.path().join("Photos/ok.jpg"), b"x").unwrap();
        std::os::unix::fs::symlink(
            dir.path().join("secret.jpg"),
            dir.path().join("Photos/link.jpg"),
        )
        .unwrap();
        let ok = resolve(dir.path(), "Photos/ok.jpg").unwrap();
        assert!(contained(dir.path(), &ok).is_ok());
        let link = resolve(dir.path(), "Photos/link.jpg").unwrap();
        assert!(matches!(
            contained(dir.path(), &link),
            Err(PathError::OutsideRoot(_))
        ));
    }
}
