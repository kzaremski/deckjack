## Lane 6 — Office suite (scoping brief)

Written 2026-11-16 against `docs/APP-SUITE.md` (§2, §3, §11, §12, Lane 6),
`docs/DESIGN.md` (the notes/documents row, the longevity rule),
`docs/APP-UI-ISOLATION.md` (accepted, not built) and the launcher in
`crates/deckjack-core/src/apps.rs` / `sandbox.rs`. Claims are tagged
**[spiked]** (run on this machine today, method given), **[cited]** (source
at the end) or **[inferred]** (reasoned, not checked). Untagged design text is
the proposal.

### Summary

- **The brief's engine plan needs one correction before anyone builds on it:
  the `soffice` binary does not start under Deckjack's sandbox.** [spiked] It
  hard-codes its single-instance pipe in `/tmp` and ignores `TMPDIR`, and
  Landlock grants an app no `/tmp`. **LibreOfficeKit (LOK)** is the same
  engine loaded in-process, and it does run under the same grants. So the
  engine is LibreOffice through LOK, in a confined worker process the app
  spawns. It is not `soffice --convert-to`.
- **Drop ONLYOFFICE from v1.** Its editor UI has to talk to its Document
  Server, and the accepted isolation design gives app frames
  `connect-src 'none'`. It also brings its own UI (against decision 1), a
  stack of nginx, RabbitMQ and PostgreSQL, and needs ports to listen on.
  DESIGN.md already allows it as a *separate service*, outside the suite.
- **Pandoc is the Markdown bridge, not the round-trip engine.** It writes
  docx from scratch using a reference document's styles, so a received
  `.docx` that goes through pandoc comes back as a different document.
- **The exit check is reachable without a round-trip engine at all.**
  "Comment on it and return it" can be a *surgical patch* of the OOXML
  package: add the comment parts, copy every other ZIP entry raw. Fidelity is
  then preserved by construction rather than hoped for. This is the
  recommended path for v1.
- **The formula engine is IronCalc**, run in the backend (Rust), not as wasm
  in the frame: the frame's planned CSP has no `'wasm-unsafe-eval'`. Writing
  an xlsx back through IronCalc can drop parts it does not model, so a save
  is gated by a loss report and never overwrites silently.
- **A platform gap blocks "open any .docx I received".** A confined app reads
  only its declared `working_dir` subtrees. Wherever the file lands, the
  office app cannot see it unless that folder is declared. v1 declares
  folders; the real fix is a per-call file handle from the core (a new
  Lane 0 item, which Lane 1's video thumbnails also need).

---

### Status: 6a first pass built (2026-11-21)

Crate `crates/deckjack-office` (app id `office`, binary `deckjack-office`).
Everything below was run on this machine (Arch, LibreOffice 26.8.1,
poppler 26.10, Linux 7.2.15, Landlock ABI 4). Pandoc is not installed and
nothing here needs it.

**What the spikes measured.**

- **S1, LOK under the real launcher: works.** The test
  (`tests/confined.rs`) installs the app the way `deckjack install` lays it
  out, builds the plan with the real `LaunchPlan::build` and `OsEnv`, and
  spawns through `Env::spawn`: Landlock *and* seccomp, not `setpriv`. docx,
  xlsx and pptx all convert to PDF. Confined, debug build: 0.4–1.9 s per
  conversion with a fresh profile, 0.5–0.7 s once fontconfig's cache exists;
  peak RSS 240–380 MiB. Unconfined with a cold profile: 2.7–2.9 s,
  430–440 MiB. The whole app (`tests/app.rs`) also runs confined end to end:
  open, pages, page image, reading view, comment, reply, save in place,
  export, all through its socket.
- **The Unix-socket gap is real.** strace on the confined worker: successful
  `connect()` to `/run/cups/cups.sock` (5), `/run/dbus/system_bus_socket` (2)
  and `/run/systemd/resolve/io.systemd.Resolve` (2). Landlock ABI 4 does not
  stop it. Pointing `CUPS_SERVER` and `DBUS_SYSTEM_BUS_ADDRESS` at paths
  that do not exist (`worker::WORKER_ENV`) removed every one of them, the
  resolver's included; the only connects left are `ENOENT` probes for
  `nvidia-mps`. That is a mitigation for this app, not a fix: any other app
  can still reach those sockets. Platform item, unchanged.
- **`RLIMIT_AS` is not a usable memory bound.** LOK will not boot under 8 GiB
  of address space on this 24-core machine (8 GiB fails, 9 GiB works) though
  it touches about 0.4 GiB. **`RLIMIT_DATA` is**: a toy docx fails at 256 MiB
  and converts at 512 MiB. The worker sets data 2 GiB, address space 16 GiB
  as a backstop, CPU 120 s, core 0, and the app kills it at 120 s wall clock.
  `tests/confined.rs` proves the data limit is applied (a 64 MiB worker
  writes no PDF).
- **No lock file.** With the source loaded in place and both lock settings
  turned back on, LOK 26.8 wrote no `.~lock` file beside it. The brief's
  spike saw one; the copy into the job directory stays anyway (it also keeps
  a read-only grant sufficient), but no test can tell the two apart here.
- **S3 on LibreOffice only.** A file patched by the app, converted by
  LibreOffice to ODT, holds both new comments with authors and initials, the
  reply threaded on its parent (`loext:parent-name`), and the range ending
  exactly after the selected words. This is a permanent assertion in
  `tests/app.rs`. **Word, Word for iOS and Google Docs are not verified**:
  none is available here.
- **S6, partly.** One page of the toy docx at 1100 px as JPEG q82: 42 KiB,
  rasterized in 20 ms. Dense real pages, phones and Tailscale latency are not
  measured.
- S2 (fidelity corpus), S5 (IronCalc) and S7 (iOS selection) were not run.
  S4 is covered by unit tests for fields, hyperlinks, tracked changes,
  entities, surrogate pairs and table cells, on documents built in the tests
  rather than files from Word.

**Built.**

- `lok.rs`: a vendored FFI (`dlopen` + `libreofficekit_hook_2`, five table
  slots, each read only when `nSize` reaches it). The layout is checked at
  run time by `getVersionInfo` returning LibreOffice's JSON.
- `worker.rs`: the job protocol (`--worker <dir>`, `job.json` in,
  `result.json` out, `_exit`), limits, one process group per job, kill on
  cancel, timeout and drop, two concurrent workers each with its own profile
  slot, the profile seeded on every job (macros off, Java off, lock files
  off, update check and crash reporting off; the engine's own records are
  wiped each time).
- `pages.rs`: PDF cache keyed by SHA-256 of the bytes plus the engine's
  version, `pdfinfo` page sizes, `pdftoppm` page images (JPEG default; there
  is no WebP in pdftoppm), 1 GiB cache ceiling.
- `ooxml/`: a hardened package reader (size, entry, per-part, total and
  ratio caps; path, case-duplicate, method and encryption refusals; DTDs and
  absurd nesting refused), one scanner shared by the reading view and the
  patcher, styles/numbering/comments/threads, the surgical patcher with a
  post-patch check (the package diff must be exactly the expected parts, and
  the comment must cover exactly the selected text, or nothing is written),
  and the CriticMarkup text for agents.
- Commands: `office.open`, `office.pages`, `office.page.render`,
  `office.text`, `office.cancel` (read); `office.comment.add`,
  `office.save.inplace`, `office.export_pdf` (write). Nothing is
  destructive; nothing overwrites.
- UI (`ui/`): page view (lazy, three pages ahead, pinch and ctrl+wheel zoom
  around the focus, double tap to fit, swipe and arrow keys, go to page),
  reading view (headings, lists with real markers, tables with spans,
  placeholders, changes as `ins`/`del` with author and date, highlighted
  comments, thread list), selection-to-comment with a bottom sheet above the
  keyboard, replies, export with Quick Look on the result, a picker over the
  granted folders. Built for stage 4: no inline script, no fetch, no content
  route, no wasm, no `window.confirm` (no `allow-modals`; saving over the
  original takes a second tap instead).
- Quick Look hands office files to Office ("Open in Office") when it is
  installed (`web/src/shell/quicklook.ts`).

**Review fixes (2026-11-21, after the first pass).** Each with a test that
was seen to fail with the fix reverted.

- **List-marker bomb, fixed.** A hostile `numbering.xml` (`w:start`
  4,000,000,000, `lowerLetter`, a `lvlText` of thirty `%1`) made `Docx::open`
  build a 153 MB marker or abort on a 4.6 GB allocation, in the app process,
  which has no memory limit. Now `w:start` is clamped to 32,767 (Word's own
  ceiling), letters past "zzzz" (item 104) are written as digits, and a
  marker is expanded in one pass and cut at 64 characters.
- **Quadratic comment handling, fixed in three places.** `text::render`
  searched all comments per root (200,000 comments: 66 s, measured by the
  review), `anchored_order` searched its output per id, and the scanner
  walked every comment marker at the end of every run (found while writing
  the test: 50,000 references took 35 s to open in a debug build). All three
  are indexed now; 50,000 comments, each referenced from the body, open and
  render in well under a second in a debug build. A reply is listed once
  even under many roots that share its parent's id. `office.text` also runs
  its open, render and serialization in `spawn_blocking`, off the runtime's
  threads.
- **Results over the frame limit, fixed.** The core drops an app's whole
  connection, every call in flight with it, on a frame over 8 MiB. A result
  over 8 MiB less 64 KiB (`app::MAX_RESULT_BYTES`) is now an error naming
  its size and what to ask for instead. `office.text` takes `only: "text"`
  or `only: "blocks"`; the reading view asks for blocks only. A
  1,100-paragraph test document over the limit with both fits with either
  alone. `office.page.render` goes through the same bound (a PNG at
  2400 px), but no test produces an image that large; only the bound itself
  is tested.
- **Folders from the installed manifest.** The refusal tells the operator to
  add a folder to Office's `app.toml` and approve again, but the app read
  the copy compiled into it, so that never worked. It now reads the
  installed `app.toml` beside its binary (the copy the core reads and the
  Landlock grant is built from), falling back to the compiled one when there
  is none or it is not Office's.
- **Comments corrected.** `Limits::default` now says `RLIMIT_DATA` is the
  memory bound and `RLIMIT_AS` only a backstop. `WORKER_ENV` now says the
  redirect removed the resolver's connect too (why that one went as well
  was not traced).
- **A vacuous assertion removed.** `tests/confined.rs` compared the working
  directory before and after, but its worker never touches it, so the check
  could not fail. The test no longer claims it. The real check, that nothing
  but the commands' own files lands in `Documents/`, is in `tests/app.rs`,
  through the app.
- Re-run after the fixes, confined, debug build: docx 517 ms / 378 MiB,
  xlsx 362 ms / 348 MiB, pptx 326 ms / 245 MiB; page 1 at 1100 px, 42 KiB
  JPEG in 28 ms.

**Not fixed, known.** The app process itself has no memory limit (only the
worker does), so any remaining unbounded allocation in the OOXML code is an
abort or the OOM killer, not an error. One such shape is left: every text
inline carries the ids of all the comment ranges open over it, so a
`document.xml` that opens many thousands of overlapping ranges and then has
many runs costs ranges × runs in memory. It was found reading the scanner
while fixing the above. It has not been measured.

**Deviations from this brief, each for a reason.**

- **Anchors are flat**: `p/N` is the Nth body paragraph in document order
  (tables and block content controls included, drawings excluded). Simpler
  than `tbl/2/r3/c1/p/0`, equally stable for an unchanged file, and the
  reader and patcher share the one scanner that counts them.
- **`office.comment.list` and `office.changes.list` are folded into
  `office.text`**, which returns blocks, comments (threaded), anchored order
  and changes in one call (`only` trims it to the text or the blocks when
  a long document does not fit one reply with both).
- **`office.pages` is separate from `office.open`.** Opening is instant
  (metadata and the survival report); laying out pages is the slow,
  cancellable part.
- **`office.export` is `office.export_pdf`.** Other targets need pandoc
  (6c) or are lossy pairs the loss report does not cover yet.
- **`[ui] invoke` is `files.list` and `files.trash`**; `files.stat` was not
  needed.
- **Cancel is the app's own** (`office.cancel {job}`): the core never
  forwards a client's cancel to an app (`wiring::DROPPED_ON_CANCEL`, and the
  SDK drops `CoreFrame::Cancel`). A job also dies with the handler future
  and at its timeout.
- **Save in place** is the UI calling `files.trash` on the original and then
  `office.save.inplace`, which refuses while anything has the target name.
  So "the original goes to the trash first" is enforced by the command, not
  trusted to the caller.
- **Repeated comments** go into the same working copy: each new version is
  written beside it, the previous version goes to the trash, and the new one
  takes its name. Every step is undoable from the trash.
- **The gesture math stayed in `office.js`** (tested there) rather than
  moving into `/kit/`, to keep shared changes out of this lane.
- **A comment is limited to one paragraph.** A selection that runs past its
  paragraph is cut at the paragraph's end, and the composer says so.

**Trying it.** `cargo xtask install` (or `--prefix DIR` for a throwaway
node, with a short path: the app sockets live under it and a Unix socket
path is limited to 108 bytes), restart the server, approve Office from the
palette ("Approve Office"), put a `.docx` in `Documents/` or `Downloads/`,
and open it from Files: Quick Look offers "Open in Office". Or open Office
from the dock and use Open. In Read, select words, tap Comment, write, Save:
a `… (commented).docx` appears beside the original. This whole path was run
on 2026-11-21 against a `--debug` node on a scratch port with Playwright
(approve, launch, pages, comment, reply, export, a deck at phone width, a
refusal outside the folders).

**Deferred.** 6b spreadsheets; 6c pandoc; the core's per-call file handles
(platform item (c)); the S2 corpus and the Carlito/Caladea fonts (not
installed here, not yet an install requirement); verification in Word and
Google Docs; phones and iOS selection; accepting or rejecting changes;
resolving, editing or deleting comments; entries in `commentsIds.xml` and
`commentsExtensible.xml` (Word regenerates them, unverified); pptx speaker
notes; a fuller survival report in the UI (today one line); the broker
transport and second socket (stage 4 and 6 of APP-UI-ISOLATION).

---

### Goal

Open the office files that arrive — `.docx`, `.xlsx`, `.pptx`, their ODF
equivalents and the legacy binary formats — and see them correctly on the
phone and the desktop. Comment on a document and hand it back in the format
it came in, with nothing else in it changed. Edit in open formats (Markdown,
CSV, ODF), and export to whatever the recipient needs. Every page states,
per format, what survives an edit, and refuses an operation that would lose
something silently.

### Useful subset for a first version

Scheduled as three sub-lanes, in this order. Each one ships alone.

**6a — Read and comment (this delivers the exit check).**
- **Page view** for docx/odt/doc/rtf, xlsx/ods/xls (as printed pages),
  pptx/odp/ppt and anything else LibreOffice can load. LOK renders to PDF,
  the PDF is rasterized per page, and the page images go to the frame. Pinch
  zoom, swipe between pages, page count and go to page.
- **Reading view** for docx only: our own OOXML reader turns `document.xml`
  into blocks (paragraphs, headings, lists, tables, images as placeholders)
  that reflow at phone width. Existing comments and tracked insertions and
  deletions are shown *as such*, with author and date. They are never
  silently accepted. This is the view comments are anchored in.
- **Add a comment and reply to one** on a docx, written as a surgical patch
  of the package. Saved by default as a sibling file
  (`Report (commented).docx`). Saving in place moves the original to the
  trash first, so it can be undone.
- **Export to PDF** (`write`), from any loadable format.
- **Plain text for agents** (`office.text`): the reading view's blocks as
  text, with comments and changes marked. This is what makes office files an
  agent tool on day one.

**6b — Spreadsheets.**
- **Grid view** of xlsx/xls/ods/csv: the cached values the file was saved
  with, number formats applied, frozen header row and column, sheet tabs.
  Reading uses the cached values, because that is what the sender saw.
- **Edit with recalculation** through IronCalc: values, formulas, insert and
  delete rows and columns. CSV saves are lossless. An xlsx save is gated by
  the loss report (see Formats).
- No charts, pivot tables, conditional-format editing or macros.

**6c — Write in open formats.**
- Markdown documents in the existing editor (Lane 2), exported through
  pandoc to docx, odt, PDF or HTML, with a per-folder reference docx for
  house styles.
- ODF in, ODF out through LOK (native, so round trips are LibreOffice's own).

**Explicitly not in v1:**
- WYSIWYG editing of docx or pptx bodies. That is ONLYOFFICE's or
  Collabora's product, and years of work.
- Accepting or rejecting tracked changes. They are shown, and listed for
  agents; resolving them is v2. The surgical patcher could do it, but every
  change type would need tests.
- Presentation editing (view and export only).
- Real-time co-editing (DESIGN.md's Yjs belongs to Notes, not here).
- Macros, in any format, ever.
- Charts in the grid.
- Mail merge.
- Printing.
- A "return it" that sends anything. Sending is `outbound` and belongs to a
  mail lane that does not exist. v1's "return" is a file the operator shares
  from Files or Quick Look.

### Engine choice

| Need | Choice | Alternatives considered | Why |
|---|---|---|---|
| Load and render any office format | **LibreOffice through LOK** (`libsofficeapp.so`'s `libreofficekit_hook_2`), in a worker subprocess | `soffice --headless --convert-to`; ONLYOFFICE x2t; docx-preview in the frame; ZetaOffice (wasm) | `soffice` fails under the sandbox [spiked]. LOK converted the same docx under the same grants in 1.8 s [spiked]. x2t is AGPL like Deckjack, but it ships inside ONLYOFFICE's tree (`/opt` or `/var/www` installs, outside the Landlock read set). docx-preview 0.4.1 (Apache-2.0) [cited] is an HTML approximation, not a layout engine. ZetaOffice is wasm, which the frame's CSP blocks, and its status since Collabora acquired Allotropia (2025) is unclear [cited]. |
| Rasterize pages | **`pdftoppm`** (poppler 26.10, already a LibreOffice dependency on Arch [spiked: `pacman -Qo`]) | LOK `paintTile`; pdf.js in the frame | `pdftoppm` is a confined helper with nothing new to install. pdf.js in an app frame would need its wasm decoders, which `script-src` without `'wasm-unsafe-eval'` blocks [inferred]. `paintTile` is the route to live editing later. |
| Reading view, comments, change listing | **Our own OOXML reader and patcher**, in Rust (`zip` with raw entry copy, `quick-xml`) | pandoc `--track-changes=all`; LOK save-as | pandoc's docx writer regenerates the document [cited], and a reported bug drops a comment on an unaccepted insertion (#9833) [cited]. LOK save-as rewrites the whole package (5 parts in, 11 parts out on a toy file [spiked]). The patcher changes only the parts it must. |
| Formula engine | **IronCalc** (`ironcalc` 0.9.1, 2026-10-21, MIT OR Apache-2.0 [cited]), in the backend | HyperFormula (GPLv3 or commercial [cited]); LOK Calc; formula.js | IronCalc is pure Rust with its own xlsx reader and writer, and is funded and active (NLnet, 2026 [cited]). HyperFormula's GPLv3 is compatible with Deckjack's AGPL-3.0, but it is JS and would run in the frame. LOK Calc is full fidelity, but it holds a whole office instance per open sheet and needs its tiled-editing API. |
| Read-only grid values | **`calamine`** or IronCalc's importer (settle in spike S5) | — | Cached values are what the sender saw. Recomputing before showing invites "my numbers changed". |
| Markdown ⇄ docx/odt/PDF | **pandoc** (Arch `pandoc-cli` 3.11, not installed on this machine [spiked: `pacman -Si`]) | LOK | pandoc's Markdown model is the right one for text-first documents. A static aarch64 build runs in Termux [cited]. |
| Exact docx/xlsx round trips | **None in v1.** ONLYOFFICE Docs and Collabora Online stay *outside* the suite. | ONLYOFFICE Document Server as a helper | See below. |

**Why ONLYOFFICE is not the fallback any more.** The lane brief named it
before APP-UI-ISOLATION was decided. Against the current platform:

1. Its editor is JavaScript that needs a WebSocket and XHR to the Document
   Server. An app frame gets `connect-src 'none'` and no socket.
   Re-enabling that is a duplex stream through the broker, which D6 turned
   down for v1.
2. It is its own UI, against decision 1.
3. It needs a server stack — nginx, RabbitMQ, PostgreSQL — with at least
   2 GB of RAM (4 GB in Docker) [cited], plus `bind_tcp`. D2 already
   restricts which ports a manifest may hold.
4. Its packages install to `/opt` or `/var/www`, outside the Landlock read
   set (`SYSTEM_READ_DIRS`, `apps.rs`).
5. The AGPL license is not the obstacle: Deckjack is AGPL-3.0-or-later
   (`Cargo.toml`). ONLYOFFICE's added §7 attribution terms are disputed
   [cited], so redistributing it needs a closer reading of the license.

DESIGN.md line 444 already treats ONLYOFFICE as a separate HTTP service.
Keep it there and open it in its own browser tab when exact editing is
needed.

**LOK in Rust.** The surface v1 needs is small: hook, `documentLoadWithOptions`,
`saveAs`, `getError`, `destroy`, plus the status callback for progress.
`libreofficekit` 0.5.0 (MIT, June 2025, dlopen-based, 16 of 949 items
documented) [cited] is a candidate. A vendored FFI of about 150 lines is the
alternative, and has fewer surprises. The spike used Python `ctypes` against
the C struct layout, which is how small this surface is.

### Backend commands

The app id is `office`, which is not a reserved core namespace per
APP-UI-ISOLATION §4.6. Every command is registered by the app binary. **None
runs LibreOffice in the core**: parsing hostile files in the unconfined
process is the mistake Lane 1 refused for video thumbnails.

| id | risk | what it does | progress |
|---|---|---|---|
| `office.open` | read | `{path}` → kind, page/sheet/slide count, a render key (SHA-256 of the bytes + engine version), and the **survival report**: parts this file holds that editing would drop (comments, tracked changes, macros present, charts, pivots, embedded objects, external links). | yes: copy → load → convert |
| `office.page.render` | read | `{path, page, width, format: webp\|png}` → one page image as base64, plus the page size. Cached by render key. Each result stays under the 8 MiB app frame (`protocol/lib.rs:104`). | per page |
| `office.text` | read | `{path, include: [comments, changes]}` → reading-view blocks, with stable anchors (`p/12`, `tbl/2/r3/c1/p/0`). | — |
| `office.comment.list` | read | Comments with anchors, authors, dates, replies, and done state (`commentsExtended`). | — |
| `office.changes.list` | read | Tracked insertions, deletions and moves, with author and date. | — |
| `office.comment.add` | write | `{path, anchor, start, end, text, author, reply_to?, dest?}`. Surgical patch. `dest` defaults to the sibling name. Never overwrites an existing file. | — |
| `office.save.inplace` | write | Replaces the source with a prepared result, after moving the original to the trash (`files.trash` semantics). `write`, because the trash is the undo (the Lane 1 precedent). | — |
| `office.export` | write | `{path, to: pdf\|docx\|odt\|md\|html\|csv, dest}`: LOK or pandoc by pair. Never overwrites. A lossy pair (e.g. docx→md) requires `accept_losses: [...]` echoing the losses the report named. | yes |
| `office.sheet.open` | read | `{path}` → sheets, dimensions, frozen panes; opens an IronCalc session held in the app (id returned, idle timeout). | yes |
| `office.sheet.cells` | read | `{session, sheet, range}` → values, formatted text, formulas. A window, never the whole sheet. | — |
| `office.sheet.edit` | write | `{session, edits[]}` → changed cells after recalculation. In memory only; nothing touches disk. `write` because it mutates session state an agent shares [inferred classification; it could be `read`, and §11 does not settle in-memory state]. | — |
| `office.sheet.save` | write | `{session, dest, format: xlsx\|csv\|ods?}`. Refuses an xlsx that would drop parts unless `accept_losses` echoes them. CSV saves only the active sheet, and says so. | yes |

Long jobs: LOK work runs in a **worker subprocess per job**
(`deckjack-office --worker`, re-exec of itself), with a wall-clock timeout,
`setrlimit` on address space and CPU (the sandbox does neither,
`sandbox.rs` module docs) and kill on cancel. Cancellation is cooperative in
this codebase (`wiring::DROPPED_ON_CANCEL` holds only `terminal.read`), so the
app must check its own cancel signal and kill the worker. Otherwise a
cancelled 300-page render runs to the end.

### UI and touch contract

Plain HTML/CSS/JS, no build step, importing `/kit/`. After APP-UI-ISOLATION
stage 4 it runs in an opaque-origin frame. Images reach it as base64 →
`blob:` (`img-src` allows `data:` and `blob:`).

- **Page view:** one page column. Two-finger pinch anchored between the
  fingers, double tap to fit width, swipe or arrows between pages, a page
  scrubber. Pages render lazily three ahead (IntersectionObserver, the Files
  thumbnail pattern), at the frame's CSS width × devicePixelRatio, capped.
  The gesture math lives in the shell's `gestures.ts`. The frame cannot
  import shell modules, so the pure functions move into `/kit/` (a small,
  testable extraction).
- **Reading view:** reflowing text at any width. Selection uses the
  platform's own handles inside the frame. A floating **Comment** button
  appears on `selectionchange`, because iOS's selection menu cannot be
  extended. The comment composer is a bottom sheet padded by the
  `visualViewport` keyboard inset (the editor's pattern). Existing comments
  show as margin markers on wide screens and as numbered chips plus a
  comments sheet on narrow ones. Tracked changes are colored and underlined
  or struck through, with author on tap.
- **Spreadsheet grid:** virtualized cells, frozen header row and column,
  and tap to select. Range selection uses drag handles on the selection
  corners: long-press starts a range, as in the Files selection mode. The
  formula bar is a bottom input with a key row: `=` `(` `)` `:` `$` `,` `;`
  `Σ`, plus a function picker. The pattern is the terminal's keyboard bar.
  Sheet tabs scroll horizontally.
- **Save is always explicit**, and the survival report is one tap away from
  every save button, in the operator's language: "This file has 3 tracked
  changes and a chart. Saving through the spreadsheet editor keeps the
  changes' text but drops the chart."

**Honest minimum widths:**

- Page view: 320 px (it is a zoomable image).
- Reading view and commenting: 360 px.
- Spreadsheet *viewing*: 360 px.
- Spreadsheet *editing* works at 360 px but is slow: about four columns
  show, and range selection by touch is the worst interaction in the suite
  after the code editor.
- `min_width = 360`; `default_width = 900` (page beside the comment list).

Decision 3 is met for reading and commenting. For spreadsheet editing it is
met in the sense that it works, not that it is pleasant, and the lane should
say that rather than claim parity.

### How it fits the sandbox

**Manifest (v1):**

```toml
[app]
name = "office"
title = "Office"
tag = "Of"
summary = "Read, comment on and export office documents"
lifecycle = "on-demand"
min_width = 360
default_width = 900
default_height = 700

[needs]
# Not "." — Manifest::validate refuses the working directory itself.
# These are the folders documents arrive in; an operator can widen it.
working_dir = [
  { path = "Documents", write = true },
  { path = "Downloads", write = true },
]
# gpu: not requested. LOK probes /dev/dri (seen in strace) and falls back to
# software rendering when it is refused [spiked: conversion succeeded without
# it]. No network, no connect_tcp, no bind_tcp.

[ui]   # APP-UI-ISOLATION §4.8, once stage 3 lands
invoke = ["files.list", "files.stat", "files.trash"]
```

Declaring `[ui] invoke` makes Office an app that needs approval (D1),
which is correct.

**Helper binaries and libraries, all under `/usr` (read and execute
granted):**

- `/usr/lib/libreoffice/program/libsofficeapp.so` and its tree (423 MiB
  installed, `libreoffice-fresh` 26.8.1 here [spiked]);
- `/usr/bin/pdftoppm` (poppler);
- `/usr/bin/pandoc`.

A child inherits Landlock and seccomp (`sandbox.rs`), so all three are
confined.

**What the spike found (Landlock reproduced with `setpriv --landlock-*`
from util-linux 2.42.4).** The rules mirrored the launcher's grants:

- read and execute on `/usr /bin /sbin /lib /lib64 /etc /sys /proc/self`;
- the `SYSTEM_READ_FILES` and `SYSTEM_READ_WRITE_FILES` lists;
- read and write on `/dev/shm`, a private `HOME`, a private `TMPDIR` and a
  work directory.

Filesystem rules only; no seccomp and no network rules.

| Run | Result |
|---|---|
| `soffice --headless --convert-to pdf` | **exit 1, no output.** Granting `/tmp` read-write alone fixes it; granting all of `/proc` does not. With `/tmp` read-only (bwrap) it prints `ERROR: no valid pipe path found`. strace shows the pipe at `/tmp/OSL_PIPE_1000_SingleOfficeIPC_*` despite `TMPDIR` being set. |
| LOK via `libreofficekit_hook_2`, `documentLoad` + `saveAs(pdf)` | **PDF produced in 1.8 s**, with no `/tmp` and no extra grant. |
| Side effect | LOK wrote `.~lock.orig.docx#` **beside the source**, so it needs write on the source's folder or it fails or warns. Mitigation: always copy the source into the private `TMPDIR` and load the copy. A read-only grant is then enough to view. `SAL_ENABLE_FILE_LOCKING=0` is reported unreliable [cited]; the expert setting `UseDocumentOOoLockfile=false` [cited] is a second belt. |
| Timing (unconfined, warm profile) | about 2.6 s per `soffice` conversion, about 4.6 s with a cold profile. With the system fontconfig cache hidden, fontconfig rebuilt 1.6 MB into the private `HOME` and the run took 2.5 s. `/var/cache/fontconfig` is not granted, so the first run per app pays this once. |
| Other accesses seen (strace, unconfined) | Connects to `/run/cups/cups.sock`, `/run/dbus/system_bus_socket`, `/run/systemd/resolve/io.systemd.Resolve`; `/tmp/hsperfdata_*` (Java); `/dev/dri/renderD128`. |

**Owed from the sandbox side:**

- **Unix-socket connects.** Landlock's filesystem rules at ABI 4 do not
  appear to govern `connect()` to an existing pathname socket [inferred;
  verify]. If they do not, a confined LibreOffice reaches CUPS, D-Bus and
  the resolver. This is a platform gap, not an office one. For this app:
  disable printing and Java in the profile, and verify with strace under
  real confinement that nothing connects.
- **Macros off.** Before any document loads, pre-seed the LOK profile's
  `registrymodifications.xcu` with macro execution disabled, Java disabled,
  and no update check or remote-content fetch. The CVE-2024-12425/12426
  class [cited] is about exactly this headless use. Pin LibreOffice at or
  above the patched 26.2.4 / 25.8.7 lines [cited]. This machine has 26.8.1.
- **UDP is unfiltered** in every mode (§2, 0.6), so "no network" means no
  TCP. LibreOffice has no reason to use UDP; the spike's strace did not
  trace UDP sends specifically.
- **Memory.** A hostile spreadsheet can ask LibreOffice for gigabytes.
  `setrlimit(RLIMIT_AS)` on the worker is the only bound available; the
  launcher sets no cgroup.

**The file-reach gap (blocking for the exit check as worded).** The core
resolves a path and hands the app a path string; the app can open it only
if Landlock granted that subtree. A `.docx` in `Projects/acme/` is
unreadable to Office unless `Projects` is declared. Options:

- (a) Declare broad folders, as in the v1 manifest above. Simple, but wide.
- (b) The bytes travel inline in the invoke. Capped at 8 MiB per app frame,
  and the broker caps params at 1 MiB, so this is too small for a deck with
  images.
- (c) **New Lane 0 item: per-call file handles.** The core resolves through
  `Namespace`, opens `O_RDONLY|O_NOFOLLOW`, and passes the fd over the app
  socket (`SCM_RIGHTS`). The app reads that one file and nothing else. It
  matches APP-UI-ISOLATION §4.11's per-call path scoping, and is the same
  primitive Lane 1's video thumbnails need ("a confined helper, which does
  not exist yet").

Recommend (a) for 6a and (c) as a scheduled platform item.

**Phones (Lane 0.5 and decision 3).** No LibreOffice for Termux was found;
the only report is a 2020 issue about `/proc` access under proot [cited].
pandoc's static aarch64 binary does run there [cited]. On the Fold, Office's
*UI* works against a backend on the R620 or the laptop. A phone-only setup
gets a refusal naming the errand: "Office needs a node with LibreOffice; this
phone can't run it. Open this from a node that has it." That is
DESIGN.md's "run the work on a node that can". It cannot be fixed inside
this lane, and Lane 0.5 (no cross-compile) blocks even the app binary on
Android today.

**UI isolation: what the broker must carry.**

- *Within v1 broker scope (§4.5), nothing new:*
  - `invoke` and `cancel` for its own `office.*` and the declared
    `files.list` / `files.stat` / `files.trash`;
  - progress for its own calls;
  - `shell.quicklook.open` (preview the exported PDF);
  - `shell.app.open` (hand a Markdown export to the editor).
- *Results are big:* page images are base64 in invoke results. A
  three-page lookahead at phone width × 3 DPR is roughly 0.5–1.5 MB per
  page as WebP [inferred]. That is the traffic §4.12's second socket exists
  for. Cap the render width and quality server-side.
- *Downloads (D6, deferred):* "return it" does not need a broker download.
  The result is a file in the working directory, and Files or Quick Look
  (shell-side, cookie-authenticated, `Content-Disposition` from the content
  route) offers it. Office calls `shell.quicklook.open` on the result, and
  the shell draws the download. **No new broker op.** On iOS the shell's
  download or share path is itself unverified (§7.2 item 8 and Lane 4's
  "on device" items).
- *Clipboard (D6, deferred):* `Permissions-Policy: clipboard-write=()`
  blocks `navigator.clipboard`. Native long-press Copy on selected text
  inside the frame is browser UI and should still work [inferred; check on
  iOS]. v1 can live without a broker clipboard.
- *wasm:* the cap-route CSP has no `'wasm-unsafe-eval'`, so IronCalc-wasm
  and pdf.js's wasm decoders cannot run in the frame [inferred from CSP
  semantics]. This brief avoids needing it. If a later version wants
  offline client-side recalculation, that is a CSP change to argue in
  APP-UI-ISOLATION, not here.
- *WebRTC (§7.2 item 3):* the Office frame holds the plaintext of
  confidential documents, and WebRTC exfiltration is open to it. The blast
  radius is the declared folders. Option (c) above shrinks it to the files
  the operator actually opened, another reason to build it.

### Formats and longevity

User data stays in the formats it arrived in. Office adds **no sidecars** in
v1: comments are stored in the docx itself (`word/comments.xml`), which is
where the recipient's Word expects them. Caches (rendered PDFs, page images,
the LOK profile, fontconfig) live under the app's private
`cache/apps/office/home`, keyed by content hash and engine version, and are
safe to delete. Renders are not reproducible across LibreOffice versions,
which is why the version is in the key.

**What survives, by format and path** (to ship as a table in the UI and
the docs):

| Format | View | Comment (surgical patch) | Save through LOK | Save through IronCalc | Through pandoc |
|---|---|---|---|---|---|
| docx | LOK layout. Not Word-exact: font substitution decides line breaks. **Carlito and Caladea (metric-compatible with Calibri and Cambria, Word's defaults) are not installed here** [spiked: `fc-list`]; only Liberation is. Make them a hard requirement of the app's install. | **Every part not touched is byte-identical** (design claim; the test is a part-by-part diff). Touched: `document.xml`, `comments.xml` (created if absent), `document.xml.rels`, `[Content_Types].xml`; plus `commentsExtended.xml` for replies and done state. Tracked changes, fields, custom XML and signatures are untouched as bytes. A digital signature is *invalidated* by any change, so refuse or warn when `_xmlsignatures/` exists. | Whole package rewritten. On a toy file (one comment, one insertion, one deletion, one field), all four survived docx→odt→docx and docx→docx [spiked]. Real documents unverified. Not offered for docx in v1 except as an export. | — | Lossy by design: styles come from the reference docx; comments and changes only with `--track-changes=all`; #9833 [cited]. Export only, never "save". |
| xlsx | LOK pages (print layout) for a quick look; the grid for real reading | — (v2: cell comments) | Rewrites; unverified | Values, formulas and basic styles survive [inferred]. Charts, pivots, drawings, VBA, external links, data validation and conditional formats: **unknown; spike S5 produces the list**, and the save refuses without an echo of it | — |
| pptx | LOK pages, speaker notes as text | — | — | — | — |
| odt/ods/odp | Native LOK | — (v2) | Native round trip | IronCalc reads xlsx only [inferred]; for ods go through LOK | Markdown export |
| doc/xls/ppt | LOK | **Refused**: we do not write legacy binary formats | Export to the OOXML/ODF equivalent only | — | — |
| csv | Grid | — | — | Lossless for values; formulas are not stored; encoding and delimiter detected and preserved | — |
| md | Editor (Lane 2) | — | — | — | Source of truth; exports are derived |

The **survival report** is computed, not asserted: list the package parts
and features before an operation, and the result's after; report the
difference. For the surgical patch the expected difference is exactly the
touched parts. Any other difference fails the operation.

### Risks and open questions, each with the spike that settles it

- **S1 — LOK under the real launcher.** The spike used `setpriv`, which
  mirrors the filesystem rules but not seccomp or Landlock network rules.
  Build a 100-line worker that loads LOK, launch it through `OsEnv` like
  `tests/sandbox.rs` does, and convert docx, xlsx and pptx to PDF. Record:
  success; files created outside `TMPDIR`; unix-socket connects (strace
  `-e connect`); time; peak RSS. *Settles:* the engine choice, and whether
  the Unix-socket gap is real.
- **S2 — Fidelity corpus.** 30 real files: Word with Calibri, tracked
  changes and comment threads; Google Docs exports; Excel with charts and
  pivots; Keynote-exported pptx. Render each with LOK, with Carlito and
  Caladea installed, and compare page counts and visual differences against
  Word's PDF of the same file where one is available. *Settles:* whether
  page view is "correct enough" to call read-fidelity done, and which
  fonts are mandatory.
- **S3 — Surgical comment round trip.** Patch comments into the S2 docx
  files. Open the results in Word (desktop and iOS), LibreOffice and Google
  Docs; check that the comment is anchored on the right text and nothing
  else changed (part-by-part byte diff). *Settles:* the exit check's core
  claim. Highest priority after S1.
- **S4 — Anchor stability.** Does a reading-view anchor (paragraph path +
  UTF-16 offsets over visible text) map back to the right run in documents
  with fields, hyperlinks, `w:sdt` content controls, smart tags and
  existing tracked changes? The patcher may have to split a run.
  *Settles:* how much of OOXML the patcher must understand (estimate: runs,
  `ins`/`del`, `hyperlink`, `fldSimple`/complex fields, `sdt`; everything
  else passes through).
- **S5 — IronCalc xlsx loss list.** Round-trip the S2 spreadsheets through
  IronCalc 0.9.x; diff parts and features; recompute every formula and
  compare against the cached values. *Settles:* the loss report's contents,
  the share of formulas IronCalc evaluates identically, and whether v1
  saves xlsx at all or only CSV and "export via LOK".
- **S6 — Pages through the broker on a phone link.** Measure page images at
  360 CSS px × 3 DPR, WebP q≈80: bytes per page, render time in `pdftoppm`,
  end-to-end latency over Tailscale from the Fold. *Settles:* the render
  width cap and lookahead, and whether this traffic needs §4.12's second
  socket.
- **S7 — iOS selection inside a sandboxed frame.** Can the operator select
  text and see a floating button in an opaque-origin frame in the iOS PWA?
  Does native Copy still work with `clipboard-write=()`? Add it to
  APP-UI-ISOLATION §8's WebKit spike rather than running a second one.
- **Concurrency and memory.** LOK holds an office instance per process.
  One job per worker, about 250–400 MB RSS each [inferred; S1 measures it].
  Cap concurrent workers at 2 on the R620; queue the rest with progress
  "waiting".
- **LibreOffice upgrades change renders.** Page images are keyed by engine
  version; a version bump invalidates the cache, which is correct and
  costs one re-render.
- **Pandoc not installed here**, and it pulls the Haskell library tree on
  Arch. It is only needed for 6c; leave it out of 6a's install
  requirements.

### Exit check

From the Fold, against a node running Office:

1. A `.docx` with an existing comment thread and tracked changes, made in
   Word and placed in `Downloads/`, opens in Office. Page view matches
   Word's own PDF of it, page for page.
2. The reading view shows the existing comments and changes, with authors.
3. Select a sentence, add a comment, and reply to the existing thread.
4. The saved `… (commented).docx` opens in Word (and in Google Docs) with
   both new comments correctly anchored, the original thread intact and the
   tracked changes still pending.
5. A part-by-part diff shows only the expected parts changed.
6. The file is handed back through the shell's share or download.

The same file in a folder Office has not declared gets a refusal that names
the folder to add (or, once (c) lands, just works).

### Rough size

| Piece | Production | Tests |
|---|---|---|
| LOK worker (FFI, profile seeding, job protocol, limits, cancel) | ~500 Rust | ~400 (real-kernel confined run, kill on cancel, timeout) |
| OOXML reader → blocks, anchors, comment and change listing | ~900 Rust | ~700 (fixture docx files, hostile ZIPs: zip bombs, path entries, huge parts) |
| Surgical patcher (comments, replies, part copy, signature refusal) | ~600 Rust | ~600 (byte-identical part diff; Word-produced fixtures) |
| Commands, render cache, survival report | ~500 Rust | ~400 |
| 6a UI (page view, reading view, comment sheet) | ~1,200 JS | ~500 |
| 6b sheets (IronCalc sessions, grid, formula bar, loss-gated save) | ~700 Rust + ~1,300 JS | ~900 |
| 6c pandoc exports and reference docs | ~300 Rust + ~200 JS | ~250 |
| Platform item (c): fd passing over the app socket | ~300 Rust | ~300 |

That is about 6.5k lines of production code and 4k lines of tests across
all three sub-lanes, plus the spikes. 6a alone (with the fd item deferred)
is about 3.7k production and 2.6k test lines, the size of Lane 1's two
passes together.

**Difficulty, plainly:**

- *6a* is tractable, because it avoids the hard problem: no layout engine
  of our own, no WYSIWYG editing.
- *6b*'s risk is IronCalc's xlsx coverage, which nobody has measured for
  this use. S5 decides whether xlsx editing ships at all.
- A real docx or pptx *editor* is not in reach of this lane at any size.
  The honest route to one is LOK's tiled editing API (what Collabora Online
  is built on) behind our own UI. That is a multi-month project and a
  separate decision, the same way parametric CAD is for Lane 7.

### Sources

- IronCalc on crates.io (0.9.1, 2026-10-21, MIT OR Apache-2.0): https://crates.io/api/v1/crates/ironcalc
- IronCalc README (engine, xlsx reader and writer, wasm bindings, "work in progress"): https://cdn.jsdelivr.net/gh/ironcalc/ironcalc@main/README.md and https://github.com/ironcalc/IronCalc
- IronCalc NLnet grant (2026): https://nlnet.nl/project/IronCalc-conditional/
- HyperFormula licensing (GPLv3 or commercial): https://hyperformula.handsontable.com/docs/guide/licensing.html
- docx-preview 0.4.1, Apache-2.0: https://registry.npmjs.org/docx-preview/latest and https://cdn.jsdelivr.net/npm/docx-preview@0.4.1/README.md
- pandoc manual (`--track-changes`, `--reference-doc`): https://pandoc.org/MANUAL.html
- pandoc issue #9833 (comment on an unaccepted insertion dropped): https://github.com/jgm/pandoc/issues/9833
- pandoc in Termux: https://github.com/jgm/pandoc/wiki/Pandoc-in-Termux
- LibreOfficeKit Rust crate `libreofficekit` 0.5.0 (MIT): https://crates.io/api/v1/crates/libreofficekit and https://docs.rs/crate/libreofficekit/latest/source/src/lib.rs
- `libreoffice-rs` (last updated 2023): https://lib.rs/crates/libreoffice-rs
- LibreOffice headless CVE-2024-12425/12426: https://codeanlabs.com/?p=1636
- LibreOffice 2026 memory-corruption fixes (26.2.4): https://forest.watch.impress.co.jp/docs/news/2117706.html; CVE-2026-4430 (25.8.7 / 26.2.3): https://www.cvelogic.com/vulnerability-list/by-assigner/security%40documentfoundation.org
- LibreOffice lock-file settings (SAL_ENABLE_FILE_LOCKING unreliable; `UseDocumentOOoLockfile`): https://ask.libreoffice.org/t/how-to-disable-lock-file/1215 and https://lists.freedesktop.org/archives/libreoffice/2023-October/091097.html
- ONLYOFFICE license and trademark policy (2026): https://onlyoffice.com/blog/2026/05/onlyoffice-license-and-trademark-policy; AGPL §7 dispute opinion: https://ie-forum.nl/documents/2026-04-14-itenrecht-nl-opinion-agplv3-logo-trap-4f02b5e1-30e9-483f-9b85-0afc90791dce.pdf
- ONLYOFFICE Docs system requirements and stack: https://helpcenter.onlyoffice.com/installation/docs-community-sys-reqs-linux.aspx, https://helpcenter.onlyoffice.com/installation/docs-community-install-windows.aspx, https://www.ssdnodes.com/learn/onlyoffice-vs-collabora-self-hosted
- ZetaOffice and the Collabora acquisition: https://blog.allotropia.de/2024/11/08/announcing-zetaoffice-a-new-libreoffice-technology-product-for-web-mobile-desktop/ and https://linuxiac.com/libreoffice-outlines-new-web-and-mobile-strategy/
- LibreOffice under Termux/proot (2020 report): https://www.Github.com/SDRausty/termux-archlinux/issues
