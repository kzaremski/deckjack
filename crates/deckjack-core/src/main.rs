//! `deckjack` — the core binary.
//!
//! Takes the working directory's lock, reads its config, generates a one-time
//! sign-in code and serves the sign-in page. Everything behind the gate is
//! still to come.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context as _, bail};

use deckjack_core::apphost::AppHost;
use deckjack_core::apps;
use deckjack_core::auth::Code;
use deckjack_core::auth::state::{AuthState, Now};
use deckjack_core::command::Registry;
use deckjack_core::config::Config;
use deckjack_core::http::Ctx;
use deckjack_core::lock::{self, Acquired, LockError, OsEnv};
use deckjack_core::lockdown::{self, HostToken, Lockdown, Watchdog};
use deckjack_core::mcpserve;
use deckjack_core::node;
use deckjack_core::store::Store;
use deckjack_core::wiring::{self, BusDispatch};
use deckjack_core::ws;

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "deckjack=info,deckjack_core=info".into()),
        )
        .init();

    let mut args = std::env::args().skip(1).peekable();
    let mut listen: Option<SocketAddr> = None;
    let mut working_dir = PathBuf::from(".");
    let mut force = false;

    // `deckjack unlock` is not a server start, so it is handled before any of
    // the server's setup — before the lock above all. Taking the working
    // directory's lock to unlock a server that is holding it would fail by
    // construction.
    if args.peek().map(String::as_str) == Some("unlock") {
        args.next();
        return unlock(args);
    }

    // `deckjack agent` is a client, not a server: it relays this process's
    // stdio to the node already serving the working directory. Handled here
    // for the same reason as `unlock` — taking the directory's lock would
    // fail by construction, because the node holding it is the one we want
    // to talk to.
    if args.peek().map(String::as_str) == Some("agent") {
        args.next();
        return agent(args);
    }

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--listen" => {
                let v = args.next().context("--listen needs an address")?;
                listen = Some(v.parse().with_context(|| format!("bad address: {v}"))?);
            }
            "--dir" => working_dir = args.next().context("--dir needs a path")?.into(),
            "--force" => force = true,
            "-h" | "--help" => {
                println!(
                    "deckjack [--listen ADDR] [--dir PATH] [--force]\n\
                     deckjack unlock --recovery-code CODE [--dir PATH]\n\
                     deckjack agent SESSION [--dir PATH]\n\n\
                     --listen  address to serve on, overriding the config file\n\
                     --dir     working directory (default .)\n\
                     --force   take the lock from another node\n\n\
                     unlock    end a lockdown on the server running in this directory;\n\
                     \x20         needs a recovery code, and reads the host token from\n\
                     \x20         .deckjack/unlock.token, which only the host can\n\
                     agent     speak MCP to the node serving this directory, as SESSION.\n\
                     \x20         For an MCP client's config; reads and writes stdio.\n\
                     \x20         SESSION must be listed in [agents] and is granted\n\
                     \x20         separately"
                );
                return Ok(());
            }
            other => bail!("unknown argument: {other}"),
        }
    }

    let install = node::install_dir();
    let node_id = node::id(&install)
        .with_context(|| format!("cannot read node identity in {}", install.display()))?;

    // The lock comes before the config: two servers reading the same
    // directory is what the lock exists to stop, and it should stop before
    // either of them starts acting on what it read.
    let env = OsEnv::new(node_id.clone());
    let (mut held, how) = match lock::acquire(&working_dir, &env, force) {
        Ok(ok) => ok,
        Err(e @ (LockError::RunningHere(_) | LockError::HeldByOtherNode { .. })) => {
            eprintln!("deckjack: {e}");
            std::process::exit(1);
        }
        Err(e) => return Err(e.into()),
    };
    match &how {
        Acquired::Clean => {}
        Acquired::AfterUncleanShutdown { previous } => {
            tracing::warn!(
                pid = previous.pid,
                "previous server did not shut down cleanly; recovery is not implemented yet"
            );
        }
        Acquired::Forced { previous } => {
            tracing::warn!(node = %previous.node_id, host = %previous.host_name, "lock taken by force");
        }
    }

    let config = Config::load_or_init(&working_dir)?;
    let listen = listen.unwrap_or(config.server.listen);

    // One connection, shared: the auth context persists sessions through it
    // and the bus writes the command log through it. A second connection to
    // the same file would be two writers racing on one WAL.
    let store = Arc::new(std::sync::Mutex::new(Store::open(&working_dir)?));
    let approvals = deckjack_core::approvals::Approvals::open(
        Arc::clone(&store),
        deckjack_core::command::RiskLevel::Destructive,
    );

    let sign_in = Code::generate().context("cannot generate a sign-in code")?;
    // `reveal` is the only way to get the text; `Code` has no Display and a
    // redacting Debug precisely so it cannot reach a log line by accident.
    // Taken now, because the code itself moves into the state machine.
    let code_text = sign_in.reveal();
    // Behind an `Arc<Mutex<_>>` rather than owned by the HTTP context,
    // because lockdown's step 1 — revoke every session — has to reach the
    // same state machine, and reaching it through the HTTP context would make
    // the kill switch depend on the request path it is trying to shut off.
    let state = Arc::new(std::sync::Mutex::new(AuthState::new(
        config.auth.limits(),
        sign_in,
        Now::current(),
    )));

    // Sessions outlive the process, so a restart does not sign everyone out.
    // The code is still fresh and still single-use: restoring a session is
    // not a sign-in, and anyone without one still needs the new code.
    let restored = store
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .live_sessions(std::time::SystemTime::now())?;
    let restored_count = restored.len();
    {
        let mut st = state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for (hash, session) in restored {
            st.restore_session(hash, session);
        }
    }
    if restored_count > 0 {
        tracing::info!(count = restored_count, "sessions restored");
    }

    // The bus, and the first commands on it. The working directory is the
    // terminal's cwd because that is what the design means by apps seeing the
    // working directory as their filesystem root — the shell opens where the
    // workspace is, not wherever the server happened to be started from.
    let registry = Arc::new(Registry::new());
    let terminals = wiring::register_terminal(&registry, working_dir.clone())
        .context("cannot register the terminal commands")?;
    // Windows are server state, so the shell's window actions are commands
    // like any other rather than something the browser does to itself.
    let windows = wiring::register_windows(&registry, node_id.clone())
        .context("cannot register the window commands")?;
    // Named rather than discarded: the HTTP file routes, WebDAV, previews and
    // downloads must all cross this same boundary. Opening a second Root for
    // them is exactly how the predecessor's configured root became decorative.
    let _files = deckjack_core::files::register_commands(&registry, &working_dir, &node_id)
        .context("cannot register the file commands")?;
    // ---- the kill switch ------------------------------------------------
    //
    // Built before the bus, because the bus's journal is what counts refused
    // agent commands for the watchdog, and before the HTTP context, because a
    // sign-in is one of the automatic triggers.

    // Read by `deckjack unlock` on the host. Created here so a node is never
    // running without a way out of a lockdown.
    let host_token = HostToken::load_or_create(&working_dir, &lockdown::OsRandom)
        .with_context(|| format!("cannot prepare {}", lockdown::HOST_TOKEN_PATH))?;

    let lockdown_deps = || lockdown::Deps {
        clock: Box::new(lockdown::OsClock),
        random: Box::new(lockdown::OsRandom),
        sessions: Box::new(wiring::AuthSessions::new(
            Arc::clone(&state),
            Arc::clone(&store),
            Arc::clone(&terminals),
        )),
        children: Box::new(wiring::TerminalChildren::new(Arc::clone(&terminals))),
        signals: Box::new(lockdown::OsSignals::new()),
        // The Inbox is real now, so step 3 freezes something: `outbound`
        // marks it frozen and counts what was waiting. Mail and scheduled
        // tasks still do not exist on this node, so those two counts are 0
        // and honestly so — they become a lie the moment either lands.
        //
        // Freezing the Inbox is half of it. The other half is on the bus,
        // a few lines down in `BusParts.halt`: the lockdown is attached as
        // the bus's `command::Halt`, so while this node is locked every
        // agent-issued dispatch — an app's over its own socket included,
        // which `lockdown::gate` never sees — is refused. `unlock` releases
        // them by flipping `is_locked` back. Nothing here has to call
        // `resume_agents`, and nothing here could: that is the point of
        // asking the lockdown instead of pushing a flag at the bus.
        //
        // What neither half does is stop a command that is ALREADY running.
        // Step 2 kills child processes; an in-process command already past
        // admission runs to completion. Admission is the line these two
        // hold.
        outbound: approvals.outbound(),
        // Still the closed stub, by decision rather than omission: there is
        // no `Purpose::Unlock` to challenge with and no passkey route is in
        // `ALLOWED_WHILE_LOCKED`, so a verifier here could only accept an
        // assertion it never challenged. The usable unlock stays the host
        // token plus a recovery code.
        passkeys: Box::new(lockdown::PasskeysUnavailable),
        journal: Box::new(lockdown::FileJournal::new(&working_dir)),
    };

    // A journal that cannot be read does not mean "presumably fine". The node
    // comes up sealed: it serves only the locked page, and the way out is on
    // the host, because the recovery digests were in the file that could not
    // be read.
    let lockdown = Arc::new(
        match Lockdown::restore(
            lockdown_deps(),
            lockdown::Policy::default(),
            node_id.clone(),
        ) {
            Ok(ld) => ld,
            Err(e) => {
                tracing::error!(error = %e, "lockdown journal unreadable");
                eprintln!("deckjack: {e}");
                Lockdown::sealed(
                    lockdown_deps(),
                    lockdown::Policy::default(),
                    node_id.clone(),
                )
            }
        },
    );

    approvals.follow_lockdown(Arc::downgrade(&lockdown));

    // Without a set of codes a lockdown could not be ended at all, so a node
    // that has none issues them now and prints them once. Only when it is not
    // already locked: minting a code is an unlock path, and
    // `issue_recovery_codes` refuses from the locked state for that reason.
    let fresh_codes = if lockdown.recovery_codes_left() == 0 && !lockdown.is_locked() {
        match lockdown.issue_recovery_codes(lockdown::RECOVERY_CODES_ISSUED) {
            Ok(codes) => codes.iter().map(lockdown::RecoveryCode::reveal).collect(),
            Err(e) => {
                tracing::error!(
                    error = %e,
                    "no recovery codes could be issued; a lockdown on this node could not \
                     be ended without restoring the journal by hand"
                );
                Vec::new()
            }
        }
    } else {
        Vec::new()
    };

    let watchdog = Arc::new(Watchdog::new(lockdown.policy().auto));

    wiring::register_lockdown(&registry, Arc::clone(&lockdown))
        .context("cannot register the lockdown commands")?;

    // The audit log, pointed at `events` rather than `command_log`: the
    // richer table — path, source address, append-only triggers — has
    // shipped since migration 2 and nothing had ever written a row to it.
    // The read path is handed the same sink the journal writes through, so
    // the timeline and the log cannot drift onto two handles to one table.
    let audit = deckjack_core::audit::EventsJournal::new(Arc::clone(&store));
    deckjack_core::audit::register_timeline(&registry, audit.sink())
        .context("cannot register the timeline command")?;

    // ONE grant source, read by the bus and by the agent bridge.
    //
    // Two that disagree is how an app's tool list and an app's dispatch come
    // to tell the same caller different things — the bridge offering nothing
    // while the bus would allow it, or offering what the bus refuses. The
    // bus re-checks every call, so a mismatch opens no hole; it only lies.
    // This was briefly wrong in exactly that way, with `NoGrants` here and
    // the real store on the bus, which is why it is one binding now.
    let agent_grants = approvals.grants();

    // Assembled in `wiring` rather than here, so that "is this bus wired the
    // way the product's bus is wired" is a question a test can ask. Every
    // field of `BusParts` is required: a bus with no grant storage refuses
    // every agent command, and a bus with an Inbox and no grants takes an
    // approval and then refuses the command it just approved — neither of
    // which is a compile error, a panic, or visible to a test of the bus.
    //
    // The journal is wrapped in `WatchJournal` inside `wiring::bus`, because
    // the journal is the one place every decision passes through and so the
    // place a burst of refusals can be counted without the bus and the kill
    // switch learning about each other.
    let bus = wiring::bus(wiring::BusParts {
        registry: Arc::clone(&registry),
        node_id: node_id.clone(),
        journal: audit.clone(),
        watchdog: Arc::clone(&watchdog),
        lockdown: Arc::clone(&lockdown),
        grants: Arc::clone(&agent_grants),
        approvals: approvals.inbox(),
        // Lockdown's fourth step, on the bus. `lockdown::gate` is an axum
        // layer, so it covers HTTP and ws; an app invokes over its own Unix
        // socket and never reaches a router. Without this the kill switch
        // revokes sessions and freezes the Inbox while an app's dispatches
        // keep running.
        halt: Arc::new(wiring::LockdownHalt(Arc::clone(&lockdown))),
    });

    deckjack_core::approvals::register_approvals(&registry, &approvals, &bus)
        .context("cannot register the grant and Inbox commands")?;
    tracing::info!(
        commands = registry.list(None).len(),
        terminals = terminals.limits().max_sessions,
        "command bus ready"
    );

    // Signing out kills the shells that session opened. A pty is a process
    // running as the host user; leaving one behind after the browser that
    // asked for it has gone is a shell nobody is watching.
    let reaper = Arc::clone(&terminals);
    let sign_in_store = Arc::clone(&store);
    let sign_in_watchdog = Arc::clone(&watchdog);
    let sign_in_lockdown = Arc::clone(&lockdown);
    let ctx = Arc::new(
        Ctx::new(
            Arc::clone(&state),
            Arc::clone(&store),
            config.auth.failure_delay(),
            config.server.trusted_proxies.clone(),
            // Required, not registered: the audit row for a revoked session
            // is written by `Ctx` itself, so there is no line here that can
            // be present in `tests/end_to_end.rs` and absent in the product.
            node_id.clone(),
        )
        .on_session_end(move |hash| reaper.close_for_session(hash))
        .on_sign_in(move |ip| {
            let now = std::time::SystemTime::now();
            let known = {
                let st = sign_in_store
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                // An address we cannot look up is treated as known. The other
                // way round, a database hiccup would lock the node down —
                // and the default response to a new device is an alert, so
                // the cost of this choice is a missed alert, not a missed
                // lockdown.
                let known = st.is_known_address(ip).unwrap_or_else(|e| {
                    tracing::warn!(error = %e, "cannot tell whether this address is known");
                    true
                });
                if let Err(e) = st.remember_address(ip, now) {
                    tracing::warn!(error = %e, "cannot record the address that signed in");
                }
                known
            };
            wiring::act_on(
                &sign_in_lockdown,
                sign_in_watchdog.observe(
                    lockdown::Observation::SignIn {
                        ip,
                        known_device: known,
                    },
                    now,
                ),
            );
        }),
    );

    // `None` when no rp_id a browser would accept could be determined: a
    // node on a non-loopback address with no `[passkey]` section. Off, with
    // a warning, rather than enrolling credentials against `localhost` on a
    // machine nobody reaches by that name — a credential cannot be rebound.
    let passkeys = match deckjack_core::passkeyhttp::settings_for(listen, &config.passkey) {
        None => None,
        Some(settings) => Some(Arc::new(
            deckjack_core::passkeyhttp::PasskeyCtx::start(
                settings,
                Arc::clone(&ctx),
                Arc::clone(&store),
                deckjack_core::passkeyhttp::os_env(),
            )
            .context("cannot start the passkey registry")?
            .on_event({
                let wd = Arc::clone(&watchdog);
                let ld = Arc::clone(&lockdown);
                move |h| {
                    use deckjack_core::passkey::Event;
                    let now = std::time::SystemTime::now();
                    match &h.event {
                        Event::Failed { .. } => wiring::act_on(
                            &ld,
                            wd.observe(lockdown::Observation::FailedPasskey { ip: h.ip }, now),
                        ),
                        Event::CloneSuspected { .. } => {
                            ld.engage(lockdown::Trigger::HostCommand);
                        }
                        _ => {}
                    }
                }
            }),
        )),
    };

    banner(
        &code_text,
        listen,
        &node_id,
        &working_dir,
        &config,
        restored_count,
        &lockdown.status(),
        &fresh_codes,
    );

    // ---- the app host ---------------------------------------------------
    //
    // Apps are separate `deckjack-<name>` programs under the install
    // directory, each with its own backend, its own embedded web UI and a
    // manifest. `apps` holds every decision about them and `apphost` holds
    // the sockets; neither was constructed, so nothing was ever discovered,
    // launched or confined.
    //
    // Built after the bus because an app's commands are registered on that
    // registry at handshake, and removed from it the moment the app's
    // connection ends.
    let app_host = AppHost::new(
        apps::Host::new(
            install.clone(),
            working_dir.clone(),
            apps::OsEnv::new(),
            // `Confinement::Enforced` is the default, and stays: an app whose
            // `[needs]` this kernel cannot enforce does not launch, rather
            // than launching with the operator's own access.
            apps::Policy::default(),
            Box::new(deckjack_core::store::StoreGrants::new(Arc::clone(&store))),
        ),
        Arc::clone(&registry),
        install.clone(),
        Duration::from_secs(apps::Policy::default().handshake_secs),
    );
    // So opening a window for an app starts it, and closing the last one lets
    // the idle sweep stop it again.
    windows.attach_app_host(Arc::clone(&app_host));
    // One bridge for both callers. An app invoking a command and an agent
    // calling an MCP tool go through the same `ToolSurface` over the same
    // grant store, so there is no second answer to "what may this
    // non-operator caller use".
    let bridge =
        deckjack_core::agentbridge::AgentBridge::new(Arc::clone(&bus), Arc::clone(&agent_grants));
    app_host.attach_bridge(Arc::clone(&bridge));
    wiring::register_apps(&registry, Arc::clone(&app_host))
        .context("cannot register the app commands")?;

    let ws_ctx = ws::WsCtx::new(
        Arc::clone(&ctx),
        BusDispatch::new(Arc::clone(&bus)),
        ws::Hub::new(Arc::new(ws::SystemClock)),
        Arc::new(ws::SystemClock),
        node_id.clone(),
        node::host_name(),
        ws::Settings::default(),
    );

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("cannot start the async runtime")?;

    let unlock_ctx = lockdown::UnlockCtx::new(Arc::clone(&lockdown), host_token);
    let result = rt.block_on(serve(
        listen,
        Arc::clone(&ctx),
        ws_ctx,
        unlock_ctx,
        Arc::clone(&lockdown),
        Arc::clone(&app_host),
        passkeys.clone(),
        AgentDoors {
            install: install.clone(),
            working_dir: working_dir.clone(),
            sessions: config.agents.sessions.clone(),
            bridge: Arc::clone(&bridge),
        },
        &env,
        &mut held,
    ));

    // Apps are asked to stop before the lock goes, so an app writing out its
    // own state finishes while the directory is still ours.
    app_host.stop_all();

    // Fold the WAL back into the main file BEFORE the lock goes. The lock is
    // what tells the next start — or whatever carries this directory to
    // another machine — that the files are safe to copy, so releasing it
    // over a database whose recent rows live only in state.db-wal makes that
    // promise false.
    if let Err(e) = ctx.checkpoint() {
        tracing::warn!(error = %e, "could not checkpoint the state database");
    }
    held.release().ok();
    result
}

/// The agent doors this node opens.
///
/// One value rather than three more arguments to `serve`, and it keeps the
/// three things that must agree together: the directory the sockets live in,
/// which sessions get one, and the bridge they are all served through. A
/// second bridge here would be a second answer to "what may this caller
/// use" — the failure `AgentBridge::new`'s own doc warns about.
struct AgentDoors {
    /// Where the sockets live: the install directory, which never travels.
    install: PathBuf,
    /// Which node they belong to, which is what the working directory names.
    working_dir: PathBuf,
    sessions: Vec<String>,
    bridge: Arc<deckjack_core::agentbridge::AgentBridge>,
}

#[allow(clippy::too_many_arguments)]
async fn serve(
    listen: SocketAddr,
    ctx: Arc<Ctx>,
    ws_ctx: Arc<ws::WsCtx>,
    unlock_ctx: Arc<lockdown::UnlockCtx>,
    lockdown: Arc<Lockdown>,
    app_host: Arc<AppHost>,
    passkeys: Option<Arc<deckjack_core::passkeyhttp::PasskeyCtx>>,
    doors: AgentDoors,
    env: &OsEnv,
    held: &mut lock::Lock,
) -> anyhow::Result<()> {
    let tcp = tokio::net::TcpListener::bind(listen)
        .await
        .with_context(|| format!("cannot listen on {listen}"))?;

    tracing::info!(%listen, "deckjack ready");

    // The heartbeat is what tells the next start on this node whether the
    // last one crashed; without it every restart looks unclean.
    let mut beat = tokio::time::interval(Duration::from_secs(15));

    // The app host's own clock: crash backoff, handshake deadlines and the
    // idle sweep all happen in `poll`, so an app host that is never polled
    // never restarts a crashed app and never stops an idle one. A second is
    // short enough that a restart is not noticeable and long enough that
    // this costs nothing.
    let mut sweep = tokio::time::interval(Duration::from_secs(1));

    // Discovery, socket binding and the service launches, now that there is
    // a runtime to spawn the attendant tasks on.
    app_host.start().await;

    // One door per configured agent session. `mcp.rs` generated a correct,
    // grant-filtered tool surface and `agentbridge.rs` backed it with the
    // real bus, and until this loop existed no agent could reach either:
    // the tool surface was built, tested and unreachable.
    //
    // The socket is the credential. `bind_socket` puts it in a directory
    // only this user can open, so connecting already means being the user
    // that runs this node, and the session a connection speaks for is fixed
    // by which socket it is rather than by anything the agent sends. The
    // grant issued to that same id is what decides whether it can do
    // anything at all; a listed session with no grant attaches and is
    // offered nothing.
    for session in &doors.sessions {
        let path = mcpserve::socket_path(&doors.install, &doors.working_dir, session);
        let listener = match apps::bind_socket(&path).await {
            Ok(listener) => listener,
            Err(e) => {
                // Not fatal: a node that cannot open one agent's door should
                // still serve the operator, who is the one who has to fix it.
                tracing::error!(
                    session = %session,
                    path = %path.display(),
                    "cannot open the agent socket: {e}"
                );
                continue;
            }
        };
        tracing::info!(session = %session, path = %path.display(), "agent door open");
        tokio::spawn(mcpserve::mount(
            listener,
            Arc::clone(&doors.bridge),
            Arc::clone(&lockdown),
            deckjack_core::mcp::AgentIdentity {
                session: session.to_string(),
                label: mcpserve::DEFAULT_LABEL.to_string(),
                via_window: None,
            },
        ));
    }

    // Assembled in `wiring` rather than here, so that "is the lockdown gate
    // mounted outside every route" is a question a test can ask.
    let app = wiring::app(
        ctx,
        ws_ctx,
        unlock_ctx,
        lockdown,
        Some(Arc::clone(&app_host)),
        passkeys,
    );

    let http = axum::serve(tcp, app.into_make_service_with_connect_info::<SocketAddr>());

    tokio::select! {
        r = http.into_future() => r.context("HTTP server failed"),
        () = async {
            loop {
                beat.tick().await;
                if let Err(e) = held.heartbeat(env) {
                    tracing::warn!(error = %e, "cannot update heartbeat");
                }
            }
        } => Ok(()),
        () = async {
            loop {
                sweep.tick().await;
                // Takes a std mutex and no await, so it cannot hold the lock
                // across a yield point.
                app_host.poll();
            }
        } => Ok(()),
        _ = shutdown_signal() => {
            tracing::info!("shutting down");
            Ok(())
        }
    }
}

/// Ctrl-C or SIGTERM.
///
/// SIGTERM is the one that matters in practice: it is what `kill` sends and
/// what systemd sends to stop a service, so a shutdown path that waits only
/// on SIGINT is a shutdown path that almost never runs. Everything after it
/// — folding the WAL back into the state file, releasing the lock — would be
/// skipped on every ordinary stop, and the next start would read a directory
/// the lock said was clean.
async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut term = match signal(SignalKind::terminate()) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(error = %e, "cannot listen for SIGTERM; Ctrl-C only");
                let _ = tokio::signal::ctrl_c().await;
                return;
            }
        };
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = term.recv() => {}
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

#[allow(clippy::too_many_arguments)]
fn banner(
    code_text: &str,
    listen: SocketAddr,
    node_id: &str,
    dir: &std::path::Path,
    config: &Config,
    restored: usize,
    lockdown: &lockdown::Status,
    fresh_codes: &[String],
) {
    println!();
    println!("  D E C K J A C K");
    println!();
    if lockdown.locked {
        println!("  *** LOCKED DOWN ***");
        println!();
        println!("  This node serves only the locked page. The sign-in code below will");
        println!("  not get you in until it is unlocked:");
        println!();
        println!(
            "    deckjack unlock --recovery-code XXXX-XXXX-XXXX-XXXX-XXXX --dir {}",
            dir.display()
        );
        println!();
        match &lockdown.engaged {
            Some(e) => println!("  locked since   {} (unix), {}", e.at, e.reason),
            None => println!("  reason         the lockdown journal could not be read"),
        }
        println!("  recovery codes {} left", lockdown.recovery_codes_left);
        println!();
    }
    println!("  sign-in code   {code_text}");
    println!("  listening on   {listen}");
    println!("  node           {node_id}");
    println!("  directory      {}", dir.display());
    println!();
    if !listen.ip().is_loopback() {
        println!("  reachable at:");
        for addr in addresses(listen.port()) {
            println!("    {addr}");
        }
        println!();
    }
    if config.server.trusted_proxies.is_empty() {
        println!("  no trusted proxies: every request is attributed to the socket it");
        println!("  arrived on. Behind a proxy that is one address for everyone, so");
        println!("  one person's wrong codes would lock out all of them.");
    } else {
        let list: Vec<String> = config
            .server
            .trusted_proxies
            .iter()
            .map(|p| p.to_string())
            .collect();
        println!("  trusting X-Forwarded-For from: {}", list.join(", "));
    }
    println!();
    println!(
        "  {} attempts per address, {} in total, then it locks.",
        config.auth.per_ip_attempts, config.auth.global_attempts
    );
    if restored > 0 {
        println!(
            "  {restored} session{} restored: already-signed-in browsers stay in.",
            if restored == 1 { "" } else { "s" }
        );
    }
    println!();
    if !fresh_codes.is_empty() {
        println!("  RECOVERY CODES — written down now or not at all.");
        println!();
        println!("  These are the only way to end a lockdown on this node. Only their");
        println!("  digests are kept, so this is the one time they are shown. Put one in");
        println!("  a wallet, one somewhere fireproof, one in a password manager on");
        println!("  another device.");
        println!();
        for code in fresh_codes {
            println!("    {code}");
        }
        println!();
    }
}

/// `deckjack unlock --recovery-code CODE [--dir PATH]`.
///
/// Ends a lockdown on the server running in a working directory. Two factors:
/// the host token, which is a file inside that directory that only something
/// with filesystem access on the host can read, and a recovery code, which
/// exists only on paper because the journal keeps digests.
///
/// It talks to the running server over loopback rather than editing the
/// journal itself. Editing the file would leave the process still holding
/// `locked: true` in memory and still refusing every request, with the disk
/// and the server disagreeing about the state of the node.
/// `deckjack agent SESSION` — relay stdio to a running node's agent socket.
///
/// This exists so an ordinary MCP client can reach a Deckjack node with no
/// Deckjack-specific transport support:
///
/// ```text
/// command = "deckjack", args = ["agent", "assistant"]
/// ```
///
/// It is a pipe and nothing else. It holds no authority, makes no decision
/// and parses none of the protocol it carries — the node on the other end
/// does all of that, and whatever this relays is checked there against the
/// grant issued to SESSION. Deliberately so: a proxy that understood the
/// frames would be a second place where "which session is this" could be
/// answered, and the whole point is that the answer is the socket.
///
/// The working directory's lock is NOT taken. The node serving it holds that
/// lock; this is a client of that node.
fn agent(args: impl Iterator<Item = String>) -> anyhow::Result<()> {
    let mut args = args.peekable();
    let mut working_dir = PathBuf::from(".");
    let mut session: Option<String> = None;

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--dir" => working_dir = args.next().context("--dir needs a path")?.into(),
            "-h" | "--help" => {
                println!(
                    "deckjack agent SESSION [--dir PATH]\n\n\
                     Speaks MCP over stdio to the node serving PATH, as the agent\n\
                     session SESSION. For an MCP client's config:\n\n\
                     \x20 command = \"deckjack\", args = [\"agent\", \"assistant\"]\n\n\
                     SESSION must be listed in the config's [agents] section, which is\n\
                     what opens its socket. What it may then do is the grant issued to\n\
                     that same id: a session with no grant connects and is offered no\n\
                     tools at all."
                );
                return Ok(());
            }
            other if other.starts_with('-') => bail!("unknown argument: {other}"),
            other => {
                if session.replace(other.to_string()).is_some() {
                    bail!("give one session id, not several");
                }
            }
        }
    }

    let session = session.context("deckjack agent needs a session id")?;
    deckjack_core::mcpserve::check_session_name(&session)
        .with_context(|| format!("{session:?} cannot be an agent session id"))?;

    let path = deckjack_core::mcpserve::socket_path(&node::install_dir(), &working_dir, &session);

    // Current-thread: this process copies bytes between two pipes and has no
    // reason to own more than one thread to do it.
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("cannot start the async runtime")?;

    rt.block_on(async move {
        let mut socket = tokio::net::UnixStream::connect(&path)
            .await
            .with_context(|| {
                format!(
                    "cannot reach the node at {}: is deckjack running in {}, and is {session:?} \
                 listed in its [agents] section?",
                    path.display(),
                    working_dir.display()
                )
            })?;
        let mut stdio = tokio::io::join(tokio::io::stdin(), tokio::io::stdout());
        // Ends when either side closes: the client exiting, or the node
        // shutting down. Neither is an error.
        tokio::io::copy_bidirectional(&mut stdio, &mut socket)
            .await
            .context("relaying to the node failed")?;
        Ok(())
    })
}

fn unlock(args: impl Iterator<Item = String>) -> anyhow::Result<()> {
    let mut args = args.peekable();
    let mut working_dir = PathBuf::from(".");
    let mut code: Option<String> = None;
    let mut listen: Option<SocketAddr> = None;

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--recovery-code" => code = Some(args.next().context("--recovery-code needs a code")?),
            "--dir" => working_dir = args.next().context("--dir needs a path")?.into(),
            "--listen" => {
                let v = args.next().context("--listen needs an address")?;
                listen = Some(v.parse().with_context(|| format!("bad address: {v}"))?);
            }
            "-h" | "--help" => {
                println!(
                    "deckjack unlock --recovery-code CODE [--dir PATH] [--listen ADDR]\n\n\
                     Ends a lockdown on the server running in PATH. Needs one of the\n\
                     recovery codes printed when they were issued; the host half of the\n\
                     proof is read from PATH/.deckjack/unlock.token."
                );
                return Ok(());
            }
            other => bail!("unknown argument to unlock: {other}"),
        }
    }

    let code = code.context(
        "unlock needs --recovery-code; it is one of the codes printed when they were issued, \
         and only its digest is on this machine",
    )?;

    let token = std::fs::read_to_string(working_dir.join(lockdown::HOST_TOKEN_PATH)).with_context(
        || {
            format!(
                "cannot read {}; run this on the host, as the user running the server",
                working_dir.join(lockdown::HOST_TOKEN_PATH).display()
            )
        },
    )?;

    // The config, not a guess, because a server told to listen somewhere else
    // is still the server holding this directory.
    let addr = match listen {
        Some(a) => a,
        None => Config::load_or_init(&working_dir)?.server.listen,
    };
    // Whatever address it serves on, it answers on loopback at that port, and
    // that is the only place this is willing to send a recovery code.
    let target = SocketAddr::new(std::net::IpAddr::from([127, 0, 0, 1]), addr.port());

    let body = format!(
        "token={}&recovery_code={}",
        urlencode(token.trim()),
        urlencode(code.trim())
    );
    let request = format!(
        "POST /unlock HTTP/1.1\r\nHost: 127.0.0.1\r\n\
         Content-Type: application/x-www-form-urlencoded\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );

    use std::io::{Read as _, Write as _};
    let mut sock = std::net::TcpStream::connect(target)
        .with_context(|| format!("cannot reach a deckjack server at {target}"))?;
    sock.write_all(request.as_bytes())?;
    let mut response = String::new();
    sock.read_to_string(&mut response)?;

    let status = response
        .split_whitespace()
        .nth(1)
        .unwrap_or("")
        .parse::<u16>()
        .unwrap_or(0);
    let detail = response
        .split("\r\n\r\n")
        .nth(1)
        .unwrap_or("")
        .trim()
        .to_string();

    if status == 200 {
        println!("unlocked");
        Ok(())
    } else {
        bail!(
            "unlock refused ({}): {}",
            status,
            if detail.is_empty() {
                "no detail".to_string()
            } else {
                detail
            }
        )
    }
}

/// Enough percent-encoding for a token (hex) and a recovery code (base32 and
/// dashes). Both alphabets are already form-safe; this is here so a mistyped
/// code with a stray `&` or `+` in it is refused as a bad code rather than
/// silently truncating the field.
fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            use std::fmt::Write as _;
            let _ = write!(out, "%{b:02X}");
        }
    }
    out
}

/// Every address this host answers on, so a phone can be pointed at one.
fn addresses(port: u16) -> Vec<String> {
    let mut out = Vec::new();
    if let Ok(o) = std::process::Command::new("ip")
        .args(["-4", "-brief", "address", "show", "scope", "global"])
        .output()
        && o.status.success()
    {
        for line in String::from_utf8_lossy(&o.stdout).lines() {
            let mut f = line.split_whitespace();
            let (Some(iface), _state, Some(cidr)) = (f.next(), f.next(), f.next()) else {
                continue;
            };
            if let Some(ip) = cidr.split('/').next() {
                out.push(format!("http://{ip}:{port}/    ({iface})"));
            }
        }
    }
    if out.is_empty() {
        out.push(format!("http://<this-host>:{port}/"));
    }
    out
}
