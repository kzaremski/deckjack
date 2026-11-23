# Lineage, decisions and how work goes wrong here

Where the code came from, decisions the operator took that are not visible in
the code, and the ways parallel work on this repo has failed. Decisions that
change DESIGN.md are also recorded as dated deltas at the top of that file.

## JackLink

The original contents of this repository (MIT, recoverable at `7b89edb`): a
FastAPI "portable computing gateway" that served a browser desktop over a USB
tether, about 2,300 lines of Python and 2,100 of per-app JavaScript. Its
battery check tried `termux-battery-status` first, so its first target was an
Android phone. Seven of its eight apps were real web UI; only the VNC app
streamed pixels.

Holes measured against its own threat model ("plug into an untrusted machine"),
kept as a checklist for Deckjack's equivalents:

1. A SOCKS5 proxy with no authentication on `0.0.0.0:1080`, started
   unconditionally and independent of the web token.
2. An 8-character token with no rate limit on `/login`.
3. No path containment: `root_path` was decoration; handlers resolved any
   absolute path. Deckjack's file layer resolves, follows symlinks before the
   containment test and refuses `.deckjack/` from the first commit.
4. `/api/proxy` was an authenticated SSRF that checked only the scheme.
5. Plain HTTP throughout.

Re-check 1, 3 and 4 whenever a proxy, file or fetch endpoint is added. Two
more lessons: its WebDAV server was written, imported, advertised in the
README and the startup screen, and never mounted, so verify an ancestor
feature by its call site, not its docs; and its KeePass app decrypted the
`.kdbx` entirely in the browser (`kdbxweb` with `argon2-browser`), so the
server never saw the vault. A Deckjack vault app should keep that split.

The whole repository is AGPL-3.0-or-later; restoring JackLink's tree or a
per-directory MIT license is not wanted.

## pocketdock

A separate, earlier Rust project (a borrowed PC as display and keyboard for a
handheld over USB), not an old name for Deckjack. The tested,
security-sensitive part was harvested: `sys.rs` as is (no pocketdock topology
in it), and `auth/state.rs`, `auth/session.rs`, `auth/code.rs` nearly as is;
`config.rs` gave only its shape. The auth tree imported three things from the
rest of its crate, and the firewall coupling was already behind methods on
`Ctx`, so it lifted cleanly. Its security tests came with it and are the
regression net (`logs_never_contain_secrets`,
`cross_origin_login_refused_and_not_counted`, `ipv6_not_sent_to_nft`,
`http_fallback_cookie_not_secure`).

The one-time code is not legacy: it is the tether-mode sign-in path, and
passkeys were added beside it.

Traps when porting from it:

- Its pages carry its URL space: the login form posted to `/auth/login`
  while Deckjack mounts `/login`. Grep harvested HTML for `/auth/` and
  absolute paths, and check every form action and fetch against the router.
- Its `client_ip` takes one `X-Forwarded-For` and refuses a request without
  it, because Caddy always sat in front; served directly, every request fails.
  Deckjack takes the address from the socket. Its Caddy forward-auth
  endpoint was not carried.
- A `use` grep undercounts a module's dependencies; crates called by inline
  path (`tracing::debug!`, `cookie::Cookie::split_parse`) only surface at
  compile time. Let `cargo check` decide.
- `auth::code::os_random` is the crate's one OS-randomness entry point; reuse
  it (rand 0.10 spells this `rand::rngs::SysRng` with `rand::TryRng`, not
  `OsRng`). A failing random source is fatal for session ids and recovery codes
  but not for `DispatchId`, which falls back to the clock because a duplicate
  only confuses a log reader.

## agentmancer

DESIGN.md cites it about twenty times, but its source was never available to
this project; only a brief existed. Read a citation as a requirement to
implement, not code to port. Its conventions were kept: a behavior correction
goes into the operator prompt or a hook, not one agent's memory; every
behavior change ships with a test that the setting reaches the agent; nothing
private in a public repo. The only real loss was `lumen`, rebuilt as a
multi-target token generator.

## Decisions taken by the operator

- **One node primarily, seams kept** (2026-11-21): "primarily one node for file
  transfer and latency purposes, but let's leave it open." Keep `target_node`
  and `node:path` single-valued, keep `registry.rs` and placement, build no
  multi-node scheduling speculatively.
- **Bus transport between nodes: WebSocket with signed JSON** ("go with choice
  A"). One transport for both legs, pure Rust on `aarch64-linux-android`, and
  it keeps one schema source (Rust types, schemars, JSON Schema, TypeScript).
  QUIC was rejected as a second transport for an unmeasured bottleneck (media
  goes browser-direct over WebRTC, never through the bus); gRPC because
  protobuf is a second schema language. JSON is not canonical, so a signature
  covers the bytes sent, never a re-serialization.
- **Build the sandbox now, fail closed**, and when a manifest asks for what the
  kernel cannot enforce, refuse the launch.
- **Wire every built subsystem before writing new apps** (app host, MCP, Inbox
  and grants, audit log, passkeys), later followed by the app-suite lanes.
- **The workspace is app-shaped**: a new app is a new crate with its own
  backend, embedded UI and `app.toml`, never a module in the core.
- **The crates an app links are permissive.** `deckjack-protocol` and
  `deckjack-app` are MIT OR Apache-2.0 while the workspace is AGPL, so an app
  that links only them is its own work (reasoning at the top of
  `crates/deckjack-protocol/src/lib.rs`).
- **Floating windows by default, tiling as a toggle.**
- **Quick Look is a shell overlay, not an app**: the browser already decodes
  images, audio, video and PDF, and preview is reachable from everywhere. The
  shell's job is judging what it can honestly show.
- **App data is plain files.** Notes are `.md` under `Notes/` with the title
  read from the first heading; tasks are one `VTODO` per `<uid>.ics` under
  `Tasks/`, which is exactly a CalDAV collection's layout. The iCalendar writer
  is a deliberate subset: it folds at 75 octets (not characters) and preserves
  unknown properties, because a task edited on a phone comes back with
  `CATEGORIES`, `LOCATION` and a `VALARM`. Recurrence, time zones and alarm
  semantics are out of scope; needing them is the point to adopt a real
  iCalendar crate.
- The design of record keeps decisions that contradict it as dated deltas in
  the comment at the top of DESIGN.md (pocketdock's `docs/SPEC.md` does the
  same).

## How parallel work goes wrong here

The lane briefs (docs/APP-SUITE.md §12, docs/lanes/) assume several agents work
at once. What has failed before:

- **Halves that compile apart and fail together.** Two agents each invented a
  wire format for one boundary; another run returned SQL migrations in its
  reports instead of in `store.rs`. Freeze contracts (frames, boundary
  signatures, schema) before anyone starts.
- **One owner for the integration files** (`wiring.rs`, `main.rs`, `lib.rs`,
  `command.rs`, `Cargo.toml`). Two agents each adding the line they thought
  was missing produced a duplicated `.with_grants(...)`. An agent that needs an
  edit there makes it in an isolated copy, confirms it builds, and reports the
  exact edit.
- **Investigate read-only, then edit in sequence.** Of eight backlog items
  investigated in parallel, five were not defects, and two of the queued fixes
  would have been regressions (wiring `decide_for`, emitting `TetherTerminal`).
  Ask whether an item is real, not to fix it; "this is Phase 2, not a gap" is a
  valid answer.
- **Aim test-writing at the real bug list.** Hardening went well when each
  agent owned one module and its test file, attacked the kinds of bug the
  project had actually had (path traversal including the case that should
  succeed, lockdown surviving restart, sign-count regression), fixed bugs only
  in its own module and reported the rest, and was told that finding nothing is
  a legitimate result.
- **A new crate needs adding to `workspace.members`** ("current package
  believes it's in a workspace when it's not"); `cargo add` writes versions
  into the crate manifest, which collide with `[workspace.dependencies]`
  ("duplicate key").
