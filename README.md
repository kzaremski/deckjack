# Deckjack

One browser-based workspace, served by machines I own, that presents as a
desktop. Every task has a native web UI; agents are an optional layer acting
through the same commands I do.

Three rules define it:

- **Every interface is real web UI.** DOM, canvas or WebGPU rendered in the
  browser. No VNC, RDP or pixel-streamed desktops.
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
cargo build
cargo test
```

## Lineage

Deckjack replaces JackLink, a Python gateway that served a browser desktop over
a USB tether. JackLink's approach was right for everything except its VNC app;
Deckjack keeps the model and drops the pixel streaming. JackLink's MIT-licensed
source is in this repository's history at `7b89edb`.

## License

AGPL-3.0-or-later. A hosted fork is the likely way someone would take this, and
the AGPL is what closes that gap.
