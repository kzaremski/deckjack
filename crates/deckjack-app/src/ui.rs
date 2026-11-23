//! The UI protocol, from inside an app: describe a window, and the shell
//! draws it.
//!
//! An app on the UI protocol ships no JavaScript and no `ui/` folder. It
//! keeps an Elm-shaped loop instead: a model, a [`Program::view`] that turns
//! the model into a [`Node`] tree, and a [`Program::update`] that changes the
//! model when a message arrives. This module runs the loop: it renders after
//! every update, diffs the new tree against the last one and sends only the
//! keyed patches, one window at a time.
//!
//! ```ignore
//! use deckjack_app::ui::{self, Event, Node, Program};
//!
//! struct Counter { n: u32 }
//! enum Msg { Bump }
//!
//! impl Program for Counter {
//!     type Msg = Msg;
//!     fn view(&self) -> Node {
//!         Node::new("root", ui::Stack::default()).with([
//!             Node::new("n", ui::Text { text: self.n.to_string(), ..Default::default() }),
//!             Node::new("bump", ui::Button { label: "Bump".into(), ..Default::default() }),
//!         ])
//!     }
//!     fn update(&mut self, Msg::Bump: Msg) { self.n += 1 }
//!     fn event(&self, e: &Event) -> Option<Msg> {
//!         (e.key() == "bump").then_some(Msg::Bump)
//!     }
//! }
//!
//! App::new(include_str!("../app.toml"))?.view(|| Counter { n: 0 }).run()
//! ```
//!
//! # Where messages come from
//!
//! Two places, and neither is a keystroke:
//!
//! - [`Program::event`]: a semantic event from one window (a field changed,
//!   a button with no action was pressed, a selection changed). The shell
//!   owns typing, focus and scrolling and sends these afterwards.
//! - [`Program::command_ran`]: one of this app's own commands finished,
//!   whoever called it — a button bound to it, the palette, an agent. That is
//!   how a window stays true after a button dispatched `tasks.add` through
//!   the bus: the app hears that its handler ran and re-reads what changed.
//!
//! # Each window is its own model
//!
//! The factory passed to [`App::view`](crate::App::view) is called once per
//! window, when the shell first shows it, and the instance lives until the
//! window closes. Two windows of one app are two models.
//!
//! # Threading
//!
//! Every window's loop runs on one dedicated thread, in arrival order, so an
//! `update` may block (read a file) without stalling the frame loop, and two
//! events for one window are never handled out of order.

use std::collections::HashMap;
use std::sync::Arc;

use deckjack_protocol::AppFrame;
use serde_json::Value;

pub use deckjack_protocol::ui::*;

/// One window's model, as the UI loop drives it.
pub trait Program: Send + 'static {
    type Msg: Send + 'static;

    /// The whole window, from the model. Called after every update that
    /// produced a message; cheap trees are the point, so build it fresh.
    fn view(&self) -> Node;

    /// Change the model.
    fn update(&mut self, msg: Self::Msg);

    /// A widget event, as a message, or `None` to ignore it.
    fn event(&self, event: &Event) -> Option<Self::Msg>;

    /// One of this app's commands ran successfully, with these parameters.
    fn command_ran(&self, _command: &str, _params: &Value) -> Option<Self::Msg> {
        None
    }
}

/// A [`Program`] with its message type erased, so windows of any program
/// can share one table.
pub(crate) trait Instance: Send {
    fn render(&self) -> Node;
    /// True when the event became a message, so the view may have changed.
    fn event(&mut self, event: &Event) -> bool;
    fn command_ran(&mut self, command: &str, params: &Value) -> bool;
}

struct Held<P: Program>(P);

impl<P: Program> Instance for Held<P> {
    fn render(&self) -> Node {
        self.0.view()
    }

    fn event(&mut self, event: &Event) -> bool {
        match self.0.event(event) {
            Some(msg) => {
                self.0.update(msg);
                true
            }
            None => false,
        }
    }

    fn command_ran(&mut self, command: &str, params: &Value) -> bool {
        match self.0.command_ran(command, params) {
            Some(msg) => {
                self.0.update(msg);
                true
            }
            None => false,
        }
    }
}

/// Makes a fresh model for a new window.
pub(crate) type Factory = Arc<dyn Fn() -> Box<dyn Instance> + Send + Sync>;

/// Wrap a program's constructor as a [`Factory`].
pub(crate) fn factory<P, F>(init: F) -> Factory
where
    P: Program,
    F: Fn() -> P + Send + Sync + 'static,
{
    Arc::new(move || Box::new(Held(init())) as Box<dyn Instance>)
}

/// What the frame loop hands the UI loop.
#[derive(Debug)]
pub(crate) enum Job {
    Open(String),
    Event(String, Event),
    Close(String),
    CommandRan(String, Value),
}

/// One open window: its model, the tree the shell has, and its revision.
struct Open {
    instance: Box<dyn Instance>,
    tree: Node,
    rev: u64,
}

/// Every open window of this app, and the logic of the loop, with no thread
/// and no socket in it so it can be tested as a function from jobs to
/// frames.
pub(crate) struct Windows {
    factory: Factory,
    open: HashMap<String, Open>,
}

impl Windows {
    pub(crate) fn new(factory: Factory) -> Windows {
        Windows {
            factory,
            open: HashMap::new(),
        }
    }

    /// Handle one job and return the frames it produced, in order.
    pub(crate) fn handle(&mut self, job: Job) -> Vec<AppFrame> {
        let mut out = Vec::new();
        match job {
            Job::Open(window) => {
                // A window already open is asked for a fresh copy (the core
                // lost track, or a second device took the seat): same model,
                // whole tree, next revision.
                let entry = self.open.entry(window.clone()).or_insert_with(|| Open {
                    instance: (self.factory)(),
                    tree: Node::new("", Stack::default()),
                    rev: 0,
                });
                let tree = render(entry.instance.as_ref(), &window, &mut out);
                entry.rev += 1;
                entry.tree = tree.clone();
                out.push(AppFrame::UiPatch {
                    window,
                    rev: entry.rev,
                    ack: None,
                    patches: vec![Patch::Replace {
                        path: Vec::new(),
                        node: tree,
                    }],
                });
            }
            Job::Event(window, event) => {
                let Some(entry) = self.open.get_mut(&window) else {
                    out.push(log(
                        "warn",
                        format!("an event for window {window}, which this app never opened"),
                    ));
                    return out;
                };
                if event.kind == EventKind::Closed {
                    // Closing goes through `Job::Close`; a `closed` event
                    // from the shell is the same request.
                    return self.handle(Job::Close(window));
                }
                let seq = event.seq;
                let changed = entry.instance.event(&event);
                let patches = if changed {
                    step(entry, &window, &mut out)
                } else {
                    Vec::new()
                };
                // Answered even when nothing changed, when the shell asked
                // for an answer: the ack is how it knows the app caught up.
                if !patches.is_empty() || seq.is_some() {
                    entry.rev += 1;
                    out.push(AppFrame::UiPatch {
                        window,
                        rev: entry.rev,
                        ack: seq,
                        patches,
                    });
                }
            }
            Job::Close(window) => {
                if let Some(mut entry) = self.open.remove(&window) {
                    // The last thing the model hears, so it can let go of
                    // anything it holds for the window. Its view is not
                    // rendered: there is nothing left to draw it in.
                    entry.instance.event(&Event {
                        path: Vec::new(),
                        kind: EventKind::Closed,
                        value: Value::Null,
                        seq: None,
                    });
                }
            }
            Job::CommandRan(command, params) => {
                let mut windows: Vec<&String> = self.open.keys().collect();
                windows.sort();
                let windows: Vec<String> = windows.into_iter().cloned().collect();
                for window in windows {
                    let Some(entry) = self.open.get_mut(&window) else {
                        continue;
                    };
                    if !entry.instance.command_ran(&command, &params) {
                        continue;
                    }
                    let patches = step(entry, &window, &mut out);
                    if !patches.is_empty() {
                        entry.rev += 1;
                        out.push(AppFrame::UiPatch {
                            window,
                            rev: entry.rev,
                            ack: None,
                            patches,
                        });
                    }
                }
            }
        }
        out
    }

    #[cfg(test)]
    fn is_open(&self, window: &str) -> bool {
        self.open.contains_key(window)
    }
}

/// Render, with sibling keys made unique and a log line if they were not.
fn render(instance: &dyn Instance, window: &str, out: &mut Vec<AppFrame>) -> Node {
    let mut tree = instance.render();
    let fixed = normalize_keys(&mut tree);
    if fixed > 0 {
        out.push(log(
            "warn",
            format!(
                "window {window}: {fixed} sibling key(s) repeated in the view; renamed so the \
                 tree still diffs. Give each row a key of its own (an id, not a position)"
            ),
        ));
    }
    tree
}

/// Re-render one window and return the patches from what the shell has.
///
/// Patches that would cost the core more than half its budget to apply
/// (`APPLY_BUDGET`: a long list with many rows changed, each found by a scan)
/// are sent as the whole tree instead, which costs nothing to apply. The
/// core would refuse them and ask for the whole tree anyway; this saves the
/// round trip.
fn step(entry: &mut Open, window: &str, out: &mut Vec<AppFrame>) -> Vec<Patch> {
    let tree = render(entry.instance.as_ref(), window, out);
    let mut patches = diff(&entry.tree, &tree);
    let cost = patches.iter().fold(0usize, |sum, p| {
        sum.saturating_add(apply_cost(&entry.tree, p))
    });
    if cost > APPLY_BUDGET / 2 {
        patches = vec![Patch::Replace {
            path: Vec::new(),
            node: tree.clone(),
        }];
    }
    entry.tree = tree;
    patches
}

fn log(level: &str, message: String) -> AppFrame {
    AppFrame::Log {
        level: level.to_string(),
        message,
    }
}

/// The UI loop: a thread that owns every window and turns jobs into frames.
pub(crate) fn spawn(
    factory: Factory,
    out: tokio::sync::mpsc::UnboundedSender<String>,
) -> std::sync::mpsc::Sender<Job> {
    let (tx, rx) = std::sync::mpsc::channel::<Job>();
    std::thread::Builder::new()
        .name("deckjack-ui".to_string())
        .spawn(move || {
            let mut windows = Windows::new(factory);
            // Ends when the frame loop drops the sender, which it does when
            // the connection to the core ends.
            while let Ok(job) = rx.recv() {
                for frame in windows.handle(job) {
                    if let Ok(line) = deckjack_protocol::encode_frame(&frame)
                        && out.send(line).is_err()
                    {
                        return;
                    }
                }
            }
        })
        .expect("the UI thread starts");
    tx
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A counter with a field, a button the app handles and a command hook.
    struct Counter {
        n: u32,
        name: String,
        closed: Arc<std::sync::atomic::AtomicBool>,
    }

    enum Msg {
        Bump,
        Name(String),
        Closed,
        Reload,
    }

    impl Program for Counter {
        type Msg = Msg;
        fn view(&self) -> Node {
            Node::new("root", Stack::default()).with([
                Node::new(
                    "n",
                    Text {
                        text: format!("{} {}", self.name, self.n),
                        ..Text::default()
                    },
                ),
                Node::new(
                    "bump",
                    Button {
                        label: "Bump".into(),
                        ..Button::default()
                    },
                ),
            ])
        }
        fn update(&mut self, msg: Msg) {
            match msg {
                Msg::Bump => self.n += 1,
                Msg::Name(s) => self.name = s,
                Msg::Closed => self.closed.store(true, std::sync::atomic::Ordering::SeqCst),
                Msg::Reload => self.n += 100,
            }
        }
        fn event(&self, e: &Event) -> Option<Msg> {
            match (e.kind, e.key()) {
                (EventKind::Activated, "bump") => Some(Msg::Bump),
                (EventKind::Changed, "name") => Some(Msg::Name(e.value.as_str()?.to_string())),
                (EventKind::Closed, _) => Some(Msg::Closed),
                _ => None,
            }
        }
        fn command_ran(&self, command: &str, _params: &Value) -> Option<Msg> {
            (command == "demo.touch").then_some(Msg::Reload)
        }
    }

    fn windows() -> (Windows, Arc<std::sync::atomic::AtomicBool>) {
        let closed = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = Arc::clone(&closed);
        let w = Windows::new(factory(move || Counter {
            n: 0,
            name: "n".into(),
            closed: Arc::clone(&flag),
        }));
        (w, closed)
    }

    fn ev(key: &str, kind: EventKind, value: Value, seq: Option<u64>) -> Event {
        Event {
            path: vec![key.to_string()],
            kind,
            value,
            seq,
        }
    }

    fn patch(frame: &AppFrame) -> (&str, u64, Option<u64>, &[Patch]) {
        match frame {
            AppFrame::UiPatch {
                window,
                rev,
                ack,
                patches,
            } => (window, *rev, *ack, patches),
            other => panic!("expected a patch frame, got {other:?}"),
        }
    }

    #[test]
    fn opening_a_window_sends_its_whole_tree_as_revision_one() {
        let (mut w, _) = windows();
        let frames = w.handle(Job::Open("w1".into()));
        let (window, rev, ack, patches) = patch(&frames[0]);
        assert_eq!((window, rev, ack), ("w1", 1, None));
        let [Patch::Replace { path, node }] = patches else {
            panic!("{patches:?}");
        };
        assert!(path.is_empty());
        assert_eq!(node.children.len(), 2);
    }

    #[test]
    fn an_event_that_changes_the_model_sends_only_what_changed_with_its_ack() {
        let (mut w, _) = windows();
        w.handle(Job::Open("w1".into()));
        let frames = w.handle(Job::Event(
            "w1".into(),
            ev("bump", EventKind::Activated, Value::Null, Some(7)),
        ));
        let (_, rev, ack, patches) = patch(&frames[0]);
        assert_eq!((rev, ack), (2, Some(7)));
        assert!(
            matches!(patches, [Patch::Props { path, .. }] if path == &vec!["n".to_string()]),
            "{patches:?}"
        );
    }

    #[test]
    fn an_ignored_event_is_still_acknowledged_and_an_unasked_one_is_silent() {
        let (mut w, _) = windows();
        w.handle(Job::Open("w1".into()));
        let frames = w.handle(Job::Event(
            "w1".into(),
            ev("nothing", EventKind::Activated, Value::Null, Some(3)),
        ));
        let (_, rev, ack, patches) = patch(&frames[0]);
        assert_eq!((rev, ack), (2, Some(3)));
        assert!(patches.is_empty());
        let frames = w.handle(Job::Event(
            "w1".into(),
            ev("nothing", EventKind::Activated, Value::Null, None),
        ));
        assert!(frames.is_empty(), "{frames:?}");
    }

    #[test]
    fn a_command_of_the_app_re_renders_every_window_that_cares() {
        let (mut w, _) = windows();
        w.handle(Job::Open("a".into()));
        w.handle(Job::Open("b".into()));
        let frames = w.handle(Job::CommandRan("demo.touch".into(), Value::Null));
        assert_eq!(frames.len(), 2, "{frames:?}");
        assert_eq!(patch(&frames[0]).0, "a");
        assert_eq!(patch(&frames[1]).0, "b");
        // A command the program does not care about changes nothing.
        assert!(
            w.handle(Job::CommandRan("demo.other".into(), Value::Null))
                .is_empty()
        );
    }

    #[test]
    fn windows_are_separate_models() {
        let (mut w, _) = windows();
        w.handle(Job::Open("a".into()));
        w.handle(Job::Open("b".into()));
        w.handle(Job::Event(
            "a".into(),
            ev("name", EventKind::Changed, Value::from("alpha"), None),
        ));
        // Re-opening b sends b's tree, which never heard about a's name.
        let frames = w.handle(Job::Open("b".into()));
        let (_, rev, _, patches) = patch(&frames[0]);
        assert_eq!(rev, 2, "a resync continues the window's revisions");
        let [Patch::Replace { node, .. }] = patches else {
            panic!()
        };
        let Widget::Text(t) = &node.children[0].widget else {
            panic!()
        };
        assert_eq!(t.text, "n 0");
    }

    #[test]
    fn closing_a_window_tells_the_model_and_forgets_it() {
        let (mut w, closed) = windows();
        w.handle(Job::Open("a".into()));
        assert!(w.handle(Job::Close("a".into())).is_empty());
        assert!(closed.load(std::sync::atomic::Ordering::SeqCst));
        assert!(!w.is_open("a"));
        // An event for a closed window is logged, not acted on.
        let frames = w.handle(Job::Event(
            "a".into(),
            ev("bump", EventKind::Activated, Value::Null, Some(1)),
        ));
        assert!(matches!(&frames[..], [AppFrame::Log { .. }]), "{frames:?}");
    }

    #[test]
    fn repeated_keys_in_a_view_are_fixed_and_reported() {
        struct Dup;
        impl Program for Dup {
            type Msg = ();
            fn view(&self) -> Node {
                Node::new("root", Stack::default()).with([
                    Node::new("x", Text::default()),
                    Node::new("x", Text::default()),
                ])
            }
            fn update(&mut self, _: ()) {}
            fn event(&self, _: &Event) -> Option<()> {
                None
            }
        }
        let mut w = Windows::new(factory(|| Dup));
        let frames = w.handle(Job::Open("a".into()));
        assert!(matches!(&frames[0], AppFrame::Log { level, .. } if level == "warn"));
        let (_, _, _, patches) = patch(&frames[1]);
        let [Patch::Replace { node, .. }] = patches else {
            panic!()
        };
        assert_eq!(node.children[1].key, "x~2");
    }

    #[test]
    fn patches_too_costly_for_the_core_are_sent_as_the_whole_tree() {
        /// A list whose every row's text changes on a bump.
        struct Rows(usize, u32);
        impl Program for Rows {
            type Msg = ();
            fn view(&self) -> Node {
                Node::new("root", List::default()).with((0..self.0).map(|i| {
                    Node::new(
                        format!("r{i}"),
                        Text {
                            text: format!("{i} {}", self.1),
                            ..Text::default()
                        },
                    )
                }))
            }
            fn update(&mut self, _: ()) {
                self.1 += 1;
            }
            fn event(&self, _: &Event) -> Option<()> {
                Some(())
            }
        }
        let bump = || ev("x", EventKind::Activated, Value::Null, None);
        // Three thousand rows changed, each found by a scan of the list: nine
        // million visits, over half the core's budget.
        let mut w = Windows::new(factory(|| Rows(3_000, 0)));
        w.handle(Job::Open("a".into()));
        let frames = w.handle(Job::Event("a".into(), bump()));
        let (_, rev, _, patches) = patch(&frames[0]);
        assert_eq!(rev, 2);
        assert!(
            matches!(patches, [Patch::Replace { path, .. }] if path.is_empty()),
            "{} patches",
            patches.len()
        );
        // A short list sends its patches.
        let mut w = Windows::new(factory(|| Rows(50, 0)));
        w.handle(Job::Open("a".into()));
        let frames = w.handle(Job::Event("a".into(), bump()));
        assert_eq!(patch(&frames[0]).3.len(), 50);
    }
}
