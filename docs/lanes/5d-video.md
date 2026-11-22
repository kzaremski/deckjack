# Lane 5d: Video editor (scoping brief)

Status: **first pass built** (2026-11-21), on the brief below. The brief was
written 2026-11-16 and is kept as it was; what the spike measured and what was
built or deferred is in the next section, which wins where they differ.

## First pass, 2026-11-21: spike S2 and what was built

### Spike S2: FilmCraft embeds. Engine A is taken.

Run first, before any app code, on the operator's laptop (24 threads, Linux
7.2, rustc 1.99.0).

| Question | Measured |
|---|---|
| Does it exist? | Yes: `github.com/storytold/filmcraft`, version 0.20.1, edition 2024, `rust-version = 1.95`. Pinned at `a378140d8e665674fd0a037784ed5112cc9fe98d` (main, 2026-11-21). |
| Does it build here? | Yes, as a git dependency, with no patches. Its own git dependencies (`storytold/craft-libs` at five pinned commits) come along; nothing from crates.io is missing. |
| Compile time | `filmcraft-engine` and everything below it, release, cold, including the git fetch: **103 s**. `deckjack-video` debug, cold: **41 s**. Incremental rebuilds of the app: about 3 s. |
| Binary size | Spike probe (engine plus a `main`), release: **50.8 MB**, **39.9 MB stripped**. `deckjack-video` release, unstripped: **54.6 MB** (the core is 25.9 MB). R12: an install stages a full copy. |
| Dependency tree | **261 crates**, all Rust. No `wgpu`, no TLS, no HTTP client, no tokio, no egui or winit, no C library (the only `-sys` crate is `linux-raw-sys`). The GPU crate, the UI, the TCP control server and the MCP server are not linked at all. |
| Embedding surface | Exists, though undocumented: `Session::default()`, `Session::start_autosave(AutosaveConfig)`, `Session::execute(id, json)` (the same entry point FilmCraft's UI, CLI and MCP use), and `Session::try_render_program_with_offline_at(scale, tick)`. `Session` is `Send`. |
| Edit, undo, render, save | Import a clip, make a sequence from it, razor, undo, render a frame on the CPU path, save `.fcproj`: all worked from a 60-line program. |
| Scrub still (CPU) | 640×360 from 720p30 H.264 at 1 s: **35 ms**. 960×540 from 1080p30 at 1 s (seek into a GOP, first decode): **349 ms**. No proxies (see Deferred). |
| Export (FilmCraft's own H.264/AAC encoder, software) | 30 s of 1080p30: **12.3 s** (73 fps). Its default bitrate is about 20 Mbit/s, so exports here use CRF 20. |
| Peak memory | **Not measured**: `/usr/bin/time` is not installed and nothing else was set up for it. R10 stays open. |
| Confined | See "Confined run" below. |

Decision: **engine A (FilmCraft in-process)**. The fallback (our own OTIO model
plus the ffmpeg binary) was not started.

### Built

Crate `crates/deckjack-video`, app id `video`, 28 commands, all `read` or
`write`, all in its own scope. Manifest: `Videos/` read-write and nothing else.

- **No GPU in the manifest.** The brief asked for `gpu = true`. The CPU
  renderer is the only path this build links (FilmCraft's `gpu` crate is not a
  dependency), so asking for `/dev/dri` would ask for something unused.
- **Projects:** `video.project.new/open/save/list/describe/wait/recover`. One
  FilmCraft session per project file, shared by every window and agent. A new
  project has V1–V2 and A1–A2; the first clip inserted sets its size and rate.
  Save writes `<name>.fcproj` and rewrites `<name>.otio` through a hidden
  staging file. **Autosave** is FilmCraft's rolling `Videos/Auto-Save/` copies
  (every 5 min, 20 kept). **Crash recovery** is FilmCraft's journal in the
  app's private `HOME`: after a crash, `open` reports the unsaved edits and
  `recover` applies them; nothing is applied silently. Only a journal written
  after the project file was last saved is offered, so saving after a crash
  retires the offer for good. FilmCraft's recover replaces the project and
  clears undo, so when the open state has unsaved edits `recover` first saves
  them as `<name> (before recovery).fcproj` (a project of its own) and names
  it in the answer. An unreadable `.fcproj` with a sidecar opens from the
  `.otio` and says what OTIO does not keep; every later opener is told too,
  and the first save copies the file that would not open to
  `<name>.fcproj.unopened` before replacing it (it may be from a newer
  FilmCraft, or only briefly unreadable). Opens and creates are serialized,
  so concurrent first uses of a project share one session.
- **Media:** `video.media.list` (recursive, under `Videos/`) and
  `video.media.import`, with FilmCraft's probe (size, rate, codec, channels,
  duration) in `describe`.
- **Edits, each one FilmCraft command and so one undo step, each naming its
  targets:** `insert`, `overwrite` (with source in and duration), `move`
  (across tracks of a kind, optionally inserting), `trim` (either edge, ripple
  on or off, linked or not), `split` (one clip and its partners, or all
  tracks), `lift`, `ripple_delete`, `link`, `unlink`, `gain`; markers
  `add/move/remove`; `undo`, `redo`. FilmCraft's Link is a toggle, so link and
  unlink check the outcome rather than trusting it.
- **Monitor:** `video.frame.render` returns a base64 JPEG and its size; the
  window shows it from a `blob:` URL. One request in flight; scrubs meanwhile
  collapse into one follow-up.
- **Export:** `video.export.start/cancel/list`, H.264/AAC MP4 at source, 1080p
  or 720p (aspect kept, never upscaled). Written under a hidden
  `.<name>.<job>.partial.mp4` and published by hard link, which fails rather
  than replaces, so a taken name becomes `name 2.mp4` (a dangling symlink
  counts as taken). On a filesystem without hard links (vfat and exFAT
  return `EPERM`) it falls back to an exclusive create and a copy; `EACCES`
  is reported, not worked around. A cancelled or failed export removes its
  own partial file and nothing else; one left by a crash is swept at startup.
  An export asks FilmCraft for `maxThreads = cores/2` and runs at nice 10 or
  lower priority (see "Review fixes" for what that does and does not cover).
  The window starts exports with `wait: true` and does not await the answer:
  the progress frames are what keep the core from stopping the app for
  idleness once the window is closed.
  Cancellation is in-process (FilmCraft's job cancel flag), not the SDK.
  Optional `loudness_lufs` passes FilmCraft's render-time loudness
  normalization through (**untested** against a meter; see Deferred).
- **Agents:** `video.project.describe` returns tracks (`V1`, `A1`…), clips
  (frames, source in, link group, gain), markers, media and revision.
  `video.project.wait {since}` long-polls for changes.
- **UI** (no build, plain ES modules, `/kit/` only): `model.js` (time and
  pixels, pinch, snapping, layout, hit testing, timecode), `timeline.js`
  (canvas and gestures), `video.js` (the app), `video.css`. Tap selects,
  long-press drags (a mouse drags at once), handles only on the selected clip
  and reaching outside it, one finger on the ruler scrubs, one finger on empty
  track pans, two fingers pinch and pan, nudge buttons (−10, −1, +1, +10) and a
  timecode field for frame accuracy, Ripple and Snap toggles, Undo and Redo
  always visible, the keyboard keys from the brief (except J/K/L, Space and
  I/O, which need playback or a source monitor). Bin as a sheet below 600 px
  and docked beside the monitor from 600 px; a message below 360 px. No inline
  script, no content route, no storage, no network.

Time on the wire is whole frames at the sequence's rate; FilmCraft's ticks
stay inside the backend.

### Deferred, and why

- **Playback / render-ahead preview segments** (`video.preview.segment`, MSE):
  the monitor is stills only. The largest missing piece.
- **Proxies**: none built. Scrubbing decodes the source; fine at 720p, slow on
  1080p+ seeks (349 ms above) and certainly on 4K HEVC.
- **Loudness measurement** (`video.loudness.measure`): not built. Export's
  `loudness_lufs` is wired to FilmCraft's normalizer but no test measures the
  result, so treat −14 LUFS as unverified.
- **Cross-dissolves, fades, transitions**: not exposed (FilmCraft has them:
  `sequence.applyVideoTransition`, `applyAudioTransition`).
- **Waveforms and filmstrip thumbnails**, `video.prefs.*`, interchange export
  to FCPXML/EDL, `video.proxy.*`: not built.
- **Spikes S1, S3–S7**: not run. S3 (real phone footage: VFR, HEVC HDR,
  rotation) is the one most likely to change a decision.

### Confined run

Done on this laptop with the release core and app, a throwaway
`DECKJACK_HOME` and working directory under `/tmp`, `--debug` on
127.0.0.1:7951 (not the live server), driven over `/ws` with the debug
session cookie: approve (`gpu: false`, `Videos/` read-write, nothing else),
open a window (which launches the app; the log says "launching app
confined"), then every command below.

| Step | Measured |
|---|---|
| `video.project.new` | 6 ms |
| `video.media.import` (30 s 1080p30 H.264, 21.7 MB) | 4 ms; probe correct (1920×1080, 30/1, H.264 High, mono AAC, 900 frames) |
| insert, split, marker, describe | correct state |
| `video.frame.render` at 640 px, 1080p source | 122 ms first, 12–13 ms for the next frames, 265–310 ms on a seek into another GOP, 28 ms after it; 34–36 KB base64 |
| `video.project.save` (`.fcproj` and `.otio`) | 4 ms |
| `video.export.start` 720p, `wait` | 14.6 s for 30 s; `ffprobe`: H.264 1280×720 30.0 s, AAC 30.0 s |
| `video.export.start` 1080p, cancelled after 1.5 s | `cancelled` at 11%; no file left |

On disk afterwards: `Videos/` holds the source, `Confined.fcproj`,
`Confined.otio` and `Confined.mp4`, and nothing else; FilmCraft's
preferences, presets and recovery journal are in the app's private
`cache/apps/video/home/filmcraft/`. Nothing else in the working directory
changed. Not yet seen: an `Auto-Save/` folder (its first copy comes after five
minutes), and **the UI in a real browser**: the window's JavaScript is tested
in happy-dom only, so layout at 360 px, canvas drawing and touch on a phone
are unverified (that is S6).

### Tests

`cargo test -p deckjack-video`: 40 tests after the review fixes (30 in the first pass), most end to end against FilmCraft's
real engine and a committed 2-second fixture (`tests/fixtures/clip.mp4`, 24 KB,
192×108 H.264/AAC): insert/overwrite, split, trim with and without ripple,
lift versus ripple delete, linked moves, one-sided trims and moves, unlink and relink, undo/redo, markers,
save and reopen in a fresh process, sidecar fallback, crash recovery, stills,
export (read back by FilmCraft's demuxer), cancel, long poll, and the
manifest/handler/UI contract, plus recovery after a save, recovery over unsaved
edits, the sidecar copy, concurrent opens and creates, first-clip placement at
another rate, dangling symlinks, the no-hard-link copy, partial-file cleanup,
the thread cap and the nice value. `web/src/apps/video.test.js`: 29 tests of the
timeline math and the mounted UI with a fake bus. Every test was
mutation-checked: the guarded code broken, the test seen failing, restored.

### Review fixes, 2026-11-21

A review of the first pass found nine problems. All nine were real; each is
fixed with a test that was seen failing with the fix taken out.

- **Recovery was offered forever, and applying it could drop live edits.**
  Fixed as described under Built: a save after the journal retires it, and a
  recover over unsaved edits saves them as a copy first. The banner no
  longer says the changes "were recovered" before anything is applied.
  `file.discardRecovery` is still not exposed: a journal superseded by a save
  stays in the app's private `HOME` until FilmCraft prunes it, but it is never
  offered again.
- **The sidecar fallback could replace the original `.fcproj` with no copy.**
  Fixed: the copy to `<name>.fcproj.unopened` comes first.
- **Concurrent first opens built two sessions.** Fixed: one lock covers open
  and create.
- **`publish` spun forever on a dangling symlink.** Fixed (`symlink_metadata`).
- **No hard links meant a failed export.** Fixed (copy fallback).
- **Cleanup by prefix could delete another job's partial file.** Fixed: only
  the job's own file is removed. FilmCraft writes an MP4 export to that one
  path and nothing beside it (`export/src/output.rs`).
- **The first clip into an empty sequence landed on the wrong frame when its
  rate differed from the placeholder's** (frame 100 at 30 fps became 120
  after a 25 fps placeholder). Fixed: the start frame and duration go to
  FilmCraft as frames, which it reads after `matchSequence`. **Not fixed:**
  `source_in` on that same first clip is still converted at the old rate; no
  caller sends it on a first clip today.
- **A background export was killed by the idle sweep.** The core noted
  activity when a command was dispatched, never while it ran, so an
  on-demand app with no window was stopped 300 s into a long export.
  Fixed in two parts. The core's frame pump now counts progress on a live
  call as activity (`deckjack-core/src/apphost.rs`, a core change; test in
  `deckjack-core/tests/apphost.rs`). The window uses `wait: true`, so its
  export reports progress for its whole length. **Still true:** an agent's
  `wait: false` export survives only while something calls the app at least
  every five minutes; the parameter's description says so. A real fix is a
  protocol-level "busy" signal from app to core, which is not built.
- **Resource limits the sandbox section requires were missing.** Partly
  fixed. An export now passes `maxThreads = cores/2`, and the call that
  starts it runs on a thread at nice 10. Linux nice is per thread and
  inherited, so every thread the export starts runs niced. Measured with a
  thread census of the test process during an export, on this 24-thread
  laptop: every export-time thread was at nice 10. That covers FilmCraft's
  `filmcraft-export*` threads (61 of them, not 12), 16 `h264-*` encoder
  threads and 24 `decode-*` threads. **The thread cap does not hold**:
  `maxThreads` sizes only FilmCraft's export render pool. Its decode pool
  (`codecs/src/gop.rs`, `decode_pool`) is a process-wide static sized to
  every core, and its H.264 encoder keeps a pool of its own. The decode pool
  is also built by whatever decodes first. In the app that is usually a scrub
  at normal priority, so decoding during a later export runs at nice 0 on 24
  threads. Capping that takes a FilmCraft change, or nicing the whole app
  process, which has not been decided. **No decode memory budget** was built
  either. FilmCraft caps each source's frame cache at 384 MB
  (`FRAME_CACHE_CEILING`, from reading the source; not measured). R10, peak
  memory, is still unmeasured.

---

The original brief follows.

Written 2026-11-16 against the tree at
`179119c` plus the uncommitted working tree, `docs/APP-SUITE.md` §2, §3, §11, §12
and Lanes 5 to 7, `docs/DESIGN.md`, `docs/APP-UI-ISOLATION.md`, and
`crates/deckjack-core/src/{sandbox.rs,apps.rs}`. Facts about FilmCraft come from
its README and docs on GitHub, read today. Claims marked **inferred** or
**unverified** have not been checked against running code.

Scheduling (from APP-SUITE Lane 5, unchanged): this sub-lane follows 5a, 5b and
5c and Lane 0.2 (done). This brief adds two dependencies the suite does not
list yet: **APP-UI-ISOLATION stage 4 (the flip)**, so the UI is written for the
opaque frame from the first line, and **stage 6's second socket**, so preview
traffic cannot stall terminal echo.

---

## Goal

Cut a short video on the phone with a touch timeline drawn in the design
system. The engine is a confined Rust process on a Linux node, and every edit
is a bus command an agent can also run. The work stays in open files that
outlive Deckjack.

DESIGN.md's phase 4 exit is "cut and export a short video from the Fold browser
with the engine on the Mac, then with the engine on the Fold itself." **Neither
half can be met as written** (see Risks R1 and R2). This brief's exit check
uses a Linux engine host and states the Fold-hosted half as blocked.

## The useful subset (v1)

v1 is a **cuts-and-audio editor**: the work you do on a phone clip before
posting it or handing it on. It is not a Premiere competitor. FilmCraft already
tries to be one, and its README rates its own production readiness at about
56%.

**In v1:**

- **Projects.** New, open, save and autosave, with recovery after a crash.
- **Media.** Import from the app's granted folder, probe the files, and build
  proxies in the background so scrubbing is fast.
- **Timeline.** Up to 2 video tracks and 4 audio tracks; the UI is laid out for
  V1 and A1 first.
- **Edits.** Insert and overwrite from a source range; move; trim both edges
  with ripple on or off; split at the playhead; lift and ripple delete; snapping.
- **Linking.** Audio and video stay linked, with an unlink option, which gives
  J- and L-cuts.
- **Audio.** Gain per clip; a fade at either end of a clip; a cross-dissolve
  between adjacent clips (video and audio) of one fixed shape.
- **Loudness.** Measure integrated loudness (EBU R128) and normalize the whole
  program to a chosen target, for example −14 or −23 LUFS. DESIGN.md's table
  names this.
- **Navigation.** Markers. Undo and redo with no depth limit for the session.
- **Monitor.** A program monitor with scrub stills, plus playback of
  render-ahead preview.
- **Export.** H.264/AAC in MP4, at source, 1080p or 720p. It never overwrites
  an existing file.
- **Sidecar.** OpenTimelineIO written next to the project on every save.
- **Agents.** A `describe` command that returns tracks, clips and markers as
  structured data, for agents.

**Explicitly not in v1, each a separate decision rather than an increment:**

- **Picture work:** color grading, LUTs, scopes, keyframed effects, titles and
  text, speed changes and time remapping, nested sequences or multicam,
  stabilization, masks.
- **Media handling:** HDR grading (HDR sources are tone-mapped to SDR for
  preview and export; R6 says whether the engine does this correctly), 10-bit
  or ProRes export.
- **Agent and integration:** concurrent human and agent editing (DESIGN.md says
  the timeline "takes turns", and v1 has one writer per project), a passthrough
  to FilmCraft's ~650 engine commands (see Backend commands), import of foreign
  projects (`.prproj`, FCPXML) as an editing source.
- **Hosting:** an engine hosted on the phone (R2), an engine on macOS (R1),
  WebRTC (R3).
- **Real-time preview:** composited real-time playback of a timeline the
  engine has not rendered yet. v1 plays rendered preview and is honest about
  the wait (see the monitor section).

## Engine choice

### Recommended: FilmCraft's engine crates, linked in-process, from a pinned fork

[FilmCraft](https://github.com/storytold/filmcraft) (storytold/ArtCraft) is a
Rust reimplementation of Premiere Pro, licensed MIT OR Apache-2.0. Its README
describes it this way:

- **Layering.** It is layered so that "nothing below the front ends depends on
  a UI toolkit or OS API". `engine` holds the command registry, session, undo
  and jobs. Below it sit `edit`, `project`, `format`, `interchange`, `render`,
  `gpu`, `export`, `time` and `media`. `ui-egui` is one front end above it.
- **Codecs.** Native Rust codecs, with **no FFmpeg inside**. H.264, HEVC, VP9,
  AV1, ProRes, DNxHD, AAC and Opus decode natively, plus ISO-BMFF, Matroska and
  AVI containers. MP3, FLAC and Vorbis come from `symphonia` for now. ffmpeg is
  used only as a test oracle.
- **Time.** Integer ticks at 254,016,000,000 per second, so 23.976 and 29.97
  drop-frame stay exact.
- **Rendering.** A wgpu compositor (Vulkan on Linux) and a CPU path that
  renders the same frames, tested against each other.
- **Headless.** `filmcraft-cli mcp` runs an in-process session with no window.
  `render_frame` returns a PNG of the program at a time. Export jobs report
  progress and can be cancelled.
- **Interchange.** OTIO, FCPXML 1.9–1.11, CMX 3600 EDL, AAF, FCP7 XML and OMF.
- **Project files.** `.fcproj`, which is UTF-8 JSON in a documented envelope
  with chained schema migrations (v1 to v17), atomic saves, rolling autosave,
  and a crash-recovery journal.
- **Maturity.** Alpha. The README rates it at about 86% feature breadth and
  about 56% production readiness. Its named gaps are speed on large footage,
  H.264 as the only software delivery codec, and light testing on Linux.

**Why it is the recommendation:**

- DESIGN.md already chose it, and decision 1 asks for an in-process engine for
  anything the operator scrubs or edits.
- Its license is compatible with AGPL (DESIGN.md, License).
- It brings undo, jobs, exact time and interchange. Those are what would take
  the longest to write ourselves, and getting them wrong silently corrupts
  edits.
- Pure-Rust decoders matter here twice. Parsing untrusted media files stays
  memory-safe. There is also no system libav ABI to track, which is the
  precondition for an Android build (Lane 0.5).

**What we take from it:** the engine and the crates below it, as a library
inside `deckjack-video`. **What we do not take:** `ui-egui`, the `--control` TCP
server, and the MCP server. The bus replaces all three, and the TCP control
port would be a second command surface with its own token, which is precisely
what DESIGN.md principle 4 forbids.

**What is not known (spike S2 settles it):**

- **No documented embedding API.** `control-protocol.md` and `agents.md`
  describe the CLI and MCP surfaces only, so "link `engine` and drive a
  session" is **inferred** from the crate layout.
- **Not published to crates.io.** It is a git dependency on a large workspace
  (4,137 commits).
- **Toolchain.** Compile time, binary size and Rust edition compatibility are
  unmeasured. The README requires edition 2024; this machine has rustc 1.99.
- **Undocumented payload.** The `.fcproj` payload schema is defined by the
  Rust type `filmcraft_project::Project`, not by a spec.

### Alternatives considered

| | What it is | Why not first |
|---|---|---|
| **B. Thin engine of our own** | An OTIO-shaped JSON timeline model, decode through `ffmpeg-next` (crate 9.0.0 exists; the upstream repo says it is in maintenance mode), and the `ffmpeg` binary for proxies, stills and export (decision 1's flagged exception) | It is the **fallback if S2 fails**, and it is tractable *for cuts only*: a cut list is concat and trim filters. We would write and own undo, ripple semantics, timebase arithmetic and A/V sync. libav ties us to the system library's ABI (ffmpeg n9.0.2 is installed here) and makes an Android build a project of its own. C decoders on untrusted input are contained by Landlock, not removed. |
| **C. GStreamer Editing Services** (`gstreamer-rs`, Pitivi's engine) | A mature timeline engine in C, LGPL | Plugin loading and the registry cache are hard to fit inside Landlock's fixed read set (**inferred**), packaging it for Android is heavy, and Pitivi has little momentum. Adds a second large C runtime. |
| **D. MLT** (Kdenlive and Shotcut's engine) | C/C++, LGPL; the MLT XML project format is open, and the `melt` CLI could be the export helper | No maintained Rust bindings. Attractive only as a *format* (Kdenlive reads it), and OTIO already covers that. |
| **E. FilmCraft's wasm build, inside the app frame** | The engine runs in the browser | The isolated frame has no file access except brokered `files.read`, capped at 16 MiB, so a 2 GB clip cannot reach it. A phone browser also runs out of memory and thermal headroom. Decision 1 rejects the egui front end in any case. |

### Per-command split, as decision 1 asks

| Work | Where it runs | Why |
|---|---|---|
| Edits, undo, describe, scrub stills | in-process engine | The operator scrubs and edits these. |
| Render-ahead preview segments | in-process engine (render) plus its own H.264 encoder | Small, latency-sensitive, and repeated on every edit. |
| **Proxies** | **in-process engine job, not the ffmpeg binary** | **A deliberate deviation from the letter of the flagged exception.** Proxy frame *N* must be original frame *N*, and two decoders (FilmCraft's and libav) can disagree on variable-frame-rate phone footage. One decoder decides frame identity. |
| Export encode | engine renders the frames → `ffmpeg` helper encodes them from pipes when present; FilmCraft's own H.264/AAC otherwise | Decision 1's exception applies cleanly: the helper decodes nothing, so there is no disagreement, and it brings x264/x265/SVT-AV1/Opus breadth. Whether pipes are practical is S4's question. |

## Backend commands

App id `video` (not a reserved core namespace per APP-UI-ISOLATION §4.6). Every
id is the app's own scope, so the UI needs **no `[ui] invoke`** entries. The
app process lists its own granted folder itself and does not call the core's
`files.*`.

| id | risk | progress | notes |
|---|---|---|---|
| `video.project.new` | write | – | Creates `<name>.fcproj` and `<name>.otio`; refuses a taken name |
| `video.project.open` | read | yes (media relink and probe) | Attaches to the one live session for that file if it exists |
| `video.project.save` | write | – | Atomic save; writes the `.fcproj` and the OTIO sidecar; earlier versions survive in the rolling autosave, so this is undoable and therefore `write` (§11) |
| `video.project.describe` | read | – | Structured tracks, clips, markers, duration and revision, per DESIGN.md "describe state" |
| `video.project.wait` | read | – | Long-poll `{since_rev}`, which returns when the session changes. Needed because **the app protocol has no app-emitted events** (`deckjack-app` exposes only `progress` and `log`) |
| `video.media.list` | read | – | The granted folder's media, with probe data |
| `video.media.import` | write | yes | Adds files to the project bin and queues proxies |
| `video.media.thumbnail` | read | – | Filmstrip frames. Also the confined helper Lane 1 says video thumbnails are waiting for |
| `video.media.waveform` | read | – | Peak data at a requested resolution, served from cache |
| `video.proxy.make` | write | yes | Writes only the app's cache; queued automatically on import |
| `video.proxy.purge` | write | – | The cache is safe to delete (§11) |
| `video.clip.insert`, `.overwrite`, `.move`, `.trim`, `.split`, `.lift`, `.ripple_delete`, `.gain`, `.fade`, `.link`, `.unlink` | write | – | Each is one undo step; each names its targets explicitly (FilmCraft's selection-dependent commands are not exposed) |
| `video.transition.add`, `.remove` | write | – | Cross-dissolve only |
| `video.marker.add`, `.move`, `.remove` | write | – | |
| `video.edit.undo`, `.redo` | write | – | |
| `video.frame.render` | read | – | `{t, width}` returns a JPEG (WebP if S1 shows a gain) as base64 with its exact frame time |
| `video.preview.segment` | read | – | `{start, dur≈2s, height}` returns a fragmented MP4 (H.264 plus AAC) as base64, or `{pending}` while rendering. Bounded under the 8 MiB app frame cap (`protocol/lib.rs:105`) |
| `video.loudness.measure` | read | yes | Integrated loudness, loudness range and true peak (`ebur128` 0.1.10, MIT, which passes EBU Tech 3341/3342) |
| `video.loudness.normalize` | write | – | Stores a target in the project; applied at render time, not baked into media |
| `video.export.start` | write | yes | Never overwrites; `keep_both` like `files.copy`; writes under a hidden partial name and renames when done |
| `video.export.cancel` | write | – | A separate command, because **`deckjack-app` handlers have no cancellation signal yet** (`TODO(cancel)` in `crates/deckjack-app/src/lib.rs`). Fixing that in the SDK is better, and is owed. |
| `video.export.list` | read | – | Jobs, progress and ETA |
| `video.interchange.export` | write | – | `{format: otio\|fcpxml\|edl}` to a new file |
| `video.prefs.get`, `.set` | read / write | – | Exists only because the opaque frame has **no `localStorage`** (APP-UI-ISOLATION §6), so zoom, track heights and layout persist through the app |

No v1 command is `destructive` or `outbound`. Nothing leaves the machine.
Nothing that cannot be undone is offered: deleting a project or media is Files'
job, and goes through the trash. An agent's edits therefore run directly under
its grant, as they should; v1 has nothing for the Inbox.

**Refused on purpose:** `video.engine.run {command, params}`, a passthrough to
FilmCraft's registry. Risk must be declared per command by the backend (§11,
principle 3). A passthrough would give `lut.import`, `file.exportMedia` and
hundreds of others one risk level that is wrong for most of them. Curated ids
are added as the UI needs them.

## UI and touch contract

The UI is plain HTML, CSS and JS with no build step (§2), importing `/kit/`.
The timeline and monitor are `<canvas>` surfaces that read the design tokens at
run time (DESIGN.md principle 1). All controls are DOM. Expect the timeline to
be the largest no-build JS file in the repo. It should be split into ES modules
under `ui/` (model, hit-testing, gestures, render), with the pure parts
(time↔pixel mapping, snapping, ripple preview) tested the way
`web/src/shell/geometry.test.ts` tests geometry.

**Layout.** On a phone, portrait is stacked: monitor (16:9), transport row,
timeline, toolbar. From 600 px up, an inspector and bin dock beside it. On a
landscape phone (about 360 px tall) the monitor collapses to a strip or a
toggle.

**Touch contract.** It matches what Files actually ships, not the brief Files
deviated from:

- **Tap selects; long-press lifts a clip and begins a drag.** Selection is a
  mode, and multi-select comes from the toolbar's Select button, never from a
  modifier.
- **Pinch zooms the timeline horizontally**, anchored between the fingers
  (reuse the math in `web/src/shell/gestures.ts`, re-implemented in the frame).
  Two-finger drag pans. One finger on empty track area scrolls the page
  vertically; one finger on the ruler scrubs.
- **Trim handles** appear on the selected clip only, with hit areas of at least
  `--dj-touch` that extend outside the clip. A finger cannot hit a frame at
  useful zoom, so a selected edge gets **nudge buttons** (−10, −1, +1, +10
  frames) and a timecode field. Frame accuracy on touch comes from these, not
  from dragging.
- **No blade tool.** Split at the playhead is a toolbar button (and `S` on a
  keyboard). Ripple is a visible toggle, and snapping is on by default with a
  toggle.
- Undo and redo are always on screen. Hover never reveals anything essential.
- **Keyboard (desktop):** J/K/L, I/O, S, ←/→ to step one frame, Shift+←/→ to
  step ten, Space, Delete (lift), Shift+Delete (ripple), Ctrl/Cmd+Z/Shift+Z.
  Every key dispatches the same command the button does.
- `touch-action: none` on the canvases. This needs checking inside the
  sandboxed frame on Android Chrome (S6).

**Honest minimum width:**

- **360 CSS px** (`min_width = 360`): one video track and one audio track,
  editable, with a 16:9 monitor about 200 px tall. Cutting a clip is
  comfortable; arranging many is not.
- **600 px or more** for 3 or more tracks with the inspector docked, which
  means the unfolded Fold, a tablet or a desktop.
- **Below 360**: the app says it needs more room rather than shrinking.

**Fullscreen is not available to the frame** (`Permissions-Policy:
fullscreen=()`). A full-screen monitor on the phone therefore needs a new
broker `shell` op (see below).

### The program monitor, and why it is not WebRTC

DESIGN.md specifies a WebRTC program monitor. **Under APP-UI-ISOLATION that is
the wrong channel**, and this brief proposes replacing it:

- The frame has `connect-src 'none'`; the only things it can load come from
  its cap path or `blob:` (`media-src {cap} blob:`).
- `RTCPeerConnection` is the one channel that still works from the frame, and
  only because it is the **documented open leak** (§7.2 item 3). Building the
  monitor on it would make the leak a feature dependency. The monitor would
  then break the day browsers implement the CSP `webrtc 'block'` directive the
  core already sends.
- On the server side, Landlock filters no UDP at all, so a WebRTC engine would
  confine nothing either.
- webrtc-rs is mid-rewrite: the v0.17 line is feature-frozen and v0.20 is a
  Sans-I/O redesign ([webrtc.rs blog,
  2026-01-31](https://webrtc.rs/blog/2026/01/31/webrtc-v0.17.0-feature-freeze-sansio-shift.html)).
  `str0m` 0.21 is the alternative.

**Proposed instead, fully inside D6's "no conveniences in v1":**

1. **Scrub:** `video.frame.render` returns a JPEG through an ordinary brokered
   invoke. The frame wraps it in a `blob:` URL (allowed by `img-src`) or draws
   it to the canvas. Each request carries a sequence number, and stale replies
   are dropped.
2. **Play:** `video.preview.segment` returns 2-second fragmented MP4 segments,
   which the frame appends to a `MediaSource` played from a `blob:` URL
   (allowed by `media-src`). On iPhone this must be `ManagedMediaSource`, which
   activates only with remote playback disabled or an AirPlay alternative
   ([WebKit, Safari 17.1](https://webkit.org/blog/14735/)). A 2026 WebKit bug
   reports an iOS 26.2 regression with simultaneous appends to several
   SourceBuffers ([bug 305712](https://bugs.webkit.org/show_bug.cgi?id=305712)),
   so v1 should mux audio and video into one SourceBuffer.
3. **Render-ahead:** after each edit the engine re-renders the dirty segments
   around the playhead first. Segments not ready yet show as a hatched bar on
   the ruler, and playback waits on a hatched segment instead of guessing.
   This is honest, and it is the opposite of "instant feel". The wait is the
   cost of no real-time compositor in the browser.

**Bandwidth (inferred, to be measured in S1):** a 720p preview at about
3 Mbit/s is about 750 KB per 2-second segment, about 1 MB after base64, or
about 4 Mbit/s on the bus. JPEG stills at 640 px are about 40 to 80 KB each.
That is acceptable on a LAN or tailnet, provided it rides **the second socket
(§4.12)** and the broker's token bucket allows bursts of about 15 stills per
second while scrubbing.

## How it fits the sandbox

### Manifest

```toml
[app]
name = "video"
title = "Video"
lifecycle = "on-demand"
min_width = 360
tag = "Vi"
summary = "Cut and export video in the working directory"
default_width = 1100
default_height = 720
compact = true
wide = true

[needs]
gpu = true                                       # /dev/dri (VA-API, Vulkan); CPU path when absent
working_dir = [{ path = "Videos", write = true }]
# no network, connect_tcp or bind_tcp, and no invoke_commands
# no [ui] invoke: the UI calls only video.* (own scope)
```

- **`working_dir` cannot be the whole working directory.** The protocol refuses
  the root (`protocol/lib.rs`, "is the working directory itself"), and a read
  need that does not exist fails the launch (`MissingPath`). So v1 sees
  **`Videos/` only**. Footage that lands elsewhere, such as a phone camera sync
  into `Photos/`, is copied or moved into `Videos/` with Files first. Adding a
  second folder means a manifest change and a re-approval. That is the
  intended friction, and it should be stated in the UI's empty state.
- **`gpu = true`** grants `/dev/dri` read-write (and `/dev/kfd` where it
  exists). This is honest in the approval text even on a host with no GPU,
  where nothing is granted. **The R620 most likely has no usable GPU**
  (**inferred**: it has a server board with onboard VGA), so **the CPU path is
  the primary path**, not a fallback, and S2 measures it. Headless wgpu on a
  render node needs no Wayland socket, which is fortunate, because
  `/run/user/<uid>` is never granted. Mesa's ICD files are under `/usr/share`,
  and `/sys` and `/usr` are already readable. **Unverified** inside Landlock.
- **Cache.** Proxies, waveforms, preview segments and FilmCraft's media cache go
  in the launcher's private directory `<install>/cache/apps/video/home`
  (`apps.rs`, `private.join("home")`, exported as `HOME`). FilmCraft's
  `$XDG_DATA_HOME` fallback (`~/.local/share/filmcraft`) lands inside it
  without configuration. §11 says `~/.deckjack/cache/<app>/`; the launcher
  actually grants `cache/apps/<app>/`. Follow the launcher, and fix §11's
  wording. The cache needs a size cap with least-recently-used eviction (proxy
  sets grow large), and it must be safe to delete at any time.
- **FilmCraft's default proxy location** (`<media folder>/Proxies/`) would put
  regenerable files in the operator's tree. Override it to the cache.
- **Helper binaries:** `/usr/bin/ffmpeg` for the export encode only. It is
  executable because `/usr` is in the read set and Landlock read includes
  Execute (§2). It inherits the confinement and `/dev/dri`, so its VA-API
  encode works if the grant does. Not available: Termux's ffmpeg under
  `/data/data/com.termux/files/usr`, which is not in `SYSTEM_READ_DIRS`. On a
  phone host the export would use FilmCraft's own encoder.
- **Network ports:** none. The UDP caveat (§0.6) means the confinement does not
  stop the process sending datagrams. Nothing here needs them, and the manifest
  claims none.
- **Seccomp:** nothing on the denylist is needed. **Memory and CPU are not
  bounded** (`sandbox.rs`: "containment, not a hypervisor"). A 4K HEVC decode
  or an export can starve the core on the same host. The app must budget
  decode memory itself (as `thumbnail.rs` does), cap decode threads at
  `cores/2`, and `setpriority` the export helper.

### UI isolation: what the broker must carry

**v1 needs no new broker conveniences:**

- `invoke` and `cancel` within its own scope;
- `shell.quicklook.open` to preview an export (its path is inside the app's
  file scope);
- `shell.app.open` to hand an exported file to Files;
- the theme by message (canvas surfaces repaint on a theme change).

Bytes travel in command results. This only works because results are bounded:
segments are under about 2 MB and stills under 200 KB, against the 8 MiB app
frame cap and the broker's 1 MiB *params* cap, which results are not subject to.

**What D6 deferred that 5d will press on, in the order it will be asked for:**

1. **Fullscreen monitor.** A broker `shell` op such as `shell.window.fullscreen`
   on the calling panel, drawn by the shell. It is low-risk because the shell
   owns the chrome. Wanted for the phone in v1; until then the UI is
   maximize-only.
2. **A binary media channel**, meaning transferable `ArrayBuffer`s or a
   `MessagePort` stream instead of base64 in JSON. This is D6's "media
   streaming through the broker". S1 decides whether it is needed: base64
   costs 33% and a JSON parse per segment. If S1's p95 scrub latency over the
   tailnet is acceptable, defer it again.
3. **A per-app media route** under the cap, for example `/apps/video/~cap/media/…`
   serving Range requests from the app's *file scope* so a `<video>` could play
   a proxy directly. Rejected for v1: it widens the cap from "this app's UI
   bytes" (APP-UI-ISOLATION §4.2) to "this app's files". A leaked cap would then
   leak `Videos/`, although the frame can already do that over WebRTC. Record
   it as an option, not a plan.
4. **Downloads** (save an export to the phone's own storage). Not needed: the
   shell's `/content` already serves `Content-Disposition`, reached through
   Files or Quick Look.

## Formats and longevity

| Artifact | Format | Survives Deckjack? |
|---|---|---|
| Project | `.fcproj`: UTF-8 JSON, documented envelope and migrations, MIT/Apache reader | Yes, as a FilmCraft project: the desktop app opens it. Its payload is specified by a Rust type, not a spec, so a third reader means reading FilmCraft's source. Open, but with a single implementation. |
| **Timeline sidecar** | **`.otio` (OpenTimelineIO JSON), rewritten atomically on every save** | Yes. An ASWF standard. Resolve and Kdenlive import OTIO (**unverified per version**; S7 checks). **Survives:** tracks, cuts, source ranges, gaps, markers, cross-dissolves (as OTIO transitions), names. **Lost:** clip gain, fades and loudness target (kept as OTIO `metadata`, which other tools ignore). Nothing in v1 is beyond that. |
| Fallbacks | FCPXML 1.9–1.11 and CMX 3600 EDL, on request | Yes; EDL loses everything except V1/A1 cuts |
| Media | Never copied or embedded; relative paths from the project folder, plus FilmCraft's size and first/last-MiB fingerprint for relinking | Yes |
| Exports | MP4 (H.264/AAC); WebM/AV1/Opus later through the helper | Yes |
| Proxies, waveforms, preview segments | Cache only | No, and they do not need to: all are regenerable and safe to delete |
| Autosave and recovery | FilmCraft's `Auto-Save/` beside the project; its journal under its data dir (in the app cache) | Autosaves yes; the journal is session-only |

Rule for the lane: **if the `.fcproj` is missing or unreadable and the `.otio` is
present, `video.project.open` imports the OTIO** and says what was not in it.
That way the open sidecar is the format of last resort, not a write-only export.
This satisfies Lane 5's "no proprietary project format" within the limits
stated above. If FilmCraft's OTIO writer turns out to be import-only, the
sidecar is ours to write (about 400 lines over `describe`'s data).

**Concurrent saves:** FilmCraft's atomic save is last-writer-wins. v1 keeps one
engine session per project file. Every window, on any device, attaches to it
and watches `video.project.wait`. Two windows therefore never hold divergent
copies.

## Risks, open questions, and the spike that settles each

| # | Risk or question | Spike | Settled when |
|---|---|---|---|
| **R1** | **macOS cannot host the engine.** `sandbox.rs` reports Landlock/seccomp unavailable off Linux, and the launcher refuses to run unconfined. DESIGN.md's "engine on the Mac" cannot happen without Seatbelt or a VM. | None needed; DESIGN.md's phase 4 exit must change. | The operator accepts "engine on a Linux node". |
| **R2** | **The phone may be unable to host the engine at all.** The sandbox floor is Landlock ABI 4 (Linux 6.7) **even for a manifest with no network**, because network denial needs ABI 4 (APP-SUITE §0.6). Phone kernels are commonly 6.1 or 6.6 (**inferred**, not checked for the operator's Fold). Android's app seccomp policy may also refuse the Landlock syscalls from Termux (**unverified**). On top of that, Lane 0.5 has no Android cross-compile. | **S5:** on the Fold, in Termux, run a static aarch64 probe that calls `landlock_create_ruleset` with `LANDLOCK_CREATE_RULESET_VERSION` and reports the ABI. | The ABI is known. If it is below 4, the "engine on the Fold" half of the exit is out of reach for this design. |
| **R3** | **The monitor transport.** It is unproven that MSE and `blob:` stills work in an opaque sandboxed frame under §4.2's CSP, at a usable latency, on Chromium, Android Chrome and WebKit (where ManagedMediaSource is required). | **S1 (do first; no engine needed):** a throwaway app whose backend makes 2-second fMP4 segments and stills from a test file with the ffmpeg helper, served through invoke into a frame carrying the exact §4.2 headers. Measure p50/p95 for a scrub still and time to first playback at 360p and 720p, on loopback and over the tailnet to the Fold. Also verify `touch-action` and a single muxed SourceBuffer on WebKit. | The numbers exist, and D6 item 2 is either wanted or deferred with data. |
| **R4** | **Can FilmCraft be embedded?** An undocumented library API, a git dependency on a large alpha workspace, compile time, binary size, edition and toolchain fit, and possible hidden dependencies on a display or XDG paths. | **S2:** a `deckjack-video` skeleton that links `engine` (and what it needs) from a pinned commit, opens a session with no window, imports a clip, does a split and an undo, renders a frame on the **CPU path**, and saves. Run it confined through `OsEnv` with exactly the manifest above. Record build time, binary size and peak memory. | It works confined. Otherwise the decision is the fallback engine B, taken now rather than discovered at the exit check. |
| **R5** | **Correctness on real footage.** Codecs hand-written from specs, given phone footage: variable frame rate, 10-bit HEVC HDR, rotation metadata, edit lists in MP4. | **S3:** run the S2 binary over a corpus (a recent phone's HEVC HDR, H.264 at 60 fps with VFR, a screen recording, a GoPro file). Compare decoded frame *N* against `ffmpeg` (FilmCraft's own oracle approach) and check A/V offset after 10 minutes. | Frame-exact match, or a named list of files to refuse with a clear error. |
| **R6** | **Speed on the engine host.** FilmCraft names "speed on large footage" as a gap, and the R620 may be CPU-only. | Part of S2 and S3: proxy build time for 10 minutes of 4K HEVC; `frame.render` from a proxy; segment render time at 720p. | A table per host. v1 is viable if a proxy still returns in under about 150 ms on the host. |
| **R7** | **The export path.** Does piping raw frames and audio to the `ffmpeg` helper over a pipe and an extra fd work inside the sandbox, and does it beat FilmCraft's native H.264 on time? | **S4:** a 2-minute timeline exported both ways on the R620 and the laptop. Compare speed, file size, VMAF (ffmpeg here has libvmaf) and A/V sync. | One path is chosen per host. |
| **R8** | **Touch editing at 360 px.** Can a person actually make frame-accurate edits at that width? | **S6:** a no-build canvas timeline prototype with fake data, on the Fold folded. Task: split, trim one edge to a given timecode with the nudge buttons, move the clip after another, undo. Time it and note mis-taps. | The task succeeds without zooming the page, and the 360 px claim stands or is raised. |
| **R9** | **Interchange fidelity.** | **S7:** a 3-track cut with gain, dissolve and markers. Write `.otio` and FCPXML, open them in Kdenlive and Resolve (whichever is available), and list what survives. Import the `.otio` back. | The table under Formats is replaced by observed rows. |
| **R10** | **Memory.** No cgroup limit, a 4K decode, and the core on the same host. | Part of S2: peak memory during 4K proxy building with the thread cap set. | A documented budget, with decodes refused over it. |
| **R11** | **Upstream churn.** FilmCraft is alpha and moving fast; a fork pinned per DESIGN.md ("Separate repos … a FilmCraft fork") rots. | None; a policy decision. Pin a commit, list the crates actually linked, and re-sync on a schedule with the S3 corpus as the regression gate. | |
| **R12** | **Lane 0.4 versus app size.** The binary will be large, and an install stages a full copy. | Measured in S2. | |

**Order:** S1 and S5 first (they need no engine and gate the platform claims),
then S2 (the engine decision), then S3, S4 and S6 in parallel, then S7. Nothing
past a skeleton is written before S2 settles A versus B.

## Exit check

From the **Fold's browser**, folded (360 px), with the engine on a **Linux node**
(the laptop, then the R620), confined, with exactly the manifest above:

1. Open Video. The approval prompt names the GPU and `Videos/` read-write and
   nothing else.
2. Import two phone clips already in `Videos/`, one of them 4K HEVC. Proxies
   build in the background with progress, and scrubbing is responsive once they
   finish.
3. Make a 60-second cut with at least five edits: a split, a trim to an exact
   timecode with the nudge buttons, a move, a J-cut (unlink plus trim), and a
   cross-dissolve. Undo and redo across them.
4. Measure loudness and normalize to −14 LUFS. Play the result in the monitor
   with render-ahead, with no wrong frames at the cut points.
5. Export 1080p MP4 into `Videos/`, watch the progress, and cancel and restart
   it once. Open the export in Quick Look on the phone, check that it seeks,
   and check with `ffprobe` and a loudness meter that it measures −14 ±1 LUFS.
6. Close everything, reopen from the unfolded Fold, and find the same project
   state. Then open the sidecar `.otio` in another editor and see the same cuts.
7. Check on disk: nothing outside `Videos/` and the app's cache changed, and
   deleting the cache costs only regeneration.

**The other half of DESIGN.md's phase 4 exit, an engine on the Fold itself, is
not part of this check.** It is blocked on S5 and Lane 0.5.

## Rough size

Inferred, before S2. Lines are production code; tests are about 0.7× on top.

| Part | Lines |
|---|---|
| `deckjack-video` backend: command adapter over the engine, session-per-file, long-poll, proxy and segment cache with eviction, export orchestration and helper pipes, loudness, OTIO sidecar if needed | 4–6k Rust |
| UI: no-build JS timeline (model, hit-testing, gestures, canvas render), monitor with MSE and stills, bin, inspector, export sheet, compact and wide layouts | 5–7k JS/CSS |
| Platform asks: SDK cancellation, the fullscreen broker op, §11 cache wording | 0.3–0.6k |
| **If S2 fails (engine B):** our own timeline model, undo, ripple semantics, and ffmpeg-backed render and export | +5–8k Rust |

That is **3 to 5 lane passes** with FilmCraft, and 5 to 8 without it. **This is the
second-hardest sub-lane in the suite after the DAW.** The engine is not the
reason, if FilmCraft embeds. The touch timeline is the reason: written without
a framework, honest at 360 px, and frame-accurate. So is the monitor, which has
to work through a sandbox designed to stop exactly the kind of media streaming
an editor wants.

## Conflicts with existing documents, for the operator to rule on

1. **DESIGN.md "WebRTC program monitor"** versus APP-UI-ISOLATION's opaque frame.
   This brief recommends brokered stills and segments (above). This is a design
   change, not an implementation detail.
2. **DESIGN.md phase 4 exit**: neither "engine on the Mac" (R1) nor "engine on
   the Fold" (R2) is achievable under the current sandbox floor and build
   matrix.
3. **Decision 1's flagged exception**: proxies go through the engine, not the
   ffmpeg binary, for frame identity. The export encode keeps the binary.
4. **§11 cache path** versus the launcher's actual grant
   (`cache/apps/<app>/home`).
5. **DESIGN.md open question "rebuild FilmCraft panels in Vue, or egui to
   wasm?"**: decision 1 already answered "our UI", but app UIs are no-build
   plain JS, not Vue. That makes the timeline harder than DESIGN.md assumed.

## Sources

- FilmCraft README: <https://raw.githubusercontent.com/storytold/filmcraft/main/README.md>
- FilmCraft repository: <https://github.com/storytold/filmcraft>
- FilmCraft project files: <https://raw.githubusercontent.com/storytold/filmcraft/main/docs/project-files.md>
- FilmCraft control protocol: <https://raw.githubusercontent.com/storytold/filmcraft/main/docs/control-protocol.md>
- FilmCraft agents and headless use: <https://raw.githubusercontent.com/storytold/filmcraft/main/docs/agents.md>
- ffmpeg-next 9.0.0: <https://docs.rs/crate/ffmpeg-next/9.0.0>; ffmpeg-sys-next 8.1.0: <https://docs.rs/crate/ffmpeg-sys-next/8.1.0>; ffmpeg-the-third: <https://docs.rs/crate/ffmpeg-the-third/latest/source/README.md>
- OpenTimelineIO Rust bindings (vfx-rs, Linux only; crates.io entries are 2020 placeholders): <https://lib.rs/crates/opentimelineio>, <https://redirect.github.com/vfx-rs/opentimelineio-bind>
- webrtc-rs v0.17 freeze and Sans-I/O shift: <https://webrtc.rs/blog/2026/01/31/webrtc-v0.17.0-feature-freeze-sansio-shift.html>; str0m: <https://docs.rs/crate/str0m/latest>
- ebur128 0.1.10: <https://docs.rs/crate/ebur128/latest>
- ManagedMediaSource on iPhone (Safari 17.1): <https://webkit.org/blog/14735/>; iOS 26.2 regression: <https://bugs.webkit.org/show_bug.cgi?id=305712>
- Local: `docs/APP-SUITE.md` §0.1, §0.2, §0.4–0.6, §2, §3, §11, §12, Lane 1 (video thumbnails), Lane 4; `docs/APP-UI-ISOLATION.md` §2 (D1, D6), §4.2, §4.5, §4.6, §4.12, §7.2; `docs/DESIGN.md` (exception for streamed content, Media streams, phase 4, open questions); `crates/deckjack-core/src/apps.rs` (`SYSTEM_READ_DIRS`, `GPU_READ_WRITE_DIRS`, private home/tmp, `working_dir` resolution); `crates/deckjack-core/src/sandbox.rs` (`denied_syscalls`, `WANTED: ABI::V4`, non-Linux refusal); `crates/deckjack-protocol/src/lib.rs` (`MAX_FRAME_BYTES`, refusal of the working-directory root); `crates/deckjack-app/src/lib.rs` (no app events, `TODO(cancel)`).
