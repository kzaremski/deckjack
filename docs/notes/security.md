# Security invariants, and why they hold

Rules a change must not break, with the reason for each and where it lives.
Most were learned from a defect; the defect is named where it shows the trap.
Code is the ground truth: if a line here disagrees with the source, the
source wins and this note is wrong.

## Risk and grants are two axes

- **Risk decides only whether the operator is asked.** Grants decide whether an
  agent may act at all. They are independent. `terminal.open`, `write` and
  `close` are `RiskLevel::Write`: an agent is kept out of a shell by never
  being granted `terminal.*` (plus step-up, once it is wired), not by a
  confirmation. Marking `terminal.write` destructive once put an "are you
  sure" on every keystroke. The reasoning is written into
  `approvals.rs` (`operator_only` and the comments around it) and the doc
  comment on `terminal.open` in `wiring.rs`, and pinned in
  `tests/terminal_ownership.rs`.
- **Pick a risk level by how often the command fires.** A per-keystroke or
  per-frame command can never be `Destructive`. `system.lockdown` is `Write`
  on purpose: the kill switch is one click.
- **Risk cannot be forged.** `Command::RISK` is an associated const, not a
  field; the wire `Submission` has no risk field and cannot name its issuer;
  the issuer comes from the authenticated connection. If per-call risk is ever
  wanted it must not become a struct field.
- **Out of grant is refused, not queued.** Above-threshold commands inside a
  grant go to the Inbox; commands outside it are refused outright. The wire
  `Outcome` keeps the two apart.

## Grants

- **Evaluate each grant whole.** `approvals::translate_grant` once unioned every
  grant an issuer held into one record, so `terminal.*` on node A plus
  `notes.list` on node B became `terminal.*` on B. `grants::decide_any` was
  always right; the facade merged. The `Grants` trait now returns
  `grants_for(..) -> Vec<command::Grant>` and the MCP `SessionGrant` keeps the
  fan-out. Never collapse grants by unioning fields, in any adapter.
- **A trailing `*` needs at least one more segment.** `files.read.*` does not
  match `files.read` (`Segment::Rest` in `grants.rs`). An agent with zero files
  tools under that grant has a wrong pattern, not a filter bug.
- **"Is this the whole bus?" comes from the id grammar.** Any pattern with no
  literal segment counts as everything (`Pattern::is_everything`), so `*.*` is
  unrestricted. Whole-bus grants are refused at issue unless
  `Policy::allow_unrestricted_grants` is set (`IssueError::Unrestricted`).
- **A grant's holder is the agent session id**, never a display name.
  `None` (a pre-migration-6 row) matches nobody, because inventing an owner
  would claim an approval the operator never gave. A security value that must
  be supplied is a function argument (`Grants::issue(owner, spec)`), not a
  field reachable through `..Default::default()`.
- **`grants::decide_for` is unwired on purpose; do not wire it or delete it.**
  `grants.rs` has no risk ceiling (it is injected by `translate_grant`), so
  routing through `decide_for` would drop the `AboveCeiling` gate silently. It
  is also the only exerciser of `Grants::active_standing`, which several
  regression tests sit behind.
- **`Scale::Unknown` is the honest answer.** The bus has no notion of how many
  items a command touches, so the adapter says unknown and
  `Policy::queue_unknown_scale` (default true) queues it.

## Agents and MCP

- **The pipe is the credential.** An agent's identity is fixed when the MCP
  handler is built for its socket; no protocol field can change it. A test
  passes `"session": "operator"` in `tools/call` arguments and checks
  attribution does not move. Keep it.
- **No HTTP agent route.** An endpoint behind the session cookie was cheaper
  and was refused: that cookie is the operator's credential, the one identity
  the system never refuses. Agents get a per-session Unix socket in a `0700`
  directory instead.
- **Which sessions get a socket comes from `[agents] sessions`, not the grant
  store.** Listing a session grants nothing; deriving sockets from grants would
  make an authorization change a topology change. A listed session with no
  grant sees an empty `tools/list`, which is correct.
- **Sockets live under the install directory** (`run/agents/`, named with an
  8-hex digest of the canonical working directory) because a socket is
  machine-local state that must not sync, and because a path under a deep
  working directory overflowed `sun_path` (108 bytes) the first time: `cannot
  open the agent socket: path must be shorter than SUN_LEN`. See
  `mcpserve.rs`.
- **Only the session id survives a dispatch.** `Issuer::Agent` carries the id;
  a label is client-renameable and is not an identity. Do not thread the label
  through dispatch to make the timeline prettier.
- **App calls are agent calls.** `AppFrame::Invoke` dispatches as the agent
  session `app:<name>` (`agentbridge.rs`), so it is journaled as the app,
  grant-checked, Inbox-routed when destructive, and halted by a lockdown. An app
  that stops responding during a lockdown is working as designed.
- **Transport for a new app-to-core call never ships ahead of its capability**,
  and the capability is inside `Manifest::fingerprint`, so declaring it forces
  re-approval. Without that, single-folder apps could have called
  `terminal.open`.
- **Acceptance check after touching agent plumbing**, run against a live node:
  with `[agents] sessions = ["assistant"]` the log says the agent door is open;
  `deckjack agent assistant` completes `initialize`; with no grant `tools/list`
  is empty and a `tools/call` comes back as a readable refusal (a tool result,
  not a protocol error); issuing a grant asks the operator to confirm; after it
  the call lands in the audit log with `"kind": "agent"`; `terminal__open` is
  refused. If the empty list or the shell refusal changes, treat it as a
  security regression.

## Sessions and cookies

- **No cookie value reaches disk.** The `sessions` table, `AuthState`'s map and
  the terminal registry key on `SessionHash` (SHA-256 of the session id, 64
  lowercase hex, no KDF because the input is already 256 random bits).
  `store.rs` does not import `SessionId`, so nothing can persist one by
  accident. `SessionHash::from_hex` refuses uppercase so one session cannot
  have two spellings. To check: sign in, then grep every file under the
  install and working directories for the cookie value.
- **Freed pages keep their bytes.** Migration 4 dropped the plaintext
  `sessions` table rather than rehashing rows (rehashing would leave cookies in
  free pages), at the cost of one sign-in per device. In WAL mode a plain
  `VACUUM` did not scrub them either (mtime unchanged was the tell); the store
  uses `VACUUM INTO` a sibling file plus `rename`. Verify a scrub by grepping
  the file, not by the statement's exit status.
- **A browser session id never reaches a log row or a frame.**
  `BrowserSession` has a redacting `Debug`, one `reveal()` call site, and
  serializes as a truncated `SessionTag`. Do not derive `Debug` or `Serialize`
  over the raw id.
- **`SessionTag` is a prefix of `SessionHash`.** They once hashed different
  encodings (base64 text vs raw bytes), each side green, and the audit log
  could not be joined to sessions. When two modules derive an id from one
  source, pin their agreement in a test that computes both.
- **Revoke through an `Event` the context applies.** Revoking for
  `max_sessions` or a blocked address once only dropped the in-memory entry; the
  row survived and a restart restored the session with its pty running.
  `AuthState` now emits `Event::SessionRevoked`, and `Ctx::apply` deletes the
  row and runs the reapers.
- **Reapers run when a session ends, never on socket close.** A reload drops
  every connection; killing shells then would defeat reattach. They are a
  `Vec<Reaper>` of closures on `http::Ctx`, so a new resource type registers a
  closure instead of making `http` depend on it.

## Audit log

- `command_log` is insert-only by API (SQLite cannot enforce it). The auth
  context and the bus journal share one `Arc<Mutex<Store>>`: a second
  connection would be a second writer on the WAL. A failed journal write is
  logged and swallowed; refusing to run because the disk is full is worse.
- **Never append audit rows while holding the store lock.** `append_dispatch`
  takes the same mutex and hangs with no panic. Collect, drop the guard, then
  append.
- **Every path that ends a session writes `auth.session.revoked`**, one row per
  session, whether it was sign-out, revocation or lockdown (`AuthSessions` in
  `wiring.rs`, `Ctx::record_revocation` in `http.rs`).
- **Redaction defaults to redacting.** A refusal for an unregistered command
  records no params, because no spec means no `REDACT_PARAMS` flag
  (`Bus::record_refusal`); otherwise `terminal.write` keystrokes arriving just
  after the terminal app unregistered would be logged. `terminal.write` sets
  `REDACT_PARAMS` because keystrokes include passwords.
- **Parameter names that look secret lose their value**: any name containing
  `password`, `passphrase`, `secret`, `token`, `code`, `key`, `cookie`,
  `credential`, `authorization` or `session` (`SECRET_HINTS` in `events.rs`).
  A row that must carry a session identifier puts it in a field named
  otherwise; revocation uses `tag`.
- **One dispatch is several `events` rows**, told apart by `phase` (for
  example `ran` and `completed`). Count dispatches by id, not rows.

## Sign-in code and lockout

- The code is 8 Crockford base32 characters shown `XXXX-XXXX`; input is
  normalized (case, spaces, dashes, O to 0, I/L to 1) and compared in constant
  time (`auth/code.rs`). `Code` has no `Display` and a redacting `Debug`; the
  `logs_never_contain_secrets` test enforces it.
- It is single use: the first login POST that presents it spends it, right or
  wrong. Limits default to 3 wrong codes per address and 10 in total
  (`auth/state.rs`). The block set is memory-only and a restart clears it.
- `restore_session` is a separate path from login: it spends no code, counts
  no attempt and revokes nothing.
- **`sessions.kind` defaults to `'tether'` on purpose** (migration 7): every
  session alive at migration time was proved only by the one-time code, and
  reading it as the stronger kind would grant what it never earned.
- **Do not emit `Observation::TetherTerminal`.** Its policy is `Engage`, and
  the only signal is `sessions.kind`, which is `'tether'` for every code
  sign-in, so the first shell opened would lock the node down. It needs a
  session kind that really distinguishes a tether first.

## Passkeys

- **A credential is bound to its `rp_id` forever.** Set `[passkey]` before the
  first enrollment; changing it orphans every passkey. With no config on a
  non-loopback listener, `passkeyhttp::settings_for` returns `None` and logs
  "passkeys are off: this node is not served on loopback and [passkey] is
  empty, ..." instead of guessing. A half-set section, or an IP as `rp_id`, is
  refused.
- **Enrollment.** With no passkeys the first one needs no approval code
  (`Enrollment::First`); after that a signed-in session issues a code at
  `/passkey/enroll/approve`. Any session qualifies, including one made with the
  one-time code, which is the recovery path after losing every device.
- **One approval enrolls one credential.** A new registration replaces any
  pending one; previously a code checked at start and spent at finish allowed
  several registrations in its window. A ticket validated at start and consumed
  at finish is not single use unless in-flight uses are bounded.
- **Nicknames refuse control characters**: the nickname is quoted into audit
  lines and errors, and a newline would forge a log line.
- **webauthn-rs checks the sign count itself** and returns
  `WebauthnError::CredentialPossibleCompromise` before Deckjack's own check
  runs. `passkey.rs` translates it back (`diagnose`) into
  `Event::CloneSuspected`, which engages lockdown. Nothing in the workspace can
  produce an ES256 signature, so this path is not tested end to end.
- **There is no CLI enrollment, by design.** A passkey is only ever approved by
  a session that was already trusted; a CLI path would let shell access mint
  one. If an error names `deckjack passkey ...`, fix the text.
- **Never gate an escape hatch on a ceremony the user cannot reach.** A step-up
  gate on `terminal.open` with no step-up UI would have bricked the browser
  terminal on any node with a passkey. The ceremony now exists
  (`start_step_up`, `finish_step_up`, `/passkey/step-up/begin`) but the gate is
  still not consulted; see docs/OPEN-THREADS.md.

## Lockdown

- **The gate is a layer, and axum layers only wrap routes already added.** Any
  router merged after `.layer()` is ungated, so every route is merged inside
  `wiring::app` before the gate is applied. Passkey routes merged at the call
  site would have kept sign-in working through a lockdown. Verified: while
  locked, `/passkey/*` and `/login` answer 503 and `/unlock` serves the unlock
  page.
- **Engage was fired for real** from a signed-in session: sessions revoked, key
  rotated, every route (existing or not) 503, the `sessions` table emptied,
  shells killed, `lockdown.json` recording the trigger. Unlock with a recovery
  code restored service; the revoked cookie still went to `/login`; a spent
  code and a wrong code were refused.
- **"From loopback" is not host presence**: behind `tailscale serve` every
  request is local. Host authority is two factors: `.deckjack/unlock.token`
  (mode 0600, `lockdown::HOST_TOKEN_PATH`) plus a recovery code. The token is
  checked before any code is spent, so code guessing is not reachable over the
  network. `deckjack unlock --recovery-code CODE` reads the token itself.
- **Recovery codes are shown once.** A node with none issues ten at startup and
  stores only digests. Anything that captures startup output must write it to
  a file only the operator can read.
- **A global gate is re-checked at release, not only at enqueue.**
  `dispatch_approved` once skipped the agent hold, so a destructive item queued
  before lockdown ran after it. The hold is read from lockdown state (the
  `Halt` port in `command.rs`) rather than pushed in, so unlocking needs no
  resume call to remember.
- **Freeze gates approval, not queueing.** `Inbox::queue` has no frozen check;
  `Inbox::approve` refuses while frozen. Both fail closed.
- **New-device detection needs `known_addresses`**, or the first sign-in on a
  fresh node trips it. Refused agent commands are counted through
  `WatchJournal`, which wraps the journal every decision already passes
  through.

## HTTP surface

- **The router ends in a catch-all shell fallback.** An unmounted path, or an
  unmatched POST, is answered (a 303 to `/login` without a session), never a
  404 or 405. Status-only route tests cannot fail; see docs/notes/testing.md.
- **SPA fallback is extension-gated.** Only extension-less paths fall through to
  `index.html`; a missing `.js` is a logged 404, because HTML served as a
  script shows up as a syntax error at `<`. Unmatched routes log method and
  path and say so in the body; a silent empty 404 renders as a blank page.
- **Pre-auth install assets are a closed allowlist**: the manifest, `/icons/`
  and `/splash/`, with `..` refused (`http::is_public_install_asset`). iOS
  fetches them with no session, and behind the gate Add to Home Screen silently
  installs a screenshot. rust-embed resolves `..`, so `GET
  /icons/../index.html` once served the shell to anyone. Prove such a guard
  with a no-session request through the composed router; a unit test of the
  predicate proves nothing about reachability. After touching the gate: those
  paths 200 with no cookie; `/`, `/index.html`, `/sw.js`, `/kit/bus.js` 303.
- **`/login` has no `script-src` at all** (`auth/html.rs`). A link to the
  passkey page needs no CSP allowance; the WebAuthn ceremony lives on its own
  page with `script-src 'self'`, served as an external `/passkey/ceremony.js`
  rather than inline, so interpolating a value into that HTML can never become
  script. The enroll page pairs a secret field with `script-src 'self'`, which
  `passkeypage.rs` argues out loud: an enrollment code is single use, short
  lived and worthless without a signed-in session.
- **Auth-critical pages are served from the binary** (`passkeypage.rs`), so
  login, unlock and enrollment work when the shell is broken or unbuilt. Their
  scripts use `getElementById` (a `"#` ends the Rust raw string) and write node
  values with `textContent`, never `innerHTML`.
- **Previewing untrusted bytes needs both halves**: `/content` sends
  `Content-Security-Policy: sandbox` with no tokens, `nosniff`,
  `Cache-Control: private` and `Referrer-Policy: no-referrer`, and the shell
  renders HTML in an iframe with neither `allow-scripts` nor
  `allow-same-origin`. Either alone leaves a hole. A bad range is a 416 naming
  the real length, never the whole file.
- **One `files::Namespace`.** The content route resolves through the same
  namespace the bus uses; a second root for HTTP is where containment rules
  drift (JackLink's root was decorative).
- **Hidden-state matching is case-insensitive.** `/.DeckJack/state.db` once
  reached the database on a case-folding filesystem: `realpath(3)` resolves
  symlinks but does not correct case. Any denylist comparison on a path
  component must be case-insensitive.
- **The client address comes from the socket**, and forwarded headers are
  believed only on the proxy's Unix socket (README, "Behind `tailscale
  serve`"). With one proxy in front only the rightmost `X-Forwarded-For` entry
  is trustworthy. Trusting loopback TCP was tried and abandoned: an app granted
  the network could claim any address and HTTPS.

## Terminals

- **Every terminal op checks ownership** through `attach_owned`, and listing is
  `list_for` the caller. Before, any signed-in session could list and type into
  another's shell. A handler that takes `_cx` and drops it is the signature of
  this class.
- **A terminal you do not own does not exist** as far as the error says; "not
  yours" would be an oracle for live ids.
- **Output rides the request path with a cursor**, not progress frames.
  `ProgressSink` drops frames when the queue is full (by design: a slow progress
  reader must not stall the bus); one dropped PTY chunk corrupts the stream.
  When scrollback overflowed, `terminal::Output::skipped` is non-zero and the
  client is told to reset its emulator (`reset` on `wiring::Chunk`).
- `terminal::Limits` caps sessions (16 by default): each costs a pty, two
  threads and scrollback, and a client opening terminals in a loop should hit a
  wall.
