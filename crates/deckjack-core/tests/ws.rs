//! The bus transport, exercised through its public surface only.
//!
//! Two kinds of test live here, and they catch different things.
//!
//! **Frame shapes, asserted as exact JSON.** `web/src/protocol.ts` mirrors
//! these frames by hand, so a renamed field or a changed tag does not fail a
//! build anywhere — it fails in a phone browser, silently, as a desktop that
//! will not talk to its own server. Comparing a serialized frame against a
//! literal `json!` is the only check that notices.
//!
//! **A real socket against a real router.** Everything interesting in this
//! module lives in private functions (`handshake`, `pump`, `handle`) and is
//! reachable only over the wire, which is also the only place the invariants
//! are actually promised: hello first, one response per invoke, a reused
//! correlation id refused. So these tests run `ws::router` on a loopback
//! listener and speak WebSocket to it with the minimal client at the bottom
//! of this file. No new dependency: the handshake is four headers and the
//! frames are a two-byte header plus a mask.
//!
//! Nothing here sleeps. The clock is injected (`TestClock`), elapsed time is
//! advanced by the command under test, and where a test has to observe that
//! something did *not* arrive it sends a later call as a fence and asserts
//! that its response is the very next frame.

use std::future::Future;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use deckjack_core::auth::state::{AuthState, Limits, LoginOutcome, Now};
use deckjack_core::auth::{Code, SessionId, session};
use deckjack_core::http;
use deckjack_core::store::Store;
use deckjack_core::ws::{
    self, Call, ClientFrame, CommandError, CommandId, ConnectionId, CorrelationId, Dispatch,
    Dispatched, Hub, IdError, Issuer, Outcome, Progress, ProgressSink, ServerFrame, Settings,
    WsCtx, close,
};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::oneshot;

// ---------------------------------------------------------------------------
// Frame shapes: what the hand-written TypeScript client parses
// ---------------------------------------------------------------------------

#[test]
fn a_welcome_frame_is_exactly_what_the_browser_parses() {
    let frame = ServerFrame::Welcome {
        protocol: ws::PROTOCOL_VERSION,
        connection: ConnectionId(7),
        node_id: "node-abc".into(),
        host_name: "pocket".into(),
    };
    assert_eq!(
        serde_json::to_value(&frame).unwrap(),
        json!({
            "type": "welcome",
            "protocol": 1,
            "connection": 7,
            "node_id": "node-abc",
            "host_name": "pocket",
        }),
        "the frame is tagged `type` and the connection id is a bare number; \
         protocol.ts declares `connection: number`"
    );
}

#[test]
fn a_progress_frame_omits_every_field_the_command_did_not_set() {
    // `Progress` is all-optional in protocol.ts. A command that only knows it
    // is alive must not send `"fraction": null`, which is neither `number` nor
    // `undefined` there.
    let frame = ServerFrame::Progress {
        id: correlation("c1"),
        seq: 0,
        progress: Progress::message("unpacking"),
    };
    assert_eq!(
        serde_json::to_value(&frame).unwrap(),
        json!({
            "type": "progress",
            "id": "c1",
            "seq": 0,
            "progress": { "message": "unpacking" },
        })
    );

    let empty = ServerFrame::Progress {
        id: correlation("c1"),
        seq: 1,
        progress: Progress::default(),
    };
    assert_eq!(
        serde_json::to_value(&empty).unwrap(),
        json!({ "type": "progress", "id": "c1", "seq": 1, "progress": {} })
    );

    let lines = ServerFrame::Progress {
        id: correlation("c1"),
        seq: 2,
        progress: Progress::line("warning: nothing to do"),
    };
    assert_eq!(
        serde_json::to_value(&lines).unwrap(),
        json!({
            "type": "progress",
            "id": "c1",
            "seq": 2,
            "progress": { "log": ["warning: nothing to do"] },
        })
    );
}

#[test]
fn a_response_tags_its_outcome_with_status_not_type() {
    // Two internally-tagged enums nest here: `type` for the frame and
    // `status` for the outcome. Getting that wrong is exactly the class of
    // defect that cost the first integration pass.
    let ok = ServerFrame::Response {
        id: correlation("c2"),
        outcome: Outcome::ok(json!({ "rows": 2 })),
        elapsed_ms: 12,
    };
    assert_eq!(
        serde_json::to_value(&ok).unwrap(),
        json!({
            "type": "response",
            "id": "c2",
            "outcome": { "status": "ok", "value": { "rows": 2 } },
            "elapsed_ms": 12,
        })
    );

    let err = ServerFrame::Response {
        id: correlation("c2"),
        outcome: Outcome::error(CommandError::new("files.denied", "outside the working dir")),
        elapsed_ms: 0,
    };
    assert_eq!(
        serde_json::to_value(&err).unwrap(),
        json!({
            "type": "response",
            "id": "c2",
            "outcome": {
                "status": "error",
                "error": { "code": "files.denied", "message": "outside the working dir" },
            },
            "elapsed_ms": 0,
        }),
        "`detail` is absent, not null: protocol.ts declares it optional"
    );

    let detailed = CommandError::new("files.denied", "outside the working dir")
        .with_detail(json!({ "path": "/etc/passwd" }));
    assert_eq!(
        serde_json::to_value(&detailed).unwrap(),
        json!({
            "code": "files.denied",
            "message": "outside the working dir",
            "detail": { "path": "/etc/passwd" },
        })
    );
}

#[test]
fn an_event_frame_carries_topic_payload_and_timestamp() {
    let frame = ServerFrame::Event {
        topic: "windows.changed".into(),
        payload: json!({ "count": 3 }),
        at_ms: 1_800_000_000_000,
    };
    assert_eq!(
        serde_json::to_value(&frame).unwrap(),
        json!({
            "type": "event",
            "topic": "windows.changed",
            "payload": { "count": 3 },
            "at_ms": 1_800_000_000_000u64,
        })
    );
}

#[test]
fn the_three_client_frames_deserialize_from_what_the_shell_sends() {
    // Copied from the shapes `web/src/stores/bus.ts` builds.
    let hello: ClientFrame = serde_json::from_value(
        json!({ "type": "hello", "protocol": 1, "client": "deckjack-shell" }),
    )
    .unwrap();
    match hello {
        ClientFrame::Hello {
            protocol, client, ..
        } => {
            assert_eq!(protocol, 1);
            assert_eq!(client.as_deref(), Some("deckjack-shell"));
        }
        other => panic!("expected hello, got {other:?}"),
    }

    let invoke: ClientFrame = serde_json::from_value(json!({
        "type": "invoke",
        "id": "c1",
        "command": "files.list",
        "params": { "path": "." },
    }))
    .unwrap();
    match invoke {
        ClientFrame::Invoke {
            id,
            command,
            params,
            confirmed,
            ..
        } => {
            assert_eq!(id.as_str(), "c1");
            assert_eq!(command.as_str(), "files.list");
            assert_eq!(params, json!({ "path": "." }));
            assert!(confirmed.is_none());
        }
        other => panic!("expected invoke, got {other:?}"),
    }

    let confirm: ClientFrame = serde_json::from_value(json!({
        "type": "confirm",
        "id": "c2",
        "ticket": "00112233445566778899aabbccddeeff",
        "command": "files.trash.empty",
        "params": {},
    }))
    .unwrap();
    match confirm {
        ClientFrame::Confirm {
            id,
            ticket,
            command,
            params,
            as_app,
        } => {
            assert_eq!(id.as_str(), "c2");
            assert_eq!(ticket, "00112233445566778899aabbccddeeff");
            assert_eq!(command.as_str(), "files.trash.empty");
            assert_eq!(params, json!({}));
            assert_eq!(as_app, None, "a confirm with no as_app is the shell's");
        }
        other => panic!("expected confirm, got {other:?}"),
    }

    let cancel: ClientFrame =
        serde_json::from_value(json!({ "type": "cancel", "id": "c1" })).unwrap();
    assert!(matches!(cancel, ClientFrame::Cancel { .. }));
}

#[test]
fn an_invoke_without_params_means_null_and_asserts_no_confirmation() {
    // protocol.ts marks params optional, so the shell leaves them out for a
    // no-argument command.
    let invoke: ClientFrame =
        serde_json::from_value(json!({ "type": "invoke", "id": "c1", "command": "node.info" }))
            .unwrap();
    match invoke {
        ClientFrame::Invoke {
            params, confirmed, ..
        } => {
            assert_eq!(params, Value::Null);
            assert!(
                confirmed.is_none(),
                "nothing was sent, so nothing is refused"
            );
        }
        other => panic!("expected invoke, got {other:?}"),
    }
}

#[test]
fn an_unknown_field_is_ignored_once_the_handshake_has_agreed() {
    // The module's contract: the version is the compatibility gate, so a
    // newer client's extra field is not fatal after a matching hello.
    let invoke: ClientFrame = serde_json::from_value(json!({
        "type": "invoke",
        "id": "c1",
        "command": "node.info",
        "deadline_ms": 5000,
    }))
    .unwrap();
    assert!(matches!(invoke, ClientFrame::Invoke { .. }));
}

#[test]
fn an_unknown_frame_type_is_refused_rather_than_ignored() {
    // Silently ignoring a frame the server does not understand would leave
    // the client waiting for an answer that is never coming.
    assert!(
        serde_json::from_value::<ClientFrame>(json!({ "type": "resume", "id": "c1" })).is_err()
    );
    assert!(serde_json::from_value::<ClientFrame>(json!({ "id": "c1" })).is_err());
}

#[test]
fn a_computed_fraction_never_leaves_the_range_the_client_can_draw() {
    // `fraction` is documented as 0.0 to 1.0 and the shell renders it as a
    // bar width, so a count that overshoots its total — a resumed download,
    // a directory that grew during the walk — must not become a 250% bar.
    // This is about the constructor only: a command that fills the field in
    // by hand can still write anything an f32 can hold.
    assert_eq!(Progress::fraction(0, 10).fraction, Some(0.0));
    assert_eq!(Progress::fraction(5, 10).fraction, Some(0.5));
    assert_eq!(Progress::fraction(10, 10).fraction, Some(1.0));
    // Nothing to do is done, not a division by zero.
    assert_eq!(Progress::fraction(0, 0).fraction, Some(1.0));
    assert_eq!(
        Progress::fraction(11, 10).fraction,
        Some(1.0),
        "an overshooting count clamps instead of reporting more than everything"
    );
    let serialized = serde_json::to_value(Progress::fraction(11, 10)).unwrap();
    assert_eq!(serialized, json!({ "fraction": 1.0 }));
}

// ---------------------------------------------------------------------------
// Identifiers off the wire
// ---------------------------------------------------------------------------

#[test]
fn an_oversized_correlation_id_is_refused_not_truncated() {
    // It is a map key held for the life of the call, so an unbounded one is
    // a memory cost a client should not be able to choose. Truncating would
    // be worse than refusing: two long ids would collide into one.
    let long = "c".repeat(CorrelationId::MAX_LEN + 1);
    match CorrelationId::try_from(long.clone()) {
        Err(IdError::TooLong { len, max, .. }) => {
            assert_eq!(len, CorrelationId::MAX_LEN + 1);
            assert_eq!(max, CorrelationId::MAX_LEN);
        }
        other => panic!("expected TooLong, got {other:?}"),
    }
    assert!(CorrelationId::try_from("c".repeat(CorrelationId::MAX_LEN)).is_ok());

    // And the refusal survives the frame: an invoke carrying it does not
    // deserialize, so it never reaches the bus.
    assert!(
        serde_json::from_value::<ClientFrame>(
            json!({ "type": "invoke", "id": long, "command": "node.info" })
        )
        .is_err()
    );
}

#[test]
fn a_correlation_id_with_a_control_character_is_refused_because_it_reaches_the_log() {
    for hostile in ["c1\n2026-01-01 INFO forged line", "c1\u{0}", "c1\t2"] {
        let err = CorrelationId::try_from(hostile.to_string())
            .expect_err("a control character in a logged id is a log-injection hole");
        assert!(matches!(err, IdError::BadChar { .. }), "got {err:?}");
    }
    // Printable ASCII, including a space, is a client's own business.
    assert!(CorrelationId::try_from("call 1".to_string()).is_ok());
    assert!(CorrelationId::try_from(String::new()).is_err());
}

#[test]
fn a_command_id_is_restricted_to_the_dotted_namespace_alphabet() {
    // The grant system matches prefixes (`files.read.*`), so case or odd
    // punctuation in a command id would make two spellings of one command.
    for ok in ["files.list", "terminal.write", "node_info", "app-2.open"] {
        assert!(
            CommandId::try_from(ok.to_string()).is_ok(),
            "{ok} should be accepted"
        );
    }
    for bad in [
        "Files.List",
        "files list",
        "files/list",
        "files.liste\u{301}",
    ] {
        assert!(
            matches!(
                CommandId::try_from(bad.to_string()),
                Err(IdError::BadChar { .. })
            ),
            "{bad} should be refused"
        );
    }
    assert!(matches!(
        CommandId::try_from(String::new()),
        Err(IdError::Empty { .. })
    ));
    assert!(matches!(
        CommandId::try_from("a".repeat(CommandId::MAX_LEN + 1)),
        Err(IdError::TooLong { .. })
    ));
}

#[test]
fn an_issuers_debug_never_reveals_the_session_id() {
    // Every attribution line in the bus prints the issuer. The session is a
    // bearer credential: a log that quotes it hands out a signed-in session
    // to whoever reads the log, for as long as the backups live.
    let session = SessionId::generate().unwrap();
    let issuer = Issuer::new(
        ConnectionId(3),
        session.clone(),
        IpAddr::V4(Ipv4Addr::LOCALHOST),
    );
    let printed = format!("{issuer:?}");
    assert!(
        !printed.contains(&session.encode()),
        "Issuer's Debug leaked the session id: {printed}"
    );
    assert!(
        printed.contains("conn-3") || printed.contains("ConnectionId(3)"),
        "it should still be useful for attribution: {printed}"
    );
    // The one accessor still hands it over, because the bus needs it to tag
    // what a session owns.
    assert_eq!(issuer.session().encode(), session.encode());
}

// ---------------------------------------------------------------------------
// Before the upgrade
// ---------------------------------------------------------------------------

#[tokio::test]
async fn an_upgrade_without_a_session_is_refused_before_a_socket_exists() {
    let server = Server::start(Settings::default(), default_bus()).await;

    // No cookie at all.
    assert_eq!(refused(server.addr, &[]).await, 401);

    // A cookie that decodes to nothing real is no better than none. The
    // refusal is an HTTP status, not an upgrade followed by a close code: a
    // browser cannot read a close code for a socket it never opened.
    let bogus = SessionId::generate().unwrap();
    let cookie = format!("{}={}", session::COOKIE_NAME, bogus.encode());
    assert_eq!(refused(server.addr, &[("Cookie", &cookie)]).await, 401);
}

#[tokio::test]
async fn a_cross_origin_upgrade_is_refused_even_with_a_valid_cookie() {
    // The same-origin policy does not cover WebSocket, so this check is the
    // whole defense: without it any page the browser has open could open a
    // socket to the bus and ride the session cookie.
    let server = Server::start(Settings::default(), default_bus()).await;

    assert_eq!(
        refused(
            server.addr,
            &[
                ("Cookie", &server.cookie),
                ("Origin", "https://evil.example"),
            ],
        )
        .await,
        403
    );

    // A sandboxed iframe sends the literal string "null" as its origin. It
    // names no host, so it cannot match one.
    assert_eq!(
        refused(
            server.addr,
            &[("Cookie", &server.cookie), ("Origin", "null")],
        )
        .await,
        403
    );

    // The real desktop's own origin is accepted.
    let origin = format!("http://{}", server.addr);
    let mut ws = Client::connect(
        server.addr,
        &[("Cookie", &server.cookie), ("Origin", &origin)],
    )
    .await
    .expect("same-origin upgrade refused");
    ws.hello(1).await;
    assert_eq!(ws.next_frame().await["type"], "welcome");
}

// ---------------------------------------------------------------------------
// The handshake
// ---------------------------------------------------------------------------

#[tokio::test]
async fn hello_must_be_the_first_frame() {
    let server = Server::start(Settings::default(), default_bus()).await;
    let mut ws = server.open().await;

    // A client that skips the handshake and gets its command run anyway is
    // how an old client silently runs something it did not mean.
    ws.send_json(&json!({ "type": "invoke", "id": "c1", "command": "test.ok" }))
        .await;
    let (code, reason) = ws.expect_close().await;
    assert_eq!(code, close::BAD_FRAME);
    assert!(
        reason.contains("hello"),
        "the reason has to name what was expected: {reason}"
    );
    assert_eq!(
        server.bus.seen().len(),
        0,
        "nothing may be dispatched before the version is agreed"
    );
}

#[tokio::test]
async fn a_protocol_mismatch_is_refused_with_both_version_numbers() {
    let server = Server::start(Settings::default(), default_bus()).await;

    for offered in [0, 2, 99] {
        let mut ws = server.open().await;
        ws.hello(offered).await;
        let (code, reason) = ws.expect_close().await;
        assert_eq!(
            code,
            close::PROTOCOL,
            "a mismatch is its own outcome, not a bad frame"
        );
        assert!(
            reason.contains(&offered.to_string()) && reason.contains("1"),
            "the reason must name what was offered and what is spoken: {reason}"
        );
    }
}

#[tokio::test]
async fn a_second_hello_is_a_confused_client_not_a_renegotiation() {
    let server = Server::start(Settings::default(), default_bus()).await;
    let mut ws = server.open().await;
    ws.hello(1).await;
    assert_eq!(ws.next_frame().await["type"], "welcome");

    ws.hello(1).await;
    let (code, reason) = ws.expect_close().await;
    assert_eq!(code, close::BAD_FRAME);
    assert!(reason.contains("already"), "{reason}");
}

#[tokio::test]
async fn frames_are_json_text_and_nothing_else() {
    let server = Server::start(Settings::default(), default_bus()).await;

    // Binary, before the handshake.
    let mut ws = server.open().await;
    ws.send_binary(b"\x00\x01\x02").await;
    let (code, reason) = ws.expect_close().await;
    assert_eq!(code, close::BAD_FRAME);
    assert!(reason.contains("binary"), "{reason}");

    // Binary, after it.
    let mut ws = server.open().await;
    ws.hello(1).await;
    ws.next_frame().await;
    ws.send_binary(b"\x00").await;
    assert_eq!(ws.expect_close().await.0, close::BAD_FRAME);

    // Text that is not JSON at all.
    let mut ws = server.open().await;
    ws.hello(1).await;
    ws.next_frame().await;
    ws.send_text("{not json").await;
    assert_eq!(ws.expect_close().await.0, close::BAD_FRAME);
}

#[tokio::test]
async fn a_frame_over_the_size_limit_closes_the_socket_instead_of_buffering_it() {
    // Params are JSON, not payloads; file contents move over the file API.
    // The limit is what stops one connection costing megabytes of buffer.
    let settings = Settings {
        max_frame_bytes: 4 * 1024,
        ..Settings::default()
    };
    let server = Server::start(settings, default_bus()).await;
    let mut ws = server.open().await;
    ws.hello(1).await;
    ws.next_frame().await;

    let huge = json!({
        "type": "invoke",
        "id": "c1",
        "command": "test.ok",
        "params": { "blob": "x".repeat(16 * 1024) },
    });
    ws.send_json(&huge).await;

    // However it ends — a close frame or a dropped connection — the one thing
    // that must not happen is the command running.
    // Either end is acceptable — a close frame, or a transport that simply
    // gave up — but the command must not have run.
    match ws.recv().await {
        Incoming::Close(..) | Incoming::Eof => {}
        Incoming::Text(t) => panic!("an oversized frame was answered: {t}"),
    }
    assert_eq!(server.bus.seen().len(), 0);
}

#[tokio::test]
async fn the_welcome_frame_names_the_node_that_is_hosting() {
    // The shell shows which machine it is on without a second request.
    let server = Server::start(Settings::default(), default_bus()).await;
    let mut ws = server.open().await;
    ws.hello(1).await;
    let welcome = ws.next_frame().await;
    assert_eq!(
        welcome,
        json!({
            "type": "welcome",
            "protocol": 1,
            "connection": 1,
            "node_id": "node-under-test",
            "host_name": "host-under-test",
        })
    );
}

// ---------------------------------------------------------------------------
// Calls
// ---------------------------------------------------------------------------

#[tokio::test]
async fn exactly_one_response_arrives_per_accepted_invoke() {
    let server = Server::start(Settings::default(), default_bus()).await;
    let mut ws = server.open().await;
    ws.hello(1).await;
    ws.next_frame().await;

    ws.invoke("c1", "test.ok", json!({ "n": 1 })).await;
    let first = ws.next_frame().await;
    assert_eq!(first["type"], "response");
    assert_eq!(first["id"], "c1");
    assert_eq!(first["outcome"]["status"], "ok");
    assert_eq!(first["outcome"]["value"], json!({ "echo": { "n": 1 } }));

    // A second call is the fence: if the first had produced two responses,
    // the next frame would be the duplicate rather than c2's answer.
    ws.invoke("c2", "test.error", Value::Null).await;
    let second = ws.next_frame().await;
    assert_eq!(second["id"], "c2");
    assert_eq!(second["outcome"]["status"], "error");
    assert_eq!(second["outcome"]["error"]["code"], "test.nope");
}

#[tokio::test]
async fn a_correlation_id_reused_while_in_flight_is_refused_rather_than_answered_ambiguously() {
    let (release, held) = oneshot::channel::<()>();
    let server = Server::start(Settings::default(), gated_bus(held)).await;
    let mut ws = server.open().await;
    ws.hello(1).await;
    ws.next_frame().await;

    ws.invoke("c1", "test.gated", Value::Null).await;
    ws.invoke("c1", "test.ok", Value::Null).await;

    // Answering would be worse than closing: the client could not tell which
    // of the two calls under this id the response belonged to.
    let (code, reason) = ws.expect_close().await;
    assert_eq!(code, close::BAD_FRAME);
    assert!(
        reason.contains("c1") && reason.contains("unique"),
        "{reason}"
    );
    drop(release);
}

#[tokio::test]
async fn a_client_can_tell_from_seq_exactly_how_much_progress_it_missed() {
    // The whole reason `seq` exists. Progress is droppable when the client
    // is behind, so a client that only counted frames would render a stale
    // bar forever and never know.
    //
    // Deterministic without sleeping: the flood is reported synchronously
    // inside `dispatch`, which runs on the same task that drains the send
    // queue, so exactly `send_queue` frames are queued and the rest are
    // provably dropped.
    let settings = Settings {
        send_queue: 4,
        ..Settings::default()
    };
    let (release, held) = oneshot::channel::<()>();
    let server = Server::start(settings, flooding_bus(held, 6)).await;
    let mut ws = server.open().await;
    ws.hello(1).await;
    ws.next_frame().await;

    ws.invoke("c1", "test.flood", Value::Null).await;

    let mut seqs = Vec::new();
    for _ in 0..4 {
        let frame = ws.next_frame().await;
        assert_eq!(frame["type"], "progress");
        seqs.push(frame["seq"].as_u64().unwrap());
    }
    assert_eq!(seqs, vec![0, 1, 2, 3], "the queue holds exactly send_queue");

    // Now that the queue is drained, one more update gets through.
    release.send(()).unwrap();
    let late = ws.next_frame().await;
    assert_eq!(late["type"], "progress");
    let late_seq = late["seq"].as_u64().unwrap();
    assert_eq!(
        late_seq, 6,
        "seq counts what was reported, not what was delivered"
    );
    assert_eq!(
        late_seq - seqs.last().unwrap() - 1,
        2,
        "the gap tells the client two updates were dropped"
    );

    let response = ws.next_frame().await;
    assert_eq!(response["type"], "response");
    assert_eq!(response["id"], "c1");
}

#[tokio::test]
async fn no_progress_arrives_after_the_response_that_ended_the_call() {
    // The shell frees its progress handler on the response. A late progress
    // frame for a finished id would be routed to nothing, or worse to the
    // next call that reuses the id.
    let kept = Arc::new(Mutex::new(None));
    let server = Server::start(Settings::default(), sink_keeping_bus(Arc::clone(&kept))).await;
    let mut ws = server.open().await;
    ws.hello(1).await;
    ws.next_frame().await;

    ws.invoke("c1", "test.keep_sink", Value::Null).await;
    assert_eq!(ws.next_frame().await["type"], "response");

    // The command kept its sink and reports after it finished.
    let sink = kept.lock().unwrap().take().expect("sink was not kept");
    sink.message("too late");
    assert_eq!(sink.id().as_str(), "c1");

    // Fence: the next frame is the next call's response, not a stray update.
    ws.invoke("c2", "test.fence", Value::Null).await;
    let next = ws.next_frame().await;
    assert_eq!(next["type"], "response", "a dead call still sent progress");
    assert_eq!(next["id"], "c2");
}

#[tokio::test]
async fn a_canceled_call_still_answers_with_a_response() {
    // Cancellation is cooperative, so the client must not clean up on
    // sending the cancel: the response is still owed.
    let server = Server::start(Settings::default(), cancelable_bus()).await;
    let mut ws = server.open().await;
    ws.hello(1).await;
    ws.next_frame().await;

    ws.invoke("c1", "test.until_canceled", Value::Null).await;
    ws.send_json(&json!({ "type": "cancel", "id": "c1" })).await;

    let response = ws.next_frame().await;
    assert_eq!(response["id"], "c1");
    assert_eq!(response["outcome"]["status"], "error");
    assert_eq!(response["outcome"]["error"]["code"], "test.canceled");
}

#[tokio::test]
async fn a_cancel_for_an_id_that_is_not_running_is_ignored_not_answered() {
    // A cancel racing a response is normal. A second response for that id
    // would be indistinguishable from the real one.
    let server = Server::start(Settings::default(), default_bus()).await;
    let mut ws = server.open().await;
    ws.hello(1).await;
    ws.next_frame().await;

    ws.invoke("c1", "test.ok", Value::Null).await;
    assert_eq!(ws.next_frame().await["id"], "c1");
    ws.send_json(&json!({ "type": "cancel", "id": "c1" })).await;
    ws.send_json(&json!({ "type": "cancel", "id": "never-used" }))
        .await;

    ws.invoke("c2", "test.fence", Value::Null).await;
    let next = ws.next_frame().await;
    assert_eq!(next["id"], "c2", "a stale cancel was answered");
}

#[tokio::test]
async fn too_many_calls_in_flight_refuses_one_call_and_keeps_the_socket() {
    // Load is not a protocol error: the connection carries a desktop full of
    // windows and must not drop because one app got enthusiastic.
    let settings = Settings {
        max_in_flight: 1,
        ..Settings::default()
    };
    let server = Server::start(settings, cancelable_bus()).await;
    let mut ws = server.open().await;
    ws.hello(1).await;
    ws.next_frame().await;

    ws.invoke("c1", "test.until_canceled", Value::Null).await;
    ws.invoke("c2", "test.ok", Value::Null).await;

    let refused = ws.next_frame().await;
    assert_eq!(refused["id"], "c2");
    assert_eq!(refused["outcome"]["status"], "error");
    assert_eq!(refused["outcome"]["error"]["code"], "ws.too_many_in_flight");

    // The socket is still good and the first call still finishes.
    ws.send_json(&json!({ "type": "cancel", "id": "c1" })).await;
    assert_eq!(ws.next_frame().await["id"], "c1");
}

#[tokio::test]
async fn a_command_that_panics_still_answers_and_frees_its_slot() {
    // A command is other people's code. If a panic swallowed the response,
    // the client would wait forever and the in-flight slot would never come
    // back, so one bad command would eventually wedge the whole connection.
    let settings = Settings {
        max_in_flight: 1,
        ..Settings::default()
    };
    let server = Server::start(settings, panicking_bus()).await;
    let mut ws = server.open().await;
    ws.hello(1).await;
    ws.next_frame().await;

    ws.invoke("c1", "test.panic", Value::Null).await;
    let response = ws.next_frame().await;
    assert_eq!(response["id"], "c1");
    assert_eq!(response["outcome"]["status"], "error");

    // The slot was released: with max_in_flight of one, a second call would
    // be refused as too many if it had not been.
    ws.invoke("c2", "test.ok", Value::Null).await;
    let next = ws.next_frame().await;
    assert_eq!(next["id"], "c2");
    assert_eq!(
        next["outcome"]["status"], "ok",
        "the panicked call never left the registry"
    );
}

#[tokio::test]
async fn the_issuer_comes_from_the_upgrade_and_cannot_be_named_in_a_frame() {
    let server = Server::start(Settings::default(), default_bus()).await;
    let mut ws = server.open().await;
    ws.hello(1).await;
    let welcome = ws.next_frame().await;
    let connection = welcome["connection"].as_u64().unwrap();

    // A client that tries to act as another session, or another connection,
    // is simply ignored: both come from the peer the upgrade authenticated.
    let forged = SessionId::generate().unwrap();
    ws.send_json(&json!({
        "type": "invoke",
        "id": "c1",
        "command": "test.ok",
        "params": {},
        "session": forged.encode(),
        "connection": 999,
        "ip": "10.0.0.1",
    }))
    .await;
    assert_eq!(ws.next_frame().await["id"], "c1");

    let seen = server.bus.seen();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].session, server.session.encode());
    assert_ne!(seen[0].session, forged.encode());
    assert_eq!(seen[0].connection, ConnectionId(connection));
    assert_eq!(seen[0].command, "test.ok");
    assert_eq!(seen[0].params, json!({}));
    assert_eq!(seen[0].ip, IpAddr::V4(Ipv4Addr::LOCALHOST));
    assert!(seen[0].ticket.is_none(), "an invoke carries no ticket");
}

/// The client-asserted `confirmed` bit is gone, and a frame still carrying it
/// is answered with an error naming the change. It never reaches the bus:
/// neither as a confirmed call, which is the hole this closes, nor as an
/// unconfirmed one, which a stale client would take for "nothing happened".
#[tokio::test]
async fn an_invoke_still_carrying_confirmed_is_refused_and_never_dispatched() {
    let server = Server::start(Settings::default(), default_bus()).await;
    let mut ws = server.open().await;
    ws.hello(1).await;
    ws.next_frame().await;

    // Control: the same call without the field reaches the bus.
    ws.invoke("ok", "test.ok", json!({})).await;
    assert_eq!(ws.next_frame().await["outcome"]["status"], "ok");
    assert_eq!(server.bus.seen().len(), 1);

    for (id, value) in [("t", json!(true)), ("f", json!(false))] {
        ws.send_json(&json!({
            "type": "invoke",
            "id": id,
            "command": "test.ok",
            "params": {},
            "confirmed": value,
        }))
        .await;
        let answer = ws.next_frame().await;
        assert_eq!(answer["id"], id);
        assert_eq!(answer["outcome"]["status"], "error", "got {answer}");
        assert_eq!(
            answer["outcome"]["error"]["code"],
            ws::CONFIRMED_REMOVED,
            "got {answer}"
        );
        let message = answer["outcome"]["error"]["message"].as_str().unwrap();
        assert!(
            message.contains("confirm") && message.contains("Reload"),
            "the error says what changed and what to do: {message}"
        );
    }
    assert_eq!(
        server.bus.seen().len(),
        1,
        "a frame asserting its own confirmation reached the bus"
    );

    // The socket is still good.
    ws.invoke("after", "test.ok", json!({})).await;
    assert_eq!(ws.next_frame().await["outcome"]["status"], "ok");
}

/// A `confirm` frame reaches the dispatcher as a call carrying its ticket,
/// with the same issuer an invoke gets: the transport does not judge the
/// ticket, and does not let a frame name who is presenting it.
#[tokio::test]
async fn a_confirm_frame_carries_its_ticket_to_the_dispatcher_under_the_upgraded_issuer() {
    let server = Server::start(Settings::default(), default_bus()).await;
    let mut ws = server.open().await;
    ws.hello(1).await;
    let connection = ws.next_frame().await["connection"].as_u64().unwrap();

    ws.send_json(&json!({
        "type": "confirm",
        "id": "c1",
        "ticket": "the-ticket",
        "command": "test.ok",
        "params": { "a": 1 },
        "session": SessionId::generate().unwrap().encode(),
    }))
    .await;
    let answer = ws.next_frame().await;
    assert_eq!(answer["id"], "c1", "a confirm is answered like an invoke");

    let seen = server.bus.seen();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].ticket.as_deref(), Some("the-ticket"));
    assert_eq!(seen[0].command, "test.ok");
    assert_eq!(seen[0].params, json!({ "a": 1 }));
    assert_eq!(seen[0].session, server.session.encode());
    assert_eq!(seen[0].connection, ConnectionId(connection));
}

#[tokio::test]
async fn elapsed_ms_is_measured_on_the_injected_clock() {
    // Proof that a timing assertion here never needs a sleep: the command
    // moves the clock itself.
    let server = Server::start(Settings::default(), default_bus()).await;
    let mut ws = server.open().await;
    ws.hello(1).await;
    ws.next_frame().await;

    ws.invoke("c1", "test.takes_time", Value::Null).await;
    let response = ws.next_frame().await;
    assert_eq!(response["elapsed_ms"], 250);
}

// ---------------------------------------------------------------------------
// The hub: pushes and the kill switch
// ---------------------------------------------------------------------------

#[tokio::test]
async fn an_event_published_on_the_server_reaches_an_open_desktop() {
    let server = Server::start(Settings::default(), default_bus()).await;
    let mut ws = server.open().await;
    ws.hello(1).await;
    ws.next_frame().await;

    // One invoke first, so the connection is known to be pumping before the
    // push: a queued event is only delivered once the handshake is done.
    ws.invoke("c1", "test.fence", Value::Null).await;
    ws.next_frame().await;

    server.clock.advance(1_000);
    assert_eq!(
        server.hub.publish("windows.changed", json!({ "count": 3 })),
        1
    );
    let event = ws.next_frame().await;
    assert_eq!(
        event,
        json!({
            "type": "event",
            "topic": "windows.changed",
            "payload": { "count": 3 },
            "at_ms": server.clock.wall_ms(),
        }),
        "the timestamp comes from the injected clock"
    );
}

#[tokio::test]
async fn an_event_for_one_session_never_reaches_another() {
    let server = Server::start(Settings::default(), default_bus()).await;
    let other = server.another_session();

    let mut mine = server.open().await;
    mine.hello(1).await;
    mine.next_frame().await;
    let mut theirs = server.open_as(&other.1).await;
    theirs.hello(1).await;
    theirs.next_frame().await;

    assert_eq!(
        server
            .hub
            .publish_to_session(&server.session, "inbox.pending", json!({ "n": 1 })),
        1,
        "exactly one connection belongs to that session"
    );
    let event = mine.next_frame().await;
    assert_eq!(event["topic"], "inbox.pending");

    // Fence on the other socket: the first thing it ever sees after its
    // welcome is the event addressed to its own session.
    server
        .hub
        .publish_to_session(&other.0, "inbox.pending", json!({ "n": 2 }));
    let event = theirs.next_frame().await;
    assert_eq!(event["payload"], json!({ "n": 2 }), "crossed sessions");
}

#[tokio::test]
async fn revoking_a_session_closes_the_socket_it_opened_and_no_other() {
    // The transport half of the kill switch: revoking a session stops the
    // next request, and this stops the socket that is already open.
    let server = Server::start(Settings::default(), default_bus()).await;
    let other = server.another_session();

    let mut mine = server.open().await;
    mine.hello(1).await;
    mine.next_frame().await;
    let mut theirs = server.open_as(&other.1).await;
    theirs.hello(1).await;
    theirs.next_frame().await;

    assert_eq!(server.hub.revoke_session(&server.session, "signed out"), 1);
    let (code, reason) = mine.expect_close().await;
    assert_eq!(code, close::UNAUTHENTICATED);
    assert_eq!(reason, "signed out");

    // The other session is untouched.
    server
        .hub
        .publish_to_session(&other.0, "still.here", json!(true));
    assert_eq!(theirs.next_frame().await["topic"], "still.here");
}

#[tokio::test]
async fn a_long_close_reason_is_cut_to_something_the_wire_accepts() {
    // A close frame is a control frame: 125 bytes, two of which are the
    // code. An over-long reason makes the whole frame invalid, so the client
    // would get no reason at all — and the cut has to land on a character
    // boundary or the frame is not valid UTF-8 either. `revoke_session`
    // takes the reason from a caller, so this is reachable.
    let server = Server::start(Settings::default(), default_bus()).await;
    let mut ws = server.open().await;
    ws.hello(1).await;
    ws.next_frame().await;

    // Multi-byte characters straddling the limit: the 123rd byte falls in
    // the middle of one of these.
    let reason = format!("revoked because {}", "é".repeat(80));
    assert!(reason.len() > 123);
    assert_eq!(server.hub.revoke_session(&server.session, &reason), 1);

    let (code, got) = ws.expect_close().await;
    assert_eq!(code, close::UNAUTHENTICATED);
    assert!(
        got.len() <= 123,
        "the frame would be invalid: {} bytes",
        got.len()
    );
    assert!(
        reason.starts_with(&got) && got.starts_with("revoked because "),
        "the operator still has to be able to read why: {got:?}"
    );
}

#[tokio::test]
async fn closing_everything_for_shutdown_tells_each_client_why() {
    let server = Server::start(Settings::default(), default_bus()).await;
    let mut ws = server.open().await;
    ws.hello(1).await;
    ws.next_frame().await;

    assert_eq!(server.hub.close_all("the node is shutting down"), 1);
    let (code, reason) = ws.expect_close().await;
    assert_eq!(code, 1001, "a shutdown is `away`, not an error");
    assert_eq!(reason, "the node is shutting down");
}

#[tokio::test]
async fn a_closed_connection_leaves_nothing_behind_in_the_hub() {
    let server = Server::start(Settings::default(), default_bus()).await;
    let mut ws = server.open().await;
    ws.hello(1).await;
    ws.next_frame().await;
    assert_eq!(server.hub.summary().len(), 1);

    // A connection that goes away must not keep a slot, a cancel handle or a
    // place in a broadcast; a desktop that reloads twenty times a day would
    // otherwise accumulate them.
    ws.close().await;
    // Waiting on the socket rather than on the clock: the server drops it
    // once the connection task has finished tearing down, so EOF here means
    // `serve` has already returned.
    ws.wait_for_end().await;
    for _ in 0..100 {
        if server.hub.summary().is_empty() {
            return;
        }
        tokio::task::yield_now().await;
    }
    panic!("the connection is still in the hub after its socket closed");
}

#[tokio::test]
async fn a_command_canceled_by_a_disconnect_learns_about_it() {
    // Work whose only consumer has disconnected is work nobody will see.
    let seen_cancel = Arc::new(Mutex::new(false));
    let flag = Arc::clone(&seen_cancel);
    let server = Server::start(Settings::default(), cancel_watching_bus(flag)).await;
    let mut ws = server.open().await;
    ws.hello(1).await;
    ws.next_frame().await;

    ws.invoke("c1", "test.until_canceled", Value::Null).await;
    // A fence, to be sure the call is running before the socket goes away:
    // its own progress frame could not arrive otherwise.
    assert_eq!(ws.next_frame().await["type"], "progress");

    ws.close().await;
    ws.wait_for_end().await;
    for _ in 0..100 {
        if *seen_cancel.lock().unwrap() {
            return;
        }
        tokio::task::yield_now().await;
    }
    panic!("a command outlived the connection that asked for it without being canceled");
}

// ---------------------------------------------------------------------------
// The injected world
// ---------------------------------------------------------------------------

/// Wall and monotonic time the test moves by hand. Nothing in this file waits
/// for real time to pass.
struct TestClock {
    wall: SystemTime,
    mono: Instant,
    advanced_ms: AtomicU64,
}

impl TestClock {
    fn new() -> Arc<TestClock> {
        Arc::new(TestClock {
            wall: SystemTime::UNIX_EPOCH + Duration::from_secs(1_800_000_000),
            mono: Instant::now(),
            advanced_ms: AtomicU64::new(0),
        })
    }

    fn advance(&self, ms: u64) {
        self.advanced_ms.fetch_add(ms, Ordering::Relaxed);
    }

    fn offset(&self) -> Duration {
        Duration::from_millis(self.advanced_ms.load(Ordering::Relaxed))
    }

    fn wall_ms(&self) -> u64 {
        (self.wall + self.offset())
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64
    }

    /// The auth state machine's pair of clocks.
    fn auth_now(&self) -> Now {
        Now {
            wall: self.wall + self.offset(),
            mono: self.mono + self.offset(),
        }
    }
}

impl ws::Clock for TestClock {
    fn now(&self) -> SystemTime {
        self.wall + self.offset()
    }

    fn mono(&self) -> Instant {
        self.mono + self.offset()
    }
}

/// What the bus saw, for asserting on attribution rather than on a log line.
struct Seen {
    command: String,
    params: Value,
    ticket: Option<String>,
    connection: ConnectionId,
    ip: IpAddr,
    /// Encoded here only so a test can prove it is the session the upgrade
    /// authenticated and not one a frame named.
    session: String,
}

type Behavior = Arc<dyn Fn(Call, ProgressSink) -> Dispatched + Send + Sync>;

/// A stand-in for `command::Registry`: records every call and runs whatever
/// the test scripted.
struct Bus {
    behavior: Behavior,
    calls: Mutex<Vec<Seen>>,
}

impl Bus {
    fn seen(&self) -> std::sync::MutexGuard<'_, Vec<Seen>> {
        self.calls.lock().unwrap()
    }
}

impl Dispatch for Bus {
    fn dispatch(&self, call: Call, progress: ProgressSink) -> Dispatched {
        self.calls.lock().unwrap().push(Seen {
            command: call.command.as_str().to_string(),
            params: call.params.clone(),
            ticket: call.ticket.clone(),
            connection: call.issuer.connection(),
            ip: call.issuer.ip(),
            session: call.issuer.session().encode(),
        });
        (self.behavior)(call, progress)
    }
}

fn boxed(f: impl Future<Output = Outcome> + Send + 'static) -> Dispatched {
    Box::pin(f)
}

/// The arms every test needs; `None` for whatever the test scripts itself.
fn common(call: &Call) -> Option<Dispatched> {
    match call.command.as_str() {
        "test.ok" => {
            let params = call.params.clone();
            Some(boxed(async move { Outcome::ok(json!({ "echo": params })) }))
        }
        "test.fence" => Some(boxed(async { Outcome::ok(json!("fence")) })),
        "test.error" => Some(boxed(async {
            Outcome::error(CommandError::new("test.nope", "not today"))
        })),
        _ => None,
    }
}

fn behavior(f: impl Fn(Call, ProgressSink) -> Dispatched + Send + Sync + 'static) -> Behavior {
    Arc::new(move |call, sink| match common(&call) {
        Some(ready) => ready,
        None => f(call, sink),
    })
}

fn default_bus() -> Behavior {
    behavior(|call, _| {
        let unknown = call.command.as_str().to_string();
        boxed(async move {
            Outcome::error(CommandError::new(
                "test.unknown",
                format!("no test behavior for {unknown}"),
            ))
        })
    })
}

/// Blocks until the test drops its end.
fn gated_bus(held: oneshot::Receiver<()>) -> Behavior {
    let held = Mutex::new(Some(held));
    behavior(move |_, _| {
        let gate = held.lock().unwrap().take();
        boxed(async move {
            if let Some(gate) = gate {
                let _ = gate.await;
            }
            Outcome::ok(json!("released"))
        })
    })
}

/// Reports `count` progress frames synchronously — while the task that drains
/// the send queue is inside `dispatch` and cannot drain — then one more after
/// the test releases it.
fn flooding_bus(held: oneshot::Receiver<()>, count: u64) -> Behavior {
    let held = Mutex::new(Some(held));
    behavior(move |_, sink| {
        for i in 0..count {
            sink.message(format!("step {i}"));
        }
        let gate = held.lock().unwrap().take();
        boxed(async move {
            if let Some(gate) = gate {
                let _ = gate.await;
            }
            sink.message("after the drain");
            Outcome::ok(json!("done"))
        })
    })
}

/// Hands its sink to the test and finishes, so the test can report progress
/// for a call that is over.
fn sink_keeping_bus(kept: Arc<Mutex<Option<ProgressSink>>>) -> Behavior {
    behavior(move |_, sink| {
        *kept.lock().unwrap() = Some(sink);
        boxed(async { Outcome::ok(json!("kept")) })
    })
}

fn cancelable_bus() -> Behavior {
    behavior(|call, _| {
        let cancel = call.cancel.clone();
        boxed(async move {
            cancel.canceled().await;
            Outcome::error(CommandError::new("test.canceled", "the command unwound"))
        })
    })
}

fn cancel_watching_bus(flag: Arc<Mutex<bool>>) -> Behavior {
    behavior(move |call, sink| {
        sink.message("running");
        let cancel = call.cancel.clone();
        let flag = Arc::clone(&flag);
        boxed(async move {
            cancel.canceled().await;
            *flag.lock().unwrap() = true;
            Outcome::error(CommandError::new("test.canceled", "the command unwound"))
        })
    })
}

fn panicking_bus() -> Behavior {
    behavior(|_, _| {
        boxed(async {
            panic!("a command panicked on purpose; the response below is the point of the test");
            #[allow(unreachable_code)]
            Outcome::ok(Value::Null)
        })
    })
}

// ---------------------------------------------------------------------------
// The server under test
// ---------------------------------------------------------------------------

/// A known one-time code. `Code::parse` is `#[cfg(test)]`, so it is not
/// visible from an integration test; `from_random_bytes` plus the public
/// `reveal` gives a code and the string that unlocks it.
fn test_code() -> Code {
    Code::from_random_bytes([0u8; 8])
}

struct Server {
    addr: SocketAddr,
    cookie: String,
    session: SessionId,
    hub: Arc<Hub>,
    clock: Arc<TestClock>,
    bus: Arc<Bus>,
    auth: Arc<http::Ctx>,
}

impl Server {
    async fn start(settings: Settings, behavior: Behavior) -> Server {
        let clock = TestClock::new();
        let limits = Limits {
            per_ip_attempts: 3,
            global_attempts: 10,
            session_max: Duration::from_secs(12 * 3600),
            max_sessions: 4,
        };
        let state = AuthState::new(limits, test_code(), clock.auth_now());
        let auth = Arc::new(http::Ctx::new(
            Arc::new(std::sync::Mutex::new(state)),
            std::sync::Arc::new(std::sync::Mutex::new(Store::in_memory().unwrap())),
            Duration::ZERO,
            Vec::new(),
            "ws-test-node".to_string(),
        ));

        let session = sign_in(&auth, &clock);
        let cookie = format!("{}={}", session::COOKIE_NAME, session.encode());

        // `test.takes_time` needs the context's own clock, which only exists
        // here, so it is wrapped around whatever the test asked for.
        let moving = Arc::clone(&clock);
        let behavior: Behavior = Arc::new(move |call, sink| {
            if call.command.as_str() == "test.takes_time" {
                let clock = Arc::clone(&moving);
                return boxed(async move {
                    clock.advance(250);
                    Outcome::ok(json!("slow"))
                });
            }
            behavior(call, sink)
        });

        let bus = Arc::new(Bus {
            behavior,
            calls: Mutex::new(Vec::new()),
        });
        let hub = Hub::new(Arc::clone(&clock) as Arc<dyn ws::Clock>);
        let ctx = WsCtx::new(
            Arc::clone(&auth),
            Arc::clone(&bus) as Arc<dyn Dispatch>,
            Arc::new(ws::NoPresence),
            Arc::clone(&hub),
            Arc::clone(&clock) as Arc<dyn ws::Clock>,
            "node-under-test".into(),
            "host-under-test".into(),
            settings,
        );

        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = ws::router(ctx);
        tokio::spawn(async move {
            let _ = axum::serve(
                listener,
                app.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .await;
        });

        Server {
            addr,
            cookie,
            session,
            hub,
            clock,
            bus,
            auth,
        }
    }

    /// A second signed-in session, for the tests that assert one session's
    /// frames never reach another.
    fn another_session(&self) -> (SessionId, String) {
        let id = sign_in(&self.auth, &self.clock);
        let cookie = format!("{}={}", session::COOKIE_NAME, id.encode());
        (id, cookie)
    }

    async fn open(&self) -> Client {
        self.open_as(&self.cookie).await
    }

    async fn open_as(&self, cookie: &str) -> Client {
        Client::connect(self.addr, &[("Cookie", cookie)])
            .await
            .expect("the upgrade was refused")
    }
}

fn sign_in(auth: &Arc<http::Ctx>, clock: &Arc<TestClock>) -> SessionId {
    let id = SessionId::generate().unwrap();
    let mut state = auth.state();
    // The code is single-use, so a second sign-in needs a fresh one.
    state.rotate_code(test_code());
    let code = test_code().reveal();
    let outcome = state.login(
        IpAddr::V4(Ipv4Addr::LOCALHOST),
        &code,
        id.clone(),
        clock.auth_now(),
        &mut Vec::new(),
    );
    assert!(
        matches!(outcome, LoginOutcome::Success { .. }),
        "the test harness could not sign in: {outcome:?}"
    );
    id
}

/// The HTTP status of an upgrade the server refused. Panics if it was
/// accepted, since every caller is asserting that it was not.
async fn refused(addr: SocketAddr, headers: &[(&str, &str)]) -> u16 {
    match Client::connect(addr, headers).await {
        Err(status) => status,
        Ok(_) => panic!("the upgrade was accepted when it should have been refused"),
    }
}

fn correlation(s: &str) -> CorrelationId {
    CorrelationId::try_from(s.to_string()).unwrap()
}

// ---------------------------------------------------------------------------
// A minimal WebSocket client
// ---------------------------------------------------------------------------
//
// Enough of RFC 6455 to drive the server and no more: a text/binary/close
// writer and a frame reader. Client frames must set the mask bit, so they do,
// with a zero key — masking is anti-cache-poisoning, not security, and a zero
// key leaves the payload readable in a packet capture while still being a
// valid frame.

enum Incoming {
    Text(String),
    Close(u16, String),
    Eof,
}

struct Client {
    stream: TcpStream,
    /// Bytes read past the end of the HTTP response, which belong to the
    /// first WebSocket frame.
    spare: Vec<u8>,
}

impl Client {
    /// `Err(status)` when the server refused before upgrading.
    async fn connect(addr: SocketAddr, headers: &[(&str, &str)]) -> Result<Client, u16> {
        let mut stream = TcpStream::connect(addr).await.unwrap();
        let mut request = format!(
            "GET {} HTTP/1.1\r\nHost: {addr}\r\nConnection: Upgrade\r\nUpgrade: websocket\r\n\
             Sec-WebSocket-Version: 13\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n",
            ws::PATH
        );
        for (name, value) in headers {
            request.push_str(&format!("{name}: {value}\r\n"));
        }
        request.push_str("\r\n");
        stream.write_all(request.as_bytes()).await.unwrap();

        // Read exactly up to the end of the headers; whatever follows is
        // frame data and is kept.
        let mut buf = Vec::new();
        let end = loop {
            if let Some(at) = find(&buf, b"\r\n\r\n") {
                break at + 4;
            }
            let mut chunk = [0u8; 1024];
            let n = stream.read(&mut chunk).await.unwrap();
            assert_ne!(n, 0, "the server closed before answering the upgrade");
            buf.extend_from_slice(&chunk[..n]);
        };
        let head = String::from_utf8_lossy(&buf[..end]).to_string();
        let status: u16 = head
            .split_whitespace()
            .nth(1)
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        if status != 101 {
            return Err(status);
        }
        Ok(Client {
            stream,
            spare: buf[end..].to_vec(),
        })
    }

    async fn hello(&mut self, protocol: u32) {
        self.send_json(&json!({ "type": "hello", "protocol": protocol, "client": "test" }))
            .await;
    }

    async fn invoke(&mut self, id: &str, command: &str, params: Value) {
        self.send_json(&json!({
            "type": "invoke",
            "id": id,
            "command": command,
            "params": params,
        }))
        .await;
    }

    async fn send_json(&mut self, value: &Value) {
        self.send_text(&serde_json::to_string(value).unwrap()).await;
    }

    async fn send_text(&mut self, text: &str) {
        self.send(0x1, text.as_bytes()).await;
    }

    async fn send_binary(&mut self, bytes: &[u8]) {
        self.send(0x2, bytes).await;
    }

    async fn close(&mut self) {
        self.send(0x8, &1000u16.to_be_bytes()).await;
    }

    async fn send(&mut self, opcode: u8, payload: &[u8]) {
        let mut frame = vec![0x80 | opcode];
        let len = payload.len();
        if len < 126 {
            frame.push(0x80 | len as u8);
        } else if len <= u16::MAX as usize {
            frame.push(0x80 | 126);
            frame.extend_from_slice(&(len as u16).to_be_bytes());
        } else {
            frame.push(0x80 | 127);
            frame.extend_from_slice(&(len as u64).to_be_bytes());
        }
        frame.extend_from_slice(&[0, 0, 0, 0]); // mask bit set, zero key
        frame.extend_from_slice(payload);
        self.stream.write_all(&frame).await.unwrap();
    }

    /// The next frame as JSON. Panics on a close, because every caller that
    /// expects one says so with [`Client::expect_close`].
    async fn next_frame(&mut self) -> Value {
        match self.recv().await {
            Incoming::Text(text) => serde_json::from_str(&text).unwrap_or_else(|e| {
                panic!("the server sent text that is not JSON: {text:?} ({e})")
            }),
            Incoming::Close(code, reason) => {
                panic!("expected a frame, got close {code}: {reason}")
            }
            Incoming::Eof => panic!("expected a frame, the connection ended"),
        }
    }

    /// Block until the server has let go of the connection: it drops the
    /// socket after its teardown, so EOF here is proof the connection task
    /// finished rather than a guess about how long that takes.
    async fn wait_for_end(&mut self) {
        loop {
            match self.recv().await {
                Incoming::Eof => return,
                Incoming::Close(..) => continue,
                Incoming::Text(text) => panic!("expected the end, got a frame: {text}"),
            }
        }
    }

    async fn expect_close(&mut self) -> (u16, String) {
        match self.recv().await {
            Incoming::Close(code, reason) => (code, reason),
            Incoming::Text(text) => panic!("expected a close, got a frame: {text}"),
            Incoming::Eof => panic!("expected a close frame, the connection just ended"),
        }
    }

    async fn recv(&mut self) -> Incoming {
        loop {
            let Some(head) = self.read_exact(2).await else {
                return Incoming::Eof;
            };
            let opcode = head[0] & 0x0f;
            assert_eq!(head[1] & 0x80, 0, "a server frame must not be masked");
            let mut len = (head[1] & 0x7f) as usize;
            if len == 126 {
                let ext = self.read_exact(2).await.expect("truncated length");
                len = u16::from_be_bytes([ext[0], ext[1]]) as usize;
            } else if len == 127 {
                let ext = self.read_exact(8).await.expect("truncated length");
                len = u64::from_be_bytes(ext.try_into().unwrap()) as usize;
            }
            let payload = self.read_exact(len).await.expect("truncated payload");
            match opcode {
                0x1 => return Incoming::Text(String::from_utf8(payload).unwrap()),
                0x2 => panic!("the server sent a binary frame"),
                0x8 => {
                    let code = if payload.len() >= 2 {
                        u16::from_be_bytes([payload[0], payload[1]])
                    } else {
                        0
                    };
                    let reason = String::from_utf8_lossy(&payload[2.min(payload.len())..]);
                    return Incoming::Close(code, reason.to_string());
                }
                // Keepalive pings and pongs are not part of what a test
                // asserts on; tungstenite answers our side for us.
                0x9 | 0xa => continue,
                other => panic!("unexpected opcode {other}"),
            }
        }
    }

    async fn read_exact(&mut self, n: usize) -> Option<Vec<u8>> {
        while self.spare.len() < n {
            let mut chunk = [0u8; 4096];
            let read = self.stream.read(&mut chunk).await;
            match read {
                Ok(0) | Err(_) => return None,
                Ok(got) => self.spare.extend_from_slice(&chunk[..got]),
            }
        }
        Some(self.spare.drain(..n).collect())
    }
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}
