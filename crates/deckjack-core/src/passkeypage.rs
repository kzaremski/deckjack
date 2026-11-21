//! The passkey sign-in page: the browser half of [`crate::passkeyhttp`].
//!
//! # Why this is not on `/login`
//!
//! A WebAuthn ceremony is JavaScript — `navigator.credentials.get` has no
//! HTML form equivalent — and [`crate::auth::html::CSP`] is
//! `default-src 'none'` with no `script-src` at all, so nothing can run
//! there. The obvious fix is to widen that constant. This module is the
//! other answer, and it is the better one.
//!
//! `/login` is reachable by anyone who can reach the port, and it is where
//! the one-time code is typed. Today `default-src 'none'` makes any
//! injection on that page inert: there is no way to run script, so there is
//! no way to read the code out of the input. Adding `script-src` would trade
//! that away permanently to buy a button, and it would do it on the one page
//! in the product that collects a secret by hand.
//!
//! So the ceremony lives here instead, on a page that collects nothing. The
//! widening is real but confined: [`CSP`] allows script and `fetch` from
//! this origin only, and the page it applies to has no input, no secret and
//! nothing interpolated into it. `/login` reaches it with an ordinary link,
//! which costs no policy at all — navigation is not governed by
//! `default-src`.
//!
//! # The script is a file, not inline
//!
//! `script-src 'self'` and a `<script src>`, never `'unsafe-inline'`. The
//! difference matters on the day someone interpolates a value into this
//! page: with `'unsafe-inline'` that is script execution, and with `'self'`
//! it is a stray string. A nonce would be stricter still, but a nonce has to
//! be minted per response and this page is a constant — `'self'` with no
//! inline script anywhere is the same guarantee for a page that never
//! varies.

use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;

/// Where the page lives. Inside [`crate::passkeyhttp::MOUNT`] so one
/// `ALLOWED_WHILE_LOCKED` decision covers the ceremony and its page
/// together: neither is reachable while the node is locked down.
pub const PAGE_PATH: &str = "/passkey";

/// The ceremony script's own path. Served from this origin, which is what
/// lets [`CSP`] say `script-src 'self'` instead of `'unsafe-inline'`.
pub const SCRIPT_PATH: &str = "/passkey/ceremony.js";

/// Where a device is enrolled: the page that was missing.
///
/// Every route for registering a credential has been mounted and working for
/// as long as `passkeyhttp` has existed, and nothing in the product called
/// any of them — so the only way to get a passkey was to paste a script into
/// a devtools console, which rules out every phone and is not a thing to ask
/// of anyone. A sign-in page that cannot lead to having something to sign in
/// with is not finished.
///
/// Inside [`crate::passkeyhttp::MOUNT`] with the rest, so one
/// `ALLOWED_WHILE_LOCKED` decision still covers the pages and the routes
/// together: enrolling a new device while the node is locked down is exactly
/// what the kill switch exists to prevent.
pub const ENROLL_PATH: &str = "/passkey/enroll";

/// The enrollment script's path. A separate file from the ceremony script
/// because the two pages share no code and a reader of either should not have
/// to skip past the other.
pub const ENROLL_SCRIPT_PATH: &str = "/passkey/enroll.js";

/// This page's policy, deliberately not [`crate::auth::html::CSP`].
///
/// Two additions over the login page's, and no more. `script-src 'self'`
/// permits the one file this page loads. `connect-src 'self'` permits the
/// two `fetch` calls a ceremony makes — without it `default-src 'none'`
/// blocks them and the rest is decoration. Everything else stays denied:
/// no images, no fonts, no frames, no outbound anything.
///
/// `frame-ancestors 'none'` for the reason the login page's policy gives:
/// a ceremony page in a frame is a page someone else can make you click.
pub const CSP: &str = "default-src 'none'; style-src 'unsafe-inline'; script-src 'self'; \
                       connect-src 'self'; form-action 'self'; frame-ancestors 'none'";

/// What the page says when no `rp_id` a browser would accept can be
/// determined — a node on a non-loopback address with no `[passkey]`
/// section. Explaining is better than a button that calls a route which is
/// not mounted, and much better than inviting an enrollment that would bind
/// a credential to a name the browser later rejects.
pub fn unavailable() -> String {
    format!(
        "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\">\
<meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">\
<meta name=\"robots\" content=\"noindex\">\
<title>Deckjack: passkey</title><style>{style}</style></head><body><main>\
<h1>Passkeys are off</h1>\
<p>This node is not served on <code>localhost</code>, and no passkey name is \
configured. A passkey is bound to the name it was created under and cannot be \
moved afterwards, so Deckjack will not guess one.</p>\
<p>Set <code>passkey.rp_id</code> and <code>passkey.origin</code> in \
<code>.deckjack/config.toml</code> and restart.</p>\
<p class=\"alt\"><a href=\"/login\">Sign in with a one-time code</a></p>\
</main></body></html>",
        style = crate::auth::html::STYLE,
    )
}

/// The ceremony page. A constant: nothing is interpolated, so there is
/// nothing on it that an attacker can influence.
pub fn page() -> String {
    format!(
        "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\">\
<meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">\
<meta name=\"robots\" content=\"noindex\">\
<title>Deckjack: passkey</title><style>{style}</style></head><body><main>\
<h1>Deckjack</h1>\
<p>Sign in with the passkey enrolled on this device.</p>\
<p id=\"status\" class=\"msg\" role=\"status\" aria-live=\"polite\"></p>\
<button id=\"go\" type=\"button\">Use a passkey</button>\
<p><a href=\"{ENROLL_PATH}\">Set up a passkey on this device</a></p>\
<p><a href=\"/login\">Use a one-time code instead</a></p>\
</main><script src=\"{SCRIPT_PATH}\" defer></script></body></html>",
        style = crate::auth::html::STYLE,
    )
}

/// The enrollment page.
///
/// A constant, like the sign-in page and for the same reason: `script-src
/// 'self'` is defensible here because there is no value an attacker could get
/// interpolated into the markup. Everything that varies — whether anything is
/// enrolled, the device list, every message — arrives by `fetch` and is
/// written with `textContent`, never `innerHTML`.
///
/// # The code input, and what it costs
///
/// This page has a text field for a secret, which is the thing this module's
/// header argues `/login` must never combine with a `script-src`. The
/// difference is worth stating plainly rather than leaving implied: the
/// enrollment code authorizes adding one credential, is single-use, expires in
/// minutes, and is useless without a session that is already signed in. The
/// one-time sign-in code on `/login` is the keys to the node. They are not the
/// same secret, and only one of them is worth a permanent policy widening on
/// the page that collects it.
pub fn enroll_page() -> String {
    format!(
        "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\">\
<meta name=\"viewport\" content=\"width=device-width,initial-scale=1,viewport-fit=cover\">\
<meta name=\"robots\" content=\"noindex\">\
<title>Deckjack: set up a passkey</title><style>{style}{extra}</style></head>\
<body><main>\
<h1>Set up a passkey</h1>\
<p id=\"intro\" class=\"alt\">Checking what this node already knows about…</p>\
<p id=\"status\" class=\"msg\" role=\"status\" aria-live=\"polite\"></p>\
<form id=\"form\" hidden>\
<label for=\"nickname\">Name this device</label>\
<input id=\"nickname\" name=\"nickname\" type=\"text\" maxlength=\"48\" \
autocomplete=\"off\" autocapitalize=\"words\" required \
placeholder=\"Phone, laptop, work desktop…\">\
<div id=\"coderow\" hidden>\
<label for=\"code\">Code from a device that is already set up</label>\
<input id=\"code\" name=\"code\" type=\"text\" inputmode=\"latin\" \
autocomplete=\"off\" autocapitalize=\"characters\" spellcheck=\"false\" \
placeholder=\"XXXX-XXXX\">\
</div>\
<button id=\"add\" type=\"submit\">Add this device</button>\
</form>\
<section id=\"devices\" hidden>\
<h2>Devices that can sign in</h2>\
<ul id=\"list\"></ul>\
<button id=\"approve\" type=\"button\" class=\"ghost\">\
Show a code for another device</button>\
<p id=\"ticket\" class=\"ticket\" hidden></p>\
</section>\
<p class=\"alt\"><a href=\"{PAGE_PATH}\">Sign in with a passkey</a></p>\
<p class=\"alt\"><a href=\"/login\">Sign in with a one-time code</a></p>\
</main><script src=\"{ENROLL_SCRIPT_PATH}\" defer></script></body></html>",
        style = crate::auth::html::STYLE,
        extra = ENROLL_STYLE,
    )
}

/// The handful of rules the shared login style has no reason to carry.
const ENROLL_STYLE: &str = "\
label{display:block;margin:0 0 .35rem;font-size:.85rem;opacity:.85}\
#form input{width:100%;margin:0 0 1rem;box-sizing:border-box}\
#coderow{margin:0}\
ul{list-style:none;margin:0 0 1rem;padding:0}\
li{display:flex;align-items:baseline;gap:.6rem;padding:.6rem 0;\
border-top:1px solid rgba(255,255,255,.12)}\
li .name{font-weight:600}\
li .when{font-size:.78rem;opacity:.7;flex:1}\
li button{flex:none;font-size:.8rem;padding:.3rem .6rem}\
button.ghost{background:transparent;border:1px solid rgba(255,255,255,.3)}\
.ticket{font-family:ui-monospace,monospace;font-size:1.5rem;letter-spacing:.12em;\
text-align:center;padding:.8rem;margin:0 0 1rem;border-radius:.4rem;\
background:rgba(255,255,255,.08);user-select:all}\
h2{font-size:1rem;margin:1.5rem 0 .5rem}\
";

/// The ceremony itself.
///
/// Hand-written rather than bundled: the whole browser half of WebAuthn is
/// two `fetch` calls and the base64url conversions around them, and a build
/// step for that would mean the sign-in page could not be served by a binary
/// someone copied to a phone.
pub const SCRIPT: &str = r#"// Deckjack passkey sign-in.
//
// Served as a file because /passkey's policy is `script-src 'self'`: there
// is no inline script on that page and no 'unsafe-inline' permitting one.
"use strict";

const statusLine = document.getElementById("status");
const button = document.getElementById("go");

// The status line carries the `msg` class, which is the error color. A
// progress message borrows the line without borrowing the color.
function say(text, bad) {
  statusLine.textContent = text;
  statusLine.style.color = bad ? "" : "inherit";
}

// webauthn-rs sends every binary field as unpadded base64url, and
// `navigator.credentials.get` wants ArrayBuffers. Done by hand rather than
// with PublicKeyCredential.parseRequestOptionsFromJSON so that an older
// browser is not refused for lacking a method we only use once.
function toBytes(s) {
  const padded = s.length % 4 === 0 ? s : s + "=".repeat(4 - (s.length % 4));
  const binary = atob(padded.replace(/-/g, "+").replace(/_/g, "/"));
  const out = new Uint8Array(binary.length);
  for (let i = 0; i < binary.length; i += 1) out[i] = binary.charCodeAt(i);
  return out;
}

function toB64url(buffer) {
  const bytes = new Uint8Array(buffer);
  let binary = "";
  for (let i = 0; i < bytes.length; i += 1) binary += String.fromCharCode(bytes[i]);
  return btoa(binary).replace(/\+/g, "-").replace(/\//g, "_").replace(/=+$/, "");
}

async function post(path, body) {
  const r = await fetch(path, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify(body || {}),
    // The session cookie on the way back is the whole point of finishing.
    credentials: "same-origin",
    cache: "no-store",
  });
  const text = await r.text();
  if (!r.ok) {
    // These routes answer with a plain sentence, not JSON, so the server's
    // own wording is what the operator reads. It never describes a refusal
    // in the client's words.
    throw new Error(text.trim() || "HTTP " + r.status);
  }
  return text ? JSON.parse(text) : {};
}

async function signIn() {
  button.disabled = true;
  say("Waiting for your passkey…");
  try {
    const started = await post("/passkey/sign-in/begin");
    // RequestChallengeResponse serializes as {publicKey: {...}}; the
    // fallback is for the day that wrapper changes.
    const options = started.challenge.publicKey || started.challenge;
    options.challenge = toBytes(options.challenge);
    if (Array.isArray(options.allowCredentials)) {
      for (const c of options.allowCredentials) c.id = toBytes(c.id);
    }

    const credential = await navigator.credentials.get({ publicKey: options });
    if (!credential) throw new Error("no passkey was presented");

    // Built field by field rather than with credential.toJSON(), because
    // what has to parse on the other side is webauthn-rs's
    // PublicKeyCredential and not whatever shape the browser prefers today.
    await post("/passkey/sign-in/finish", {
      ceremony: started.ceremony,
      credential: {
        id: credential.id,
        rawId: toB64url(credential.rawId),
        type: credential.type,
        response: {
          clientDataJSON: toB64url(credential.response.clientDataJSON),
          authenticatorData: toB64url(credential.response.authenticatorData),
          signature: toB64url(credential.response.signature),
          userHandle: credential.response.userHandle
            ? toB64url(credential.response.userHandle)
            : null,
        },
        extensions: credential.getClientExtensionResults(),
      },
    });

    // replace, not assign: the back button should not return to a spent
    // ceremony that can only fail.
    window.location.replace("/");
  } catch (e) {
    // NotAllowedError is what a browser reports for a cancelled prompt and
    // for a timeout alike, and calling that "refused" would be a lie about
    // which one happened.
    const message =
      e && e.name === "NotAllowedError"
        ? "No passkey was used. Try again, or use a one-time code."
        : (e && e.message) || "the ceremony failed";
    say(message, true);
    button.disabled = false;
  }
}

if (window.PublicKeyCredential) {
  button.addEventListener("click", signIn);
} else {
  button.disabled = true;
  say("This browser has no passkey support. Use a one-time code.", true);
}
"#;

/// The page and its script. Merged inside [`crate::wiring::app`] above the
/// lockdown gate, like every other pre-authentication route.
///
/// Mounted either way, because `/login` links here unconditionally and a
/// link to a 404 is worse than a page that explains. With `available` false
/// the page says passkeys are off and carries no script at all.
pub fn page_router(available: bool) -> axum::Router {
    let page = axum::Router::new().route(
        PAGE_PATH,
        get(move || async move { if available { html() } else { off() } }),
    );
    if available {
        page.route(SCRIPT_PATH, get(script))
            // Enrollment is mounted only when a browser would actually accept
            // a credential from this node. Offering it otherwise is worse than
            // saying nothing: the ceremony would succeed, the route would
            // refuse the result, and a passkey cannot be moved to a different
            // rp_id afterwards. `unavailable()` explains instead.
            .route(ENROLL_PATH, get(|| async { enroll_html() }))
            .route(ENROLL_SCRIPT_PATH, get(enroll_script))
    } else {
        page
    }
}

fn html() -> Response {
    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, "text/html; charset=utf-8"),
            (header::CACHE_CONTROL, "no-store"),
            (header::CONTENT_SECURITY_POLICY, CSP),
            (header::REFERRER_POLICY, "no-referrer"),
            (header::X_FRAME_OPTIONS, "DENY"),
        ],
        page(),
    )
        .into_response()
}

/// The unavailable page keeps the LOGIN page's policy, not [`CSP`]: it runs
/// no script, so it has no business permitting any.
fn off() -> Response {
    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, "text/html; charset=utf-8"),
            (header::CACHE_CONTROL, "no-store"),
            (header::CONTENT_SECURITY_POLICY, crate::auth::html::CSP),
            (header::REFERRER_POLICY, "no-referrer"),
            (header::X_FRAME_OPTIONS, "DENY"),
        ],
        unavailable(),
    )
        .into_response()
}

async fn script() -> Response {
    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, "text/javascript; charset=utf-8"),
            // Not cached, because it is served by the same binary that
            // serves the page and a stale copy of one with a fresh copy of
            // the other is a failure nobody would diagnose quickly.
            (header::CACHE_CONTROL, "no-store"),
        ],
        SCRIPT,
    )
        .into_response()
}

/// The browser half of enrollment.
///
/// Hand-written and dependency-free for the same reason the ceremony script
/// is: the whole browser side of WebAuthn is a few `fetch` calls and the
/// base64url conversions around them, and a build step would mean this page
/// could not be served by a binary someone copied onto a machine.
///
/// Every value from the node is written with `textContent`. A device nickname
/// is operator-supplied text that round-trips through the database, and
/// `innerHTML` would make naming a laptop `<img onerror=…>` a stored
/// injection — inert under this CSP, but inert by luck is not a design.
pub const ENROLL_SCRIPT: &str = r#"// Deckjack passkey enrollment.
//
// Served as a file because this page's policy is `script-src 'self'`: there
// is no inline script here and no 'unsafe-inline' permitting one.
"use strict";

const intro = document.getElementById("intro");
const statusLine = document.getElementById("status");
const form = document.getElementById("form");
const nickname = document.getElementById("nickname");
const codeRow = document.getElementById("coderow");
const code = document.getElementById("code");
const addButton = document.getElementById("add");
const devices = document.getElementById("devices");
const list = document.getElementById("list");
const approveButton = document.getElementById("approve");
const ticket = document.getElementById("ticket");

function say(text, bad) {
  statusLine.textContent = text;
  statusLine.style.color = bad ? "" : "inherit";
}

// webauthn-rs sends every binary field as unpadded base64url, and
// `navigator.credentials.create` wants ArrayBuffers.
function toBytes(s) {
  const padded = s.length % 4 === 0 ? s : s + "=".repeat(4 - (s.length % 4));
  const binary = atob(padded.replace(/-/g, "+").replace(/_/g, "/"));
  const out = new Uint8Array(binary.length);
  for (let i = 0; i < binary.length; i += 1) out[i] = binary.charCodeAt(i);
  return out;
}

function toB64url(buffer) {
  const bytes = new Uint8Array(buffer);
  let binary = "";
  for (let i = 0; i < bytes.length; i += 1) binary += String.fromCharCode(bytes[i]);
  return btoa(binary).replace(/\+/g, "-").replace(/\//g, "_").replace(/=+$/, "");
}

async function send(method, path, body) {
  const init = {
    method: method,
    credentials: "same-origin",
    cache: "no-store",
  };
  if (body !== undefined) {
    init.headers = { "content-type": "application/json" };
    init.body = JSON.stringify(body);
  }
  const r = await fetch(path, init);
  const text = await r.text();
  if (!r.ok) {
    // These routes answer with a plain sentence rather than JSON, so the
    // server's own wording is what the operator reads. The status is carried
    // along because 401 is the one case the page handles structurally.
    const e = new Error(text.trim() || "HTTP " + r.status);
    e.status = r.status;
    throw e;
  }
  return text ? JSON.parse(text) : {};
}

const post = (path, body) => send("POST", path, body === undefined ? {} : body);

function when(seconds) {
  if (!seconds) return "never used";
  const days = Math.floor((Date.now() / 1000 - seconds) / 86400);
  if (days <= 0) return "used today";
  if (days === 1) return "used yesterday";
  return "used " + days + " days ago";
}

/** One row. Built node by node so no nickname is ever parsed as markup. */
function row(cred, only) {
  const li = document.createElement("li");

  const name = document.createElement("span");
  name.className = "name";
  name.textContent = cred.nickname;
  li.appendChild(name);

  const detail = document.createElement("span");
  detail.className = "when";
  detail.textContent = when(cred.last_used);
  li.appendChild(detail);

  // Two clicks, not a native dialog. Removing the wrong device is a real
  // mistake to make on a phone, and `confirm()` in an installed PWA is a
  // modal the operator cannot always see the context behind.
  const remove = document.createElement("button");
  remove.type = "button";
  remove.textContent = "Remove";
  let armed = false;
  remove.addEventListener("click", async () => {
    if (!armed) {
      armed = true;
      remove.textContent = only ? "Remove the only one?" : "Really remove?";
      return;
    }
    remove.disabled = true;
    try {
      const gone = await post("/passkey/credentials/remove", { id: cred.id });
      say(gone.removed + " can no longer sign in.");
      await refresh();
    } catch (e) {
      say(e.message, true);
      remove.disabled = false;
      armed = false;
      remove.textContent = "Remove";
    }
  });
  li.appendChild(remove);
  return li;
}

/**
 * Asks the node what it holds, and shapes the page around the answer.
 *
 * The device list is also how the page learns whether this is the first
 * credential: the node allows an unapproved enrollment only while none is
 * enrolled, so asking for a code before that would invent a step, and not
 * asking for one afterwards would offer a button that is refused.
 */
async function refresh() {
  let state;
  try {
    state = await send("GET", "/passkey/credentials");
  } catch (e) {
    if (e.status === 401) {
      intro.textContent =
        "Sign in first — enrolling a passkey has to be authorized by a session that is already signed in.";
      const link = document.createElement("a");
      link.href = "/login";
      link.textContent = "Sign in with a one-time code";
      intro.appendChild(document.createElement("br"));
      intro.appendChild(link);
      form.hidden = true;
      devices.hidden = true;
      return;
    }
    intro.textContent = "";
    say(e.message, true);
    return;
  }

  const creds = state.credentials || [];
  form.hidden = false;
  devices.hidden = creds.length === 0;
  codeRow.hidden = creds.length === 0;
  code.required = creds.length > 0;

  intro.textContent =
    creds.length === 0
      ? "Nothing is enrolled yet, so this first device needs no approval code."
      : "This node already has a passkey, so adding another needs a code from a device that is already set up.";

  list.textContent = "";
  for (const c of creds) list.appendChild(row(c, creds.length === 1));
}

async function enroll(event) {
  event.preventDefault();
  const name = nickname.value.trim();
  if (!name) {
    say("Name the device first, so the list means something later.", true);
    return;
  }
  addButton.disabled = true;
  say("Waiting for your authenticator…");
  try {
    const body = { nickname: name };
    const typed = code.value.trim();
    if (!codeRow.hidden && typed) body.code = typed;

    const started = await post("/passkey/enroll/begin", body);
    // CreationChallengeResponse serializes as {publicKey: {...}}; the
    // fallback is for the day that wrapper changes.
    const options = started.challenge.publicKey || started.challenge;
    options.challenge = toBytes(options.challenge);
    options.user.id = toBytes(options.user.id);
    if (Array.isArray(options.excludeCredentials)) {
      for (const c of options.excludeCredentials) c.id = toBytes(c.id);
    }

    const credential = await navigator.credentials.create({ publicKey: options });
    if (!credential) throw new Error("no credential was created");

    // Built field by field rather than with credential.toJSON(), because what
    // has to parse on the other side is webauthn-rs's
    // RegisterPublicKeyCredential and not whatever shape the browser prefers.
    const done = await post("/passkey/enroll/finish", {
      ceremony: started.ceremony,
      credential: {
        id: credential.id,
        rawId: toB64url(credential.rawId),
        type: credential.type,
        response: {
          clientDataJSON: toB64url(credential.response.clientDataJSON),
          attestationObject: toB64url(credential.response.attestationObject),
        },
        extensions: credential.getClientExtensionResults(),
      },
    });

    nickname.value = "";
    code.value = "";
    ticket.hidden = true;
    say(done.nickname + " can now sign in with a passkey.");
    await refresh();
  } catch (e) {
    // NotAllowedError is what a browser reports for a cancelled prompt and
    // for a timeout alike, and calling that "refused" would be a lie about
    // which one happened.
    const message =
      e && e.name === "NotAllowedError"
        ? "The authenticator was not used. Try again."
        : e && e.name === "InvalidStateError"
          ? "This device already has a passkey for this node."
          : (e && e.message) || "enrollment failed";
    say(message, true);
  } finally {
    addButton.disabled = false;
  }
}

async function approve() {
  approveButton.disabled = true;
  try {
    const t = await post("/passkey/enroll/approve");
    ticket.textContent = t.code;
    ticket.hidden = false;
    say("Enter this code on the new device. It is good once, and not for long.");
  } catch (e) {
    say(e.message, true);
  } finally {
    approveButton.disabled = false;
  }
}

if (window.PublicKeyCredential) {
  form.addEventListener("submit", enroll);
  approveButton.addEventListener("click", approve);
  refresh();
} else {
  intro.textContent =
    "This browser has no passkey support, so there is nothing to enroll here.";
  say("Use a one-time code to sign in instead.", true);
}
"#;

/// The enrollment page, with the ceremony policy.
fn enroll_html() -> Response {
    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, "text/html; charset=utf-8"),
            (header::CACHE_CONTROL, "no-store"),
            (header::CONTENT_SECURITY_POLICY, CSP),
            (header::REFERRER_POLICY, "no-referrer"),
            (header::X_FRAME_OPTIONS, "DENY"),
        ],
        enroll_page(),
    )
        .into_response()
}

async fn enroll_script() -> Response {
    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, "text/javascript; charset=utf-8"),
            (header::CACHE_CONTROL, "no-store"),
        ],
        ENROLL_SCRIPT,
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_page_loads_no_inline_script_and_no_third_party_asset() {
        let html = page();
        // The guarantee `script-src 'self'` is worth having: one script
        // tag, with a src, and nothing between the tags.
        assert_eq!(html.matches("<script").count(), 1);
        assert!(html.contains(&format!("<script src=\"{SCRIPT_PATH}\" defer></script>")));
        assert!(
            !html.contains("onclick") && !html.contains("onload"),
            "an inline handler is inline script, which this CSP refuses"
        );
        // No absolute URL anywhere: every reference is a path on this origin.
        for scheme in ["http://", "https://", "data:"] {
            assert!(!html.contains(scheme), "third-party reference: {scheme}");
        }
    }

    #[test]
    fn the_policy_widens_exactly_twice_over_the_login_page() {
        // The login page's policy is the baseline, and this test is what
        // notices if someone "simplifies" the two into one.
        assert!(CSP.contains("default-src 'none'"));
        assert!(CSP.contains("script-src 'self'"));
        assert!(CSP.contains("connect-src 'self'"));
        assert!(
            !CSP.contains("script-src 'unsafe-inline'")
                && !CSP.contains("'unsafe-eval'")
                && !CSP.contains("script-src *"),
            "the point of a separate page is that script here is still confined: {CSP}"
        );
        // And the login page must not have grown one.
        assert!(
            !crate::auth::html::CSP.contains("script-src"),
            "/login gained a script-src; this module exists so it would not"
        );
    }

    #[test]
    fn the_script_is_served_from_inside_the_passkey_mount() {
        // So that lockdown::ALLOWED_WHILE_LOCKED needs no second entry and
        // the gate covers the page, the script and the routes alike.
        assert!(PAGE_PATH.starts_with(crate::passkeyhttp::MOUNT));
        assert!(SCRIPT_PATH.starts_with(crate::passkeyhttp::MOUNT));
    }

    #[test]
    fn the_script_posts_to_the_routes_that_exist() {
        // A typo here is a page that loads, offers a button and 404s on the
        // click, which no Rust test would otherwise notice.
        assert!(SCRIPT.contains("\"/passkey/sign-in/begin\""));
        assert!(SCRIPT.contains("\"/passkey/sign-in/finish\""));
        assert!(SCRIPT.contains("credentials: \"same-origin\""));
    }

    #[test]
    fn the_page_carries_its_own_policy() {
        let r = html();
        assert_eq!(
            r.headers().get(header::CONTENT_SECURITY_POLICY).unwrap(),
            CSP
        );
        assert_eq!(r.headers().get(header::CACHE_CONTROL).unwrap(), "no-store");
    }

    // ---- enrolling a device, which used to need a pasted console script ----

    #[test]
    fn the_sign_in_page_offers_a_way_to_enroll() {
        // The gap this closes: every route for enrolling a credential was
        // mounted and working, and nothing in the product called them, so the
        // only way to register a passkey was to paste a script into a devtools
        // console on a desktop browser. A sign-in page that cannot lead to
        // having something to sign in WITH is not finished.
        let html = page();
        assert!(
            html.contains(&format!("href=\"{ENROLL_PATH}\"")),
            "no way to reach enrollment from the sign-in page: {html}"
        );
    }

    #[test]
    fn the_enrollment_page_loads_no_inline_script_and_no_third_party_asset() {
        let html = enroll_page();
        assert_eq!(html.matches("<script").count(), 1);
        assert!(html.contains(&format!(
            "<script src=\"{ENROLL_SCRIPT_PATH}\" defer></script>"
        )));
        assert!(
            !html.contains("onclick") && !html.contains("onload"),
            "an inline handler is inline script, which this CSP refuses"
        );
        for scheme in ["http://", "https://", "data:"] {
            assert!(!html.contains(scheme), "third-party reference: {scheme}");
        }
    }

    #[test]
    fn nothing_about_the_node_is_baked_into_the_enrollment_page() {
        // The whole CSP argument for this module rests on these pages being
        // constants: `script-src 'self'` is safe here because there is no
        // value an attacker could get interpolated in. The device list and
        // every message come from fetch, so the page must be byte-identical
        // every time it is served.
        assert_eq!(enroll_page(), enroll_page());
    }

    #[test]
    fn the_enrollment_page_collects_a_nickname_and_an_approval_code() {
        let html = enroll_page();
        assert!(
            html.contains("id=\"nickname\""),
            "no way to name the device"
        );
        assert!(
            html.contains("id=\"code\""),
            "no way to enter the code from an already-enrolled device"
        );
    }

    #[test]
    fn the_enrollment_script_posts_to_the_routes_that_exist() {
        // A typo here is a page that loads, offers a button, and 404s on the
        // click. These five are the whole enrollment surface.
        // Each route with the METHOD its handler is mounted under. Asserting
        // only that the path appears somewhere is what let the first draft of
        // this script POST to the device list -- a 405 on page load, and this
        // test was green for it.
        for (method, route) in [
            ("post", "/passkey/enroll/begin"),
            ("post", "/passkey/enroll/finish"),
            ("post", "/passkey/enroll/approve"),
            ("post", "/passkey/credentials/remove"),
        ] {
            assert!(
                ENROLL_SCRIPT.contains(&format!("{method}(\"{route}\"")),
                "the script never {method}s {route}"
            );
        }
        assert!(
            ENROLL_SCRIPT.contains("send(\"GET\", \"/passkey/credentials\")"),
            "the device list is mounted as a GET; anything else is a 405 on load"
        );
        assert!(ENROLL_SCRIPT.contains("credentials: \"same-origin\""));
    }

    #[test]
    fn the_enrollment_script_runs_the_ceremony_the_authenticator_expects() {
        // `create`, not `get`: enrolling makes a credential, and calling the
        // assertion API here would prompt for a passkey that does not exist
        // yet. The field names are webauthn-rs's `RegisterPublicKeyCredential`
        // and are not negotiable -- a `PublicKeyCredential` shape would be
        // rejected by the route after the operator had already touched their
        // authenticator.
        assert!(ENROLL_SCRIPT.contains("navigator.credentials.create"));
        assert!(!ENROLL_SCRIPT.contains("navigator.credentials.get"));
        for field in ["attestationObject", "clientDataJSON", "rawId"] {
            assert!(ENROLL_SCRIPT.contains(field), "missing {field}");
        }
        // `user.id` and `excludeCredentials` are base64url on the wire and
        // ArrayBuffers in the browser. Forgetting either is a ceremony that
        // fails inside the authenticator with nothing useful to report.
        assert!(ENROLL_SCRIPT.contains("user.id"));
        assert!(ENROLL_SCRIPT.contains("excludeCredentials"));
    }

    #[test]
    fn the_enrollment_page_and_script_live_inside_the_passkey_mount() {
        // So `lockdown::ALLOWED_WHILE_LOCKED` needs no new entry and the gate
        // covers enrollment too: adding a device must not be possible while
        // the node is locked down.
        assert!(ENROLL_PATH.starts_with(crate::passkeyhttp::MOUNT));
        assert!(ENROLL_SCRIPT_PATH.starts_with(crate::passkeyhttp::MOUNT));
    }

    #[test]
    fn the_enrollment_page_carries_the_ceremony_policy() {
        let r = enroll_html();
        assert_eq!(
            r.headers().get(header::CONTENT_SECURITY_POLICY).unwrap(),
            CSP
        );
        assert_eq!(r.headers().get(header::CACHE_CONTROL).unwrap(), "no-store");
        assert_eq!(
            r.headers().get(header::REFERRER_POLICY).unwrap(),
            "no-referrer"
        );
    }

    #[test]
    fn enrollment_is_not_offered_at_all_when_passkeys_are_off() {
        // Offering to enroll against an rp_id no browser would accept is worse
        // than saying nothing: the credential would be created and then
        // refused, and a passkey cannot be moved to another rp_id afterwards.
        let html = unavailable();
        assert!(
            !html.contains(ENROLL_PATH),
            "the off page invites an enrollment that cannot work: {html}"
        );
    }

    #[test]
    fn the_unavailable_page_runs_no_script_and_permits_none() {
        let html = unavailable();
        assert!(!html.contains("<script"), "{html}");
        assert!(html.contains("href=\"/login\""), "no way back to the code");
        // It borrows the login page's stricter policy rather than this
        // module's, because it has nothing to execute.
        let r = off();
        assert_eq!(
            r.headers().get(header::CONTENT_SECURITY_POLICY).unwrap(),
            crate::auth::html::CSP
        );
        // Nothing frames it, for the reason the login page gives.
        assert_eq!(r.headers().get(header::X_FRAME_OPTIONS).unwrap(), "DENY");
        assert!(crate::auth::html::CSP.contains("frame-ancestors 'none'"));
    }
}
