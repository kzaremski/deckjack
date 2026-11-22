//! Working-directory addresses, the folders Office may use, and writing a
//! new file without ever replacing one.
//!
//! The app is launched with the working directory as its cwd and Landlock
//! grants it the manifest's folders (`Documents/`, `Downloads/`) and nothing
//! else, so the kernel already refuses everything outside them. This module
//! exists so the refusal is a sentence naming the folder to add, not an
//! `EACCES` from three calls down; and because confinement is off in
//! development, where this is the only check.

use std::io::Write as _;
use std::path::{Component, Path, PathBuf};

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum PathError {
    #[error("a path is needed")]
    Empty,
    #[error("{0:?} is not a working-directory path: no '..', '.', or empty segments")]
    Bad(String),
    #[error(
        "Office can only open files in {allowed}. {path:?} is in {folder}; move it into one of \
         those, or add {folder_name:?} to Office's working_dir in its app.toml and approve it again"
    )]
    Outside {
        path: String,
        folder: String,
        folder_name: String,
        allowed: String,
    },
    #[error("{0:?} leads outside Office's folders through a link")]
    Escapes(String),
    #[error("there is no file at {0:?}")]
    Missing(String),
    #[error("{0:?} is a folder, not a file")]
    NotAFile(String),
    #[error("{0:?} already exists; Office never replaces a file")]
    Exists(String),
    #[error("{doing} {path:?}: {reason}")]
    Io {
        doing: &'static str,
        path: String,
        reason: String,
    },
}

/// The folders Office may open: the installed `app.toml`'s `working_dir`
/// when there is an installed copy that is Office's, else the copy compiled
/// in (a development run with no install beside the binary).
///
/// The installed copy is the one the core reads, the operator approves and
/// Landlock is built from; reading the compiled one instead would keep
/// refusing a folder the operator added there and approved.
pub fn folder_names(installed: Option<&str>, compiled: &str) -> anyhow::Result<Vec<String>> {
    let path = Path::new(deckjack_protocol::MANIFEST_FILE);
    let compiled = deckjack_protocol::Manifest::parse(path, compiled)?;
    let manifest = installed
        .and_then(|text| deckjack_protocol::Manifest::parse(path, text).ok())
        .filter(|m| m.app.name == compiled.app.name)
        .unwrap_or(compiled);
    Ok(manifest
        .needs
        .working_dir
        .iter()
        .map(|d| d.path.clone())
        .collect())
}

/// The folders this app was granted, under the working directory.
#[derive(Debug, Clone)]
pub struct Folders {
    pub root: PathBuf,
    pub names: Vec<String>,
}

/// A path inside one of the folders.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolved {
    pub path: PathBuf,
    /// The working-directory address, `/Documents/Report.docx`.
    pub address: String,
}

impl Folders {
    pub fn new(root: PathBuf, names: Vec<String>) -> Folders {
        Folders { root, names }
    }

    fn allowed(&self) -> String {
        let list: Vec<String> = self.names.iter().map(|n| format!("{n}/")).collect();
        match list.len() {
            0 => "no folder at all".to_string(),
            1 => list[0].clone(),
            _ => format!(
                "{} and {}",
                list[..list.len() - 1].join(", "),
                list[list.len() - 1]
            ),
        }
    }

    /// The segments of an address, checked, the way the core's own paths
    /// are: a `node:` qualifier (what `files.list` puts on every address)
    /// is dropped, empty and `.` segments are no-ops, `..` is refused.
    fn segments(address: &str) -> Result<Vec<String>, PathError> {
        let trimmed = address.trim();
        if trimmed.contains('\0') {
            return Err(PathError::Bad(trimmed.to_string()));
        }
        let head = trimmed.split('/').next().unwrap_or("");
        let body = match head.find(':') {
            Some(colon) => &trimmed[colon + 1..],
            None => trimmed,
        };
        let mut out = Vec::new();
        for segment in body.split('/') {
            match segment {
                "" | "." => {}
                ".." => return Err(PathError::Bad(trimmed.to_string())),
                name => out.push(name.to_string()),
            }
        }
        if out.is_empty() {
            return Err(PathError::Empty);
        }
        Ok(out)
    }

    /// Resolve an address to a path inside a granted folder. The target need
    /// not exist; when it (or its parent) does, symlinks are followed and the
    /// result must still be inside.
    pub fn resolve(&self, address: &str) -> Result<Resolved, PathError> {
        let segments = Folders::segments(address)?;
        let first = &segments[0];
        if !self.names.iter().any(|n| n == first) || segments.len() < 2 {
            let folder = if segments.len() < 2 {
                "the top of the working directory".to_string()
            } else {
                format!("{first}/")
            };
            return Err(PathError::Outside {
                path: address.trim().to_string(),
                folder,
                folder_name: if segments.len() < 2 {
                    ".".to_string()
                } else {
                    first.clone()
                },
                allowed: self.allowed(),
            });
        }
        let path = segments.iter().fold(self.root.clone(), |p, s| p.join(s));
        let address = format!("/{}", segments.join("/"));
        // Followed links must stay inside the folder they started in.
        let base = self.root.join(first);
        let check = if path.exists() {
            Some(path.clone())
        } else {
            path.parent().filter(|p| p.exists()).map(Path::to_path_buf)
        };
        if let Some(check) = check {
            let real = check.canonicalize().map_err(|e| PathError::Io {
                doing: "cannot resolve",
                path: address.clone(),
                reason: e.to_string(),
            })?;
            let real_base = base.canonicalize().unwrap_or(base);
            if !real.starts_with(&real_base) {
                return Err(PathError::Escapes(address));
            }
        }
        Ok(Resolved { path, address })
    }

    /// Resolve an address that must be an existing file.
    pub fn file(&self, address: &str) -> Result<Resolved, PathError> {
        let resolved = self.resolve(address)?;
        match std::fs::metadata(&resolved.path) {
            Ok(m) if m.is_file() => Ok(resolved),
            Ok(_) => Err(PathError::NotAFile(resolved.address)),
            Err(_) => Err(PathError::Missing(resolved.address)),
        }
    }

    /// The address of a path under the root.
    pub fn address_of(&self, path: &Path) -> String {
        let rel = path.strip_prefix(&self.root).unwrap_or(path);
        let mut out = String::new();
        for c in rel.components() {
            if let Component::Normal(s) = c {
                out.push('/');
                out.push_str(&s.to_string_lossy());
            }
        }
        out
    }
}

/// `Report.docx` → (`Report`, `.docx`).
pub fn split_name(name: &str) -> (&str, &str) {
    match name.rfind('.') {
        Some(dot) if dot > 0 => (&name[..dot], &name[dot..]),
        _ => (name, ""),
    }
}

/// The stem without a ` (commented)` or ` (commented N)` suffix this app
/// added, so commenting on a commented copy does not stack suffixes.
pub fn base_stem(stem: &str) -> &str {
    if let Some(rest) = stem.strip_suffix(')')
        && let Some(at) = rest.rfind(" (commented")
    {
        let tail = &rest[at + " (commented".len()..];
        if tail.is_empty()
            || (tail.starts_with(' ')
                && tail[1..].chars().all(|c| c.is_ascii_digit())
                && tail.len() > 1)
        {
            return &stem[..at];
        }
    }
    stem
}

/// Candidate names for a new sibling: `Report (commented).docx`, then
/// `Report (commented 2).docx`, …
pub fn commented_names(name: &str) -> impl Iterator<Item = String> + '_ {
    let (stem, ext) = split_name(name);
    let stem = base_stem(stem);
    (1..).map(move |n| {
        if n == 1 {
            format!("{stem} (commented){ext}")
        } else {
            format!("{stem} (commented {n}){ext}")
        }
    })
}

/// Candidate names for an export: `Report.pdf`, then `Report (2).pdf`, …
pub fn export_names<'a>(name: &'a str, ext: &'a str) -> impl Iterator<Item = String> + 'a {
    let (stem, _) = split_name(name);
    (1..).map(move |n| {
        if n == 1 {
            format!("{stem}.{ext}")
        } else {
            format!("{stem} ({n}).{ext}")
        }
    })
}

/// Write `bytes` to a new file, refusing to replace anything.
///
/// `create_new` is the whole guarantee: the kernel refuses `O_EXCL` on an
/// existing name, so two writers racing for one name cannot both win and
/// nothing that exists is ever truncated. A write that fails part-way removes
/// the file it created.
pub fn write_new(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)?;
    let written = file.write_all(bytes).and_then(|_| file.sync_all());
    if let Err(e) = written {
        drop(file);
        let _ = std::fs::remove_file(path);
        return Err(e);
    }
    Ok(())
}

/// Write to the first free name in `names` inside `dir`. Returns the path.
pub fn write_first_free(
    dir: &Path,
    names: impl Iterator<Item = String>,
    bytes: &[u8],
) -> std::io::Result<PathBuf> {
    for name in names.take(1000) {
        let path = dir.join(&name);
        match write_new(&path, bytes) {
            Ok(()) => return Ok(path),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
    Err(std::io::Error::other("no free name after 1000 tries"))
}

/// Give `prepared` the name `target`, which must not exist. Same folder only.
///
/// A hard link then an unlink, so the name is taken atomically and only if
/// free; where the filesystem has no hard links, a checked rename (with the
/// small race that implies, stated rather than hidden).
pub fn take_name(prepared: &Path, target: &Path) -> std::io::Result<()> {
    match std::fs::hard_link(prepared, target) {
        Ok(()) => std::fs::remove_file(prepared),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Err(e),
        Err(_) => {
            if target.exists() {
                return Err(std::io::Error::from(std::io::ErrorKind::AlreadyExists));
            }
            std::fs::rename(prepared, target)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn folders() -> (tempfile::TempDir, Folders) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("Documents/sub")).unwrap();
        std::fs::create_dir_all(dir.path().join("Downloads")).unwrap();
        std::fs::create_dir_all(dir.path().join("Projects")).unwrap();
        std::fs::write(dir.path().join("Documents/a.docx"), b"x").unwrap();
        let f = Folders::new(
            dir.path().to_path_buf(),
            vec!["Documents".into(), "Downloads".into()],
        );
        (dir, f)
    }

    #[test]
    fn an_address_resolves_inside_a_granted_folder() {
        let (dir, f) = folders();
        let r = f.file("/Documents/a.docx").unwrap();
        assert_eq!(r.path, dir.path().join("Documents/a.docx"));
        assert_eq!(r.address, "/Documents/a.docx");
        assert_eq!(
            f.resolve("Downloads/new.pdf").unwrap().address,
            "/Downloads/new.pdf"
        );
        // As `files.list` qualifies it, and with the no-op segments the core
        // ignores too.
        assert_eq!(f.file("r620:/Documents/a.docx").unwrap().path, r.path);
        assert_eq!(f.file("/Documents/./a.docx").unwrap().path, r.path);
    }

    /// The refusal tells the operator to add a folder to the installed
    /// app.toml and approve again; that only works if the installed copy is
    /// the one read. A foreign or broken copy falls back to the compiled one.
    #[test]
    fn the_folders_come_from_the_installed_manifest_the_refusal_points_at() {
        let compiled = include_str!("../app.toml");
        let widened = compiled.replace(
            "{ path = \"Downloads\", write = true }",
            "{ path = \"Downloads\", write = true },\n  { path = \"Projects\", write = true }",
        );
        assert_ne!(widened, compiled, "the manifest's working_dir line moved");
        assert_eq!(
            folder_names(Some(&widened), compiled).unwrap(),
            ["Documents", "Downloads", "Projects"]
        );
        assert_eq!(
            folder_names(None, compiled).unwrap(),
            ["Documents", "Downloads"]
        );
        assert_eq!(
            folder_names(Some("not toml ["), compiled).unwrap(),
            ["Documents", "Downloads"]
        );
        // Another app's manifest, valid in itself (its command ids renamed
        // with it), is not Office's.
        let other = widened
            .replace("name = \"office\"", "name = \"other\"")
            .replace("\"office.", "\"other.");
        assert_eq!(
            deckjack_protocol::Manifest::parse(Path::new("app.toml"), &other)
                .unwrap()
                .needs
                .working_dir
                .len(),
            3
        );
        assert_eq!(
            folder_names(Some(&other), compiled).unwrap(),
            ["Documents", "Downloads"]
        );
    }

    #[test]
    fn a_file_outside_the_folders_is_refused_naming_the_folder_to_add() {
        let (_dir, f) = folders();
        let e = f.file("/Projects/acme/report.docx").unwrap_err();
        let text = e.to_string();
        assert!(matches!(e, PathError::Outside { .. }));
        assert!(text.contains("\"Projects\""), "{text}");
        assert!(text.contains("Documents/ and Downloads/"), "{text}");
        assert!(matches!(
            f.file("/report.docx").unwrap_err(),
            PathError::Outside { .. }
        ));
    }

    #[test]
    fn climbing_and_odd_segments_are_refused() {
        let (_dir, f) = folders();
        for bad in [
            "/Documents/../.deckjack/state.db",
            "/../x",
            "",
            "/",
            "r620:/",
        ] {
            assert!(
                matches!(
                    f.resolve(bad),
                    Err(PathError::Bad(_)) | Err(PathError::Empty)
                ),
                "{bad:?} was accepted"
            );
        }
    }

    #[test]
    fn a_link_out_of_the_folder_is_refused() {
        let (dir, f) = folders();
        std::os::unix::fs::symlink(
            dir.path().join("Projects"),
            dir.path().join("Documents/out"),
        )
        .unwrap();
        std::fs::write(dir.path().join("Projects/secret.docx"), b"x").unwrap();
        assert!(matches!(
            f.file("/Documents/out/secret.docx"),
            Err(PathError::Escapes(_))
        ));
    }

    #[test]
    fn commented_copies_do_not_stack_suffixes() {
        let names: Vec<String> = commented_names("Report.docx").take(2).collect();
        assert_eq!(
            names,
            ["Report (commented).docx", "Report (commented 2).docx"]
        );
        let again: Vec<String> = commented_names("Report (commented 2).docx")
            .take(1)
            .collect();
        assert_eq!(again, ["Report (commented).docx"]);
        assert_eq!(
            base_stem("Notes (commented by Ann)"),
            "Notes (commented by Ann)"
        );
        let exports: Vec<String> = export_names("Deck.pptx", "pdf").take(2).collect();
        assert_eq!(exports, ["Deck.pdf", "Deck (2).pdf"]);
    }

    #[test]
    fn a_new_file_never_replaces_one() {
        let dir = tempfile::tempdir().unwrap();
        let taken = dir.path().join("Report (commented).docx");
        std::fs::write(&taken, b"keep me").unwrap();
        let written = write_first_free(dir.path(), commented_names("Report.docx"), b"new").unwrap();
        assert_eq!(written, dir.path().join("Report (commented 2).docx"));
        assert_eq!(std::fs::read(&taken).unwrap(), b"keep me");
        assert_eq!(
            write_new(&taken, b"x").unwrap_err().kind(),
            std::io::ErrorKind::AlreadyExists
        );
    }

    #[test]
    fn taking_a_name_refuses_while_the_name_is_in_use() {
        let dir = tempfile::tempdir().unwrap();
        let prepared = dir.path().join("p.docx");
        let target = dir.path().join("t.docx");
        std::fs::write(&prepared, b"new").unwrap();
        std::fs::write(&target, b"original").unwrap();
        assert!(take_name(&prepared, &target).is_err());
        assert_eq!(std::fs::read(&target).unwrap(), b"original");
        std::fs::remove_file(&target).unwrap();
        take_name(&prepared, &target).unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"new");
        assert!(!prepared.exists());
    }
}
