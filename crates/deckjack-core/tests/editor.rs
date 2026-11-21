//! The editor's commands, driven through the bus the way the editor's UI
//! drives them.
//!
//! Every bus here is built by `files::register_commands` and nothing else, so
//! each test is also a construction test: if the editor's commands stopped
//! being registered with the file commands, the editor would open nothing,
//! and every test below would fail on its first dispatch.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};

use deckjack_core::command::{
    Admitted, Bus, CommandError, CommandId, Completion, Decision, Issuer, Journal, JournalError,
    LogEntry, NodeId, OsEnv, Registry, Request, RiskLevel,
};
use deckjack_core::editor::{
    EDIT_LIMIT, FIND_QUERY_MAX, SAVE_FRAME_LIMIT, TAIL_WINDOW, WHOLE_LIMIT,
};
use deckjack_core::files::{HIDDEN_DIR, register_commands};

struct Workspace {
    _dir: tempfile::TempDir,
    root: PathBuf,
}

impl Workspace {
    fn new() -> Workspace {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("work");
        std::fs::create_dir(&root).unwrap();
        std::fs::create_dir(root.join(HIDDEN_DIR)).unwrap();
        std::fs::write(root.join(HIDDEN_DIR).join("state.db"), b"hunter2").unwrap();
        std::fs::write(dir.path().join("outside.txt"), b"outside").unwrap();
        Workspace { _dir: dir, root }
    }

    fn host(&self, rel: &str) -> PathBuf {
        self.root.join(rel)
    }

    fn file(&self, rel: &str, body: impl AsRef<[u8]>) {
        let path = self.host(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, body).unwrap();
    }

    fn read(&self, rel: &str) -> String {
        std::fs::read_to_string(self.host(rel)).unwrap()
    }

    fn registry(&self) -> Arc<Registry> {
        let registry = Arc::new(Registry::new());
        register_commands(
            &registry,
            &self.root,
            "r620",
            &self.root.parent().unwrap().join("install"),
        )
        .unwrap();
        registry
    }

    fn bus(&self) -> Bus {
        Bus::new(self.registry(), NodeId::new("r620"), Arc::new(OsEnv))
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
    match run(bus, command, params).outcome {
        Ok(value) => value,
        Err(e) => panic!("{command} failed: {e}"),
    }
}

fn err(bus: &Bus, command: &str, params: Value) -> CommandError {
    match run(bus, command, params).outcome {
        Ok(value) => panic!("{command} unexpectedly succeeded: {value}"),
        Err(e) => e,
    }
}

// ---------------------------------------------------------------------------
// Opening
// ---------------------------------------------------------------------------

#[test]
fn a_small_text_file_opens_for_editing_with_a_version_to_save_against() {
    let ws = Workspace::new();
    ws.file("src/main.rs", "fn main() {}\n");
    let bus = ws.bus();

    let opened = ok(&bus, "editor.open", json!({ "path": "/src/main.rs" }));
    assert_eq!(opened["text"], "fn main() {}\n");
    assert_eq!(opened["mode"], "edit");
    assert_eq!(opened["path"], "r620:/src/main.rs");
    assert!(opened["reason"].is_null());
    assert!(opened["window"].is_null());
    assert!(opened["version"].as_str().is_some_and(|v| !v.is_empty()));
    assert_eq!(opened["save_limit"], SAVE_FRAME_LIMIT);
    assert!(opened["modified"].as_u64().is_some());
}

#[test]
fn a_file_too_large_to_save_back_opens_whole_and_read_only_and_says_why() {
    let ws = Workspace::new();
    let body = "x".repeat(EDIT_LIMIT as usize + 10);
    ws.file("big.txt", &body);

    let opened = ok(&ws.bus(), "editor.open", json!({ "path": "/big.txt" }));
    assert_eq!(opened["mode"], "read_only");
    assert_eq!(opened["text"].as_str().unwrap().len(), body.len());
    let reason = opened["reason"].as_str().unwrap();
    assert!(reason.contains("read-only"), "{reason}");
    assert!(opened["window"].is_null());
}

#[test]
fn a_huge_file_opens_as_its_tail_starting_on_a_line() {
    let ws = Workspace::new();
    // Numbered lines, so the window's position in the file is checkable.
    let mut body = String::new();
    let mut n = 0u64;
    while (body.len() as u64) <= WHOLE_LIMIT + TAIL_WINDOW {
        body.push_str(&format!("line {n:08}\n"));
        n += 1;
    }
    ws.file("app.log", &body);
    let size = body.len() as u64;

    let opened = ok(&ws.bus(), "editor.open", json!({ "path": "/app.log" }));
    assert_eq!(opened["mode"], "read_only");
    assert!(opened["version"].is_null(), "a window can never be saved");
    let text = opened["text"].as_str().unwrap();
    let start = opened["window"]["start"].as_u64().unwrap();
    let end = opened["window"]["end"].as_u64().unwrap();
    assert_eq!(end, size);
    assert!(end - start <= TAIL_WINDOW);
    assert!(text.ends_with(&format!("line {:08}\n", n - 1)));
    assert!(text.starts_with("line "), "{}", &text[..20]);
    assert_eq!(&body[start as usize..end as usize], text);
    assert!(
        opened["reason"]
            .as_str()
            .unwrap()
            .contains("too large to open whole")
    );

    // Paging back: the window ending where this one starts, contiguous.
    let earlier = ok(
        &ws.bus(),
        "editor.open",
        json!({ "path": "/app.log", "before": start }),
    );
    let e_start = earlier["window"]["start"].as_u64().unwrap();
    assert_eq!(earlier["window"]["end"].as_u64().unwrap(), start);
    assert_eq!(
        earlier["text"].as_str().unwrap(),
        &body[e_start as usize..start as usize]
    );
}

#[test]
fn a_file_that_is_not_text_is_refused_with_somewhere_else_to_go() {
    let ws = Workspace::new();
    ws.file("photo.jpg", [0xFFu8, 0xD8, 0xFF, 0x00, 0xC3]);
    let e = err(&ws.bus(), "editor.open", json!({ "path": "/photo.jpg" }));
    let message = e.to_string();
    assert!(message.contains("not text"), "{message}");
    assert!(message.contains("Quick Look"), "{message}");
}

#[test]
fn a_huge_file_that_is_not_text_is_refused_too_not_shown_as_replacement_characters() {
    let ws = Workspace::new();
    // A video or an archive: past the size that opens as a tail, and bytes
    // all the way through. A small xorshift so the bytes look like media.
    let mut state = 0x9E37_79B9_7F4A_7C15u64;
    let body: Vec<u8> = (0..WHOLE_LIMIT + 500_000)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            (state >> 24) as u8
        })
        .collect();
    ws.file("clip.mp4", &body);
    let e = err(&ws.bus(), "editor.open", json!({ "path": "/clip.mp4" }));
    let message = e.to_string();
    assert!(message.contains("not text"), "{message}");
    assert!(message.contains("Quick Look"), "{message}");

    // Paging back into it is refused the same way.
    let e = err(
        &ws.bus(),
        "editor.open",
        json!({ "path": "/clip.mp4", "before": WHOLE_LIMIT }),
    );
    assert!(e.to_string().contains("not text"), "{e}");

    // A disk image's empty stretch: NUL bytes are valid UTF-8, and still not
    // text anyone means to read.
    ws.file("disk.img", vec![0u8; WHOLE_LIMIT as usize + 1]);
    let e = err(&ws.bus(), "editor.open", json!({ "path": "/disk.img" }));
    assert!(e.to_string().contains("not text"), "{e}");
}

#[test]
fn a_huge_log_with_a_stray_bad_byte_still_opens_as_its_tail() {
    // Logs pick up the odd Latin-1 byte; one of those is not a binary file.
    let ws = Workspace::new();
    let mut body = Vec::new();
    while (body.len() as u64) <= WHOLE_LIMIT + TAIL_WINDOW {
        body.extend_from_slice(b"GET /index.html 200 caf\xe9 ok\n");
    }
    ws.file("access.log", &body);
    let opened = ok(&ws.bus(), "editor.open", json!({ "path": "/access.log" }));
    assert_eq!(opened["mode"], "read_only");
    assert!(
        opened["text"]
            .as_str()
            .unwrap()
            .contains("GET /index.html 200")
    );
}

#[test]
fn a_file_read_only_on_disk_opens_read_only() {
    use std::os::unix::fs::PermissionsExt as _;
    let ws = Workspace::new();
    ws.file("locked.txt", "fixed");
    std::fs::set_permissions(
        ws.host("locked.txt"),
        std::fs::Permissions::from_mode(0o444),
    )
    .unwrap();
    let opened = ok(&ws.bus(), "editor.open", json!({ "path": "/locked.txt" }));
    assert_eq!(opened["mode"], "read_only");
    assert!(
        opened["reason"]
            .as_str()
            .unwrap()
            .contains("read-only on disk")
    );
}

#[test]
fn the_editor_cannot_open_or_save_outside_the_workspace_or_into_hidden_state() {
    let ws = Workspace::new();
    let bus = ws.bus();
    for path in ["/.deckjack/state.db", "/../outside.txt"] {
        err(&bus, "editor.open", json!({ "path": path }));
        err(&bus, "editor.find", json!({ "path": path, "query": "h" }));
        err(
            &bus,
            "editor.save",
            json!({ "path": path, "content": "pwned", "force": true }),
        );
    }
    assert_eq!(
        std::fs::read(ws.host(".deckjack/state.db")).unwrap(),
        b"hunter2"
    );
}

// ---------------------------------------------------------------------------
// Saving
// ---------------------------------------------------------------------------

#[test]
fn a_save_against_the_version_opened_writes_and_hands_back_the_next_version() {
    let ws = Workspace::new();
    ws.file("a.txt", "one");
    let bus = ws.bus();
    let opened = ok(&bus, "editor.open", json!({ "path": "/a.txt" }));

    let saved = ok(
        &bus,
        "editor.save",
        json!({ "path": "/a.txt", "content": "two", "expected": opened["version"] }),
    );
    assert_eq!(saved["status"], "saved");
    assert_eq!(ws.read("a.txt"), "two");
    assert_ne!(saved["version"], opened["version"]);

    // The version handed back is the one a second save must name: the
    // editor keeps saving without reopening.
    let again = ok(
        &bus,
        "editor.save",
        json!({ "path": "/a.txt", "content": "three", "expected": saved["version"] }),
    );
    assert_eq!(again["status"], "saved");
    assert_eq!(ws.read("a.txt"), "three");
    let reopened = ok(&bus, "editor.open", json!({ "path": "/a.txt" }));
    assert_eq!(reopened["version"], again["version"]);
}

#[test]
fn a_save_over_a_change_made_on_disk_is_a_conflict_and_writes_nothing() {
    let ws = Workspace::new();
    ws.file("a.txt", "mine");
    let bus = ws.bus();
    let opened = ok(&bus, "editor.open", json!({ "path": "/a.txt" }));

    // Someone else's write, the same length and within the same second: a
    // modified time and a size would both miss it.
    ws.file("a.txt", "them");

    let saved = ok(
        &bus,
        "editor.save",
        json!({ "path": "/a.txt", "content": "edit", "expected": opened["version"] }),
    );
    assert_eq!(saved["status"], "conflict");
    assert!(
        saved["message"]
            .as_str()
            .unwrap()
            .contains("changed on disk")
    );
    assert_eq!(ws.read("a.txt"), "them", "the other write was clobbered");
    let theirs = ok(&bus, "editor.open", json!({ "path": "/a.txt" }));
    assert_eq!(saved["version"], theirs["version"]);
}

#[test]
fn saving_anyway_replaces_what_is_on_disk() {
    let ws = Workspace::new();
    ws.file("a.txt", "mine");
    let bus = ws.bus();
    let opened = ok(&bus, "editor.open", json!({ "path": "/a.txt" }));
    ws.file("a.txt", "them");
    let saved = ok(
        &bus,
        "editor.save",
        json!({ "path": "/a.txt", "content": "edit", "expected": opened["version"], "force": true }),
    );
    assert_eq!(saved["status"], "saved");
    assert_eq!(ws.read("a.txt"), "edit");
}

#[test]
fn a_file_deleted_since_it_was_opened_is_a_conflict_not_a_silent_recreate() {
    let ws = Workspace::new();
    ws.file("a.txt", "mine");
    let bus = ws.bus();
    let opened = ok(&bus, "editor.open", json!({ "path": "/a.txt" }));
    std::fs::remove_file(ws.host("a.txt")).unwrap();

    let saved = ok(
        &bus,
        "editor.save",
        json!({ "path": "/a.txt", "content": "edit", "expected": opened["version"] }),
    );
    assert_eq!(saved["status"], "conflict");
    assert!(saved["version"].is_null());
    assert!(!ws.host("a.txt").exists());
}

#[test]
fn a_new_file_is_saved_only_where_nothing_is() {
    let ws = Workspace::new();
    ws.file("taken.txt", "keep me");
    let bus = ws.bus();

    let saved = ok(
        &bus,
        "editor.save",
        json!({ "path": "/fresh.txt", "content": "new" }),
    );
    assert_eq!(saved["status"], "saved");
    assert_eq!(ws.read("fresh.txt"), "new");

    let refused = ok(
        &bus,
        "editor.save",
        json!({ "path": "/taken.txt", "content": "overwrite" }),
    );
    assert_eq!(refused["status"], "conflict");
    assert!(
        refused["message"]
            .as_str()
            .unwrap()
            .contains("already exists")
    );
    assert_eq!(ws.read("taken.txt"), "keep me");
}

#[test]
fn a_saved_files_contents_never_reach_the_audit_log() {
    #[derive(Default)]
    struct Captured(Mutex<Vec<LogEntry>>);
    impl Journal for Captured {
        fn record(&self, entry: LogEntry) -> Result<(), JournalError> {
            self.0.lock().unwrap().push(entry);
            Ok(())
        }
    }
    let ws = Workspace::new();
    let journal = Arc::new(Captured::default());
    let bus = Bus::new(ws.registry(), NodeId::new("r620"), Arc::new(OsEnv))
        .with_journal(Arc::clone(&journal) as Arc<dyn Journal>);

    let secret = "AKIAIOSFODNN7EXAMPLE-not-a-real-key";
    ok(
        &bus,
        "editor.save",
        json!({ "path": "/creds.txt", "content": secret }),
    );
    let entries = journal.0.lock().unwrap();
    assert!(entries.iter().any(|e| e.decision == Decision::Ran));
    for entry in entries.iter() {
        let rendered = format!("{:?} {:?}", entry.params, entry.detail);
        assert!(!rendered.contains(secret), "{rendered}");
    }
}

#[test]
fn reverting_reads_what_is_on_disk_now() {
    let ws = Workspace::new();
    ws.file("a.txt", "before");
    let bus = ws.bus();
    ok(&bus, "editor.open", json!({ "path": "/a.txt" }));
    ws.file("a.txt", "after");
    let reverted = ok(&bus, "editor.revert", json!({ "path": "/a.txt" }));
    assert_eq!(reverted["text"], "after");
    assert_eq!(reverted["mode"], "edit");
}

// ---------------------------------------------------------------------------
// Finding
// ---------------------------------------------------------------------------

#[test]
fn find_returns_lines_and_columns_in_a_file() {
    let ws = Workspace::new();
    ws.file(
        "notes.md",
        "# Title\nsome TODO here\nnothing\n  todo: later\n",
    );
    let bus = ws.bus();
    let found = ok(
        &bus,
        "editor.find",
        json!({ "path": "/notes.md", "query": "todo" }),
    );
    let at: Vec<(u64, u64)> = found["matches"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| (m["line"].as_u64().unwrap(), m["column"].as_u64().unwrap()))
        .collect();
    assert_eq!(at, vec![(2, 6), (4, 3)]);
    assert_eq!(found["truncated"], false);

    let exact = ok(
        &bus,
        "editor.find",
        json!({ "path": "/notes.md", "query": "todo", "case_sensitive": true }),
    );
    assert_eq!(exact["matches"].as_array().unwrap().len(), 1);
}

#[test]
fn find_refuses_a_query_longer_than_its_cap_and_takes_one_at_it() {
    let ws = Workspace::new();
    ws.file("notes.md", "short\n");
    let bus = ws.bus();
    let at_cap = "q".repeat(FIND_QUERY_MAX);
    let found = ok(
        &bus,
        "editor.find",
        json!({ "path": "/notes.md", "query": at_cap }),
    );
    assert_eq!(found["matches"].as_array().unwrap().len(), 0);
    let over = "q".repeat(FIND_QUERY_MAX + 1);
    match err(
        &bus,
        "editor.find",
        json!({ "path": "/notes.md", "query": over }),
    ) {
        CommandError::BadParams { message, .. } => {
            assert!(message.contains("1024"), "{message}");
        }
        other => panic!("expected bad params, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// What the bus is told
// ---------------------------------------------------------------------------

#[test]
fn registering_the_file_commands_registers_exactly_the_editors_four() {
    let ws = Workspace::new();
    let mut ids: Vec<String> = ws
        .registry()
        .list(None)
        .into_iter()
        .map(|l| l.spec.id().as_str().to_string())
        .filter(|id| id.starts_with("editor."))
        .collect();
    ids.sort();
    assert_eq!(
        ids,
        ["editor.find", "editor.open", "editor.revert", "editor.save"]
    );
}

#[test]
fn reads_run_without_asking_and_a_save_is_an_ordinary_write() {
    let ws = Workspace::new();
    let registry = ws.registry();
    let spec = |id: &str| registry.spec(&CommandId::new(id).unwrap()).unwrap();
    for id in ["editor.open", "editor.revert", "editor.find"] {
        assert_eq!(spec(id).risk(), RiskLevel::ReadOnly, "{id}");
    }
    assert_eq!(spec("editor.save").risk(), RiskLevel::Write);
    assert!(spec("editor.save").redacts_params());
    assert!(!spec("editor.open").redacts_params());
}
