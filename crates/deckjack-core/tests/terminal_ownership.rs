//! One session's shell is not another session's shell.
//!
//! `terminal.open` has always recorded who opened a pty — `Opener::Client`
//! carries the session hash, and `close_for_session` has always used it to
//! reap a terminal when its session ends. Every other terminal command threw
//! the caller away: `read`, `write`, `resize` and `close` took `_cx` and
//! resolved the id through `Registry::attach`, which looks a terminal up by
//! id and nothing else. `terminal.list` is read-only and returned every
//! terminal on the node, and an `Info` carries the id — so learning another
//! session's terminal id took one read-only call, and typing into its shell
//! took one more.
//!
//! That made the session boundary decorative for the single most dangerous
//! surface the product has. These tests are the boundary.

use std::sync::Arc;

use serde_json::{Value, json};

use deckjack_core::auth::SessionId;
use deckjack_core::command::{
    Admitted, Bus, CommandId, Grants, Issuer, Journal, JournalError, LogEntry, NoGrants, NodeId,
    OsEnv as BusEnv, Registry, Request, RiskLevel,
};
use deckjack_core::wiring;

const NODE: &str = "terminal-ownership";

struct Discard;

impl Journal for Discard {
    fn record(&self, _entry: LogEntry) -> Result<(), JournalError> {
        Ok(())
    }
}

struct Rig {
    bus: Arc<Bus>,
    _dir: tempfile::TempDir,
}

fn rig() -> Rig {
    let dir = tempfile::tempdir().unwrap();
    let registry = Arc::new(Registry::new());
    wiring::register_terminal(&registry, dir.path().to_path_buf()).expect("terminal commands");

    let bus = Arc::new(
        Bus::new(Arc::clone(&registry), NodeId::new(NODE), Arc::new(BusEnv))
            .with_journal(Arc::new(Discard) as Arc<dyn Journal>)
            .with_grants(Arc::new(NoGrants) as Arc<dyn Grants>),
    );
    Rig { bus, _dir: dir }
}

impl Rig {
    /// Dispatch as one signed-in browser session.
    ///
    /// `confirmed` is true because these commands are `Write` and the
    /// operator confirming is a different axis from whose terminal it is —
    /// conflating the two is how a test ends up asserting a confirmation
    /// prompt when it meant to assert an ownership refusal.
    async fn call(&self, who: &SessionId, command: &str, params: Value) -> Result<Value, String> {
        self.as_issuer(Issuer::browser(who.clone()), command, params)
            .await
    }

    async fn as_issuer(
        &self,
        issuer: Issuer,
        command: &str,
        params: Value,
    ) -> Result<Value, String> {
        let mut request = Request::new(
            CommandId::new(command.to_string()).expect("a command id"),
            params,
            issuer,
        );
        request.confirmed = true;
        match self.bus.dispatch(request) {
            Ok(Admitted::Running(running)) => {
                // `rx` is held rather than dropped: these commands report no
                // progress, but a dropped receiver is how a future one would
                // start failing to send for a reason unrelated to the test.
                let (_rx, done) = running.split();
                done.await.outcome.map_err(|e| e.to_string())
            }
            Ok(other) => Err(format!("{other:?}")),
            Err(refused) => Err(format!("{refused:?}")),
        }
    }

    async fn open(&self, who: &SessionId) -> String {
        let opened = self
            .call(who, "terminal.open", json!({ "rows": 24, "cols": 80 }))
            .await
            .expect("the owner may open a terminal");
        opened["id"].as_str().expect("an id").to_string()
    }
}

fn two_sessions() -> (SessionId, SessionId) {
    let a = SessionId::generate().expect("a session id");
    let b = SessionId::generate().expect("a session id");
    assert_ne!(a, b);
    (a, b)
}

#[tokio::test]
async fn a_stranger_cannot_read_write_resize_or_close_another_sessions_terminal() {
    let rig = rig();
    let (mine, stranger) = two_sessions();
    let id = rig.open(&mine).await;

    // Every one of these succeeded before the ownership check existed.
    for (command, params) in [
        ("terminal.read", json!({ "id": id, "cursor": 0 })),
        ("terminal.write", json!({ "id": id, "data": "whoami\n" })),
        ("terminal.resize", json!({ "id": id, "rows": 1, "cols": 1 })),
        ("terminal.close", json!({ "id": id })),
    ] {
        let outcome = rig.call(&stranger, command, params).await;
        assert!(
            outcome.is_err(),
            "{command} reached another session's terminal: {outcome:?}"
        );
    }

    // And the owner still has a working, correctly sized, un-closed shell:
    // a check that refused everybody would pass the assertions above.
    rig.call(
        &mine,
        "terminal.resize",
        json!({ "id": id, "rows": 30, "cols": 100 }),
    )
    .await
    .expect("the owner may resize");
    let listed = rig
        .call(&mine, "terminal.list", json!({}))
        .await
        .expect("the owner may list");
    assert_eq!(listed[0]["size"]["rows"], json!(30));
}

#[tokio::test]
async fn the_refusal_does_not_reveal_whether_the_terminal_exists() {
    // Otherwise a stranger sweeps ids and learns exactly which are live,
    // which is most of what the unfiltered list used to give away.
    let rig = rig();
    let (mine, stranger) = two_sessions();
    let real = rig.open(&mine).await;

    let denied = rig
        .call(
            &stranger,
            "terminal.read",
            json!({ "id": real, "cursor": 0 }),
        )
        .await
        .expect_err("a stranger is refused");
    let absent = rig
        .call(
            &stranger,
            "terminal.read",
            json!({ "id": "t-does-not-exist", "cursor": 0 }),
        )
        .await
        .expect_err("an unknown id is refused");

    assert_eq!(
        denied.replace(&real, "ID"),
        absent.replace("t-does-not-exist", "ID"),
        "a terminal that exists is refused differently from one that does not, \
         so the refusal is an oracle for which ids are live"
    );
}

#[tokio::test]
async fn a_session_lists_only_its_own_terminals() {
    let rig = rig();
    let (mine, theirs) = two_sessions();
    let my_id = rig.open(&mine).await;
    let their_id = rig.open(&theirs).await;

    let ids = |listed: &Value| -> Vec<String> {
        listed
            .as_array()
            .expect("an array")
            .iter()
            .map(|i| i["id"].as_str().unwrap().to_string())
            .collect()
    };

    let mine_listed = rig.call(&mine, "terminal.list", json!({})).await.unwrap();
    assert_eq!(ids(&mine_listed), vec![my_id.clone()]);

    let theirs_listed = rig.call(&theirs, "terminal.list", json!({})).await.unwrap();
    assert_eq!(ids(&theirs_listed), vec![their_id.clone()]);

    // The id is the whole handle to a shell, so the list leaking it was the
    // disclosure, not just untidy reporting.
    assert!(
        !ids(&mine_listed).contains(&their_id),
        "one session saw another's terminal id"
    );
}

#[tokio::test]
async fn a_browser_session_cannot_reach_a_terminal_the_host_opened() {
    // `Opener::Host` is what a dispatch with no browser behind it gets — the
    // CLI, and (today) an agent that was mistakenly granted `terminal.*`.
    // Those terminals are never revoked by a session ending, so a browser
    // session reaching one would be a shell nothing reaps.
    let rig = rig();
    let opened = rig
        .as_issuer(Issuer::Cli, "terminal.open", json!({}))
        .await
        .expect("the CLI may open a terminal");
    let id = opened["id"].as_str().unwrap().to_string();

    let (browser, _) = two_sessions();
    let outcome = rig
        .call(&browser, "terminal.write", json!({ "id": id, "data": "x" }))
        .await;
    assert!(
        outcome.is_err(),
        "a browser session wrote into a host terminal: {outcome:?}"
    );
}

/// Risk is about whether the operator confirms; ownership is about whose
/// shell it is. Pinned because collapsing them is the mistake that would
/// make the check above look redundant.
#[test]
fn the_terminal_commands_still_declare_the_risk_they_did_before() {
    let registry = Registry::new();
    let dir = tempfile::tempdir().unwrap();
    wiring::register_terminal(&registry, dir.path().to_path_buf()).unwrap();

    let risk = |id: &str| {
        registry
            .list(None)
            .into_iter()
            .find(|c| c.spec.id().as_str() == id)
            .unwrap_or_else(|| panic!("{id} is not registered"))
            .spec
            .risk()
    };

    // `write` is called on every keystroke: anything above Write would ask
    // the operator to confirm each keypress.
    assert_eq!(risk("terminal.write"), RiskLevel::Write);
    assert_eq!(risk("terminal.read"), RiskLevel::ReadOnly);
    assert_eq!(risk("terminal.list"), RiskLevel::ReadOnly);
}
