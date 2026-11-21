# Lane 7a — CAD viewer (scoping brief)

Scoped 2026-11-16 against the tree at `179119c` plus the working-tree changes,
`docs/APP-SUITE.md` (§2, §3, §11, §12, Lanes 5–7), `docs/DESIGN.md`,
`docs/APP-UI-ISOLATION.md` (accepted, not built), `sandbox.rs` and `apps.rs`.
Library facts are from web sources listed at the end and were not re-verified
by building anything. Where something is inferred rather than established, it
says so.

**Difficulty, plainly.** STL is a weekend. DXF is a few weeks to do honestly,
because real DXF files use blocks, hatches and dimensions. STEP is the hard
part, and it is hard because of *tessellating trimmed B-rep surfaces*, not
because of the file syntax. No Rust library does that as well as OpenCascade.
The second hard part is not CAD at all: the platform has no way to hand an app
a file the operator picked outside its granted trees (§ Sandbox), and none to
move tens of megabytes of mesh into an isolated frame except by chunked JSON.
Both are platform problems, and this lane would be the first to hit them.

---

## Goal

Open the CAD files that arrive (a vendor's STEP part, a STL for the printer, a
DXF drawing from a laser-cutting shop, KiCad's 3D models), look at them from
any angle, and **measure them correctly** from the phone or the desktop,
without a native CAD program. A viewer, not a modeler. "Correctly" means a
hole's diameter comes from the B-rep's cylinder, not from the chord of a
tessellated circle.

## The useful subset (v1)

**In:**

- **STL**, binary and ASCII. Orbit, zoom, pan, fit; bounding box in stated
  units; triangle count; point-to-point distance with vertex snapping. STL has
  no units, and the viewer says "unitless (assumed mm)" rather than inventing
  them.
- **STEP** (AP203, AP214, AP242 B-rep geometry):
  - assembly tree with part names, visibility toggles and isolate;
  - colors from styled items;
  - units from the file;
  - B-rep edges drawn over the shaded mesh;
  - **exact measurement computed on the B-rep, not the mesh**: point to
    point, edge length, face info (plane normal, cylinder or cone axis and
    radius, sphere radius, area), minimum distance between two picked
    entities, and the angle between two planes or axes.
- **DXF** (ASCII and binary), model space only:
  - LINE, ARC, CIRCLE, ELLIPSE, LWPOLYLINE/POLYLINE (with bulges), SPLINE
    (tessellated), POINT, TEXT/MTEXT (in a fallback font), INSERT (blocks,
    nested), and DIMENSION drawn from its anonymous block;
  - layers with on/off;
  - snaps (endpoint, midpoint, center, nearest) and exact distance, radius
    and angle on the entity geometry;
  - units from `$INSUNITS`, shown as "unitless" when it is 0, which is common.
- **One section plane** (a three.js clipping plane). It is cheap and is
  what makes an assembly's insides inspectable.
- **Export the tessellation**: STEP to STL or glTF into the working
  directory, never overwriting. "I need this STEP as an STL for the printer"
  is the most common reason to open a STEP at all.
- `cad.inspect` for agents: units, bounding box, part list and counts as
  structured data (DESIGN.md's "describe state").

**Explicitly not in v1:**

- **Any editing, and parametric modeling.** Lane 7 says parametric modeling
  is a separate decision, not an increment.
- **DWG.** Proprietary and undocumented. LibreDWG's `dwg2dxf` could later be
  a helper binary. It is GPL-3.0, which can be combined with this AGPL
  project; that is my reading of GPLv3 §13 and has not been checked by a
  lawyer.
- **IGES, BREP, OBJ, 3MF and glTF input.** OCCT reads IGES and BREP almost for
  free, but nothing will be promised that has not been run on a corpus.
- **AP242 semantic PMI and GD&T display.**
- **Exploded views; LOD and streaming for huge assemblies.** v1 refuses above
  a triangle budget and offers coarser tessellation instead.
- **Paper-space layouts and SHX fonts.** Text renders in a fallback font,
  and the viewer says so.
- **Annotations saved to sidecars.** See Formats; this is deferred so that v1
  needs no write grant at all.

## Engine choice

| Format | Choice | Where it runs |
|---|---|---|
| STL | `stl_io` or a ~200-line parser of our own (binary STL is 50 bytes per triangle) | in the app process |
| DXF | IxMilia `dxf` crate, 0.6.1, MIT in that release (older releases were Apache-2.0) | in the app process |
| STEP | **OpenCascade (OCCT) 7.8.1 through `occt-sys`**, behind a small C++ shim | **a separate helper binary**, `deckjack-cad-occt`, exec'd by the app per open model |

**Why OCCT for STEP.**

- It is the reference open implementation: STEP read through XCAF (names,
  colors, assembly structure), `BRepMesh_IncrementalMesh` for tessellation,
  `BRepExtrema_DistShapeShape` for exact minimum distance, and
  `BRepAdaptor_Surface` to classify faces. Every measurement in the subset
  above is a call it already has.
- `occt-sys` 7.8.1 (Aug 2026) builds a static OCCT with the `cmake` crate. It
  strips visualization and keeps the kernel and data exchange. It is LGPL-2.1,
  about 59 MB of source and about 1M lines, maintained by Brian Schwind and
  contributors, at about 26k downloads a month.
- OCCT itself is at 8.0.0 (May 2026; C++17, CMake ≥ 3.16; stream-based,
  per-instance thread-safe STEP read and write). `occt-sys` tracks 7.8.1, so
  v1 uses 7.8.1. An OCCT 8 bump is an upgrade task, not a blocker.
- License: LGPL-2.1 with the Open CASCADE Exception 1.0. Deckjack is
  AGPL-3.0-or-later, so statically linking it into an open-source binary meets
  LGPL's relinking terms by publishing the source. Compatible, per my reading.
- Using OCCT this way does not break Lane 7's "must not start with a 3D
  kernel". We call a kernel's reader, mesher and distance query; we do not
  build or extend a modeling kernel.

**Why a helper process and not in-process.** A STEP file is untrusted input to
roughly a million lines of C++.

- **Containment:** a crash or a hang kills one child, not the app. The app
  can put `RLIMIT_AS` and `RLIMIT_CPU` on the child and a wall-clock
  deadline around it. `setrlimit`/`prlimit` are not on the seccomp denylist.
- **The child is confined too:** it inherits Landlock and seccomp
  (`sandbox.rs`; APP-SUITE §2, "an app may exec a helper binary, and the
  helper is confined too").
- **Life of the helper:** it lives as long as the model is open, because
  exact measurement needs the B-rep loaded and re-parsing a 30 MB STEP per
  click is not acceptable. It is killed on close, on idle, or when the
  dispatch is dropped.
- **Protocol:** length-prefixed binary frames over stdin and stdout.

**Alternatives considered:**

- **`truck` with `truck-stepio`** (ricosjp; Apache-2.0, pure Rust).
  - `truck-stepio` 0.3.0 (Sep 2024) reads STEP through `ruststep`. Its docs
    still describe output as the mature direction.
  - meshStep's author reports truck's generic triangulation twisting trimmed
    periodic faces: 13% of the edges spiralled on a cylinder-with-cutout test.
  - The cadence is slow.
  - Good for a pure-Rust fallback spike, not as the primary engine.
- **`brepkit-io`** (Rust, a wasm target, STEP marked "Stable").
  - It is the most interesting pure-Rust option, but it is AGPL-3.0-only
    since 3.0.0 (compatible with this project) and dual-licensed
    commercially.
  - It went from 3.0.0 (7 Aug 2026) to 4.11.28 in about three months. That
    is churn, single-vendor risk, and a short track record.
  - It is the candidate for the **Android** path if OCCT will not
    cross-compile. Decide by corpus, not by README.
- **meshStep** (pure TypeScript, zero dependencies, AGPL-3.0-only).
  - It reports 97.2% of 10,000 ABC-dataset files watertight and 0.7% timing
    out at 120 s.
  - It runs in the browser, which here means inside the isolated frame. The
    file's bytes then have to reach the frame, which is the transport problem
    below.
  - It also gives mesh-only measurement unless we also build B-rep queries.
  - Worth keeping as the **benchmark** to beat, and as a no-native-code
    fallback.
- **`occt-import-js`** 0.0.23 (OCCT built to wasm, LGPL-2.1).
  - Client-side. It needs `'wasm-unsafe-eval'` added to the app frame's
    `script-src` (APP-UI-ISOLATION §4.2 does not have it), so it is a
    security-policy change, not just a dependency.
  - It also needs the bytes in the frame.
  - Rejected for v1.
- **Formlabs `foxtrot`** (Rust, MIT/Apache).
  - A proof of concept, self-described as not industrial. It fails some
    surfaces and has had no visible activity for about two years.
  - Its CDT-per-surface approach is instructive and nothing more.
- **`opencascade` (the high-level crate on `occt-sys`).**
  - Its API wraps modeling more than XCAF reading and measurement. A ~600–900
    line C++ shim of our own over `occt-sys` is smaller than bending it,
    though that is inferred, not tried.

**Renderer.** three.js (r18x; npm `latest` was 0.18x and sources disagree on
the exact number, so pin a version during the spike). Vendor
`three.module.min.js` into `ui/vendor/`. Draw with tokens: the scene reads the
shell's CSS custom properties for background, edge and highlight colors.

- **No-build-step trap:** three.js addons import the bare specifier `'three'`.
  That needs an import map, and an inline `<script type="importmap">` is
  blocked by D4 (no inline scripts) and the frame CSP.
- **Fix:** rewrite that one import to a relative path when vendoring, with a
  committed and noted patch. Or write our own camera controls; touch needs
  custom gestures anyway, so we likely want our own.

DXF is drawn with Canvas2D from our own renderer (no library). That keeps
snapping exact against entity geometry.

## Backend commands

App name `cad` (not a reserved core namespace under APP-UI-ISOLATION §4.6).
Ids follow §11. All `read` unless stated.

| id | risk | what | progress |
|---|---|---|---|
| `cad.open {path, quality?}` | read | Parse and tessellate. Returns `{model, kind, units, bbox, counts, tree, chunks, warnings}`. A cache hit returns at once. | yes: "reading", "meshing part n/N", fraction where known |
| `cad.mesh.chunk {model, index}` | read | One piece of the encoded mesh or the DXF entity stream (see Transport). | no |
| `cad.face.info {model, face}` | read | Surface type and parameters: plane origin and normal, cylinder axis and radius, and so on; also area. STEP only. | no |
| `cad.measure {model, a, b}` | read | Exact distance, angle or edge length between picked entities (`{vertex|edge|face, id}` or a point). B-rep for STEP, entity geometry for DXF, mesh for STL (labeled approximate only where it is). | no |
| `cad.inspect {path}` | read | For agents and scripts: units, bbox, part names, counts. No mesh. | yes |
| `cad.close {model}` | read | Drop the model and kill its helper. Also happens on idle and when the window closes. | no |
| `cad.export {model, format: "stl"\|"glb", path}` | **write** | Writes a new file. Never overwrites: refused if taken; `keep_both` is the same rule as `files.copy`. Writes under a hidden partial name, then renames. Needs a write grant (see Sandbox), so it may slip past v1. | yes |

- No `destructive` or `outbound` command exists. Nothing leaves the machine
  and nothing is unrecoverable.
- Cancellation is the existing dispatch drop: dropping `cad.open` kills the
  helper.
- Errors name the thing to fix, for example:
  - "`Downloads/` is not one of the folders CAD can read; open it from
    `CAD/` or grant `Downloads` in its approval";
  - "this STEP file uses a surface type OCCT could not mesh: face 412 was
    skipped, and its part is shown with a hole";
  - "the file declares no units (DXF `$INSUNITS = 0`)".

**Transport (the honest part).**

- Mesh bytes cannot use the content route. The frame has
  `connect-src 'none'`, `/content` is cookie-gated, and D6 put byte streaming
  through the broker out of v1.
- Today's limits: app socket frames ≤ 8 MiB (`MAX_FRAME_BYTES`); broker
  params ≤ 1 MiB, and results look uncapped beyond the frame limit.
- So `cad.open` returns a chunk count, and `cad.mesh.chunk` returns
  base64-encoded, deflated buffers of about 4 MiB raw each, comfortably under
  8 MiB after base64.
- Encoding: positions quantized to 16 bits per axis within the part's bbox (6
  bytes per vertex), oct-encoded normals (4 bytes), `u32` indices (12 bytes
  per triangle), and edges as polylines with B-rep ids.
- The frame inflates with the browser's native `DecompressionStream`. No wasm
  and no build step; that it works in an opaque-origin frame is inferred, not
  tested.
- Rough cost:
  - one million triangles is about 17 MB raw;
  - deflate might halve that (a guess until measured);
  - base64 then adds a third;
  - so about 3 chunks.
- That is fine on desktop and tolerable on the phone. The spike measures it.
- If it is not tolerable, this lane is the concrete case for revisiting D6
  with a narrow broker byte channel.

## UI and touch contract

- **3D viewport:**
  - one-finger drag orbits;
  - two-finger pinch zooms about the midpoint, and a two-finger drag pans;
  - double tap fits the view, or focuses the tapped part;
  - view cube or snap buttons for standard views;
  - all of it is pointer events, with one code path for mouse and touch.
- **Picking on touch:**
  - fingers are imprecise, so a pick shows a **loupe** offset above the
    finger;
  - it snaps within a radius to vertex, then edge, then face, in that
    order;
  - the snapped entity is highlighted before lift, and lifting commits it.
  - Measure is a mode (like Files' selection mode), not a modifier: tap A,
    tap B, read the result.
- **Measurement results:** large type in the stated units, with a units
  toggle (mm/in). Results stay selectable text. The async Clipboard API is
  blocked by the frame's `clipboard-write=()`. Native long-press copy
  probably still works, but that is unverified.
- **Model tree:** a drawer, with an eye toggle per part and "isolate".
  Targets are at least `--dj-touch`.
- **DXF:** the same pan and zoom; layers in the drawer; the same snap and
  loupe.
- **Minimum width, honestly:** **320 px works for viewing** with the drawer
  closed. Measurement works at 320 but is slow, because of the loupe and
  zooming in to separate dense edges. **The tree and the viewport side by
  side need about 720 px.**
  - Manifest: `min_width = 320`, `compact` layout = viewport plus drawer;
    `wide` = tree pane plus viewport plus measurement panel.
- **Phone limits that are not ours to fix:**
  - iOS Safari caps WebGL memory and loses the context when the app is
    backgrounded, so the viewer must rebuild GPU buffers on
    `webglcontextrestored` from its CPU copy;
  - a 5M-triangle assembly will not be pleasant on a phone whatever we do.

## How it fits the sandbox

```toml
[app]
name = "cad"
title = "CAD"
lifecycle = "on-demand"
min_width = 320
tag = "Cd"
summary = "View and measure STEP, STL and DXF"

[needs]
# Read-only trees. The whole working directory CANNOT be granted: apps.rs
# refuses any grant that contains `.deckjack/` ("hidden.starts_with(&real)").
working_dir = [
  { path = "CAD", write = false },
  { path = "Downloads", write = false },   # operator's choice at approval
]
# gpu = false   — rendering is the browser's WebGL; the backend never draws.
# no network, no connect_tcp, no bind_tcp.
# invoke_commands = false — it serves its commands; it calls nothing.

[ui]                                        # APP-UI-ISOLATION §4.8, once built
invoke = []        # only its own cad.* commands; nothing in files.*
events = []
```

- **The file-access gap.** This is the most important sandbox finding.
  - An app reaches only the named subtrees, and the working-directory root
    can never be one of them.
  - So "open with CAD" from Files on `Projects/robot/arm.step` fails unless
    `Projects` was granted, and each widening re-asks for approval
    (fingerprint).
  - Two ways out:
    1. **Fixed trees.** That is v1's answer, and it works today.
    2. **A powerbox** (proposed, not designed): on a user gesture in Files
       or Quick Look, the core opens the one file and passes the descriptor
       over the app's Unix socket with `SCM_RIGHTS`. That is a per-file,
       per-gesture grant that Landlock does not need to know about.
  - (2) is a Lane 0 item that 5a (photo), Lane 6 (office) and 7b (KiCad)
    will all want. I recommend raising it now rather than letting each lane
    grant `Downloads`.
- **Helper binary.**
  - `deckjack-cad-occt` lives in the app's install directory. That directory
    is read-granted, and Landlock's read set includes Execute, so it can be
    exec'd.
  - The install pipeline ELF-checks only the main binary (`install.rs`
    `Source.binary`), so a helper built for the wrong architecture installs
    and then fails at runtime. A check of every ELF in the directory is owed
    (small).
  - D9 applies twice: the helper is code the approval does not cover until
    signing covers the whole directory.
- **Confinement compatibility (inferred, needs the spike):**
  - OCCT reads `/proc/self` (granted), `/proc/meminfo` and `/proc/cpuinfo`
    (granted), and uses threads (`clone` allowed).
  - `occt-sys` builds without TBB, so there is no extra runtime.
  - Nothing on the denylist (`ptrace`, `bpf`, `perf_event_open`, `mount`, …)
    should be touched by a STEP reader.
  - Writes go only to the app's private `HOME` and `TMPDIR`.
- **Cache.** §11 says `~/.deckjack/cache/<app>/`. The launcher grants an app
  only `cache/apps/<app>/{home,tmp}`, so the mesh cache lives at
  `$HOME/.cache/cad/`. That is under `cache/apps/cad/home`, safe to delete,
  keyed by SHA-256 of (resolved path, mtime ns, length, quality, engine
  version), with a size cap and LRU. The same rules as the thumbnail cache.
  §11's wording and the launcher disagree; the launcher wins.
- **UI isolation:**
  - The frame needs only its own commands, so `[ui] invoke` stays empty and
    D1 does not force an approval prompt on that account. `working_dir`
    already does.
  - Nothing from `system.*`, `terminal.*` or `files.*`.
  - Broker ops it uses: `invoke`/`cancel` for `cad.*`, and progress events
    for its own calls.
  - Broker ops it would like:
    - `shell.app.open` (to "open fully" from Quick Look into CAD);
    - `shell.quicklook.open` (not needed).
- **What D6 deferred that it would want:**
  1. **Byte streaming.** Chunked commands work around it, at some
     performance cost (measure it).
  2. **Downloads.** "Save STL to my phone" is not possible, so export lands
     in the working directory. That is better for longevity anyway.
  3. **Clipboard.** Copy a measurement; selectable text is the workaround.
- **WebGL in the frame.** The isolation CSP and Permissions-Policy do not
  restrict WebGL. In Chromium, WebGL works in an opaque-origin
  `allow-scripts` frame; WebKit and the iOS PWA are untested here.
  `worker-src blob:` would allow a mesh-decoding worker if one is needed.
- **WebRTC (§7.2 item 3).** A CAD viewer holding read access to `CAD/` can
  exfiltrate proprietary designs over WebRTC like any other app. Read-only
  scope does not help there. Stated, not solved.

## Formats and longevity

- **The viewer owns no data.** It reads STEP, STL and DXF, which are open
  (ISO 10303-21; de facto STL; DXF documented by Autodesk), and never writes
  to them.
- **Exports** are STL and binary glTF (`.glb`), both open. A STEP export of a
  modified model does not exist because nothing is modified.
- **Cache** is derived and disposable.
- **Sidecars (v2, if annotations are wanted).** Saved views and measurements
  as JSON beside the file: `arm.step.cad.json`, holding camera poses and
  measurements referenced by B-rep entity *and* by world coordinates. OCCT's
  face and edge ids are not stable across re-export from the authoring tool,
  so coordinates are the fallback that survives. Plain JSON, documented, and
  readable without Deckjack. It needs a write grant, which is why it is not
  in v1.
- **What survives Deckjack:** everything. The originals were never touched
  and the exports are standard.

## Risks and open questions, and the spike that settles each

1. **Will OCCT build and run confined on every target?**
   - Spike: build a 100-line `deckjack-cad-occt` on `occt-sys` 7.8.1 for
     x86_64, aarch64-linux-gnu and aarch64-linux-android (cargo-ndk, NDK
     r27+).
   - Run it under the real launcher confinement against one STEP.
   - Record build time, static binary size and peak RSS.
   - Android is the likeliest failure: there is no current official guide,
     only OCCT 7.4-era docs and community reports of FreeType path
     problems.
   - Lane 0.5 (no cross-compile) blocks hosting on the Fold either way.
     Using it *as a client* from the phone is unaffected.
2. **Fidelity across real files.**
   - Spike: a corpus of about 60 STEP files:
     - KiCad 3D models (which also feeds 7b);
     - vendor parts from McMaster and Misumi;
     - exports from Fusion, Onshape, SolidWorks, FreeCAD and Inventor, in
       AP203, AP214 and AP242;
     - 20 from the ABC dataset;
     - three large assemblies.
   - Run OCCT, `brepkit-io`, `truck-stepio` and meshStep. Score parse
     success, faces failed, time, peak memory and watertightness.
   - This decides the Android engine, and whether OCCT is worth its weight
     at all.
3. **Exact measurement correctness.**
   - Spike: for 10 parts with known drawings, compare `cad.measure` and
     `cad.face.info` against the drawing dimensions to 0.01 mm.
   - Mutation-check that a mesh-chord answer would fail the test (a hole of
     Ø10 meshed coarsely reads about 9.9).
4. **Transport cost.**
   - Spike: push 5, 20 and 60 MB encoded meshes through `cad.mesh.chunk`.
     Do it once over today's direct socket and once through a stub broker
     (MessagePort hop).
   - Measure p50 and p95 to first frame and to complete, on desktop and on
     the phone over Tailscale.
   - Measure terminal echo during the transfer (the APP-UI-ISOLATION §4.12
     second-socket question).
   - Settles whether D6 must be reopened for bytes.
5. **WebGL in an opaque-origin sandboxed frame on WebKit and on the Fold.**
   - Spike: a 50-line probe app.
   - Check context creation, the memory ceiling with a 2M-triangle mesh,
     context loss on backgrounding and restore, and pinch gestures not
     being stolen by the shell.
   - It should ride the WebKit spike APP-UI-ISOLATION §8 already requires.
6. **Hostile input.**
   - Spike: fuzz the helper with mutated STEP (cargo-fuzz on the shim's
     entry point, plus truncated and enormous files).
   - Check that `RLIMIT_AS`, `RLIMIT_CPU` and the wall deadline kill it
     cleanly, and that the app reports the error rather than hanging.
   - DXF and STL parsers get the same treatment in-process; they are Rust
     but can still allocate without bound. Apply the thumbnail module's
     budget rule: reserve before allocating.
7. **The file-access gap.**
   - Not a spike but a decision: fixed trees for v1, and a powerbox
     descriptor-passing design written as a Lane 0 item.
   - The open question is whether the operator accepts granting `Downloads`
     to a CAD viewer in the meantime.
8. **Touch precision for measurement.**
   - Spike: a paper prototype of the loupe and snap order on the phone.
   - Measure a 2 mm feature on a 300 mm part without zooming into the
     wrong edge.
9. **DXF reality.**
   - Spike: 20 DXFs from AutoCAD, LibreCAD, Inkscape, QCAD, KiCad export and
     laser-shop files, through the `dxf` crate.
   - Count unknown entities, broken hatches and missing block references.
   - The crate's handling of AC1032 (2018+) files is unverified.
10. **Vendoring three.js without a build step.**
    - Trivial spike: confirm the relative-import patch works under the
      frame CSP, and pin the version.

## Exit check

From the phone, and again from the desktop:

1. Open a vendor STEP assembly of at least 20 MB and 50 parts from `CAD/`.
   It opens with progress, and the tree shows part names and colors.
2. Orbit, isolate a part, and section it.
3. Tap a hole and read its diameter. Tap two parallel faces and read their
   distance. Both match the vendor's drawing to 0.01 mm.
4. Export that part to STL into `CAD/`, and confirm the STL opens in a
   slicer.
5. Open an STL and read its bounding box.
6. Open a DXF floor plan or laser drawing, toggle layers, and measure an arc
   radius and a wall length against the drawing.

Checked on disk and against the drawing, not by the command's status. A STEP
file whose faces partly failed shows the failure in the UI rather than a
silently holed model.

## Rough size

- **Backend (Rust app), ~3–4k lines:** STL and DXF parsing glue, the DXF
  entity model and snapping geometry, mesh encoding and chunking, the cache,
  commands, the helper supervisor (spawn, rlimits, deadline, frames).
- **C++ shim and helper, ~700–1,000 lines:** XCAF read, meshing, face
  classification, distance and angle queries, STL and glTF export.
- **UI (plain JS), ~3.5–4.5k lines:** scene and tokens, touch camera,
  picking with loupe and snap, measurement panel, model tree, section plane,
  the DXF Canvas2D renderer, context-loss recovery.
- **Tests, ~2.5–3k lines:**
  - Rust behavior tests against a small committed corpus: exact radii,
    distances, units, refusal cases, and a helper killed by its limits;
  - JS tests for snap order, quantization round-trip error bounds and chunk
    reassembly;
  - one wiring test that the app's commands are constructed and answered
    end to end.
- **Total:** about 10–12k lines over 3–5 lane passes:
  1. STL and the viewer shell;
  2. DXF;
  3. STEP through OCCT;
  4. measurement and export;
  5. phone polish.
- **Not counted:** the spikes, which come first.
- **Platform work this lane would pull, and depends on but does not own:**
  - the powerbox (Lane 0);
  - Lane 0.5 cross-compile (to host on the Fold);
  - the APP-UI-ISOLATION flip (to ship isolated);
  - possibly a D6 byte channel.

---

## Sources

- truck repository (Apache-2.0, crate list): https://github.com/ricosjp/truck
- truck-stepio 0.3.0 (2024-09-20, reads/writes STEP via ruststep): https://docs.rs/crate/truck-stepio/latest
- truck-stepio older docs ("input will come further down the road"): https://docs.rs/truck-stepio
- OCCT 8.0.0 release (7 May 2026; C++17; STEP changes): https://occt3d.com/dev/content/open-cascade-technology-800-release/index.html
- OCCT repository, LGPL-2.1 + exception files: https://github.com/Open-Cascade-SAS/OCCT
- Open CASCADE LGPL 2.1 Exception 1.0 text: https://android.googlesource.com/platform/external/gentoo/overlays/gentoo/+/brillo-m8-release/licenses/Open-CASCADE-LGPL-2.1-Exception-1.0
- OCCT LGPL relicensing announcement: https://dev.opencascade.org/content/open-cascade-technology-changes-its-license-gnu-lgpl
- occt-sys 7.8.1 (Aug 2026, static CMake build, LGPL-2.1): https://lib.rs/crates/occt-sys
- OCCT Android build guide (7.4-era): https://dev.opencascade.org/doc/occt-7.4.0/overview/html/occt_dev_guides__building_android.html
- brepkit-io (Rust B-rep I/O, STEP stable, AGPL-3.0-only from 3.0.0): https://docs.rs/crate/brepkit-io/3.0.0
- meshStep (TS STEP mesher, ABC-dataset results, truck comparison, AGPL-3.0-only): https://cdn.jsdelivr.net/npm/meshstep@0.1.1/README.md
- occt-import-js (OCCT wasm, LGPL-2.1, 0.0.23): https://www.jsdelivr.com/package/npm/occt-import-js and https://cdn.jsdelivr.net/npm/occt-import-js@0.0.23/README.md
- Foxtrot (proof of concept, Apache/MIT): https://github.com/Formlabs/foxtrot and https://mattkeeter.com/projects/foxtrot
- IxMilia dxf crate 0.6.1 (MIT in Cargo.toml): https://docs.rs/crate/dxf/0.6.1/source/Cargo.toml and https://github.com/ixmilia/dxf-rs
- WebAssembly CSP (`wasm-unsafe-eval`) proposal: https://github.com/WebAssembly/content-security-policy/blob/main/proposals/CSP.md and Mozilla's note that wasm needs it in `script-src`: https://developer.mozilla.org/docs/Mozilla/Add-ons/WebExtensions/Content_Security_Policy
- three.js versions (sources disagree; 0.184.0 latest per registry snapshot, unpkg shows 0.187.0): https://registry.npmjs.com/three and https://app.unpkg.com/three@0.187.0/files/README.md

Repository facts cited from the tree, not the web: whole-working-dir grant refused
(`crates/deckjack-core/src/apps.rs`, hidden-directory check in confinement
planning); private `home`/`tmp` grants (`apps.rs`, same function);
`MAX_FRAME_BYTES = 8 MiB` (`crates/deckjack-protocol/src/lib.rs`); seccomp
denylist (`crates/deckjack-core/src/sandbox.rs`, `denied_syscalls`); install
ELF check on the main binary only (`crates/deckjack-core/src/install.rs`,
`Source`); frame CSP, D4, D6 (`docs/APP-UI-ISOLATION.md` §2, §4.2).
