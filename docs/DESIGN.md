<!--
  The design of record, as written 2026-11-03, kept verbatim below.
  Decisions taken since, which contradict parts of it:

  2026-11-08
  - Greenfield. Deckjack does NOT grow out of agentmancer. Four of the five
    foundations the doc names (server, sign-in, terminal, session browser)
    were Node-specific and could not cross to Rust; the editor is an external
    binary either way. agentmancer survives only as the optional Node app
    described under App suite.
  - `lumen` is therefore not promoted, it is rebuilt. The token file is
    multi-target from the first commit rather than a generalization of a
    VS Code theme generator.
  - The Rust foundation is harvested from pocketdock instead: `sys.rs` and the
    one-time-code auth state machine with its lockout rules, which become the
    tether-mode sign-in path. Nothing of pocketdock's cable mode, Xvnc, Caddy,
    nftables or CLI comes across.
  - JackLink's Python is removed rather than kept as a port reference. Its
    MIT-licensed source stays in git history at 7b89edb.
  - Licensed AGPL-3.0-or-later as the doc proposes.
-->

# Deckjack: Design

Nov 3, 2026 · @Konstantin Zaremski

## Summary

Deckjack is one browser-based workspace, served by machines I own, that presents as a desktop. Every task has a native web UI, and agents are an optional layer acting through the same commands I do. It reuses agentmancer's foundations (server, sign-in, terminal, editor, session browser), but where agentmancer is an agent console, Deckjack is a computer: agent work, including the Inbox, lives in the Assistant app.

Three rules define it:

- **Every interface is real web UI.** DOM, canvas or WebGPU rendered in the browser. No VNC, RDP or pixel-streamed desktops for the UI.
- **Backends are Rust services and CLI tools.** The browser is the only front end; the heavy lifting runs on whichever node fits.
- **One command bus.** Every action is a typed command. I, the CLI and agents all call it; an agent's risky commands wait in the Assistant's Inbox.

Status: draft proposal. Nothing beyond current agentmancer is built.

### Lineage

Deckjack is a personal take on the thin-client idea: stateless clients, the server holds everything, and my session follows me.

| Ancestor | What it got right | Why it faded | Deckjack's difference |
| --- | --- | --- | --- |
| [Sun Ray](https://en.wikipedia.org/wiki/Sun_Ray) (Sun, 1999) | Smart card in any terminal brings the session back exactly as left | Streamed pixels felt laggy off the LAN; a brick without its server; discontinued under Oracle | Browser renders the UI locally; the server can be any machine I own, including the laptop offline |
| [Network Computer](https://en.wikipedia.org/wiki/Network_Computer) (Oracle, about 1996–2000) | Cheap diskless desktop on open web standards | PCs fell below $1,000; software was neither mature nor open; 28.8 kbit/s modems; no backend available | Runs on devices I already own; AGPL and plain files; the backend is a single binary |
| ChromeOS | Browser as the whole OS | Someone else's server | My server, possibly in my pocket |

Lessons carried forward: keep input handling and rendering in the browser, never depend on one network path, and never let a vendor own the data.

### Daily driver

The success test is using the computer through Deckjack full time; every time I leave it, the reason goes on a list that becomes the roadmap.

- **Installed as a PWA in its own window,** with the Keyboard Lock API in Chromium so Deckjack gets Ctrl/Cmd+W, T, Tab and Escape instead of the browser.
- **Files in and out of the machine I'm sitting at:** drag and drop into Files, the File System Access API in Chromium, and WebDAV for the host's file manager.
- **The web stays in the real browser.** Links open in browser tabs; the session browser is only for agents and locked-down machines.
- **Honest exceptions:** games and native-only pro apps stay outside. Calls and printing go through the browser.
- **Boot straight into it.** On a Linux laptop the login session can be a full-screen Chromium in app mode (`--app=https://deckjack.zarem.ski --kiosk`) or a minimal Wayland compositor such as `cage` running only the browser, with the server local so it works offline: effectively my own Chromebook.

## Goals and non-goals

The goal is a daily-driver computer for the overwhelming majority of my tasks, used from any browser: Mac, Fold, DeX, tablet.

**Goals**

- Native web UI for every app I use regularly: terminal, editor, files, browser sessions, PR review, media editing, notes, task lists.
- One consistent design system with my own optimizations, applied to every surface, including embedded third-party UIs.
- Work runs on the best available node: Mac, Linux box, cloud GPU, or a phone in Termux.
- Agents are a first-class option for any task, never a requirement.
- Private by default: single operator, my machines, my accounts.

**Non-goals**

- **Pixel-streamed UI.** No VNC, RDP, Kasm-style desktops or remote X sessions as an app's interface. If an app only exists as a native desktop window, it is out of scope until it has a web front end.
- Multi-user or team hosting.
- Electron or native wrappers as the primary client. A PWA install is fine.
- Replacing the OS. Nodes keep their normal OS; this is a layer on top.

**The one exception: streamed content, never streamed UI.** Video frames are allowed where the content itself is video: the FilmCraft program monitor, and a live view of an external web page inside the session browser. The controls around them are always web UI.

## Core principles

These rules decide every design question below; when a feature conflicts with one, the feature changes.

1. **Web-native or not at all.** Every control is rendered by the browser from my own components. Canvas and WebGPU are allowed for surfaces that need them (timelines, monitors, scopes, waveforms), but they draw with the same tokens as the DOM.
2. **Structured first, terminal second.** A CLI tool gets a typed adapter and a real UI for its common paths. The raw terminal stays available as the escape hatch, never as the main interface.
3. **Everything is a command.** Each app action has an id, typed parameters, a risk level and an enabled state. The UI, keyboard shortcuts, the command palette, the CLI and agents all dispatch the same commands.
4. **Agents ask, I act.** My own actions run directly, with ordinary confirmations for destructive ones. Agent commands above a risk threshold wait in the Inbox, which is part of the Assistant. Agents get no back door.
5. **Agents are optional, never bolted on.** Any command an agent can run, I can run from the palette, and any task I do by hand, an agent can be handed.
6. **One working directory.** The host machine's working directory is the whole world: user data in plain files and open formats, system state in one hidden SQLite file. No database server.
7. **Location doesn't matter to the UI.** A panel never knows or cares which node its backend runs on.

## Deckjack as an OS

The goal is a complete user-space OS I can run my life out of, started on any computer and reached from any browser. It sits on top of the host OS rather than replacing it.

| OS concept | In Deckjack |
| --- | --- |
| Kernel and processes | The server and its nodes, running backends wherever they fit |
| Filesystem | The host's working directory, managed by whichever machine is hosting |
| System state | One hidden SQLite file in the working directory |
| Window manager | Desktop windows, or compact single-window mode with an optional split |
| System calls | The command bus |
| Permissions | Ordinary confirmations for me; scoped grants plus the Assistant's Inbox for agents |
| Package manager | Installable apps: a web panel plus a backend or CLI adapter |
| Shell | The command palette and the terminal |
| Login | Single operator: passkeys, plus tether mode for untrusted machines |
| Notifications | Web push to the installed PWA |

**Longevity rule:** user data lives in plain files and open protocols, so if Deckjack disappeared my files, mail, calendar, photos and docs would still work with other tools.

## App suite

Deckjack owns every UI and borrows the hard backends through open protocols; each new app is a web panel over an existing server, not a new project.

| Area | Deckjack UI over | Notes |
| --- | --- | --- |
| Files | The working directory, WebDAV | Ported from JackLink |
| Notes and documents | Own editor (TipTap/ProseMirror + Yjs) | Office files via conversion (pandoc, LibreOffice headless); ONLYOFFICE as the fallback for exact round-trips |
| Tasks | Native app | Grows from agentmancer's todos |
| Terminal and code | xterm + PTY, code-server | Already in agentmancer |
| Calendar and contacts | CalDAV, CardDAV | Nextcloud on the R620 serves both today |
| Mail | JMAP or IMAP | Read the provider's mailbox; never self-host outbound mail |
| Photos | Immich API | AGPL as well |
| Passwords | KeePass `.kdbx` with TOTP | Ported from JackLink; disabled in tether mode |
| Video and audio | FilmCraft engine | Touch-first timeline, bus-level audio, loudness normalization |
| Chat | Matrix | Bridges for other networks |
| Assistant | Rust agent harness whose only tools are Deckjack commands | Holds the Inbox; sandboxed with Landlock and seccomp; optional |

**Order:** files, notes/documents and tasks first, since they're daily use and force the platform pieces to exist. Then protocol-backed apps one at a time, and the video editor last, landing on a proven platform.

### PIM suite

Mail, calendar and contacts run as a long-running app that mirrors everything into the working directory in open formats, so they work offline and outlive any provider.

|  | Pass-through client | Local mirror (chosen) |
| --- | --- | --- |
| Offline laptop | Nothing works | Everything works; changes sync when back online |
| Search | Depends on the server | Fast local full-text search |
| Longevity | Data lives only at the provider | Mail, calendar and contacts I own |
| Cost | Simple | Sync conflicts and storage |

- **Formats in the working directory:** Maildir (one `.eml` per message), one `.ics` per event or task, one `.vcf` per contact. Search index and sync state live in hidden state (SQLite FTS5).
- **Sync services on the active node:** JMAP push where the provider supports it, else IMAP IDLE; CalDAV and CardDAV against Nextcloud on the R620. Existing tools (`mbsync`, `vdirsyncer`) can run as CLI adapters first and be replaced by a Rust service later.
- **Sending** goes through the provider's submission endpoint. Outbound mail and invites an agent drafts wait in the Assistant's Inbox.
- **Agents help with** triage, summaries, drafting replies, turning email into events or tasks, and finding meeting times.
- **Watch:** a mirrored mailbox can reach tens of gigabytes (fine on the R620's ZFS pool; laptop and phone may mirror recent mail only), and two-way calendar edits from Deckjack and a phone app can occasionally duplicate.
- **Open:** which mail provider (decides JMAP vs IMAP).

### Reminders

Reminders are data in iCalendar and the Deckjack server is the one thing that fires them.

- **Storage:** a task (`VTODO`) or event (`VEVENT`) with an alarm (`VALARM`), as `.ics` files synced over CalDAV. Android can share them (DAVx⁵ plus a tasks app); Apple Reminders no longer syncs over CalDAV.
- **Scheduler:** computes the next fire time for every alarm, expanding recurrence rules, and keeps the queue in hidden state. Times carry their IANA zone (`America/Denver`) so daylight-saving changes don't shift them. Snooze and done write back to the `.ics` file.
- **Delivery:** web push to the installed PWA (on iPhone it must be added to the home screen), with ntfy, email or Matrix as fallbacks and optional escalation when a high-priority reminder goes unacknowledged.
- **One firing authority:** turn off alerts in other clients, or every reminder fires three times.
- **Missed reminders** after downtime or a host switch fire on startup if within about 24 hours, marked as missed; older ones arrive as one digest.
- **Limits:** an offline laptop host can't use push services, so reminders show only in open windows; location-based reminders stay with the phone's native app.
- **Agents** may create reminders freely; deleting or bulk-changing them goes through the Inbox.

## Architecture

&#91;embedded content: system architecture · browser, shell node, nodes on a private mesh\]

The browser only talks to the shell node, which forwards signed commands to whichever node owns the backend; media streams go straight back to the browser.

- **Browser client.** A PWA desktop shell: windows, a dock and the command palette. Panels are Vue components on the shared tokens; heavy surfaces are canvas or WebGPU (wasm where it pays off).
- **Shell node.** The Rust deckjack core: it hosts the embedded shell, runs the command bus, generates MCP tools for agents, owns sign-in and keeps the event log and node registry. It replaces agentmancer's Node server; agentmancer survives as an optional coding-agent app on hosts with Node.
- **Nodes.** Any machine running the node daemon. It registers its backends and capabilities (GPU, codecs, storage) with the shell node, and the scheduler places work by capability.
- **Command bus.** Commands carry an id, a JSON Schema generated from Rust types (schemars), a risk level and a target node; the browser's TypeScript types are generated from the same definitions (ts-rs or specta). Results and progress return over the same WebSocket.
- **File layer.** One namespace across nodes (`node:path`), with on-demand fetch and local caching; panels never handle raw node addresses.
- **Media streams.** Rust services stream preview video and audio over WebRTC directly to the browser, never UI pixels.

### Installation and app binaries

Deckjack installs per machine into `~/.deckjack/`: one core binary plus one `deckjack-<name>` executable per app, discovered and launched by the core like `git-*` subcommands.

**Install directory vs working directory.** `~/.deckjack/` holds binaries, caches and the node's identity key, built for that machine (x86-64 tuned for the R620, `aarch64-linux-android` for the Fold), and never travels. The working directory holds my data and the hidden `.deckjack/state.db`, and is the only thing synced or carried.

```
~/.deckjack/
  bin/deckjack                  core: server, shell, command bus, window manager
  apps/
    video-editor/
      deckjack-video-editor
      app.toml                  manifest
    pim/deckjack-pim
    assistant/deckjack-assistant
  cache/                        thumbnails, proxies, build artifacts
  keys/                         node identity
```

**Each app binary contains** its backend (for example FilmCraft's engine), its web UI embedded like the core's shell, and an `app.toml` manifest: name, version, protocol version, the commands it provides, and what it needs (GPU, network, which parts of the working directory, on-demand or long-running service).

**How the core runs apps**

1. **Discovery:** scan `~/.deckjack/apps/*/app.toml` at startup.
2. **Launch:** on demand when a window opens, or at startup for services such as PIM sync, reminders and the assistant. Idle on-demand apps are stopped.
3. **Handshake** over a Unix socket or stdio: the app registers its commands as JSON Schema, and they appear in the command palette and as agent tools automatically.
4. **UI routing:** the core serves each app's UI at `/apps/<name>/` on the same origin, so sign-in, theming and the window manager stay unified. The browser never talks to an app directly.
5. **Supervision:** a crashed app is restarted, and its windows show "reconnecting" instead of taking down the desktop.

**Permissions.** Each app runs under the Landlock and seccomp launcher with only what its manifest requested and I approved. No app ever sees `.deckjack/`; the core is the only process that opens `state.db`, and apps reach state through the bus.

**Installing and updating.** `deckjack install <app>` downloads the build for this machine's architecture, verifies its signature and unpacks it into `apps/`. Updates swap the binary and restart the app only when none of its windows are open. A host installs only the apps it can run: the Fold can skip the video editor.

**Decide early**

- **Protocol stability:** version the handshake and command schemas from day one, and refuse apps built against an incompatible protocol with a clear message.
- **Third-party apps and AGPL:** my own apps are unaffected. Separate programs talking over a socket are generally separate works, but plugins exchanging complex internal structures can be argued to be one combined program, so publish the app protocol under a permissive license or add an explicit exception if outside apps are expected.

**Embedded assets.** The core binary carries the whole web shell: Vite builds `web/dist/`, `rust-embed` bakes it in at compile time, and `axum` serves it with an `index.html` fallback for client-side routes. Assets are pre-compressed (gzip and Brotli) at build time; hashed files are cached forever, `index.html` never. Debug builds read from disk so Vite hot reload still works. An `xtask` runs the web build before `cargo build`, so Rust builds need no Node (including inside Termux). SQL migrations, design tokens, themes, fonts (license permitting) and default manifests are embedded too; large optional parts (the FilmCraft engine, AI models, extra font packs, code-server) ship as separate app binaries or downloads.

## Design system

One token set, promoted from agentmancer's `lumen` UI kit, styles every surface; nothing gets its own palette.

**Tokens as the single source**

- Color, type, spacing, radius, motion and density live in one token file. CSS variables, Rust (for canvas and WebGPU surfaces) and generated themes all come from it.
- The existing Night and Circuit themes become the first two token sets. `npm run vscode-theme` generalizes into one generator that also emits xterm, code-server, mermaid and Monaco themes.
- Canvas surfaces read the same tokens at runtime. A FilmCraft timeline or a waveform should be indistinguishable in style from the DOM around it.

**Layout primitives**

- A shell with docked panels, tabs and a split system, shared by every app.
- Responsive breakpoints for phone, folded Fold, unfolded Fold, tablet and desktop/DeX. Each app declares its compact and wide layouts rather than shrinking the desktop one.

**Operator optimizations (mine, built in rather than per app)**

- Keyboard first: a global command palette over the command bus, user-editable shortcuts, and vim-style navigation where it fits.
- Density control: compact by default, with a global density token.
- Pointer, touch and pen all first-class: hover states never hide essential controls, and touch targets scale with the density token.
- Instant feel: optimistic UI for local state, skeletons over spinners, no layout shift.
- Desktop first: the default experience is windows, a dock and the palette. The Assistant is one app among many, and its pending approvals surface as a dock badge and a notification.

**Embedded third-party UIs**

Web UIs I don't own (code-server, other web apps) are themed through generated themes and wrapped in the shell's chrome. If one can't be themed acceptably, it gets replaced by an own panel over its backend.

## Window manager

One set of open windows, shown two ways: a desktop window manager on wide screens and a single-window view with an optional split on phones and tablets.

- **Windows are server state.** Each window is an app instance held by the server, so every device sees the same open windows.
- **Layouts are per device class.** The server keeps a desktop layout (positions, sizes, z-order, tiling) and a compact layout (focus order, split pairs) for the same windows, so using the phone never scrambles the desktop arrangement.
- **Mode follows viewport and input, not device.** Wide screen with a pointer gets desktop mode; narrow or touch-first gets compact mode. An unfolded Fold or a phone in DeX switches automatically, with a manual override.
- **Desktop mode:** floating windows with snapping, optional keyboard-driven tiling, a dock, and pop-out of any window into its own browser window for a second monitor. JackLink's window manager is the reference for chrome, drag and resize.
- **Compact mode:** one window fills the screen, a switcher orders the rest by recent focus, and two windows can split with a draggable divider (defaulting to the hinge on an unfolded Fold).
- **App contract:** every app declares compact and wide layouts and a minimum width, and responds to whatever space it's given.

**Prototype:** [Deckjack Shell Prototype](https://claude.ai/artifact/JYJ3ghsbsmQ7KtDLmHK7RG) has clickable login (passkey and one-time code), desktop (draggable windows, taskbar, command palette) and tablet (single window, switcher, split) screens sharing one app component.

## Backends

Every app is a web panel plus a backend that registers commands; the backend is one of three kinds.

| Backend kind | Example | How it plugs in |
| --- | --- | --- |
| Rust service | FilmCraft engine, file indexer | Long-running process on a node; registers commands over the bus; streams media when needed |
| CLI adapter | git, gh, ffmpeg, docker, rsync | A manifest maps commands to invocations with typed params (JSON Schema) and parses output into structured results |
| Node app (optional) | agentmancer with Claude Code coding sessions | An app binary that installs only on hosts with Node; unavailable when a phone hosts |

**CLI adapters in detail**

- Each adapter declares: the binary, commands and their parameter schemas, how to parse output (JSON flag, regex or line parser), risk level per command, and which panel renders the result.
- Long-running invocations stream progress and logs to the panel; a "show terminal" toggle reveals the raw PTY for the same run.
- Unadapted tools run in the terminal panel. Frequent unadapted commands are a signal to write an adapter.

**Rust services**

- Built as headless crates with a command registry, following FilmCraft's layering: no UI toolkit or OS API below the front end.
- Cross-compiled per node target: macOS, Linux x86\_64 and ARM64, and `aarch64-linux-android` for Termux.
- Media-heavy services stream preview frames and audio to the browser over WebRTC; all controls stay in the web panel.

### Server language: Rust

The whole server is Rust; TypeScript lives only in the browser, so any machine, including the Fold in Termux, can host everything.

|  | Rust | TypeScript (Node) |
| --- | --- | --- |
| Hosting on Termux | One static binary per target, no runtime | Works, but native addons (`node-pty`) break across Android and Node versions |
| Memory and battery on a phone | Low | Noticeably higher |
| Same language as the heavy parts | Yes: FilmCraft engine, assistant harness, Landlock/seccomp sandbox | No: two server languages |
| Reuse agentmancer | Rewrite | Direct reuse |
| Iteration speed | Slower compiles | Fast, hot reload |

- **Building blocks:** `axum` and `tokio`, `rusqlite`, `portable-pty`, `rmcp`, the official ACP Rust SDK (`agent-client-protocol`), `webrtc-rs`.
- **Browser side stays TypeScript/Vue,** with wire types generated from Rust.
- **Why not keep Claude Code on every host:** it is not open source, so there is nothing to build for Android. Since v2.1.113 it ships a glibc native binary with no Android build; Termux users report "native binary not installed", `/tmp` permission errors, DNS hangs, "Bad system call" crashes from Android's seccomp filter and auto-updates that break working installs. Workarounds (pinning v2.1.112, patching with glibc-runner, proot Ubuntu) all make the phone imitate desktop Linux. Claude Code coding sessions therefore run only on hosts with Node and glibc (the R620, the laptop).
- **Termux rules for my own code:** use `$TMPDIR`, not `/tmp`; use the system DNS resolver; avoid syscalls Android's seccomp blocks; test on a real phone before each release.

## Agents

Agents see the same command bus I do, exposed as MCP tools, and every command they run is attributed and gated like mine.

Agent work lives in one app, the Assistant: the always-on assistant, agentmancer-style coding sessions, and the Inbox of pending approvals from any of them. With the Assistant closed or uninstalled, Deckjack is a complete desktop with no agents at all.

- **One tool surface.** The bus generates MCP tool definitions from command schemas. agentmancer's hand-written tools (`ask_operator`, `review_desk`, `todo_*`, browser tools) migrate into bus commands over time.
- **Scoped grants.** A session gets a set of command namespaces (for example `git.*`, `files.read.*`, `filmcraft.timeline.*`) and nodes it may target. Anything outside its grant is refused, not queued.
- **Risk routing.** Read-only commands run immediately. Writes inside a project run under the session's grant. Destructive, outbound or cross-project commands go to the Assistant's Inbox, as tool permissions do in agentmancer today.
- **Attribution.** Every command records who issued it (me, a named session, the CLI) in an append-only log per node, viewable as a timeline panel.
- **Hand-off both ways.** From any panel I can hand the current context to an agent ("finish this cut", "triage these files"); from any agent session I can take over and continue by hand, as with the session browser today.
- **Existing conventions stay.** Behavior corrections go into OPERATOR\_PROMPT or a hook, with a test that the setting reaches the agent, as CLAUDE.md requires.

### The always-on assistant

An OpenClaw-style personal assistant, except it can only act through the command bus: every capability is a typed, scoped, logged Deckjack command, and every risky one waits in the Inbox.

- **Own Rust harness, not Claude Code.** The agent loop calls the Claude API directly (API key billing), and the only tools it is ever sent are Deckjack commands. There is no shell or file tool to deny and no hook to bypass. It speaks the agent side of ACP with the official Rust SDK, so the Assistant app talks to it like any other session.
- **Sandbox, three layers deep:** (1) tools are bus commands only; (2) the process runs under a Landlock and seccomp launcher built into the same binary, seeing only its private home, a Unix socket to the bus and a network path to the model API (an allowlisting HTTPS proxy via `HTTPS_PROXY`); (3) risky commands still wait in the Inbox. It never holds credentials: it calls `mail.send`, not IMAP.
- **Platforms:** Landlock and seccomp are Linux-only (R620, Linux laptop). macOS needs a small Linux VM or Seatbelt profiles; Termux has no namespaces without root, so on the phone the tools-only layer carries the weight, or the assistant runs on the R620 even when the Fold hosts. Plain `chroot` is not a security boundary and isn't used.
- **Triggers:** reminders firing, mail arriving, schedules ("brief me every morning"), Inbox events, and messages I send it from the PWA or Matrix. Quiet hours apply.
- **Memory:** plain files in the working directory I can read, edit and diff; sessions rotate daily or by topic.
- **Lessons from OpenClaw**, the reference design for this category and its security record:
  - Full system access by default led researchers to call it a security nightmare → no access beyond bus commands.
  - Poisoning any part of an agent's persistent state raised attack success from about 25% to 64–74% in one study → memory changes made after reading untrusted content go through the Inbox.
  - Prompt injection through email, web pages and logs → content is data, never instructions.
  - Thousands of malicious marketplace skills → no open marketplace; every tool written or vetted by me.
  - Exposed and hijackable instances → VPS front door, OAuth, origin checks, no public ports.
  - An agent deleting much of a researcher's inbox → bulk and destructive commands show a count and need approval.

### Agents in apps

Every app's commands are already on the bus, so agents can work inside apps in two modes.

- **Hop into a running app (co-pilot).** I invite it from a window's menu; it gets that one window's commands until I end the session. It shows its work (agent cursor, "Assistant is editing" banner), its changes group into undo steps, and a stop button sits on the window. Documents merge concurrent edits through Yjs; apps without that (the video timeline) take turns.
- **Spawn apps it controls.** The agent opens its own instance (a document, timeline or file browser), visible under the Assistant in the dock or headless, hands over the result, then closes it or lets me take over. Instances are capped and carry only the Assistant's grants.
- **What every app provides:** commands as tools; a "describe state" command returning structured data (tracks, outline, folder), with screenshots only for visual checks; events ("export finished") so agents react instead of polling; automation IDs on UI elements as a last resort, as FilmCraft does.
- **Guardrails carry over:** risky commands wait in the Inbox even inside an invited window, content stays data, and every action is logged as "Assistant via window X".
- **Order:** documents and files first, then the video editor on FilmCraft's command set.

## Security and remote access

A browser that reaches a shell, my files and agent sessions is the highest-value target I own, so remote access never means an open port.

- **Private network only.** Nodes keep binding to `127.0.0.1` and are reached through a private mesh (Tailscale or equivalent) using its HTTPS certificates. No public ingress.
- **Strong sign-in.** Passkeys replace the bare access-token cookie for remote clients; the token stays for local CLI use. Sessions expire, and a new device needs approval from an already signed-in one.
- **Keep today's protections.** WebSocket origin checks, secrets in `~/.agentmancer/secrets.json` at mode 0600, and the Inbox gate stay as they are.
- **Node-to-node trust.** Each node has its own key pair; the shell node signs commands it forwards, and nodes refuse unsigned or replayed ones.
- **Per-command risk levels** are declared by the backend, not inferred by the UI, so an adapter bug cannot downgrade a destructive command.
- **Untrusted content stays data.** Web pages in the session browser, file contents and tool output never become commands, matching how agentmancer already treats them.
- **Kill switch.** One command, available from the CLI and any signed-in client, revokes all remote sessions and pauses all agents.

### Access path

One public name points at a cheap VPS, which routes to whichever node is active over the tailnet; no node ever opens a public port.

- **DNS and TLS.** `deckjack.zarem.ski` resolves to the VPS's public IP, where Caddy terminates TLS. One stable domain keeps passkeys working from every device.
- **Off-tailnet clients** (work laptop, borrowed PC) pass an OAuth proxy (oauth2-proxy, allowlisting my email) and then Deckjack's own sign-in.
- **Tailnet clients** use split DNS to reach the VPS over its tailnet IP; Caddy skips the OAuth step for `100.64.0.0/10`. Deckjack's own sign-in still applies.
- **Routing to the active node.** Caddy lists every node as an upstream with `lb_policy first` and health checks. Only the node holding the lock answers `/healthz`, returning its name and lock generation.
- **On each node** the server keeps binding to `127.0.0.1`; `tailscale serve` exposes it to the tailnet.
- **Media.** The VPS carries WebRTC signaling only; media flows directly over the tailnet where possible, with a TURN relay on the VPS for off-tailnet clients.
- **Fallbacks.** Direct tailnet node names work if the VPS is down (passkeys excepted). An offline laptop maps the domain to `127.0.0.1` in its hosts file and keeps a copy of the certificate.
- **App changes needed:** WebSocket origin checks allow the public domain, and the OAuth identity header is trusted only from the VPS's tailnet address.

### Host access

The web UI gets full filesystem and terminal access on the host through the server; the browser itself stays sandboxed.

- **Two filesystems.** The host's (R620, laptop or Fold) is fully reachable through the server: Files can browse `/` or home, with the working directory as its home, and the terminal is a real shell as my user over a PTY. The device I'm sitting at only shares what I pick, drop or grant through the File System Access API.
- **`.deckjack/` stays hidden from app routes;** the terminal is the documented escape hatch (see Working directory and hidden state).
- **Scope by session type.** My devices on the tailnet get full access. Tether-mode and off-tailnet sessions through the VPS get the working directory only and no terminal until they step up.
- **Step-up sign-in:** opening a terminal or browsing outside the working directory from a remote session asks for the passkey again, so a stolen cookie alone never yields a shell.
- **Agents never inherit host access;** a specific path must be granted.
- **Audit log** records terminal sessions and paths touched outside the working directory; destructive file actions outside it ask for confirmation.

### Lockdown

A kill switch that locks everything down rather than just killing the process, since a supervisor or attacker could simply restart it.

1. **Revoke** every session and rotate the session signing key.
2. **Kill** all child processes: terminals, agents, app binaries, sync services.
3. **Freeze** outbound actions: queued mail, pending Inbox items and scheduled tasks.
4. **Serve only a "locked down" page** until unlocked.

- **Triggers:** one click in the menu, palette or a hotkey (any signed-in session, no step-up); `deckjack lockdown` on the host; a separate lockdown link or signed ntfy/Matrix command from the phone; automatically on a sign-in from a new device, repeated failed passkey attempts, a terminal opened from a tether session, or a burst of refused agent commands (or an alert asking me to confirm).
- **Asymmetric:** unlocking needs the passkey plus either `deckjack unlock` on the host or an offline recovery code.
- **Limit:** it defeats a stolen session, not a compromised host. If someone has a shell as my user, the levers are outside Deckjack: disable or revoke the node in the Tailscale admin console, stop Caddy on the VPS, stop the LXC in Proxmox, or pull power or network.
- **Afterward:** the audit log ships to the VPS as written so a host attacker can't erase it; preserve evidence before wiping; rotate the API key, mail and calendar passwords, the OAuth client secret, Tailscale auth keys and passkeys if needed; restore from a pre-incident ZFS snapshot if anything changed.

### Tether mode

Tether mode is JackLink's original idea: serve Deckjack from a phone or laptop over a USB tether to any computer's browser, with nothing installed on that computer. The host is untrusted (keyloggers, screen recorders, malicious extensions).

- **Sign-in:** a one-time code shown on the phone and approved there, not a passkey on the borrowed machine.
- **Reduced scope by default:** working directory only, no terminal, no KeePass or secrets, no agents with outbound permissions; Inbox approvals only from the phone.
- **Ends with the cable or the tab:** unplugging or closing revokes the token; nothing is cached on the host.
- **JackLink features to port as backends** (from Python to Rust): the file manager, WebDAV (off by default per session), the SOCKS5 proxy for borrowing the node's network and Tailscale (off by default), the KeePass reader with TOTP (never in tether mode), and the startup screen showing interfaces, URL and token. JackLink's VNC desktop is dropped: it breaks the no-pixel-streamed-UI rule.

## Data directory and portability

The system assumes exactly one server running out of one data directory at a time; how that directory moves between machines is up to me.

Connected clients hold no state of their own: the running server is the single source of truth. A browser that reconnects, or a different device that connects, renders whatever the server holds; anything a client keeps locally (cached panels, unsent input) is disposable and reconciled from the server on reconnect.

- **One data dir, one server.** The whole workspace (layouts, Inbox, todos, sessions, transcripts) lives under `AGENTMANCER_HOME`. Start the server on the R620, the laptop or the Fold, and it resumes from whatever that directory holds.
- **Syncing is out of scope.** Nextcloud, Syncthing, a USB drive or nothing at all are my choice; the project never syncs, merges or resolves conflicts.
- **A lock file guards against two servers and unclean exits.** It records host id, PID, start time and a heartbeat timestamp updated while running. This extends today's `lock.ts` and `server.json`, which already enforce one server per machine.
- **Startup rules:**
  - No lock: take it and start.
  - Lock from this host, PID gone: the last shutdown was unclean. Run recovery (resume checkpointed sessions, replay or discard partial writes), then take the lock.
  - Lock from another host: refuse to start and show the host and last heartbeat, since liveness can't be checked remotely. `--force` takes over after a warning.
- **Graceful shutdown** checkpoints agent sessions, flushes state and removes the lock, so the next start on any device is clean.
- **Keep node-local files outside the data dir.** Caches, build output and per-machine keys go elsewhere (for example the XDG cache directory), so whatever I use to move the data dir doesn't drag them along.
- **Hardware notes for the R620 (Proxmox LXC).** No GPU, so media rendering falls back to CPU. Its Sandy Bridge Xeons lack AVX2, so Rust services are built with `-C target-cpu=native` there, and prebuilt binaries that assume AVX2 are avoided.

### Working directory and hidden state

Deckjack runs out of one working directory on the hosting machine, which manages the filesystem; system state lives in a SQLite file inside it that Deckjack never exposes.

- **Apps see the working directory as their filesystem root.** User data stays in plain files and open formats there.
- **System state** (windows, layouts, Inbox, sessions, grants, settings) lives in one SQLite file, for example `.deckjack/state.db`.
- **Never exposed, under any circumstances:** the file API, WebDAV, search, previews, downloads and agent tools all refuse `.deckjack/`. Path resolution rejects `..` traversal and symlinks that point into it.
- **The terminal is an escape hatch.** It is a real shell as the host user, so like any shell on a real computer it can read `state.db`. Secrets stay encrypted inside the database; hiding the file from my own shell would need a separate OS user, which Termux can't provide. Remote sessions get the terminal only after step-up sign-in (see Host access).
- **Consistency for whatever sync I use.** SQLite runs in WAL mode. Graceful shutdown checkpoints the WAL into the main file (`wal_checkpoint(TRUNCATE)`) and closes the database before releasing the lock, so the directory is safe to copy. Copying while the server runs is unsafe, and the lock file says so.
- **Unclean shutdown** relies on SQLite's own journaling, plus Deckjack's recovery pass for sessions.
- **Migrations and backups.** The schema is versioned and migrated at startup; backups use SQLite's online backup API rather than copying the live file.
- **Secrets** are encrypted inside the database. Whether the key lives per node (secrets re-entered on each machine) or is unlocked by a passphrase or passkey at startup (secrets travel with the directory) is still open.

## Repo layout and public/private split

The system code stays public, as agentmancer is; everything personal lives outside the repo.

| Lives in | What |
| --- | --- |
| Public repo (agentmancer, grown) | Shell, node daemon, command bus, design system, generic adapters, docs/design notes |
| Separate repos | Large Rust services such as a FilmCraft fork, consumed as binaries per node target |
| `~/.agentmancer` on each node (never committed) | Node identity keys, secrets, host names, personal adapters, token overrides for my own theme tweaks |
| Private config repo (optional) | Sync of personal adapters and theme overrides across nodes |

CLAUDE.md's rule that nothing private goes into the public repo applies to every new module. A test that scans for host names, emails and key material before commit is worth adding, since there is no CI today.

### License

Deckjack is licensed AGPL v3, replacing JackLink's MIT, because a hosted fork is the likely way someone would take it.

- GPLv3's copyleft triggers only on distribution, so a hosted service could run a modified Deckjack without sharing changes. AGPL closes that gap, which is the lesson from Apple Notes Exporter applied to a server-first project.
- Relicensing isn't retroactive: JackLink versions already published under MIT stay MIT.
- MIT and Apache-2.0 code (FilmCraft, most Rust crates) can be used inside Deckjack; the reverse isn't true.
- Separate services reached over HTTP (ONLYOFFICE, Immich, Nextcloud) don't affect Deckjack's license either way.
- Accepting outside contributions without a contributor agreement would limit later relicensing and dual licensing; decide before the first external PR.

## Phased plan

Each phase ends with something I use daily; no phase starts until the previous one's exit check passes.

1. **Foundations.** The Rust core binary with the embedded shell, lock file and SQLite state; the command bus with generated MCP tools; the command palette; one terminal window; passkey login. Promote `lumen` to the token system and generate all themes from it.
   - Exit: one deckjack binary serves the shell and a working terminal on the R620, the laptop and the Fold.
2. **Remote and multi-node.** Tailscale access with passkeys, the node daemon, and the Fold in Termux as a second node.
   - Exit: I run a terminal and a git adapter on the Fold from the Mac's browser, and the reverse.
3. **First non-coding apps.** A files panel across nodes, then two or three CLI adapters I use daily (for example git, docker, ffmpeg).
   - Exit: a full day of non-agent work without opening a native terminal window.
4. **Media.** The FilmCraft engine as a headless Rust service, a touch-first web timeline in my design system, and the WebRTC program monitor.
   - Exit: cut and export a short video from the Fold browser with the engine on the Mac, then with the engine on the Fold itself.
5. **Daily-driver polish.** Phone and Fold layouts for every panel, offline-tolerant PWA shell, and adapters for whatever still sends me to a native window.
   - Exit: a week where native desktop apps are the exception.

Agents ride along in every phase: each new command is an agent tool the day it ships.

## Open questions

- [ ] Does the session browser's live view stream pixels (screencast) today? If so, confirm it fits the "streamed content, never streamed UI" exception. Not verified: browser.ts hasn't been read.
- [ ] Bus transport between nodes: WebSocket with signed JSON, or something with built-in streams (QUIC, gRPC)?
- [ ] FilmCraft UI: rebuild its panels in Vue on my tokens, or keep egui compiled to wasm and theme it from the same tokens?
- [ ] Does growing agentmancer into this make the public repo too personal, or should the shell become its own repo?
- [ ] CI: add typecheck and tests on push before multi-node work raises the stakes?
- [ ] Mail provider: JMAP-capable (push, better search) or IMAP?
- [ ] Secrets key: per node, or unlocked by passphrase or passkey so secrets travel with the working directory?
- [ ] App protocol license: permissive license or an explicit AGPL exception if third-party apps are expected?
- [ ] Assistant on the phone: tools-only enforcement in Termux, or always run it on the R620?
- [ ] Does `claude-code-acp-rs` call the API directly or wrap the Node CLI? Only the former helps on Termux.
