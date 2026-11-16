<!--
The userland app suite: one brief per lane, written to be handed to a
subagent whole.

Read `docs/DESIGN.md` first for the principles. This document does not repeat
them; it records what the platform *actually does today* (with citations a
lane agent can verify), the decisions already taken, and what each lane owes.

Where DESIGN.md describes an intention and the code does something narrower,
this document follows the code and says so. A lane planned against the
intention will fail at the first integration.
-->

# The userland app suite

## How to use this document

Each lane below is a self-contained brief. A lane agent is given:

- **§2 The platform as it is** — read in full, always. Every lane depends on it.
- **§3 Decisions taken** — the rules; a lane does not relitigate these.
- **its own lane section** — the goal, the commands, the refusals, the exit check.
- **§11 Conventions** and **§12 How a lane works**.

Lanes are not independent. **Lane 0 blocks almost everything** — read it before
scheduling anything else.

---

## 2. The platform as it is

An app is a `deckjack-<name>` binary plus an `app.toml`, installed under
`~/.deckjack/apps/<name>/`, discovered by scanning for manifests. It embeds its
own web UI with `rust-embed` and the core serves it at `/apps/<name>/`
(`apps.rs:101`) behind the session gate, inside an iframe. The browser never
talks to an app directly.

**The UI is plain HTML, CSS and JS with no build step.** It imports
`/kit/bus.js` and `/kit/kit.css` from the core's origin, and the shell injects
the token stylesheet into the frame so the app follows the desktop theme
without knowing a theme exists. `crates/deckjack-notes/ui/index.html` is the
worked example — read it before writing any app UI. The no-build-step rule is
load-bearing: it is what lets a binary someone copied onto a machine serve its
own UI.

**Commands are declared twice and must agree.** `app.toml` lists them and the
binary registers them; a mismatch stops the app starting. Risk levels are
`read`, `write`, `destructive`, `outbound` (`protocol/lib.rs:267`), and
`destructive`/`outbound` are what send an *agent's* call to the Assistant's
Inbox while the operator gets an ordinary confirmation.

**`[needs]`** (`protocol/lib.rs:177`) is the whole capability vocabulary:

| key | shape | reality |
| --- | --- | --- |
| `gpu` | bool | **nominal today** — see Lane 0 |
| `network` | list of hosts | **coarse**: any non-empty list opens the network whole, warned on every launch |
| `working_dir` | `[{path, write}]` | the only tree an app may ever be granted |
| `invoke_commands` | bool | lets the app *call* the bus, not merely serve it |

`lifecycle` is `on-demand` or `service` (`protocol/lib.rs:165`).

**The sandbox is Landlock ABI 4 plus a seccomp denylist.** It read-grants
`/usr /bin /sbin /lib /lib64 /etc /sys /proc/self` (`apps.rs:684`), and
Landlock's read set includes `Execute`. `fork`/`clone` are deliberately not
denied and a child inherits both Landlock and seccomp
(`sandbox.rs:163`). **So an app may exec a helper binary, and the helper is
confined too.** That is what makes CLI-backed work possible at all.

Not granted, and not close: `/proc` as a whole (the core's `environ` holds API
keys), `/dev` as a whole (`/dev/input/event*` is a keylogger), the home
directory, and `.deckjack/`.

### The gap that reorders the roadmap

**There is no byte transport.** The only way file content reaches the browser is
`files.read`, which returns it *inside a JSON command response* — base64 when
it is not valid UTF-8 — capped at 16 MiB (`files.rs:100`), with app socket
frames capped at 8 MiB (`protocol/lib.rs:104`). The core serves exactly three
HTTP routes plus the shell fallback (`http.rs:283`). There is no content route,
no range requests, no streaming.

`files.rs:99` says it itself: *"anything bigger than this should be streamed
from `Resolved::real_path` instead of buffered."* That streaming path does not
exist.

Consequence: a 40 MB RAW, a 2 GB video, an audio file to scrub, or a PDF to
page through cannot be shown at all today. **Lane 0 builds it. Quick Look is
its first consumer and its proof.**

---

## 3. Decisions taken

These were decided deliberately. A lane applies them; it does not reopen them.

1. **Creative engines are in-process Rust/C libraries, and the UI is ours.**
   Not wrappers around the ArtCraft/Storytold GUI apps, not screen-scraped
   native windows. This follows "web-native or not at all" and the FilmCraft
   engine pattern: one design system, one theme, phone and desktop from the
   same components.
   *Flagged exception to confirm:* for a long **export or transcode**, shelling
   out to the `ffmpeg` binary is strictly better than driving the libraries
   in-process — the sandbox already allows it, the binary is battle-tested, and
   a two-hour job gains nothing from being in our address space. The
   recommendation is libraries for anything the operator *scrubs or edits*, the
   binary for batch jobs that produce a file. Lane 5 states this per command.

2. **Order: finish what exists first.** File manager, text/code editor,
   terminal emulator. They are daily-use, they are partly built, and they close
   DESIGN.md phase 3's exit check ("a full day of non-agent work without
   opening a native terminal"). Quick Look is being built in the same pass
   because it is the proof of Lane 0.

3. **Full parity, touch-first, on the phone.** Every lane works on the phone
   and the Fold, including editing — not view-only. Each lane states its touch
   contract explicitly and its minimum width honestly. A lane that cannot do
   this says so in its brief rather than shipping a desktop-only panel.

---

## Lane 0 — Platform prerequisites (blocking)

Nothing in lanes 3–7 can ship without most of this. It is not an app.

**0.1 The content route.** A session-gated HTTP route that streams bytes from
the working directory: correct `Content-Type`, `Range` support (seeking is not
optional for audio and video), `ETag`/`Last-Modified`, and
`Content-Disposition` for a download. It must resolve paths through the
existing `files` resolution so the working-directory boundary and the
`.deckjack/` denial are enforced by the code that already enforces them, and it
must **not** join the pre-auth carve-out. *Being built in this pass.*

**0.2 `gpu` is nominal.** `/dev/kfd` sits in `PENDING_GPU_FILE_GRANTS`
(`apps.rs:755`) and is **not granted**, because Landlock refuses a
directory-only right on a non-directory and fails the whole ruleset rather than
one rule. An app declaring `gpu = true` gets the approval prompt and no device.
Fix: one field on `sandbox::Policy` for non-directory paths, granted with
`AccessFs::from_read(WANTED) & !AccessFs::ReadDir`. The same gap blocks
`/dev/null`, `/dev/urandom` and `/proc/cpuinfo` (`apps.rs:740`) — an app that
opens `/dev/null` gets `EACCES` today. **Blocks: any GPU work, and any library
that opens `/dev/urandom`.**

**0.3 The shell's app list is a closed union.** `web/src/stores/apps.ts` hard-codes
seven `AppId`s and their contracts. DESIGN.md says a real install scans
manifests and sends the list down the bus; that is not what the shell reads.
**Adding any new app to the dock requires making this dynamic first** — the
manifest already carries `title` and `min_width`, so the missing pieces are
`tag`, `defaultSize`, `compact`/`wide` and a transport for them.

**0.4 No install pipeline.** `deckjack install <app>` does not exist; the CLI
accepts `--listen`, `--dir`, `--force`, `unlock`, `agent` only. No signing, no
architecture selection, no update-swap. Apps are only usable if placed by hand.

**0.5 No cross-compile.** `xtask` has no `aarch64-linux-android` target, so the
Fold cannot run any app binary. **Blocks the phone half of decision 3** for
anything with a backend.

**0.6 Network granularity.** An app needing one host gets the whole network.
Until Landlock's per-port rules are used, any lane that declares `network`
should say in its brief what it would have asked for.

---

## Lane 1 — File manager

**Exists:** `deckjack-files`, plus `files.rs` in the core (2190 lines, its own
test file). This lane finishes it rather than starting it.

**Owes:** multi-select and bulk actions; move and copy with progress; a real
rename flow; sort and filter; drag and drop that also works by long-press on
touch; thumbnails for images and video (cache under `~/.deckjack/cache`,
`apps.rs:98`); "open with" that routes to Quick Look or an app; free-space and
transfer feedback; and the cross-node view DESIGN.md promises.

**Touch contract:** long-press is the context menu; selection is a mode, not a
modifier; targets are at least `--dj-touch`; the breadcrumb is scrollable and
never truncates to nothing.

**Commands:** `files.*` mostly exist. Add `files.move`, `files.copy` (both
`write`, with progress via `Ctx::progress`), `files.trash` (`destructive` —
prefer trash over delete so a mistake is recoverable), `files.thumbnail`
(`read`).

**Must not:** serve bytes itself — use Lane 0's route. Resolve paths by string
concatenation. Report a move as done before the bytes are there.

**Exit check:** a working day of file management from the phone, including
moving a folder of photos between nodes, without opening a native file manager.

---

## Lane 2 — Text and code editor

**Owes:** a real editor — syntax highlighting, find and replace, multiple
buffers, soft wrap, go-to-line, undo across saves, external-change detection
(`files.read` already returns `modified` for exactly this). Large-file
behavior must be honest: a 200 MB log opens read-only and tail-first rather
than hanging.

**Backend:** none of its own. It is `files.read`/`files.write` plus a browser
editor component. DESIGN.md names `code-server` as the heavy fallback; treat
that as a separate later app, not this lane — it is a second UI with its own
theme, which decision 1 exists to avoid.

**Touch contract:** this is the hardest touch problem in the suite and the lane
must not pretend otherwise. No virtual keyboard overlap (the viewport store
already tracks `visualViewport`); a gutter wide enough to tap; find and replace
reachable without a hardware keyboard; an explicit save affordance, because
Ctrl+S does not exist on a phone.

**Commands:** `editor.open`, `editor.save` (`write`), `editor.revert`,
`editor.find`. Keep them thin — the file operations belong to `files.*`.

**Must not:** invent a project model. One working directory, already decided.

**Exit check:** make a real commit to this repo, edited entirely from the
phone.

---

## Lane 3 — Terminal emulator

**Exists:** xterm plus a PTY, `terminal.rs` in the core, and
`TerminalPanel.vue`. Known open defects from earlier work: zombie shells after
`kill_all`, and cancellation dropped at the wiring seam.

**Owes:** reattach that survives a reload and a network change (the heartbeat
work makes this testable now); scrollback with search; copy and paste that work
on iOS Safari; a touch keyboard bar for Ctrl, Esc, Tab and arrows; multiple
sessions with a switcher; and sane resize semantics on rotate.

**Touch contract:** the keyboard bar is the lane's whole premise on a phone.
80 columns is a real minimum — below it this is a different tool, and
`apps.ts` already says so.

**Must not:** become the primary interface. DESIGN.md principle 2: the terminal
is the escape hatch, never the main path. If a lane finds itself telling the
operator to drop to a shell, that is a missing command elsewhere.

**Exit check:** a full `cargo test` run driven and read from the phone,
including interrupting it, after a wifi drop.

---

## Lane 4 — Quick Look

**Being built in this pass.** Brief kept here so later work matches it.

**Goal:** open and inspect anything without launching the app that owns it —
images, audio, video, PDF, text and source, Markdown, archives (listing), web
pages, and the common office formats via Lane 6's conversion.

**Shape:** a shell overlay, not an app binary. Deliberate: the browser already
decodes images, audio, video and PDF, so a separate binary would add a process
and a theme boundary to reach the same `<img>`. It is also cross-cutting —
reachable from Files, from the palette, and from any app — which an app-owned
window is not.

**Depends on:** Lane 0.1 entirely. Nothing here works without range requests.

**Owes:** a viewer per family chosen by MIME with an honest fallback ("no
preview for this type" beats a broken frame); keyboard and swipe navigation
through the containing folder; zoom and pan that work with two fingers;
audio/video with real seeking; a page-through PDF; "open fully" that hands off
to the owning app; and metadata worth reading (dimensions, duration, codec,
EXIF for photographs).

**Must not:** execute anything it previews. A preview of an HTML file or an SVG
is the sharpest injection surface in the product — sandbox the frame and do not
grant it script.

**Exit check:** space-bar a RAW file, a 2 GB video and a 40-page PDF from the
phone; each opens, seeks and closes without the shell stuttering.

---

## Lane 5 — Creative suite

Per decision 1: in-process engines, our UI. This is the largest lane; it is
really five, and should be scheduled as five.

**5a Photo editor.** Crop, rotate, levels, curves, masking, non-destructive
edits as a sidecar. Engine: `image`, `libraw` for RAW, `fast_image_resize`.
Needs Lane 0.2 for GPU, otherwise CPU-bound and honest about it.

**5b Vector editor.** Paths, boolean ops, text on path, SVG in and out. Engine:
`resvg`/`usvg` for rendering, `kurbo`/`lyon` for geometry. SVG as the native
format satisfies the longevity rule exactly.

**5c Converter and compressor.** The ffmpeg frontend. Per the flagged exception
in decision 1: the **binary** for export jobs, the libraries only if a preview
needs decoding. Presets, batch queue, progress, and a plain statement of what
each preset will cost in quality.

**5d Video editor.** DESIGN.md puts FilmCraft's engine here as a headless Rust
service and a touch-first timeline, and says it lands last on a proven
platform. Keep that: this lane is scheduled after 5a–5c and after Lane 0.2.

**5e DAW.** Hardest realtime constraint in the suite: audio cannot miss a
deadline. Engine: `cpal`/`symphonia`; the bus is not in the audio path, ever.
Treat Web Audio versus a native engine streaming to the browser as an open
question to settle with a spike before any UI is written.

**Must not:** any of these — invent a proprietary project format. Sidecars and
open containers, so the work survives Deckjack.

---

## Lane 6 — Office suite

**Backend is conversion, as DESIGN.md already decided:** `pandoc` and
LibreOffice headless, with ONLYOFFICE as the fallback for exact round-trips.
Both are helper binaries, which the sandbox allows.

**Owes:** read-fidelity first — open a `.docx`, `.xlsx`, `.pptx` and see it
correctly, which is most of the daily need. Then editing in open formats
(ODF/Markdown/CSV) with export back. A spreadsheet needs a real formula engine;
scope that explicitly rather than discovering it late.

**Must not:** claim round-trip fidelity it has not verified. A document that
loses its tracked changes silently is worse than one that refuses to open.
State per format what survives.

**Exit check:** receive a `.docx`, read it, comment on it and return it, from
the phone.

---

## Lane 7 — Engineering suite

The most speculative lane — nothing in DESIGN.md yet. Honest difficulty: both
halves are years of work as full products, so the lane's first job is to scope
a *useful subset*, not a competitor.

**7a CAD.** Start as a viewer: STEP, STL, DXF, with measurement. Engine
candidates: `truck` (Rust B-rep) or OpenCascade via FFI. Parametric modeling is
a separate decision, not an increment.

**7b Circuit.** KiCad's file formats are open and well documented, which makes
a viewer and a netlist-level editor tractable where a full EDA tool is not.
Schematic capture first; PCB layout is a second lane.

**Must not:** start with a 3D kernel. Prove the viewer, the file formats and
the touch interaction first.

---

## 11. Conventions every lane follows

**Command ids** are `<app>.<noun>.<verb>` or `<app>.<verb>`, lowercase, dotted.
**Risk is declared by the backend, never inferred by the UI** — DESIGN.md
principle 3. Anything that leaves the machine is `outbound`; anything an
operator cannot undo from inside Deckjack is `destructive`.

**User data is plain files in open formats** (principle 6). System state is the
core's hidden SQLite, reached only through the bus — no app opens it.

**Errors name the thing to go and fix.** "This device is offline" and "the node
is not answering" are different errands; the shell's existing wording is the
house style.

**Progress** goes through `Ctx::progress`, not log lines.

**Caches** live under `~/.deckjack/cache/<app>/` and must be safe to delete.

**Nothing is in the dock until Lane 0.3 lands.** A lane that finishes early
ships its app reachable by command and says so.

---

## 12. How a lane agent works

Tests first, red, then green — and behavior, never implementation. A test that
pins an exact coordinate or counts calls to a private helper goes green for a
rewrite that breaks the promise, which is the failure mode this project has
been bitten by repeatedly.

Read these before writing a line: `web/src/stores/bus.test.ts`,
`web/src/stores/windows.test.ts`, `web/src/shell/geometry.test.ts` for the web
style, and `crates/deckjack-core/tests/wiring.rs` for the Rust style —
especially `every_route_the_wiring_mounts_is_answered_by_its_own_handler`, which
shows how to write a test that *cannot* pass vacuously.

**Then mutation-check your own tests.** For each one, name the single-line
source change that would make it fail, apply it, watch it fail, revert, watch
it pass. 69 of this project's tests survived that check and had to be
rewritten; two were vacuous. A test you have not watched fail is a test you do
not know works.

**The defect class to design against:** a module built complete against
injected traits, unit-tested in isolation, and never constructed — tests green,
feature impossible at runtime. The structural answer is required struct fields
and a construction test, because an unread field still compiles.
