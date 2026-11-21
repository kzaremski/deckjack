//! The text editor's commands: `editor.open`, `editor.save`, `editor.revert`
//! and `editor.find`.
//!
//! # Why these are the core's and not the editor app's
//!
//! The editor is an app (`crates/deckjack-editor`), but like Files it is a UI
//! and nothing else, and its commands are registered here, beside `files.*`,
//! on the same [`Namespace`]. Two facts force that, and both are worth
//! knowing before anyone tries to move them:
//!
//! - An app process cannot be granted the working directory. A manifest
//!   asking for `working_dir = [{ path = "." }]` is refused by
//!   `Manifest::validate`, because Landlock would grant the tree under it,
//!   `.deckjack/` included. An editor that can only open one subfolder is not
//!   an editor.
//! - An app *can* call `files.*` over the bus with `invoke_commands`, but each
//!   call runs as the agent session `app:editor` and needs a grant for it.
//!   Nothing in the product issues that grant, so every save would be refused
//!   at runtime: a feature built, unit-tested and impossible to use.
//!
//! So the commands are thin layers over the file namespace — the one
//! implementation of the working-directory boundary — and add only what an
//! editor needs that `files.read` and `files.write` do not say:
//!
//! - **An honest answer for a big file.** Opening a 200 MB log must not pull
//!   200 MB through a JSON response or hang the tab. [`plan`] decides, in one
//!   place: small enough to save back is editable; bigger but still sane is
//!   whole and read-only; anything larger is its *tail*, read-only, with a
//!   way to page backwards.
//! - **A save that cannot silently clobber.** `editor.open` returns a content
//!   hash, and `editor.save` refuses (as an answer, not an error) when what is
//!   on disk no longer has it. Modified times are whole seconds here, so a
//!   timestamp alone misses an external write in the same second; a hash does
//!   not.
//! - **Search for an agent.** `editor.find` returns line and column matches
//!   in a file, which is what an agent asked "where is this used" wants and
//!   what the palette can show. The editor UI searches its own buffer.
//!
//! None of this is a project model. There is one working directory and an
//! address in it; that is the whole of what these commands know.

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use crate::command::{Command, CommandError, Context, RegistryError, RiskLevel};
use crate::files::{self, FileError, MAX_WRITE, Namespace, NodePath, off_thread};

/// Largest file opened for editing.
///
/// Not a taste: it is what a save can carry back. The browser's socket
/// refuses an inbound frame over [`SAVE_FRAME_LIMIT`], and a save is one
/// frame holding the whole file JSON-escaped (a newline is two bytes, a quote
/// two) plus its envelope. Three quarters of the frame leaves room for that
/// on any ordinary source file; the UI measures the real frame before
/// sending, so a file that grows past the limit while open is told so rather
/// than dropping the socket.
pub const EDIT_LIMIT: u64 = (SAVE_FRAME_LIMIT as u64) * 3 / 4;

/// Largest file read whole, read-only. Above this a file opens as its tail.
pub const WHOLE_LIMIT: u64 = 4 * 1024 * 1024;

/// How much of a big file one `editor.open` returns: the window size for
/// tail-first viewing.
pub const TAIL_WINDOW: u64 = 256 * 1024;

/// The browser socket's inbound frame limit, which a save has to fit in.
///
/// The socket's default, restated because `Default` is not `const`; a test
/// below fails if the two drift. The socket settings are not configurable
/// yet; when they are, this becomes a field.
pub const SAVE_FRAME_LIMIT: usize = 256 * 1024;

/// Most matches `editor.find` returns. A search for `e` in a log is a
/// question with no useful answer at a million matches.
pub const FIND_MAX: usize = 1000;
/// Longest query `editor.find` takes, in characters. Literal text to look
/// for is a name or a phrase; a longer one is a request for work, not a
/// search.
pub const FIND_QUERY_MAX: usize = 1024;
const FIND_DEFAULT: usize = 200;
/// A matched line is returned cut to this many characters around the match.
const FIND_CONTEXT: usize = 160;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    /// The whole file, editable, and small enough to save back.
    Edit,
    /// Shown, not editable. `reason` says why.
    ReadOnly,
}

/// What part of a file a read-only window shows, in bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Window {
    pub start: u64,
    pub end: u64,
}

/// How [`plan`] decided to open a file of a given size.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Plan {
    Edit,
    WholeReadOnly,
    /// The window of bytes ending at `end`.
    Tail {
        end: u64,
    },
}

/// Decide how to open `size` bytes. One function so the thresholds are a
/// table, not a scatter of comparisons.
///
/// `before` asks for the window ending there, which is how a read-only view
/// pages backwards through a big file. Asking for a window of a file small
/// enough to read whole is answered with the whole file: there is nothing
/// earlier to page to.
pub fn plan(size: u64, before: Option<u64>) -> Plan {
    if size > WHOLE_LIMIT {
        return Plan::Tail {
            end: before.unwrap_or(size).min(size),
        };
    }
    if size > EDIT_LIMIT {
        Plan::WholeReadOnly
    } else {
        Plan::Edit
    }
}

/// A file as the editor opens it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Opened {
    /// The address the read landed on, qualified. A symlink resolves to its
    /// target, so a save goes where the bytes came from.
    pub path: NodePath,
    pub name: String,
    pub size: u64,
    pub modified: Option<u64>,
    pub mode: Mode,
    /// Why it is read-only, in words for the operator. Absent when editable.
    pub reason: Option<String>,
    pub text: String,
    /// The content hash a save must match, for a whole-file read. Absent for
    /// a window, which can never be saved.
    pub version: Option<String>,
    /// Set when `text` is part of the file.
    pub window: Option<Window>,
    /// The frame a save must fit in, so the UI can check before it sends.
    pub save_limit: usize,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct OpenParams {
    pub path: NodePath,
    /// For a file too big to read whole: the byte offset the returned window
    /// ends at, to page backwards from a previous window's `start`. Left out,
    /// the window is the end of the file.
    #[serde(default)]
    pub before: Option<u64>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SaveParams {
    pub path: NodePath,
    /// The whole new text.
    pub content: String,
    /// The `version` `editor.open` returned. The save is refused, as a
    /// `conflict`, if the file on disk no longer has it. Left out, the file
    /// must not exist yet: that is "save as a new file", and it never
    /// overwrites.
    #[serde(default)]
    pub expected: Option<String>,
    /// Write whatever is on disk now. For after the operator was shown a
    /// conflict and chose their own text.
    #[serde(default)]
    pub force: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SaveStatus {
    Saved,
    /// Nothing was written: the file changed (or appeared, or vanished) since
    /// it was opened. `disk` says what is there now.
    Conflict,
}

/// What is on disk, for a conflict.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OnDisk {
    /// Absent when the file is not there.
    pub version: Option<String>,
    pub size: Option<u64>,
    pub modified: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Saved {
    pub status: SaveStatus,
    pub path: NodePath,
    /// The version now on disk: the new one after a save, the other party's
    /// after a conflict.
    pub version: Option<String>,
    pub size: Option<u64>,
    pub modified: Option<u64>,
    /// For a conflict, a sentence saying what happened and what to do.
    pub message: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FindParams {
    pub path: NodePath,
    /// Literal text, not a pattern.
    pub query: String,
    #[serde(default)]
    pub case_sensitive: bool,
    /// At most this many matches, up to the server's cap of 1000.
    #[serde(default)]
    pub max: Option<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Match {
    /// 1-based.
    pub line: usize,
    /// 1-based, in characters.
    pub column: usize,
    /// The line, cut around the match when it is long.
    pub text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Found {
    pub path: NodePath,
    pub matches: Vec<Match>,
    /// More matches exist than were returned.
    pub truncated: bool,
}

// ---------------------------------------------------------------------------
// The rules, without a bus
// ---------------------------------------------------------------------------

/// The version string for some bytes. A content hash, truncated: it only has
/// to tell one revision of one file from another, not resist an adversary.
pub fn version_of(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    digest[..16].iter().map(|b| format!("{b:02x}")).collect()
}

/// Whether a window of a big file is binary rather than text.
///
/// A NUL byte, or invalid UTF-8 making up more than a tenth of the window.
/// Not "any invalid byte": a window may start or end mid-character, and a
/// long log with the odd Latin-1 byte in it is still a log someone wants to
/// read. Random or compressed data is invalid UTF-8 roughly every other
/// byte, far past the line.
fn looks_binary(bytes: &[u8]) -> bool {
    if bytes.contains(&0) {
        return true;
    }
    let mut invalid = 0usize;
    let mut rest = bytes;
    while let Err(e) = std::str::from_utf8(rest) {
        match e.error_len() {
            Some(n) => {
                invalid += n;
                rest = &rest[e.valid_up_to() + n..];
            }
            // Cut off at the end of the window, not wrong.
            None => break,
        }
    }
    invalid * 10 > bytes.len()
}

/// Bytes from the middle of a file, made into text that starts on a line.
///
/// A window that starts mid-file starts mid-line and perhaps mid-character,
/// so everything before the first newline is dropped and `start` moves past
/// it. A window with no newline at all (one enormous line) is kept from the
/// first character boundary instead, because an empty window is not an
/// honest view of a file that has bytes in it. The end is the caller's: it
/// is the file's end, or a previous window's start, which was a line start.
fn window_text(bytes: Vec<u8>, start: u64) -> (String, u64) {
    if start == 0 {
        return (String::from_utf8_lossy(&bytes).into_owned(), 0);
    }
    let skip = match bytes.iter().position(|b| *b == b'\n') {
        Some(newline) if newline + 1 < bytes.len() => newline + 1,
        // A UTF-8 continuation byte is 10xxxxxx; skip those to land on a
        // character's first byte.
        _ => bytes
            .iter()
            .position(|b| (b & 0b1100_0000) != 0b1000_0000)
            .unwrap_or(0),
    };
    (
        String::from_utf8_lossy(&bytes[skip..]).into_owned(),
        start + skip as u64,
    )
}

/// Every literal occurrence of `query` in `text`, by line and column.
///
/// Linear in the text whatever the query (Knuth-Morris-Pratt over folded
/// characters): comparing the query at every position is about n * m work,
/// and this runs on a blocking thread nobody can cancel, for a command that
/// runs without asking.
pub fn find_in(text: &str, query: &str, case_sensitive: bool, max: usize) -> (Vec<Match>, bool) {
    let mut matches = Vec::new();
    if query.is_empty() {
        return (matches, false);
    }
    // Compared character by character so a column is a character count and
    // case folding cannot shift it: lowercasing a whole line can change its
    // length ("İ" is two characters lowercased), which would point the column
    // at the wrong place.
    let fold = |c: char| -> char {
        if case_sensitive {
            return c;
        }
        let mut lower = c.to_lowercase();
        match (lower.next(), lower.next()) {
            (Some(one), None) => one,
            _ => c,
        }
    };
    let needle: Vec<char> = query.chars().map(fold).collect();
    // `border[i]`: the length of the longest proper prefix of
    // `needle[..=i]` that is also its suffix.
    let mut border = vec![0usize; needle.len()];
    let mut k = 0;
    for i in 1..needle.len() {
        while k > 0 && needle[i] != needle[k] {
            k = border[k - 1];
        }
        if needle[i] == needle[k] {
            k += 1;
        }
        border[i] = k;
    }
    for (index, line) in text.lines().enumerate() {
        // Counted once per line, and only for a line that matched.
        let mut count = None;
        let mut k = 0;
        for (at, (byte, original)) in line.char_indices().enumerate() {
            let c = fold(original);
            while k > 0 && c != needle[k] {
                k = border[k - 1];
            }
            if c == needle[k] {
                k += 1;
            }
            if k < needle.len() {
                continue;
            }
            if matches.len() == max {
                return (matches, true);
            }
            let column = at + 1 - needle.len();
            let count = *count.get_or_insert_with(|| line.chars().count());
            matches.push(Match {
                line: index + 1,
                column: column + 1,
                text: excerpt(
                    line,
                    byte + original.len_utf8(),
                    needle.len(),
                    column,
                    count,
                ),
            });
            // Matches do not overlap: the next starts after this one.
            k = 0;
        }
    }
    (matches, false)
}

/// A long line cut to [`FIND_CONTEXT`] characters with the match in view.
///
/// `end` is the byte just past the match, `len` its length and `at` its
/// first character's index, all known to the caller; the cut is found by
/// walking back from the match rather than forward from the start of what
/// may be a 16 MB line, once per match.
fn excerpt(line: &str, end: usize, len: usize, at: usize, count: usize) -> String {
    if count <= FIND_CONTEXT {
        return line.to_string();
    }
    let start = at.saturating_sub(FIND_CONTEXT / 4);
    let back = at + len - start;
    let from = line[..end]
        .char_indices()
        .rev()
        .nth(back - 1)
        .map_or(0, |(i, _)| i);
    let body: String = line[from..].chars().take(FIND_CONTEXT).collect();
    let lead = if start > 0 { "…" } else { "" };
    let tail = if start + FIND_CONTEXT < count {
        "…"
    } else {
        ""
    };
    format!("{lead}{body}{tail}")
}

fn kib(bytes: u64) -> String {
    if bytes >= 1024 * 1024 {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
    } else {
        format!("{} KB", bytes.div_ceil(1024))
    }
}

/// Not text, said so a person knows where to go instead.
fn not_text(path: &NodePath) -> FileError {
    FileError::Io {
        path: path.path.clone(),
        source: std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "is not text (it is not valid UTF-8), so the editor will not open it; \
             preview it with Quick Look instead",
        ),
    }
}

/// Read a file the way the editor opens it.
pub fn open(
    files: &Namespace,
    address: &NodePath,
    before: Option<u64>,
) -> Result<Opened, FileError> {
    let entry = files.stat(address)?;
    let path = files.qualify(&entry.path);
    let size = entry.len;
    let base = |mode, reason, text, version, window| Opened {
        path: path.clone(),
        name: entry.name.clone(),
        size,
        modified: entry.modified,
        mode,
        reason,
        text,
        version,
        window,
        save_limit: SAVE_FRAME_LIMIT,
    };

    match plan(size, before) {
        Plan::Tail { end } => {
            let start = end.saturating_sub(TAIL_WINDOW);
            let bytes = files.root().read_range(&entry.path, start, end - start)?;
            // The whole-file branch refuses bytes that are not UTF-8; a tail
            // is decoded leniently because it may start mid-character, so a
            // video or an archive would otherwise open as a page of
            // replacement characters instead of being sent to Quick Look.
            if looks_binary(&bytes) {
                return Err(not_text(&path));
            }
            let (text, start) = window_text(bytes, start);
            let reason = format!(
                "{} is too large to open whole, so this is {} of it, read-only. \
                 Load earlier parts to page back.",
                kib(size),
                if end == size { "the end" } else { "a part" },
            );
            Ok(base(
                Mode::ReadOnly,
                Some(reason),
                text,
                None,
                Some(Window { start, end }),
            ))
        }
        whole => {
            let bytes = files.read(address, WHOLE_LIMIT)?;
            let version = version_of(&bytes);
            let text = String::from_utf8(bytes).map_err(|_| not_text(&path))?;
            let (mode, reason) = if whole == Plan::WholeReadOnly {
                (
                    Mode::ReadOnly,
                    Some(format!(
                        "{} is more than can be saved back from the browser ({} at most), \
                         so it is read-only here",
                        kib(size),
                        kib(EDIT_LIMIT),
                    )),
                )
            } else if entry.readonly {
                (
                    Mode::ReadOnly,
                    Some("the file is read-only on disk".to_string()),
                )
            } else {
                (Mode::Edit, None)
            };
            Ok(base(mode, reason, text, Some(version), None))
        }
    }
}

/// What is on disk at `address` now, or nothing.
fn on_disk(
    files: &Namespace,
    address: &NodePath,
) -> Result<Option<(String, u64, Option<u64>)>, FileError> {
    match files.stat(address) {
        Ok(entry) => {
            let bytes = files.read(address, files::DEFAULT_MAX_READ)?;
            Ok(Some((version_of(&bytes), entry.len, entry.modified)))
        }
        Err(FileError::NotFound { .. }) => Ok(None),
        Err(e) => Err(e),
    }
}

/// Save, unless the file is not what the caller last saw.
pub fn save(files: &Namespace, params: &SaveParams) -> Result<Saved, FileError> {
    let address = &params.path;
    if !params.force {
        let now = on_disk(files, address)?;
        let conflict = match (&params.expected, &now) {
            (Some(expected), Some((version, _, _))) if expected == version => None,
            (None, None) => None,
            (Some(_), Some(_)) => Some(
                "the file changed on disk since it was opened; reload it, or save anyway \
                 to replace those changes",
            ),
            (Some(_), None) => Some(
                "the file was deleted or moved on disk since it was opened; save anyway to \
                 write it again",
            ),
            (None, Some(_)) => {
                Some("a file with that name already exists; open it, or save anyway to replace it")
            }
        };
        if let Some(message) = conflict {
            let (version, size, modified) = match now {
                Some((v, s, m)) => (Some(v), Some(s), m),
                None => (None, None, None),
            };
            return Ok(Saved {
                status: SaveStatus::Conflict,
                path: files.qualify(&address.path),
                version,
                size,
                modified,
                message: Some(message.to_string()),
            });
        }
    }

    let written = files.write(address, params.content.as_bytes())?;
    let entry = files.stat(&written)?;
    Ok(Saved {
        status: SaveStatus::Saved,
        path: written,
        version: Some(version_of(params.content.as_bytes())),
        size: Some(entry.len),
        modified: entry.modified,
        message: None,
    })
}

/// Search a file for literal text.
pub fn find(files: &Namespace, params: &FindParams) -> Result<Found, FileError> {
    let entry = files.stat(&params.path)?;
    let path = files.qualify(&entry.path);
    let bytes = files.read(&params.path, files::DEFAULT_MAX_READ)?;
    let text = String::from_utf8(bytes).map_err(|_| not_text(&path))?;
    let max = params.max.unwrap_or(FIND_DEFAULT).clamp(1, FIND_MAX);
    let (matches, truncated) = find_in(&text, &params.query, params.case_sensitive, max);
    Ok(Found {
        path,
        matches,
        truncated,
    })
}

// ---------------------------------------------------------------------------
// The commands
// ---------------------------------------------------------------------------

pub struct OpenFile {
    files: Arc<Namespace>,
}

impl Command for OpenFile {
    const ID: &'static str = "editor.open";
    const TITLE: &'static str = "Open in the editor";
    const RISK: RiskLevel = RiskLevel::ReadOnly;

    type Params = OpenParams;
    type Output = Opened;

    async fn run(&self, params: OpenParams, cx: Context) -> Result<Opened, CommandError> {
        let files = Arc::clone(&self.files);
        off_thread(cx.command.clone(), move || {
            open(&files, &params.path, params.before)
        })
        .await
    }
}

/// The same read as `editor.open`, named for what the operator is doing:
/// throwing their changes away for what is on disk. The buffer replacement is
/// the UI's, and it does it as one undoable change, so a revert is itself
/// undoable.
pub struct RevertFile {
    files: Arc<Namespace>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RevertParams {
    pub path: NodePath,
}

impl Command for RevertFile {
    const ID: &'static str = "editor.revert";
    const TITLE: &'static str = "Revert to what is on disk";
    const RISK: RiskLevel = RiskLevel::ReadOnly;

    type Params = RevertParams;
    type Output = Opened;

    async fn run(&self, params: RevertParams, cx: Context) -> Result<Opened, CommandError> {
        let files = Arc::clone(&self.files);
        off_thread(cx.command.clone(), move || open(&files, &params.path, None)).await
    }
}

pub struct SaveFile {
    files: Arc<Namespace>,
}

impl Command for SaveFile {
    const ID: &'static str = "editor.save";
    const TITLE: &'static str = "Save from the editor";
    /// `write`, like `files.write`, for the reason `files.rs` gives at length:
    /// nothing reachable here is outside the working directory, and an
    /// operator confirming every save is a prompt nobody reads by the tenth.
    const RISK: RiskLevel = RiskLevel::Write;
    /// The parameters are a file's contents. Same reason as `files.write`.
    const REDACT_PARAMS: bool = true;

    type Params = SaveParams;
    type Output = Saved;

    async fn run(&self, params: SaveParams, cx: Context) -> Result<Saved, CommandError> {
        if params.content.len() as u64 > MAX_WRITE {
            return Err(CommandError::BadParams {
                command: cx.command.clone(),
                message: format!(
                    "{} bytes is over the {MAX_WRITE} byte limit for a save",
                    params.content.len()
                ),
            });
        }
        let files = Arc::clone(&self.files);
        off_thread(cx.command.clone(), move || save(&files, &params)).await
    }
}

pub struct FindInFile {
    files: Arc<Namespace>,
}

impl Command for FindInFile {
    const ID: &'static str = "editor.find";
    const TITLE: &'static str = "Find in a file";
    const RISK: RiskLevel = RiskLevel::ReadOnly;

    type Params = FindParams;
    type Output = Found;

    async fn run(&self, params: FindParams, cx: Context) -> Result<Found, CommandError> {
        let length = params.query.chars().count();
        if length > FIND_QUERY_MAX {
            return Err(CommandError::BadParams {
                command: cx.command.clone(),
                message: format!(
                    "a {length}-character query is over the {FIND_QUERY_MAX}-character limit \
                     for a search"
                ),
            });
        }
        let files = Arc::clone(&self.files);
        off_thread(cx.command.clone(), move || find(&files, &params)).await
    }
}

/// Put the editor's commands on the bus, over the file namespace.
///
/// Called by [`files::register_commands`] itself rather than by `main`, so
/// there is no way to have the file commands without these: the editor's UI
/// calls them, and a core that registered one and not the other would serve
/// an editor that opens nothing.
pub fn register_commands(
    registry: &crate::command::Registry,
    files: &Arc<Namespace>,
) -> Result<(), RegistryError> {
    registry.register(OpenFile {
        files: Arc::clone(files),
    })?;
    registry.register(SaveFile {
        files: Arc::clone(files),
    })?;
    registry.register(RevertFile {
        files: Arc::clone(files),
    })?;
    registry.register(FindInFile {
        files: Arc::clone(files),
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_plan_is_edit_then_whole_then_tail() {
        assert_eq!(plan(0, None), Plan::Edit);
        assert_eq!(plan(EDIT_LIMIT, None), Plan::Edit);
        assert_eq!(plan(EDIT_LIMIT + 1, None), Plan::WholeReadOnly);
        assert_eq!(plan(WHOLE_LIMIT, None), Plan::WholeReadOnly);
        assert_eq!(
            plan(WHOLE_LIMIT + 1, None),
            Plan::Tail {
                end: WHOLE_LIMIT + 1
            }
        );
        // Paging back asks for the window ending earlier, never past the end.
        assert_eq!(plan(WHOLE_LIMIT * 2, Some(100)), Plan::Tail { end: 100 });
        assert_eq!(
            plan(WHOLE_LIMIT * 2, Some(WHOLE_LIMIT * 3)),
            Plan::Tail {
                end: WHOLE_LIMIT * 2
            }
        );
    }

    #[test]
    fn the_save_limit_is_the_sockets_own() {
        assert_eq!(
            SAVE_FRAME_LIMIT,
            crate::ws::Settings::default().max_frame_bytes
        );
    }

    #[test]
    fn an_editable_file_fits_in_the_frame_a_save_travels_in() {
        // Escaped, the worst ordinary source file is well under 4/3 of its
        // size; what must hold is that the limit leaves real room.
        assert!(EDIT_LIMIT < SAVE_FRAME_LIMIT as u64);
        assert!(SAVE_FRAME_LIMIT as u64 - EDIT_LIMIT >= 32 * 1024);
    }

    #[test]
    fn a_window_mid_file_starts_on_a_line() {
        let (text, start) = window_text(b"tail of a line\nnext\nlast\n".to_vec(), 100);
        assert_eq!(text, "next\nlast\n");
        assert_eq!(start, 115);
        // From the start of the file nothing is dropped.
        let (text, start) = window_text(b"first\nsecond".to_vec(), 0);
        assert_eq!(text, "first\nsecond");
        assert_eq!(start, 0);
    }

    #[test]
    fn a_window_inside_one_huge_line_is_kept_from_a_character_boundary() {
        // "é" is 0xC3 0xA9; a window starting on the 0xA9 must not begin with
        // a replacement character, and must not be empty either.
        let mut bytes = vec![0xA9];
        bytes.extend("ébc".as_bytes());
        let (text, start) = window_text(bytes, 10);
        assert_eq!(text, "ébc");
        assert_eq!(start, 11);
    }

    #[test]
    fn find_reports_lines_and_character_columns() {
        let text = "alpha beta\nßeta Beta\nnone\n";
        let (found, truncated) = find_in(text, "beta", false, 10);
        assert!(!truncated);
        let at: Vec<(usize, usize)> = found.iter().map(|m| (m.line, m.column)).collect();
        assert_eq!(at, vec![(1, 7), (2, 6)]);
        // Case-sensitive finds only the exact spelling.
        let (found, _) = find_in(text, "Beta", true, 10);
        assert_eq!(found.len(), 1);
        assert_eq!((found[0].line, found[0].column), (2, 6));
    }

    #[test]
    fn find_stops_at_its_cap_and_says_so() {
        let text = "x\n".repeat(50);
        let (found, truncated) = find_in(&text, "x", true, 10);
        assert_eq!(found.len(), 10);
        assert!(truncated);
        let (found, truncated) = find_in(&text, "x", true, 50);
        assert_eq!(found.len(), 50);
        assert!(!truncated);
    }

    #[test]
    fn find_with_nothing_to_find_finds_nothing() {
        assert!(find_in("anything", "", false, 10).0.is_empty());
    }

    #[test]
    fn a_long_matched_line_is_cut_with_the_match_in_view() {
        let line = format!("{}needle{}", "a".repeat(500), "b".repeat(500));
        let (found, _) = find_in(&line, "needle", true, 1);
        let shown = &found[0].text;
        assert!(shown.contains("needle"), "{shown}");
        assert!(shown.chars().count() <= FIND_CONTEXT + 2);
        assert!(shown.starts_with('…') && shown.ends_with('…'));
    }

    #[test]
    fn a_window_whose_only_newline_is_its_last_byte_is_not_empty() {
        // Paging back through a log whose chunk is one long line ending in a
        // newline: dropping "everything up to the first newline" would drop
        // everything, and "Load earlier" would never get anywhere.
        let (text, start) = window_text(b"one long line\n".to_vec(), 100);
        assert_eq!(text, "one long line\n");
        assert_eq!(start, 100);
    }

    #[test]
    fn find_does_not_lose_a_match_that_starts_inside_a_near_miss() {
        // "aab" in "aaab": the first two "a"s look like the start of a match
        // until the third character; the match begins at the second "a".
        let (found, _) = find_in("aaab\nxabababcx\n", "aab", true, 10);
        assert_eq!(
            found.iter().map(|m| (m.line, m.column)).collect::<Vec<_>>(),
            vec![(1, 2)]
        );
        let (found, _) = find_in("xabababcx\n", "ababc", true, 10);
        assert_eq!(
            found.iter().map(|m| (m.line, m.column)).collect::<Vec<_>>(),
            vec![(1, 4)]
        );
        // Matches do not overlap: "aa" in "aaaa" is two, not three.
        assert_eq!(find_in("aaaa", "aa", true, 10).0.len(), 2);
    }

    #[test]
    fn find_agrees_with_checking_every_position_on_every_small_case() {
        // Every text and query over a two-letter alphabet up to a size, which
        // is where a search that skips ahead on a near miss gets it wrong.
        fn spell(mut n: u32, len: u32) -> String {
            (0..len)
                .map(|_| {
                    let c = if n & 1 == 0 { 'a' } else { 'b' };
                    n >>= 1;
                    c
                })
                .collect()
        }
        let slow = |hay: &str, needle: &str| {
            let hay: Vec<char> = hay.chars().collect();
            let needle: Vec<char> = needle.chars().collect();
            let mut at = 0;
            let mut out = Vec::new();
            while at + needle.len() <= hay.len() {
                if hay[at..at + needle.len()] == needle[..] {
                    out.push(at + 1);
                    at += needle.len();
                } else {
                    at += 1;
                }
            }
            out
        };
        for qlen in 1..=7 {
            for q in 0..(1u32 << qlen) {
                let needle = spell(q, qlen);
                for tlen in 0..=11 {
                    for t in 0..(1u32 << tlen) {
                        let hay = spell(t, tlen);
                        let fast: Vec<usize> = find_in(&hay, &needle, true, 100)
                            .0
                            .iter()
                            .map(|m| m.column)
                            .collect();
                        assert_eq!(fast, slow(&hay, &needle), "{needle:?} in {hay:?}");
                    }
                }
            }
        }
    }

    #[test]
    fn find_cuts_every_match_on_one_huge_line_around_that_match() {
        let mut line = "x".repeat(4 * 1024 * 1024);
        line.push_str(&"needle ".repeat(10));
        let (found, truncated) = find_in(&line, "needle", true, 5);
        assert!(truncated);
        assert_eq!(found.len(), 5);
        let at = 4 * 1024 * 1024 + 7;
        assert_eq!(found[1].column, at + 1);
        // The cut starts a quarter of the context before the match.
        let expected: String = line
            .chars()
            .skip(at - FIND_CONTEXT / 4)
            .take(FIND_CONTEXT)
            .collect();
        assert_eq!(found[1].text, format!("…{expected}"));
    }

    #[test]
    fn versions_tell_contents_apart() {
        assert_eq!(version_of(b"a"), version_of(b"a"));
        assert_ne!(version_of(b"a"), version_of(b"b"));
        assert_eq!(version_of(b"").len(), 32);
    }
}
