//! An expired browser session takes its shells with it.
//!
//! Terminal cleanup hangs off `http::Ctx::on_session_end`, which runs when a
//! session is revoked. Expiry was not a revocation: `AuthState` dropped an
//! expired session from its map in silence, so nothing ran. The shells that
//! session opened kept running as the host user, owned by a cookie that no
//! longer verified — and a new sign-in is a new owner, so no session could
//! list them, reattach to them or close them. They still counted toward the
//! terminal cap, and only a lockdown or a restart reached them.
//!
//! These tests run real shells through the real terminal wiring, because the
//! promise is about processes: after expiry, the expired session's terminal
//! is gone from the registry, and a live session's is untouched.

use std::net::IpAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use serde_json::{Value, json};

use deckjack_core::auth::Code;
use deckjack_core::auth::session::{Session, SessionId};
use deckjack_core::auth::state::{AuthState, Limits, Now};
use deckjack_core::command::{
    Admitted, Bus, CommandId, Grants, Issuer, Journal, JournalError, LogEntry, NoGrants, NodeId,
    OsEnv as BusEnv, Registry, Request,
};
use deckjack_core::http::Ctx;
use deckjack_core::lockdown::{self, HostToken, Lockdown};
use deckjack_core::store::Store;
use deckjack_core::terminal::{self, Opener};
use deckjack_core::{node, wiring, ws};

const NODE: &str = "session-expiry";

struct Discard;

impl Journal for Discard {
    fn record(&self, _entry: LogEntry) -> Result<(), JournalError> {
        Ok(())
    }
}

struct Rig {
    dir: tempfile::TempDir,
    state: Arc<Mutex<AuthState>>,
    store: Arc<Mutex<Store>>,
    registry: Arc<Registry>,
    terminals: Arc<terminal::Registry>,
    bus: Arc<Bus>,
    ctx: Arc<Ctx>,
}

fn rig() -> Rig {
    let dir = tempfile::tempdir().unwrap();
    let state = Arc::new(Mutex::new(AuthState::new(
        Limits {
            per_ip_attempts: 3,
            global_attempts: 10,
            session_max: Duration::from_secs(3600),
            max_sessions: 4,
        },
        Code::generate().expect("a sign-in code"),
        Now::current(),
    )));
    let store = Arc::new(Mutex::new(Store::open(dir.path()).expect("the state db")));
    let registry = Arc::new(Registry::new());
    let terminals =
        wiring::register_terminal(&registry, dir.path().to_path_buf()).expect("terminal commands");
    let bus = Arc::new(
        Bus::new(Arc::clone(&registry), NodeId::new(NODE), Arc::new(BusEnv))
            .with_journal(Arc::new(Discard) as Arc<dyn Journal>)
            .with_grants(Arc::new(NoGrants) as Arc<dyn Grants>),
    );
    // As main.rs registers it: a session that ends closes its terminals.
    let reaper = Arc::clone(&terminals);
    let ctx = Arc::new(
        Ctx::new(
            Arc::clone(&state),
            Arc::clone(&store),
            Duration::ZERO,
            Vec::new(),
            NODE.to_string(),
        )
        .on_session_end(move |hash| reaper.close_for_session(hash)),
    );
    Rig {
        dir,
        state,
        store,
        registry,
        terminals,
        bus,
        ctx,
    }
}

impl Rig {
    /// A session as a sign-in leaves it, but ending at `expires`.
    fn session_until(&self, expires: SystemTime) -> SessionId {
        let id = SessionId::generate().expect("a session id");
        let ip: IpAddr = "127.0.0.1".parse().unwrap();
        let session = Session::new(ip, expires - Duration::from_secs(3600), expires);
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

    async fn open_terminal(&self, who: &SessionId) -> String {
        let mut request = Request::new(
            CommandId::new("terminal.open").unwrap(),
            json!({ "rows": 24, "cols": 80 }),
            Issuer::browser(who.clone()),
        );
        request.confirmed = true;
        let opened: Value = match self.bus.dispatch(request) {
            Ok(Admitted::Running(running)) => {
                let (_rx, done) = running.split();
                done.await.outcome.expect("terminal.open runs")
            }
            other => panic!("terminal.open was not admitted: {other:?}"),
        };
        opened["id"].as_str().expect("an id").to_string()
    }

    fn terminals_of(&self, who: &SessionId) -> Vec<String> {
        self.terminals
            .list_for(&Opener::Client(who.hash()))
            .into_iter()
            .map(|i| i.id.as_str().to_string())
            .collect()
    }

    /// Wait, up to `deadline`, for `who` to have no terminals left. A failure
    /// deadline, not pacing: the reap resolves this as soon as it happens.
    async fn reaped_within(&self, who: &SessionId, deadline: Duration) -> bool {
        let until = tokio::time::Instant::now() + deadline;
        while tokio::time::Instant::now() < until {
            if self.terminals_of(who).is_empty() {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        self.terminals_of(who).is_empty()
    }

    fn revocation_reasons(&self) -> Vec<String> {
        deckjack_core::audit::StoreSink::new(Arc::clone(&self.store))
            .timeline(&deckjack_core::events::Query::default())
            .expect("the timeline reads")
            .iter()
            .filter(|r| r.entry.command == "auth.session.revoked")
            .map(|r| r.entry.params.as_str().to_string())
            .collect()
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_expired_sessions_terminals_are_closed_without_anyone_signing_in() {
    let r = rig();
    let now = SystemTime::now();
    // Expires a moment from now, so its terminal can be opened while it is
    // still a session, as it would have been in use.
    let expiring = r.session_until(now + Duration::from_millis(300));
    let staying = r.session_until(now + Duration::from_secs(3600));
    let doomed = r.open_terminal(&expiring).await;
    let kept = r.open_terminal(&staying).await;
    assert_eq!(r.terminals_of(&expiring), std::slice::from_ref(&doomed));

    wiring::spawn_session_expiry(&r.ctx, Duration::from_millis(20));

    assert!(
        r.reaped_within(&expiring, Duration::from_secs(5)).await,
        "an expired session's shell is still running: {:?}",
        r.terminals_of(&expiring)
    );
    assert_eq!(
        r.terminals_of(&staying),
        [kept],
        "a live session's terminal was reaped with the expired one"
    );
    assert!(
        r.terminals
            .attach(&terminal::TerminalId::parse(&doomed).unwrap())
            .is_err(),
        "the expired session's terminal can still be reattached by id"
    );
    let reasons = r.revocation_reasons();
    assert!(
        reasons.iter().any(|p| p.contains("expired")),
        "the expiry left no audit row saying why: {reasons:?}"
    );
}

/// The product assembly runs the sweep by itself: `wiring::app`, which
/// `main.rs` builds the node with, starts it. A sweep that exists but that
/// nothing starts is the defect class `docs/APP-SUITE.md` §12 names.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_assembled_node_reaps_expired_sessions_on_its_own() {
    let r = rig();
    let expiring = r.session_until(SystemTime::now() + Duration::from_millis(300));
    r.open_terminal(&expiring).await;

    let approvals = deckjack_core::approvals::Approvals::open(
        Arc::clone(&r.store),
        deckjack_core::command::RiskLevel::Destructive,
    );
    let lockdown = Arc::new(
        Lockdown::restore(
            lockdown::Deps {
                clock: Box::new(lockdown::OsClock),
                random: Box::new(lockdown::OsRandom),
                sessions: Box::new(wiring::AuthSessions::new(
                    Arc::clone(&r.state),
                    Arc::clone(&r.store),
                    Arc::clone(&r.terminals),
                    NODE.to_string(),
                )),
                children: Box::new(wiring::TerminalChildren::new(Arc::clone(&r.terminals))),
                signals: Box::new(lockdown::OsSignals::new()),
                outbound: approvals.outbound(),
                passkeys: Box::new(lockdown::PasskeysUnavailable),
                journal: Box::new(lockdown::FileJournal::new(r.dir.path())),
            },
            lockdown::Policy::default(),
            NODE.to_string(),
        )
        .expect("a fresh journal"),
    );
    let ws_ctx = ws::WsCtx::new(
        Arc::clone(&r.ctx),
        wiring::BusDispatch::new(
            Arc::clone(&r.bus),
            deckjack_core::ticket::Tickets::new(Arc::new(ws::SystemClock)),
        ),
        ws::Hub::new(Arc::new(ws::SystemClock)),
        Arc::new(ws::SystemClock),
        NODE.to_string(),
        node::host_name(),
        ws::Settings::default(),
    );
    let files = deckjack_core::files::register_commands(
        &r.registry,
        r.dir.path(),
        NODE,
        &r.dir.path().join("install"),
    )
    .expect("the file commands register");
    let _app = wiring::app(
        Arc::clone(&r.ctx),
        ws_ctx,
        lockdown::UnlockCtx::new(
            Arc::clone(&lockdown),
            HostToken::load_or_create(r.dir.path(), &lockdown::OsRandom).expect("a host token"),
        ),
        lockdown,
        None,
        None,
        files,
    );

    assert!(
        r.reaped_within(&expiring, wiring::SESSION_SWEEP + Duration::from_secs(5))
            .await,
        "the assembled node never reaped an expired session's shell"
    );
}
