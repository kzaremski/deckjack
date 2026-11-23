//! `deckjack-convert`: convert and compress video and audio.
//!
//! The confined ffmpeg helper the app suite was missing: every encode is a
//! child of this process, under its Landlock and seccomp policy, with an
//! argument list built in [`presets`] from typed values and never from a
//! caller's text. Jobs are app-owned ([`queue`]); the window is described
//! with the UI protocol ([`window`]); the app ships no web page.

mod encode;
mod estimate;
mod files;
mod presets;
mod probe;
mod progress;
mod queue;
mod tools;
mod window;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use deckjack_app::{App, Ctx};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::presets::{Container, PresetId};
use crate::queue::{AddRequest, JobState, Time};
use crate::window::{Measuring, Shared};

const MANIFEST: &str = include_str!("../app.toml");

/// The longest a `convert.job.watch` holds.
const MAX_WATCH_SECS: u64 = 50;

#[derive(Deserialize, JsonSchema)]
struct NoParams {}

#[derive(Deserialize, JsonSchema)]
struct Browse {
    /// A folder inside Videos/ or Music/, relative to the working directory.
    /// Absent: every granted folder.
    #[serde(default)]
    dir: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
struct PathParam {
    /// Relative to the working directory, inside Videos/ or Music/.
    path: String,
}

#[derive(Deserialize, JsonSchema)]
struct EstimateParams {
    /// The file to measure, relative to the working directory.
    path: String,
    preset: PresetId,
    #[serde(default)]
    start: Option<Time>,
    #[serde(default)]
    end: Option<Time>,
    #[serde(default)]
    container: Option<Container>,
    #[serde(default)]
    keep_all_audio: Option<bool>,
    #[serde(default)]
    allow_lossy_to_lossless: Option<bool>,
}

#[derive(Deserialize, JsonSchema)]
struct FrameParams {
    path: String,
    /// Seconds into the file.
    at: f64,
    /// With a preset that re-encodes the picture, the frame comes from a
    /// short sample encode there: what the output will look like.
    #[serde(default)]
    preset: Option<PresetId>,
    /// The long side, at most 1280 pixels.
    #[serde(default)]
    max_edge: Option<u32>,
}

#[derive(Deserialize, JsonSchema)]
struct JobId {
    /// As `convert.job.list` reports it (`j12`).
    id: String,
}

#[derive(Deserialize, JsonSchema)]
struct Watch {
    id: String,
    /// How long to wait for the job to end before answering
    /// `still_running`, at most 50.
    #[serde(default)]
    max_secs: Option<u64>,
}

#[derive(Deserialize, JsonSchema)]
struct Clear {
    /// These jobs, if finished. Absent: every finished job.
    #[serde(default)]
    ids: Option<Vec<String>>,
    /// Ignored; `{"finished": true}` reads as what it does.
    #[serde(default)]
    #[allow(dead_code)]
    finished: Option<bool>,
}

#[derive(Deserialize, JsonSchema)]
struct Settings {
    /// Jobs at once: 1 or 2.
    #[serde(default)]
    parallel: Option<u8>,
    /// The same as `parallel: 2` (true) or `1` (false).
    #[serde(default)]
    two_at_once: Option<bool>,
}

fn env_dir(name: &str, fallback: &str) -> PathBuf {
    std::env::var_os(name)
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::temp_dir().join(fallback))
}

fn main() -> anyhow::Result<()> {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let grants = files::Grants::from_manifest(MANIFEST, cwd);
    // The launcher's private HOME and TMPDIR for this app
    // (`cache/apps/convert/{home,tmp}`).
    let home = env_dir("HOME", "deckjack-convert-home");
    let scratch = env_dir("TMPDIR", "deckjack-convert-tmp").join("convert");

    let app = App::new(MANIFEST)?;
    let notifier = app.notifier();
    let queue = {
        let notifier = notifier.clone();
        queue::Queue::open(grants, tools::Tools::locate(), &home, move || {
            notifier.notify()
        })
    };
    queue.start();
    let shared = Arc::new(Shared {
        queue: Arc::clone(&queue),
        notifier,
        scratch,
        estimates: Default::default(),
        gate: Default::default(),
    });

    let s = Arc::clone(&shared);
    let app = app.view(move || window::Convert::open(Arc::clone(&s)));
    let app = with_commands(app, shared);
    app.run()
}

fn with_commands(app: App, shared: Arc<Shared>) -> App {
    let s = Arc::clone(&shared);
    let app = app.command("convert.status", move |_p: NoParams, _cx: Ctx| {
        let s = Arc::clone(&s);
        async move {
            let grants: Vec<Value> = s
                .queue
                .grants()
                .dirs
                .iter()
                .map(|(d, w)| json!({ "path": d, "write": w }))
                .collect();
            let mut status = match s.queue.tools() {
                Ok(t) => {
                    let t = t.clone();
                    tokio::task::spawn_blocking(move || t.status())
                        .await
                        .map_err(|e| e.to_string())?
                }
                Err(e) => json!({ "found": false, "problem": e }),
            };
            status["granted"] = Value::Array(grants);
            Ok(status)
        }
    });
    let s = Arc::clone(&shared);
    let app = app.command("convert.presets", move |_p: NoParams, _cx: Ctx| {
        let s = Arc::clone(&s);
        async move {
            let threads = s.queue.tools().map_or(1, |t| t.threads);
            Ok(json!({ "presets": presets::table(threads) }))
        }
    });
    let s = Arc::clone(&shared);
    let app = app.command("convert.browse", move |p: Browse, _cx: Ctx| {
        let s = Arc::clone(&s);
        async move {
            let (files, truncated) =
                files::browse(s.queue.grants(), p.dir.as_deref()).map_err(|e| e.to_string())?;
            Ok(json!({ "files": files, "truncated": truncated }))
        }
    });
    let s = Arc::clone(&shared);
    let app = app.command("convert.probe", move |p: PathParam, _cx: Ctx| {
        let s = Arc::clone(&s);
        async move {
            let tools = s.queue.tools()?.clone();
            let path = s.queue.grants().media(&p.path).map_err(|e| e.to_string())?;
            let probe = tokio::task::spawn_blocking(move || probe::probe(&tools, &path))
                .await
                .map_err(|e| e.to_string())?
                .map_err(|e| e.to_string())?;
            Ok(serde_json::to_value(probe).unwrap_or(Value::Null))
        }
    });
    let s = Arc::clone(&shared);
    let app = app.command("convert.estimate", move |p: EstimateParams, cx: Ctx| {
        let s = Arc::clone(&s);
        async move {
            let request = AddRequest {
                paths: vec![p.path.clone()],
                preset: p.preset,
                start: p.start,
                end: p.end,
                container: p.container,
                keep_all_audio: p.keep_all_audio,
                allow_lossy_to_lossless: p.allow_lossy_to_lossless,
                dest: None,
            };
            let options = request.options()?;
            let measured_with = options.clone();
            let tools = s.queue.tools()?.clone();
            let (path, preset) = (p.path.clone(), p.preset);
            s.set_estimate(
                &path,
                preset,
                Measuring::Running {
                    fraction: 0.0,
                    step: "Starting".into(),
                },
            );
            let worker = Arc::clone(&s);
            let cancel = cx.cancellation().clone();
            let report = cx.clone();
            let result = tokio::task::spawn_blocking(move || {
                let _turn = worker.gate.enter(&cancel, || {
                    let step = "Waiting for another measurement to finish".to_string();
                    report.progress(None, step.clone());
                    worker.set_estimate(
                        &path,
                        preset,
                        Measuring::Running {
                            fraction: 0.0,
                            step,
                        },
                    );
                })?;
                estimate::estimate(
                    &tools,
                    worker.queue.grants(),
                    &path,
                    preset,
                    &options,
                    &worker.scratch,
                    &cancel,
                    |fraction, step| {
                        report.progress(Some(fraction), step.clone());
                        worker.set_estimate(&path, preset, Measuring::Running { fraction, step });
                    },
                )
            })
            .await
            .map_err(|e| e.to_string())?;
            match result {
                Ok(e) => {
                    s.queue
                        .remember_estimate(&p.path, preset, &measured_with, e.predicted_bytes);
                    let value = serde_json::to_value(&e).unwrap_or(Value::Null);
                    s.set_estimate(&p.path, preset, Measuring::Done(Box::new(e), measured_with));
                    Ok(value)
                }
                Err(e) => {
                    s.set_estimate(&p.path, preset, Measuring::Failed(e.clone()));
                    Err(e)
                }
            }
        }
    });
    let s = Arc::clone(&shared);
    let app = app.command("convert.frame", move |p: FrameParams, cx: Ctx| {
        let s = Arc::clone(&s);
        async move {
            let tools = s.queue.tools()?.clone();
            let worker = Arc::clone(&s);
            let cancel = cx.cancellation().clone();
            let report = cx.clone();
            tokio::task::spawn_blocking(move || {
                let _turn = worker.gate.enter(&cancel, || {
                    report.progress(None, "Waiting for another measurement to finish");
                })?;
                estimate::frame(
                    &tools,
                    worker.queue.grants(),
                    &p.path,
                    p.at,
                    p.preset,
                    p.max_edge.unwrap_or(estimate::MAX_EDGE),
                    &worker.scratch.join("frame"),
                    &cancel,
                )
            })
            .await
            .map_err(|e| e.to_string())?
        }
    });
    let s = Arc::clone(&shared);
    let app = app.command("convert.job.add", move |p: AddRequest, _cx: Ctx| {
        let s = Arc::clone(&s);
        async move {
            let queue = Arc::clone(&s.queue);
            let added = tokio::task::spawn_blocking(move || queue.add(&p))
                .await
                .map_err(|e| e.to_string())??;
            Ok(serde_json::to_value(added).unwrap_or(Value::Null))
        }
    });
    let s = Arc::clone(&shared);
    let app = app.command("convert.job.list", move |_p: NoParams, _cx: Ctx| {
        let s = Arc::clone(&s);
        async move {
            Ok(json!({
                "jobs": s.queue.list(),
                "parallel": s.queue.parallel(),
            }))
        }
    });
    let s = Arc::clone(&shared);
    let app = app.command("convert.job.watch", move |p: Watch, cx: Ctx| {
        let s = Arc::clone(&s);
        async move {
            let limit =
                Duration::from_secs(p.max_secs.unwrap_or(MAX_WATCH_SECS).min(MAX_WATCH_SECS));
            let deadline = Instant::now() + limit;
            loop {
                let job = s
                    .queue
                    .get(&p.id)
                    .ok_or_else(|| format!("there is no job {}", p.id))?;
                if job.state.finished() {
                    return Ok(json!({ "still_running": false, "job": job }));
                }
                let message = match (job.state, job.eta_secs) {
                    (JobState::Queued, _) => "Queued".to_string(),
                    (_, Some(eta)) => format!("{} left", window::duration_words(eta)),
                    _ => "Converting".to_string(),
                };
                cx.progress(job.fraction.map(|f| f as f32), message);
                if Instant::now() >= deadline || cx.is_cancelled() {
                    return Ok(json!({ "still_running": true, "job": job }));
                }
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
        }
    });
    let s = Arc::clone(&shared);
    let app = app.command("convert.job.cancel", move |p: JobId, _cx: Ctx| {
        let s = Arc::clone(&s);
        async move { Ok(serde_json::to_value(s.queue.cancel(&p.id)?).unwrap_or(Value::Null)) }
    });
    let s = Arc::clone(&shared);
    let app = app.command("convert.job.retry", move |p: JobId, _cx: Ctx| {
        let s = Arc::clone(&s);
        async move { Ok(serde_json::to_value(s.queue.retry(&p.id)?).unwrap_or(Value::Null)) }
    });
    let s = Arc::clone(&shared);
    let app = app.command("convert.job.clear", move |p: Clear, _cx: Ctx| {
        let s = Arc::clone(&s);
        async move {
            let cleared = s.queue.clear(p.ids.as_deref());
            Ok(json!({ "cleared": cleared }))
        }
    });
    let s = shared;
    app.command("convert.settings", move |p: Settings, _cx: Ctx| {
        let s = Arc::clone(&s);
        async move {
            let wanted = p
                .parallel
                .or(p.two_at_once.map(|two| if two { 2 } else { 1 }));
            if let Some(n) = wanted {
                s.queue.set_parallel(n)?;
            }
            Ok(json!({ "parallel": s.queue.parallel() }))
        }
    })
}

#[cfg(test)]
mod tests;
