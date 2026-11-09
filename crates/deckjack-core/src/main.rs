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

use deckjack_core::auth::Code;
use deckjack_core::auth::state::{AuthState, Now};
use deckjack_core::command::{Bus, NodeId, OsEnv as BusEnv, Registry};
use deckjack_core::config::Config;
use deckjack_core::http::{self, Ctx};
use deckjack_core::lock::{self, Acquired, LockError, OsEnv};
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

    let mut args = std::env::args().skip(1);
    let mut listen: Option<SocketAddr> = None;
    let mut working_dir = PathBuf::from(".");
    let mut force = false;

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
                    "deckjack [--listen ADDR] [--dir PATH] [--force]\n\n\
                     --listen  address to serve on, overriding the config file\n\
                     --dir     working directory (default .)\n\
                     --force   take the lock from another node"
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

    let sign_in = Code::generate().context("cannot generate a sign-in code")?;
    // `reveal` is the only way to get the text; `Code` has no Display and a
    // redacting Debug precisely so it cannot reach a log line by accident.
    // Taken now, because the code itself moves into the state machine.
    let code_text = sign_in.reveal();
    let mut state = AuthState::new(config.auth.limits(), sign_in, Now::current());

    // Sessions outlive the process, so a restart does not sign everyone out.
    // The code is still fresh and still single-use: restoring a session is
    // not a sign-in, and anyone without one still needs the new code.
    let restored = store
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .live_sessions(std::time::SystemTime::now())?;
    let restored_count = restored.len();
    for (id, session) in restored {
        state.restore_session(id, session);
    }
    if restored_count > 0 {
        tracing::info!(count = restored_count, "sessions restored");
    }

    banner(
        &code_text,
        listen,
        &node_id,
        &working_dir,
        &config,
        restored_count,
    );

    // The bus, and the first commands on it. The working directory is the
    // terminal's cwd because that is what the design means by apps seeing the
    // working directory as their filesystem root — the shell opens where the
    // workspace is, not wherever the server happened to be started from.
    let registry = Arc::new(Registry::new());
    let terminals = wiring::register_terminal(&registry, working_dir.clone())
        .context("cannot register the terminal commands")?;
    // Windows are server state, so the shell's window actions are commands
    // like any other rather than something the browser does to itself.
    wiring::register_windows(&registry, node_id.clone())
        .context("cannot register the window commands")?;
    // Named rather than discarded: the HTTP file routes, WebDAV, previews and
    // downloads must all cross this same boundary. Opening a second Root for
    // them is exactly how the predecessor's configured root became decorative.
    let _files = deckjack_core::files::register_commands(&registry, &working_dir, &node_id)
        .context("cannot register the file commands")?;
    // Without a journal the bus records attribution to stdout and nowhere
    // else, which makes "every command records who issued it" true only for
    // as long as the terminal scrollback lasts.
    let bus = Arc::new(
        Bus::new(
            Arc::clone(&registry),
            NodeId::new(node_id.clone()),
            Arc::new(BusEnv),
        )
        .with_journal(Arc::new(deckjack_core::store::StoreJournal::new(
            Arc::clone(&store),
        ))),
    );
    tracing::info!(
        commands = registry.list(None).len(),
        terminals = terminals.limits().max_sessions,
        "command bus ready"
    );

    // Signing out kills the shells that session opened. A pty is a process
    // running as the host user; leaving one behind after the browser that
    // asked for it has gone is a shell nobody is watching.
    let reaper = Arc::clone(&terminals);
    let ctx = Arc::new(
        Ctx::new(
            state,
            Arc::clone(&store),
            config.auth.failure_delay(),
            config.server.trusted_proxies.clone(),
        )
        .on_session_end(move |id| reaper.close_for_session(id)),
    );

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

    let result = rt.block_on(serve(listen, Arc::clone(&ctx), ws_ctx, &env, &mut held));

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

async fn serve(
    listen: SocketAddr,
    ctx: Arc<Ctx>,
    ws_ctx: Arc<ws::WsCtx>,
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

    // Two routers with different state types, merged. The bus route keeps
    // its own state so neither router's type infects the other.
    let app = http::router(ctx).merge(ws::router(ws_ctx));

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

fn banner(
    code_text: &str,
    listen: SocketAddr,
    node_id: &str,
    dir: &std::path::Path,
    config: &Config,
    restored: usize,
) {
    println!();
    println!("  D E C K J A C K");
    println!();
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
