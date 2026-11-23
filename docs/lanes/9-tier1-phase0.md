## Lane 9 — Tier 1, phase 0: the UI protocol, spiked on Tasks

Written 2026-11-22. Governing decision: **apps stop shipping JavaScript.** An
app is a Rust process that describes its UI; the browser runs only code
Deckjack ships. Tier 1 is the first of three tiers: an Elm-shaped loop in the
SDK, keyed patches to the shell, the shell drawing with its own components.
"The UI protocol" is a working name; every identifier is spelled `ui` so a
rename is one word.

Claims are tagged **[measured]** (run on this machine, method given),
**[tested]** (a test asserts it, named), or **[inferred]** (reasoned, not
checked).

### Goal

Prove, on one real app, that a window described by a Rust process and drawn
by the shell is indistinguishable in use from the app's hand-written page,
and that **round trips are never visible in normal use**: typing, the caret,
selection, scrolling, hover, focus, pressed states and checkbox ticks never
wait on the app. The app only hears semantic events.

### What was built

**The protocol** (`crates/deckjack-protocol/src/ui.rs`, MIT/Apache like the
rest of the crate):

- `Node { key, widget, children }`. Twelve widgets: `Stack`, `Row`, `Scroll`,
  `Text`, `Heading`, `Button`, `Checkbox`, `TextField` (text, search, date;
  `secret`), `Select`, `List` (keyed, virtualized by the shell, selection
  `none|single|multi`), `Empty`, `Date`. Two more than the suggested ten:
  `Select` (Tasks' priority) and `Date` (the due date must be formatted and
  judged "late" in the *viewer's* locale and time zone, which the app, run
  with a cleared environment, does not know).
- `Action { command, params, fields, value, required, clear }`: a widget's
  bus command. Parameters are built in the shell at the moment it fires, from
  `params`, then other fields' *current local* values (`fields`), then the
  widget's own value (`value`). `required` disables the widget locally.
  `clear` names fields the shell resets after the command succeeds, in the
  window that fired it only, and only where the field still says what was
  sent (text typed while the command ran is kept).
- `Event { path, kind, value, seq }` with `kind` one of `changed`,
  `activated`, `selected`, `submitted`, `closed`, and `opened` (sent by the
  core, not the shell, when it serves a window from its mirror, so an app
  can re-read data that changed behind its back).
- `Patch`: `replace`, `props`, `insert`, `remove`, `move`, each addressed by
  a path of keys. `diff(a, b)` is keyed (children matched by key through a
  hash map, never by position) and keeps the longest run of children already
  in order in place, so one row moved anywhere is one `move`; a reorder that
  would cost more than about a million list visits to place row by row sends
  that node whole instead. `apply` replays.
- Limits, enforced where a tree comes from somebody else
  (`apply_bounded`, used by the core's mirror): depth 64 (`MAX_DEPTH`),
  250,000 nodes (`MAX_NODES`), and 8 million sibling visits of apply work per
  frame (`APPLY_BUDGET`, about 30 ms). The SDK sends the whole tree instead
  of patches estimated at over half that budget. `Node::count` and
  `Node::depth` are iterative. `normalize_keys` makes repeated sibling keys
  unique (`a`, `a~2`) so an app bug costs one confused row, not a broken diff.
  `redacted` drops `secret` field values.
- Every field at its default is omitted from the JSON (it cut the 10,000-row
  first frame from 7.7 MB to 6.3 MB with UUID ids).
- Frames: `CoreFrame::UiOpen | UiEvent | UiClose`, `AppFrame::UiPatch { window,
  rev, ack, patches }`. Revisions count per window; a frame starting with a
  root `replace` starts over, any other must be exactly one past the last.
- Manifest: `[app] tier = 1` (absent = page in a frame). Not in the
  fingerprint: switching an app to tier 1 does not re-ask for approval
  [tested: `the_tier_does_not_change_what_the_operator_approved`]. Any other
  value is refused.
- `ui` added to `RESERVED_NAMESPACES` and `HARD_DENY`.
- No protocol version bump: the core sends UI frames only to an app whose
  manifest says `tier = 1`, which an older core refuses at discovery and no
  older app could have written (crate doc, "The UI protocol frames").

**The SDK** (`crates/deckjack-app/src/ui.rs`): `trait Program { type Msg;
view(&self) -> Node; update(&mut self, Msg); event(&self, &Event) ->
Option<Msg>; command_ran(&self, cmd, params) -> Option<Msg> }` and
`App::view(init)`. One model per window. One dedicated thread runs every
window's loop in arrival order (so `update` may block on a file read). After
each update it re-renders, diffs against what the shell has, and sends the
patches; an event that asked for an `ack` is always answered, even with no
patches. `command_ran` fires after any of the app's own commands succeeds,
whoever called it (button, palette, agent): that is how a window follows a
bound button's effect without the app knowing a button exists.
`App::run` refuses to start with `tier = 1` and no view, or a view without
the tier.

**The core relay** (`crates/deckjack-core/src/uirelay.rs`): per-window
state `{ app, subscribers, mirror tree, rev }`.

- Browser → node: `ClientFrame::Ui { window, body }` on the shell's own
  WebSocket, body `{op: open|event|close}`. A connection whose hello named no
  device (an app's own tier-0 page on its own socket, an "attendant") is
  refused (`not_a_device`) before anything else: `HARD_DENY` on `ui.*` only
  governs `as_app` invokes, so without this an app's page could open, drive
  or watch another app's window [tested: `an_apps_own_page_cannot_open_drive_or_close_a_window`,
  and over the real socket in `a_ui_frame_reaches_the_relay_…`]. Every other
  frame is admitted by the single seat (`Seat::admit`, as `BusDispatch`
  does); the session is the socket's, checked at the upgrade and rechecked
  by `ws`. The production `impl UiGate for Seat` is tested against a real
  `Seat` (`the_real_seat_gates_the_channel`).
- Node → browser: `ServerFrame::Ui { window, body }` pushed only to the
  connections showing that window, and only while `Seat::admits_now`; a
  connection that lost the seat is unsubscribed.
- The first open fixes a window's app; a patch from any other app for that
  window is dropped, and reopening it as another app is refused.
- The mirror (the same patches applied in the core, in place, never
  cloned) serves a second open without asking the app for the tree (the app
  is sent an `opened` event instead, and its reload arrives as patches), a
  connection whose queue dropped a frame (sent the whole tree next), and
  `ui.snapshot {window}` → `{window, app, rev, tree}` with secrets withheld.
  The reopen's tree is delivered under the relay's lock, as patches are, so
  a patch the app emits meanwhile can never reach the shell before the older
  tree it applies to [tested, and mutation-checked by moving the delivery
  out of the lock: `a_patch_sent_while_a_window_is_reopened_never_arrives_before_its_tree`].
  A revision gap, a patch that does not fit, or a frame over the apply
  budget clears the mirror and asks the app again. A *whole* tree over a
  limit (or followed by patches that do not fit it) is refused for good: the
  shells are told (`ui_refused`, then `lost`) and the app is not asked again
  until a shell opens the window again, so a bad app cannot loop.
- App restart: shells are told `{status: "lost"}`; on reconnect every open
  window is re-requested.
- `ui.snapshot` is `read_only`; `ui.*` is hard-denied to app windows
  [tested over the real socket:
  `an_apps_window_cannot_read_another_window_through_ui_snapshot`].

**The shell** (`web/src/uiproto/`): `tree.ts` (types, immutable
path-copying `applyPatches`: unchanged subtrees keep identity so Vue skips
them), `channel.ts` (per-window state that outlives the panel; local field
values, overrides for bound widgets, list selections; `changed` debounced
250 ms and flushed on blur, submit and before any action; reopen on
reconnect; close when the workspace no longer has the window), `UiNode.vue`
(every widget, text interpolation only, no `v-html`, styled only with
`--dj-*` tokens), `UiList.vue` (keyed rows; above 200 rows, virtualized from
the scroll position with measured row height and 8 rows of overscan; above
that threshold rows are one line), `UiProtocolPanel.vue` (the new panel
kind; `panelFor` picks it for an installed app with `tier: 1`, `AppFramePanel`
stays for every other app). Bound buttons outside lists become palette
entries while their window is focused, `available()` exactly when the
button is enabled. Buttons and ticks dispatch through `bus.call(...,
{ asApp })`, the first real sender of `as_app`: the app UI's scope,
`HARD_DENY`, the seat and the confirmation ticket (bound to the `AppUi`
issuer) all apply, and `tasks.delete` is confirmed by the shell's own dialog.
List selection uses `public/kit/selection.js` itself, not a copy.

**Tasks** (`crates/deckjack-tasks/src/window.rs`, about 330 lines plus 270 of tests;
`ui/index.html`, 235 lines, deleted): the same commands, the same files, the
same order, the same "Show done" default, the same date and "late" rule
(moved to the shell's `Date` widget), the same priority marks. `app.toml`
says `tier = 1`; `/apps/tasks/` now answers 404 [tested].

### Measurements

Method: `npm run e2e -- --project=tier1-measure`
(`web/e2e/tier1/measure.e2e.ts`), headless Chromium, one machine (GPD Pocket
4), node, app and browser all local. RTT = shell sends an event → core →
app `update`/`view`/`diff` → `UiPatch` with `ack` → core applies to mirror →
shell receives, by `performance.now()` in the page. The event is ticking
"Show done" (unbound, so it is a pure UI event). 40 samples each. Latency is
injected by Playwright's WebSocket route (`web/e2e/support/latency.ts`),
half each way; no `tc`, no root. Release build (`DJ_E2E_RELEASE=1`) unless
noted. Numbers are from single runs. [measured]

Re-measured on 2026-11-22 after the review fixes (keyed diff with a hash
map and a longest-increasing-run move pass, bounded apply in place in the
core, `opened` reloads, `Action::clear`). Release only; the debug numbers of
the first run are no longer current and were not re-taken.

Event round trip, 50-task window:

| Path | p50 ms | p95 ms | max ms |
|---|---|---|---|
| direct, loopback | 13.4 | 14.8 | 15.1 |
| through the proxy, +0 ms | 15.1 | 16.3 | 16.7 |
| through the proxy, +40 ms RTT | 44.6 | 46.4 | 46.9 |
| through the proxy, +120 ms RTT | 125.2 | 128.4 | 129.3 |

The injected rows track the injected delay plus the proxy's own cost, so the
protocol adds no amplification (one frame each way per event). The first
run measured 8.7 ms direct p50; the difference between runs is not explained
(machine load is the likely cause; not investigated) [measured, single runs].

Patch sizes. Bytes are the whole WebSocket text message the shell receives,
in UTF-8: the `{"type":"ui","window":…,"body":…}` envelope included
(`channel.ts` re-serializes the parsed frame and encodes it; the first run
counted only `JSON.stringify(body).length`, which left out the envelope and
under-counted non-ASCII text). 50 tasks:

| Edit | Frames | Bytes | Patches |
|---|---|---|---|
| type a word, then pause (one debounced `changed`, ack only) | 1 | 103 | 0 |
| add a task (new row, status line) | 1 | 816 | 2 |
| complete a task, Show done off (row leaves the list, status line) | 1 | 272 | 2 |
| tick "Show done" (box, one done row inserted) | 1 | 787 | 2 |
| complete a task, Show done on (tick, strike, **one move** to the end, status) | 1 | 627 | 4 |
| whole window, first frame | 1 | 24,950 | 1 |

"Add a task" is smaller than in the first run (1,406 bytes, 5 patches)
because the add row is no longer cleared by bumping three field revisions in
the tree; the shell clears it locally (`Action::clear`). "Complete a task,
Show done on" was not in the first run; a reviewer measured the old diff at
49 moves and 4,254 bytes for it.

Time to first paint (`show` to first tree applied), release:

| Case | ms |
|---|---|
| cold: window opens, on-demand app spawns, 50 tasks | 42 |
| warm: page reload, app running, served from the core's mirror | 2 |
| 10,000 tasks, cold | 438 (857 from navigation to visible) |

10,000 tasks: first frame 4.65 MB (short ids; 6.3 MB with UUID ids,
`ten_thousand_tasks_fit_in_one_frame`, under the 8 MiB frame limit with about
25% headroom). 18 rows in the DOM. In that window:

- Ticking "Show done": 49 ms round trip for a 223-byte, one-patch frame
  (273 ms in the first run). Completing the first task with Show done on
  (its row moves past the other 9,999): 108 ms from the click to the status
  line updating, one 629-byte frame of 4 patches (a reviewer measured the
  old diff at 9,999 moves, 888,802 bytes, for this edit).
- Where the time went before: diffing 10,000 unchanged rows was quadratic
  (a linear search for every child). Scratch bench against the crate,
  release, after the fix: `diff(a, a.clone())` 9.2 ms at 10,000 rows (0.7 ms
  at 1,000); first row ticked and moved last 14.9 ms → 2 patches, 152 bytes;
  `apply_bounded` of that 0.4 ms. A reviewer's bench of the old diff gave
  103 ms for the identical case at 10,000 rows. That the 273 → 49 ms drop
  is mostly the diff is [inferred], not profiled.
- A hostile 95,000-insert frame (8 MB) took the old apply 32.9 s under the
  relay's lock (reviewer's measurement); `apply_bounded` refuses it after
  33 ms (scratch bench, `Err(Budget)`), and the core asks the app for the
  whole tree instead.

**Never waits on the network** — `web/e2e/shell.e2e.ts`, "typing, ticking
and scrolling never wait for the network, at 500 ms round trip", in all four
shell projects (desktop and phone, Chromium and WebKit), with 10,000 tasks
and 250 ms added each way [tested]. Strengthened after review, because the
first version could only fail on the browser's own native behavior:

- Typing: the field shows "local" within a 250 ms assertion window. Then the
  route's frame counter must show exactly **one** upstream UI frame for the
  five keystrokes (the debounced `changed`), and after the node's answer has
  come back the text is still "local" with the caret at 5.
- Scrolling: `scrollTop` to row 5,000; after two animation frames row 5,000
  is in the DOM, fewer than 200 rows are, it took under 500 ms, and the
  route counted **zero** UI frames in either direction.
- Ticking a row: checked within 250 ms; sampled in the page every 20 ms from
  the click until the row is removed, it is never unticked; the row leaving
  the list (the app's consequence) took at least 500 ms; and the field still
  holds "local" after those patches landed.
- Ticking "Show done" on and off 50 ms apart (inside one round trip): sampled
  every 20 ms for 2 s, the box never shows ticked, although the app's answer
  to the first tick arrives after the second.

Mutation-checked: with the old settle rule (an unbound box's override drops
on any tree change) the last check fails, showing the box ticked again for
about 120 ms; with `changed` flushed on every keystroke the frame count is 5,
not 1. Both mutations pass the first version of this test.

Unit-level, against the real shell stores over a fake socket
(`UiProtocolPanel.test.ts`, `channel.test.ts`): Add enables as you type with
no frame sent; the app hears the text only after 250 ms, or at once on blur;
an echo of old text never overwrites what was typed; a refused tick goes
back; Add flushes the pending `changed` before the invoke; the app's answer
to an earlier tick does not flip a box back; a tick sent while disconnected,
refused by the seat, or in flight across a reconnect goes back to what the
app has; a bound box's commands run one at a time in tick order; `clear`
keeps text typed while the command ran; a list's app-set selection applies
on first render, takes one key in a single-select list, and ignores an echo
older than the latest click; regaining the seat reopens the windows on
screen.

**Pending: the phone over the tailnet.** Not measured: it needs the
operator's phone. `web/scripts/tier1-rtt.mjs` runs the same RTT measurement
against any node from a laptop (`DJ_NODE`, `DJ_COOKIE`), and its header has
a two-bookmark recipe that measures it on the phone itself, by the phone's
clock (`localStorage["deckjack.uiStats"]="1"`, tick Show done 40 times, read
`__djUiStats`). The script was syntax-checked, not run against a remote node.

### Is Tasks on tier 1 indistinguishable from before?

**Functionally, yes; visually, nearly.** Same commands, same files, same
sort, same Show-done default, same late-date rule, same priority marks,
destructive delete still confirmed by the shell. The e2e adds, completes,
reveals and deletes on desktop and phone in Chromium and WebKit and checks
the `.ics` files. Screenshots of both versions on the same node and data,
desktop and Pixel 7, were compared by eye [measured, method: build the old
`deckjack-tasks` from `ff26a4c` and the new one, install each into a scratch
node, screenshot]. Differences found:

- The old page's toolbar rendered **dark with unreadable text** under the
  current theme (its `kit.css` reaches for `--dj-color-surface-raised`, not in
  the theme, and falls back to a dark constant), and its Add button was black.
  The tier-1 window uses the shell's tokens and is consistent. This is a fix,
  but it is a visible difference.
- Metadata (priority marks, dates) is a little smaller (`type.micro` vs the
  page's `0.75rem`); checkbox-to-text spacing is the shell's.
- After clicking **Add**, focus stays on the button; the page moved it back
  to the summary field. After **Enter** in the field both keep focus there.
  Not built: a widget-level focus request.
- Long summaries wrap as before, but only while the list has 200 rows or
  fewer; above that rows are one line with an ellipsis (virtualization needs
  uniform height).

### Are round trips ever visible in normal use?

**At 50 tasks: no**, by measurement up to 120 ms RTT: what the operator does
is local, and the app's consequence (a row leaving, the strike-through, the
list filtering) lands one network round trip plus about 13–15 ms later,
which is what the old page also paid (it made *two* bus calls per action,
the command then `tasks.list`). Ticking twice inside a round trip no longer
shows a stale answer (it did before the review: the box re-ticked itself for
a round trip). **At 10,000 tasks: barely**, for consequences only: a filter
round trip is about 50 ms and a re-sorting tick about 110 ms locally,
release (down from about 0.3 s); the 0.4–0.9 s first paint of a 4.6 MB
first frame is visible. Typing, scrolling and ticking stay local at any
size.

### Decision

Tier 1 works for list-and-form apps of normal size: proceed to phase 1 and
migrate the next small app. Two findings shape phase 1 rather than block it:

1. **The first frame grows with the list.** A 10,000-row window is a
   4.6–6.3 MB first frame; re-renders are now about 50–110 ms at that size.
   Phase 1 should add windowed lists (the app sends rows for a range; the
   shell asks for more as it scrolls) before any app with unbounded lists
   (Mail, Files) moves, and should consider memoized subtrees in `view`.
2. **The phone measurement is still owed** before claiming the round trip
   is invisible on the device that matters most.

### Deferred

- Windowed/paged lists; row templates to shrink repeated rows.
- Async effects in `update` (an app's own bus calls from the UI loop, Elm's
  `Cmd`); Tasks reads its folder directly and does not need them.
- Focus requests, images (only through the content route, not built), menus,
  dialogs, split panes, keyboard shortcuts declared by the app.
- Seat move for an open window: the relay stops sending to a connection that
  lost the seat (tested against the real `Seat`), and the shell's channel
  reopens the windows on screen when a `seat` event says it holds the seat
  again (unit-tested in `channel.test.ts`). Not e2e-tested with a tier-1
  window open across a takeover.
- The relay still has one mutex for every window. Its work per frame is now
  bounded (apply budget, no clone), but a 30 ms frame for one window still
  delays every other window's frames by up to that; per-window locks were not
  built.
- An unbound box whose event the app never acknowledges (the app is alive
  but silently drops it) keeps the operator's value until a reconnect, an
  error frame or the app's loss; there is no timeout.
- Panel remount on the same connection does not send `opened` (no new
  `open` is sent), so it does not re-read the folder; a page reload or another
  device opening the window does. The old page re-read on every iframe load.
- Event fan-out filtering, and per-window event scopes: windows only ever
  hear their own tree.
- The `uiStats` hook (`localStorage["deckjack.uiStats"]`) is a measurement
  convenience; decide whether to keep it.

### Changes outside the new modules (for the merge with the stream lane)

- `deckjack-protocol/src/lib.rs`: `pub mod ui`; `AppInfo.tier`,
  `TIER_UI_PROTOCOL`, its validation; `"ui"` in `HARD_DENY` and
  `RESERVED_NAMESPACES`; `CoreFrame::{UiOpen, UiEvent, UiClose}`;
  `AppFrame::UiPatch`; crate doc section.
- `deckjack-app/src/lib.rs`: `pub mod ui`, `App::view`, `AppError::{NoView,
  ViewWithoutTier}`, UI frames in the frame loop, `command_ran` after a
  handler succeeds.
- `deckjack-core/src/ws.rs`: `ClientFrame::Ui`, `ServerFrame::Ui`, trait
  `UiRelay`, `WsCtx::attach_ui` (a `OnceLock` field; the constructor is
  unchanged), `Hub::send_ui`, one arm in `handle`.
- `deckjack-core/src/apphost.rs`: `Outgoing::UiFrame`, `AppHost::attach_ui`,
  `impl UiApps for AppHost`, the `UiPatch` arm in `pump`, reopen/lost calls
  in `attend`.
- `deckjack-core/src/wiring.rs`: `"tier"` in `system.apps`.
- `deckjack-core/src/seat.rs`: `Seat::names_device(connection)` (one public
  read, added by the review fixes, for refusing attendants on the ui
  channel).
- `deckjack-core/src/main.rs` and `tests/end_to_end.rs`: build `UiChannel`
  from hub and seat, attach to the app host and the socket, register
  `ui.snapshot`.
- `deckjack-core/src/lib.rs`: `pub mod uirelay`.
- Web: `protocol.ts` (`as_app` on invoke/confirm, `UiUp`/`UiDown`, `isUi`,
  `ServerEvent` `ui`); `stores/bus.ts` (`asApp` option, `ui()` sender, `ui`
  frames emitted); `stores/apps.ts` (`tier`, `isDescribed`);
  `components/panels/index.ts` (`panelFor(app, apps)`);
  `components/AppSurface.vue`; `shell/context.ts` and
  `testing/shell-harness.ts` (`uiproto` on `Shell`); `env.d.ts` (types for
  `public/kit/selection.js`).
- E2E: `tasks` in `APPS`; `seedTasks`; `support/latency.ts`;
  `DJ_E2E_RELEASE=1` switch in `paths.ts`/`global-setup.ts`; project
  `tier1-measure`.
