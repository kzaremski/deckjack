<!--
How app UIs stop running with the operator's authority.

Status: DESIGN ACCEPTED, NOT IMPLEMENTED. Decided 2026-11-16. Nothing below
exists in the code yet except where a section says so. Read docs/DESIGN.md
("UI routing") and docs/APP-SUITE.md §2 first.

Where this came from: a design panel (one trust-model map, four competing
proposals, three judges scoring on security, product fit and cost), a
synthesized recommendation, and a red team that attacked the recommendation
as a hostile installed app. The red team's fixes are folded into the design
below rather than listed separately. Line numbers are deliberately left out
where they would rot; function and file names are given instead.
-->

# App UI isolation

## 1. The problem

DESIGN.md's "UI routing" serves every app's UI at `/apps/<name>/` on the
shell's own origin. That was chosen so sign-in, theming and the window manager
stay unified, and it has a cost nobody paid for on purpose: **an app's
JavaScript runs with the operator's full authority.**

- `/apps/*` is gated only by `http::require_session` (cookie plus client
  address). The cookie is `dj_session`, `HttpOnly; SameSite=Strict; Path=/`.
  HttpOnly stops a script *reading* it, not *riding* it.
- `/ws` checks that `Origin` equals `Host` and that the cookie is valid. An
  app frame is same-origin, so it passes. The `Hello` frame carries no client
  identity, so every socket call becomes `Issuer::Operator`
  (`wiring.rs`, the socket adapter).
- **Confirmation is a client-asserted bit.** The invoke frame carries
  `confirmed: bool`, copied straight into the call. Any script can send
  `files.trash.empty` or `system.recovery_codes` with `confirmed: true` and
  skip the prompt.
- An app frame can also script the shell's DOM (`parent.document`), so even
  a correct confirmation drawn by the shell could be clicked by the app.
- An app whose `[needs]` is empty launches with no approval at all
  (`Host::check_grant`). Files is such an app.
- Landlock and seccomp confine the app's **process**. They do nothing about
  the JavaScript that process serves.

So today an installed app's UI can open a terminal, approve itself or another
app, issue grants, read the audit log, empty the trash, read the recovery
codes, enroll or remove a passkey, and lock the operator out. The trust map
behind this document lists 24 such attacks; §7 says which this design
closes.

## 2. Decisions taken

These are accepted. A later change reopens one by editing this section, not
by quietly building something else.

| | Decision | Accepted answer |
|---|---|---|
| **D1** | Breaking change to approvals | Every app re-approves once. Calling commands outside its own scope counts as a need, so **Files will need approval**. An app that only calls its own commands still launches unasked. |
| **D2** | Proxy trust behind `tailscale serve` | **Do not trust 127.0.0.1 as a proxy** once any app can reach loopback TCP, because an app process could then forge `X-Forwarded-For` and `X-Forwarded-Proto`. The target is a Unix-socket listener for `tailscale serve unix:/…` (supported by the installed tailscale 1.104.2; see `tailscale serve --help`). Only that listener is trusted. Until it lands: no manifest may be granted the core's listen port, and the cookie is renamed `__Host-dj_session` when `Secure` is reliably set (one re-login). |
| **D3** | Permanent deletes from app UIs | From an app UI, `files.delete` (recursive or not) is **rewritten to the trash**. From the shell it stays as it is, but routes as `destructive` with a confirmation drawn by the shell. |
| **D4** | Inline boot scripts in app UIs | **Moved to files.** One edit per app keeps `'unsafe-inline'` out of `script-src`. |
| **D5** | How much of `system.*` an app may call | **None of it.** All of `system.*` is hard-denied to app UIs. |
| **D6** | Broker conveniences at the flip | **None in v1** (no downloads, clipboard, per-app storage or media streaming through the broker). Each is new attack surface. Revisit when an app needs one; iOS downloads have no path until then. |
| **D7** | A confirmation is needed and no shell is connected | **Fail the call.** Do not hold it. |
| **D8** | Full-trust app UIs | **Never supported.** Anything that needs operator-wide power becomes a core panel, like the terminal. |
| **D9** | Code changing after approval | An approval must cover the code as well as the manifest. App UIs are embedded in the app binary (rust-embed), so covering the binary covers the UI. When install signing lands (APP-SUITE §0.4 lists it as out of scope today), the signature covers the binary and feeds the fingerprint. Until then this threat stays open (§7.2). |

## 3. The design in one paragraph

App UIs run in a **sandboxed iframe with an opaque origin**. They never see
the operator's cookie, cannot open `/ws`, and cannot reach the network
(`connect-src 'none'`). Each frame loads its own assets through a
**capability path** that authorizes nothing but GETs of that one app's bytes.
Everything else goes through a **MessagePort broker in the shell**. The
broker forwards calls over the shell's own socket, tagged with the app's
identity, and the **server** enforces that app's approved scope plus a
hard-deny list. **Confirmations are tickets held by the server.** A ticket
never reaches the frame, and the prompt is drawn by the shell, naming the
app and the exact target.

## 4. The design

`core` means `crates/deckjack-core/src`.

### 4.1 The frame, in two layers

- `AppFramePanel.vue`: `<iframe sandbox="allow-scripts allow-forms" allow=""
  referrerpolicy="no-referrer" src="/apps/<id>/~<cap>/">`.
  - It never gets `allow-same-origin`, `allow-top-navigation*`,
    `allow-popups`, `allow-modals` or `allow-downloads`.
  - `allow-forms` is kept so the phone keyboard's Go button still submits
    inside the app. `form-action 'none'` stops any real POST. This
    combination is inferred from the HTML spec and must be checked on
    devices (§8).
- Every `/apps` response also carries the CSP `sandbox allow-scripts
  allow-forms` directive. A document opened top-level, or served under a
  content type the app chose, is therefore still opaque.
- The theming that reaches into the frame's `contentDocument` is deleted.
  The theme travels in the broker's hello and theme-change messages instead.

### 4.2 Capability path for assets (`apps.rs` UI router, `wiring.rs`)

- A new shell command, `shell.app.frame {app}`, mints the cap. Only browser
  Operator issuers may call it: not agents, not app UIs.
- The cap is 128 random bits held in memory, mapped to (session hash, app,
  fingerprint). It is stable for the session, so the HTTP cache survives
  window reopen and PWA resume.
- It is revoked on logout, session expiry, lockdown, `system.app.revoke`, a
  fingerprint change, and an app process restart.
- `/apps/{app}/~{cap}/{*path}` ignores the cookie. It sits outside
  `require_session` but inside the lockdown gate. An unknown, stale or
  other-app cap returns 404. The old `/apps/{app}/` returns 404 too, so
  nothing can fall back to cookie authority.
- The cap grants GETs of that app's bytes and nothing else, so leaking it
  gives nothing away.
- **Headers set by the core that the app cannot override.** Every
  response on this route carries them, including 404s and redirects:
  - `Content-Security-Policy: sandbox allow-scripts allow-forms; default-src
    'none'; script-src {O}/kit/ {O}/apps/{app}/~{cap}/; style-src {same}
    'unsafe-inline'; img-src {same} data: blob:; font-src {same}; media-src
    {same} blob:; connect-src 'none'; worker-src blob:; frame-src 'none';
    form-action 'none'; base-uri 'none'; frame-ancestors {O}; webrtc 'block'`
    - `{O}` is the configured public origin. There is no `'self'`, because
      what it means inside an opaque document is unsettled.
    - `webrtc 'block'` is ignored by every shipping browser today (§7.2) and
      is sent so it takes effect when engines implement it.
  - `Permissions-Policy: publickey-credentials-get=(),
    publickey-credentials-create=(), camera=(), microphone=(),
    geolocation=(), payment=(), usb=(), serial=(), hid=(), clipboard-read=(),
    clipboard-write=(), fullscreen=(), display-capture=()`
  - `Referrer-Policy: no-referrer`
  - `X-Content-Type-Options: nosniff`
  - `X-DNS-Prefetch-Control: off`
  - `Cache-Control: private, max-age=31536000, immutable` for immutable
    assets, otherwise `no-store`. This fixes today's `public`.
  - `Access-Control-Allow-Origin: *`, without credentials, on JS, CSS,
    fonts and module preloads. These are CORS-mode loads from origin `null`.
  - No CORP header, because it would block the frame's own no-cors loads.
- `WireAsset` forwards only `content_type`, never `Set-Cookie` or any other
  app-chosen header. That is a tested invariant, because forwarding headers
  would turn into session fixation.

### 4.3 `/kit/` becomes public

- Add `/kit/` to `is_public_install_asset`, matched **after** path
  normalization, so `/kit/../` and `%2e%2e` variants are refused. Serve it
  with `ACAO: *`. It is static library code, not a secret.
- The test asserting that the kit is not public flips, deliberately.

### 4.4 Inline scripts (D4)

Each app's inline `<script type="module">` moves into a file, such as
`ui/main.js`, for Files, Notes, Tasks and the editor.

### 4.5 The broker (`frame-bridge.ts` becomes `broker.ts`; `kit/bus.js`, `kit/shell.js`)

- **Handshake:**
  1. The kit posts `deckjack.ready` to `parent`.
  2. The shell accepts it only if `event.source` is the `contentWindow` of
     a mounted panel and `event.origin === "null"`.
  3. **Identity comes from the panel's window record, never from the
     message.**
  4. On every frame `load`, the shell creates a fresh `MessageChannel`,
     closes the old port, rejects that port's pending calls, and posts
     `{type: "deckjack.hello", app, theme}` with targetOrigin `"*"`,
     transferring one end of the channel.
  5. The kit accepts a hello only from `event.source === window.parent`, so
     a sibling frame cannot plant a port.
- **Operations:**
  - `invoke` and `cancel`;
  - `subscribe` and `unsubscribe`, limited to the topics the app declared,
    exactly, with no wildcard outside its own scope;
  - `shell`, with an allowlist of `shell.quicklook.open` and
    `shell.app.open`.
- **Bookkeeping:** call ids are local to the frame and mapped to socket ids.
  `cancel` and progress events reach only that frame's own calls.
- **Limits:** 32 calls in flight per frame, 1 MiB of params, and a
  token-bucket rate limit, including on `shell.app.open`.
- **The kit keeps its API.** `bus.invoke`, `bus.on`, progress events and
  the shell helpers stay as they are; only the transport changes, from a
  WebSocket to the port. Opened with no parent, the kit fails with a clear
  message.
- **When the shell socket drops:** pending brokered calls reject with
  `disconnected`. The port stays alive and works again after `/ws`
  reconnects.
- **`shell.app.open` exists today** (Lane 2; `open-app.ts`,
  `frame-bridge.ts`) and becomes a broker `shell` op under these rules:
  - targets are installed app ids only, never core panels such as the
    terminal;
  - it carries no authority from the caller;
  - the opened app receives `?path=` as inert data and must not act on it
    without a user gesture;
  - the path is checked against the *caller's* file scope.

  `shell.quicklook.open` gets the same path checks, for its path and
  `siblings`.

### 4.6 Server identity and scope (`ws.rs`, `wiring.rs`, `command.rs`)

- The invoke frame gains `as_app?: "<name>"`, which the server accepts only
  from browser Operator sockets. A call carrying it gets a new
  `Issuer::AppUi { app, session }` as its issuer, never `Operator`.
- **An app's own scope** is exactly the command ids its process registered
  in its handshake. It is **never** the string prefix `<name>.*`. The
  difference matters: an app named `files` must not inherit the core's
  `files.*`.
- **Anything beyond its own scope** comes only from `[ui] invoke = [...]`
  in the *approved, stored* manifest:
  - each entry is an exact command id;
  - a wildcard `ns.*` is allowed only inside the app's own scope, because a
    wildcard on a core namespace would silently pick up commands added
    later.
- **`HARD_DENY`** is one constant in the protocol crate: `terminal.*`,
  `system.*` (D5; this includes lockdown and recovery codes), `shell.*`
  (shell operations go only through the broker's `shell` op), `auth.*` and
  `seat.*` (which device is using the workspace; DESIGN.md "The single
  seat"; an app UI's own `/kit/bus.js` socket, which this list does not
  see, is refused the seat's commands by the seat itself).
  It applies to commands **and to event topics**. It is checked in four
  places:
  1. manifest validation, so a manifest naming a denied id or topic fails
     with a message;
  2. the broker;
  3. the server's dispatch;
  4. the server's event fan-out.
- **Reserved names:** every core namespace (`files`, `git`, `terminal`,
  `system`, `shell`, `auth`, `editor`, and any future one) is refused both
  as an app name and as a command prefix in `validate`. The list is kept in
  the same constant file.
  - The in-tree Files app is the one casualty. **Its app id is renamed**
    (for example `filer`; its title stays "Files"), and its use of
    `files.*` is declared in `[ui] invoke`, which is what makes D1 true for
    it.
  - Lane 2's editor already puts its commands in the core (`editor.rs`), so
    the `deckjack-editor` app declares `[ui] invoke` for the `editor.*` and
    `files.*` ids it uses.
- `approvals::operator_only` rejects AppUi as well as Agent.
- The audit log records `app-ui:<name>` and the session hash, so the
  timeline can tell an app frame from the shell.
- The shell is trusted to name `as_app` truthfully. That is acceptable: the
  shell is the trust root (§7.2).

### 4.7 Confirmation tickets, for every client

> **Status: stage 1 built.** `crates/deckjack-core/src/ticket.rs` holds the
> tickets; `wiring::BusDispatch` issues and redeems them; `ws.rs` refuses an
> invoke carrying `confirmed` (error code `confirmed_removed`, no protocol
> bump) and carries `confirm` frames; `Command::subject` renders the target
> and `command::plain_text` strips and truncates it; `bus.ts` redeems, and
> `ConfirmDialog.vue` shows the target as text. Tests: `tests/wiring.rs`
> (the "Confirmation tickets" section), `tests/ticket.rs`, `tests/ws.rs`,
> `web/src/stores/bus.test.ts`, `web/src/components/ConfirmDialog.test.ts`.
>
> **Deviation, with reason:** the `confirm` frame is
> `{type: "confirm", id, ticket, command, params}`, not `{type, id,
> ticket}`. The server keeps only the binding (socket, issuer, command id,
> SHA-256 of canonical params), not the params themselves, so the client
> resends the call and the digest must match. Holding the params instead
> would let one socket park up to a frame's worth (256 KiB) per unredeemed
> ticket. Tickets are consumed on any presentation, matching or not, and one
> socket may hold at most 16 unredeemed (oldest dropped).
>
> **Addition beyond the design:** a ticket is also bound to the target text
> it showed. On redemption the target is rendered again from the same
> params and must match, so "empty everything (1 item)" cannot be confirmed
> after more has landed in the trash (refusal: `TargetChanged`). The window
> between redemption and the command running is not covered, and a change
> the target text does not show (one item swapped for another, same count)
> passes.
>
> **Which prompts name a target:** only commands that are ever confirmed
> carry one: `files.trash.empty` (paths, or the item count),
> `system.app.approve`/`revoke` (the app), `system.grant.issue` (agent,
> nodes, lifetime, project, then patterns broadest first with "and N more",
> sized to fit uncut). `files.delete`, `files.trash` and
> `files.trash.restore` are `RiskLevel::Write` and never asked about, so
> they have no subject.
>
> **Not built here (later stages):** the prompt's app id and icon, the
> 500 ms arming delay, dimming frames, per-app rate limits and "Stop this
> app" (stage 4); targets for app-declared (`DynCommand`) commands, which
> have only a schema and so show their title alone.

- `confirmed: bool` is removed from the invoke frame, and a frame that
  still sends it is rejected.
- `NeedsConfirmation` returns `{status: "needs_confirmation", prompt, risk,
  ticket}`. The ticket is 128 random bits, single-use, with a 60-second
  lifetime. It is bound to:
  - the socket id;
  - the issuer, including the app;
  - the command id;
  - the SHA-256 of the canonical params.
- A `{type: "confirm", id, ticket}` frame redeems it, only on the socket
  that received it.
- **The prompt names the target, rendered by the server from the validated
  params.** Example: "Filer asks to move to the trash: `thesis/` (1,204
  items)". The target text has bidi and control characters stripped, is
  truncated in the middle, and is displayed as plain text. A prompt made
  from the command title alone is what made bait-and-switch work.
- **Drawn by the shell, above every frame:**
  - all app iframes get `pointer-events: none` and are dimmed while the
    prompt is open;
  - the prompt shows the app's id and icon next to its title;
  - titles are capped in length, and a title that imitates "Deckjack" or
    the shell is refused at validation;
  - the Confirm button arms only for a press that *starts* after about
    500 ms, and re-arms if the prompt is laid out again;
  - one prompt is shown at a time, confirmations are rate-limited per app,
    and the prompt offers "Stop this app".
- The ticket never crosses the port. The CLI path is unchanged. Agents are
  unchanged: their calls still queue in the Inbox.

### 4.8 Manifest and fingerprint (`crates/deckjack-protocol/src/lib.rs`)

- New fields: `[ui] invoke` and `[ui] events`.
- The fingerprint already switched to SHA-256 over a canonical encoding
  (Lane 0.4, merged 2026-11-16, which costs a one-time re-approval). It
  must also cover `[ui]`, ideally in the same release, so operators
  re-approve once and not twice.
- `Needs::is_empty` treats any `[ui] invoke` entry as a need.
- `summarize()` lists the UI scope grouped by the highest risk it reaches,
  for example: "Its window can, on your behalf: move files to the trash
  (files.trash)".

### 4.9 A gate on cookie routes (`http.rs`, as a middleware layer)

- It applies to every cookie-authenticated route: `/`, shell assets, `/ws`,
  `/content`, `/passkey/*`, `/logout` and `/api/*`.
- It returns 403 when:
  - `Origin` is `null`;
  - `Sec-Fetch-Site` is `cross-site` or `same-site`, unless
    `Sec-Fetch-Dest` is `document` (so a top-level navigation still
    reaches `/login`);
  - `Sec-Fetch-Site` is **missing** on `/ws`, `/content`, `/passkey/*` or
    `/logout`. It fails closed.
- `/content` and the cap route refuse a `Sec-Fetch-Dest` of `script`,
  `worker`, `sharedworker`, `serviceworker` or `style` when the request
  comes with the shell's cookie. That way neither working-directory files
  nor app bytes can be loaded as shell script.
- Nothing in the design depends on browsers withholding SameSite=Strict
  cookies from sandboxed frames. WebKit's behavior there is unverified.

**Built in stage 2** (`core/fetchgate.rs`, mounted once in `wiring::app`
inside the lockdown gate). The gate covers every route except a closed
exempt list with no cookie authority: `/login`, `/unlock`, `/healthz` and
the public install assets. A route added later is gated by default.
Deviations, each with its reason:

- **`/ws` does not fail closed on a missing `Sec-Fetch-Site`.** Measured on
  this machine: Chromium 155 sends no Fetch Metadata at all on a WebSocket
  handshake. Firefox 158 sends `Sec-Fetch-Site`. WebKit bug 237550 records
  the same split. Failing closed would lock every Chrome and Android browser
  out of the shell. At `/ws` only, a missing `Sec-Fetch-Site` passes if
  `Origin` names the same host as `Host`. No script can forge `Origin`, and
  a sandboxed frame sends `null`, which is refused first. A handshake with
  neither header is refused.
- **A missing `Sec-Fetch-Site` on `/content`, `/passkey/*` and `/logout`
  passes on a node served over plain HTTP at a non-loopback address.**
  Browsers send Fetch Metadata only to potentially trustworthy URLs
  (`https`, `localhost`, loopback); measured with Chromium against
  `http://<tailscale ip>`, no `Sec-Fetch-*` arrives on the document, an
  `<img>` or a `fetch`. Failing closed there would lock the shell out of
  Quick Look, sign-out and the passkey page on a supported `--listen`
  deployment. The exception applies only when `Host` is not loopback and
  nothing on the request (`Origin`, `Referer`, `X-Forwarded-Proto`,
  `Forwarded`) says `https`, and an unsafe method must still carry a
  non-`null` `Origin`. Those signals only make the gate stricter, so they
  need no proxy trust. On such a node the destination checks cannot run
  (the browser does not say the destination); HTTPS gets the whole gate.
- **The `document` exception is for navigations only.** §4.9 lets a
  cross-site or same-site request through when `Sec-Fetch-Dest` is
  `document`; the gate also requires `GET` or `HEAD`, so a sibling host's
  auto-submitted top-level form `POST` to `/logout` or `/passkey/*` is
  refused by the gate and not only by the handler's `Origin` check.
- **The script-destination refusal is on `/content` only, not on the app
  asset routes yet.** Today's app frames are same-origin and load their own
  JavaScript from `/apps/<name>/` with the cookie, so refusing `script`
  there would break every app UI. It belongs on stage 4's cap route. Until
  then `/apps/*` refuses only `serviceworker`.
- **The refused destinations are a superset.** `/content` also refuses
  `audioworklet`, `paintworklet`, `xslt`, `object` and `embed`. Nothing
  legitimate loads a file that way.
- **Non-browser clients.** Nothing in the product calls a cookie route
  except the shell. The CLI uses the working directory or `/unlock`
  (exempt), and agents use the MCP socket. A `--debug` script sends
  `Sec-Fetch-Site: same-origin` (README, startup banner).
- **For stage 4:** the cap route and the public `/kit/` are loaded from an
  opaque origin (`Origin: null`, `Sec-Fetch-Site: cross-site`, measured in
  Chromium and Firefox). They must be added to `fetchgate::is_exempt` when
  they land, or the gate refuses them.
- **Unverified on device:** WebKit's Fetch Metadata on iOS. WebKit has had
  wrong values before (bugs 256472 and 260284). One iOS 26 report shows a
  `cross-site` WebSocket handshake whose page origin is unknown. Every
  refusal is logged at `warn` with the site, dest, mode and origin, so a
  lockout can be diagnosed from the host.

### 4.10 The shell document (`shell.rs`)

- `Content-Security-Policy: default-src 'self'; script-src {O}/assets/
  {O}/kit/; worker-src {O}/sw.js; style-src 'self' 'unsafe-inline';
  connect-src 'self' wss://{host}; frame-src {O}/apps/ {O}/content; img-src
  'self' data: blob:; object-src 'none'; base-uri 'none'; form-action
  'self'; frame-ancestors 'none'`
  - `script-src` and `worker-src` are **pinned by path**, never `'self'`.
    Under `'self'`, an HTML injection in the shell could run a file served
    by `/content` or an app's cap path, or register a service worker from
    one.
- `X-Frame-Options: DENY`. `/login` and the passkey page also get
  `frame-ancestors 'none'`. (`frame-ancestors 'self'` does not stop
  framing by a same-origin top-level page, so it is not used.)
- `frame-src` is the exfiltration jail: a frame that navigates itself
  somewhere outside is blocked. The shell's `securitypolicyviolation`
  listener tears that frame down with "<app> was stopped: it tried to leave
  its window".
- **Unverified:** whether the Vue build emits any inline script. If it
  does, this stage has to change.

### 4.11 File scope and deletes

- For `Issuer::AppUi`, **every path parameter** must fall under the app's
  declared file scope:
  - sources and destinations;
  - bulk lists;
  - `siblings`;
  - Quick Look and `shell.app.open` paths;
  - `files.*`, trash, transfer, thumbnail, archive, editor and content
    paths.

  The check runs on the canonical path after `Namespace` resolution, so the
  `.deckjack/` denial still applies.
- That resolution is backed by `openat` / `O_NOFOLLOW`, so the app's own
  process (which can write inside its scope) cannot win a race that swaps a
  directory for a symlink between check and use. This has not been audited
  yet.
- D3: a `files.delete` from an AppUi is rewritten to `files.trash`.

### 4.12 A second socket for app traffic

The shell opens a second `/ws` socket for brokered app calls. That way a
16 MiB `files.read` on a phone link cannot stall terminal echo or the
shell's own calls. A confirmation ticket for a brokered call is redeemed on
this second socket.

## 5. Staged migration

Each stage lands green with its own tests. **Plainly: stages 0–3 close no
attack from an app UI.** Until stage 4 the frame is same-origin and can
script the shell, which means it can drive the shell's own confirmation
path. The earlier stages make stage 4's guarantees possible and fix flaws on
the shell side.

| Stage | Contents | Why it can ship alone | Size (est.) |
|---|---|---|---|
| **0. Proxy trust (D2)** — **BUILT** | Validation refuses any grant that reaches the core's listen port. The core listens on a Unix socket for `tailscale serve unix:/…` and trusts only that listener. `__Host-` cookie. <br><br>**As built.** *Config:* `[server] unix_socket = "proxy/http.sock"` (relative to the install directory; empty = off), `trusted_proxies = []`, and `[passkey] rp_id`/`origin` set to the node's tailnet name before any passkey is enrolled (left empty on loopback they default to `localhost`, which `https://<node>.ts.net` cannot use); then `sudo tailscale serve --bg unix:$HOME/.deckjack/proxy/http.sock` (the banner prints the resolved path). A path inside `<install>/run` is refused at startup: app sockets there were `<app>.sock` and launching an app removed whatever was at its path, so an app named `proxy` would have taken over `run/proxy.sock` (apps no longer have sockets there since lane 10; `run/` holds the core's MCP sockets, and the rule stays). *Merge note:* `proxy/` is an entry of `INSTALL_LAYOUT`, so the socket and its `.lock` are never listed, moved or granted through Files, the file commands, `/content` or a manifest's `working_dir`; and a `unix_socket` inside the install directory but not under `proxy/` is refused at startup, because anywhere else there it would be an ordinary file an app granted that folder could replace (`tests/install_layout.rs`, `config.rs` tests). *Listener* (`proxy.rs`): an exclusive `flock` on `<path>.lock` is held for the node's life, so two nodes sharing an install directory cannot both find the path empty and both rename into it (the loser gets "in use"); then the socket is bound under a temporary name, set to 0600 and renamed into place, in a parent created 0700. An existing socket is probed: one that answers is a live node and is left alone (startup fails, "in use"), one that refuses is a crashed node's and is replaced, and anything that is not a socket is never deleted. On clean shutdown the file is removed only if it is still this process's inode. Requests off it carry the peer `[::]:0`, which no TCP connection can have, and `http::Ctx` trusts that and listed `trusted_proxies` only. *Deviation 1, peer uid:* the file mode alone does not keep apps out. Apps run as the operator's uid, and Landlock v4 does not restrict `connect` on a pathname Unix socket (apps already connect to their own socket in the same `run/`). So every accepted connection is checked with `SO_PEERCRED` against `unix_socket_peer_uid`, default 0. tailscaled runs as root (`CAP_DAC_OVERRIDE` passes the 0600 mode); no app does. *Since lane 10 (docs/lanes/10-sandbox-all-apps.md):* every app is held to Landlock's `ResolveUnix` (ABI 9) with no path granted and reaches the core over an inherited socket pair, so an app can no longer connect to this socket at all; the peer-uid check stays as the second wall. *Deviation 2, Host:* `tailscale serve` sends `Host: localhost` to a socket backend (`ipn/ipnlocal/serve.go`), which would fail every Origin check. On this listener only, `Host` is restored from `X-Forwarded-Host`. Its proxy is Go's `ReverseProxy` with `Rewrite`, which drops client-sent `X-Forwarded-*`. *Deviation 3, TCP stays:* the socket is in addition to TCP, not instead of it. `deckjack unlock` and debug mode talk to loopback TCP, and nothing arriving there is believed unless listed. *Grant rule* (`apps::reaches_core`, checked by `Host::approve` and at every `Host::launch`, so a grant stored before the rule cannot slip through; `Host::new` takes an `apps::CoreDoor` — listen address plus `trusted_proxies` — as a required argument): refuse (1) `connect_tcp` containing the core's port, on any listen address, because Landlock port rules ignore addresses; (2) the legacy whole-network grant (hosts, no ports) when the core listens on loopback or the unspecified address (IPv4-mapped spellings included), **or when `trusted_proxies` names any address**. That app is under no TCP rule, so it can bind any address the machine owns as its source and connect from it: loopback, or the core's own tailnet address when a TCP `tailscale serve` is pointed there and trusted. Which addresses are local cannot be known at approval (a tailnet address comes and goes with tailscaled), so every trusted address counts. A core bound to one non-loopback address with nothing trusted over TCP (the socket setup) admits it. *Sandbox gap closed with it:* Landlock does not mediate MPTCP (upstream treats it as a separate protocol by design), and an MPTCP socket aimed at a TCP listener falls back to TCP, so on this kernel a process allowed no TCP port connected to loopback anyway. The sandbox now refuses `socket(_, _, IPPROTO_MPTCP)` with `EPROTONOSUPPORT` (what a kernel without MPTCP says, so programs fall back to TCP and meet Landlock) and denies `io_uring_setup`/`enter`/`register`, whose `IORING_OP_SOCKET` would go around a seccomp argument filter. TCP Fast Open (`sendto(MSG_FASTOPEN)`) was checked on 7.2.15 and is already refused by Landlock. `bind_tcp` is not checked because the core already holds its port. The protocol validator cannot apply this: it does not know the listen address. *Banner:* warns when `trusted_proxies` holds an address local processes connect from — loopback in either spelling (`::ffff:127.0.0.1`), or the core's own listen address — while any stored app grant names hosts or `connect_tcp` (`Store::app_grants`). *Cookie:* `__Host-dj_session` whenever `Secure` is set (HTTPS known via a trusted hop), else `dj_session`; `from_cookie_headers(secure, …)` reads only the name that matches the request. Sign-out clears the same name. *Tests:* `tests/proxy.rs` (the real `deckjack --debug` binary with `unix_socket` set, holding `main.rs`'s wiring to the same answers and checking removal on SIGTERM; real Unix and TCP sockets: attribution by the forwarded address over the socket and not over TCP, cookie name and strictness, Host restore, wrong-uid connection dropped, 0600/0700, stale vs. live, two nodes racing for one path, non-socket, successor's socket), `tests/apps.rs` D2 block, `tests/sandbox.rs` (MPTCP and io_uring refused in a confined process, against live listeners), unit tests in `proxy.rs`, `session.rs`, `config.rs`. Every one was mutation-checked. *Verified live:* debug binary, curl over the socket: sign-in sets `__Host-dj_session; Secure`, the session is refused from another forwarded address, `/ws` upgrades with 101 (403 without `X-Forwarded-Host`), the socket is removed on SIGTERM, a SIGKILL leftover is replaced, and the warning prints. *Not verified:* a real `tailscale serve` connecting as root. The uid and DAC reasoning above is inferred, not observed. | Fixes the forgery risk created by trusting 127.0.0.1 as a proxy. Independent of everything else. | ~150 |
| **1. Confirmation tickets** — **BUILT** (see §4.7 status) | Invoke frame, socket adapter, `Confirm` carrying a ticket, `bus.ts` redeeming it, `confirmed` rejected, the target rendered into the prompt. | The shell's flow keeps its shape. It removes the client-asserted bit for every client. | ~300 Rust, ~60 TS |
| **2. Gates and headers** — **BUILT** | The cookie-route gate (§4.9); `frame-ancestors 'none'` and XFO; `private` caching on app assets. **Built:** `core/fetchgate.rs`, one layer in `wiring::app`; `frame-ancestors 'none'` + `X-Frame-Options: DENY` on the shell document, `/login` (and its locked/wrong-code pages) and the passkey pages; app assets `private, max-age=31536000, immutable` or `no-store`. Deviations in §4.9. | Purely defensive. No client changes. | ~150 |
| **3. Identity and scope on the server** — **BUILT** | `[ui]` fields, `HARD_DENY` and reserved names in validation, `Issuer::AppUi`, `as_app`, audit attribution, `summarize` lines, `Needs::is_empty`. The Files app id is renamed and given `[ui]`; the editor gets `[ui]`. **Built (2026-11-16), see the note below the table.** Merged with stage 1: `as_app` rides on the `confirm` frame too, and the confirmation ticket is bound to the `AppUi` issuer it names (§4.7). | Testable entirely from Rust. Nothing sends `as_app` yet. Every app re-approves once. | ~450 |
| **4. The flip** (one change) | `shell.app.frame` and the cap route; app response headers; public `/kit`; inline scripts moved out; sandboxed iframe; broker plus prompt attribution; kit transport over the port; theme by message; old `/apps/{app}/` returns 404. | **The first stage that closes app-UI attacks.** | ~350 Rust, ~550 TS |
| **5. Shell CSP** | §4.10 and the violation teardown. | Separate because it can break the Vue build. | ~80 |
| **6. Narrowing** | AppUi file scope over every path parameter; `openat`-based resolution; D3's rewrite to the trash; the second socket. | Each piece is independent. | ~300 |

Total: about 1.8k lines of production code and 1.2k lines of tests.

**Stage 3 as built.** Where it lives:

- `deckjack-protocol`: `UiNeeds` (`[ui] invoke`, `[ui] events`), `HARD_DENY` +
  `is_hard_denied`, `RESERVED_NAMESPACES` + `is_reserved_namespace`, all
  checked in `Manifest::validate`; the fingerprint sorts and covers `[ui]`.
- `command.rs`: `Issuer::AppUi { app, session }`, `Refused::NotPermitted`,
  and `Bus::check_app_ui` at both public doors (`dispatch` and
  `dispatch_approved`). The own scope is `Registry::provider` (who registered
  each id at handshake, via `Registry::register_provided`); the approved list
  is `Registry::app_ui_invoke`, set by `AppHost` at handshake from
  `Host::approved_ui_invoke` (the stored grant, only while its fingerprint
  matches the installed manifest) and cleared when the connection ends or
  the operator revokes the app (`AppHost::revoke` drops its commands and
  scope at once, whether or not the process obeys the stop).
- `ws.rs` / `wiring::BusDispatch`: `as_app` on the invoke frame becomes
  `Issuer::AppUi`; a name that is not a valid app name is refused before the
  bus; refusals by scope go out as `not_permitted`. Merged with stage 1:
  `as_app` is also carried on the `confirm` frame, the issuer is decided
  before any ticket is looked at, and the ticket's binding holds that issuer,
  so a ticket issued for app A's window is refused when redeemed as the
  shell's or app B's, and a shell ticket is refused when redeemed under any
  `as_app` (`tests/wiring.rs`, "Tickets and `as_app`", mutation-checked).
- Audit: `events::Issuer::AppUi`, labeled `app-ui:<name>`, with the browser's
  session tag in `issuer_session`. `approvals::operator_only` refuses AppUi.
  `system.timeline` filters by `issuer: "app_ui"` (with `session` naming one
  app).
- Tests: `deckjack-protocol/tests/ui_scope.rs`, `deckjack-core/tests/appui.rs`
  (real handshake, real approval store, real file commands), the `[ui]`
  section of `tests/apps.rs`, and the socket tests at the end of
  `tests/end_to_end.rs` (including that every namespace the product's
  registry holds is reserved).

Deviations, each for a stated reason:

- **`[ui]` is held in `Needs::ui`** although `app.toml` writes it as a
  top-level table (`[needs.ui]` is refused). That is what lets
  `Needs::is_empty`, the stored approval (the `needs` JSON column) and the
  fingerprint all cover it without a second struct each must remember.
- **`[ui] events` also counts as a need**, not only `invoke`: a window that
  listens to `files.*` events learns every file name.
- **The editor app is renamed too, to `scribe`** (title still "Editor").
  `editor` is a core namespace (`editor.rs`), so the reserved-name rule this
  stage adds refuses it as an app name; §4.6 named Files as the only casualty,
  which missed this. Files is `filer`.
- **Crate directories keep their names** (`crates/deckjack-files`,
  `crates/deckjack-editor`); the packages and binaries are
  `deckjack-filer` / `deckjack-scribe`, and `xtask`'s `find_apps` now reads
  the app name from `app.toml` instead of the directory. Renaming the
  directories would have broken every concurrent lane's patch to those apps.
  `xtask install`/`dev` removes an `apps/files/` or `apps/editor/` left from
  before the rename (through `deckjack uninstall`) when it installs `filer` or
  `scribe`. Their old stored approvals stay in the database, inert: no app
  may take those names again. A browser's saved dock or window state that
  names `files`/`editor` is not migrated.
- Event fan-out filtering by `[ui] events` is not built: there is no per-app
  subscription to filter until the broker (stage 4). The topics are validated.

## 6. Tests that prove it

Every refusal is paired, in the same fixture, with a positive control that
passes first. Each refusal asserts its specific error code (`not_permitted`,
not `bad_command`) and checks the effect on disk, not just the status. Each
gate gets a mutation: remove it, and at least one test must go red.

**Rust** (`crates/deckjack-core/tests/`, on a scratch port and directory):
- **Tickets:**
  - a raw `confirmed: true` frame is rejected and the trash is unchanged on
    disk;
  - a correct redemption empties it;
  - reuse, altered params, another socket, and a call after 61 s (injected
    clock) are each refused;
  - the prompt contains the target path, with bidi characters stripped.
- **Gate:** `/ws`, `/content`, `/passkey/credentials/remove` and `/logout`,
  each with a valid cookie, are refused:
  - under `Origin: null`;
  - under `Sec-Fetch-Site: cross-site`;
  - with `Sec-Fetch-Site` missing.

  The credential still exists afterwards. The control, sent with
  `Origin: https://H` and `same-origin`, succeeds.
- **Scope:**
  - via notes, `notes.list` succeeds;
  - `terminal.open` via any app returns `not_permitted`, no pty is spawned,
    and a manifest naming it fails validation;
  - an app named `files` or `git` fails validation;
  - `system.recovery_codes` via any app is refused, and the codes are
    unchanged;
  - `files.read` is refused without `[ui]` and succeeds with it;
  - a wildcard grant does not pick up a core command registered later;
  - an `auth.*` topic is refused;
  - the audit log records `app-ui:<name>`.
- **Cap route:**
  - with only a cookie, `/apps/notes/` returns 404; the control mints a
    cap, and `/apps/notes/~cap/` returns 200 with no cookie;
  - a cap minted for notes returns 404 under `/apps/tasks/`;
  - the cap is refused after logout, lockdown, revoke, a fingerprint change
    and a process restart, each case with a control;
  - headers are asserted on HTML, assets and 404s alike: `sandbox
    allow-scripts` without `allow-same-origin`, `connect-src 'none'`, never
    `public`, no CORP, ACAO on JS, CSS and fonts;
  - `/kit/bus.js` with no cookie returns 200, and `/kit/%2e%2e/index.html`
    does not.
- **Fingerprint:** changing only `[ui] invoke` blocks launch until the app
  is re-approved.
- **D2:** a manifest granting the core's listen port fails validation. A
  forged `X-Forwarded-For` over loopback TCP is not trusted once the Unix
  listener exists.

**Browser** (Playwright, Chromium **and** WebKit; a hostile "probe" app
reports through the broker). Controls run first: the probe's own echo works,
its declared `files.list` works, the theme arrives, and Quick Look opens.
Then:
- `parent.document` throws, `localStorage` throws, and service worker
  registration rejects.
- `new WebSocket('/ws')` fails, and the server sees no upgrade.
  `fetch('/content?…')` rejects, and the server's hit count is unchanged.
- A `fetch` and an `<img>` aimed at a second test server are blocked, and
  that server's counter stays at 0.
- `navigator.credentials.get` raises NotAllowedError, and a virtual
  authenticator records no ceremonies.
- `top.location` fails and `window.open` returns null. Setting
  `location.href` to the second server triggers a frame-src violation and
  a teardown, and that server's counter stays at 0.
- A fake hello planted into a sibling frame is ignored. A frame `cancel`
  of a shell call id does nothing.
- The confirmation prompt appears in the shell document as "Probe asks to:"
  and names the target. The probe's fake confirm message has no effect.
  Cancel leaves the file on disk; Confirm moves it to the trash.
- The shell refuses `<script src="/content?path=x.js">` and a script from
  `/apps/…/~cap/x.js`.
- No `Cookie` header arrives on any request the frame initiates. If WebKit
  does send one, the test proves the cookie grants nothing.
- A WebRTC data channel **documents the leak** (§7.2); the test does not
  pretend it is blocked.
- A cross-origin attempt to steal focus from a terminal panel, run on
  WebKit.
- **Non-vacuity rerun:** the same suite runs with the sandbox attribute and
  the CSP sandbox header removed. The attack rows must then fail, and CI
  asserts which subset fails.

**Performance:** brokered vs direct `files.read` at 1, 8 and 16 MiB (p50
and p95), and terminal echo latency during a 16 MiB brokered read, with and
without the second socket.

**On device** (unverified until run): the iOS PWA and the Fold PWA.
- Open Files and Notes, type into an input, and delete through the shell's
  prompt.
- Switch the theme.
- Do a passkey step-up from the shell.
- Survive an OS kill and resume.
- Record the engine versions.

## 7. What it closes, and what it does not

### 7.1 Closed for app UIs after stage 4

- **Any bus command, including the terminal:** the frame has no socket,
  `connect-src` is `'none'`, `/ws` refuses `Origin: null`, and `terminal.*`
  is hard-denied.
- **Skipping confirmation:** tickets live on the server, are bound to one
  socket and one set of params, and never reach the frame.
- **Self-approval, grant issuance, Inbox approval, recovery codes, lockdown
  and revocation:** all hard-denied (`system.*`). `/logout` is refused by
  the gate.
- **Passkey backdoors:** there is no cookie, `/passkey/*` is gated, and
  WebAuthn is disabled for the frame.
- **Controlling the shell's DOM and forging confirmations:** the parent is
  cross-origin. This is what makes a prompt drawn by the shell mean
  anything.
- **Service worker and shared storage:** opaque origins get neither.
- **Interfering with another app:** caps are bound to one app, and
  identity comes from the panel.
- **Eavesdropping on events:** only declared topics are forwarded, and
  `auth.*` and the other hard-denied topics never are.
- **Inheriting a core namespace by name:** core names are reserved, and an
  app's own scope is what its process registered.
- **Attribution:** the audit log records `app-ui:<name>`.
- **Files launching unapproved:** `[ui]` counts as a need, and the app id is
  renamed.

### 7.2 Remains, stated plainly

1. **Authority inside the approved scope is real authority.** A UI granted
   `files.write` can silently overwrite anything in its file scope. The
   file scope (stage 6) is the only limit.
2. **Code can change after approval** until install signing covers the
   binary (D9). The UI bytes come from the app binary at runtime.
3. **WebRTC exfiltration is open.** `connect-src` does not govern
   `RTCPeerConnection`, no shipping browser implements the CSP `webrtc`
   directive, and the sandbox does not disable it. **An app UI can send
   anything inside its read scope to the internet.** DNS prefetch is
   another low-bandwidth channel, and so is timing.
4. **Spoofing inside the frame.** On a phone, a window is nearly
   full-screen, and an app can paint a fake login or a fake prompt. The
   mitigation is design, not enforcement: a persistent strip in the shell
   naming the app, and real prompts always drawn with the shell's chrome
   and the frames dimmed.
5. **Confirming by tap timing.** A ticket proves that the shell drew the
   prompt, not that a human meant to approve it. The arming delay narrows
   this; it does not remove it.
6. **The shell is the single trust root.** An XSS or a supply-chain
   compromise in the shell bundle is total. Stage 5 narrows this, but
   `style-src 'unsafe-inline'` remains.
7. **Gaps on the process side: closed by lane 10**
   (docs/lanes/10-sandbox-all-apps.md). There are no app sockets any more:
   each app inherits one end of a socket pair at descriptor 3, and
   `run/` is granted to no app. So:
   - the whole-`run/` read grant is gone;
   - no SO_PEERCRED check is needed on an app's link, because nothing
     else can connect to a socket pair;
   - there is no app socket path left for one app to squat, and Landlock
     ABI 9 (`ResolveUnix`) refuses an app's connect to any pathname socket
     made outside its own sandbox.
8. **Behavior in the browser engines.** The spike (§8) settled the first
   three, and a fourth found along the way, for Chromium 156 and for
   WebKit 27.2 as Playwright ships it on Linux (WPE, libsoup networking).
   Safari on Apple platforms uses a different network stack, so its cookie
   behavior is still unverified until the device checks run.
   - **WebKit sends Strict cookies from a sandboxed frame. Answered: yes.**
     It sends both SameSite=Strict and Lax cookies on every request the
     frame makes to the parent's origin: images, scripts, styles, fetch,
     its own navigation and WebSockets. Chromium sends none. Both engines
     label every such request `Sec-Fetch-Site: cross-site`, and CORS and
     WebSocket requests carry `Origin: null`. So on WebKit the §4.9 gate is
     load-bearing: it is the only thing between a frame and the cookie.
   - **WebKit enforces the parent's `frame-src` on a child that navigates
     itself. Answered: yes**, in both engines, for another site and for the
     parent's own origin outside the allowed path. Each raises a
     `securitypolicyviolation` in the parent.
   - **CSP host-source paths in `script-src`. Answered: honored** in both
     engines, at top level and inside the opaque app frame of §4.2.
     Directory and exact-file sources both work, and a script outside them
     is refused before it is fetched.
   - **The §4.10 shell policy and Quick Look's PDF viewer. Answered:
     incompatible** in both engines. `worker-src {O}/sw.js` refuses
     pdf.js's module worker at `/assets/`, and `script-src` without
     `'wasm-unsafe-eval'` and `'unsafe-eval'` refuses its WebAssembly and
     its eval probe. §4.10 needs a decision before stage 5 (§8.1).
   - Still open: the soft keyboard and `allow-forms` in an iOS standalone
     PWA, and all of the above in Safari itself.
   - Also: Firefox before 141 ignored the path in `frame-src` for frame
     navigations (CVE-2025-8038). Scheme and host were still enforced, so
     that does not open exfiltration. Firefox was not part of the spike.
9. **Full-trust app UIs cannot exist** (D8). This is intended.

## 8. Before stage 4 starts

- ~~A WebKit spike settling the unverified browser behavior in §7.2, item
  8.~~ **Done 2026-11-16**, results below.
- ~~Confirm that the Vue build emits no inline script (needed for
  §4.10).~~ **Done 2026-11-16**: it emits none, and the shell mounts under
  the §4.10 policy. But Quick Look's PDF viewer does not run under it
  (pdf.js's worker, WebAssembly and eval probe are refused), so §4.10's
  `worker-src` and `script-src` need a decision before stage 5. Results
  below.
- Confirm that CodeMirror needs no `worker-src` beyond `blob:` (inferred;
  not part of the spike).
- ~~Settle the Files app's new id (§4.6).~~ **Done in stage 3**: Files is
  `filer` and Editor is `scribe`.

### 8.1 The browser spike, 2026-11-16

**Where it is.** `web/e2e/spike/`: a throwaway static page set and a small
Node server (`server.ts`), not the core. It runs as the `spike-chromium` and
`spike-webkit` projects of `npm run e2e`. Two listeners make two sites: A
(`http://127.0.0.1:<port>`) plays the Deckjack origin, and B
(`http://localhost:<port>`) is somewhere else. Every request is logged with
`Cookie`, `Origin` and `Sec-Fetch-*`. Each answer is paired with a control
that has to hold first, and the observed answers are pinned per engine, so
an engine update that changes one fails the spike. Every test was
mutation-checked: one change to the page, policy or build, run, watch it
fail, revert.

**Engines.** Playwright 1.64.1. Chromium 156.0.8078.4 (Chrome for Testing)
and WebKit 27.2. Playwright's WebKit on Linux is the WPE port, with libsoup
networking. **It is not Safari:** cookie and fetch-metadata behavior come
from the network stack, and Safari's is Apple's. The WebKit rows below are
WebKit's answers, not Safari's.

**Running WebKit on Arch.** Playwright builds WebKit on Ubuntu 24.04, and it
links ICU 74, `libxml2.so.2` and flite 2.2, which Arch does not ship at
those sonames (Arch has ICU 78 and libxml2 2.15). `npx playwright
install-deps` needs apt and root. `web/e2e/webkit/fetch-compat-libs.sh`
fetches the three Ubuntu packages, pinned by SHA-256, into
`~/.cache/deckjack-e2e/webkit-compat` without root. When that directory
exists, the config launches WebKit through `web/e2e/webkit/minibrowser.sh`,
which adds it to the library path. With that, WebKit runs here headless.
Headed WebKit (GTK) was not tried.

| | Question | Chromium 156 | WebKit 27.2 (WPE) |
|---|---|---|---|
| 1 | A sandboxed frame (`allow-scripts allow-forms`, no `allow-same-origin`) sends SameSite=Strict cookies to the parent's origin? | **No**, for img, script, style, `fetch` (no-cors and CORS), its own navigation and a WebSocket. Lax is not sent either. | **Yes**, Strict and Lax, on every one of those. |
| | `Sec-Fetch-Site` on those requests | `cross-site` | `cross-site` (cookie attached anyway) |
| | `Origin` on CORS and WebSocket requests from the frame | `null` | `null` |
| 2 | The parent's `frame-src {A}/frames/` blocks a child that navigates itself to another site? | **Blocked**: the server saw 0 requests, and a `frame-src` violation fired in the parent. | **Blocked**: 0 requests, violation fired. |
| | …to the parent's own origin, outside the allowed path? | **Blocked**: 0 requests, violation. | **Blocked**: 0 requests, violation. |
| 3 | `script-src` host-source paths honored, at top level (`{A}/js/allowed/` and the exact file `{A}/js/exact.js`)? | **Yes.** Scripts outside them are refused before they are fetched. | **Yes**, the same. |
| | Inside the §4.2 app frame (opaque, CSP `sandbox` header, `script-src {O}/kit/ {O}/apps/demo/~cap/`)? | **Yes**: the kit's classic script and the app's own module run, and another cap's path is refused unfetched. | **Yes**, the same. |
| | A module script from the opaque frame without `Access-Control-Allow-Origin` | Fails CORS and does not run, which is why §4.2 sends `ACAO: *`. With it, the module runs, sent with `Origin: null` and no cookie. | The same. |
| | A *classic* `<script>` from the opaque frame (no-cors) | No cookie. | **Cookie sent** (answer 1 again). |
| 4 | `Origin: null` on a WebSocket opened by the opaque frame? | **Yes.** | **Yes.** |
| | Is that socket stopped by `connect-src 'none'` before it reaches the server? | **Yes**: no upgrade arrived. | **Yes**: no upgrade arrived. |
| | `Sec-Fetch-Site` on a WebSocket handshake | **Never sent**, not even on the parent's own same-origin socket. | `same-origin` from the parent, `cross-site` from the frame. |
| 5 | Inline script in the Vue production build? | **None.** `web/dist` emits one HTML file, `index.html`, with no inline `<script>`, no `on*=` attribute and no `javascript:` URL. | Same build. |
| | Does the shell run under the §4.10 policy (`script-src {O}/assets/ {O}/kit/`, no `'self'`, no `'unsafe-inline'`)? | **Yes**: the shell mounted, and no violation report or console error came. The control (an inline script under the same policy) was refused and reported. | **Yes**, the same. |
| | Does Quick Look's PDF viewer run under the §4.10 policy? (pdf.js's module worker at `/assets/pdf.worker.min-*.mjs`, WebAssembly, pdf.js's eval probe, each asked from the real build's page) | **No.** The worker is refused by `worker-src {O}/sw.js` before it is fetched, with a `worker-src` violation. `WebAssembly.compile` is refused (`CompileError`) and `new Function` is refused (`EvalError`), each with a `script-src` violation. Control: with `{O}/assets/` added to `worker-src`, the worker is fetched and runs. | **No**, the same three refusals. WebKit reported `script-src` once, not once per refusal. |

**What this changes for the design.**

- **§4.9, `/ws`:** "fail closed when `Sec-Fetch-Site` is missing on `/ws`"
  cannot ship as written. Chromium sends no fetch metadata on a WebSocket
  handshake, so that rule would refuse the shell's own socket in Chrome.
  WebKit's bug tracker records the same gap in Chromium, and WebKit
  added the headers in 2022 ([WebKit bug
  237550](https://bugs.webkit.org/show_bug.cgi?id=237550)). On `/ws` the
  reliable signal is `Origin`: it is `null` from the frame in both
  engines, and the existing `Origin == Host` check already refuses that.
  For `/ws`, the gate should key on `Origin` and treat a missing
  `Sec-Fetch-Site` as allowed. The `Sec-Fetch-Site: cross-site` refusal is
  still right, because WebKit sends `cross-site` from the frame. This was
  measured over `ws://` to 127.0.0.1. `wss://` was not tested, but nothing
  suggests Chromium differs there.
- **§4.9 generally:** the gate is what protects cookie routes on WebKit. A
  frame's request there arrives with the operator's Strict cookie and with
  `Sec-Fetch-Site: cross-site`, so refusing `cross-site` and `same-site`
  (unless `Sec-Fetch-Dest: document`) is what stops it. The frame's own
  navigation arrives as `Sec-Fetch-Dest: iframe`, not `document`, so the
  exception does not let it through. The design already assumed this
  ("nothing in the design depends on browsers withholding SameSite=Strict
  cookies"). The spike shows the assumption is needed, not just prudent.
- **§4.2's cap route ignores the cookie**, so a cookie on the frame's asset
  requests (WebKit) grants nothing there, as intended.
- **§4.10 cannot ship as written: it breaks Quick Look's PDF viewer.** The
  shell itself mounts under it in both engines with no violation, but
  `src/shell/pdf.ts` starts pdf.js's module worker from `/assets/`, which
  `worker-src {O}/sw.js` refuses in both. pdf.js is built to fall back to
  parsing on the shell's main thread when its worker fails (from its
  source, not observed here), and each PDF opened would raise violation
  reports. pdf.js's WebAssembly decoders (`wasmUrl`, for JPEG 2000
  and color profiles) and its eval probe are refused too, by the absence of
  `'wasm-unsafe-eval'` and `'unsafe-eval'` in `script-src`. Open for stage
  5, not decided here: either `worker-src` gains `{O}/assets/` (the bundle
  is already trusted as script), or the worker is served from a pinned path
  of its own; and either `'wasm-unsafe-eval'` is accepted for the shell or
  pdf.js runs without its WASM decoders (the build already ships its
  `*_nowasm_fallback.js` decoders in `/pdfjs/wasm/`), with
  `isEvalSupported: false` set so the probe stops reporting. Whether pdf.js renders a PDF end to end
  under the fallback was not tested. Also not exercised: the shell after a
  live `/ws` connection, and the service worker. Playwright blocked service
  workers in the spike, so `worker-src {O}/sw.js` is unverified.
- **§4.10's `frame-src` jail holds** on both engines, including for a path
  on the parent's own origin. The `securitypolicyviolation` event that the
  teardown listens for fires in the parent on both engines.

**Not answered by the spike:** Safari itself (macOS, iOS), the iOS PWA soft
keyboard with `allow-forms`, Firefox, and CodeMirror's `worker-src`. These
remain on the device-check list in §6.
