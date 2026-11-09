//! The file layer: the working directory as the filesystem apps see.
//!
//! Apps see the working directory as their filesystem root, and `.deckjack/`
//! inside it is never exposed — not through the file API, WebDAV, search,
//! previews, downloads or agent tools. The terminal is the one documented
//! escape hatch, and it does not come through here.
//!
//! **Why the API is shaped this way.** The predecessor project took an
//! absolute path from the client, canonicalized it, and used it: the
//! configured root was decorative. So in this module there is no function
//! anywhere that accepts a host path. A caller names a [`VirtualPath`], which
//! cannot hold `..` or an absolute prefix, and the only thing that can turn one
//! into a host path is [`Root::resolve`] (or [`Root::resolve_link`]), which
//! canonicalizes first and then proves the result is inside the root and
//! outside `.deckjack/`. Those two return a [`Resolved`], which has no public
//! constructor. Skipping the check is not something a caller can do by
//! forgetting; it would take editing this file.
//!
//! **Why canonicalize before the containment test.** A symlink is otherwise a
//! one-line escape: `ln -s / out` makes `/out/etc/shadow` a path whose
//! components are all innocent. Containment is therefore tested on the
//! canonical path, and `.deckjack/` is tested twice — once on the requested
//! components (cheap, before touching the disk) and once on the canonical
//! result, because `ln -s .deckjack notes` is exactly the "symlink that points
//! into it" the design forbids.
//!
//! **Why a final symlink is not followed for writes.** Resolution for create,
//! write, delete and rename stops at the parent directory, so deleting a link
//! unlinks the link rather than its target. Writing through an existing link
//! is refused outright: the link may be the owner's own shortcut to somewhere
//! legitimate, but the file API is not the place to discover which.
//!
//! **Cross-node names.** [`NodePath`] is the `node:path` namespace from the
//! design, so panels and commands carry a parsed address rather than a raw
//! host path. [`Namespace`] says whether an address is this node's work or
//! another node's; actually fetching another node's bytes needs the node
//! registry, which does not exist yet (see the TODO on
//! [`Target::Remote`]).
//!
//! **The commands.** [`register_commands`] puts the `files.*` namespace on the
//! command bus and hands back the [`Namespace`] it built, so the HTTP file
//! routes, WebDAV and previews share one boundary with the bus rather than
//! each opening a [`Root`] of their own. The commands take a [`NodePath`] and
//! return [`FileInfo`], which is flat and directly renderable by a file
//! browser. Their risk levels are argued where they are declared; the short
//! version is that everything reachable through them is inside the working
//! directory by construction, which is the condition the design attaches
//! confirmation to.
//!
//! ```no_run
//! use deckjack_core::files::{ListOptions, NodePath, Namespace, Root};
//!
//! let root = Root::open(std::path::Path::new("/srv/deckjack"))?;
//! let ns = Namespace::new("r620".parse()?, root);
//!
//! // What a panel sends over the bus.
//! let addr: NodePath = "r620:/photos/2026".parse()?;
//! let listing = ns.list(&addr, &ListOptions::default())?;
//! for entry in &listing.entries {
//!     println!("{} {}", entry.kind.as_str(), ns.qualify(&entry.path));
//! }
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```

use std::fmt;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::str::FromStr;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::command::{
    Command, CommandError, CommandId, Context, Registry, RegistryError, RiskLevel,
};

/// The hidden directory inside the working directory. Named here rather than
/// derived from [`crate::store::DB_PATH`], because the rule is about the whole
/// directory and holds even once it contains more than the database.
pub const HIDDEN_DIR: &str = ".deckjack";

/// Longest single path component accepted. Linux's own limit is 255 bytes on
/// every filesystem worth supporting, so a longer name cannot exist and asking
/// for one is a malformed request, not a missing file.
pub const MAX_NAME_LEN: usize = 255;

/// Longest whole path accepted, matching Linux's `PATH_MAX`.
pub const MAX_PATH_LEN: usize = 4096;

/// Deepest path accepted. Nothing legitimate is this deep; the cap exists so a
/// generated or hostile path cannot make resolution do unbounded work.
pub const MAX_DEPTH: usize = 128;

/// Default cap on a whole-file read into memory. Previews, editors and the
/// JSON file API all go through [`Root::read`]; anything bigger than this
/// should be streamed from [`Resolved::real_path`] instead of buffered.
pub const DEFAULT_MAX_READ: u64 = 16 * 1024 * 1024;

// ---------------------------------------------------------------------------
// Virtual paths
// ---------------------------------------------------------------------------

/// A path as apps see it: rooted at `/` inside the working directory.
///
/// Parsing is the only way to build one, and it rejects `..` outright rather
/// than normalizing it away. Normalizing would be wrong twice over: it quietly
/// answers a different question than the one asked, and with symlinks in the
/// path the lexical answer and the kernel's answer differ, so the check would
/// be testing a path that is never opened.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct VirtualPath {
    /// Plain names, no separators, no `.` or `..`. Empty means the root.
    parts: Vec<String>,
}

impl VirtualPath {
    /// The working directory itself.
    pub fn root() -> VirtualPath {
        VirtualPath { parts: Vec::new() }
    }

    pub fn parse(text: &str) -> Result<VirtualPath, PathError> {
        if text.len() > MAX_PATH_LEN {
            return Err(PathError::TooLong {
                len: text.len(),
                limit: MAX_PATH_LEN,
            });
        }
        // A NUL ends a path at the syscall boundary, so a name containing one
        // would address a different file than the one that was checked.
        if text.contains('\0') {
            return Err(PathError::Nul);
        }

        let mut parts = Vec::new();
        for segment in text.split('/') {
            match segment {
                // Repeated and trailing separators, and a leading one, carry
                // no meaning here; `.` is a no-op the caller may have typed.
                "" | "." => continue,
                ".." => return Err(PathError::Traversal),
                name => {
                    if name.len() > MAX_NAME_LEN {
                        return Err(PathError::NameTooLong {
                            name: name.to_string(),
                            limit: MAX_NAME_LEN,
                        });
                    }
                    parts.push(name.to_string());
                }
            }
        }

        if parts.len() > MAX_DEPTH {
            return Err(PathError::TooDeep {
                depth: parts.len(),
                limit: MAX_DEPTH,
            });
        }
        Ok(VirtualPath { parts })
    }

    /// Build one from a relative host path, for reporting back what resolution
    /// actually landed on.
    ///
    /// Non-UTF-8 names are refused rather than lossily converted: a name that
    /// cannot survive the JSON API is a name this layer must not hand out, or
    /// a later request would address a file that does not exist.
    fn from_relative(rel: &Path) -> Result<VirtualPath, PathError> {
        let mut parts = Vec::new();
        for component in rel.components() {
            match component {
                Component::Normal(name) => match name.to_str() {
                    Some(name) => parts.push(name.to_string()),
                    None => {
                        return Err(PathError::NonUtf8 {
                            path: rel.to_path_buf(),
                        });
                    }
                },
                Component::CurDir => continue,
                // `canonicalize` has already removed these; seeing one means
                // the assumption behind containment no longer holds.
                _ => {
                    return Err(PathError::NotRelative {
                        path: rel.to_path_buf(),
                    });
                }
            }
        }
        Ok(VirtualPath { parts })
    }

    pub fn is_root(&self) -> bool {
        self.parts.is_empty()
    }

    pub fn components(&self) -> impl Iterator<Item = &str> {
        self.parts.iter().map(String::as_str)
    }

    pub fn depth(&self) -> usize {
        self.parts.len()
    }

    /// The last component, or `None` for the root.
    pub fn name(&self) -> Option<&str> {
        self.parts.last().map(String::as_str)
    }

    /// The containing directory, or `None` for the root.
    pub fn parent(&self) -> Option<VirtualPath> {
        if self.parts.is_empty() {
            return None;
        }
        Some(VirtualPath {
            parts: self.parts[..self.parts.len() - 1].to_vec(),
        })
    }

    /// Append one plain name.
    pub fn join(&self, name: &str) -> Result<VirtualPath, PathError> {
        if name.is_empty() || name == "." {
            return Err(PathError::EmptyName);
        }
        if name == ".." {
            return Err(PathError::Traversal);
        }
        if name.contains('/') || name.contains('\0') {
            return Err(PathError::NotAName {
                name: name.to_string(),
            });
        }
        if name.len() > MAX_NAME_LEN {
            return Err(PathError::NameTooLong {
                name: name.to_string(),
                limit: MAX_NAME_LEN,
            });
        }
        if self.parts.len() + 1 > MAX_DEPTH {
            return Err(PathError::TooDeep {
                depth: self.parts.len() + 1,
                limit: MAX_DEPTH,
            });
        }
        let mut parts = self.parts.clone();
        parts.push(name.to_string());
        Ok(VirtualPath { parts })
    }

    /// Whether this path is the hidden directory or inside it.
    ///
    /// Any component counts, not only the first. A nested working directory
    /// has its own `.deckjack/state.db`, and the rule is about hidden state
    /// wherever it sits rather than about one position in the tree. The cost
    /// is that a directory the owner happened to name `.deckjack` is
    /// unreachable through this layer; the terminal still reaches it.
    pub fn is_hidden_state(&self) -> bool {
        self.parts.iter().any(|p| p == HIDDEN_DIR)
    }
}

impl fmt::Display for VirtualPath {
    /// Always leading-slash form, so the output of this parses back to the
    /// same path even when the first component contains a colon.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("/")?;
        for (i, part) in self.parts.iter().enumerate() {
            if i > 0 {
                f.write_str("/")?;
            }
            f.write_str(part)?;
        }
        Ok(())
    }
}

impl FromStr for VirtualPath {
    type Err = PathError;
    fn from_str(s: &str) -> Result<VirtualPath, PathError> {
        VirtualPath::parse(s)
    }
}

impl TryFrom<String> for VirtualPath {
    type Error = PathError;
    fn try_from(s: String) -> Result<VirtualPath, PathError> {
        VirtualPath::parse(&s)
    }
}

impl From<VirtualPath> for String {
    fn from(p: VirtualPath) -> String {
        p.to_string()
    }
}

// ---------------------------------------------------------------------------
// node:path
// ---------------------------------------------------------------------------

/// A node's name in the cross-node namespace.
///
/// Node ids are URL-safe base64 (see [`crate::node::id`]), so `-` and `_` are
/// allowed and case is significant: lowercasing a name here would turn one
/// node's id into nothing at all.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct NodeName(String);

impl NodeName {
    pub const MAX_LEN: usize = 63;

    pub fn parse(text: &str) -> Result<NodeName, PathError> {
        if text.is_empty() {
            return Err(PathError::EmptyNode);
        }
        if text.len() > NodeName::MAX_LEN {
            return Err(PathError::NodeTooLong {
                node: text.to_string(),
                limit: NodeName::MAX_LEN,
            });
        }
        let ok = text
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'));
        // A leading dash or dot reads as a flag or a relative path wherever
        // the name is interpolated, so it is refused at the door.
        if !ok || text.starts_with('-') || text.starts_with('.') {
            return Err(PathError::BadNode {
                node: text.to_string(),
            });
        }
        Ok(NodeName(text.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for NodeName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl FromStr for NodeName {
    type Err = PathError;
    fn from_str(s: &str) -> Result<NodeName, PathError> {
        NodeName::parse(s)
    }
}

impl TryFrom<String> for NodeName {
    type Error = PathError;
    fn try_from(s: String) -> Result<NodeName, PathError> {
        NodeName::parse(&s)
    }
}

impl From<NodeName> for String {
    fn from(n: NodeName) -> String {
        n.0
    }
}

/// An address in the one namespace across nodes: `node:/path`, or `/path` for
/// wherever the address is being interpreted.
///
/// Panels and commands carry this rather than a host path, which is what keeps
/// "panels never handle raw node addresses" true in the type system instead of
/// only in review.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct NodePath {
    /// `None` means unqualified: this node, or whichever node is reading it.
    pub node: Option<NodeName>,
    pub path: VirtualPath,
}

impl NodePath {
    /// Parse `node:/path` or a bare `/path`.
    ///
    /// A colon only introduces a node when it appears before the first
    /// separator, so `/notes/a:b.txt` is a local file and not node `/notes/a`.
    /// Ambiguity is only possible for a path written without its leading
    /// slash; [`fmt::Display`] always writes the slash, so anything this layer
    /// produces round-trips.
    pub fn parse(text: &str) -> Result<NodePath, PathError> {
        if text.len() > MAX_PATH_LEN + NodeName::MAX_LEN + 1 {
            return Err(PathError::TooLong {
                len: text.len(),
                limit: MAX_PATH_LEN,
            });
        }
        let head = match text.find('/') {
            Some(slash) => &text[..slash],
            None => text,
        };
        match head.find(':') {
            Some(colon) => {
                let node = NodeName::parse(&head[..colon])?;
                Ok(NodePath {
                    node: Some(node),
                    path: VirtualPath::parse(&text[colon + 1..])?,
                })
            }
            None => Ok(NodePath {
                node: None,
                path: VirtualPath::parse(text)?,
            }),
        }
    }

    pub fn local(path: VirtualPath) -> NodePath {
        NodePath { node: None, path }
    }

    pub fn on(node: NodeName, path: VirtualPath) -> NodePath {
        NodePath {
            node: Some(node),
            path,
        }
    }
}

impl fmt::Display for NodePath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.node {
            Some(node) => write!(f, "{node}:{}", self.path),
            None => write!(f, "{}", self.path),
        }
    }
}

impl FromStr for NodePath {
    type Err = PathError;
    fn from_str(s: &str) -> Result<NodePath, PathError> {
        NodePath::parse(s)
    }
}

impl TryFrom<String> for NodePath {
    type Error = PathError;
    fn try_from(s: String) -> Result<NodePath, PathError> {
        NodePath::parse(&s)
    }
}

impl From<NodePath> for String {
    fn from(p: NodePath) -> String {
        p.to_string()
    }
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// A path or address that is malformed, before any disk is touched.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum PathError {
    #[error("paths may not contain '..'; name the location directly from the workspace root")]
    Traversal,

    #[error("paths may not contain a NUL byte")]
    Nul,

    #[error("path is {len} bytes, over the {limit} byte limit")]
    TooLong { len: usize, limit: usize },

    #[error("path is {depth} levels deep, over the {limit} level limit")]
    TooDeep { depth: usize, limit: usize },

    #[error("name {name:?} is longer than the {limit} byte limit every supported filesystem has")]
    NameTooLong { name: String, limit: usize },

    #[error("a name is required here")]
    EmptyName,

    #[error("{name:?} is a path, not a single name; join one component at a time")]
    NotAName { name: String },

    #[error("a node name is required before the ':'")]
    EmptyNode,

    #[error("node name {node:?} is longer than the {limit} character limit")]
    NodeTooLong { node: String, limit: usize },

    #[error("node name {node:?} is not valid; use letters, digits, '-', '_' or '.'")]
    BadNode { node: String },

    #[error(
        "{path} has a name that is not UTF-8, so it cannot be addressed through the file API; \
         rename it from the terminal to reach it"
    )]
    NonUtf8 { path: PathBuf },

    #[error("{path} is not a relative path; this is a bug in path resolution, not in your request")]
    NotRelative { path: PathBuf },
}

/// Everything the file layer refuses or cannot do.
#[derive(Debug, Error)]
pub enum FileError {
    #[error(transparent)]
    Path(#[from] PathError),

    #[error(
        "working directory {path} cannot be opened as the workspace root: {source}; \
         create it, or point Deckjack at a directory that exists"
    )]
    BadRoot {
        path: PathBuf,
        #[source]
        source: io::Error,
    },

    #[error(
        "{requested} resolves to {resolved}, which is outside the workspace; \
         refused. Use the terminal if you mean to reach outside the working directory"
    )]
    OutsideRoot {
        requested: VirtualPath,
        resolved: PathBuf,
    },

    #[error(
        "{requested} is inside {HIDDEN_DIR}/, which Deckjack never exposes; \
         use the terminal if you need to inspect system state"
    )]
    HiddenState { requested: VirtualPath },

    #[error("{path} does not exist")]
    NotFound { path: VirtualPath },

    #[error("{path} already exists; delete or rename it first")]
    Exists { path: VirtualPath },

    #[error("{path} is not a directory")]
    NotADirectory { path: VirtualPath },

    #[error("{path} is a directory")]
    IsADirectory { path: VirtualPath },

    #[error("{path} is not a regular file")]
    NotAFile { path: VirtualPath },

    #[error(
        "{path} is a directory and is not empty; pass recursive to delete it with its contents"
    )]
    DirectoryNotEmpty { path: VirtualPath },

    #[error(
        "{path} is a symbolic link; writing through it would land wherever it points, \
         so replace the link or write to its target by name"
    )]
    WriteThroughSymlink { path: VirtualPath },

    #[error("the workspace root itself cannot be {action}")]
    RootIsNotAFile { action: &'static str },

    #[error(
        "{path} is {len} bytes, over the {limit} byte limit for a buffered read; stream it instead"
    )]
    TooLarge {
        path: VirtualPath,
        len: u64,
        limit: u64,
    },

    #[error(
        "permission denied by the host on {path}; Deckjack acts as the host user and cannot grant itself more"
    )]
    Denied { path: VirtualPath },

    #[error("{address} names node {node}, which this node does not know; register it first")]
    UnknownNode { address: NodePath, node: NodeName },

    #[error(
        "{address} is on another node, and fetching another node's files needs the node registry \
         and the command bus, which this build does not have yet"
    )]
    NotLocal { address: NodePath },

    #[error("{path}: {source}")]
    Io {
        path: VirtualPath,
        #[source]
        source: io::Error,
    },
}

impl FileError {
    /// Whether this is a refusal by the boundary rather than a condition of
    /// the filesystem. Audit logging and the Inbox care about the difference:
    /// a missing file is routine, an attempt to leave the workspace is not.
    pub fn is_refusal(&self) -> bool {
        matches!(
            self,
            FileError::OutsideRoot { .. }
                | FileError::HiddenState { .. }
                | FileError::WriteThroughSymlink { .. }
                | FileError::Path(_)
        )
    }
}

// ---------------------------------------------------------------------------
// The filesystem, injected
// ---------------------------------------------------------------------------

/// What this module is allowed to do to the world.
///
/// Resolution is the security boundary, and a boundary that can only be
/// exercised by creating real symlinks on a real disk is a boundary that gets
/// tested once and then trusted. Injecting the primitives means every rule
/// above is reachable from a table of paths. [`OsFs`] is the real one; each of
/// its methods is a single `std::fs` call.
pub trait Fs: Send + Sync {
    /// Resolve every symlink and `.`/`..`, and fail if the path does not
    /// exist. The containment test depends on this having been done.
    fn canonicalize(&self, path: &Path) -> io::Result<PathBuf>;

    /// Metadata, following symlinks.
    fn meta(&self, path: &Path) -> io::Result<Meta>;

    /// Metadata of the path itself, not following a final symlink.
    fn link_meta(&self, path: &Path) -> io::Result<Meta>;

    /// Names in a directory, in whatever order the filesystem gives.
    fn read_dir(&self, path: &Path) -> io::Result<Vec<String>>;

    /// Up to `max` bytes starting at `offset`.
    fn read_range(&self, path: &Path, offset: u64, max: u64) -> io::Result<Vec<u8>>;

    /// Replace the file's contents in one step, so a reader never sees a
    /// half-written file and a crash leaves the old one.
    fn write_atomic(&self, path: &Path, bytes: &[u8]) -> io::Result<()>;

    fn create_dir(&self, path: &Path) -> io::Result<()>;

    /// Remove a file or a symlink, never what the symlink points at.
    fn remove_file(&self, path: &Path) -> io::Result<()>;

    fn remove_dir(&self, path: &Path) -> io::Result<()>;

    fn remove_dir_all(&self, path: &Path) -> io::Result<()>;

    fn rename(&self, from: &Path, to: &Path) -> io::Result<()>;
}

/// What this layer reports about a file. A deliberately small subset of
/// `std::fs::Metadata`: everything here survives JSON and means the same thing
/// on every host Deckjack runs on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Meta {
    pub kind: EntryKind,
    pub len: u64,
    /// Unix seconds. `None` when the filesystem has no usable timestamp,
    /// rather than a fabricated zero that sorts as 1970.
    pub modified: Option<u64>,
    pub readonly: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EntryKind {
    File,
    Dir,
    Symlink,
    /// A socket, device, fifo or anything else. Listed, never opened: reading
    /// a fifo through the file API would hang the request forever.
    Other,
}

impl EntryKind {
    pub fn as_str(self) -> &'static str {
        match self {
            EntryKind::File => "file",
            EntryKind::Dir => "dir",
            EntryKind::Symlink => "symlink",
            EntryKind::Other => "other",
        }
    }

    pub fn is_dir(self) -> bool {
        matches!(self, EntryKind::Dir)
    }

    pub fn is_file(self) -> bool {
        matches!(self, EntryKind::File)
    }
}

/// The real filesystem.
#[derive(Debug, Default, Clone, Copy)]
pub struct OsFs;

impl OsFs {
    fn convert(md: &std::fs::Metadata) -> Meta {
        let kind = if md.is_dir() {
            EntryKind::Dir
        } else if md.is_file() {
            EntryKind::File
        } else if md.is_symlink() {
            EntryKind::Symlink
        } else {
            EntryKind::Other
        };
        Meta {
            kind,
            len: md.len(),
            modified: md
                .modified()
                .ok()
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map(|d| d.as_secs()),
            readonly: md.permissions().readonly(),
        }
    }
}

impl Fs for OsFs {
    fn canonicalize(&self, path: &Path) -> io::Result<PathBuf> {
        std::fs::canonicalize(path)
    }

    fn meta(&self, path: &Path) -> io::Result<Meta> {
        Ok(OsFs::convert(&std::fs::metadata(path)?))
    }

    fn link_meta(&self, path: &Path) -> io::Result<Meta> {
        Ok(OsFs::convert(&std::fs::symlink_metadata(path)?))
    }

    fn read_dir(&self, path: &Path) -> io::Result<Vec<String>> {
        let mut names = Vec::new();
        for entry in std::fs::read_dir(path)? {
            let entry = entry?;
            match entry.file_name().into_string() {
                Ok(name) => names.push(name),
                // A name that is not UTF-8 cannot be sent to the browser or
                // asked for again, so listing it would offer the operator
                // something that does not work. It is logged instead of
                // dropped in silence; the terminal can still see it.
                Err(raw) => tracing::warn!(
                    directory = %path.display(),
                    name = %raw.to_string_lossy(),
                    "skipping directory entry whose name is not UTF-8"
                ),
            }
        }
        Ok(names)
    }

    fn read_range(&self, path: &Path, offset: u64, max: u64) -> io::Result<Vec<u8>> {
        use std::io::{Read as _, Seek as _, SeekFrom};
        let mut file = std::fs::File::open(path)?;
        if offset > 0 {
            file.seek(SeekFrom::Start(offset))?;
        }
        let mut buf = Vec::new();
        file.take(max).read_to_end(&mut buf)?;
        Ok(buf)
    }

    fn write_atomic(&self, path: &Path, bytes: &[u8]) -> io::Result<()> {
        use std::io::Write as _;
        // The temporary file must share the destination's directory: a rename
        // across filesystems is not atomic and on Linux is not even allowed.
        let dir = path.parent().unwrap_or(Path::new("."));
        let mut tmp = tempfile::NamedTempFile::new_in(dir)?;
        tmp.as_file_mut().write_all(bytes)?;
        tmp.as_file().sync_all()?;
        tmp.persist(path).map_err(|e| e.error)?;
        Ok(())
    }

    fn create_dir(&self, path: &Path) -> io::Result<()> {
        std::fs::create_dir(path)
    }

    fn remove_file(&self, path: &Path) -> io::Result<()> {
        std::fs::remove_file(path)
    }

    fn remove_dir(&self, path: &Path) -> io::Result<()> {
        std::fs::remove_dir(path)
    }

    fn remove_dir_all(&self, path: &Path) -> io::Result<()> {
        std::fs::remove_dir_all(path)
    }

    fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        std::fs::rename(from, to)
    }
}

// ---------------------------------------------------------------------------
// Resolution
// ---------------------------------------------------------------------------

/// A path that has passed the boundary: proven inside the workspace root and
/// outside `.deckjack/`.
///
/// There is no public constructor, and the host path is private. The only
/// sources are [`Root::resolve`] and [`Root::resolve_link`], which is what
/// makes "a caller cannot accidentally skip the check" a property of the type
/// rather than a convention.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolved {
    real: PathBuf,
    path: VirtualPath,
    /// Whether a final symlink was followed. `false` means `real` may itself
    /// be a link, which is what delete and rename want.
    followed: bool,
}

impl Resolved {
    /// The host path, for handing to a streaming reader or an external tool.
    pub fn real_path(&self) -> &Path {
        &self.real
    }

    /// Where this sits in the workspace, for showing a person or sending back
    /// over the bus.
    pub fn path(&self) -> &VirtualPath {
        &self.path
    }

    /// Whether a final symlink was followed during resolution.
    pub fn followed_symlink(&self) -> bool {
        self.followed
    }
}

/// The workspace root: one working directory, seen by apps as `/`.
///
/// Cheap to clone and shared across requests; the clone shares one [`Fs`].
#[derive(Clone)]
pub struct Root {
    /// Canonical, so the containment test compares like with like. A working
    /// directory reached through a symlink (`~/work` -> `/srv/deckjack`) would
    /// otherwise fail the test on every single path.
    real: PathBuf,
    fs: Arc<dyn Fs>,
}

impl fmt::Debug for Root {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Root").field("real", &self.real).finish()
    }
}

impl Root {
    /// Open a working directory as the workspace root.
    pub fn open(working_dir: &Path) -> Result<Root, FileError> {
        Root::with_fs(working_dir, Arc::new(OsFs))
    }

    pub fn with_fs(working_dir: &Path, fs: Arc<dyn Fs>) -> Result<Root, FileError> {
        let real = fs
            .canonicalize(working_dir)
            .map_err(|source| FileError::BadRoot {
                path: working_dir.to_path_buf(),
                source,
            })?;
        // Refusing a non-directory here means nothing below has to wonder.
        match fs.meta(&real) {
            Ok(meta) if meta.kind.is_dir() => {}
            Ok(_) => {
                return Err(FileError::BadRoot {
                    path: working_dir.to_path_buf(),
                    source: io::Error::new(io::ErrorKind::NotADirectory, "not a directory"),
                });
            }
            Err(source) => {
                return Err(FileError::BadRoot {
                    path: working_dir.to_path_buf(),
                    source,
                });
            }
        }
        Ok(Root { real, fs })
    }

    /// The canonical host path of the root. For logs and for the operator;
    /// never for building a path to open, which is what [`resolve`] is for.
    ///
    /// [`resolve`]: Root::resolve
    pub fn real_path(&self) -> &Path {
        &self.real
    }

    // -- the boundary ------------------------------------------------------

    /// Turn a workspace path into a usable host path.
    ///
    /// The path must exist. Symlinks are resolved first and the result is then
    /// proven inside the root and outside `.deckjack/`; a link that points
    /// anywhere else is refused, not followed.
    pub fn resolve(&self, path: &VirtualPath) -> Result<Resolved, FileError> {
        // Cheap check first: a request that spells out the hidden directory is
        // refused without a syscall, and without revealing whether it exists.
        self.reject_hidden(path)?;

        let candidate = self.join_real(path);
        let real = self
            .fs
            .canonicalize(&candidate)
            .map_err(|source| self.map_io(path, source))?;
        self.contain(path, real, true)
    }

    /// Turn a workspace path into a host path **without** following a final
    /// symlink.
    ///
    /// For create, delete and rename: the containing directory is resolved and
    /// contained as usual, and the last component is taken literally. That is
    /// what makes deleting a link unlink the link rather than its target, and
    /// it lets a path that does not exist yet be named.
    pub fn resolve_link(&self, path: &VirtualPath) -> Result<Resolved, FileError> {
        self.reject_hidden(path)?;

        let Some(name) = path.name() else {
            // The root has no parent to resolve, and it is never created,
            // deleted or renamed.
            return Ok(Resolved {
                real: self.real.clone(),
                path: VirtualPath::root(),
                followed: true,
            });
        };
        let parent = path.parent().unwrap_or_else(VirtualPath::root);

        // The parent is resolved the strict way, so the only unresolved
        // element is one plain name that cannot contain a separator.
        let parent = self.resolve(&parent)?;
        let parent_meta = self
            .fs
            .meta(parent.real_path())
            .map_err(|source| self.map_io(parent.path(), source))?;
        if !parent_meta.kind.is_dir() {
            return Err(FileError::NotADirectory {
                path: parent.path().clone(),
            });
        }

        let real = parent.real.join(name);
        // Joining one plain name onto a canonical contained directory cannot
        // leave the root, so there is nothing further to prove. Rebuilding the
        // virtual path from the parent's keeps it honest if the parent's own
        // canonical name differed from what was asked for.
        let path = parent.path.join(name)?;
        if path.is_hidden_state() {
            return Err(FileError::HiddenState { requested: path });
        }
        Ok(Resolved {
            real,
            path,
            followed: false,
        })
    }

    fn reject_hidden(&self, path: &VirtualPath) -> Result<(), FileError> {
        if path.is_hidden_state() {
            return Err(FileError::HiddenState {
                requested: path.clone(),
            });
        }
        Ok(())
    }

    /// Prove a canonical host path is inside the root and outside the hidden
    /// directory. This is the whole boundary; everything else is plumbing.
    fn contain(
        &self,
        requested: &VirtualPath,
        real: PathBuf,
        followed: bool,
    ) -> Result<Resolved, FileError> {
        // Component-wise, not string-wise: `/srv/deckjack-old` shares a string
        // prefix with `/srv/deckjack` and is a different directory.
        let rel = real
            .strip_prefix(&self.real)
            .map_err(|_| FileError::OutsideRoot {
                requested: requested.clone(),
                resolved: real.clone(),
            })?;

        let path = VirtualPath::from_relative(rel)?;
        // The second hidden check, on the canonical result. `ln -s .deckjack
        // notes` gives a requested path with no forbidden component whose
        // canonical form is inside the hidden directory, and that is precisely
        // the "symlink that points into it" the design rules out.
        if path.is_hidden_state() {
            return Err(FileError::HiddenState {
                requested: requested.clone(),
            });
        }
        Ok(Resolved {
            real,
            path,
            followed,
        })
    }

    fn join_real(&self, path: &VirtualPath) -> PathBuf {
        let mut out = self.real.clone();
        for part in path.components() {
            out.push(part);
        }
        out
    }

    fn map_io(&self, path: &VirtualPath, source: io::Error) -> FileError {
        match source.kind() {
            io::ErrorKind::NotFound => FileError::NotFound { path: path.clone() },
            io::ErrorKind::PermissionDenied => FileError::Denied { path: path.clone() },
            io::ErrorKind::NotADirectory => FileError::NotADirectory { path: path.clone() },
            io::ErrorKind::IsADirectory => FileError::IsADirectory { path: path.clone() },
            io::ErrorKind::AlreadyExists => FileError::Exists { path: path.clone() },
            io::ErrorKind::DirectoryNotEmpty => FileError::DirectoryNotEmpty { path: path.clone() },
            _ => FileError::Io {
                path: path.clone(),
                source,
            },
        }
    }

    // -- reading -----------------------------------------------------------

    /// What a path is, following symlinks.
    pub fn stat(&self, path: &VirtualPath) -> Result<Entry, FileError> {
        let resolved = self.resolve(path)?;
        let meta = self
            .fs
            .meta(resolved.real_path())
            .map_err(|source| self.map_io(resolved.path(), source))?;
        Ok(Entry {
            name: resolved.path().name().unwrap_or("/").to_string(),
            path: resolved.path().clone(),
            kind: meta.kind,
            len: meta.len,
            modified: meta.modified,
            readonly: meta.readonly,
            escapes: false,
        })
    }

    /// One directory's contents.
    ///
    /// Symlinks are reported as symlinks and never followed into: a link whose
    /// target is outside the workspace is listed with `escapes` set, so the UI
    /// can show it greyed out instead of the layer pretending it is not there.
    /// `.deckjack/` is left out whatever the options say.
    pub fn list(&self, dir: &VirtualPath, options: &ListOptions) -> Result<Listing, FileError> {
        let resolved = self.resolve(dir)?;
        let meta = self
            .fs
            .meta(resolved.real_path())
            .map_err(|source| self.map_io(resolved.path(), source))?;
        if !meta.kind.is_dir() {
            return Err(FileError::NotADirectory {
                path: resolved.path().clone(),
            });
        }

        let names = self
            .fs
            .read_dir(resolved.real_path())
            .map_err(|source| self.map_io(resolved.path(), source))?;

        let mut entries = Vec::new();
        let mut truncated = false;
        for name in names {
            let Ok(path) = resolved.path().join(&name) else {
                // A name the virtual namespace cannot hold (too long for this
                // filesystem's own rules, or at the depth cap) is not
                // addressable, so offering it would be a dead link.
                tracing::warn!(name = %name, "skipping unaddressable directory entry");
                continue;
            };
            if path.is_hidden_state() {
                continue;
            }
            if !options.show_dotfiles && name.starts_with('.') {
                continue;
            }
            if entries.len() >= options.max_entries {
                truncated = true;
                break;
            }

            let real = resolved.real.join(&name);
            let Ok(link) = self.fs.link_meta(&real) else {
                // Vanished between the read_dir and the stat, or unreadable.
                // A listing that drops one racing entry is right; failing the
                // whole listing because a build directory is churning is not.
                continue;
            };

            let mut entry = Entry {
                name,
                path: path.clone(),
                kind: link.kind,
                len: link.len,
                modified: link.modified,
                readonly: link.readonly,
                escapes: false,
            };

            if link.kind == EntryKind::Symlink {
                // Resolution is the authority on whether the link is usable,
                // so ask it rather than reimplementing containment here.
                match self.resolve(&path) {
                    Ok(target) => {
                        if let Ok(target_meta) = self.fs.meta(target.real_path()) {
                            entry.len = target_meta.len;
                            entry.modified = target_meta.modified.or(entry.modified);
                        }
                    }
                    Err(e) if e.is_refusal() => entry.escapes = true,
                    // Broken link: shown as a symlink with nothing behind it.
                    Err(_) => {}
                }
            }
            entries.push(entry);
        }

        // Directories first, then names case-insensitively: what every file
        // manager does, decided here so each panel does not decide differently.
        entries.sort_by(|a, b| {
            b.kind
                .is_dir()
                .cmp(&a.kind.is_dir())
                .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
                .then_with(|| a.name.cmp(&b.name))
        });

        Ok(Listing {
            path: resolved.path().clone(),
            entries,
            truncated,
        })
    }

    /// A whole file, up to `limit` bytes. Over that it is refused rather than
    /// silently truncated, because a half file looks like a corrupt file.
    pub fn read(&self, path: &VirtualPath, limit: u64) -> Result<Vec<u8>, FileError> {
        let resolved = self.resolve(path)?;
        let meta = self
            .fs
            .meta(resolved.real_path())
            .map_err(|source| self.map_io(resolved.path(), source))?;
        if !meta.kind.is_file() {
            return Err(FileError::NotAFile {
                path: resolved.path().clone(),
            });
        }
        if meta.len > limit {
            return Err(FileError::TooLarge {
                path: resolved.path().clone(),
                len: meta.len,
                limit,
            });
        }
        self.fs
            .read_range(resolved.real_path(), 0, limit)
            .map_err(|source| self.map_io(resolved.path(), source))
    }

    /// A byte range, for previews, range requests and WebDAV partial reads.
    pub fn read_range(
        &self,
        path: &VirtualPath,
        offset: u64,
        max: u64,
    ) -> Result<Vec<u8>, FileError> {
        let resolved = self.resolve(path)?;
        let meta = self
            .fs
            .meta(resolved.real_path())
            .map_err(|source| self.map_io(resolved.path(), source))?;
        if !meta.kind.is_file() {
            return Err(FileError::NotAFile {
                path: resolved.path().clone(),
            });
        }
        self.fs
            .read_range(resolved.real_path(), offset, max)
            .map_err(|source| self.map_io(resolved.path(), source))
    }

    // -- writing -----------------------------------------------------------

    /// Create or replace a file's contents in one step.
    pub fn write(&self, path: &VirtualPath, bytes: &[u8]) -> Result<Resolved, FileError> {
        let resolved = self.resolve_link(path)?;
        if resolved.path().is_root() {
            return Err(FileError::RootIsNotAFile { action: "written" });
        }
        match self.fs.link_meta(resolved.real_path()) {
            Ok(meta) => match meta.kind {
                EntryKind::File => {}
                EntryKind::Dir => {
                    return Err(FileError::IsADirectory {
                        path: resolved.path().clone(),
                    });
                }
                EntryKind::Symlink => {
                    return Err(FileError::WriteThroughSymlink {
                        path: resolved.path().clone(),
                    });
                }
                // Replacing a device or a fifo with a regular file is not
                // something a file panel should be able to do by accident.
                EntryKind::Other => {
                    return Err(FileError::NotAFile {
                        path: resolved.path().clone(),
                    });
                }
            },
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(source) => return Err(self.map_io(resolved.path(), source)),
        }

        self.fs
            .write_atomic(resolved.real_path(), bytes)
            .map_err(|source| self.map_io(resolved.path(), source))?;
        Ok(resolved)
    }

    /// Create one directory. The parent must already exist, so a typo makes
    /// one failed command rather than a tree of empty directories.
    pub fn create_dir(&self, path: &VirtualPath) -> Result<Resolved, FileError> {
        let resolved = self.resolve_link(path)?;
        if resolved.path().is_root() {
            return Err(FileError::Exists {
                path: VirtualPath::root(),
            });
        }
        self.fs
            .create_dir(resolved.real_path())
            .map_err(|source| self.map_io(resolved.path(), source))?;
        Ok(resolved)
    }

    /// Delete a file, a symlink, or a directory.
    ///
    /// A symlink is unlinked, never followed, so deleting a link cannot delete
    /// what it points at. A non-empty directory needs `recursive`.
    pub fn remove(&self, path: &VirtualPath, recursive: bool) -> Result<Resolved, FileError> {
        let resolved = self.resolve_link(path)?;
        if resolved.path().is_root() {
            return Err(FileError::RootIsNotAFile { action: "deleted" });
        }
        let meta = self
            .fs
            .link_meta(resolved.real_path())
            .map_err(|source| self.map_io(resolved.path(), source))?;

        let outcome = match meta.kind {
            EntryKind::Dir if recursive => self.fs.remove_dir_all(resolved.real_path()),
            EntryKind::Dir => self.fs.remove_dir(resolved.real_path()),
            _ => self.fs.remove_file(resolved.real_path()),
        };
        outcome.map_err(|source| self.map_io(resolved.path(), source))?;
        Ok(resolved)
    }

    /// Move or rename within the workspace.
    ///
    /// Both ends are resolved without following a final link, so moving a
    /// symlink moves the link. An existing destination is refused: silently
    /// replacing a file the operator forgot about is the one file-manager
    /// mistake with no undo.
    pub fn rename(
        &self,
        from: &VirtualPath,
        to: &VirtualPath,
    ) -> Result<(Resolved, Resolved), FileError> {
        let src = self.resolve_link(from)?;
        if src.path().is_root() {
            return Err(FileError::RootIsNotAFile { action: "renamed" });
        }
        let dst = self.resolve_link(to)?;
        if dst.path().is_root() {
            return Err(FileError::Exists {
                path: VirtualPath::root(),
            });
        }

        // Proof that the source exists, before anything about the destination
        // is reported: otherwise a mistyped source reads as a conflict.
        self.fs
            .link_meta(src.real_path())
            .map_err(|source| self.map_io(src.path(), source))?;

        match self.fs.link_meta(dst.real_path()) {
            Ok(_) => {
                return Err(FileError::Exists {
                    path: dst.path().clone(),
                });
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(source) => return Err(self.map_io(dst.path(), source)),
        }

        self.fs
            .rename(src.real_path(), dst.real_path())
            .map_err(|source| self.map_io(src.path(), source))?;
        Ok((src, dst))
    }
}

/// How much of a directory to return and what to leave out.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct ListOptions {
    /// Hard cap on one listing. A directory with a million entries must not be
    /// able to exhaust the server's memory on one panel opening. The cap is
    /// applied while reading, before sorting, so a truncated listing holds an
    /// arbitrary subset rather than the first page of a sorted one — the point
    /// is the memory bound, and [`Listing::truncated`] tells the UI to say so.
    pub max_entries: usize,
    /// Dotfiles are shown by default: this is the owner's own machine, and a
    /// file manager that hides their config directories is less useful than
    /// one that does not. `.deckjack/` is not a dotfile for this purpose and
    /// is never shown.
    pub show_dotfiles: bool,
}

impl Default for ListOptions {
    fn default() -> ListOptions {
        ListOptions {
            max_entries: 10_000,
            show_dotfiles: true,
        }
    }
}

/// One directory's contents.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Listing {
    pub path: VirtualPath,
    pub entries: Vec<Entry>,
    /// Set when [`ListOptions::max_entries`] cut the listing short, so the UI
    /// can say so rather than quietly showing part of a directory.
    pub truncated: bool,
}

/// One entry in a listing, or the answer to a stat.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    pub name: String,
    pub path: VirtualPath,
    pub kind: EntryKind,
    pub len: u64,
    /// Unix seconds, or `None` when the filesystem has no usable timestamp.
    pub modified: Option<u64>,
    pub readonly: bool,
    /// A symlink whose target is outside the workspace or inside
    /// `.deckjack/`. Listed so the operator can see and delete it; opening it
    /// is refused.
    pub escapes: bool,
}

// ---------------------------------------------------------------------------
// The cross-node namespace
// ---------------------------------------------------------------------------

/// Where an address points, once this node has read it.
#[derive(Debug)]
pub enum Target {
    /// This node's own workspace.
    Local(Resolved),
    /// Another node's workspace.
    ///
    /// TODO: on-demand fetch and local caching, per the design's file layer.
    /// That needs the node registry: which node is reachable, and how. The
    /// `files.*` commands to ask another node with are registered below, so
    /// what is missing is only the registry that says where to send them.
    /// Writing a fetch against an invented registry
    /// would be a stub that compiles and lies, so callers get this variant and
    /// [`FileError::NotLocal`] from [`Namespace::resolve`] instead. The cache
    /// will want a table of its own in `store.rs`; see integration notes.
    Remote { node: NodeName, path: VirtualPath },
}

/// One namespace across nodes: `node:path`.
///
/// Panels and the command bus address files through this, so nothing above
/// here holds a host path or decides which node a path belongs to.
#[derive(Debug, Clone)]
pub struct Namespace {
    local: NodeName,
    root: Root,
}

impl Namespace {
    pub fn new(local: NodeName, root: Root) -> Namespace {
        Namespace { local, root }
    }

    pub fn local_node(&self) -> &NodeName {
        &self.local
    }

    pub fn root(&self) -> &Root {
        &self.root
    }

    /// Whether an address names this node. An address with no node is this
    /// node's, since that is the node reading it.
    pub fn is_local(&self, address: &NodePath) -> bool {
        match &address.node {
            None => true,
            Some(node) => *node == self.local,
        }
    }

    /// Attach this node's name to a local path, so what goes back to the
    /// browser is a complete address and a panel never has to guess which node
    /// a result came from.
    pub fn qualify(&self, path: &VirtualPath) -> NodePath {
        NodePath::on(self.local.clone(), path.clone())
    }

    /// Point an address at something usable, or say which node to ask.
    pub fn target(&self, address: &NodePath) -> Result<Target, FileError> {
        if self.is_local(address) {
            return Ok(Target::Local(self.root.resolve(&address.path)?));
        }
        Ok(Target::Remote {
            node: address.node.clone().expect("non-local implies a node name"),
            path: address.path.clone(),
        })
    }

    /// The same, for create, delete and rename, which must not follow a final
    /// symlink.
    pub fn target_link(&self, address: &NodePath) -> Result<Target, FileError> {
        if self.is_local(address) {
            return Ok(Target::Local(self.root.resolve_link(&address.path)?));
        }
        Ok(Target::Remote {
            node: address.node.clone().expect("non-local implies a node name"),
            path: address.path.clone(),
        })
    }

    /// Resolve an address that must be this node's.
    ///
    /// The file operations below go through this, so a command aimed at
    /// another node fails with [`FileError::NotLocal`] rather than being
    /// quietly served from the wrong filesystem.
    pub fn resolve(&self, address: &NodePath) -> Result<Resolved, FileError> {
        match self.target(address)? {
            Target::Local(resolved) => Ok(resolved),
            Target::Remote { .. } => Err(FileError::NotLocal {
                address: address.clone(),
            }),
        }
    }

    fn require_local(&self, address: &NodePath) -> Result<(), FileError> {
        if self.is_local(address) {
            return Ok(());
        }
        Err(FileError::NotLocal {
            address: address.clone(),
        })
    }

    pub fn list(&self, address: &NodePath, options: &ListOptions) -> Result<Listing, FileError> {
        self.require_local(address)?;
        self.root.list(&address.path, options)
    }

    pub fn stat(&self, address: &NodePath) -> Result<Entry, FileError> {
        self.require_local(address)?;
        self.root.stat(&address.path)
    }

    pub fn read(&self, address: &NodePath, limit: u64) -> Result<Vec<u8>, FileError> {
        self.require_local(address)?;
        self.root.read(&address.path, limit)
    }

    pub fn write(&self, address: &NodePath, bytes: &[u8]) -> Result<NodePath, FileError> {
        self.require_local(address)?;
        let resolved = self.root.write(&address.path, bytes)?;
        Ok(self.qualify(resolved.path()))
    }

    pub fn create_dir(&self, address: &NodePath) -> Result<NodePath, FileError> {
        self.require_local(address)?;
        let resolved = self.root.create_dir(&address.path)?;
        Ok(self.qualify(resolved.path()))
    }

    pub fn remove(&self, address: &NodePath, recursive: bool) -> Result<NodePath, FileError> {
        self.require_local(address)?;
        let resolved = self.root.remove(&address.path, recursive)?;
        Ok(self.qualify(resolved.path()))
    }

    /// Rename within one node. Moving between nodes is a fetch plus a write
    /// and belongs with the remote work in [`Target::Remote`].
    pub fn rename(&self, from: &NodePath, to: &NodePath) -> Result<NodePath, FileError> {
        self.require_local(from)?;
        self.require_local(to)?;
        let (_, dst) = self.root.rename(&from.path, &to.path)?;
        Ok(self.qualify(dst.path()))
    }
}

/// Unix seconds for a `SystemTime`, for callers assembling an [`Entry`] from
/// something other than [`Fs`].
pub fn to_unix(t: SystemTime) -> Option<u64> {
    t.duration_since(UNIX_EPOCH).ok().map(|d| d.as_secs())
}

// ---------------------------------------------------------------------------
// The command bus
// ---------------------------------------------------------------------------

/// Cap on a whole-file write through the bus.
///
/// The same number as [`DEFAULT_MAX_READ`], for a related reason: a command's
/// parameters arrive as one buffered JSON value, so the bytes exist in memory
/// twice — once base64 or escaped, once decoded — before any of them reach the
/// disk. Anything larger belongs in a streaming upload.
pub const MAX_WRITE: u64 = DEFAULT_MAX_READ;

/// `NodePath` is one string on the wire, so that is what its schema says.
///
/// Hand-written because the derive would describe the `String` the serde
/// attribute converts from and say nothing about the shape, and this schema is
/// what the palette, the MCP tool definition and the Files app all read.
impl schemars::JsonSchema for NodePath {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "NodePath".into()
    }

    fn json_schema(_generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
        schemars::json_schema!({
            "type": "string",
            "title": "File address",
            "description": "A file address: `node:/path`, or `/path` for this node. Paths are \
                            relative to the working directory, which is the root; `..`, host \
                            paths and anything inside `.deckjack/` are refused.",
            "examples": ["/", "/notes/today.md", "r620:/photos/2026"]
        })
    }
}

/// What the file commands hand back for one file or directory.
///
/// Flat on purpose: a file browser renders a row straight from this. `is_dir`
/// is spelled out beside `kind` because sorting folders first is the one thing
/// every listing does, and no client should have to know that `"dir"` is the
/// string that means it.
#[derive(Debug, Clone, Serialize)]
pub struct FileInfo {
    pub name: String,
    /// The complete `node:/path` address, so what the operator clicked can be
    /// sent back as the next command's parameter without being reassembled.
    pub path: NodePath,
    pub is_dir: bool,
    /// `file`, `dir`, `symlink` or `other`.
    pub kind: &'static str,
    pub size: u64,
    /// Unix seconds, or null when the filesystem has no usable timestamp.
    /// Never a fabricated zero, which a client would render as 1970.
    pub modified: Option<u64>,
    pub readonly: bool,
    /// A symlink whose target is outside the workspace or inside
    /// `.deckjack/`. Shown so it can be seen and deleted; opening it fails.
    pub escapes: bool,
}

impl FileInfo {
    /// Qualified with the node it came from, so a listing merged from two
    /// nodes in one panel still addresses each row correctly.
    fn new(entry: Entry, files: &Namespace) -> FileInfo {
        FileInfo {
            path: files.qualify(&entry.path),
            is_dir: entry.kind.is_dir(),
            kind: entry.kind.as_str(),
            size: entry.len,
            modified: entry.modified,
            readonly: entry.readonly,
            escapes: entry.escapes,
            name: entry.name,
        }
    }
}

/// One directory, as `files.list` returns it.
#[derive(Debug, Clone, Serialize)]
pub struct DirListing {
    /// Where the listing actually landed, which is not always what was asked
    /// for: a symlinked directory resolves to its target's address.
    pub path: NodePath,
    pub entries: Vec<FileInfo>,
    /// Set when the entry cap cut the listing short, so the UI says so rather
    /// than showing part of a directory as if it were all of it.
    pub truncated: bool,
}

/// How bytes travel through JSON, in both directions.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContentEncoding {
    /// The file's bytes as text. The default, because most of what a file
    /// panel opens is text and base64 would make every diff and log line
    /// unreadable for no gain.
    #[default]
    Utf8,
    /// Standard base64 with padding, which is what the browser's `atob` and
    /// `btoa` speak.
    Base64,
}

impl schemars::JsonSchema for ContentEncoding {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "ContentEncoding".into()
    }

    fn json_schema(_generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
        schemars::json_schema!({
            "type": "string",
            "enum": ["utf8", "base64"],
            "default": "utf8",
            "description": "How `content` is encoded. `base64` is standard base64 with padding."
        })
    }
}

/// A file's contents, as `files.read` returns them.
#[derive(Debug, Clone, Serialize)]
pub struct FileContents {
    pub path: NodePath,
    pub name: String,
    pub size: u64,
    /// Which of the two `content` is. A flag rather than a guess from the
    /// extension: a `.txt` holding invalid UTF-8 would otherwise be decoded
    /// as text and arrive corrupted.
    pub encoding: ContentEncoding,
    pub content: String,
    /// Guessed from the name, for picking a viewer. A guess is all it is;
    /// nothing here is executed on the strength of it.
    pub mime: String,
    pub modified: Option<u64>,
    pub readonly: bool,
}

/// What `files.delete` returns. There is nothing left to stat, so the answer
/// is which address is now empty.
#[derive(Debug, Clone, Serialize)]
pub struct Deleted {
    pub path: NodePath,
    pub recursive: bool,
}

// -- parameters -------------------------------------------------------------

/// The workspace root, for a panel that opens without a path yet.
fn workspace_root() -> NodePath {
    NodePath::local(VirtualPath::root())
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ListParams {
    /// The directory to list. The workspace root when left out.
    #[serde(default = "workspace_root")]
    pub path: NodePath,
    /// Shown by default; see [`ListOptions::show_dotfiles`].
    #[serde(default)]
    pub show_dotfiles: Option<bool>,
    /// Ask for a smaller page than the server's cap. See [`ListParams::options`].
    #[serde(default)]
    pub max_entries: Option<usize>,
}

impl ListParams {
    /// A client may ask for fewer entries, never more. The cap is the
    /// server's memory bound, not a pagination preference: raising it would
    /// let one panel opening one directory decide how much the server holds.
    fn options(&self) -> ListOptions {
        let mut options = ListOptions::default();
        if let Some(show) = self.show_dotfiles {
            options.show_dotfiles = show;
        }
        if let Some(max) = self.max_entries {
            options.max_entries = max.clamp(1, options.max_entries);
        }
        options
    }
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReadParams {
    pub path: NodePath,
    /// Refuse the read above this many bytes, if lower than the server's own
    /// limit. A preview pane asks for little and fails fast on a huge file
    /// instead of pulling it across the network to throw most of it away.
    #[serde(default)]
    pub max_bytes: Option<u64>,
}

impl ReadParams {
    fn limit(&self) -> u64 {
        match self.max_bytes {
            Some(max) => max.clamp(1, DEFAULT_MAX_READ),
            None => DEFAULT_MAX_READ,
        }
    }
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WriteParams {
    pub path: NodePath,
    /// The whole new contents. There is no append and no offset: the write is
    /// atomic underneath, and a partial write through a lossy connection is
    /// how a half-saved file happens.
    pub content: String,
    #[serde(default)]
    pub encoding: ContentEncoding,
}

impl WriteParams {
    /// Decoded before anything touches the disk, so bad base64 is a parameter
    /// error rather than a temporary file nobody cleans up.
    fn decode(&self, command: &CommandId) -> Result<Vec<u8>, CommandError> {
        let bytes = match self.encoding {
            ContentEncoding::Utf8 => self.content.as_bytes().to_vec(),
            ContentEncoding::Base64 => base64::engine::general_purpose::STANDARD
                .decode(self.content.as_bytes())
                .map_err(|e| CommandError::BadParams {
                    command: command.clone(),
                    message: format!(
                        "content is not valid base64: {e}; send text with encoding \"utf8\" \
                         or standard padded base64 with encoding \"base64\""
                    ),
                })?,
        };
        if bytes.len() as u64 > MAX_WRITE {
            return Err(CommandError::BadParams {
                command: command.clone(),
                message: format!(
                    "{} bytes is over the {MAX_WRITE} byte limit for a write through the bus; \
                     copy a file this large from the terminal instead",
                    bytes.len()
                ),
            });
        }
        Ok(bytes)
    }
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PathParams {
    pub path: NodePath,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RenameParams {
    pub from: NodePath,
    /// The complete new address, not a new name: a rename and a move are the
    /// same operation, and spelling it this way means the Files app does not
    /// need two commands to drag a file into a folder.
    pub to: NodePath,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DeleteParams {
    pub path: NodePath,
    /// Required to delete a directory that has anything in it. Off by
    /// default, so a mistyped path cannot take a tree with it.
    #[serde(default)]
    pub recursive: bool,
}

// -- plumbing ---------------------------------------------------------------

/// Run one filesystem operation off the runtime's worker threads.
///
/// [`Fs`] is synchronous, and the working directory may sit on a spinning or
/// network-backed disk. Doing this work on a worker thread would park that
/// thread inside a syscall, and the terminal's long poll shares those
/// threads: one slow `stat` on a stalled mount would hold up output the
/// operator is watching come out of a shell.
async fn off_thread<T, F>(command: CommandId, work: F) -> Result<T, CommandError>
where
    F: FnOnce() -> Result<T, FileError> + Send + 'static,
    T: Send + 'static,
{
    match tokio::task::spawn_blocking(work).await {
        Ok(result) => result.map_err(|e| to_command_error(command, e)),
        // Nothing but a panic in the work above reaches this arm, and a panic
        // in this module is a bug rather than a condition of the filesystem.
        // It is reported instead of resumed so one bad path cannot take the
        // whole server down with it.
        Err(joined) => Err(CommandError::Failed(anyhow::anyhow!(
            "the file operation did not finish: {joined}"
        ))),
    }
}

/// Keep the distinctions the bus already makes.
///
/// [`FileError`]'s messages are written for the operator, so they are passed
/// through as they are; what this chooses is the *kind* of failure, because
/// the shell and the Inbox render the kinds differently.
fn to_command_error(command: CommandId, error: FileError) -> CommandError {
    let message = error.to_string();
    match error {
        // A path the namespace cannot even hold is a malformed request, and
        // saying so puts the message beside the field that was typed instead
        // of in a failure banner.
        FileError::Path(_) => CommandError::BadParams { command, message },
        // Another node's file is not this node's command to run. The bus has
        // a word for that, and using it keeps "ask the other node" from
        // reading as "the filesystem broke".
        FileError::NotLocal { .. } | FileError::UnknownNode { .. } => CommandError::Unsupported {
            command,
            reason: message,
        },
        other => CommandError::Failed(anyhow::Error::new(other)),
    }
}

// -- reads ------------------------------------------------------------------

pub struct ListDir {
    files: Arc<Namespace>,
}

impl Command for ListDir {
    const ID: &'static str = "files.list";
    const TITLE: &'static str = "List files";
    const RISK: RiskLevel = RiskLevel::ReadOnly;

    type Params = ListParams;
    type Output = DirListing;

    async fn run(&self, params: ListParams, cx: Context) -> Result<DirListing, CommandError> {
        let files = Arc::clone(&self.files);
        let options = params.options();
        off_thread(cx.command.clone(), move || {
            let listing = files.list(&params.path, &options)?;
            Ok(DirListing {
                path: files.qualify(&listing.path),
                entries: listing
                    .entries
                    .into_iter()
                    .map(|entry| FileInfo::new(entry, &files))
                    .collect(),
                truncated: listing.truncated,
            })
        })
        .await
    }
}

pub struct ReadFile {
    files: Arc<Namespace>,
}

impl Command for ReadFile {
    const ID: &'static str = "files.read";
    const TITLE: &'static str = "Read file";
    const RISK: RiskLevel = RiskLevel::ReadOnly;

    type Params = ReadParams;
    type Output = FileContents;

    async fn run(&self, params: ReadParams, cx: Context) -> Result<FileContents, CommandError> {
        let files = Arc::clone(&self.files);
        off_thread(cx.command.clone(), move || {
            let limit = params.limit();
            // Stat first, so the response carries the address the read landed
            // on and the timestamp an editor needs to detect a change under
            // it. `size` comes from the bytes themselves rather than from
            // this metadata, so the two cannot disagree if the file is
            // rewritten between the two calls.
            let entry = files.stat(&params.path)?;
            let bytes = files.read(&params.path, limit)?;
            let path = files.qualify(&entry.path);
            let mime = mime_guess::from_path(&entry.name)
                .first_or_octet_stream()
                .to_string();

            // Text is returned as text. A browser decoding base64 to show a
            // README is work that buys nothing, and the one flag below tells
            // it when there is no choice. The size is measured before
            // encoding, so it is the file's own length rather than the length
            // of its transport form.
            let (encoding, content, size) = match String::from_utf8(bytes) {
                Ok(text) => {
                    let size = text.len() as u64;
                    (ContentEncoding::Utf8, text, size)
                }
                Err(e) => {
                    let raw = e.into_bytes();
                    let size = raw.len() as u64;
                    let engine = base64::engine::general_purpose::STANDARD;
                    (ContentEncoding::Base64, engine.encode(&raw), size)
                }
            };

            Ok(FileContents {
                size,
                path,
                name: entry.name,
                encoding,
                content,
                mime,
                modified: entry.modified,
                readonly: entry.readonly,
            })
        })
        .await
    }
}

pub struct StatEntry {
    files: Arc<Namespace>,
}

impl Command for StatEntry {
    const ID: &'static str = "files.stat";
    const TITLE: &'static str = "File details";
    const RISK: RiskLevel = RiskLevel::ReadOnly;

    type Params = PathParams;
    type Output = FileInfo;

    async fn run(&self, params: PathParams, cx: Context) -> Result<FileInfo, CommandError> {
        let files = Arc::clone(&self.files);
        off_thread(cx.command.clone(), move || {
            let entry = files.stat(&params.path)?;
            Ok(FileInfo::new(entry, &files))
        })
        .await
    }
}

// -- writes -----------------------------------------------------------------
//
// Everything below is `RiskLevel::Write`, including the delete, and that is
// the considered answer rather than an oversight.
//
// Risk level decides whether *the operator* is asked "are you sure". The
// design says my own actions run directly, with ordinary confirmations for
// destructive ones, and the confirmation it asks for specifically is for
// "destructive file actions outside the working directory". Nothing reachable
// through these commands is outside it: every path goes through
// `Root::resolve` or `Root::resolve_link`, which prove containment on the
// canonical path, so the case the design gates cannot arrive here at all.
//
// The tempting mistake — already made once in this project — is to mark the
// delete `Destructive` because it would be dangerous for an *agent*. That
// conflates two instruments. What keeps an agent out of these commands is the
// grant: an agent is simply never given `files.write`, `files.delete` or
// `files.rename`, and anything outside a grant is refused rather than queued.
// Remote sessions are gated again by step-up sign-in. Risk level is neither of
// those, and setting it high would only mean the operator confirms every file
// they save in a text editor, which is a prompt nobody reads by the tenth one.

pub struct WriteFile {
    files: Arc<Namespace>,
}

impl Command for WriteFile {
    const ID: &'static str = "files.write";
    const TITLE: &'static str = "Save file";
    const RISK: RiskLevel = RiskLevel::Write;
    /// The parameters are the file's contents, and the audit log is readable
    /// from the terminal by design. A saved password file or private key must
    /// not end up in it.
    ///
    /// The cost is real: redaction is per command, not per field, so the path
    /// is lost from the log row along with the bytes. The decision is a
    /// deliberate trade and the fix is per-field redaction in `CommandSpec`,
    /// which is the bus's business rather than this module's.
    const REDACT_PARAMS: bool = true;

    type Params = WriteParams;
    type Output = FileInfo;

    async fn run(&self, params: WriteParams, cx: Context) -> Result<FileInfo, CommandError> {
        let files = Arc::clone(&self.files);
        let bytes = params.decode(&cx.command)?;
        off_thread(cx.command.clone(), move || {
            let written = files.write(&params.path, &bytes)?;
            // Stat afterwards so the caller gets the new size and timestamp
            // and can update its row without asking again.
            let entry = files.stat(&written)?;
            Ok(FileInfo::new(entry, &files))
        })
        .await
    }
}

pub struct MakeDir {
    files: Arc<Namespace>,
}

impl Command for MakeDir {
    const ID: &'static str = "files.mkdir";
    const TITLE: &'static str = "New folder";
    const RISK: RiskLevel = RiskLevel::Write;

    type Params = PathParams;
    type Output = FileInfo;

    async fn run(&self, params: PathParams, cx: Context) -> Result<FileInfo, CommandError> {
        let files = Arc::clone(&self.files);
        off_thread(cx.command.clone(), move || {
            let created = files.create_dir(&params.path)?;
            let entry = files.stat(&created)?;
            Ok(FileInfo::new(entry, &files))
        })
        .await
    }
}

pub struct RenameEntry {
    files: Arc<Namespace>,
}

impl Command for RenameEntry {
    const ID: &'static str = "files.rename";
    const TITLE: &'static str = "Rename or move";
    const RISK: RiskLevel = RiskLevel::Write;

    type Params = RenameParams;
    type Output = FileInfo;

    async fn run(&self, params: RenameParams, cx: Context) -> Result<FileInfo, CommandError> {
        let files = Arc::clone(&self.files);
        off_thread(cx.command.clone(), move || {
            // An existing destination is refused underneath rather than
            // overwritten, so this cannot quietly consume a file the operator
            // forgot was there.
            let moved = files.rename(&params.from, &params.to)?;
            let entry = files.stat(&moved)?;
            Ok(FileInfo::new(entry, &files))
        })
        .await
    }
}

pub struct DeleteEntry {
    files: Arc<Namespace>,
}

impl Command for DeleteEntry {
    const ID: &'static str = "files.delete";
    const TITLE: &'static str = "Delete";
    /// Inside the working directory, deleting a file is ordinary work; see the
    /// section note above for why this is not `Destructive`. A recursive
    /// delete is the one case where "acts in bulk" could argue the other way,
    /// and the honest place for that is a separate command with its own id and
    /// a count in its prompt — not a risk level on this one, which would
    /// charge every single-file delete for it.
    const RISK: RiskLevel = RiskLevel::Write;

    type Params = DeleteParams;
    type Output = Deleted;

    async fn run(&self, params: DeleteParams, cx: Context) -> Result<Deleted, CommandError> {
        let files = Arc::clone(&self.files);
        off_thread(cx.command.clone(), move || {
            // A symlink is unlinked and never followed, so deleting a
            // shortcut cannot delete what it points at.
            let removed = files.remove(&params.path, params.recursive)?;
            Ok(Deleted {
                path: removed,
                recursive: params.recursive,
            })
        })
        .await
    }
}

// -- registration -----------------------------------------------------------

/// What can go wrong while putting the file commands on the bus. Each message
/// names the one thing to change.
#[derive(Debug, Error)]
pub enum SetupError {
    #[error(transparent)]
    Root(#[from] FileError),

    #[error(
        "node id {node:?} cannot be used in file addresses: {source}. File addresses are \
         `node:/path`, so the node's id has to be a name that can appear there; regenerate \
         the node id in the install directory"
    )]
    Node {
        node: String,
        #[source]
        source: PathError,
    },

    #[error(transparent)]
    Registry(#[from] RegistryError),
}

/// Put the file commands on the bus.
///
/// Returns the [`Namespace`] it built so the caller keeps a handle. The HTTP
/// file routes, WebDAV, previews and downloads all have to cross the same
/// boundary, and building a second [`Root`] for them would be a second place
/// for the containment rules to drift — which is exactly how the predecessor
/// project ended up with a decorative root.
pub fn register_commands(
    registry: &Registry,
    working_dir: &Path,
    node_id: &str,
) -> Result<Arc<Namespace>, SetupError> {
    let local = NodeName::parse(node_id).map_err(|source| SetupError::Node {
        node: node_id.to_string(),
        source,
    })?;
    let files = Arc::new(Namespace::new(local, Root::open(working_dir)?));

    macro_rules! reg {
        ($command:ident) => {
            registry.register($command {
                files: Arc::clone(&files),
            })?;
        };
    }
    reg!(ListDir);
    reg!(ReadFile);
    reg!(StatEntry);
    reg!(WriteFile);
    reg!(MakeDir);
    reg!(RenameEntry);
    reg!(DeleteEntry);

    Ok(files)
}
