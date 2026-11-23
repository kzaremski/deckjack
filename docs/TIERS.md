# Three tiers of app UI: the plan

Status: proposed 2026-11-22. Not built, except tier 3's first pass, which is
in progress as lane 8 (`docs/lanes/8-stream.md`).

## Summary

Every Deckjack window is drawn in one of three ways, inside one window
manager, one seat and one command bus:

| Tier | Who draws it | What the app sends | Use for |
|---|---|---|---|
| **1. Declarative** | The shell, from its own components | A widget tree and patches | Most of the suite: lists, forms, documents, settings, the Inbox |
| **2. Surface** | A surface module shipped in the shell, inside a tier-1 tree | Typed data on a binary channel | Terminal, code editor, photo canvas, timeline, page view, waveform |
| **3. Streamed** | The browser's video decoder | H.264 from Deckjack's compositor | Native-only Linux apps (GIMP, KiCad) |

The decision this plan asks for: **apps stop shipping JavaScript.** An app is
a Rust process that describes its UI. The browser runs only code that
Deckjack ships and reviews once. Tier 2 is a fixed set of surface kinds in the
shell, not a way back in for per-app scripts. Tier 3 is a tier-2 surface whose
data happens to be video.

Today each app ships a hand-written page (`crates/deckjack-*/ui/`, from 185
lines for Notes to 1,735 for Photos) that runs in an iframe and talks through
`web/public/kit/bus.js`. Every security problem in `docs/APP-UI-ISOLATION.md`
exists because that page is the app's own script running on the operator's
origin. Tier 1 removes the script instead of containing it.

## Why

- **Security by absence.** An app with no script in the browser cannot run
  code as the operator. APP-UI-ISOLATION stage 4 (opaque-origin frames, the
  MessagePort broker) and the stages after it become unnecessary for migrated
  apps.
- **Everything is a command, structurally.** A button names a bus command and
  its parameters. The palette, shortcuts, agents and the UI cannot drift
  apart, because the UI is a view over the same commands.
- **Agents read the real UI.** A widget tree is semantic ("button Export,
  disabled: no photo open"). `ui.snapshot` gives agents and accessibility
  tools what the operator sees, not pixels.
- **One design system that cannot fork.** Apps get the shell's controls and
  nothing else. Density, touch sizing and theming are decided once.
- **Apps get cheaper.** One language, one state model, no duplicate state
  between a backend and its page.

What it costs is a UI toolkit. That is the main risk, and the plan caps it.

## Architecture

```
app process (Rust)                core                       shell (browser)
──────────────────                ────                       ───────────────
model ─ view() ─▶ Node tree       relays per window,         renders Node tree with
        diff ──▶ patches ───────▶ checks session + seat ───▶ Vue components
                                                              │
update(msg) ◀── events ◀───────── as_app issuer ◀──────────── semantic events only
                                                              │
surface data ─▶ binary channel ─▶ relays, seat-gated ───────▶ surface module
                                                              (canvas, WebGL,
compositor ──▶ H.264 ───────────▶ relays, seat-gated ───────▶  CodeMirror, video)
```

Shared by all three tiers, all of it already built:

- **Windows** are server state, pushed as `windows` events (the seat lane).
- **The seat** gates input and moves everything on takeover.
- **The bus** carries every action. UI events that change something become
  command invocations with `Issuer::AppUi`, so the scopes and `HARD_DENY` from
  APP-UI-ISOLATION stage 3 keep applying.
- **The app process model** stays: Landlock, seccomp, manifests, approval
  fingerprints, idle reaping.

## Tier 1: the declarative protocol

### Model

The app keeps an Elm-shaped loop in the SDK (`deckjack_app::ui`):
`view(&Model) -> Node`, `update(&mut Model, Msg)`. The SDK diffs successive
trees and sends keyed patches. The shell holds the current tree per window and
applies patches.

**Local-first interaction is the rule that decides whether this feels native.**
The browser handles typing, selection, caret, scrolling, hover, focus, pressed
states and drag feedback without asking the app. The app receives semantic
events only: `changed` (debounced, and always on blur or submit), `activated`,
`selected`, `submitted`, `moved`, `closed`. A round trip is never on the path
of a keystroke or a frame.

**Widgets bind to commands.** A button is `Button { label, command, params }`.
Pressing it dispatches that command through the bus, so the same action is in
the palette with the same enabled state. Widget-only events (`changed` on a
search field) go to the app's `update` without the bus.

### Widget catalog v1

Capped at about twenty. A new widget needs two apps that need it.

- **Layout:** Stack, Row, Split (resizable, collapses on phone), Scroll,
  Grid, Tabs, Toolbar, Spacer.
- **Content:** Text, Heading, Icon, Image (through the content route),
  Badge, Empty state, Progress.
- **Input:** Button, Icon button, Toggle, Checkbox, Text field, Text area,
  Select, Slider (local while dragging, `changed` on release, coalesced while
  moving).
- **Collections:** List and Table (virtualized, keyed, with the selection model
  from `web/public/kit/selection.js`), Tree.
- **Overlays:** Menu and context menu, Dialog and sheet (confirmations use the
  existing ticket path), Toast.
- **Surface:** the tier-2 host node.

Each widget has a compact and a wide rendering and a minimum touch size, and
reads only design tokens.

### Commands and channels

- New `ui` channel per window on the shell WebSocket, relayed by the core from
  the app socket, seat-gated like every other call.
- `ui.snapshot {window}` (read): the current tree as structured data, for
  agents and tests. Text fields marked `secret` are redacted.
- Reserve the `ui.*` namespace and add it to `HARD_DENY` for app UIs.

## Tier 2: surfaces

A `Surface { kind, props }` node inside a tier-1 tree. `kind` names a module
compiled into the shell bundle. The app feeds it through a typed binary
channel and receives typed events back.

**The fixed set, each added when its app migrates:**

| Kind | Draws | Replaces |
|---|---|---|
| `terminal` | xterm.js | `TerminalPanel.vue` |
| `code` | CodeMirror | Scribe's vendored bundle |
| `pages` | rasterized pages with pinch and swipe | Office's page view |
| `image` | zoomable image, before and after | Quick Look viewer, Photos' fallback |
| `photo-canvas` | WebGL2 with the 3D LUT shader, crop and mask handles | Photos' `ui/gl.js` |
| `timeline` | canvas timeline with touch gestures | Video's `ui/timeline.js` |
| `monitor` | stills and video segments | Video's monitor |
| `stream` | WebCodecs decode plus input capture | Lane 8's StreamPanel (tier 3) |

**Rules:**
- Only Deckjack ships surface kinds. An app cannot register one.
- A surface module gets its channel and its props, nothing else: no bus
  access, no fetch.
- A surface that needs a new capability is a change to the shell, reviewed as
  one.

## Tier 3: streamed native apps

Lane 8 is building the first pass now: a headless Smithay compositor with
XWayland, one Deckjack window per toplevel and popup, H.264 to WebCodecs,
input only from the seat holder, Linux only, a fixed 96 dpi on a logical
1920×1080 output. Its rules are in DESIGN.md, "streamed native apps".

This plan adds two things:
- **Stream is a surface kind.** Lane 8's panel moves into the surface
  framework when phase 2 builds it.
- **A launcher.** Scan `.desktop` entries on the node, show native apps in the
  dock marked as streamed, and approve each one like an app (manifest,
  fingerprint).

## What happens to existing work

- **APP-UI-ISOLATION.** Stages 0–3 stay: the proxy socket, confirmation
  tickets, the fetch-metadata gate, `[ui]` scopes and `as_app`. Tier-1 events
  reuse the stage-3 issuer and scopes. **Stage 4 and later freeze.** The iframe
  path stays only until the last app has migrated, then it is deleted with
  `web/public/kit/*.js`, `frame-bridge.ts` and `AppFramePanel.vue`.
- **App UIs.** Each migration deletes the app's `ui/` directory and moves its
  canvas code, if any, into a shell surface module.
- **The SDK** grows `deckjack_app::ui`. Its existing commands, cancellation
  and socket handling stay as they are.

## Phases

Each phase ends in a check that runs on the phone and the laptop over the
tailnet, not only locally.

### Phase 0: spike and decide (one lane)

- A minimal protocol with ten widgets, a Rust diff, and a Vue renderer.
- **Rebuild Tasks on it** (235 lines of UI today).
- Measure, over the tailnet from the phone: event round trip p50 and p95,
  patch sizes, time to first paint of a window.
- Typing in a field, scrolling a 10,000-row list, and dragging a slider must
  never wait on the network.
- **Exit:** Tasks on tier 1 is indistinguishable from today's, with numbers
  recorded. If round trips are visible in normal use, stop and rethink before
  phase 1.

### Phase 1: the toolkit (two lanes)

- The full catalog, keyboard navigation and focus order, the selection model,
  virtualization, text input with IME and selection, menus, dialogs, toasts,
  compact and wide renderings, `ui.snapshot`.
- Migrate **Notes**, **Tasks** and **Filer** (Filer is the hard test: lists,
  tables, trees, context menus, drag and drop, Quick Look).
- **Exit:** Filer on tier 1 passes its existing e2e suite, re-pointed at the
  new DOM, on Chromium and WebKit, desktop and phone.

### Phase 2: the surface framework (one lane)

- The module registry, binary channels with backpressure, and seat gating.
- Move `terminal` first (it already exists as a panel), then `code` (Scribe)
  and `pages` (Office).
- **Exit:** Scribe and Office are tier 1 plus surfaces, with no app script.

### Phase 3: tier 3 in the framework (one lane, after lane 8 merges)

- `stream` becomes a surface kind. Add the `.desktop` launcher and per-app
  approval. Check takeover with a streamed app open.
- **Exit:** open GIMP from the dock on the laptop, take over from the tablet,
  keep working.

### Phase 4: heavy surfaces (two lanes)

- `photo-canvas`, `image`, `timeline`, `monitor`. Migrate **Photos** and
  **Video**.
- **Exit:** both apps' exit checks from their briefs still pass, with no app
  script.

### Phase 5: retire the iframe path (half a lane)

- Delete the per-app page path and the kit scripts. Update DESIGN.md,
  APP-SUITE.md and APP-UI-ISOLATION.md (stage 4 marked "superseded by tier 1").
- **Exit:** no route serves app-supplied script. A test asserts it.

Lanes 5b, 5c, 5e, 6b/6c and 7 are planned against tiers 1 and 2 from the
start.

## Risks and the spike for each

| Risk | Spike | If it fails |
|---|---|---|
| Round trips make it feel remote | Phase 0's tailnet numbers | Move more state into widgets (local filtering, optimistic toggles), or drop tier 1 for that interaction |
| Text editing and IME are hard to get right | A text field and area with IME on Android Chrome and iOS Safari in phase 1 | Use `code` (CodeMirror) for anything beyond plain fields |
| Toolkit scope creep | The two-apps rule for new widgets | Say no; the app uses a surface or waits |
| Surfaces become per-app script by another name | Rule: only the shell ships kinds | Review each new kind as a shell change |
| Huge lists | 100,000 rows in phase 1, measured | Server-side windowing: the app sends only the visible range |
| `ui.snapshot` leaks secrets to agents | `secret` fields redacted, with a test | Agents read only through scopes |
| Offline or slow links | The node is often the laptop itself; measure a slow tailnet path | Tier 1 already keeps interaction local; the rest is the network |

## Rough size

| Phase | Production | Tests |
|---|---|---|
| 0 Spike + Tasks | ~1.5k (Rust SDK diff, Vue renderer, protocol) | ~1k |
| 1 Toolkit + 3 apps | ~6–8k | ~5k |
| 2 Surfaces + 3 kinds | ~2–3k | ~2k |
| 3 Stream in framework + launcher | ~1–1.5k on top of lane 8 | ~1k |
| 4 Heavy surfaces + 2 apps | ~4–5k (mostly moved, not new) | ~3k |
| 5 Retire iframes | net deletion | ~0.3k |

Inferred, not measured. Phase 1 is the one most likely to run long.

## Open questions for the operator

1. **The protocol's name.** It will appear in the SDK and the docs.
2. **Third-party surface kinds, ever?** This plan says never; an app that
   needs a new kind gets one added to the shell.
3. **Non-Linux nodes.** Tiers 1 and 2 are node-agnostic, so a Mac node can
   host tier-1 apps; tier 3 stays Linux-only. Confirm that a Mac node is still
   wanted at all.
