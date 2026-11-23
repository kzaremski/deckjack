//! `[ui]`, the hard-deny list and reserved names, as a manifest author meets
//! them: through `Manifest::parse`, with the message an author has to act on.
//!
//! Each refusal sits beside a control that is the same manifest with the one
//! offending line changed, and the control passes first. A refusal that also
//! refused the control would be a validator refusing everything, which these
//! tests would otherwise read as working.

use std::path::Path;

use deckjack_protocol::{
    HARD_DENY, Manifest, ManifestError, RESERVED_NAMESPACES, is_hard_denied, is_reserved_namespace,
};

fn parse(text: &str) -> Result<Manifest, ManifestError> {
    Manifest::parse(Path::new("app.toml"), text)
}

fn app(name: &str, rest: &str) -> String {
    format!("[app]\nname = \"{name}\"\nversion = \"1.0.0\"\nprotocol = 1\n{rest}")
}

fn ui(name: &str, invoke: &[&str], events: &[&str]) -> String {
    let list = |items: &[&str]| {
        items
            .iter()
            .map(|i| format!("{i:?}"))
            .collect::<Vec<_>>()
            .join(", ")
    };
    app(
        name,
        &format!(
            "\n[ui]\ninvoke = [{}]\nevents = [{}]\n",
            list(invoke),
            list(events)
        ),
    )
}

fn refused(text: &str) -> String {
    match parse(text) {
        Err(ManifestError::Invalid { message, .. }) => message,
        Err(ManifestError::Parse { source, .. }) => source.to_string(),
        other => panic!("expected a refusal, got {other:?}\n{text}"),
    }
}

// ---------------------------------------------------------------------------
// Reserved names
// ---------------------------------------------------------------------------

#[test]
fn a_core_namespace_is_refused_as_an_app_name_and_an_ordinary_name_is_not() {
    parse(&app("filer", "")).expect("the control: an ordinary name is fine");
    parse(&app("notes", "")).expect("the control: an ordinary name is fine");
    for name in [
        "files", "git", "editor", "terminal", "system", "shell", "auth",
    ] {
        let message = refused(&app(name, ""));
        assert!(
            message.contains("namespace the core itself uses"),
            "{name}: {message}"
        );
    }
}

#[test]
fn a_reserved_name_cannot_be_reached_as_a_command_prefix_either() {
    // A command must be named `<app>.<something>`, so with `files` refused as
    // a name, `files.read` cannot be declared by any app.
    let message = refused(&app(
        "filer",
        "\n[[commands]]\nid = \"files.read\"\nsummary = \"Read\"\nrisk = \"read\"\n",
    ));
    assert!(message.contains("own namespace"), "{message}");
    parse(&app(
        "filer",
        "\n[[commands]]\nid = \"filer.read\"\nsummary = \"Read\"\nrisk = \"read\"\n",
    ))
    .expect("the control: its own namespace");
}

#[test]
fn the_reserved_list_and_the_hard_deny_list_agree_with_their_predicates() {
    for ns in RESERVED_NAMESPACES {
        assert!(is_reserved_namespace(ns), "{ns}");
    }
    assert!(!is_reserved_namespace("filer"));
    // Every hard-denied namespace is also reserved: an app that could be
    // called `terminal` would own the namespace its window is barred from.
    for ns in HARD_DENY {
        assert!(
            is_reserved_namespace(ns),
            "{ns} is hard-denied but not reserved"
        );
    }
}

// ---------------------------------------------------------------------------
// The hard-deny list
// ---------------------------------------------------------------------------

#[test]
fn hard_deny_is_by_namespace_and_not_by_string_prefix() {
    for id in [
        "terminal",
        "terminal.open",
        "system.recovery_codes",
        "system.*",
        "shell.app.open",
        "auth.session.revoke",
    ] {
        assert!(is_hard_denied(id), "{id}");
    }
    for id in [
        "terminals.open",
        "files.read",
        "systemd.status",
        "filer.list",
    ] {
        assert!(!is_hard_denied(id), "{id}");
    }
}

#[test]
fn a_window_may_not_ask_for_any_hard_denied_command() {
    parse(&ui("filer", &["files.read"], &[])).expect("the control: a core file command");
    for id in [
        "terminal.open",
        "system.recovery_codes",
        "system.lockdown",
        "shell.app.open",
        "auth.passkey.remove",
    ] {
        let message = refused(&ui("filer", &[id], &[]));
        assert!(
            message.contains("no app's window may ever reach"),
            "{id}: {message}"
        );
    }
}

#[test]
fn a_window_may_not_listen_to_any_hard_denied_topic() {
    parse(&ui("filer", &[], &["files.changed"])).expect("the control: a file topic");
    for topic in [
        "auth.session",
        "auth.passkey.added",
        "system.lockdown",
        "terminal.output",
    ] {
        let message = refused(&ui("filer", &[], &[topic]));
        assert!(
            message.contains("ui.events") && message.contains("no app's window may ever reach"),
            "{topic}: {message}"
        );
    }
}

// ---------------------------------------------------------------------------
// Wildcards and shape
// ---------------------------------------------------------------------------

#[test]
fn a_wildcard_is_allowed_only_inside_the_apps_own_namespace() {
    parse(&ui("notes", &["notes.*"], &["notes.*"])).expect("the control: its own namespace");
    for entry in ["files.*", "git.*", "tasks.*"] {
        let message = refused(&ui("notes", &[entry], &[]));
        assert!(message.contains("wildcard outside"), "{entry}: {message}");
        let message = refused(&ui("notes", &[], &[entry]));
        assert!(message.contains("wildcard outside"), "{entry}: {message}");
    }
}

#[test]
fn an_entry_that_is_not_an_id_is_refused_with_what_to_write() {
    parse(&ui("filer", &["files.trash.list"], &[])).expect("the control: a deep id");
    for entry in [
        "files",
        "Files.read",
        "files..read",
        "files.*.read",
        "*",
        "files__x.read",
        "",
    ] {
        let message = refused(&ui("filer", &[entry], &[]));
        assert!(message.contains("is not a command"), "{entry:?}: {message}");
    }
}

#[test]
fn an_entry_listed_twice_is_refused() {
    let message = refused(&ui("filer", &["files.read", "files.read"], &[]));
    assert!(message.contains("twice"), "{message}");
}

#[test]
fn the_window_scope_lives_in_a_top_level_ui_table_only() {
    let at_top = ui("filer", &["files.read"], &[]);
    let manifest = parse(&at_top).expect("the control: [ui] at the top level");
    assert_eq!(manifest.needs.ui.invoke, ["files.read"]);

    let nested = app("filer", "\n[needs.ui]\ninvoke = [\"files.read\"]\n");
    let message = refused(&nested);
    assert!(message.contains("top-level [ui]"), "{message}");
}

#[test]
fn a_manifest_survives_a_round_trip_with_its_window_scope() {
    // `Manifest` is (de)serialized through the file's shape, so what a core
    // writes out (a report, an approval record) reads back as the same
    // manifest rather than losing `[ui]` or tripping over `[needs.ui]`.
    let manifest = parse(&ui(
        "filer",
        &["files.read", "files.list"],
        &["files.changed"],
    ))
    .unwrap();
    let json = serde_json::to_string(&manifest).unwrap();
    assert!(json.contains("\"ui\""), "{json}");
    let back: Manifest = serde_json::from_str(&json).unwrap();
    assert_eq!(back, manifest);
}

// ---------------------------------------------------------------------------
// Approval: `Needs::is_empty` and the fingerprint
// ---------------------------------------------------------------------------

#[test]
fn a_window_scope_is_something_to_approve() {
    let nothing = parse(&app("filer", "")).unwrap();
    assert!(
        nothing.needs.is_empty(),
        "the control: no [ui], nothing to ask"
    );

    let invokes = parse(&ui("filer", &["files.list"], &[])).unwrap();
    assert!(
        !invokes.needs.is_empty(),
        "calling a core command is a need (D1)"
    );

    let listens = parse(&ui("filer", &[], &["files.changed"])).unwrap();
    assert!(
        !listens.needs.is_empty(),
        "listening to file events is a need"
    );
}

#[test]
fn the_fingerprint_covers_the_window_scope_as_a_set() {
    let one = parse(&ui("filer", &["files.list"], &[])).unwrap();
    let two = parse(&ui("filer", &["files.list", "files.trash"], &[])).unwrap();
    let reordered = parse(&ui("filer", &["files.trash", "files.list"], &[])).unwrap();
    let listening = parse(&ui("filer", &["files.list"], &["files.changed"])).unwrap();

    assert_ne!(
        one.fingerprint(),
        two.fingerprint(),
        "a window that starts calling files.trash would run on the old approval"
    );
    assert_eq!(
        two.fingerprint(),
        reordered.fingerprint(),
        "reordering the list is not a new request"
    );
    assert_ne!(one.fingerprint(), listening.fingerprint());

    // The same holds for the topics it listens to.
    let topics = parse(&ui(
        "filer",
        &["files.list"],
        &["files.changed", "files.moved"],
    ))
    .unwrap();
    let topics_reordered = parse(&ui(
        "filer",
        &["files.list"],
        &["files.moved", "files.changed"],
    ))
    .unwrap();
    assert_eq!(
        topics.fingerprint(),
        topics_reordered.fingerprint(),
        "reordering [ui] events is not a new request"
    );
    assert_ne!(listening.fingerprint(), topics.fingerprint());
}

#[test]
fn an_app_without_a_ui_table_keeps_the_fingerprint_it_had() {
    // An empty [ui] and no [ui] are the same request, and both hash as the
    // manifest did before the table existed, so no installed app is asked to
    // re-approve for a key it does not use.
    let absent = parse(&app("notes", "")).unwrap();
    let empty = parse(&app("notes", "\n[ui]\n")).unwrap();
    assert_eq!(absent.fingerprint(), empty.fingerprint());
}

// ---------------------------------------------------------------------------
// The UI protocol: its namespace and the manifest's tier
// ---------------------------------------------------------------------------

#[test]
fn the_ui_protocol_namespace_is_reserved_and_hard_denied_to_windows() {
    // `ui.snapshot` reads any window's tree; a window reading another's is
    // what isolation exists to stop. The control is an app's own command.
    assert!(is_reserved_namespace("ui"));
    assert!(is_hard_denied("ui.snapshot"));
    assert!(!is_hard_denied("uix.snapshot"));
    parse(&ui("demo", &["demo.go"], &[])).expect("the control: the app's own command");
    let message = refused(&ui("demo", &["ui.snapshot"], &[]));
    assert!(message.contains("ui.snapshot"), "{message}");
    let message = refused(&app("ui", ""));
    assert!(
        message.contains("namespace the core itself uses"),
        "{message}"
    );
}

#[test]
fn a_manifest_may_declare_the_ui_protocol_tier_and_no_other() {
    let m = parse(&app("demo", "tier = 1\n")).expect("tier 1 is the UI protocol");
    assert_eq!(m.app.tier, Some(deckjack_protocol::TIER_UI_PROTOCOL));
    assert_eq!(parse(&app("demo", "")).unwrap().app.tier, None);
    let message = refused(&app("demo", "tier = 2\n"));
    assert!(message.contains("app.tier 2"), "{message}");
}

#[test]
fn the_tier_does_not_change_what_the_operator_approved() {
    // How a window is drawn is not a permission: switching an app to the UI
    // protocol must not ask the operator to approve it again.
    let page = parse(&app("demo", "")).unwrap();
    let described = parse(&app("demo", "tier = 1\n")).unwrap();
    assert_eq!(page.fingerprint(), described.fingerprint());
}
