//! Names on disk: projects, media and exports under `Videos/`.
//!
//! The app is confined to `Videos/` by Landlock, so an escape would be refused
//! by the kernel anyway. These checks exist for the reasons Notes gives for
//! its own: a caller (often an agent) should be told what was wrong with a
//! name in a sentence rather than with a bare `EPERM`, and confinement is off
//! in development, where this is the only check there is.

use std::path::{Component, Path, PathBuf};

/// Where everything this app touches lives, relative to the working directory.
/// Also what the manifest asks for, and so the only part of the working
/// directory the process can open at all.
pub const VIDEOS_DIR: &str = "Videos";

/// FilmCraft's project file. The format is FilmCraft's own, documented and
/// migrated; the open sidecar beside it is [`SIDECAR_EXTENSION`].
pub const PROJECT_EXTENSION: &str = "fcproj";

/// OpenTimelineIO, written next to the project on every save.
pub const SIDECAR_EXTENSION: &str = "otio";

/// Longest project name. A paste accident, not a filesystem limit.
pub const MAX_NAME: usize = 100;

/// What [`media_files`](crate::engine) lists and import accepts: what
/// FilmCraft's own decoders read, and nothing that would only fail later.
pub const MEDIA_EXTENSIONS: &[&str] = &[
    "mp4", "mov", "m4v", "mkv", "webm", "avi", "mxf", "wav", "mp3", "flac", "aif", "aiff", "m4a",
    "ogg", "opus", "png", "jpg", "jpeg",
];

/// The suffix of an export still being written. Hidden (a leading dot) and
/// never a name an export is published under, so a half-written file is
/// never mistaken for a finished one by Files, Quick Look or a person.
pub const PARTIAL_SUFFIX: &str = ".partial.mp4";

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum NameError {
    #[error("a project needs a name with something in it")]
    Empty,
    #[error("{0:?} is longer than {MAX_NAME} characters")]
    TooLong(String),
    #[error(
        "{0:?} is not a usable name: one segment, with no '/', no leading '.', and no control \
         characters"
    )]
    Bad(String),
    #[error(
        "{0:?} is not a path inside Videos/: give it relative to Videos/, with no '..' and no \
         hidden folders"
    )]
    Outside(String),
    #[error("{0:?} is not a media file this editor reads (by its extension)")]
    NotMedia(String),
}

/// A project's name as the caller gave it, checked, with an optional
/// `.fcproj` taken off. The name is the id every command takes.
pub fn project_name(name: &str) -> Result<String, NameError> {
    let trimmed = name.trim();
    let trimmed = trimmed
        .strip_suffix(&format!(".{PROJECT_EXTENSION}"))
        .unwrap_or(trimmed)
        .trim_end();
    if trimmed.is_empty() {
        return Err(NameError::Empty);
    }
    if trimmed.chars().count() > MAX_NAME {
        return Err(NameError::TooLong(trimmed.to_string()));
    }
    let bad = trimmed.starts_with('.')
        || trimmed
            .chars()
            .any(|c| c == '/' || c == '\\' || c.is_control());
    if bad {
        return Err(NameError::Bad(trimmed.to_string()));
    }
    Ok(trimmed.to_string())
}

/// `<root>/<name>.fcproj`.
pub fn project_path(root: &Path, name: &str) -> PathBuf {
    root.join(format!("{name}.{PROJECT_EXTENSION}"))
}

/// `<root>/<name>.otio`.
pub fn sidecar_path(root: &Path, name: &str) -> PathBuf {
    root.join(format!("{name}.{SIDECAR_EXTENSION}"))
}

/// A media path from a caller, relative to `Videos/`, as an absolute path
/// inside it. Subfolders are fine; `..`, absolute paths and hidden segments
/// are not, and neither is a file this editor cannot read.
pub fn media_path(root: &Path, rel: &str) -> Result<PathBuf, NameError> {
    let rel_path = Path::new(rel.trim());
    if rel.trim().is_empty() {
        return Err(NameError::Outside(rel.to_string()));
    }
    for part in rel_path.components() {
        match part {
            Component::Normal(seg) if !seg.to_string_lossy().starts_with('.') => {}
            _ => return Err(NameError::Outside(rel.to_string())),
        }
    }
    if !is_media(rel_path) {
        return Err(NameError::NotMedia(rel.to_string()));
    }
    Ok(root.join(rel_path))
}

/// Whether a file's extension is one [`MEDIA_EXTENSIONS`] names.
pub fn is_media(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| MEDIA_EXTENSIONS.contains(&e.to_ascii_lowercase().as_str()))
}

/// The first free export name for `stem`: `stem.mp4`, then `stem 2.mp4`,
/// `stem 3.mp4` and on. `taken` says whether a name is in use; the caller's
/// publish is still exclusive, because a name free here can be taken a moment
/// later.
pub fn export_name(stem: &str, taken: impl Fn(&str) -> bool) -> String {
    let first = format!("{stem}.mp4");
    if !taken(&first) {
        return first;
    }
    let mut n: u32 = 2;
    loop {
        let candidate = format!("{stem} {n}.mp4");
        if !taken(&candidate) {
            return candidate;
        }
        n = n.saturating_add(1);
    }
}

/// The hidden name an export is written under until it is complete.
pub fn partial_name(stem: &str, job: u64) -> String {
    format!(".{stem}.{job}{PARTIAL_SUFFIX}")
}

/// Whether a file in `Videos/` is an export left half-written, by a crash or
/// a kill, which nothing will ever finish.
pub fn is_partial(name: &str) -> bool {
    name.starts_with('.') && name.ends_with(PARTIAL_SUFFIX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_project_name_is_one_plain_segment() {
        assert_eq!(project_name("  Holiday cut ").unwrap(), "Holiday cut");
        // The extension is accepted and dropped, so `describe` and `open`
        // agree on one id for one file.
        assert_eq!(project_name("Holiday.fcproj").unwrap(), "Holiday");
        assert_eq!(project_name(""), Err(NameError::Empty));
        assert_eq!(project_name(".fcproj"), Err(NameError::Empty));
        for bad in ["../up", "a/b", ".hidden", "a\\b", "tab\there"] {
            assert!(
                matches!(project_name(bad), Err(NameError::Bad(_))),
                "{bad:?} was accepted"
            );
        }
        let long = "x".repeat(MAX_NAME + 1);
        assert!(matches!(project_name(&long), Err(NameError::TooLong(_))));
        assert!(project_name(&"x".repeat(MAX_NAME)).is_ok());
    }

    #[test]
    fn a_media_path_stays_inside_videos() {
        let root = Path::new("/w/Videos");
        assert_eq!(
            media_path(root, "trip/a.MP4").unwrap(),
            PathBuf::from("/w/Videos/trip/a.MP4")
        );
        for outside in [
            "../a.mp4",
            "/etc/a.mp4",
            "trip/../../a.mp4",
            ".cache/a.mp4",
            "",
        ] {
            assert!(
                matches!(media_path(root, outside), Err(NameError::Outside(_))),
                "{outside:?} was accepted"
            );
        }
        assert!(matches!(
            media_path(root, "notes.txt"),
            Err(NameError::NotMedia(_))
        ));
    }

    #[test]
    fn an_export_never_takes_a_name_in_use() {
        let taken = ["Cut.mp4", "Cut 2.mp4"];
        assert_eq!(export_name("Cut", |n| taken.contains(&n)), "Cut 3.mp4");
        assert_eq!(export_name("Fresh", |n| taken.contains(&n)), "Fresh.mp4");
    }

    #[test]
    fn a_partial_export_is_hidden_and_recognized() {
        let name = partial_name("Cut", 7);
        assert_eq!(name, ".Cut.7.partial.mp4");
        assert!(is_partial(&name));
        assert!(!is_partial("Cut.mp4"));
        // A visible file that merely ends the same way is the operator's.
        assert!(!is_partial("my.partial.mp4"));
    }
}
