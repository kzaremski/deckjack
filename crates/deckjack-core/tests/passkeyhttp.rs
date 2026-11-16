//! What the passkey wiring promises: that a credential outlives the process,
//! and that the routes let in exactly who they say they do.
//!
//! Three things shape this file.
//!
//! **The defect being defended against is "complete and never constructed".**
//! `passkey.rs` was a finished WebAuthn implementation that nothing called,
//! so its tests passed while no credential ever reached disk. Every test here
//! therefore goes through the real assembly — a real `Store` in a real
//! directory, `PasskeyCtx::start` as `main` would call it, and the router
//! served over a real loopback socket — rather than building objects and
//! attaching doubles to them. A test that assembled its own registry would
//! prove nothing about whether the server does.
//!
//! **No private key.** A WebAuthn assertion is a signature and nothing in
//! this workspace can produce one, for the reasons `tests/passkey.rs`
//! explains at length. So the fixtures here forge a credential's public half
//! and the tests reach as far as the point where an authenticator would be
//! needed: a ceremony can be *started*, which is what every route check in
//! this file is about, and the persistence can be driven end to end, because
//! a stored credential is public key material and nothing else.
//!
//! **The restart is the assertion.** Several tests below close the `Store`
//! and open it again on the same directory. That is not ceremony: it is the
//! only way to tell a credential that was written down from one that merely
//! sat in a `Vec`, which is the difference the whole module exists for.

use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;

use deckjack_core::auth::session::{Session, SessionId};
use deckjack_core::auth::state::{AuthState, Limits, Now};
use deckjack_core::auth::{Code, SessionHash};
use deckjack_core::http::Ctx;
use deckjack_core::passkey::{
    Credential, CredentialId, CredentialRow, OsEnv, Settings, UserHandle,
};
use deckjack_core::passkeyhttp::{PasskeyCtx, PasskeyStore, StartError, passkey_router};
use deckjack_core::store::Store;
use webauthn_rs::prelude::Passkey;

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

/// The node every `http::Ctx` here is built for. It lands in the
/// `target_node` column of the audit rows a revocation writes, which is what
/// `a_passkey_sign_in_on_a_full_node_...` reads back.
const NODE_ID: &str = "passkey-test-node";

/// A real point on SECP256R1, as `tests/passkey.rs` uses: a malformed key
/// would be refused by the key parser rather than by whatever is under test.
const KEY_X: [u8; 32] = [
    194, 126, 127, 109, 252, 23, 131, 21, 252, 6, 223, 99, 44, 254, 140, 27, 230, 17, 94, 5, 133,
    28, 104, 41, 144, 69, 171, 149, 161, 26, 200, 243,
];
const KEY_Y: [u8; 32] = [
    143, 123, 183, 156, 24, 178, 21, 248, 117, 159, 162, 69, 171, 52, 188, 252, 26, 59, 6, 47, 103,
    92, 19, 58, 117, 103, 249, 0, 219, 8, 95, 196,
];

fn fake_key(cred_id: &[u8], counter: u32) -> Passkey {
    let json = serde_json::json!({
        "cred": {
            "cred_id": URL_SAFE_NO_PAD.encode(cred_id),
            "cred": {
                "type_": "ES256",
                "key": { "EC_EC2": {
                    "curve": "SECP256R1",
                    "x": KEY_X.to_vec(),
                    "y": KEY_Y.to_vec(),
                }},
            },
            "counter": counter,
            "transports": null,
            "user_verified": true,
            "backup_eligible": false,
            "backup_state": false,
            "registration_policy": "required",
            "extensions": {
                "cred_protect": "NotRequested",
                "hmac_create_secret": "NotRequested",
            },
            "attestation": { "data": "None", "metadata": "None" },
            "attestation_format": "None",
        }
    });
    serde_json::from_value(json).expect("the fixture passkey is the shape webauthn-rs stores")
}

fn row(cred_id: &[u8], nickname: &str, sign_count: u32) -> CredentialRow {
    Credential {
        id: CredentialId::from_bytes(cred_id.to_vec()),
        nickname: nickname.to_string(),
        created: SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000),
        last_used: None,
        sign_count,
        backup_eligible: false,
        backup_state: false,
        key: fake_key(cred_id, sign_count),
    }
    .to_row()
    .expect("the fixture credential encodes")
}

fn limits() -> Limits {
    Limits {
        per_ip_attempts: 3,
        global_attempts: 10,
        session_max: Duration::from_secs(3600),
        max_sessions: 4,
    }
}

/// The server as `main` assembles it, over a working directory of its own.
struct Harness {
    addr: SocketAddr,
    dir: tempfile::TempDir,
    store: Arc<Mutex<Store>>,
    state: Arc<Mutex<AuthState>>,
    passkeys: Arc<PasskeyCtx>,
}

impl Harness {
    /// `rows` are credentials already in the database when the node starts,
    /// which is the ordinary case: enrollment happened on some earlier run.
    async fn start(rows: &[CredentialRow]) -> Harness {
        let dir = tempfile::tempdir().expect("a working directory");
        let store = Arc::new(Mutex::new(
            Store::open(dir.path()).expect("the state database opens"),
        ));
        for r in rows {
            store
                .lock()
                .unwrap()
                .upsert_passkey(r)
                .expect("a fixture credential stores");
        }

        let state = Arc::new(Mutex::new(AuthState::new(
            limits(),
            Code::generate().expect("a sign-in code"),
            Now::current(),
        )));
        // Zero failure delay: the real delay is a cost imposed on someone
        // guessing codes over a network, and paying it here would only make
        // the suite slow.
        let ctx = Arc::new(Ctx::new(
            Arc::clone(&state),
            Arc::clone(&store),
            Duration::ZERO,
            Vec::new(),
            NODE_ID.to_string(),
        ));
        let passkeys = Arc::new(
            PasskeyCtx::start(
                Settings::default(),
                Arc::clone(&ctx),
                Arc::clone(&store),
                Box::new(OsEnv),
            )
            .expect("the passkey context starts"),
        );

        // Both routers, as `wiring::app` merges them. Merged rather than
        // served alone because a route that collided with `http::router`'s
        // own — or with its fallback — would be a panic at startup, and a
        // test that served this router by itself would never find out.
        let app = deckjack_core::http::router(Arc::clone(&ctx))
            .merge(passkey_router(Arc::clone(&passkeys)))
            // The pages too, as `wiring::app` merges them. Serving the routes
            // without them would leave the one failure this suite most needs
            // to catch invisible: a page that calls a route with a method the
            // router does not mount answers 405 on load, and no amount of
            // asserting on the routes alone would show it.
            .merge(deckjack_core::passkeypage::page_router(true));
        let tcp = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("a loopback port");
        let addr = tcp.local_addr().expect("the bound address");
        tokio::spawn(async move {
            let _ = axum::serve(tcp, app.into_make_service_with_connect_info::<SocketAddr>()).await;
        });

        Harness {
            addr,
            dir,
            store,
            state,
            passkeys,
        }
    }

    async fn post(&self, path: &str, headers: &[(&str, &str)], body: &str) -> Reply {
        request(self.addr, "POST", path, headers, Some(body)).await
    }

    async fn get(&self, path: &str, headers: &[(&str, &str)]) -> Reply {
        request(self.addr, "GET", path, headers, None).await
    }

    /// A session as the one-time code path leaves it: in `AuthState`, in the
    /// database, and with `sessions.kind` at its DEFAULT of 'tether'.
    fn tether_session(&self, ip: IpAddr) -> SessionId {
        let id = SessionId::generate().expect("a session id");
        let now = SystemTime::now();
        let session = Session::new(ip, now, now + limits().session_max);
        self.state
            .lock()
            .unwrap()
            .restore_session(id.hash(), session.clone());
        self.store
            .lock()
            .unwrap()
            .insert_session(&id.hash(), &session)
            .expect("the session row writes");
        id
    }

    fn cookie(&self, id: &SessionId) -> String {
        format!("dj_session={}", id.encode())
    }

    /// Every byte of the state database, write-ahead log included. A value
    /// that is only in the WAL is still a value on disk.
    fn state_files(&self) -> Vec<u8> {
        let mut bytes = Vec::new();
        let dir = self.dir.path().join(".deckjack");
        for entry in std::fs::read_dir(&dir).expect("the .deckjack directory exists") {
            let path = entry.expect("a directory entry").path();
            if path.is_file() {
                bytes.extend_from_slice(&std::fs::read(&path).expect("the file reads"));
            }
        }
        bytes
    }

    fn session_kind_text(&self, hash: &SessionHash) -> Option<String> {
        self.store
            .lock()
            .unwrap()
            .with_conn(|conn| {
                use rusqlite::OptionalExtension as _;
                conn.query_row(
                    "SELECT kind FROM sessions WHERE hash = ?1",
                    rusqlite::params![hash.hex()],
                    |r| r.get(0),
                )
                .optional()
            })
            .expect("the sessions table reads")
    }
}

struct Reply {
    status: u16,
    body: String,
}

/// One request over loopback. `Connection: close` so the read ends with the
/// response rather than waiting on keep-alive.
async fn request(
    addr: SocketAddr,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: Option<&str>,
) -> Reply {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    let mut text =
        format!("{method} {path} HTTP/1.1\r\nHost: deckjack.test\r\nConnection: close\r\n");
    for (k, v) in headers {
        text.push_str(&format!("{k}: {v}\r\n"));
    }
    if let Some(body) = body {
        text.push_str("Content-Type: application/json\r\n");
        text.push_str(&format!("Content-Length: {}\r\n", body.len()));
    }
    text.push_str("\r\n");
    if let Some(body) = body {
        text.push_str(body);
    }

    let mut stream = tokio::net::TcpStream::connect(addr)
        .await
        .expect("the server is listening");
    stream
        .write_all(text.as_bytes())
        .await
        .expect("the request writes");
    let mut raw = Vec::new();
    stream
        .read_to_end(&mut raw)
        .await
        .expect("the response reads");
    let whole = String::from_utf8_lossy(&raw).into_owned();
    let status = whole
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let body = whole
        .split_once("\r\n\r\n")
        .map(|(_, b)| b.to_string())
        .unwrap_or_default();
    Reply { status, body }
}

// ---------------------------------------------------------------------------
// Persistence
// ---------------------------------------------------------------------------

#[test]
fn a_credential_survives_closing_and_reopening_the_state_database() {
    // The whole defect, in one assertion. `passkey.rs` was complete and
    // unit-tested before this wiring existed, and a credential enrolled
    // against it lived in a `Vec` until the process ended. If this fails,
    // the operator enrolls a passkey, restarts the node, and the
    // authenticator still offers a credential the server has never heard of.
    let dir = tempfile::tempdir().expect("a working directory");
    let stored = row(b"pocket-4", "Pocket 4", 41);

    {
        let store = Store::open(dir.path()).expect("the state database opens");
        store
            .upsert_passkey(&stored)
            .expect("the credential stores");
        // Folded back into the main file before the connection goes, which
        // is what `main` does on shutdown. Without it the rows live in the
        // WAL and "reopen" would not be a restart.
        store.checkpoint().expect("the WAL folds back");
    }

    let store = Store::open(dir.path()).expect("the state database reopens");
    let read = store.passkeys().expect("the credentials read back");
    assert_eq!(read.len(), 1, "the credential did not survive the restart");
    assert_eq!(read[0], stored, "SQLite changed the row");

    // And the registry takes it back as the credential it was, which is what
    // `PasskeyCtx::start` does at every boot.
    let handle = UserHandle::generate(&OsEnv).expect("a user handle");
    let mut keys = deckjack_core::passkey::Passkeys::new(Settings::default(), handle)
        .expect("the default settings are valid");
    keys.restore(&read).expect("the stored row restores");
    assert!(keys.is_enrolled(), "a restored credential is enrolled");
    assert_eq!(keys.credentials()[0].sign_count, 41);
}

#[test]
fn the_user_handle_is_never_overwritten_by_a_second_set() {
    // `passkey.rs`: the handle "must be stable for the life of the working
    // directory: the handle is baked into every credential, so regenerating
    // it orphans every passkey already enrolled." If this fails, the second
    // write silently invalidates every credential in the table, and the
    // symptom is an authenticator that offers a passkey the server refuses.
    let store = Store::in_memory().expect("an in-memory database");
    let first = UserHandle::generate(&OsEnv).expect("a user handle");
    let second = UserHandle::generate(&OsEnv).expect("another user handle");
    assert_ne!(first.encode(), second.encode(), "two distinct handles");

    let kept = store
        .set_passkey_user(first, SystemTime::now())
        .expect("the first handle stores");
    assert_eq!(kept, first.encode());

    let after = store
        .set_passkey_user(second, SystemTime::now())
        .expect("the second set is a no-op, not an error");
    assert_eq!(
        after,
        first.encode(),
        "the second set overwrote the handle and orphaned every credential"
    );
    assert_eq!(
        store.passkey_user().expect("the handle reads back"),
        Some(first.encode())
    );
}

#[test]
fn the_handle_a_node_mints_on_its_first_start_is_the_one_it_comes_back_with() {
    // A handle that lived only in memory would be a different handle after
    // the next restart, which orphans every credential enrolled in between.
    // This is the test that the minting path writes before it returns.
    let dir = tempfile::tempdir().expect("a working directory");
    let first = {
        let store = Arc::new(Mutex::new(
            Store::open(dir.path()).expect("the state database opens"),
        ));
        let ctx = http_ctx(&store);
        PasskeyCtx::start(
            Settings::default(),
            ctx,
            Arc::clone(&store),
            Box::new(OsEnv),
        )
        .expect("the first start mints a handle");
        let handle = store
            .lock()
            .unwrap()
            .passkey_user()
            .expect("the handle reads");
        store.lock().unwrap().checkpoint().expect("the WAL folds");
        handle
    };
    assert!(first.is_some(), "the first start wrote a handle");

    let store = Arc::new(Mutex::new(
        Store::open(dir.path()).expect("the state database reopens"),
    ));
    let ctx = http_ctx(&store);
    PasskeyCtx::start(
        Settings::default(),
        ctx,
        Arc::clone(&store),
        Box::new(OsEnv),
    )
    .expect("the second start reuses it");
    assert_eq!(
        store
            .lock()
            .unwrap()
            .passkey_user()
            .expect("the handle reads"),
        first,
        "the second start minted a new handle over the old one"
    );
}

#[test]
fn a_user_handle_that_is_not_a_uuid_fails_the_start_rather_than_being_replaced() {
    // Replacing an unreadable handle would orphan every credential in the
    // table while looking like a successful boot. Refusing names the value
    // and leaves the row for a person to look at.
    let store = Arc::new(Mutex::new(
        Store::in_memory().expect("an in-memory database"),
    ));
    store
        .lock()
        .unwrap()
        .with_conn(|conn| {
            conn.execute(
                "INSERT INTO passkey_user (id, handle, created) VALUES (1, 'not-a-uuid', 0)",
                [],
            )
        })
        .expect("the junk row writes");

    let ctx = http_ctx(&store);
    let err = PasskeyCtx::start(
        Settings::default(),
        ctx,
        Arc::clone(&store),
        Box::new(OsEnv),
    )
    .expect_err("an unreadable handle is not something to work around");
    assert!(
        matches!(err, StartError::BadUserHandle { .. }),
        "expected a bad user handle, got {err}"
    );
    assert!(
        err.to_string().contains("not-a-uuid"),
        "the message has to name the value: {err}"
    );
}

#[tokio::test]
async fn a_removed_credential_does_not_come_back_after_a_restart() {
    // "Remove a lost device" has to mean the device is gone. A removal that
    // only took effect in memory would hand the lost device its access back
    // at the next restart, and nothing would say so.
    let keep = row(b"phone", "Phone", 3);
    let lost = row(b"lost-laptop", "Lost laptop", 9);
    let h = Harness::start(&[keep.clone(), lost.clone()]).await;
    let ip: IpAddr = "127.0.0.1".parse().unwrap();
    let id = h.tether_session(ip);

    let reply = h
        .post(
            "/passkey/credentials/remove",
            &[("Cookie", &h.cookie(&id))],
            &format!(
                "{{\"id\":\"{}\"}}",
                CredentialId::from_bytes(b"lost-laptop".to_vec()).encode()
            ),
        )
        .await;
    assert_eq!(reply.status, 200, "{}", reply.body);
    assert!(reply.body.contains("Lost laptop"), "{}", reply.body);

    {
        let store = h.store.lock().unwrap();
        store.checkpoint().expect("the WAL folds back");
    }
    let reopened = Store::open(h.dir.path()).expect("the state database reopens");
    let rows = reopened.passkeys().expect("the credentials read back");
    assert_eq!(
        rows,
        vec![keep],
        "the removed credential came back after a restart"
    );
}

// ---------------------------------------------------------------------------
// Who may reach what
// ---------------------------------------------------------------------------

#[tokio::test]
async fn signing_in_needs_no_session_and_enrolling_does() {
    // The one that would be easy to get backwards. A sign-in route behind a
    // session check is unreachable by the only person who needs it; an
    // enrollment route in front of one lets anybody who can reach the port
    // add their own passkey to this node.
    let h = Harness::start(&[row(b"pocket-4", "Pocket 4", 1)]).await;

    let begin = h.post("/passkey/sign-in/begin", &[], "").await;
    assert_eq!(
        begin.status, 200,
        "a sign-in must be startable with no cookie: {}",
        begin.body
    );
    assert!(
        begin.body.contains("ceremony") && begin.body.contains("challenge"),
        "the browser needs both halves: {}",
        begin.body
    );

    for path in [
        "/passkey/enroll/approve",
        "/passkey/enroll/begin",
        "/passkey/step-up/begin",
        "/passkey/credentials/remove",
    ] {
        let reply = h
            .post(
                path,
                &[],
                "{\"nickname\":\"x\",\"access\":\"terminal\",\"id\":\"x\"}",
            )
            .await;
        assert_eq!(
            reply.status, 401,
            "{path} was reachable without a session: {}",
            reply.body
        );
    }
    assert_eq!(
        h.get("/passkey/credentials", &[]).await.status,
        401,
        "the device list is not public"
    );
}

#[tokio::test]
async fn a_cross_origin_post_is_refused_on_every_mutating_route() {
    // Without this, any page open in the same browser posts these routes
    // with the session cookie attached: it cannot read the answer, but
    // enrolling a credential or spending a step-up does its damage on the
    // way in.
    let h = Harness::start(&[row(b"pocket-4", "Pocket 4", 1)]).await;
    let ip: IpAddr = "127.0.0.1".parse().unwrap();
    let id = h.tether_session(ip);
    let cookie = h.cookie(&id);
    let evil = [
        ("Origin", "https://evil.example"),
        ("Cookie", cookie.as_str()),
    ];

    for path in [
        "/passkey/sign-in/begin",
        "/passkey/sign-in/finish",
        "/passkey/enroll/approve",
        "/passkey/enroll/begin",
        "/passkey/enroll/finish",
        "/passkey/step-up/begin",
        "/passkey/step-up/finish",
        "/passkey/credentials/remove",
    ] {
        let reply = h.post(path, &evil, "{}").await;
        assert_eq!(
            reply.status, 403,
            "{path} accepted a cross-origin post: {}",
            reply.body
        );
        assert!(
            reply.body.contains("cross-origin"),
            "{path}: {}",
            reply.body
        );
    }

    // The other half, or the test above would pass against a server that
    // refuses everything: the same origin as the Host header is fine.
    let same = [
        ("Origin", "https://deckjack.test"),
        ("Cookie", cookie.as_str()),
    ];
    assert_eq!(
        h.post("/passkey/sign-in/begin", &same, "").await.status,
        200
    );
}

// ---------------------------------------------------------------------------
// The session a passkey earns
// ---------------------------------------------------------------------------

#[tokio::test]
async fn no_session_cookie_reaches_the_state_file_after_a_passkey_sign_in() {
    // `store.rs` does not import `SessionId` at all so that a cookie cannot
    // reach disk by reaching for the wrong variable. This module writes
    // through `with_conn`, which is outside that protection, so the promise
    // has to be re-checked here: a copy of the working directory — a backup,
    // a sync target, another local account reading the file — must not hand
    // over anything that can be presented as a cookie.
    let h = Harness::start(&[row(b"pocket-4", "Pocket 4", 1)]).await;
    let ip: IpAddr = "100.69.236.100".parse().unwrap();
    let id = SessionId::generate().expect("a session id");

    h.passkeys
        .admit(ip, &id)
        .expect("a fresh node refuses nobody");
    h.store.lock().unwrap().checkpoint().expect("the WAL folds");

    let bytes = h.state_files();
    let cookie = id.encode();
    assert!(
        !String::from_utf8_lossy(&bytes).contains(&cookie),
        "the session cookie is in the state file"
    );
    assert!(
        String::from_utf8_lossy(&bytes).contains(&id.hash().hex()),
        "the session was not written down at all, so this test proves nothing"
    );
}

#[tokio::test]
async fn a_passkey_sign_in_on_a_full_node_evicts_instead_of_refusing_and_deletes_the_row() {
    // `admit` used to answer `SessionsFull`, which `sign_in_finish` turned
    // into a 503: with every slot held by a forgotten tab, a verified passkey
    // could not sign in for up to `session_max` — twenty-four hours on a
    // default node — while the one-time code path would have evicted happily.
    //
    // The ROW is what makes this a wiring test rather than another
    // state-machine test. It only goes away if `admit` routes its events
    // through `http::Ctx::apply`, which is the one place that knows a revoked
    // session owes a deleted row, a reaped pty and an audit entry.
    let h = Harness::start(&[row(b"pocket-4", "Pocket 4", 1)]).await;
    let ip: IpAddr = "100.69.236.100".parse().unwrap();

    let stale: Vec<SessionId> = (0..limits().max_sessions)
        .map(|_| h.tether_session(ip))
        .collect();

    let fresh = SessionId::generate().expect("a session id");
    h.passkeys
        .admit(ip, &fresh)
        .expect("a proven passkey was refused a session on a full node");

    assert!(
        h.state.lock().unwrap().session(&fresh).is_some(),
        "the admitted session is not in the state machine"
    );

    // Which one went is settled by the unit tests, with injected clocks.
    // Here the question is only that exactly one did, and that it really left.
    let gone: Vec<&SessionId> = stale
        .iter()
        .filter(|id| h.state.lock().unwrap().session(id).is_none())
        .collect();
    assert_eq!(
        gone.len(),
        1,
        "a full node should have evicted exactly one session"
    );

    let live = h
        .store
        .lock()
        .unwrap()
        .live_sessions(SystemTime::now())
        .expect("the session rows read back");
    assert!(
        !live.iter().any(|(hash, _)| *hash == gone[0].hash()),
        "the evicted session is still in the state database and would come back at the next start"
    );
    assert!(
        live.iter().any(|(hash, _)| *hash == fresh.hash()),
        "the admitted session was not persisted, so this test would pass for the wrong reason"
    );

    // And it is written down. The audit row is produced by `http::Ctx`
    // itself, from the store it already holds, so this asserts on the
    // product's own behavior and not on a closure a harness registered.
    let rows = deckjack_core::audit::StoreSink::new(Arc::clone(&h.store))
        .timeline(&deckjack_core::events::Query::default())
        .expect("the timeline reads");
    let revoked: Vec<_> = rows
        .iter()
        .filter(|r| r.entry.command == "auth.session.revoked")
        .collect();
    assert_eq!(
        revoked.len(),
        1,
        "a session was revoked to seat a passkey and the audit log does not say so"
    );
    assert_eq!(revoked[0].entry.target_node, NODE_ID);
    assert!(
        revoked[0].entry.params.as_str().contains("sessions_full"),
        "the row has to say why the session went: {}",
        revoked[0].entry.params
    );
    assert!(
        revoked[0]
            .entry
            .params
            .as_str()
            .contains(&gone[0].hash().tag()),
        "the row has to name which session went: {}",
        revoked[0].entry.params
    );
}

#[tokio::test]
async fn a_passkey_sign_in_from_a_blocked_address_is_refused_without_evicting() {
    // The route checks `refusal` before it spends the ceremony. This checks
    // the second one, inside `AuthState::admit` and so atomic with the
    // eviction: without it, an address blocked during the ceremony would
    // destroy a live session on its way in.
    let h = Harness::start(&[row(b"pocket-4", "Pocket 4", 1)]).await;
    let operator: IpAddr = "100.69.236.100".parse().unwrap();
    let hostile: IpAddr = "100.69.236.200".parse().unwrap();

    // The node is full, and every slot belongs to the operator's own
    // address, so a refusal that nonetheless evicted would be visible here.
    let live: Vec<SessionId> = (0..limits().max_sessions)
        .map(|_| h.tether_session(operator))
        .collect();

    // Blocked through the state machine rather than over HTTP: the test
    // client connects from loopback, so posting wrong codes would block
    // 127.0.0.1 and leave `hostile` as welcome as ever.
    for _ in 0..limits().per_ip_attempts {
        h.state.lock().unwrap().login(
            hostile,
            "0000-0000",
            SessionId::generate().expect("a session id"),
            Now::current(),
            &mut Vec::new(),
        );
    }
    assert!(h.state.lock().unwrap().is_blocked(hostile));

    let fresh = SessionId::generate().expect("a session id");
    assert!(
        h.passkeys.admit(hostile, &fresh).is_err(),
        "a blocked address was admitted"
    );
    assert!(h.state.lock().unwrap().session(&fresh).is_none());
    for id in &live {
        assert!(
            h.state.lock().unwrap().session(id).is_some(),
            "a refused admission evicted a live session on its way out"
        );
    }
}

#[tokio::test]
async fn a_passkey_session_is_recorded_as_the_stronger_kind() {
    // Migration 7 defaults `kind` to 'tether' precisely so that a session
    // only counts as the stronger kind if something said so. If this fails,
    // every passkey session reads as tether and step-up refuses the one
    // thing it exists to allow.
    let h = Harness::start(&[row(b"pocket-4", "Pocket 4", 1)]).await;
    let ip: IpAddr = "100.69.236.100".parse().unwrap();
    let id = SessionId::generate().expect("a session id");
    h.passkeys.admit(ip, &id).expect("the session is admitted");

    assert_eq!(
        h.session_kind_text(&id.hash()).as_deref(),
        Some("passkey"),
        "a passkey sign-in was recorded as something else"
    );
    assert_eq!(
        h.store
            .lock()
            .unwrap()
            .session_kind(&id.hash())
            .expect("the kind reads back"),
        Some(deckjack_core::passkey::SessionKind::Trusted)
    );

    // And it really is an ordinary session: the rest of the system looks it
    // up the same way, through the same `AuthState`, under the same hash.
    assert!(
        h.state.lock().unwrap().session(&id).is_some(),
        "a passkey sign-in has to produce a session the rest of the system can find"
    );
    assert_eq!(
        h.state.lock().unwrap().session(&id).unwrap().ip,
        ip,
        "the session is bound to the address that proved itself"
    );
}

#[tokio::test]
async fn a_tether_session_cannot_step_up_and_a_passkey_session_can_start_to() {
    // The payoff of recording the kind, and the one request where the two
    // kinds must behave differently. A borrowed machine in tether mode holds
    // no passkey of mine; letting it step up would hand a shell on the host
    // to whatever is logging that keyboard.
    let h = Harness::start(&[row(b"pocket-4", "Pocket 4", 1)]).await;
    let ip: IpAddr = "127.0.0.1".parse().unwrap();

    let tether = h.tether_session(ip);
    let refused = h
        .post(
            "/passkey/step-up/begin",
            &[("Cookie", &h.cookie(&tether))],
            "{\"access\":\"terminal\"}",
        )
        .await;
    assert_eq!(refused.status, 403, "{}", refused.body);
    assert!(
        refused.body.contains("tether mode"),
        "the refusal has to say why: {}",
        refused.body
    );

    let trusted = SessionId::generate().expect("a session id");
    h.passkeys
        .admit(ip, &trusted)
        .expect("the session is admitted");
    let allowed = h
        .post(
            "/passkey/step-up/begin",
            &[("Cookie", &h.cookie(&trusted))],
            "{\"access\":\"terminal\"}",
        )
        .await;
    assert_eq!(
        allowed.status, 200,
        "a passkey session must be able to start a step-up: {}",
        allowed.body
    );
    assert!(allowed.body.contains("challenge"), "{}", allowed.body);
}

#[tokio::test]
async fn a_node_with_no_credential_says_so_instead_of_offering_an_empty_prompt() {
    // `Passkeys::keys` refuses rather than asking the browser to choose from
    // an empty allow-list, because that surfaces as an opaque failure in the
    // authenticator UI. The route has to pass that through, so the sign-in
    // page can offer the one-time code instead.
    let h = Harness::start(&[]).await;
    assert!(!h.passkeys.is_enrolled());
    let reply = h.post("/passkey/sign-in/begin", &[], "").await;
    assert_eq!(reply.status, 409, "{}", reply.body);
    assert!(
        reply.body.contains("no passkey is enrolled"),
        "{}",
        reply.body
    );
}

// ---------------------------------------------------------------------------
// Helpers that need the crate's own types
// ---------------------------------------------------------------------------

fn http_ctx(store: &Arc<Mutex<Store>>) -> Arc<Ctx> {
    Arc::new(Ctx::new(
        Arc::new(Mutex::new(AuthState::new(
            limits(),
            Code::generate().expect("a sign-in code"),
            Now::current(),
        ))),
        Arc::clone(store),
        Duration::ZERO,
        Vec::new(),
        NODE_ID.to_string(),
    ))
}

#[test]
fn a_second_write_of_one_credential_moves_its_sign_count_instead_of_adding_a_row() {
    // The sign count is the clone check's whole input. If an assertion's
    // moved count were written as a second row — or not written at all — a
    // restart would compare the next assertion against a stale number, and
    // a cloned authenticator would sign in without raising the alarm that
    // the design makes a lockdown trigger rather than a warning.
    let store = Store::in_memory().expect("an in-memory database");
    store
        .upsert_passkey(&row(b"pocket-4", "Pocket 4", 7))
        .expect("the first write lands");
    let mut moved = row(b"pocket-4", "Pocket 4", 7);
    moved.sign_count = 8;
    moved.last_used = Some(1_767_225_600);
    moved.backup_state = true;
    store
        .upsert_passkey(&moved)
        .expect("the second write lands");

    let rows = store.passkeys().expect("the credentials read back");
    assert_eq!(rows.len(), 1, "one credential became two rows");
    assert_eq!(rows[0].sign_count, 8, "the sign count did not move");
    assert_eq!(rows[0].last_used, Some(1_767_225_600));
    assert!(rows[0].backup_state, "the backup state did not move");
    assert_eq!(
        rows[0].created,
        row(b"pocket-4", "Pocket 4", 7).created,
        "`created` is not a field an assertion may rewrite"
    );
}

// ---------------------------------------------------------------------------
// the pages that call these routes
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_device_list_answers_the_method_the_enrollment_page_uses() {
    // The bug this exists for: the enrollment script's first draft POSTed to
    // the device list, which is mounted as a GET. The page loaded, asked the
    // node what it held, got a 405, and showed an error with no way forward --
    // and the unit test that checked the script "calls /passkey/credentials"
    // was green throughout, because the path was right and the verb was not.
    let h = Harness::start(&[]).await;

    let get = h.get("/passkey/credentials", &[]).await;
    assert_eq!(
        get.status, 401,
        "the list must refuse a caller with no session, not reject the method: {}",
        get.body
    );
    assert!(get.body.contains("sign in first"), "{}", get.body);

    let posted = h.post("/passkey/credentials", &[], "{}").await;
    assert_eq!(
        posted.status, 405,
        "a POST to the list is a method error, which is what the page must not send"
    );
}

#[tokio::test]
async fn the_enrollment_page_and_its_script_are_served() {
    let h = Harness::start(&[]).await;

    let page = h.get(deckjack_core::passkeypage::ENROLL_PATH, &[]).await;
    assert_eq!(page.status, 200, "{}", page.body);
    assert!(
        page.body.contains("Set up a passkey"),
        "the enrollment page was not served: {}",
        page.body
    );
    // No session, and that is deliberate: the page explains what to do, and
    // every route it calls checks for itself. A redirect here would mean an
    // operator who has not signed in gets no explanation at all.
    assert!(
        page.body.contains("id=\"nickname\""),
        "the page offered no way to name a device"
    );

    let script = h
        .get(deckjack_core::passkeypage::ENROLL_SCRIPT_PATH, &[])
        .await;
    assert_eq!(script.status, 200, "{}", script.body);
    assert!(
        script.body.contains("navigator.credentials.create"),
        "the enrollment script was not served"
    );
}

#[tokio::test]
async fn the_sign_in_page_links_to_enrollment_so_a_first_passkey_is_reachable() {
    // The whole point of this page's existence: before it, the only way to
    // register a credential was to paste a script into a devtools console.
    let h = Harness::start(&[]).await;
    let page = h.get(deckjack_core::passkeypage::PAGE_PATH, &[]).await;
    assert_eq!(page.status, 200);
    assert!(
        page.body.contains(deckjack_core::passkeypage::ENROLL_PATH),
        "no route from signing in to having something to sign in with: {}",
        page.body
    );
}

#[tokio::test]
async fn enrollment_is_not_mounted_when_no_browser_would_accept_a_credential() {
    // `page_router(false)` is what a node with no usable rp_id assembles. The
    // ceremony page still answers, because /login links to it unconditionally
    // and a link to a 404 is worse than a page that explains -- but nothing
    // may invite an enrollment that would bind a credential to a name the
    // browser rejects, because a passkey cannot be moved afterwards.
    let app = deckjack_core::passkeypage::page_router(false);
    let tcp = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("a loopback port");
    let addr = tcp.local_addr().expect("the bound address");
    tokio::spawn(async move {
        let _ = axum::serve(tcp, app.into_make_service_with_connect_info::<SocketAddr>()).await;
    });

    let page = request(
        addr,
        "GET",
        deckjack_core::passkeypage::PAGE_PATH,
        &[],
        None,
    )
    .await;
    assert_eq!(page.status, 200);
    assert!(page.body.contains("Passkeys are off"), "{}", page.body);

    for path in [
        deckjack_core::passkeypage::ENROLL_PATH,
        deckjack_core::passkeypage::ENROLL_SCRIPT_PATH,
        deckjack_core::passkeypage::SCRIPT_PATH,
    ] {
        let r = request(addr, "GET", path, &[], None).await;
        assert_eq!(
            r.status, 404,
            "{path} was mounted on a node where no credential could work"
        );
    }
}
