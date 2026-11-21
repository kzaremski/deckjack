## Lane 7b — Circuit (KiCad schematics: viewer and netlist-level editor)

*Scoping brief, 2026-11-16. Research only; nothing here is built. Facts about
outside projects are cited at the end; anything marked **inferred** or
**unverified** has not been checked against a running system.*

**Goal.** Open a KiCad 6–10 project from the working directory on any
device, read its schematics correctly, find out what is connected to what,
and make small, real edits from the phone: change a value or footprint,
place a part, connect a pin to a net, annotate, save. The files must stay
plain KiCad that KiCad opens with no complaint. This is *not* an EDA tool.
Wire routing by hand, the symbol editor and PCB layout are later lanes or
never.

**Honest difficulty.** The viewer is tractable, because an MIT-licensed
browser renderer for KiCad files already exists (KiCanvas). The editor is
two hard problems that look like one:

1. **Connectivity that matches KiCad's.** KiCad decides nets from
   coordinates (wire endpoints, junctions, a pin landing on a wire), from
   names (local, global and hierarchical labels, power symbols, hidden
   power pins in older symbols) and from the sheet hierarchy. Buses and bus
   aliases add a third layer. Getting nets *mostly* right takes a week.
   Getting them identical to `kicad-cli sch export netlist` on real projects
   is the long pole of the lane. A wrong netlist is worse than none, because
   a board gets fabricated from it.
2. **Writing without damage.** Every save has to round-trip unknown and
   newer constructs verbatim and keep `git diff` limited to the intended
   change.

### Useful subset (v1)

**In:**
- **Project browser.** `.kicad_pro` files under the app's grant, the sheet
  hierarchy, and sheets opened one at a time.
- **Rendering.** Schematic rendering at KiCad fidelity (stroke font, symbols
  from the file's embedded `lib_symbols` cache, drawing sheet). Pan, zoom,
  fit, and a theme that follows `--dj-*` tokens.
- **Inspection.** Tap a symbol to see its fields and pins. Tap a pin or wire
  to see its net. **Net highlight across sheets**: every member, with a
  jump to each sheet. Search by reference, value or net name.
- **Netlist.** A computed netlist (nets with ref, pin number, pin name and
  pin type), shown as a list and readable by agents.
- **Edits, all through one op batch, all undoable:**
  - set or add or remove a symbol field (Value, Footprint, Datasheet, custom
    fields, DNP and exclude-from-BOM flags);
  - place a symbol from a library, or copy one already in the sheet;
  - delete items;
  - rotate and mirror;
  - drag a symbol with its attached wire endpoints stretched along (not
    re-routed);
  - **connect a pin to a net by label**, which writes a short stub wire plus
    a local or global label. This is the touch-native way to make a
    connection;
  - add a no-connect flag;
  - add a straight or single-elbow wire between two pins;
  - annotate unannotated references (`R?` becomes the next free `R12`).
- **Save with a netlist diff.** Before writing, the UI shows which nets
  gained or lost members. Conflict detection uses a content hash, as
  `editor.save` does.
- **Autosave that KiCad can recover.** Unsaved edits are written to KiCad's
  own autosave file beside the sheet, so a crash leaves something KiCad
  itself offers to restore. That convention still holds in KiCad 10 is
  **unverified**; see spike 4.
- **Exports into the project folder:** a KiCad S-expression netlist and a
  CSV BOM. With `kicad-cli` present, also ERC, PDF/SVG plots and the
  netlist formats it supports.

**Explicitly not in v1:**
- PCB view or editing. KiCanvas can render `.kicad_pcb` read-only nearly for
  free once it is vendored; that view belongs in Quick Look later, not here.
- **Buses.** They are drawn but not resolved. A sheet that uses them gets
  its netlist marked *approximate* unless `kicad-cli` produced it.
- Creating or editing hierarchical sheets and sheet pins.
- The symbol and footprint editors, and browsing footprint libraries (the
  Footprint field is edited as text).
- Free-form wire drawing, auto-routing, and orthogonal rubber-banding on
  drag.
- Variants editing (a KiCad 10 feature), shown read-only. SPICE simulation.
- KiCad 5 legacy `.sch`, refused with "open it once in KiCad 6 or later to
  convert it".
- Importers (Altium, Eagle, and so on), ERC rule configuration, and anything
  multi-user.
- Our own full ERC. v1 checks only unconnected pins and single-pin nets; the
  real ERC comes from `kicad-cli` where it exists.

### Engine choice

**Rendering: KiCanvas, vendored and forked.** It is TypeScript with no
runtime dependencies and MIT licensed. It parses `.kicad_sch` and
`.kicad_pcb` itself, renders with WebGL2 (a Canvas2D renderer is also in
the tree), and embeds KiCad's newstroke font. Its last commit was on
2026-04-28 across 1,114 commits. It calls itself "early alpha", has no
releases, and has one principal author. It is distributed as a web
component, not a library with a stable API. So the plan is the CodeMirror
pattern from Lane 2: pin a commit, keep a small patch set (a broker-backed
VFS, a hook to read the viewport transform, token-driven colors), bundle it
with the esbuild Vite already ships into one offline ES module under
`ui/vendor/`, check it in, and have a test fail when the checked-in copy is
stale or imports anything at runtime. Bundle size is **unmeasured**; the
glyph table alone is large. **We own the fork from day one.** If upstream
stalls, nothing changes for us.

**Alternatives rejected:**
- **`kicad-cli sch export svg` per change, shown as an image.** Exact
  fidelity, but no hit-testing and a process per edit. KiCad is not packaged
  for Termux (checked: no `kicad` recipe in termux-packages), so the
  phone-hosted case would have no viewer at all. It stays as the *fidelity
  oracle* in tests.
- **KiCad's IPC API** (protobuf over nng). It talks only to a running KiCad
  GUI, its schematic endpoints are "planned" in the official docs, and
  headless use and plotting arrive in KiCad 11. Wrong shape on every axis.
- **Our own renderer in Rust compiled to wasm.** It repeats KiCanvas's
  hardest work (stroke text, KiCad's text-justification rules, symbol
  graphics) and buys nothing over owning a fork.
- **tscircuit and other code-first EDA.** Not KiCad's format, so it fails
  the longevity rule for files the operator already has.
- **Python kiutils, kicad-skip or kicad-sch-api.** These need a Python
  runtime on every node, including Termux.

**Model, edits and connectivity: Rust, in the app process.**
- **S-expression layer: write our own lossless CST** (concrete syntax tree:
  tokens, whitespace and comments kept), about 600 lines. The
  `kiutils_sexpr`/`kiutils_kicad` crates (MIT, v0.3.0, 2026-03-29,
  targeting KiCad 10 then 9) take the same approach and are worth reading.
  They are alpha with 9 stars, and `.kicad_sch` is not in their v1 public
  API list, so depending on them would mean depending on the part that is
  not promised. Vendor ideas, not the crate. `kiparse` lists schematics as
  "planned". `kicad_parse_gen` predates the S-expression schematic format.
- **Typed model** over the CST: symbols, lib_symbols, wires, junctions,
  labels, sheets, instances. Unknown nodes are kept as CST and never
  reprinted.
- **Connectivity engine**, ours. It is the only part of the lane that has
  no shortcut.
- **`kicad-cli` as an optional helper binary** for ERC, plots and an
  authoritative netlist, used when `/usr/bin/kicad-cli` exists (the Arch
  `kicad` package ships it). KiCad is GPL; running it as a separate program
  does not touch Deckjack's AGPL (inferred from DESIGN.md's own reading of
  separate programs; not legal advice).

**Pin geometry has one source.** Pin world positions (after rotation,
mirroring and unit selection) are computed in Rust and sent to the UI.
KiCanvas only paints. Otherwise there are two transform implementations,
and "tap pin 3, connect pin 4" is the bug.

### Backend commands

All are registered by `deckjack-circuit` and declared in `app.toml`. The app
name `circuit` is not a reserved core namespace (APP-UI-ISOLATION §4.6).

| id | risk | notes |
| --- | --- | --- |
| `circuit.project.list` | read | `.kicad_pro` files under the grant, newest first |
| `circuit.project.open` | read | sheet tree, file version tokens, warnings ("newer than this app understands: opened read-only") |
| `circuit.sheet.read` | read | `{path}` → sheet text for the renderer, a content-hash `version`, and the model summary the overlay needs (items with ids and bounds, pins with world positions) |
| `circuit.netlist.get` | read | whole project, or filtered by `{net}` or `{ref,pin}`; `source: "engine" \| "kicad-cli"`; `approximate: true` with reasons (buses). Progress on large hierarchies |
| `circuit.symbol.search` | read | across the schematic's cache, project `.kicad_sym` and `sym-lib-table`, and system libraries |
| `circuit.edit.apply` | write | `{path, base_version, ops[]}` → new version, a netlist diff and the undo depth. A stale base returns `status: "conflict"` as an answer, not an error. Writes the autosave file |
| `circuit.edit.undo` / `circuit.edit.redo` | write | per sheet session |
| `circuit.sheet.save` | write | content-hash conflict check like `editor.save`; `force` to overwrite. Redacted from the audit log like `files.write` |
| `circuit.sheet.revert` | write | an ordinary undoable op (Lane 2's rule), so not `destructive` |
| `circuit.annotate` | write | scope: sheet or project; only unannotated or `?` references unless `{reset: true}`, which is still undoable |
| `circuit.erc.run` | read | `kicad-cli sch erc` into TMPDIR when present, with progress; otherwise the two engine checks, labelled as such |
| `circuit.export.netlist` | write | to a file in the project; `kicad` S-expression from the engine, other formats only through `kicad-cli`. Never overwrites without `force` |
| `circuit.export.bom` | write | CSV in the project folder |
| `circuit.export.plot` | write | PDF or SVG through `kicad-cli`; refused with "KiCad is not installed on *node*" when it is absent |

No `destructive` and no `outbound` command in v1. Nothing leaves the
machine, and every change can be undone from inside Deckjack or by
`git checkout`. The edit session lives in the app process, so an idle stop
loses only the undo stack, never edits, because of the autosave file. That
is why autosave is in v1 rather than polish.

**For agents** this is a good surface. `circuit.netlist.get` plus
`circuit.edit.apply` lets an agent "add 100 nF decoupling on every VDD pin
of U3" and the operator sees a netlist diff. The commands are `write`, so
they do not queue in the Inbox. If that is too permissive for schematic
edits, it is a §3-level decision, not this lane's.

### UI and touch contract

- **Gestures.** One finger on empty canvas pans, two fingers pinch-zoom, and
  double-tap zooms in. **Tap selects.** Selection is a mode entered by
  long-press, matching the Files deviation, and the selection bar is the
  menu.
- **Hit targets.** Pins sit 2.54 mm apart, so at fit-to-screen on a phone
  dozens of pins share one `--dj-touch` circle. When a tap has more than one
  candidate inside the touch radius, a **disambiguation sheet** lists them
  ("U3 pin 7 VDD", "C4 pin 1", the wire). Taps never guess. A loupe while
  dragging shows what will be hit.
- **Connect flow, with no keyboard needed.** Tap a pin, choose **Connect**,
  then pick an existing net from the list (searchable, most recent first),
  type a new name, or tap a second pin. Names are offered from the netlist,
  so a typo cannot silently create a new net. If the operator types one
  anyway, the save diff shows it as a *new single-member net*.
- **Properties.** They open in a bottom sheet padded by the `visualViewport`
  inset, as the editor does. Save, Undo, Redo, Fit and Search are buttons,
  because Ctrl+S does not exist on a phone.
- **Placing a part.** Search the library in a sheet, tap a result, then tap
  the canvas to drop it, snapped to the 1.27 mm grid.
- **Minimum width, honestly:**
  - **360 CSS px** for viewing, inspection, net highlight and field edits;
  - **about 600 px** (unfolded Fold, tablet) before placing parts and
    wiring feel reasonable rather than possible.

  The manifest declares `compact` for the phone layout (canvas plus bottom
  sheets) and `wide` (canvas plus a side panel with the netlist and
  properties).
- **Pen.** Pointer events, so a pen is a precise finger. No pressure
  features.

### How it fits the sandbox

```toml
[needs]
working_dir = [{ path = "Hardware", write = true }]   # the operator picks the folder at approval; "." is refused by validate
# gpu: false — the browser does the WebGL; the backend is CPU-only
# network, connect_tcp, bind_tcp: none
# invoke_commands: false — the app reads its grant directly, never files.* over the bus
lifecycle = "on-demand"
```

- **Why the app process reads files itself, not `files.*` over the bus.**
  Parsing untrusted KiCad files is exactly what belongs in a confined
  process. Lane 1 refused to run ffmpeg in the unconfined core for the same
  reason. So the editor's pattern of putting commands in the core is the
  wrong precedent here.
- **The friction this creates** is real. KiCad projects usually live inside
  code repositories scattered across the tree, and a manifest grants fixed
  subtrees. v1 lives with one or more declared folders.
  - Platform ask, not v1: **per-project grants**. "Open with Circuit" on a
    folder would launch an instance confined to that folder.
  - Landlock cannot widen a running process, so this means a per-grant app
    process. That is a Lane 0-sized change to `apps.rs` and approvals.
- **Helper binary.** `/usr/bin/kicad-cli`, executable through the `/usr`
  grant and confined by inheritance (`sandbox.rs`).
  - It gets the app's `HOME` and `TMPDIR` (per-app, under the install
    cache), so its config lands in `$HOME/.config/kicad` and outputs go to
    TMPDIR before being copied into the grant.
  - **Unverified:** whether it starts at all under the policy. Fontconfig
    wants `/var/cache/fontconfig`, and `/var` is not granted, so it would
    rebuild a per-app cache slowly on first run. It may also try Python
    initialization or the session D-Bus (`/run/user` is never granted). The
    seccomp denylist (no `unshare`, `ptrace` or `bpf`) should not matter to
    it. See spike 5.
- **Libraries.**
  - System symbol libraries (`/usr/share/kicad/symbols`, from the Arch
    library packages) are readable through `/usr`.
  - The operator's **global** `sym-lib-table` under `~/.config/kicad/<ver>/`
    is **not readable**, because home is never granted. Project
    `sym-lib-table` files are honored, with `${KICADn_SYMBOL_DIR}` expanded
    by us to the system path.
  - On Termux there are no system libraries. Placing a part there is limited
    to copying a symbol already in the schematic or using a project library.
  - Never bundle the KiCad libraries. They are CC-BY-SA 4.0 with an
    exception that covers *designs*, not redistribution of the collection.
- **Caches.** `~/.deckjack/cache/circuit/` holds the library index, keyed by
  path, mtime and size, and is safe to delete.
- **Message sizes.** App socket frames are capped at 8 MiB
  (`protocol/lib.rs`, `MAX_FRAME_BYTES`). `circuit.sheet.read` returns one
  sheet, not a project, so ordinary sheets (100 KB to 2 MB, **inferred**;
  measure in spike 6) fit. A sheet over about 6 MiB is refused with a
  sentence naming the size. Edits travel as ops, far under the broker's
  1 MiB param cap and the browser socket's 256 KB inbound frame.
- **Phone hosting.** The backend is plain Rust with no C dependencies, so it
  cross-compiles to `aarch64-linux-android` once Lane 0.5 exists. There is
  no `kicad-cli` there: ERC, plots and the authoritative netlist are
  unavailable when the Fold hosts, and the UI says so per command. A phone
  as a *client* of the R620 works as soon as the app does.

**UI isolation (APP-UI-ISOLATION stage 4 and later).**
- **Fits without new authority.** The UI calls only `circuit.*`, which is
  its own scope, so `[ui] invoke` stays empty. The UI adds no approval line
  beyond `working_dir`, and the frame needs no `files.*`.
- **Sheet bytes go through the broker as ordinary command results.**
  `connect-src 'none'` rules out KiCanvas's `fetch` paths (its GitHub and
  Codeberg VFS are deleted from the fork), so the patched VFS answers from
  `circuit.sheet.read`. That is one reason the backend returns sheet text
  rather than the UI reading files.
- **Must be checked in the stage-4 probe harness** (Chromium and WebKit):
  - WebGL2 contexts in an opaque-origin sandboxed frame (**inferred**
    allowed: no permission policy governs WebGL);
  - KiCanvas's custom elements and constructed stylesheets under
    `style-src` without `'self'`;
  - whether anything in it spawns a worker (only `blob:` would be allowed).
- **D6 deferrals that bite:**
  - **Clipboard.** Copying a net name or a BOM out of the frame does not
    work (`clipboard-write=()`). Copy and paste *of symbols* inside the app
    is held in app memory and works.
  - **Downloads.** Exports write into the project folder instead, which is
    the better answer anyway.
  - **Per-app storage.** `localStorage` throws in an opaque origin. View
    state (zoom, last sheet) is either kept in the app process per window
    or lost on reload. The autosave file covers what matters.

  Nothing in v1 needs D6 reopened. A byte-stream broker op would be needed
  only for sheets over the frame cap.
- **WebRTC (§7.2.3)** is the open leak, as for every app UI. A malicious
  build of this app could send any schematic in its grant out of the frame.
  The grant is the limit, which is one more argument for a narrow
  `working_dir`.

### Formats and longevity

- **The native format is KiCad's own** (`.kicad_pro`, `.kicad_sch`,
  `.kicad_sym`). There are no sidecars and no Deckjack project file. If
  Deckjack disappears, KiCad opens everything, which is the longevity rule
  exactly.
- **Version policy:**
  - Read from KiCad 6's first token through the KiCad 10 token. The exact
    value is pinned from a KiCad 10.0.x-saved file in the corpus; the
    official format page gives only "YYYYMMDD".
  - **Write at the file's own version token, never upgrade it**, and emit
    only constructs valid at that version.
  - A file newer than the newest known token opens read-only with a reason.
  - KiCad's cadence is a major release every year (10.0.0 in March 2026,
    10.0.5 by July), and its official schematic format page already lags
    (it does not mention KiCad 10 and has visible errors). So the lane owes
    a written *format-bump procedure*: add the token, run the corpus, and
    read KiCad's own parser (`sch_io_kicad_sexpr_parser.cpp`) rather than
    trusting the docs.
- **What survives:**
  - **Every node we do not understand**, byte-for-byte, which a CST
    guarantees.
  - **Untouched regions**, byte-identical (the test: open and save with no
    edits produces zero diff over the whole corpus).
  - **New nodes**, printed in KiCad's own formatting, with fresh UUIDv4s and
    correct `instances` paths, so the edit is a small, reviewable
    `git diff`.
  - `.kicad_prl`, the per-user local settings file, is never touched.
- **What does not:** nothing should be lost. If a write would drop
  something, it is a refused save, not a silent loss. That rule is lifted
  from Lane 6.

### Risks and open questions, each with the spike that settles it

1. **Netlist parity with KiCad (the big one).** Spike: build the engine for
   flat sheets plus labels plus power symbols plus hierarchy, and diff it
   against `kicad-cli sch export netlist` over a corpus (the `kicad-demos`
   package plus about 20 open-hardware projects from v6 to v10, some
   KiCad 10 with jumpers and hop-overs). Report the percentage of projects
   with identical nets and every class of mismatch. **Go/no-go for editing.**
   If parity is not near total on bus-free designs, v1 ships as viewer plus
   field edits only, with `kicad-cli` netlists where available.
2. **KiCanvas fidelity on KiCad 10 files.** Its README already admits gaps
   (KiCad 7 custom fonts). Spike: render the corpus and compare against
   `kicad-cli sch export svg` by eye and by a perceptual diff. List the
   patches the fork owes.
3. **KiCanvas as an editing substrate.** Questions: re-parse and re-render
   cost after each edit for a 1 MB sheet on the Fold; whether the viewport
   transform is reachable without deep surgery. Spike: a 200-line overlay
   that draws a pin marker at backend coordinates, tracking pan and zoom,
   timed on the device.
4. **Lossless writer, and the autosave convention.** Spike: CST round trip
   over the corpus with zero byte diff; one edit of each op type opened in
   KiCad 10 with no "repaired" or "modified" prompt; confirm the autosave
   file name KiCad 10 recovers from.
5. **`kicad-cli` under Landlock plus seccomp.** Spike: run
   `sch export netlist` and `sch erc` through `sandbox::command` with
   exactly this manifest's policy, and record every `EACCES` (fontconfig,
   Python, D-Bus, `/var/tmp`). Settles whether the helper is viable or needs
   grants the platform does not offer.
6. **Sheet sizes against the 8 MiB frame.** Spike: measure the corpus
   distribution. If real sheets exceed it, the answer is a byte-stream
   broker op (a D6 reopening), not a bigger cap.
7. **Working-directory friction.** Open question for the operator: one
   `Hardware/` folder by convention, or the per-project grant platform
   change? No spike; a decision.
8. **Two geometry implementations** (KiCanvas TypeScript and Rust). The
   mitigation is designed in (Rust is the single source for hit-testing).
   A test cross-checks a sample of pin positions from both on the corpus.
9. **Upstream bus factor.** KiCanvas has one author and kiutils-rs has
   9 stars. Both are mitigated by forking or reading rather than depending.
   No spike.

### Exit check

From the phone, open a real hierarchical KiCad 10 project. Tap a pin on a
child sheet and see every member of its net highlighted across sheets. Add a
100 nF capacitor from the library connected to `+3V3` and `GND` by label,
change another part's value and footprint, annotate, and save. Then, on the
desktop:
- KiCad 10 opens the project with no repair prompt;
- `kicad-cli sch erc` reports no new violations;
- `kicad-cli sch export netlist` equals Deckjack's netlist;
- `git diff` shows only the intended change.

The netlist and ERC comparison are run, not reasoned.

### Rough size

| part | estimate |
| --- | --- |
| CST, typed model, writer | about 1.5k Rust |
| connectivity engine (no buses) | about 2k Rust |
| edit ops, undo, autosave | about 1.5k Rust |
| library index | about 0.6k Rust |
| `kicad-cli` adapter | about 0.4k Rust |
| commands and glue | about 0.5k Rust |
| KiCanvas fork patches | about 0.5k TS |
| overlay, edit UI, touch flows | about 2.5k JS |
| tests | about 4–5k lines plus a checked-in corpus (licenses checked per project) |

Total: about **9–10k lines of production code**.

Realistically **four to six lane passes**:
1. viewer plus inspection;
2. engine plus netlist parity;
3. writer plus field edits;
4. placement and connect flows;
5. touch polish and `kicad-cli`.

The viewer alone is one pass and useful on its own. Ship it first, and let
spike 1 decide whether the editor half goes ahead as scoped.

### Sources

- KiCanvas repository, status, KiCad 6+ support, source layout: https://github.com/theacodes/kicanvas ; license (MIT): https://raw.githubusercontent.com/theacodes/kicanvas/main/LICENSE.md ; last commit 2026-04-28 read from the GitHub API, `repos/theacodes/kicanvas/commits`
- KiCanvas introduction: https://blog.thea.codes/introducing-kicanvas/ ; KiCad's listing: https://www.kicad.org/external-tools/kicanvas/
- kiutils-rs (MIT, alpha, KiCad 10/9 target, schematic not in the v1 public API): https://github.com/Milind220/kiutils-rs ; https://docs.rs/crate/kiutils_kicad/0.3.0 ; https://docs.rs/crate/kiutils_sexpr/0.1.1
- kiparse (schematics "planned"): https://docs.rs/crate/kiparse/0.1.0 ; kicad_parse_gen: https://docs.rs/kicad_parse_gen ; serde_kicad_sexpr: https://docs.rs/serde_kicad_sexpr/0.1.0
- KiCad schematic file format: https://dev-docs.kicad.org/en/file-formats/sexpr-schematic ; S-expression intro: https://dev-docs.kicad.org/en/file-formats/sexpr-intro
- KiCad 10.0.0: https://www.kicad.org/blog/2026/03/Version-10.0.0-Released/ ; 10.0.5: https://kicad.org/blog/2026/07/KiCad-10.0.5-Release/ ; schematic features (variants, hop-overs, jumpers, groups): https://www.cnx-software.com/2026/03/22/kicad-10-release-dark-mode-graphical-drc-rule-editor-new-file-importers-and-more/
- IPC API scope (GUI-only, schematic planned, headless in 11): https://dev-docs.kicad.org/en/apis-and-binding/ipc-api/for-addon-developers/ ; https://dev-docs.kicad.org/en/apis-and-binding
- kicad-cli: https://docs.kicad.org/9.0/en/cli/cli.pdf ; https://www.mintlify.com/KiCad/kicad-source-mirror/api/cli-reference ; Arch package ships `usr/bin/kicad-cli`: https://archlinux.org/packages/extra/x86_64/kicad/files/ ; aarch64 Arch Linux ARM build: https://archlinuxarm.org/packages/aarch64/kicad
- No Termux package: GitHub API 404 for `termux/termux-packages` paths `packages/kicad`, `x11-packages/kicad` and `root-packages/kicad` (checked 2026-11-16); KiCad-on-Android via proot plus VNC only: https://mra.sh/posts/kicad-on-android
- KiCad library license and exception: https://www.kicad.org/libraries/license/ ; https://spdx.org/licenses/KiCad-libraries-exception.html
- Platform facts: `docs/APP-SUITE.md` §2, §3, Lanes 0–2 and 7; `docs/APP-UI-ISOLATION.md` §2 (D6), §4.2, §4.5, §4.6, §7.2; `crates/deckjack-core/src/apps.rs` (`SYSTEM_READ_DIRS`, per-app `HOME`/`TMPDIR`); `crates/deckjack-core/src/sandbox.rs` (`denied_syscalls`, Landlock V4); `crates/deckjack-protocol/src/lib.rs` (`MAX_FRAME_BYTES`, the `working_dir` refusal of `.`); `crates/deckjack-core/src/ws.rs` (256 KB inbound frame)
