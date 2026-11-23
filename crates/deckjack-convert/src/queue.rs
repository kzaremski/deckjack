//! The job queue: app-owned state that outlives any window, socket or call.
//!
//! Three facts shape it (lane brief, "Backend commands"): a socket that
//! closes cancels its dispatches, so a job modeled as one long call would die
//! when a phone locks; a cancel the core sends is only a flag the handler
//! must look at; and progress reaches a caller only inside a call. So a job
//! is a record here, run by this process's own threads, cancelled by an
//! explicit `convert.job.cancel`, and watched by short calls a caller
//! re-issues (`convert.job.watch`) or by the window, which this module
//! re-renders through a notifier as jobs move.
//!
//! The queue is `queue.json` in the app's private home, written atomically on
//! every change of state (not on every progress tick). Losing it loses only
//! the list; outputs are files. A job found `running` when the app starts was
//! interrupted: it becomes `failed`, with Retry, and its partial file (known
//! by the exact name recorded in the job) is removed. Nothing else is.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use deckjack_app::Cancellation;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::encode::{self, Ended};
use crate::files::{self, Grants};
use crate::presets::{self, Options, PresetId, Range};
use crate::probe::{self, Probe};
use crate::progress;
use crate::tools::Tools;

/// The most paths one `convert.job.add` takes.
pub const MAX_BATCH: usize = 100;
/// The most jobs the queue holds, finished ones included.
pub const MAX_QUEUE: usize = 500;
/// Free space a job leaves on top of its estimate, as a fraction of it.
const SPACE_MARGIN: f64 = 0.10;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum JobState {
    Queued,
    Running,
    Done,
    Failed,
    Cancelled,
}

impl JobState {
    pub fn finished(self) -> bool {
        matches!(
            self,
            JobState::Done | JobState::Failed | JobState::Cancelled
        )
    }
}

/// One job, as `convert.job.list` reports it and `queue.json` stores it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Job {
    pub id: String,
    /// The source, relative to the working directory.
    pub path: String,
    pub preset: PresetId,
    #[serde(default)]
    pub options: Options,
    /// The folder the output goes to; absent is beside the source.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dest: Option<String>,
    pub state: JobState,
    /// From 0 to 1 while running, when the length is known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fraction: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub speed: Option<f64>,
    /// Seconds left, from the current speed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub eta_secs: Option<f64>,
    /// The finished file, relative to the working directory.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<String>,
    /// Why it failed, as a sentence.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
    /// What the output was expected to take on disk when it was queued.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub estimated_bytes: Option<u64>,
    /// The hidden file being written, relative to the working directory.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub partial: Option<String>,
    /// The exact ffmpeg line, once it ran.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    /// For a trim: where the copy really starts (the keyframe before).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub real_start: Option<f64>,
    /// Seconds since the epoch.
    pub added: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished: Option<u64>,
}

impl Job {
    /// The source's file name, for a row.
    pub fn name(&self) -> &str {
        self.path.rsplit('/').next().unwrap_or(&self.path)
    }
}

/// A seconds value as a caller may give it: a number, or text a person
/// typed (`75`, `1:15`, `0:01:15.5`).
#[derive(Debug, Clone, PartialEq, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum Time {
    Seconds(f64),
    Text(String),
}

impl Time {
    pub fn seconds(&self) -> Result<f64, String> {
        match self {
            Time::Seconds(s) => Ok(*s),
            Time::Text(t) => parse_clock(t).ok_or_else(|| {
                format!("{t:?} is not a time: write seconds (75) or minutes and seconds (1:15)")
            }),
        }
    }
}

/// `75`, `1:15`, `1:02:03.5`.
pub fn parse_clock(text: &str) -> Option<f64> {
    let parts: Vec<&str> = text.trim().split(':').collect();
    if parts.is_empty() || parts.len() > 3 {
        return None;
    }
    let mut total = 0.0;
    for (i, part) in parts.iter().enumerate() {
        let last = i == parts.len() - 1;
        let value: f64 = if last {
            part.trim().parse().ok()?
        } else {
            part.trim().parse::<u32>().ok()? as f64
        };
        if !value.is_finite() || value < 0.0 || (i > 0 && value >= 60.0) {
            return None;
        }
        total = total * 60.0 + value;
    }
    Some(total)
}

/// `convert.job.add`'s parameters. Flat, so a window's fields map onto them
/// one to one.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct AddRequest {
    /// Sources, relative to the working directory, inside Videos/ or Music/.
    pub paths: Vec<String>,
    pub preset: PresetId,
    /// Where to start, in seconds or `m:ss`. With `end`, a range.
    #[serde(default)]
    pub start: Option<Time>,
    #[serde(default)]
    pub end: Option<Time>,
    /// Remux only: `mp4` or `mkv`.
    #[serde(default)]
    pub container: Option<presets::Container>,
    /// Remux: every audio track, not only the first.
    #[serde(default)]
    pub keep_all_audio: Option<bool>,
    /// FLAC from a lossy source, otherwise refused.
    #[serde(default)]
    pub allow_lossy_to_lossless: Option<bool>,
    /// A granted folder for the outputs; absent or `beside` puts each beside
    /// its source.
    #[serde(default)]
    pub dest: Option<String>,
}

impl AddRequest {
    pub fn options(&self) -> Result<Options, String> {
        let range = match (&self.start, &self.end) {
            (None, None) => None,
            (Some(s), Some(e)) => Some(Range {
                start: s.seconds()?,
                end: e.seconds()?,
            }),
            (Some(_), None) => return Err("a start needs an end".into()),
            (None, Some(_)) => return Err("an end needs a start".into()),
        };
        Ok(Options {
            range,
            container: self.container,
            keep_all_audio: self.keep_all_audio.unwrap_or(false),
            allow_lossy_to_lossless: self.allow_lossy_to_lossless.unwrap_or(false),
        })
    }

    pub fn dest(&self) -> Option<&str> {
        self.dest
            .as_deref()
            .map(str::trim)
            .filter(|d| !d.is_empty() && *d != "beside")
    }
}

/// One path `add` would not queue, and why.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Refusal {
    pub path: String,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct Added {
    pub added: Vec<Job>,
    pub refused: Vec<Refusal>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Saved {
    #[serde(default)]
    next: u64,
    #[serde(default = "one")]
    parallel: u8,
    #[serde(default)]
    jobs: Vec<Job>,
}

fn one() -> u8 {
    1
}

struct Running {
    cancel: Cancellation,
    pid: Option<u32>,
}

struct State {
    saved: Saved,
    running: HashMap<String, Running>,
}

type Space = dyn Fn(&Path) -> Option<u64> + Send + Sync;

pub struct Queue {
    grants: Grants,
    tools: Result<Tools, String>,
    file: PathBuf,
    state: Mutex<State>,
    wake: Condvar,
    notify: Box<dyn Fn() + Send + Sync>,
    /// Predicted output sizes from `convert.estimate`, by source and preset,
    /// with the options they were measured with: a measurement of ten
    /// seconds says nothing about the whole file, nor the reverse.
    estimates: Mutex<HashMap<(String, PresetId), (Options, u64)>>,
    space: Box<Space>,
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

impl Queue {
    /// Load the queue from `home`, recover from an interrupted run, and
    /// return it. `notify` is called after every change a window shows.
    pub fn open(
        grants: Grants,
        tools: Result<Tools, String>,
        home: &Path,
        notify: impl Fn() + Send + Sync + 'static,
    ) -> Arc<Queue> {
        Queue::open_with(grants, tools, home, notify, files::free_bytes)
    }

    pub fn open_with(
        grants: Grants,
        tools: Result<Tools, String>,
        home: &Path,
        notify: impl Fn() + Send + Sync + 'static,
        space: impl Fn(&Path) -> Option<u64> + Send + Sync + 'static,
    ) -> Arc<Queue> {
        let file = home.join("queue.json");
        let mut saved: Saved = std::fs::read(&file)
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        saved.parallel = saved.parallel.clamp(1, 2);
        let queue = Queue {
            grants,
            tools,
            file,
            state: Mutex::new(State {
                saved: Saved::default(),
                running: HashMap::new(),
            }),
            wake: Condvar::new(),
            notify: Box::new(notify),
            estimates: Mutex::new(HashMap::new()),
            space: Box::new(space),
        };
        let mut changed = false;
        for job in &mut saved.jobs {
            if job.state == JobState::Running {
                queue.remove_partial(job);
                job.state = JobState::Failed;
                job.error = Some(
                    "interrupted: Convert stopped while this was running; Retry starts it again"
                        .into(),
                );
                job.fraction = None;
                job.speed = None;
                job.eta_secs = None;
                job.finished = Some(now());
                changed = true;
            }
        }
        {
            let mut state = queue.lock();
            state.saved = saved;
            if changed {
                queue.save(&state);
            }
        }
        Arc::new(queue)
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub fn grants(&self) -> &Grants {
        &self.grants
    }

    pub fn tools(&self) -> Result<&Tools, String> {
        self.tools.as_ref().map_err(Clone::clone)
    }

    /// Write `queue.json` atomically: a reader (or a crash) sees the old
    /// file or the new one, never half of one.
    fn save(&self, state: &State) {
        let Ok(bytes) = serde_json::to_vec_pretty(&state.saved) else {
            return;
        };
        let tmp = self.file.with_extension("json.tmp");
        if let Some(dir) = self.file.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if std::fs::write(&tmp, bytes).is_ok() {
            let _ = std::fs::rename(&tmp, &self.file);
        }
    }

    /// Remove a job's partial file: the exact name recorded for it, and only
    /// if that name is this app's partial name for that job.
    fn remove_partial(&self, job: &Job) {
        let Some(rel) = &job.partial else { return };
        let path = self.grants.root.join(rel);
        let expected = files::partial_name(&job.id);
        if path
            .file_name()
            .is_some_and(|n| n.to_string_lossy() == expected)
        {
            let _ = std::fs::remove_file(path);
        }
    }

    pub fn list(&self) -> Vec<Job> {
        self.lock().saved.jobs.clone()
    }

    pub fn get(&self, id: &str) -> Option<Job> {
        self.lock().saved.jobs.iter().find(|j| j.id == id).cloned()
    }

    pub fn parallel(&self) -> u8 {
        self.lock().saved.parallel
    }

    pub fn set_parallel(&self, n: u8) -> Result<u8, String> {
        if !(1..=2).contains(&n) {
            return Err(format!("{n} at once is not offered: choose 1 or 2"));
        }
        {
            let mut state = self.lock();
            state.saved.parallel = n;
            self.save(&state);
        }
        self.wake.notify_all();
        (self.notify)();
        Ok(n)
    }

    /// Remember an estimate, so a job queued after it with the same options
    /// checks free space against a measurement rather than a guess.
    pub fn remember_estimate(&self, path: &str, preset: PresetId, options: &Options, bytes: u64) {
        self.estimates
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert((path.to_string(), preset), (options.clone(), bytes));
    }

    /// What a job's output is expected to take, before it runs: the
    /// estimate if one was measured with these same options, otherwise a
    /// deliberately generous guess (the source's size for anything that
    /// re-encodes or copies; a CD-quality rate for FLAC).
    fn expected_bytes(
        &self,
        rel: &str,
        preset: PresetId,
        options: &Options,
        probe: &Probe,
        built: &presets::Built,
    ) -> u64 {
        if let Some((measured, b)) = self
            .estimates
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&(rel.to_string(), preset))
            && measured == options
        {
            return *b;
        }
        let share = match (built.seconds, probe.duration) {
            (Some(s), Some(d)) if d > 0.0 => (s / d).min(1.0),
            _ => 1.0,
        };
        match preset {
            PresetId::AudioFlac => (built.seconds.unwrap_or(0.0) * 1_100_000.0 / 8.0) as u64,
            PresetId::AudioOpus => (built.seconds.unwrap_or(0.0) * 256_000.0 / 8.0) as u64,
            _ => (probe.size as f64 * share) as u64,
        }
    }

    /// Queue `req.paths` with one preset. Each path is checked, sniffed,
    /// probed and planned now, so a refusal (a playlist, a remux the
    /// container cannot hold, FLAC from a lossy source) is said before
    /// anything runs. Paths that pass are queued; the rest are listed with
    /// their reasons. Refused as a whole when nothing passes, or when the
    /// outputs would not fit the free space.
    pub fn add(&self, req: &AddRequest) -> Result<Added, String> {
        if req.paths.is_empty() {
            return Err("no files given".into());
        }
        if req.paths.len() > MAX_BATCH {
            return Err(format!(
                "{} files at once is more than {MAX_BATCH}; queue them in batches",
                req.paths.len()
            ));
        }
        let tools = self.tools()?;
        let options = req.options()?;
        let dest = match req.dest() {
            Some(d) => Some((
                d.to_string(),
                self.grants.writable_dir(d).map_err(|e| e.to_string())?,
            )),
            None => None,
        };
        let mut planned: Vec<(String, PathBuf, presets::Built, u64)> = Vec::new();
        let mut refused = Vec::new();
        for rel in &req.paths {
            let checked = self
                .grants
                .media(rel)
                .map_err(|e| e.to_string())
                .and_then(|path| {
                    let probe = probe::probe(tools, &path).map_err(|e| e.to_string())?;
                    let built = presets::build(req.preset, &probe, &options, tools.threads)?;
                    let bytes = self.expected_bytes(rel, req.preset, &options, &probe, &built);
                    Ok((path, built, bytes))
                });
            match checked {
                Ok((path, built, bytes)) => planned.push((rel.clone(), path, built, bytes)),
                Err(reason) => refused.push(Refusal {
                    path: rel.clone(),
                    reason,
                }),
            }
        }
        if planned.is_empty() {
            return Err(refused
                .iter()
                .map(|r| r.reason.clone())
                .collect::<Vec<_>>()
                .join("; "));
        }
        // Free space, per destination folder, for the whole batch.
        let mut need: HashMap<PathBuf, u64> = HashMap::new();
        for (_, path, _, bytes) in &planned {
            let dir = match &dest {
                Some((_, d)) => d.clone(),
                None => path.parent().map(Path::to_path_buf).unwrap_or_default(),
            };
            *need.entry(dir).or_default() += (*bytes as f64 * (1.0 + SPACE_MARGIN)) as u64;
        }
        for (dir, bytes) in &need {
            if let Some(free) = (self.space)(dir)
                && free < *bytes
            {
                return Err(format!(
                    "these outputs need about {} in {}, and only {} is free; nothing was queued",
                    human_bytes(*bytes),
                    self.grants.relative(dir),
                    human_bytes(free)
                ));
            }
        }
        let mut state = self.lock();
        if state.saved.jobs.len() + planned.len() > MAX_QUEUE {
            return Err(format!(
                "the queue holds at most {MAX_QUEUE} jobs; clear finished ones first"
            ));
        }
        let mut added = Vec::new();
        for (rel, _, built, bytes) in planned {
            state.saved.next += 1;
            let job = Job {
                id: format!("j{}", state.saved.next),
                path: rel,
                preset: req.preset,
                options: options.clone(),
                dest: dest.as_ref().map(|(d, _)| d.clone()),
                state: JobState::Queued,
                fraction: None,
                speed: None,
                eta_secs: None,
                output: None,
                error: None,
                warnings: built.warnings,
                estimated_bytes: Some(bytes),
                partial: None,
                command: None,
                real_start: None,
                added: now(),
                started: None,
                finished: None,
            };
            state.saved.jobs.push(job.clone());
            added.push(job);
        }
        self.save(&state);
        drop(state);
        self.wake.notify_all();
        (self.notify)();
        Ok(Added { added, refused })
    }

    /// Cancel a queued job at once, or ask a running one's ffmpeg to stop.
    pub fn cancel(&self, id: &str) -> Result<Job, String> {
        let mut state = self.lock();
        let running = state.running.get(id).map(|r| r.cancel.clone());
        let job = state
            .saved
            .jobs
            .iter_mut()
            .find(|j| j.id == id)
            .ok_or_else(|| format!("there is no job {id}"))?;
        match job.state {
            JobState::Queued => {
                job.state = JobState::Cancelled;
                job.finished = Some(now());
                let out = job.clone();
                self.save(&state);
                drop(state);
                (self.notify)();
                Ok(out)
            }
            JobState::Running => {
                // The worker sees the flag within a tenth of a second, stops
                // ffmpeg (SIGTERM, then SIGKILL after 5 s), removes the
                // partial and records the cancel.
                if let Some(cancel) = running {
                    cancel.cancel();
                }
                Ok(job.clone())
            }
            done => Err(format!("job {id} has already {}", describe(done))),
        }
    }

    /// Queue a failed or cancelled job again, as it was first asked for.
    pub fn retry(&self, id: &str) -> Result<Job, String> {
        let mut state = self.lock();
        let job = state
            .saved
            .jobs
            .iter_mut()
            .find(|j| j.id == id)
            .ok_or_else(|| format!("there is no job {id}"))?;
        if !matches!(job.state, JobState::Failed | JobState::Cancelled) {
            return Err(format!(
                "job {id} is {}; only a failed or cancelled job can be retried",
                describe(job.state)
            ));
        }
        job.state = JobState::Queued;
        job.error = None;
        job.fraction = None;
        job.speed = None;
        job.eta_secs = None;
        job.partial = None;
        job.started = None;
        job.finished = None;
        let out = job.clone();
        self.save(&state);
        drop(state);
        self.wake.notify_all();
        (self.notify)();
        Ok(out)
    }

    /// Forget finished jobs: these, or every finished one. Outputs stay.
    pub fn clear(&self, ids: Option<&[String]>) -> usize {
        let mut state = self.lock();
        let before = state.saved.jobs.len();
        state.saved.jobs.retain(|j| {
            let named = ids.is_none_or(|ids| ids.contains(&j.id));
            !(named && j.state.finished())
        });
        let cleared = before - state.saved.jobs.len();
        if cleared > 0 {
            self.save(&state);
            drop(state);
            (self.notify)();
        }
        cleared
    }

    /// Start the thread that runs queued jobs, at most `parallel` at once.
    pub fn start(self: &Arc<Self>) {
        let queue = Arc::clone(self);
        std::thread::Builder::new()
            .name("convert-queue".into())
            .spawn(move || queue.dispatch())
            .expect("the queue thread starts");
    }

    fn dispatch(self: Arc<Self>) {
        let mut state = self.lock();
        loop {
            while state.running.len() < state.saved.parallel as usize {
                let Some(job) = state
                    .saved
                    .jobs
                    .iter_mut()
                    .find(|j| j.state == JobState::Queued)
                else {
                    break;
                };
                job.state = JobState::Running;
                job.started = Some(now());
                let id = job.id.clone();
                let cancel = Cancellation::new();
                state.running.insert(
                    id.clone(),
                    Running {
                        cancel: cancel.clone(),
                        pid: None,
                    },
                );
                self.save(&state);
                let queue = Arc::clone(&self);
                std::thread::Builder::new()
                    .name(format!("convert-{id}"))
                    .spawn(move || queue.work(&id, &cancel))
                    .expect("a job thread starts");
                (self.notify)();
            }
            state = self
                .wake
                .wait_timeout(state, Duration::from_secs(1))
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
    }

    /// Run one job to its end and record how it ended.
    fn work(&self, id: &str, cancel: &Cancellation) {
        let outcome = self.run(id, cancel);
        let mut state = self.lock();
        state.running.remove(id);
        if let Some(job) = state.saved.jobs.iter_mut().find(|j| j.id == id) {
            job.finished = Some(now());
            job.speed = None;
            job.eta_secs = None;
            match outcome {
                Ok(output) => {
                    job.state = JobState::Done;
                    job.fraction = Some(1.0);
                    job.output = Some(output);
                    job.partial = None;
                }
                Err(Ended::Cancelled) => {
                    job.state = JobState::Cancelled;
                    self.remove_partial(job);
                    job.partial = None;
                }
                Err(Ended::Failed(error)) => {
                    job.state = JobState::Failed;
                    job.error = Some(error);
                    self.remove_partial(job);
                    job.partial = None;
                }
                Err(Ended::Done) => unreachable!("a finished run is Ok"),
            }
        }
        self.save(&state);
        drop(state);
        self.wake.notify_all();
        (self.notify)();
    }

    /// The job's steps: check, probe, plan, check space, encode, publish.
    fn run(&self, id: &str, cancel: &Cancellation) -> Result<String, Ended> {
        let job = self.get(id).ok_or(Ended::Cancelled)?;
        let failed = |e: String| Ended::Failed(e);
        let tools = self.tools().map_err(failed)?;
        let input = self
            .grants
            .media(&job.path)
            .map_err(|e| failed(e.to_string()))?;
        let probe = probe::probe(tools, &input).map_err(|e| failed(e.to_string()))?;
        let built =
            presets::build(job.preset, &probe, &job.options, tools.threads).map_err(failed)?;
        let real_start = match (job.preset, job.options.range) {
            (PresetId::Trim, Some(r)) => Some(
                probe::keyframe_before(tools, &input, r.start)
                    .map_err(|e| failed(e.to_string()))?,
            ),
            _ => None,
        };
        let dir = match &job.dest {
            Some(d) => self
                .grants
                .writable_dir(d)
                .map_err(|e| failed(e.to_string()))?,
            None => input
                .parent()
                .map(Path::to_path_buf)
                .ok_or_else(|| failed("the source has no folder".into()))?,
        };
        let need = (job.estimated_bytes.unwrap_or(0) as f64 * (1.0 + SPACE_MARGIN)) as u64;
        if let Some(free) = (self.space)(&dir)
            && free < need
        {
            return Err(failed(format!(
                "not enough space in {}: about {} needed, {} free",
                self.grants.relative(&dir),
                human_bytes(need),
                human_bytes(free)
            )));
        }
        let partial = dir.join(files::partial_name(&job.id));
        // A stale partial of this same job (a retry after a crash that
        // could not clean up): ours by name, and `-n` would refuse it.
        let _ = std::fs::remove_file(&partial);
        let args = presets::argv(&input, &built, &partial, true);
        let line = presets::command_line(&tools.ffmpeg, &args);
        {
            let mut state = self.lock();
            if let Some(j) = state.saved.jobs.iter_mut().find(|j| j.id == id) {
                j.partial = Some(self.grants.relative(&partial));
                j.command = Some(line);
                j.real_start = real_start;
                if let Some(rs) = real_start
                    && let Some(r) = j.options.range
                    && (r.start - rs).abs() > 0.05
                {
                    // Once: a retried job runs this again.
                    let warning = format!(
                        "The cut starts at {}, the keyframe before {}.",
                        presets::clock(rs),
                        presets::clock(r.start)
                    );
                    if !j.warnings.contains(&warning) {
                        j.warnings.push(warning);
                    }
                }
                j.fraction = Some(0.0);
            }
            self.save(&state);
        }
        (self.notify)();
        let total = built.seconds;
        let mut last_note = Instant::now() - Duration::from_secs(10);
        let ended = encode::run_with_pid(
            tools,
            &args,
            cancel,
            None,
            |pid| {
                if let Some(r) = self.lock().running.get_mut(id) {
                    r.pid = Some(pid);
                }
            },
            |tick| {
                let (fraction, left) = progress::estimate(tick, total);
                {
                    let mut state = self.lock();
                    if let Some(j) = state.saved.jobs.iter_mut().find(|j| j.id == id) {
                        j.fraction = fraction.or(j.fraction);
                        j.speed = tick.speed;
                        j.eta_secs = left;
                    }
                }
                // A window re-renders at most twice a second for progress.
                if last_note.elapsed() >= Duration::from_millis(500) {
                    last_note = Instant::now();
                    (self.notify)();
                }
            },
        );
        match ended {
            Ended::Done => {}
            other => return Err(other),
        }
        let stem = input
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "output".into());
        let out = files::publish(
            &dir,
            &partial,
            &stem,
            job.preset.preset().suffix,
            built.extension,
        )
        .map_err(|e| failed(format!("the output could not be put in place: {e}")))?;
        Ok(self.grants.relative(&out))
    }

    /// The pid of a running job's ffmpeg, for tests that kill it.
    #[cfg(test)]
    pub fn pid_of(&self, id: &str) -> Option<u32> {
        self.lock().running.get(id).and_then(|r| r.pid)
    }
}

fn describe(state: JobState) -> &'static str {
    match state {
        JobState::Queued => "queued",
        JobState::Running => "running",
        JobState::Done => "finished",
        JobState::Failed => "failed",
        JobState::Cancelled => "been cancelled",
    }
}

/// `1.4 GB`, for sentences.
pub fn human_bytes(n: u64) -> String {
    const UNITS: &[&str] = &["B", "KB", "MB", "GB", "TB"];
    let mut v = n as f64;
    let mut unit = 0;
    while v >= 1000.0 && unit < UNITS.len() - 1 {
        v /= 1000.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{n} B")
    } else if v < 10.0 {
        format!("{v:.1} {}", UNITS[unit])
    } else {
        format!("{v:.0} {}", UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn times_are_read_as_people_type_them() {
        assert_eq!(parse_clock("75"), Some(75.0));
        assert_eq!(parse_clock("1:15"), Some(75.0));
        assert_eq!(parse_clock(" 0:01:15.5 "), Some(75.5));
        assert_eq!(parse_clock("1:75"), None, "seconds past 59 are a typo");
        assert_eq!(parse_clock("a"), None);
        assert_eq!(parse_clock("1:2:3:4"), None);
        assert_eq!(parse_clock("-3"), None);
        assert_eq!(Time::Seconds(4.5).seconds(), Ok(4.5));
        assert!(Time::Text("soon".into()).seconds().is_err());
    }

    #[test]
    fn a_request_reads_its_range_and_its_destination() {
        let req: AddRequest = serde_json::from_value(serde_json::json!({
            "paths": ["Videos/a.mp4"], "preset": "trim",
            "start": "0:10", "end": 25, "dest": "beside",
            "keep_all_audio": null, "container": null
        }))
        .unwrap();
        let opts = req.options().unwrap();
        assert_eq!(
            opts.range,
            Some(Range {
                start: 10.0,
                end: 25.0
            })
        );
        assert!(!opts.keep_all_audio);
        assert_eq!(req.dest(), None, "beside is no destination folder");
        let half: AddRequest = serde_json::from_value(serde_json::json!({
            "paths": ["Videos/a.mp4"], "preset": "trim", "start": 3
        }))
        .unwrap();
        assert!(half.options().unwrap_err().contains("needs an end"));
    }

    #[test]
    fn sizes_read_like_files_shows_them() {
        assert_eq!(human_bytes(999), "999 B");
        assert_eq!(human_bytes(1_400_000), "1.4 MB");
        assert_eq!(human_bytes(250_000_000), "250 MB");
    }
}
