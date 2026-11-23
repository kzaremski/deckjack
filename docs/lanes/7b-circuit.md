## Lane 7b — Circuit (KiCad schematics: viewer and netlist-level editor)

*Scoping brief, 2026-11-16; the backend library was built on 2026-11-23 (see
"Built" below, which supersedes the brief where they differ). Facts about
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

### Built (backend library, 2026-11-23)

Crate `crates/deckjack-circuit`, a library only: no app manifest, no
commands, no UI. Those wait for tier-1 phase 1 and the tier-2 surfaces
(`docs/TIERS.md`). Plain Rust, no C dependencies; `rand` (UUIDs from the
system generator) and `thiserror` are its only dependencies.

**Modules.**
- `sexpr`: the lossless S-expression tree. Every node keeps its leading
  whitespace, every atom its raw token, so an unedited load and save is
  byte-identical and an edit rewrites only the bytes it touches. Iterative
  parser; 64 MiB and depth-256 limits; every failure is an error. A file over
  the size limit (64 MiB for a sheet, 16 MiB for the `.kicad_pro`) is refused
  from its size before any of it is read.
- `project_file`: the same guarantee for the JSON `.kicad_pro`: original text
  plus a span tree, so a setting change replaces one span (16 MiB, depth 128).
- `model`: a typed view of one `.kicad_sch` read from the tree (symbols,
  `lib_symbols` with `extends`, wires, buses, junctions, no-connects, labels,
  sheets and sheet pins, bus entries, bus aliases, KiCad 6's
  `symbol_instances`/`sheet_instances`, KiCad 7+ `instances`). The model is
  never written back; edits change the tree. Coordinates are integers in
  KiCad's 100 nm unit, so "pin on wire end" is exact. `placed_pins` is the one
  place pin positions are computed (unit, body style, rotation, mirroring),
  with each pin's outward direction; the engine and the edits both use it, and
  a UI must.
- `project`: a project from its `.kicad_pro` or root `.kicad_sch`, the sheet
  files and the hierarchy. A sheet file is loaded only when it resolves,
  lexically and through symlinks, inside the project directory; anything else
  (`..`, absolute paths, symlinks out, missing files, a sheet containing
  itself) is a warning and an empty sheet. At most 4,096 sheet instances and
  64 levels (both tested). Sheet order is KiCad's (depth first, then by page
  number).
- `conn` and `connectivity`: the connection engine, written from a reading of
  KiCad 10's `connection_graph.cpp`: items per sheet instance, subgraphs, driver
  priority and tie-breaks, same-sheet merging, weak-name collisions (`_1`),
  global labels and power symbols (global and KiCad 9's local power), hidden
  power pins in old symbols (with KiCad's legacy power-symbol value fix for
  pre-20230221 files), sheet pins and hierarchical labels, no-connects,
  KiCad 10 jumper pins and stacked pin numbers, `unconnected-(...)` names, and
  **buses**: vector buses, groups, aliases (sheet and project), bus entries,
  bus-to-bus entries, member propagation through the hierarchy, and nets with
  several bus parents. **Work limits on hostile input**: one label expands to
  at most 8,192 bus members across its groups, vectors and aliases, and an
  alias that refers to itself (directly or through another) stays a plain
  member (KiCad itself recurses without end on one); the whole project is
  capped at 2,000,000 pins, items and bus members counted per sheet instance,
  which bounds a shared sheet placed thousands of times and a many-pin library
  symbol placed many times. Past either limit the rest is left out and
  `approximate` says so. Item joins at a point, the label and junction
  on-line tests (an interval index over the lines) and same-sheet name
  matching (a name index) are linear or near-linear rather than quadratic.
- `netlist`: KiCad's S-expression netlist (format "E"): built from the engine,
  written in KiCad 10's layout (or the compact KiCad 6–9 one), read back, and
  compared (`compare`, the checks KiCad's own regression test makes, plus a
  whole-net check).
- `edit`: the v1 edits as operations on the tree, each all-or-nothing, and
  `apply_all` for a batch: set, add or remove a field (Value and Footprint also
  update KiCad 6's per-instance copies); set DNP, in-BOM and on-board; place a
  symbol from the sheet's `lib_symbols` cache; copy a symbol; add wires;
  connect two pins with a straight or single-elbow wire; **connect a pin to a
  net by label** (a 2.54 mm stub along the pin's direction and a local or
  global label); add a no-connect flag; annotate (first free number per
  prefix, hierarchy order then X then Y, each sheet instance of a shared sheet
  numbered separately; with `reset`, the units of a multi-unit part that
  shared a number keep sharing one). A placement's rotation must be 0, 90,
  180 or 270 (KiCad loads nothing else), and it takes the default body style.
- `examples/netlist.rs`: the CLI helper. `cargo run -p deckjack-circuit
  --example netlist -- PROJECT.kicad_pro` prints the netlist; `--compare
  REF.net` prints a parity report and exits non-zero on a mismatch.

**Design decisions.**
- *New nodes are clones.* KiCad's syntax drifted across 6–10 (quoted UUIDs,
  `(hide yes)` versus a bare `hide`, `dnp`, `exclude_from_sim`, tabs versus
  two spaces). A new wire, label, no-connect or symbol is a copy of one already
  in the file with its position, text and UUID changed; a new symbol's fields
  are copied from its own `lib_symbols` entry (written in the file's syntax)
  and re-indented. Only when the file has nothing to copy is a per-version
  template used. The file's version token is never changed.
- *Refusals, not upgrades.* A file newer than 20260306 (KiCad 10.0, checked
  against `sch_file_versions.h` on the 10.0 branch) is read-only; DNP on a
  pre-KiCad-7 file, removing a mandatory field, or setting Reference as a field
  (it lives in the instance tables) are errors. A failed edit, or a failed op
  in a batch, leaves every file as it was.
- *KiCad's hashes, where names depend on them.* An unannotated symbol's
  default net name embeds `KIID::Hash()`, which is Boost's `uuids::hash_value`;
  `kiid_hash32` implements the Boost 1.86+ formula (Arch builds KiCad 10.0.7
  against Boost 1.92). A KiCad built against an older Boost names those nets
  differently.
- *Two fixes found by parity*, both one-liners against KiCad's code: a bus
  wire entry does not propagate into the bus it touches
  (`SCH_BUS_WIRE_ENTRY::ConnectionPropagatesTo`), and a net with two bus
  parents is counted per parent, not per distinct member name.

**Measured parity.** KiCad 10.0.7 (`/usr/bin/kicad-cli`, installed on this
machine on 2026-11-22, so the parity test ran live) and the 34 projects in the
fixture corpus (`tests/fixtures/README.md`: KiCad's own
`qa/data/eeschema/netlists`, byte-identical to upstream, GPL-3.0-or-later).

| check | result |
| --- | --- |
| Unedited load and save, every `.kicad_sch` (68 files) and `.kicad_pro` (33) | byte-identical, all |
| Against `kicad-cli sch export netlist`, per pin: net name, pin function, pin type | **34 of 34 projects identical** |
| Against `kicad-cli`, every net by name and members | 34 of 34 |
| Against `kicad-cli`, components: reference, value, footprint, sheet path, UUID set, primary UUID | 34 of 34 |
| Against `kicad-cli`, component properties (Sheetname, Sheetfile, the placing sheet's fields, user fields, dnp, exclude_from_bom, ki_keywords, ki_fp_filters) | 211 of 211 components identical. The root sheet's name (a root-sheet component's Sheetname) follows a rule **inferred** from these fixtures, not read from KiCad's code: the project's name for files older than 20251012, the root's entry in the project file's `sheets` table for newer ones, "Root" without either |
| Against the 22 reference netlists KiCad's repository ships | 21 of 21 that KiCad's own test uses, identical; `legacy_power` (written by KiCad 6.0.10, not used by KiCad's test) identical in pin partition, differing only by KiCad 7's pin-name net naming |
| Edits on one project per format era (20200512, 20211123, 20230121, 20231120, 20240716, 20250227, 20260306), then exported by `kicad-cli` | KiCad parses the edited file and its netlist equals ours after each of: set Value, set Footprint, add a field, DNP, in-BOM off, on-board off, place and connect, copy and wire, annotate. Remove-field is checked as a byte-for-byte undo of add-field, not exported. Only KiCad 10 parses them here: whether KiCad 8 opens a 20231120/20240716 file after an edit is **unverified** (no KiCad 8 is installed) |
| Randomized corruption, seeded (`tests/hostile.rs`) | no panic. Release, two distinct seeds at 300 rounds: about 36,000 tree-level corruptions taken through model, engine and netlist writer, and about 74,000 byte-level ones through the parsers (about 1,400 of which still parsed and ran the whole pipeline). Debug (overflow checks on), one seed at 120 rounds: 7,168 tree-level and 279 byte-level full-pipeline runs. The gate runs 24 rounds |
| Speed | `complex_hierarchy` (two 46 KB files, 74 symbols, 120 symbol instances across 3 sheet instances) opens, resolves and writes its netlist in about 5 ms including process start (release build) |

The live comparison is `tests/kicad_cli.rs`; without KiCad it passes with a
"SKIPPED" line. **To run it elsewhere: `pacman -S kicad`.**

**Gaps, plainly.**
- **Net classes**: every net is written as class `Default`. KiCad resolves
  classes from project patterns, directive labels and label fields, with
  composite names (`CLASS1,CLASS3,Default`). 31 nets in 4 of 34 projects
  differ (`issue14494`, `issue16915`, `issue24220`, `multinetclasses`).
- **Net codes** differ from KiCad's on 21 nets: KiCad numbers over its
  internal net map, which holds entries this engine does not reproduce. Names,
  members and order match; codes are positional and nothing downstream should
  key on them, but a byte diff shows them.
- **The rest of the netlist file**: no `fields` block, `libraries` section,
  title blocks, per-unit `units` blocks, component classes or variants, and
  the order of a multi-unit symbol's secondary UUIDs follows KiCad's R-tree,
  which is not reproduced (the set and the primary UUID match). So the file is
  not byte-identical to KiCad's. The nets, and each component's reference,
  value, footprint, sheet path, UUIDs and properties, match on every fixture
  (above).
- **Hostile input, still open**: the line index tests diagonal lines by
  their X range and then exactly, so many overlapping diagonal lines still
  cost labels times lines; renaming many weakly driven subgraphs that share
  one name (`_1`, `_2`, ...) is quadratic in their count.
- **Not modeled in connectivity**: text variables in labels (`${VAR}`),
  KiCad 10 variants, and hit-testing within a wire's pen width (KiCad's own
  model is exact coordinates too; inferred, not checked on off-grid files).
  Bus resolution matches on all 12 fixtures that use buses but is the least exercised
  part on real designs; the engine says so in `approximate`.
- **Edits**: annotation does not group unannotated units into parts (each
  `U?` unit gets its own number; units that already share a number keep
  sharing one) and has only KiCad's default "first free number, sort by X"
  mode. A new field is hidden as its file's format hides fields: in
  `effects` up to 20240819, on the property from 20241004. The switch version
  is **inferred**: the fixtures show 20240819 and 20250227 on either side,
  and 20241004 ("use booleans for 'hide' in symbols") is the only format
  change between them about `hide`. Placement copies field positions from
  the library without KiCad's autoplacement, and labels get a simple
  justification, so new items look plainer than KiCad's. Copying a symbol
  between files, delete, rotate, mirror, drag with stretch, undo/redo,
  autosave, the content-hash save check and the netlist diff on save are not
  built (they belong with the commands).
- **Not checked**: that KiCad 10's GUI opens an edited file without a repair
  prompt (spike 4). `kicad-cli` loading and exporting each edited project is
  the closest check made. KiCad's autosave file name, `kicad-cli` under the
  sandbox (spike 5) and the corpus beyond KiCad's own QA projects (spike 1's
  open-hardware set) are still open.

**Format-bump procedure** (the brief owed one): when KiCad ships a new
schematic token, read `eeschema/sch_file_versions.h` and the parser
(`sch_io_kicad_sexpr_parser.cpp`) on that release branch; add any new
connectivity-relevant syntax to `model.rs`; add a project saved by that
release to the fixtures with its `kicad-cli` netlist; run the suite with
`kicad-cli` installed; then raise `NEWEST_KNOWN_VERSION` in `model.rs`. Until
then files with the new token open read-only.

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
