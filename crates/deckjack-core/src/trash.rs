//! The trash: `files.trash`, `files.trash.list`, `files.trash.restore` and
//! `files.trash.empty`.
//!
//! **Where.** `/.Trash` in the working directory, in the freedesktop.org
//! layout: the item itself under `files/<id>`, and a `info/<id>.trashinfo`
//! beside it naming where it came from and when. Inside the working directory
//! because that is the one tree Deckjack owns and the one that travels: a
//! trash in `~/.local/share` would stay behind on the old host when the
//! directory moves, and would be outside everything Files can show. The
//! layout is the standard one so the files make sense to any other tool that
//! opens the directory, and survive Deckjack.
//!
//! **Why it exists.** Deleting through Files used to be final, and a file the
//! operator wrote by hand is not recoverable from inside Deckjack once it is
//! gone. Trash is the undo. So moving to the trash is `Write` — it is undone
//! by `files.trash.restore` — and only emptying it is `Destructive`.
//!
//! **All or nothing**, as in `transfer`: a batch that fails partway puts back
//! what it had already done.

use std::io;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::command::{Command, CommandError, Context, RiskLevel};
use crate::files::{EntryKind, FileError, Namespace, NodePath, Resolved, Root, VirtualPath};
use crate::wiring::NoParams;

/// The trash's address in the working directory.
pub const TRASH_DIR: &str = ".Trash";
const INFO_SUFFIX: &str = ".trashinfo";

fn trash_root() -> VirtualPath {
    VirtualPath::root()
        .join(TRASH_DIR)
        .expect(".Trash is a valid name")
}

fn subdir(name: &str) -> VirtualPath {
    trash_root()
        .join(name)
        .expect("files and info are valid names")
}

/// Make `/.Trash/files` and `/.Trash/info` if they are not there yet.
fn ensure(root: &Root) -> Result<(), FileError> {
    for dir in [trash_root(), subdir("files"), subdir("info")] {
        match root.create_dir(&dir) {
            Ok(_) | Err(FileError::Exists { .. }) => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

fn is_in_trash(path: &VirtualPath) -> bool {
    path.components().next() == Some(TRASH_DIR)
}

/// A name in `files/` that is not taken: the item's own name, then `name.2`,
/// `name.3`, as the freedesktop spec suggests.
fn free_id(root: &Root, name: &str, taken: &[String]) -> Result<String, FileError> {
    for n in 1..10_000 {
        let id = if n == 1 {
            name.to_string()
        } else {
            format!("{name}.{n}")
        };
        if taken.contains(&id) {
            continue;
        }
        let files = root.resolve_link(&subdir("files").join(&id)?)?;
        let info = root.resolve_link(&subdir("info").join(&format!("{id}{INFO_SUFFIX}"))?)?;
        let free = |r: &Resolved| matches!(root.fs().link_meta(r.real_path()), Err(e) if e.kind() == io::ErrorKind::NotFound);
        if free(&files) && free(&info) {
            return Ok(id);
        }
    }
    Err(FileError::Exists {
        path: subdir("files").join(name)?,
    })
}

/// `YYYY-MM-DDThh:mm:ss` in the host's local time, as the spec asks.
fn deletion_date(unix: i64) -> String {
    let t = unix as libc::time_t;
    // SAFETY: `localtime_r` writes only into `tm`, which is ours and
    // zeroed; a null return leaves it zeroed and the date reads as 1900,
    // which is wrong but harmless in an informational field.
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    unsafe { libc::localtime_r(&t, &mut tm) };
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}",
        tm.tm_year + 1900,
        tm.tm_mon + 1,
        tm.tm_mday,
        tm.tm_hour,
        tm.tm_min,
        tm.tm_sec
    )
}

/// Percent-encode a path for `Path=`: everything but unreserved characters
/// and `/`, so a name with a newline cannot forge a second key.
fn encode(path: &str) -> String {
    let mut out = String::new();
    for b in path.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn decode(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = text.get(i + 1..i + 3)?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

/// What an `.trashinfo` says, or `None` if it does not say it clearly.
fn parse_info(text: &str) -> Option<(VirtualPath, Option<String>)> {
    let mut lines = text.lines();
    if lines.next()?.trim() != "[Trash Info]" {
        return None;
    }
    let mut path = None;
    let mut date = None;
    for line in lines {
        if let Some(v) = line.strip_prefix("Path=") {
            path = Some(decode(v)?);
        } else if let Some(v) = line.strip_prefix("DeletionDate=") {
            date = Some(v.to_string());
        }
    }
    // Relative to the directory the trash is in, which is the working
    // directory; `VirtualPath::parse` refuses `..`, NUL and `.deckjack`, so a
    // hand-edited info file cannot restore something outside the workspace.
    let path = VirtualPath::parse(&path?).ok()?;
    if path.is_root() || path.is_hidden_state() {
        return None;
    }
    Some((path, date))
}

/// One thing that went to the trash.
#[derive(Debug, Clone, Serialize)]
pub struct Trashed {
    pub id: String,
    pub from: VirtualPath,
}

/// Move each path to the trash, or none of them.
pub fn trash(root: &Root, paths: &[VirtualPath], now: i64) -> Result<Vec<Trashed>, FileError> {
    let mut planned: Vec<(Resolved, VirtualPath)> = Vec::new();
    for path in paths {
        let src = root.resolve_link(path)?;
        if src.path().is_root() {
            return Err(FileError::RootIsNotAFile {
                action: "moved to the trash",
            });
        }
        if is_in_trash(src.path()) {
            return Err(FileError::AlreadyInTrash {
                path: src.path().clone(),
            });
        }
        root.fs()
            .link_meta(src.real_path())
            .map_err(|e| root.map_io(src.path(), e))?;
        if planned.iter().any(|(s, _)| s.path() == src.path()) {
            return Err(FileError::Duplicate {
                path: src.path().clone(),
            });
        }
        let original = src.path().clone();
        planned.push((src, original));
    }
    ensure(root)?;

    let mut done: Vec<(Trashed, Resolved, Resolved, Resolved)> = Vec::new();
    let undo = |done: &[(Trashed, Resolved, Resolved, Resolved)]| {
        for (_, src, item, info) in done.iter().rev() {
            let _ = root.fs().rename(item.real_path(), src.real_path());
            let _ = root.fs().remove_file(info.real_path());
        }
    };
    let date = deletion_date(now);
    for (src, original) in planned {
        let taken: Vec<String> = done.iter().map(|(t, ..)| t.id.clone()).collect();
        let name = original.name().unwrap_or_default().to_string();
        let step = (|| {
            let id = free_id(root, &name, &taken)?;
            let item = root.resolve_link(&subdir("files").join(&id)?)?;
            let info_path = subdir("info").join(&format!("{id}{INFO_SUFFIX}"))?;
            // Info first, as the spec orders it: the info file is what claims
            // the id, and a crash between the two leaves a stale info file
            // rather than a trashed item nobody can trace.
            let text = format!(
                "[Trash Info]\nPath={}\nDeletionDate={date}\n",
                encode(original.to_string().trim_start_matches('/'))
            );
            root.write(&info_path, text.as_bytes())?;
            let info = root.resolve_link(&info_path)?;
            if let Err(e) = root.fs().rename(src.real_path(), item.real_path()) {
                let _ = root.fs().remove_file(info.real_path());
                return Err(if e.raw_os_error() == Some(libc::EXDEV) {
                    FileError::CrossesFilesystems {
                        path: original.clone(),
                    }
                } else {
                    root.map_io(&original, e)
                });
            }
            Ok((
                Trashed {
                    id,
                    from: original.clone(),
                },
                item,
                info,
            ))
        })();
        match step {
            Ok((trashed, item, info)) => done.push((trashed, src, item, info)),
            Err(e) => {
                undo(&done);
                return Err(e);
            }
        }
    }
    Ok(done.into_iter().map(|(t, ..)| t).collect())
}

/// One item in the trash, as `files.trash.list` reports it.
#[derive(Debug, Clone, Serialize)]
pub struct TrashEntry {
    pub id: String,
    /// Where it came from and where restoring puts it back.
    pub from: VirtualPath,
    /// As the info file says it, in the host's local time.
    pub deleted_at: Option<String>,
    pub kind: EntryKind,
    pub len: u64,
}

/// What is in the trash, newest first. An item with no readable info file is
/// left out rather than guessed at: restoring it would mean inventing where
/// it came from.
pub fn list(root: &Root) -> Result<Vec<TrashEntry>, FileError> {
    let info_dir = match root.resolve(&subdir("info")) {
        Ok(r) => r,
        Err(FileError::NotFound { .. }) => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    let names = match root.fs().read_dir(info_dir.real_path()) {
        Ok(names) => names,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(root.map_io(info_dir.path(), e)),
    };
    let mut entries = Vec::new();
    for name in names {
        let Some(id) = name.strip_suffix(INFO_SUFFIX) else {
            continue;
        };
        let Ok(info) = root.read(&subdir("info").join(&name)?, 64 * 1024) else {
            continue;
        };
        let Some((from, deleted_at)) = parse_info(&String::from_utf8_lossy(&info)) else {
            continue;
        };
        let Ok(item) = root.resolve_link(&subdir("files").join(id)?) else {
            continue;
        };
        let Ok(meta) = root.fs().link_meta(item.real_path()) else {
            continue;
        };
        entries.push(TrashEntry {
            id: id.to_string(),
            from,
            deleted_at,
            kind: meta.kind,
            len: meta.len,
        });
    }
    entries.sort_by(|a, b| b.deleted_at.cmp(&a.deleted_at).then(a.id.cmp(&b.id)));
    Ok(entries)
}

/// Where each trash id came from, for a confirmation's target: the original
/// path where its info file says one, the id itself where it does not.
///
/// Lazy, so a prompt that shows three names reads three info files and not
/// one per id.
fn origins<'a>(root: &'a Root, ids: &'a [String]) -> impl Iterator<Item = String> + 'a {
    ids.iter().enumerate().map(move |(i, id)| {
        if i >= 3 {
            return id.clone();
        }
        locate(root, id)
            .map(|(_, _, from)| from.to_string())
            .unwrap_or_else(|_| id.clone())
    })
}

/// How many items the trash holds, by counting info files without reading
/// them: cheap enough to ask while a confirmation is drawn. `None` if the
/// directory cannot be read, which a prompt says nothing about rather than
/// guessing.
fn count(root: &Root) -> Option<usize> {
    let info_dir = match root.resolve(&subdir("info")) {
        Ok(r) => r,
        Err(FileError::NotFound { .. }) => return Some(0),
        Err(_) => return None,
    };
    match root.fs().read_dir(info_dir.real_path()) {
        Ok(names) => Some(names.iter().filter(|n| n.ends_with(INFO_SUFFIX)).count()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Some(0),
        Err(_) => None,
    }
}

/// The item and info file for an id, refusing an id that is not one name.
fn locate(root: &Root, id: &str) -> Result<(Resolved, Resolved, VirtualPath), FileError> {
    // `join` refuses `/`, `..` and the empty name, so an id cannot reach
    // outside `files/`.
    let item = root.resolve_link(&subdir("files").join(id)?)?;
    let info_path = subdir("info").join(&format!("{id}{INFO_SUFFIX}"))?;
    let info = root.resolve_link(&info_path)?;
    let text = root.read(&info_path, 64 * 1024)?;
    let (from, _) = parse_info(&String::from_utf8_lossy(&text)).ok_or_else(|| FileError::Io {
        path: info_path.clone(),
        source: io::Error::other(
            "this trash entry's info file does not say where it came from, so it cannot be \
             put back; move it out of /.Trash/files by hand",
        ),
    })?;
    root.fs()
        .link_meta(item.real_path())
        .map_err(|e| root.map_io(item.path(), e))?;
    Ok((item, info, from))
}

/// Put each item back where it came from, or none of them. Missing parent
/// folders are recreated; an existing file at the original path is not
/// replaced.
pub fn restore(root: &Root, ids: &[String]) -> Result<Vec<VirtualPath>, FileError> {
    let mut planned = Vec::new();
    for id in ids {
        let (item, info, from) = locate(root, id)?;
        let dst = root.resolve_link(&from);
        let dst = match dst {
            Ok(d) => d,
            // The folder it came from is gone: made again below.
            Err(FileError::NotFound { .. }) => {
                recreate_parents(root, &from)?;
                root.resolve_link(&from)?
            }
            Err(e) => return Err(e),
        };
        match root.fs().link_meta(dst.real_path()) {
            Ok(_) => {
                return Err(FileError::Exists { path: from.clone() });
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(root.map_io(&from, e)),
        }
        if planned
            .iter()
            .any(|(_, _, d): &(Resolved, Resolved, Resolved)| d.path() == dst.path())
        {
            return Err(FileError::Exists { path: from });
        }
        planned.push((item, info, dst));
    }
    let mut done: Vec<&(Resolved, Resolved, Resolved)> = Vec::new();
    for entry in &planned {
        let (item, _, dst) = entry;
        if let Err(e) = root.fs().rename(item.real_path(), dst.real_path()) {
            for (item, _, dst) in done.iter().rev() {
                let _ = root.fs().rename(dst.real_path(), item.real_path());
            }
            return Err(root.map_io(dst.path(), e));
        }
        done.push(entry);
    }
    // Only once everything is back: an info file removed early would make a
    // rolled-back item untraceable.
    for (_, info, _) in &planned {
        let _ = root.fs().remove_file(info.real_path());
    }
    Ok(planned
        .into_iter()
        .map(|(_, _, d)| d.path().clone())
        .collect())
}

fn recreate_parents(root: &Root, path: &VirtualPath) -> Result<(), FileError> {
    let mut at = VirtualPath::root();
    let parts: Vec<&str> = path.components().collect();
    for part in &parts[..parts.len().saturating_sub(1)] {
        at = at.join(part)?;
        match root.create_dir(&at) {
            Ok(_) | Err(FileError::Exists { .. }) => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

/// Delete items from the trash for good. `None` empties it.
pub fn empty(root: &Root, ids: Option<&[String]>) -> Result<usize, FileError> {
    let ids: Vec<String> = match ids {
        Some(ids) => ids.to_vec(),
        None => list(root)?.into_iter().map(|e| e.id).collect(),
    };
    let mut removed = 0;
    for id in &ids {
        let item = root.resolve_link(&subdir("files").join(id)?)?;
        let info = root.resolve_link(&subdir("info").join(&format!("{id}{INFO_SUFFIX}"))?)?;
        match root.fs().link_meta(item.real_path()) {
            Ok(meta) => {
                let gone = if meta.kind.is_dir() {
                    root.fs().remove_dir_all(item.real_path())
                } else {
                    root.fs().remove_file(item.real_path())
                };
                gone.map_err(|e| root.map_io(item.path(), e))?;
                removed += 1;
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(root.map_io(item.path(), e)),
        }
        match root.fs().remove_file(info.real_path()) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(root.map_io(info.path(), e)),
        }
    }
    Ok(removed)
}

// ---------------------------------------------------------------------------
// The commands
// ---------------------------------------------------------------------------

fn local(files: &Namespace, address: &NodePath) -> Result<VirtualPath, FileError> {
    if files.is_local(address) {
        Ok(address.path.clone())
    } else {
        Err(FileError::NotLocal {
            address: address.clone(),
        })
    }
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TrashParams {
    pub paths: Vec<NodePath>,
}

#[derive(Debug, Clone, Serialize)]
pub struct TrashedAll {
    pub trashed: Vec<Trashed>,
}

pub struct TrashEntries {
    pub(crate) files: Arc<Namespace>,
}

impl Command for TrashEntries {
    const ID: &'static str = "files.trash";
    const TITLE: &'static str = "Move to the trash";
    /// Undone by `files.trash.restore`, so not destructive.
    const RISK: RiskLevel = RiskLevel::Write;

    type Params = TrashParams;
    type Output = TrashedAll;

    async fn run(&self, params: TrashParams, cx: Context) -> Result<TrashedAll, CommandError> {
        if params.paths.is_empty() {
            return Err(CommandError::BadParams {
                command: cx.command.clone(),
                message: "paths is empty; name at least one file or folder".into(),
            });
        }
        let files = Arc::clone(&self.files);
        crate::files::off_thread(cx.command.clone(), move || {
            let paths = params
                .paths
                .iter()
                .map(|a| local(&files, a))
                .collect::<Result<Vec<_>, _>>()?;
            Ok(TrashedAll {
                trashed: trash(files.root(), &paths, now())?,
            })
        })
        .await
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct TrashListing {
    pub entries: Vec<TrashEntry>,
}

pub struct ListTrash {
    pub(crate) files: Arc<Namespace>,
}

impl Command for ListTrash {
    const ID: &'static str = "files.trash.list";
    const TITLE: &'static str = "What is in the trash";
    const RISK: RiskLevel = RiskLevel::ReadOnly;

    type Params = NoParams;
    type Output = TrashListing;

    async fn run(&self, _params: NoParams, cx: Context) -> Result<TrashListing, CommandError> {
        let files = Arc::clone(&self.files);
        crate::files::off_thread(cx.command.clone(), move || {
            Ok(TrashListing {
                entries: list(files.root())?,
            })
        })
        .await
    }
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct IdsParams {
    /// Trash ids, as `files.trash` and `files.trash.list` report them.
    pub ids: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Restored {
    pub paths: Vec<NodePath>,
}

pub struct RestoreTrash {
    pub(crate) files: Arc<Namespace>,
}

impl Command for RestoreTrash {
    const ID: &'static str = "files.trash.restore";
    const TITLE: &'static str = "Put back from the trash";
    const RISK: RiskLevel = RiskLevel::Write;

    type Params = IdsParams;
    type Output = Restored;

    async fn run(&self, params: IdsParams, cx: Context) -> Result<Restored, CommandError> {
        let files = Arc::clone(&self.files);
        crate::files::off_thread(cx.command.clone(), move || {
            let back = restore(files.root(), &params.ids)?;
            Ok(Restored {
                paths: back.iter().map(|p| files.qualify(p)).collect(),
            })
        })
        .await
    }
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EmptyParams {
    /// Which items to delete for good. Absent empties the whole trash.
    #[serde(default)]
    pub ids: Option<Vec<String>>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Emptied {
    pub removed: usize,
}

pub struct EmptyTrash {
    pub(crate) files: Arc<Namespace>,
}

impl Command for EmptyTrash {
    const ID: &'static str = "files.trash.empty";
    const TITLE: &'static str = "Empty the trash";
    /// Not undoable from inside Deckjack, which is what `Destructive` means
    /// here: the operator confirms, and an agent's call waits in the Inbox.
    const RISK: RiskLevel = RiskLevel::Destructive;

    type Params = EmptyParams;
    type Output = Emptied;

    /// The items' original paths when the call names them, and how many
    /// items there are when it empties everything: "Empty the trash" with no
    /// object is the prompt that made a bait-and-switch work.
    fn subject(&self, params: &EmptyParams) -> Option<String> {
        let root = self.files.root();
        match params.ids.as_deref() {
            Some(ids) => crate::command::name_paths(origins(root, ids)),
            None => Some(match count(root) {
                Some(1) => "everything in the trash (1 item)".to_string(),
                Some(n) => format!("everything in the trash ({n} items)"),
                None => "everything in the trash".to_string(),
            }),
        }
    }

    async fn run(&self, params: EmptyParams, cx: Context) -> Result<Emptied, CommandError> {
        let files = Arc::clone(&self.files);
        crate::files::off_thread(cx.command.clone(), move || {
            Ok(Emptied {
                removed: empty(files.root(), params.ids.as_deref())?,
            })
        })
        .await
    }
}
