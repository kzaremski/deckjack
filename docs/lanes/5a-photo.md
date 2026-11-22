## Lane 5a — Photo editor

Scoping brief, 2026-11-16, with the first build pass and the measured spikes
added 2026-11-21 (the two sections right below). Everything after them is the
original brief, unchanged; where the build departed from it, "Deviations"
says so. Timings and sizes marked *(est.)* in the original are inferred, not
measured.

### Spikes 1–3, as measured (2026-11-21)

All three ran on the laptop (Ryzen AI / Radeon 890M, 24 threads, Arch). The
R620 and the phone were not available to this pass, so nothing below is an
R620 or phone number.

**Spike 1, LUT preview parity: passes. Option D stands.**

- Harness: `crates/deckjack-photo/examples/parity.rs` writes the production
  proxy, the production LUT bake and the direct Rust render;
  `spike/lut-parity.mjs` draws the proxy through the production shader
  (`ui/gl.js`) in headless Chromium via the repo's Playwright, and
  `spike/compare.py` scores CIEDE2000 (validated against Sharma's test pairs)
  against the direct render quantized to 8 bits.
- Photo: the operator's own `IMG_2058.JPG` (Canon 750D), proxy 683×1024.
- Recipe A (the brief's spike recipe: +0.7 EV, temperature +30, tint −10, an
  S-curve, one radial mask +0.7 EV):

  | | mean ΔE2000 | p99 | max |
  | --- | --- | --- | --- |
  | 33³, SwiftShader | 0.115 | 0.973 | 1.72 |
  | 33³, Radeon 890M (ANGLE/Vulkan) | 0.136 | 0.846 | 1.82 |
  | 17³, SwiftShader | 0.359 | 1.205 | 2.01 |
  | 17³, Radeon 890M | 0.377 | 1.166 | 1.99 |

- Recipe B (stress: +2 EV, temperature +80, tint +30, saturation +0.6,
  levels, a hard master S-curve plus a red curve, a radial −1.5 EV mask):
  33³ mean 0.168, p99 1.110 on both renderers; 17³ mean 0.52, **p99 3.24**,
  which misses the 33³ bar but is only shown while a control moves.
- The GPU matches the Rust trilinear model of the shader to within 0.02 mean
  ΔE, so the error is LUT interpolation, not the GPU. That is what lets
  `tests/parity.rs` stand in for the GPU in `cargo test`: preview (model)
  against export at the same scale on a synthetic card with the exit-check
  recipe gives mean **0.23**, p99 0.72 (bar: mean < 1.5).
- **The numbers above are the sRGB path only** (a JPEG in the spike, a PNG
  card in the test). The RAW path, the exit check's main case, goes through
  an 8-bit log proxy with dither and log-indexed LUTs, and scores worse.
  Measured after review with `tests/parity.rs`'s own `score()` and
  exit-check recipe at a 640 px proxy (production model of the shader, the
  proxy resampled; not the spike's LUT-only model):

  | photo | 33³ mean | 33³ p99 | 17³ mean | 17³ p99 |
  | --- | --- | --- | --- | --- |
  | `IMG_2058.CR2` (750D) | 1.045 | 4.57 | 1.163 | 4.50 |
  | `DSC00870.ARW` (ZV-1) | 0.838 | 4.43 | 0.998 | 4.41 |
  | `IMG_2058.JPG`, same model | 0.472 | 3.98 | 0.509 | 3.97 |
  | synthetic Bayer DNG card (in `cargo test`) | 0.600 | 2.32 | 0.723 | 2.43 |
  | synthetic PNG card (in `cargo test`) | 0.226 | 0.72 | 0.241 | — |

  The RAW means pass the 1.5 bar, but by less than the sRGB figures
  suggested, and every real photo's p99 is over the spike's 3.0 bar. The
  JPEG's p99 is close to the RAWs', so most of that tail is probably the
  proxy's resampling at edges and fine texture rather than color; that is
  inferred, not isolated. `tests/parity.rs` now scores a Bayer DNG card
  through the RAW path too (mean < 1.5 at both sizes, p99 < 3.5), so a log
  or dither regression fails a test; it was checked by breaking the log
  encoding, doubling the dither and turning a RAW proxy sRGB.
- Cost: baking a 33³ LUT takes 3.7 ms, a 17³ one 0.5 ms; a 33³ LUT is 216 KB
  raw, 288 KB as base64. Uploading one and drawing a 683×1024 frame took
  15 ms (33³) and 2 ms (17³) in the harness, base64 decode included.
- **Not measured:** ≥10 LUT updates a second from the R620 to the phone over
  `/ws`. No phone or R620 in this pass. The bake is not the limit; the link
  is unknown.

**Spike 2, 8-bit log proxy banding: banding confirmed on a clean gradient;
dither kept; the precise fix does not fit the frame budget.**

- `examples/banding.rs`: a synthetic sky (0.03–0.12 linear, 2 EV under),
  developed +2 EV through a strong S-curve, each 8-bit proxy variant against
  the f32 reference, through the pipeline directly (no LUT).

  | proxy | longest flat run (px) | largest step (codes) | column-mean error (codes) | PNG, 2048×256 |
  | --- | --- | --- | --- | --- |
  | f32 reference | 24 | 1 | — | — |
  | 8-bit sRGB | 60 | 4 | 2.00 | 8.6 KB |
  | 8-bit sRGB + dither | 20 | 7 | 1.05 | 480 KB |
  | **8-bit log** | **112** | **6** | **3.00** | 8.5 KB |
  | **8-bit log + dither** | 18 | 12 | 1.07 | 481 KB |
  | 16-bit log | — | — | — | 21.5 KB |

  Undithered 8-bit log bands visibly (112 px plateaus with 6-code steps).
  Dither removes the bands in the mean (error ~1 code) and trades them for
  per-pixel grain up to 12 codes under this edit.
- The same edit on the two real RAWs at a 2048 px proxy, ΔE2000 per pixel
  against the f32 reference: CR2 8-bit log mean 0.69 / p99 2.90, dithered
  1.10 / 5.06; ARW 0.58 / 2.23, dithered 0.94 / 3.91. Per-pixel ΔE punishes
  grain, so it favors no dither; a smooth sky is where bands show, so the
  brief's dither is kept. Sizes: 8-bit log PNG 3.1 MB (CR2) / 4.7 MB (ARW),
  dithered 3.2 / 4.8 MB; **16-bit log PNG 14.6 / 16.3 MB**.
- Decision: v1 ships 8-bit log + dither for RAW proxies (`render::quantize`).
  A 16-bit or hi/lo proxy at 2048 px is 2× the 8 MiB socket frame before
  base64, so the precise fix needs either a ≤1024 px 16-bit proxy or the
  binary transfer D6 deferred. Exports never see this: they render from f32.
  The proxy defaults to 1600 px long edge in the UI.

**Spike 3, RAW decoder: rawler 0.8.0.**

- On disk: 192 CR2 from a Canon EOS 750D (rawler says "Rebel T6i") under
  `~/Nextcloud/Photography`, and a zip of Sony ZV-1 ARWs. One of each was
  copied to the scratchpad; nothing was downloaded.
- Both decoders read both cameras. LibRaw is the system's 0.22.2 (via its
  `dcraw_emu`, AHD, 16-bit linear); rawler 0.8.0 through
  `RawDevelop::develop_intermediate` (PPG demosaic, as-shot WB, camera matrix).

  | | rawler: decode + develop | rawler peak RSS | LibRaw wall | LibRaw peak RSS |
  | --- | --- | --- | --- | --- |
  | CR2, 24 MP | 0.18 + 0.16 s (0.76 s process) | 750 MiB | 0.75 s | 289 MiB |
  | ARW, 20 MP | 0.06 + 0.14 s (0.52 s process) | 625 MiB | 0.44 s | 249 MiB |

- Color agreement, aligned and box-downscaled 4×: mean ΔE2000 2.35 (CR2) and
  2.07 (ARW) between the two, rawler ~6% brighter. At 1:1 the two demosaics
  look alike on the CR2's fine texture (no measurement of zipper or maze
  artifacts was made).
- Licensing: rawler LGPL-2.1, pure Rust, no system dependency. LibRaw would
  need either the system `libraw.so` (present here, absent on Termux and the
  R620 unless installed) or `rsraw`'s C++ build.
- Decision: **rawler**, pinned `=0.8.0`. It decodes the operator's actual
  cameras, costs no C toolchain, and keeps the Android build cheap. Its cost
  is ~2.5× LibRaw's peak memory. Two findings it forced: rawler's own
  `RawImage::orientation` read "Normal" for a CR2 whose EXIF says 8, so the
  EXIF value is used; and rawler lists its extensions in upper case, which
  the first extension check missed (found by the spike, now tested).

### Built in the first pass, and deferred

Crate `crates/deckjack-photo` (app id `photo`, title "Photos"), its UI in
`ui/`, the format spec `docs/formats/photo-edit-v1.md`, and cancellation in
the `deckjack-app` SDK.

Built:

- **Open:** JPEG, PNG, WebP (via `image`), TIFF 8/16-bit/float, chunky or
  planar (via `tiff` directly, so the core's thumbnailer does not gain TIFF
  through feature unification), and camera RAW/DNG through rawler, each
  under `catch_unwind`. EXIF orientation applied. 120 MP decode cap. The
  `tiff` crate's default 256 MiB buffer limit is raised to that cap: left
  in place, it refused any 16-bit RGB TIFF over ~44.7 MP, the app's own
  `tiff16` export of a 45 MP RAW included (found in review; a 45.4 MP
  16-bit TIFF now opens in `tests/tiff.rs`).
- **Decode memory:** one full-resolution job at a time, a decode or an
  export's full-size render and encode (`Engine::heavy`), and the cached
  photo is dropped before a different one is decoded. The sandbox sets no
  memory limit, so this is the only bound besides the pixel cap. Measured
  (release, two threads opening two different 10950×10950 black PNGs, peak
  RSS): 1.76 GB with the gate, 3.46 GB without it. One decode inside the
  cap still peaks near 2 GB for an 8-bit display file (15 bytes a pixel:
  the decoded bytes plus the `f32` copy); that single-decode peak is not
  reduced, and an export of such a file still adds its full-size render.
  This is serialization, not a byte budget: an open waits behind a running
  export.
- **Edit model:** exposure, white balance (temperature/tint), levels (master
  and per channel), curves (master and per channel, monotone cubic, 16
  points), saturation, quarter turns, flips, straighten ±45°, crop, up to four
  radial masks with exposure and saturation. `deny_unknown_fields` and range
  validation that names the field.
- **Sidecar** `<photo>.deckjack.json`: schema, original's size and SHA-256,
  decoder, revision, recipe, bounded history (100) with a position. Atomic
  write through a temporary file created with `O_EXCL`, so a symlink
  planted at the predictable temporary name is passed over rather than
  written through (found in review: `File::create` would have truncated a
  photo the link pointed at); `base_rev` refuses a stale save with "edited
  elsewhere since you opened it"; undo/redo are revisions too
  (`photo.edit.step`).
- **Preview (option D):** `photo.proxy` (8-bit sRGB, or log + dither for
  RAW, PNG), `photo.lut` (17³/33³ half floats, global plus one per mask),
  the WebGL2 shader in `ui/gl.js` with context-loss rebuild, and
  `photo.render` as fallback A when WebGL2 is missing.
- **Export:** JPEG or 16-bit TIFF with an embedded sRGB ICC profile (moxcms),
  full size or a long-edge limit, progress, `-edit`/`-edit (n)` naming,
  `renameat2(RENAME_NOREPLACE)` (hard-link fallback), a hidden partial name
  removed on every non-success path, cancellation. Original's SHA-256
  asserted unchanged in `tests/original.rs`.
- **Agents:** `photo.describe` (named changes, output size, history position,
  histogram, clipped fractions) and `photo.list`.
- **`photo.preview.embedded`:** the RAW's embedded JPEG (or a scaled decode
  of a display photo), oriented and capped.
- **SDK:** `deckjack_app::Cancellation` on `Ctx`, flipped by the core's
  `cancel` frame for that call only (`deckjack-app` tests over a real socket
  pair).
- **UI:** plain JS, no build step, `ui/main.js` as a file (no inline script),
  only command results for pixels. Tabs Light, Levels, Curves, Crop, Local,
  Export; a canvas preview on a fixed gray; sliders with a whole-row hit
  target, numeric entry and double-tap reset; a curves editor with a touch
  offset, drag-off and long-press delete and an input → output readout; crop
  handles at touch size with aspect chips, turns, flips and straighten;
  radial mask pin, outline through the geometry, and a tint overlay; Before
  (button, press-and-hold on the photo, `\`); undo/redo (`Ctrl+Z`,
  `Ctrl+Shift+Z`); `C` for crop; the open-photo picker; `?path=` and
  `deckjack.app.open` only open. One LUT call in flight, newest recipe next;
  saves debounced and stopped on a conflict. `await save()` waits out every
  save in flight and any started after it, however many callers are
  waiting, and a save's answer is dropped if another photo was opened
  meanwhile (found in review: two waiters could let an undo go out on the
  revision before a save still in flight, which marked the window stale for
  no reason; tested in `web/src/apps/photo.test.js`, not in a browser).
- **Verified live** (release build, `cargo xtask install` into a throwaway
  home, `deckjack --debug` on 127.0.0.1:8781, app approved and running
  confined): the CR2, the ARW and the JPEG open (0.3–0.6 s), edits preview
  and save, a 4000×5002 16-bit TIFF export took 0.4 s and carried the sRGB
  profile, a second export was named `-edit (2)`, a cancelled export
  answered "export cancelled; nothing was written" and left no file, and
  the three originals' SHA-256 were unchanged. Screens checked at 1280×800
  and 360×780.

Deferred, each a known gap:

- **The core does not forward a client's `cancel` to an app.** The SDK now
  honors the frame, but `apphost.rs` never sends it and `wiring.rs` only
  drops `terminal.read` on cancel, and `kit/bus.js` has no cancel at all.
  Until those land (core and shell changes, outside this lane), the UI
  cancels an export through the app's own `photo.export.cancel {job}`, which
  flips the same `Cancellation`.
- **Quick Look does not call `photo.preview.embedded` yet.** The command is
  built and tested; wiring it into `web/src/shell/quicklook.ts` is a shell
  change left for the merge.
- Brush and linear-gradient masks; vibrance; contrast/highlights/shadows as
  named controls (a curve does it today); WB picker and as-shot temperature
  for RAW.
- 1:1 zoom and `photo.tile`, pinch zoom and pan, `/kit/gestures.js`; fine
  slider control by moving the finger away; the crop's image-under-a-fixed-
  frame interaction (the frame moves instead); two-finger rotate and the
  level-line tool; a live histogram in the UI (it is in `photo.describe`).
- Reading embedded ICC profiles on input; EXIF copied into exports; PNG
  export; the on-disk proxy/LUT cache under `$HOME/.cache/photo/` (proxies
  are cached in memory, four at a time, plus one decoded photo); a byte
  budget for decodes and exports (full-resolution jobs are serialized
  instead, see "Decode memory"), or a lower pixel cap for phone hosts;
  strip-wise export (a 24 MP RAW export holds the f32 image, ~290 MB, plus
  rawler's ~750 MB peak).
- Spike 4 (45–61 MP export memory), 5 (HEIC), 8 (WebKit), 9 (touch on the
  devices), 10 (moxcms against lcms2) were not run.

Deviations from the brief:

- Linear working space is sRGB/Rec.709 primaries, not Rec.2020: rawler's
  `Calibrate` step already lands there, and nothing in v1 needs the wider
  gamut. Out-of-gamut RAW colors clip at step 3.
- `photo.render` (whole picture at a size) instead of `photo.tile` (a rect at
  a scale); it serves as fallback A and as the parity reference.
- Two commands the brief did not list: `photo.list` (the picker), and
  `photo.export.cancel` plus `photo.edit.step` (see above).

**Exists (before this lane):** nothing. Related pieces: `thumbnail.rs` in the core decodes
JPEG/PNG/GIF/WebP/BMP with the `image` crate for Files thumbnails. Quick Look
refuses camera RAW and lists "RAW previews (needs a node-side developer)" as
still owed. `web/src/shell/gestures.ts` has the pinch, pan and swipe geometry,
but it is shell TypeScript, so an app frame cannot import it.

### Goal

Develop a photo, RAW or JPEG, from the phone as well as the desktop. Crop,
straighten, levels, curves and a few masked local adjustments, all
non-destructive. The original is never written. Every edit lives in a
plain-text sidecar next to the original, and an export produces an ordinary
JPEG, PNG or TIFF that any tool opens with the right colors and orientation.

The app is also the **confined RAW developer** that Quick Look and Files lack
today. Its embedded-preview command gives them RAW thumbnails and previews
without the unconfined core parsing RAW files.

### The useful subset (v1)

In:

- **Open:** JPEG, PNG, TIFF (8- and 16-bit), WebP, and camera RAW plus DNG
  through one decoder (see Engine). EXIF orientation is applied on decode.
- **Geometry:** crop with aspect presets (free, original, 1:1, 4:5, 3:2, 16:9),
  90° rotations, horizontal and vertical flip, and straighten to ±45° with a
  level-line tool.
- **Global tone and color:** exposure, white balance (temperature and tint;
  as-shot and a picker for RAW), contrast, highlights and shadows done as a
  tone curve (not spatial), levels (black point, white point and gamma, master
  plus per channel, and output range), curves (master plus R/G/B, monotone
  cubic, up to 16 points), saturation and vibrance.
- **Masks:** up to **four** local adjustments. Each has one mask: a linear
  gradient, a radial gradient, or a brush (strokes with size, feather and
  flow, plus an eraser). Each can invert. A local adjustment changes exposure,
  contrast, saturation and temperature under its mask.
- **Non-destructive:** the sidecar holds the edit recipe and its history.
  Undo and redo, before/after, reset, and copy and paste of a recipe between
  photos (it is just JSON).
- **Inspect:** a live histogram of the rendered result, clipping warnings, and
  1:1 zoom rendered by the backend at full resolution.
- **Export:** JPEG (quality setting), PNG, or 16-bit TIFF, at full size or a
  long-edge limit. sRGB with an embedded ICC profile. Orientation is baked in
  and reset to 1. Basic EXIF (camera, lens, exposure, date) is copied.
  Progress is reported and the export can be cancelled. It never overwrites.

Explicitly **not** in v1:

- Spatial operations: denoise, clarity and texture, dehaze, and capture
  sharpening (output sharpening on export is the only candidate, and it waits
  until after v1).
- Lens corrections (lensfun), perspective and keystone, healing and clone,
  AI or subject masks, layers and compositing, HDR merge and panoramas.
- Highlight reconstruction beyond keeping clipping neutral.
- A library: albums, ratings, search and a catalog belong to Photos/Immich per
  DESIGN.md. This app edits one file at a time, and Files is the browser.
- Batch export (5c's queue is the right home), AVIF/JXL/HEIC export,
  wide-gamut or HDR display, printing, tethering.
- **HEIC input.** iPhone and Samsung photos are often HEIC, so leaving it out
  is a real gap. libheif is C, LGPL, and carries HEVC patent questions. See
  Risks.
- Backend GPU (see Engine).
- Pressure-sensitive stylus brushing.

### Engine

The architecture decision comes first, because it decides every other choice.

**Where does the live preview render?**

| Option | Pros | Cons |
| --- | --- | --- |
| A. Backend renders every slider tick, returns a JPEG | One implementation, so preview equals export | A 0.3–1 MB image per tick through the bus and the broker. Few frames per second over a phone link *(est.)*. The R620 has no AVX2 and no GPU |
| B. The whole pipeline reimplemented in WebGL2 in the frame | 60 fps on the phone | Two implementations of every operation, and they drift. That is a parity bug factory |
| C. Run Rust in the frame as wasm | One codebase | **Not possible under APP-UI-ISOLATION.** The frame's `script-src` has no `'wasm-unsafe-eval'`, and browsers block `WebAssembly.compile`/`instantiate` without it. Allowing it means reopening the accepted CSP |
| **D. Backend bakes a 3D LUT and the frame applies it (recommended)** | The backend's Rust is the only definition of color. The frame runs one generic shader: trilinear lookup in a 3D texture plus an affine transform for geometry. A LUT is 17³ while dragging (~40 KB as half float) and 33³ on release (~290 KB) | Only per-pixel operations fit in a LUT. Spatial operations would need backend-rendered tiles. The proxy has to carry enough precision |

**Recommended: D.** It works like this:

1. `photo.proxy` returns the image at most 2048 px on its long edge,
   *unadjusted*.
   - For RAW, it is demosaiced, converted from camera space to linear
     Rec.2020, and log-encoded to 8 bits with blue-noise dither.
   - For JPEG and other display images, it is the decoded sRGB data.
   - It is encoded losslessly (PNG or lossless WebP) and cached by the
     backend.
2. Every recipe change asks for `photo.lut`. All global per-pixel operations
   (exposure, white balance, tone, levels, curves, saturation) compose into a
   single function from proxy RGB to display sRGB, and the backend bakes that
   function into a LUT.
3. Each local adjustment is a **delta LUT in display space** plus a mask. The
   mask is computed in the shader from gradient parameters, or rasterized from
   brush strokes. They compose in order: `y = lerp(y, D_i(y), m_i)`.
4. **The export pipeline uses the same semantics**: the same order and the same
   display-space blend. It evaluates the functions directly instead of
   through a LUT. So the preview differs from the export only by LUT
   interpolation, and by mask rasterization, which is three small formulas
   kept in both Rust and GLSL and tested for parity.
5. Zooming past the proxy's resolution asks for backend-rendered `photo.tile`
   images at real resolution. While one is pending, the view says it is
   rendering.

**Libraries.** Versions are current on crates.io as of 2026-11-16.

| Need | Choice | Alternatives and why not (yet) |
| --- | --- | --- |
| Display formats | `image` 0.25.10 (MIT/Apache). The core already uses it | `zune-image` 0.5 is fast but young. AVIF decoding in `image` needs `avif-native` (dav1d, C), so it is left off |
| RAW decode | **`rawler` 0.8.0** (DNGLab; pure Rust, LGPL-2.1, released 2026-08-30, MSRV 1.89). It has `imgop::develop::RawDevelop` for development steps | **LibRaw** 0.22.x (C++, LGPL-2.1 or CDDL-1.0, 1,284 cameras) is the reference for coverage. The Rust bindings are thin and young: `rsraw` 0.1.1 (MIT) builds LibRaw with CMake and a C++ toolchain, which multiplies the Android cross-compile problem (Lane 0.5), and LibRaw has a long fuzzing-CVE history. **Keep both behind one `RawSource` trait and let the spike decide.** LibRaw can also be a cargo feature for x86_64 hosts only. `rawloader` 0.37 and `imagepipe` 0.5 (LGPL-3.0) are an older pair. rawler states its API is not SemVer-stable, so pin it exactly |
| Resampling | `fast_image_resize` 6.1.0 (MIT/Apache). SIMD is picked at run time, which matters because the R620 lacks AVX2 | `image::imageops` is slower. Rotation for straightening is not a resize: an own bicubic or Lanczos affine sampler (~200 lines), or `imageproc` |
| Color management | **`moxcms` 0.9.2** (pure Rust, BSD-3/Apache, very active) | `lcms2` 6.2 (MIT wrapper around Little CMS, C) is the reference. Use it in tests to check moxcms, not in the shipped binary |
| EXIF read | `kamadak-exif` 0.6.2 (BSD-2) | `nom-exif` 3.8 has a non-standard license, so check it before use |
| EXIF write (export) | `little_exif` 0.6.23 (MIT/Apache) | The `exiftool` binary as an optional helper (later, see Sandbox). `xmp_toolkit` wraps Adobe's C++ toolkit, which is heavy |
| Parallelism | `rayon`, inside `spawn_blocking` | — |

**GPU in the backend: no, and `gpu = false`.** The lane note says "GPU via
/dev/kfd only if real", and it is not real for this workload:

- `/dev/kfd` is ROCm compute. An image pipeline would use Vulkan through
  `wgpu` (30.0.2), which needs `/dev/dri` render nodes, not kfd.
- The R620 has no GPU (DESIGN.md).
- The Fold under Termux has no supported Vulkan for a Linux binary *(not
  verified)*.
- The laptop's iGPU is the only candidate.

The GPU that matters is the **viewer's**, through WebGL2 in the frame, and D
already uses it. Revisit backend `wgpu` only if export time turns out to be the
complaint.

**Licenses:** rawler and LibRaw are LGPL-2.1, which may be combined into an
AGPL-3.0 binary because LGPL-2.1 allows conversion to GPL. Everything else is
MIT, BSD or Apache. No AGPL-incompatible dependency.

### Backend commands

The app id is `photo`. Ids follow §11, and none collides with a core namespace
(APP-UI-ISOLATION §4.6). Every command takes a path relative to the app's
`working_dir` grant.

| id | risk | progress | what |
| --- | --- | --- | --- |
| `photo.open` | read | — | Metadata (dimensions, camera, lens, exposure, orientation, color profile), whether the decoder supports it, and the sidecar's recipe, revision and history. An unsupported RAW is refused with the camera named ("this camera's RAW is not supported by the decoder"), not a generic error |
| `photo.proxy` | read | yes, for a RAW decode (seconds *(est.)*) | `{path, long_edge≤2048}` → the unadjusted proxy (base64 PNG or lossless WebP) and its input encoding. Cached |
| `photo.lut` | read | — | `{path, recipe, size: 17\|33, scope: global\|local[i]}` → a half-float 3D LUT, base64. Needs the path only for the camera matrices |
| `photo.tile` | read | — | `{path, recipe, rect, scale}` → a rendered tile through the full export pipeline, for 1:1 zoom and for parity checks |
| `photo.preview.embedded` | read | — | The embedded JPEG from a RAW, size-capped, for Quick Look and Files thumbnails |
| `photo.describe` | read | — | Structured state for agents: the recipe as named values, the histogram summary, clipping (DESIGN.md "describe state") |
| `photo.edit.save` | write | — | `{path, recipe, base_rev}` → writes the sidecar atomically. A stale `base_rev` is refused with "edited elsewhere since you opened it", and the new revision is returned |
| `photo.export` | write | yes | `{path, recipe?, format, quality?, long_edge?, dest}` → writes a new file. **Never overwrites.** It uses `keep_both`-style naming like `files.copy`, writes under a hidden partial name, renames only when complete, and leaves no partial file when cancelled |

Notes:

- **No command is `destructive` or `outbound`.** Nothing touches the original.
  A sidecar save is undoable through the history it carries. An export creates
  a file and never replaces one. Reset is a `photo.edit.save` of an empty
  recipe, with a history entry. An agent can therefore save edits and export
  without going through the Inbox, which matches §11's definitions. The
  operator should know this.
- **Prerequisite in `deckjack-app`:** cancellation is dropped today
  (`TODO(cancel)` in `crates/deckjack-app/src/lib.rs`, at `CoreFrame::Cancel`).
  A cancellable export needs a cancellation signal in `Ctx` first. That is a
  small SDK change and a test, but it blocks the export exit check.
- Handlers already run concurrently (`tokio::spawn` per call). CPU work goes to
  `spawn_blocking`/rayon, so a 5-second export does not stall `photo.lut`.
- **Size limits:** proxy results are 1–3 MB base64, under the 8 MiB socket
  frame (`MAX_FRAME_BYTES`). `long_edge` is capped so no result can exceed it.

### UI and touch contract

The UI is plain HTML, JS and WebGL2 with no build step, served from
rust-embed. The shader is about 150 lines of GLSL inline in a JS module. One
`<canvas>` holds the image. The tool panel is a bottom sheet in compact mode
and a side column in wide mode.

**Touch:**

- **Image:** pinch zooms, anchored between the fingers. Two fingers pan. A
  double tap toggles fit and 1:1. **Press and hold shows the before.** One
  finger never paints unless a brush tool is armed. Two fingers never paint.
  - The gesture geometry is ported to a kit module (`/kit/gestures.js`)
    rather than copied, which also serves Quick Look's future needs. This is
    owed work.
- **Sliders:** the whole row is the hit target, at least `--dj-touch` high.
  Horizontal drag is coarse. Moving the finger up away from the track while
  dragging gives fine control at a smaller ratio. A double tap resets. Tapping
  the value opens numeric entry.
- **Curves:** a square editor, ≥ 280 px. Tap adds a point. A point is dragged
  through a handle offset above the finger, so the finger does not hide it.
  Dragging a point off the graph deletes it, and so does a long press. The
  readout shows input → output.
- **Crop:** corner and edge handles at least `--dj-touch`. Two-finger rotate
  straightens. The level-line tool means drawing a line along the horizon.
  Aspect chips. The image moves under a fixed frame, which works better on a
  small screen than moving the frame.
- **Masks:**
  - Gradients have on-canvas handles (center, angle and feather for linear;
    center, radii and feather for radial).
  - The brush shows a size ring under the finger and an offset preview dot.
    Size and feather are set by sheet sliders, not modifier keys.
  - A mask-overlay toggle tints the mask.
- **Keyboard (wide):** `\` toggles before/after, `C` crop, `Z` 1:1, undo and
  redo, `[`/`]` brush size.

**Surround color:** the image sits on a fixed neutral gray, whatever the
theme. The theme colors only the chrome. A tinted surround throws off color
judgment.

**Minimum width: 360 px.**

- At 360 px the image takes the top ~55% and the sheet the rest, with the
  curves editor at 300 px.
- Below 360 px the curves editor drops under 280 px, and point dragging stops
  being reliable.
- **Honest caveat:** some Fold cover screens may be narrower than 360 CSS px.
  Not verified on the device; check during the touch spike.
- Masks with a brush are usable on the phone but fiddly. That is a fact about
  fingers, not something left unfinished.

`app.toml` declares `min_width = 360`, `compact = true` and `wide = true`.

### How it fits the sandbox

The process side:

```toml
[app]
name = "photo"
title = "Photos"            # or "Develop"; must not imitate the shell
lifecycle = "on-demand"
min_width = 360

[needs]
working_dir = [{ path = "Photos", write = true }]
# gpu: not requested (see Engine). network, connect_tcp, bind_tcp: none.
```

- **No network, no ports.** Nothing in the lane talks to anything. So the
  resolver directories are not granted either.
- **Why `write = true` on `Photos/`:**
  - Sidecars must sit beside the originals to survive Deckjack, and exports
    land there by default.
  - The consequence: the process *could* overwrite an original.
  - Landlock's separate `MakeReg`/`WriteFile` rights cannot fix this. An
    atomic sidecar save is write-temp-then-rename, and a rename over a
    file needs `RemoveFile` in that directory, which would let the app
    replace an original the same way.
  - So "never writes an original" is a **tested property of the app**: every
    test checks that the original's SHA-256 is unchanged. The sandbox does
    not enforce it, and the brief says so.
- **Only one tree.** The grant is a fixed path in the manifest, so photos
  outside `Photos/` (for example `DCIM/` synced from a phone) cannot be opened.
  - Once APP-UI-ISOLATION stage 6 lands, "Open with Photos" from Files on such
    a path is refused at the broker as out of scope.
  - A manifest-level path chosen by the operator does not exist in `[needs]`.
    See Risks.
- **Cache:** the proxy and LUT cache goes in `$HOME/.cache/photo/`.
  - The launcher points `HOME` at `<install>/cache/apps/photo/home`
    (`apps.rs`, `LaunchPlan`). That is inside the install cache and safe to
    delete, which meets §11's intent.
  - §11 names `~/.deckjack/cache/<app>/`, but the path the launcher actually
    grants is `cache/apps/<app>/`.
  - The cache is capped (256 MiB, LRU) and keyed by path, size, mtime and
    decoder version.
- **Memory:** a 45 MP RAW is ~540 MB as one f32 RGB buffer *(est.)*.
  - Export processes in strips wherever the operation allows. Per-pixel
    operations stream. Geometry reads a source window.
  - A decode-memory budget like `thumbnail.rs`'s is enforced, so two exports
    cannot exhaust a phone.
- **Helper binaries:** none in v1. A later candidate is `exiftool` (Perl,
  under `/usr`, so it runs confined under the existing read+execute grant) for
  full metadata copying including maker notes. It would be feature-detected,
  because Termux may not have it.
- **seccomp:** nothing in the pipeline uses a denied syscall. rayon uses
  `clone`, which is allowed. `/dev/shm` is granted if a library uses it.
- **Android:** a backend on the Fold is blocked by Lane 0.5 (no cross-compile).
  Phone parity in v1 means **the UI on the phone, with the backend on the
  R620 or the laptop**, which decision 3 accepts for anything with a backend
  until 0.5 lands. Choosing pure-Rust rawler over LibRaw is what keeps the
  later Android build cheap.

**UI isolation.** The design assumes stage 4 of APP-UI-ISOLATION has flipped.

- **No byte route.** The frame has `connect-src 'none'` and no cookie, so it
  cannot use `/content`.
  - Every pixel arrives as a brokered command result.
  - The frame decodes it with `createImageBitmap(new Blob([bytes]))` and
    uploads a texture. That avoids `img-src` questions entirely, although
    `blob:` and `data:` are allowed anyway.
  - This is why the proxy is capped at 2048 px and the LUT is small.
- **What the broker must carry, beyond what §4.5 already specifies:**
  1. **Slider-rate traffic.** About 15–20 `photo.lut` calls a second while
     dragging.
     - §4.5's token bucket must allow that for read-risk calls, or the
       kit's `invoke` coalesces to one call in flight with the latest one
       winning.
     - **The coalescing is the recommendation**: it belongs in the app's own
       JS, and the broker limit then only has to allow ~10/s.
     - This needs stating in the broker's rate-limit test fixtures, so the
       broker does not ship a limit that makes slider-driven apps unusable.
  2. **Multi-megabyte results.** Proxies and tiles are 1–3 MB. §4.5 caps only
     *params*, at 1 MiB, and results are within the 8 MiB frame. Brush-stroke
     params are simplified (Ramer–Douglas–Peucker) client-side to stay far
     under 1 MiB.
  3. **The second socket (§4.12, stage 6)** matters here. Without it, a
     proxy transfer delays terminal echo on a phone link.
- **What D6 deferred that this lane would want (none of it blocks v1):**
  - *Media streaming / binary transfer through the broker.* Base64 inside JSON
    costs +33% plus a parse. A transferable `ArrayBuffer` result would be the
    first convenience worth asking for, once the proxy cost is measured.
  - *Downloads.* "Save to my phone's gallery" has no path. **Workaround with no
    new surface:** after an export, call `shell.quicklook.open` on the file.
    Quick Look runs in the shell and can offer the content route's
    `Content-Disposition` download. That has to be confirmed to work on iOS.
  - *Clipboard (copy image).* Not in v1.
- **WebGL2 in an opaque-origin sandboxed frame:** no CSP directive or
  Permissions-Policy in §4.2 blocks it. Context loss when a mobile tab is
  backgrounded must be handled by rebuilding textures from the cached bitmap.
  Untested on WebKit.
- **`?path=` from `shell.app.open` is inert data (§4.5).**
  - Opening it is a read.
  - The app never saves, exports or rewrites a sidecar without a user action.
    Saves are debounced on edits the user makes.
- **Workers:** a histogram or brush rasterization worker can use `blob:`
  workers, which `worker-src blob:` allows. No wasm (see Engine C).

### Formats and longevity

- **Originals are never modified.** The SHA-256 of the original is an
  assertion in every write-path test.
- **Sidecar:** `<original filename>.deckjack.json`, beside the original. For
  example `IMG_1234.CR3.deckjack.json`.
  - It does not take `IMG_1234.xmp` or `IMG_1234.CR3.xmp`. Lightroom and
    darktable own those names, and writing them would clobber another tool's
    edits.
  - Contents:
    - a schema version;
    - the original's size and SHA-256, so an orphaned or mismatched sidecar is
      detected;
    - the recipe as **named physical values**, such as
      `{"exposure_ev": 0.7, "levels": {"black": 0.02, …}}`;
    - masks as vectors (gradient parameters, brush polylines in
      normalized image coordinates with radius and feather);
    - a bounded history list;
    - the decoder name and version.
  - Two-space-indented JSON, with stable key order for diffability.
- **The spec is part of the deliverable:** `docs/formats/photo-edit-v1.md`
  defines the pipeline order, the math of each operation (curve
  interpolation, level formula, mask falloff, display-space blend) and the
  color spaces.
  - Without it, the sidecar is a proprietary format with a `.json` suffix.
  - With it, someone can reimplement the look.
- **What survives Deckjack, plainly:**
  - the originals (untouched);
  - every export (standard JPEG, PNG or TIFF with an ICC profile and EXIF);
  - the sidecars as readable documentation of intent.
- **What does not survive:** no other tool will *apply* a `.deckjack.json`.
  There is no open interchange standard for non-destructive edits:
  - darktable's XMP history is per-module binary parameters, hex or gzipped
    base64, decodable only with each module's C struct layout;
  - Adobe's `crs:` namespace is Adobe's.
  - Possible later step: an optional `crs:` crop, exposure and white-balance
    hint, written only when no `.xmp` exists. Not v1.
- **RAW color is neutral, not matched to the camera.** It is built from the
  decoder's color matrices, not Adobe DCP or camera-vendor profiles. A RAW
  will look different from the camera's JPEG or from Lightroom. The UI should
  say so once rather than let it look like a bug.

### Risks, open questions, and the spike for each

1. **LUT preview parity and latency** (the architecture bet).
   - *Spike:* implement exposure, white balance, one curve and one radial
     mask in Rust. Bake 17³ and 33³ LUTs and apply them in a WebGL2 page in
     headless Chromium (SwiftShader) and real Chromium. Compare against the
     direct Rust render.
   - *Pass:* mean ΔE2000 < 1 and p99 < 3 at 33³; 17³ while dragging may be
     worse. ≥10 LUT updates a second, R620 to phone, over today's `/ws`, to
     approximate the broker path.
   - *If it fails:* fall back to A (backend-rendered proxy, debounced) and
     accept a slower but always-correct preview.
2. **8-bit log proxy precision.**
   - *Spike:* a sky gradient from a RAW at +2 EV with a strong S-curve.
     Compare 8-bit log with dither against a 16-bit reference for visible
     banding, and record the proxy size in bytes.
   - *If banding shows:* send a hi/lo 8-bit pair, or a 10-bit-packed proxy,
     and measure the cost in bytes.
3. **RAW decoder: rawler or LibRaw.**
   - *Spike:* decode the operator's own cameras' files, plus raw.pixls.us
     samples for the same models, with both.
   - Record: supported or not, demosaic quality at 1:1 (rawler's
     `RawDevelop` demosaic algorithm is undocumented in its API docs), time
     per file on the R620, and peak RSS.
   - Choose by the operator's actual cameras, not by headline camera counts.
4. **Export memory at 45–61 MP**, and phone hosts later.
   - *Spike:* strip-processed export of the largest RAW available. Measure
     peak RSS and wall time on the R620 and the laptop.
5. **HEIC input** (phones shoot it).
   - *Question:* does the operator need HEIC in v1?
   - *Spike:* libheif-rs 3.0 (MIT wrapper; libheif and libde265 are LGPL C)
     against the system libheif on Arch and in Termux. Check the license and
     patent position, and whether the Android cross-compile is acceptable.
   - Until settled, HEIC is refused by name.
6. **The fixed `Photos/` grant.**
   - *Question:* should `[needs].working_dir` accept a path chosen by the
     operator at approval time?
   - That is a protocol change with fingerprint consequences, and it belongs
     to Lane 0 or the isolation work, not this lane.
   - Until then, the photos to edit live under `Photos/`.
7. **The broker rate limit versus sliders.**
   - Settle it in APP-UI-ISOLATION stage 4's fixtures with a slider-burst
     case: 20 calls a second for 5 s, coalesced, and nothing rejected.
8. **WebGL2 in a sandboxed frame on iOS WebKit and the Fold**, including
   context loss and 3D texture upload.
   - Fold it into the isolation doc's WebKit spike (§8): one page, one 3D LUT
     texture.
9. **Touch ergonomics at 360 px and on the Fold cover screen.**
   - *Spike:* a static mock of the curves editor, crop handles and brush
     ring, on the real devices, before any pipeline code is written.
10. **Color management correctness.**
    - Test moxcms against lcms2 on the same transforms. Exports must open
      correctly in GIMP, the phone's gallery and a browser, each with the
      embedded ICC profile.
11. **Two clients editing one photo** (phone and desktop).
    - `base_rev` refuses a stale save. Test it; it needs no spike.
12. **Untrusted input.** A RAW file is attacker-controlled input to a decoder.
    - It runs in the confined app, never in the core.
    - rawler is memory-safe Rust, but its parser can still panic: catch the
      panic and report "could not decode".
    - LibRaw, if chosen, raises the stakes. Run its decode under a timeout and
      a memory cap.

### Exit check

From the **phone** (the UI; the backend on the R620):

1. Open a RAW from the operator's own camera under `Photos/`, and a phone
   JPEG.
2. Straighten 3°, crop to 4:5, set levels, add a two-point S-curve, and add a
   radial mask that brightens a face by +0.7 EV.
3. Close the window, then reopen it from the desktop. The edits are there,
   the history undoes them, and the sidecar is readable JSON that
   `docs/formats/photo-edit-v1.md` explains.
4. Export full-size JPEG and 16-bit TIFF from the phone.
   - The original's SHA-256 is unchanged.
   - The exports open in GIMP and the phone's gallery with the right
     orientation and colors.
   - The export matches the on-screen preview at the same scale within mean
     ΔE2000 < 1.5 (measured by a test, not by eye).
5. Cancel an export halfway. No partial file remains.
6. Space-bar the RAW in Quick Look. It shows the embedded preview served by
   `photo.preview.embedded`.

### Rough size

- **Backend:** ~3.5–4.5k lines of Rust (decoder adapter, pipeline, LUT bake,
  masks, sidecar, export, cache, describe) and ~2k lines of tests, including
  parity, golden images, the original-unchanged checks and the export
  never-overwrites checks.
- **UI:** ~3–4k lines of JS and GLSL with no build step (canvas and shader,
  curves editor, crop, brush, sheet and tools, undo, histogram), plus
  `/kit/gestures.js`.
- **SDK:** cancellation in `deckjack-app` (~150 lines plus tests).
- **Format spec:** ~1 page.
- **Total:** roughly the size of Lanes 1 and 2 together *(est.)*.

**The hard part is not any single operation.** It is keeping the preview
honest. Run spikes 1–3 **before** the UI. If spike 1 fails, the brief changes
shape: the preview falls back to option A.

**Dependencies:**

- Lane 0.1 is not needed, because pixels travel as command results.
- APP-UI-ISOLATION stage 4 changes the transport, not the design. Build
  against the brokered constraints from day one (no `/content`, no wasm) so
  the flip costs nothing.
- Lane 0.5 is needed only for a backend on the Fold.

### Sources

- rawler on crates.io (0.8.0, LGPL-2.1, MSRV 1.89): https://crates.io/crates/rawler; the `imgop::develop` module: https://docs.rs/rawler/0.8.0/rawler/imgop/develop/index.html; DNGLab: https://github.com/dnglab/dnglab; rawler API stability and license: https://docs.rs/crate/rawler/0.7.0/source/Cargo.toml.orig, https://dfsg-new-queue.debian.org/reviews/rust-rawler
- LibRaw 0.22 release and camera count: https://www.libraw.org/node/2861; dual license: https://dfsg-new-queue.debian.org/reviews/libraw; Arch package 0.22.3: https://archlinux.org/packages/extra/x86_64/libraw/
- rsraw (LibRaw bindings, 0.1.1, needs CMake and C++): https://docs.rs/crate/rsraw/latest, https://github.com/hexilee/rsraw; libraw-rs: https://github.com/Kilerd/libraw-rs
- image 0.25.10 (formats, `avif-native` needs dav1d): https://docs.rs/crate/image/latest/source/CHANGES.md, https://docs.rs/crate/image/0.25.10/source/Cargo.toml
- fast_image_resize (6.1.0 on crates.io; 5.x changelog): https://docs.rs/crate/fast_image_resize, https://rust-digger.code-maven.com/src/fast_image_resize-5.0.0/CHANGELOG.md
- crates.io API, queried 2026-11-16, for the versions and licenses of moxcms, lcms2, kamadak-exif, nom-exif, little_exif, xmp_toolkit, libheif-rs, jxl-oxide, zune-image, wgpu, imagepipe and rawloader: https://crates.io/api/v1/crates/<name>
- CSP and WebAssembly (`'wasm-unsafe-eval'` required for compile/instantiate): https://bugzilla.mozilla.org/show_bug.cgi?id=1740263, https://developer.mozilla.org/docs/Mozilla/Add-ons/WebExtensions/Content_Security_Policy, https://groups.google.com/a/chromium.org/g/blink-dev/c/VeOlvhJo7Dc
- darktable XMP sidecar format (binary per-module parameters): https://docs.darktable.org/usermanual/development/en/preferences-settings/storage/, https://discuss.pixls.us/t/expand-sidecar-params-to-actual-values/24097
- In-repo: `docs/APP-SUITE.md` §2, §3, §11, §12, Lanes 0, 1, 4 and 5; `docs/APP-UI-ISOLATION.md` §2 (D6), §4.2, §4.5, §4.11, §4.12, §7.2; `docs/DESIGN.md` (R620 without GPU or AVX2, longevity rule, license); `crates/deckjack-core/src/apps.rs` (`LaunchPlan`, `GPU_READ_WRITE_*`, cache/home layout); `crates/deckjack-core/src/sandbox.rs` (seccomp denylist); `crates/deckjack-app/src/lib.rs` (`TODO(cancel)`, concurrent handlers); `crates/deckjack-protocol/src/lib.rs` (`MAX_FRAME_BYTES`).
