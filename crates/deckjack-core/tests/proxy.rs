//! The proxy socket, over real sockets (D2, docs/APP-UI-ISOLATION.md §5
//! stage 0).
//!
//! One front door served two ways, as `main.rs` serves it: on TCP with the
//! peer address as connect info, and on a Unix socket bound by `proxy::bind`
//! and wrapped in `proxy::router`. Most tests build that composition here;
//! [`the_binary_serves_the_proxy_socket_it_is_configured_with`] runs the real
//! `deckjack` binary, so `main.rs`'s own wiring is held to the same answers. `trusted_proxies` is empty
//! throughout, so the only thing that can make a forwarded header count is
//! which socket the request came in on.
//!
//! What is observed is what the product does with the address: a session is
//! bound to the address it signed in from, so a cookie that works from one
//! `X-Forwarded-For` and not from another proves which address it was
//! attributed to; and wrong codes block an address, so which address is
//! blocked afterwards proves the same for a failure.

use std::net::{IpAddr, SocketAddr};
use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream, UnixStream};

use deckjack_core::auth::Code;
use deckjack_core::auth::state::{AuthState, Limits, Now, Refusal};
use deckjack_core::http::{self, Ctx};
use deckjack_core::proxy;
use deckjack_core::store::Store;

const TAILNET_CLIENT: &str = "100.64.0.9";
const OTHER_CLIENT: &str = "100.64.0.10";
const SITE: &str = "pocket.example.ts.net";

struct Harness {
    tcp: SocketAddr,
    unix: PathBuf,
    code: String,
    ctx: Arc<Ctx>,
    _dir: tempfile::TempDir,
}

/// This test process's uid, without a libc dependency: the owner of a file
/// it just created.
fn my_uid(dir: &Path) -> u32 {
    let probe = dir.join("uid-probe");
    std::fs::write(&probe, b"").unwrap();
    std::fs::metadata(&probe).unwrap().uid()
}

async fn start_with_peer_uid(peer_uid: impl FnOnce(u32) -> u32) -> Harness {
    let dir = tempfile::tempdir().unwrap();
    let code = Code::generate().unwrap();
    let code_text = code.reveal();
    let limits = Limits {
        per_ip_attempts: 3,
        global_attempts: 100,
        session_max: Duration::from_secs(3600),
        max_sessions: 8,
    };
    let state = Arc::new(Mutex::new(AuthState::new(limits, code, Now::current())));
    let store = Arc::new(Mutex::new(Store::open(dir.path()).unwrap()));
    let ctx = Arc::new(Ctx::new(
        state,
        store,
        Duration::ZERO,
        // Nothing trusted over TCP: the socket is the only credential.
        Vec::new(),
        "proxy-node".to_string(),
    ));
    let app = http::router(Arc::clone(&ctx));

    let tcp = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let tcp_addr = tcp.local_addr().unwrap();
    let served = app.clone();
    tokio::spawn(async move {
        axum::serve(
            tcp,
            served.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await
        .unwrap();
    });

    let unix = dir.path().join("run").join("proxy.sock");
    let uid = peer_uid(my_uid(dir.path()));
    let socket = proxy::bind(&unix, uid).expect("the proxy socket binds");
    let (listener, guard) = socket.into_parts();
    tokio::spawn(async move {
        let _guard = guard;
        axum::serve(listener, proxy::router(app).into_make_service())
            .await
            .unwrap();
    });

    Harness {
        tcp: tcp_addr,
        unix,
        code: code_text,
        ctx,
        _dir: dir,
    }
}

async fn start() -> Harness {
    start_with_peer_uid(|me| me).await
}

struct Reply {
    status: u16,
    headers: Vec<(String, String)>,
}

impl Reply {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    /// `name=value` out of the `Set-Cookie`, ready to send back.
    fn cookie_pair(&self) -> String {
        let raw = self.header("set-cookie").expect("a Set-Cookie header");
        raw.split(';').next().unwrap().to_string()
    }
}

async fn exchange<S: AsyncRead + AsyncWrite + Unpin>(
    mut stream: S,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: Option<&str>,
) -> Option<Reply> {
    let mut req = format!("{method} {path} HTTP/1.1\r\nConnection: close\r\n");
    for (name, value) in headers {
        req.push_str(&format!("{name}: {value}\r\n"));
    }
    match body {
        Some(b) => {
            req.push_str("Content-Type: application/x-www-form-urlencoded\r\n");
            req.push_str(&format!("Content-Length: {}\r\n\r\n{b}", b.len()));
        }
        None => req.push_str("\r\n"),
    }
    // A refused connection may be closed before the write lands; that is an
    // answer too, and `None` is how it is reported.
    if stream.write_all(req.as_bytes()).await.is_err() {
        return None;
    }
    let mut raw = Vec::new();
    let _ = stream.read_to_end(&mut raw).await;
    let split = raw.windows(4).position(|w| w == b"\r\n\r\n")?;
    let head = String::from_utf8_lossy(&raw[..split]).into_owned();
    let mut lines = head.split("\r\n");
    let status = lines.next()?.split_whitespace().nth(1)?.parse().ok()?;
    let headers = lines
        .filter_map(|l| l.split_once(':'))
        .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
        .collect();
    Some(Reply { status, headers })
}

impl Harness {
    async fn over_unix(
        &self,
        method: &str,
        path: &str,
        headers: &[(&str, &str)],
        body: Option<&str>,
    ) -> Option<Reply> {
        let stream = UnixStream::connect(&self.unix).await.expect("connect");
        exchange(stream, method, path, headers, body).await
    }

    async fn over_tcp(
        &self,
        method: &str,
        path: &str,
        headers: &[(&str, &str)],
        body: Option<&str>,
    ) -> Reply {
        let stream = TcpStream::connect(self.tcp).await.expect("connect");
        exchange(stream, method, path, headers, body)
            .await
            .expect("a reply over TCP")
    }

    /// What `tailscale serve` sends to a Unix-socket backend: `Host:
    /// localhost`, and the browser's host, address and scheme forwarded.
    fn tailscale_headers<'a>(
        client: &'a str,
        extra: &[(&'a str, &'a str)],
    ) -> Vec<(&'a str, &'a str)> {
        let mut h = vec![
            ("Host", "localhost"),
            ("X-Forwarded-Host", SITE),
            ("X-Forwarded-For", client),
            ("X-Forwarded-Proto", "https"),
        ];
        h.extend_from_slice(extra);
        h
    }

    fn form(&self) -> String {
        format!("code={}", self.code)
    }
}

fn wrong(real: &str) -> String {
    let mut symbols: Vec<char> = real.chars().filter(|c| *c != '-').collect();
    symbols[0] = if symbols[0] == 'Z' { 'Y' } else { 'Z' };
    format!("code={}", symbols.into_iter().collect::<String>())
}

fn ip(s: &str) -> IpAddr {
    s.parse().unwrap()
}

// ---------------------------------------------------------------------------
// Attribution
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_sign_in_over_the_proxy_socket_is_attributed_to_the_forwarded_address() {
    let h = start().await;
    let origin = format!("https://{SITE}");
    let login = h
        .over_unix(
            "POST",
            "/login",
            &Harness::tailscale_headers(TAILNET_CLIENT, &[("Origin", &origin)]),
            Some(&h.form()),
        )
        .await
        .expect("a reply over the proxy socket");
    assert_eq!(
        login.status, 303,
        "the right code over the proxy socket signs in"
    );
    let cookie = login.cookie_pair();

    // The session is bound to the address it signed in from. It works from
    // the forwarded address...
    let home = h
        .over_unix(
            "GET",
            "/",
            &Harness::tailscale_headers(TAILNET_CLIENT, &[("Cookie", &cookie)]),
            None,
        )
        .await
        .unwrap();
    assert_eq!(
        home.status, 200,
        "the session works from the address it was given"
    );

    // ...and not from another one, so the forwarded address is what it was
    // bound to, not the socket and not some fallback.
    let elsewhere = h
        .over_unix(
            "GET",
            "/",
            &Harness::tailscale_headers(OTHER_CLIENT, &[("Cookie", &cookie)]),
            None,
        )
        .await
        .unwrap();
    assert_eq!(
        elsewhere.status, 303,
        "a session moved to another address is refused"
    );
}

#[tokio::test]
async fn the_same_header_over_loopback_tcp_is_not_believed() {
    let h = start().await;
    let tcp_host = h.tcp.to_string();
    let origin = format!("http://{tcp_host}");
    let forged = [
        ("Host", tcp_host.as_str()),
        ("Origin", origin.as_str()),
        ("X-Forwarded-For", TAILNET_CLIENT),
        ("X-Forwarded-Proto", "https"),
    ];

    // Failures: three wrong codes block an address. The address blocked is
    // the socket's, not the one the header named.
    for _ in 0..3 {
        let r = h
            .over_tcp("POST", "/login", &forged, Some(&wrong(&h.code)))
            .await;
        assert!(r.status == 401 || r.status == 403, "got {}", r.status);
    }
    assert_eq!(
        h.ctx.state().refusal(ip("127.0.0.1")),
        Some(Refusal::IpBlocked)
    );
    assert_eq!(h.ctx.state().refusal(ip(TAILNET_CLIENT)), None);

    // And the address the header named can still sign in through the proxy
    // socket, which it could not if the failures had been counted against it.
    let login = h
        .over_unix(
            "POST",
            "/login",
            &Harness::tailscale_headers(TAILNET_CLIENT, &[("Origin", &format!("https://{SITE}"))]),
            Some(&h.form()),
        )
        .await
        .unwrap();
    assert_eq!(login.status, 303);
}

#[tokio::test]
async fn a_session_signed_in_over_tcp_ignores_the_forwarded_address() {
    let h = start().await;
    let tcp_host = h.tcp.to_string();
    let origin = format!("http://{tcp_host}");
    let login = h
        .over_tcp(
            "POST",
            "/login",
            &[
                ("Host", &tcp_host),
                ("Origin", &origin),
                ("X-Forwarded-For", TAILNET_CLIENT),
                ("X-Forwarded-Proto", "https"),
            ],
            Some(&h.form()),
        )
        .await;
    assert_eq!(login.status, 303);
    // Not HTTPS, whatever the header claimed: the plain name, not Secure.
    let set = login.header("set-cookie").unwrap().to_string();
    assert!(set.starts_with("dj_session="), "{set}");
    assert!(!set.contains("Secure"), "{set}");
    let cookie = login.cookie_pair();

    // Bound to loopback, so a different forged address changes nothing.
    let home = h
        .over_tcp(
            "GET",
            "/",
            &[
                ("Host", &tcp_host),
                ("Cookie", &cookie),
                ("X-Forwarded-For", OTHER_CLIENT),
            ],
            None,
        )
        .await;
    assert_eq!(home.status, 200);
}

// ---------------------------------------------------------------------------
// The cookie
// ---------------------------------------------------------------------------

#[tokio::test]
async fn over_https_the_cookie_is_host_prefixed_and_only_that_name_is_read() {
    let h = start().await;
    let origin = format!("https://{SITE}");
    let login = h
        .over_unix(
            "POST",
            "/login",
            &Harness::tailscale_headers(TAILNET_CLIENT, &[("Origin", &origin)]),
            Some(&h.form()),
        )
        .await
        .unwrap();
    let set = login.header("set-cookie").unwrap().to_string();
    assert!(set.starts_with("__Host-dj_session="), "{set}");
    assert!(set.contains("; Secure") && set.contains("Path=/") && !set.contains("Domain"));

    // The same value under the plain name is not accepted over HTTPS.
    let value = login.cookie_pair().split_once('=').unwrap().1.to_string();
    let plain = format!("dj_session={value}");
    let r = h
        .over_unix(
            "GET",
            "/",
            &Harness::tailscale_headers(TAILNET_CLIENT, &[("Cookie", &plain)]),
            None,
        )
        .await
        .unwrap();
    assert_eq!(r.status, 303, "a plain dj_session is not read over HTTPS");

    // Signing out clears the name that was set.
    let out = h
        .over_unix(
            "POST",
            "/logout",
            &Harness::tailscale_headers(
                TAILNET_CLIENT,
                &[("Origin", &origin), ("Cookie", &login.cookie_pair())],
            ),
            Some(""),
        )
        .await
        .unwrap();
    let cleared = out.header("set-cookie").unwrap();
    assert!(cleared.starts_with("__Host-dj_session=;"), "{cleared}");
}

// ---------------------------------------------------------------------------
// Host, which tailscale rewrites to localhost for a socket backend
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_forwarded_host_is_what_the_origin_check_sees_on_the_proxy_socket_only() {
    let h = start().await;
    let origin = format!("https://{SITE}");

    // Without X-Forwarded-Host, Host is `localhost` and the browser's Origin
    // does not match it: refused. This is the control for the next request.
    let bare = h
        .over_unix(
            "POST",
            "/login",
            &[
                ("Host", "localhost"),
                ("Origin", &origin),
                ("X-Forwarded-For", TAILNET_CLIENT),
            ],
            Some(&h.form()),
        )
        .await
        .unwrap();
    assert_eq!(bare.status, 403);

    let forwarded = h
        .over_unix(
            "POST",
            "/login",
            &Harness::tailscale_headers(TAILNET_CLIENT, &[("Origin", &origin)]),
            Some(&h.form()),
        )
        .await
        .unwrap();
    assert_eq!(forwarded.status, 303);

    // Over TCP the same X-Forwarded-Host is not believed.
    let tcp = h
        .over_tcp(
            "POST",
            "/login",
            &[
                ("Host", "localhost"),
                ("Origin", &origin),
                ("X-Forwarded-Host", SITE),
            ],
            Some(&h.form()),
        )
        .await;
    assert_eq!(tcp.status, 403);
}

// ---------------------------------------------------------------------------
// Who may connect
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_connection_from_any_uid_but_the_proxys_is_dropped_unanswered() {
    // The proxy's uid is someone else's: this process stands in for an app,
    // which runs as the operator and can open the file.
    let h = start_with_peer_uid(|me| me.wrapping_add(1)).await;
    let reply = h
        .over_unix(
            "POST",
            "/login",
            &Harness::tailscale_headers(TAILNET_CLIENT, &[("Origin", &format!("https://{SITE}"))]),
            Some(&h.form()),
        )
        .await;
    assert!(
        reply.is_none(),
        "a connection from the wrong uid was answered"
    );
    assert!(
        h.ctx
            .state()
            .sessions(std::time::SystemTime::now())
            .is_empty(),
        "a connection from the wrong uid signed in"
    );
}

// ---------------------------------------------------------------------------
// The file
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_socket_is_owner_only_in_an_owner_only_directory() {
    let h = start().await;
    let meta = std::fs::symlink_metadata(&h.unix).unwrap();
    assert_eq!(meta.permissions().mode() & 0o777, 0o600);
    let dir = std::fs::metadata(h.unix.parent().unwrap()).unwrap();
    assert_eq!(dir.permissions().mode() & 0o777, 0o700);
}

#[tokio::test]
async fn a_stale_socket_is_replaced_and_a_live_one_is_left_alone() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("proxy.sock");
    let me = my_uid(dir.path());

    // Stale: bound, then the listener dropped, as a crash leaves it.
    drop(std::os::unix::net::UnixListener::bind(&path).unwrap());
    assert!(path.exists());
    let first = proxy::bind(&path, me).expect("a stale socket is replaced");

    // Live: the socket just bound answers, so a second bind refuses and the
    // first is untouched.
    let ino = std::fs::symlink_metadata(&path).unwrap().ino();
    match proxy::bind(&path, me) {
        Err(proxy::BindError::InUse { .. }) => {}
        Err(e) => panic!("expected InUse, got {e}"),
        Ok(_) => panic!("a live socket was taken over"),
    }
    assert_eq!(std::fs::symlink_metadata(&path).unwrap().ino(), ino);
    assert!(std::os::unix::net::UnixStream::connect(&path).is_ok());

    // Clean shutdown removes it.
    drop(first);
    assert!(!path.exists(), "the socket outlived a clean shutdown");
}

#[tokio::test]
async fn two_nodes_racing_for_one_socket_path_cannot_both_win() {
    // Probing is check-then-act: two nodes that both find the path empty
    // would both rename their socket into place, and the first would be
    // silently orphaned. The state the second sees in that race — the path
    // empty while a live node holds it — is set up directly here.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("proxy.sock");
    let me = my_uid(dir.path());
    let first = proxy::bind(&path, me).unwrap();
    std::fs::remove_file(&path).unwrap();
    match proxy::bind(&path, me) {
        Err(proxy::BindError::InUse { .. }) => {}
        Err(e) => panic!("expected InUse, got {e}"),
        Ok(_) => panic!("a second node bound a path a live node holds"),
    }
    // Control: once the first node is gone, the path is free again.
    drop(first);
    let _second = proxy::bind(&path, me).expect("free after the first node stops");
}

#[tokio::test]
async fn a_file_that_is_not_a_socket_is_never_deleted() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("proxy.sock");
    std::fs::write(&path, b"precious").unwrap();
    match proxy::bind(&path, 0) {
        Err(proxy::BindError::NotASocket { .. }) => {}
        Err(e) => panic!("expected NotASocket, got {e}"),
        Ok(_) => panic!("a regular file was replaced by a socket"),
    }
    assert_eq!(std::fs::read(&path).unwrap(), b"precious");
}

#[tokio::test]
async fn shutting_down_does_not_delete_a_successors_socket() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("proxy.sock");
    let ours = proxy::bind(&path, 0).unwrap();
    // Something else now owns the path: removed and rebound behind our back.
    std::fs::remove_file(&path).unwrap();
    let theirs = std::os::unix::net::UnixListener::bind(&path).unwrap();
    drop(ours);
    assert!(
        path.exists(),
        "a clean shutdown deleted another process's socket"
    );
    drop(theirs);
}

// ---------------------------------------------------------------------------
// The binary: main.rs's own wiring
// ---------------------------------------------------------------------------

/// Kills the node on the way out, whatever the test did.
struct Node(std::process::Child);

impl Drop for Node {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[tokio::test]
async fn the_binary_serves_the_proxy_socket_it_is_configured_with() {
    // Everything above composes the listeners by hand. This runs `deckjack
    // --debug` with `server.unix_socket` set, so a main.rs that never binds
    // the socket, serves it without `proxy::router` (every handler would
    // lack its connect info), or serves it unmarked fails here.
    //
    // The debug session is bound to loopback, which is what makes the
    // attribution observable: over the socket it works only when the
    // forwarded address is 127.0.0.1, and over TCP the forwarded address
    // changes nothing.
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("h");
    std::fs::create_dir_all(home.join(".deckjack")).unwrap();
    let port = {
        let probe = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        probe.local_addr().unwrap().port()
    };
    let uid = my_uid(tmp.path());
    std::fs::write(
        home.join(".deckjack/config.toml"),
        format!(
            "[server]\nlisten = \"127.0.0.1:{port}\"\nunix_socket = \"proxy/http.sock\"\n\
             unix_socket_peer_uid = {uid}\n"
        ),
    )
    .unwrap();
    let log = std::fs::File::create(tmp.path().join("node.log")).unwrap();
    let mut node = Node(
        std::process::Command::new(env!("CARGO_BIN_EXE_deckjack"))
            .args(["--debug", "--dir"])
            .arg(&home)
            .env("DECKJACK_HOME", &home)
            .env_remove("RUST_LOG")
            .stdout(log.try_clone().unwrap())
            .stderr(log)
            .spawn()
            .expect("the binary starts"),
    );
    let socket = home.join("proxy/http.sock");
    let cookie_file = home.join(deckjack_core::debugmode::COOKIE_FILE);
    let log_text = || std::fs::read_to_string(tmp.path().join("node.log")).unwrap_or_default();
    let deadline = std::time::Instant::now() + Duration::from_secs(60);
    while !(socket.exists()
        && cookie_file.exists()
        && TcpStream::connect(("127.0.0.1", port)).await.is_ok())
    {
        if let Some(status) = node.0.try_wait().unwrap() {
            panic!("the node exited ({status}):\n{}", log_text());
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the node never came up:\n{}",
            log_text()
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let cookie = std::fs::read_to_string(&cookie_file).unwrap();
    let cookie = cookie.trim();

    let over_socket = |client: &'static str| {
        let socket = socket.clone();
        let cookie = cookie.to_string();
        async move {
            let stream = UnixStream::connect(&socket).await.expect("connect");
            exchange(
                stream,
                "GET",
                "/",
                &[
                    ("Host", "localhost"),
                    ("X-Forwarded-Host", SITE),
                    ("X-Forwarded-For", client),
                    ("Cookie", &cookie),
                ],
                None,
            )
            .await
            .expect("a reply over the proxy socket")
        }
    };
    assert_eq!(
        over_socket("127.0.0.1").await.status,
        200,
        "over the socket, the forwarded loopback address is believed:\n{}",
        log_text()
    );
    assert_eq!(
        over_socket(TAILNET_CLIENT).await.status,
        303,
        "over the socket, the forwarded tailnet address is believed too"
    );

    let stream = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    let tcp = exchange(
        stream,
        "GET",
        "/",
        &[
            ("Host", &format!("127.0.0.1:{port}")),
            ("X-Forwarded-For", TAILNET_CLIENT),
            ("Cookie", cookie),
        ],
        None,
    )
    .await
    .expect("a reply over TCP");
    assert_eq!(
        tcp.status, 200,
        "over TCP the same header is not believed: the peer is loopback"
    );

    // A clean shutdown removes the socket file.
    // SAFETY: signalling the child this test spawned.
    unsafe { libc::kill(node.0.id() as libc::pid_t, libc::SIGTERM) };
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    while node.0.try_wait().unwrap().is_none() {
        assert!(
            std::time::Instant::now() < deadline,
            "the node did not stop"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(
        !socket.exists(),
        "a clean shutdown leaves no proxy socket behind:\n{}",
        log_text()
    );
}
