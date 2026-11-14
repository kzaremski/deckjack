//! Login and locked pages: inline HTML and CSS, no JavaScript, no *off-origin*
//! assets (§9.3). Nothing user-supplied is ever interpolated.
//!
//! It did fetch nothing at all until the shell became installable on iOS.
//! These pages now reference two static same-origin files — the manifest and
//! the home-screen icon — because an installed app whose session expired is
//! served this page, and without a manifest iOS drops it out of standalone
//! into a Safari tab. The property that mattered is kept exactly: no script,
//! and nothing loaded from a host someone else controls.
//!
//! [`CSP`] has no `script-src`, so nothing can execute here even if a future
//! edit did interpolate something — which is why the WebAuthn ceremony is
//! NOT on this page. It needs script; this page collects the one-time code
//! by hand. [`crate::passkeypage`] carries the ceremony under its own
//! slightly wider policy, and this page reaches it with an ordinary link.
//! A link costs no policy: navigation is not governed by `default-src`.

/// `img-src` and `manifest-src` are open to this origin and nothing else, so
/// the page can carry the home-screen icon and the manifest. Both are needed
/// because an installed app that loses its session lands *here*, and a login
/// page with no manifest drops out of standalone into a Safari tab — and
/// because "Add to Home Screen" pressed on this page is what iOS captures.
///
/// Still no `script-src`: nothing can execute here, which is the property
/// that lets this page interpolate a message without the WebAuthn ceremony
/// being reachable. Images and a manifest are static same-origin assets and
/// carry no such risk.
pub const CSP: &str = "default-src 'none'; style-src 'unsafe-inline'; img-src 'self'; manifest-src 'self'; \
form-action 'self'";

/// Shared with [`crate::passkeypage`], which is a sibling pre-authentication
/// page and should not look like a different product. `pub(crate)` rather
/// than public: it is this crate's house style for the pages it serves
/// before a session exists, not an API.
pub(crate) const STYLE: &str = "\
*{box-sizing:border-box}\
body{margin:0;min-height:100dvh;display:flex;align-items:center;justify-content:center;\
padding:env(safe-area-inset-top) env(safe-area-inset-right) env(safe-area-inset-bottom) \
env(safe-area-inset-left);\
font:16px/1.5 system-ui,sans-serif;background:#f4f4f2;color:#1c1c1c;\
-webkit-tap-highlight-color:transparent;overscroll-behavior:none}\
main{width:min(22rem,100% - 2rem);padding:2rem;background:#fff;border-radius:12px;\
box-shadow:0 1px 3px rgba(0,0,0,.12)}\
h1{margin:0 0 1rem;font-size:1.25rem}\
p{margin:0 0 1rem}\
.msg{color:#a12a1f}\
.alt{margin:1rem 0 0;font-size:.875rem;text-align:center}\
.alt a{color:inherit;opacity:.75}\
input{width:100%;padding:.6rem .7rem;font:inherit;font-size:1.4rem;letter-spacing:.15em;\
text-transform:uppercase;font-family:ui-monospace,monospace;border:1px solid #bbb;border-radius:8px}\
button{margin-top:1rem;width:100%;padding:.6rem;font:inherit;border:0;border-radius:8px;\
background:#1c1c1c;color:#fff;cursor:pointer}\
@media (prefers-color-scheme:dark){body{background:#151515;color:#eee}\
main{background:#222;box-shadow:none}input{background:#111;color:#eee;border-color:#444}\
button{background:#eee;color:#111}.msg{color:#ff8a7a}}";

fn page(title: &str, body: &str) -> String {
    // `viewport-fit=cover` plus the safe-area padding in `STYLE` is what makes
    // `black-translucent` safe here: the status bar sits over the background
    // rather than over the form. The apple-* tags are repeated from the
    // shell's `index.html` rather than shared, because this page is served by
    // Rust and that one by Vite, and iOS reads whichever document was open
    // when "Add to Home Screen" was pressed.
    format!(
        "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\">\
<meta name=\"viewport\" content=\"width=device-width,initial-scale=1,viewport-fit=cover\">\
<meta name=\"robots\" content=\"noindex\">\
<meta name=\"color-scheme\" content=\"dark light\">\
<meta name=\"theme-color\" content=\"#161D27\" media=\"(prefers-color-scheme: dark)\">\
<meta name=\"theme-color\" content=\"#FFFFFF\" media=\"(prefers-color-scheme: light)\">\
<meta name=\"apple-mobile-web-app-capable\" content=\"yes\">\
<meta name=\"mobile-web-app-capable\" content=\"yes\">\
<meta name=\"apple-mobile-web-app-status-bar-style\" content=\"black-translucent\">\
<meta name=\"apple-mobile-web-app-title\" content=\"Deckjack\">\
<meta name=\"format-detection\" content=\"telephone=no\">\
<link rel=\"manifest\" href=\"/manifest.webmanifest\">\
<link rel=\"icon\" href=\"/icons/deckjack.svg\" type=\"image/svg+xml\">\
<link rel=\"apple-touch-icon\" href=\"/icons/deckjack-180.png\">\
<title>{title}</title><style>{STYLE}</style></head><body><main>{body}</main></body></html>"
    )
}

/// The login form, with an optional error line.
pub fn login(message: Option<&str>) -> String {
    let msg = message
        .map(|m| format!("<p class=\"msg\" role=\"alert\">{m}</p>"))
        .unwrap_or_default();
    page(
        "Deckjack",
        &format!(
            "<h1>Deckjack</h1>\
<p>Enter the code shown on the device.</p>{msg}\
<form method=\"post\" action=\"/login\">\
<label for=\"code\" hidden>Code</label>\
<input id=\"code\" name=\"code\" type=\"text\" inputmode=\"text\" autocomplete=\"off\" \
autocapitalize=\"characters\" spellcheck=\"false\" maxlength=\"12\" required autofocus \
placeholder=\"XXXX-XXXX\">\
<button type=\"submit\">Log in</button></form>\
<p class=\"alt\"><a href=\"/passkey\">Use a passkey instead</a></p>"
        ),
    )
}

pub fn wrong_code(left: u32) -> String {
    let attempts = if left == 1 { "attempt" } else { "attempts" };
    login(Some(&format!("Wrong code. {left} {attempts} left.")))
}

pub fn locked() -> String {
    page(
        "Deckjack: locked",
        "<h1>Locked</h1>\
<p>Too many wrong codes. Sign-in is blocked until the server is restarted \
on the host.</p>",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn login_page_has_no_script_or_external_assets() {
        // The login page gained a link to the passkey page, so the old
        // blanket `!contains("href=")` had to go.
        //
        // It has since gained two asset references as well — the manifest and
        // the apple-touch-icon, both needed for the installed iOS app — so
        // "fetches nothing" is no longer the invariant and claiming it here
        // would make this test pass for a reason that stopped being true.
        // What still holds, and is what actually matters: this page executes
        // nothing, and every reference it makes is a path on this origin.
        for html in [login(None), wrong_code(2), locked()] {
            assert!(
                !html.contains("<script"),
                "no script on a page with CSP {CSP}"
            );
            assert!(!html.contains("src="), "no asset reference: {html}");
            // An inline handler is script by another name.
            for attr in ["onclick", "onsubmit", "onload", "onfocus", "javascript:"] {
                assert!(!html.contains(attr), "inline script via {attr}");
            }
            // Every reference is a path on this origin, so nothing here can
            // be redirected by whoever controls another host.
            for scheme in ["http://", "https://", "//", "data:"] {
                assert!(
                    !html.contains(scheme),
                    "off-origin reference {scheme}: {html}"
                );
            }
        }
        // And the policy that makes the above enforced rather than merely
        // true today is still the narrow one.
        assert!(
            !CSP.contains("script-src"),
            "the ceremony belongs on its own page"
        );
        assert!(CSP.contains("default-src 'none'"));
        assert!(
            login(None).contains("href=\"/passkey\""),
            "no way to reach the ceremony"
        );
        let l = login(None);
        for attr in [
            "autocomplete=\"off\"",
            "autocapitalize=\"characters\"",
            "spellcheck=\"false\"",
            "maxlength=\"12\"",
            "name=\"code\"",
        ] {
            assert!(l.contains(attr), "missing {attr}");
        }
    }

    #[test]
    fn attempts_wording() {
        assert!(wrong_code(2).contains("2 attempts left"));
        assert!(wrong_code(1).contains("1 attempt left"));
    }
}
