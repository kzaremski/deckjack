//! `deckjack-photo`: the Photos app, a non-destructive developer.
//!
//! This file is the seam between the bus and [`deckjack_photo::engine`]:
//! parameter types (their doc comments are what the palette and agents read)
//! and the move of CPU work onto the blocking pool, so a five-second export
//! never holds up a slider's `photo.lut`.

use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

use deckjack_app::{App, Ctx};
use deckjack_photo::engine::Engine;
use deckjack_photo::export;
use deckjack_photo::recipe::Recipe;
use deckjack_photo::sidecar::Change;
use serde::Deserialize;
use serde_json::{Value, json};

#[derive(rust_embed::RustEmbed)]
#[folder = "ui"]
struct Ui;

/// The working directory is the process's cwd: the core launches an app
/// there.
fn engine() -> Arc<Engine> {
    static ENGINE: OnceLock<Arc<Engine>> = OnceLock::new();
    Arc::clone(ENGINE.get_or_init(|| {
        Arc::new(Engine::new(
            std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
        ))
    }))
}

/// Run `f` on the blocking pool with the engine.
async fn blocking<T: Send + 'static>(
    f: impl FnOnce(&Engine) -> Result<T, String> + Send + 'static,
) -> Result<T, String> {
    let e = engine();
    tokio::task::spawn_blocking(move || f(&e))
        .await
        .map_err(|e| format!("the photo engine failed: {e}"))?
}

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct Dir {
    /// A folder under `Photos/`. Defaults to `Photos`.
    #[serde(default)]
    dir: Option<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct PathOnly {
    /// The photo, as a working-directory path under `Photos/`, e.g.
    /// `Photos/2026/IMG_1234.CR2`.
    path: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct Proxy {
    path: String,
    /// Longest edge in pixels, 64 to 2048. Defaults to 1600.
    #[serde(default)]
    long_edge: Option<u32>,
}

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct LutParams {
    path: String,
    /// The recipe to bake. See docs/formats/photo-edit-v1.md.
    recipe: Recipe,
    /// 17 while a control is moving, 33 when it settles.
    #[serde(default = "settled")]
    size: usize,
}

fn settled() -> usize {
    33
}

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct RenderParams {
    path: String,
    /// The recipe to render. Defaults to the one saved in the sidecar.
    #[serde(default)]
    recipe: Option<Recipe>,
    /// Longest edge in pixels, at most 2048. Defaults to 1024.
    #[serde(default)]
    long_edge: Option<u32>,
}

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct Embedded {
    path: String,
    /// Longest edge in pixels, at most 2048. Defaults to 1024.
    #[serde(default)]
    max_edge: Option<u32>,
}

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct Save {
    path: String,
    /// The whole new recipe, replacing the current one.
    recipe: Recipe,
    /// The revision this edit was made on, from `photo.open` or the last
    /// save. A save on an older revision is refused.
    base_rev: u64,
    /// What the history list shows for this step, e.g. "Exposure".
    #[serde(default)]
    label: Option<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
enum Direction {
    Undo,
    Redo,
}

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct Step {
    path: String,
    base_rev: u64,
    direction: Direction,
}

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct Export {
    path: String,
    format: export::Format,
    /// JPEG quality, 1 to 100. Defaults to 92.
    #[serde(default)]
    quality: Option<u8>,
    /// Longest edge in pixels. Full size when absent.
    #[serde(default)]
    long_edge: Option<u32>,
    /// The folder to write into, under `Photos/`. Defaults to the photo's.
    #[serde(default)]
    dest: Option<String>,
    /// The recipe to export. Defaults to the one saved in the sidecar.
    #[serde(default)]
    recipe: Option<Recipe>,
    /// A name for this export, so `photo.export.cancel` can stop it.
    #[serde(default)]
    job: Option<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct CancelJob {
    /// The `job` an export was started with.
    job: String,
}

fn main() -> anyhow::Result<()> {
    App::new(include_str!("../app.toml"))?
        .ui::<Ui>()
        .command("photo.list", |p: Dir, _cx: Ctx| async move {
            blocking(move |e| e.list(p.dir.as_deref().unwrap_or("Photos"))).await
        })
        .command("photo.open", |p: PathOnly, _cx: Ctx| async move {
            blocking(move |e| e.open(&p.path)).await
        })
        .command("photo.proxy", |p: Proxy, cx: Ctx| async move {
            cx.progress(None, "Decoding");
            blocking(move |e| e.proxy(&p.path, p.long_edge)).await
        })
        .command("photo.lut", |p: LutParams, _cx: Ctx| async move {
            blocking(move |e| e.lut(&p.path, &p.recipe, p.size)).await
        })
        .command("photo.render", |p: RenderParams, _cx: Ctx| async move {
            blocking(move |e| e.render(&p.path, p.recipe, p.long_edge)).await
        })
        .command(
            "photo.preview.embedded",
            |p: Embedded, _cx: Ctx| async move {
                blocking(move |e| e.embedded(&p.path, p.max_edge)).await
            },
        )
        .command("photo.describe", |p: PathOnly, _cx: Ctx| async move {
            blocking(move |e| e.describe(&p.path)).await
        })
        .command("photo.edit.save", |p: Save, _cx: Ctx| async move {
            let change = Change::Save {
                recipe: p.recipe,
                label: p.label.unwrap_or_else(|| "Edit".to_string()),
            };
            blocking(move |e| e.edit(&p.path, p.base_rev, change)).await
        })
        .command("photo.edit.step", |p: Step, _cx: Ctx| async move {
            let change = match p.direction {
                Direction::Undo => Change::Undo,
                Direction::Redo => Change::Redo,
            };
            blocking(move |e| e.edit(&p.path, p.base_rev, change)).await
        })
        .command("photo.export", |p: Export, cx: Ctx| async move {
            let cancel = cx.cancellation().clone();
            engine().start_job(p.job.as_deref(), &cancel);
            let job = p.job.clone();
            let progress_cx = cx.clone();
            let result = blocking(move |e| {
                e.export(
                    &p.path,
                    p.recipe,
                    p.format,
                    p.quality,
                    p.long_edge,
                    p.dest.as_deref(),
                    &cancel,
                    &|f| progress_cx.progress(Some(f), "Exporting"),
                )
            })
            .await;
            engine().end_job(job.as_deref());
            result
        })
        .command("photo.export.cancel", |p: CancelJob, _cx: Ctx| async move {
            let cancelled = engine().cancel_job(&p.job);
            Ok::<Value, String>(json!({ "job": p.job, "cancelled": cancelled }))
        })
        .run()
}
