## Lane 8 — Streamed native apps, window by window (scoping brief)

Written 2026-11-22 against the operator's decision of the same day (now in
`docs/DESIGN.md`: principle 1 and the second exception under non-goals),
`docs/APP-UI-ISOLATION.md` (decision D6), the single seat
(`crates/deckjack-core/src/seat.rs`, `wiring::BusDispatch`) and the
launcher (`crates/deckjack-core/src/apps.rs`, `sandbox.rs`). Claims are
tagged **[spiked]** (run on this machine today, method given), **[cited]**
(source at the end) or **[inferred]** (reasoned, not checked). Untagged
design text is the proposal. The machine: Arch, Linux 7.2.15, AMD Ryzen AI
9 HX 370 (GPD Pocket 4), Xwayland 24.1.14, libopenh264 2.6.0, ffmpeg 9.0.2,
Chromium 156 and WebKit 27.2 as Playwright ships them.

### Summary

- **It works under the real sandbox.** [spiked] Deckjack's own headless
  compositor (`deckjack-stream`, Smithay with the pixman renderer), spawned
  through the real `LaunchPlan::build` and `Env::spawn` (Landlock *and*
  seccomp), runs a GTK 4 app natively on Wayland and the same app as an X11
  client through XWayland. Its window streams as H.264, keys typed through
  the link reach the app, its menus come through as popups placed against
  their window, and closing the window asks the app to close.
- **Two upstream gaps had to be closed, one with a vendored patch.**
  Smithay's XWayland launcher hard-codes `/tmp/.X11-unix` and
  `/tmp/.X<n>-lock` (0.7.0 and master as of 2026-11-19) [cited], and the
  sandbox grants no `/tmp`. Deckjack carries a trimmed smithay 0.7.0 in
  `vendor/smithay` with one addition, `XWayland::spawn_abstract`: an
  abstract socket only, nothing on disk, plus a cookie (`-auth`) because an
  abstract socket has no file permissions. Xwayland's own GL probe crashed
  inside NVIDIA's EGL library under the sandbox [spiked]; `-shm` (no
  glamor) and pointing libglvnd at Mesa fixed it.
- **The encoder is the system's libopenh264, on the CPU.** [spiked] A
  1920x1080 picture costs 18–26 ms as a keyframe and 10–15 ms when only a
  caret changed, on one thread; a 300x252 dialog under a millisecond. The
  crate's bundled source, built here without nasm, is 3–4x slower. x264
  (through ffmpeg) is about 2x faster again and the upgrade path if CPU
  matters; it was not chosen for v1 (see Engine choice).
- **WebKit decodes AVC, not Annex B.** [spiked] Both engines' `VideoDecoder`
  say an Annex B config is supported; WebKit then fails every chunk with
  `EncodingError`. With an `avcC` description and length-prefixed NAL units
  both Chromium and WebKit decode the compositor's pictures correctly. The
  stream panel always converts.
- **The stream view is a shell panel with its own socket.** D6 refused
  open-ended duplex streams into app frames, so pictures and input never
  touch an app iframe or the command bus: each streamed window's panel
  opens `/stream?window=<id>`, which the core admits only for a signed-in
  session (cookie, `Origin`) that holds the seat, re-checks on every input
  message and every 2 s, and closes when the seat moves. `stream.*` is
  reserved and hard-denied to app UIs, like `seat.*`.
- **The Unix-socket gap is closed for native apps, not for other apps.**
  The first pass found a confined process could connect to the operator's
  own X server with no cookie; review found it could reach every Unix
  socket of the operator's uid, the session bus among them, and run
  commands unconfined through `systemd-run --user`. Native apps are now held
  to Landlock ABI 9 (`LANDLOCK_ACCESS_FS_RESOLVE_UNIX`) plus the ABI 6
  abstract-socket and signal scopes; a kernel without them refuses native
  apps. Measured on this machine (kernel ABI 10): the same probes are
  refused from inside the sandbox and succeed with the scoping removed.
  Every other app is still at ABI 4 with the gap open; that is a platform
  decision, recorded under "Second pass" below.

---

### Status: first pass built (2026-11-22)

Everything below was run on this machine. "Confined" means the real
launcher: `LaunchPlan::build` from a manifest, `OsEnv::spawn`, Landlock ABI 4
rules plus the seccomp denylist, with the app as the compositor's child.
Since the second pass (below) native apps also get the ABI 9 Unix-socket
rule and the ABI 6 scopes (`sandbox::Ipc::Scoped`), through
`stream::native_plan`; the S1 tests run under that.

**What the spikes measured.**

- **S1, the compositor under the real launcher: works, Wayland and X11.**
  `crates/deckjack-stream/tests/confined.rs` (permanent tests, not a
  one-off) launches `zenity --entry` (GTK 4) and reads the link the way the
  core does. Debug build of the compositor:
  - Wayland: first picture (a keyframe, SPS first) 331–429 ms after spawn,
    a 300x252 dialog, about 2.3 KB.
  - X11 through XWayland: first picture about 490 ms after spawn. xkbcomp
    runs fine confined (warnings only). Xwayland listens on an abstract
    socket, `:64` upward.
  - Typed keys reach the app: "Deckjack 8" sent as evdev codes over the
    link comes back on zenity's stdout, on both backends.
  - The X cookie works: `xprop` with the app's `XAUTHORITY` reaches the
    server; the same `xprop` with `XAUTHORITY=/dev/null` is refused.
  - The app inherits the confinement: foot and xterm both fail to get a
    pty (`/dev/pts` is granted to no app) and exit without mapping a
    window. Deckjack has its own terminal; streaming a terminal is not a
    use case, and no pty grant is proposed.
  - Killing the compositor outright kills the app (`PR_SET_PDEATHSIG`,
    tested with a non-GUI child, because a GTK app exits on display loss
    anyway).
  - Unconfined harness, release build, zenity's dialog with its caret
    blinking: 21 pictures a second, compositor 4.4% of one core, zenity
    4.4%; compositor RSS 20 MiB, zenity 46 MiB. The debug build used 21%.
  - GIMP 3.2.6 (GTK 3, Wayland, unconfined harness): first windows 17.4 s
    after spawn with a fresh `HOME` (font cache cold). Its main window
    resized on request to 1920x1080; the keyframe at that size was 47 KB
    (mostly flat UI); idle at that size: 0 pictures and 0.8% CPU over 5 s
    (damage tracking works); compositor RSS 76 MiB.
- **S2, encode cost: openh264 2.6.0 (system, asm) on one thread.**
  `cargo run --release -p deckjack-stream --example encode_bench`, synthetic
  screen-like content (rows of glyph-sized blocks), 20 pictures each, two
  runs (the second ran with other load on the machine; both shown as a
  range of medians). Times include the BGRA to I420 conversion.

  | Size | Keyframe | Caret changed | Scrolled one line | Convert only |
  |---|---|---|---|---|
  | 1920x1080 | 17.6–25.7 ms, 301 KB | 10.3–15.2 ms, 45 B | 21.3–32.9 ms, 11.9 KB | 1.0 ms |
  | 1280x800 | 8.0–12.1 ms, 149 KB | 4.6–7.1 ms, 45 B | 10.8–16.6 ms, 5.3 KB | 0.5 ms |
  | 800x600 | 3.7–9.5 ms, 71 KB | 1.6–5.1 ms, 44 B | 4.3–10.6 ms, 2.6 KB | 0.2 ms |
  | 300x252 | 0.6–0.8 ms, 10.9 KB | 0.2–0.3 ms, 44 B | 0.8–1.0 ms, 0.5 KB | 0.03 ms |

  The synthetic keyframe is a worst case (dense text everywhere); GIMP's
  real 1920x1080 keyframe was 47 KB. A caret blink costs almost no bits but
  still about 10 ms at 1080p: openh264 searches the whole picture. Encoding
  only the damaged region is the obvious next step (not built).
  - Bundled openh264 source (the crate's default `source` feature, no nasm
    on this machine): keyframe 73 ms, scrolled 88 ms at 1080p, encode only.
  - x264 via ffmpeg (`-preset ultrafast -tune zerolatency -threads 1`), 120
    scrolled 1080p pictures: 1.15 s wall, about 9.6 ms a picture including
    conversion and reading the file, 969 KB in all; `superfast`: 1.56 s,
    372 KB.
- **S3, browser decode: both engines, as AVC.**
  `web/e2e/spike/stream.e2e.ts` (projects `spike-chromium` and
  `spike-webkit`) decodes the compositor's own output (fixtures made by the
  bench with `--fixture`), checks pixels (light background, red caret), and
  pins the engines' answers:
  - Chromium 156: Annex B and AVC both decode. Codec strings from the SPS:
    `avc1.42c015` (320x240, Constrained Baseline, level 2.1) and
    `avc1.42c02a` (1920x1080, level 4.2).
  - WebKit 27.2 (Linux, GStreamer): `isConfigSupported` says yes to Annex B
    and every chunk then fails with `EncodingError`; AVC decodes, colors
    right.
  - Submit-to-output times, with all ten chunks queued at once (so queueing
    is included; an upper bound on per-picture latency, not a measure of
    it): Chromium 4.5 ms (320x240) and 19 ms (1080p) median; WebKit 25 ms
    and 129 ms.
  - Safari on macOS and iOS (VideoToolbox underneath) were not tested.
- **S4, input: keys, pointer, popups.**
  - Keys: the browser sends `KeyboardEvent.code` mapped to evdev
    (`web/src/shell/stream.ts`, `EVDEV`); the compositor adds 8 for xkb and
    feeds a US keymap. Verified end to end in the browser: "dj e2e" typed in
    the shell reaches zenity in Chromium and WebKit, desktop and phone
    projects (`web/e2e/shell.e2e.ts`).
  - Pointer: panel coordinates to window-local logical pixels through the
    same `object-fit: contain` math the canvas is drawn with (`fit`), so a
    letterboxed picture maps right; unit-tested.
  - Popups, Wayland: a right-click at (150, 126) in zenity's dialog maps a
    251x249 popup at (149, 127) relative to its parent [spiked, confined
    test]. GIMP's canvas menu: right-click at (400, 318.5), popup 151x305
    at (401, 320) [spiked, unconfined harness].
  - Popups, X11: GTK's menu is an override-redirect window; the compositor
    ties it to the X11 toplevel it is transient for, else the focused one.
    A right-click at (150, 126) maps a 297x295 popup at (126, 106) (GTK
    draws a shadow margin around X11 menus) [spiked, confined test].

**Built.**

- **`crates/deckjack-stream`** (Linux only; the binary prints an error and
  exits elsewhere):
  - `compositor.rs`: Smithay state, xdg-shell, xdg-decoration (always
    server-side, so foot-like clients draw no frame), wl_shm, data device,
    one 1920x1080 scale-1 output at 96 dpi, XWayland and the X11 window
    manager, one window per toplevel and popup, per-window offscreen pixman
    image and damage tracker, frame callbacks at up to 60 Hz, focus,
    pointer, wheel (v120) and keys, popup dismissal on a press in the
    parent, resize requests clamped to the output, close, and the app's
    exit ending the compositor. Software-rendering environment for the app
    (`LIBGL_ALWAYS_SOFTWARE`, `GSK_RENDERER=cairo`, Mesa's EGL vendor file,
    `GDK_SCALE=1`, `QT_SCALE_FACTOR=1`).
  - `encode.rs`: openh264 loaded from the system library, one encoder thread
    per window behind a one-slot mailbox (newest picture wins; a keyframe
    request survives being replaced), Constrained Baseline, no B-frames,
    keyframes only on request, bitrate scaled by area.
  - `link.rs`: the framed link, over one end of a socket pair the
    compositor inherits; one queue drained by a blocking writer, control
    frames never dropped, pictures dropped past eight waiting, and the
    window then owes its viewer a keyframe.
  - `xauth.rs`: the MIT-MAGIC-COOKIE-1 authority file, checked against
    `xauth` itself.
  - `input.rs`: DOM buttons to evdev, key code bounds, wheel scaling, size
    and point clamping.
- **`vendor/smithay`**: smithay 0.7.0, trimmed to the features used (the
  removed modules are one-line stubs so `cargo fmt --all` can walk the
  tree), with the abstract-socket XWayland launcher. Its own `[workspace]`
  table, excluded from Deckjack's, lints allowed.
- **Core (`crates/deckjack-core/src/stream.rs`)**: native app declarations,
  the launch (the real `LaunchPlan::build`, the compositor binary from
  `<install>/native/`), the link, server-made windows, the `/stream`
  socket with session, origin and seat checks, viewers that start with a
  keyframe and skip to the next keyframe when they fall behind, the seat
  hook, and the commands.
- **Shell (`web/`)**: `StreamPanel.vue` (WebCodecs into a canvas, input,
  resize, reconnect, compact-mode popups drawn over their parent),
  `shell/stream.ts` (framing, AVC conversion, key table, pointer math,
  popup placement, the decoder feeder), streamed windows in the window
  store (`syncStreamed` for the seat holder), native apps in the dock, a
  "Streamed" label on the window and a dashed tag in the dock.

**Deviations from the decision, each for a reason.**

- **Popups in compact mode are drawn by their parent's panel**, not as
  windows of their own: a phone shows one window at a time, and a menu that
  replaced its window on screen would be unusable. On the desktop each
  popup is its own chromeless window, placed against its parent.
- **"96 dpi, scale 1" is read as one CSS pixel per logical pixel**: the
  panel asks the app for its own size in CSS pixels (clamped to the
  output), so at desktop zoom there are no bars and no scaling; an app that
  keeps its own size is letterboxed, never stretched. On a high-density
  screen the browser scales the picture up, as decided.
- **No approval flow for native apps yet.** Placing the declaration in
  `<install>/native/` (which no app grant can reach: `native` joined the
  install layout) is the only consent. The fingerprint-and-approve path
  apps get is deferred (below).

**Deferred, said plainly.** Clipboard (both directions), drag and drop,
audio, IME and non-US keyboard layouts, cursor shapes (the browser shows its
own pointer), keyboard navigation inside menus (no popup grab), touch input
(taps arrive as pointer events), damage-region encoding, `deckjack install`
support for native apps (today the declaration and binary are placed by
hand), the approval flow, xdg-foreign, fractional scaling, primary
selection, and more than one keymap. (A lockdown now stops compositors; see
the second pass.)

**Untested.** Safari and iOS decode; a phone over Tailscale; anything but
zenity, GIMP, foot and xterm; long sessions (encoder threads, memory);
several apps at once; an app that opens many windows; X11 apps that use
their own window placement for dialogs. In the browser, only what
`web/e2e/shell.e2e.ts` drives is exercised (open from a shortcut, decode,
tap or click, type, the app exiting, a takeover); the panel's resize
requests, wheel, release-on-blur and reconnect are not exercised in a
browser (the pointer and wheel math is unit-tested). Resizing is verified from the link to the app (GIMP took
1920x1080), not from a dragged window.

---

### Second pass: review fixes (2026-11-22)

A review found the sandbox claim did not hold and a set of input,
pipeline and supervision defects. What changed, how each was checked, and
what is still open. Every new test was mutation-checked: the fix was
reverted or broken, the test failed, the fix was restored.

**Decisions.**

- **Native apps get Landlock ABI 9 and the ABI 6 scopes; other apps stay
  at ABI 4.** `sandbox::Policy` has a new `ipc` field. `Ipc::Scoped` handles
  `LANDLOCK_ACCESS_FS_RESOLVE_UNIX` (granted nowhere, so no pathname socket
  made outside the sandbox can be connected to) and scopes abstract sockets
  and signals to the sandbox's own domain, with `HardRequirement`, so a
  kernel below ABI 9 refuses rather than degrades. `stream::native_plan`
  sets it and refuses up front, in words, when `sandbox::ipc_scoping()`
  says the kernel cannot. Sockets made inside the domain keep working: the
  app reaches its compositor's Wayland socket and Xwayland's abstract
  socket, and the compositor signals its children. Why not every app: it
  would refuse every app on a kernel older than ABI 9 (this machine has
  ABI 10; the R620's LXC and phones were not checked), and web-app
  backends connect to the core's sockets in `run/`, which would need a
  `connect_unix` grant on `run/` and a look at what else they reach. That
  is a platform decision for its own change; until then every non-native
  app keeps the gap, said plainly here and in `sandbox::Ipc`.
- **User and mount namespaces were not used.** They work here without root
  (`unshare -Urn --mount`), but the seccomp filter denies `unshare` and
  the Landlock rules close the same doors without them.
- **The compositor's link is a socket pair, not a path.** The core makes
  the pair; `LaunchPlan` gained `pass_fds`, opened up in the child only,
  after the sandbox, so no other spawn inherits it; the compositor checks
  the descriptor is a socket and sets close-on-exec on it so the app and
  Xwayland cannot speak to the core as the compositor. There is no
  `run/stream-N.sock` for another process to win a race for.
- **Native apps' private trees are `cache/native/<name>/`**, apart from the
  apps' `cache/apps/<name>/` (`LaunchPlan::build_in`).
- **Each compositor has its own runtime directory**,
  `$TMPDIR/runtime-XXXXXX` (mkdtemp), holding its Wayland socket and X
  cookie, removed when it ends. A crash (`SIGKILL`) leaves the directory
  behind; nothing cleans those up yet.

**Fixed, and how each was checked.**

| Finding | Fix | Checked by |
|---|---|---|
| Unix-socket escape (session bus, X0, `systemd-run`) | `Ipc::Scoped` for native apps | `confined::a_native_app_reaches_no_socket_or_process_outside_its_sandbox` (path and abstract sockets made by the test process, and a signal to it); plus a one-off probe from inside the sandbox against this machine's real sockets: `xwininfo -root` on `:0` refused, curl to `/tmp/.X11-unix/X0` (path and abstract) and `/run/user/1000/wayland-1` refused (exit 7), `busctl --user` refused, `systemd-run --user touch <marker>` refused and no marker. The same probe with the scoping removed: X0, the bus and `systemd-run` all worked and the marker appeared. [measured] |
| Another app maps windows into this app's compositor | the same rule (a socket made in another sandbox is outside this one) | `confined::a_native_app_cannot_reach_another_apps_compositor`, two real sandboxes [measured] |
| No signal scope | `Scope::Signal` | the signal half of the first test |
| Link socket hijack (two findings) | socket pair | no rendezvous exists to test; `native_plan`'s unit test pins the fd hand-over |
| Instances share runtime dir and cookie | per-instance mkdtemp dir | `confined::each_instance_has_its_own_runtime_directory` |
| Native name shares an app's cache | `cache/native/` | `stream::tests::a_native_plan_has_its_own_private_tree_the_scoped_sandbox_and_the_link_fd` |
| Keys and buttons stuck on takeover (three findings) | `StreamCommand::ReleaseAll`, sent on every seat move and when an instance's last viewer goes; the compositor releases every pressed key and every button it tracked; a key release is applied even for a window that is gone; the panel releases what it holds before closing its socket | core unit tests (moved, last viewer), e2e `a_streamed_window_follows_the_seat_and_nobody_else_gets_it`, and the confined round trip (a Shift left down, then `ReleaseAll`, then "Deckjack 8" arrives as typed) |
| Lockdown did not stop streamed apps | `lockdown::AllChildren` of the terminals and `Streams` in `main.rs` | e2e `a_lockdown_stops_streamed_apps` (the harness's copy of the wiring, not `main.rs` itself) |
| Close lost on a full queue; pictures after a takeover (three findings) | `moved()` takes the old session's viewers off every window and sets a per-viewer `revoked` flag that the socket checks before sending anything, queued pictures included | `stream::tests::a_takeover_cuts_off_a_viewer_whose_queue_is_full`; the socket-side check of `revoked` has no test of its own |
| `/stream` outside the socket rule of the fetch gate | rule 4 applies to `/stream` | `fetchgate` unit test |
| Popup cascade never ran | descendants worked out before anything is removed (`input::with_descendants`), on removal and on unmap; orphaned Wayland popups are told they are done | unit test of `with_descendants`; the compositor wiring has no test (no installed client unmaps a parent with a menu open) |
| Close destroyed X11 menus | `Close` on an override-redirect window does nothing | untested: GTK, the only installed X11 client with menus, sets `WM_DELETE_WINDOW` and ignores it, so it cannot tell the two apart [inferred] |
| Descendants survived a stop | the compositor is a subreaper, the app leads its own process group, `SIGTERM` stops the loop, and every exit path ends the group and every child (TERM, then KILL after a second) | `confined::what_the_app_left_running_goes_when_the_app_exits` (a `setsid` daemon) and `confined::stopping_the_compositor_ends_what_the_app_started`. A `SIGKILL` of the compositor still leaves grandchildren (the subreaper is gone); only the app itself dies then |
| Keyframe storm from a stalled viewer | a keyframe is asked for when a viewer starts missing pictures, and again only once its queue has room (checked on each picture and on the socket's 2 s recheck); sends time out after 5 s | `a_stalled_viewer_does_not_turn_keyframes_into_more_requests`, `a_viewer_that_caught_up_on_an_idle_window_is_nudged`; the send timeout is untested |
| Encoder refuses under 16 px and past 3840x2160 | pictures are drawn and encoded at `max(16, even)`, padded black, header carries the visible size; the visible size is held to the 1920x1080 output for every window kind, and xdg geometry is clipped to the surface tree | unit tests of `encoded_size`, `visible_size` |
| Browser decode backpressure | deltas dropped while more than 4 wait in the decoder, one keyframe asked | `stream.test.ts` |
| Unsupported codec loop | `VideoDecoder.isConfigSupported` before the first configure; failures back off and stop after 3 in a row with "cannot decode" shown | `stream.test.ts` |
| Compact panel kept the old window's socket; keys to the wrong window | the panel reconnects when handed another window and takes the browser's keyboard when its window is focused | Playwright `hands the keyboard to the streamed window the shell focuses`, all four projects |
| Tapping a menu focused it and unmounted it | a popup is never the focused window: focusing it focuses its parent, which keeps its popups above it | `windows-streamed.test.ts` |
| Releases dropped outside the picture; chorded buttons; cancelled pointers | a release, and moves while held, are held to the picture's edge; button changes come from diffing `buttons` on every pointer event; `pointercancel` and lost capture release | `stream.test.ts` (`buttonChanges`, `toWindowPointClamped`); the panel's wiring of them is not driven in a browser |
| A queued move sent after a press | the queued move is dropped when a button or wheel event goes | not tested |
| Tile and cycle took popups; menus floated over a minimized window; dock picked the menu | popups skipped by `tile`, `cycle` and the dock; hidden while their parent is minimized | `windows-streamed.test.ts` (dock change untested) |
| Cmd+key left the key held on macOS | releasing Meta releases every held non-modifier key | `releasedWithMeta` unit test; not run on a Mac |
| A delta after a dropped picture | the encoder's sink drops deltas until a keyframe is queued | not tested |
| Link writer polled 500 times a second | one queue, blocking receive | `link` unit tests |
| `ETXTBSY` flake in the stream tests | the compositor is copied once per test process and hard-linked | not reproduced before or after |
| Toplevels never deactivated | the previously activated toplevel is told it is not | not tested |

**Still open.**

- Every non-native app keeps the ABI 4 Unix-socket and signal gap (above).
- Xwayland's abstract socket is reachable by any unconfined local process
  of any uid and by any non-native app; the cookie is what keeps them out
  (R2).
- A second tab of the same browser session: stream sockets are admitted by
  session, so a stale panel in the first tab keeps its socket until it
  unmounts. Binding admission to the seat-holding connection needs the seat
  to hand out a per-connection token; not done.
- Caps Lock and Num Lock state are not synchronized with the browser.
- Popups are not re-placed when their parent is dragged or resized locally
  (only on the node's next window list).
- The compositor's 16 ms frame timer still runs `announce` every tick when
  nothing changed.
- A `SIGKILL`ed compositor leaves its runtime directory and any
  grandchildren of the app.
- The `ended`/`stop` paths now refuse pid 0 (to `kill(2)` the whole process
  group); found when a mutation of that guard sent `SIGTERM` to the test
  runner's group.

---

### Goal

Use a native Linux app that has no web UI from any device, inside
Deckjack, without a remote desktop: each of the app's windows is a Deckjack
window with Deckjack's chrome, the app is confined like any app, and only
the device holding the seat drives it. The exit check is GIMP on the
laptop's node, used from the Fold.

### Useful subset for a first version

- Launch a declared native app from the dock or the palette.
- Its toplevels and popups become Deckjack windows, marked as streamed.
- Pictures are damage-driven H.264, decoded with WebCodecs.
- Pointer, wheel and keyboard (US layout) from the seat holder only.
- Resizing the Deckjack window resizes the app's window.
- A takeover moves the streams; the new holder gets a keyframe.
- Closing the window asks the app; force quit is `stream.stop`.
- The app's exit, or the compositor's, removes every window cleanly.

**Explicitly not in v1:** clipboard, drag and drop, audio, IME, GPU
rendering or encoding, macOS or Android capture, whole desktops, running a
native app the suite already has a web UI for.

### Engine choice

| Need | Choice | Alternatives considered | Why |
|---|---|---|---|
| Compositor | **Smithay 0.7.0** (crates.io, 2025-06-24; master active, last commit 2026-11-19) [cited], headless, vendored and trimmed | wlroots via `cage`/headless backend; a Weston headless instance per app; `xpra` | Rust, embeddable, its own pixman renderer, an X11 WM for XWayland, and the per-window rendering this design needs is a few dozen lines on top of it. wlroots and Weston render one output, not one picture per window, and are C processes we would drive from outside. xpra is a remote-desktop product with its own UI and protocol. |
| XWayland | **Xwayland 24.1 with `-shm`, `-auth`, abstract socket only** via the vendored `spawn_abstract` | Smithay's stock `XWayland::spawn` | The stock path needs `/tmp`, which the sandbox does not grant [spiked]. |
| Rendering | **pixman (CPU)** | GLES through EGL | The sandbox denies `/dev/dri` unless a manifest asks for the GPU; asking would grant every app with a native UI the GPU. |
| Encoding | **libopenh264 2.6 from the system**, `openh264` crate 0.9.10 with `libloading` [cited] | the crate's bundled source; x264 (ffmpeg binary or `x264` bindings); VA-API; VP8/VP9/AV1 | System build has its assembly (S2: 3–4x the bundled build here). x264 is about 2x faster and smaller still, but is GPL (compatible with Deckjack's AGPL) through C bindings or a subprocess per window; openh264's API is a crate call and makes IDR on request trivial. Hardware encoders need the GPU. VP8/VP9 decode is everywhere but CPU encode is slower; AV1 more so. Revisit with damage-region encoding first. |
| Decode | **WebCodecs `VideoDecoder`**, AVC format | `<video>` with MSE; a wasm decoder | MSE adds buffering latency; wasm would be a decoder we ship and patch. |
| Transport | **one WebSocket per streamed window** at `/stream`, relayed by the core | JSON over the command bus; WebRTC | The bus drops progress frames and carries JSON. WebRTC needs ICE/STUN, a second stack, and its congestion control fights a screen's bursts; over Tailscale a WebSocket is enough for v1 [inferred]. |

### Commands

| Command | Risk | Notes |
|---|---|---|
| `stream.apps` | read | Declared native apps, and the declarations that failed to parse, with why. |
| `stream.launch` `{app}` | write | Same level as `terminal.open`: starts a process, confined to what the declaration asks for. |
| `stream.list` | read | Running instances and their windows. |
| `stream.stop` `{instance}` | destructive | Force quit: the app gets no chance to save, so the operator confirms (the subject names the app). Closing its window is the ordinary way. |

`stream` is in `RESERVED_NAMESPACES` (no app may be named `stream`) and in
`HARD_DENY` (no app window may call or subscribe to `stream.*`). Window
changes use the existing `shell.window.*` commands; `shell.window.close` on
a streamed window asks the app instead of removing the window.

### UI and touch contract

- A streamed window is drawn by `StreamPanel`: a canvas scaled with
  `object-fit: contain`. The title bar says "Streamed"; the dock tag has a
  dashed outline and the label reads ", streamed native app" to a screen
  reader.
- Pointer: move (one per animation frame), buttons (DOM numbering), wheel
  (pixels; the compositor turns 100 px into one 120-unit notch), leave.
  Touch taps arrive as pointer events; gestures are deferred.
- Keys: `KeyboardEvent.code` to evdev, prevented in the browser, repeat left
  to the app; every held key is released when the panel loses focus.
- The panel asks for its own size (debounced 120 ms), in CSS pixels.
- Desktop: popups are chromeless windows above their parent. Compact:
  popups are drawn over their parent's panel and never appear in the
  switcher; tiling skips them.
- Status line while not live: connecting, "streaming to the device that
  holds the seat", or "this browser cannot decode the stream".

### How it fits the sandbox

The compositor is spawned by the core with `LaunchPlan::build_in` from the
native app's declaration (`stream::native_plan`), as an app is, plus the
Unix-socket and signal scoping (`sandbox::Ipc::Scoped`, Landlock ABI 9).
The app is the compositor's child and inherits the Landlock domain and the
seccomp filter; it dies with the compositor (`PR_SET_PDEATHSIG`), and an
orderly stop also ends everything it started (the compositor is their
subreaper). Every socket it uses is made inside the sandbox: Wayland in
the compositor's own runtime directory under its `TMPDIR`, X11 abstract
with a cookie. Its link to the core is an inherited socket pair.

`<install>/native/gimp.toml`:

```toml
[app]
name = "gimp"
version = "3.2"
protocol = 1
title = "GIMP"
tag = "Gi"

# Exactly what the compositor and the app may touch. Pictures folders,
# read and write; no network; no GPU (pixman, CPU encoding).
[needs]
working_dir = [{ path = "Pictures", write = true }]

[native]
exec = ["/usr/bin/gimp", "--no-splash"]
x11 = false
```

The compositor binary sits beside the declarations as
`<install>/native/deckjack-stream`.

### Risks and open questions, each with the spike that settles it

- **R1 — The operator's sockets from inside the sandbox.** Closed for
  native apps (second pass): ABI 9's `RESOLVE_UNIX` plus the ABI 6 scopes,
  measured against this machine's X0, session bus, `systemd-run` and
  Hyprland's Wayland socket. Open for every other app, which is still at
  ABI 4: a platform decision, shared with Lane 6's CUPS and D-Bus findings.
- **R2 — Our Xwayland's abstract socket is reachable from outside.** A
  native app can no longer connect out to abstract sockets, but the scope
  does not stop others connecting in: any unconfined local process, and
  any non-native app, can reach `@/tmp/.X11-unix/X64`; the cookie is what
  stops them [spiked in the first pass]. Moving Xwayland to a
  pre-connected socket (`-listenfd` with no listening name) would remove
  the name altogether; not tried.
- **R3 — CPU at 1080p.** One full-screen app scrolling costs 21–33 ms a
  picture on one core: about 30–45 pictures a second, before the app's own
  rendering. *Spike:* encode only the damaged rows (openh264 cannot skip
  regions itself, but a picture whose unchanged area matches the reference
  encodes to almost nothing; measure what the conversion and motion search
  then cost), and compare x264 `ultrafast` over the same pictures through
  its C API.
- **R4 — Latency over Tailscale from the Fold.** Not measured. *Spike:*
  keypress to picture on the phone, with the timestamp in the header echoed
  back.
- **R5 — Safari and iOS decode.** Not tested. *Spike:* the S3 page on a
  Mac and an iPhone; WebKit on Linux needed AVC, Safari may differ.
- **R6 — Keyboard layouts.** The compositor's keymap is US; the browser
  sends physical keys. Someone on another layout types US characters.
  *Spike:* send the browser's layout name and load the matching xkb
  layout; IME needs `text-input-v3`, which is a separate piece of work.
- **R7 — The vendored smithay.** A trimmed fork of 0.7.0 with one patch.
  *Spike:* propose `spawn_abstract` (or a configurable socket directory)
  upstream; once released, drop the vendor directory.
- **R8 — Socket path length.** The Wayland socket lives at
  `<install>/cache/apps/<name>/tmp/runtime/wayland-N`; a deep install
  directory can pass the 108-byte limit for a Unix socket path
  [inferred]. *Spike:* fall back to a short directory under `run/` when
  the path is long.
- **R9 — Approval.** Native declarations are not approved or fingerprinted.
  *Spike:* reuse `apps::Host`'s grant store with a `native:` key, and
  re-ask when a declaration changes.
- **R10 — Many windows, many encoders.** One thread per window. An app with
  twenty palettes is twenty encoders. *Spike:* GIMP with every dock open.

### Exit check

From the Fold, against the laptop's node:

1. GIMP is in the dock, marked streamed; tapping it opens GIMP's windows as
   Deckjack windows within 20 s.
2. Open a photo from `Pictures/`, crop it with the pointer, use the
   canvas's right-click menu, save it. The menu appears next to the click.
3. Take the seat on the laptop: the same windows are there, live, within a
   second; the Fold shows the takeover screen and sends nothing.
4. Close GIMP's main window: GIMP asks to save; the dialog is a Deckjack
   window; answering it ends GIMP and every window goes.
5. GIMP cannot read `~/.ssh` (confinement) and cannot reach the operator's
   X server (R1: met for native apps on this machine, by the probe in the
   second pass, with a shell standing in for GIMP).

### Rough size

Built in this pass (lines, including comments):

| Piece | Production | Tests |
|---|---|---|
| Compositor, encoder, link, X authority, input (`deckjack-stream`) | ~1,950 Rust | ~830 Rust (confined, core-with-compositor) + ~400 unit |
| Core registry, socket, commands (`stream.rs`) | ~960 Rust | ~360 Rust + ~110 end to end |
| Protocol (`deckjack-protocol::stream`) | ~280 Rust | ~100 |
| Window list, seat and wiring changes | ~250 Rust | — |
| Shell: panel, stream module, store, dock, frame | ~900 TS/Vue | ~410 vitest + ~80 Playwright |
| Spikes kept: bench, decode page and test | ~390 | — |
| Vendored smithay (trimmed upstream, one patch of ~70 lines) | 2.6 MB of upstream source | — |

The remaining lane (clipboard, layouts and IME, damage-region encoding,
install and approval, R1 and R2) is larger than this pass [inferred:
clipboard alone is the data device plus the X11 selection bridge in both
directions, plus a browser-side permission flow].

### Sources

- smithay on crates.io (0.7.0, 2025-06-24): https://crates.io/api/v1/crates/smithay
- smithay master, `src/xwayland/x11_sockets.rs` (still `/tmp/.X{n}-lock` and `/tmp/.X11-unix`): https://raw.githubusercontent.com/Smithay/smithay/master/src/xwayland/x11_sockets.rs ; last commit 2026-11-19: https://api.github.com/repos/Smithay/smithay/commits?per_page=1
- openh264 crate 0.9.10 (2026-11-10; `libloading` feature; `from_blob_path` checks Cisco's hashes, `from_blob_path_unchecked` does not): https://crates.io/api/v1/crates/openh264 and the crate source, `openh264-sys2/src/lib.rs`
- wayland-server 0.31.14, xkbcommon 0.9.0 (smithay pins 0.8), pixman 0.2.1: https://crates.io/api/v1/crates/wayland-server, https://crates.io/api/v1/crates/xkbcommon, https://crates.io/api/v1/crates/pixman
- Installed versions (`pacman -Q`): openh264 2.6.0-2, ffmpeg 9.0.2-2, x264 0.165.r3222, xorg-xwayland 24.1.14-1, foot 1.28.0-2, gimp 3.2.6-2, libxkbcommon 1.13.2-1, pixman 0.46.4-1, wayland 1.26.0-1
- `Xwayland -help` (24.1.14): `-shm`, `-glamor [gl|es|off]`, `-listenfd`
