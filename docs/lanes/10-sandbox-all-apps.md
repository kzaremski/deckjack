## Lane 10 — Unix sockets, abstract sockets and signals scoped for every app

Written 2026-11-22. The machine: Arch, Linux 7.2.15 (Landlock ABI 10 in the
kernel; this build asks for 9), GPD Pocket 4, Hyprland with Xwayland on
`/tmp/.X11-unix/X0`. Claims are tagged **[measured]** (run on this machine,
method given), **[cited]** (source given) or **[inferred]** (reasoned, not
checked).

### The gap this closes

Every non-native app was held to Landlock ABI 4 (`sandbox::WANTED`). ABI 4
has filesystem and TCP rules and nothing for Unix sockets or signals, so a
confined app (scribe, filer, notes, tasks, photo, office, video) could
`connect()` to `/run/user/1000/bus` and run `systemd-run --user …` outside
any sandbox, reach the X server, Hyprland's IPC socket, gpg-agent and
PipeWire, connect to any abstract socket, and signal any process of the
operator's uid. Lane 8 had closed this for streamed native apps only.

### What was built

**Every launch is `Ipc::Scoped` with no path granted.** `sandbox::Ipc`
now defaults to `Scoped { connect_unix: [] }`, and `LaunchPlan::build` sets
it explicitly for every app. That handles Landlock's `ResolveUnix` (ABI 9),
so a pathname Unix socket made outside the app's own Landlock domain
cannot be connected to unless it is under a `connect_unix` path, and no app
gets one. It also sets `Scope::AbstractUnixSocket | Scope::Signal` (ABI 6),
so abstract sockets and signals reach only the app's own domain. Inside
that domain things still work: an app's own server sockets, by path or by
abstract name, and its helpers (a LibreOffice worker, ffmpeg) can still
talk to each other and signal each other. `Ipc::Open` is still there, but
only the escape test's control run uses it.

`WANTED` stays at V4 for the filesystem and TCP rights on purpose. Raising
it to V5 would handle `IoctlDev` and take `/dev/dri` ioctls away from GPU
apps. `ResolveUnix` is handled on its own, never as part of a writable
tree's rights.

**Apps reach the core over an inherited socket pair, not a path.** Before,
the core bound `<install>/run/<app>.sock`, granted the app read on `run/`,
and the app connected to it. Now:

- `apps::Links` (in `OsEnv`, shared with `AppHost`) holds one attendant
  channel per installed app. `AppHost::start` registers each app's
  attendant before anything is spawned.
- On every launch, `OsEnv::spawn` calls `Links::open(app)`. That makes a
  `socketpair`, sends the core's end to the app's attendant, and moves the
  app's end to **descriptor 3** in the child (`dup2` in `pre_exec`, after
  the sandbox hook). The child's environment carries
  `DECKJACK_APP_FD=3` (`deckjack_protocol::LINK_FD_ENV`, `LINK_FD`).
- The attendant runs `apps::handshake` on that stream. It used to be
  `accept_handshake` on a listener. When a restarted app launches, a new
  link arrives at the same attendant.
- `<install>/run` is no longer granted to any app. It holds only the
  core's MCP session sockets now.
- A launch for an app that has no attendant is refused with a message,
  instead of producing an app that hangs at its handshake. The old code
  bound sockets only for apps found at start, so this is the same set
  [inferred]. The refusal is tested (`tests/apps.rs`
  `a_link_for_an_app_nothing_attends_is_refused_in_words`, through
  `Links::open` and through `OsEnv::spawn`), as is the refusal of a plan
  whose `pass_fds` already holds descriptor 3
  (`a_plan_that_passes_the_link_descriptor_itself_is_refused`). When the
  app's end of the pair is already at descriptor 3 there is nothing to
  `dup2`, and close-on-exec is cleared by hand; `tests/link_fd.rs` forces
  the pair onto 3 and checks the child can still write on it.
- **Every other descriptor is close-on-exec in the child.** After the
  link is in place, a `pre_exec` hook runs
  `close_range(first, ~0, CLOSE_RANGE_CLOEXEC)` from descriptor 4 (from 3
  when there is no link), and the `pass_fds` hook after it clears the flag
  on exactly the plan's descriptors. Before this an app inherited any
  descriptor the core held without close-on-exec. portable-pty's
  `openpty` sets FD_CLOEXEC on the master and slave only after the call
  returns, so a launch forked on another thread in that window would have
  handed the app the operator's shell's pty, which Landlock does not
  mediate. That race is inferred from reading portable-pty 0.9.0 and was
  not reproduced. The escape test holds a deliberately inheritable
  descriptor open in the test process and asserts each probe inherits
  exactly stdin, stdout, stderr and its link (see below). With the hook
  removed, every kind inherits the stray descriptor and the test fails
  [measured].

The compositor of a streamed native app keeps its own stream link in
`pass_fds` (lane 8). Its plan sets `core_link = false` and drops
`DECKJACK_APP_FD`.

**The SDK's public API changed, and the change breaks out-of-tree code.**
`App::run()` still takes no arguments, so an app that only calls it
compiles and runs as before. What was removed:

- `deckjack_app::SOCKET_ENV` (`DECKJACK_APP_SOCKET`). Its replacement is
  `LINK_ENV` (`DECKJACK_APP_FD`, always `3`), which names a descriptor,
  not a path.
- `AppError::NoSocket` and `AppError::Connect { path, source }`. Their
  replacements are `AppError::NoLink`, `AppError::BadLink { value }` and
  `AppError::Link { fd, source }`.

Code that reads `SOCKET_ENV` or matches those variants no longer compiles.
No in-repo crate does. An app written without the SDK that connects to
`$DECKJACK_APP_SOCKET` now finds the variable unset: it has to take the
socket at descriptor 3 instead. No deprecated `SOCKET_ENV` was kept, because
the core no longer sets that variable, and a constant that still compiles
would only move the failure to run time. Inside, the SDK
checks the descriptor through a close-on-exec duplicate
(`F_DUPFD_CLOEXEC`). Only once it is known to be a socket does the SDK take
and close the original. So a wrong descriptor is left open and named in an
error (`NoLink`, `BadLink`, `Link`), and helpers the app spawns do not
inherit its link to the core. Unit tests cover all three errors, the
close-on-exec flag, and that dropping the link ends it for the core (no
copy of the original left open).

**Why no pathname socket is granted anywhere.** The core's link is
inherited, Xwayland and the Wayland socket of a streamed app are created
inside its sandbox, and no first-party app talks to D-Bus (see below). The
UI socket from APP-UI-ISOLATION (`/kit/bus.js`) is a browser WebSocket to
the core's HTTP listener and is not an app-process socket. The core's
proxy socket (`proxy/http.sock`) is the core's own listener. Apps used to
be able to reach it, which is why it checks `SO_PEERCRED`. Now Landlock
refuses it as well.

### Old kernels refuse instead of running less confined

The minimum kernel for launching any app is **Linux 7.1** (Landlock ABI
9). [cited: landlock 0.4.7 `ABI::V9` docs, kernel commit b8f82cb0d84d]

- `sandbox::availability()` probes ABI 9 separately and reports
  `Enforcing { ipc_scoping, .. }`. Its `landlock_abi` is the version the
  kernel reports (`landlock_create_ruleset` with
  `LANDLOCK_CREATE_RULESET_VERSION`, asked through libc because the crate
  keeps it private). Only messages read that number; what can be enforced
  is still decided by probing. Before, it was the highest ABI this build
  asked for, so a 6.12 kernel (ABI 6) was told it had ABI 4, and this
  machine (ABI 10) reported 9.
- `sandbox::command` returns `SandboxError::NoIpcScoping` for a scoped
  policy on a kernel that cannot enforce it, before it forks. It does not
  rely on `CompatLevel::HardRequirement` alone. The decision is the pure
  function `sandbox::refusal`, unit-tested with fabricated availabilities
  (ABI 4, 6, 8, 9, 10 and none) under scoped and open policies.
- `LaunchPlan::build` refuses each app with
  `ConfinementError::NoIpcScoping`. The message names the kernel's ABI,
  Linux 7.1, what the app could otherwise reach, and the developer-mode
  alternative.
- `apps::launch_refusal` gives the same answer once for the whole node.
  It is surfaced in three places:
  - `deckjack apps` prints a line saying none of the listed apps will
    launch, and why.
  - `system.apps` adds a `refusal` field to each entry, and the shell
    shows it as the app's summary in the dock and launcher
    (`Cannot run on this node: …`).
  - A refused `window.open` returns the reason instead of "see the log".
- Developer mode (unconfined) refuses nothing for this. It already says
  at every launch that apps run with the operator's access.

Tested with a fabricated availability:

- `tests/apps.rs`
  `every_app_is_refused_on_a_kernel_that_cannot_keep_unix_sockets_in_its_sandbox`:
  plain, network and GPU manifests are all refused, nothing is spawned,
  and the node-level refusal names 7.1.
- `sandbox.rs`
  `a_scoped_policy_is_refused_before_forking_on_a_kernel_without_ipc_scoping`:
  the pre-fork refusal in `sandbox::command`.
- The CLI line: `tests/install.rs`
  `the_apps_listing_says_when_none_of_the_apps_will_launch_and_why`, on
  `install::render_node_refusal`, which `deckjack apps` prints.
- The `system.apps` field and the refused `window.open`: `tests/apphost.rs`
  `system_apps_carries_the_nodes_refusal_on_every_entry` and
  `opening_an_app_on_a_node_that_launches_none_says_why`, through the real
  bus and wiring on a host built with `OsEnv::assuming_sandbox` (ABI 6, no
  IPC scoping). That constructor only changes what the planner is told;
  `sandbox::command` probes the real kernel again before any confined
  spawn.
- The shell's summary: `web/src/stores/apps.test.ts`, with a hand-written
  `refusal` string.

**Not observed live:** this machine has no kernel older than 7.1, so none
of these surfaces has been seen on a real old kernel. The `deckjack apps`
binary's own `print!` of that line is not run by any test; the text it
prints is.

### Escape probes

`crates/deckjack-core/tests/escape.rs`,
`no_app_kind_reaches_a_socket_or_process_outside_its_sandbox`. For each
first-party manifest (editor/scribe, files/filer, notes, office, photo,
tasks, video) and for a streamed compositor's plan, the test does this:

1. Installs the test binary as the app.
2. Plans it with the real `LaunchPlan::build` (or `stream::native_plan`).
3. Spawns it with the real `OsEnv::spawn`, which applies Landlock and
   seccomp.
4. Repeats the run with only `ipc` changed to `Ipc::Open`.

Every target refused in the scoped run must be reached in the control run,
or the test fails. A real socket that cannot be reached even in the
control run is reported as skipped and not counted.

[measured] Results on this machine. Every row holds for all eight kinds
(the seven apps and the streamed compositor), except the link row, which
is for the seven apps: a compositor has no app link, only its own stream
link in `pass_fds`. For the compositor, the planted socket is in
`cache/native/np/tmp` (the probe is named `np` so the path fits in a socket
address).

| Target | Scoped | Open (control) |
|---|---|---|
| session bus `/run/user/1000/bus` | EACCES | ok |
| systemd user manager `systemd/private` | EACCES | ok |
| X server `/tmp/.X11-unix/X0` | EACCES | ok |
| Wayland `wayland-1` | EACCES | ok |
| Hyprland `.socket.sock` | EACCES | ok |
| PipeWire `pipewire-0` | EACCES | ok |
| gpg-agent `S.gpg-agent` | EACCES | ok |
| another confined app's pathname socket | EACCES | ok |
| another confined app's abstract socket | EPERM | ok |
| a socket the test made inside the app's own writable TMPDIR | EACCES | ok |
| signal 0 to another confined app | EPERM | ok |
| signal 0 to the test process (the core's side) | EPERM | ok |
| X server by abstract name `@/tmp/.X11-unix/X0` | ECONNREFUSED | ECONNREFUSED (skipped: Xwayland here does not listen on it) |
| its own socket, by path and by abstract name | ok | ok |
| say hello on the inherited link, read by the core's end (apps only) | ok | ok |
| descriptors inherited, with a stray inheritable one held open in the core | apps: 0, 1, 2, 3; compositor: 0, 1, 2 and its stream link | not compared |

The test skips itself on a host below ABI 9 and prints why. Its results
mean nothing on such a host.

### What loses D-Bus

None of the first-party apps uses D-Bus, PipeWire, portals or the display
server. [measured: `grep` for zbus, dbus, pipewire, pulse and portal across
the app crates finds only the office worker's environment and the
sandbox.] LibreOffice in the office worker had been measured connecting to
CUPS, the system bus and systemd-resolved (lane 6, S1). The sandbox now
refuses those connects. The worker still points `DBUS_SESSION_BUS_ADDRESS`
and the CUPS server at a missing path, so the library gives up instead of
logging a refusal. A streamed native app (lane 8) had already lost the
session bus, so file pickers through portals, notifications and
single-instance activation do not work for it. Nothing changes there.

### The deliberate office crash, kept off the desktop

`a_worker_held_under_its_memory_limit_fails_instead_of_converting` starves
the LibreOffice worker to 64 MiB, and the worker dies with SIGSEGV. Before
this lane, every passing run triggered a desktop crash notification.

- **How Omarchy notices crashes** [measured: read
  `/usr/share/omarchy/bin/omarchy-crash-watch`]. It runs
  `journalctl -f MESSAGE_ID=fc2e22bc6ee647b6b90729ab34a250b1`, the
  systemd-coredump journal entry, and notifies for any crash by the
  operator's uid that is not muted.
- **Why `RLIMIT_CORE=0` did not stop it.** `core_pattern` here is a pipe
  to `systemd-coredump`. With a pipe, the kernel runs the handler whatever
  the core rlimit is (except the value 1), and systemd-coredump logs the
  crash and only then declines to store it.
- **The fix.** The worker now calls `prctl(PR_SET_DUMPABLE, 0)` next to the
  rlimit, before LibreOffice boots. The kernel's coredump path stops for a
  non-dumpable process before it runs any handler, so nothing reaches the
  journal. The death itself does not change (still SIGSEGV), the app
  reports it from the exit status as before, and the test still asserts
  that no PDF was written and that any result does not claim success. As a
  side effect, a converter crash on a confidential document no longer hands
  the process image to systemd-coredump at all.
- **Verified** [measured]:
  - The test passes with the change, and `coredumpctl list` gained no
    `deckjack-office` entry. It had 29 before the run and 29 after.
  - Control: the same test with `PR_SET_DUMPABLE` set back to 1 added one
    entry (SIGSEGV, 23:46:11, `/tmp/.tmpkk6ntv/install/apps/office/deckjack-office`).
    That control run triggered one desktop notification. Restoring the
    change took it back to zero.
  - The normal conversions (`docx`, `xlsx`, `pptx`) under the real launcher
    still pass with the worker non-dumpable.
- **Guarded by tests** [measured]: the worker reads
  `prctl(PR_GET_DUMPABLE)` just before booting LibreOffice and reports it
  in `result.json` (`dumpable`). The conversion test asserts it is 0, and
  fails with the `prctl` changed to 1. The worker also writes a `booting`
  marker in its job directory just before boot, and the starved test
  asserts it is there. So a worker that failed before LibreOffice (a
  refused `prctl`, an unreadable job) no longer passes as "held under its
  memory limit"; with the `prctl` made to fail, the test fails. What no
  test checks is the last step, that the kernel then skips
  systemd-coredump: that rests on the one-time `coredumpctl` check above.
  The starved test also does not check how the worker died (signal or
  exit code), because `OsEnv` reaps the child and does not hand its exit
  status back.
- **Side effect** [inferred]: in a non-dumpable process, `/proc/self/*` is
  owned by root. The process's own access is unaffected (self-access is
  exempt from the ptrace check), and the conversions above passing is
  consistent with that.

### Gates

Run in the lane worktree with
`CARGO_TARGET_DIR=/home/kzaremski/Repositories/.dj-target-sandbox` and
`DJ_E2E_PORT=19100`:

- `cargo fmt --all --check`: clean.
- `cargo clippy --workspace --all-targets -- -D warnings`: clean.
- `cargo test --workspace`: 74 suites, 1545 passed, 0 failed, 7 ignored
  (after the review fixes).
- `npx vue-tsc --noEmit`: clean. `npx vitest run`: 42 files, 818 tests,
  all passing.
- Playwright (whole suite, every project): 78 passed. The `--debug` node the
  e2e tests start confines apps (`Confinement::Enforced`), so Files,
  Scribe, Tasks and the streamed native app ran under the scoped sandbox
  and the inherited link.

### Open

- The refusal on an old kernel has only been seen in tests (see above).
- The X server's abstract socket could not be probed: Xwayland on this
  machine does not listen on one. Abstract-socket scoping is still proven
  against the other app's abstract socket.
- A third-party app that really needs a pathname socket (an `ssh-agent`, a
  database's local socket) has no way to ask for one. `Ipc::Scoped`'s
  `connect_unix` can carry the grant, but the manifest has no field for it,
  and adding one is a separate approval-UI decision.
- An app installed while the core is running has no attendant, and its
  launch is refused with a message telling the operator to restart the
  core. The refusal is unit-tested but has not been seen in a running
  node. That was effectively true before as well (its socket was never
  bound) [inferred].
