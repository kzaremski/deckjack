//! The gate on cookie routes: docs/APP-UI-ISOLATION.md §4.9.
//!
//! The session cookie is `HttpOnly; SameSite=Strict`. That stops a script
//! *reading* it and stops another site's page *sending* it, and it does
//! nothing about a document on this origin that has no business holding the
//! operator's authority: an app UI in a sandboxed frame (opaque origin, so it
//! sends `Origin: null`), a working-directory file opened through `/content`,
//! or an HTML injection that tries to load one of those as script. This
//! layer answers those with a 403 before any handler runs.
//!
//! It reads the Fetch Metadata headers (`Sec-Fetch-Site`, `Sec-Fetch-Dest`).
//! Their `Sec-` prefix makes them forbidden header names: no script in any
//! browser can set or change them, so a page cannot claim to be the shell.
//! A non-browser client can set them, and that is fine — a client with the
//! cookie in hand already has the authority this gate protects.
//!
//! # The rules
//!
//! On every cookie route (everything except [`is_exempt`]):
//!
//! 1. `Origin: null` is refused.
//! 2. `Sec-Fetch-Site: cross-site` or `same-site` is refused unless
//!    `Sec-Fetch-Dest` is `document` **and** the method is `GET` or `HEAD`,
//!    so a link elsewhere still reaches the shell (which then sends it to
//!    `/login`) while a top-level form `POST` from a sibling host to
//!    `/logout` or `/passkey/*` does not. A value that is not one of the
//!    four the Fetch spec defines is refused.
//! 3. On the routes that act — `/content`, `/passkey/*` and `/logout` — a
//!    missing `Sec-Fetch-Site` is refused, with one exception: a request the
//!    browser made from an insecure context (below).
//! 4. `/ws` is rule 3 with one documented exception, below.
//! 5. `/content` refuses every `Sec-Fetch-Dest` that executes or styles —
//!    `script`, the workers, the worklets, `style`, `xslt`, `object` and
//!    `embed` — so a file in the working directory can never be loaded as the
//!    shell's script, and above all never registered as a service worker
//!    scoped to `/`. `/apps/*` refuses `serviceworker` for the same reason.
//!
//! # `/ws` and Chromium
//!
//! Measured, not assumed: Chromium 155 sends no Fetch Metadata at all on a
//! WebSocket handshake, while Firefox 158 sends `Sec-Fetch-Site`,
//! `Sec-Fetch-Mode: websocket` and `Sec-Fetch-Dest: empty` (WebKit bug 237550
//! records the same split). Failing closed on a missing header at `/ws` would
//! lock every Chrome and Android browser out of the shell. So at `/ws` alone,
//! a handshake with no `Sec-Fetch-Site` passes **only if** it carries an
//! `Origin` that names the same host as `Host`. Every browser sends `Origin`
//! on a WebSocket handshake and no script can forge it; a sandboxed frame
//! sends `null`, which rule 1 refuses first. A handshake with neither header
//! is refused.
//!
//! # Plain HTTP on a non-loopback address
//!
//! Browsers send Fetch Metadata only to potentially trustworthy URLs:
//! `https`, `localhost` and loopback addresses (Fetch Metadata §2; Chromium
//! returns early in `sec_header_helpers`). A node started with `--listen
//! 0.0.0.0:7878` and opened at `http://100.x.y.z:7878` gets no `Sec-Fetch-*`
//! header on any request, so rule 3 would lock the real shell out of
//! `/content`, sign-out and the passkey page there. So a missing
//! `Sec-Fetch-Site` on those routes passes when [`insecure_context`] says
//! the browser was never going to send one: `Host` is not loopback and
//! nothing on the request says `https` (`Origin`, `Referer`,
//! `X-Forwarded-Proto` or `Forwarded`). Those `https` signals only ever make
//! the gate stricter, so they need no proxy trust. A `POST` there must still
//! carry an `Origin` (every browser sends one on an unsafe method, and rule 1
//! has already refused `null`). A modern browser in a
//! secure context always sends Fetch Metadata, so it never reaches this
//! branch at all. What is lost on such a node is everything the browser did
//! not say: rule 5 cannot see a destination it was not told. Rule 1 and the
//! handlers' own `Origin` checks still apply. Serve it over HTTPS (for
//! example `tailscale serve`) to get the whole gate.
//!
//! # Non-browser clients
//!
//! Nothing in the product calls a cookie route except the browser shell: the
//! CLI talks to the working directory (`deckjack install`, `apps`) or to
//! `/unlock` (`deckjack unlock`), which carries no cookie authority and is
//! exempt, and agents use the MCP socket. A script driving a `--debug` node
//! with `debug-session.cookie` must say what a browser says:
//! `Sec-Fetch-Site: same-origin` on `/content`, `/passkey/*` and `/logout`,
//! and either that or `Origin: http://<host>` on `/ws`.
//!
//! # What this does not do
//!
//! It is not the cross-origin check. `/ws`, `/logout`, `/login` and the
//! passkey routes still compare `Origin` with `Host` themselves; this layer
//! is a second, independent wall, and removing either must not open the
//! other. It does not depend on a browser withholding SameSite=Strict
//! cookies from a sandboxed frame — WebKit's behavior there is unverified.

use axum::extract::Request;
use axum::http::{HeaderMap, Method, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

/// The marker every refusal's body starts with, so a test (or a person
/// reading a 403 in devtools) can tell this layer's answer from a handler's.
pub const REFUSED: &str = "refused by the fetch gate";

/// Why a request was refused. The text goes in the 403 body and the log
/// line, never anything the client sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    /// `Origin: null`: an opaque origin, such as a sandboxed frame.
    NullOrigin,
    /// `Sec-Fetch-Site` was `cross-site` or `same-site` on a subresource.
    CrossSite,
    /// `Sec-Fetch-Site` was missing where it is required.
    NoFetchMetadata,
    /// `Sec-Fetch-Site` was present but not a value the spec defines, or not
    /// readable as text.
    UnknownSite,
    /// The request would load a file as something that runs or styles.
    ExecutableDestination,
}

impl Refusal {
    pub fn as_str(self) -> &'static str {
        match self {
            Refusal::NullOrigin => "an opaque origin may not use the session",
            Refusal::CrossSite => "another site may not use the session",
            Refusal::NoFetchMetadata => "this route requires Sec-Fetch-Site",
            Refusal::UnknownSite => "Sec-Fetch-Site has a value this node does not know",
            Refusal::ExecutableDestination => "this route cannot be loaded as script or style",
        }
    }
}

/// Routes that carry no cookie authority, so the gate does not apply.
///
/// A closed list rather than a prefix match: everything not named here is a
/// cookie route and is gated, so a route added later is protected by
/// default instead of by someone remembering.
///
/// - `/login` issues the cookie; it does not honor one. Its POST already
///   checks `Origin` against `Host`.
/// - `/unlock` and `/healthz` are the lockdown carve-outs; `deckjack unlock`
///   posts to `/unlock` from the host and sends no Fetch Metadata.
/// - The install assets are readable without a session by design (see
///   `http::is_public_install_asset`).
///
/// Stage 4's capability route (`/apps/{app}/~{cap}/`) and the public `/kit/`
/// are loaded from an opaque origin and must be added here when they land,
/// or rule 1 refuses every asset a sandboxed frame asks for.
pub fn is_exempt(path: &str) -> bool {
    path == "/login"
        || under(path, "/unlock")
        || under(path, "/healthz")
        || crate::http::is_public_install_asset(path)
}

/// `path` is `prefix` itself or something beneath it — on a segment
/// boundary, so `/contents` is not `/content`.
fn under(path: &str, prefix: &str) -> bool {
    path == prefix
        || path
            .strip_prefix(prefix)
            .is_some_and(|rest| rest.starts_with('/'))
}

/// The routes where a missing `Sec-Fetch-Site` fails closed (rule 3).
fn requires_fetch_metadata(path: &str) -> bool {
    under(path, crate::content::MOUNT) || under(path, "/passkey") || under(path, "/logout")
}

/// Destinations that execute or style what they load (rule 5).
const EXECUTABLE: &[&str] = &[
    "script",
    "worker",
    "sharedworker",
    "serviceworker",
    "audioworklet",
    "paintworklet",
    "style",
    "xslt",
    "object",
    "embed",
];

/// A header's trimmed text: `None` when absent, `Err` when not visible ASCII.
fn header_text(headers: &HeaderMap, name: header::HeaderName) -> Option<Result<&str, ()>> {
    headers
        .get(name)
        .map(|v| v.to_str().map(str::trim).map_err(|_| ()))
}

/// A present `Origin` naming the same host as `Host`. Absent, unreadable or
/// `null` is not a match.
fn origin_matches_host(headers: &HeaderMap) -> bool {
    let (Some(Ok(origin)), Some(Ok(host))) = (
        header_text(headers, header::ORIGIN),
        header_text(headers, header::HOST),
    ) else {
        return false;
    };
    origin
        .strip_prefix("https://")
        .or_else(|| origin.strip_prefix("http://"))
        .is_some_and(|authority| authority.eq_ignore_ascii_case(host))
}

/// `Host` names a loopback address or `localhost`, which a browser treats as
/// potentially trustworthy even over plain HTTP. Absent or unreadable is not
/// loopback.
fn host_is_loopback(headers: &HeaderMap) -> bool {
    let Some(Ok(host)) = header_text(headers, header::HOST) else {
        return false;
    };
    let name = if let Some(rest) = host.strip_prefix('[') {
        rest.split(']').next().unwrap_or("")
    } else {
        match host.rsplit_once(':') {
            Some((name, port)) if port.bytes().all(|b| b.is_ascii_digit()) => name,
            _ => host,
        }
    };
    let name = name.trim_end_matches('.').to_ascii_lowercase();
    name == "localhost"
        || name.ends_with(".localhost")
        || name
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
}

/// Anything on the request says the browser reached this node over HTTPS.
fn says_https(headers: &HeaderMap) -> bool {
    let starts_https = |name: header::HeaderName| {
        headers.get_all(name).iter().any(|v| {
            v.as_bytes()
                .trim_ascii_start()
                .get(..8)
                .is_some_and(|p| p.eq_ignore_ascii_case(b"https://"))
        })
    };
    let lists_https = |name: &str, needle: &[u8]| {
        headers.get_all(name).iter().any(|v| {
            v.as_bytes()
                .split(|&b| b == b',' || b == b';')
                .any(|part| part.trim_ascii().eq_ignore_ascii_case(needle))
        })
    };
    starts_https(header::ORIGIN)
        || starts_https(header::REFERER)
        || lists_https("x-forwarded-proto", b"https")
        || lists_https("forwarded", b"proto=https")
}

/// The browser was in an insecure context, so it was never going to send
/// Fetch Metadata: plain HTTP to a host that is not loopback. See the
/// module doc.
pub fn insecure_context(headers: &HeaderMap) -> bool {
    headers.contains_key(header::HOST) && !host_is_loopback(headers) && !says_https(headers)
}

/// The whole decision, with no I/O, so every rule can be pinned directly.
pub fn verdict(method: &Method, path: &str, headers: &HeaderMap) -> Result<(), Refusal> {
    if is_exempt(path) {
        return Ok(());
    }

    // Rule 1. Every `Origin` line, not just the first: a request that sends
    // two has no business being believed about either.
    if headers
        .get_all(header::ORIGIN)
        .iter()
        .any(|v| v.as_bytes().trim_ascii().eq_ignore_ascii_case(b"null"))
    {
        return Err(Refusal::NullOrigin);
    }

    let dest = match header_text(
        headers,
        axum::http::HeaderName::from_static("sec-fetch-dest"),
    ) {
        Some(Ok(d)) => Some(d.to_ascii_lowercase()),
        Some(Err(())) => return Err(Refusal::UnknownSite),
        None => None,
    };

    // Rule 5, before the site check: a same-origin `<script src=/content…>`
    // is exactly the injection this exists for.
    if let Some(d) = dest.as_deref() {
        if under(path, crate::content::MOUNT) && EXECUTABLE.contains(&d) {
            return Err(Refusal::ExecutableDestination);
        }
        if under(path, crate::apps::UI_MOUNT) && d == "serviceworker" {
            return Err(Refusal::ExecutableDestination);
        }
    }

    match header_text(
        headers,
        axum::http::HeaderName::from_static("sec-fetch-site"),
    ) {
        Some(Ok(site)) => {
            let site = site.to_ascii_lowercase();
            match site.as_str() {
                "same-origin" | "none" => Ok(()),
                // Rule 2.
                "cross-site" | "same-site" => {
                    let navigation = method == Method::GET || method == Method::HEAD;
                    if navigation && dest.as_deref() == Some("document") {
                        Ok(())
                    } else {
                        Err(Refusal::CrossSite)
                    }
                }
                _ => Err(Refusal::UnknownSite),
            }
        }
        Some(Err(())) => Err(Refusal::UnknownSite),
        None => {
            if requires_fetch_metadata(path) {
                // Rule 3, unless the browser could not have sent it. Even
                // then a browser sends `Origin` on every unsafe method, so a
                // POST without one is not a browser.
                let safe = method == Method::GET || method == Method::HEAD;
                if insecure_context(headers) && (safe || headers.contains_key(header::ORIGIN)) {
                    Ok(())
                } else {
                    Err(Refusal::NoFetchMetadata)
                }
            } else if under(path, crate::ws::PATH) {
                // Rule 4: Chromium sends no Fetch Metadata on a handshake,
                // so a same-host `Origin` stands in for it. See the module
                // doc for the measurement.
                if origin_matches_host(headers) {
                    Ok(())
                } else {
                    Err(Refusal::NoFetchMetadata)
                }
            } else {
                // The shell document, its assets and `/apps/*`: an engine
                // without Fetch Metadata still gets the shell, which cannot
                // do anything without `/ws` anyway.
                Ok(())
            }
        }
    }
}

/// The layer. Mounted once in [`crate::wiring::app`], outside every route
/// and inside the lockdown gate:
/// `.layer(axum::middleware::from_fn(fetchgate::gate))`.
pub async fn gate(request: Request, next: Next) -> Response {
    match verdict(request.method(), request.uri().path(), request.headers()) {
        Ok(()) => next.run(request).await,
        Err(why) => {
            let headers = request.headers();
            let text = |name: &str| {
                headers
                    .get(name)
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("-")
                    .chars()
                    .take(64)
                    .collect::<String>()
            };
            // Logged at warn because the one failure this layer can cause on
            // its own is locking a real browser out — WebKit's Fetch Metadata
            // has had wrong values before (bugs 256472, 260284) — and that
            // has to be diagnosable from the host's log.
            tracing::warn!(
                path = %request.uri().path(),
                reason = why.as_str(),
                site = %text("sec-fetch-site"),
                dest = %text("sec-fetch-dest"),
                mode = %text("sec-fetch-mode"),
                origin = %text("origin"),
                "refused by the fetch gate"
            );
            (
                StatusCode::FORBIDDEN,
                [
                    (header::CONTENT_TYPE, "text/plain; charset=utf-8"),
                    (header::CACHE_CONTROL, "no-store"),
                ],
                format!("{REFUSED}: {}\n", why.as_str()),
            )
                .into_response()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The decision for a `GET`, which is what most rules are about.
    fn v(path: &str, headers: &HeaderMap) -> Result<(), Refusal> {
        verdict(&Method::GET, path, headers)
    }

    fn h(pairs: &[(&str, &str)]) -> HeaderMap {
        at("node.example", pairs)
    }

    /// Headers as a browser sends them to `host`.
    fn at(host: &str, pairs: &[(&str, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        map.insert(header::HOST, host.parse().unwrap());
        for (k, v) in pairs {
            map.append(
                axum::http::HeaderName::from_bytes(k.as_bytes()).unwrap(),
                v.parse().unwrap(),
            );
        }
        map
    }

    const SAME: (&str, &str) = ("sec-fetch-site", "same-origin");

    #[test]
    fn a_same_origin_request_passes_every_cookie_route() {
        for path in [
            "/",
            "/assets/x.js",
            "/ws",
            "/content",
            "/passkey/credentials/remove",
            "/logout",
            "/apps/notes/",
            "/api/x",
        ] {
            assert_eq!(v(path, &h(&[SAME])), Ok(()), "{path}");
        }
        // A user typing the URL, or the PWA launching: `none`.
        assert_eq!(v("/content", &h(&[("sec-fetch-site", "none")])), Ok(()));
    }

    #[test]
    fn a_null_origin_is_refused_everywhere_gated_even_when_the_site_says_same_origin() {
        for path in [
            "/",
            "/assets/x.js",
            "/ws",
            "/content",
            "/passkey",
            "/logout",
            "/apps/notes/x.js",
        ] {
            assert_eq!(
                v(path, &h(&[SAME, ("origin", "null")])),
                Err(Refusal::NullOrigin),
                "{path}"
            );
        }
        // A second Origin line does not launder the first.
        assert_eq!(
            v(
                "/ws",
                &h(&[SAME, ("origin", "http://node.example"), ("origin", "null")])
            ),
            Err(Refusal::NullOrigin)
        );
    }

    #[test]
    fn another_site_may_navigate_to_a_document_and_nothing_else() {
        for site in ["cross-site", "same-site"] {
            assert_eq!(
                v(
                    "/",
                    &h(&[("sec-fetch-site", site), ("sec-fetch-dest", "document")])
                ),
                Ok(()),
                "a link from {site} must still reach the shell"
            );
            for dest in ["empty", "iframe", "image", "script", "websocket"] {
                assert_eq!(
                    v(
                        "/passkey/credentials/remove",
                        &h(&[("sec-fetch-site", site), ("sec-fetch-dest", dest)])
                    ),
                    Err(Refusal::CrossSite),
                    "{site} {dest}"
                );
            }
            assert_eq!(
                v(
                    "/",
                    &h(&[("sec-fetch-site", site), ("sec-fetch-dest", "empty")])
                ),
                Err(Refusal::CrossSite)
            );
        }
        assert_eq!(
            v("/", &h(&[("sec-fetch-site", "sideways")])),
            Err(Refusal::UnknownSite)
        );
    }

    #[test]
    fn a_top_level_form_post_from_another_site_is_not_a_navigation() {
        // `<form method=post action=/logout>` auto-submitted at top level by
        // a sibling host: dest is `document`, and the Strict cookie rides
        // along because the request is same-site.
        for site in ["cross-site", "same-site"] {
            let doc = h(&[("sec-fetch-site", site), ("sec-fetch-dest", "document")]);
            for path in ["/logout", "/passkey/credentials/remove", "/"] {
                for method in [Method::POST, Method::PUT, Method::DELETE] {
                    assert_eq!(
                        verdict(&method, path, &doc),
                        Err(Refusal::CrossSite),
                        "{site} {method} {path}"
                    );
                }
            }
            assert_eq!(verdict(&Method::HEAD, "/", &doc), Ok(()), "{site} HEAD");
        }
        // The shell's own sign-out is a same-origin POST.
        assert_eq!(verdict(&Method::POST, "/logout", &h(&[SAME])), Ok(()));
    }

    const ACTING: [&str; 4] = [
        "/content",
        "/passkey",
        "/passkey/credentials/remove",
        "/logout",
    ];

    #[test]
    fn a_missing_site_fails_closed_on_the_routes_that_act() {
        // Every case here is one where the browser would have sent Fetch
        // Metadata: a loopback host, or a page it reached over HTTPS.
        let secure: [(&str, &[(&str, &str)]); 9] = [
            ("127.0.0.1:7878", &[]),
            ("127.0.0.1:7878", &[("origin", "http://127.0.0.1:7878")]),
            ("localhost:7878", &[]),
            ("app.localhost", &[]),
            ("[::1]:7878", &[]),
            ("node.example", &[("origin", "https://node.example")]),
            ("node.example", &[("referer", "https://node.example/")]),
            ("node.example", &[("x-forwarded-proto", "https")]),
            ("node.example", &[("forwarded", "for=1.2.3.4;proto=https")]),
        ];
        for path in ACTING {
            for (host, pairs) in secure {
                assert_eq!(
                    v(path, &at(host, pairs)),
                    Err(Refusal::NoFetchMetadata),
                    "{path} at {host} {pairs:?}"
                );
            }
            assert_eq!(
                verdict(&Method::POST, path, &at("127.0.0.1:7878", &[])),
                Err(Refusal::NoFetchMetadata),
                "POST {path}"
            );
        }
        // No Host at all is not evidence of anything: closed.
        assert_eq!(
            v("/content", &HeaderMap::new()),
            Err(Refusal::NoFetchMetadata)
        );
        // The shell document stays reachable for an engine without it.
        assert_eq!(v("/", &at("127.0.0.1:7878", &[])), Ok(()));
    }

    #[test]
    fn a_browser_on_plain_http_to_a_remote_host_sends_no_fetch_metadata_and_still_works() {
        // Measured: Chromium sends no `Sec-Fetch-*` to http://<tailscale ip>,
        // on the document, an `<img>` or a `fetch`. These are the shapes the
        // real shell sends there.
        let insecure: [(&str, &[(&str, &str)]); 4] = [
            ("100.70.30.57:7878", &[]),
            (
                "100.70.30.57:7878",
                &[("referer", "http://100.70.30.57:7878/")],
            ),
            ("node.example", &[("origin", "http://node.example")]),
            ("node.example", &[("x-forwarded-proto", "http")]),
        ];
        for path in ACTING {
            for (host, pairs) in insecure {
                assert_eq!(
                    v(path, &at(host, pairs)),
                    Ok(()),
                    "{path} at {host} {pairs:?}"
                );
                let mut with_origin = pairs.to_vec();
                if !pairs.iter().any(|(k, _)| *k == "origin") {
                    with_origin.push(("origin", "http://node.example"));
                }
                assert_eq!(
                    verdict(&Method::POST, path, &at(host, &with_origin)),
                    Ok(()),
                    "POST {path} at {host}"
                );
            }
            // A POST with no `Origin` is not a browser, insecure or not.
            assert_eq!(
                verdict(&Method::POST, path, &at("100.70.30.57:7878", &[])),
                Err(Refusal::NoFetchMetadata),
                "POST {path} without Origin"
            );
            // Rule 1 still holds there: a sandboxed frame's fetch says null.
            assert_eq!(
                v(path, &at("100.70.30.57:7878", &[("origin", "null")])),
                Err(Refusal::NullOrigin)
            );
        }
    }

    #[test]
    fn a_socket_without_fetch_metadata_needs_a_same_host_origin() {
        assert_eq!(v("/ws", &h(&[("origin", "http://node.example")])), Ok(()));
        assert_eq!(v("/ws", &h(&[("origin", "https://NODE.example")])), Ok(()));
        assert_eq!(v("/ws", &h(&[])), Err(Refusal::NoFetchMetadata));
        assert_eq!(
            v("/ws", &h(&[("origin", "https://evil.example")])),
            Err(Refusal::NoFetchMetadata)
        );
        // Same host, another port is another origin: some other local
        // service's page, which Chromium lets open a socket with no
        // Fetch Metadata.
        for (host, origin) in [
            ("127.0.0.1:7878", "http://127.0.0.1:3000"),
            ("127.0.0.1:7878", "http://127.0.0.1"),
            ("node.example", "http://node.example:8080"),
            ("node.example", "http://node.example.evil.example"),
        ] {
            assert_eq!(
                v("/ws", &at(host, &[("origin", origin)])),
                Err(Refusal::NoFetchMetadata),
                "{origin} for {host}"
            );
        }
        assert_eq!(
            v(
                "/ws",
                &at("127.0.0.1:7878", &[("origin", "http://127.0.0.1:7878")])
            ),
            Ok(())
        );
        // Firefox's handshake.
        assert_eq!(
            v(
                "/ws",
                &h(&[
                    SAME,
                    ("sec-fetch-dest", "empty"),
                    ("origin", "http://node.example")
                ])
            ),
            Ok(())
        );
    }

    #[test]
    fn content_cannot_be_loaded_as_anything_that_runs() {
        // Literals, not `EXECUTABLE`: dropping an entry from the list must
        // fail here, not delete its own assertion.
        for dest in [
            "script",
            "worker",
            "sharedworker",
            "serviceworker",
            "audioworklet",
            "paintworklet",
            "style",
            "xslt",
            "object",
            "embed",
            "ServiceWorker",
        ] {
            assert_eq!(
                v("/content", &h(&[SAME, ("sec-fetch-dest", dest)])),
                Err(Refusal::ExecutableDestination),
                "{dest}"
            );
        }
        for dest in ["empty", "image", "video", "audio", "iframe", "document"] {
            assert_eq!(
                v("/content", &h(&[SAME, ("sec-fetch-dest", dest)])),
                Ok(()),
                "{dest}"
            );
        }
        assert_eq!(
            v(
                "/apps/notes/sw.js",
                &h(&[SAME, ("sec-fetch-dest", "serviceworker")])
            ),
            Err(Refusal::ExecutableDestination)
        );
        // The shell's own service worker is the shell's.
        assert_eq!(
            v("/sw.js", &h(&[SAME, ("sec-fetch-dest", "serviceworker")])),
            Ok(())
        );
    }

    #[test]
    fn the_exempt_routes_are_exactly_the_ones_without_cookie_authority() {
        for path in [
            "/login",
            "/unlock",
            "/healthz",
            "/manifest.webmanifest",
            "/icons/a.png",
        ] {
            assert_eq!(v(path, &h(&[("origin", "null")])), Ok(()), "{path}");
        }
        // Segment boundaries: near misses are gated.
        for path in [
            "/loginx",
            "/login/x",
            "/contents",
            "/wsx",
            "/logouts",
            "/passkeys",
        ] {
            assert!(!is_exempt(path), "{path}");
        }
        assert_eq!(
            v("/login/x", &h(&[("origin", "null")])),
            Err(Refusal::NullOrigin)
        );
        assert!(under("/content", "/content") && under("/content/x", "/content"));
        assert!(!under("/contents", "/content"));
    }
}
