//! The core's half, with the real compositor: `deckjack_core::stream`
//! launching a declared native app through the real launcher (Landlock and
//! seccomp), the app's window appearing in the real window list as a
//! server-made window, closing it asking the app, and the app's exit
//! tearing everything down.
//!
//! Skips, saying why, on a host that cannot confine or has no zenity.

mod common;

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use deckjack_core::apps::{Confinement, OsEnv};
use deckjack_core::command::Registry;
use deckjack_core::sandbox;
use deckjack_core::stream::{COMPOSITOR, NATIVE_DIR, StreamClose as _, Streams};
use deckjack_core::wiring;
use serde_json::Value;

const ZENITY: &str = r#"
[app]
name = "zenity"
version = "4"
protocol = 1
title = "Zenity"

[native]
exec = ["/usr/bin/zenity", "--entry", "--text=Name"]
"#;

struct Node {
    registry: Arc<Registry>,
    streams: Arc<Streams>,
    windows: Arc<wiring::Windows>,
    _dir: tempfile::TempDir,
}

fn node() -> Option<Node> {
    if !sandbox::availability().is_enforcing() {
        eprintln!("skipped: this host cannot confine");
        return None;
    }
    if !Path::new("/usr/bin/zenity").exists() {
        eprintln!("skipped: zenity is not installed");
        return None;
    }
    let dir = tempfile::tempdir().unwrap();
    let install = dir.path().join("install");
    let work = dir.path().join("work");
    let native = install.join(NATIVE_DIR);
    std::fs::create_dir_all(&native).unwrap();
    std::fs::create_dir_all(&work).unwrap();
    common::install_compositor(&native.join(COMPOSITOR));
    std::fs::write(native.join("zenity.toml"), ZENITY).unwrap();
    let registry = Arc::new(Registry::new());
    let windows = wiring::register_windows(&registry, "node".into()).unwrap();
    let streams = Streams::new(
        install,
        work,
        Confinement::Enforced,
        Arc::new(OsEnv::new()),
        Arc::clone(&windows) as _,
    );
    windows.attach_streams(Arc::clone(&streams) as _);
    Some(Node {
        registry,
        streams,
        windows,
        _dir: dir,
    })
}

/// Wait until the window list satisfies `ok`.
async fn until(windows: &wiring::Windows, what: &str, ok: impl Fn(&[Value]) -> bool) -> Vec<Value> {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let list = windows.wire().as_array().cloned().unwrap_or_default();
        if ok(&list) {
            return list;
        }
        assert!(Instant::now() < deadline, "{what}; the list is {list:?}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_native_app_becomes_a_server_window_and_closing_it_asks_the_app() {
    let Some(node) = node() else { return };
    let instance = node
        .streams
        .launch("zenity")
        .await
        .expect("zenity launches");
    let list = until(&node.windows, "no streamed window appeared", |l| {
        !l.is_empty()
    })
    .await;
    let window = &list[0];
    assert_eq!(window["app"], "stream");
    assert_eq!(window["stream"]["app"], "zenity");
    assert_eq!(window["stream"]["kind"], "toplevel");
    assert!(window["stream"]["width"].as_u64().unwrap() > 100);
    let id = window["id"].as_str().unwrap().to_string();
    assert_eq!(id, format!("stream-{instance}-1"));
    assert_eq!(node.streams.list()[0]["windows"][0], id.as_str());

    // The close button: the app is asked, and the window goes when the app
    // unmaps it (zenity exits), not before.
    assert!(node.streams.close_requested(&id));
    until(&node.windows, "the window outlived its app", |l| {
        l.is_empty()
    })
    .await;
    let deadline = Instant::now() + Duration::from_secs(10);
    while node
        .streams
        .list()
        .as_array()
        .is_some_and(|a| !a.is_empty())
    {
        assert!(Instant::now() < deadline, "the instance outlived its app");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    // Not a streamed window any more, so not the stream registry's.
    assert!(!node.streams.close_requested(&id));
}

/// The shell's close button is `shell.window.close` on the bus. For a
/// streamed window it asks the app and leaves the window in the list: the
/// window goes when the app unmaps it, not when the command returns.
#[tokio::test(flavor = "multi_thread")]
async fn the_window_close_command_asks_the_app_instead_of_removing_the_window() {
    use deckjack_core::command::{
        Admitted, Bus, CommandId, Issuer, NodeId, OsEnv as BusEnv, Request,
    };
    let Some(node) = node() else { return };
    let bus = Bus::new(
        Arc::clone(&node.registry),
        NodeId::new("node"),
        Arc::new(BusEnv),
    );
    node.streams
        .launch("zenity")
        .await
        .expect("zenity launches");
    let list = until(&node.windows, "no streamed window appeared", |l| {
        !l.is_empty()
    })
    .await;
    let id = list[0]["id"].as_str().unwrap().to_string();
    let request = Request::new(
        CommandId::new("shell.window.close").unwrap(),
        serde_json::json!({ "id": id }),
        Issuer::operator(),
    );
    let Ok(Admitted::Running(running)) = bus.dispatch(request) else {
        panic!("shell.window.close was not admitted");
    };
    let (_, done) = running.split();
    done.await;
    assert_eq!(
        node.windows.wire().as_array().unwrap().len(),
        1,
        "the window was removed before the app said anything"
    );
    until(&node.windows, "the app did not close", |l| l.is_empty()).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn stopping_a_native_app_takes_its_windows_with_it() {
    let Some(node) = node() else { return };
    let instance = node
        .streams
        .launch("zenity")
        .await
        .expect("zenity launches");
    until(&node.windows, "no streamed window appeared", |l| {
        !l.is_empty()
    })
    .await;
    node.streams
        .stop(instance)
        .expect("a running instance stops");
    until(&node.windows, "the window outlived a stop", |l| {
        l.is_empty()
    })
    .await;
    // The instance itself is gone once its link has closed, and a second
    // stop says so rather than signaling a pid that may be reused.
    let deadline = Instant::now() + Duration::from_secs(10);
    while !node.streams.list().as_array().unwrap().is_empty() {
        assert!(Instant::now() < deadline, "the instance outlived a stop");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(node.streams.stop(instance).is_err());
}

#[tokio::test(flavor = "multi_thread")]
async fn an_undeclared_app_is_refused_by_name() {
    let Some(node) = node() else { return };
    let err = node.streams.launch("gimp").await.unwrap_err();
    assert!(err.contains("no native app \"gimp\""), "{err}");
}
