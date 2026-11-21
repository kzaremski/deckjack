//! A reload or a network drop reattaches to the same shell, through the bus.
//!
//! The browser keeps nothing a reload would lose. The shell, its pty and a
//! bounded ring of its recent output stay on the server; a panel that comes
//! back asks `terminal.list` what its session owns and `terminal.read` from
//! cursor zero, and is handed the ring to replay. These tests are that
//! promise asked the way the panel asks it: commands on the product's own
//! bus, against a real shell.
//!
//! What the replay owes beyond the bytes is one bit: whether the ring had
//! already dropped the start of the stream. If it had, the first byte handed
//! over may sit in the middle of an escape sequence, and a client that writes
//! it into an emulator still holding the old screen paints garbage. So
//! `terminal.read` says `reset`, and the panel clears before it writes.

use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};

use deckjack_core::auth::SessionId;
use deckjack_core::command::{
    Admitted, Bus, CommandId, Grants, Issuer, Journal, JournalError, LogEntry, NoGrants, NodeId,
    OsEnv as BusEnv, Registry, Request,
};
use deckjack_core::terminal::SCROLLBACK_BYTES;
use deckjack_core::wiring;

const NODE: &str = "terminal-reattach";

struct Discard;

impl Journal for Discard {
    fn record(&self, _entry: LogEntry) -> Result<(), JournalError> {
        Ok(())
    }
}

struct Rig {
    bus: Arc<Bus>,
    me: SessionId,
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
    Rig {
        bus,
        me: SessionId::generate().expect("a session id"),
        _dir: dir,
    }
}

impl Rig {
    /// Dispatch as the one signed-in browser session, the way `BusDispatch`
    /// does for a socket frame.
    async fn call(&self, command: &str, params: Value) -> Value {
        let mut request = Request::new(
            CommandId::new(command.to_string()).expect("a command id"),
            params,
            Issuer::browser(self.me.clone()),
        );
        request.confirmed = true;
        match self.bus.dispatch(request) {
            Ok(Admitted::Running(running)) => {
                let (_rx, done) = running.split();
                done.await
                    .outcome
                    .unwrap_or_else(|e| panic!("{command} failed: {e}"))
            }
            Ok(other) => panic!("{command} did not run: {other:?}"),
            Err(refused) => panic!("{command} was refused: {refused:?}"),
        }
    }

    async fn open(&self) -> String {
        let opened = self
            .call("terminal.open", json!({ "rows": 24, "cols": 80 }))
            .await;
        opened["id"].as_str().expect("an id").to_string()
    }

    /// Follow from `cursor` until `needle` has been printed, as the panel's
    /// pump does. Returns the cursor after it.
    async fn follow_until(&self, id: &str, mut cursor: u64, needle: &str) -> u64 {
        let mut seen = String::new();
        let found = tokio::time::timeout(Duration::from_secs(20), async {
            loop {
                let chunk = self
                    .call("terminal.read", json!({ "id": id, "cursor": cursor }))
                    .await;
                cursor = chunk["cursor"].as_u64().expect("a cursor");
                seen.push_str(chunk["data"].as_str().unwrap_or_default());
                // Only the tail is kept: the overflow test prints far more
                // than is worth holding to search.
                if seen.len() > 4096 {
                    seen.drain(..seen.len() - 4096);
                }
                if seen.contains(needle) {
                    return cursor;
                }
                assert_ne!(chunk["exited"], true, "the shell exited first: {seen:?}");
            }
        })
        .await;
        found.unwrap_or_else(|_| panic!("the shell never printed {needle}: ...{seen:?}"))
    }
}

/// A reloaded panel finds its terminal by asking what it owns, and the
/// replay from zero holds what the shell printed before the reload.
#[tokio::test]
async fn a_reloaded_panel_finds_its_shell_and_replays_what_it_printed() {
    let rig = rig();
    let id = rig.open().await;
    // Arithmetic, so the shell's echo of the typed line cannot satisfy the
    // search: only the command's output contains the answer.
    rig.call(
        "terminal.write",
        json!({ "id": id, "data": "echo before-reload-$((6*7))\r" }),
    )
    .await;
    rig.follow_until(&id, 0, "before-reload-42").await;

    // The reload: the page and everything in it are gone. What it knows is
    // what the server tells it.
    let listed = rig.call("terminal.list", json!({})).await;
    let ids: Vec<&str> = listed
        .as_array()
        .expect("a list")
        .iter()
        .filter_map(|t| t["id"].as_str())
        .collect();
    assert_eq!(
        ids,
        vec![id.as_str()],
        "the shell did not survive the reload"
    );

    let replay = rig
        .call("terminal.read", json!({ "id": id, "cursor": 0 }))
        .await;
    assert!(
        replay["data"]
            .as_str()
            .is_some_and(|d| d.contains("before-reload-42")),
        "the replay lost what the shell printed before the reload: {replay}"
    );
    assert_eq!(
        replay["reset"], false,
        "nothing has fallen out of the ring, so the replay starts at the true \
         beginning and needs no reset: {replay}"
    );

    // And the same shell keeps going after it.
    rig.call(
        "terminal.write",
        json!({ "id": id, "data": "echo after-reload-$((7*7))\r" }),
    )
    .await;
    let cursor = replay["cursor"].as_u64().expect("a cursor");
    rig.follow_until(&id, cursor, "after-reload-49").await;
}

/// A replay that begins after the start of the stream says so.
#[tokio::test]
async fn a_replay_that_starts_mid_stream_tells_the_client_to_reset() {
    let rig = rig();
    let id = rig.open().await;
    let flood = SCROLLBACK_BYTES + SCROLLBACK_BYTES / 2;
    rig.call(
        "terminal.write",
        json!({
            "id": id,
            "data": format!("head -c {flood} /dev/zero | tr '\\0' x; echo; echo flood-$((2+3))\r"),
        }),
    )
    .await;
    let end = rig.follow_until(&id, 0, "flood-5").await;
    assert!(
        end > SCROLLBACK_BYTES as u64,
        "the shell printed {end} bytes, which does not overflow the ring, so \
         nothing here is under test"
    );

    let replay = rig
        .call("terminal.read", json!({ "id": id, "cursor": 0 }))
        .await;
    assert_eq!(
        replay["reset"], true,
        "the start of the stream is gone, and the client was not told: {}",
        replay["cursor"]
    );
    let data = replay["data"].as_str().expect("data");
    assert!(
        data.len() <= SCROLLBACK_BYTES,
        "the replay is bounded by the ring, got {} bytes",
        data.len()
    );
    assert!(
        data.contains("flood-5"),
        "the replay lost its newest output"
    );

    // A client that is caught up is not told to reset: the flag is about the
    // hole, not about the stream having ever overflowed.
    rig.call(
        "terminal.write",
        json!({ "id": id, "data": "echo tail-$((3+3))\r" }),
    )
    .await;
    let next = rig
        .call(
            "terminal.read",
            json!({ "id": id, "cursor": replay["cursor"] }),
        )
        .await;
    assert_eq!(
        next["reset"], false,
        "a caught-up read was told to reset: {next}"
    );
}
