//! The app host's runtime half: sockets, frames and the bus bridge.
//!
//! [`crate::apps`] is the app host's decisions — discovery, launch, the
//! handshake rules, backoff, idle stops, what a manifest's `[needs]` becomes
//! as a sandbox policy. It takes everything it touches through a trait, which
//! is what makes all of that testable without spawning a process. What it
//! deliberately does not contain is the half that holds a live socket: the
//! task that accepts a connection, the frame pump that correlates a `Call`
//! with its `Result`, and the bridge that turns an app's registered commands
//! into commands on the bus.
//!
//! This is that half.
//!
//! # Shape
//!
//! One **attendant** task per installed app, owning that app's
//! [`UnixListener`] for the life of the process. It waits for the app to
//! connect, runs the handshake, registers the app's commands on the bus,
//! pumps frames until the connection ends, then drops the commands again and
//! goes back to waiting. An app that crashes and is restarted by
//! [`apps::Host::poll`] reconnects to the same socket and the same attendant
//! picks it up, which is why the listener outlives any one connection.
//!
//! One **link** per connected app: a channel into its pump. Everything that
//! wants something from an app — a bus dispatch, a UI asset request, a clean
//! stop — sends an [`Outgoing`] and waits on a `oneshot`. Nothing outside
//! the pump touches the socket, so there is one writer and no interleaving of
//! two frames on one line.
//!
//! # The palette is never offered what cannot run
//!
//! An app's commands are registered at handshake and removed the moment the
//! connection ends, through `Registry::remove_namespace`. That is the
//! invariant worth stating: a palette entry for a dead app is a command that
//! fails when clicked, and the failure looks like Deckjack's rather than the
//! app's.
//!
//! # Apps calling the bus, and why that needs a capability
//!
//! Protocol 2 added the other direction: an app may send `AppFrame::Invoke`
//! to run a bus command. The pump serves it through
//! [`crate::agentbridge::AgentBridge`], which dispatches it as the agent
//! session `app:<name>` so the log records the app and not the operator.
//!
//! The gate is in [`Invocations::refusal`], and it is checked before anything
//! else because of what the alternative would be. Three apps are installed
//! today, each confined by Landlock to one folder and approved by the
//! operator for exactly that; an invocation frame with no capability behind
//! it would let any of them call `terminal.open` and have a shell, on the
//! strength of a manifest that asked to read a Notes folder. So an app whose
//! manifest does not set `needs.invoke_commands` is refused here, by the
//! core, with a message naming the key — rather than being asked nicely not
//! to try.
//!
//! The capability only lets an app *ask*. What it may actually run is the
//! grant for its session, which the bus checks on every dispatch; see
//! `agentbridge`'s note on where authority lives.

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError};
use std::time::Duration;

use serde_json::Value;
use tokio::sync::{mpsc, oneshot};

// The protocol types this module shares with `apps` come through that
// module's re-export, which is where a reader of `apps.rs` finds them by
// name. These three are imported from the protocol crate directly because
// they arrived with protocol 2 and that re-export list is in a file this
// change does not own.
use deckjack_protocol::{AvailableCommand, INVOKE_CAPABILITY, InvokeOutcome};

use crate::agentbridge::AgentBridge;
use crate::apps::{
    self, AppFrame, Connection, CoreFrame, HostEvent, Manifest, RegisteredCommand, UiAsset,
    UiError, WindowStatus,
};
use crate::command::{
    BoxFuture, CommandError, CommandId, Context, Declaration, DynCommand, ProgressSink, Registry,
    RiskLevel,
};

/// What the core asks of a connected app.
enum Outgoing {
    Call {
        command: String,
        params: Value,
        progress: ProgressSink,
        reply: oneshot::Sender<Result<Value, String>>,
    },
    Ui {
        path: String,
        reply: oneshot::Sender<Option<UiAsset>>,
    },
    /// Ask it to go away cleanly. [`apps::Env`] sends `SIGTERM` after.
    Stop,
    /// The answer to one of the app's own invocations, on its way back out.
    ///
    /// Routed through the pump like everything else rather than written by
    /// the task that produced it: a dispatch takes as long as the command
    /// takes, so it runs in its own task, and two tasks writing the socket is
    /// how two frames end up interleaved on one line.
    Invoked { call: u64, outcome: InvokeOutcome },
    /// The answer to one `AppFrame::Commands`.
    Available {
        call: u64,
        commands: Vec<AvailableCommand>,
    },
}

/// A connected app, as everything outside the pump sees it.
struct Link {
    out: mpsc::UnboundedSender<Outgoing>,
}

/// The app host, running.
pub struct AppHost {
    host: Mutex<apps::Host<apps::OsEnv>>,
    links: Mutex<BTreeMap<String, Link>>,
    /// What each app actually put on the bus, so losing the app takes away
    /// exactly that and nothing else. A namespace is not owned by one
    /// registrant — `files.*` is the core's and an app may be called `files`
    /// — so removal is by id.
    provided: Mutex<BTreeMap<String, Vec<CommandId>>>,
    registry: Arc<Registry>,
    install_dir: PathBuf,
    handshake: Duration,
    /// How an app's invocation reaches the bus. Attached after construction
    /// because the bus is built from this host's registry and an app's
    /// commands land on it at handshake, so the host has to exist first.
    ///
    /// A `OnceLock` rather than an argument so [`AppHost::new`]'s signature
    /// is unchanged, and `None` fails in the safe direction: without a bridge
    /// every invocation is refused, which is what should happen on a core
    /// that forgot to attach one. The missing wiring is then an app that
    /// cannot call anything, not an app calling with nobody checking.
    bridge: OnceLock<Arc<AgentBridge>>,
}

impl AppHost {
    pub fn new(
        mut host: apps::Host<apps::OsEnv>,
        registry: Arc<Registry>,
        install_dir: PathBuf,
        handshake: Duration,
    ) -> Arc<AppHost> {
        // Approval summaries group a window's `[ui]` scope by risk, read from
        // the same registry the bus routes by. Set here rather than left to
        // the caller, so a host the product builds can never summarize
        // without it.
        host.set_command_risks(Arc::clone(&registry) as Arc<dyn apps::CommandRisks>);
        Arc::new(AppHost {
            host: Mutex::new(host),
            links: Mutex::new(BTreeMap::new()),
            provided: Mutex::new(BTreeMap::new()),
            registry,
            install_dir,
            handshake,
            bridge: OnceLock::new(),
        })
    }

    /// Let apps invoke bus commands, through this bridge.
    ///
    /// One call, at startup, before `start`. Until it is made no app can
    /// invoke anything: see the `bridge` field. A second call is ignored
    /// rather than swapping the bridge under a live connection — an app
    /// holding an in-flight invocation would have it answered by a different
    /// bus than the one that admitted it.
    pub fn attach_bridge(&self, bridge: Arc<AgentBridge>) {
        if self.bridge.set(bridge).is_err() {
            tracing::warn!(
                "an agent bridge is already attached to this app host; the second was ignored"
            );
        }
    }

    /// Transitions are short and leave the host consistent even if a holder
    /// panicked, so a poisoned lock is still usable — as in `http::Ctx`.
    ///
    /// Every caller takes this guard inside a plain method and drops it
    /// before returning. That is not style: a `MutexGuard` alive across an
    /// `await` makes the whole future non-`Send`, and both the bus and axum
    /// require `Send` futures.
    fn host(&self) -> MutexGuard<'_, apps::Host<apps::OsEnv>> {
        self.host.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn links(&self) -> MutexGuard<'_, BTreeMap<String, Link>> {
        self.links.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn provided(&self) -> MutexGuard<'_, BTreeMap<String, Vec<CommandId>>> {
        self.provided.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Discover what is installed and start the attendants.
    ///
    /// Sockets are bound here, before anything is spawned, because an app
    /// starts by connecting to one: binding after the spawn is a race the app
    /// loses, and it loses it as a handshake timeout that looks like the
    /// app's fault.
    pub async fn start(self: &Arc<AppHost>) {
        let events = self.host().refresh();
        self.report(events);

        for problem in self.host().problems() {
            // An unusable app is reported, never fatal: a desktop that
            // refuses to start because one app was built against last
            // month's protocol is a worse computer than one that starts with
            // that app greyed out.
            tracing::warn!(
                dir = %problem.dir.display(),
                app = ?problem.name,
                error = %problem.error,
                "app unusable"
            );
        }

        let manifests: Vec<Manifest> = self
            .host()
            .installed()
            .map(|app| app.manifest.clone())
            .collect();

        for manifest in manifests {
            let name = manifest.app.name.clone();
            let path = apps::socket_path(&self.install_dir, &name);
            match apps::bind_socket(&path).await {
                Ok(listener) => {
                    let me = Arc::clone(self);
                    tokio::spawn(attend(me, manifest, listener));
                }
                Err(e) => {
                    // No socket means no handshake, so the app can never
                    // become ready. Saying so now beats a timeout later that
                    // blames the app.
                    tracing::error!(
                        app = %name,
                        path = %path.display(),
                        error = %e,
                        "cannot bind this app's socket; it will not be launched"
                    );
                }
            }
        }

        let events = self.host().start_services();
        self.report(events);
    }

    /// The idle sweep, the crash-backoff restarts and the handshake
    /// deadlines. Called on a timer by the server loop; everything it decides
    /// comes back as [`HostEvent`].
    pub fn poll(&self) {
        let events = self.host().poll();
        self.report(events);
    }

    /// Stop every app, for shutdown. Asks first and lets [`apps::Env`] do the
    /// rest: an app writing out its state deserves the chance to finish.
    pub fn stop_all(&self) {
        for link in self.links().values() {
            let _ = link.out.send(Outgoing::Stop);
        }
        self.host().stop_all();
    }

    /// Make sure an app is running because a window wants it.
    pub fn open_window(&self, app: &str) -> WindowStatus {
        match self.host().open_window(app) {
            Ok(status) => status,
            Err(e) => {
                tracing::warn!(app, error = %e, "cannot open a window for this app");
                WindowStatus::Unavailable
            }
        }
    }

    pub fn close_window(&self, app: &str) {
        self.host().close_window(app);
    }

    pub fn window_status(&self, app: &str) -> WindowStatus {
        self.host().window_status(app)
    }

    /// Approve an app's `[needs]`, which is what lets it launch confined.
    pub fn approve(&self, app: &str) -> Result<apps::Grant, apps::HostError> {
        self.host().approve(app)
    }

    /// Withdraw an app's approval. Its commands and its window's scope leave
    /// the bus now, not when its socket closes: a process may trap the stop
    /// signal and hold the connection, and a scope with no stored approval
    /// behind it is not a scope (docs/APP-UI-ISOLATION.md §4.6).
    pub fn revoke(&self, app: &str) -> Result<(), apps::HostError> {
        self.host().revoke(app)?;
        let mine = self.provided().remove(app).unwrap_or_default();
        self.registry.remove_all(&mine);
        self.registry.unregister_app(app);
        let _ = self.send(app, Outgoing::Stop);
        Ok(())
    }

    /// Where an installed app stands with the operator's approval, and what
    /// it asks for in words, for `system.apps` and a refused window open.
    /// `None` for an app that is not installed.
    pub fn approval(&self, app: &str) -> Option<(Result<apps::ApprovalState, String>, String)> {
        let host = self.host();
        let installed = host.installed().find(|i| i.name == app)?;
        let asks = host.summary(&installed.manifest.needs);
        Some((host.approval(app).map_err(|e| e.to_string()), asks))
    }

    pub fn installed(&self) -> Vec<apps::AppInfo> {
        self.host()
            .installed()
            .map(|app| app.manifest.app.clone())
            .collect()
    }

    fn report(&self, events: Vec<HostEvent>) {
        for event in events {
            match &event {
                HostEvent::Ready { app, commands } => {
                    tracing::info!(app, commands, "app ready")
                }
                HostEvent::Reconnecting { app, attempt } => {
                    tracing::warn!(app, attempt, "app reconnecting")
                }
                HostEvent::Stopped { app, idle } => tracing::info!(app, idle, "app stopped"),
                HostEvent::GaveUp {
                    app,
                    crashes,
                    reason,
                } => tracing::error!(app, crashes, reason, "app given up on"),
                HostEvent::Unavailable { app, reason } => {
                    tracing::warn!(app, reason, "app unavailable")
                }
            }
            // TODO(hub): publish these as an `apps` topic so a window can show
            // "reconnecting" rather than the desktop appearing to hang. The
            // events exist and are correct; nothing pushes them yet, which is
            // the same gap the `windows` topic has.
        }
    }

    /// An app completed its handshake: put its commands on the bus.
    fn registered(self: &Arc<AppHost>, app: &str, commands: Vec<RegisteredCommand>) {
        // Anything left over from a previous connection would collide. Only
        // what *this app* registered, by id: see `Registry::remove_all`.
        let stale = self.provided().remove(app).unwrap_or_default();
        self.registry.remove_all(&stale);

        // What this app's window may call beyond its own commands, from the
        // operator's stored approval, recorded before the commands so the
        // app is never registered without its scope. Its own scope is what
        // `register_provided` records below, command by command.
        let ui_invoke = self.host().approved_ui_invoke(app);
        self.registry.register_app(app, ui_invoke);

        let mut accepted = 0;
        let mut ids = Vec::with_capacity(commands.len());
        for command in &commands {
            match declaration(command) {
                Ok(declaration) => {
                    let id = declaration.id.clone();
                    let bridge = AppCommand {
                        app: app.to_string(),
                        command: command.id.clone(),
                        host: Arc::clone(self),
                    };
                    match self.registry.register_provided(app, declaration, bridge) {
                        Ok(registered) => {
                            accepted += 1;
                            ids.push(registered);
                        }
                        Err(e) => tracing::error!(
                            app,
                            command = %id,
                            error = %e,
                            "an app's command could not be registered"
                        ),
                    }
                }
                Err(e) => tracing::error!(
                    app,
                    command = %command.id,
                    error = %e,
                    "an app declared a command id the bus will not accept"
                ),
            }
        }

        self.provided().insert(app.to_string(), ids);
        let events = self.host().on_registered(app, commands);
        self.report(events);
        tracing::info!(app, commands = accepted, "app commands on the bus");
    }

    /// The connection ended. Take the commands away before anything else, so
    /// there is no window in which the palette offers what cannot run.
    fn lost(&self, app: &str, reason: &str) {
        let mine = self.provided().remove(app).unwrap_or_default();
        let dropped = self.registry.remove_all(&mine);
        // And its window's scope with them: a window whose process is gone
        // may call nothing, not even what the app used to provide.
        self.registry.unregister_app(app);
        self.links().remove(app);
        let events = self.host().on_lost(app, reason);
        self.report(events);
        tracing::warn!(app, reason, dropped, "app connection ended");
    }

    /// Send something to a connected app. `None` when it is not connected,
    /// which is a different answer from "it failed".
    fn send(&self, app: &str, out: Outgoing) -> Option<()> {
        self.links().get(app)?.out.send(out).ok()
    }
}

/// Translate an app's registered command into what the bus registers.
fn declaration(command: &RegisteredCommand) -> Result<Declaration, crate::command::IdError> {
    Ok(Declaration {
        id: CommandId::new(command.id.clone())?,
        title: command.summary.clone(),
        risk: risk(command.risk),
        // The app's own schema, as it registered it. `accept_registration`
        // has already refused anything that is not a schema object.
        params: schemars::Schema::try_from(command.params.clone()).unwrap_or_default(),
        target: crate::command::Target::ThisNode,
        // An app cannot ask for its parameters to be redacted through the
        // manifest, and should not be able to: redaction hides a command's
        // inputs from the audit log, which is the operator's record of what
        // ran on their machine, not the app's.
        redact_params: false,
    })
}

/// `apps::Risk` to `command::RiskLevel`.
///
/// Two enums that must agree, as `apps::Risk`'s own TODO says. They are not
/// merged here because `apps` has to parse `app.toml` on a node where the bus
/// may not be constructed; this is the one place the translation lives, and
/// it is exhaustive with no `_` arm so adding a level to either side is a
/// compile error rather than a silent downgrade.
fn risk(risk: apps::Risk) -> RiskLevel {
    match risk {
        apps::Risk::Read => RiskLevel::ReadOnly,
        apps::Risk::Write => RiskLevel::Write,
        apps::Risk::Destructive => RiskLevel::Destructive,
        apps::Risk::Outbound => RiskLevel::Outbound,
    }
}

/// One app command, on the bus.
struct AppCommand {
    app: String,
    command: String,
    host: Arc<AppHost>,
}

impl DynCommand for AppCommand {
    fn run(&self, params: Value, cx: Context) -> BoxFuture<'static, Result<Value, CommandError>> {
        let host = Arc::clone(&self.host);
        let app = self.app.clone();
        let command = self.command.clone();

        Box::pin(async move {
            // A dispatch is use: it keeps an on-demand app from being stopped
            // for idleness underneath a command it is running.
            host.host().note_activity(&app);

            let (reply, answer) = oneshot::channel();
            let sent = host.send(
                &app,
                Outgoing::Call {
                    command: command.clone(),
                    params,
                    progress: cx.progress.clone(),
                    reply,
                },
            );
            if sent.is_none() {
                // Registered but not connected: the connection ended between
                // the lookup and here. `Unsupported` rather than `Failed`,
                // because nothing was attempted and the honest answer is
                // "not on this node right now".
                return Err(CommandError::Unsupported {
                    command: cx.command.clone(),
                    reason: format!("{app} is not connected"),
                });
            }

            match answer.await {
                Ok(Ok(value)) => Ok(value),
                Ok(Err(message)) => Err(CommandError::Failed(anyhow::anyhow!(message))),
                // The pump dropped the sender, which it does when the socket
                // ends. The command did not finish and must not look as if it
                // did.
                Err(_) => Err(CommandError::Failed(anyhow::anyhow!(
                    "{app} disconnected while running {command}"
                ))),
            }
        })
    }
}

/// Serve one app's UI out of the app itself.
impl apps::UiHost for AppHost {
    fn window_status(&self, app: &str) -> WindowStatus {
        self.host().window_status(app)
    }

    fn ui_asset(&self, app: &str, path: &str) -> Result<Option<UiAsset>, UiError> {
        let (reply, answer) = oneshot::channel();
        if self
            .send(
                app,
                Outgoing::Ui {
                    path: path.to_string(),
                    reply,
                },
            )
            .is_none()
        {
            return Err(UiError::Unreachable {
                app: app.to_string(),
                reason: "not connected".to_string(),
            });
        }
        // Called on a blocking thread, which is what makes this wait allowed:
        // see the `UiHost` doc. Blocking a runtime worker on an app's reply
        // would let a wedged app stall every other request.
        answer.blocking_recv().map_err(|_| UiError::Unreachable {
            app: app.to_string(),
            reason: "the connection ended while fetching this asset".to_string(),
        })
    }
}

/// One app's invocations: the capability gate and the way back to the bus.
///
/// Built per connection from the manifest the attendant already has, so the
/// capability is read off the file the operator approved rather than off
/// anything the app said about itself on the wire.
struct Invocations {
    host: Arc<AppHost>,
    app: String,
    /// `needs.invoke_commands`, from the manifest on disk.
    allowed: bool,
    /// The pump's own inbox. A spawned dispatch answers by sending here, so
    /// the pump stays the only writer on the socket.
    out: mpsc::UnboundedSender<Outgoing>,
}

impl Invocations {
    /// Why this app may not call the bus, or `None` if it may.
    ///
    /// Both refusals name what has to change and neither is a maybe. The
    /// capability one quotes the `[needs]` key, because the reader is an app
    /// author or an operator looking at `app.toml`, and "permission denied"
    /// would send them to the sandbox instead.
    fn refusal(&self) -> Option<InvokeOutcome> {
        if !self.allowed {
            return Some(InvokeOutcome::Refused {
                message: format!(
                    "{} did not declare needs.{INVOKE_CAPABILITY}, so it may not call bus \
                     commands. Add `{INVOKE_CAPABILITY} = true` under [needs] in its app.toml \
                     and approve the app again: calling the bus is the whole machine, so it is \
                     a capability the operator grants and not something an app can take by \
                     sending a frame",
                    self.app
                ),
            });
        }
        if self.host.bridge.get().is_none() {
            // The core was built without `attach_bridge`. Refusing is the
            // only honest answer: there is nothing to check a grant against,
            // and running the command anyway would be dispatching with no
            // authorization at all.
            return Some(InvokeOutcome::Refused {
                message: format!(
                    "this core has no agent bridge attached, so no app can invoke a command; \
                     {} asked anyway",
                    self.app
                ),
            });
        }
        None
    }

    /// Serve one `AppFrame::Invoke`.
    ///
    /// Spawned, never awaited here: a dispatch takes as long as the command
    /// takes, and an app that invoked a command served by *itself* would
    /// deadlock if the pump were blocked waiting for it — the pump is what
    /// would have to deliver the `Call`.
    fn begin(&self, call: u64, command: String, params: Value) {
        if let Some(outcome) = self.refusal() {
            tracing::warn!(app = %self.app, %command, "refused an app's invocation");
            let _ = self.out.send(Outgoing::Invoked { call, outcome });
            return;
        }
        let Some(bridge) = self.host.bridge.get().cloned() else {
            return;
        };

        // An invocation is use. Without this an on-demand app could be
        // stopped for idleness while waiting on a command it asked for.
        // Taken and dropped inside this statement: a guard alive across an
        // await makes the future non-Send, and the bus requires Send.
        self.host.host().note_activity(&self.app);

        let app = self.app.clone();
        let out = self.out.clone();
        tokio::spawn(async move {
            let outcome = bridge.invoke_for_app(&app, &command, params).await;
            // If the send fails the connection is already gone, which is the
            // one case where an unanswered invocation is correct: there is
            // nobody left to answer.
            let _ = out.send(Outgoing::Invoked { call, outcome });
        });
    }

    /// Serve one `AppFrame::Commands`.
    ///
    /// Answered inline because building the list only reads the registry and
    /// the grant store, with nothing to await.
    fn list(&self, call: u64) {
        if let Some(outcome) = self.refusal() {
            // Refused with the same message an invocation gets, as an empty
            // list would read as "the bus has nothing for you" and send an
            // app author looking for the missing commands rather than the
            // missing capability.
            let _ = self.out.send(Outgoing::Invoked { call, outcome });
            return;
        }
        let Some(bridge) = self.host.bridge.get() else {
            return;
        };
        let commands = bridge.commands_for_app(&self.app);
        let _ = self.out.send(Outgoing::Available { call, commands });
    }
}

/// One app's attendant: accept, handshake, pump, repeat.
async fn attend(host: Arc<AppHost>, manifest: Manifest, listener: tokio::net::UnixListener) {
    let app = manifest.app.name.clone();
    loop {
        match apps::accept_handshake(&listener, &manifest, host.handshake).await {
            Ok((conn, commands)) => {
                let (out, rx) = mpsc::unbounded_channel();
                host.links().insert(app.clone(), Link { out: out.clone() });
                host.registered(&app, commands);

                let invocations = Invocations {
                    host: Arc::clone(&host),
                    app: app.clone(),
                    allowed: manifest.needs.invoke_commands,
                    out,
                };
                let reason = pump(conn, rx, invocations).await;
                host.lost(&app, &reason);
            }
            Err(e) => {
                // Usually a timeout, and usually because the app is simply
                // not running: an on-demand app spends most of its life not
                // running, and this loop is what is waiting when it starts.
                // So it is debug, not warn — and the timeout is itself what
                // paces this loop rather than a sleep.
                tracing::debug!(app = %app, error = %e, "no app connected");
            }
        }
    }
}

/// The frame pump for one connection. Returns why it ended.
///
/// The single writer. One task owning the socket is what keeps two frames
/// from interleaving on a line, and it is why everything else goes through a
/// channel instead of holding the `Connection`.
async fn pump(
    mut conn: Connection,
    mut rx: mpsc::UnboundedReceiver<Outgoing>,
    invocations: Invocations,
) -> String {
    let mut next = 1u64;
    let mut calls: HashMap<u64, (ProgressSink, oneshot::Sender<Result<Value, String>>)> =
        HashMap::new();
    let mut uis: HashMap<u64, oneshot::Sender<Option<UiAsset>>> = HashMap::new();
    let app = conn.app().to_string();

    loop {
        tokio::select! {
            outgoing = rx.recv() => {
                let Some(outgoing) = outgoing else {
                    return "the core closed the link".to_string();
                };
                match outgoing {
                    Outgoing::Call { command, params, progress, reply } => {
                        let id = next;
                        next += 1;
                        if let Err(e) = conn.send(&CoreFrame::Call { id, command, params }).await {
                            let _ = reply.send(Err(e.to_string()));
                            return e.to_string();
                        }
                        calls.insert(id, (progress, reply));
                    }
                    Outgoing::Ui { path, reply } => {
                        let id = next;
                        next += 1;
                        if let Err(e) = conn.send(&CoreFrame::Ui { id, path }).await {
                            // The waiter is dropped rather than left to time
                            // out: `ui_asset` blocks a thread on it.
                            drop(reply);
                            return e.to_string();
                        }
                        uis.insert(id, reply);
                    }
                    Outgoing::Stop => {
                        let _ = conn.send(&CoreFrame::Stop).await;
                        return "asked to stop".to_string();
                    }
                    Outgoing::Invoked { call, outcome } => {
                        if let Err(e) = conn.send(&CoreFrame::Invoked { call, outcome }).await {
                            return e.to_string();
                        }
                    }
                    Outgoing::Available { call, commands } => {
                        if let Err(e) = conn.send(&CoreFrame::Available { call, commands }).await {
                            return e.to_string();
                        }
                    }
                }
            }

            frame = conn.recv() => {
                match frame {
                    Err(e) => return e.to_string(),
                    Ok(AppFrame::Result { id, result, error }) => {
                        match calls.remove(&id) {
                            Some((_, reply)) => {
                                let outcome = match (result, error) {
                                    (_, Some(message)) => Err(message),
                                    (Some(value), None) => Ok(value),
                                    // Neither field: the app answered without
                                    // saying anything. Null is the honest
                                    // reading of "it worked and returned
                                    // nothing", not an error.
                                    (None, None) => Ok(Value::Null),
                                };
                                let _ = reply.send(outcome);
                            }
                            // An id we never sent, or one already answered.
                            // Logged rather than fatal: a buggy app should not
                            // be able to take its own connection down and so
                            // lose the commands still in flight on it.
                            None => tracing::warn!(app = %app, id, "a result for no such call"),
                        }
                    }
                    Ok(AppFrame::Progress { id, fraction, message }) => {
                        if let Some((progress, _)) = calls.get(&id) {
                            progress.step(fraction, message);
                        }
                    }
                    Ok(AppFrame::Ui { id, asset }) => {
                        if let Some(reply) = uis.remove(&id) {
                            // An absent asset is a 404, not a failure. An
                            // asset that will not decode is the app's bug and
                            // is said out loud, because a silent 404 on a
                            // script is indistinguishable in a browser from a
                            // page that failed to render.
                            let decoded = match asset {
                                None => None,
                                Some(a) => match a.decode() {
                                    Ok(asset) => Some(asset),
                                    Err(e) => {
                                        tracing::error!(app = %app, error = %e, "an app sent an undecodable UI asset");
                                        None
                                    }
                                },
                            };
                            let _ = reply.send(decoded);
                        }
                    }
                    Ok(AppFrame::Log { level, message }) => {
                        // An app's own logging, kept distinguishable from the
                        // core's: `app` is on every line.
                        match level.as_str() {
                            "error" => tracing::error!(app = %app, "{message}"),
                            "warn" => tracing::warn!(app = %app, "{message}"),
                            "debug" | "trace" => tracing::debug!(app = %app, "{message}"),
                            _ => tracing::info!(app = %app, "{message}"),
                        }
                    }
                    Ok(AppFrame::Invoke { call, command, params }) => {
                        invocations.begin(call, command, params);
                    }
                    Ok(AppFrame::Commands { call }) => {
                        invocations.list(call);
                    }
                    // Registration happens once, inside the handshake. A
                    // second one is an app trying to change what it is after
                    // the manifest was checked against it.
                    Ok(AppFrame::Register(_)) => {
                        return "registered twice".to_string();
                    }
                }
            }
        }
    }
}
