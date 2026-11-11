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

/// This page's policy, deliberately not [`crate::auth::html::CSP`].
///
/// Two additions over the login page's, and no more. `script-src 'self'`
/// permits the one file this page loads. `connect-src 'self'` permits the
/// two `fetch` calls a ceremony makes — without it `default-src 'none'`
/// blocks them and the rest is decoration. Everything else stays denied:
/// no images, no fonts, no frames, no outbound anything.
pub const CSP: &str = "default-src 'none'; style-src 'unsafe-inline'; script-src 'self'; \
                       connect-src 'self'; form-action 'self'";

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
<p><a href=\"/login\">Use a one-time code instead</a></p>\
</main><script src=\"{SCRIPT_PATH}\" defer></script></body></html>",
        style = crate::auth::html::STYLE,
    )
}

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
    }
}
