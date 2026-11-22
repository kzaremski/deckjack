//! What each command does, apart from the bus.
//!
//! `main.rs` only deserializes parameters, moves CPU work onto the blocking
//! pool and returns these functions' answers; the tests drive them directly
//! against a temporary working directory.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::SystemTime;

use base64::Engine as _;
use deckjack_app::Cancellation;
use serde_json::{Value, json};

use crate::color::{self, Global};
use crate::decode::{self, Format, Source};
use crate::export::{self, ExportError};
use crate::lut::{self, Lut};
use crate::paths::{self, Address};
use crate::recipe::Recipe;
use crate::render::{self, MAX_PROXY_EDGE};
use crate::sidecar::{self, Change};

const B64: base64::engine::GeneralPurpose = base64::engine::general_purpose::STANDARD;

/// A decoded photo is kept while it is the one being edited, keyed by what
/// would change if the file did.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
struct Key {
    path: PathBuf,
    size: u64,
    modified: Option<SystemTime>,
}

impl Key {
    fn of(path: &Path) -> Result<Key, String> {
        let m =
            std::fs::metadata(path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
        if !m.is_file() {
            return Err(format!("{} is not a file", path.display()));
        }
        Ok(Key {
            path: path.to_path_buf(),
            size: m.len(),
            modified: m.modified().ok(),
        })
    }
}

/// Proxies kept, most recent last. Each is a few MB of PNG.
const PROXIES_KEPT: usize = 4;

pub struct Engine {
    working: PathBuf,
    /// One decoded photo: at 24 MP that is 288 MB of `f32`, so one.
    source: Mutex<Option<(Key, Arc<Source>)>>,
    /// Held for each full-resolution job: a decode, or an export's
    /// full-size render and encode. The sandbox sets no memory limit, and
    /// a decode inside the pixel cap can peak near 2 GB (15 bytes a pixel
    /// for an 8-bit display file: the decoded bytes plus the `f32` copy),
    /// so these run one at a time rather than one per call in flight.
    /// Never taken while already held: `source` is called before it.
    heavy: Mutex<()>,
    proxies: Mutex<Vec<(Key, u32, Arc<render::Proxy>)>>,
    /// Exports in flight, by the job name the caller chose, so a separate
    /// `photo.export.cancel` can reach them.
    jobs: Mutex<HashMap<String, Cancellation>>,
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Engine {
    pub fn new(working: PathBuf) -> Engine {
        Engine {
            working,
            source: Mutex::new(None),
            heavy: Mutex::new(()),
            proxies: Mutex::new(Vec::new()),
            jobs: Mutex::new(HashMap::new()),
        }
    }

    pub fn working(&self) -> &Path {
        &self.working
    }

    /// Resolve an address to an existing photo this app opens.
    pub fn photo(&self, address: &str) -> Result<Address, String> {
        let a = paths::resolve(&self.working, address).map_err(|e| e.to_string())?;
        paths::contained(&self.working, &a).map_err(|e| e.to_string())?;
        if a.relative.ends_with(sidecar::SUFFIX) {
            return Err(format!("{} is an edit sidecar, not a photo", a.relative));
        }
        if decode::format_of(&a.relative).is_none() {
            return Err(decode::DecodeError::NotAPhoto(a.relative.clone()).to_string());
        }
        Key::of(&a.disk)?;
        Ok(a)
    }

    /// The decoded photo, from the cache when the file has not changed.
    pub fn source(&self, a: &Address) -> Result<Arc<Source>, String> {
        let key = Key::of(&a.disk)?;
        let cached = || match lock(&self.source).as_ref() {
            Some((k, s)) if *k == key => Some(Arc::clone(s)),
            _ => None,
        };
        if let Some(s) = cached() {
            return Ok(s);
        }
        let _heavy = lock(&self.heavy);
        // Another call may have decoded this photo while this one waited.
        if let Some(s) = cached() {
            return Ok(s);
        }
        // Let go of the previous photo first, so its buffer and this
        // decode's peak are not both held by the cache. (A call still
        // using it keeps its own `Arc`.)
        *lock(&self.source) = None;
        let s = Arc::new(decode::decode(&a.disk).map_err(|e| e.to_string())?);
        *lock(&self.source) = Some((key, Arc::clone(&s)));
        Ok(s)
    }

    /// `photo.list`: photos and folders in one folder under `Photos/`.
    pub fn list(&self, dir: &str) -> Result<Value, String> {
        let a = paths::resolve(&self.working, dir).map_err(|e| e.to_string())?;
        paths::contained(&self.working, &a).map_err(|e| e.to_string())?;
        let read =
            std::fs::read_dir(&a.disk).map_err(|e| format!("cannot list {}: {e}", a.relative))?;
        let mut entries = Vec::new();
        for e in read.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') || name.ends_with(sidecar::SUFFIX) {
                continue;
            }
            let path = format!("{}/{name}", a.relative);
            let Ok(kind) = e.file_type() else { continue };
            if kind.is_dir() {
                entries.push(json!({ "name": name, "path": path, "kind": "folder" }));
            } else if let Some(format) = decode::format_of(&name) {
                let edited = sidecar::path_for(&e.path()).exists();
                entries.push(json!({
                    "name": name, "path": path, "kind": "photo",
                    "format": format, "edited": edited,
                }));
            }
        }
        entries.sort_by(|x, y| {
            (x["kind"] != "folder", x["name"].as_str())
                .cmp(&(y["kind"] != "folder", y["name"].as_str()))
        });
        Ok(json!({ "dir": a.relative, "entries": entries }))
    }

    /// `photo.open`.
    pub fn open(&self, address: &str) -> Result<Value, String> {
        let a = self.photo(address)?;
        let src = self.source(&a)?;
        let state = sidecar::state(&a.disk).map_err(|e| e.to_string())?;
        let raw = src.format == Format::Raw;
        Ok(json!({
            "path": a.relative,
            "format": src.format,
            "info": src.info,
            "proxy_encoding": render::proxy_encoding(&src),
            "edit": short_floats(&state),
            "note": raw.then_some(
                "RAW color here is neutral, built from the decoder's camera matrix. It will not \
                 match the camera's own JPEG or another developer's rendering."
            ),
        }))
    }

    /// `photo.proxy`.
    pub fn proxy(&self, address: &str, long_edge: Option<u32>) -> Result<Value, String> {
        let a = self.photo(address)?;
        let edge = long_edge.unwrap_or(1600).clamp(64, MAX_PROXY_EDGE);
        let key = Key::of(&a.disk)?;
        let cached = lock(&self.proxies)
            .iter()
            .find(|(k, e, _)| *k == key && *e == edge)
            .map(|(_, _, p)| Arc::clone(p));
        let p = match cached {
            Some(p) => p,
            None => {
                let src = self.source(&a)?;
                let p = Arc::new(render::proxy(&src, edge).map_err(|e| e.to_string())?);
                let mut list = lock(&self.proxies);
                list.retain(|(k, e, _)| !(*k == key && *e == edge));
                list.push((key, edge, Arc::clone(&p)));
                let len = list.len();
                if len > PROXIES_KEPT {
                    list.drain(..len - PROXIES_KEPT);
                }
                p
            }
        };
        Ok(json!({
            "path": a.relative,
            "width": p.width,
            "height": p.height,
            "encoding": p.encoding,
            "png": B64.encode(&p.png),
        }))
    }

    /// `photo.lut`. The proxy's encoding follows from the file's format, so
    /// this never decodes the photo: it is the call made on every slider
    /// tick.
    pub fn lut(&self, address: &str, recipe: &Recipe, size: usize) -> Result<Value, String> {
        recipe.validate().map_err(|e| e.to_string())?;
        let a = self.photo(address)?;
        let format = decode::format_of(&a.relative).ok_or("not a photo")?;
        let encoding = if format == Format::Raw {
            color::Encoding::Log
        } else {
            color::Encoding::Srgb
        };
        let size = if size <= lut::DRAG {
            lut::DRAG
        } else {
            lut::SETTLED
        };
        let global = Global::new(recipe, encoding);
        let g = Lut::bake(size, |c| global.apply(c));
        let locals: Vec<String> = recipe
            .masks
            .iter()
            .map(|l| {
                let op = color::LocalOp::new(l);
                B64.encode(Lut::bake(size, |c| op.apply(c)).to_bytes())
            })
            .collect();
        Ok(json!({
            "size": size,
            "encoding": encoding,
            "global": B64.encode(g.to_bytes()),
            "locals": locals,
        }))
    }

    /// The recipe a call means: the one given, else the sidecar's.
    fn recipe_or_saved(&self, a: &Address, recipe: Option<Recipe>) -> Result<Recipe, String> {
        let r = match recipe {
            Some(r) => r,
            None => sidecar::state(&a.disk).map_err(|e| e.to_string())?.recipe,
        };
        r.validate().map_err(|e| e.to_string())?;
        Ok(r)
    }

    /// `photo.render`: the full pipeline at preview size, as a JPEG. The
    /// preview's fallback where WebGL2 is missing, and the reference the
    /// parity test compares the LUT path against.
    pub fn render(
        &self,
        address: &str,
        recipe: Option<Recipe>,
        long_edge: Option<u32>,
    ) -> Result<Value, String> {
        let a = self.photo(address)?;
        let recipe = self.recipe_or_saved(&a, recipe)?;
        let src = self.source(&a)?;
        let edge = long_edge.unwrap_or(1024).clamp(16, MAX_PROXY_EDGE);
        let r = render::render(&src, &recipe, Some(edge), &|| false, &|_| {})
            .map_err(|_| "cancelled".to_string())?;
        let jpeg = r.jpeg(90).map_err(|e| e.to_string())?;
        Ok(json!({ "width": r.width, "height": r.height, "jpeg": B64.encode(jpeg) }))
    }

    /// `photo.preview.embedded`.
    pub fn embedded(&self, address: &str, max_edge: Option<u32>) -> Result<Value, String> {
        let a = self.photo(address)?;
        let edge = max_edge.unwrap_or(1024).clamp(16, MAX_PROXY_EDGE);
        // A RAW's embedded JPEG is small; any other photo is decoded in
        // full to make this, so it waits its turn like any decode.
        let _heavy =
            (decode::format_of(&a.relative) != Some(Format::Raw)).then(|| lock(&self.heavy));
        let img = decode::embedded_preview(&a.disk, edge).map_err(|e| e.to_string())?;
        let rgb = img.to_rgb8();
        let mut jpeg = Vec::new();
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut jpeg, 85)
            .encode_image(&rgb)
            .map_err(|e| e.to_string())?;
        Ok(json!({ "width": rgb.width(), "height": rgb.height(), "jpeg": B64.encode(jpeg) }))
    }

    /// `photo.describe`: the state an agent needs to reason about an edit
    /// without looking at pixels.
    pub fn describe(&self, address: &str) -> Result<Value, String> {
        let a = self.photo(address)?;
        let src = self.source(&a)?;
        let state = sidecar::state(&a.disk).map_err(|e| e.to_string())?;
        let r = render::render(&src, &state.recipe, Some(512), &|| false, &|_| {})
            .map_err(|_| "cancelled".to_string())?;
        let (w, h) =
            crate::geometry::output_size(&state.recipe.geometry, src.width(), src.height());
        Ok(json!({
            "path": a.relative,
            "format": src.format,
            "source": { "width": src.width(), "height": src.height(), "camera": src.info.camera },
            "output": { "width": w, "height": h },
            "rev": state.rev,
            "history": { "position": state.position, "entries": state.history.len() },
            "matches_original": state.matches_original,
            "changes": changes(&state.recipe),
            "recipe": short_floats(&state.recipe),
            "histogram": histogram(&r),
        }))
    }

    /// `photo.edit.save` and `photo.edit.step`.
    pub fn edit(&self, address: &str, base_rev: u64, change: Change) -> Result<Value, String> {
        let a = self.photo(address)?;
        let decoder = if decode::format_of(&a.relative) == Some(Format::Raw) {
            "rawler 0.8.0"
        } else {
            "image 0.25"
        };
        let s = sidecar::write(&a.disk, base_rev, change, decoder).map_err(|e| e.to_string())?;
        Ok(json!({
            "path": a.relative,
            "rev": s.rev,
            "recipe": short_floats(&s.recipe),
            "position": s.position,
            "history": s.history.iter().map(|e| json!({ "label": e.label, "at": e.at })).collect::<Vec<_>>(),
        }))
    }

    /// Register an export under `job` so `photo.export.cancel` reaches it.
    pub fn start_job(&self, job: Option<&str>, cancel: &Cancellation) {
        if let Some(job) = job {
            lock(&self.jobs).insert(job.to_string(), cancel.clone());
        }
    }

    pub fn end_job(&self, job: Option<&str>) {
        if let Some(job) = job {
            lock(&self.jobs).remove(job);
        }
    }

    /// `photo.export.cancel`. True when an export was running under `job`.
    pub fn cancel_job(&self, job: &str) -> bool {
        match lock(&self.jobs).get(job) {
            Some(c) => {
                c.cancel();
                true
            }
            None => false,
        }
    }

    /// `photo.export`: render, encode, write under a new name.
    #[allow(clippy::too_many_arguments)]
    pub fn export(
        &self,
        address: &str,
        recipe: Option<Recipe>,
        format: export::Format,
        quality: Option<u8>,
        long_edge: Option<u32>,
        dest: Option<&str>,
        cancel: &Cancellation,
        progress: &dyn Fn(f32),
    ) -> Result<Value, String> {
        let a = self.photo(address)?;
        let recipe = self.recipe_or_saved(&a, recipe)?;
        let dir = match dest {
            Some(d) => {
                let d = paths::resolve(&self.working, d).map_err(|e| e.to_string())?;
                paths::contained(&self.working, &d).map_err(|e| e.to_string())?;
                if !d.disk.is_dir() {
                    return Err(format!("{} is not a folder", d.relative));
                }
                d
            }
            None => {
                let parent = Path::new(&a.relative)
                    .parent()
                    .unwrap_or(Path::new(paths::ROOT));
                paths::resolve(&self.working, &parent.to_string_lossy())
                    .map_err(|e| e.to_string())?
            }
        };
        let src = self.source(&a)?;
        // The full-size render and its encoded copy are the other big
        // allocations: one at a time, and never beside a decode.
        let heavy = lock(&self.heavy);
        let is_cancelled = || cancel.is_cancelled();
        progress(0.0);
        let rendered = render::render(&src, &recipe, long_edge, &is_cancelled, &|f| {
            progress(f * 0.85)
        })
        .map_err(|e| ExportError::from(e).to_string())?;
        drop(src);
        let bytes =
            export::encode(&rendered, format, quality.unwrap_or(92)).map_err(|e| e.to_string())?;
        drop(heavy);
        progress(0.95);
        let stem = Path::new(&a.relative)
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "photo".to_string());
        let written = export::write_new(&dir.disk, &stem, format, &bytes, &is_cancelled)
            .map_err(|e| e.to_string())?;
        progress(1.0);
        let name = written
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        Ok(json!({
            "path": format!("{}/{name}", dir.relative),
            "bytes": bytes.len(),
            "width": rendered.width,
            "height": rendered.height,
        }))
    }
}

/// The recipe's non-default values as named lines, for agents and the
/// describe-style summary.
pub fn changes(r: &Recipe) -> Vec<String> {
    let d = Recipe::default();
    let mut out = Vec::new();
    if r.exposure_ev != 0.0 {
        out.push(format!("exposure {:+.2} EV", r.exposure_ev));
    }
    if r.white_balance != d.white_balance {
        out.push(format!(
            "white balance: temperature {:+.0}, tint {:+.0}",
            r.white_balance.temperature, r.white_balance.tint
        ));
    }
    for (name, l, dl) in [
        ("master", r.levels.master, d.levels.master),
        ("red", r.levels.red, d.levels.red),
        ("green", r.levels.green, d.levels.green),
        ("blue", r.levels.blue, d.levels.blue),
    ] {
        if l != dl {
            out.push(format!(
                "levels {name}: black {:.3}, white {:.3}, gamma {:.2}, output {:.3}-{:.3}",
                l.black, l.white, l.gamma, l.out_black, l.out_white
            ));
        }
    }
    for (name, c) in [
        ("master", &r.curves.master),
        ("red", &r.curves.red),
        ("green", &r.curves.green),
        ("blue", &r.curves.blue),
    ] {
        if !c.is_empty() {
            out.push(format!("{name} curve through {} points", c.len()));
        }
    }
    if r.saturation != 0.0 {
        out.push(format!("saturation {:+.2}", r.saturation));
    }
    let g = r.geometry;
    if g.quarter_turns != 0 {
        out.push(format!(
            "turned {} degrees clockwise",
            u32::from(g.quarter_turns) * 90
        ));
    }
    if g.flip_h {
        out.push("flipped left to right".to_string());
    }
    if g.flip_v {
        out.push("flipped top to bottom".to_string());
    }
    if g.straighten_deg != 0.0 {
        out.push(format!("straightened {:+.1} degrees", g.straighten_deg));
    }
    if g.crop != crate::recipe::Rect::FULL {
        out.push(format!(
            "cropped to {:.0}% x {:.0}% at ({:.2}, {:.2})",
            g.crop.w * 100.0,
            g.crop.h * 100.0,
            g.crop.x,
            g.crop.y
        ));
    }
    for (i, m) in r.masks.iter().enumerate() {
        let crate::recipe::Mask::Radial { cx, cy, .. } = m.mask;
        out.push(format!(
            "local {} (radial at {cx:.2}, {cy:.2}{}): exposure {:+.2} EV, saturation {:+.2}",
            i + 1,
            if m.invert { ", inverted" } else { "" },
            m.exposure_ev,
            m.saturation
        ));
    }
    out
}

/// Luma statistics and clipping of a render.
pub fn histogram(r: &render::Rendered) -> Value {
    let n = (r.width * r.height) as usize;
    let mut bins = [0u32; 256];
    let (mut black, mut white) = (0usize, 0usize);
    for p in r.pixels.as_chunks::<3>().0 {
        let y = 0.2126 * p[0] + 0.7152 * p[1] + 0.0722 * p[2];
        bins[(y.clamp(0.0, 1.0) * 255.0).round() as usize] += 1;
        if p.iter().all(|&c| c <= 0.5 / 255.0) {
            black += 1;
        }
        if p.iter().any(|&c| c >= 254.5 / 255.0) {
            white += 1;
        }
    }
    let pct = |q: f64| {
        let target = (q * n as f64).ceil() as u64;
        let mut acc = 0u64;
        for (i, b) in bins.iter().enumerate() {
            acc += u64::from(*b);
            if acc >= target.max(1) {
                return i as f64 / 255.0;
            }
        }
        1.0
    };
    let mean: f64 = bins
        .iter()
        .enumerate()
        .map(|(i, b)| i as f64 * f64::from(*b))
        .sum::<f64>()
        / (n as f64 * 255.0);
    json!({
        "luma": { "mean": round3(mean), "p01": round3(pct(0.01)), "p50": round3(pct(0.5)), "p99": round3(pct(0.99)) },
        "clipped": {
            "shadows": round3(black as f64 / n as f64),
            "highlights": round3(white as f64 / n as f64),
        },
        "bins": bins.chunks(8).map(|c| c.iter().sum::<u32>()).collect::<Vec<_>>(),
    })
}

/// A value whose `f32` fields read as written: `0.7`, not the
/// `0.699999988079071` that `serde_json::to_value` makes by widening each
/// `f32` to `f64`. Through text, so each number keeps its shortest form.
pub fn short_floats<T: serde::Serialize>(v: &T) -> Value {
    serde_json::to_string(v)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or(Value::Null)
}

fn round3(v: f64) -> f64 {
    (v * 1000.0).round() / 1000.0
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Barrier;
    use std::time::Duration;

    /// A working directory with `Photos/<name>`: a `w` x `h` PNG.
    fn workdir(names: &[&str], w: u32, h: u32) -> (tempfile::TempDir, Engine) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("Photos")).unwrap();
        let img = image::RgbImage::from_fn(w, h, |x, y| image::Rgb([x as u8, y as u8, 128]));
        for name in names {
            img.save(dir.path().join("Photos").join(name)).unwrap();
        }
        let engine = Engine::new(dir.path().to_path_buf());
        (dir, engine)
    }

    #[test]
    fn two_calls_opening_one_photo_decode_it_once() {
        // Big enough that the second call reaches the gate before the
        // first decode finishes.
        let (_dir, engine) = workdir(&["a.png"], 3000, 2000);
        let a = engine.photo("Photos/a.png").unwrap();
        let start = Barrier::new(2);
        let (x, y) = std::thread::scope(|s| {
            let run = || {
                start.wait();
                engine.source(&a).unwrap()
            };
            let x = s.spawn(run);
            let y = s.spawn(run);
            (x.join().unwrap(), y.join().unwrap())
        });
        assert!(Arc::ptr_eq(&x, &y), "the photo was decoded twice");
    }

    #[test]
    fn a_decode_waits_for_the_full_resolution_job_before_it() {
        let (_dir, engine) = workdir(&["a.png", "b.png"], 64, 48);
        for name in ["Photos/a.png", "Photos/b.png"] {
            let held = lock(&engine.heavy);
            std::thread::scope(|s| {
                let job = if name.ends_with("a.png") {
                    s.spawn(|| engine.source(&engine.photo(name).unwrap()).map(|_| ()))
                } else {
                    s.spawn(|| engine.embedded(name, Some(32)).map(|_| ()))
                };
                std::thread::sleep(Duration::from_millis(300));
                assert!(!job.is_finished(), "{name} decoded beside another job");
                drop(held);
                job.join().unwrap().unwrap();
            });
        }
    }

    #[test]
    fn the_previous_photo_is_let_go_before_the_next_decode() {
        let (dir, engine) = workdir(&["a.png"], 64, 48);
        std::fs::write(dir.path().join("Photos/bad.png"), b"not a png").unwrap();
        let first = engine
            .source(&engine.photo("Photos/a.png").unwrap())
            .unwrap();
        assert_eq!(Arc::strong_count(&first), 2);
        let bad = engine.photo("Photos/bad.png").unwrap();
        assert!(engine.source(&bad).is_err());
        assert_eq!(Arc::strong_count(&first), 1, "the cache still holds it");
    }

    #[test]
    fn an_export_renders_while_holding_the_full_resolution_gate() {
        let (_dir, engine) = workdir(&["a.png"], 64, 48);
        let held = std::cell::Cell::new(Vec::new());
        let progress = |f: f32| {
            let mut v = held.take();
            v.push((f, engine.heavy.try_lock().is_err()));
            held.set(v);
        };
        engine
            .export(
                "Photos/a.png",
                None,
                export::Format::Jpeg,
                None,
                None,
                None,
                &Cancellation::new(),
                &progress,
            )
            .unwrap();
        // Reports before 0.95 come from the render; 0.95 follows the encode.
        let v = held.take();
        let encoded = v
            .iter()
            .position(|(f, _)| *f == 0.95)
            .expect("no 0.95 report");
        assert!(encoded > 0 && v[..encoded].iter().all(|p| p.1), "{v:?}");
        // Released before the file is written.
        assert!(v[encoded..].iter().all(|p| !p.1), "{v:?}");
    }
}
