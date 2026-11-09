//! Login and locked pages: inline HTML and CSS, no JavaScript, no external
//! assets (§9.3). Nothing user-supplied is ever interpolated.

pub const CSP: &str = "default-src 'none'; style-src 'unsafe-inline'; form-action 'self'";

const STYLE: &str = "\
*{box-sizing:border-box}\
body{margin:0;min-height:100vh;display:flex;align-items:center;justify-content:center;\
font:16px/1.5 system-ui,sans-serif;background:#f4f4f2;color:#1c1c1c}\
main{width:min(22rem,100% - 2rem);padding:2rem;background:#fff;border-radius:12px;\
box-shadow:0 1px 3px rgba(0,0,0,.12)}\
h1{margin:0 0 1rem;font-size:1.25rem}\
p{margin:0 0 1rem}\
.msg{color:#a12a1f}\
input{width:100%;padding:.6rem .7rem;font:inherit;font-size:1.4rem;letter-spacing:.15em;\
text-transform:uppercase;font-family:ui-monospace,monospace;border:1px solid #bbb;border-radius:8px}\
button{margin-top:1rem;width:100%;padding:.6rem;font:inherit;border:0;border-radius:8px;\
background:#1c1c1c;color:#fff;cursor:pointer}\
@media (prefers-color-scheme:dark){body{background:#151515;color:#eee}\
main{background:#222;box-shadow:none}input{background:#111;color:#eee;border-color:#444}\
button{background:#eee;color:#111}.msg{color:#ff8a7a}}";

fn page(title: &str, body: &str) -> String {
    format!(
        "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\">\
<meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">\
<meta name=\"robots\" content=\"noindex\">\
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
<button type=\"submit\">Log in</button></form>"
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
        for html in [login(None), wrong_code(2), locked()] {
            assert!(!html.contains("<script"));
            assert!(!html.contains("src="));
            assert!(!html.contains("href="));
        }
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
