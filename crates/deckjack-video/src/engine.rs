//! The engine: FilmCraft sessions, one per open project, behind this app's
//! own command surface.
//!
//! FilmCraft exposes about 700 commands through `Session::execute`. This app
//! exposes the few dozen a cuts-and-audio editor needs, each with its own
//! risk level, and each naming its targets explicitly: FilmCraft's
//! selection-dependent behavior is never reached, because a selection is UI
//! state and two windows (or a window and an agent) would fight over it.
//! There is deliberately no passthrough (docs/lanes/5d-video.md, "Refused on
//! purpose").
//!
//! **One session per project file.** Every window, on any device, and every
//! agent attaches to the same session, so two copies of one project never
//! diverge and the last save never silently discards the other's edits.
//!
//! Every call here blocks (decoding, rendering, file I/O); the bus handlers
//! run them on tokio's blocking pool.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, SystemTime};

use filmcraft_engine::Session;
use filmcraft_engine::autosave::AutosaveConfig;
use serde_json::{Value, json};
use tokio::sync::watch;

use crate::names::{self, NameError};
use crate::timeline::{self, Describe, Rate};

/// Tracks a new project starts with: the brief's v1 minimum is V1 and A1–A2,
/// and a second video track is what an overlay or a cutaway needs.
pub const NEW_VIDEO_TRACKS: u32 = 2;
pub const NEW_AUDIO_TRACKS: u32 = 2;

/// Longest a `video.project.wait` call is held before it answers anyway.
pub const MAX_WAIT: Duration = Duration::from_secs(55);

#[derive(Debug, thiserror::Error)]
pub enum VideoError {
    #[error(transparent)]
    Name(#[from] NameError),
    #[error("there is already a project called {0:?}")]
    Exists(String),
    #[error("there is no project called {0:?} in Videos/")]
    NoProject(String),
    #[error("{0}")]
    Engine(String),
    #[error("{0}")]
    Refused(String),
    #[error("{doing}: {source}")]
    Io {
        doing: String,
        #[source]
        source: std::io::Error,
    },
}

impl VideoError {
    fn io(doing: impl Into<String>) -> impl FnOnce(std::io::Error) -> VideoError {
        let doing = doing.into();
        move |source| VideoError::Io { doing, source }
    }
}

pub type Result<T> = std::result::Result<T, VideoError>;

/// All open projects, and everything else that outlives one command.
pub struct Studio {
    root: PathBuf,
    data: PathBuf,
    open: Mutex<HashMap<String, Arc<Project>>>,
    /// Held for the whole of an open or a create, so two first uses of one
    /// project (a window opening it while an agent's command names it) build
    /// one session between them, not one each with the second replacing the
    /// first in `open` and the first caller's edit landing on an orphan.
    /// Opens are rare and short next to edits, so one lock for all projects
    /// costs nothing worth a lock per name.
    opening: Mutex<()>,
}

/// One open project: its FilmCraft session and the revision windows wait on.
pub struct Project {
    pub name: String,
    pub path: PathBuf,
    session: Mutex<Session>,
    revision: watch::Sender<u64>,
    /// Set when the session was rebuilt from the OTIO sidecar because the
    /// project file would not open: the note the caller is shown. While it
    /// is set, the first save copies the unopened file aside before
    /// replacing it, because "would not open" may mean "from a newer
    /// FilmCraft" or "briefly unreadable", not "worthless".
    rebuilt: Mutex<Option<String>>,
}

/// What opening a project found, beyond its state.
#[derive(Debug, Clone, serde::Serialize, PartialEq)]
pub struct Opened {
    /// Unsaved edits a crashed session left behind, offered rather than
    /// applied: `video.project.recover` applies them.
    pub recovery: Option<Recovery>,
    /// Set when the project file was missing or unreadable and the OTIO
    /// sidecar was imported instead, with what that format does not keep.
    pub imported_sidecar: Option<String>,
}

#[derive(Debug, Clone, serde::Serialize, PartialEq)]
pub struct Recovery {
    pub id: String,
    /// Unix seconds when the journal was written.
    pub saved_unix: i64,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    // Every holder leaves the session consistent (FilmCraft edits are
    // copy-on-write and swapped in whole), so a poisoned lock is still a
    // usable one.
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Studio {
    /// `root` is `Videos/`, created if it is not there (the core creates it
    /// when building the sandbox; this is for the unconfined case). `data` is
    /// the app's private directory for FilmCraft's preferences and recovery
    /// journal.
    pub fn new(root: PathBuf, data: PathBuf) -> Result<Studio> {
        std::fs::create_dir_all(&root).map_err(VideoError::io("create Videos/"))?;
        std::fs::create_dir_all(&data).map_err(VideoError::io("create the app's data folder"))?;
        Ok(Studio {
            root,
            data,
            open: Mutex::new(HashMap::new()),
            opening: Mutex::new(()),
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Project names in `Videos/`, sorted.
    pub fn list(&self) -> Result<Vec<Value>> {
        let mut out = Vec::new();
        let read = std::fs::read_dir(&self.root).map_err(VideoError::io("list Videos/"))?;
        for entry in read.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some(names::PROJECT_EXTENSION) {
                continue;
            }
            let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            if stem.starts_with('.') {
                continue;
            }
            let meta = entry.metadata().ok();
            out.push(json!({
                "name": stem,
                "modified": meta.as_ref().and_then(|m| m.modified().ok()).and_then(unix),
                "open": lock(&self.open).contains_key(stem),
            }));
        }
        out.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
        Ok(out)
    }

    /// Media files in `Videos/`, recursively, skipping hidden folders and
    /// FilmCraft's `Auto-Save/`. Paths are relative to `Videos/`.
    pub fn media_files(&self) -> Vec<Value> {
        let mut out = Vec::new();
        walk(&self.root, &self.root, 0, &mut out);
        out.sort_by(|a, b| a["path"].as_str().cmp(&b["path"].as_str()));
        out
    }

    /// Make a new project and its sidecar, refusing a name already taken by
    /// either file.
    pub fn create(&self, name: &str) -> Result<Arc<Project>> {
        let name = names::project_name(name)?;
        let _opening = lock(&self.opening);
        let path = names::project_path(&self.root, &name);
        if lock(&self.open).contains_key(&name)
            || path.exists()
            || names::sidecar_path(&self.root, &name).exists()
        {
            return Err(VideoError::Exists(name));
        }
        let mut session = self.session()?;
        run(&mut session, "file.newProject", json!({ "name": name }))?;
        run(
            &mut session,
            "file.newSequence",
            json!({
                "name": name,
                // The first clip placed decides the real size and rate
                // (`matchSequence` on every insert); these are only what an
                // empty project shows.
                "width": 1920, "height": 1080, "fps": 30,
                "video": NEW_VIDEO_TRACKS, "audio": NEW_AUDIO_TRACKS,
            }),
        )?;
        let project = self.adopt(name.clone(), path, session, None);
        project.save()?;
        Ok(project)
    }

    /// The open session for a project, opening it if no one has.
    pub fn open(&self, name: &str) -> Result<(Arc<Project>, Opened)> {
        let name = names::project_name(name)?;
        if let Some(found) = self.already_open(&name) {
            return Ok(found);
        }
        let _opening = lock(&self.opening);
        // Someone else may have opened it while this call waited.
        if let Some(found) = self.already_open(&name) {
            return Ok(found);
        }
        let path = names::project_path(&self.root, &name);
        let sidecar = names::sidecar_path(&self.root, &name);
        let mut session = self.session()?;
        let mut imported_sidecar = None;
        let opened = if path.exists() {
            run(
                &mut session,
                "file.open",
                json!({ "path": path_str(&path) }),
            )
        } else {
            Err(VideoError::NoProject(name.clone()))
        };
        if let Err(e) = opened {
            // The rule for the lane: an unreadable project with a sidecar
            // opens from the sidecar, and says what that format lost.
            if !sidecar.exists() {
                return Err(e);
            }
            session = self.session()?;
            run(&mut session, "file.newProject", json!({ "name": name }))?;
            run(
                &mut session,
                "file.import",
                json!({ "paths": [path_str(&sidecar)] }),
            )?;
            imported_sidecar = Some(format!(
                "{name}.fcproj could not be opened ({e}), so the timeline was rebuilt from \
                 {name}.otio. Clip gain, fades and anything else OpenTimelineIO does not carry \
                 are not in it. Saving writes a new {name}.fcproj and first keeps the one that \
                 would not open as {name}.{BACKUP_EXTENSION}."
            ));
            session.path = Some(path_str(&path));
        }
        ensure_sequence(&mut session, &name)?;
        let project = self.adopt(name, path, session, imported_sidecar.clone());
        let recovery = project.recovery();
        Ok((
            project,
            Opened {
                recovery,
                imported_sidecar,
            },
        ))
    }

    /// An open project and what opening it found, as a later caller sees
    /// it: the sidecar note stays until a save settles it, so a window
    /// opening a project an agent opened first is still told.
    fn already_open(&self, name: &str) -> Option<(Arc<Project>, Opened)> {
        let p = lock(&self.open).get(name).cloned()?;
        let opened = Opened {
            recovery: p.recovery(),
            imported_sidecar: lock(&p.rebuilt).clone(),
        };
        Some((p, opened))
    }

    /// An open project, or a refusal naming the command that opens it.
    pub fn get(&self, name: &str) -> Result<Arc<Project>> {
        let name = names::project_name(name)?;
        if let Some(p) = lock(&self.open).get(&name) {
            return Ok(Arc::clone(p));
        }
        // Opening on first use is what an agent expects of a command that
        // names its project; a window has always opened it already.
        self.open(&name).map(|(p, _)| p)
    }

    /// Flush every journal and stop every autosave worker, on the way out.
    pub fn shutdown(&self) {
        for p in lock(&self.open).values() {
            lock(&p.session).shutdown();
        }
    }

    fn session(&self) -> Result<Session> {
        let mut session = Session::default();
        session
            .start_autosave(AutosaveConfig::new(&self.data))
            .map_err(VideoError::io("start autosave and the recovery journal"))?;
        Ok(session)
    }

    fn adopt(
        &self,
        name: String,
        path: PathBuf,
        session: Session,
        rebuilt: Option<String>,
    ) -> Arc<Project> {
        let revision = session.revision;
        let project = Arc::new(Project {
            name: name.clone(),
            path,
            session: Mutex::new(session),
            revision: watch::Sender::new(revision),
            rebuilt: Mutex::new(rebuilt),
        });
        lock(&self.open).insert(name, Arc::clone(&project));
        project
    }
}

impl Project {
    /// Run one FilmCraft command against this project and tell waiting
    /// windows if it changed anything.
    pub fn execute(&self, id: &str, params: Value) -> Result<Value> {
        let mut s = lock(&self.session);
        let out = run(&mut s, id, params);
        s.poll_persistence();
        let rev = s.revision;
        drop(s);
        self.notify(rev);
        out
    }

    /// Tell waiting windows the revision is now `rev`, if that is news.
    fn notify(&self, rev: u64) {
        self.revision.send_if_modified(|r| {
            let changed = *r != rev;
            *r = rev;
            changed
        });
    }

    /// Read the session without changing it.
    pub fn with<T>(&self, f: impl FnOnce(&Session) -> T) -> T {
        f(&lock(&self.session))
    }

    pub fn revision(&self) -> u64 {
        *self.revision.borrow()
    }

    /// Wait until the revision differs from `since`, or `timeout` passes.
    pub async fn wait(&self, since: u64, timeout: Duration) -> u64 {
        let mut rx = self.revision.subscribe();
        let _ = tokio::time::timeout(timeout.min(MAX_WAIT), rx.wait_for(|r| *r != since)).await;
        *rx.borrow()
    }

    /// Save the project and rewrite its OpenTimelineIO sidecar.
    ///
    /// The sidecar goes to a hidden name first and is renamed over the old
    /// one, so a reader never sees half of it and a failure leaves the last
    /// good one in place.
    pub fn save(&self) -> Result<Value> {
        let path = path_str(&self.path);
        // Held across the save so two saves do not both make a copy.
        let mut rebuilt = lock(&self.rebuilt);
        let kept = match rebuilt.as_ref() {
            Some(_) => keep_unopened(&self.path)?,
            None => None,
        };
        let saved = self.execute("file.save", json!({ "path": path }))?;
        let dir = self.path.parent().unwrap_or(Path::new("."));
        let sidecar = names::sidecar_path(dir, &self.name);
        let staging = dir.join(format!(".{}.otio.partial", self.name));
        let written = self
            .execute("file.exportOtio", json!({ "path": path_str(&staging) }))
            .and_then(|_| {
                std::fs::rename(&staging, &sidecar)
                    .map_err(VideoError::io(format!("write {}.otio", self.name)))
            });
        if written.is_err() {
            let _ = std::fs::remove_file(&staging);
        }
        // The project file is written: the copy is made, and a later save
        // replaces a file this app wrote itself.
        *rebuilt = None;
        drop(rebuilt);
        written?;
        Ok(json!({
            "saved": format!("{}.fcproj", self.name),
            "sidecar": format!("{}.otio", self.name),
            "bytes": saved.get("bytes"),
            "kept_unopened_as": kept,
        }))
    }

    /// The crash-recovery journal entry for this project, if a session that
    /// died left one.
    ///
    /// Only a journal written after the project file was last saved is
    /// offered. A save after the crash is the operator choosing a newer
    /// version, and offering the older crash snapshot on every later open
    /// would invite rolling the newer cut back.
    pub fn recovery(&self) -> Option<Recovery> {
        let want = path_str(&self.path);
        let saved = std::fs::metadata(&self.path)
            .and_then(|m| m.modified())
            .ok();
        self.with(|s| {
            s.recovery_candidates()
                .iter()
                .filter(|c| c.meta.project_path.as_deref() == Some(want.as_str()))
                .find(|c| match (saved, journal_written(c)) {
                    (Some(saved), Some(journal)) => journal > saved,
                    // No project file (it was rebuilt from the sidecar, or
                    // removed), or a journal whose time cannot be read:
                    // offering is the side that loses nothing.
                    _ => true,
                })
                .map(|c| Recovery {
                    id: c.id.clone(),
                    saved_unix: c.meta.saved_unix,
                })
        })
    }

    /// Die the way a crash does: the journal stays and its lock is released,
    /// so the next session finds it as a recovery candidate. Tests only.
    #[cfg(test)]
    pub fn crash(&self) {
        if let Some(p) = lock(&self.session).persistence.take() {
            p.simulate_crash();
        }
    }

    /// Replace the open state with the journal's unsaved edits.
    ///
    /// FilmCraft replaces the project and clears the undo history, so when
    /// the open state has unsaved edits of its own they are first saved as a
    /// copy, `<name> (before recovery).fcproj`, which lists and opens as a
    /// project of its own. Nothing is lost either way, and the copy's name is
    /// in the answer. The session stays locked from the copy to the swap, so
    /// no edit can land between them and be lost.
    pub fn recover(&self) -> Result<Value> {
        let Some(r) = self.recovery() else {
            return Err(VideoError::Refused(format!(
                "{} has no unsaved changes from an earlier session to recover",
                self.name
            )));
        };
        let mut s = lock(&self.session);
        let kept = if s.is_dirty() {
            let dir = self.path.parent().unwrap_or(Path::new("."));
            let name = (1u32..)
                .map(|n| match n {
                    1 => format!("{} (before recovery)", self.name),
                    n => format!("{} (before recovery {n})", self.name),
                })
                .find(|n| {
                    std::fs::symlink_metadata(names::project_path(dir, n)).is_err()
                        && std::fs::symlink_metadata(names::sidecar_path(dir, n)).is_err()
                })
                .unwrap_or_else(|| format!("{} (before recovery)", self.name));
            let copy = names::project_path(dir, &name);
            run(&mut s, "file.saveCopy", json!({ "path": path_str(&copy) }))?;
            Some(format!("{name}.{}", names::PROJECT_EXTENSION))
        } else {
            None
        };
        let recovered = run(&mut s, "file.recover", json!({ "id": r.id }))
            .and_then(|_| ensure_sequence(&mut s, &self.name));
        s.poll_persistence();
        let rev = s.revision;
        drop(s);
        self.notify(rev);
        recovered?;
        Ok(json!({
            "recovered": r.id,
            "saved_unix": r.saved_unix,
            "kept_unsaved_as": kept,
        }))
    }

    /// The sequence's rate. Every frame number in and out goes through it.
    pub fn rate(&self) -> Result<Rate> {
        let seq = self.execute("sequence.inspect", json!({}))?;
        timeline::sequence_rate(&seq)
            .ok_or_else(|| VideoError::Engine("the sequence has no frame rate".into()))
    }

    /// The whole state, in frames, for windows and agents.
    pub fn describe(&self, root: &Path) -> Result<Describe> {
        let seq = self.execute("sequence.inspect", json!({}))?;
        let rate = timeline::sequence_rate(&seq)
            .ok_or_else(|| VideoError::Engine("the sequence has no frame rate".into()))?;
        let settings = &seq["settings"];
        let (tracks, markers) = timeline::tracks_and_markers(&seq, rate);
        let dir = self.path.parent().unwrap_or(root).to_path_buf();
        let (dirty, can_undo, can_redo, items) = self.with(|s| {
            let items: Vec<(u64, Value)> = s
                .project
                .items
                .iter()
                .filter_map(|(id, item)| Some((id.0, serde_json::to_value(item).ok()?)))
                .collect();
            (
                s.is_dirty(),
                s.history.can_undo(),
                s.history.can_redo(),
                items,
            )
        });
        let media = items
            .iter()
            .filter_map(|(id, item)| {
                timeline::media(*id, item, rate, |stored| relative_to(root, &dir, stored))
            })
            .collect();
        Ok(Describe {
            project: self.name.clone(),
            revision: self.revision(),
            dirty,
            can_undo,
            can_redo,
            fps: rate,
            width: settings["width"].as_u64().unwrap_or(0),
            height: settings["height"].as_u64().unwrap_or(0),
            duration: seq["durationFrames"].as_i64().unwrap_or(0),
            tracks,
            markers,
            media,
        })
    }

    /// Render the program at a frame, scaled to about `width` pixels wide.
    /// Returns the RGBA bytes and the size.
    pub fn render(&self, frame: i64, width: u32) -> Result<(Vec<u8>, u32, u32)> {
        let rate = self.rate()?;
        let seq_width = self.execute("sequence.inspect", json!({}))?["settings"]["width"]
            .as_u64()
            .unwrap_or(1920)
            .max(1);
        let scale = (f64::from(width) / seq_width as f64).clamp(0.05, 1.0) as f32;
        let tick = rate.ticks(frame.max(0));
        self.with(|s| {
            let (img, _offline) = s
                .try_render_program_with_offline_at(scale, filmcraft_engine::time::Tick(tick))
                .map_err(|e| VideoError::Engine(e.to_string()))?;
            let w = u32::try_from(img.w).unwrap_or(0);
            let h = u32::try_from(img.h).unwrap_or(0);
            Ok((img.over_black_rgba8(), w, h))
        })
    }

    /// Start a FilmCraft export job writing `path`. Returns the job id and
    /// its shared progress and result.
    pub fn start_export(&self, path: &Path, settings: Value) -> Result<filmcraft_engine::Job> {
        let mut params = settings;
        params["path"] = json!(path_str(path));
        params["wait"] = json!(false);
        let started = self.execute("file.exportMedia", params)?;
        let job = started["job"]
            .as_u64()
            .ok_or_else(|| VideoError::Engine(format!("the export did not start: {started}")))?;
        self.with(|s| s.jobs.iter().find(|j| j.id == job).cloned())
            .ok_or_else(|| VideoError::Engine(format!("export job {job} vanished")))
    }
}

/// Run one FilmCraft command, with its error as this app's.
fn run(s: &mut Session, id: &str, params: Value) -> Result<Value> {
    s.execute(id, params)
        .map_err(|e| VideoError::Engine(format!("{id}: {e}")))
}

/// A project with no sequence (made by hand in FilmCraft, or an OTIO import
/// that brought none) gets one, so every edit has somewhere to land.
fn ensure_sequence(s: &mut Session, name: &str) -> Result<()> {
    if s.state.active_sequence.is_some() {
        return Ok(());
    }
    let existing = s
        .project
        .items
        .iter()
        .find(|(_, i)| matches!(i.kind, filmcraft_engine::project::ItemKind::Sequence(_)))
        .map(|(id, _)| id.0);
    match existing {
        Some(id) => run(s, "sequence.open", json!({ "item": id })).map(|_| ()),
        None => run(
            s,
            "file.newSequence",
            json!({
                "name": name, "width": 1920, "height": 1080, "fps": 30,
                "video": NEW_VIDEO_TRACKS, "audio": NEW_AUDIO_TRACKS,
            }),
        )
        .map(|_| ()),
    }
}

/// When a recovery journal was last written: its newest file's time.
fn journal_written(c: &filmcraft_engine::autosave::RecoveryCandidate) -> Option<SystemTime> {
    ["session.json", "snapshot.fcproj"]
        .iter()
        .filter_map(|f| {
            std::fs::metadata(c.dir.join(f))
                .and_then(|m| m.modified())
                .ok()
        })
        .max()
}

/// What a project file that would not open is kept as before a save
/// replaces it: `<name>.fcproj.unopened`, not listed as a project and not
/// a media file, so nothing in the app touches it again.
pub const BACKUP_EXTENSION: &str = "fcproj.unopened";

/// Copy a project file that would not open aside, under the first free
/// `<name>.fcproj.unopened`, `<name> 2.fcproj.unopened`… name. Copied rather
/// than linked so it works on any filesystem, and created exclusively so it
/// never replaces an earlier copy. `None` when there is no file to keep.
fn keep_unopened(path: &Path) -> Result<Option<String>> {
    use std::io::ErrorKind;
    let mut from = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(VideoError::Io {
                doing: format!("keep {} before replacing it", path.display()),
                source,
            });
        }
    };
    let dir = path.parent().unwrap_or(Path::new("."));
    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    for n in 1u32.. {
        let name = if n == 1 {
            format!("{stem}.{BACKUP_EXTENSION}")
        } else {
            format!("{stem} {n}.{BACKUP_EXTENSION}")
        };
        let target = dir.join(&name);
        let mut to = match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&target)
        {
            Ok(f) => f,
            Err(e) if e.kind() == ErrorKind::AlreadyExists => continue,
            Err(source) => {
                return Err(VideoError::Io {
                    doing: format!("keep the unopened project as {name}"),
                    source,
                });
            }
        };
        let copied = std::io::copy(&mut from, &mut to).and_then(|_| to.sync_all());
        if let Err(source) = copied {
            let _ = std::fs::remove_file(&target);
            return Err(VideoError::Io {
                doing: format!("keep the unopened project as {name}"),
                source,
            });
        }
        return Ok(Some(name));
    }
    unreachable!("u32 names run out long after the disk does")
}

pub fn path_str(p: &Path) -> String {
    p.to_string_lossy().into_owned()
}

fn unix(t: std::time::SystemTime) -> Option<u64> {
    t.duration_since(std::time::UNIX_EPOCH)
        .ok()
        .map(|d| d.as_secs())
}

/// A path FilmCraft stored (relative to the project's folder, or absolute)
/// as one relative to `Videos/`, when it is inside it.
fn relative_to(root: &Path, project_dir: &Path, stored: &str) -> Option<String> {
    let joined = project_dir.join(stored);
    let mut clean = PathBuf::new();
    for part in joined.components() {
        match part {
            std::path::Component::ParentDir => {
                clean.pop();
            }
            std::path::Component::CurDir => {}
            other => clean.push(other),
        }
    }
    clean
        .strip_prefix(root)
        .ok()
        .map(|p| p.to_string_lossy().into_owned())
}

/// How deep the media listing looks. A camera card's folder layout is two or
/// three levels; deeper is a tree Files is for.
const MEDIA_DEPTH: usize = 4;

fn walk(root: &Path, dir: &Path, depth: usize, out: &mut Vec<Value>) {
    let Ok(read) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in read.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') || name == "Auto-Save" {
            continue;
        }
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        let path = entry.path();
        if kind.is_dir() {
            if depth + 1 < MEDIA_DEPTH {
                walk(root, &path, depth + 1, out);
            }
        } else if kind.is_file() && names::is_media(&path) {
            let meta = entry.metadata().ok();
            out.push(json!({
                "path": path.strip_prefix(root).unwrap_or(&path).to_string_lossy(),
                "name": name,
                "size": meta.as_ref().map(|m| m.len()),
                "modified": meta.as_ref().and_then(|m| m.modified().ok()).and_then(unix),
            }));
        }
    }
}
