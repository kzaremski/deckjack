//! The install directory's own files are never reachable through the file
//! layer, wherever the working directory is.
//!
//! The working directory defaults to the install directory (`~/.deckjack`, or
//! `$DECKJACK_HOME`), which also holds the unconfined core binary, every
//! app's binary and manifest, the app sockets, the node's identity, the caches
//! and, in `--debug`, a live operator session. So the file layer has to treat
//! those entries the way it treats `.deckjack/`: refused when named, refused
//! when a symlink leads there, and left out of listings. These tests build
//! that layout on a real disk and ask through the bus, the way a Files window
//! or an agent with a files grant would. Each refusal has an ordinary file
//! beside it that must still work, so a fix that simply broke the file layer
//! would fail here too.
//!
//! The last two tests run the real `deckjack` binary, because what `main`
//! passes to the file layer is only observable from outside it.

use std::io::{Cursor, Read as _, Write as _};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command as Process, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use image::{DynamicImage, ImageFormat, Rgb, RgbImage};
use serde_json::{Value, json};

use deckjack_core::command::{
    Admitted, Bus, CommandId, Completion, Issuer, NodeId, OsEnv, Registry, Request,
};
use deckjack_core::files::register_commands;
use deckjack_core::node::INSTALL_LAYOUT;

const NODE: &str = "r620";
const CORE: &[u8] = b"\x7fELF the core binary";
const APP_BINARY: &[u8] = b"\x7fELF an app binary";
const COOKIE: &str = "dj_session=a-live-operator-session";

/// Lay out an install directory the way `cargo xtask install`, `deckjack
/// install`, a running server and `--debug` leave it, plus one ordinary file
/// of the operator's.
fn lay_out(install: &Path) {
    let put = |rel: &str, bytes: &[u8]| {
        let path = install.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, bytes).unwrap();
    };
    put("bin/deckjack", CORE);
    put("apps/x/deckjack-x", APP_BINARY);
    put("apps/x/app.toml", b"[app]\nname = \"x\"\n");
    put("keys/node-id", b"0123456789abcdef");
    put("cache/files/thumb", b"a thumbnail");
    put("cache/apps/x/home/secret", b"x's private home");
    put("debug-session.cookie", COOKIE.as_bytes());
    put("server.log", b"a log line");
    std::fs::create_dir_all(install.join("run")).unwrap();
    put("notes.txt", b"the operator's own notes");
}

fn bus(working_dir: &Path, install: &Path) -> Bus {
    let registry = Arc::new(Registry::new());
    register_commands(&registry, working_dir, NODE, install).unwrap();
    Bus::new(registry, NodeId::new(NODE), Arc::new(OsEnv))
}

fn run(bus: &Bus, command: &str, params: Value) -> Completion {
    let request =
        Request::new(CommandId::new(command).unwrap(), params, Issuer::operator()).confirmed();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .build()
        .unwrap();
    match bus.dispatch(request) {
        Ok(Admitted::Running(running)) => runtime.block_on(running.finish()),
        Ok(other) => panic!("expected {command} to run, not {other:?}"),
        Err(e) => panic!("{command} was not admitted: {e:?}"),
    }
}

fn ok(bus: &Bus, command: &str, params: Value) -> Value {
    run(bus, command, params)
        .outcome
        .unwrap_or_else(|e| panic!("{command} failed: {e}"))
}

/// The refusal's text, or a panic naming what the command returned instead.
fn refused(bus: &Bus, command: &str, params: Value) -> String {
    match run(bus, command, params.clone()).outcome {
        Ok(value) => panic!("{command} {params} must be refused, and returned {value}"),
        Err(e) => e.to_string(),
    }
}

fn names(listing: &Value) -> Vec<String> {
    listing["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["name"].as_str().unwrap().to_string())
        .collect()
}

// ---------------------------------------------------------------------------
// The default: the working directory is the install directory
// ---------------------------------------------------------------------------

#[test]
fn files_read_cannot_reach_the_core_binary_or_the_debug_session() {
    let dir = tempfile::tempdir().unwrap();
    let install = dir.path();
    lay_out(install);
    let bus = bus(install, install);

    for path in [
        "/bin/deckjack",
        "/debug-session.cookie",
        "/keys/node-id",
        "/apps/x/app.toml",
        "/cache/apps/x/home/secret",
        "/server.log",
    ] {
        let why = refused(&bus, "files.read", json!({ "path": path }));
        assert!(
            why.contains("install directory"),
            "{path}: the refusal should say why: {why}"
        );
        refused(&bus, "files.stat", json!({ "path": path }));
    }

    // The control: an ordinary file beside them still reads.
    let read = ok(&bus, "files.read", json!({ "path": "/notes.txt" }));
    assert_eq!(read["content"], "the operator's own notes");
}

#[test]
fn files_write_cannot_replace_an_apps_binary_or_plant_a_cookie() {
    let dir = tempfile::tempdir().unwrap();
    let install = dir.path();
    lay_out(install);
    std::fs::remove_file(install.join("debug-session.cookie")).unwrap();
    let bus = bus(install, install);

    refused(
        &bus,
        "files.write",
        json!({ "path": "/apps/x/deckjack-x", "content": "#!/bin/sh\necho pwned\n" }),
    );
    refused(
        &bus,
        "files.write",
        json!({ "path": "/bin/deckjack", "content": "payload" }),
    );
    // A name that does not exist yet is the install directory's all the same.
    refused(
        &bus,
        "files.write",
        json!({ "path": "/debug-session.cookie", "content": "dj_session=forged" }),
    );
    refused(&bus, "files.mkdir", json!({ "path": "/apps/evil" }));
    refused(
        &bus,
        "files.rename",
        json!({ "from": "/notes.txt", "to": "/bin/notes.txt" }),
    );
    refused(
        &bus,
        "files.rename",
        json!({ "from": "/bin/deckjack", "to": "/core" }),
    );
    refused(
        &bus,
        "files.delete",
        json!({ "path": "/apps", "recursive": true }),
    );
    refused(&bus, "files.trash", json!({ "paths": ["/bin/deckjack"] }));
    refused(
        &bus,
        "files.copy",
        json!({ "paths": ["/keys/node-id"], "to": "/" }),
    );
    refused(
        &bus,
        "files.move",
        json!({ "paths": ["/notes.txt"], "to": "/apps/x" }),
    );

    assert_eq!(
        std::fs::read(install.join("apps/x/deckjack-x")).unwrap(),
        APP_BINARY
    );
    assert_eq!(std::fs::read(install.join("bin/deckjack")).unwrap(), CORE);
    assert!(!install.join("debug-session.cookie").exists());
    assert!(!install.join("apps/evil").exists());

    // The control: an ordinary write beside them lands.
    ok(
        &bus,
        "files.write",
        json!({ "path": "/notes2.txt", "content": "fine" }),
    );
    assert_eq!(std::fs::read(install.join("notes2.txt")).unwrap(), b"fine");
}

#[test]
fn files_list_of_the_root_leaves_the_install_layout_out() {
    let dir = tempfile::tempdir().unwrap();
    let install = dir.path();
    lay_out(install);
    let bus = bus(install, install);

    let listing = ok(
        &bus,
        "files.list",
        json!({ "path": "/", "show_dotfiles": true }),
    );
    let shown = names(&listing);
    for entry in INSTALL_LAYOUT {
        assert!(
            !shown.iter().any(|n| n.eq_ignore_ascii_case(entry)),
            "{entry} is listed: {shown:?}"
        );
    }
    assert!(shown.contains(&"notes.txt".to_string()), "{shown:?}");
}

#[test]
fn a_symlink_into_the_install_layout_is_refused_and_listed_as_escaping() {
    let dir = tempfile::tempdir().unwrap();
    let install = dir.path();
    lay_out(install);
    std::os::unix::fs::symlink(install.join("bin"), install.join("tools")).unwrap();
    std::os::unix::fs::symlink(install.join("notes.txt"), install.join("shortcut")).unwrap();
    let bus = bus(install, install);

    refused(&bus, "files.read", json!({ "path": "/tools/deckjack" }));
    refused(
        &bus,
        "files.write",
        json!({ "path": "/tools/deckjack", "content": "payload" }),
    );
    let listing = ok(&bus, "files.list", json!({ "path": "/" }));
    let tools = listing["entries"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["name"] == "tools")
        .expect("the link itself is the operator's and is listed");
    assert_eq!(tools["escapes"], true, "{tools}");

    // The control: a link to an ordinary file still reads.
    let read = ok(&bus, "files.read", json!({ "path": "/shortcut" }));
    assert_eq!(read["content"], "the operator's own notes");
}

// ---------------------------------------------------------------------------
// A working directory above the install directory
// ---------------------------------------------------------------------------

#[test]
fn a_working_directory_above_the_install_directory_still_cannot_reach_it() {
    let dir = tempfile::tempdir().unwrap();
    let work = dir.path();
    let install = work.join("dj");
    lay_out(&install);
    std::fs::write(work.join("top.txt"), b"top").unwrap();
    let bus = bus(work, &install);

    refused(&bus, "files.read", json!({ "path": "/dj/bin/deckjack" }));
    refused(
        &bus,
        "files.read",
        json!({ "path": "/dj/debug-session.cookie" }),
    );
    refused(
        &bus,
        "files.write",
        json!({ "path": "/dj/apps/x/deckjack-x", "content": "payload" }),
    );
    // Moving, copying or deleting the install directory as a whole would
    // carry its binaries and its cookie somewhere unprotected.
    refused(
        &bus,
        "files.delete",
        json!({ "path": "/dj", "recursive": true }),
    );
    refused(
        &bus,
        "files.rename",
        json!({ "from": "/dj", "to": "/moved" }),
    );
    refused(&bus, "files.copy", json!({ "paths": ["/dj"], "to": "/" }));
    refused(&bus, "files.move", json!({ "paths": ["/dj"], "to": "/" }));
    refused(&bus, "files.trash", json!({ "paths": ["/dj"] }));
    assert_eq!(std::fs::read(install.join("bin/deckjack")).unwrap(), CORE);

    let shown = names(&ok(
        &bus,
        "files.list",
        json!({ "path": "/dj", "show_dotfiles": true }),
    ));
    for entry in INSTALL_LAYOUT {
        assert!(!shown.iter().any(|n| n == entry), "{entry}: {shown:?}");
    }
    assert_eq!(shown, ["notes.txt"]);

    // The controls: the operator's files, in and beside it.
    let read = ok(&bus, "files.read", json!({ "path": "/dj/notes.txt" }));
    assert_eq!(read["content"], "the operator's own notes");
    let read = ok(&bus, "files.read", json!({ "path": "/top.txt" }));
    assert_eq!(read["content"], "top");
}

// ---------------------------------------------------------------------------
// The thumbnail cache follows the install directory it is given
// ---------------------------------------------------------------------------

fn jpeg(w: u32, h: u32) -> Vec<u8> {
    let img = RgbImage::from_fn(w, h, |x, _| Rgb([(x * 255 / w.max(1)) as u8, 80, 160]));
    let mut out = Cursor::new(Vec::new());
    DynamicImage::ImageRgb8(img)
        .write_to(&mut out, ImageFormat::Jpeg)
        .unwrap();
    out.into_inner()
}

/// Every regular file under `dir`, recursively.
fn files_under(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            out.extend(files_under(&path));
        } else {
            out.push(path);
        }
    }
    out
}

#[test]
fn thumbnails_are_cached_under_the_install_directory_the_file_layer_is_given() {
    let dir = tempfile::tempdir().unwrap();
    let work = dir.path().join("work");
    let install = dir.path().join("home");
    std::fs::create_dir_all(&work).unwrap();
    std::fs::create_dir_all(&install).unwrap();
    std::fs::write(work.join("a.jpg"), jpeg(640, 480)).unwrap();
    let bus = bus(&work, &install);

    ok(&bus, "files.thumbnail", json!({ "path": "/a.jpg" }));
    assert_eq!(
        files_under(&deckjack_core::thumbnail::cache_dir(&install)).len(),
        1,
        "one thumbnail, in <install>/cache/files"
    );
    assert_eq!(files_under(&work), [work.join("a.jpg")]);
}

// ---------------------------------------------------------------------------
// The real binary: what `main` actually wires
// ---------------------------------------------------------------------------

/// A `deckjack --debug` process, killed when dropped.
struct Server {
    child: Child,
    addr: SocketAddr,
    cookie: String,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// Start the installed layout's server the way the install hint says, with
/// `--dir` only when given.
fn serve(install: &Path, dir: Option<&Path>) -> Server {
    let port = free_port();
    let addr: SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
    let cookie_file = install.join("debug-session.cookie");
    let _ = std::fs::remove_file(&cookie_file);
    let mut cmd = Process::new(env!("CARGO_BIN_EXE_deckjack"));
    cmd.arg("--debug")
        .arg("--listen")
        .arg(addr.to_string())
        .env("DECKJACK_HOME", install)
        .env_remove("RUST_LOG")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    if let Some(dir) = dir {
        cmd.arg("--dir").arg(dir);
    }
    let child = cmd.spawn().expect("the server starts");
    let mut server = Server {
        child,
        addr,
        cookie: String::new(),
    };

    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        if let Some(status) = server.child.try_wait().unwrap() {
            panic!("the server exited before it listened: {status}");
        }
        let cookie = std::fs::read_to_string(&cookie_file).unwrap_or_default();
        if !cookie.trim().is_empty() && TcpStream::connect(addr).is_ok() {
            server.cookie = cookie.trim().to_string();
            return server;
        }
        assert!(Instant::now() < deadline, "the server never listened");
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// One HTTP GET, with the session cookie: status and body.
fn get(server: &Server, path: &str) -> (u16, Vec<u8>) {
    let mut stream = TcpStream::connect(server.addr).unwrap();
    write!(
        stream,
        // `Sec-Fetch-Site` as the shell's own fetch carries it: the gate on
        // cookie routes (`fetchgate.rs`) refuses `/content` without it.
        "GET {path} HTTP/1.1\r\nHost: {}\r\nCookie: {}\r\nSec-Fetch-Site: same-origin\r\n\
         Connection: close\r\n\r\n",
        server.addr, server.cookie
    )
    .unwrap();
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).unwrap();
    let split = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .expect("a response head");
    let head = String::from_utf8_lossy(&raw[..split]).into_owned();
    let status = head.split_whitespace().nth(1).unwrap().parse().unwrap();
    (status, raw[split + 4..].to_vec())
}

/// The bus over a hand-masked WebSocket, as in `end_to_end.rs`: the crate
/// has no WebSocket client and this file may not add one.
struct Ws {
    stream: TcpStream,
    next: u32,
}

impl Ws {
    fn connect(server: &Server) -> Ws {
        let mut stream = TcpStream::connect(server.addr).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(30)))
            .unwrap();
        write!(
            stream,
            "GET {} HTTP/1.1\r\nHost: {}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\
             Sec-WebSocket-Version: 13\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\
             Sec-Fetch-Site: same-origin\r\nCookie: {}\r\n\r\n",
            deckjack_core::ws::PATH,
            server.addr,
            server.cookie
        )
        .unwrap();
        let mut head = Vec::new();
        while !head.ends_with(b"\r\n\r\n") {
            let mut byte = [0u8; 1];
            assert_eq!(stream.read(&mut byte).unwrap(), 1, "closed mid-handshake");
            head.push(byte[0]);
        }
        let text = String::from_utf8_lossy(&head).into_owned();
        assert!(text.starts_with("HTTP/1.1 101"), "no upgrade: {text}");
        let mut ws = Ws { stream, next: 0 };
        ws.send(&json!({ "type": "hello", "protocol": deckjack_core::ws::PROTOCOL_VERSION }));
        ws.recv();
        ws
    }

    fn send(&mut self, value: &Value) {
        const MASK: [u8; 4] = [0x37, 0xfa, 0x21, 0x3d];
        let payload = value.to_string().into_bytes();
        let mut frame = vec![0x81];
        if payload.len() < 126 {
            frame.push(0x80 | payload.len() as u8);
        } else {
            frame.push(0x80 | 126);
            frame.extend_from_slice(&(payload.len() as u16).to_be_bytes());
        }
        frame.extend_from_slice(&MASK);
        frame.extend(payload.iter().enumerate().map(|(i, b)| b ^ MASK[i % 4]));
        self.stream.write_all(&frame).unwrap();
    }

    fn exact(&mut self, n: usize) -> Vec<u8> {
        let mut buf = vec![0u8; n];
        self.stream.read_exact(&mut buf).unwrap();
        buf
    }

    fn recv(&mut self) -> Value {
        loop {
            let head = self.exact(2);
            let len = match head[1] & 0x7f {
                126 => {
                    let b = self.exact(2);
                    usize::from(u16::from_be_bytes([b[0], b[1]]))
                }
                127 => u64::from_be_bytes(self.exact(8).try_into().unwrap()) as usize,
                n => usize::from(n),
            };
            let payload = self.exact(len);
            match head[0] & 0x0f {
                0x1 => return serde_json::from_slice(&payload).unwrap(),
                0x8 => panic!("the bus closed: {}", String::from_utf8_lossy(&payload)),
                _ => {}
            }
        }
    }

    /// The outcome of one command.
    fn invoke(&mut self, command: &str, params: Value) -> Value {
        self.next += 1;
        let id = format!("c{}", self.next);
        self.send(&json!({
            "type": "invoke", "id": id, "command": command, "params": params
        }));
        loop {
            let frame = self.recv();
            if frame["type"] == "response" && frame["id"] == id {
                return frame["outcome"].clone();
            }
        }
    }
}

#[test]
fn the_served_install_directory_hides_its_own_layout_from_files_and_content() {
    let dir = tempfile::tempdir().unwrap();
    let install = dir.path().join("home");
    lay_out(&install);
    // No --dir: the working directory is the install directory, as plain
    // `deckjack` and the `cargo xtask install` hint run it.
    let server = serve(&install, None);

    let (status, body) = get(&server, "/content?path=/bin/deckjack");
    assert_eq!(status, 403, "{}", String::from_utf8_lossy(&body));
    assert!(!body.starts_with(b"\x7fELF"));
    let (status, body) = get(&server, "/content?path=/notes.txt");
    assert_eq!(status, 200);
    assert_eq!(body, b"the operator's own notes");

    let mut ws = Ws::connect(&server);
    let cookie = ws.invoke("files.read", json!({ "path": "/debug-session.cookie" }));
    assert_eq!(cookie["status"], "error", "{cookie}");
    assert!(
        !cookie.to_string().contains(&server.cookie),
        "the live session leaked: {cookie}"
    );
    let write = ws.invoke(
        "files.write",
        json!({ "path": "/apps/x/deckjack-x", "content": "payload" }),
    );
    assert_eq!(write["status"], "error", "{write}");
    assert_eq!(
        std::fs::read(install.join("apps/x/deckjack-x")).unwrap(),
        APP_BINARY
    );
    let listing = ws.invoke("files.list", json!({ "path": "/" }));
    let shown: Vec<&str> = listing["value"]["entries"]
        .as_array()
        .unwrap_or_else(|| panic!("{listing}"))
        .iter()
        .map(|e| e["name"].as_str().unwrap())
        .collect();
    for entry in INSTALL_LAYOUT {
        assert!(!shown.contains(entry), "{entry}: {shown:?}");
    }
    assert!(shown.contains(&"notes.txt"), "{shown:?}");
    let notes = ws.invoke("files.read", json!({ "path": "/notes.txt" }));
    assert_eq!(notes["value"]["content"], "the operator's own notes");
}

#[test]
fn the_served_thumbnail_cache_is_the_install_directorys_not_the_working_directorys() {
    let dir = tempfile::tempdir().unwrap();
    let install = dir.path().join("home");
    let work = dir.path().join("work");
    std::fs::create_dir_all(&install).unwrap();
    std::fs::create_dir_all(&work).unwrap();
    std::fs::write(work.join("a.jpg"), jpeg(640, 480)).unwrap();
    let server = serve(&install, Some(&work));

    let mut ws = Ws::connect(&server);
    let thumb = ws.invoke("files.thumbnail", json!({ "path": "/a.jpg" }));
    assert_eq!(thumb["status"], "ok", "{thumb}");
    drop(server);

    assert_eq!(
        files_under(&deckjack_core::thumbnail::cache_dir(&install)).len(),
        1,
        "main must hand the file layer <install>/cache/files"
    );
    let visible: Vec<PathBuf> = files_under(&work)
        .into_iter()
        .filter(|p| !p.starts_with(work.join(".deckjack")))
        .collect();
    assert_eq!(visible, [work.join("a.jpg")]);
}

// ---------------------------------------------------------------------------
// The list is complete
// ---------------------------------------------------------------------------

#[test]
fn every_path_the_core_keeps_in_the_install_directory_is_in_the_layout() {
    // A new entry written into the install directory without being listed
    // would be readable and writable through Files on a default setup.
    let install = Path::new("/srv/deckjack");
    let written = [
        install.join(deckjack_core::apps::APPS_SUBDIR),
        deckjack_core::apps::socket_path(install, "notes"),
        install.join(deckjack_core::apps::CACHE_SUBDIR),
        deckjack_core::thumbnail::cache_dir(install),
        install.join(deckjack_core::node::ID_FILE),
        install.join(deckjack_core::debugmode::COOKIE_FILE),
        // Where the xtask installer puts the core binary.
        install.join("bin").join("deckjack"),
        // The proxy socket at the documented setting, and its lock file.
        proxy_socket(install),
        proxy_socket(install).with_extension("sock.lock"),
    ];
    for path in written {
        let first = path
            .strip_prefix(install)
            .unwrap()
            .components()
            .next()
            .unwrap();
        let first = first.as_os_str().to_string_lossy();
        assert!(
            INSTALL_LAYOUT.contains(&first.as_ref()),
            "{} is not covered by INSTALL_LAYOUT",
            path.display()
        );
    }
}

/// Where `[server] unix_socket = "proxy/http.sock"`, the setting the config
/// and README recommend for `tailscale serve`, puts the proxy socket.
fn proxy_socket(install: &Path) -> PathBuf {
    let cfg: deckjack_core::config::Config =
        toml::from_str("[server]\nunix_socket = \"proxy/http.sock\"\n").unwrap();
    cfg.server.unix_socket_path(install).unwrap().unwrap()
}

#[test]
fn the_proxy_socket_cannot_be_listed_moved_or_granted_to_an_app() {
    // An app with a read-write grant on the proxy directory could unlink
    // the socket and bind its own in its place; tailscaled would then hand
    // it every forwarded session. So the directory is part of the layout.
    let dir = tempfile::tempdir().unwrap();
    let install = dir.path();
    lay_out(install);
    let socket = proxy_socket(install);
    std::fs::create_dir_all(socket.parent().unwrap()).unwrap();
    let _listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
    std::fs::write(socket.with_extension("sock.lock"), b"").unwrap();
    let bus = bus(install, install);

    let shown = names(&ok(&bus, "files.list", json!({ "path": "/" })));
    assert!(!shown.contains(&"proxy".to_string()), "{shown:?}");
    refused(&bus, "files.list", json!({ "path": "/proxy" }));
    refused(
        &bus,
        "files.rename",
        json!({ "from": "/proxy/http.sock", "to": "/proxy/moved.sock" }),
    );
    assert!(
        socket.exists(),
        "the socket must stay where tailscaled looks"
    );

    let manifest = "[app]\nname = \"sneaky\"\nversion = \"1.0.0\"\nprotocol = 1\n\n[needs]\n\
                    working_dir = [{ path = \"proxy\", write = true }]\n";
    let err = deckjack_protocol::Manifest::parse(Path::new("app.toml"), manifest).unwrap_err();
    assert!(err.to_string().contains("install directory"), "{err}");

    // The control: the operator's own file beside it still lists.
    assert!(shown.contains(&"notes.txt".to_string()), "{shown:?}");
}

#[test]
fn the_operators_own_folders_named_like_an_entry_in_another_case_stay_visible() {
    // Linux filesystems are case-sensitive: `Cache/` and a file `Keys` are
    // the operator's, not the install directory's `cache/` and `keys/`. Only
    // a name that is the very same file as an entry (a case-folding mount)
    // is the entry.
    let dir = tempfile::tempdir().unwrap();
    let install = dir.path();
    lay_out(install);
    std::fs::create_dir_all(install.join("Cache")).unwrap();
    std::fs::write(install.join("Cache/a.txt"), b"mine").unwrap();
    std::fs::write(install.join("Keys"), b"house keys").unwrap();
    std::fs::create_dir_all(install.join("Apps")).unwrap();
    let bus = bus(install, install);

    let shown = names(&ok(&bus, "files.list", json!({ "path": "/" })));
    for name in ["Cache", "Keys", "Apps", "notes.txt"] {
        assert!(shown.contains(&name.to_string()), "{name}: {shown:?}");
    }
    let read = ok(&bus, "files.read", json!({ "path": "/Cache/a.txt" }));
    assert_eq!(read["content"], "mine", "{read}");
    let read = ok(&bus, "files.read", json!({ "path": "/Keys" }));
    assert_eq!(read["content"], "house keys", "{read}");
    ok(
        &bus,
        "files.write",
        json!({ "path": "/Apps/list.txt", "content": "ok" }),
    );
    assert_eq!(std::fs::read(install.join("Apps/list.txt")).unwrap(), b"ok");

    // The control: the real entries beside them are still refused.
    refused(&bus, "files.read", json!({ "path": "/cache/files/thumb" }));
    refused(&bus, "files.read", json!({ "path": "/keys/node-id" }));
    assert!(!shown.contains(&"cache".to_string()), "{shown:?}");
}

/// The real disk, seen the way a case-folding mount (exFAT, NTFS, a casefold
/// ext4 directory) shows it: a name in any case opens the file whatever case
/// it was created in, and `realpath` hands the name back as written.
struct CaseFolding(deckjack_core::files::OsFs);

impl CaseFolding {
    /// The on-disk spelling of `path`, matching each component without case.
    fn fold(path: &Path) -> PathBuf {
        let mut out = PathBuf::new();
        for part in path.components() {
            let next = out.join(part);
            let found = if next.exists() {
                None
            } else {
                std::fs::read_dir(&out).ok().and_then(|entries| {
                    entries.filter_map(Result::ok).find_map(|e| {
                        e.file_name()
                            .to_string_lossy()
                            .eq_ignore_ascii_case(&part.as_os_str().to_string_lossy())
                            .then(|| e.path())
                    })
                })
            };
            out = found.unwrap_or(next);
        }
        out
    }
}

impl deckjack_core::files::Fs for CaseFolding {
    fn canonicalize(&self, path: &Path) -> std::io::Result<PathBuf> {
        let folded = Self::fold(path);
        let real = self.0.canonicalize(&folded)?;
        // As written, the way realpath answers on a folding filesystem.
        Ok(if real == folded {
            path.to_path_buf()
        } else {
            real
        })
    }
    fn meta(&self, path: &Path) -> std::io::Result<deckjack_core::files::Meta> {
        self.0.meta(&Self::fold(path))
    }
    fn link_meta(&self, path: &Path) -> std::io::Result<deckjack_core::files::Meta> {
        self.0.link_meta(&Self::fold(path))
    }
    fn read_dir(&self, path: &Path) -> std::io::Result<Vec<String>> {
        self.0.read_dir(&Self::fold(path))
    }
    fn read_range(&self, path: &Path, offset: u64, max: u64) -> std::io::Result<Vec<u8>> {
        self.0.read_range(&Self::fold(path), offset, max)
    }
    fn write_atomic(&self, path: &Path, bytes: &[u8]) -> std::io::Result<()> {
        self.0.write_atomic(&Self::fold(path), bytes)
    }
    fn create_dir(&self, path: &Path) -> std::io::Result<()> {
        self.0.create_dir(&Self::fold(path))
    }
    fn remove_file(&self, path: &Path) -> std::io::Result<()> {
        self.0.remove_file(&Self::fold(path))
    }
    fn remove_dir(&self, path: &Path) -> std::io::Result<()> {
        self.0.remove_dir(&Self::fold(path))
    }
    fn remove_dir_all(&self, path: &Path) -> std::io::Result<()> {
        self.0.remove_dir_all(&Self::fold(path))
    }
    fn rename(&self, from: &Path, to: &Path) -> std::io::Result<()> {
        self.0.rename(&Self::fold(from), &Self::fold(to))
    }
    fn copy_file(
        &self,
        from: &Path,
        to: &Path,
        chunk: &mut dyn FnMut(u64) -> bool,
    ) -> std::io::Result<u64> {
        self.0.copy_file(&Self::fold(from), &Self::fold(to), chunk)
    }
    fn read_link(&self, path: &Path) -> std::io::Result<PathBuf> {
        self.0.read_link(&Self::fold(path))
    }
    fn symlink(&self, target: &Path, at: &Path) -> std::io::Result<()> {
        self.0.symlink(target, &Self::fold(at))
    }
    fn same_file(&self, a: &Path, b: &Path) -> std::io::Result<bool> {
        self.0.same_file(&Self::fold(a), &Self::fold(b))
    }
}

#[test]
fn on_a_case_folding_filesystem_another_spelling_of_an_entry_is_still_the_entry() {
    use deckjack_core::files::{Root, VirtualPath};

    let dir = tempfile::tempdir().unwrap();
    let install = dir.path();
    lay_out(install);
    let root = Root::with_fs(install, Arc::new(CaseFolding(deckjack_core::files::OsFs)))
        .unwrap()
        .protecting_install(install);

    for path in ["/BIN/deckjack", "/Keys/node-id", "/Debug-Session.cookie"] {
        let asked = VirtualPath::parse(path).unwrap();
        match root.read(&asked, 1 << 20) {
            Ok(bytes) => panic!(
                "{path} is the install entry on this filesystem, and read {:?}",
                String::from_utf8_lossy(&bytes)
            ),
            Err(e) => assert!(e.to_string().contains("install directory"), "{path}: {e}"),
        }
    }
    // The control: an ordinary file, in another spelling, still reads.
    let notes = root
        .read(&VirtualPath::parse("/NOTES.txt").unwrap(), 1 << 20)
        .expect("an ordinary file reads in any case here");
    assert_eq!(notes, b"the operator's own notes");
}
