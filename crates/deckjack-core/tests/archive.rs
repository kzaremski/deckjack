//! `files.archive.list`: what Quick Look shows for an archive.
//!
//! Through the bus, on a real working directory, because the promises are
//! about the command an operator (or an agent) actually calls: it answers
//! with the names inside, it never extracts anything, it is bounded, and it
//! crosses the same boundary as every other file command.
//!
//! The archives are built here byte by byte rather than checked in, so each
//! test says exactly what is inside the thing it lists.

use std::io::Write as _;
use std::path::PathBuf;
use std::sync::Arc;

use serde_json::{Value, json};

use deckjack_core::command::{
    Admitted, Bus, CommandId, Completion, Issuer, NodeId, OsEnv, Registry, Request,
};
use deckjack_core::files::register_commands;

struct Workspace {
    _dir: tempfile::TempDir,
    root: PathBuf,
}

impl Workspace {
    fn new() -> Workspace {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("work");
        std::fs::create_dir_all(&root).unwrap();
        Workspace { _dir: dir, root }
    }

    fn put(&self, rel: &str, bytes: &[u8]) {
        let path = self.root.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, bytes).unwrap();
    }

    /// Every path under the root, so a test can see that nothing appeared.
    fn tree(&self) -> Vec<String> {
        fn walk(dir: &std::path::Path, base: &std::path::Path, out: &mut Vec<String>) {
            for e in std::fs::read_dir(dir).unwrap() {
                let p = e.unwrap().path();
                out.push(p.strip_prefix(base).unwrap().display().to_string());
                if p.is_dir() {
                    walk(&p, base, out);
                }
            }
        }
        let mut out = Vec::new();
        walk(&self.root, &self.root, &mut out);
        out.sort();
        out
    }

    fn bus(&self) -> Bus {
        let registry = Arc::new(Registry::new());
        register_commands(
            &registry,
            &self.root,
            "r620",
            &self.root.parent().unwrap().join("install"),
        )
        .unwrap();
        Bus::new(registry, NodeId::new("r620"), Arc::new(OsEnv))
    }
}

fn run(bus: &Bus, params: Value) -> Completion {
    let request = Request::new(
        CommandId::new("files.archive.list").unwrap(),
        params,
        Issuer::operator(),
    );
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .build()
        .unwrap();
    match bus.dispatch(request).expect("the operator's own command") {
        Admitted::Running(running) => runtime.block_on(running.finish()),
        other => panic!("expected files.archive.list to run, not {other:?}"),
    }
}

fn ok(bus: &Bus, params: Value) -> Value {
    let done = run(bus, params);
    match done.outcome {
        Ok(v) => v,
        Err(e) => panic!("files.archive.list failed: {e}"),
    }
}

fn err(bus: &Bus, params: Value) -> String {
    match run(bus, params).outcome {
        Ok(v) => panic!("expected a refusal, got {v}"),
        Err(e) => e.to_string(),
    }
}

fn names(listing: &Value) -> Vec<String> {
    listing["entries"]
        .as_array()
        .expect("entries")
        .iter()
        .map(|e| e["name"].as_str().unwrap().to_string())
        .collect()
}

fn entry<'a>(listing: &'a Value, name: &str) -> &'a Value {
    listing["entries"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["name"] == name)
        .unwrap_or_else(|| panic!("{name} is not in {listing}"))
}

// ---------------------------------------------------------------------------
// fixtures
// ---------------------------------------------------------------------------

/// A zip with stored (uncompressed) members, which is all a directory needs.
/// `zip64` saturates the end record and puts the real values in a Zip64
/// record, the way a writer does for an archive past 65535 entries or 4 GiB.
fn zip(members: &[(&str, &[u8])], zip64: bool) -> Vec<u8> {
    let mut out = Vec::new();
    let mut central = Vec::new();
    for (name, body) in members {
        let offset = out.len() as u32;
        let utf8 = if name.is_ascii() { 0u16 } else { 0x0800 };
        // 2024-03-15 13:45:30, DOS-encoded.
        let date: u16 = ((2024 - 1980) << 9) | (3 << 5) | 15;
        let time: u16 = (13 << 11) | (45 << 5) | 15;
        out.extend_from_slice(&0x0403_4b50u32.to_le_bytes());
        out.extend_from_slice(&20u16.to_le_bytes());
        out.extend_from_slice(&utf8.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes()); // stored
        out.extend_from_slice(&time.to_le_bytes());
        out.extend_from_slice(&date.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes()); // crc, unchecked here
        out.extend_from_slice(&(body.len() as u32).to_le_bytes());
        out.extend_from_slice(&(body.len() as u32).to_le_bytes());
        out.extend_from_slice(&(name.len() as u16).to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(name.as_bytes());
        out.extend_from_slice(body);

        central.extend_from_slice(&0x0201_4b50u32.to_le_bytes());
        central.extend_from_slice(&((3u16 << 8) | 20).to_le_bytes()); // made by Unix
        central.extend_from_slice(&20u16.to_le_bytes());
        central.extend_from_slice(&utf8.to_le_bytes());
        central.extend_from_slice(&0u16.to_le_bytes());
        central.extend_from_slice(&time.to_le_bytes());
        central.extend_from_slice(&date.to_le_bytes());
        central.extend_from_slice(&0u32.to_le_bytes());
        central.extend_from_slice(&(body.len() as u32).to_le_bytes());
        central.extend_from_slice(&(body.len() as u32).to_le_bytes());
        central.extend_from_slice(&(name.len() as u16).to_le_bytes());
        central.extend_from_slice(&0u16.to_le_bytes()); // extra
        central.extend_from_slice(&0u16.to_le_bytes()); // comment
        central.extend_from_slice(&0u16.to_le_bytes()); // disk
        central.extend_from_slice(&0u16.to_le_bytes()); // internal
        let mode: u32 = if name.ends_with('/') {
            0o040_755
        } else {
            0o100_644
        };
        central.extend_from_slice(&(mode << 16).to_le_bytes());
        central.extend_from_slice(&offset.to_le_bytes());
        central.extend_from_slice(name.as_bytes());
    }
    let dir_offset = out.len() as u64;
    let dir_size = central.len() as u64;
    out.extend_from_slice(&central);
    let count = members.len() as u64;

    if zip64 {
        let record_at = out.len() as u64;
        out.extend_from_slice(&0x0606_4b50u32.to_le_bytes());
        out.extend_from_slice(&44u64.to_le_bytes());
        out.extend_from_slice(&45u16.to_le_bytes());
        out.extend_from_slice(&45u16.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes());
        out.extend_from_slice(&count.to_le_bytes());
        out.extend_from_slice(&count.to_le_bytes());
        out.extend_from_slice(&dir_size.to_le_bytes());
        out.extend_from_slice(&dir_offset.to_le_bytes());
        out.extend_from_slice(&0x0706_4b50u32.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes());
        out.extend_from_slice(&record_at.to_le_bytes());
        out.extend_from_slice(&1u32.to_le_bytes());
    }

    out.extend_from_slice(&0x0605_4b50u32.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    let (c, s, o) = if zip64 {
        (0xffffu16, 0xffff_ffffu32, 0xffff_ffffu32)
    } else {
        (count as u16, dir_size as u32, dir_offset as u32)
    };
    out.extend_from_slice(&c.to_le_bytes());
    out.extend_from_slice(&c.to_le_bytes());
    out.extend_from_slice(&s.to_le_bytes());
    out.extend_from_slice(&o.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out
}

fn tar_header(name: &[u8], size: u64, kind: u8) -> [u8; 512] {
    let mut h = [0u8; 512];
    let n = name.len().min(100);
    h[..n].copy_from_slice(&name[..n]);
    h[100..107].copy_from_slice(b"0000644");
    h[108..115].copy_from_slice(b"0001750");
    h[116..123].copy_from_slice(b"0001750");
    h[124..135].copy_from_slice(format!("{size:011o}").as_bytes());
    h[136..147].copy_from_slice(format!("{:011o}", 1_710_510_330u64).as_bytes());
    h[156] = kind;
    h[257..263].copy_from_slice(b"ustar\0");
    h[263..265].copy_from_slice(b"00");
    h[148..156].copy_from_slice(b"        ");
    let sum: u32 = h.iter().map(|&b| u32::from(b)).sum();
    h[148..155].copy_from_slice(format!("{sum:06o}\0").as_bytes());
    h
}

fn pad(out: &mut Vec<u8>) {
    while !out.len().is_multiple_of(512) {
        out.push(0);
    }
}

/// A member, with a GNU long-name record in front when the name needs one.
fn tar_member(out: &mut Vec<u8>, name: &str, body: &[u8], kind: u8) {
    if name.len() > 100 {
        let mut long = name.as_bytes().to_vec();
        long.push(0);
        out.extend_from_slice(&tar_header(b"././@LongLink", long.len() as u64, b'L'));
        out.extend_from_slice(&long);
        pad(out);
    }
    out.extend_from_slice(&tar_header(name.as_bytes(), body.len() as u64, kind));
    out.extend_from_slice(body);
    pad(out);
}

/// A member whose path is carried in a pax extended header.
fn tar_pax_member(out: &mut Vec<u8>, path: &str, body: &[u8]) {
    let record = |key: &str, value: &str| {
        let tail = format!(" {key}={value}\n");
        let mut len = tail.len() + 1;
        while format!("{len}{tail}").len() != len {
            len += 1;
        }
        format!("{len}{tail}")
    };
    let pax = record("path", path);
    out.extend_from_slice(&tar_header(b"PaxHeaders/x", pax.len() as u64, b'x'));
    out.extend_from_slice(pax.as_bytes());
    pad(out);
    out.extend_from_slice(&tar_header(b"truncated-name", body.len() as u64, b'0'));
    out.extend_from_slice(body);
    pad(out);
}

fn tar_end(out: &mut Vec<u8>) {
    out.extend_from_slice(&[0u8; 1024]);
}

fn gzip(bytes: &[u8], name: Option<&str>) -> Vec<u8> {
    let mut builder = flate2::GzBuilder::new();
    if let Some(n) = name {
        builder = builder.filename(n);
    }
    let mut enc = builder.write(Vec::new(), flate2::Compression::fast());
    enc.write_all(bytes).unwrap();
    enc.finish().unwrap()
}

// ---------------------------------------------------------------------------
// what is inside
// ---------------------------------------------------------------------------

#[test]
fn a_zip_is_listed_by_name_size_and_kind_without_extracting_anything() {
    let ws = Workspace::new();
    ws.put(
        "photos.zip",
        &zip(
            &[
                ("album/", b""),
                ("album/beach.jpg", b"0123456789"),
                ("Résumé.txt", b"hello"),
            ],
            false,
        ),
    );
    let before = ws.tree();

    let listing = ok(&ws.bus(), json!({ "path": "/photos.zip" }));

    assert_eq!(listing["format"], "zip");
    assert_eq!(
        names(&listing),
        ["album/", "album/beach.jpg", "Résumé.txt"],
        "a UTF-8 name came back mangled, or an entry went missing"
    );
    assert_eq!(entry(&listing, "album/")["is_dir"], true);
    assert_eq!(entry(&listing, "album/beach.jpg")["is_dir"], false);
    assert_eq!(entry(&listing, "album/beach.jpg")["size"], 10);
    assert_eq!(
        entry(&listing, "album/beach.jpg")["modified"],
        1_710_510_330,
        "the DOS timestamp was not decoded"
    );
    assert_eq!(listing["total_entries"], 3);
    assert_eq!(listing["truncated"], false);
    assert_eq!(ws.tree(), before, "listing an archive wrote to the disk");
}

#[test]
fn a_zip64_directory_is_found_through_its_locator() {
    // Every archive past 65535 entries or 4 GiB is Zip64, and its plain end
    // record holds 0xffff placeholders. Reading those as the truth lists
    // nothing, or reads a directory from a nonsense offset.
    let ws = Workspace::new();
    ws.put(
        "backup.zip",
        &zip(&[("a.txt", b"a"), ("b/c.txt", b"bc")], true),
    );

    let listing = ok(&ws.bus(), json!({ "path": "/backup.zip" }));

    assert_eq!(names(&listing), ["a.txt", "b/c.txt"]);
    assert_eq!(listing["total_entries"], 2);
}

#[test]
fn a_tarball_is_listed_including_names_too_long_for_its_header() {
    // A ustar header has 100 bytes for a name; real source trees overflow it
    // constantly, through GNU long-name records or pax headers. Reading only
    // the header shows a truncated or placeholder name.
    let long = format!("src/{}/main.rs", "deeply/nested".repeat(10));
    let mut tar = Vec::new();
    tar_member(&mut tar, "project/", b"", b'5');
    tar_member(&mut tar, "project/README.md", b"# hi\n", b'0');
    tar_member(&mut tar, &long, b"fn main() {}\n", b'0');
    tar_pax_member(&mut tar, "project/ünïcode name.txt", b"x");
    tar_member(&mut tar, "project/link", b"", b'2');
    tar_end(&mut tar);

    let ws = Workspace::new();
    ws.put("src.tar", &tar);

    let listing = ok(&ws.bus(), json!({ "path": "/src.tar" }));

    assert_eq!(listing["format"], "tar");
    assert_eq!(
        names(&listing),
        [
            "project/",
            "project/README.md",
            long.as_str(),
            "project/ünïcode name.txt",
            "project/link"
        ]
    );
    assert_eq!(entry(&listing, "project/")["is_dir"], true);
    assert_eq!(entry(&listing, "project/README.md")["size"], 5);
    assert_eq!(entry(&listing, "project/link")["is_link"], true);
    assert_eq!(
        entry(&listing, "project/README.md")["modified"],
        1_710_510_330
    );
}

#[test]
fn a_gzipped_tarball_is_listed_through_the_compression() {
    let mut tar = Vec::new();
    tar_member(&mut tar, "pkg/a.txt", b"aaa", b'0');
    tar_member(&mut tar, "pkg/b.txt", &[b'b'; 3000], b'0');
    tar_end(&mut tar);

    let ws = Workspace::new();
    ws.put("pkg.tar.gz", &gzip(&tar, None));

    let listing = ok(&ws.bus(), json!({ "path": "/pkg.tar.gz" }));

    assert_eq!(listing["format"], "tar_gzip");
    assert_eq!(names(&listing), ["pkg/a.txt", "pkg/b.txt"]);
    assert_eq!(entry(&listing, "pkg/b.txt")["size"], 3000);
}

#[test]
fn a_gzip_of_one_file_is_shown_as_that_file() {
    // `server.log.gz` is not an archive, but it is what people double-click
    // expecting to see what is inside.
    let ws = Workspace::new();
    ws.put(
        "server.log.gz",
        &gzip(b"line one\nline two\n", Some("server.log")),
    );

    let listing = ok(&ws.bus(), json!({ "path": "/server.log.gz" }));

    assert_eq!(listing["format"], "gzip");
    assert_eq!(names(&listing), ["server.log"]);
    assert_eq!(listing["entries"][0]["size"], 18);
}

#[test]
fn the_format_comes_from_the_bytes_not_the_name() {
    let ws = Workspace::new();
    ws.put("download.bin", &zip(&[("inside.txt", b"x")], false));

    let listing = ok(&ws.bus(), json!({ "path": "/download.bin" }));

    assert_eq!(listing["format"], "zip");
    assert_eq!(names(&listing), ["inside.txt"]);
}

// ---------------------------------------------------------------------------
// what it refuses, and how
// ---------------------------------------------------------------------------

#[test]
fn a_format_it_cannot_list_is_named_in_the_refusal() {
    let ws = Workspace::new();
    ws.put("bundle.7z", b"7z\xbc\xaf\x27\x1c\x00\x04rest of a 7z file");
    ws.put("notes.txt", b"just some text, not an archive at all");
    let bus = ws.bus();

    let seven = err(&bus, json!({ "path": "/bundle.7z" }));
    assert!(seven.contains("7-Zip"), "{seven}");

    let text = err(&bus, json!({ "path": "/notes.txt" }));
    assert!(text.contains("not a zip"), "{text}");
}

#[test]
fn a_damaged_zip_is_refused_with_a_reason_rather_than_listed_as_empty() {
    let ws = Workspace::new();
    let whole = zip(&[("a.txt", b"aaaa"), ("b.txt", b"bbbb")], false);
    // Cut off the end record: what a half-finished download looks like.
    ws.put("partial.zip", &whole[..whole.len() - 30]);

    let why = err(&ws.bus(), json!({ "path": "/partial.zip" }));
    assert!(why.contains("end-of-directory"), "{why}");
}

#[test]
fn a_long_listing_stops_at_the_cap_and_says_so() {
    let ws = Workspace::new();
    let bodies: Vec<String> = (0..10).map(|i| format!("file-{i:02}.txt")).collect();
    let members: Vec<(&str, &[u8])> = bodies.iter().map(|n| (n.as_str(), &b"x"[..])).collect();
    ws.put("many.zip", &zip(&members, false));

    let listing = ok(&ws.bus(), json!({ "path": "/many.zip", "max_entries": 3 }));

    assert_eq!(names(&listing).len(), 3);
    assert_eq!(listing["truncated"], true, "a partial listing looked whole");
    assert_eq!(
        listing["total_entries"], 10,
        "the operator is not told how many there are"
    );
}

#[test]
fn a_client_cannot_raise_the_servers_entry_cap() {
    // The cap is the node's memory bound for one answer, not a preference.
    let cap = deckjack_core::archive::MAX_ENTRIES;
    let mut tar = Vec::new();
    for i in 0..=cap {
        tar.extend_from_slice(&tar_header(format!("f{i}").as_bytes(), 0, b'0'));
    }
    tar_end(&mut tar);
    let ws = Workspace::new();
    ws.put("huge.tar", &tar);

    let listing = ok(
        &ws.bus(),
        json!({ "path": "/huge.tar", "max_entries": u64::MAX }),
    );

    assert_eq!(names(&listing).len(), cap);
    assert_eq!(listing["truncated"], true);
}

#[test]
fn it_crosses_the_same_boundary_as_every_other_file_command() {
    let ws = Workspace::new();
    ws.put(".deckjack/state.zip", &zip(&[("secret", b"x")], false));
    ws.put("folder/a.txt", b"x");
    let bus = ws.bus();

    let hidden = err(&bus, json!({ "path": "/.deckjack/state.zip" }));
    assert!(!hidden.contains("secret"), "{hidden}");

    let climbing = err(&bus, json!({ "path": "/../outside.zip" }));
    assert!(!climbing.is_empty());

    let folder = err(&bus, json!({ "path": "/folder" }));
    assert!(folder.contains("folder"), "{folder}");
}

#[test]
fn a_named_pipe_is_refused_rather_than_waited_on() {
    // Opening a FIFO for reading blocks until something writes to it, and a
    // read-only command needs no approval: without the check, any caller
    // could park a blocking-pool thread per call, forever.
    let ws = Workspace::new();
    let fifo = ws.root.join("pipe.zip");
    let c = std::ffi::CString::new(fifo.as_os_str().as_encoded_bytes()).unwrap();
    // SAFETY: a valid NUL-terminated path; mkfifo only reads it.
    assert_eq!(
        unsafe { libc::mkfifo(c.as_ptr(), 0o600) },
        0,
        "mkfifo failed"
    );
    let bus = ws.bus();

    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(err(&bus, json!({ "path": "/pipe.zip" })));
    });
    match rx.recv_timeout(std::time::Duration::from_secs(5)) {
        Ok(why) => assert!(why.contains("not a regular file"), "{why}"),
        Err(_) => {
            // Release the blocked open so the thread can finish, then fail.
            let _ = std::fs::OpenOptions::new().write(true).open(&fifo);
            panic!("files.archive.list blocked on a named pipe");
        }
    }
}

// ---------------------------------------------------------------------------
// bounded in what it holds and what it sends
// ---------------------------------------------------------------------------

#[test]
fn a_pax_name_longer_than_any_real_path_is_refused_and_the_limit_named() {
    // A pax `path` record compresses about a thousand to one, so a half
    // megabyte .tgz could hand the node a gigabyte of "file name" to hold and
    // send. One absurd name refuses the listing, and the refusal says why.
    let limit = deckjack_core::archive::MAX_NAME_BYTES;
    let mut tar = Vec::new();
    tar_pax_member(&mut tar, &"n".repeat(1_000_000), b"x");
    tar_end(&mut tar);
    let ws = Workspace::new();
    ws.put("bomb.tgz", &gzip(&tar, None));

    let why = err(&ws.bus(), json!({ "path": "/bomb.tgz" }));

    assert!(why.contains(&format!("{limit} byte")), "{why}");
    assert!(
        why.len() < 1024,
        "the refusal carries the name: {} bytes",
        why.len()
    );
}

#[test]
fn a_gnu_long_name_longer_than_any_real_path_is_refused_too() {
    let limit = deckjack_core::archive::MAX_NAME_BYTES;
    let mut tar = Vec::new();
    tar_member(&mut tar, &"g".repeat(60_000), b"x", b'0');
    tar_end(&mut tar);
    let ws = Workspace::new();
    ws.put("long.tar", &tar);

    let why = err(&ws.bus(), json!({ "path": "/long.tar" }));

    assert!(why.contains(&format!("{limit} byte")), "{why}");
}

#[test]
fn a_zip_name_longer_than_any_real_path_is_refused_too() {
    let limit = deckjack_core::archive::MAX_NAME_BYTES;
    let long = "z".repeat(60_000);
    let ws = Workspace::new();
    ws.put("long.zip", &zip(&[(long.as_str(), b"x")], false));

    let why = err(&ws.bus(), json!({ "path": "/long.zip" }));

    assert!(why.contains(&format!("{limit} byte")), "{why}");
}

#[test]
fn a_gzip_header_name_longer_than_any_real_path_is_refused_too() {
    // A single gzipped file carries its name in the gzip header, which no
    // tar or zip check sees. Without its own check a 60 kB name came back
    // whole in the listing.
    let limit = deckjack_core::archive::MAX_NAME_BYTES;
    let long = "g".repeat(60_000);
    let ws = Workspace::new();
    ws.put("long.gz", &gzip(b"one line\n", Some(long.as_str())));

    let why = err(&ws.bus(), json!({ "path": "/long.gz" }));

    assert!(why.contains(&format!("{limit} byte")), "{why}");
    assert!(!why.contains(&long[..100]), "the refusal echoed the name");
}

#[test]
fn many_long_names_stop_the_listing_at_its_size_limit_and_say_so() {
    // Each name is legal on its own; together they would be a reply of tens
    // of megabytes. What is held and sent is bounded by bytes, not only by
    // the entry count.
    let limit = deckjack_core::archive::MAX_LISTING_BYTES;
    let name_len = deckjack_core::archive::MAX_NAME_BYTES - 16;
    let count = 3 * limit / name_len;
    assert!(count < deckjack_core::archive::MAX_ENTRIES);
    let mut tar = Vec::new();
    for i in 0..count {
        let name = format!("{i:08}{}", "n".repeat(name_len - 8));
        tar_pax_member(&mut tar, &name, b"");
    }
    tar_end(&mut tar);
    let ws = Workspace::new();
    ws.put("long-names.tgz", &gzip(&tar, None));

    let listing = ok(&ws.bus(), json!({ "path": "/long-names.tgz" }));

    let sent = serde_json::to_vec(&listing).unwrap().len();
    assert!(
        sent <= limit + 4096,
        "a reply of {sent} bytes against {limit}"
    );
    assert!(
        !names(&listing).is_empty(),
        "the listing stopped before anything"
    );
    assert_eq!(listing["truncated"], true, "a partial listing looked whole");
    let stopped = listing["stopped"].as_str().unwrap_or("");
    assert!(
        stopped.contains(&format!("{} MiB", limit / (1024 * 1024))),
        "{stopped}"
    );
}

#[test]
fn a_zip_of_long_names_is_held_to_the_same_size_limit() {
    // A zip name can be 64 KiB and its directory 64 MiB; the reply bound is
    // the same whichever format the names came from.
    let limit = deckjack_core::archive::MAX_LISTING_BYTES;
    let name_len = deckjack_core::archive::MAX_NAME_BYTES - 16;
    let owned: Vec<String> = (0..3 * limit / name_len)
        .map(|i| format!("{i:08}{}", "z".repeat(name_len - 8)))
        .collect();
    let members: Vec<(&str, &[u8])> = owned.iter().map(|n| (n.as_str(), &b""[..])).collect();
    let ws = Workspace::new();
    ws.put("long-names.zip", &zip(&members, false));

    let listing = ok(&ws.bus(), json!({ "path": "/long-names.zip" }));

    let sent = serde_json::to_vec(&listing).unwrap().len();
    assert!(
        sent <= limit + 4096,
        "a reply of {sent} bytes against {limit}"
    );
    assert_eq!(listing["truncated"], true);
}
