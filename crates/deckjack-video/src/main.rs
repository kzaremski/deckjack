//! `deckjack-video`: the Video app.
//!
//! A cuts-and-audio editor whose engine is FilmCraft's, linked into this
//! process. This file is only the seam between the bus and the modules:
//! [`engine`] holds the sessions, [`edits`] the edits, [`export`] the export
//! jobs, [`timeline`] the frame-based description, [`names`] the rules for
//! names on disk. See `app.toml` for what the process may touch, and
//! `docs/lanes/5d-video.md` for why it is built this way.

mod edits;
mod engine;
mod export;
mod names;
mod still;
#[cfg(test)]
mod tests;
mod timeline;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use deckjack_app::{App, Ctx};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

use engine::{Result, Studio};

#[derive(rust_embed::RustEmbed)]
#[folder = "ui"]
struct Ui;

#[derive(Deserialize, schemars::JsonSchema)]
struct NoParams {}

#[derive(Deserialize, schemars::JsonSchema)]
struct Named {
    /// The project's name: its file in `Videos/` without `.fcproj`.
    project: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct NewProject {
    /// What to call it. Refused when `<name>.fcproj` or `<name>.otio` exists.
    name: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct Wait {
    project: String,
    /// The revision the caller has; answers as soon as it differs.
    since: u64,
    /// Longest to wait, in milliseconds. At most 55000; 25000 by default.
    #[serde(default)]
    timeout_ms: Option<u64>,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct Import {
    project: String,
    /// Files relative to `Videos/`, as `video.media.list` reports them.
    paths: Vec<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct Render {
    project: String,
    /// The program frame to show.
    frame: i64,
    /// About how wide, in pixels. 640 by default; 64 to 1920.
    #[serde(default)]
    width: Option<u32>,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct ExportStart {
    project: String,
    size: export::Size,
    /// The file's name without `.mp4`; the project's name by default. A name
    /// in use gets ` 2`, ` 3`… and never replaces the file.
    #[serde(default)]
    name: Option<String>,
    /// Normalize the program's integrated loudness to this, in LUFS (for
    /// example -14 for the web, -23 for broadcast). Off when absent.
    #[serde(default)]
    loudness_lufs: Option<f64>,
    /// Answer only when the export has ended, reporting progress meanwhile.
    /// Use it for anything long: the progress is what keeps the app running.
    /// Without it the export runs in the background only while the app is in
    /// use (a window open, or a call at least every five minutes, such as
    /// `video.export.list`); after that the core stops an idle app and the
    /// export with it.
    #[serde(default)]
    wait: bool,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct ExportId {
    /// The export's id, from `video.export.start` or `video.export.list`.
    id: u64,
}

/// Run blocking work off the async threads and turn its error into the
/// sentence the caller sees.
async fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> Result<T> + Send + 'static,
) -> std::result::Result<T, String> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| format!("the command stopped unexpectedly: {e}"))?
        .map_err(|e| e.to_string())
}

/// Register a command whose whole body is blocking work on the studio.
fn on<P>(app: App, id: &str, studio: &Arc<Studio>, f: fn(&Studio, P) -> Result<Value>) -> App
where
    P: DeserializeOwned + schemars::JsonSchema + Send + 'static,
{
    let studio = Arc::clone(studio);
    app.command(id, move |p: P, _cx: Ctx| {
        let studio = Arc::clone(&studio);
        async move { blocking(move || f(&studio, p)).await }
    })
}

fn describe(studio: &Studio, project: &engine::Project) -> Result<Value> {
    Ok(serde_json::to_value(project.describe(studio.root())?).unwrap_or(Value::Null))
}

/// An edit's reply: what FilmCraft said, and the state after it, so a window
/// redraws from one round trip.
fn edited(studio: &Studio, project: &engine::Project, result: Value) -> Result<Value> {
    Ok(json!({ "result": result, "state": describe(studio, project)? }))
}

/// The private directory FilmCraft keeps its preferences and recovery
/// journal in. `HOME` is the app's own, inside the install's cache.
fn data_dir() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("filmcraft")
}

fn main() -> anyhow::Result<()> {
    // The working directory is the process's cwd: the core launches an app
    // there.
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let studio = Arc::new(Studio::new(cwd.join(names::VIDEOS_DIR), data_dir())?);
    let exports = Arc::new(export::Exports::new(studio.root().to_path_buf()));
    exports.sweep_partials();
    let result = build(&studio, &exports)?.run();
    exports.cancel_all();
    studio.shutdown();
    result
}

/// Every command, wired to its handler. `App::run` refuses to start when this
/// and `app.toml` disagree in either direction.
fn build(studio: &Arc<Studio>, exports: &Arc<export::Exports>) -> anyhow::Result<App> {
    let studio = Arc::clone(studio);
    let exports = Arc::clone(exports);
    let mut app = App::new(include_str!("../app.toml"))?.ui::<Ui>();
    app = on(app, "video.project.list", &studio, |s, _p: NoParams| {
        Ok(json!({ "projects": s.list()? }))
    });
    app = on(app, "video.project.new", &studio, |s, p: NewProject| {
        let project = s.create(&p.name)?;
        describe(s, &project)
    });
    app = on(app, "video.project.open", &studio, |s, p: Named| {
        let (project, opened) = s.open(&p.project)?;
        Ok(json!({ "opened": opened, "state": describe(s, &project)? }))
    });
    app = on(app, "video.project.save", &studio, |s, p: Named| {
        s.get(&p.project)?.save()
    });
    app = on(app, "video.project.recover", &studio, |s, p: Named| {
        let project = s.get(&p.project)?;
        let result = project.recover()?;
        edited(s, &project, result)
    });
    app = on(app, "video.project.describe", &studio, |s, p: Named| {
        describe(s, &*s.get(&p.project)?)
    });
    app = {
        let studio = Arc::clone(&studio);
        app.command("video.project.wait", move |p: Wait, _cx: Ctx| {
            let studio = Arc::clone(&studio);
            async move {
                let name = p.project.clone();
                let project = blocking(move || studio.get(&name)).await?;
                let timeout = Duration::from_millis(p.timeout_ms.unwrap_or(25_000));
                let revision = project.wait(p.since, timeout).await;
                Ok(json!({ "revision": revision, "changed": revision != p.since }))
            }
        })
    };
    app = on(app, "video.media.list", &studio, |s, _p: NoParams| {
        Ok(json!({ "media": s.media_files() }))
    });
    app = on(app, "video.media.import", &studio, |s, p: Import| {
        let project = s.get(&p.project)?;
        let mut paths = Vec::with_capacity(p.paths.len());
        for rel in &p.paths {
            paths.push(engine::path_str(&names::media_path(s.root(), rel)?));
        }
        let result = project.execute("file.import", json!({ "paths": paths }))?;
        edited(s, &project, result)
    });
    app = on(app, "video.clip.insert", &studio, |s, p: edits::Place| {
        let project = s.get(&p.project)?;
        let r = edits::place(&project, &p, true)?;
        edited(s, &project, r)
    });
    app = on(
        app,
        "video.clip.overwrite",
        &studio,
        |s, p: edits::Place| {
            let project = s.get(&p.project)?;
            let r = edits::place(&project, &p, false)?;
            edited(s, &project, r)
        },
    );
    app = on(app, "video.clip.move", &studio, |s, p: edits::Move| {
        let project = s.get(&p.project)?;
        let r = edits::move_clip(&project, &p)?;
        edited(s, &project, r)
    });
    app = on(app, "video.clip.trim", &studio, |s, p: edits::Trim| {
        let project = s.get(&p.project)?;
        let r = edits::trim(&project, &p)?;
        edited(s, &project, r)
    });
    app = on(app, "video.clip.split", &studio, |s, p: edits::Split| {
        let project = s.get(&p.project)?;
        let r = edits::split(&project, &p)?;
        edited(s, &project, r)
    });
    app = on(app, "video.clip.lift", &studio, |s, p: edits::Clips| {
        let project = s.get(&p.project)?;
        let r = edits::delete(&project, &p, false)?;
        edited(s, &project, r)
    });
    app = on(
        app,
        "video.clip.ripple_delete",
        &studio,
        |s, p: edits::Clips| {
            let project = s.get(&p.project)?;
            let r = edits::delete(&project, &p, true)?;
            edited(s, &project, r)
        },
    );
    app = on(app, "video.clip.link", &studio, |s, p: edits::Clips| {
        let project = s.get(&p.project)?;
        let r = edits::link(&project, &p)?;
        edited(s, &project, r)
    });
    app = on(app, "video.clip.unlink", &studio, |s, p: edits::Clips| {
        let project = s.get(&p.project)?;
        let r = edits::unlink(&project, &p)?;
        edited(s, &project, r)
    });
    app = on(app, "video.clip.gain", &studio, |s, p: edits::Gain| {
        let project = s.get(&p.project)?;
        let r = edits::gain(&project, &p)?;
        edited(s, &project, r)
    });
    app = on(
        app,
        "video.marker.add",
        &studio,
        |s, p: edits::MarkerAdd| {
            let project = s.get(&p.project)?;
            let r = edits::marker_add(&project, &p)?;
            edited(s, &project, r)
        },
    );
    app = on(
        app,
        "video.marker.move",
        &studio,
        |s, p: edits::MarkerMove| {
            let project = s.get(&p.project)?;
            let r = edits::marker_move(&project, &p)?;
            edited(s, &project, r)
        },
    );
    app = on(
        app,
        "video.marker.remove",
        &studio,
        |s, p: edits::MarkerRemove| {
            let project = s.get(&p.project)?;
            let r = edits::marker_remove(&project, &p)?;
            edited(s, &project, r)
        },
    );
    app = on(app, "video.edit.undo", &studio, |s, p: Named| {
        let project = s.get(&p.project)?;
        let r = edits::history(&project, false)?;
        edited(s, &project, r)
    });
    app = on(app, "video.edit.redo", &studio, |s, p: Named| {
        let project = s.get(&p.project)?;
        let r = edits::history(&project, true)?;
        edited(s, &project, r)
    });
    app = on(app, "video.frame.render", &studio, |s, p: Render| {
        let project = s.get(&p.project)?;
        still::render(&project, p.frame, p.width.unwrap_or(still::DEFAULT_WIDTH))
    });
    app = {
        let studio = Arc::clone(&studio);
        let exports = Arc::clone(&exports);
        app.command("video.export.start", move |p: ExportStart, cx: Ctx| {
            let studio = Arc::clone(&studio);
            let exports = Arc::clone(&exports);
            async move {
                let wait = p.wait;
                let started = {
                    let exports = Arc::clone(&exports);
                    blocking(move || {
                        let project = studio.get(&p.project)?;
                        exports.start(&project, p.size, p.name.as_deref(), p.loudness_lufs)
                    })
                    .await?
                };
                if !wait {
                    return Ok(started);
                }
                let id = started["id"].as_u64().unwrap_or(0);
                while !exports.finished(id) {
                    if let Some(job) = exports.get(id) {
                        let fraction = job["progress"].as_f64().map(|f| f as f32);
                        cx.progress(fraction, "exporting");
                    }
                    tokio::time::sleep(Duration::from_millis(250)).await;
                }
                Ok(exports.get(id).unwrap_or(Value::Null))
            }
        })
    };
    app = {
        let exports = Arc::clone(&exports);
        app.command("video.export.cancel", move |p: ExportId, _cx: Ctx| {
            let exports = Arc::clone(&exports);
            async move { exports.cancel(p.id).map_err(|e| e.to_string()) }
        })
    };
    app = {
        let exports = Arc::clone(&exports);
        app.command("video.export.list", move |_p: NoParams, _cx: Ctx| {
            let exports = Arc::clone(&exports);
            async move { Ok(json!({ "exports": exports.list() })) }
        })
    };
    Ok(app)
}
