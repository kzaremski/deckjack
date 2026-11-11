//! Write a Deckjack app.
//!
//! An app is a `deckjack-<name>` binary with three jobs: answer the core's
//! handshake, run the commands its manifest declares, and serve its own web
//! UI. None of that is interesting, and all of it is the same in every app,
//! so it is here instead of nine times over.
//!
//! The example below is `ignore` rather than compiled, and that is the shape
//! of the thing rather than a cop-out: it needs an `app.toml` and a `ui/`
//! folder beside the crate root, which only a real app has. The first-party
//! apps are the compiled version of it — `crates/deckjack-notes` is this
//! example with six commands instead of one.
//!
//! ```ignore
//! use deckjack_app::{App, Ctx};
//! use serde::Deserialize;
//!
//! #[derive(Deserialize, schemars::JsonSchema)]
//! struct Greet {
//!     name: String,
//! }
//!
//! #[derive(rust_embed::RustEmbed)]
//! #[folder = "ui"]
//! struct Ui;
//!
//! fn main() -> anyhow::Result<()> {
//!     App::new(include_str!("../app.toml"))?
//!         .ui::<Ui>()
//!         .command("demo.greet", |p: Greet, _cx: Ctx| async move {
//!             Ok(serde_json::json!({ "said": format!("hello {}", p.name) }))
//!         })
//!         .run()
//! }
//! ```
//!
//! # The manifest is the only source of truth
//!
//! [`App::new`] takes the app's own `app.toml`, embedded with `include_str!`,
//! and registers exactly what that file declares: the ids, the summaries and
//! above all the **risk levels**. Nothing about a command is stated twice.
//!
//! That is not only tidiness. The core compares an app's registration against
//! the manifest copy it discovered on disk and refuses the app outright if a
//! risk level differs — so an app whose code said `Write` where its manifest
//! said `Destructive` would not start at all. Reading the risk from the one
//! file makes that failure unreachable rather than caught.
//!
//! [`App::run`] also refuses to start when the two halves do not line up:
//! a declared command with no handler would appear in the palette and fail
//! when clicked, and a handler for an undeclared command is dead code that
//! the operator never approved. Both are startup errors naming the command.
//!
//! # What an app may assume
//!
//! Very little, and deliberately. The core clears the environment before
//! exec, so there is no inherited `PATH`, no terminal and no `HOME` but the
//! private one it is given. The process runs under Landlock and seccomp with
//! only what the manifest asked for and the operator approved: it cannot read
//! the working directory unless `[needs]` said so, cannot reach the network
//! unless `[needs]` named hosts, and can never open `.deckjack/`.
//!
//! An app reaches durable state through the bus, not the filesystem — which
//! is also why the UI talks to the core's own WebSocket rather than to its
//! app. See `ui/` in any of the first-party apps.
//!
//! # Calling the bus back
//!
//! An app can also *call* commands, not only serve them: [`Bus::invoke`],
//! reached from a handler as `cx.bus()`. It needs `invoke_commands = true`
//! under `[needs]` and `protocol = 2`, and without the capability the core
//! refuses every invocation with a message naming the key. That is deliberate
//! and it is not a formality: a program that could dispatch anything on the
//! bus would not be confined by anything else its manifest asked for, since
//! one of the things on the bus opens a terminal.
//!
//! [`Bus::commands`] is what this app may call, as the core filters it. Worth
//! asking rather than assuming: the answer depends on the grant the operator
//! gave this app, and that can be narrowed while the app is running.

use std::collections::{BTreeMap, HashMap};
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use deckjack_protocol::{
    AppFrame, AvailableCommand, CoreFrame, InvokeOutcome, Manifest, PROTOCOL_VERSION,
    RegisteredCommand, WireAsset, decode_frame, encode_frame,
};
use serde::de::DeserializeOwned;
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::sync::{mpsc, oneshot};

/// The socket to connect back on, set by the core.
pub const SOCKET_ENV: &str = "DECKJACK_APP_SOCKET";

/// Whether the core is holding this app to its manifest. `"1"` or `"0"`.
pub const CONFINED_ENV: &str = "DECKJACK_CONFINED";

type Handler = Arc<
    dyn Fn(Value, Ctx) -> Pin<Box<dyn Future<Output = Result<Value, String>> + Send>> + Send + Sync,
>;

/// What an app's assets look like to this crate. `rust_embed::RustEmbed`
/// provides it; nothing else needs to.
pub trait Assets: Send + Sync + 'static {
    fn get(path: &str) -> Option<Vec<u8>>;
}

impl<T: rust_embed::RustEmbed + Send + Sync + 'static> Assets for T {
    fn get(path: &str) -> Option<Vec<u8>> {
        T::get(path).map(|f| f.data.into_owned())
    }
}

type AssetFn = Arc<dyn Fn(&str) -> Option<Vec<u8>> + Send + Sync>;

/// What a running command is given.
#[derive(Clone)]
pub struct Ctx {
    id: u64,
    out: mpsc::UnboundedSender<String>,
    bus: Bus,
}

impl Ctx {
    /// The command bus, to call commands of this core's own or of another
    /// app's. Needs the `invoke_commands` capability; see the crate doc.
    pub fn bus(&self) -> &Bus {
        &self.bus
    }

    /// Report where a long command is. Dropped if nobody is watching, so a
    /// command is never throttled by its audience.
    pub fn progress(&self, fraction: Option<f32>, message: impl Into<String>) {
        self.send(&AppFrame::Progress {
            id: self.id,
            fraction,
            message: message.into(),
        });
    }

    /// A line for the core's log, tagged with this app's name.
    pub fn log(&self, level: &str, message: impl Into<String>) {
        self.send(&AppFrame::Log {
            level: level.to_string(),
            message: message.into(),
        });
    }

    fn send(&self, frame: &AppFrame) {
        if let Ok(line) = encode_frame(frame) {
            let _ = self.out.send(line);
        }
    }
}

// ---------------------------------------------------------------------------
// Calling the bus
// ---------------------------------------------------------------------------

/// What the core answered one invocation with.
enum Reply {
    Invoked(InvokeOutcome),
    Available(Vec<AvailableCommand>),
}

/// The command bus, from inside an app.
///
/// Cheap to clone and safe to use from several handlers at once: each call
/// takes its own number out of the counter and waits on its own channel.
///
/// The numbers are this app's own space, not the core's. The core numbers its
/// `Call` frames with `id` and an app numbers its invocations with `call`;
/// both start at 1 and mean different things, which is why they are different
/// fields and why nothing here ever looks at an `id`.
#[derive(Clone)]
pub struct Bus {
    out: mpsc::UnboundedSender<String>,
    next: Arc<AtomicU64>,
    waiting: Arc<Mutex<HashMap<u64, oneshot::Sender<Reply>>>>,
}

/// Why an invocation did not come back with a value.
///
/// [`CallError::Queued`] is the one worth handling separately: it is not a
/// failure, it means a person has to decide, and calling again only puts a
/// second copy in front of them.
#[derive(Debug, thiserror::Error)]
pub enum CallError {
    #[error("{command} was refused and calling it again will not help: {message}")]
    Refused { command: String, message: String },

    #[error(
        "{command} has not run: it is waiting for the operator's approval as {approval}. \
         Carry on with other work rather than calling it again"
    )]
    Queued { command: String, approval: String },

    #[error("{command} failed: {message}")]
    Failed { command: String, message: String },

    #[error("the core answered {command} with the wrong kind of frame")]
    WrongAnswer { command: String },

    #[error("the connection to the core ended while {command} was in flight")]
    Disconnected { command: String },
}

impl Bus {
    /// Call a command and wait for its value.
    ///
    /// Everything that is not a plain success — refused, queued, failed — is
    /// a [`CallError`], so the ordinary call site is `?`. An app that needs
    /// to tell those apart asks [`Bus::outcome`] instead.
    pub async fn invoke(&self, command: &str, params: Value) -> Result<Value, CallError> {
        match self.outcome(command, params).await? {
            InvokeOutcome::Ok { value } => Ok(value),
            InvokeOutcome::Queued { approval } => Err(CallError::Queued {
                command: command.to_string(),
                approval,
            }),
            InvokeOutcome::Refused { message } => Err(CallError::Refused {
                command: command.to_string(),
                message,
            }),
            InvokeOutcome::Failed { message } => Err(CallError::Failed {
                command: command.to_string(),
                message,
            }),
        }
    }

    /// Call a command and get the core's answer as it came, without folding
    /// a refusal and a failure into one shape. Only a transport problem is an
    /// error here.
    pub async fn outcome(&self, command: &str, params: Value) -> Result<InvokeOutcome, CallError> {
        let (call, answer) = self.register();
        self.send(&AppFrame::Invoke {
            call,
            command: command.to_string(),
            params,
        });
        match answer.await {
            Ok(Reply::Invoked(outcome)) => Ok(outcome),
            Ok(Reply::Available(_)) => Err(CallError::WrongAnswer {
                command: command.to_string(),
            }),
            // The sender is dropped when the frame loop ends, so this is the
            // connection going away rather than a lost reply. Said as such:
            // an app told "no answer" would retry into a socket that is gone.
            Err(_) => Err(CallError::Disconnected {
                command: command.to_string(),
            }),
        }
    }

    /// What this app may call right now, filtered by its grant.
    pub async fn commands(&self) -> Result<Vec<AvailableCommand>, CallError> {
        let (call, answer) = self.register();
        self.send(&AppFrame::Commands { call });
        match answer.await {
            Ok(Reply::Available(commands)) => Ok(commands),
            // The core answers a listing with a refusal when this app has no
            // capability, which is the honest answer to "what may I call":
            // nothing, and here is why.
            Ok(Reply::Invoked(InvokeOutcome::Refused { message })) => Err(CallError::Refused {
                command: "the command list".to_string(),
                message,
            }),
            Ok(Reply::Invoked(_)) => Err(CallError::WrongAnswer {
                command: "the command list".to_string(),
            }),
            Err(_) => Err(CallError::Disconnected {
                command: "the command list".to_string(),
            }),
        }
    }

    /// Take the next call number and the channel its answer will arrive on.
    fn register(&self) -> (u64, oneshot::Receiver<Reply>) {
        // Starts at 1, like the core's own counter, so 0 never appears and a
        // missing field cannot be mistaken for the first call.
        let call = self.next.fetch_add(1, Ordering::Relaxed) + 1;
        let (tx, rx) = oneshot::channel();
        // Held for one insertion and never across an await: a guard alive in
        // an async body makes the whole future non-Send, and a handler's
        // future has to be Send to be spawned.
        self.pending().insert(call, tx);
        (call, rx)
    }

    fn pending(&self) -> std::sync::MutexGuard<'_, HashMap<u64, oneshot::Sender<Reply>>> {
        // A poisoned lock is still usable here: every holder inserts or
        // removes one entry and leaves the map consistent.
        self.waiting.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn send(&self, frame: &AppFrame) {
        if let Ok(line) = encode_frame(frame) {
            let _ = self.out.send(line);
        }
    }

    /// Hand one answer to whoever is waiting for it.
    ///
    /// A call number this app never sent, or one already answered, is
    /// dropped rather than fatal: a core bug should not take down an app's
    /// connection and with it every command still in flight on it. A dropped
    /// receiver — the waiting task gave up — is the same, hence the ignored
    /// send.
    fn deliver(&self, call: u64, reply: Reply) {
        if let Some(tx) = self.pending().remove(&call) {
            let _ = tx.send(reply);
        }
    }
}

/// Fails every in-flight invocation when the frame loop ends.
///
/// Without this an app waiting on an invocation when the core closes the
/// socket would wait forever: the `oneshot::Sender` lives in the pending
/// table, the table lives behind an `Arc` the waiter itself holds, so nothing
/// would ever drop it. Clearing the table drops the senders, and each waiter
/// gets [`CallError::Disconnected`].
struct FailPending(Arc<Mutex<HashMap<u64, oneshot::Sender<Reply>>>>);

impl Drop for FailPending {
    fn drop(&mut self) {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clear();
    }
}

#[derive(Debug, thiserror::Error)]
pub enum AppError {
    #[error("this app's own app.toml is not valid: {0}")]
    Manifest(#[from] deckjack_protocol::ManifestError),

    #[error(
        "{command} is declared in app.toml but has no handler, so it would appear in the \
         palette and fail when clicked"
    )]
    NoHandler { command: String },

    #[error(
        "{command} has a handler but is not declared in app.toml, so the operator never \
         approved it and the core would refuse the registration"
    )]
    Undeclared { command: String },

    #[error(
        "{SOCKET_ENV} is not set: an app is started by the Deckjack core, which passes the \
         socket to connect back on. Run `deckjack` and let it launch this."
    )]
    NoSocket,

    #[error("cannot reach the core on {path}: {source}")]
    Connect {
        path: String,
        #[source]
        source: std::io::Error,
    },

    #[error(transparent)]
    Protocol(#[from] deckjack_protocol::ProtocolError),

    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// One app, under construction.
pub struct App {
    manifest: Manifest,
    handlers: BTreeMap<String, (Handler, schemars::Schema)>,
    assets: Option<AssetFn>,
}

impl App {
    /// Start from the app's own manifest, embedded with `include_str!`.
    pub fn new(manifest_toml: &str) -> Result<App, AppError> {
        let manifest = Manifest::parse(
            std::path::Path::new(deckjack_protocol::MANIFEST_FILE),
            manifest_toml,
        )?;
        Ok(App {
            manifest,
            handlers: BTreeMap::new(),
            assets: None,
        })
    }

    pub fn name(&self) -> &str {
        &self.manifest.app.name
    }

    /// The app's embedded web UI, served by the core at `/apps/<name>/`.
    ///
    /// # The debug-build trap
    ///
    /// `rust_embed` reads from disk in debug builds and only embeds in
    /// release. An app is launched confined, and its sandbox grants its own
    /// install directory — not the source tree it was compiled in — so a
    /// debug app binary cannot read its own `ui/` folder and serves 404 for
    /// every asset. That is a blank window with nothing in the log.
    ///
    /// So [`App::run`] checks at startup that the index is actually there and
    /// says so if it is not. Apps are installed artifacts; build them with
    /// `--release`.
    pub fn ui<A: Assets>(mut self) -> App {
        self.assets = Some(Arc::new(A::get));
        self
    }

    /// Handle one of the commands the manifest declares.
    ///
    /// The parameter type generates the JSON Schema the palette renders and
    /// an agent plans against, so it is worth being specific in: a
    /// `serde_json::Value` here is a command nobody can call without reading
    /// the source.
    pub fn command<P, F, Fut>(mut self, id: &str, handler: F) -> App
    where
        P: DeserializeOwned + schemars::JsonSchema + Send + 'static,
        F: Fn(P, Ctx) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<Value, String>> + Send + 'static,
    {
        let handler: Handler = Arc::new(move |params: Value, cx: Ctx| {
            // Deserialization is where a bad call is refused, and the message
            // has to say which field: the caller may be an agent that will
            // try again, and "invalid type" alone teaches it nothing.
            match serde_json::from_value::<P>(params) {
                Ok(params) => Box::pin(handler(params, cx)),
                Err(e) => Box::pin(async move { Err(format!("bad parameters: {e}")) }),
            }
        });
        let schema = schemars::schema_for!(P);
        self.handlers.insert(id.to_string(), (handler, schema));
        self
    }

    /// What this app will tell the core it provides.
    ///
    /// Every field comes from the manifest except the schema, which comes
    /// from the handler's parameter type.
    fn registration(&self) -> Result<Vec<RegisteredCommand>, AppError> {
        let mut out = Vec::with_capacity(self.manifest.commands.len());
        for declared in &self.manifest.commands {
            let Some((_, schema)) = self.handlers.get(&declared.id) else {
                return Err(AppError::NoHandler {
                    command: declared.id.clone(),
                });
            };
            out.push(RegisteredCommand {
                id: declared.id.clone(),
                summary: declared.summary.clone(),
                risk: declared.risk,
                params: serde_json::to_value(schema)
                    .unwrap_or_else(|_| serde_json::json!({ "type": "object", "properties": {} })),
            });
        }
        for id in self.handlers.keys() {
            if !self.manifest.commands.iter().any(|c| &c.id == id) {
                return Err(AppError::Undeclared {
                    command: id.clone(),
                });
            }
        }
        Ok(out)
    }

    /// Connect, register and serve until the core says stop.
    ///
    /// Builds its own runtime, so an app's `main` is an ordinary `fn main`.
    pub fn run(self) -> anyhow::Result<()> {
        let commands = self.registration()?;
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()?;
        runtime.block_on(self.serve(commands))?;
        Ok(())
    }

    async fn serve(self, commands: Vec<RegisteredCommand>) -> Result<(), AppError> {
        let path = std::env::var(SOCKET_ENV).map_err(|_| AppError::NoSocket)?;
        let stream = UnixStream::connect(&path)
            .await
            .map_err(|source| AppError::Connect {
                path: path.clone(),
                source,
            })?;
        let (read, mut write) = stream.into_split();
        let mut reader = BufReader::new(read);

        // One writer. Every reply, every progress line and every log line
        // goes through this channel, so two concurrent commands cannot
        // interleave halves of a frame on one line.
        let (out, mut outbox) = mpsc::unbounded_channel::<String>();
        tokio::spawn(async move {
            while let Some(line) = outbox.recv().await {
                if write.write_all(line.as_bytes()).await.is_err() {
                    return;
                }
                if write.flush().await.is_err() {
                    return;
                }
            }
        });

        // The core speaks first.
        let hello: CoreFrame = read_frame(&mut reader).await?;
        let CoreFrame::Hello(hello) = hello else {
            return Err(deckjack_protocol::ProtocolError::Malformed(
                "expected a hello frame first".to_string(),
            )
            .into());
        };
        deckjack_protocol::check_protocol(self.name(), hello.protocol)?;

        // Said before registering, so it is in the log above the app's first
        // command rather than after whatever failed.
        if let Some(get) = &self.assets
            && get(deckjack_protocol::UI_INDEX).is_none()
        {
            {
                send(
                    &out,
                    &AppFrame::Log {
                        level: "error".to_string(),
                        message: format!(
                            "this build has no {} embedded, so /apps/{}/ will serve 404 for \
                             everything. rust_embed only embeds in release builds: rebuild \
                             with --release and reinstall.",
                            deckjack_protocol::UI_INDEX,
                            self.manifest.app.name
                        ),
                    },
                );
            }
        }

        send(
            &out,
            &AppFrame::Register(deckjack_protocol::Registration {
                protocol: PROTOCOL_VERSION,
                app: self.manifest.app.name.clone(),
                version: self.manifest.app.version.clone(),
                commands,
            }),
        );

        let handlers = Arc::new(self.handlers);
        let assets = self.assets.clone();

        let bus = Bus {
            out: out.clone(),
            next: Arc::new(AtomicU64::new(0)),
            waiting: Arc::new(Mutex::new(HashMap::new())),
        };
        // Dropped on every exit from this loop, including the error paths, so
        // an invocation in flight when the core goes away fails rather than
        // hanging.
        let _fail_pending = FailPending(Arc::clone(&bus.waiting));

        loop {
            let frame: CoreFrame = match read_frame(&mut reader).await {
                Ok(frame) => frame,
                // The core closing the socket is how an app is told to go
                // away when the core itself is going away. Not an error.
                Err(AppError::Protocol(deckjack_protocol::ProtocolError::Closed)) => return Ok(()),
                Err(e) => return Err(e),
            };

            match frame {
                CoreFrame::Stop => return Ok(()),
                CoreFrame::Hello(_) => {
                    // The handshake happens once. A second hello is the core
                    // confused about which connection this is.
                    return Err(deckjack_protocol::ProtocolError::Malformed(
                        "a second hello on one connection".to_string(),
                    )
                    .into());
                }
                CoreFrame::Call {
                    id,
                    command,
                    params,
                } => {
                    let cx = Ctx {
                        id,
                        out: out.clone(),
                        bus: bus.clone(),
                    };
                    match handlers.get(&command) {
                        Some((handler, _)) => {
                            // Spawned, so a slow command does not stop this
                            // loop reading the next one. An app that wants to
                            // serialize its own work does that inside its
                            // handlers.
                            let future = handler(params, cx.clone());
                            let out = out.clone();
                            tokio::spawn(async move {
                                let (result, error) = match future.await {
                                    Ok(value) => (Some(value), None),
                                    Err(message) => (None, Some(message)),
                                };
                                send(&out, &AppFrame::Result { id, result, error });
                            });
                        }
                        // Registered-but-unhandled is unreachable —
                        // `registration` refuses to start — so this is the
                        // core asking for something it was never told about.
                        None => send(
                            &out,
                            &AppFrame::Result {
                                id,
                                result: None,
                                error: Some(format!("{command} is not a command of this app")),
                            },
                        ),
                    }
                }
                CoreFrame::Cancel { id: _ } => {
                    // TODO(cancel): handlers have no cancellation signal yet,
                    // so this is dropped rather than answered. Saying so here
                    // beats a silent ignore: a cancel that does nothing is
                    // the same bug the core has at its own wiring seam.
                }
                CoreFrame::Ui { id, path } => {
                    let asset = assets
                        .as_ref()
                        .and_then(|get| ui_asset(get.as_ref(), &path));
                    send(&out, &AppFrame::Ui { id, asset });
                }
                // The two answers to this app's own frames. Routed by `call`,
                // which is this app's number and never the core's `id`.
                CoreFrame::Invoked { call, outcome } => {
                    bus.deliver(call, Reply::Invoked(outcome));
                }
                CoreFrame::Available { call, commands } => {
                    bus.deliver(call, Reply::Available(commands));
                }
            }
        }
    }
}

fn send(out: &mpsc::UnboundedSender<String>, frame: &AppFrame) {
    if let Ok(line) = encode_frame(frame) {
        let _ = out.send(line);
    }
}

async fn read_frame<T: DeserializeOwned>(
    reader: &mut BufReader<tokio::net::unix::OwnedReadHalf>,
) -> Result<T, AppError> {
    let mut line = String::new();
    let read = reader.read_line(&mut line).await?;
    if read == 0 {
        return Err(deckjack_protocol::ProtocolError::Closed.into());
    }
    Ok(decode_frame(line.trim_end())?)
}

/// One asset, as a frame carries it.
///
/// An empty path and `index.html` are the same request: the core resolves
/// `/apps/<name>/` to the index, and an app that answered one and not the
/// other would serve a blank window for the ordinary case.
fn ui_asset(
    get: &(dyn Fn(&str) -> Option<Vec<u8>> + Send + Sync),
    path: &str,
) -> Option<WireAsset> {
    let path = path.trim_start_matches('/');
    let path = if path.is_empty() {
        deckjack_protocol::UI_INDEX
    } else {
        path
    };
    let bytes = get(path)?;
    // The app states the type rather than leaving the core to guess from an
    // extension: the app is the half that knows its own build.
    let content_type = mime_guess::from_path(path)
        .first_or_octet_stream()
        .to_string();
    // `index.html` has one name and different contents every build, so it is
    // the one asset that must never be cached. Vite's hashed filenames are
    // the opposite and are cached for a year.
    let immutable = path != deckjack_protocol::UI_INDEX;
    Some(WireAsset::new(content_type, &bytes, immutable))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------
//
// The frame loop needs a core on the other end of a socket, and the test that
// drives a real one is `deckjack-core`'s `tests/agentbridge.rs`: this crate is
// not a dependency of the core, so it cannot be driven from there. What is
// worth testing here is the half that is this crate's alone — the call
// numbering and the routing of an answer back to the right waiter — and that
// needs no socket.

#[cfg(test)]
mod tests {
    use super::*;

    /// A bus handle plus the frames it writes.
    fn handle() -> (Bus, mpsc::UnboundedReceiver<String>) {
        let (out, rx) = mpsc::unbounded_channel();
        let bus = Bus {
            out,
            next: Arc::new(AtomicU64::new(0)),
            waiting: Arc::new(Mutex::new(HashMap::new())),
        };
        (bus, rx)
    }

    fn sent(rx: &mut mpsc::UnboundedReceiver<String>) -> AppFrame {
        let line = rx.try_recv().expect("the app wrote no frame");
        decode_frame(line.trim_end()).expect("the app wrote a frame it cannot read back")
    }

    #[tokio::test]
    async fn an_invocation_is_sent_as_an_invoke_frame_with_its_own_call_number() {
        // The first call number must be 1 and the frame must be `invoke`. If
        // it went out as something else the core would drop the connection,
        // and if it went out as call 0 a core that treats a missing field as
        // zero could match it to the wrong waiter.
        let (bus, mut rx) = handle();
        let waiting = tokio::spawn({
            let bus = bus.clone();
            async move {
                bus.invoke("files.list", serde_json::json!({ "at": "." }))
                    .await
            }
        });

        // Spun rather than slept on: the spawned task has to reach its first
        // await before the frame exists.
        let frame = loop {
            if let Ok(line) = rx.try_recv() {
                break decode_frame::<AppFrame>(line.trim_end()).unwrap();
            }
            tokio::task::yield_now().await;
        };
        let AppFrame::Invoke {
            call,
            command,
            params,
        } = frame
        else {
            panic!("expected an invoke frame, got {frame:?}");
        };
        assert_eq!(call, 1);
        assert_eq!(command, "files.list");
        assert_eq!(params["at"], ".");

        bus.deliver(
            call,
            Reply::Invoked(InvokeOutcome::Ok {
                value: serde_json::json!(["one"]),
            }),
        );
        assert_eq!(waiting.await.unwrap().unwrap()[0], "one");
    }

    #[tokio::test]
    async fn two_invocations_in_flight_each_get_their_own_answer() {
        // The reason the id spaces are separate at all. If a second
        // invocation reused the first's number, or an answer were matched by
        // arrival order instead of by number, one command's value would be
        // returned as another's — a wrong answer presented as a right one.
        let (bus, _rx) = handle();
        let (one, two) = (bus.clone(), bus.clone());
        let first = tokio::spawn(async move { one.invoke("a.one", Value::Null).await });
        let second = tokio::spawn(async move { two.invoke("a.two", Value::Null).await });

        while bus.pending().len() < 2 {
            tokio::task::yield_now().await;
        }

        // Answered in the opposite order to prove nothing depends on arrival
        // order.
        bus.deliver(
            2,
            Reply::Invoked(InvokeOutcome::Ok {
                value: serde_json::json!("second"),
            }),
        );
        bus.deliver(
            1,
            Reply::Invoked(InvokeOutcome::Ok {
                value: serde_json::json!("first"),
            }),
        );
        assert_eq!(first.await.unwrap().unwrap(), "first");
        assert_eq!(second.await.unwrap().unwrap(), "second");
    }

    #[tokio::test]
    async fn a_refusal_reaches_the_app_as_a_refusal_and_not_as_a_failure() {
        // An app has to be able to tell "you were never allowed to call this"
        // from "it broke": the first is an install problem for the operator
        // and must never be retried, the second may be.
        let (bus, _rx) = handle();
        let waiting = tokio::spawn({
            let bus = bus.clone();
            async move { bus.invoke("terminal.open", Value::Null).await }
        });
        while bus.pending().is_empty() {
            tokio::task::yield_now().await;
        }
        bus.deliver(
            1,
            Reply::Invoked(InvokeOutcome::Refused {
                message: "notes did not declare needs.invoke_commands".to_string(),
            }),
        );
        let error = waiting
            .await
            .unwrap()
            .expect_err("a refusal returned a value");
        assert!(matches!(error, CallError::Refused { .. }), "{error}");
        assert!(error.to_string().contains("invoke_commands"), "{error}");
    }

    #[tokio::test]
    async fn an_invocation_in_flight_when_the_loop_ends_fails_instead_of_hanging() {
        // A command handler that awaits an invocation forever is a command
        // that never answers, which the core reports as the app disconnecting
        // while running it — the app's own log would say nothing at all.
        let (bus, _rx) = handle();
        let fail = FailPending(Arc::clone(&bus.waiting));
        let waiting = tokio::spawn({
            let bus = bus.clone();
            async move { bus.invoke("files.list", Value::Null).await }
        });
        while bus.pending().is_empty() {
            tokio::task::yield_now().await;
        }
        drop(fail);
        let error = waiting
            .await
            .unwrap()
            .expect_err("an invocation survived the connection ending");
        assert!(matches!(error, CallError::Disconnected { .. }), "{error}");
    }

    #[tokio::test]
    async fn a_command_listing_refused_for_the_capability_says_so() {
        // `commands()` answered with an empty list would send an app author
        // looking for missing commands rather than the missing capability.
        let (bus, mut rx) = handle();
        let waiting = tokio::spawn({
            let bus = bus.clone();
            async move { bus.commands().await }
        });
        while bus.pending().is_empty() {
            tokio::task::yield_now().await;
        }
        assert!(matches!(sent(&mut rx), AppFrame::Commands { call: 1 }));
        bus.deliver(
            1,
            Reply::Invoked(InvokeOutcome::Refused {
                message: "notes did not declare needs.invoke_commands".to_string(),
            }),
        );
        let error = waiting
            .await
            .unwrap()
            .expect_err("a refused listing returned commands");
        assert!(error.to_string().contains("invoke_commands"), "{error}");
    }
}
