# Open threads

Implementation work known to be unfinished, each with where it stands. Checked
against the source on 2026-11-23; re-check before relying on a line here.
Unanswered design questions stay in DESIGN.md's "Open questions", and each
lane's own gaps stay in its brief (docs/APP-SUITE.md, docs/lanes/).

## Security

- **Nothing signs anything.** There is no signing primitive in the tree;
  `TODO(session-signing)` stands in `lockdown.rs` and `wiring.rs` (session
  cookies), and DESIGN.md's signed node-to-node commands have nothing under
  them. Treat signing as new work when planning Phase 2, not a wiring detail.
- **Step-up is built and not enforced.** `start_step_up`, `finish_step_up` and
  `has_step_up` exist and the routes are served, but `terminal.open` never asks
  (doc comment on `terminal.open` in `wiring.rs`), so a stolen cookie still
  opens a shell. Undecided with it: when no ceremony can be demanded (no
  `[passkey]` on a tailnet node, nothing enrolled on loopback), fail open with
  a loud warning or closed. The recommendation was open, because closed bricks
  the terminal, the documented escape hatch, on two normal configurations.
- **The operator has no agent pause lever.** `Bus::pause_agents` and
  `resume_agents` have no caller outside tests and no command reaches them.
  Lockdown halts agents through the `Halt` port, which is what the kill switch
  needs.
- **Lockdown does not revoke grants.** `Approvals::revoke_all` exists, but
  `lockdown::Deps` has no slot for grant storage. Lockdown halts agents and
  freezes the Inbox, so it fails safe; grants survive an unlock.
- **A suspected passkey clone engages lockdown as `Trigger::HostCommand`**
  (`main.rs`), because there is no trigger kind for it, so the lockdown record
  names the wrong cause.
- **New-device sign-ins only reach the log** (`trigger=NewDevice`, policy
  `alert`). There is no notification channel (push, ntfy, mail).
- **`passkey.require_user_verification = false` is inert**; it now says so at
  startup rather than relaxing anything.
- **A command admitted just before a lockdown engages still completes**: the
  hold is checked at admission and at Inbox release, not mid-run. Inferred
  from the design of the halt; not re-tested.
- **`app:` session names are a convention.** Apps dispatch as agent sessions
  `app:<name>`; `[agents] sessions` cannot collide with them (a `:` is refused
  by `check_session_name`), but `AgentSessionId` itself validates nothing.
- **Tether mode is not built**, so `Observation::TetherTerminal` stays
  unproduced; it needs a session kind that tells a tether from a code sign-in
  before it can be wired (docs/notes/security.md).

## Operations

- **No `deckjack code`.** The sign-in code cannot be read or rotated on a running
  node; `AuthState::rotate_code` has no caller and its doc comment names a
  `code --new` command that does not exist.
- **Crash recovery is not implemented.** After an unclean stop the node says so
  and carries on.
- **`/healthz` was mounted ahead of the design** (DESIGN.md schedules it with
  the multi-node and Caddy work) because an unauthenticated allow-list entry
  with no handler was worse. Awaiting the operator: keep it, or remove the
  mount and the `ALLOWED_WHILE_LOCKED` entry together, never the entry alone.
- **Stale comment:** `apphost.rs`'s module doc says app commands are removed
  through `Registry::remove_namespace`; the code uses `Registry::remove_all`,
  which is the point (docs/notes/sandbox.md).
- **`deckjack-office` crashed repeatedly on the development machine** (ten
  segfaults in coredumps on 2026-11-22). Observed in passing, not
  investigated.

## Phase 2 and the app suite

- **Multi-node foundations are absent**: no node daemon (`node.rs` is identity
  only), capability probes return empty, no `aarch64-linux-android` target
  (APP-SUITE.md Lane 0.5), no git adapter. `registry.rs` is complete and
  unwired on purpose; `Store` does not implement `NodeStore` and no SQL uses
  the `nodes` table. Close the Phase 1 security threads above before the
  attack surface becomes several machines.
- **DESIGN.md's Phase 2 and Phase 4 exit checks assume two nodes**, which the
  one-node-primarily decision (docs/notes/history.md) makes the exception;
  they need rewording.
- **Mail: JMAP or IMAP.** The operator's current provider offers no JMAP, so
  the plan is IMAP with OAuth unless the provider changes. Ask whether a
  provider move is planned before starting the mail app; the answer decides
  the backend.
- **Which calendar server the calendar app targets** is not settled.
- **Long transcodes may shell out to `ffmpeg`**: written into APP-SUITE.md §3 as
  a flagged exception to "libraries in-process" and never confirmed by the
  operator. Confirm before building on it.
- **Per-host network allowlisting** needs the userspace proxy DESIGN.md
  describes; Landlock enforces ports only (APP-SUITE.md Lane 0.6).
