//! Serving the browser shell out of the binary.
//!
//! The whole web build is baked in with `rust-embed`, so a Deckjack node is
//! one file to copy and needs no Node, no `npm install` and no asset
//! directory beside it. That matters most where it is hardest: a phone in
//! Termux can host the shell without a JavaScript toolchain anywhere on it.
//!
//! In debug builds `rust-embed` reads from disk instead, so `vite dev` edits
//! appear on reload without recompiling Rust.
//!
//! # Caching
//!
//! Vite emits asset filenames containing a content hash, so an asset is
//! immutable: if its bytes change, its name changes. Those are cached for a
//! year. `index.html` is the opposite — same name forever, different contents
//! every build — so it is never cached. Getting this backwards is how a
//! browser ends up holding a stale `index.html` that asks for assets which no
//! longer exist.

use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use rust_embed::RustEmbed;

#[derive(RustEmbed)]
#[folder = "../../web/dist"]
struct Assets;

/// The entry document. Held separately because every client route resolves to
/// it and it gets different cache treatment from everything else.
pub const INDEX: &str = "index.html";

/// A year. Only ever applied to content-hashed filenames.
// `private`, not `public`: these bytes are served only behind the session
// cookie, and `public` explicitly authorizes a shared cache to store them and
// hand them to a client that has no session. Latent behind Caddy, which does
// not cache by default — but the contract this module states is that the
// build is not readable without a session, and `public` contradicts it.
const IMMUTABLE: &str = "private, max-age=31536000, immutable";

/// Whether the shell was built into this binary at all.
///
/// A release build with no `web/dist` present embeds nothing, and the
/// honest answer then is to say so rather than serve a blank page.
pub fn is_embedded() -> bool {
    Assets::get(INDEX).is_some()
}

/// Serve one path out of the embedded build.
///
/// An unknown path falls back to `index.html` rather than 404, because the
/// shell routes client-side: `/files/notes` is a view the browser resolves,
/// not a file on any server. A request that looks like an asset — it has a
/// file extension — is NOT given that treatment, so a genuinely missing
/// script 404s instead of being handed HTML that then fails to parse as
/// JavaScript, which is a famously confusing way to fail.
pub fn serve(path: &str) -> Response {
    let path = path.trim_start_matches('/');

    if let Some(file) = Assets::get(path) {
        let mime = mime_guess::from_path(path).first_or_octet_stream();
        return (
            StatusCode::OK,
            [
                (header::CONTENT_TYPE, mime.as_ref().to_string()),
                (header::CACHE_CONTROL, IMMUTABLE.to_string()),
            ],
            file.data.into_owned(),
        )
            .into_response();
    }

    let looks_like_a_file = path
        .rsplit('/')
        .next()
        .is_some_and(|name| name.contains('.'));
    if looks_like_a_file && !path.is_empty() {
        // Logged because a silent 404 on an asset is indistinguishable in a
        // browser from a page that failed to render, which has already cost
        // this project an evening once.
        tracing::warn!(path, "404: no such asset in the embedded shell");
        return (StatusCode::NOT_FOUND, "not found\n").into_response();
    }

    index()
}

/// The entry document, never cached.
pub fn index() -> Response {
    match Assets::get(INDEX) {
        Some(file) => (
            StatusCode::OK,
            [
                (header::CONTENT_TYPE, "text/html; charset=utf-8"),
                (header::CACHE_CONTROL, "no-store"),
            ],
            file.data.into_owned(),
        )
            .into_response(),
        None => (
            StatusCode::SERVICE_UNAVAILABLE,
            [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
            "<!doctype html><meta charset=utf-8><title>Deckjack</title>\
<h1>No shell in this build</h1>\
<p>The browser shell was not compiled in. Build it and rebuild:</p>\
<pre>cd web &amp;&amp; npm install &amp;&amp; npm run build\ncargo build --release</pre>",
        )
            .into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_shell_is_embedded() {
        assert!(
            is_embedded(),
            "web/dist was not built before this binary; run `npm run build` in web/"
        );
    }

    #[test]
    fn a_client_route_falls_back_to_the_document() {
        // /files/notes is a view the browser resolves, not a file here.
        let r = serve("/files/notes");
        assert_eq!(r.status(), StatusCode::OK);
    }

    #[test]
    fn a_missing_asset_404s_rather_than_returning_html() {
        // Handing HTML back for a missing .js is how you get a syntax error
        // pointing at "<" and half an hour of confusion.
        let r = serve("/assets/does-not-exist-abc123.js");
        assert_eq!(r.status(), StatusCode::NOT_FOUND);
    }

    #[test]
    fn the_document_is_never_cached() {
        let r = index();
        let cc = r.headers().get(header::CACHE_CONTROL).unwrap();
        assert_eq!(cc, "no-store");
    }
}
