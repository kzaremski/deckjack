//! The content route: how file bytes reach the browser.
//!
//! Until this existed the only way to see a file was `files.read`, which
//! returns the content inside a JSON command response — base64 when it is not
//! valid UTF-8 — capped at 16 MiB. So a photograph, a song, a video or a PDF
//! could not be shown at all, and `files.rs` said so in a comment for as long
//! as the cap existed: "anything bigger than this should be streamed from
//! `Resolved::real_path` instead of buffered."
//!
//! These tests are over a real socket, with real headers, because almost every
//! promise here *is* a header: a browser seeks by sending `Range` and believes
//! `Accept-Ranges`, and refuses to run a script in this origin because of
//! `Content-Security-Policy`. Asserting on a handler's return value would
//! prove none of that.

use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use deckjack_core::auth::code::Code;
use deckjack_core::auth::session::{COOKIE_NAME, Session, SessionId};
use deckjack_core::auth::state::{AuthState, Limits, Now};
use deckjack_core::files;
use deckjack_core::http::Ctx;
use deckjack_core::store::Store;

const NODE_ID: &str = "test-node";

/// A response, with the body kept as bytes. A range of a JPEG is not a string.
struct Reply {
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Reply {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).to_string()
    }
}

struct Harness {
    addr: SocketAddr,
    dir: tempfile::TempDir,
    state: Arc<Mutex<AuthState>>,
    store: Arc<Mutex<Store>>,
}

impl Harness {
    async fn start() -> Harness {
        let dir = tempfile::tempdir().expect("a working directory");
        let store = Arc::new(Mutex::new(
            Store::open(dir.path()).expect("the state database opens"),
        ));
        let state = Arc::new(Mutex::new(AuthState::new(
            Limits {
                per_ip_attempts: 3,
                global_attempts: 10,
                session_max: Duration::from_secs(3600),
                max_sessions: 4,
            },
            Code::generate().expect("a sign-in code"),
            Now::current(),
        )));
        let ctx = Arc::new(Ctx::new(
            Arc::clone(&state),
            Arc::clone(&store),
            Duration::ZERO,
            Vec::new(),
            NODE_ID.to_string(),
        ));
        let ns = Arc::new(files::Namespace::new(
            NODE_ID.parse().expect("the node name parses"),
            files::Root::open(dir.path()).expect("the root opens"),
        ));

        let app = deckjack_core::content::router(Arc::clone(&ctx), ns);
        let tcp = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("a loopback port");
        let addr = tcp.local_addr().expect("the bound address");
        tokio::spawn(async move {
            let _ = axum::serve(tcp, app.into_make_service_with_connect_info::<SocketAddr>()).await;
        });
        Harness {
            addr,
            dir,
            state,
            store,
        }
    }

    fn write(&self, rel: &str, bytes: &[u8]) {
        let path = self.dir.path().join(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("the parent directory");
        }
        std::fs::write(path, bytes).expect("the fixture file writes");
    }

    /// A session as the one-time code path leaves it.
    fn session(&self) -> SessionId {
        let id = SessionId::generate().expect("a session id");
        let now = SystemTime::now();
        let ip: IpAddr = "127.0.0.1".parse().unwrap();
        let session = Session::new(ip, now, now + Duration::from_secs(3600));
        self.state
            .lock()
            .unwrap()
            .restore_session(id.hash(), session.clone());
        self.store
            .lock()
            .unwrap()
            .insert_session(&id.hash(), &session)
            .expect("the session row writes");
        id
    }

    async fn get(&self, path: &str, headers: &[(&str, &str)]) -> Reply {
        request(self.addr, path, headers).await
    }

    /// Signed in, which is the ordinary case for every route here.
    async fn signed_get(&self, path: &str, extra: &[(&str, &str)]) -> Reply {
        let cookie = format!("{COOKIE_NAME}={}", self.session().encode());
        let mut headers = vec![("Cookie", cookie.as_str())];
        headers.extend_from_slice(extra);
        request(self.addr, path, &headers).await
    }
}

async fn request(addr: SocketAddr, path: &str, headers: &[(&str, &str)]) -> Reply {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    let mut text = format!("GET {path} HTTP/1.1\r\nHost: deckjack.test\r\nConnection: close\r\n");
    for (k, v) in headers {
        text.push_str(&format!("{k}: {v}\r\n"));
    }
    text.push_str("\r\n");

    let mut stream = tokio::net::TcpStream::connect(addr)
        .await
        .expect("the server is listening");
    stream
        .write_all(text.as_bytes())
        .await
        .expect("the request writes");
    let mut raw = Vec::new();
    stream
        .read_to_end(&mut raw)
        .await
        .expect("the response reads");

    let split = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .expect("a header terminator");
    let head = String::from_utf8_lossy(&raw[..split]).to_string();
    let body = raw[split + 4..].to_vec();

    let mut lines = head.lines();
    let status = lines
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|s| s.parse().ok())
        .expect("a status code");
    let headers = lines
        .filter_map(|l| l.split_once(':'))
        .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
        .collect();

    Reply {
        status,
        headers,
        body,
    }
}

// ---------------------------------------------------------------------------
// what a reader gets
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_file_arrives_as_its_own_bytes_and_not_as_json() {
    let h = Harness::start().await;
    h.write("hello.txt", b"the quick brown fox");

    let r = h.signed_get("/content?path=/hello.txt", &[]).await;

    assert_eq!(r.status, 200, "{}", r.text());
    assert_eq!(r.body, b"the quick brown fox");
    assert!(
        r.header("content-type")
            .unwrap_or("")
            .starts_with("text/plain"),
        "content-type was {:?}",
        r.header("content-type")
    );
}

#[tokio::test]
async fn the_type_is_the_files_own_so_a_browser_can_pick_a_viewer() {
    // The whole point of this route over `files.read`: the browser decodes the
    // image, so it has to be told what it is holding.
    let h = Harness::start().await;
    h.write("a.png", b"\x89PNG\r\n\x1a\nnot really");
    h.write("b.pdf", b"%PDF-1.7 not really");

    assert!(
        h.signed_get("/content?path=/a.png", &[])
            .await
            .header("content-type")
            .unwrap_or("")
            .starts_with("image/png")
    );
    assert!(
        h.signed_get("/content?path=/b.pdf", &[])
            .await
            .header("content-type")
            .unwrap_or("")
            .starts_with("application/pdf")
    );
}

#[tokio::test]
async fn nothing_is_served_to_a_caller_with_no_session() {
    // This route reads the operator's whole working directory. It is the one
    // thing on the node that must never join the pre-auth carve-out.
    let h = Harness::start().await;
    h.write("secret.txt", b"private");

    let r = h.get("/content?path=/secret.txt", &[]).await;

    assert_ne!(r.status, 200, "a file was served without a session");
    assert!(
        !r.text().contains("private"),
        "the bytes leaked in the refusal: {}",
        r.text()
    );
}

// ---------------------------------------------------------------------------
// seeking, which is what makes audio and video usable at all
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_browser_is_told_it_may_seek() {
    let h = Harness::start().await;
    h.write("song.mp3", b"0123456789");

    let r = h.signed_get("/content?path=/song.mp3", &[]).await;

    assert_eq!(
        r.header("accept-ranges"),
        Some("bytes"),
        "without this a browser will not offer a scrubber at all"
    );
}

#[tokio::test]
async fn a_range_is_answered_with_that_range_and_nothing_else() {
    let h = Harness::start().await;
    h.write("song.mp3", b"0123456789");

    let r = h
        .signed_get("/content?path=/song.mp3", &[("Range", "bytes=2-5")])
        .await;

    assert_eq!(r.status, 206, "a range must be a partial response");
    assert_eq!(r.body, b"2345");
    assert_eq!(r.header("content-range"), Some("bytes 2-5/10"));
    assert_eq!(r.header("content-length"), Some("4"));
}

#[tokio::test]
async fn an_open_ended_range_runs_to_the_end_of_the_file() {
    // How a player resumes: "give me everything from here".
    let h = Harness::start().await;
    h.write("clip.mp4", b"0123456789");

    let r = h
        .signed_get("/content?path=/clip.mp4", &[("Range", "bytes=7-")])
        .await;

    assert_eq!(r.status, 206);
    assert_eq!(r.body, b"789");
    assert_eq!(r.header("content-range"), Some("bytes 7-9/10"));
}

#[tokio::test]
async fn a_range_past_the_end_is_refused_rather_than_quietly_answered_in_full() {
    // Answering a bad range with the whole file is how a player ends up
    // decoding a header where it expected the middle of a stream.
    let h = Harness::start().await;
    h.write("clip.mp4", b"0123456789");

    let r = h
        .signed_get("/content?path=/clip.mp4", &[("Range", "bytes=99-200")])
        .await;

    assert_eq!(r.status, 416, "body was {:?}", r.text());
    assert_eq!(r.header("content-range"), Some("bytes */10"));
}

#[tokio::test]
async fn an_unchanged_file_is_not_sent_a_second_time() {
    let h = Harness::start().await;
    h.write("big.bin", b"0123456789");

    let first = h.signed_get("/content?path=/big.bin", &[]).await;
    let tag = first.header("etag").expect("an entity tag").to_string();

    let second = h
        .signed_get("/content?path=/big.bin", &[("If-None-Match", &tag)])
        .await;

    assert_eq!(second.status, 304, "the bytes were sent again");
    assert!(second.body.is_empty());
}

// ---------------------------------------------------------------------------
// what it refuses
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_hidden_state_directory_is_never_served() {
    // `.deckjack/state.db` holds session hashes, grants and the audit log.
    let h = Harness::start().await;
    h.write(".deckjack/state.db", b"sqlite pretend");

    for path in [
        "/content?path=/.deckjack/state.db",
        "/content?path=/.DeckJack/state.db",
    ] {
        let r = h.signed_get(path, &[]).await;
        assert_ne!(r.status, 200, "{path} was served");
        assert!(!r.body.starts_with(b"sqlite"), "{path} leaked bytes");
    }
}

#[tokio::test]
async fn a_path_that_climbs_out_of_the_working_directory_is_refused() {
    let h = Harness::start().await;

    for path in [
        "/content?path=/../../etc/passwd",
        "/content?path=/a/../../../etc/passwd",
    ] {
        let r = h.signed_get(path, &[]).await;
        assert_ne!(r.status, 200, "{path} was served");
    }
}

#[tokio::test]
async fn a_directory_is_not_a_document() {
    let h = Harness::start().await;
    h.write("folder/inside.txt", b"x");

    let r = h.signed_get("/content?path=/folder", &[]).await;

    assert_ne!(r.status, 200);
    assert!(
        !r.text().is_empty(),
        "a refusal with no reason is a blank frame for the operator"
    );
}

#[tokio::test]
async fn a_missing_file_says_so() {
    let h = Harness::start().await;
    let r = h.signed_get("/content?path=/nope.txt", &[]).await;
    assert_eq!(r.status, 404, "{}", r.text());
}

// ---------------------------------------------------------------------------
// the sharpest edge: previewing the operator's own files on the shell's origin
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_served_file_can_never_run_script_in_the_shells_origin() {
    // An HTML file or an SVG in the working directory is attacker-authored as
    // far as this route is concerned -- it could have arrived by download,
    // email or sync. Served inline on the shell's own origin with no policy,
    // opening one in a preview would run its script with the operator's
    // session cookie. `sandbox` with no allow tokens puts the response in an
    // opaque origin with scripting off, and `nosniff` stops a text/plain file
    // being re-interpreted as HTML.
    let h = Harness::start().await;
    h.write("evil.html", b"<script>fetch('/api/steal')</script>");
    h.write(
        "evil.svg",
        b"<svg xmlns='http://www.w3.org/2000/svg'><script/></svg>",
    );

    for path in ["/content?path=/evil.html", "/content?path=/evil.svg"] {
        let r = h.signed_get(path, &[]).await;
        assert_eq!(r.status, 200, "{path}: {}", r.text());
        let csp = r.header("content-security-policy").unwrap_or("");
        assert!(
            csp.contains("sandbox"),
            "{path} was served without a sandbox policy: {csp:?}"
        );
        assert!(
            !csp.contains("allow-scripts") && !csp.contains("allow-same-origin"),
            "{path}'s sandbox let script back in: {csp:?}"
        );
        assert_eq!(
            r.header("x-content-type-options"),
            Some("nosniff"),
            "{path} could be re-sniffed into something executable"
        );
    }
}

#[tokio::test]
async fn the_operators_files_are_never_stored_by_a_shared_cache() {
    let h = Harness::start().await;
    h.write("payslip.pdf", b"%PDF");

    let cache = h
        .signed_get("/content?path=/payslip.pdf", &[])
        .await
        .header("cache-control")
        .unwrap_or("")
        .to_string();

    assert!(
        cache.contains("private") || cache.contains("no-store"),
        "a proxy was authorized to keep the operator's file: {cache:?}"
    );
    assert!(
        !cache.contains("public"),
        "`public` hands this to any shared cache: {cache:?}"
    );
}

#[tokio::test]
async fn a_download_is_offered_as_a_file_with_its_own_name() {
    let h = Harness::start().await;
    h.write("report.pdf", b"%PDF");

    let r = h
        .signed_get("/content?path=/report.pdf&download=1", &[])
        .await;

    let disposition = r.header("content-disposition").unwrap_or("");
    assert!(
        disposition.starts_with("attachment"),
        "a download was served inline: {disposition:?}"
    );
    assert!(
        disposition.contains("report.pdf"),
        "the browser would invent a filename: {disposition:?}"
    );
}

#[tokio::test]
async fn a_preview_is_shown_in_place_rather_than_downloaded() {
    // The pair of the test above. A Quick Look that downloads instead of
    // showing is not a Quick Look.
    let h = Harness::start().await;
    h.write("holiday.png", b"\x89PNG");

    let r = h.signed_get("/content?path=/holiday.png", &[]).await;

    let disposition = r.header("content-disposition").unwrap_or("inline");
    assert!(
        disposition.starts_with("inline"),
        "an image preview was sent as a download: {disposition:?}"
    );
}
