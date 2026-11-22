//! Exports: H.264/AAC MP4 at source size, 1080p or 720p.
//!
//! Two promises, both from the brief:
//!
//! - **It never overwrites.** The file is written under a hidden partial name
//!   and published with a hard link, which fails rather than replaces when
//!   the name is taken; a taken name moves on to `name 2.mp4`. A filesystem
//!   without hard links (exFAT, FAT32, some network mounts) gets an exclusive
//!   create and a copy instead, which keeps the same promise.
//! - **A cancelled or failed export leaves nothing behind.** The partial file
//!   is removed when the job ends any way but success, and a partial left by
//!   a crash is swept when the app next starts.
//!
//! The sandbox bounds what an export may touch, not how much CPU it takes,
//! so this module bounds that itself: an export runs on at most half the
//! cores ([`export_threads`]) at a lowered priority ([`EXPORT_NICE`]), so a
//! long render never starves the core it shares the machine with.
//!
//! Cancellation is this process's own: the SDK has no cancellation signal
//! yet (`TODO(cancel)` in `deckjack-app`), so `video.export.cancel` is a
//! command of its own that sets FilmCraft's cancel flag on the job.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use serde::Deserialize;
use serde_json::{Value, json};

use crate::engine::{Project, Result, VideoError};
use crate::names;

/// The sizes the export sheet offers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, schemars::JsonSchema)]
pub enum Size {
    /// The sequence's own frame size.
    #[serde(rename = "source")]
    Source,
    /// 1080 lines, width to the sequence's aspect. Never upscales.
    #[serde(rename = "1080p")]
    P1080,
    /// 720 lines, width to the sequence's aspect. Never upscales.
    #[serde(rename = "720p")]
    P720,
}

impl Size {
    /// The frame size for a sequence of `w`×`h`. Sizes are even, which H.264
    /// in 4:2:0 requires, and a target taller than the sequence is the
    /// sequence's size: upscaling adds bytes and no picture.
    pub fn frame(self, w: u64, h: u64) -> (u64, u64) {
        let lines = match self {
            Size::Source => return (even(w), even(h)),
            Size::P1080 => 1080,
            Size::P720 => 720,
        };
        if h <= lines || h == 0 {
            return (even(w), even(h));
        }
        (even((w * lines + h / 2) / h), lines)
    }
}

fn even(n: u64) -> u64 {
    (n / 2 * 2).max(2)
}

#[derive(Debug, Clone, PartialEq)]
enum State {
    Running,
    Done { name: String },
    Failed { error: String },
    Cancelled,
}

struct Job {
    id: u64,
    project: String,
    size: Size,
    partial: PathBuf,
    stem: String,
    engine: filmcraft_engine::Job,
    cancelled: AtomicBool,
    state: Mutex<State>,
}

impl Job {
    fn state(&self) -> State {
        self.state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    fn json(&self) -> Value {
        let (state, name, error) = match self.state() {
            State::Running => ("running", None, None),
            State::Done { name } => ("done", Some(name), None),
            State::Failed { error } => ("failed", None, Some(error)),
            State::Cancelled => ("cancelled", None, None),
        };
        let p = &self.engine.progress;
        json!({
            "id": self.id,
            "project": self.project,
            "size": match self.size { Size::Source => "source", Size::P1080 => "1080p", Size::P720 => "720p" },
            "state": state,
            "progress": if state == "done" { 1.0 } else { p.fraction() },
            "eta_seconds": if state == "running" { p.eta().map(|d| d.as_secs_f64()) } else { None },
            "name": name,
            "error": error,
        })
    }
}

/// Every export this process has started.
pub struct Exports {
    root: PathBuf,
    next: AtomicU64,
    jobs: Mutex<BTreeMap<u64, Arc<Job>>>,
}

/// How often a job's watcher looks for the engine's result.
const POLL: Duration = Duration::from_millis(100);

/// The nice value an export's threads run at, at least. Ten is what `nice`
/// gives by default: background work that still finishes on an idle machine.
pub const EXPORT_NICE: i32 = 10;

/// Threads an export may use on a machine with `cores`: half, and at least
/// one, so the core and the rest of the desktop keep the other half.
pub fn export_threads(cores: usize) -> u32 {
    u32::try_from((cores / 2).max(1)).unwrap_or(u32::MAX)
}

/// FilmCraft's export settings for one of this app's exports.
fn export_settings(width: u64, height: u64, loudness_lufs: Option<f64>, threads: u32) -> Value {
    let mut settings = json!({
        "format": "h264",
        "audioCodec": "aac",
        "width": width,
        "height": height,
        "bitrateMode": "crf",
        "crf": 20,
        "range": "entire",
        // FilmCraft runs a capped export on a rayon pool of its own instead
        // of the global one, which would take every core.
        "maxThreads": threads,
    });
    if let Some(l) = loudness_lufs {
        settings["loudnessLufs"] = json!(l);
    }
    settings
}

/// Run `f` on a thread of its own at [`EXPORT_NICE`] or lower priority, and
/// return what it returns.
///
/// On Linux the nice value is a per-thread attribute that a new thread
/// inherits from the one creating it, so the export thread FilmCraft spawns
/// inside `f`, and the rayon pool that thread builds, all run niced, while
/// the caller's own thread (one of tokio's, reused for other work) is left as
/// it was. Lowering is always allowed; a process already running nicer than
/// this keeps its own value. Best effort: a failure leaves normal priority.
pub fn niced<T: Send>(f: impl FnOnce() -> T + Send) -> T {
    std::thread::scope(|scope| {
        std::thread::Builder::new()
            .name("export-start".into())
            .spawn_scoped(scope, || {
                lower_priority(EXPORT_NICE);
                f()
            })
            .map(|h| h.join())
            .unwrap_or_else(|_| Err(Box::new("could not start a thread")))
    })
    .unwrap_or_else(|panic| std::panic::resume_unwind(panic))
}

/// The calling thread's nice value.
#[cfg(unix)]
pub fn thread_nice() -> i32 {
    // SAFETY: getpriority reads the calling thread's own attribute and has
    // no memory effects. `who` 0 is the caller.
    unsafe { libc::getpriority(libc::PRIO_PROCESS, 0) }
}

#[cfg(unix)]
fn lower_priority(nice: i32) {
    let current = thread_nice();
    if current < nice {
        // SAFETY: as above; on Linux `who` 0 names the calling thread alone.
        unsafe {
            libc::setpriority(libc::PRIO_PROCESS, 0, nice);
        }
    }
}

#[cfg(not(unix))]
fn lower_priority(_nice: i32) {}

impl Exports {
    pub fn new(root: PathBuf) -> Exports {
        Exports {
            root,
            next: AtomicU64::new(0),
            jobs: Mutex::new(BTreeMap::new()),
        }
    }

    fn jobs(&self) -> std::sync::MutexGuard<'_, BTreeMap<u64, Arc<Job>>> {
        self.jobs.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Remove exports a crash or a kill left half-written. Nothing will ever
    /// finish them, and they are hidden, so no one else would remove them.
    pub fn sweep_partials(&self) {
        let Ok(read) = std::fs::read_dir(&self.root) else {
            return;
        };
        for entry in read.flatten() {
            if names::is_partial(&entry.file_name().to_string_lossy()) {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }

    /// Start exporting a project. `name` is the file's stem; the project's
    /// name when absent. `loudness_lufs` normalizes the program to that
    /// integrated loudness at render time.
    pub fn start(
        self: &Arc<Self>,
        project: &Arc<Project>,
        size: Size,
        name: Option<&str>,
        loudness_lufs: Option<f64>,
    ) -> Result<Value> {
        let stem = names::project_name(name.unwrap_or(&project.name))?;
        if let Some(l) = loudness_lufs
            && !(-70.0..=-5.0).contains(&l)
        {
            return Err(VideoError::Refused(format!(
                "a loudness target of {l} LUFS is outside -70 to -5"
            )));
        }
        let describe = project.describe(&self.root)?;
        let (width, height) = size.frame(describe.width, describe.height);
        let id = self.next.fetch_add(1, Ordering::Relaxed) + 1;
        let partial = self.root.join(names::partial_name(&stem, id));
        let cores = std::thread::available_parallelism().map_or(1, |n| n.get());
        let settings = export_settings(width, height, loudness_lufs, export_threads(cores));
        let engine = niced(|| project.start_export(&partial, settings))?;
        let job = Arc::new(Job {
            id,
            project: project.name.clone(),
            size,
            partial,
            stem,
            engine,
            cancelled: AtomicBool::new(false),
            state: Mutex::new(State::Running),
        });
        self.jobs().insert(id, Arc::clone(&job));
        let exports = Arc::clone(self);
        std::thread::Builder::new()
            .name(format!("export-{id}"))
            .spawn(move || exports.watch(&job))
            .map_err(|e| VideoError::Engine(format!("could not start the export watcher: {e}")))?;
        Ok(self.get(id).unwrap_or(Value::Null))
    }

    /// Ask a running export to stop. Its partial file is removed when the
    /// engine has let go of it.
    pub fn cancel(&self, id: u64) -> Result<Value> {
        let job = self
            .jobs()
            .get(&id)
            .cloned()
            .ok_or_else(|| VideoError::Refused(format!("there is no export {id}")))?;
        if job.state() == State::Running {
            job.cancelled.store(true, Ordering::Relaxed);
            job.engine.progress.cancel.store(true, Ordering::Relaxed);
        }
        Ok(job.json())
    }

    pub fn cancel_all(&self) {
        let ids: Vec<u64> = self.jobs().keys().copied().collect();
        for id in ids {
            let _ = self.cancel(id);
        }
    }

    pub fn get(&self, id: u64) -> Option<Value> {
        self.jobs().get(&id).map(|j| j.json())
    }

    pub fn list(&self) -> Vec<Value> {
        self.jobs().values().rev().map(|j| j.json()).collect()
    }

    /// Whether an export has ended, any way.
    pub fn finished(&self, id: u64) -> bool {
        self.jobs()
            .get(&id)
            .is_none_or(|j| j.state() != State::Running)
    }

    /// Wait for the engine to finish a job, then publish or clean up.
    fn watch(&self, job: &Job) {
        let result = loop {
            let done = job
                .engine
                .result
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .clone();
            if let Some(r) = done {
                break r;
            }
            std::thread::sleep(POLL);
        };
        let state = if job.cancelled.load(Ordering::Relaxed) {
            State::Cancelled
        } else {
            match result {
                Ok(_) => match publish(&self.root, &job.partial, &job.stem) {
                    Ok(name) => State::Done { name },
                    Err(e) => State::Failed {
                        error: e.to_string(),
                    },
                },
                Err(error) => State::Failed { error },
            }
        };
        if !matches!(state, State::Done { .. }) {
            remove_leftovers(&job.partial);
        }
        *job.state.lock().unwrap_or_else(PoisonError::into_inner) = state;
    }
}

/// Give a finished partial file its public name, never replacing a file.
///
/// `hard_link` fails with `AlreadyExists` instead of replacing, which a
/// rename does not; the check-then-act race between two writers is closed by
/// the kernel rather than by hoping. The link is in the same directory, so
/// Landlock's cross-directory `REFER` right is not needed.
pub fn publish(root: &Path, partial: &Path, stem: &str) -> Result<String> {
    publish_with(root, partial, stem, |from, to| std::fs::hard_link(from, to))
}

/// [`publish`], with the link step given, so a filesystem without hard links
/// can be stood in for.
fn publish_with(
    root: &Path,
    partial: &Path,
    stem: &str,
    link: impl Fn(&Path, &Path) -> std::io::Result<()>,
) -> Result<String> {
    use std::io::ErrorKind;
    // `symlink_metadata`, not `exists`: `exists` follows a symlink, so a
    // dangling one reads as free while `hard_link` still finds the name
    // taken, and the same name would be tried forever.
    let taken = |n: &str| std::fs::symlink_metadata(root.join(n)).is_ok();
    let mut linking = true;
    loop {
        let name = names::export_name(stem, taken);
        let target = root.join(&name);
        let placed = if linking {
            match link(partial, &target) {
                // No hard links here: copy instead, from this name on.
                Err(e)
                    if matches!(
                        e.kind(),
                        ErrorKind::Unsupported | ErrorKind::PermissionDenied
                    ) && !e.raw_os_error().is_some_and(|c| c == libc::EACCES) =>
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
                return Ok(name);
            }
            Err(e) if e.kind() == ErrorKind::AlreadyExists => continue,
            Err(source) => {
                return Err(VideoError::Io {
                    doing: format!("publish the export as {name}"),
                    source,
                });
            }
        }
    }
}

/// Copy `from` to `to`, creating `to` exclusively: `AlreadyExists` if the
/// name is taken, and never a half-copied file left under it on failure.
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

/// The job's partial file. FilmCraft writes an MP4 export to that one path
/// and nothing beside it, so that is all there is to remove; matching by
/// prefix instead would also take another job's partial whose stem happens
/// to start the same way.
fn remove_leftovers(partial: &Path) {
    let _ = std::fs::remove_file(partial);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_keep_the_aspect_and_never_upscale() {
        assert_eq!(Size::P720.frame(1920, 1080), (1280, 720));
        assert_eq!(Size::P1080.frame(3840, 2160), (1920, 1080));
        // A portrait phone clip keeps its shape: 720 lines, not 720 wide.
        assert_eq!(Size::P720.frame(1080, 1920), (404, 720));
        // Smaller than the target stays as it is.
        assert_eq!(Size::P1080.frame(1280, 720), (1280, 720));
        // Odd sizes are made even, which 4:2:0 H.264 needs.
        assert_eq!(Size::Source.frame(193, 109), (192, 108));
    }

    #[test]
    fn publishing_never_replaces_a_file() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("Cut.mp4"), b"the operator's").unwrap();
        let partial = root.join(names::partial_name("Cut", 1));
        std::fs::write(&partial, b"new export").unwrap();
        let name = publish(root, &partial, "Cut").unwrap();
        assert_eq!(name, "Cut 2.mp4");
        assert_eq!(
            std::fs::read(root.join("Cut.mp4")).unwrap(),
            b"the operator's"
        );
        assert_eq!(
            std::fs::read(root.join("Cut 2.mp4")).unwrap(),
            b"new export"
        );
        assert!(!partial.exists(), "the partial file was left behind");
    }

    #[test]
    fn a_failed_job_removes_its_partial_and_no_other_jobs() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let partial = root.join(names::partial_name("A", 1));
        std::fs::write(&partial, b"x").unwrap();
        // A second job whose stem makes its partial name start with the
        // first one's: ".A.1.partial x.2.partial.mp4".
        let other = root.join(names::partial_name("A.1.partial x", 2));
        std::fs::write(&other, b"still rendering").unwrap();
        std::fs::write(root.join("A.mp4"), b"keep").unwrap();
        remove_leftovers(&partial);
        let mut left: Vec<String> = std::fs::read_dir(root)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        left.sort();
        assert_eq!(left, vec![".A.1.partial x.2.partial.mp4", "A.mp4"]);
    }

    /// `publish` on its own thread, failing the test rather than hanging it
    /// if it never returns.
    fn publish_within(root: &Path, partial: &Path, stem: &str) -> Result<String> {
        let (tx, rx) = std::sync::mpsc::channel();
        let (root, partial, stem) = (root.to_path_buf(), partial.to_path_buf(), stem.to_string());
        std::thread::spawn(move || {
            let _ = tx.send(publish(&root, &partial, &stem));
        });
        rx.recv_timeout(Duration::from_secs(5))
            .expect("publish never returned")
    }

    #[cfg(unix)]
    #[test]
    fn a_dangling_symlink_is_a_taken_name_not_an_endless_retry() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::os::unix::fs::symlink(root.join("moved away.mp4"), root.join("Cut.mp4")).unwrap();
        let partial = root.join(names::partial_name("Cut", 1));
        std::fs::write(&partial, b"new export").unwrap();
        assert_eq!(publish_within(root, &partial, "Cut").unwrap(), "Cut 2.mp4");
        assert_eq!(
            std::fs::read(root.join("Cut 2.mp4")).unwrap(),
            b"new export"
        );
        assert!(
            std::fs::symlink_metadata(root.join("Cut.mp4"))
                .unwrap()
                .is_symlink()
        );
    }

    #[test]
    fn without_hard_links_it_copies_and_still_never_replaces() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("Cut.mp4"), b"the operator's").unwrap();
        let partial = root.join(names::partial_name("Cut", 1));
        std::fs::write(&partial, b"new export").unwrap();
        // What link(2) says on vfat and exFAT.
        let no_links = |_: &Path, _: &Path| Err(std::io::Error::from_raw_os_error(libc::EPERM));
        let name = publish_with(root, &partial, "Cut", no_links).unwrap();
        assert_eq!(name, "Cut 2.mp4");
        assert_eq!(
            std::fs::read(root.join("Cut 2.mp4")).unwrap(),
            b"new export"
        );
        assert_eq!(
            std::fs::read(root.join("Cut.mp4")).unwrap(),
            b"the operator's"
        );
        assert!(!partial.exists(), "the partial file was left behind");
        // A denial is not "no hard links": it is reported, and the finished
        // render is not copied around it.
        std::fs::write(&partial, b"again").unwrap();
        let denied = |_: &Path, _: &Path| Err(std::io::Error::from_raw_os_error(libc::EACCES));
        assert!(publish_with(root, &partial, "Cut", denied).is_err());
        assert!(!root.join("Cut 3.mp4").exists());
    }

    #[test]
    fn an_export_takes_half_the_cores_and_at_least_one() {
        assert_eq!(export_threads(24), 12);
        assert_eq!(export_threads(4), 2);
        assert_eq!(export_threads(1), 1);
        let settings = export_settings(1280, 720, None, export_threads(24));
        assert_eq!(settings["maxThreads"], 12);
    }

    #[cfg(unix)]
    #[test]
    fn export_work_and_the_threads_it_starts_run_niced() {
        let caller = thread_nice();
        let (inside, child) = niced(|| {
            let child = std::thread::spawn(thread_nice).join().unwrap();
            (thread_nice(), child)
        });
        assert!(inside >= EXPORT_NICE, "nice {inside}");
        assert!(
            child >= EXPORT_NICE,
            "a thread started inside got nice {child}"
        );
        // The calling thread is left as it was.
        assert_eq!(thread_nice(), caller);
    }

    #[test]
    fn a_sweep_removes_only_partial_exports() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join(".Old.9.partial.mp4"), b"x").unwrap();
        std::fs::write(root.join("Keep.mp4"), b"x").unwrap();
        std::fs::write(root.join("my.partial.mp4"), b"x").unwrap();
        Exports::new(root.to_path_buf()).sweep_partials();
        assert!(!root.join(".Old.9.partial.mp4").exists());
        assert!(root.join("Keep.mp4").exists());
        assert!(root.join("my.partial.mp4").exists());
    }
}
