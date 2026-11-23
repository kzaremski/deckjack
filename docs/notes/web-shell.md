# The web shell: liveness, offline honesty, layout and iOS

What the browser half of Deckjack (`web/`) must keep doing, and why. The
connection rules are the ones most likely to be "simplified" away.

## Knowing the node is gone

- **Browser JavaScript cannot see WebSocket ping/pong.** The browser answers
  protocol pings itself and exposes no API, so the server could detect a dead
  page while the page could not detect a dead server. Deckjack's own protocol
  carries an app-level `ping`/`pong` with an echoed token; do not go back to
  transport pings.
- **`readyState === OPEN` proves nothing.** A killed node, a closed lid or a
  phone changing networks sends no FIN, and the socket stays open for minutes
  while calls vanish. Only a completed round trip is evidence; any inbound
  frame counts, even one that fails to parse.
- **The budget** (`web/src/stores/bus.ts`): a probe every 8 s
  (`HEARTBEAT_MS`), dead after 26 s of silence (`SILENCE_IS_DEAD_MS`). 26 is
  more than two beats so one lost pong during a cell handoff does not tear down
  a working session. The next wake is deadline-aware; checking only when a
  probe came due once made the real worst case 34 s, and the fix moved the
  code, not the promise. Pongs are matched by token so a late one cannot
  flatter the round trip.
- Condemning a socket fails its in-flight calls at once (waiting for `onclose`
  is waiting for exactly what a dead socket will not send), and schedules the
  reconnect once, not from both the heartbeat and `onclose`.
- **`navigator.onLine` only explains a failure the bus already found**, never
  declares one: a captive portal reports `true`. It is trustworthy only when it
  says `false`. "This device is offline" and "the node is not answering" are
  different errands for the operator.

## Offline honesty

The operator does not want the app to look functional when it cannot reach the
node; it shows that it is disconnected instead.

- No optimistic change to anything the node owns while disconnected: the window
  store refuses open, close, minimize, restore and maximize, and refusals say
  which failure it is. `place()` is the exception that returns silently,
  because it runs every pointer frame; `WindowFrame` refuses to start the drag
  instead.
- `ConnectionCard` is a standing card, not a toast. With both offline and
  closed it leads with closed, the condition still true once the network is
  back.
- **The service worker caches the shell and nothing else** (`web/public/sw.js`).
  It once cached the login page as the offline shell, because a followed
  redirect is still `response.ok`; it now requires `!response.redirected`.
- **Two teardown paths become one.** `bus.close()` closed the socket with its
  handlers attached, so `onclose` rewrote "closed" to "detached" and reconnected
  forever. `close()` and `giveUp()` both go through one helper that detaches
  before closing.
- **Fix every reader of a cached value.** The top bar once showed `offline
  40ms`: the latency pill was gated on bus status, which does not change when
  the network drops, while the node label was gated on `online`.

## Commands and confirmation

- Rust is canonical for the wire protocol; the TypeScript side follows it. The
  two halves were once written by different hands and shared nothing (tag
  field, shapes, correlation). Normalized caller-facing shapes stay in
  `protocol.ts` so wire changes are absorbed in `stores/bus.ts`, not in
  components. Progress is delivered to the call that owns it, never as a free
  event.
- Confirmation: the client asks and re-invokes inside `bus.call`, so callers
  never see a confirm state; the server supplies the prompt text, so a client
  cannot describe a destructive command as something milder; a confirmed call
  the server asks about again is refused, never looped. The mechanism is a
  ticket redeemed by a `confirm` frame (docs/APP-UI-ISOLATION.md §4.7); an
  invoke carrying `confirmed` is refused.
- `ui.confirm()` answers a displaced confirmation `false`. Leaving it pending
  hung its command forever; carrying a yes over would run a destructive command
  on the strength of a different dialog.
- `CommandEntry.run()` takes optional `params`, forwarded by `dispatch`. A
  command that can only read its argument from a panel's selection is UI-only,
  whatever the registry claims (DESIGN.md principle 3).

## Windows and layout

- **Window rects are server state.** Anything changed only locally is
  overwritten by the next `windows` event and snaps back, so a layout action
  records to the server (`shell.windows.tile`). The one exception is a
  work-area resize, which re-derives rects and would otherwise send a command
  per frame.
- Floating is the default and tiling a toggle, by the operator's choice.
  Layout mode persists under `deckjack.layout` through `safeStorage()`
  (`web/src/tokens/runtime.ts`), because reading `localStorage` in a private
  window throws; only the exact string `tiling` turns tiling on.
- Tiling keeps three invariants (`web/src/stores/windows.ts`): each window's
  pre-tiling rect is saved once and restored on leaving; tiled windows refuse
  drag in the store, not only in the UI, and lose their resize grips; every
  path that moves a window (`place`, `snap`, `toggleMaximize`) holds the guard.
- `tileMasterStack` splits on the long axis: when the area is taller than wide
  the master is a band on top. A portrait panel (a rotated laptop LCD, an
  unfolded Fold) otherwise gets two tall ribbons. `tileRow` stays for the
  one-shot side-by-side command only.
- The desktop shell measures its work area from its container, not the
  viewport minus a constant.
- A lit dock button acts on the app's front-most window (`at(-1)` after
  `raise()`), not `open[0]`.

## Keyboard

- An alias (two chords, one command, both live) and a rebind (a new chord
  replaces the old) are different operations; treating them alike once evicted
  `mod+k` for the palette.
- A chord counts as modified only with a real modifier: a shift-only chord
  would fire while typing a capital letter.

## Tokens and theme color

- Tokens are read from computed CSS (`resolveToken` in `runtime.ts`), never from
  the TypeScript theme object, so an operator override in CSS reaches canvas
  and WebGPU surfaces too. Cached consumers watch `ThemeController`'s
  `revision` (the terminal re-themes xterm from it).
- The browser chrome color must match the bars (`--dj-color-raised`), not the
  content well. It lives in three places: the theme objects (`browserColor`,
  applied at runtime to an unmediated `meta[name="theme-color"]`), the two
  media-queried metas in `web/index.html` that color the page before the shell
  loads, and the same two metas in the binary-served login page
  (`auth/html.rs`). Change all three.
- The token CSS is emitted by one pure string builder used in the browser and
  under Node, so the two cannot disagree. `tsconfig.node.json` lists the token
  sources one by one; a glob pulled browser-only `runtime.ts` into a DOM-less
  build.

## iOS home-screen install

- iOS reads almost none of the web manifest. What it needs is in
  `web/index.html`: `apple-mobile-web-app-capable` (without it the icon opens a
  Safari tab), `apple-touch-icon`, `apple-mobile-web-app-status-bar-style`,
  `apple-mobile-web-app-title`, and `apple-touch-startup-image` links.
- Startup images match by exact media query (device width, height, pixel ratio,
  orientation) with no fallback, hence 36 images. `web/scripts/emit-ios-assets.sh`
  holds the device table, renders them, and splices the links between markers
  in `index.html`; edit the table and re-run, never the block by hand. The
  images are embedded in the binary, so they are palette-reduced
  (`magick ... -colors 64 -depth 8 -define png:compression-level=9 -strip
  png8:OUT`, about 1.3 MB down to 166 KiB).
- The touch icon is square and opaque: iOS applies its own mask and composites
  transparency onto black, so rounded art comes out double-rounded with dark
  corners.
- `-webkit-touch-callout: none` goes on the chrome only; on `body` it kills
  long-press selection over terminal output.
- The install assets must be readable without a session; see
  docs/notes/security.md, HTTP surface.
