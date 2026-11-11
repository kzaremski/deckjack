//! The file layer, exercised as a security boundary.
//!
//! The predecessor project took an absolute path from the client, ran
//! `canonicalize` on it and used the result, which made its configured root
//! decorative. Most of what follows is an attempt to reproduce that: spell a
//! path so that something outside the workspace, or inside `.deckjack/`, comes
//! back. Each test is named for the claim it makes, and a handful assert the
//! opposite direction — a boundary that refuses paths it should serve is also
//! broken, just less dangerously.
//!
//! Two kinds of fixture are used on purpose. [`Workspace`] is a real directory
//! with real symlinks, because the kernel is the only authority on what
//! `canonicalize` does. [`FakeFs`] is an injected [`Fs`], for the two things a
//! real disk cannot express from a test: proving that containment is decided on
//! the canonical path rather than the requested one, and recording which host
//! path is handed to the filesystem after resolution has finished.

use std::collections::BTreeMap;
use std::io;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use base64::Engine as _;
use serde_json::{Value, json};

use deckjack_core::command::{
    Admitted, Bus, CommandError, CommandId, Completion, Decision, Issuer, Journal, JournalError,
    LogEntry, NodeId, OsEnv, Registry, Request, RiskLevel,
};
use deckjack_core::files::{
    DEFAULT_MAX_READ, EntryKind, FileError, Fs, HIDDEN_DIR, ListOptions, MAX_DEPTH, MAX_NAME_LEN,
    MAX_PATH_LEN, MAX_WRITE, Meta, NodeName, NodePath, PathError, Root, VirtualPath,
    register_commands,
};

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

/// A real working directory, with room for siblings *outside* it.
///
/// The root is a subdirectory of the temporary directory rather than the
/// temporary directory itself, so a test can create something next to the
/// workspace and then try to reach it. `.deckjack/state.db` always exists,
/// because "refused" and "not there" are different answers and the tests below
/// have to be able to tell them apart.
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
        Workspace { _dir: dir, root }
    }

    /// A host path beside the workspace, which nothing through this layer may
    /// reach.
    fn beside(&self, name: &str) -> PathBuf {
        self.root.parent().unwrap().join(name)
    }

    fn host(&self, rel: &str) -> PathBuf {
        self.root.join(rel)
    }

    fn file(&self, rel: &str, body: &str) -> PathBuf {
        let path = self.host(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(&path, body).unwrap();
        path
    }

    fn dir(&self, rel: &str) -> PathBuf {
        let path = self.host(rel);
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    /// A symlink at `rel` inside the workspace, pointing wherever `target`
    /// says — including out of the workspace.
    fn link(&self, rel: &str, target: impl AsRef<Path>) {
        let path = self.host(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::os::unix::fs::symlink(target.as_ref(), &path).unwrap();
    }

    fn open(&self) -> Root {
        Root::open(&self.root).unwrap()
    }

    /// The whole file layer plus the bus, as the server wires it.
    fn bus(&self) -> Bus {
        let registry = Arc::new(Registry::new());
        register_commands(&registry, &self.root, "r620").unwrap();
        Bus::new(registry, NodeId::new("r620"), Arc::new(OsEnv))
    }
}

fn vp(text: &str) -> VirtualPath {
    VirtualPath::parse(text).unwrap()
}

fn np(text: &str) -> NodePath {
    NodePath::parse(text).unwrap()
}

/// Run one command to completion. Dispatch is synchronous and the handler is
/// not, so each call gets its own runtime; nothing here is timing-dependent.
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
    let done = run(bus, command, params);
    match done.outcome {
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

/// Everything the bus wrote down, so a test can assert what did *not* reach
/// the log.
#[derive(Default)]
struct Captured(Mutex<Vec<LogEntry>>);

impl Journal for Captured {
    fn record(&self, entry: LogEntry) -> Result<(), JournalError> {
        self.0.lock().unwrap().push(entry);
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// An injected filesystem, for what a real disk cannot express
// ---------------------------------------------------------------------------

/// A filesystem described by a table.
///
/// It exists for two claims. First, that `canonicalize` is what containment is
/// tested on: here a path's canonical form is whatever the table says, so a
/// resolution that tested the *requested* components would pass tests the real
/// disk makes awkward to set up. Second, that the path handed to the
/// filesystem after resolution is the canonical one — recorded in `opened`, so
/// a test can prove the unproven path is never the one opened.
struct FakeFs {
    canonical: BTreeMap<PathBuf, PathBuf>,
    kinds: BTreeMap<PathBuf, EntryKind>,
    opened: Mutex<Vec<PathBuf>>,
}

impl FakeFs {
    fn new() -> FakeFs {
        FakeFs {
            canonical: BTreeMap::new(),
            kinds: BTreeMap::new(),
            opened: Mutex::new(Vec::new()),
        }
    }

    fn entry(mut self, path: &str, kind: EntryKind) -> FakeFs {
        self.kinds.insert(PathBuf::from(path), kind);
        self
    }

    /// `path` exists and canonicalizes to `target`, as a symlink would.
    fn links(mut self, path: &str, target: &str) -> FakeFs {
        self.canonical
            .insert(PathBuf::from(path), PathBuf::from(target));
        self
    }
}

impl Fs for FakeFs {
    fn canonicalize(&self, path: &Path) -> io::Result<PathBuf> {
        if let Some(target) = self.canonical.get(path) {
            return Ok(target.clone());
        }
        if self.kinds.contains_key(path) {
            return Ok(path.to_path_buf());
        }
        Err(io::Error::from(io::ErrorKind::NotFound))
    }

    fn meta(&self, path: &Path) -> io::Result<Meta> {
        let kind = *self
            .kinds
            .get(path)
            .ok_or_else(|| io::Error::from(io::ErrorKind::NotFound))?;
        Ok(Meta {
            kind,
            len: 3,
            modified: Some(1_700_000_000),
            readonly: false,
        })
    }

    fn link_meta(&self, path: &Path) -> io::Result<Meta> {
        self.meta(path)
    }

    fn read_dir(&self, _path: &Path) -> io::Result<Vec<String>> {
        Ok(Vec::new())
    }

    fn read_range(&self, path: &Path, _offset: u64, _max: u64) -> io::Result<Vec<u8>> {
        self.opened.lock().unwrap().push(path.to_path_buf());
        Ok(b"abc".to_vec())
    }

    fn write_atomic(&self, _path: &Path, _bytes: &[u8]) -> io::Result<()> {
        unimplemented!("not reached by the resolution tests")
    }
    fn create_dir(&self, _path: &Path) -> io::Result<()> {
        unimplemented!("not reached by the resolution tests")
    }
    fn remove_file(&self, _path: &Path) -> io::Result<()> {
        unimplemented!("not reached by the resolution tests")
    }
    fn remove_dir(&self, _path: &Path) -> io::Result<()> {
        unimplemented!("not reached by the resolution tests")
    }
    fn remove_dir_all(&self, _path: &Path) -> io::Result<()> {
        unimplemented!("not reached by the resolution tests")
    }
    fn rename(&self, _from: &Path, _to: &Path) -> io::Result<()> {
        unimplemented!("not reached by the resolution tests")
    }
}

// ---------------------------------------------------------------------------
// The lexical gate: what a path may say at all
// ---------------------------------------------------------------------------

#[test]
fn a_dotdot_segment_is_refused_wherever_it_appears() {
    // Including the spelling that would land back inside the workspace. This
    // layer does not normalize `..` away, because with a symlink in the path
    // the lexical answer and the kernel's answer differ, and the lexical one
    // would be a check on a path that is never opened. The result is that
    // `/notes/../notes` is refused rather than served; the module's own docs
    // argue the trade, and the claim under test here is only that no spelling
    // of `..` is treated as data.
    for text in [
        "..",
        "/..",
        "../",
        "/../etc/passwd",
        "/notes/../notes",
        "/notes/../../../../etc/shadow",
        &format!("/notes{}", "/..".repeat(64)),
        &format!("/{HIDDEN_DIR}/../{HIDDEN_DIR}"),
    ] {
        assert_eq!(
            VirtualPath::parse(text),
            Err(PathError::Traversal),
            "{text:?} must not parse"
        );
    }
}

#[test]
fn a_nul_byte_is_refused_because_the_syscall_would_stop_there() {
    // `/etc/passwd\0.txt` passes an extension check and opens /etc/passwd.
    assert_eq!(
        VirtualPath::parse("/notes/a\0.txt"),
        Err(PathError::Nul),
        "a NUL would make the checked path and the opened path differ"
    );
    assert_eq!(
        vp("/notes").join("a\0b").unwrap_err(),
        PathError::NotAName {
            name: "a\0b".to_string()
        }
    );
}

#[test]
fn every_spelling_of_one_path_parses_to_the_same_path_and_round_trips() {
    let canonical = vp("/notes/today.md");
    for text in [
        "/notes/today.md",
        "notes/today.md",
        "/notes/today.md/",
        "//notes//today.md//",
        "/./notes/./today.md",
    ] {
        assert_eq!(vp(text), canonical, "{text:?} is the same path");
    }
    // What this layer prints has to parse back to what it printed, or an
    // address handed to the browser would address something else next time.
    assert_eq!(canonical.to_string(), "/notes/today.md");
    assert_eq!(vp(&canonical.to_string()), canonical);
    assert_eq!(VirtualPath::root().to_string(), "/");
    assert_eq!(vp("/"), VirtualPath::root());
}

#[test]
fn a_path_no_filesystem_could_hold_is_refused_before_the_disk_is_touched() {
    let longest = "a".repeat(MAX_NAME_LEN);
    assert_eq!(vp(&longest).depth(), 1, "255 bytes is a legal name");
    assert!(matches!(
        VirtualPath::parse(&"a".repeat(MAX_NAME_LEN + 1)),
        Err(PathError::NameTooLong { .. })
    ));

    let deepest = "/a".repeat(MAX_DEPTH);
    assert_eq!(vp(&deepest).depth(), MAX_DEPTH);
    assert!(matches!(
        VirtualPath::parse(&"/a".repeat(MAX_DEPTH + 1)),
        Err(PathError::TooDeep { .. })
    ));

    assert!(matches!(
        VirtualPath::parse(&"a".repeat(MAX_PATH_LEN + 1)),
        Err(PathError::TooLong { .. })
    ));
}

#[test]
fn unicode_spellings_of_one_name_are_not_folded_together() {
    // Linux compares bytes, so precomposed and decomposed "café" are two
    // different files. Normalizing here would answer a different question than
    // the one asked, and would let one name stand in for another.
    let precomposed = vp("/caf\u{e9}.md");
    let decomposed = vp("/cafe\u{301}.md");
    assert_ne!(precomposed, decomposed);
    assert_eq!(precomposed.name().unwrap().len(), "caf\u{e9}.md".len());

    // A name that merely looks like `..` is an ordinary name, and refusing it
    // would be the over-refusal direction of the same mistake.
    let fullwidth = vp("/\u{ff0e}\u{ff0e}/notes");
    assert_eq!(fullwidth.depth(), 2);
    assert!(!fullwidth.is_hidden_state());
}

#[test]
fn join_takes_one_plain_name_and_nothing_else() {
    let base = vp("/notes");
    assert_eq!(base.join("today.md").unwrap(), vp("/notes/today.md"));
    assert_eq!(base.join("..").unwrap_err(), PathError::Traversal);
    assert_eq!(base.join("").unwrap_err(), PathError::EmptyName);
    assert_eq!(base.join(".").unwrap_err(), PathError::EmptyName);
    assert!(matches!(
        base.join("a/b").unwrap_err(),
        PathError::NotAName { .. }
    ));
    // `join` builds the name; it is resolution that refuses it, and the path
    // it built has to say what it is so resolution can.
    assert!(base.join(HIDDEN_DIR).unwrap().is_hidden_state());
}

// ---------------------------------------------------------------------------
// node:path
// ---------------------------------------------------------------------------

#[test]
fn an_absolute_host_path_is_read_as_workspace_relative() {
    // The predecessor's bug in one line: a client sends a host path and the
    // layer obliges. Here `/etc/passwd` can only ever mean `etc/passwd`
    // *inside* the workspace, and there is no function in the module that
    // accepts a host path from a caller at all.
    let address = np("/etc/passwd");
    assert_eq!(address.node, None);
    assert_eq!(address.path, vp("etc/passwd"));

    let ws = Workspace::new();
    let secret = ws.file("etc/passwd", "workspace copy");
    let resolved = ws.open().resolve(&address.path).unwrap();
    assert_eq!(resolved.real_path(), secret.canonicalize().unwrap());
    assert!(
        resolved.real_path().starts_with(&ws.root),
        "{} escaped the workspace",
        resolved.real_path().display()
    );
}

#[test]
fn a_colon_after_the_first_slash_belongs_to_the_file_name() {
    let local = np("/notes/a:b.txt");
    assert_eq!(local.node, None);
    assert_eq!(local.path.name().unwrap(), "a:b.txt");

    let remote = np("r620:/photos/2026");
    assert_eq!(remote.node.as_ref().unwrap().as_str(), "r620");
    assert_eq!(remote.path, vp("/photos/2026"));
    assert_eq!(remote.to_string(), "r620:/photos/2026");
    assert_eq!(np(&remote.to_string()), remote);
}

#[test]
fn a_node_name_that_reads_as_a_flag_or_a_relative_path_is_refused() {
    for text in ["-rf", ".", "..", ".hidden", "a/b", "a b", "a:b"] {
        assert!(
            NodeName::parse(text).is_err(),
            "{text:?} must not be a node name"
        );
    }
    assert_eq!(NodeName::parse(""), Err(PathError::EmptyNode));
    assert!(matches!(
        NodeName::parse(&"a".repeat(NodeName::MAX_LEN + 1)),
        Err(PathError::NodeTooLong { .. })
    ));
    assert!(NodeName::parse("r620-lxc_1.local").is_ok());
}

#[test]
fn every_node_id_this_build_can_generate_is_usable_in_a_file_address() {
    // `node::id` writes hex, and this is the reason it does. URL-safe base64
    // begins with '-' whenever the first byte is 0xf8..=0xfb — 4 of 256, so
    // about 1 fresh install in 64 — and `NodeName::parse` refuses a leading
    // dash because it reads as a flag wherever the name is interpolated. Such
    // an id made its own file namespace unreachable and the server unable to
    // start, recoverable only by deleting the id file.
    //
    // The old encoding, kept here as the thing that must not come back:
    let base64_id = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode([0xf8u8; 16]);
    assert!(
        base64_id.starts_with('-'),
        "{base64_id} should start with a dash"
    );
    assert!(
        NodeName::parse(&base64_id).is_err(),
        "a leading dash must stay refused: that is why the encoding changed"
    );

    // What this build actually generates, over a fresh install directory each
    // time so every byte pattern gets a turn.
    for _ in 0..64 {
        let dir = tempfile::tempdir().expect("an install directory");
        let id = deckjack_core::node::id(dir.path()).expect("an id");
        assert_eq!(id.len(), 32, "{id} is not 16 bytes of hex");
        assert!(
            id.bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
            "{id} is not hex"
        );
        NodeName::parse(&id).expect("an id node::id can generate must be a usable address");
    }
}

#[test]
fn an_address_on_another_node_is_never_served_from_this_filesystem() {
    let ws = Workspace::new();
    ws.file("notes/today.md", "local");
    let bus = ws.bus();

    // The same path exists here. Asked for on another node, the answer must be
    // "ask that node", not this node's copy.
    let e = err(
        &bus,
        "files.read",
        json!({ "path": "fold:/notes/today.md" }),
    );
    assert!(
        matches!(e, CommandError::Unsupported { .. }),
        "expected an unsupported-node refusal, got {e}"
    );

    // This node's own name is served, so the refusal is about the node and not
    // about qualified addresses.
    let contents = ok(
        &bus,
        "files.read",
        json!({ "path": "r620:/notes/today.md" }),
    );
    assert_eq!(contents["content"], "local");
}

// ---------------------------------------------------------------------------
// `.deckjack/` is never exposed
// ---------------------------------------------------------------------------

#[test]
fn the_hidden_directory_is_refused_by_every_spelling() {
    let ws = Workspace::new();
    // A nested workspace, with a hidden directory of its own that exists.
    ws.dir("project");
    std::fs::create_dir(ws.host("project").join(HIDDEN_DIR)).unwrap();
    let root = ws.open();

    for text in [
        "/.deckjack",
        ".deckjack",
        "/.deckjack/",
        "//.deckjack//state.db",
        "/./.deckjack/state.db",
        // Any component, not only the first: a nested workspace has its own
        // hidden directory and the rule is about hidden state, not position.
        "/project/.deckjack",
        "/project/.deckjack/state.db",
    ] {
        let path = vp(text);
        assert!(path.is_hidden_state(), "{text:?} is hidden state");
        assert!(
            matches!(root.resolve(&path), Err(FileError::HiddenState { .. })),
            "{text:?} must be refused as hidden state"
        );
        assert!(matches!(
            root.resolve_link(&path),
            Err(FileError::HiddenState { .. })
        ));
    }
}

#[test]
fn case_is_not_a_way_into_the_hidden_directory() {
    // The working directory may sit on a case-insensitive filesystem — APFS by
    // default, or a mounted exFAT or NTFS volume — where `realpath` resolves
    // symlinks without correcting case, so `.DeckJack/state.db` opens the real
    // thing. The refusal therefore cannot depend on the host's case folding.
    let ws = Workspace::new();
    let root = ws.open();
    for text in ["/.DECKJACK/state.db", "/.DeckJack", "/project/.Deckjack/x"] {
        let path = vp(text);
        assert!(
            path.is_hidden_state(),
            "{text:?} names the hidden directory"
        );
        assert!(
            matches!(root.resolve(&path), Err(FileError::HiddenState { .. })),
            "{text:?} must be refused whatever the filesystem thinks of case"
        );
    }
}

#[test]
fn the_hidden_directory_is_refused_without_saying_whether_it_exists() {
    // A refusal that turned into "not found" for a missing name would make the
    // boundary a probe for what is in there.
    let ws = Workspace::new();
    let root = ws.open();
    for text in [
        "/.deckjack/state.db",
        "/.deckjack/there-is-no-such-file",
        "/.deckjack/nested/deep/missing",
    ] {
        assert!(matches!(
            root.resolve(&vp(text)),
            Err(FileError::HiddenState { .. })
        ));
    }
}

#[test]
fn a_symlink_pointing_into_the_hidden_directory_is_refused_not_followed() {
    let ws = Workspace::new();
    // Every component of the requested path is innocent; only the canonical
    // form is not. This is why containment is tested after canonicalize.
    ws.link("notes", HIDDEN_DIR);
    ws.link("db.sqlite", ws.host(HIDDEN_DIR).join("state.db"));
    let root = ws.open();

    assert!(matches!(
        root.resolve(&vp("/notes")),
        Err(FileError::HiddenState { .. })
    ));
    assert!(matches!(
        root.resolve(&vp("/notes/state.db")),
        Err(FileError::HiddenState { .. })
    ));
    let e = root.read(&vp("/db.sqlite"), DEFAULT_MAX_READ).unwrap_err();
    assert!(
        matches!(e, FileError::HiddenState { .. }),
        "reading through the link must be refused, got {e}"
    );
    assert!(e.is_refusal(), "a reach into hidden state is not routine");
}

#[test]
fn the_hidden_directory_never_appears_in_a_listing() {
    let ws = Workspace::new();
    ws.file("notes.md", "hello");
    ws.file(".bashrc", "export");
    let root = ws.open();

    for show_dotfiles in [true, false] {
        let listing = root
            .list(
                &VirtualPath::root(),
                &ListOptions {
                    show_dotfiles,
                    ..ListOptions::default()
                },
            )
            .unwrap();
        assert!(
            !listing.entries.iter().any(|e| e.name == HIDDEN_DIR),
            "{HIDDEN_DIR} listed with show_dotfiles={show_dotfiles}"
        );
    }

    // `.deckjack` is not a dotfile for this purpose: dotfiles are the owner's
    // own config and are shown by default, the hidden directory never is.
    let shown = root
        .list(&VirtualPath::root(), &ListOptions::default())
        .unwrap();
    assert!(shown.entries.iter().any(|e| e.name == ".bashrc"));
    let hidden = root
        .list(
            &VirtualPath::root(),
            &ListOptions {
                show_dotfiles: false,
                ..ListOptions::default()
            },
        )
        .unwrap();
    assert!(!hidden.entries.iter().any(|e| e.name == ".bashrc"));
}

#[test]
fn the_commands_refuse_the_hidden_directory_although_the_file_is_right_there() {
    let ws = Workspace::new();
    let bus = ws.bus();
    // End to end: the same boundary, through the bus the browser talks to.
    for command in ["files.read", "files.stat"] {
        let e = err(&bus, command, json!({ "path": "/.deckjack/state.db" }));
        assert!(
            e.to_string().contains(HIDDEN_DIR),
            "{command} should say what it refused, said {e}"
        );
    }
    let e = err(
        &bus,
        "files.write",
        json!({ "path": "/.deckjack/state.db", "content": "" }),
    );
    assert!(e.to_string().contains(HIDDEN_DIR), "{e}");
    assert_eq!(
        std::fs::read(ws.host(HIDDEN_DIR).join("state.db")).unwrap(),
        b"hunter2",
        "state.db was written through the file API"
    );
    let e = err(&bus, "files.delete", json!({ "path": "/.deckjack" }));
    assert!(e.to_string().contains(HIDDEN_DIR), "{e}");
    assert!(
        ws.host(HIDDEN_DIR).exists(),
        "the hidden directory was deleted"
    );
}

// ---------------------------------------------------------------------------
// Containment
// ---------------------------------------------------------------------------

#[test]
fn a_symlink_out_of_the_workspace_is_refused_not_followed() {
    let ws = Workspace::new();
    let outside = ws.beside("outside");
    std::fs::create_dir(&outside).unwrap();
    std::fs::write(outside.join("secret"), "not yours").unwrap();
    ws.link("escape", &outside);
    ws.link("absolute", "/etc");
    let root = ws.open();

    for text in ["/escape", "/escape/secret", "/absolute/passwd"] {
        let e = root.resolve(&vp(text)).unwrap_err();
        assert!(
            matches!(e, FileError::OutsideRoot { .. }),
            "{text:?} must be refused as outside the workspace, got {e}"
        );
        assert!(
            e.is_refusal(),
            "{text:?} is a refusal, not a filesystem fact"
        );
    }
    assert!(matches!(
        root.read(&vp("/escape/secret"), DEFAULT_MAX_READ),
        Err(FileError::OutsideRoot { .. })
    ));
}

#[test]
fn a_sibling_directory_sharing_a_string_prefix_is_outside_the_workspace() {
    // `<tmp>/work-old` starts with `<tmp>/work`. Containment compared as
    // strings would accept it; compared component-wise it does not.
    let ws = Workspace::new();
    let sibling = ws.beside("work-old");
    std::fs::create_dir(&sibling).unwrap();
    std::fs::write(sibling.join("notes.md"), "last year").unwrap();
    ws.link("old", &sibling);

    let e = ws.open().resolve(&vp("/old/notes.md")).unwrap_err();
    assert!(
        matches!(e, FileError::OutsideRoot { .. }),
        "work-old is a different directory, got {e}"
    );
}

#[test]
fn a_symlink_that_stays_inside_the_workspace_is_followed() {
    // The over-refusal direction: a boundary that refuses the owner's own
    // shortcuts is broken too, just less dangerously.
    let ws = Workspace::new();
    ws.file("notes/today.md", "real contents");
    ws.link("shortcut", ws.host("notes"));
    ws.link("relative", "notes/today.md");
    let root = ws.open();

    let resolved = root.resolve(&vp("/shortcut/today.md")).unwrap();
    assert_eq!(
        resolved.path(),
        &vp("/notes/today.md"),
        "resolved to target"
    );
    assert!(resolved.followed_symlink());
    assert_eq!(
        root.read(&vp("/shortcut/today.md"), DEFAULT_MAX_READ)
            .unwrap(),
        b"real contents"
    );
    assert_eq!(
        root.read(&vp("/relative"), DEFAULT_MAX_READ).unwrap(),
        b"real contents"
    );
}

#[test]
fn a_workspace_reached_through_a_symlink_still_resolves_its_own_paths() {
    // `~/work -> /srv/deckjack`. If the root were stored as given rather than
    // canonicalized, every single path would fail containment.
    let ws = Workspace::new();
    ws.file("notes/today.md", "hello");
    let alias = ws.beside("alias");
    std::os::unix::fs::symlink(&ws.root, &alias).unwrap();

    let root = Root::open(&alias).unwrap();
    assert_eq!(root.real_path(), ws.root.canonicalize().unwrap());
    assert_eq!(
        root.read(&vp("/notes/today.md"), DEFAULT_MAX_READ).unwrap(),
        b"hello"
    );
}

#[test]
fn a_file_that_is_not_a_directory_cannot_be_opened_as_the_workspace_root() {
    let ws = Workspace::new();
    let file = ws.file("notes.md", "hello");
    assert!(matches!(Root::open(&file), Err(FileError::BadRoot { .. })));
    assert!(matches!(
        Root::open(&ws.beside("nothing-here")),
        Err(FileError::BadRoot { .. })
    ));
}

#[test]
fn containment_is_decided_on_the_canonical_path_not_the_requested_one() {
    // Both requests name innocent components. Only the canonical form betrays
    // them, which is the case a lexical check passes and this one must not.
    let fs = FakeFs::new()
        .entry("/srv/work", EntryKind::Dir)
        .entry("/srv/work/notes", EntryKind::Dir)
        .links("/srv/work/evil", "/etc/shadow")
        .links("/srv/work/sneaky", "/srv/work/.deckjack/state.db")
        .entry("/etc/shadow", EntryKind::File)
        .entry("/srv/work/.deckjack/state.db", EntryKind::File);
    let root = Root::with_fs(Path::new("/srv/work"), Arc::new(fs)).unwrap();

    match root.resolve(&vp("/evil")).unwrap_err() {
        FileError::OutsideRoot { resolved, .. } => {
            assert_eq!(resolved, PathBuf::from("/etc/shadow"))
        }
        other => panic!("expected an outside-the-root refusal, got {other}"),
    }
    assert!(matches!(
        root.resolve(&vp("/sneaky")),
        Err(FileError::HiddenState { .. })
    ));
}

#[test]
fn what_reaches_the_filesystem_is_the_path_resolution_proved() {
    // A second resolution at open time would be a second chance to be
    // redirected. The host path handed to the filesystem is the canonical one
    // containment was proved on, and nothing re-walks the requested spelling.
    let fs = Arc::new(
        FakeFs::new()
            .entry("/srv/work", EntryKind::Dir)
            .entry("/srv/work/notes/a.txt", EntryKind::File)
            .links("/srv/work/shortcut", "/srv/work/notes/a.txt"),
    );
    let root = Root::with_fs(Path::new("/srv/work"), Arc::clone(&fs) as Arc<dyn Fs>).unwrap();

    assert_eq!(root.read(&vp("/shortcut"), 64).unwrap(), b"abc");
    assert_eq!(
        *fs.opened.lock().unwrap(),
        vec![PathBuf::from("/srv/work/notes/a.txt")],
        "the filesystem must be given the resolved path, never the requested one"
    );
}

// ---------------------------------------------------------------------------
// Writes, deletes and the final symlink
// ---------------------------------------------------------------------------

#[test]
fn writing_through_a_symlink_is_refused_even_when_it_points_inside() {
    let ws = Workspace::new();
    let outside = ws.beside("outside.txt");
    std::fs::write(&outside, "not yours").unwrap();
    ws.file("notes/today.md", "real");
    ws.link("out", &outside);
    ws.link("in", ws.host("notes/today.md"));
    let root = ws.open();

    // Outward: refused as a symlink before containment even matters, because
    // `resolve_link` stops at the parent.
    let e = root.write(&vp("/out"), b"owned").unwrap_err();
    assert!(
        matches!(e, FileError::WriteThroughSymlink { .. }),
        "got {e}"
    );
    assert_eq!(std::fs::read_to_string(&outside).unwrap(), "not yours");

    // Inward too: the link may be the owner's own shortcut, but the file API
    // is not where to work out which, so it refuses rather than guesses.
    let e = root.write(&vp("/in"), b"owned").unwrap_err();
    assert!(
        matches!(e, FileError::WriteThroughSymlink { .. }),
        "got {e}"
    );
    assert_eq!(
        std::fs::read_to_string(ws.host("notes/today.md")).unwrap(),
        "real"
    );
}

#[test]
fn deleting_a_symlink_unlinks_the_link_and_never_its_target() {
    let ws = Workspace::new();
    let outside = ws.beside("outside.txt");
    std::fs::write(&outside, "not yours").unwrap();
    ws.file("notes/today.md", "real");
    ws.link("out", &outside);
    ws.link("in", ws.host("notes/today.md"));
    let root = ws.open();

    // An escaping link must still be removable: it is listed so the operator
    // can see it, and a boundary that refuses to let them clean it up would
    // leave it there forever.
    root.remove(&vp("/out"), false).unwrap();
    assert!(!ws.host("out").exists());
    assert!(outside.exists(), "deleting the link deleted its target");

    root.remove(&vp("/in"), false).unwrap();
    assert!(
        ws.host("notes/today.md").exists(),
        "deleting the link deleted its target"
    );
}

#[test]
fn an_escaping_symlink_is_listed_as_escaping_rather_than_hidden() {
    let ws = Workspace::new();
    let outside = ws.beside("outside");
    std::fs::create_dir(&outside).unwrap();
    ws.link("escape", &outside);
    ws.link("hidden", ws.host(HIDDEN_DIR));
    ws.file("notes.md", "hello");
    let root = ws.open();

    let listing = root
        .list(&VirtualPath::root(), &ListOptions::default())
        .unwrap();
    let by_name = |name: &str| {
        listing
            .entries
            .iter()
            .find(|e| e.name == name)
            .unwrap_or_else(|| panic!("{name} missing from the listing"))
            .clone()
    };
    assert!(by_name("escape").escapes, "an outward link must be flagged");
    assert!(
        by_name("hidden").escapes,
        "a link into hidden state must be flagged"
    );
    assert_eq!(by_name("escape").kind, EntryKind::Symlink);
    assert!(!by_name("notes.md").escapes);
}

#[test]
fn the_workspace_root_itself_cannot_be_written_deleted_or_renamed() {
    let ws = Workspace::new();
    let root = ws.open();
    assert!(matches!(
        root.write(&VirtualPath::root(), b"x"),
        Err(FileError::RootIsNotAFile { .. })
    ));
    assert!(matches!(
        root.remove(&VirtualPath::root(), true),
        Err(FileError::RootIsNotAFile { .. })
    ));
    assert!(matches!(
        root.rename(&VirtualPath::root(), &vp("/elsewhere")),
        Err(FileError::RootIsNotAFile { .. })
    ));
    assert!(ws.root.exists());
}

#[test]
fn a_rename_never_consumes_an_existing_destination() {
    let ws = Workspace::new();
    ws.file("a.md", "first");
    ws.file("b.md", "second");
    let root = ws.open();

    assert!(matches!(
        root.rename(&vp("/a.md"), &vp("/b.md")),
        Err(FileError::Exists { .. })
    ));
    assert_eq!(std::fs::read_to_string(ws.host("b.md")).unwrap(), "second");

    // A mistyped source must read as missing, not as a conflict, or the
    // message sends the operator looking at the wrong end.
    assert!(matches!(
        root.rename(&vp("/typo.md"), &vp("/c.md")),
        Err(FileError::NotFound { .. })
    ));

    root.rename(&vp("/a.md"), &vp("/c.md")).unwrap();
    assert_eq!(std::fs::read_to_string(ws.host("c.md")).unwrap(), "first");
}

#[test]
fn a_write_keeps_the_mode_the_file_already_had() {
    // The write is a temporary file plus a rename, and a temporary file is
    // created 0600. Saving a shell script through the file API must not strip
    // its executable bit or lock out everyone but the owner.
    let ws = Workspace::new();
    let script = ws.file("build.sh", "#!/bin/sh\necho old\n");
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();

    ws.open()
        .write(&vp("/build.sh"), b"#!/bin/sh\necho new\n")
        .unwrap();

    assert_eq!(
        std::fs::read_to_string(&script).unwrap(),
        "#!/bin/sh\necho new\n"
    );
    assert_eq!(
        std::fs::metadata(&script).unwrap().permissions().mode() & 0o777,
        0o755,
        "the file's mode was not carried across the atomic replace"
    );
}

#[test]
fn a_directory_with_anything_in_it_needs_recursive_to_go() {
    let ws = Workspace::new();
    ws.file("project/src/main.rs", "fn main() {}");
    let root = ws.open();

    assert!(matches!(
        root.remove(&vp("/project"), false),
        Err(FileError::DirectoryNotEmpty { .. })
    ));
    assert!(ws.host("project/src/main.rs").exists());
    root.remove(&vp("/project"), true).unwrap();
    assert!(!ws.host("project").exists());
}

#[test]
fn a_new_directory_needs_a_parent_that_already_exists() {
    let ws = Workspace::new();
    let root = ws.open();
    assert!(matches!(
        root.create_dir(&vp("/a/b/c")),
        Err(FileError::NotFound { .. })
    ));
    assert!(
        !ws.host("a").exists(),
        "a typo must not leave a tree behind"
    );
    root.create_dir(&vp("/a")).unwrap();
    assert!(matches!(
        root.create_dir(&vp("/a")),
        Err(FileError::Exists { .. })
    ));
}

#[test]
fn a_write_under_an_escaping_parent_is_refused() {
    let ws = Workspace::new();
    let outside = ws.beside("outside");
    std::fs::create_dir(&outside).unwrap();
    ws.link("escape", &outside);
    let root = ws.open();

    for result in [
        root.write(&vp("/escape/planted"), b"x").err(),
        root.create_dir(&vp("/escape/planted")).err(),
        root.remove(&vp("/escape/planted"), false).err(),
    ] {
        let e = result.expect("writing under an escaping parent must be refused");
        assert!(matches!(e, FileError::OutsideRoot { .. }), "got {e}");
    }
    assert!(!outside.join("planted").exists());
}

// ---------------------------------------------------------------------------
// Reads and listings
// ---------------------------------------------------------------------------

#[test]
fn a_read_over_the_limit_is_refused_rather_than_truncated() {
    // Half a file looks like a corrupt file, which is worse than an error.
    let ws = Workspace::new();
    ws.file("big.bin", "0123456789");
    let root = ws.open();

    match root.read(&vp("/big.bin"), 4) {
        Err(FileError::TooLarge { len, limit, .. }) => {
            assert_eq!((len, limit), (10, 4));
        }
        other => panic!("expected a refusal, got {other:?}"),
    }
    assert_eq!(root.read(&vp("/big.bin"), 10).unwrap().len(), 10);
}

#[test]
fn only_a_regular_file_can_be_read() {
    let ws = Workspace::new();
    ws.dir("notes");
    let root = ws.open();
    assert!(matches!(
        root.read(&vp("/notes"), DEFAULT_MAX_READ),
        Err(FileError::NotAFile { .. })
    ));
    assert!(matches!(
        root.read(&vp("/nothing"), DEFAULT_MAX_READ),
        Err(FileError::NotFound { .. })
    ));
    assert!(matches!(
        root.list(&vp("/nothing"), &ListOptions::default()),
        Err(FileError::NotFound { .. })
    ));
}

#[test]
fn a_listing_cut_short_by_the_cap_says_so() {
    let ws = Workspace::new();
    for i in 0..40 {
        ws.file(&format!("notes/{i:03}.md"), "x");
    }
    let root = ws.open();

    let capped = root
        .list(
            &vp("/notes"),
            &ListOptions {
                max_entries: 10,
                ..ListOptions::default()
            },
        )
        .unwrap();
    assert_eq!(capped.entries.len(), 10);
    assert!(
        capped.truncated,
        "a partial listing must admit it is partial"
    );

    let whole = root.list(&vp("/notes"), &ListOptions::default()).unwrap();
    assert_eq!(whole.entries.len(), 40);
    assert!(!whole.truncated);
}

#[test]
fn a_client_cannot_raise_the_servers_own_listing_cap() {
    // The cap is the server's memory bound, not a pagination preference: one
    // panel opening one directory must not decide how much the server holds.
    let ws = Workspace::new();
    let dir = ws.dir("many");
    let cap = ListOptions::default().max_entries;
    for i in 0..=cap {
        std::fs::File::create(dir.join(format!("{i:06}"))).unwrap();
    }
    let bus = ws.bus();

    let listing = ok(
        &bus,
        "files.list",
        json!({ "path": "/many", "max_entries": u64::MAX }),
    );
    assert_eq!(
        listing["entries"].as_array().unwrap().len(),
        cap,
        "a client asked for more than the cap and got it"
    );
    assert_eq!(listing["truncated"], true);

    // And it cannot ask for nothing either: zero would make every listing
    // look like an empty directory.
    let listing = ok(
        &bus,
        "files.list",
        json!({ "path": "/many", "max_entries": 0 }),
    );
    assert_eq!(listing["entries"].as_array().unwrap().len(), 1);
}

#[test]
fn a_read_limit_of_zero_refuses_rather_than_returning_an_empty_file() {
    let ws = Workspace::new();
    ws.file("notes.md", "hello");
    let bus = ws.bus();
    let e = err(
        &bus,
        "files.read",
        json!({ "path": "/notes.md", "max_bytes": 0 }),
    );
    assert!(
        e.to_string().contains("limit"),
        "a zero limit must refuse, not hand back an empty file: {e}"
    );
}

// ---------------------------------------------------------------------------
// The command layer
// ---------------------------------------------------------------------------

#[test]
fn reads_are_read_only_and_no_file_command_asks_the_operator_to_confirm() {
    // `terminal.write` was once marked Destructive and so asked the operator
    // to confirm every keystroke. The same mistake here would ask them to
    // confirm every save. Nothing these commands can reach is outside the
    // working directory, which is the condition the design attaches
    // confirmation to; what keeps an agent out of them is the grant.
    let ws = Workspace::new();
    let registry = Registry::new();
    register_commands(&registry, &ws.root, "r620").unwrap();

    for id in ["files.list", "files.read", "files.stat"] {
        let spec = registry.spec(&CommandId::new(id).unwrap()).unwrap();
        assert_eq!(
            spec.risk(),
            RiskLevel::ReadOnly,
            "{id} must not be able to write"
        );
    }
    for id in ["files.write", "files.mkdir", "files.rename", "files.delete"] {
        let spec = registry.spec(&CommandId::new(id).unwrap()).unwrap();
        assert_eq!(spec.risk(), RiskLevel::Write, "{id}");
    }
    for id in [
        "files.list",
        "files.read",
        "files.stat",
        "files.write",
        "files.mkdir",
        "files.rename",
        "files.delete",
    ] {
        let spec = registry.spec(&CommandId::new(id).unwrap()).unwrap();
        assert!(
            !spec.risk().needs_confirmation_from_operator(),
            "{id} would put a dialog in front of ordinary work"
        );
    }
}

#[test]
fn a_saved_files_contents_never_reach_the_audit_log() {
    // The log is readable from the terminal by design, and the parameters of a
    // write *are* the file. Saving a key or a password file must not copy it
    // into the log.
    let ws = Workspace::new();
    let registry = Arc::new(Registry::new());
    register_commands(&registry, &ws.root, "r620").unwrap();
    let journal = Arc::new(Captured::default());
    let bus = Bus::new(registry, NodeId::new("r620"), Arc::new(OsEnv))
        .with_journal(Arc::clone(&journal) as Arc<dyn Journal>);

    let secret = "AKIAIOSFODNN7EXAMPLE-not-a-real-key";
    ok(
        &bus,
        "files.write",
        json!({ "path": "/creds.txt", "content": secret }),
    );

    let entries = journal.0.lock().unwrap();
    assert!(!entries.is_empty(), "the write was not logged at all");
    for entry in entries.iter() {
        let rendered = format!("{:?} {:?} {:?}", entry.params, entry.detail, entry.decision);
        assert!(
            !rendered.contains(secret),
            "the file's contents reached the log: {rendered}"
        );
    }
    assert!(
        entries.iter().any(|e| e.decision == Decision::Ran),
        "the write must still be attributable even with its parameters redacted"
    );
    // A read's path is not a secret, so reads are not redacted; the redaction
    // is targeted rather than blanket.
    assert_eq!(
        std::fs::read_to_string(ws.host("creds.txt")).unwrap(),
        secret
    );
}

#[test]
fn bytes_that_are_not_utf8_round_trip_through_base64() {
    let ws = Workspace::new();
    let bus = ws.bus();
    // Lone 0xff and 0xfe are invalid UTF-8 in any position, and the embedded
    // NUL is the byte a text path would stop at.
    let raw = [0xffu8, 0xfe, 0x00, 0x41, 0x80, 0x0a];
    let encoded = base64::engine::general_purpose::STANDARD.encode(raw);

    ok(
        &bus,
        "files.write",
        json!({ "path": "/blob.bin", "content": encoded, "encoding": "base64" }),
    );
    assert_eq!(std::fs::read(ws.host("blob.bin")).unwrap(), raw);

    let contents = ok(&bus, "files.read", json!({ "path": "/blob.bin" }));
    assert_eq!(
        contents["encoding"], "base64",
        "a file that is not UTF-8 must be flagged, not decoded as text"
    );
    assert_eq!(
        contents["size"],
        raw.len(),
        "size is the file's own length, not the length of its transport form"
    );
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(contents["content"].as_str().unwrap())
        .unwrap();
    assert_eq!(decoded, raw, "the round trip changed the file");

    // Text stays text, so a browser is not made to decode a README.
    ws.file("readme.md", "# hello");
    let text = ok(&bus, "files.read", json!({ "path": "/readme.md" }));
    assert_eq!(text["encoding"], "utf8");
    assert_eq!(text["content"], "# hello");
}

#[test]
fn a_write_over_the_bus_cap_is_a_parameter_error_and_creates_nothing() {
    let ws = Workspace::new();
    let bus = ws.bus();
    let too_big = "a".repeat(MAX_WRITE as usize + 1);

    let e = err(
        &bus,
        "files.write",
        json!({ "path": "/huge.txt", "content": too_big }),
    );
    assert!(
        matches!(e, CommandError::BadParams { .. }),
        "an over-size write is a bad request, not a filesystem failure: {e}"
    );
    assert!(
        !ws.host("huge.txt").exists(),
        "the cap was checked after something was already on disk"
    );
}

#[test]
fn content_that_is_not_base64_is_a_parameter_error_and_leaves_nothing_behind() {
    let ws = Workspace::new();
    let dir = ws.dir("drop");
    let bus = ws.bus();

    let e = err(
        &bus,
        "files.write",
        json!({ "path": "/drop/f.bin", "content": "!!!not base64!!!", "encoding": "base64" }),
    );
    assert!(matches!(e, CommandError::BadParams { .. }), "got {e}");
    assert_eq!(
        std::fs::read_dir(&dir).unwrap().count(),
        0,
        "a decode failure left a temporary file behind"
    );
}

#[test]
fn unknown_parameters_are_refused_rather_than_ignored() {
    // A client that misspells a field must be told, not quietly given the
    // default: that is how `max_entries` becomes decorative.
    let ws = Workspace::new();
    let bus = ws.bus();
    let e = err(&bus, "files.list", json!({ "path": "/", "maxEntries": 5 }));
    assert!(matches!(e, CommandError::BadParams { .. }), "got {e}");
}

#[test]
fn files_delete_through_the_bus_unlinks_a_symlink_and_not_its_target() {
    let ws = Workspace::new();
    let outside = ws.beside("outside.txt");
    std::fs::write(&outside, "not yours").unwrap();
    ws.link("shortcut", &outside);
    let bus = ws.bus();

    let deleted = ok(&bus, "files.delete", json!({ "path": "/shortcut" }));
    assert_eq!(deleted["path"], "r620:/shortcut");
    assert!(!ws.host("shortcut").exists());
    assert!(outside.exists(), "deleting a shortcut deleted its target");
}

#[test]
fn a_path_the_namespace_cannot_hold_is_a_parameter_error_not_a_failure() {
    // The distinction matters to the UI: a bad parameter belongs beside the
    // field that was typed, a failure belongs in a banner.
    let ws = Workspace::new();
    let bus = ws.bus();
    for path in ["/../etc/passwd", "/notes/../..", "/a\u{0}b"] {
        let e = err(&bus, "files.stat", json!({ "path": path }));
        assert!(
            matches!(e, CommandError::BadParams { .. }),
            "{path:?} gave {e}"
        );
    }
}

#[test]
fn every_file_command_is_registered_under_the_files_namespace() {
    // `Snap::Maximize` for `Snap::Maximized`: a name that compiles and is
    // never reachable. A command the browser cannot address is the same bug.
    let ws = Workspace::new();
    let registry = Registry::new();
    register_commands(&registry, &ws.root, "r620").unwrap();

    let ids: Vec<String> = registry
        .list(None)
        .into_iter()
        .map(|l| l.spec.id().as_str().to_string())
        .collect();
    for id in [
        "files.list",
        "files.read",
        "files.stat",
        "files.write",
        "files.mkdir",
        "files.rename",
        "files.delete",
    ] {
        assert!(ids.contains(&id.to_string()), "{id} is not on the bus");
    }
    assert_eq!(ids.len(), 7, "registered: {ids:?}");
}

#[test]
fn a_node_id_that_cannot_appear_in_an_address_fails_at_registration() {
    // Better a named startup failure than a server whose file addresses do not
    // round-trip.
    let ws = Workspace::new();
    let registry = Registry::new();
    let e = register_commands(&registry, &ws.root, "not a node name").unwrap_err();
    assert!(e.to_string().contains("node"), "{e}");
    assert!(!registry.contains(&CommandId::new("files.list").unwrap()));
}

// ---------------------------------------------------------------------------
// Ordering
//
// Nothing in this module sleeps, waits, or measures elapsed time, so there is
// no clock to inject and nothing above needed one. What is injected instead is
// [`Fs`]: every containment rule is reachable from a table of paths, which is
// why the two sharpest tests here need no disk at all.
// ---------------------------------------------------------------------------

#[test]
fn a_listing_puts_directories_first_and_then_names_case_insensitively() {
    // Decided here so two panels cannot decide it differently.
    let ws = Workspace::new();
    ws.file("Zebra.md", "z");
    ws.file("apple.md", "a");
    ws.dir("Mango");
    ws.dir("banana");

    let listing = ws
        .open()
        .list(&VirtualPath::root(), &ListOptions::default())
        .unwrap();
    let names: Vec<&str> = listing.entries.iter().map(|e| e.name.as_str()).collect();
    assert_eq!(names, vec!["banana", "Mango", "apple.md", "Zebra.md"]);
}
