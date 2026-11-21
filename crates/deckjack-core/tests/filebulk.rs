//! Moving, copying and trashing several things at once, on a real disk.
//!
//! The single-item commands and the boundary are proved in `tests/files.rs`.
//! What is proved here is what a bulk operation owes on top: it overwrites
//! nothing, it is all or nothing, nothing appears before its bytes are there,
//! cancelling stops the disk, and the trash is a real undo.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};

use deckjack_core::command::{
    Admitted, Bus, CommandError, CommandId, Completion, Issuer, NodeId, OsEnv, Registry, Request,
    RiskLevel,
};
use deckjack_core::files::{FileError, Root, VirtualPath, register_commands};
use deckjack_core::transfer::{self, CopyProgress};
use deckjack_core::trash;

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

    fn file(&self, rel: &str, body: &str) {
        let path = self.host(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, body).unwrap();
    }

    fn dir(&self, rel: &str) {
        std::fs::create_dir_all(self.host(rel)).unwrap();
    }

    fn read(&self, rel: &str) -> Option<String> {
        std::fs::read_to_string(self.host(rel)).ok()
    }

    fn exists(&self, rel: &str) -> bool {
        std::fs::symlink_metadata(self.host(rel)).is_ok()
    }

    /// Every name in a folder, hidden ones included, sorted.
    fn names(&self, rel: &str) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(self.host(rel))
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    fn open(&self) -> Root {
        Root::open(&self.root).unwrap()
    }

    /// The install directory `register_commands` is given (its thumbnail
    /// cache goes under `<install>/cache/files`), beside the working
    /// directory and never inside it.
    fn install(&self) -> PathBuf {
        self.root.parent().unwrap().join("install")
    }

    fn bus(&self) -> Bus {
        let registry = Arc::new(Registry::new());
        register_commands(&registry, &self.root, "r620", &self.install()).unwrap();
        Bus::new(registry, NodeId::new("r620"), Arc::new(OsEnv))
    }
}

fn vp(text: &str) -> VirtualPath {
    VirtualPath::parse(text).unwrap()
}

fn vps(texts: &[&str]) -> Vec<VirtualPath> {
    texts.iter().map(|t| vp(t)).collect()
}

/// Progress that never cancels and remembers what it was told.
#[derive(Default)]
struct Watch {
    seen: Vec<(u64, u64)>,
    cancel_after: Option<usize>,
}

impl CopyProgress for Watch {
    fn bytes(&mut self, done: u64, total: u64) {
        self.seen.push((done, total));
    }
    fn cancelled(&self) -> bool {
        self.cancel_after.is_some_and(|n| self.seen.len() > n)
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

// ---------------------------------------------------------------------------
// Move
// ---------------------------------------------------------------------------

#[test]
fn several_things_move_into_a_folder_under_their_own_names() {
    let ws = Workspace::new();
    ws.file("a.txt", "A");
    ws.file("pics/b.jpg", "B");
    ws.dir("dest");

    let moved = transfer::move_into(&ws.open(), &vps(&["/a.txt", "/pics"]), &vp("/dest")).unwrap();

    assert_eq!(moved, vps(&["/dest/a.txt", "/dest/pics"]));
    assert_eq!(ws.read("dest/a.txt").as_deref(), Some("A"));
    assert_eq!(ws.read("dest/pics/b.jpg").as_deref(), Some("B"));
    assert!(!ws.exists("a.txt") && !ws.exists("pics"));
}

#[test]
fn a_move_that_would_overwrite_moves_nothing() {
    let ws = Workspace::new();
    ws.file("a.txt", "new");
    ws.file("b.txt", "B");
    ws.file("dest/b.txt", "the one already there");

    let refused = transfer::move_into(&ws.open(), &vps(&["/a.txt", "/b.txt"]), &vp("/dest"));

    assert!(
        matches!(refused, Err(FileError::Exists { .. })),
        "{refused:?}"
    );
    // Checked before anything moved: the first item is still where it was.
    assert_eq!(ws.read("a.txt").as_deref(), Some("new"));
    assert_eq!(
        ws.read("dest/b.txt").as_deref(),
        Some("the one already there")
    );
    assert!(!ws.exists("dest/a.txt"));
}

#[test]
fn a_folder_cannot_go_inside_itself() {
    let ws = Workspace::new();
    ws.dir("pics/2026");
    for into in ["/pics", "/pics/2026"] {
        let refused = transfer::move_into(&ws.open(), &vps(&["/pics"]), &vp(into));
        assert!(
            matches!(refused, Err(FileError::IntoItself { .. })),
            "{into}: {refused:?}"
        );
        let refused = transfer::copy_into(
            &ws.open(),
            &vps(&["/pics"]),
            &vp(into),
            &mut Watch::default(),
        );
        assert!(
            matches!(refused, Err(FileError::IntoItself { .. })),
            "{into}: {refused:?}"
        );
    }
    // A sibling whose name starts the same is not inside it.
    ws.dir("pics-old");
    transfer::move_into(&ws.open(), &vps(&["/pics"]), &vp("/pics-old")).unwrap();
}

#[test]
fn the_same_thing_named_twice_is_refused_rather_than_half_done() {
    let ws = Workspace::new();
    ws.file("a.txt", "A");
    ws.dir("dest");
    let refused = transfer::move_into(&ws.open(), &vps(&["/a.txt", "/a.txt"]), &vp("/dest"));
    assert!(
        matches!(refused, Err(FileError::Duplicate { .. })),
        "{refused:?}"
    );
    assert!(ws.exists("a.txt"));
}

#[test]
fn nothing_moves_into_or_out_of_hidden_state() {
    let ws = Workspace::new();
    ws.file("a.txt", "A");
    ws.file(".deckjack/state.db", "secret");
    let root = ws.open();
    assert!(transfer::move_into(&root, &vps(&["/a.txt"]), &vp("/.deckjack")).is_err());
    assert!(
        transfer::copy_into(
            &root,
            &vps(&["/.deckjack/state.db"]),
            &vp("/"),
            &mut Watch::default()
        )
        .is_err()
    );
    assert!(ws.exists("a.txt") && ws.exists(".deckjack/state.db"));
}

// ---------------------------------------------------------------------------
// Copy
// ---------------------------------------------------------------------------

#[test]
fn a_copy_reproduces_a_tree_and_leaves_the_original() {
    let ws = Workspace::new();
    ws.file("pics/a.jpg", "AAAA");
    ws.file("pics/deep/b.jpg", "BB");
    std::os::unix::fs::symlink("a.jpg", ws.host("pics/latest")).unwrap();
    ws.dir("backup");

    let mut watch = Watch::default();
    let copied =
        transfer::copy_into(&ws.open(), &vps(&["/pics"]), &vp("/backup"), &mut watch).unwrap();

    assert_eq!(copied, vps(&["/backup/pics"]));
    assert_eq!(ws.read("backup/pics/a.jpg").as_deref(), Some("AAAA"));
    assert_eq!(ws.read("backup/pics/deep/b.jpg").as_deref(), Some("BB"));
    // A shortcut is copied as a shortcut, with its text as written.
    assert_eq!(
        std::fs::read_link(ws.host("backup/pics/latest")).unwrap(),
        Path::new("a.jpg")
    );
    assert_eq!(ws.read("pics/a.jpg").as_deref(), Some("AAAA"));
    // Progress starts at zero of the whole and ends at the whole.
    assert_eq!(watch.seen.first(), Some(&(0, 6)));
    assert_eq!(watch.seen.last(), Some(&(6, 6)));
    // And no partial name is left lying around.
    assert_eq!(ws.names("backup"), vec!["pics".to_string()]);
}

#[test]
fn a_copy_keeps_the_permission_bits() {
    use std::os::unix::fs::PermissionsExt as _;
    let ws = Workspace::new();
    ws.file("run.sh", "#!/bin/sh\n");
    std::fs::set_permissions(ws.host("run.sh"), std::fs::Permissions::from_mode(0o750)).unwrap();
    ws.dir("dest");
    transfer::copy_into(
        &ws.open(),
        &vps(&["/run.sh"]),
        &vp("/dest"),
        &mut Watch::default(),
    )
    .unwrap();
    let mode = std::fs::metadata(ws.host("dest/run.sh"))
        .unwrap()
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o750, "a copied script lost its executable bit");
}

#[test]
fn a_cancelled_copy_leaves_nothing_behind() {
    let ws = Workspace::new();
    // Two items, and a cancel during the second: the first, already complete,
    // has to go too, or the operator is left with half a selection.
    ws.file("first.txt", "done before the cancel");
    std::fs::write(ws.host("big.bin"), vec![7u8; 3 << 20]).unwrap();
    ws.dir("dest");

    let mut watch = Watch {
        cancel_after: Some(2),
        ..Watch::default()
    };
    let result = transfer::copy_into(
        &ws.open(),
        &vps(&["/first.txt", "/big.bin"]),
        &vp("/dest"),
        &mut watch,
    );

    assert!(matches!(result, Err(FileError::Cancelled)), "{result:?}");
    assert_eq!(
        ws.names("dest"),
        Vec::<String>::new(),
        "a cancelled copy left files"
    );
    assert!(
        ws.exists("first.txt") && ws.exists("big.bin"),
        "the originals were touched"
    );
}

#[test]
fn a_copy_that_would_overwrite_copies_nothing() {
    let ws = Workspace::new();
    ws.file("a.txt", "A");
    ws.file("b.txt", "B");
    ws.file("dest/b.txt", "keep me");
    let refused = transfer::copy_into(
        &ws.open(),
        &vps(&["/a.txt", "/b.txt"]),
        &vp("/dest"),
        &mut Watch::default(),
    );
    assert!(
        matches!(refused, Err(FileError::Exists { .. })),
        "{refused:?}"
    );
    assert_eq!(ws.names("dest"), vec!["b.txt".to_string()]);
    assert_eq!(ws.read("dest/b.txt").as_deref(), Some("keep me"));
}

#[test]
fn files_copy_reports_progress_and_dropping_it_stops_the_disk() {
    // The command, through the bus: a dropped dispatch is how cancelling
    // reaches a handler, and the copy runs on a blocking thread a dropped
    // future does not stop by itself.
    let ws = Workspace::new();
    std::fs::write(ws.host("big.bin"), vec![1u8; 256 << 20]).unwrap();
    ws.dir("dest");
    let bus = ws.bus();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let request = Request::new(
        CommandId::new("files.copy").unwrap(),
        json!({ "paths": ["/big.bin"], "to": "/dest" }),
        Issuer::operator(),
    );
    let Admitted::Running(running) = bus.dispatch(request).unwrap() else {
        panic!("files.copy did not run");
    };
    let (mut progress, finish) = running.split();
    let task = runtime.spawn(finish);
    runtime.block_on(async {
        // Wait for the copy to be under way, then cancel it.
        let first = tokio::time::timeout(Duration::from_secs(10), progress.recv())
            .await
            .expect("no progress within ten seconds");
        assert!(first.is_some(), "the copy reported no progress");
        task.abort();
        let _ = task.await;
    });
    // The blocking thread notices between chunks, removes its partial file
    // and returns. Give it a bounded while; a copy that kept going would take
    // far longer than this to finish 256 MiB on most disks, and would leave
    // a partial file behind meanwhile.
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while !ws.names("dest").is_empty() {
        assert!(
            std::time::Instant::now() < deadline,
            "the cancelled copy is still writing: {:?}",
            ws.names("dest")
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(!ws.exists("dest/big.bin"));
}

// ---------------------------------------------------------------------------
// Trash
// ---------------------------------------------------------------------------

#[test]
fn trash_is_an_undo_that_puts_things_back_where_they_were() {
    let ws = Workspace::new();
    ws.file("notes/today.md", "hello");
    ws.file("pics/a.jpg", "A");
    let root = ws.open();

    let trashed = trash::trash(&root, &vps(&["/notes/today.md", "/pics"]), 1_700_000_000).unwrap();
    assert!(!ws.exists("notes/today.md") && !ws.exists("pics"));

    let listed = trash::list(&root).unwrap();
    let mut from: Vec<String> = listed.iter().map(|e| e.from.to_string()).collect();
    from.sort();
    assert_eq!(from, vec!["/notes/today.md", "/pics"]);

    // The folder it came from is gone too: restoring makes it again.
    std::fs::remove_dir(ws.host("notes")).unwrap();
    let ids: Vec<String> = trashed.iter().map(|t| t.id.clone()).collect();
    let back = trash::restore(&root, &ids).unwrap();
    assert_eq!(back.len(), 2);
    assert_eq!(ws.read("notes/today.md").as_deref(), Some("hello"));
    assert_eq!(ws.read("pics/a.jpg").as_deref(), Some("A"));
    assert!(trash::list(&root).unwrap().is_empty());
}

#[test]
fn the_trash_is_in_the_standard_layout_inside_the_working_directory() {
    let ws = Workspace::new();
    ws.file("my notes/a b.md", "x");
    let trashed = trash::trash(&ws.open(), &vps(&["/my notes/a b.md"]), 1_700_000_000).unwrap();
    let id = &trashed[0].id;
    assert!(ws.exists(&format!(".Trash/files/{id}")));
    let info = ws.read(&format!(".Trash/info/{id}.trashinfo")).unwrap();
    assert!(info.starts_with("[Trash Info]\n"), "{info}");
    // Relative to the trash's own directory and percent-encoded, per the
    // freedesktop spec, so another tool reading it finds the same place.
    assert!(info.contains("\nPath=my%20notes/a%20b.md\n"), "{info}");
    assert!(info.contains("\nDeletionDate="), "{info}");
}

#[test]
fn two_things_with_one_name_both_fit_in_the_trash() {
    let ws = Workspace::new();
    ws.file("a/x.txt", "one");
    ws.file("b/x.txt", "two");
    let root = ws.open();
    let first = trash::trash(&root, &vps(&["/a/x.txt"]), 1).unwrap();
    let second = trash::trash(&root, &vps(&["/b/x.txt"]), 2).unwrap();
    assert_ne!(first[0].id, second[0].id);
    trash::restore(&root, &[second[0].id.clone()]).unwrap();
    assert_eq!(ws.read("b/x.txt").as_deref(), Some("two"));
}

#[test]
fn restoring_never_replaces_something_that_took_the_name() {
    let ws = Workspace::new();
    ws.file("a.txt", "old");
    let root = ws.open();
    let trashed = trash::trash(&root, &vps(&["/a.txt"]), 1).unwrap();
    ws.file("a.txt", "new one");
    let refused = trash::restore(&root, &[trashed[0].id.clone()]);
    assert!(
        matches!(refused, Err(FileError::Exists { .. })),
        "{refused:?}"
    );
    assert_eq!(ws.read("a.txt").as_deref(), Some("new one"));
    assert_eq!(
        trash::list(&root).unwrap().len(),
        1,
        "the trashed copy was lost"
    );
}

#[test]
fn a_trash_id_cannot_reach_outside_the_trash() {
    let ws = Workspace::new();
    ws.file("a.txt", "A");
    let root = ws.open();
    for id in ["../../a.txt", "/a.txt", "..", ""] {
        assert!(trash::restore(&root, &[id.to_string()]).is_err(), "{id:?}");
        assert!(
            trash::empty(&root, Some(&[id.to_string()])).is_err(),
            "{id:?}"
        );
    }
    assert!(ws.exists("a.txt"));
}

#[test]
fn a_hand_edited_info_file_cannot_restore_outside_the_workspace() {
    let ws = Workspace::new();
    ws.file("a.txt", "A");
    let root = ws.open();
    let id = trash::trash(&root, &vps(&["/a.txt"]), 1).unwrap()[0]
        .id
        .clone();
    for path in ["../outside.txt", ".deckjack/state.db", "/etc/passwd"] {
        std::fs::write(
            ws.host(&format!(".Trash/info/{id}.trashinfo")),
            format!("[Trash Info]\nPath={path}\nDeletionDate=2026-01-01T00:00:00\n"),
        )
        .unwrap();
        let result = trash::restore(&root, std::slice::from_ref(&id));
        // `/etc/passwd` reads as the workspace path `/etc/passwd`, which is
        // inside; the other two are refused outright.
        if path == "/etc/passwd" {
            let back = result.unwrap();
            assert_eq!(back, vps(&["/etc/passwd"]));
            assert_eq!(ws.read("etc/passwd").as_deref(), Some("A"));
            return;
        }
        assert!(result.is_err(), "{path} was restored");
        // Nor is it offered: the listing would otherwise advertise a restore
        // that can only fail.
        assert!(
            trash::list(&root).unwrap().is_empty(),
            "the trash lists an entry claiming to come from {path}"
        );
    }
}

#[test]
fn trashing_the_trash_is_refused() {
    let ws = Workspace::new();
    ws.file("a.txt", "A");
    let root = ws.open();
    let id = trash::trash(&root, &vps(&["/a.txt"]), 1).unwrap()[0]
        .id
        .clone();
    let refused = trash::trash(&root, &vps(&[&format!("/.Trash/files/{id}")]), 2);
    assert!(
        matches!(refused, Err(FileError::AlreadyInTrash { .. })),
        "{refused:?}"
    );
}

#[test]
fn a_batch_that_fails_partway_puts_the_first_ones_back() {
    let ws = Workspace::new();
    ws.file("a.txt", "A");
    let root = ws.open();
    // The second path does not exist: refused before anything moves.
    let refused = trash::trash(&root, &vps(&["/a.txt", "/missing.txt"]), 1);
    assert!(refused.is_err());
    assert_eq!(ws.read("a.txt").as_deref(), Some("A"));
    assert!(trash::list(&root).unwrap().is_empty());
}

#[test]
fn emptying_deletes_for_good_and_only_what_was_named() {
    let ws = Workspace::new();
    ws.file("a.txt", "A");
    ws.file("b.txt", "B");
    let root = ws.open();
    let trashed = trash::trash(&root, &vps(&["/a.txt", "/b.txt"]), 1).unwrap();
    assert_eq!(
        trash::empty(&root, Some(&[trashed[0].id.clone()])).unwrap(),
        1
    );
    let left: Vec<String> = trash::list(&root)
        .unwrap()
        .into_iter()
        .map(|e| e.id)
        .collect();
    assert_eq!(left, vec![trashed[1].id.clone()]);
    assert_eq!(trash::empty(&root, None).unwrap(), 1);
    assert!(trash::list(&root).unwrap().is_empty());
    assert_eq!(ws.names(".Trash/files"), Vec::<String>::new());
    assert_eq!(ws.names(".Trash/info"), Vec::<String>::new());
}

// ---------------------------------------------------------------------------
// Through the bus
// ---------------------------------------------------------------------------

#[test]
fn the_commands_are_on_the_bus_with_the_risk_each_one_carries() {
    let ws = Workspace::new();
    let bus = ws.bus();
    for (id, risk) in [
        ("files.move", RiskLevel::Write),
        ("files.copy", RiskLevel::Write),
        ("files.trash", RiskLevel::Write),
        ("files.trash.list", RiskLevel::ReadOnly),
        ("files.trash.restore", RiskLevel::Write),
        // The one that cannot be undone from inside Deckjack.
        ("files.trash.empty", RiskLevel::Destructive),
    ] {
        let spec = bus
            .registry()
            .spec(&CommandId::new(id).unwrap())
            .unwrap_or_else(|| panic!("{id} is not registered"));
        assert_eq!(spec.risk(), risk, "{id}");
    }
}

#[test]
fn trash_and_restore_round_trip_through_the_bus() {
    let ws = Workspace::new();
    ws.file("a.txt", "A");
    let bus = ws.bus();
    let trashed = run(&bus, "files.trash", json!({ "paths": ["/a.txt"] }))
        .outcome
        .unwrap();
    let id = trashed["trashed"][0]["id"].as_str().unwrap().to_string();
    assert!(!ws.exists("a.txt"));
    let listed = run(&bus, "files.trash.list", json!({})).outcome.unwrap();
    assert_eq!(listed["entries"][0]["from"], "/a.txt");
    run(&bus, "files.trash.restore", json!({ "ids": [id] }))
        .outcome
        .unwrap();
    assert_eq!(ws.read("a.txt").as_deref(), Some("A"));
}

#[test]
fn an_empty_list_of_paths_is_a_parameter_error() {
    let ws = Workspace::new();
    let bus = ws.bus();
    for command in ["files.move", "files.copy"] {
        let e = run(&bus, command, json!({ "paths": [], "to": "/" }))
            .outcome
            .unwrap_err();
        assert!(
            matches!(e, CommandError::BadParams { .. }),
            "{command}: {e}"
        );
    }
    let e = run(&bus, "files.trash", json!({ "paths": [] }))
        .outcome
        .unwrap_err();
    assert!(matches!(e, CommandError::BadParams { .. }), "{e}");
}

// ---------------------------------------------------------------------------
// Failures partway through, and what a half-done copy looks like
// ---------------------------------------------------------------------------

/// Make `rel` read-only so renaming anything out of it fails with EACCES, and
/// put it back when dropped so the temp dir can be cleaned up.
struct ReadOnly(PathBuf);

impl ReadOnly {
    fn new(ws: &Workspace, rel: &str) -> ReadOnly {
        use std::os::unix::fs::PermissionsExt as _;
        let path = ws.host(rel);
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o555)).unwrap();
        ReadOnly(path)
    }
}

impl Drop for ReadOnly {
    fn drop(&mut self) {
        use std::os::unix::fs::PermissionsExt as _;
        let _ = std::fs::set_permissions(&self.0, std::fs::Permissions::from_mode(0o755));
    }
}

#[test]
fn a_move_that_fails_partway_puts_back_what_already_moved() {
    // Root ignores directory permissions, which would make this vacuous.
    assert_ne!(
        unsafe { libc::geteuid() },
        0,
        "run the tests as a normal user"
    );
    let ws = Workspace::new();
    ws.file("a.txt", "A");
    ws.file("locked/b.txt", "B");
    ws.dir("dest");
    let _lock = ReadOnly::new(&ws, "locked");

    let refused = transfer::move_into(&ws.open(), &vps(&["/a.txt", "/locked/b.txt"]), &vp("/dest"));

    assert!(
        matches!(refused, Err(FileError::Denied { .. })),
        "{refused:?}"
    );
    assert_eq!(
        ws.read("a.txt").as_deref(),
        Some("A"),
        "the first item was not put back"
    );
    assert_eq!(ws.names("dest"), Vec::<String>::new());
}

#[test]
fn a_trash_that_fails_partway_puts_back_what_already_went() {
    assert_ne!(
        unsafe { libc::geteuid() },
        0,
        "run the tests as a normal user"
    );
    let ws = Workspace::new();
    ws.file("a.txt", "A");
    ws.file("locked/b.txt", "B");
    let root = ws.open();
    let _lock = ReadOnly::new(&ws, "locked");

    let refused = trash::trash(&root, &vps(&["/a.txt", "/locked/b.txt"]), 1);

    assert!(refused.is_err(), "{refused:?}");
    assert_eq!(
        ws.read("a.txt").as_deref(),
        Some("A"),
        "the first item stayed in the trash"
    );
    assert!(
        trash::list(&root).unwrap().is_empty(),
        "a trash entry was left behind"
    );
    assert_eq!(ws.names(".Trash/info"), Vec::<String>::new());
}

/// Progress that looks at the destination while the copy is running.
struct Peek<'a> {
    ws: &'a Workspace,
    rel: &'static str,
    seen_midway: bool,
}

impl CopyProgress for Peek<'_> {
    fn bytes(&mut self, done: u64, total: u64) {
        if done > 0 && done < total {
            self.seen_midway = true;
            assert!(
                !self.ws.exists(self.rel),
                "{} is visible under its real name at {done} of {total} bytes",
                self.rel
            );
        }
    }
    fn cancelled(&self) -> bool {
        false
    }
}

#[test]
fn a_copy_is_not_under_its_real_name_until_every_byte_is_there() {
    let ws = Workspace::new();
    std::fs::write(ws.host("big.bin"), vec![3u8; 3 << 20]).unwrap();
    ws.dir("dest");
    let mut peek = Peek {
        ws: &ws,
        rel: "dest/big.bin",
        seen_midway: false,
    };
    transfer::copy_into(&ws.open(), &vps(&["/big.bin"]), &vp("/dest"), &mut peek).unwrap();
    assert!(
        peek.seen_midway,
        "the copy reported no progress midway, so this proved nothing"
    );
    assert_eq!(
        std::fs::read(ws.host("dest/big.bin")).unwrap().len(),
        3 << 20
    );
}
