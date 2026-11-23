//! The UI protocol's relay: windows an app describes, between the shell's
//! socket and the app's.
//!
//! An app on the UI protocol (`tier = 1` in its manifest) never talks to the
//! browser. The shell opens a window over its own WebSocket (`ClientFrame::Ui`
//! with `{"op": "open", "app": …}`), this relay asks the app for the window's
//! tree over the app's Unix socket (`CoreFrame::UiOpen`), and the app's
//! patches (`AppFrame::UiPatch`) come back here and go out to the connections
//! showing that window. Events go the other way. Nothing else moves.
//!
//! # What the relay checks
//!
//! - **The session**, by construction: a `ui` frame only arrives on a socket
//!   whose session cookie the upgrade verified, and `ws` closes the socket
//!   when the session ends.
//! - **The single seat**: every upstream frame is admitted by the same seat
//!   the bus adapter asks ([`UiGate::admit`]), and patches go only to
//!   connections the seat admits at that moment ([`UiGate::admits_now`]). A
//!   device that was taken over stops receiving its windows' trees.
//! - **Which app owns a window**: decided by the first open and fixed for the
//!   window's life. A patch from any other app for that window is dropped, so
//!   one app can never draw into another's window.
//!
//! # The mirror
//!
//! The relay keeps every window's current tree, by applying the same patches
//! it forwards. Three things need it: `ui.snapshot` (an agent or a test
//! reading a window as data, secrets withheld), a second open of a window
//! that is already drawn (served from here, with no round trip to the app),
//! and a connection that missed a frame (sent the whole tree next instead of
//! patches it could not apply).

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, Weak};

use deckjack_protocol::ui::{self, Event, Node, Patch};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::command::{Command, CommandError, Context, Registry, RegistryError, RiskLevel};
use crate::ws::{self, ConnectionId};

/// What the relay asks of the app host.
pub trait UiApps: Send + Sync + 'static {
    /// Whether `app` is installed and describes its window (`tier = 1`).
    fn is_described(&self, app: &str) -> bool;
    /// Send one frame to a connected app. False when it is not connected.
    fn send_ui(&self, app: &str, frame: ToApp) -> bool;
}

/// A UI protocol frame on its way to an app.
#[derive(Debug, Clone, PartialEq)]
pub enum ToApp {
    Open { window: String },
    Event { window: String, event: Event },
    Close { window: String },
}

/// Where the relay's frames to browsers go: [`ws::Hub`] in the product.
pub trait UiSink: Send + Sync + 'static {
    /// `None`: no such connection. `Some(false)`: dropped, queue full.
    fn deliver(&self, connection: ConnectionId, window: &str, body: Value) -> Option<bool>;
}

impl UiSink for ws::Hub {
    fn deliver(&self, connection: ConnectionId, window: &str, body: Value) -> Option<bool> {
        self.send_ui(connection, window, body)
    }
}

/// The single seat, as the relay asks it: [`crate::seat::Seat`].
pub trait UiGate: Send + Sync + 'static {
    /// May this connection use the workspace now? On refusal, the seat's
    /// status, for the shell's takeover screen.
    fn admit(&self, connection: ConnectionId) -> Result<(), Value>;
    /// Does it hold the seat at this moment? Asked before every push.
    fn admits_now(&self, connection: ConnectionId) -> bool;
    /// Did its hello name a device, which only the shell's does? An app's
    /// own page talking over its own socket (an attendant) names none, and
    /// may not open, drive or watch any window: the hard-deny list on `ui.*`
    /// only governs `as_app` calls, not frames on a socket of its own.
    fn is_device(&self, connection: ConnectionId) -> bool;
}

impl UiGate for crate::seat::Seat {
    fn admit(&self, connection: ConnectionId) -> Result<(), Value> {
        crate::seat::Seat::admit(self, connection).map_err(|status| status.to_json())
    }

    fn admits_now(&self, connection: ConnectionId) -> bool {
        crate::seat::Seat::admits_now(self, connection)
    }

    fn is_device(&self, connection: ConnectionId) -> bool {
        crate::seat::Seat::names_device(self, connection)
    }
}

/// One window an app describes.
struct WindowUi {
    app: String,
    /// Connections showing it, each with whether it missed a frame and so
    /// needs the whole tree rather than the next patches.
    subscribers: HashMap<ConnectionId, bool>,
    /// The tree as the app last left it. `None` until the app's first frame
    /// after an open, and again whenever the app restarts or sends a patch
    /// that does not fit.
    tree: Option<Node>,
    rev: u64,
    /// The app's last whole tree was refused (too deep, too large, or its
    /// own patches did not fit it). Its incremental frames are dropped
    /// without asking again, which would only loop; a shell's next open
    /// asks once more.
    refused: bool,
}

/// What a browser may say on a window's channel.
#[derive(Debug, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
enum Upstream {
    /// Show this window, which `app` describes. Also how a shell that lost
    /// track asks for the whole tree again.
    Open {
        app: String,
    },
    Event {
        event: Event,
    },
    /// The window is gone from the workspace.
    Close,
}

/// A delivery decided under the lock and made after it.
enum Deliver {
    Browser(ConnectionId, String, Value),
    App(String, ToApp),
}

/// The relay. One per node.
pub struct UiChannel {
    windows: Mutex<HashMap<String, WindowUi>>,
    sink: Arc<dyn UiSink>,
    gate: Arc<dyn UiGate>,
    apps: std::sync::OnceLock<Weak<dyn UiApps>>,
}

/// The longest window id accepted. The shell's are short; this bounds what a
/// frame can make the relay store.
const WINDOW_MAX: usize = 128;

impl UiChannel {
    pub fn new(sink: Arc<dyn UiSink>, gate: Arc<dyn UiGate>) -> Arc<UiChannel> {
        Arc::new(UiChannel {
            windows: Mutex::new(HashMap::new()),
            sink,
            gate,
            apps: std::sync::OnceLock::new(),
        })
    }

    /// The app host, attached once it exists (it is built after the relay's
    /// other half, and holds the relay too, hence the weak reference).
    pub fn attach_apps(&self, apps: Weak<dyn UiApps>) {
        if self.apps.set(apps).is_err() {
            tracing::warn!("the UI relay already has an app host; the second was ignored");
        }
    }

    fn apps(&self) -> Option<Arc<dyn UiApps>> {
        self.apps.get().and_then(Weak::upgrade)
    }

    /// Short and leaves the table consistent, so a poisoned lock is usable.
    fn windows(&self) -> MutexGuard<'_, HashMap<String, WindowUi>> {
        self.windows.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn flush(&self, deliveries: Vec<Deliver>) {
        let apps = self.apps();
        for d in deliveries {
            match d {
                Deliver::Browser(connection, window, body) => {
                    if self.sink.deliver(connection, &window, body).is_none() {
                        // The connection closed: it shows nothing any more.
                        if let Some(w) = self.windows().get_mut(&window) {
                            w.subscribers.remove(&connection);
                        }
                    }
                }
                Deliver::App(app, frame) => {
                    if !apps.as_ref().is_some_and(|a| a.send_ui(&app, frame)) {
                        tracing::debug!(%app, "UI frame not sent: the app is not connected");
                    }
                }
            }
        }
    }

    /// The whole tree as one frame body.
    fn snapshot_body(w: &WindowUi, ack: Option<u64>) -> Option<Value> {
        let tree = w.tree.as_ref()?;
        Some(json!({
            "rev": w.rev,
            "ack": ack,
            "patches": [Patch::Replace { path: Vec::new(), node: tree.clone() }],
        }))
    }

    /// A refusal on the window's channel, for the shell to show.
    fn error(connection: ConnectionId, window: &str, code: &str, message: String) -> Deliver {
        Deliver::Browser(
            connection,
            window.to_string(),
            json!({ "error": { "code": code, "message": message } }),
        )
    }

    // -----------------------------------------------------------------------
    // From browsers
    // -----------------------------------------------------------------------

    fn handle_upstream(&self, connection: ConnectionId, window: String, body: Value) {
        if window.is_empty() || window.len() > WINDOW_MAX {
            tracing::debug!(%connection, "ui frame refused: bad window id");
            return;
        }
        // Only the shell: an app's own page, on a socket of its own, may
        // not reach another app's window (or its own) this way.
        if !self.gate.is_device(connection) {
            let body = json!({ "error": {
                "code": "not_a_device",
                "message": "only the shell may open a described window; this connection named no device",
            }});
            let _ = self.sink.deliver(connection, &window, body);
            return;
        }
        // The seat first, before anything is looked at or stored: a device
        // that does not hold it may not drive a window.
        if let Err(status) = self.gate.admit(connection) {
            let body = json!({ "error": {
                "code": crate::seat::NOT_HELD,
                "message": "another device is using this workspace; nothing was sent",
                "detail": status,
            }});
            let _ = self.sink.deliver(connection, &window, body);
            return;
        }
        let op = match serde_json::from_value::<Upstream>(body) {
            Ok(op) => op,
            Err(e) => {
                self.flush(vec![Self::error(
                    connection,
                    &window,
                    "bad_frame",
                    format!("not a UI protocol frame: {e}"),
                )]);
                return;
            }
        };
        let deliveries = match op {
            Upstream::Open { app } => self.open(connection, window, app),
            Upstream::Event { event } => self.event(connection, window, event),
            Upstream::Close => self.close(window),
        };
        self.flush(deliveries);
    }

    fn open(&self, connection: ConnectionId, window: String, app: String) -> Vec<Deliver> {
        if !deckjack_protocol::valid_name(&app)
            || !self.apps().is_some_and(|a| a.is_described(&app))
        {
            return vec![Self::error(
                connection,
                &window,
                "not_described",
                format!("{app} is not an installed app that describes its window"),
            )];
        }
        let mut windows = self.windows();
        let w = windows.entry(window.clone()).or_insert_with(|| WindowUi {
            app: app.clone(),
            subscribers: HashMap::new(),
            tree: None,
            rev: 0,
            refused: false,
        });
        if w.app != app {
            return vec![Self::error(
                connection,
                &window,
                "wrong_app",
                format!("window {window} belongs to {}, not {app}", w.app),
            )];
        }
        w.subscribers.insert(connection, false);
        match Self::snapshot_body(w, None) {
            // Already drawn: served from the mirror, no round trip. Sent
            // under the lock, as `patched` sends, so a patch the app emits
            // meanwhile reaches this connection after the tree it applies
            // to and never before it (a newer patch followed by an older
            // whole tree would move the window backwards). The app is told
            // the window was shown, so it can read again what may have
            // changed behind its back; what it changes arrives as patches.
            Some(body) => {
                match self.sink.deliver(connection, &window, body) {
                    None => {
                        w.subscribers.remove(&connection);
                    }
                    Some(sent) => {
                        w.subscribers.insert(connection, !sent);
                    }
                }
                let event = Event {
                    path: Vec::new(),
                    kind: ui::EventKind::Opened,
                    value: Value::Null,
                    seq: None,
                };
                vec![Deliver::App(app, ToApp::Event { window, event })]
            }
            // Not yet: the app is asked, and its first frame reaches every
            // subscriber. If it is not connected yet, it is asked when it
            // connects (`app_connected`).
            None => {
                w.refused = false;
                vec![Deliver::App(app, ToApp::Open { window })]
            }
        }
    }

    fn event(&self, connection: ConnectionId, window: String, event: Event) -> Vec<Deliver> {
        let windows = self.windows();
        let Some(w) = windows.get(&window) else {
            return vec![Self::error(
                connection,
                &window,
                "not_open",
                format!("window {window} is not open; open it first"),
            )];
        };
        if !w.subscribers.contains_key(&connection) {
            return vec![Self::error(
                connection,
                &window,
                "not_open",
                format!("this connection has not opened window {window}"),
            )];
        }
        vec![Deliver::App(w.app.clone(), ToApp::Event { window, event })]
    }

    fn close(&self, window: String) -> Vec<Deliver> {
        match self.windows().remove(&window) {
            Some(w) => vec![Deliver::App(w.app, ToApp::Close { window })],
            None => Vec::new(),
        }
    }

    // -----------------------------------------------------------------------
    // From apps
    // -----------------------------------------------------------------------

    /// An app's `UiPatch`: apply it to the mirror and pass it on.
    pub fn patched(
        &self,
        app: &str,
        window: &str,
        rev: u64,
        ack: Option<u64>,
        patches: Vec<Patch>,
    ) {
        let mut deliveries = Vec::new();
        {
            let mut windows = self.windows();
            let Some(w) = windows.get_mut(window) else {
                // Closed while the frame was on its way, or never opened.
                tracing::debug!(app, window, "a patch for a window that is not open");
                return;
            };
            if w.app != app {
                tracing::warn!(
                    app,
                    owner = %w.app,
                    window,
                    "an app sent a patch for another app's window; dropped"
                );
                return;
            }
            let restart =
                matches!(patches.first(), Some(Patch::Replace { path, .. }) if path.is_empty());
            if !restart && w.refused {
                // Its last whole tree was refused; patches against it mean
                // nothing, and asking again would only loop.
                return;
            }
            let fits = restart || (w.tree.is_some() && rev == w.rev + 1);
            // Applied in place: on any refusal the mirror is thrown away
            // anyway, so it is never copied first.
            let applied = if !fits {
                Err(None)
            } else {
                let tree = match (&mut w.tree, restart) {
                    (Some(tree), false) => tree,
                    (slot, _) => slot.insert(Node::new("", ui::Stack::default())),
                };
                ui::apply_bounded(tree, &patches).map_err(Some)
            };
            match applied {
                Ok(()) => {
                    w.rev = rev;
                    w.refused = false;
                    let body = json!({ "rev": rev, "ack": ack, "patches": patches });
                    let subscribers: Vec<(ConnectionId, bool)> =
                        w.subscribers.iter().map(|(c, s)| (*c, *s)).collect();
                    for (connection, stale) in subscribers {
                        if !self.gate.admits_now(connection) {
                            // Taken over. Its shell shows the takeover screen
                            // and opens the window again when the seat comes
                            // back.
                            w.subscribers.remove(&connection);
                            continue;
                        }
                        let body = if stale || restart {
                            Self::snapshot_body(w, ack).unwrap_or_else(|| body.clone())
                        } else {
                            body.clone()
                        };
                        match self.sink.deliver(connection, window, body) {
                            None => {
                                w.subscribers.remove(&connection);
                            }
                            Some(sent) => {
                                w.subscribers.insert(connection, !sent);
                            }
                        }
                    }
                }
                // A whole tree that cannot be kept: too deep, too large, or
                // followed by patches that do not fit it. Asking again would
                // get the same tree, so the window is given up until a shell
                // opens it again, and the shells showing it are told.
                Err(e) if restart => {
                    let reason = e.map_or_else(|| "it did not fit".to_string(), |e| e.to_string());
                    tracing::warn!(app, window, %reason, "an app's whole tree was refused");
                    w.tree = None;
                    w.refused = true;
                    let message = format!("{app} sent a window the shell cannot show: {reason}");
                    for connection in w.subscribers.keys() {
                        deliveries.push(Self::error(
                            *connection,
                            window,
                            "ui_refused",
                            message.clone(),
                        ));
                        deliveries.push(Deliver::Browser(
                            *connection,
                            window.to_string(),
                            json!({ "status": "lost" }),
                        ));
                    }
                }
                // Lost track (a revision gap, a patch that does not fit), or
                // patches too costly to apply: ask for the whole tree again,
                // which costs nothing to apply. Until it comes, nobody is
                // sent patches against a tree nobody has.
                Err(e) => {
                    tracing::warn!(
                        app,
                        window,
                        rev,
                        have = w.rev,
                        reason = %e.map_or_else(|| "revision gap".to_string(), |e| e.to_string()),
                        "UI frame not applied; asking for the whole tree"
                    );
                    w.tree = None;
                    deliveries.push(Deliver::App(
                        app.to_string(),
                        ToApp::Open {
                            window: window.to_string(),
                        },
                    ));
                }
            }
        }
        self.flush(deliveries);
    }

    /// An app finished its handshake: every window it describes that a
    /// browser is showing is asked for afresh, since a new process has none
    /// of the old one's state.
    pub fn app_connected(&self, app: &str) {
        let deliveries: Vec<Deliver> = self
            .windows()
            .iter_mut()
            .filter(|(_, w)| w.app == app)
            .map(|(id, w)| {
                w.tree = None;
                w.rev = 0;
                w.refused = false;
                Deliver::App(app.to_string(), ToApp::Open { window: id.clone() })
            })
            .collect();
        self.flush(deliveries);
    }

    /// An app's connection ended: its windows have no tree until it is back,
    /// and the shells showing them are told.
    pub fn app_lost(&self, app: &str) {
        let mut deliveries = Vec::new();
        for (id, w) in self.windows().iter_mut().filter(|(_, w)| w.app == app) {
            w.tree = None;
            for connection in w.subscribers.keys() {
                deliveries.push(Deliver::Browser(
                    *connection,
                    id.clone(),
                    json!({ "status": "lost" }),
                ));
            }
        }
        self.flush(deliveries);
    }

    /// One window's tree as data, secrets withheld. `ui.snapshot`.
    pub fn snapshot(&self, window: &str) -> Option<Value> {
        let windows = self.windows();
        let w = windows.get(window)?;
        let tree = w.tree.as_ref()?;
        Some(json!({
            "window": window,
            "app": w.app,
            "rev": w.rev,
            "tree": ui::redacted(tree),
        }))
    }

    /// How many connections show `window`. For tests and the log.
    pub fn subscribers(&self, window: &str) -> usize {
        self.windows()
            .get(window)
            .map_or(0, |w| w.subscribers.len())
    }
}

impl ws::UiRelay for UiChannel {
    fn upstream(&self, connection: ConnectionId, window: String, body: Value) {
        self.handle_upstream(connection, window, body);
    }
}

// ---------------------------------------------------------------------------
// ui.snapshot
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SnapshotParams {
    /// The shell window's id.
    pub window: String,
}

/// `ui.snapshot`: a window an app describes, as structured data.
///
/// What an agent reads instead of a screenshot, and what a test asserts on.
/// Secret fields are withheld (`ui::redacted`). Under `ui.*`, which is in
/// `HARD_DENY`, so no app's window can call it to read another's.
pub struct Snapshot {
    channel: Arc<UiChannel>,
}

impl Command for Snapshot {
    const ID: &'static str = "ui.snapshot";
    const TITLE: &'static str = "Read a window's described UI";
    const RISK: RiskLevel = RiskLevel::ReadOnly;

    type Params = SnapshotParams;
    type Output = Value;

    async fn run(&self, p: SnapshotParams, _cx: Context) -> Result<Value, CommandError> {
        self.channel.snapshot(&p.window).ok_or_else(|| {
            CommandError::Failed(anyhow::anyhow!(
                "window {} has no described UI right now: it is not open, its app does not \
                 describe its window, or the app has not drawn it yet",
                p.window
            ))
        })
    }
}

/// Put `ui.snapshot` on the bus.
pub fn register(registry: &Registry, channel: &Arc<UiChannel>) -> Result<(), RegistryError> {
    registry.register(Snapshot {
        channel: Arc::clone(channel),
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[derive(Default)]
    struct Sink {
        sent: Mutex<Vec<(ConnectionId, String, Value)>>,
        gone: Mutex<HashSet<ConnectionId>>,
        full: Mutex<HashSet<ConnectionId>>,
    }

    impl UiSink for Sink {
        fn deliver(&self, connection: ConnectionId, window: &str, body: Value) -> Option<bool> {
            if self.gone.lock().unwrap().contains(&connection) {
                return None;
            }
            if self.full.lock().unwrap().contains(&connection) {
                return Some(false);
            }
            self.sent
                .lock()
                .unwrap()
                .push((connection, window.to_string(), body));
            Some(true)
        }
    }

    impl Sink {
        fn take(&self) -> Vec<(ConnectionId, String, Value)> {
            std::mem::take(&mut *self.sent.lock().unwrap())
        }
    }

    /// The seat, as a set of connections that may use the workspace.
    #[derive(Default)]
    struct Gate(Mutex<HashSet<ConnectionId>>);

    impl UiGate for Gate {
        fn admit(&self, connection: ConnectionId) -> Result<(), Value> {
            if self.0.lock().unwrap().contains(&connection) {
                Ok(())
            } else {
                Err(json!({ "held": false }))
            }
        }
        fn admits_now(&self, connection: ConnectionId) -> bool {
            self.0.lock().unwrap().contains(&connection)
        }
        fn is_device(&self, connection: ConnectionId) -> bool {
            connection != ATTENDANT
        }
    }

    #[derive(Default)]
    struct Apps {
        sent: Mutex<Vec<(String, ToApp)>>,
        offline: Mutex<bool>,
    }

    impl UiApps for Apps {
        fn is_described(&self, app: &str) -> bool {
            app == "tasks" || app == "other"
        }
        fn send_ui(&self, app: &str, frame: ToApp) -> bool {
            if *self.offline.lock().unwrap() {
                return false;
            }
            self.sent.lock().unwrap().push((app.to_string(), frame));
            true
        }
    }

    struct Rig {
        channel: Arc<UiChannel>,
        sink: Arc<Sink>,
        gate: Arc<Gate>,
        apps: Arc<Apps>,
    }

    const A: ConnectionId = ConnectionId(1);
    const B: ConnectionId = ConnectionId(2);
    /// An app's own page on its own socket: named no device.
    const ATTENDANT: ConnectionId = ConnectionId(3);

    fn rig() -> Rig {
        let sink = Arc::new(Sink::default());
        let gate = Arc::new(Gate::default());
        gate.0.lock().unwrap().insert(A);
        let apps = Arc::new(Apps::default());
        let channel = UiChannel::new(
            Arc::clone(&sink) as Arc<dyn UiSink>,
            Arc::clone(&gate) as Arc<dyn UiGate>,
        );
        let weak: Weak<dyn UiApps> = Arc::downgrade(&(Arc::clone(&apps) as Arc<dyn UiApps>));
        channel.attach_apps(weak);
        Rig {
            channel,
            sink,
            gate,
            apps,
        }
    }

    fn tree(text: &str) -> Node {
        Node::new("root", ui::Stack::default()).with([Node::new(
            "t",
            ui::Text {
                text: text.to_string(),
                ..ui::Text::default()
            },
        )])
    }

    fn replace(text: &str) -> Vec<Patch> {
        vec![Patch::Replace {
            path: vec![],
            node: tree(text),
        }]
    }

    fn props(text: &str) -> Vec<Patch> {
        vec![Patch::Props {
            path: vec!["t".into()],
            widget: ui::Text {
                text: text.to_string(),
                ..ui::Text::default()
            }
            .into(),
        }]
    }

    impl Rig {
        fn up(&self, c: ConnectionId, window: &str, body: Value) {
            self.channel.handle_upstream(c, window.to_string(), body);
        }
        fn app_frames(&self) -> Vec<(String, ToApp)> {
            std::mem::take(&mut *self.apps.sent.lock().unwrap())
        }
    }

    #[test]
    fn an_open_asks_the_app_and_its_first_frame_reaches_the_shell() {
        let r = rig();
        r.up(A, "w1", json!({ "op": "open", "app": "tasks" }));
        assert_eq!(
            r.app_frames(),
            vec![(
                "tasks".into(),
                ToApp::Open {
                    window: "w1".into()
                }
            )]
        );
        r.channel.patched("tasks", "w1", 1, None, replace("hi"));
        let sent = r.sink.take();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].0, A);
        assert_eq!(sent[0].2["rev"], 1);
        // The mirror followed, and the snapshot reads it.
        let snap = r.channel.snapshot("w1").unwrap();
        assert_eq!(snap["tree"]["children"][0]["widget"]["text"], "hi");
    }

    #[test]
    fn a_second_open_is_served_from_the_mirror_without_asking_the_app() {
        let r = rig();
        r.up(A, "w1", json!({ "op": "open", "app": "tasks" }));
        r.channel.patched("tasks", "w1", 1, None, replace("hi"));
        r.channel.patched("tasks", "w1", 2, None, props("there"));
        r.app_frames();
        r.sink.take();
        r.up(A, "w1", json!({ "op": "open", "app": "tasks" }));
        // The app is not asked for the tree; it is told the window was shown,
        // so it can read again what may have changed behind its back.
        let frames = r.app_frames();
        let [(app, ToApp::Event { window, event })] = &frames[..] else {
            panic!("{frames:?}");
        };
        assert_eq!((app.as_str(), window.as_str()), ("tasks", "w1"));
        assert_eq!(event.kind, ui::EventKind::Opened);
        let sent = r.sink.take();
        assert_eq!(sent[0].2["rev"], 2);
        assert_eq!(
            sent[0].2["patches"][0]["node"]["children"][0]["widget"]["text"],
            "there"
        );
    }

    #[test]
    fn a_connection_without_the_seat_can_neither_drive_nor_see_a_window() {
        let r = rig();
        // B does not hold the seat: refused before anything reaches the app.
        r.up(B, "w1", json!({ "op": "open", "app": "tasks" }));
        assert!(r.app_frames().is_empty());
        let sent = r.sink.take();
        assert_eq!(sent[0].2["error"]["code"], crate::seat::NOT_HELD);
        // The control: A may.
        r.up(A, "w1", json!({ "op": "open", "app": "tasks" }));
        assert_eq!(r.app_frames().len(), 1);
        r.channel.patched("tasks", "w1", 1, None, replace("hi"));
        assert_eq!(r.sink.take().len(), 1);
        // A loses the seat: the next patch does not reach it, and it is no
        // longer subscribed.
        r.gate.0.lock().unwrap().remove(&A);
        r.channel.patched("tasks", "w1", 2, None, props("secret"));
        assert!(r.sink.take().is_empty());
        assert_eq!(r.channel.subscribers("w1"), 0);
        // And its events are refused, not forwarded.
        r.up(
            A,
            "w1",
            json!({ "op": "event", "event": { "path": ["t"], "kind": "activated" } }),
        );
        assert!(r.app_frames().is_empty());
    }

    #[test]
    fn an_app_cannot_draw_into_another_apps_window() {
        let r = rig();
        r.up(A, "w1", json!({ "op": "open", "app": "tasks" }));
        r.channel.patched("tasks", "w1", 1, None, replace("mine"));
        r.sink.take();
        r.channel.patched("other", "w1", 1, None, replace("forged"));
        assert!(r.sink.take().is_empty());
        assert_eq!(
            r.channel.snapshot("w1").unwrap()["tree"]["children"][0]["widget"]["text"],
            "mine"
        );
        // Nor can a shell reopen it as the other app.
        r.up(A, "w1", json!({ "op": "open", "app": "other" }));
        assert_eq!(r.sink.take()[0].2["error"]["code"], "wrong_app");
    }

    #[test]
    fn only_an_installed_described_app_can_be_opened() {
        let r = rig();
        r.up(A, "w1", json!({ "op": "open", "app": "filer" }));
        assert_eq!(r.sink.take()[0].2["error"]["code"], "not_described");
        assert!(r.app_frames().is_empty());
        assert_eq!(r.channel.subscribers("w1"), 0);
    }

    #[test]
    fn events_go_to_the_owning_app_only_from_a_connection_showing_the_window() {
        let r = rig();
        let event =
            json!({ "op": "event", "event": { "path": ["t"], "kind": "activated", "seq": 4 } });
        r.up(A, "w1", event.clone());
        assert_eq!(r.sink.take()[0].2["error"]["code"], "not_open");
        r.up(A, "w1", json!({ "op": "open", "app": "tasks" }));
        r.app_frames();
        r.up(A, "w1", event);
        let frames = r.app_frames();
        let [(app, ToApp::Event { window, event })] = &frames[..] else {
            panic!("{frames:?}");
        };
        assert_eq!(
            (app.as_str(), window.as_str(), event.seq),
            ("tasks", "w1", Some(4))
        );
    }

    #[test]
    fn a_revision_gap_asks_the_app_again_and_sends_nobody_a_patch() {
        let r = rig();
        r.up(A, "w1", json!({ "op": "open", "app": "tasks" }));
        r.channel.patched("tasks", "w1", 1, None, replace("hi"));
        r.app_frames();
        r.sink.take();
        r.channel.patched("tasks", "w1", 3, None, props("skipped"));
        assert!(r.sink.take().is_empty());
        assert_eq!(
            r.app_frames(),
            vec![(
                "tasks".into(),
                ToApp::Open {
                    window: "w1".into()
                }
            )]
        );
        assert!(r.channel.snapshot("w1").is_none());
    }

    #[test]
    fn a_connection_that_missed_a_frame_gets_the_whole_tree_next() {
        let r = rig();
        r.up(A, "w1", json!({ "op": "open", "app": "tasks" }));
        r.channel.patched("tasks", "w1", 1, None, replace("one"));
        r.sink.take();
        r.sink.full.lock().unwrap().insert(A);
        r.channel.patched("tasks", "w1", 2, None, props("two"));
        r.sink.full.lock().unwrap().remove(&A);
        r.channel.patched("tasks", "w1", 3, None, props("three"));
        let sent = r.sink.take();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].2["patches"][0]["op"], "replace");
        assert_eq!(sent[0].2["rev"], 3);
        // And then patches again.
        r.channel.patched("tasks", "w1", 4, None, props("four"));
        assert_eq!(r.sink.take()[0].2["patches"][0]["op"], "props");
    }

    #[test]
    fn an_app_restart_reopens_its_windows_and_tells_the_shell_meanwhile() {
        let r = rig();
        r.up(A, "w1", json!({ "op": "open", "app": "tasks" }));
        r.channel.patched("tasks", "w1", 1, None, replace("one"));
        r.app_frames();
        r.sink.take();
        r.channel.app_lost("tasks");
        assert_eq!(r.sink.take()[0].2["status"], "lost");
        assert!(r.channel.snapshot("w1").is_none());
        r.channel.app_connected("tasks");
        assert_eq!(
            r.app_frames(),
            vec![(
                "tasks".into(),
                ToApp::Open {
                    window: "w1".into()
                }
            )]
        );
    }

    #[test]
    fn closing_tells_the_app_and_forgets_the_window() {
        let r = rig();
        r.up(A, "w1", json!({ "op": "open", "app": "tasks" }));
        r.app_frames();
        r.up(A, "w1", json!({ "op": "close" }));
        assert_eq!(
            r.app_frames(),
            vec![(
                "tasks".into(),
                ToApp::Close {
                    window: "w1".into()
                }
            )]
        );
        assert_eq!(r.channel.subscribers("w1"), 0);
    }

    #[test]
    fn an_app_not_connected_yet_is_asked_when_it_connects() {
        let r = rig();
        *r.apps.offline.lock().unwrap() = true;
        r.up(A, "w1", json!({ "op": "open", "app": "tasks" }));
        assert!(r.app_frames().is_empty());
        *r.apps.offline.lock().unwrap() = false;
        r.channel.app_connected("tasks");
        assert_eq!(r.app_frames().len(), 1);
    }

    #[test]
    fn a_closed_connection_is_unsubscribed() {
        let r = rig();
        r.up(A, "w1", json!({ "op": "open", "app": "tasks" }));
        r.sink.gone.lock().unwrap().insert(A);
        r.channel.patched("tasks", "w1", 1, None, replace("one"));
        assert_eq!(r.channel.subscribers("w1"), 0);
    }

    #[test]
    fn the_snapshot_withholds_secrets() {
        let r = rig();
        r.up(A, "w1", json!({ "op": "open", "app": "tasks" }));
        let node = Node::new("root", ui::Stack::default()).with([Node::new(
            "pw",
            ui::TextField {
                name: "pw".into(),
                value: "hunter2".into(),
                secret: true,
                ..ui::TextField::default()
            },
        )]);
        r.channel.patched(
            "tasks",
            "w1",
            1,
            None,
            vec![Patch::Replace { path: vec![], node }],
        );
        let snap = r.channel.snapshot("w1").unwrap().to_string();
        assert!(!snap.contains("hunter2"), "{snap}");
    }

    #[test]
    fn an_apps_own_page_cannot_open_drive_or_close_a_window() {
        let r = rig();
        // Admitted by the seat, as an attendant of the holding session is:
        // the refusal is about naming no device, not about the seat.
        r.gate.0.lock().unwrap().insert(ATTENDANT);
        r.up(A, "w1", json!({ "op": "open", "app": "tasks" }));
        r.channel
            .patched("tasks", "w1", 1, None, replace("private"));
        r.app_frames();
        r.sink.take();
        for body in [
            json!({ "op": "open", "app": "tasks" }),
            json!({ "op": "event", "event": { "path": ["t"], "kind": "activated" } }),
            json!({ "op": "close" }),
        ] {
            r.up(ATTENDANT, "w1", body);
            let sent = r.sink.take();
            assert_eq!(sent.len(), 1, "{sent:?}");
            assert_eq!(sent[0].2["error"]["code"], "not_a_device");
            assert!(!sent[0].2.to_string().contains("private"));
        }
        assert!(r.app_frames().is_empty());
        assert_eq!(r.channel.subscribers("w1"), 1);
        // And it is sent nothing the window draws afterwards.
        r.channel.patched("tasks", "w1", 2, None, props("later"));
        assert!(r.sink.take().iter().all(|(c, _, _)| *c == A));
    }

    /// A chain of `depth` stacks, keyed `k`.
    fn chain(depth: usize) -> Node {
        let mut node = Node::new("k", ui::Stack::default());
        for _ in 1..depth {
            node = Node::new("k", ui::Stack::default()).with([node]);
        }
        node
    }

    #[test]
    fn a_tree_deeper_than_the_limit_is_never_kept_and_never_asked_for_again() {
        let r = rig();
        r.up(A, "w1", json!({ "op": "open", "app": "tasks" }));
        r.app_frames();
        // Grown a few levels a frame: each frame on its own is shallow.
        let root = Node::new("root", ui::Stack::default()).with([chain(40)]);
        r.channel.patched(
            "tasks",
            "w1",
            1,
            None,
            vec![Patch::Replace {
                path: vec![],
                node: root,
            }],
        );
        r.sink.take();
        r.channel.patched(
            "tasks",
            "w1",
            2,
            None,
            vec![Patch::Insert {
                path: vec!["k".to_string(); 40],
                index: 0,
                node: chain(40),
            }],
        );
        // Refused before it was applied: nobody is sent it, the app is asked
        // for the whole tree, and the mirror holds nothing.
        assert!(r.sink.take().is_empty());
        assert!(matches!(&r.app_frames()[..], [(_, ToApp::Open { .. })]));
        assert!(r.channel.snapshot("w1").is_none());
        // The whole tree it answers with is as deep, and is refused for good:
        // the shell is told, and the app is not asked again.
        let deep = Node::new("root", ui::Stack::default()).with([chain(81)]);
        r.channel.patched(
            "tasks",
            "w1",
            3,
            None,
            vec![Patch::Replace {
                path: vec![],
                node: deep,
            }],
        );
        let sent = r.sink.take();
        assert_eq!(sent[0].2["error"]["code"], "ui_refused", "{sent:?}");
        assert_eq!(sent[1].2["status"], "lost");
        assert!(r.app_frames().is_empty());
        r.channel.patched("tasks", "w1", 4, None, props("x"));
        assert!(r.app_frames().is_empty(), "a refused window looped");
        // A shell's next open asks once more.
        r.up(A, "w1", json!({ "op": "open", "app": "tasks" }));
        assert!(matches!(&r.app_frames()[..], [(_, ToApp::Open { .. })]));
    }

    #[test]
    fn a_frame_too_costly_to_apply_is_answered_by_asking_for_the_whole_tree() {
        let r = rig();
        r.up(A, "w1", json!({ "op": "open", "app": "tasks" }));
        r.channel.patched("tasks", "w1", 1, None, replace("one"));
        r.app_frames();
        r.sink.take();
        // Five thousand inserts at the front: each shifts and checks every
        // sibling, about twelve million visits, over the budget.
        let flood: Vec<Patch> = (0..5_000)
            .map(|i| Patch::Insert {
                path: vec![],
                index: 0,
                node: Node::new(format!("n{i}"), ui::Text::default()),
            })
            .collect();
        r.channel.patched("tasks", "w1", 2, None, flood);
        assert!(r.sink.take().is_empty());
        assert!(matches!(&r.app_frames()[..], [(_, ToApp::Open { .. })]));
        assert!(r.channel.snapshot("w1").is_none());
    }

    #[test]
    fn a_patch_sent_while_a_window_is_reopened_never_arrives_before_its_tree() {
        use std::sync::mpsc;
        use std::time::Duration;

        /// Holds the delivery of the reopen's whole tree long enough for a
        /// patch on another thread to try to get ahead of it.
        struct Slow {
            sent: Mutex<Vec<u64>>,
            started: Mutex<Option<mpsc::Receiver<()>>>,
        }
        impl UiSink for Slow {
            fn deliver(&self, _: ConnectionId, _: &str, body: Value) -> Option<bool> {
                let restart = body["patches"][0]["op"] == "replace";
                if restart
                    && body["rev"] == 2
                    && let Some(rx) = self.started.lock().unwrap().take()
                {
                    let _ = rx.recv_timeout(Duration::from_secs(5));
                    std::thread::sleep(Duration::from_millis(100));
                }
                self.sent
                    .lock()
                    .unwrap()
                    .push(body["rev"].as_u64().unwrap_or(0));
                Some(true)
            }
        }
        let (tx, rx) = mpsc::channel();
        let sink = Arc::new(Slow {
            sent: Mutex::new(Vec::new()),
            started: Mutex::new(None),
        });
        let gate = Arc::new(Gate::default());
        gate.0.lock().unwrap().insert(A);
        let apps = Arc::new(Apps::default());
        let channel = UiChannel::new(Arc::clone(&sink) as _, gate as _);
        let weak: Weak<dyn UiApps> = Arc::downgrade(&(Arc::clone(&apps) as Arc<dyn UiApps>));
        channel.attach_apps(weak);
        channel.handle_upstream(A, "w1".into(), json!({ "op": "open", "app": "tasks" }));
        channel.patched("tasks", "w1", 1, None, replace("one"));
        channel.patched("tasks", "w1", 2, None, props("two"));
        sink.sent.lock().unwrap().clear();
        *sink.started.lock().unwrap() = Some(rx);
        // The shell reconnected and opens the window again (served rev 2
        // from the mirror) while the app emits rev 3.
        let other = Arc::clone(&channel);
        let patcher = std::thread::spawn(move || {
            tx.send(()).unwrap();
            other.patched("tasks", "w1", 3, None, props("three"));
        });
        channel.handle_upstream(A, "w1".into(), json!({ "op": "open", "app": "tasks" }));
        patcher.join().unwrap();
        assert_eq!(*sink.sent.lock().unwrap(), vec![2, 3]);
    }

    #[test]
    fn the_real_seat_gates_the_channel() {
        use crate::auth::SessionId;
        use crate::ws::{Hub, Presence};

        let clock: Arc<dyn ws::Clock> = Arc::new(ws::SystemClock);
        let hub = Hub::new(Arc::clone(&clock));
        let seat = crate::seat::Seat::new(hub, clock, Vec::new());
        let sink = Arc::new(Sink::default());
        let apps = Arc::new(Apps::default());
        let channel = UiChannel::new(Arc::clone(&sink) as _, Arc::clone(&seat) as _);
        let weak: Weak<dyn UiApps> = Arc::downgrade(&(Arc::clone(&apps) as Arc<dyn UiApps>));
        channel.attach_apps(weak);
        let mine = SessionId::generate().unwrap();
        let theirs = SessionId::generate().unwrap();
        // A joins first and holds the seat; B is another signed-in device;
        // C is an app's page in A's session, which the seat admits.
        seat.joined(A, &mine, Some("laptop"));
        seat.joined(B, &theirs, Some("phone"));
        seat.joined(ATTENDANT, &mine, None);
        let open = json!({ "op": "open", "app": "tasks" });
        channel.handle_upstream(B, "w1".into(), open.clone());
        assert_eq!(sink.take()[0].2["error"]["code"], crate::seat::NOT_HELD);
        channel.handle_upstream(ATTENDANT, "w1".into(), open.clone());
        assert_eq!(sink.take()[0].2["error"]["code"], "not_a_device");
        assert!(apps.sent.lock().unwrap().is_empty());
        channel.handle_upstream(A, "w1".into(), open);
        assert_eq!(apps.sent.lock().unwrap().len(), 1);
        channel.patched("tasks", "w1", 1, None, replace("one"));
        assert_eq!(sink.take().len(), 1);
        // B takes the seat: A is no longer sent the window.
        seat.take(B, Some("phone")).unwrap();
        channel.patched("tasks", "w1", 2, None, props("two"));
        assert!(sink.take().is_empty());
        assert_eq!(channel.subscribers("w1"), 0);
    }
}
