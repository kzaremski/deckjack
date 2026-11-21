# Lane 5b — Vector editor (scoping brief)

Status: research brief, not built. Crate facts were checked against the
crates.io API and upstream repositories on 2026-11-16 (sources at the end).
Statements about Deckjack cite files in this repo. Where something is a
judgment or a guess, the brief says so.

## Goal

Open, draw and edit SVG drawings in the working directory, from the phone and
the desktop, with the operations that make a vector editor more than a shape
toy: Bézier paths with node editing, boolean operations, text on a path, and
honest export. **The file on disk is plain SVG that opens unchanged in
Inkscape and every browser.** Deckjack adds nothing to it that another tool
needs in order to read it.

This is not an Illustrator or Inkscape competitor. The target is roughly
"Inkscape for the 80% of edits that happen to a logo, an icon, a diagram or a
poster", done well on touch.

## The useful subset (v1)

**In:**

- Open and save `.svg` under the app's directory (see "How it fits the
  sandbox" for why this is a directory and not anywhere).
- Pan and zoom: two-finger pinch and pan, wheel, `+`/`-`/`0`, fit to page.
- Select, move, scale, rotate with on-canvas handles. Numeric x, y, w, h and
  rotation in a property sheet, which is how precision works on a phone.
- Shapes: rectangle (with corner radius), ellipse, line, polygon/star.
- Pen tool (cubic Béziers): tap for a corner, drag for a smooth node, tap the
  first node to close, a **Done** button to finish an open path.
- Pencil (freehand), simplified to Béziers on lift.
- Node editing: move nodes and handles, add, delete, toggle corner and
  smooth, break and join.
- Style: flat fill and stroke color, opacity, stroke width, dash, caps and
  joins. Existing gradients, patterns, filters, masks and clips are **shown
  and preserved, not edited**.
- Arrange: z-order, group and ungroup, duplicate, align and distribute,
  snapping to a grid and to nodes (one toggle each).
- Layers: Inkscape-compatible layers (`<g inkscape:groupmode="layer">`) with
  show, hide, lock, rename, reorder.
- Text: point text and multi-line text (`<text>`/`<tspan>`); **text on a
  path** by attaching text to a selected path (`<textPath>`, start offset,
  side).
- Path operations: union, difference, intersection, exclusion; outline
  stroke; offset (inset/outset); convert object to path; convert text to
  outlines; simplify.
- Place a raster image that already lives in the app's directory (linked by
  relative path, or embedded as `data:` on request).
- Undo and redo for the session; copy and paste inside the app.
- Export: PNG at a chosen scale or pixel width, and "plain SVG" (text as
  outlines, CSS resolved, for tools that choke on the original).
- Unsaved work survives a reload (drafts kept by the backend, see below).

**Explicitly not in v1** (each is a later decision, not an increment that
quietly lands):

- Gradient, pattern, filter, mask and clip **editing** (they render and
  survive a save).
- PDF export (see Risks: the obvious library is archived), and any import
  other than SVG: no PDF, AI, EPS, DXF, CDR, Figma.
- Non-destructive ("live") boolean operations, symbols and `<use>` editing,
  CSS stylesheet editing, multiple pages or artboards.
- Fonts from the working directory; the v1 font set is embedded (see below).
- Pressure, brushes, calligraphy, image tracing, mesh gradients, perspective,
  animation, scripting, collaboration.
- Copy and paste between Deckjack apps or to and from other programs. The
  broker carries no clipboard (APP-UI-ISOLATION D6).
- SVG 2 `shape-inside` text flow: no browser renders it, so it cannot be
  shown on the canvas.
- Byte-for-byte preservation of formatting. Untouched elements keep their
  exact attribute text; whitespace between elements may change.

## Engine choice

### The split: the browser draws, Rust does geometry and export

**The live canvas is the browser's own SVG renderer, in the app's frame.**
The document is parsed with `DOMParser` (`image/svg+xml`) and mounted inside a
**shadow root** in the app's page, so the drawing's own `<style>` cannot
restyle the editor's UI (an inline `<svg>`'s stylesheet otherwise applies to
the whole document). Drag, hit-testing, handles and snapping run in plain JS
at frame rate with no round trip. The browser's DOM is also, for free, a
model that preserves unknown elements, foreign namespaces (Inkscape,
Sodipodi, Illustrator) and attribute order, which is the single hardest
thing for a vector editor to get right.

**Discrete geometry and export run in the app's Rust backend,**
`deckjack-vector`, per decision 1 ("in-process engines, our UI"). These are
user-initiated, one-shot operations (press Union, lift the pencil), so a
brokered round trip of tens to low hundreds of milliseconds is acceptable
(this needs measuring from a phone; see Risks). Putting them in the backend
also makes them agent tools on the day they ship, and needs no change to the
app-frame CSP (below).

| Job | Engine | Status (2026-11-16) |
| --- | --- | --- |
| Path math: parse `d`, stroke expansion (outline stroke), offset, simplify/fit (pencil), area, bounds, nearest point | **`kurbo` 0.13.1** | Linebender; Apache-2.0 OR MIT; pure Rust; already a `usvg` dependency |
| Boolean ops | **`linesweeper` 0.5.0**, chosen by spike over the alternatives below | MIT OR Apache-2.0; on `kurbo` 0.13; README says **"early beta"**; GitHub repo archived, development moved to Radicle |
| SVG parsing for checks, text layout, text-to-outline, plain-SVG export | **`usvg` 0.50.0** | Linebender; Apache-2.0 OR MIT; released 2026-11-11; text via `harfrust` 0.15, `skrifa` 0.49, `fontdb` 0.24 (optional `text` feature) |
| PNG export, thumbnails | **`resvg` 0.50.0** (CPU, `tiny-skia` 0.12, BSD-3-Clause) | same project; no GPU needed |
| Fonts | `fontdb` 0.24 loaded **only from fonts embedded in the binary** | MIT |

Everything in that table is pure Rust with no C dependencies, so it should
cross-compile to `aarch64-linux-android` once Lane 0.5 exists (inferred from
the dependency lists; not built).

### Alternatives and why not

- **Boolean ops, other engines.** All four candidates go into the spike:
  - `flo_curves` 0.8.1 (Apache-2.0): Bézier-native booleans, actively
    released; robustness on coincident and tangent edges is the known worry
    with this class of algorithm (not tested here).
  - Graphite's `path-bool` (port of PathBool.js; union, difference,
    intersection, exclusion, division, fracture; NonZero and EvenOdd). Not
    published on crates.io; it would be vendored from the Graphite repo.
    Only a mirror was consulted for this brief, so its current state and
    license are to be confirmed against the official repository.
  - `i_overlay` 9.2.0 (MIT OR Apache-2.0): fast, robust, very widely used,
    but **polygon-only**. Curves would be flattened and then refit with
    `kurbo`'s fitting, which drifts and adds nodes. It is the fallback if
    no Bézier-native engine survives the corpus.
- **`lyon`** (named in the lane text). It is a tessellator for GPU
  rendering. With the browser drawing the canvas there is nothing to
  tessellate, and lyon has no boolean ops. **Not used in v1.** This deviates
  from the lane text, deliberately.
- **Geometry as wasm inside the frame.** This would keep everything local
  and remove the round trip. Rejected for v1 for three reasons:
  - the app-frame CSP in APP-UI-ISOLATION §4.2 has no `'wasm-unsafe-eval'`,
    so `WebAssembly.instantiate` would be blocked; adding it is a change to
    an accepted design;
  - a wasm artifact breaks the no-build-step rule unless it is checked in,
    as the CodeMirror bundle is;
  - agents could not call the operations.

  Revisit if live boolean previews are wanted later.
- **Rendering the canvas with `resvg` and streaming tiles.** Dragging would
  cost a round trip per frame, and DESIGN.md rules out streamed UI. Rejected.
- **Wrapping Inkscape** (the binary is a helper the sandbox would allow).
  Rejected by decision 1 for the editor. Batch-only use is plausible, for
  example a later PDF export, but Inkscape would have to be installed on the
  node and it cannot run on Android.
- **Graphite as an engine.** Apache-2.0 and a Rust vector editor, but its
  model is a node graph with its own document format; adopting it would
  bring in a second UI or a proprietary project file. Rejected; its
  boolean library is a candidate (above).
- **A Rust XML DOM as the document of record** (`roxmltree` is read-only;
  `usvg` is a normalized render tree that throws away structure). Keeping
  the model in the browser avoids writing a preserving XML DOM in Rust.
  The cost is that the backend sees the document only as text it is handed,
  or by reading the file from disk.

## Backend commands

The ids follow §11. **Every geometry command is pure**: it takes path data
and returns path data, touches no file, and so is `read`. The backend is
`deckjack-vector`; the UI reaches these through the broker in its own scope,
so it needs no `[ui] invoke` for them.

| id | risk | what | progress / cancel |
| --- | --- | --- | --- |
| `vector.path.boolean` | read | `{op: union\|difference\|intersection\|exclusion, paths: [d...], fill_rule}` → `{d}` or an error naming the degenerate input | cancel honored between subpaths |
| `vector.path.outline_stroke` | read | `{d, width, join, cap, miter, dash?}` → filled `d` (`kurbo::stroke`) | — |
| `vector.path.offset` | read | `{d, distance, join}` → `d` | — |
| `vector.path.simplify` | read | `{points or d, tolerance}` → `d` (pencil and "Simplify") | — |
| `vector.text.outline` | read | `{svg_fragment, font_family}` → paths, laid out by `usvg` with the embedded fonts; this covers `<textPath>` | — |
| `vector.doc.open` | read | `{path}` → `{text, version}` (content hash, the `editor.save` pattern); refuses a file over the cap with a sentence | — |
| `vector.doc.check` | read | `{path}` → features `resvg` will not render, external references, missing fonts, size and node count; shown as a banner | progress for a large file |
| `vector.doc.describe` | read | `{path}` → layers, top-level objects, ids, bounding boxes; the DESIGN.md "describe state" command for agents | — |
| `vector.doc.asset` | read | `{path, href}` → bytes of a linked raster image under the app's directory, which the UI turns into a `blob:` URL | — |
| `vector.doc.save` | write | `{path, text, version, force?}` → `{status: saved\|conflict, version}`; refuses a conflict as an answer, not an error, like `editor.save` | — |
| `vector.export` | write | `{path, format: png\|plain-svg, scale\|width, area: page\|drawing\|selection, out?}`. Reads the saved SVG **from disk** itself, so the output never crosses the browser. **Never overwrites:** a taken name becomes "name 2.png" | `Ctx::progress` for the render and the write; cancel drops the partial file |
| `vector.draft.put` / `.get` / `.drop` | write / read / write | an unsaved document kept under the app's private home (`cache/apps/vector/home/drafts/`), keyed by path and base version | — |

Why `vector.doc.save` is `write` and not `destructive`: it is the same act as
`editor.save`, guarded the same way (a version hash, refused on conflict).

**Why drafts are a backend command.** After APP-UI-ISOLATION stage 4,
`localStorage` throws in the frame, and per-app storage through the broker
is deferred (D6). The app's private `home` is already granted read-write
(`apps.rs`, the launcher's private dirs). §11 says caches must be safe to
delete; drafts are not a cache, so they live in `home/`, not `tmp/`, and
deleting them loses unsaved work. Say so in the UI.

## UI and touch contract

- **Two fingers always navigate; one finger acts with the current tool.**
  There are no exceptions; this is the rule that makes a canvas usable on
  glass.
- **Handles:** drawn about 8–10 px, but hit-tested at `--dj-touch` radius.
  When targets overlap, the nearest node wins over a handle, and a handle
  wins over the object.
- **Selection:** a tap selects. A **Multi** toggle in the toolbar replaces
  Shift, because a phone has no Shift. Marquee selection is a drag on empty
  canvas with the Select tool.
- **Long-press on an object opens the context menu** (arrange, group, path
  ops, convert). Unlike Files, the canvas needs no separate selection mode,
  so the contract can be honored here.
- **Precision:** the property sheet has numeric fields and a nudge pad, so
  nothing requires pixel-accurate dragging.
- **Pen and stylus:** `pointerType === "pen"` gets a small hit radius and
  hover. While the pen is drawing, touch input only pans (palm rejection).
  Pressure is ignored in v1.
- **Explicit buttons for keyboard-only acts:** Undo, Redo, Done (finish the
  path), Delete, Save. Keyboard shortcuts exist on desktop too.
- **Text entry** pads for the on-screen keyboard via `visualViewport`, as
  the editor does.
- **Layout:** the canvas is always full-bleed. Tools sit in one bottom row
  on compact and a left rail when wide. Layers and properties are bottom
  sheets on compact and side panels when wide.

**Honest minimum width: 360 px**, at which it is a touch-up and
simple-drawing tool: open, move, recolor, a boolean or two, fix a node,
place text on a path. **Real drawing wants about 600 px or more** (the Fold
unfolded, a tablet, desktop), where the layers and properties panels can
stay open beside the canvas. Manifest: `min_width = 360`,
`default_width = 960`, `default_height = 700`.

## How it fits the sandbox

```toml
[app]
name = "vector"           # not a reserved core namespace (APP-UI-ISOLATION §4.6)
title = "Vector"
tag = "Ve"
summary = "Draw and edit SVG"
lifecycle = "on-demand"
min_width = 360
default_width = 960
default_height = 700

[needs]
working_dir = [{ path = "Drawings", write = true }]
# no gpu, no network, no connect_tcp/bind_tcp, no invoke_commands
```

- **Files:** only `Drawings/`, read-write. The backend reads documents and
  linked images there and writes saves and exports there. `usvg`'s resource
  directory is set to the document's folder, and Landlock is the backstop
  if an `href` climbs out (`../../.ssh/…` yields `EACCES`, which is reported
  as a missing image).
- **Network:** none. `usvg` loads no remote resources. Remote `href`s are
  reported by `vector.doc.check`, and in the frame they are blocked by
  `img-src`.
- **GPU:** none. `resvg` is CPU-only. A 2048 px PNG export is
  sub-second-to-seconds work (estimate, not measured).
- **Helper binaries:** none in v1. A later PDF export could exec
  `rsvg-convert` or `inkscape` if present under `/usr/bin` (readable and
  executable under the current grants, and confined as a child). That is
  feature-detected and absent on Android.
- **Fonts:** a small OFL set (for example a sans, a serif and a mono, Latin
  and Cyrillic, low single-digit MB) is embedded in the binary. It is
  served to the frame from the cap path as `@font-face` (`font-src` allows
  the app's own path) and loaded into `fontdb` in the backend. **The canvas
  and the export then use the same glyphs.** System fonts are not used, even
  though `/usr/share/fonts` is readable: the phone's browser does not have
  the node's fonts, so the canvas would disagree with the export.
- **Hostile input:** an SVG from anywhere is parsed by `usvg` in a confined
  process. The sandbox does not limit memory (`sandbox.rs`: "Neither layer
  constrains CPU, memory…"), so the backend enforces its own caps: an input
  cap (proposed 32 MiB), a render-area cap (proposed 100 megapixels), and a
  nesting and element-count cap before rendering.

### UI isolation (APP-UI-ISOLATION.md)

The UI is written against the broker contract from day one, so stage 4's
flip changes nothing in the app. Concretely: no `localStorage` (drafts go to
the backend), no clipboard API, no downloads, no reaching into `parent`, no
fetch of `/content`. All bytes come through `vector.*` commands. The scripts
are files with no inline boot script (D4). The sandboxed frame is
`allow-scripts allow-forms` and needs nothing more.

**What the broker must carry:**

- `invoke`, `cancel` and progress for the app's own `vector.*` commands. No
  `[ui] invoke` is needed, because every file operation is the app's own
  command over its own grant.
- Shell ops: `shell.quicklook.open` (preview an exported PNG) and
  `shell.app.open` (hand an export to Files), both already on the
  allowlist.
- Theme messages.
- Payload sizes:
  - the broker's 1 MiB params limit;
  - the browser socket's **256 KB inbound frame cap**
    (`ws.rs`, `max_frame_bytes`), which is the binding limit;
  - the app socket's 8 MiB cap (`protocol/lib.rs`, `MAX_FRAME_BYTES`) on
    responses.

**What the UI design does not cover and this lane needs:**

1. **Saves over about 192 KB.** A saved SVG crosses the browser socket as one
   inbound frame. An illustration with an embedded image, or a few thousand
   nodes, exceeds 192 KB easily. The editor already hits this wall
   (Lane 2: "Files over 192 KB cannot be saved until there is a chunked or
   streamed write"). **Prerequisite: a platform chunked upload** (call it
   Lane 0.7), shared with Lane 2. Plan B is app-level chunking
   (`vector.doc.save` with `{upload, seq, last}` writing a partial file and
   renaming on commit). That works, but it is every app inventing its own
   transfer protocol, so it is not recommended.
2. **Opening an SVG outside `Drawings/`.** Files can hand Vector any path
   (`shell.app.open`), but stage 6 checks the path against the *caller's*
   file scope, and Landlock **cannot widen a running process**: a ruleset
   only narrows. v1 refuses with a sentence ("Vector edits drawings under
   Drawings/; move or copy it there"), plus a one-tap "Copy into Drawings
   and open" that calls Files. Doing better needs a **per-file grant**
   ("powerbox"): the operator's pick in Files grants that one file to the
   app for the session, with the core reading and writing it on the app's
   behalf. That is not in the isolation design. It is the general answer for
   every editor lane (5a, 6), so it should be designed once.
3. **D6 deferrals it would use:**
   - **clipboard**: paste SVG from another program, copy shapes out. v1
     works without it.
   - **downloads**: share a PNG from the phone. v1 writes to the working
     directory. iOS has no path until D6 is revisited.
   - **per-app storage**: replaced by backend drafts, so not needed.
4. **The WebRTC leak** (APP-UI-ISOLATION §7.2 item 3) applies: a malicious
   build of this UI could send anything under `Drawings/` out. Nothing in
   this lane can close it. The mitigation is the narrow scope.

## Formats and longevity

- **Native format: SVG**, as the file it was. Documents written by Vector
  are SVG 1.1 plus the SVG 2 features that both browsers and `resvg`
  render. Layers use Inkscape's own layer markup, so they appear as layers
  in Inkscape.
- **Preserved untouched:** unknown elements, foreign-namespace attributes
  (`inkscape:*`, `sodipodi:*`, Illustrator's `i:*`), comments, `<metadata>`,
  gradients, filters, masks, clips, `<use>`, `<style>`. Only an element the
  operator edited is rewritten, and only the attributes the edit touched
  (an edited path's `d`, its `transform`, its style properties).
- **Removed on open, with a visible notice:** `<script>`, `on*` attributes,
  and `javascript:` URLs. The frame's CSP would block them anyway
  (`script-src` has no `'unsafe-inline'`). Keeping them inert in the model
  while hiding them from the live DOM is possible but not worth v1. This is
  stated as a known loss.
- **Deckjack-specific data: none.** If guides or a grid setting are wanted
  later, they go in a `deckjack:` namespace that other tools ignore. There is
  no sidecar for the document itself.
- **Exports:**
  - PNG (`resvg`);
  - plain SVG (`usvg`'s writer: text outlined, CSS resolved, `<use>`
    expanded), which is lossy by design and named as such.

  Both are new files beside the source. The source is never replaced by an
  export.
- **Drafts** are plain SVG files in the app's private home, so even those
  open in another tool.
- **What survives Deckjack disappearing:** everything. The drawings are SVG
  files in a folder.

## Risks and open questions

Each risk is paired with the spike that would settle it, cheapest first.

1. **Boolean robustness and curve preservation.** linesweeper is "early beta"
   and its GitHub repo moved to Radicle (a maintenance-channel risk: issues
   and releases are now outside GitHub). It is unconfirmed from its docs
   whether results stay as curves or come back flattened.
   *Spike (2–3 days):* a corpus of about 200 cases (coincident edges,
   tangencies, self-intersections, holes, nested subpaths, text outlines,
   EvenOdd vs NonZero, Inkscape-exported real logos) run through
   linesweeper, flo_curves, vendored path-bool, and i_overlay plus a kurbo
   refit. Score: failures and panics (run under `catch_unwind` and fuzzing),
   area error against a raster reference made with `resvg`, output node
   count, and time. Settles the engine, and whether booleans need a
   flatten-and-refit fallback.
2. **Round trip through the browser DOM.** Does `DOMParser` plus
   `XMLSerializer` keep namespaces, entities, CDATA, `xml:space`, comments
   and the DOCTYPE in both Chromium and WebKit?
   *Spike (1 day):* open and save with no edits on files exported by
   Inkscape, Illustrator, Figma, Affinity and draw.io, plus the `resvg` test
   suite. Compare the canonical XML and render both versions with `resvg`.
   Any difference is a defect or a documented loss.
3. **Canvas vs export disagreement**, in text above all: the browser's shaper
   vs `harfrust`, and `textPath` glyph placement.
   *Spike (1 day):* 50 documents rendered by Chromium, WebKit and `resvg`
   with the embedded fonts, compared by pixel diff. If text disagrees
   visibly, "export outlines text first" becomes the rule for PNG.
4. **Latency of brokered geometry calls from a phone.**
   *Spike (half a day):* p50 and p95 for a `vector.path.boolean` with 20 KB
   of path data, and for pencil simplify, over the tailnet from the phone.
   Above about 300 ms p95, pencil simplify moves into JS (a small fitter)
   and booleans keep a spinner.
5. **Saves over 192 KB** (see isolation item 1). This is a platform decision,
   not a spike: build chunked upload before this lane's save path, or accept
   plan B. **Blocking for real-world files.**
6. **Touch editing at 360 px.**
   *Spike (2 days, before any engine work):* an HTML prototype with the pen
   tool, node editing and handles, timed on a real phone and the Fold:
   redraw a given logo outline and fix three nodes. This settles whether
   360 px is "touch-up only" or better than that, and the hit-radius
   numbers.
7. **Imported `<style>` and hostile SVG in the frame.** Shadow-root scoping
   should contain CSS. Confirm that `:host`, `@font-face` and `@import`
   inside the drawing cannot restyle or reach out of the editor, in both
   engines (half a day, folded into spike 2). Backend parser limits are
   tested with billion-laughs, deep nesting and huge `viewBox` files.
8. **PDF export.** `svg2pdf` is archived ("not maintained anymore. Use
   krilla with krilla-svg"). `krilla-svg` 0.8.1 pins `usvg`/`resvg` 0.47, so
   it would put a second `usvg` copy in the binary beside 0.50.
   *Defer.* When it is wanted, spike `krilla-svg` against an `rsvg-convert`
   helper on the same corpus.
9. **Font set and binary size.** Choose the families, confirm OFL terms for
   embedding, and measure the binary (target: under about 15 MB total;
   unmeasured).
10. **Android as a node** waits on Lane 0.5. Nothing in the stack is
    expected to block the cross-compile (pure Rust; inferred, not built).
11. **Before APP-UI-ISOLATION stage 4,** this app's UI runs with the
    operator's full authority, like every app today. Shipping before stage 4
    is a deliberate acceptance of that, not something this lane can fix.

## Exit check

From the phone:

1. Open an Inkscape-made SVG with two layers, a gradient and a blur filter,
   copied into `Drawings/`.
2. Draw a mark: two shapes unioned, a circle subtracted, text set on a
   circular path, the text outlined, a node fixed by hand.
3. Save, close, reopen. The draft and undo state are not needed after a
   save.
4. Export a 2048 px PNG.

Pass when:

- Inkscape and Chromium both open the saved file with the layers, the
  gradient and the filter intact, and the edits identical to the canvas.
- The PNG matches Chromium's render of the same file within a stated
  pixel-diff threshold.
- A no-edit open-and-save of the round-trip corpus from spike 2 changes no
  rendered pixel.

## Rough size

- **Backend** (`deckjack-vector`): about 1.5–2.5k lines of Rust plus about
  1.5k of tests. Most of it is wiring and limits; the engines do the math.
- **UI:** about 6–9k lines of plain ES modules, with no build step and no
  framework. That covers the path parser and serializer, transforms,
  selection and handles, six tools, node editing, layers, property sheets,
  gestures, undo, text and textPath UI, and the shadow-root document host.
  Plus about 2–3k lines of tests.
- **Effort:** the largest single app after the editor. A realistic plan is
  one spike pass (spikes 1, 2, 4 and 6), then three or four build passes:
  (1) open, view, select, transform, save; (2) shapes, pen and node editing;
  (3) text, text on path and booleans; (4) export, drafts and the exit
  check.
- **Difficulty:** the geometry is borrowed and the rendering is the
  browser's. The hard, schedule-eating parts are touch node editing that
  feels precise, lossless round trips across other tools' SVG, and boolean
  robustness on real-world paths. None of those is a library choice; each is
  a corpus and a lot of testing.

## Sources

- crates.io API, queried 2026-11-16 (versions, licenses, dependency lists):
  [resvg](https://crates.io/crates/resvg), [usvg](https://crates.io/crates/usvg),
  [kurbo](https://crates.io/crates/kurbo), [lyon](https://crates.io/crates/lyon),
  [linesweeper](https://crates.io/crates/linesweeper),
  [flo_curves](https://crates.io/crates/flo_curves),
  [i_overlay](https://crates.io/crates/i_overlay),
  [svg2pdf](https://crates.io/crates/svg2pdf),
  [krilla-svg](https://crates.io/crates/krilla-svg),
  [fontdb](https://crates.io/crates/fontdb),
  [harfrust](https://crates.io/crates/harfrust),
  [tiny-skia](https://crates.io/crates/tiny-skia)
- [linesweeper repository](https://github.com/jneem/linesweeper) ("early beta"; archived, moved to Radicle) and [docs](https://docs.rs/linesweeper/0.5.0/linesweeper/)
- [svg2pdf repository](https://github.com/typst/svg2pdf) ("not maintained anymore. Use krilla with krilla-svg"; archived 2026-07-10)
- [resvg unsupported features](https://github.com/linebender/resvg/blob/main/docs/unsupported.md)
- [usvg text layout source](https://doc.servo.org/src/usvg/text/layout.rs.html) (harfrust and skrifa) and [usvg text parser](https://docs.rs/crate/usvg/latest/source/src/parser/text.rs) (`textPath`)
- Graphite `path-bool` (mirror only, to confirm upstream): [README mirror](https://huggingface.co/spaces/openfree/graphite2/blob/main/libraries/path-bool/README.md), [Graphite Q2 2024 report](https://graphite.art/blog/graphite-progress-report-q2-2024/)
- In-repo: `docs/APP-SUITE.md` §2, §3, Lanes 2 and 5; `docs/APP-UI-ISOLATION.md` §2 (D4, D6), §4.2, §4.5, §4.11, §7.2; `crates/deckjack-core/src/apps.rs` (`SYSTEM_READ_DIRS`, private home/tmp); `sandbox.rs` (seccomp denylist, no memory limits); `ws.rs` (`max_frame_bytes` 256 KB); `protocol/lib.rs` (`MAX_FRAME_BYTES` 8 MiB)
