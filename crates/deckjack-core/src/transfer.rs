//! Copying and moving several things into a folder: `files.copy` and
//! `files.move`.
//!
//! Every path goes through the same [`Root`] resolution the single-file
//! commands use, so the workspace boundary and the `.deckjack/` refusal are
//! enforced by the code that already enforces them. What this module adds is
//! the part a bulk operation owes on top:
//!
//! - **Never overwrite.** A destination name that exists is refused before
//!   anything moves, the same rule `files.rename` keeps.
//! - **All or nothing.** If the fourth of five items fails, the first three
//!   are put back (a move) or removed (a copy). A file manager that leaves
//!   half a selection behind on an error leaves the operator to work out
//!   which half.
//! - **Nothing appears before its bytes do.** A copy is written under a
//!   hidden partial name and renamed into place when complete, so a folder of
//!   photos never shows a truncated one, and a crash leaves a dotfile rather
//!   than a file that looks finished.
//! - **Cancelling stops the disk.** The copy runs on a blocking thread, which
//!   a dropped future does not stop; the command holds a flag that is set when
//!   its future is dropped, and the copy checks it between chunks.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use serde::{Deserialize, Serialize};

use crate::command::{Command, CommandError, Context, RiskLevel};
use crate::files::{EntryKind, FileError, Namespace, NodePath, Resolved, Root, VirtualPath};

/// What to do when a name is already taken in the destination folder.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Conflict {
    /// Refuse the whole request. The only answer for a move, and the default
    /// for a copy.
    #[default]
    Refuse,
    /// Copy under the next free "name copy", "name copy 2", ... instead. What
    /// pasting a copy into the folder it came from means: the operator asked
    /// for a second one, and refusing would make that impossible.
    KeepBoth,
}

/// The `n`th "copy" name for `name`: `report copy.txt`, then
/// `report copy 2.txt`. A folder's name has no extension to keep, and a
/// dotfile's leading dot is not one either. Copying a copy counts on from it
/// (`report copy.txt` becomes `report copy 2.txt`, not
/// `report copy copy.txt`).
pub fn copy_name(name: &str, n: u32, is_dir: bool) -> String {
    let (stem, ext) = match name.rsplit_once('.') {
        Some((stem, ext)) if !is_dir && !stem.is_empty() => (stem, Some(ext)),
        _ => (name, None),
    };
    let stem = strip_copy_suffix(stem);
    let suffix = if n <= 1 {
        " copy".to_string()
    } else {
        format!(" copy {n}")
    };
    let tail = match ext {
        Some(ext) => format!("{suffix}.{ext}"),
        None => suffix,
    };
    // Shorten the stem rather than produce a name no filesystem accepts.
    let room = crate::files::MAX_NAME_LEN.saturating_sub(tail.len());
    let mut cut = stem.len().min(room);
    while !stem.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}{tail}", &stem[..cut])
}

fn strip_copy_suffix(stem: &str) -> &str {
    if let Some(base) = stem.strip_suffix(" copy")
        && !base.is_empty()
    {
        return base;
    }
    if let Some((base, n)) = stem.rsplit_once(" copy ")
        && !base.is_empty()
        && !n.is_empty()
        && n.bytes().all(|b| b.is_ascii_digit())
    {
        return base;
    }
    stem
}

/// The most "copy" names tried before giving up.
const MAX_COPY_NAMES: u32 = 10_000;

/// One item that will land in the destination folder.
struct Planned {
    src: Resolved,
    dst: Resolved,
}

/// Resolve and check every item before anything is touched, so a refusal is
/// a refusal of the whole request and not of whatever was left after the
/// first half ran.
fn plan(
    root: &Root,
    sources: &[VirtualPath],
    dir: &VirtualPath,
    conflict: Conflict,
) -> Result<Vec<Planned>, FileError> {
    let into = root.resolve(dir)?;
    let meta = root
        .fs()
        .meta(into.real_path())
        .map_err(|source| root.map_io(into.path(), source))?;
    if !meta.kind.is_dir() {
        return Err(FileError::NotADirectory {
            path: into.path().clone(),
        });
    }

    let mut planned: Vec<Planned> = Vec::with_capacity(sources.len());
    for source in sources {
        let src = root.resolve_link(source)?;
        if src.path().is_root() {
            return Err(FileError::RootIsNotAFile {
                action: "moved or copied",
            });
        }
        root.fs()
            .link_meta(src.real_path())
            .map_err(|e| root.map_io(src.path(), e))?;
        if planned.iter().any(|p| p.src.path() == src.path()) {
            return Err(FileError::Duplicate {
                path: src.path().clone(),
            });
        }
        // A folder into itself or anywhere below itself. Compared on the
        // virtual paths, which are what the operator named; the real paths
        // agree because both came from one resolution.
        if is_within(into.path(), src.path()) {
            return Err(FileError::IntoItself {
                path: src.path().clone(),
            });
        }
        let name = src.path().name().unwrap_or_default().to_string();
        let is_dir = root
            .fs()
            .link_meta(src.real_path())
            .is_ok_and(|m| m.kind.is_dir());
        let mut dst = root.resolve_link(&into.path().join(&name)?)?;
        let mut tries = 0;
        while let Some(taken) = taken(root, &planned, &dst)? {
            if conflict == Conflict::Refuse || tries >= MAX_COPY_NAMES {
                return Err(FileError::Exists { path: taken });
            }
            tries += 1;
            dst = root.resolve_link(&into.path().join(&copy_name(&name, tries, is_dir))?)?;
        }
        planned.push(Planned { src, dst });
    }
    Ok(planned)
}

/// The destination's address if something already has it: a file on disk,
/// or another item of this same request (two sources with one name, from
/// different folders).
fn taken(
    root: &Root,
    planned: &[Planned],
    dst: &Resolved,
) -> Result<Option<VirtualPath>, FileError> {
    if planned.iter().any(|p| p.dst.path() == dst.path()) {
        return Ok(Some(dst.path().clone()));
    }
    match root.fs().link_meta(dst.real_path()) {
        Ok(_) => Ok(Some(dst.path().clone())),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(root.map_io(dst.path(), e)),
    }
}

/// Whether `path` is `ancestor` or somewhere below it.
fn is_within(path: &VirtualPath, ancestor: &VirtualPath) -> bool {
    path.depth() >= ancestor.depth()
        && path
            .components()
            .zip(ancestor.components())
            .all(|(a, b)| a == b)
}

/// Move every source into `dir`, or none of them.
pub fn move_into(
    root: &Root,
    sources: &[VirtualPath],
    dir: &VirtualPath,
) -> Result<Vec<VirtualPath>, FileError> {
    let planned = plan(root, sources, dir, Conflict::Refuse)?;
    let mut done: Vec<&Planned> = Vec::new();
    for item in &planned {
        if let Err(e) = root.fs().rename(item.src.real_path(), item.dst.real_path()) {
            // Put back what already moved, newest first. Best effort: a
            // failure here is reported by the original error, which is the
            // one the operator can act on.
            for moved in done.iter().rev() {
                let _ = root
                    .fs()
                    .rename(moved.dst.real_path(), moved.src.real_path());
            }
            return Err(if e.raw_os_error() == Some(libc::EXDEV) {
                FileError::CrossesFilesystems {
                    path: item.src.path().clone(),
                }
            } else {
                root.map_io(item.src.path(), e)
            });
        }
        done.push(item);
    }
    Ok(planned.into_iter().map(|p| p.dst.path().clone()).collect())
}

/// What a copy reports while it runs.
pub trait CopyProgress {
    /// `done` of `total` bytes copied so far.
    fn bytes(&mut self, done: u64, total: u64);
    /// Whether to stop.
    fn cancelled(&self) -> bool;
}

/// Copy every source into `dir`, or none of them. A name already taken is
/// refused.
pub fn copy_into(
    root: &Root,
    sources: &[VirtualPath],
    dir: &VirtualPath,
    progress: &mut dyn CopyProgress,
) -> Result<Vec<VirtualPath>, FileError> {
    copy_into_with(root, sources, dir, Conflict::Refuse, progress)
}

/// [`copy_into`], saying what a taken name means.
pub fn copy_into_with(
    root: &Root,
    sources: &[VirtualPath],
    dir: &VirtualPath,
    conflict: Conflict,
    progress: &mut dyn CopyProgress,
) -> Result<Vec<VirtualPath>, FileError> {
    let planned = plan(root, sources, dir, conflict)?;

    let mut total = 0u64;
    for item in &planned {
        total += size_of(root, item.src.real_path(), item.src.path())?;
    }
    progress.bytes(0, total);

    let mut copied = 0u64;
    let mut finished: Vec<&Planned> = Vec::new();
    for item in &planned {
        let partial = partial_name(root, item.dst.real_path())?;
        let outcome = copy_tree(
            root,
            item.src.real_path(),
            &partial,
            item.src.path(),
            &mut |n| {
                copied += n;
                progress.bytes(copied, total);
                !progress.cancelled()
            },
        )
        .and_then(|()| {
            root.fs()
                .rename(&partial, item.dst.real_path())
                .map_err(|e| root.map_io(item.dst.path(), e))
        });
        if let Err(e) = outcome {
            remove_any(root, &partial);
            for done in finished.iter().rev() {
                remove_any(root, done.dst.real_path());
            }
            return Err(e);
        }
        finished.push(item);
    }
    Ok(planned.into_iter().map(|p| p.dst.path().clone()).collect())
}

/// Bytes of regular files under `real`, not following symlinks.
fn size_of(root: &Root, real: &Path, path: &VirtualPath) -> Result<u64, FileError> {
    let meta = root
        .fs()
        .link_meta(real)
        .map_err(|e| root.map_io(path, e))?;
    match meta.kind {
        EntryKind::File => Ok(meta.len),
        EntryKind::Symlink => Ok(0),
        EntryKind::Dir => {
            let mut sum = 0;
            for name in root.fs().read_dir(real).map_err(|e| root.map_io(path, e))? {
                sum += size_of(root, &real.join(&name), &path.join(&name)?)?;
            }
            Ok(sum)
        }
        EntryKind::Other => Err(FileError::NotCopyable { path: path.clone() }),
    }
}

/// A free hidden name beside `dst` for the copy to grow in.
fn partial_name(root: &Root, dst: &Path) -> Result<PathBuf, FileError> {
    let dir = dst.parent().unwrap_or(Path::new("."));
    let name = dst
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    for n in 0..1000 {
        // The partial name is longer than the final one, so a final name
        // near the limit is shortened here, or no long-named file could be
        // copied at all.
        let tail = format!(".deckjack-partial-{n}");
        let room = crate::files::MAX_NAME_LEN.saturating_sub(1 + tail.len());
        let mut cut = name.len().min(room);
        while !name.is_char_boundary(cut) {
            cut -= 1;
        }
        let candidate = dir.join(format!(".{}{tail}", &name[..cut]));
        if matches!(root.fs().link_meta(&candidate), Err(e) if e.kind() == io::ErrorKind::NotFound)
        {
            return Ok(candidate);
        }
    }
    Err(FileError::Io {
        path: VirtualPath::root(),
        source: io::Error::other(format!(
            "no free partial name beside {}; remove the .deckjack-partial files there",
            dst.display()
        )),
    })
}

/// Copy `src` to `dst` (which must not exist), recursing into folders and
/// copying symlinks as symlinks. `path` is only for naming errors.
fn copy_tree(
    root: &Root,
    src: &Path,
    dst: &Path,
    path: &VirtualPath,
    chunk: &mut dyn FnMut(u64) -> bool,
) -> Result<(), FileError> {
    let fs = root.fs();
    let meta = fs.link_meta(src).map_err(|e| root.map_io(path, e))?;
    match meta.kind {
        EntryKind::File => {
            fs.copy_file(src, dst, chunk).map_err(|e| {
                if e.kind() == io::ErrorKind::Interrupted {
                    FileError::Cancelled
                } else {
                    root.map_io(path, e)
                }
            })?;
        }
        // The link's text, not what it points at: a copy of a shortcut is a
        // shortcut, and following it could copy something from outside the
        // workspace into it.
        EntryKind::Symlink => {
            let target = fs.read_link(src).map_err(|e| root.map_io(path, e))?;
            fs.symlink(&target, dst).map_err(|e| root.map_io(path, e))?;
        }
        EntryKind::Dir => {
            fs.create_dir(dst).map_err(|e| root.map_io(path, e))?;
            for name in fs.read_dir(src).map_err(|e| root.map_io(path, e))? {
                copy_tree(
                    root,
                    &src.join(&name),
                    &dst.join(&name),
                    &path.join(&name)?,
                    chunk,
                )?;
            }
        }
        EntryKind::Other => return Err(FileError::NotCopyable { path: path.clone() }),
    }
    Ok(())
}

fn remove_any(root: &Root, real: &Path) {
    let fs = root.fs();
    let _ = match fs.link_meta(real) {
        Ok(meta) if meta.kind.is_dir() => fs.remove_dir_all(real),
        Ok(_) => fs.remove_file(real),
        Err(_) => Ok(()),
    };
}

// ---------------------------------------------------------------------------
// The commands
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TransferParams {
    /// What to move or copy. Each lands in `to` under its own name.
    pub paths: Vec<NodePath>,
    /// The folder they go into.
    pub to: NodePath,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CopyParams {
    /// What to copy. Each lands in `to` under its own name.
    pub paths: Vec<NodePath>,
    /// The folder they go into.
    pub to: NodePath,
    /// When a name is taken, copy under "name copy", "name copy 2", ...
    /// instead of refusing. Off by default: nothing is renamed unless asked.
    #[serde(default)]
    pub keep_both: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct Transferred {
    /// Where each item now is, in the order they were named.
    pub paths: Vec<NodePath>,
}

/// Checks and splits the addresses: every one must be this node's.
fn local_paths(
    files: &Namespace,
    paths: &[NodePath],
    to: &NodePath,
) -> Result<(Vec<VirtualPath>, VirtualPath), FileError> {
    let mut sources = Vec::with_capacity(paths.len());
    for address in paths {
        if !files.is_local(address) {
            return Err(FileError::NotLocal {
                address: address.clone(),
            });
        }
        sources.push(address.path.clone());
    }
    if !files.is_local(to) {
        return Err(FileError::NotLocal {
            address: to.clone(),
        });
    }
    Ok((sources, to.path.clone()))
}

fn bad_empty(cx: &Context) -> CommandError {
    CommandError::BadParams {
        command: cx.command.clone(),
        message: "paths is empty; name at least one file or folder".into(),
    }
}

pub struct MoveEntries {
    pub(crate) files: Arc<Namespace>,
}

impl Command for MoveEntries {
    const ID: &'static str = "files.move";
    const TITLE: &'static str = "Move into a folder";
    /// Write, like `files.rename`, which is the single-item form of this. It
    /// overwrites nothing and is undone by moving back.
    const RISK: RiskLevel = RiskLevel::Write;

    type Params = TransferParams;
    type Output = Transferred;

    async fn run(&self, params: TransferParams, cx: Context) -> Result<Transferred, CommandError> {
        if params.paths.is_empty() {
            return Err(bad_empty(&cx));
        }
        let files = Arc::clone(&self.files);
        crate::files::off_thread(cx.command.clone(), move || {
            let (sources, to) = local_paths(&files, &params.paths, &params.to)?;
            let moved = move_into(files.root(), &sources, &to)?;
            Ok(Transferred {
                paths: moved.iter().map(|p| files.qualify(p)).collect(),
            })
        })
        .await
    }
}

/// Set when the command's future is dropped: the bus's way of cancelling.
struct CancelOnDrop(Arc<AtomicBool>);

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
    }
}

/// Progress for one copy: a step per percent, not per chunk.
struct Reporter {
    sink: crate::command::ProgressSink,
    cancel: Arc<AtomicBool>,
    last_percent: Option<u64>,
}

impl CopyProgress for Reporter {
    fn bytes(&mut self, done: u64, total: u64) {
        let percent = (done * 100).checked_div(total).unwrap_or(100);
        if self.last_percent == Some(percent) {
            return;
        }
        self.last_percent = Some(percent);
        let fraction = if total == 0 {
            1.0
        } else {
            done as f32 / total as f32
        };
        self.sink
            .step(Some(fraction), format!("copied {done} of {total} bytes"));
    }

    fn cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }
}

pub struct CopyEntries {
    pub(crate) files: Arc<Namespace>,
}

impl Command for CopyEntries {
    const ID: &'static str = "files.copy";
    const TITLE: &'static str = "Copy into a folder";
    const RISK: RiskLevel = RiskLevel::Write;

    type Params = CopyParams;
    type Output = Transferred;

    async fn run(&self, params: CopyParams, cx: Context) -> Result<Transferred, CommandError> {
        if params.paths.is_empty() {
            return Err(bad_empty(&cx));
        }
        let files = Arc::clone(&self.files);
        let cancel = Arc::new(AtomicBool::new(false));
        let _guard = CancelOnDrop(Arc::clone(&cancel));
        let mut reporter = Reporter {
            sink: cx.progress.clone(),
            cancel,
            last_percent: None,
        };
        crate::files::off_thread(cx.command.clone(), move || {
            let (sources, to) = local_paths(&files, &params.paths, &params.to)?;
            let conflict = if params.keep_both {
                Conflict::KeepBoth
            } else {
                Conflict::Refuse
            };
            let copied = copy_into_with(files.root(), &sources, &to, conflict, &mut reporter)?;
            Ok(Transferred {
                paths: copied.iter().map(|p| files.qualify(p)).collect(),
            })
        })
        .await
    }
}
