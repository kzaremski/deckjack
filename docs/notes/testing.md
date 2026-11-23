# Testing: the gate, the traps, and the defect classes

docs/APP-SUITE.md §12 states the short version: tests first, behavior not
implementation, mutation-check every test, and design against the
built-but-never-constructed module. This note is the long version, with the
specific ways tests in this repo have passed while proving nothing.

## The gate

- `make check` runs `cargo fmt --check`, `cargo clippy --all-targets -- -D
  warnings`, `vue-tsc` and every Rust and web test. The three Rust gates came
  from pocketdock unchanged. Run the full suite after touching anything
  cross-cutting (the auth sessions, the ws protocol, the HTTP gate, the bus,
  the window store), not at the end of the day.
- **A test is accepted when a one-line change that breaks a real promise turns
  it red**, not when it is green. For each test: apply the mutation, watch the
  new test fail, revert, watch it pass. Require that fail-then-pass evidence
  for any test claimed to close a mutation, including from a helper or
  subagent; otherwise tests that pass for unrelated reasons get reported as
  kills. The first full pass found 69 survivors and five product defects
  (commits `45fd970`, `6fcc624`).
- **Some survivors are left on purpose, with the reason written down**: the
  `Admitted::Queued` arm of `BusDispatch::dispatch` (a socket invoke always
  carries a browser issuer, and letting the ws path carry an agent identity to
  reach it would be a regression); the audit-append order in
  `AuthSessions::revoke_all` (observing it needs a failing store write, and a
  production port added only for a test was declined). An unkilled mutation
  with a reason beats a test that pins implementation or a seam nobody asked
  for.

## Defect classes this codebase keeps producing

- **Built, unit-tested, never constructed.** A module written against injected
  traits stays green forever because the tests build what production never
  builds. Lockdown, the app host, grants, passkeys, events, MCP and the sandbox
  all sat like this once. Rust will not warn: `pub` items in a library crate
  never trip `dead_code`, and an unread struct field is not an error (deleting
  `.with_halt(parts.halt)` from `wiring::bus` still compiled). Answers, in
  order: required fields on an assembly struct (`wiring::BusParts`); assembly
  in a function a test can call (`wiring::app`, `wiring::bus`) instead of in
  `main`; a live request against a running node. Before calling a feature done,
  find where the real startup path constructs it. Do not `#[cfg]` or
  `#[allow(dead_code)]` such a module to quiet it.
- **Advertised with nothing behind it.** `/healthz` sat in the lockdown
  allow-list with no handler, and `tests/lockdown.rs` mounted its own stand-in,
  so the test proved the gate and never the route. The panic chord was bound to
  `shell.lockdown`, which nothing registers (the real id is `system.lockdown`;
  the button worked, so nobody noticed). Every id in a keybinding table should
  be asserted to resolve, and a safety control needs a test on each route to
  it.
- **A test standing in for the seam it should exercise.** A fetch test asserted
  the enroll page "calls `/passkey/credentials`" while the code POSTed to a
  GET route. Pin the method as well as the URL, and pair a mocked unit test
  with one that drives the real router.
- **A comment asserting what the code does not do.** `main.rs` discarded the
  `files` handle directly under a comment saying it was "named rather than
  discarded"; `snap()` and `toggleMaximize()` lacked the tiling guard that
  `place()`'s comment said "holds when something else calls in". When a comment
  and the code disagree, the code is the fact; fix the comment or the code.
- **An optimistic-success stub that outlived its reason.** `bus.ts` answered
  `ok` for every `shell.*` id "because the server side does not exist yet",
  long after it did. When wiring a backend for real, grep for stubs keyed on id
  prefixes.
- **A safeguard checked at the wrong time.** A global gate must be re-read when
  a queued item is released, not only when it is queued (docs/notes/security.md,
  Lockdown).
- **Stale annotations.** An `#[ignore]` reason and a TODO both described code
  that had since changed. Re-verify ignore reasons and TODOs before trusting
  them.

## HTTP route tests

- **The router's catch-all fallback answers everything**, so `assert_ne!(status,
  404)` cannot fail, and neither can a 405 check on a POST route (an unmatched
  POST is a 303). Measure the real response before writing an assertion; do not
  predict status codes from HTTP convention.
- **Assert a mark only that handler can produce.** The first `/content` row
  used "there is no file at that address", which the files layer produces
  before the content handler runs; it was rewritten around "that is a
  directory". A handler's opinion is a good mark: `POST
  /passkey/sign-in/begin` on a fresh node answers 409 "no passkey is enrolled,
  ...", which an unmounted route cannot say.
- **Calibrate against a control.**
  `every_route_the_wiring_mounts_is_answered_by_its_own_handler`
  (`tests/wiring.rs`) first measures what the build answers for
  `/wiring-mounts-nothing-here` with the same method and session, and fails a
  row whose assertion the control already satisfies. A new route adds a row.
- **Pre-auth guards are proven through the composed router**, with a request
  and no session; see the install-asset traversal in docs/notes/security.md.

## The end-to-end harness

`tests/end_to_end.rs` builds the objects `main.rs` builds, in the same order,
through the same `wiring::app`, binds a real listener and talks to it like a
browser, with a hand-written HTTP and WebSocket client (no `tower` or
`tokio-tungstenite` was added for tests). It catches what module tests cannot:
two halves of a protocol that never agreed, a wrong risk level, a form posting
to an unrouted path. Keep it that way. It once held two copies of the
production assembly, so a change to one left the other proving the old shape;
both now go through `wiring::bus`. Check for duplicated assembly before
trusting any harness that claims to mirror production.

## Writing assertions

- **Time-based tests:** advance to just before the deadline and assert nothing
  happened, then past it and assert exactly one event. A retry test that jumped
  far past the deadline passed for any backoff. A "dead after N seconds" check
  that runs only on a poll tick has a worst case of N plus the tick.
- **A regression test carries its premise.** The grant cross-product test
  builds the old unioned grant and asserts it would have authorized the
  command, then asserts the fix denies it. A year later it still shows what it
  defends against.
- **Mutate a security fix away before calling it verified.** Reverting the
  terminal ownership check made four of five new tests fail with a stranger
  session reading real shell output; the fifth correctly did not depend on
  ownership.
- **Dates:** pin the pair hand-rolled converters get wrong, a leap day
  (2024-02-29 = day 19782) and a century leap year (2000-02-29 = day 11016),
  computed outside the code under test.

## Web tests

- **The harness builds the real stores** (`web/src/testing/shell-harness.ts`)
  and fakes only the edges: socket, clock, storage, `matchMedia`. A stub agrees
  with whatever the component asks, which is how the `shell.*` stub survived.
  Fake a new dependency at the edge; do not swap a store for a stub.
- **Components take no props**; they call `useShell()`, which throws "useShell()
  outside the Deckjack shell" without the harness.
- **Guard that a component is mounted at all**: shallow-mount `App` with the
  harness's shell and assert it is in the tree, then delete it from `App.vue`
  and watch exactly that test fail. Once per new panel.
- **Geometry is asserted as properties**: no overlap, nothing off-screen,
  nothing below the minimum size, most of the area covered. Exact rectangles
  freeze one arrangement and still pass a wrong one.
- **Assert what a peer would see**, and nothing private. Frames on the wire are
  the exception, because they are the protocol.
- **Tooling:** the web suite is on `vitest` 3 with `vite` 6.0.11. vitest 5
  refuses to install (`ERESOLVE ... peer vite@"^6.4.0 || ^7.0.0 || ^8.0.0"`), and
  upgrading Vite only for a test runner was judged not worth it, so do not bump
  either without a separate reason. To see a version's peers, ask for the whole
  object (`npm view vitest@<ver> peerDependencies`). Component tests need only
  `@vue/test-utils` and `happy-dom` plus a `// @vitest-environment happy-dom`
  docblock per file; no vitest config change.

## Store migrations

`a_version_two_database_migrates_forward_with_its_rows_intact`
(`tests/store.rs`) builds a real v2 database by reversing every later
migration, so each new migration must be reversed there too (adding 5 to 7
failed with `duplicate column name: dispatch` until it was).
`the_migrations_that_turn_the_unused_tables_on_are_actually_applied` exists
because a migration that silently never runs leaves `PRAGMA user_version`
lying. Expect to touch both with every schema bump. New SQL belongs next to its
subsystem through `Store::with_conn`, which never hands the connection out.

## Running tests

- **`cargo test` filters are substrings, not regexes.** `foo\|bar` matches
  nothing and reports `0 passed`, which looks like success.
- **To find a hanging test**, run it bounded and serialized so the last name
  printed is the culprit: `timeout 100 cargo test --test NAME --
  --test-threads=1`. Output piped through `head` or `tail`, or a backgrounded
  run, shows nothing until it ends.
- **Give every worktree its own `CARGO_TARGET_DIR`.** The final binary lands at
  `$CARGO_TARGET_DIR/debug/<name>`, not keyed by package or worktree, so a
  build from an older worktree once overwrote the binary the main tree was
  serving. Parallel builds sharing one target dir also serialize on "Blocking
  waiting for file lock on build directory". To tell which binary a process
  runs, compare `md5sum /proc/<pid>/exe` with the build output.
- **`Running::split` spawns nothing.** The command body runs inside the
  returned completion future, so awaiting the progress receiver first
  deadlocks. Poll both (`select!`, or spawn the completion), as
  `wiring::BusDispatch` does.
