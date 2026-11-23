# Running, restarting and debugging a node

The README covers building, installing and `tailscale serve`. This note covers
what happens when a node is started, stopped, restarted or probed, and the
traps in each.

## Install directory and working directory

- The install directory is `$DECKJACK_HOME`, else `~/.deckjack`; `--dir` sets
  only the working directory (lock, config, `state.db`). A probe started with
  `--dir` alone still writes node state into the real install directory.
- **For a throwaway node**, export `DECKJACK_HOME` to a scratch directory,
  point `--dir` at another, and listen on a spare loopback port. Read that
  node's own sign-in code from its output. `deckjack agent SESSION` resolves
  the install directory by itself, so it needs the same `DECKJACK_HOME`; its
  error names the socket path it tried. Both sides print the same 8-hex
  working-directory tag when they agree on `--dir`.
- **Never copy the install directory to another machine**: it holds binaries
  built for this one and the node's identity, and a copy clones the node. Back
  up and sync the working directory only.
- **Node identity is random**, generated once into `keys/node-id`, and
  deliberately not derived from `/etc/machine-id`: a hardware id would follow a
  synced directory, and a node restored onto new hardware should keep its
  identity. It is hex because the earlier URL-safe base64 started with `-`
  about one install in 64, which `files::NodeName::parse` refuses as a flag,
  so the node would not start. An identifier that may be interpolated into a
  command line, flag or path must start alphanumeric.

## The sign-in code

- It is printed once at startup and exists nowhere else, not even on disk.
  There is no command to read or rotate it on a running node
  (`AuthState::rotate_code` has no caller), so losing that output means a
  restart, which mints a new one. Build first, restart once, then hand over the
  new code.
- It is single use, and a wrong code spends an attempt from a small budget
  (docs/notes/security.md). Never POST `/login` to probe a node someone is
  using; probe `GET /` and `GET /login`, or use a throwaway node, or the
  `--debug` cookie file (README).
- Over `tailscale serve` to a TCP port, every tailnet client arrives from
  loopback, so one device's wrong codes count against everyone. The Unix-socket
  setup in the README is the fix.

## What a restart does

- Rotates the sign-in code and clears the in-memory address block list.
- **Kills every terminal.** A pty master is a descriptor in the server process;
  reattach survives a page reload, not a restart.
- **Does not sign anyone out.** Sessions are restored from `state.db`
  (`sessions restored count=N` in the log). The cookie was renamed from
  pocketdock's `pd_session` to `dj_session`, so a pocketdock cookie never
  restores.
- After a stop that skipped shutdown (`kill -9`), the next start logs
  `previous server did not shut down cleanly; recovery is not implemented yet`.
  That is expected and is not evidence of a damaged database.

## Stopping cleanly

- `shutdown_signal()` in `main.rs` waits on SIGINT **and** SIGTERM. Awaiting
  only `tokio::signal::ctrl_c()` once meant every `kill` or systemd stop skipped
  shutdown: the WAL stayed beside `state.db` and the lock claimed a clean
  directory. Testing shutdown with Ctrl-C proves nothing about a service stop.
- Shutdown checkpoints the WAL (`wal_checkpoint(TRUNCATE)`) and only then
  releases the lock, because the released lock is the promise that the
  directory is safe to copy.
- **Do not `pkill -f deckjack`.** The pattern matches the invoking shell's own
  command line and kills it, and a pattern built from an empty shell variable
  widens to whatever node is running. Take the pid from the listener (`ss
  -ltnp`), `kill` it, confirm the port is free, then start the new one; a
  half-dead instance holding the port makes the new one exit.

## Recovery codes and unlock

- A node with no recovery codes issues ten at startup and keeps only their
  digests, so that output is the only time they appear, and they are the only
  way to end a lockdown. If startup output is redirected to a file, make it
  mode 0600 (a log in the install directory is readable by anyone with a shell
  there otherwise).
- `deckjack unlock --recovery-code CODE [--dir PATH]` reads the host token from
  `PATH/.deckjack/unlock.token`; a bare `deckjack unlock` is refused.
- While locked, apps cannot call the bus (they dispatch as agent sessions), so
  every bus-backed app looks broken until unlock.

## Web changes and rebuilds

- **A changed `web/dist` needs a Rust rebuild.** rust-embed's mode is one switch
  for the whole build, and `debug-embed` is on because app binaries run under
  Landlock and cannot read their UI from the source tree (a debug app once
  answered 404 on every asset with nothing in the log). So debug builds embed
  the shell too, and a restart alone serves the old UI. `cargo xtask build`
  builds the web bundle and then Rust; `make dev` runs Vite for shell work.
- **npm is optional** (`xtask`): a fresh bundle skips it, a stale bundle with no
  npm warns and still builds (the Termux case), and only no bundle and no npm
  is an error.
- Hashed assets are cached for a year and `index.html` is `no-store`. Reversing
  that leaves a browser holding an old document that asks for assets that no
  longer exist.
- The web shell is compiled into the core, so the Vite build stays a plain
  static bundle with `base: "/"`: no SSR, no runtime manifest, nothing that
  needs Node at runtime.

## When the log is silent

- The tracing filter must name the binary target as well as the library
  (`deckjack=info,deckjack_core=info` in `main.rs`); with only `deckjack_core`
  the startup line never appeared. Output redirected to a file is
  block-buffered and may simply not be there yet.

## "It shows a blank page"

Check, in order:

1. Is the process alive and listening (`ss -ltnp`)? Most blank pages were a
   node that was down, often mid-rebuild.
2. Is the request going where the router expects? A form ported from pocketdock
   posted to `/auth/login`; axum answered with an empty 404, which a browser
   shows as white, the handler never ran and the code was never spent. Unmatched
   routes now log and say so in the body.
3. Not the causes, already checked: the post-login 303 sends a relative
   `location: /login`, not an absolute loopback URL; the service worker is
   network-first for navigations and never caches `/login`.

## Devices and passkeys

- Set `[passkey]` to the node's tailnet name before the first enrollment, and
  enroll and sign in from a page loaded over that name: a browser refuses a
  ceremony whose `rp_id` is not a suffix of the page's origin, and a credential
  cannot be moved.
- An installed iOS home-screen app has its own cookie jar, separate from
  Safari's, so it needs its own sign-in; have a current code at hand.
- A sign-in from an unseen device only writes `trigger=NewDevice` to the log:
  the policy is `alert` and there is no notification channel yet.
