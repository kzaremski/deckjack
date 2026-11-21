//! What the Files view needs from the core beyond moving bytes around:
//! thumbnails, free space, and a copy into the folder it came from.
//!
//! On a real disk with real pictures, made here with the same decoders the
//! core uses, because a thumbnail test against a fake decoder proves only that
//! the fake was called.

use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use base64::Engine as _;
use image::{DynamicImage, ImageEncoder as _, ImageFormat, Rgb, RgbImage, Rgba, RgbaImage};
use serde_json::{Value, json};

use deckjack_core::command::{
    Admitted, Bus, CommandError, CommandId, Completion, Issuer, NodeId, OsEnv, Registry, Request,
    RiskLevel,
};
use deckjack_core::files::{Namespace, NodePath, Root, register_commands};
use deckjack_core::thumbnail::{self, ThumbnailCache, ThumbnailError, Thumbnailer};
use deckjack_core::transfer::copy_name;

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

    fn host(&self, rel: &str) -> PathBuf {
        self.root.join(rel)
    }

    /// The install directory `register_commands` is given: beside the
    /// working directory, as `--dir` elsewhere leaves it.
    fn install(&self) -> PathBuf {
        self.root.parent().unwrap().to_path_buf()
    }

    /// Where the file commands keep thumbnails: the install directory's.
    fn cache(&self) -> PathBuf {
        thumbnail::cache_dir(&self.install())
    }

    fn write(&self, rel: &str, bytes: &[u8]) {
        let path = self.host(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, bytes).unwrap();
    }

    fn read(&self, rel: &str) -> Option<Vec<u8>> {
        std::fs::read(self.host(rel)).ok()
    }

    fn bus(&self) -> Bus {
        let registry = Arc::new(Registry::new());
        register_commands(&registry, &self.root, "r620", &self.install()).unwrap();
        Bus::new(registry, NodeId::new("r620"), Arc::new(OsEnv))
    }

    /// A thumbnailer of its own, for the tests that need a small cache cap.
    fn thumbnailer(&self, cache: &Path, cap: u64) -> Thumbnailer {
        let files = Arc::new(Namespace::new(
            "r620".parse().unwrap(),
            Root::open(&self.root).unwrap(),
        ));
        Thumbnailer::new(files, ThumbnailCache::new(cache.to_path_buf(), cap))
    }
}

fn run(bus: &Bus, command: &str, params: Value) -> Completion {
    let request = Request::new(CommandId::new(command).unwrap(), params, Issuer::operator());
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .build()
        .unwrap();
    match bus.dispatch(request).expect("the operator's own command") {
        Admitted::Running(running) => runtime.block_on(running.finish()),
        other => panic!("expected {command} to run, not {other:?}"),
    }
}

fn ok(bus: &Bus, command: &str, params: Value) -> Value {
    run(bus, command, params)
        .outcome
        .unwrap_or_else(|e| panic!("{command} failed: {e}"))
}

fn addr(text: &str) -> NodePath {
    text.parse().unwrap()
}

/// A JPEG of a horizontal gradient, `w` by `h`.
fn jpeg(w: u32, h: u32) -> Vec<u8> {
    let img = RgbImage::from_fn(w, h, |x, _| Rgb([(x * 255 / w.max(1)) as u8, 80, 160]));
    let mut out = Cursor::new(Vec::new());
    DynamicImage::ImageRgb8(img)
        .write_to(&mut out, ImageFormat::Jpeg)
        .unwrap();
    out.into_inner()
}

/// A PNG with a transparent half.
fn png_with_alpha(w: u32, h: u32) -> Vec<u8> {
    let img = RgbaImage::from_fn(w, h, |x, _| {
        Rgba([200, 30, 30, if x < w / 2 { 0 } else { 255 }])
    });
    let mut out = Cursor::new(Vec::new());
    DynamicImage::ImageRgba8(img)
        .write_to(&mut out, ImageFormat::Png)
        .unwrap();
    out.into_inner()
}

/// The thumbnail's own bytes, decoded: what the browser will actually show.
fn shown(thumb: &Value) -> DynamicImage {
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(thumb["content"].as_str().unwrap())
        .unwrap();
    image::load_from_memory(&bytes).unwrap()
}

fn cache_files(dir: &Path) -> Vec<(PathBuf, u64)> {
    match std::fs::read_dir(dir) {
        Ok(entries) => entries
            .map(|e| {
                let e = e.unwrap();
                (e.path(), e.metadata().unwrap().len())
            })
            .collect(),
        Err(_) => Vec::new(),
    }
}

// ---------------------------------------------------------------------------
// Thumbnails: what they look like
// ---------------------------------------------------------------------------

#[test]
fn a_photo_is_shrunk_to_the_size_asked_for_and_keeps_its_shape() {
    let ws = Workspace::new();
    ws.write("photos/wide.jpg", &jpeg(800, 400));
    let bus = ws.bus();

    let thumb = ok(
        &bus,
        "files.thumbnail",
        json!({ "path": "/photos/wide.jpg", "size": 256 }),
    );
    assert_eq!(thumb["mime"], "image/jpeg");
    assert_eq!(
        (thumb["width"].clone(), thumb["height"].clone()),
        (json!(256), json!(128))
    );
    let picture = shown(&thumb);
    assert_eq!(
        (picture.width(), picture.height()),
        (256, 128),
        "the reported size is not the size of the bytes"
    );
    assert_eq!(thumb["path"], "r620:/photos/wide.jpg");
}

#[test]
fn a_size_between_the_kept_ones_is_rounded_up_and_a_huge_one_is_capped() {
    let ws = Workspace::new();
    ws.write("big.jpg", &jpeg(2000, 1000));
    let bus = ws.bus();
    let at = |size: u64| {
        ok(
            &bus,
            "files.thumbnail",
            json!({ "path": "/big.jpg", "size": size }),
        )["width"]
            .as_u64()
            .unwrap()
    };
    assert_eq!(at(200), 256);
    assert_eq!(at(100), 128);
    assert_eq!(
        at(10_000),
        512,
        "a client must not be able to ask for a full-size decode"
    );
    let default = ok(&bus, "files.thumbnail", json!({ "path": "/big.jpg" }));
    assert_eq!(default["width"], 256);
}

#[test]
fn a_picture_smaller_than_the_thumbnail_is_not_enlarged() {
    let ws = Workspace::new();
    ws.write("icon.jpg", &jpeg(40, 30));
    let thumb = ok(&ws.bus(), "files.thumbnail", json!({ "path": "/icon.jpg" }));
    assert_eq!(
        (thumb["width"].clone(), thumb["height"].clone()),
        (json!(40), json!(30))
    );
}

#[test]
fn transparency_survives_as_png_rather_than_turning_black() {
    let ws = Workspace::new();
    ws.write("logo.png", &png_with_alpha(600, 300));
    let thumb = ok(&ws.bus(), "files.thumbnail", json!({ "path": "/logo.png" }));
    assert_eq!(thumb["mime"], "image/png");
    let picture = shown(&thumb).to_rgba8();
    assert_eq!(
        picture.get_pixel(0, 0)[3],
        0,
        "the transparent half became opaque"
    );
    assert_eq!(picture.get_pixel(picture.width() - 1, 0)[3], 255);
}

#[test]
fn a_sideways_phone_photo_is_shown_the_right_way_up() {
    // EXIF orientation 6: stored landscape, meant to be seen rotated 90
    // degrees clockwise. Every portrait a phone takes looks like this.
    let exif: Vec<u8> = vec![
        b'I', b'I', 0x2a, 0, 8, 0, 0, 0, // TIFF header, first IFD at 8
        1, 0, // one entry
        0x12, 0x01, 3, 0, 1, 0, 0, 0, 6, 0, 0, 0, // Orientation = 6
        0, 0, 0, 0, // no next IFD
    ];
    let img = RgbImage::from_pixel(400, 200, Rgb([10, 200, 10]));
    let mut out = Cursor::new(Vec::new());
    let mut encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, 90);
    encoder.set_exif_metadata(exif).unwrap();
    encoder
        .write_image(img.as_raw(), 400, 200, image::ExtendedColorType::Rgb8)
        .unwrap();

    let ws = Workspace::new();
    ws.write("portrait.jpg", &out.into_inner());
    let thumb = ok(
        &ws.bus(),
        "files.thumbnail",
        json!({ "path": "/portrait.jpg", "size": 128 }),
    );
    assert_eq!(
        (thumb["width"].clone(), thumb["height"].clone()),
        (json!(64), json!(128)),
        "the portrait is lying on its side"
    );
}

#[test]
fn the_format_is_read_from_the_bytes_not_trusted_from_the_name() {
    let ws = Workspace::new();
    ws.write("really-a-png.jpg", &png_with_alpha(300, 300));
    let thumb = ok(
        &ws.bus(),
        "files.thumbnail",
        json!({ "path": "/really-a-png.jpg" }),
    );
    assert_eq!(thumb["mime"], "image/png");
}

// ---------------------------------------------------------------------------
// Thumbnails: what is refused
// ---------------------------------------------------------------------------

#[test]
fn a_file_that_is_not_a_picture_has_no_thumbnail_and_is_not_read() {
    let ws = Workspace::new();
    ws.write("notes.txt", b"hello");
    let e = run(
        &ws.bus(),
        "files.thumbnail",
        json!({ "path": "/notes.txt" }),
    )
    .outcome
    .unwrap_err();
    assert!(matches!(e, CommandError::Unsupported { .. }), "{e}");
    assert!(e.to_string().contains("JPEG"), "say what it does read: {e}");
}

#[test]
fn a_corrupt_picture_is_a_failure_with_its_name_not_a_crash() {
    let ws = Workspace::new();
    ws.write("broken.jpg", b"\xff\xd8\xff\xe0 this is not really a jpeg");
    let e = run(
        &ws.bus(),
        "files.thumbnail",
        json!({ "path": "/broken.jpg" }),
    )
    .outcome
    .unwrap_err();
    assert!(matches!(e, CommandError::Failed(_)), "{e}");
    assert!(e.to_string().contains("/broken.jpg"), "{e}");
}

#[test]
fn a_file_over_the_size_limit_is_refused_before_it_is_read() {
    let ws = Workspace::new();
    // Sparse: the length is real, the disk use is not.
    let f = std::fs::File::create(ws.host("huge.jpg")).unwrap();
    f.set_len(thumbnail::MAX_SOURCE + 1).unwrap();
    let e = run(&ws.bus(), "files.thumbnail", json!({ "path": "/huge.jpg" }))
        .outcome
        .unwrap_err();
    assert!(matches!(e, CommandError::Unsupported { .. }), "{e}");
    assert!(e.to_string().contains("limit"), "{e}");
}

#[test]
fn the_workspace_boundary_holds_for_thumbnails_too() {
    let ws = Workspace::new();
    let outside = ws.root.parent().unwrap().join("secret.jpg");
    std::fs::write(&outside, jpeg(64, 64)).unwrap();
    std::os::unix::fs::symlink(&outside, ws.host("out.jpg")).unwrap();
    ws.write(".deckjack/inside.jpg", &jpeg(64, 64));
    let bus = ws.bus();

    for (path, what) in [
        ("/out.jpg", "a link out of the workspace"),
        ("/.deckjack/inside.jpg", "the hidden state directory"),
        ("/../secret.jpg", "a parent reference"),
    ] {
        let outcome = run(&bus, "files.thumbnail", json!({ "path": path })).outcome;
        assert!(outcome.is_err(), "{what} was thumbnailed: {outcome:?}");
    }
}

#[test]
fn a_picture_inside_the_cache_itself_is_not_thumbnailed() {
    // When the working directory is the install directory, which is the
    // default, the cache is inside what Files lists.
    let ws = Workspace::new();
    let cache = ws.host("cache/files");
    ws.write("cache/files/stray.png", &png_with_alpha(64, 64));
    let thumbnailer = ws.thumbnailer(&cache, thumbnail::DEFAULT_CACHE_CAP);
    let e = thumbnailer
        .make(&addr("/cache/files/stray.png"), None)
        .unwrap_err();
    assert!(matches!(e, ThumbnailError::NotAnImage { .. }), "{e}");
    // An ordinary picture beside it still works.
    ws.write("cache/elsewhere.png", &png_with_alpha(64, 64));
    thumbnailer
        .make(&addr("/cache/elsewhere.png"), None)
        .unwrap();
}

#[test]
fn it_is_a_read_command() {
    let ws = Workspace::new();
    let bus = ws.bus();
    for id in ["files.thumbnail", "files.space"] {
        let spec = bus.registry().spec(&CommandId::new(id).unwrap()).unwrap();
        assert_eq!(spec.risk(), RiskLevel::ReadOnly, "{id}");
    }
}

// ---------------------------------------------------------------------------
// Thumbnails: the cache
// ---------------------------------------------------------------------------

#[test]
fn the_cache_lives_in_the_install_directory_cache_beside_the_apps_own() {
    // The documented place: deleting `<install>/cache` is how an operator
    // clears every cache at once. That `register_commands` uses it for the
    // install directory it is given is checked by
    // `thumbnails_are_cached_under_the_install_directory_the_file_layer_is_given`,
    // and that `main` gives it the real install directory by
    // `the_served_thumbnail_cache_is_the_install_directorys_not_the_working_directorys`,
    // which runs the binary (both in `tests/install_layout.rs`).
    let install = Path::new("/srv/deckjack");
    assert_eq!(
        thumbnail::cache_dir(install),
        install
            .join(deckjack_core::apps::CACHE_SUBDIR)
            .join("files")
    );
    assert_eq!(
        thumbnail::cache_dir(install),
        Path::new("/srv/deckjack/cache/files")
    );
}

#[test]
fn the_second_request_is_served_from_the_cache_under_the_install_cache() {
    let ws = Workspace::new();
    ws.write("a.jpg", &jpeg(640, 480));
    let bus = ws.bus();

    let first = ok(&bus, "files.thumbnail", json!({ "path": "/a.jpg" }));
    assert_eq!(first["cached"], false);
    assert_eq!(
        cache_files(&ws.cache()).len(),
        1,
        "nothing was written to the cache"
    );
    assert!(
        cache_files(&ws.root)
            .iter()
            .all(|(p, _)| !p.ends_with("cache")),
        "the cache is in the working directory"
    );

    let second = ok(&bus, "files.thumbnail", json!({ "path": "/a.jpg" }));
    assert_eq!(second["cached"], true);
    assert_eq!(second["content"], first["content"]);
    assert_eq!(second["width"], first["width"]);
}

#[test]
fn an_edited_picture_gets_a_new_thumbnail_not_the_old_one() {
    let ws = Workspace::new();
    ws.write("a.jpg", &jpeg(640, 480));
    let bus = ws.bus();
    ok(&bus, "files.thumbnail", json!({ "path": "/a.jpg" }));

    ws.write("a.jpg", &jpeg(300, 600));
    let after = ok(&bus, "files.thumbnail", json!({ "path": "/a.jpg" }));
    assert_eq!(after["cached"], false);
    assert_eq!(
        (after["width"].clone(), after["height"].clone()),
        (json!(128), json!(256))
    );
}

/// Two pictures of exactly the same length: a JPEG decoder stops at the
/// end-of-image marker, so trailing zeros change the length and not the
/// picture.
fn same_length(a: Vec<u8>, b: Vec<u8>) -> (Vec<u8>, Vec<u8>) {
    let len = a.len().max(b.len());
    let pad = |mut v: Vec<u8>| {
        v.resize(len, 0);
        v
    };
    (pad(a), pad(b))
}

#[test]
fn a_picture_replaced_by_one_of_the_same_length_gets_a_new_thumbnail() {
    let ws = Workspace::new();
    let (before, after) = same_length(jpeg(640, 480), jpeg(300, 600));
    ws.write("a.jpg", &before);
    let bus = ws.bus();
    ok(&bus, "files.thumbnail", json!({ "path": "/a.jpg" }));

    // Later, so the modification time differs even on a coarse clock.
    let f = std::fs::File::options()
        .write(true)
        .open(ws.host("a.jpg"))
        .unwrap();
    std::io::Write::write_all(&mut &f, &after).unwrap();
    f.set_modified(std::time::SystemTime::now() + std::time::Duration::from_secs(5))
        .unwrap();
    drop(f);
    let thumb = ok(&bus, "files.thumbnail", json!({ "path": "/a.jpg" }));
    assert_eq!(
        thumb["width"], 128,
        "the old picture was served for the new one"
    );
}

#[test]
fn a_picture_whose_length_changed_under_the_same_timestamp_gets_a_new_thumbnail() {
    // A copy tool that preserves timestamps, or a coarse filesystem clock,
    // leaves the time alone; the length is what tells.
    let ws = Workspace::new();
    ws.write("a.jpg", &jpeg(640, 480));
    let stamp = std::fs::metadata(ws.host("a.jpg"))
        .unwrap()
        .modified()
        .unwrap();
    let bus = ws.bus();
    ok(&bus, "files.thumbnail", json!({ "path": "/a.jpg" }));

    let mut longer = jpeg(300, 600);
    longer.resize(longer.len().max(jpeg(640, 480).len()) + 7, 0);
    ws.write("a.jpg", &longer);
    std::fs::File::options()
        .write(true)
        .open(ws.host("a.jpg"))
        .unwrap()
        .set_modified(stamp)
        .unwrap();
    let thumb = ok(&bus, "files.thumbnail", json!({ "path": "/a.jpg" }));
    assert_eq!(
        thumb["width"], 128,
        "the old picture was served for the new one"
    );
}

#[test]
fn each_size_is_its_own_cache_entry() {
    let ws = Workspace::new();
    ws.write("a.jpg", &jpeg(1000, 1000));
    let bus = ws.bus();
    ok(
        &bus,
        "files.thumbnail",
        json!({ "path": "/a.jpg", "size": 128 }),
    );
    let big = ok(
        &bus,
        "files.thumbnail",
        json!({ "path": "/a.jpg", "size": 512 }),
    );
    assert_eq!(big["cached"], false, "a 128 thumbnail was served for 512");
    assert_eq!(big["width"], 512);
}

#[test]
fn deleting_the_cache_while_running_costs_a_regeneration_and_nothing_else() {
    let ws = Workspace::new();
    ws.write("a.jpg", &jpeg(640, 480));
    let bus = ws.bus();
    ok(&bus, "files.thumbnail", json!({ "path": "/a.jpg" }));

    std::fs::remove_dir_all(ws.root.parent().unwrap().join("cache")).unwrap();
    let again = ok(&bus, "files.thumbnail", json!({ "path": "/a.jpg" }));
    assert_eq!(again["cached"], false);
    assert_eq!(
        cache_files(&ws.cache()).len(),
        1,
        "the cache was not recreated"
    );
    assert_eq!(
        ok(&bus, "files.thumbnail", json!({ "path": "/a.jpg" }))["cached"],
        true
    );
}

#[test]
fn a_cache_that_cannot_be_written_still_gives_the_thumbnail() {
    let ws = Workspace::new();
    ws.write("a.jpg", &jpeg(640, 480));
    // A file where the cache directory should be.
    let blocked = ws.root.parent().unwrap().join("blocked");
    std::fs::write(&blocked, b"not a directory").unwrap();
    let thumbnailer = ws.thumbnailer(&blocked.join("files"), thumbnail::DEFAULT_CACHE_CAP);
    let thumb = thumbnailer.make(&addr("/a.jpg"), None).unwrap();
    assert_eq!(thumb.width, 256);
    assert!(!thumb.cached);
}

#[test]
fn the_cache_stays_under_its_cap_and_keeps_what_was_used_last() {
    let ws = Workspace::new();
    let cache = ws.root.parent().unwrap().join("small-cache");
    // Measure one entry, then allow about three.
    ws.write("probe.jpg", &jpeg(600, 600));
    let probe = ws.thumbnailer(&cache, u64::MAX);
    probe.make(&addr("/probe.jpg"), None).unwrap();
    let one = cache_files(&cache)[0].1;
    std::fs::remove_dir_all(&cache).unwrap();

    let cap = one * 3 + one / 2;
    let thumbnailer = ws.thumbnailer(&cache, cap);
    for i in 0..10 {
        ws.write(&format!("p{i}.jpg"), &jpeg(600, 600 - i));
        thumbnailer
            .make(&addr(&format!("/p{i}.jpg")), None)
            .unwrap();
        let total: u64 = cache_files(&cache).iter().map(|(_, len)| len).sum();
        assert!(
            total <= cap,
            "after {i}: {total} bytes in a {cap} byte cache"
        );
    }
    let last = thumbnailer.make(&addr("/p9.jpg"), None).unwrap();
    assert!(last.cached, "the newest entry was the one evicted");
}

#[test]
fn a_thumbnail_looked_at_again_outlives_newer_ones_nobody_looked_at() {
    let ws = Workspace::new();
    let cache = ws.root.parent().unwrap().join("small-cache");
    ws.write("probe.jpg", &jpeg(600, 600));
    ws.thumbnailer(&cache, u64::MAX)
        .make(&addr("/probe.jpg"), None)
        .unwrap();
    let one = cache_files(&cache)[0].1;
    std::fs::remove_dir_all(&cache).unwrap();

    let thumbnailer = ws.thumbnailer(&cache, one * 3 + one / 2);
    ws.write("kept.jpg", &jpeg(600, 600));
    thumbnailer.make(&addr("/kept.jpg"), None).unwrap();
    for i in 0..8 {
        // Distinct timestamps, so "last used" has an order to keep.
        std::thread::sleep(std::time::Duration::from_millis(15));
        ws.write(&format!("p{i}.jpg"), &jpeg(600, 590 - i));
        thumbnailer
            .make(&addr(&format!("/p{i}.jpg")), None)
            .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(15));
        let again = thumbnailer.make(&addr("/kept.jpg"), None).unwrap();
        assert!(again.cached, "after {i}: the one in use was evicted");
    }
}

// ---------------------------------------------------------------------------
// Free space
// ---------------------------------------------------------------------------

#[test]
fn free_space_is_the_working_directory_filesystem_in_bytes() {
    let ws = Workspace::new();
    let space = ok(&ws.bus(), "files.space", json!({}));
    let total = space["total"].as_u64().unwrap();
    let free = space["free"].as_u64().unwrap();
    let available = space["available"].as_u64().unwrap();
    assert!(available <= free && free <= total, "{space}");

    // Checked against the kernel's own numbers, read here independently, so
    // a count of blocks reported as bytes cannot pass.
    let stat = nix::sys::statvfs::statvfs(&ws.root).unwrap();
    let unit = stat.fragment_size() as u64;
    assert_eq!(total, stat.blocks() as u64 * unit);
    let kernel_available = stat.blocks_available() as u64 * unit;
    // Other tests write to the same disk while this one runs.
    let slack = 512 * 1024 * 1024;
    assert!(
        available.abs_diff(kernel_available) < slack,
        "{available} available, the kernel says {kernel_available}"
    );
}

#[test]
fn available_is_what_this_user_can_write_not_what_root_could() {
    // On tmpfs the two are equal and nothing can tell them apart, so this
    // runs in the build's own scratch directory, which is on a real disk
    // with blocks reserved for root wherever the filesystem reserves them.
    let dir = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let registry = Arc::new(Registry::new());
    register_commands(&registry, dir.path(), "r620", &dir.path().join("install")).unwrap();
    let bus = Bus::new(registry, NodeId::new("r620"), Arc::new(OsEnv));
    let space = ok(&bus, "files.space", json!({}));
    let available = space["available"].as_u64().unwrap();
    let free = space["free"].as_u64().unwrap();

    let stat = nix::sys::statvfs::statvfs(dir.path()).unwrap();
    let unit = stat.fragment_size() as u64;
    let kernel_free = stat.blocks_free() as u64 * unit;
    let kernel_available = stat.blocks_available() as u64 * unit;
    let gap = kernel_free - kernel_available;
    if gap < 256 * 1024 * 1024 {
        eprintln!("this filesystem reserves nothing ({gap} bytes); nothing to tell apart");
        return;
    }
    assert!(
        available.abs_diff(kernel_available) < available.abs_diff(kernel_free),
        "available {available} is the root-only figure {kernel_free}, not {kernel_available}"
    );
    assert!(free.abs_diff(kernel_free) < free.abs_diff(kernel_available));
}

#[test]
fn free_space_takes_no_parameters() {
    let ws = Workspace::new();
    let e = run(&ws.bus(), "files.space", json!({ "path": "/etc" }))
        .outcome
        .unwrap_err();
    assert!(matches!(e, CommandError::BadParams { .. }), "{e}");
}

// ---------------------------------------------------------------------------
// Copying into the folder it came from
// ---------------------------------------------------------------------------

#[test]
fn a_copy_name_keeps_the_extension_and_counts_on() {
    assert_eq!(copy_name("report.txt", 1, false), "report copy.txt");
    assert_eq!(copy_name("report.txt", 2, false), "report copy 2.txt");
    assert_eq!(copy_name("Photos", 1, true), "Photos copy");
    assert_eq!(
        copy_name("v1.2", 1, true),
        "v1.2 copy",
        "a folder has no extension"
    );
    assert_eq!(
        copy_name(".env", 1, false),
        ".env copy",
        "a dotfile's dot is its name"
    );
    assert_eq!(copy_name("Makefile", 3, false), "Makefile copy 3");
    assert_eq!(
        copy_name("report copy.txt", 2, false),
        "report copy 2.txt",
        "copying a copy should not say copy twice"
    );
    assert_eq!(
        copy_name("report copy 2.txt", 3, false),
        "report copy 3.txt"
    );
    assert_eq!(copy_name("a copycat.txt", 1, false), "a copycat copy.txt");
}

#[test]
fn a_copy_name_never_exceeds_what_a_filesystem_accepts() {
    let long = format!("{}.jpg", "é".repeat(126));
    let name = copy_name(&long, 12, false);
    assert!(name.len() <= 255, "{} bytes", name.len());
    assert!(name.ends_with(" copy 12.jpg"), "{name}");
}

#[test]
fn pasting_a_copy_where_it_came_from_makes_name_copy_then_name_copy_2() {
    let ws = Workspace::new();
    ws.write("docs/report.txt", b"draft");
    let bus = ws.bus();

    let first = ok(
        &bus,
        "files.copy",
        json!({ "paths": ["/docs/report.txt"], "to": "/docs", "keep_both": true }),
    );
    assert_eq!(first["paths"], json!(["r620:/docs/report copy.txt"]));
    let second = ok(
        &bus,
        "files.copy",
        json!({ "paths": ["/docs/report.txt"], "to": "/docs", "keep_both": true }),
    );
    assert_eq!(second["paths"], json!(["r620:/docs/report copy 2.txt"]));

    assert_eq!(ws.read("docs/report.txt").as_deref(), Some(&b"draft"[..]));
    assert_eq!(
        ws.read("docs/report copy.txt").as_deref(),
        Some(&b"draft"[..])
    );
    assert_eq!(
        ws.read("docs/report copy 2.txt").as_deref(),
        Some(&b"draft"[..])
    );
}

#[test]
fn a_folder_copied_beside_itself_is_copied_whole() {
    let ws = Workspace::new();
    ws.write("Photos/a.jpg", b"A");
    ws.write("Photos/sub/b.jpg", b"B");
    let out = ok(
        &ws.bus(),
        "files.copy",
        json!({ "paths": ["/Photos"], "to": "/", "keep_both": true }),
    );
    assert_eq!(out["paths"], json!(["r620:/Photos copy"]));
    assert_eq!(ws.read("Photos copy/sub/b.jpg").as_deref(), Some(&b"B"[..]));
}

#[test]
fn without_keep_both_a_taken_name_is_still_refused_and_nothing_is_copied() {
    let ws = Workspace::new();
    ws.write("docs/report.txt", b"draft");
    let e = run(
        &ws.bus(),
        "files.copy",
        json!({ "paths": ["/docs/report.txt"], "to": "/docs" }),
    )
    .outcome
    .unwrap_err();
    assert!(e.to_string().contains("already exists"), "{e}");
    assert!(ws.read("docs/report copy.txt").is_none());
}

#[test]
fn two_sources_with_one_name_both_land_when_keeping_both() {
    let ws = Workspace::new();
    ws.write("a/notes.md", b"A");
    ws.write("b/notes.md", b"B");
    ws.write("into/.keep", b"");
    let out = ok(
        &ws.bus(),
        "files.copy",
        json!({ "paths": ["/a/notes.md", "/b/notes.md"], "to": "/into", "keep_both": true }),
    );
    assert_eq!(
        out["paths"],
        json!(["r620:/into/notes.md", "r620:/into/notes copy.md"])
    );
    assert_eq!(ws.read("into/notes.md").as_deref(), Some(&b"A"[..]));
    assert_eq!(ws.read("into/notes copy.md").as_deref(), Some(&b"B"[..]));
}

#[test]
fn a_move_never_renames_to_make_room() {
    // Keeping both is a copy's option only: a move that quietly renamed
    // would leave the operator looking for a file under a name they never
    // chose.
    let ws = Workspace::new();
    ws.write("a.txt", b"A");
    ws.write("into/a.txt", b"other");
    let e = run(
        &ws.bus(),
        "files.move",
        json!({ "paths": ["/a.txt"], "to": "/into", "keep_both": true }),
    )
    .outcome
    .unwrap_err();
    assert!(matches!(e, CommandError::BadParams { .. }), "{e}");
    assert_eq!(ws.read("a.txt").as_deref(), Some(&b"A"[..]));
}

#[test]
fn a_folder_with_a_dot_in_its_name_keeps_it_whole_when_copied_beside_itself() {
    // A folder's name has no extension: `v1.2 copy`, not `v1 copy.2`.
    let ws = Workspace::new();
    ws.write("v1.2/readme.txt", b"R");
    let out = ok(
        &ws.bus(),
        "files.copy",
        json!({ "paths": ["/v1.2"], "to": "/", "keep_both": true }),
    );
    assert_eq!(out["paths"], json!(["r620:/v1.2 copy"]));
    assert_eq!(ws.read("v1.2 copy/readme.txt").as_deref(), Some(&b"R"[..]));
}

/// A name of `len` bytes ending in `.txt`.
fn long_name(len: usize) -> String {
    format!("{}.txt", "n".repeat(len - 4))
}

#[test]
fn a_file_with_a_name_near_the_limit_can_be_copied() {
    // The copy grows under a hidden partial name before it is renamed into
    // place; that name must fit too, or no long-named file can be copied.
    let ws = Workspace::new();
    let name = long_name(250);
    ws.write(&name, b"L");
    ws.write("into/.keep", b"");
    let out = ok(
        &ws.bus(),
        "files.copy",
        json!({ "paths": [format!("/{name}")], "to": "/into" }),
    );
    assert_eq!(out["paths"], json!([format!("r620:/into/{name}")]));
    assert_eq!(ws.read(&format!("into/{name}")).as_deref(), Some(&b"L"[..]));
}

#[test]
fn keeping_both_of_a_name_near_the_limit_shortens_the_copy_rather_than_failing() {
    let ws = Workspace::new();
    let name = long_name(250);
    ws.write(&name, b"L");
    let out = ok(
        &ws.bus(),
        "files.copy",
        json!({ "paths": [format!("/{name}")], "to": "/", "keep_both": true }),
    );
    let landed = out["paths"][0].as_str().unwrap();
    let landed_name = landed.rsplit('/').next().unwrap();
    assert!(landed_name.len() <= 255, "{} bytes", landed_name.len());
    assert!(landed_name.ends_with(" copy.txt"), "{landed_name}");
    assert_eq!(ws.read(landed_name).as_deref(), Some(&b"L"[..]));
    assert_eq!(ws.read(&name).as_deref(), Some(&b"L"[..]));
}

// ---------------------------------------------------------------------------
// The cache directory is not trusted: by default it is inside the working
// directory, where Write-level commands can replace it
// ---------------------------------------------------------------------------

fn names_in(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .map(|entries| {
            entries
                .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    names
}

#[test]
fn eviction_never_removes_a_file_the_cache_did_not_write() {
    let ws = Workspace::new();
    let cache = ws.root.parent().unwrap().join("small-cache");
    ws.write("probe.jpg", &jpeg(600, 600));
    ws.thumbnailer(&cache, u64::MAX)
        .make(&addr("/probe.jpg"), None)
        .unwrap();
    let one = cache_files(&cache)[0].1;
    std::fs::remove_dir_all(&cache).unwrap();

    // Something the operator saved there, older than every thumbnail.
    std::fs::create_dir_all(&cache).unwrap();
    std::fs::write(cache.join("notes.txt"), vec![b'n'; (one * 4) as usize]).unwrap();
    let thumbnailer = ws.thumbnailer(&cache, one * 3 + one / 2);
    for i in 0..8 {
        ws.write(&format!("p{i}.jpg"), &jpeg(600, 600 - i));
        thumbnailer
            .make(&addr(&format!("/p{i}.jpg")), None)
            .unwrap();
    }
    assert!(
        cache.join("notes.txt").exists(),
        "eviction deleted a file the cache never wrote"
    );
}

#[test]
fn a_cache_directory_that_is_a_symlink_is_not_used() {
    let ws = Workspace::new();
    let base = ws.root.parent().unwrap();
    // Someone else's files, and a cache directory swapped for a link to them.
    let elsewhere = base.join("elsewhere");
    std::fs::create_dir_all(&elsewhere).unwrap();
    for i in 0..3 {
        std::fs::write(elsewhere.join(format!("keep-{i}.bin")), vec![0u8; 100_000]).unwrap();
    }
    std::fs::write(elsewhere.join("a".repeat(64)), vec![0u8; 100_000]).unwrap();
    let before = names_in(&elsewhere);
    let cache = base.join("small-cache");
    std::os::unix::fs::symlink(&elsewhere, &cache).unwrap();

    let thumbnailer = ws.thumbnailer(&cache, 1_000);
    for i in 0..3 {
        ws.write(&format!("p{i}.jpg"), &jpeg(300, 300 - i));
        let thumb = thumbnailer
            .make(&addr(&format!("/p{i}.jpg")), None)
            .unwrap();
        assert!(!thumb.cached);
    }
    assert_eq!(
        names_in(&elsewhere),
        before,
        "the cache wrote into or deleted from where its link pointed"
    );
}

#[test]
fn a_cache_whose_parent_is_a_symlink_is_not_used() {
    let ws = Workspace::new();
    let base = ws.root.parent().unwrap();
    let elsewhere = base.join("elsewhere");
    std::fs::create_dir_all(&elsewhere).unwrap();
    std::os::unix::fs::symlink(&elsewhere, base.join("linked")).unwrap();

    ws.write("a.jpg", &jpeg(300, 200));
    let thumb = ws
        .thumbnailer(
            &base.join("linked").join("files"),
            thumbnail::DEFAULT_CACHE_CAP,
        )
        .make(&addr("/a.jpg"), None)
        .unwrap();
    assert_eq!(thumb.width, 256);
    assert!(
        names_in(&elsewhere).is_empty(),
        "the cache was created through a link: {:?}",
        names_in(&elsewhere)
    );
}

#[test]
fn a_cache_entry_replaced_by_a_symlink_is_not_followed() {
    let ws = Workspace::new();
    let cache = ws.root.parent().unwrap().join("small-cache");
    let thumbnailer = ws.thumbnailer(&cache, thumbnail::DEFAULT_CACHE_CAP);
    ws.write("a.jpg", &jpeg(640, 480));
    thumbnailer.make(&addr("/a.jpg"), None).unwrap();
    let entries = cache_files(&cache);
    assert_eq!(entries.len(), 1);

    // A picture outside the working directory, linked in under the entry's
    // name: following it would hand out a file the boundary refuses.
    let outside = ws.root.parent().unwrap().join("outside.jpg");
    std::fs::write(&outside, jpeg(100, 50)).unwrap();
    std::fs::remove_file(&entries[0].0).unwrap();
    std::os::unix::fs::symlink(&outside, &entries[0].0).unwrap();

    let thumb = thumbnailer.make(&addr("/a.jpg"), None).unwrap();
    assert!(
        !thumb.cached,
        "the linked file was served as the cache entry"
    );
    assert_eq!((thumb.width, thumb.height), (256, 192));
}

#[test]
fn a_picture_needing_more_memory_than_the_decode_budget_is_refused() {
    let ws = Workspace::new();
    let cache = ws.root.parent().unwrap().join("cache-b");
    // 1000 x 1000 RGB decodes to 3 MB; 300 x 300 to 270 KB.
    ws.write("big.jpg", &jpeg(1000, 1000));
    ws.write("small.jpg", &jpeg(300, 300));
    let thumbnailer = ws
        .thumbnailer(&cache, thumbnail::DEFAULT_CACHE_CAP)
        .with_decode_budget(1024 * 1024);
    let e = thumbnailer.make(&addr("/big.jpg"), None).unwrap_err();
    assert!(matches!(e, ThumbnailError::TooManyPixels { .. }), "{e}");
    thumbnailer.make(&addr("/small.jpg"), None).unwrap();
}
