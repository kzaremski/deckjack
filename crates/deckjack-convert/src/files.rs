//! The folders this app was granted, the names it reads and writes in them,
//! and how a finished file is put in place without ever replacing one.
//!
//! Landlock confines the process to the granted folders whatever this module
//! says; these checks exist so a caller (often an agent) is told what was
//! wrong in a sentence rather than with a bare `EACCES`, and because
//! confinement can be off in development.

use std::path::{Component, Path, PathBuf};

use serde::Serialize;

/// Files [`browse`] lists and a job accepts, by extension.
pub const MEDIA_EXTENSIONS: &[&str] = &[
    "mp4", "m4v", "mov", "mkv", "webm", "avi", "mts", "m2ts", "ts", "flv", "wmv", "3gp", "mp3",
    "m4a", "aac", "flac", "wav", "ogg", "oga", "opus", "aif", "aiff", "wma",
];

/// The most files one browse returns.
pub const BROWSE_LIMIT: usize = 5_000;
/// How deep a browse looks below a granted folder.
const BROWSE_DEPTH: usize = 6;

/// The part of a partial file's name after its job id. Hidden (a leading
/// dot) and never a name an output is published under.
pub const PARTIAL_SUFFIX: &str = ".convert-partial";

/// The folders the manifest asks for, under the working directory.
#[derive(Debug, Clone)]
pub struct Grants {
    pub root: PathBuf,
    /// Relative to `root`, with whether each may be written.
    pub dirs: Vec<(String, bool)>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PathError {
    #[error(
        "{0:?} is not a path inside {1}: give it relative to the working directory, with no \
         '..' and no hidden folders"
    )]
    Outside(String, String),
    #[error("{0:?} is not a video or audio file Convert reads (by its extension)")]
    NotMedia(String),
    #[error("{0:?} is not a folder Convert may write to; it may write to {1}")]
    NotWritable(String, String),
}

impl Grants {
    /// From the app's own manifest and the working directory.
    pub fn from_manifest(manifest_toml: &str, root: PathBuf) -> Grants {
        let dirs = deckjack_protocol::Manifest::parse(
            Path::new(deckjack_protocol::MANIFEST_FILE),
            manifest_toml,
        )
        .map(|m| {
            m.needs
                .working_dir
                .into_iter()
                .map(|d| (d.path.trim_end_matches('/').to_string(), d.write))
                .collect()
        })
        .unwrap_or_default();
        Grants { root, dirs }
    }

    /// `Videos/, Music/`, for sentences.
    pub fn names(&self) -> String {
        self.dirs
            .iter()
            .map(|(d, _)| format!("{d}/"))
            .collect::<Vec<_>>()
            .join(", ")
    }

    /// A caller's path, relative to the working directory, checked to be a
    /// plain path inside a granted folder.
    pub fn resolve(&self, rel: &str) -> Result<PathBuf, PathError> {
        let outside = || PathError::Outside(rel.to_string(), self.names());
        let trimmed = rel.trim().trim_end_matches('/');
        if trimmed.is_empty() {
            return Err(outside());
        }
        let path = Path::new(trimmed);
        for part in path.components() {
            match part {
                Component::Normal(seg) if !seg.to_string_lossy().starts_with('.') => {}
                _ => return Err(outside()),
            }
        }
        let inside = self
            .dirs
            .iter()
            .any(|(dir, _)| path.starts_with(Path::new(dir)));
        if !inside {
            return Err(outside());
        }
        Ok(self.root.join(path))
    }

    /// A media file inside a grant.
    pub fn media(&self, rel: &str) -> Result<PathBuf, PathError> {
        let path = self.resolve(rel)?;
        if !is_media(&path) {
            return Err(PathError::NotMedia(rel.to_string()));
        }
        Ok(path)
    }

    /// A folder this app may write to.
    pub fn writable_dir(&self, rel: &str) -> Result<PathBuf, PathError> {
        let path = self.resolve(rel)?;
        let rel_path = Path::new(rel.trim().trim_end_matches('/'));
        let writable = self
            .dirs
            .iter()
            .any(|(dir, write)| *write && rel_path.starts_with(Path::new(dir)));
        if !writable {
            let names = self
                .dirs
                .iter()
                .filter(|(_, w)| *w)
                .map(|(d, _)| format!("{d}/"))
                .collect::<Vec<_>>()
                .join(", ");
            return Err(PathError::NotWritable(rel.to_string(), names));
        }
        Ok(path)
    }

    /// An absolute path back to the working-directory-relative form callers
    /// use.
    pub fn relative(&self, path: &Path) -> String {
        path.strip_prefix(&self.root)
            .unwrap_or(path)
            .to_string_lossy()
            .into_owned()
    }
}

pub fn is_media(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| MEDIA_EXTENSIONS.contains(&e.to_ascii_lowercase().as_str()))
}

/// One file [`browse`] found.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Entry {
    /// Relative to the working directory: what every command takes.
    pub path: String,
    pub size: u64,
    /// Seconds since the epoch.
    pub modified: u64,
}

/// Media files under `dir` (relative to the working directory), or under
/// every granted folder, newest first. Hidden files and folders are skipped,
/// which also skips this app's own partial files. Symlinks are not followed.
pub fn browse(grants: &Grants, dir: Option<&str>) -> Result<(Vec<Entry>, bool), PathError> {
    let roots: Vec<PathBuf> = match dir {
        Some(d) => vec![grants.resolve(d)?],
        None => grants
            .dirs
            .iter()
            .map(|(d, _)| grants.root.join(d))
            .collect(),
    };
    let mut found = Vec::new();
    let mut truncated = false;
    for root in roots {
        let mut stack = vec![(root, 0usize)];
        while let Some((dir, depth)) = stack.pop() {
            let Ok(read) = std::fs::read_dir(&dir) else {
                continue;
            };
            for entry in read.flatten() {
                let name = entry.file_name();
                if name.to_string_lossy().starts_with('.') {
                    continue;
                }
                let Ok(kind) = entry.file_type() else {
                    continue;
                };
                let path = entry.path();
                if kind.is_dir() && depth < BROWSE_DEPTH {
                    stack.push((path, depth + 1));
                } else if kind.is_file() && is_media(&path) {
                    if found.len() >= BROWSE_LIMIT {
                        truncated = true;
                        continue;
                    }
                    let meta = entry.metadata().ok();
                    found.push(Entry {
                        path: grants.relative(&path),
                        size: meta.as_ref().map_or(0, |m| m.len()),
                        modified: meta
                            .and_then(|m| m.modified().ok())
                            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                            .map_or(0, |d| d.as_secs()),
                    });
                }
            }
        }
    }
    found.sort_by(|a, b| b.modified.cmp(&a.modified).then(a.path.cmp(&b.path)));
    Ok((found, truncated))
}

/// The first free output name: `clip (Share).mp4`, then `clip (Share) 2.mp4`
/// and on, the "copy 2" rule Files uses. `taken` says whether a name is in
/// use; the publish is still exclusive, because a name free here can be
/// taken a moment later.
pub fn output_name(stem: &str, suffix: &str, ext: &str, taken: impl Fn(&str) -> bool) -> String {
    let first = format!("{stem} ({suffix}).{ext}");
    if !taken(&first) {
        return first;
    }
    let mut n: u32 = 2;
    loop {
        let candidate = format!("{stem} ({suffix}) {n}.{ext}");
        if !taken(&candidate) {
            return candidate;
        }
        n = n.saturating_add(1);
    }
}

/// The hidden name a job writes under until ffmpeg exits 0. It carries the
/// job's id, so a restart can remove exactly the partials of jobs it knows
/// were interrupted, and nothing else.
pub fn partial_name(job: &str) -> String {
    format!(".{job}{PARTIAL_SUFFIX}")
}

/// Give a finished partial file its public name in `dir`, never replacing a
/// file.
///
/// `hard_link` fails with `AlreadyExists` instead of replacing, which a
/// rename does not, so the check-then-act race between two writers is closed
/// by the kernel (Video's export does the same). Same directory, so
/// Landlock's cross-directory `REFER` right is not needed. On a filesystem
/// without hard links it copies to a name opened `create_new`.
pub fn publish(
    dir: &Path,
    partial: &Path,
    stem: &str,
    suffix: &str,
    ext: &str,
) -> std::io::Result<PathBuf> {
    publish_with(dir, partial, stem, suffix, ext, |from, to| {
        std::fs::hard_link(from, to)
    })
}

fn publish_with(
    dir: &Path,
    partial: &Path,
    stem: &str,
    suffix: &str,
    ext: &str,
    link: impl Fn(&Path, &Path) -> std::io::Result<()>,
) -> std::io::Result<PathBuf> {
    use std::io::ErrorKind;
    // `symlink_metadata`, not `exists`: a dangling symlink is a taken name.
    let taken = |n: &str| std::fs::symlink_metadata(dir.join(n)).is_ok();
    let mut linking = true;
    loop {
        let target = dir.join(output_name(stem, suffix, ext, taken));
        let placed = if linking {
            match link(partial, &target) {
                Err(e)
                    if matches!(
                        e.kind(),
                        ErrorKind::Unsupported | ErrorKind::PermissionDenied
                    ) && e.raw_os_error() != Some(libc::EACCES) =>
                {
                    linking = false;
                    copy_new(partial, &target)
                }
                other => other,
            }
        } else {
            copy_new(partial, &target)
        };
        match placed {
            Ok(()) => {
                let _ = std::fs::remove_file(partial);
                return Ok(target);
            }
            Err(e) if e.kind() == ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
}

fn copy_new(from: &Path, to: &Path) -> std::io::Result<()> {
    let mut src = std::fs::File::open(from)?;
    let mut dst = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(to)?;
    let copied = std::io::copy(&mut src, &mut dst).and_then(|_| dst.sync_all());
    if copied.is_err() {
        let _ = std::fs::remove_file(to);
    }
    copied
}

/// Bytes free to an unprivileged writer on the filesystem holding `dir`.
pub fn free_bytes(dir: &Path) -> Option<u64> {
    use std::os::unix::ffi::OsStrExt;
    let c = std::ffi::CString::new(dir.as_os_str().as_bytes()).ok()?;
    // SAFETY: statvfs writes into the struct we pass and reads the
    // NUL-terminated path; both live for the call.
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::statvfs(c.as_ptr(), &mut st) };
    (rc == 0).then(|| (st.f_bavail as u64).saturating_mul(st.f_frsize as u64))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn grants(root: &Path) -> Grants {
        Grants::from_manifest(include_str!("../app.toml"), root.to_path_buf())
    }

    #[test]
    fn the_grants_are_the_manifests_folders() {
        let g = grants(Path::new("/w"));
        assert_eq!(
            g.dirs,
            [("Videos".to_string(), true), ("Music".to_string(), true)]
        );
        assert_eq!(g.names(), "Videos/, Music/");
    }

    #[test]
    fn a_path_must_be_plain_and_inside_a_grant() {
        let g = grants(Path::new("/w"));
        assert_eq!(
            g.media("Videos/trip/a.MOV").unwrap(),
            PathBuf::from("/w/Videos/trip/a.MOV")
        );
        assert!(g.media("Music/song.mp3").is_ok());
        for outside in [
            "../Videos/a.mp4",
            "/w/Videos/a.mp4",
            "Videos/../Documents/a.mp4",
            "Videos/.hidden/a.mp4",
            "Documents/a.mp4",
            "VideosX/a.mp4",
            ".deckjack/state.db",
            "",
        ] {
            assert!(
                matches!(g.media(outside), Err(PathError::Outside(..))),
                "{outside:?} was accepted"
            );
        }
        assert!(matches!(
            g.media("Videos/notes.txt"),
            Err(PathError::NotMedia(_))
        ));
        assert_eq!(g.writable_dir("Music/").unwrap(), PathBuf::from("/w/Music"));
    }

    #[test]
    fn a_read_only_grant_is_not_a_destination() {
        let g = Grants {
            root: PathBuf::from("/w"),
            dirs: vec![("Videos".into(), true), ("Archive".into(), false)],
        };
        assert!(g.media("Archive/a.mp4").is_ok());
        let err = g.writable_dir("Archive").unwrap_err();
        assert_eq!(
            err,
            PathError::NotWritable("Archive".into(), "Videos/".into())
        );
    }

    #[test]
    fn browse_lists_media_newest_first_and_skips_hidden_and_other_files() {
        let dir = tempfile::tempdir().unwrap();
        let g = grants(dir.path());
        std::fs::create_dir_all(dir.path().join("Videos/trip")).unwrap();
        std::fs::create_dir_all(dir.path().join("Music")).unwrap();
        std::fs::create_dir_all(dir.path().join("Documents")).unwrap();
        for f in [
            "Videos/a.mp4",
            "Videos/trip/b.MOV",
            "Videos/notes.txt",
            "Videos/.j1.convert-partial",
            "Music/song.flac",
            "Documents/c.mp4",
        ] {
            std::fs::write(dir.path().join(f), b"x").unwrap();
        }
        let (all, truncated) = browse(&g, None).unwrap();
        let mut names: Vec<&str> = all.iter().map(|e| e.path.as_str()).collect();
        names.sort();
        assert_eq!(
            names,
            ["Music/song.flac", "Videos/a.mp4", "Videos/trip/b.MOV"]
        );
        assert!(!truncated);
        let (some, _) = browse(&g, Some("Videos/trip")).unwrap();
        assert_eq!(some.len(), 1);
        assert!(browse(&g, Some("Documents")).is_err());
    }

    #[test]
    fn an_output_never_takes_a_name_in_use() {
        assert_eq!(
            output_name("clip", "Share", "mp4", |_| false),
            "clip (Share).mp4"
        );
        let taken = ["clip (Share).mp4", "clip (Share) 2.mp4"];
        assert_eq!(
            output_name("clip", "Share", "mp4", |n| taken.contains(&n)),
            "clip (Share) 3.mp4"
        );
    }

    #[test]
    fn publishing_never_replaces_a_file() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("clip (Share).mp4"), b"theirs").unwrap();
        let partial = dir.path().join(partial_name("j7"));
        std::fs::write(&partial, b"ours").unwrap();
        let out = publish(dir.path(), &partial, "clip", "Share", "mp4").unwrap();
        assert_eq!(out, dir.path().join("clip (Share) 2.mp4"));
        assert_eq!(std::fs::read(&out).unwrap(), b"ours");
        assert_eq!(
            std::fs::read(dir.path().join("clip (Share).mp4")).unwrap(),
            b"theirs"
        );
        assert!(!partial.exists(), "the partial is gone once published");
    }

    #[test]
    fn without_hard_links_it_copies_and_still_never_replaces() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a (FLAC).flac"), b"theirs").unwrap();
        let partial = dir.path().join(partial_name("j1"));
        std::fs::write(&partial, b"ours").unwrap();
        let out = publish_with(dir.path(), &partial, "a", "FLAC", "flac", |_, _| {
            Err(std::io::Error::from_raw_os_error(libc::EPERM))
        })
        .unwrap();
        assert_eq!(out, dir.path().join("a (FLAC) 2.flac"));
        assert_eq!(std::fs::read(out).unwrap(), b"ours");
        assert_eq!(
            std::fs::read(dir.path().join("a (FLAC).flac")).unwrap(),
            b"theirs"
        );
    }

    #[test]
    fn a_partial_is_hidden_and_names_its_job() {
        let name = partial_name("j12");
        assert!(name.starts_with('.') && name.contains("j12"), "{name}");
        assert!(
            !is_media(Path::new(&name)),
            "a partial is never listed as media"
        );
    }

    #[test]
    fn free_space_is_reported_for_a_real_folder() {
        let dir = tempfile::tempdir().unwrap();
        assert!(free_bytes(dir.path()).unwrap() > 0);
        assert_eq!(free_bytes(Path::new("/no/such/folder")), None);
    }
}
