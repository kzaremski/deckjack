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
(`apps::UI_MOUNT`, `apps::ui_router`) behind the session gate, inside an
iframe. The browser never
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
`read`, `write`, `destructive`, `outbound` (`protocol::Risk`), and
`destructive`/`outbound` are what send an *agent's* call to the Assistant's
Inbox while the operator gets an ordinary confirmation.

**`[needs]`** (`protocol::Needs`) is the whole capability vocabulary:

| key | shape | reality |
| --- | --- | --- |
| `gpu` | bool | `/dev/dri` and, where it exists, `/dev/kfd` read-write (Lane 0.2) |
| `network` | list of hosts | shown to the operator, **never enforced as hosts**. On its own (no ports named) it opens the network whole, warned on every launch |
| `connect_tcp` | list of ports | narrows the app to TCP connects on exactly these ports, to *any* host (Lane 0.6); requires a `network` host |
| `bind_tcp` | list of ports | TCP listens on exactly these ports (Lane 0.6) |
| `working_dir` | `[{path, write}]` | the only tree an app may ever be granted |
| `invoke_commands` | bool | lets the app *call* the bus, not merely serve it |

Naming any port switches the app from the whole network to per-port
Landlock rules (`apps::network_policy`); UDP is not filtered in any mode.
Lane 0.6 has the full table and its limits.

`lifecycle` is `on-demand` or `service` (`protocol::Lifecycle`).

**The sandbox is Landlock (ABI 4 filesystem and TCP rules, plus ABI 9's Unix-socket rule and the abstract-socket and signal scopes) and a seccomp denylist.** An app reaches no Unix socket, abstract socket or process outside its own sandbox; its link to the core is an inherited socket pair (docs/lanes/10-sandbox-all-apps.md). That changed the app contract: the core sets `DECKJACK_APP_FD=3` and no longer sets `DECKJACK_APP_SOCKET`, and the SDK's `SOCKET_ENV`, `AppError::NoSocket` and `AppError::Connect` are gone (replaced by `LINK_ENV`, `NoLink`, `BadLink` and `Link`), so out-of-tree code using them no longer builds. It read-grants
`/usr /bin /sbin /lib /lib64 /etc /sys /proc/self` (`apps::SYSTEM_READ_DIRS`), and
Landlock's read set includes `Execute`. `fork`/`clone` are deliberately not
denied and a child inherits both Landlock and seccomp
(`sandbox::denied_syscalls`). **So an app may exec a helper binary, and the helper is
confined too.** That is what makes CLI-backed work possible at all.

Not granted, and not close: `/proc` as a whole (the core's `environ` holds API
keys), `/dev` as a whole (`/dev/input/event*` is a keylogger), the home
directory, and `.deckjack/`.

### Byte transport: the content route, and what is still JSON

**Reading bytes goes through `GET /content?path=<address>`** (`content.rs`,
merged in `wiring.rs` beside the WebSocket, `/healthz`, unlock, passkey and
`/apps` routes). It is behind the session gate (the router applies
`require_session` itself), resolves the path through `files::Namespace` like
every file command, streams the body in 256 KiB pieces, answers `Range` with
206, and sends an `ETag` (no `Last-Modified`), `Content-Disposition`
(`?download=1` for an attachment) and `Content-Security-Policy: sandbox`,
`X-Content-Type-Options: nosniff`. Quick Look is its first consumer (Lane 4).
An app UI that needs a file's bytes uses this route, not `files.read`.

**Everything else is still a JSON command.** `files.read` returns content
inside the response, base64 when it is not valid UTF-8, capped at 16 MiB
(`files::DEFAULT_MAX_READ`); app socket frames are capped at 8 MiB
(`protocol::MAX_FRAME_BYTES`); and the browser socket refuses an inbound frame
over 256 KiB (`ws::Settings::max_frame_bytes`), so **there is no streamed or
chunked write**: `files.write` and `editor.save` carry the whole file in one
frame, which is why the editor cannot save a file over 192 KB (Lane 2).

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
   opening a native terminal"). Quick Look was built alongside them because
   it is the proof of Lane 0 (Lane 4).

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
must **not** join the pre-auth carve-out. **Done** (`content.rs`, tests in
`tests/content.rs`; §2 says what it sends). Deviation: it sends an `ETag` and
honors `If-None-Match`, but no `Last-Modified`.

**0.2 `gpu` grants a device. Done.** `sandbox::Policy` now has
`read_files` and `read_write_files` for non-directory paths, granted with
`AccessFs::from_file` masked onto the usual rights, because Landlock refuses a
directory-only right like `ReadDir` on a file and fails the whole ruleset. The
launcher grants `/dev/kfd` read-write to an app with `gpu = true` (and only if
the device exists), and grants every app `/dev/null`, `/dev/zero`,
`/dev/full` read-write and `/dev/urandom`, `/dev/random` plus a few `/proc`
sizing files read-only (`apps.rs`, `SYSTEM_READ_FILES`). The old
`PENDING_*_FILE_GRANTS` and `Sandbox::unexpressible` are gone.
`crates/deckjack-core/tests/sandbox.rs` checks this against the real kernel:
a confined shell can use exactly what it was granted, cannot list `/dev`, and
an app discovered, planned and spawned through `OsEnv` opens `/dev/kfd`.

**0.3 The shell's app list comes from manifests. Done.** `app.toml` gained
`tag`, `summary`, `default_width`, `default_height`, `compact` and `wide`
(validated in `protocol/lib.rs`; none of them changes the approval
fingerprint), and `system.apps` reports them. The shell's `AppTable`
(`web/src/stores/apps.ts`) merges the built-in entries with that list, the
manifest winning wherever it says something, and the dock, the compact
switcher and the palette's "Open" entries all read it. An installed app the
shell has no entry for joins the dock before the Assistant. The table is a
per-shell store passed to `createWindows` as a required argument, so the test
harness cannot quietly build a different one. Two defects closed with it:
`system.apps` was fired while the socket was still connecting and so always
came back empty (now loaded on each `welcome`), and the PWA `?open=<app>`
shortcut dispatched before the node was connected and never opened anything
(now `openWhenReady`). Still open: mail, calendar and the Assistant keep
placeholder dock slots although nothing backs them yet.

**0.4 The install pipeline. Done, from a local build; signing is out of
scope.** `deckjack install APP_DIR [--binary FILE]`, `deckjack uninstall NAME`
and `deckjack apps` (`install.rs`, CLI in `main.rs`). An install:

- validates the manifest with the protocol crate's own `Manifest::parse` plus
  `check_protocol`, so nothing lands that discovery would grey out;
- checks the binary is an ELF executable whose machine, class and byte order
  match this core's build (`install::Machine::this`), refusing a mismatch with
  an error that names both, e.g. "built for aarch64, but this machine is
  x86_64";
- stages the whole directory under `apps/.staging/` (invisible to discovery,
  which only claims a directory with an `app.toml` directly inside), runs both
  checks on the *staged copies*, then swaps it into `apps/<name>/` with one
  `renameat2(RENAME_EXCHANGE)` (`RENAME_NOREPLACE` for a fresh install). There
  is no instant with a new manifest beside an old binary. A filesystem without
  those flags gets two plain renames, with the app briefly absent but never
  mixed. A refused install leaves the previous version and `apps/` exactly as
  they were;
- refuses any name that is not `valid_name` (so never `..`, `/` or the
  `.staging` directory), and refuses to replace anything at `apps/<name>` that
  is not an app: a stray file, or a directory with no `app.toml` in it.
  `uninstall` draws the same line. (A directory whose `app.toml` is there but
  unreadable is still replaced, and reported as `PreviousUnreadable`);
- reports the approval outcome from the fingerprint: `NotNeeded`, `Unchanged`
  (same fingerprint as the version replaced), or `Required` for a new app, a
  changed fingerprint, or an unreadable previous manifest.

**Install cannot bypass approval because it cannot touch it.** `install.rs`
has no `Grants` handle and does not open the store; the gate is still
`Host::check_grant`, which refuses a launch whose manifest fingerprint differs
from the grant's. `tests/install.rs` proves the path end to end: install v1,
approve it through `Host`, install a v2 that asks for another host, and a fresh
`Host` refuses it as `GrantStale`; the same with unchanged needs launches.

The flip side of not touching the store: `Unchanged` compares the new
manifest's fingerprint with the *replaced manifest's*, both computed by the
running code. It never looks at the stored grant, so it can say "an approval
the previous version had still applies" when there is no such grant (an app
never approved) or when the grant is stale (see the fingerprint change
below). The launch gate still refuses in both cases; only the install report
is wrong.

**The fingerprint is now SHA-256 (a change to the protocol crate).** It was a
64-bit FNV-1a hash. The app author writes both the approved manifest and the
update, so a colliding pair (about 2^32 tries by birthday search; inferred,
not demonstrated) would have let a widened update report `Unchanged` and pass
`check_grant`. `Manifest::fingerprint` now returns `sha256:<hex>` over the same
canonical JSON, pinned in `tests/install.rs` against a digest computed with
`sha256sum` outside Rust. **Consequence: every grant recorded before this
change is stale once**, so an app with `[needs]` (Notes and Tasks, in tree)
is refused at launch as `GrantStale` after the upgrade. That is the safe
direction, but **nothing in the product asks again**: `deckjack install`
reports such an app as `Unchanged` (above), the stale-grant and not-approved
errors tell the operator to run `deckjack approve {app}`, which `main.rs`
does not have (it answers "unknown argument: approve"), or to
approve it in Settings, which has no such screen, and nothing in `web/src`
calls `system.app.approve`. At the time of writing the only way to re-approve
is that bus command (`wiring.rs`, `CrossProject`, so the operator confirms),
sent by hand with `{app}`.

`xtask install` and `xtask dev` now install apps by running the freshly built
`deckjack install` rather than copying files themselves. Linking the core into
xtask to share the function directly was not practical: xtask deliberately
depends only on `thiserror`. The core binary itself is still copied by xtask's
`install_file`; it is not an app.

Tests that defend the claims above, beyond the basic install/refuse cases:
an update loop with a concurrent reader that never finds `apps/<name>/`
missing or a manifest beside another version's binary (killed by forcing the
two-rename fallback and by remove-then-rename, so the atomicity claim is
tested, not just reasoned); six threads installing and uninstalling at once
(the shared `.staging` directory was removed under a concurrent operation,
failing it with ENOENT or EEXIST; `staging_dir` now retries the lost race);
the `deckjack apps` listing of what will not run; and an xtask unit test that
runs a stand-in core and checks `install_into` passes the crate directory,
`--binary` and `DECKJACK_HOME=<prefix>`.

Verified beyond the tests: `cargo xtask install --prefix` installed files,
notes and tasks through the new path; a live `--debug` server on that prefix
listed all three in `system.apps`, launched `files` confined and served its UI;
reinstalling `files` while it ran left the process alive on its deleted inode
and the app still `live`.

Deviations and what is still owed:

- **No registry, no download, no signing.** DESIGN.md's `deckjack install <app>`
  fetches and verifies a signed build; this installs a local directory. Error
  texts in `apps.rs` (unknown app, half-swapped install) and the protocol crate
  (protocol mismatch) still say `deckjack install {app}`, by name, but the
  command that exists takes `APP_DIR`, so following the hint fails. The
  approval errors name `deckjack approve {app}` and Settings, neither of
  which exists (above).
- **A running server does not notice.** `Host::refresh` runs only at startup,
  so an install or uninstall takes effect on restart; until then the server
  keeps the manifest it read, and the binary it launches is whatever is on
  disk. That is not a widening (it runs under the old, approved manifest's
  confinement, and a command list that disagrees is refused at handshake),
  but a mid-session install is not live. Notifying the node is owed.
- An update does not stop or restart the running app; the old process runs on
  until it exits.
- An approval outlives `uninstall`. It is keyed by fingerprint, so it applies
  again only to a reinstall asking for exactly what was approved.
- The two-rename fallback for a filesystem without `RENAME_EXCHANGE` is not
  exercised as a success path by any test; the reader test only shows it is
  not atomic, which is what this section says.
- The xtask test uses a stand-in core. The release path (`make install`) was
  not run end to end after the reroute; one debug `cargo xtask install
  --prefix` run was.
- A crash mid-install can leave a directory under `apps/.staging/`. It is
  inert (discovery skips it) and not cleaned up automatically.
- The ELF table names x86, x86_64, arm, aarch64 and riscv; on any other
  architecture every binary is refused, with both sides named. Android
  (Lane 0.5) uses the same `e_machine`, so an aarch64 Linux build and an
  Android one are not told apart by this check.

**0.5 No cross-compile.** `xtask` has no `aarch64-linux-android` target, so the
Fold cannot run any app binary. **Blocks the phone half of decision 3** for
anything with a backend.

**0.6 Network granularity. Done, to the limit of what Landlock can do.**
`[needs]` gained two keys beside `network`:

```toml
[needs]
network = ["cloud.example.net"]   # who the app talks to: shown to the operator
connect_tcp = [443]               # TCP ports it may connect to, on any host
bind_tcp = [8080]                 # TCP ports it may listen on
```

What a manifest now resolves to (`apps::network_policy`, into
`sandbox::Network`, which replaced the old `Policy::network: bool`):

| manifest says | the app gets |
| --- | --- |
| nothing | no TCP connect or bind at all (unchanged) |
| `network` hosts, no ports | the whole network, warned on every launch (unchanged, so approved manifests keep their meaning; the warning now says naming ports would narrow it) |
| any `connect_tcp` / `bind_tcp` | Landlock handles TCP and adds one `NetPort` rule per port: connect only to `connect_tcp`, bind only on `bind_tcp`; everything else is `EACCES` |

The flat keys are a deliberate shape, not `network.connect_tcp`: TOML cannot
make `network` both the existing host array and a table, so a nested form
would have meant migrating every networked manifest. No in-tree `app.toml`
declares the network, so none needed migrating either way.

Validation (`protocol/lib.rs`): port 0 is refused (to Landlock it means "a
port the kernel picks", which no manifest author means), a port listed twice
is refused, a port above 65535 is a parse error, and `connect_tcp` without
any `network` host is refused, because the host list is what the operator
reads when approving. The reverse is refused too: `network` hosts with
`bind_tcp` but no `connect_tcp`, because once any port is named the app may
connect only on `connect_tcp`, so those hosts would be unreachable while the
approval text still promised "network access to" them (a review finding; the
first cut accepted it and resolved it to no outbound TCP). `bind_tcp` alone
needs no host and still counts as a need, so a listen-only app goes through
approval like any other.
`summarize` names the ports in the approval text.

**The fingerprint covers the ports.** `Manifest::fingerprint` already hashed
`[needs]` whole; the two new fields are skipped when empty, so a manifest that
names no ports hashes exactly as before ports existed (pinned in
`a_fingerprint_covers_the_port_grants_and_not_their_order`). This lane pinned
the FNV-1a value computed before the change; when Lane 0.4 replaced the hash
the pin became the SHA-256 digest of the same no-ports canonical JSON, which
`sha256sum` reproduces outside Rust. So the pin shows that naming no ports
leaves the canonical form untouched. It says nothing about grants recorded
under FNV-1a, which all went stale once when the hash changed (Lane 0.4).

The port lists are sorted on a copy before hashing, so reordering ports does
not re-ask the operator, while adding one, changing one, or moving one between
connect and bind does.

A kernel without Landlock network rules (below ABI 4) now refuses a ported
manifest as it already refused a network-less one, rather than widening it to
the whole network. This machine's kernel reports Landlock ABI 10.

**Limits, stated so nobody plans against them:**

- **Ports, not hosts.** `connect_tcp = [443]` reaches *every* host's 443. The
  host list is a statement to the operator that nothing enforces. Closing that
  still needs the allowlisting proxy DESIGN.md describes.
- **UDP is not filtered at all**, in any mode including "no network": DNS over
  UDP, QUIC and anything else datagram-based is outside Landlock's network
  rules through ABI 10. An app that resolves names over TCP fallback needs 53
  in `connect_tcp`.
- **Loopback is a host like any other.** `connect_tcp = [5432]` lets the app
  reach a database on this machine's 5432 as well as anyone else's.
- Resolver config (`/run/systemd/resolve` and friends) is now granted only to
  an app that may connect out (whole network or any `connect_tcp`), not to a
  listen-only app.

Tests: `tests/apps.rs` (planning, validation, approval, fingerprint, the
old-kernel refusal) and `tests/sandbox.rs` against the real kernel — the test
binary re-execs itself confined and reports per port: a granted connect port
reaches a live loopback listener and another live listener is refused with
`EACCES`; the same for bind; a connect grant is not a bind grant; and the
controls (whole network reaches both, denied reaches neither). The wiring
test writes an `app.toml` with `connect_tcp`, discovers, plans and spawns it
through `OsEnv` exactly as the core does, and checks the launched app gets the
manifest's port and is refused on another. 20 one-line source mutations of
the new code were applied in the first pass and each failed at least one of
these tests. That set was not complete: review found that dropping the
`bind_tcp` sort in the fingerprint survived (only `connect_tcp` order was
tested); a bind-order assertion now kills it, and the hosts-plus-bind-only
refusal was mutation-checked the same way. Other surviving mutations may
exist; the list above is what was tried, not a proof of coverage.

Still owed: the ports reach the operator through `summarize`, which is the
text of the not-approved and stale-grant errors; nothing on the web side was
changed or checked for how that text is shown. Per-host enforcement remains
the proxy's job and is not started.

---

## Lane 1 — File manager

**Exists:** `deckjack-files`, plus `files.rs` in the core (with its own
test file). This lane finishes it rather than starting it.

**Owes:** multi-select and bulk actions; move and copy with progress; a real
rename flow; sort and filter; drag and drop that also works by long-press on
touch; thumbnails for images and video (cache under `~/.deckjack/cache`,
`apps::CACHE_SUBDIR`); "open with" that routes to Quick Look or an app; free-space and
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

**Progress (first pass).**
- *Selection:* `/kit/selection.js`, shared by any app list: click, Ctrl/Cmd and
  Shift, arrows, select-all, and a touch selection mode entered by long-press
  or the Select button. Selection is by key, never by index.
- *Quick Look from Files:* Space, Enter, or the Quick Look button opens the
  shell's preview with the folder's files to step through. App frames reach
  the shell through `/kit/shell.js` and `web/src/shell/frame-bridge.ts`, which
  accepts only same-origin messages from frames the shell hosts, and only two
  requests: `shell.quicklook.open` (`FRAME_COMMANDS`) and `shell.app.open`
  (Lane 2's open-app contract, checked by `open-app.ts`). That is a contract,
  not a boundary; see the note on app UIs below.
- *Commands:* `files.move` and `files.copy` (`write`; never overwrite; all or
  nothing; copy reports progress, writes under a hidden partial name and stops
  when its dispatch is dropped), and the trash: `files.trash`,
  `files.trash.list`, `files.trash.restore` (`write`) and `files.trash.empty`
  (`destructive`). The trash is `/.Trash` in the freedesktop layout.
  **Deviation from this brief:** it said `files.trash` should be `destructive`;
  it is `write`, because §11 calls `destructive` what cannot be undone from
  inside Deckjack, and the trash is the undo. Emptying it is `destructive`.
- *Files UI:* inline rename (F2), Cut/Copy/Paste across folders, Delete to
  the trash with Undo, and a Trash view with Restore. Emptying the trash is
  done from the palette, because a destructive command's confirmation is drawn
  by the shell and an app frame can neither draw nor answer it.

**Progress (second pass).**
- *Thumbnails:* `files.thumbnail` (`read`, `crates/deckjack-core/src/thumbnail.rs`)
  takes `{path, size?}` and returns a JPEG (opaque) or PNG (transparent)
  thumbnail as base64, with its real size and whether it came from the cache.
  The source is resolved through `Namespace::resolve`, so the boundary and the
  `.deckjack/` refusal are the ones every file command uses. Decoding is the
  `image` crate's pure-Rust JPEG, PNG, GIF, WebP and BMP decoders, in the core,
  with dimension and allocation limits, a 48 MiB source cap, EXIF orientation
  applied (after shrinking, so it never copies a full-size picture), never
  enlarged, sizes rounded up to 128/256/512, at most `cores/2` (1 to 4)
  decodes at once, and one 384 MiB decode-memory budget shared by all of them:
  each decode reserves the size its header declares before allocating, waits
  while others hold the budget, and is refused if it alone exceeds it. The cache is `<install>/cache/files/`
  (`thumbnail::cache_dir`), keyed by SHA-256 of address, mtime in nanoseconds,
  length and size; capped at 64 MiB with least-recently-used eviction (a hit
  touches the entry); deleting it while running costs a regeneration; a cache
  that cannot be written is logged and the thumbnail is still returned. By
  default the working directory is the install directory, so `cache/files` is
  reachable through Write-level file commands; the cache therefore refuses a
  `cache` or `cache/files` that is a symlink or not a directory, opens entries
  with `O_NOFOLLOW`, and counts and evicts only regular files named like its
  own entries (64 hex digits), so a ReadOnly thumbnail request cannot delete or
  read anything it did not write. The checks are path-based, so a swap between
  check and use is not closed; the entry-name rule is what limits that window.
  It is
  a required argument of `files::register_commands`, so nothing can register
  the file commands without one. The Files UI asks for thumbnails of image rows
  as they scroll into view (IntersectionObserver on the row), three at a time,
  and remembers them per frame by address, size and time.
  **Video thumbnails are not done.** They need ffmpeg, and running ffmpeg on an
  untrusted file in the unconfined core is the wrong place for it; it belongs
  in a confined helper, which does not exist yet.
- *Sort and filter:* name (numeric-aware), size, modified, kind (extension),
  either direction, folders always first; the choice is remembered in the
  frame's `localStorage` (guarded; a blocked store just means the default). A
  filter field (`/` to focus) narrows by name; select-all selects only what is
  shown; going to another folder clears it.
- *Drag and drop:* pointer-based, one code path for mouse and touch. A mouse
  drag past 5 px, or on touch a long-press followed by a move, carries the
  row (or the selection it belongs to) onto a folder row or a breadcrumb; the
  drop is `files.move`. While a touch drag is armed, `touchmove` is
  prevented so the page does not scroll and cancel the pointer. Not offered in
  the trash, and never onto itself, into itself, or where it already is.
- *Free space:* `files.space` (`read`, `crates/deckjack-core/src/space.rs`):
  `statvfs` of the working directory as `{total, free, available}` bytes,
  `available` being what the server's user can write. Shown in the footer,
  flagged under 1 GiB or 5%, re-read after every listing, and a paste of
  files whose total is known is refused before it starts if it cannot fit.
- *Copy into the folder it came from:* `files.copy` takes `keep_both`; a taken
  name then becomes "name copy", "name copy 2", ... (extension kept, a folder
  or dotfile has none, a copy of a copy counts on, never over 255 bytes, two
  same-named sources in one request both land). The hidden partial name a
  copy grows under is shortened to fit 255 bytes too; before that, no file
  whose name was over about 235 bytes could be copied at all, with or without
  `keep_both`. Pasting a copy where it came
  from sends it; a copy refused elsewhere for a taken name offers **Keep
  both**. `files.move` does not accept it: a move that renamed would leave a
  file under a name nobody chose.
- *Open with:* Enter (or a tap) on a text file calls
  `window.deckjackShell.openApp("scribe", { path })` per the open-app
  contract; the selection bar's **Open with…** and the context menu offer the
  text editor and Quick Look. Feature-detected at the moment of use: with no
  `openApp` the button is hidden and Enter falls back to Quick Look; a `false`
  answer is reported. This lane built the calling side and tests it against a
  stub (`web/src/apps/files.test.js`). The shell side is Lane 2's and is built
  (frame bridge, `shell.app.open`, the windows store's `openApp`); each half
  is tested, but the whole chain from a Files row to an editor window showing
  that file has not been run in a browser. Lane 2's live check called
  `shell.openApp` from the Files frame by hand; it did not go through this
  button.
- *Context menu:* right-click, the context-menu key or Shift+F10.
- *Defects fixed on the way.* The UI read `len` where the core sends `size`,
  so every file showed "0 B"; and it kept the core's node-qualified addresses
  (`r620:/a`) as paths, so Up from any folder listed `/r620:` and failed. Both
  were invisible to the first pass's tests because the fake bus answered in a
  shape the core never sends; the fake now answers like the core. And the
  selection bar sat above the list, so the first click of a double-click
  showed it and pushed every row down, and the second opened a different
  folder (seen in Chromium); it now sits below the list.
- *Verified* against a live server (debug mode, headless Chromium): rows,
  sizes and free space (matching `df`), thumbnails rendered from the cache
  path, sort, filter, Backspace up, same-folder paste as "today copy.md",
  the context menu, a mouse drop on a breadcrumb, and a real touch long-press
  drag onto a folder, each checked on disk.
- **Deviation from the touch contract:** long-press is selection mode (and
  then a drag), not the context menu. On touch the selection bar is the menu.
- *Still owed:* video thumbnails (above); the cross-node view, because no
  remote file transport exists and this lane did not build one; a test of
  "open with" end to end, Files row to editor window (both halves exist,
  above); "open with" for apps beyond the editor (the list is one entry,
  `OPEN_WITH` in `files.js`, not read from the AppTable);
  free space for a folder that is another mount inside the working directory
  (it reports the working directory's own filesystem); the semaphore that
  bounds the number of concurrent decodes has no test that would fail without
  it (the memory budget does); the decode budget counts only the size the
  header declares, not a decoder's own working buffers; and of the
  reviewer's 18 surviving JS mutations, only the six it named (pointer
  cancel, long-press context menu, move exempt from the free-space check,
  clipboard pruning on drop, primary-button drags, the 5% low-space branch)
  now have killing tests; the other twelve were not listed and were not
  re-examined.

**App UIs run as the operator.** An app's UI is served on the shell's origin
and its socket carries the operator's session cookie (DESIGN.md, "UI
routing"). So an app's UI can call any bus command, `terminal.open`
included, whatever its manifest asked for: Landlock confines the app's
*process*, not the JavaScript it serves. **Decided 2026-11-16, not yet
built:** the fix is in `docs/APP-UI-ISOLATION.md`. App UIs move into
opaque-origin sandboxed frames that reach the bus only through a broker in
the shell, the server enforces each app's approved scope, and confirmations
become tickets held by the server. Until its stage 4 lands, this paragraph
stays true.

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

**Progress (first pass).**
- *The app:* `crates/deckjack-editor`, the Files pattern: a binary that serves
  its embedded UI and nothing else, with `tag`, `summary` and sizes in
  `app.toml`, so it reaches the dock from its manifest (checked live: "Ed" in
  the dock with no shell change). Empty `[needs]`, no approval dialog.
- *Commands, and a deviation:* `editor.open`, `editor.revert`, `editor.find`
  (`read`) and `editor.save` (`write`, parameters redacted from the audit log
  like `files.write`) are registered **by the core**, in
  `crates/deckjack-core/src/editor.rs`, over the same `files::Namespace`, and
  `files::register_commands` registers them itself so one cannot exist
  without the other. They cannot live in the app binary: `Manifest::validate`
  refuses `working_dir = "."` (Landlock would grant `.deckjack/` with it), and
  an app calling `files.*` over the bus runs as agent session `app:editor`,
  for which nothing in the product issues a grant, so every save would be
  refused at runtime. What the commands add over `files.*`: `editor.open`
  decides large-file handling in one place (up to 192 KB editable, up to
  4 MB whole and read-only, above that the last 256 KB read-only, starting on
  a line, with `before` to page backwards); it returns a content hash
  `version`; `editor.save` refuses with `status: "conflict"` (an answer, not
  an error, and nothing written) when the file on disk no longer has that
  version, when a "new file" save would overwrite one, or when the file was
  deleted, unless `force`. A hash rather than `modified`, because `modified`
  is whole seconds and misses a write in the same second. `editor.find` is
  literal text with line and character columns, for agents and the palette;
  the UI searches its own buffer.
- *Why 192 KB:* the browser socket refuses an inbound frame over 256 KB
  (`ws::Settings::max_frame_bytes`), and a save is one frame. The UI measures
  the real frame before sending and refuses with a sentence rather than
  dropping the socket.
- *The UI:* CodeMirror 6, vendored as one offline ES module
  (`ui/vendor/codemirror.js`, 943 KB, 337 KB gzipped), built by
  `npm run build:editor-vendor` from `web/scripts/codemirror-entry.js` with
  the esbuild Vite already ships, and **checked in**, so `cargo build` needs no
  Node. `web/src/apps/editor-vendor.test.js` rebuilds it and fails when the
  checked-in copy is stale or imports anything at runtime. Owed items done:
  syntax highlighting (about twenty languages, chosen by file name, painted
  with the theme's `--dj-color-syntax-*` tokens), several open files as tabs (one view, one
  `EditorState` per file, so undo is per file), soft wrap (remembered per
  device), go-to-line, undo across saves and across a revert (a revert is one
  ordinary undoable change), find and replace, an explicit Save button and
  Ctrl/Cmd+S, external change detection (each open file's `modified` and size
  polled with `files.stat` every 4 s and on focus: a clean buffer reloads
  itself, a dirty one is told and offered "Reload theirs" or "Keep mine"),
  a file picker over `files.list`, Quick Look offered for a file that is not
  text, and unsaved text kept per window in this device's storage and
  restored after a reload, saved against the version it was based on.
- *Touch:* Save, Undo, Redo, Find, Line, Wrap and Revert are buttons; the
  search and go-to-line panels open at the top and are sized to
  `--dj-touch`; the gutter is 2.75rem and a tap on a line number selects the
  line. The keyboard: the frame is padded by however much of it the top
  window's `visualViewport` says is covered, re-measured on its `resize` and
  `scroll` events, and the cursor is scrolled back into view.
- *The open-app contract, shell side:* `shell.openApp(name, { path })` in
  `/kit/shell.js` (also `window.deckjackShell`) posts
  `{ type: "shell.app.open", app, path }`. The frame bridge accepts it only
  from same-origin frames it hosts; `web/src/shell/open-app.ts` checks the
  name is in the AppTable and the path is a path (no NUL, no `..`, a bare
  `a/b` made `/a/b`). It dispatches the shell command `shell.app.open`, which
  the palette and an agent can call with the same rules. The windows store's
  `openApp` focuses that app's most recent window (restoring it through
  `restore`, so the node is told and tiling re-flows, when it was minimized)
  or opens one; a new window's frame loads `/apps/<app>/?window=<id>&path=<path>`, and
  an already-open one is sent `{ type: "deckjack.app.open", path }` instead,
  because changing its URL would reload it and lose unsaved text. Found and
  fixed while checking it live: the compact shell renders the focused window
  in one reused panel, so a frame URL fixed at mount showed the previous app.
- *Verified:* 28 Rust and 41 web tests for this lane, and 76 one-line
  mutations applied and watched fail (four survived the first pass; the tests
  were strengthened and one redundant line removed). Live, against a debug
  core on loopback: the editor's UI and bundle are served from the binary;
  open, save, a stale save refused as a conflict with the other write intact,
  find, a 300 KB file read-only, `.deckjack/` refused. In headless Chromium at
  phone size: a Rust file rendered highlighted, typed into, saved with the
  button and checked on disk, search panel opened by its button, an external
  write picked up by the poll; and from inside the Files frame,
  `shell.openApp("editor", { path: "/README.md" })` opened an Editor window
  showing that file (verified before stage 3 of docs/APP-UI-ISOLATION.md
  renamed the apps; the call is now `openApp("scribe", …)`, and Files is
  `filer`).
- *Not verified, and still owed:* the exit check (a commit made from the
  phone) was not run. The keyboard inset is unit-tested and untried on a
  real phone with a real on-screen keyboard. Files over 192 KB cannot be
  saved until there is a chunked or streamed write. Saving into a folder
  that does not exist fails (`files.write` does not create parents). A tail
  view does not follow a growing log. Files' "open with" (Lane 1) now calls
  this contract, but the joined path, a Files row to an editor window on that
  file, has not been run end to end in a browser. No language server, no
  project-wide search.

**Progress (review fixes).** Ten review findings were checked against the
code; all ten were real, and each was fixed with a test seen failing first
and then mutation-checked (25 one-line mutations, all killed).
- *`editor.find` cost:* it runs without asking on a blocking thread nobody
  can cancel, so it is now linear in the file whatever the query
  (Knuth-Morris-Pratt over folded characters, checked against a brute-force
  search on every small case), a query is capped at 1024 characters
  (`FIND_QUERY_MAX`, refused as bad parameters above it), and a match's
  excerpt is cut by walking back from the match rather than forward from the
  start of a possibly 16 MB line. No timing test: the old comparison loop
  compiles to `memcmp` and was only about 2.6 times slower on a 4 MB line,
  too close for a test that must not flake.
- *A paging window that is one line ending in a newline* is kept, not
  emptied; a unit test pins it, so "Load earlier" always progresses.
- *Typing during a poll-triggered reload* is no longer erased: the reload
  re-checks that the buffer is still clean when the disk's text arrives, and
  otherwise shows the "changed on disk" choice.
- *Theme tokens:* the editor named three tokens that do not exist
  (`surface-hover`, `surface-raised`, `warning-soft`), which left a light
  theme with a near-black active line, gutter and conflict bar. They are now
  `hairline`, `raised` and `warn-soft`, and a test fails if the editor's UI
  names any `--dj-*` property the token stylesheet does not define (which
  also caught `--dj-text-xs`, now `--dj-type-micro-size`).
- *Unsaved text on the device:* kept under the shell's per-window key
  (`windowStorageKey` in the windows store, `deckjack.window.<id>`), which
  the store now removes when the window is closed; a draft that could not be
  stored (storage full or blocked) is reported in the status line instead of
  silently dropped.
- *Saving a new file over an existing one:* the conflict now offers "Open it"
  (the existing file, in its own tab, with the new text kept) instead of a
  "Reload theirs" that did nothing, and "Save anyway" writes to the path that
  was chosen instead of asking again.
- *Tests added for behavior that had none:* the keyboard re-pad on the
  visual viewport's events; a poll during the editor's own save; a
  same-second write caught by size; a reloaded file that grew too big
  becoming read-only; the check on window focus; the paging cap; the windows
  store's restore through `openApp`; and a frame launch that arrives before
  the frame has loaded.
- *Still owed from these:* drafts kept for a window that is closed from
  another device, or before this change, are not removed on this device, and
  nothing removes them on sign-out, because the shell has no sign-out flow
  to hook. The time-of-check gap between the version check and the write in
  `editor.save` is unchanged.

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

### Status (lane 3 pass)

**Both known defects are fixed, each with a test that was red first.**

- *Zombie shells.* The output pump reaped the child through the session,
  which it holds weakly; `kill_all` and `close` forget the session first, so
  the pump found it gone and never called `wait`. It now holds the pty
  strongly, drains the stream to its end whether or not the session exists,
  and reaps unconditionally (`terminal.rs`, `pump`). A second defect was
  hiding under it: an interactive shell puts each job in its own process
  group, so `killpg` left `cmd &` running as the host user after a lockdown
  and holding the pty open, so the stream never ended and even a correct pump
  never reached `wait`. `Hangup::Now` now kills every member of the shell's
  session (found through `/proc` on Linux and Android; elsewhere it stays
  group-only). `tests/terminal_native.rs` asks the kernel: a real shell and a
  real background job, `/proc/<pid>/stat` before and after. Red was an
  observed `Z` state and a surviving `sleep`. A reaped pid is never signaled
  again (pid-reuse guard); that guard is defensive and **not tested** — pid
  reuse cannot be provoked from a test.
- *Cancellation dropped at the wiring seam.* `ws` flipped `Call::cancel` on a
  `cancel` frame and on socket close; `BusDispatch` never read it, so a
  cancelled `terminal.read` parked until the shell next printed — forever,
  for a quiet one — holding an in-flight slot. `Running::split_until`
  (`command.rs`) drops the command future when the signal fires and still
  journals the completion (`Failed`, detail "was cancelled"), and the socket
  answers with error code `cancelled`. Test: `tests/wiring.rs`,
  `a_cancelled_terminal_read_answers_instead_of_parking_forever`, over the
  real socket.
  **Only `terminal.read` is cancelled this way** (`wiring::DROPPED_ON_CANCEL`).
  The first pass applied it to every command, which review caught: dropping
  a future does not stop `spawn_blocking` work under it, so `files.write`,
  `files.delete`, `files.trash` or a lockdown "cancelled" mid-flight still
  landed while the journal said `Failed` and the client heard `cancelled`,
  and the in-flight cap stopped bounding blocking threads. Every other
  command runs to its end and answers with what it did, as `ws::Call`
  documents (cancellation is cooperative). Test:
  `a_cancel_does_not_report_blocking_work_as_cancelled_while_it_lands`.
- *Kill switch after close and after exit* (found in review). `close` and
  sign-out send `SIGHUP` and forget the session, so a job that ignored the
  hangup was out of a later lockdown's reach; and lockdown skipped exited
  shells, so a `nohup job >/dev/null &` left behind by a shell that exited
  survived it. The registry now keeps closed terminals' ptys until the pump is
  done and nothing lingers in the session, and `kill_all` signals those and
  exited shells too. After the shell is reaped its session is scanned only
  while the shell's pid is free (the kernel does not reuse a pid any process
  still carries as its session id; a pid that has been reused may lead an
  unrelated session). Tests: two in `tests/terminal.rs` (fake pty) and two in
  `tests/terminal_native.rs` (real jobs, `/proc`). The "pid is free" guard
  and the release of finished entries are **not tested**.

**Owed items, built:**

- *Reattach after reload and after a network drop.* The server already kept
  the pty and a bounded ring (`SCROLLBACK_BYTES`, 256 KiB). `terminal.read`
  now says `reset: true` when the read starts before the ring's oldest byte,
  so the client clears before replaying a stream that may begin mid-escape.
  The panel remembers which terminal a window showed (localStorage, per
  window id; a private window falls back to the newest live terminal nobody
  on the page shows), replays from cursor 0, and on a drop waits for the bus
  and resumes from the same cursor (`shell/terminal.ts`, `follow`).
  `Bus.call` gained an `AbortSignal` so a panel that switches tabs or closes
  cancels its parked read on the node instead of abandoning it.
- *Scrollback search*: Find (or Ctrl+Shift+F) over the emulator buffer,
  wrapped rows joined so a match broken across rows is found, newest-first
  stepping, match-case toggle. Known limit: a match after a wide (CJK)
  character is highlighted one cell early.
- *Copy and paste on iOS Safari.* Copy tries the async Clipboard API and
  falls back to `execCommand("copy")`; with no selection (xterm cannot select
  by touch) it opens the scrollback as plain text in a native textarea, where
  iOS's own selection handles and Copy work. Paste tries `readText` and,
  where the browser withholds it (iOS on plain http has no Clipboard API at
  all), opens a paste box the ordinary long-press Paste works in; the text
  goes through `term.paste`, so bracketed paste is honored.
- *Touch keyboard bar* on coarse pointers: Ctrl (a one-shot latch), Esc, Tab,
  arrows (application-cursor mode honored; Ctrl+arrow sends the xterm
  modifier form), `|`, `~`, `/`. Buttons do not take focus, so the soft
  keyboard stays up. The panel pads itself by the `visualViewport` keyboard
  inset so the bar is not hidden behind the iOS keyboard.
- *Multiple sessions with a switcher*: a tab strip of the session's
  terminals, new and close (closing a running shell asks first, at the
  backend's declared `write` risk), exited shells struck through.
- *Resize on rotate*: the emulator refits every layout frame; the shell is
  told only the settled size, 150 ms after the last change, and only when it
  differs — one `SIGWINCH` per rotation.
- *80 columns*: the font shrinks (to 8 px at the smallest) until 80 columns
  fit; below that the panel keeps 80 and scrolls sideways.
- Keystrokes go one `terminal.write` at a time, typed-ahead input coalesced,
  because the node runs each invoke as its own task and two writes in flight
  are not ordered. Reordering was reasoned from the code, **not observed**.
  Each keystroke carries the terminal it was typed into, fixed when typed:
  the first pass read the active tab when the queue flushed, so input typed
  before a tab switch went to the other shell (found in review).

**Verified how:** Rust tests listed above plus `tests/terminal_reattach.rs`
(reload replay and `reset`, real shell over the product bus); web tests in
`shell/terminal.test.ts` (logic), `stores/bus.test.ts` (abort to cancel) and
`components/panels/TerminalPanel.test.ts` (the mounted panel against the
real stores). Every new test was mutation-checked. Review found the first
pass overclaimed here: search, the copy sheet, the keyboard inset, close
confirmation and the replay `reset` were tested only as pure helpers, and
removing their wiring in the panel left the suite green. Each now has a
panel test, and each of those mutations (and the tab-switch write target)
was confirmed to fail it. A live `--debug` node was driven over its real WebSocket:
open, read, a cancelled parked read, Ctrl-C interrupting `sleep 300`, a
dropped socket and a new one reattaching and replaying, then close with
`/proc` showing the shell reaped.

**Still owed / not verified:**

- **Nothing here was tried on a phone.** The iOS paths (paste box, select
  sheet, keyboard inset, no focus theft from the bar) are built to how iOS
  Safari is documented to behave and tested in happy-dom, not on a device.
  The exit check — `cargo test` driven from the phone through a wifi drop —
  has not been run.
- Terminal sessions do not survive a server restart (a pty master is a file
  descriptor in the process), by design.
- A shell that exits normally while a `nohup`/disowned job still holds its
  pty is reaped only when that job lets go of the pty.
- The step-up gate on `terminal.open` noted in `wiring.rs` is still open.

---

## Lane 4 — Quick Look

**Built; see Status below.** Brief kept here so later work matches it.

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

### Status (this pass)

**Done.**

- *Viewer per family* (`web/src/shell/quicklook.ts`, `components/QuickLook.vue`,
  `components/quicklook/`): image, video, audio, PDF, text/source, Markdown,
  archive, web (HTML/SVG/XML). Refusals name the reason: camera RAW,
  undecodable containers, 7z/RAR/xz/zstd/bzip2 (by name), office formats,
  binary files detected after fetching, and a decode failure reported by the
  browser itself (`@error` on img/video/audio).
- *Navigation*: arrows, Space/Escape, `i` for details; touch swipe (pure
  `classifySwipe` in `shell/gestures.ts`); never from a player's controls.
- *Zoom and pan*: two-finger pinch anchored between the fingers, pan clamped
  to the stage, double tap, wheel, `+`/`-`/`0` (`ImageView.vue`, geometry in
  `gestures.ts`). Panning a zoomed photo and lifting from a pinch do not swipe.
- *Seeking*: verified live, headless Chromium against a real node: a 17 MB
  WebM played, seeked to 71.5 s, ready state 4, every `/content` request a
  `Range` answered 206. curl: no-range body byte-identical, mid-file range
  identical.
- *PDF*: pdf.js 6.4.299 (legacy build), vendored by npm and bundled; worker
  and run-time data (cmaps, standard fonts, wasm decoders, ICC) served by the
  core under `/pdfjs/` via a Vite plugin; lazily imported. The browser's own
  viewer was rejected: the content route's `CSP: sandbox` blocks Chromium's
  PDF plugin, and it cannot be paged or measured from the shell. Verified live:
  40 pages, page-through, painted canvas, phone width at 3x density.
- *Never executes*: Markdown via an escape-first renderer (`shell/markdown.ts`,
  no library, no `v-html` of anything it did not build; remote images not
  loaded, links only http/https/mailto). Every pass is linear, because it runs
  on the shell's main thread on files anyone can drop in: brackets matched once
  per line, `**`/`__`/`~~` paired against a precomputed closer table, one
  address-scan budget per line, nesting capped (16 quote/list levels, 2 link
  levels) with the rest shown as text. Hostile 512 KiB inputs (unclosed `[`,
  `[a](`, `**a `, 20,000 nested `>`) render in under 150 ms in Node; before the
  fix they took seconds to minutes or overflowed the stack. HTML/SVG in `sandbox=""` frames, with
  a Source toggle rendered as text. Verified live: hostile README, HTML and SVG
  fixtures set no `window.pwned`.
- *Archives*: `files.archive.list` (`crates/deckjack-core/src/archive.rs`,
  read-only, registered in `files::register_commands`): zip incl. Zip64, tar
  (ustar, GNU long names, pax), tar.gz, single-file gzip; chosen by magic
  bytes. Bounded: 5,000 entries (client may lower), 64 MiB zip directory,
  512 MiB inflated for tar.gz (enforced inside a member), 64 KiB long names;
  truncation and its reason are reported. Anything that is not a regular file
  (a FIFO would block the open forever) is refused, the same rule as
  `files.read`. Cross-checked against Python
  `zipfile` (70,001-entry Zip64) and GNU tar (gz and pax).
- *Metadata*: size/modified from `files.stat`; dimensions, duration, pages,
  entries, lines from the viewers; JPEG EXIF parsed client-side
  (`shell/exif.ts`) from a 128 KiB range, because the bytes are already
  reachable and the core then needs no image parser. Fetched only while the
  panel is open.

- *Open fully*: `quicklook.handoff`/`openFully()` hand text, Markdown and web
  source to the `editor` AppTable entry (`crates/deckjack-editor`, Lane 2),
  feature-detected (`apps.isKnown`); no button when it is absent. Required
  `apps`/`openApp` env fields, wired in `context.ts` and the harness through
  one shared `openAppVia(windows)`, which calls the window store's
  `openApp(app, { path })`. Its fallback to `open(app)`, which drops the
  path, is for a store without `openApp`; the shell's store always has it
  since Lane 2 merged, so that branch is reached only by a unit test. The
  shell test in `QuickLookViewers.test.ts` drives the real stores and asserts
  the editor window's `launch.path` is the previewed file. **Not verified in
  a browser:** neither a new editor window opened from Quick Look nor the
  `deckjack.app.open` message to an already-open one has been run live.

**Defects found and fixed on the way.** The content route sent at most one
8 MiB window: a non-range GET of a larger file returned a truncated 200, and
`Range: bytes=0-` returned a `Content-Range` claiming the whole file over an
8 MiB body. It now streams exactly the span it declares. Quick Look's
`z-index` named an undefined token and fell back to 90, under every focused
window and the top bar, which hid its own Close button; it now uses
`--dj-layer-sheet`.

**Still owed.**

- *Open fully* in a browser, end to end (see Done).
- RAW previews (needs a node-side developer), video codec in metadata (not
  observable from the browser), office formats (Lane 6).
- Exit check on the phone itself: verified only in headless Chromium at a
  phone viewport, not on a device. No 2 GB video was tried (17 MB).
- Unicode in zip names without the UTF-8 flag is decoded as CP437; DOS zip
  timestamps are local time reported as UTC.

---

## Lane 5 — Creative suite

**Scoping briefs for Lanes 5–7 are in `docs/lanes/`:** `5a-photo.md`,
`5b-vector.md`, `5c-convert.md`, `5d-video.md`, `5e-daw.md`,
`6-office.md`, `7a-cad.md` and `7b-circuit.md`. Each one gives a useful
first subset, the engine choice, commands, the touch contract, how it fits
the sandbox and `docs/APP-UI-ISOLATION.md`, and the spikes that would settle
its open questions. They were researched on 2026-11-16 and are proposals,
not decisions. Where one disagrees with the short entries below, the brief
is the more considered view, and the entry below stays until the brief is
accepted.

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

**The dock is built from manifests** (Lane 0.3). A new app reaches it by
setting `title`, `tag`, `summary` and its sizes in `app.toml`; nothing in the
shell has to change.

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

The other ways tests here have passed while proving nothing, and the traps in
the route tests and the end-to-end harness, are in docs/notes/testing.md. The
security invariants a lane must not break are in docs/notes/security.md.
