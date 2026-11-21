## Lane 5a — Photo editor

Scoping brief, 2026-11-16. Research only: nothing here is built and nothing was
measured. Crate versions come from the crates.io API on this date. Timings and
sizes marked *(est.)* are inferred, not measured.

**Exists:** nothing. Related pieces: `thumbnail.rs` in the core decodes
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
