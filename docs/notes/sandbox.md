# App sandbox and app host: facts that cost time

docs/APP-SUITE.md §2 and Lane 0 describe what the sandbox grants and how
network ports work, and docs/lanes/10-sandbox-all-apps.md the Unix-socket and
signal scopes. This note keeps the kernel and crate behavior behind them, the
accepted gaps, and the app-host traps, so nobody rediscovers them.

## Fail closed

- **A node that cannot enforce a manifest refuses to launch the app**; it never
  runs it less confined (`sandbox.rs`, module doc). Decided before the sandbox
  existed, against "defer it" and "ship the interface only": a declared
  boundary nobody enforces is worse than none, because callers trust it.
- `CompatLevel::HardRequirement` makes a missing kernel feature an error. The
  `landlock` crate (0.4.7) has no runtime ABI detection by design
  (`LandlockStatus::current()` is private), so the sandbox probes by building
  the real ruleset and reading `RulesetStatus`. Read the crate's own source
  under `~/.cargo/registry/src/` after `cargo fetch` rather than guessing its
  API.

## Landlock behavior

- **Landlock is unprivileged.** It needs no user namespaces and no root, and
  neither does seccomp. DESIGN.md's "Termux has no namespaces without root, so
  on the phone the tools-only layer carries the weight" carries over
  bubblewrap's reasoning; for Landlock the only question on a phone is whether
  its kernel was built with `CONFIG_SECURITY_LANDLOCK=y`, checked on the
  device (`/sys/kernel/security/lsm`).
- **It governs opening a path, not learning that it exists.** Probed from a real
  confined app (ABI 4): opening `~/.deckjack/state.db`, listing or writing the
  working directory, and a TCP connect out were refused, but `stat` on
  `state.db` succeeded, so an app learns that the file exists, its size and its
  mtime. "No app ever sees `.deckjack/`" means no app can open it. Never encode
  a secret in a filename or in whether a file exists. If existence must be
  hidden, Landlock is the wrong layer; seccomp here is a denylist of escape
  syscalls, not a metadata filter.
- **A directory-only right on a non-directory fails the whole ruleset**, not
  that one rule: `incompatible directory-only access-rights: ReadDir`. Device
  nodes and single files therefore go through `Policy::read_files` /
  `read_write_files` with file rights only. The symptom before the fix was
  `gpu = true` that prompted for approval and granted nothing.
- **A path in a rule that does not exist also takes the ruleset down**, so the
  app's run, home and tmp directories are created before launch. An optional
  device that may be absent (`/dev/kfd` only exists with an AMD GPU and its
  driver) is skipped rather than named.
- `AccessFs::from_read` includes `Execute`, and children inherit both Landlock
  and seccomp, so an app may run a helper binary and the helper is confined
  too (APP-SUITE.md §2).

## Accepted gaps

- **`/dev/shm` is shared.** It is granted read-write to every app because
  POSIX shared memory is how ordinary libraries allocate big buffers. It is one
  tmpfs for every process of the user, so two apps can see each other's
  segments. Landlock cannot draw a line inside one mount; closing it means
  mount namespaces, which this launcher does without on purpose.
- **Network is per port, not per host** (APP-SUITE.md Lane 0.6). An empty
  `network` is a real denial (connect fails with `EPERM`/`EACCES`); naming
  hosts without ports opens the network whole and is warned on every launch.
  Per-host enforcement needs the allowlisting proxy, which is not started.

## Invariants in the app host

- **`apps::Confinement` must never get a `Deserialize`, `From<bool>` or
  `FromStr` impl.** Their absence is what stops a manifest or config from
  turning its own sandbox off; the one unconfined path,
  `Confinement::unconfined_for_development`, is reachable only from code and
  warns on every launch. If it ever needs serializing, Serialize only.
- **The environment is cleared before exec**, so each app is handed its own
  `HOME` and `TMPDIR` under the install directory's cache. Without them a
  library falls back to `/` or `/tmp`, writes where nothing was granted, and
  the failure looks like an app bug.
- **A namespace is not owned by one registrant.** The Files app is called
  `files`, and tearing it down with `remove_namespace("files")` once deleted the
  core's whole `files.*` API after the app disconnected. App teardown removes
  exactly the ids that app registered (`Registry::remove_all`); never tear
  down by name prefix.
- **The Files app registers no commands and has an empty `[needs]`.** The file
  API belongs to the core because one implementation of that boundary is the
  right number, so Files is the least privileged process in the suite and
  launches without an approval.
- **`apps.rs` holds decisions, `apphost.rs` the runtime.** Discovery, launch
  planning, handshake rules, backoff and `[needs]` to policy sit behind traits
  in `apps.rs` so they test without spawning; the per-app attendant task, the
  frame pump and the bus bridge are in `apphost.rs`. Put new code in the right
  one.
- **Approvals live in `state.db`; the installed-app inventory never does.**
  Binaries belong to the install directory, which does not travel; an approval
  is a decision, and decisions travel with the working directory. The approval
  is keyed by `Manifest::fingerprint`, so a manifest asking for more no longer
  matches.
- **Discovery needs `apps/<name>/deckjack-<name>` present and executable.**
  `MissingBinary` and `NotExecutable` are separate problems, so a `chmod`
  mistake reports differently from a wrong path. "`<name>` is installed but
  cannot run on this node" means the approval gate fired, not a broken build.
- **Judge app isolation through the app's own commands.** With Notes granted
  only `Notes/`, `notes.read` on `../Tasks/x.ics` and `../../etc/passwd` was
  refused and `notes.list` saw no Tasks files. The core's `files.list` still
  sees everything because those are the core's commands, not the app's.
- **Reinstall with `deckjack install`, never `cp` over the binary.** Copying
  onto a running app's executable fails with `Text file busy`; the installer
  swaps the directory atomically and the old process runs on its deleted inode.
- **Measure release builds.** An app is about 2.5 to 2.8 MB in release and
  around 38 MB in debug.
