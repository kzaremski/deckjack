//! An app window whose frame loads while the app is still starting.
//!
//! The shell mounts an app's frame as soon as the window exists, and the
//! window's `shell.window.open` is what starts the app, so the frame's first
//! request routinely lands in the few milliseconds before the app is ready.
//! The core answers that with its "restarting" page. Found by the browser
//! harness (web/e2e, "Open with opens the editor on that file"): the page
//! said "editor is restarting." and stayed, because nothing ever asked again.
//! `Retry-After` is a header a frame's navigation cannot act on.
//!
//! The page itself now asks the browser to load the same URL again, so the
//! frame recovers on its own once the app is up. These tests hold that
//! contract over a real socket: the refresh, that it keeps the URL (the
//! window id and the file to open are in the query), that the retry lands on
//! the app, and that a window for an app that is not coming back does not
//! spin.

use std::sync::{Arc, Mutex};

use axum::Router;
use deckjack_core::apps::{self, UiError, UiHost, WindowStatus};
use deckjack_protocol::UiAsset;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::{TcpListener, TcpStream};

const APP_PAGE: &str = "<!doctype html><p>the editor itself</p>";

/// An app host whose app is in whatever state the test sets.
struct Host {
    status: Mutex<WindowStatus>,
}

impl Host {
    fn set(&self, status: WindowStatus) {
        *self.status.lock().unwrap() = status;
    }
}

impl UiHost for Host {
    fn window_status(&self, _app: &str) -> WindowStatus {
        *self.status.lock().unwrap()
    }

    fn ui_asset(&self, _app: &str, _path: &str) -> Result<Option<UiAsset>, UiError> {
        Ok(Some(UiAsset {
            content_type: "text/html; charset=utf-8".into(),
            bytes: APP_PAGE.as_bytes().to_vec(),
            immutable: false,
        }))
    }
}

async fn serve(host: Arc<Host>) -> std::net::SocketAddr {
    let router = Router::new().nest(apps::UI_MOUNT, apps::ui_router(host as Arc<dyn UiHost>));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    addr
}

struct Reply {
    status: u16,
    head: String,
    body: String,
}

async fn get(addr: std::net::SocketAddr, path: &str) -> Reply {
    let mut stream = TcpStream::connect(addr).await.unwrap();
    stream
        .write_all(
            format!("GET {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n").as_bytes(),
        )
        .await
        .unwrap();
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).await.unwrap();
    let text = String::from_utf8_lossy(&raw).into_owned();
    let (head, body) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
    let status = head
        .split(' ')
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    Reply {
        status,
        head: head.to_ascii_lowercase(),
        body: body.to_string(),
    }
}

/// The `content` of a `<meta http-equiv="refresh">`, if the page has one.
fn refresh(body: &str) -> Option<String> {
    let lower = body.to_ascii_lowercase();
    let at = lower
        .find("http-equiv=\"refresh\"")
        .or_else(|| lower.find("http-equiv=refresh"))?;
    let tag_start = lower[..at].rfind('<')?;
    let tag_end = at + lower[at..].find('>')?;
    let tag = &lower[tag_start..tag_end];
    let content = tag.split("content=").nth(1)?;
    // Quoted: everything up to the matching quote, `; url=...` included.
    // Unquoted: up to whitespace.
    let value = match content.chars().next()? {
        q @ ('"' | '\'') => content[1..].split(q).next()?,
        _ => content.split(char::is_whitespace).next()?,
    };
    Some(value.trim().to_string())
}

const WINDOW_URL: &str = "/apps/editor/?window=w-1&path=%2Fhello.txt";

#[tokio::test]
async fn a_frame_loaded_while_the_app_starts_reloads_itself_onto_the_app() {
    let host = Arc::new(Host {
        status: Mutex::new(WindowStatus::Reconnecting),
    });
    let addr = serve(Arc::clone(&host)).await;

    let starting = get(addr, WINDOW_URL).await;
    assert_eq!(starting.status, 503, "{}", starting.head);
    assert!(
        starting.head.contains("cache-control: no-store"),
        "a cached restarting page would be served again after the app is up"
    );
    let content = refresh(&starting.body).unwrap_or_else(|| {
        panic!(
            "the restarting page does not ask the browser to try again, so a frame that \
             loaded it stays on it forever: {}",
            starting.body
        )
    });
    // No `url=`: the browser reloads the document's own URL, query included.
    // A refresh to `/apps/editor/` would drop the window id and the file the
    // editor was opened on.
    assert!(
        !content.contains("url"),
        "the refresh names a URL, so the window's query would be lost: {content}"
    );
    let delay: f64 = content
        .trim_end_matches(';')
        .parse()
        .unwrap_or_else(|_| panic!("the refresh delay is not a number: {content}"));
    assert!(
        (0.0..=5.0).contains(&delay),
        "a refresh of {delay}s leaves the window blank for longer than starting an app takes"
    );

    // The retry lands on the app once it is up: same URL, the app's page.
    host.set(WindowStatus::Live);
    let ready = get(addr, WINDOW_URL).await;
    assert_eq!(ready.status, 200);
    assert_eq!(ready.body, APP_PAGE);
}

#[tokio::test]
async fn a_frame_for_an_app_that_is_not_coming_back_does_not_reload() {
    let host = Arc::new(Host {
        status: Mutex::new(WindowStatus::Unavailable),
    });
    let addr = serve(host).await;

    let gone = get(addr, WINDOW_URL).await;
    assert_eq!(gone.status, 404);
    assert_eq!(
        refresh(&gone.body),
        None,
        "a window for an app that gave up would reload forever"
    );
}
