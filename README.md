# Deckjack

One browser-based workspace, served by machines I own, that presents as a
desktop. Every task has a native web UI; agents are an optional layer acting
through the same commands I do.

Three rules define it:

- **Every interface is real web UI.** DOM, canvas or WebGPU rendered in the
  browser. No VNC, RDP or pixel-streamed desktops. The one exception is a
  native-only Linux app, streamed window by window and marked as streamed
  (docs/DESIGN.md, "streamed native apps").
- **Backends are Rust services and CLI tools.** The browser is the only front
  end; the heavy lifting runs on whichever node fits.
- **One command bus.** Every action is a typed command. I, the CLI and agents
  all call it; an agent's risky commands wait for approval.

The design is in [docs/DESIGN.md](docs/DESIGN.md).

## Status

Early. The workspace builds and the harvested modules are tested; nothing
serves anything yet.

| Piece | State |
|---|---|
| Sign-in: one-time code, lockout, sessions | harvested from pocketdock, tested |
| System and privilege wrapper | harvested from pocketdock, tested |
| Node identity | written, tested |
| Lock file and single-server rule | written, tested |
| Config | not started |
| SQLite state | not started |
| Command bus | not started |
| Web shell | not started |
| Passkey sign-in | not started |

`cargo test` is 50 tests; `cargo clippy --all-targets -- -D warnings` and
`cargo fmt --check` are clean.

## Layout

```
crates/deckjack-core/     server, command bus, window manager
docs/DESIGN.md            the design of record
```

Apps will install as separate `deckjack-<name>` binaries, each carrying its own
backend, embedded web UI and manifest.

## Build

```sh
make build      # web bundle if stale, then cargo build
make check      # fmt, clippy, vue-tsc, then every Rust and web test
make install    # release build into ~/.deckjack (PREFIX=... to change)
make run        # the installed server, in the foreground
make dev        # dev core (--debug, :8777, ~/.deckjack-dev) + Vite on :5273
```

Each target is a thin wrapper over `cargo xtask` (`cargo xtask help`), which
works without make. `deckjack` serves `~/.deckjack` (or `$DECKJACK_HOME`) by
default; `--dir` points it elsewhere.

When the working directory is the install directory, the install directory's
own entries (`bin/`, `apps/`, `run/`, `keys/`, `cache/`, `proxy/`,
`debug-session.cookie` and `server.log`) are never shown or reachable through
Files, the file commands or `/content`, and no app manifest may ask for them.
The same holds when `--dir` points at a directory that contains the install
directory. Your own files and folders beside those entries work as usual,
including ones whose names differ only in case, such as `Cache/` or `Apps/`
(on a case-folding mount, where `BIN` is the same file as `bin`, it is refused
too).

Apps install one at a time with the core's own installer, which `make install`
also uses for the apps in this repository:

```sh
deckjack install crates/deckjack-notes --binary target/release/deckjack-notes
deckjack apps                # what is installed, and what will not run and why
deckjack uninstall notes
```

It validates the manifest, refuses a binary built for another architecture,
and swaps `~/.deckjack/apps/<name>/` atomically, so updating a running app is
safe. It never approves anything. Its output says whether the approval stored
in the working directory (`--dir`, by default the install directory, as for the
server) covers the installed version. Without `--dir` that directory is a
guess, so where no server has recorded approvals there (as with `make install`
for a server started with `--dir`), the report says the approval is unknown and
to rerun with the same `--dir`. An app that needs approving, because it
is new, was never approved or its `[needs]` or commands changed, is approved
from the shell: open the command palette and choose "Approve <app>". The entry
appears for every app that is waiting and runs `system.app.approve` after you
confirm. Binaries are not signed; install only what you built or trust. A
running server sees the change when it restarts.

**Upgrading past the SHA-256 approval fingerprint.** Approvals now fingerprint
an app's `[needs]` and commands with SHA-256 instead of fnv1a64. Every app
approved before the change is asked for approval once more, even if nothing
about it changed. After upgrading, `deckjack install` reports such apps as
needing approval ("recorded under the fingerprint this version of Deckjack
replaced"), and so does opening them. Approve each one once from the palette.

`deckjack --debug` is for tools rather than people: it listens on loopback
only, logs at debug level, and writes a session cookie to
`~/.deckjack/debug-session.cookie` so a script can call the node without the
sign-in code. Anyone who can read that file is signed in. Send what a browser
sends, too: `/content`, `/passkey/*` and `/logout` refuse a request without
`Sec-Fetch-Site: same-origin`, and `/ws` wants that or an `Origin` naming the
node's own host.

## Behind `tailscale serve`

Deckjack listens on loopback and is reached through Tailscale's HTTPS. Point
`tailscale serve` at a Unix socket, not at the TCP port. In
`~/.deckjack/.deckjack/config.toml` (the working directory's config):

```toml
[server]
unix_socket = "proxy/http.sock"  # relative to ~/.deckjack, or $DECKJACK_HOME
trusted_proxies = []             # nothing believed over TCP

[passkey]                        # before enrolling a passkey; see below
rp_id = "pocket.tail1234.ts.net"
origin = "https://pocket.tail1234.ts.net"
```

Restart, then:

```sh
sudo tailscale serve --bg unix:$HOME/.deckjack/proxy/http.sock
```

The startup banner prints the exact path. Only requests that arrive on that
socket have their `X-Forwarded-For`, `X-Forwarded-Proto` and
`X-Forwarded-Host` believed. The socket is mode 0600 and every connection's
peer uid must be root, which is what tailscaled runs as
(`unix_socket_peer_uid`). Apps run as you, so an app cannot pose as the
proxy. Inside the install directory the socket must be under `proxy/`, which
Files and app grants never reach; the node refuses to start with it anywhere
else there (including `run/`, where app sockets live). A path outside the
install directory is taken as written. Signed in over HTTPS, the cookie is
`__Host-dj_session`, so every browser signs in once after this change.

Set `[passkey]` to the node's tailnet name before enrolling a passkey. Left
empty on a loopback listener, passkeys bind to `localhost`, which a browser on
`https://<node>.ts.net` refuses, and a passkey cannot be moved to another name
afterwards. Sign-in with the code works either way.

The old setup, `tailscale serve http://127.0.0.1:7878` with `trusted_proxies =
["127.0.0.1"]`, still works. It believes any local process that can open a TCP
connection, and the banner warns when an approved app holds a network grant.
Approval and launch refuse any app whose grant could reach the core's port:
`connect_tcp` naming the core's port, or a host list with no ports while the
core listens on loopback or while `trusted_proxies` names any address. The
sandbox refuses MPTCP sockets, which Landlock's port rules do not cover, and
io_uring, which would go around that refusal.

## Lineage

Deckjack replaces JackLink, a Python gateway that served a browser desktop over
a USB tether. JackLink's approach was right for everything except its VNC app;
Deckjack keeps the model and drops the pixel streaming. JackLink's MIT-licensed
source is in this repository's history at `7b89edb`.

## License

AGPL-3.0-or-later. A hosted fork is the likely way someone would take this, and
the AGPL is what closes that gap.
