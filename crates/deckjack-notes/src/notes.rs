//! Notes on disk: Markdown files under `Notes/` in the working directory.
//!
//! The whole store is the filesystem. There is no index, no database and no
//! metadata file, because the design's rule for user data is that it stays in
//! plain files and open formats: "if Deckjack disappeared the files would
//! still work with other tools". A note is a `.md` file, its title is its
//! first heading or its filename, and its modified time is the file's.
//!
//! Everything here takes the root as an argument and touches nothing else, so
//! every rule below is exercised against a temporary directory.

use std::io;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use serde::Serialize;

/// Where notes live, relative to the working directory. Also what the
/// manifest asks for, and so the only part of the working directory this app
/// can open at all.
pub const NOTES_DIR: &str = "Notes";

pub const EXTENSION: &str = "md";

/// Longest name this will write. Not a filesystem limit — 255 bytes is —
/// but a title that long is a paste accident, and refusing it with a sentence
/// beats a `ENAMETOOLONG` from three layers down.
pub const MAX_NAME: usize = 120;

#[derive(Debug, thiserror::Error)]
pub enum NoteError {
    #[error(
        "{0:?} is not a note in this folder: a name is one path segment ending in .md, with \
         no directories and no '..'"
    )]
    BadName(String),

    #[error("a note needs a name with something in it")]
    EmptyName,

    #[error("{0:?} is longer than {MAX_NAME} characters")]
    TooLong(String),

    #[error("there is no note called {0:?}")]
    NoSuchNote(String),

    #[error("there is already a note called {0:?}")]
    Exists(String),

    #[error("cannot {doing} {name:?}: {source}")]
    Io {
        doing: &'static str,
        name: String,
        #[source]
        source: io::Error,
    },
}

/// One note, as the UI and the palette see it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Note {
    /// The filename, which is also the id. Never a path.
    pub name: String,
    /// First `# heading`, else the filename without its extension.
    pub title: String,
    pub bytes: u64,
    /// Unix seconds. Zero when the filesystem does not say, which is better
    /// than refusing to list a note over a missing timestamp.
    pub modified: u64,
}

/// The notes folder, created if it is not there.
///
/// Creating it is right rather than presumptuous: the manifest asked for
/// `Notes/` with write access, the operator approved that, and an app that
/// showed "no such directory" on a fresh working directory would be asking
/// them to go and make it by hand. The core has already created it as part of
/// building the sandbox policy; this is for the unconfined development case.
pub fn root(working_dir: &Path) -> Result<PathBuf, NoteError> {
    let root = working_dir.join(NOTES_DIR);
    std::fs::create_dir_all(&root).map_err(|source| NoteError::Io {
        doing: "create the notes folder",
        name: NOTES_DIR.to_string(),
        source,
    })?;
    Ok(root)
}

/// Turn a name from a caller into a path inside `root`, or refuse.
///
/// The app is confined to `Notes/` by Landlock, so an escape would be refused
/// by the kernel anyway. This exists for two reasons that outlive that: the
/// error a caller gets should say what was wrong with the name rather than
/// being a bare `EPERM` from a syscall, and confinement is off in development
/// — `Confinement::unconfined_for_development` — where this is the only
/// check there is.
pub fn path_for(root: &Path, name: &str) -> Result<PathBuf, NoteError> {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return Err(NoteError::EmptyName);
    }
    if trimmed.len() > MAX_NAME {
        return Err(NoteError::TooLong(trimmed.to_string()));
    }
    // One segment, nothing clever. `Path::file_name` would quietly accept
    // `a/b.md` by returning `b.md`, which is how a check like this becomes
    // decoration.
    let bad = trimmed.contains('/')
        || trimmed.contains('\\')
        || trimmed.contains('\0')
        || trimmed.starts_with('.')
        || trimmed != trimmed.trim_start_matches('.');
    if bad {
        return Err(NoteError::BadName(trimmed.to_string()));
    }
    if Path::new(trimmed).extension().and_then(|e| e.to_str()) != Some(EXTENSION) {
        return Err(NoteError::BadName(trimmed.to_string()));
    }
    Ok(root.join(trimmed))
}

/// Every note, newest first. A note the filesystem will not describe is
/// listed with what is known rather than dropped.
pub fn list(root: &Path) -> Result<Vec<Note>, NoteError> {
    let entries = match std::fs::read_dir(root) {
        Ok(entries) => entries,
        // A folder that is not there holds no notes. The alternative is an
        // app that errors on a working directory nobody has used yet.
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(source) => {
            return Err(NoteError::Io {
                doing: "read the notes folder",
                name: NOTES_DIR.to_string(),
                source,
            });
        }
    };

    let mut notes = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some(EXTENSION) {
            continue;
        }
        // A *directory* called `folder.md` is not a note. Checked with
        // `file_type` from the entry rather than `path.is_file`, which
        // follows symlinks and would also stat the path a second time.
        if !entry.file_type().is_ok_and(|t| t.is_file()) {
            continue;
        }
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if name.starts_with('.') {
            continue;
        }
        let meta = entry.metadata().ok();
        let bytes = meta.as_ref().map(|m| m.len()).unwrap_or(0);
        let modified = meta
            .as_ref()
            .and_then(|m| m.modified().ok())
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_secs())
            .unwrap_or(0);
        // Only the first line is read for a title, so listing a folder of
        // large notes does not mean reading all of them.
        let title = title_of(&path, name);
        notes.push(Note {
            name: name.to_string(),
            title,
            bytes,
            modified,
        });
    }

    notes.sort_by(|a, b| {
        b.modified
            .cmp(&a.modified)
            .then_with(|| a.name.cmp(&b.name))
    });
    Ok(notes)
}

/// A note's first `# heading`, or its filename without the extension.
fn title_of(path: &Path, name: &str) -> String {
    let fallback = || {
        name.strip_suffix(&format!(".{EXTENSION}"))
            .unwrap_or(name)
            .to_string()
    };
    let Ok(file) = std::fs::File::open(path) else {
        return fallback();
    };
    use std::io::BufRead as _;
    let mut reader = io::BufReader::new(file);
    let mut line = String::new();
    // One line, bounded: a 2GB note with no newline must not be read into
    // memory to find out it has no heading.
    let mut limited = std::io::Read::take(&mut reader, 4096);
    let mut buf = Vec::new();
    if std::io::Read::read_to_end(&mut limited, &mut buf).is_err() {
        return fallback();
    }
    if let Some(first) = buf.lines().next().and_then(Result::ok) {
        line = first;
    }
    let heading = line.trim().strip_prefix("# ").map(str::trim);
    match heading {
        Some(h) if !h.is_empty() => h.to_string(),
        _ => fallback(),
    }
}

pub fn read(root: &Path, name: &str) -> Result<String, NoteError> {
    let path = path_for(root, name)?;
    match std::fs::read_to_string(&path) {
        Ok(text) => Ok(text),
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            Err(NoteError::NoSuchNote(name.to_string()))
        }
        Err(source) => Err(NoteError::Io {
            doing: "read",
            name: name.to_string(),
            source,
        }),
    }
}

/// Save a note, replacing what was there.
///
/// Written to a temporary file in the same folder and renamed over the
/// original, so a note is never half-written: a crash or a full disk leaves
/// either the old text or the new one. `rename` is atomic within a
/// filesystem, and both paths are inside `Notes/`.
pub fn write(root: &Path, name: &str, text: &str) -> Result<(), NoteError> {
    let path = path_for(root, name)?;
    let temp = path.with_extension("md.saving");
    let io_err = |doing: &'static str| {
        move |source: io::Error| NoteError::Io {
            doing,
            name: name.to_string(),
            source,
        }
    };
    std::fs::write(&temp, text).map_err(io_err("save"))?;
    std::fs::rename(&temp, &path).map_err(io_err("save"))?;
    Ok(())
}

/// Create a note from a title, deriving a filename from it.
pub fn create(root: &Path, title: &str) -> Result<Note, NoteError> {
    let name = filename_for(title)?;
    let path = path_for(root, &name)?;
    if path.exists() {
        return Err(NoteError::Exists(name));
    }
    // The title becomes the note's first heading, so the file explains itself
    // to whatever opens it next, and `title_of` reads back what was asked for
    // even after a rename.
    write(root, &name, &format!("# {}\n\n", title.trim()))?;
    let notes = list(root)?;
    notes
        .into_iter()
        .find(|n| n.name == name)
        .ok_or(NoteError::NoSuchNote(name))
}

/// A filename from a title: the title, with anything a path cares about
/// replaced, and `.md` on the end.
pub fn filename_for(title: &str) -> Result<String, NoteError> {
    let trimmed = title.trim();
    if trimmed.is_empty() {
        return Err(NoteError::EmptyName);
    }
    let mut out = String::with_capacity(trimmed.len());
    for c in trimmed.chars() {
        match c {
            // Not a denylist of "dangerous" characters: an allowlist of what
            // is kept. A title is arbitrary text from a person and may contain
            // anything at all, including things that are legal in a filename
            // and still a bad idea in one.
            c if c.is_alphanumeric() => out.push(c),
            ' ' | '-' | '_' => out.push(if c == ' ' { '-' } else { c }),
            _ => {}
        }
    }
    let out = out.trim_matches('-').to_string();
    if out.is_empty() {
        // A title of only punctuation or emoji. Refused rather than given a
        // generated name, because a note called `note-1.md` is one the person
        // cannot find again.
        return Err(NoteError::BadName(trimmed.to_string()));
    }
    let name = format!(
        "{}.{EXTENSION}",
        truncate(&out, MAX_NAME - EXTENSION.len() - 1)
    );
    Ok(name)
}

/// Cut to a byte budget without splitting a character.
fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].trim_end_matches('-').to_string()
}

pub fn rename(root: &Path, from: &str, to: &str) -> Result<Note, NoteError> {
    let from_path = path_for(root, from)?;
    let to_name = if to.ends_with(&format!(".{EXTENSION}")) {
        let _ = path_for(root, to)?;
        to.trim().to_string()
    } else {
        filename_for(to)?
    };
    let to_path = path_for(root, &to_name)?;
    if !from_path.exists() {
        return Err(NoteError::NoSuchNote(from.to_string()));
    }
    // Checked rather than relying on `rename`, which would silently replace
    // the destination and so lose a note the caller never mentioned.
    if to_path.exists() && to_path != from_path {
        return Err(NoteError::Exists(to_name));
    }
    std::fs::rename(&from_path, &to_path).map_err(|source| NoteError::Io {
        doing: "rename",
        name: from.to_string(),
        source,
    })?;
    list(root)?
        .into_iter()
        .find(|n| n.name == to_name)
        .ok_or(NoteError::NoSuchNote(to_name))
}

pub fn delete(root: &Path, name: &str) -> Result<(), NoteError> {
    let path = path_for(root, name)?;
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            Err(NoteError::NoSuchNote(name.to_string()))
        }
        Err(source) => Err(NoteError::Io {
            doing: "delete",
            name: name.to_string(),
            source,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rig() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let root = root(dir.path()).unwrap();
        (dir, root)
    }

    #[test]
    fn a_name_must_be_one_md_segment() {
        let (_d, root) = rig();
        assert!(path_for(&root, "today.md").is_ok());
        assert!(path_for(&root, "a note with spaces.md").is_ok());

        // Every one of these is a way out of the folder or a hidden file.
        for bad in [
            "../escape.md",
            "..",
            "sub/dir.md",
            "sub\\dir.md",
            ".hidden.md",
            "/etc/passwd",
            "note.txt",
            "note",
            "",
            "   ",
        ] {
            assert!(path_for(&root, bad).is_err(), "{bad:?} was accepted");
        }
    }

    #[test]
    fn a_name_cannot_carry_a_nul() {
        // A NUL truncates the path at the syscall boundary, so `a.md\0.txt`
        // would write `a.md`. Rust's `CString` conversion refuses it, but the
        // error would be an io error about an invalid argument rather than
        // anything a caller could act on.
        let (_d, root) = rig();
        assert!(path_for(&root, "a.md\0.txt").is_err());
    }

    #[test]
    fn an_overlong_name_is_refused_before_the_filesystem_sees_it() {
        let (_d, root) = rig();
        let long = format!("{}.md", "a".repeat(MAX_NAME));
        assert!(matches!(path_for(&root, &long), Err(NoteError::TooLong(_))));
    }

    #[test]
    fn a_note_round_trips() {
        let (_d, root) = rig();
        write(&root, "today.md", "# Today\n\nthings").unwrap();
        assert_eq!(read(&root, "today.md").unwrap(), "# Today\n\nthings");
    }

    #[test]
    fn reading_a_note_that_is_not_there_says_so() {
        let (_d, root) = rig();
        assert!(matches!(
            read(&root, "ghost.md"),
            Err(NoteError::NoSuchNote(_))
        ));
    }

    #[test]
    fn saving_leaves_no_partial_file_behind() {
        let (_d, root) = rig();
        write(&root, "a.md", "first").unwrap();
        write(&root, "a.md", "second").unwrap();
        assert_eq!(read(&root, "a.md").unwrap(), "second");
        // The temporary file is renamed, not left: a `.saving` file in the
        // notes folder would show up in the operator's own file manager.
        let strays: Vec<_> = std::fs::read_dir(&root)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.contains("saving"))
            .collect();
        assert!(strays.is_empty(), "left {strays:?}");
    }

    #[test]
    fn listing_is_newest_first_and_ignores_what_is_not_a_note() {
        let (_d, root) = rig();
        write(&root, "old.md", "# Old").unwrap();
        write(&root, "new.md", "# New").unwrap();
        std::fs::write(root.join("notes.txt"), "not a note").unwrap();
        std::fs::write(root.join(".hidden.md"), "# Hidden").unwrap();
        std::fs::create_dir(root.join("folder.md")).unwrap();

        // Modified times can land in the same second, so the order is forced
        // rather than assumed: the test is about the sort, not the clock.
        let past = std::time::SystemTime::now() - std::time::Duration::from_secs(600);
        set_modified(&root.join("old.md"), past);

        let notes = list(&root).unwrap();
        let names: Vec<&str> = notes.iter().map(|n| n.name.as_str()).collect();
        assert_eq!(names, vec!["new.md", "old.md"], "got {names:?}");
    }

    #[test]
    fn a_title_comes_from_the_first_heading_and_falls_back_to_the_name() {
        let (_d, root) = rig();
        write(&root, "headed.md", "# A Real Title\n\nbody").unwrap();
        write(&root, "plain.md", "no heading here").unwrap();
        write(&root, "empty-heading.md", "#\n").unwrap();

        let notes = list(&root).unwrap();
        let title = |name: &str| notes.iter().find(|n| n.name == name).unwrap().title.clone();
        assert_eq!(title("headed.md"), "A Real Title");
        assert_eq!(title("plain.md"), "plain");
        assert_eq!(title("empty-heading.md"), "empty-heading");
    }

    #[test]
    fn a_title_with_no_newline_does_not_read_the_whole_file() {
        // The guard is a 4KiB cap on the first read. A note that is one very
        // long line must not be pulled into memory to look for a heading.
        let (_d, root) = rig();
        let huge = format!("# {}", "x".repeat(200_000));
        write(&root, "huge.md", &huge).unwrap();
        let notes = list(&root).unwrap();
        let title = &notes[0].title;
        assert!(title.len() < 4096, "read {} bytes of title", title.len());
        assert!(title.starts_with('x'));
    }

    #[test]
    fn creating_derives_a_filename_and_writes_the_heading() {
        let (_d, root) = rig();
        let note = create(&root, "Monday Standup").unwrap();
        assert_eq!(note.name, "Monday-Standup.md");
        assert_eq!(note.title, "Monday Standup");
        assert_eq!(read(&root, &note.name).unwrap(), "# Monday Standup\n\n");
    }

    #[test]
    fn a_filename_keeps_only_what_is_safe() {
        assert_eq!(filename_for("Hello, World!").unwrap(), "Hello-World.md");
        assert_eq!(filename_for("../../etc/passwd").unwrap(), "etcpasswd.md");
        assert_eq!(filename_for("  spaced  out  ").unwrap(), "spaced--out.md");
        assert_eq!(filename_for("Ünïcødé").unwrap(), "Ünïcødé.md");

        // A title with nothing to keep is refused rather than given a
        // generated name the person could never find again.
        for bad in ["", "   ", "!!!", "../.."] {
            assert!(filename_for(bad).is_err(), "{bad:?} produced a name");
        }
    }

    #[test]
    fn a_very_long_title_is_cut_without_splitting_a_character() {
        let name = filename_for(&"é".repeat(400)).unwrap();
        assert!(name.len() <= MAX_NAME, "{} bytes", name.len());
        assert!(name.ends_with(".md"));
        // Valid UTF-8 and not a replacement character, which is what a cut
        // through a multi-byte character would leave.
        assert!(!name.contains('\u{fffd}'));
    }

    #[test]
    fn creating_the_same_title_twice_is_refused() {
        let (_d, root) = rig();
        create(&root, "Once").unwrap();
        assert!(matches!(create(&root, "Once"), Err(NoteError::Exists(_))));
    }

    #[test]
    fn renaming_moves_the_file_and_never_overwrites_another_note() {
        let (_d, root) = rig();
        create(&root, "First").unwrap();
        create(&root, "Second").unwrap();

        let moved = rename(&root, "First.md", "Third").unwrap();
        assert_eq!(moved.name, "Third.md");
        assert!(read(&root, "First.md").is_err());

        // The destination exists: refused, because `rename` would otherwise
        // replace a note nobody mentioned.
        assert!(matches!(
            rename(&root, "Third.md", "Second"),
            Err(NoteError::Exists(_))
        ));
        assert_eq!(read(&root, "Second.md").unwrap(), "# Second\n\n");
    }

    #[test]
    fn renaming_accepts_a_filename_as_well_as_a_title() {
        let (_d, root) = rig();
        create(&root, "Alpha").unwrap();
        let moved = rename(&root, "Alpha.md", "beta.md").unwrap();
        assert_eq!(moved.name, "beta.md");
    }

    #[test]
    fn renaming_cannot_be_used_to_leave_the_folder() {
        let (_d, root) = rig();
        create(&root, "Alpha").unwrap();
        assert!(rename(&root, "Alpha.md", "../escaped.md").is_err());
        assert!(rename(&root, "../../etc/passwd", "x").is_err());
    }

    #[test]
    fn deleting_removes_it_and_says_so_when_it_was_not_there() {
        let (_d, root) = rig();
        create(&root, "Doomed").unwrap();
        delete(&root, "Doomed.md").unwrap();
        assert!(list(&root).unwrap().is_empty());
        assert!(matches!(
            delete(&root, "Doomed.md"),
            Err(NoteError::NoSuchNote(_))
        ));
    }

    #[test]
    fn a_missing_notes_folder_lists_nothing_rather_than_failing() {
        let dir = tempfile::tempdir().unwrap();
        let never_created = dir.path().join("Notes");
        assert!(list(&never_created).unwrap().is_empty());
    }

    fn set_modified(path: &Path, when: std::time::SystemTime) {
        let file = std::fs::File::options().write(true).open(path).unwrap();
        file.set_modified(when).unwrap();
    }
}
