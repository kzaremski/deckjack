//! The UI protocol: an app describes its window as a tree, and the shell
//! draws it.
//!
//! The rule this module exists for is that **apps stop shipping
//! JavaScript**. An app is a Rust process that says what its window contains
//! — a heading, a text field, a list of rows — and the browser only ever runs
//! code Deckjack itself ships. The shell renders each widget with its own
//! components, styled only by design tokens, and every string in the tree is
//! drawn as text, never as markup.
//!
//! (The operator has not named this yet. "The UI protocol" is a working
//! name; everything here is spelled `ui` so a rename is one word.)
//!
//! # Shape
//!
//! - [`Node`]: a key, a [`Widget`] and children. Keys are unique among
//!   siblings and are what makes a patch *keyed*: a row is addressed by the
//!   task it shows, never by its position, so a refresh that reorders a list
//!   moves rows rather than rewriting every one.
//! - [`Patch`]: one change to a tree, addressed by a [`Path`] of keys from
//!   the root. [`diff`] turns two trees into patches and [`apply`] plays them
//!   back; `apply(diff(a, b), a) == b` is the property the tests hold it to.
//! - [`Event`]: what the shell tells the app. Only semantic events: a field
//!   *changed* (debounced, and always on blur or submit), a button was
//!   *activated*, a list selection was *selected*, a field was *submitted*,
//!   the window *closed*, or was *opened* again from the core's copy. Never
//!   a keystroke, a hover or a scroll position.
//!
//! # Local first
//!
//! Typing, the caret, selection, scrolling, hover, focus, pressed states and
//! a checkbox's tick never wait for the app: the shell owns them and tells
//! the app afterwards. That is why a text field's `value` is not echoed back
//! on every change (see [`TextField::revision`]) and why an [`Action`] can
//! read other fields' current text at the moment it fires ([`Action::fields`])
//! rather than asking the app what they say.
//!
//! # Actions are bus commands
//!
//! A button bound to an [`Action`] dispatches that command through the
//! ordinary command bus, as the app's window (`as_app`), so the app UI's
//! scope, the hard-deny list, the single seat and the destructive-command
//! confirmation all apply exactly as they do to any other caller. Nothing in
//! this module can reach further than the app's own window already could.

use std::collections::{BTreeMap, HashMap, HashSet};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// Keys from the root's children down to one node. Empty is the root.
pub type Path = Vec<String>;

// Every field at its default is left out of the JSON: a ten-thousand-row
// window's first frame is mostly defaults otherwise, and the shell reads an
// absent field as its default anyway.
fn is_default<T: Default + PartialEq>(value: &T) -> bool {
    *value == T::default()
}

fn is_true(value: &bool) -> bool {
    *value
}

// ---------------------------------------------------------------------------
// The tree
// ---------------------------------------------------------------------------

/// One node of a window's tree.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Node {
    /// Unique among this node's siblings, stable across renders for the
    /// same thing (a task's id, not its row number).
    pub key: String,
    pub widget: Widget,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub children: Vec<Node>,
}

impl Node {
    pub fn new(key: impl Into<String>, widget: impl Into<Widget>) -> Node {
        Node {
            key: key.into(),
            widget: widget.into(),
            children: Vec::new(),
        }
    }

    /// With these children, replacing any already set.
    pub fn with(mut self, children: impl IntoIterator<Item = Node>) -> Node {
        self.children = children.into_iter().collect();
        self
    }

    /// How many nodes this tree holds, itself included. Iterative, so a
    /// tree of any depth is counted without recursion.
    pub fn count(&self) -> usize {
        let mut n = 0;
        let mut stack = vec![self];
        while let Some(node) = stack.pop() {
            n += 1;
            stack.extend(node.children.iter());
        }
        n
    }

    /// Levels in this tree: 1 for a node with no children. Iterative.
    pub fn depth(&self) -> usize {
        let mut deepest = 0;
        let mut stack = vec![(self, 1usize)];
        while let Some((node, depth)) = stack.pop() {
            deepest = deepest.max(depth);
            stack.extend(node.children.iter().map(|c| (c, depth + 1)));
        }
        deepest
    }

    /// The node at `path`, if there is one.
    pub fn find(&self, path: &[String]) -> Option<&Node> {
        let mut node = self;
        for key in path {
            node = node.children.iter().find(|c| &c.key == key)?;
        }
        Some(node)
    }

    fn find_mut(&mut self, path: &[String]) -> Option<&mut Node> {
        let mut node = self;
        for key in path {
            node = node.children.iter_mut().find(|c| &c.key == key)?;
        }
        Some(node)
    }
}

/// What a node is. Tagged by `kind` on the wire.
///
/// Thirteen widgets, deliberately few: each is a component the shell owns and
/// styles with tokens, so each one added is shell code to keep. A new one
/// needs two apps that need it (docs/TIERS.md).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Widget {
    /// Children top to bottom.
    Stack(Stack),
    /// Children left to right.
    Row(Row),
    /// A region that scrolls its children when they do not fit.
    Scroll(Scroll),
    Text(Text),
    Heading(Heading),
    Button(Button),
    Checkbox(Checkbox),
    TextField(TextField),
    Select(Select),
    /// Rows, keyed, virtualized by the shell: only what is on screen is in
    /// the DOM, so ten thousand rows scroll like ten.
    List(List),
    /// What a list or a pane says when there is nothing in it.
    Empty(Empty),
    /// A date or date-time, formatted by the shell in the viewer's locale
    /// and time zone, which only the viewer's device knows.
    Date(Date),
    /// How far a long piece of work has got, as a bar. Added for Convert's
    /// job queue; Video's export list and Files' transfers are the next two
    /// users (docs/lanes/5c-convert.md, "Built").
    Progress(Progress),
}

/// Space between children, in the shell's spacing steps.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Gap {
    None,
    Small,
    #[default]
    Medium,
    Large,
}

/// A color role, never a color: the theme decides what each looks like.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Tone {
    #[default]
    Normal,
    Muted,
    Accent,
    Warning,
    Danger,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Stack {
    #[serde(skip_serializing_if = "is_default")]
    pub gap: Gap,
    /// Inset its content by the shell's standard padding.
    #[serde(skip_serializing_if = "is_default")]
    pub padded: bool,
    /// Take the remaining height in its parent.
    #[serde(skip_serializing_if = "is_default")]
    pub grow: bool,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Row {
    #[serde(skip_serializing_if = "is_default")]
    pub gap: Gap,
    #[serde(skip_serializing_if = "is_default")]
    pub padded: bool,
    /// A hairline under the row, as a toolbar or a list row has.
    #[serde(skip_serializing_if = "is_default")]
    pub divider: bool,
    /// Lay the row out as a toolbar: raised background, divider below.
    #[serde(skip_serializing_if = "is_default")]
    pub bar: bool,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Scroll {}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Text {
    #[serde(skip_serializing_if = "is_default")]
    pub text: String,
    #[serde(skip_serializing_if = "is_default")]
    pub tone: Tone,
    /// Struck through, as a finished task is.
    #[serde(skip_serializing_if = "is_default")]
    pub strike: bool,
    /// The small size, for metadata beside a primary line.
    #[serde(skip_serializing_if = "is_default")]
    pub small: bool,
    /// Take the remaining width in a row.
    #[serde(skip_serializing_if = "is_default")]
    pub grow: bool,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Heading {
    #[serde(skip_serializing_if = "is_default")]
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Button {
    #[serde(skip_serializing_if = "is_default")]
    pub label: String,
    /// What it does. `None` sends the app an [`EventKind::Activated`].
    #[serde(skip_serializing_if = "is_default")]
    pub action: Option<Action>,
    #[serde(skip_serializing_if = "is_true")]
    pub enabled: bool,
    #[serde(skip_serializing_if = "is_default")]
    pub tone: Tone,
    /// A borderless button, for a row's secondary action.
    #[serde(skip_serializing_if = "is_default")]
    pub quiet: bool,
    /// The accessible name and tooltip, for a button whose label is a glyph.
    #[serde(skip_serializing_if = "is_default")]
    pub title: Option<String>,
}

impl Default for Button {
    fn default() -> Button {
        Button {
            label: String::new(),
            action: None,
            enabled: true,
            tone: Tone::Normal,
            quiet: false,
            title: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Checkbox {
    #[serde(skip_serializing_if = "is_default")]
    pub label: String,
    /// Shown visually as well as read by assistive technology. False keeps
    /// the label for screen readers only, as a list row's tick has.
    #[serde(skip_serializing_if = "is_true")]
    pub show_label: bool,
    /// The name an [`Action::fields`] entry reads this box by.
    #[serde(skip_serializing_if = "is_default")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "is_default")]
    pub checked: bool,
    /// What a tick does. `None` sends the app [`EventKind::Changed`] with
    /// the new value; with one, `action.value` names the parameter that
    /// receives it.
    #[serde(skip_serializing_if = "is_default")]
    pub action: Option<Action>,
    #[serde(skip_serializing_if = "is_true")]
    pub enabled: bool,
}

impl Default for Checkbox {
    fn default() -> Checkbox {
        Checkbox {
            label: String::new(),
            show_label: true,
            name: None,
            checked: false,
            action: None,
            enabled: true,
        }
    }
}

/// What a text field accepts. The shell picks the native control for it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Input {
    #[default]
    Text,
    Search,
    /// `YYYY-MM-DD`.
    Date,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct TextField {
    /// Unique in the window: what [`Action::fields`] and events name it by.
    #[serde(skip_serializing_if = "is_default")]
    pub name: String,
    /// The text the field shows when it first appears, and again whenever
    /// [`TextField::revision`] changes. Between those the text is the
    /// operator's: a patch that changes only `value` is not applied, so an
    /// app that echoes what it was told can never overwrite keystrokes typed
    /// since.
    #[serde(skip_serializing_if = "is_default")]
    pub value: String,
    /// Bump it to make the field show `value` again, in every window showing
    /// it: a form the app resets on its own initiative. To clear a field
    /// after the window that submitted it succeeded, use [`Action::clear`],
    /// which leaves other windows' drafts alone.
    #[serde(skip_serializing_if = "is_default")]
    pub revision: u64,
    #[serde(skip_serializing_if = "is_default")]
    pub placeholder: String,
    /// The accessible name. Required in spirit: a field nobody can name is
    /// a field a screen reader calls "edit text".
    #[serde(skip_serializing_if = "is_default")]
    pub label: String,
    #[serde(skip_serializing_if = "is_default")]
    pub input: Input,
    /// Masked as typed, and left out of `ui.snapshot`.
    #[serde(skip_serializing_if = "is_default")]
    pub secret: bool,
    /// What Enter does. `None` sends the app [`EventKind::Submitted`].
    #[serde(skip_serializing_if = "is_default")]
    pub submit: Option<Action>,
    #[serde(skip_serializing_if = "is_default")]
    pub grow: bool,
}

/// One option of a [`Select`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Choice {
    /// What an action receives when this is chosen. Any JSON value, so a
    /// priority can be the number a command takes rather than a string.
    pub value: Value,
    pub label: String,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Select {
    #[serde(skip_serializing_if = "is_default")]
    pub name: String,
    #[serde(skip_serializing_if = "is_default")]
    pub label: String,
    #[serde(skip_serializing_if = "is_default")]
    pub options: Vec<Choice>,
    /// Shown when it first appears and whenever `revision` changes, as for
    /// [`TextField`].
    #[serde(skip_serializing_if = "is_default")]
    pub value: Value,
    #[serde(skip_serializing_if = "is_default")]
    pub revision: u64,
}

/// How a [`List`] selects, per `web/public/kit/selection.js`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Selection {
    #[default]
    None,
    Single,
    Multi,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct List {
    /// The accessible name.
    #[serde(skip_serializing_if = "is_default")]
    pub label: String,
    #[serde(skip_serializing_if = "is_default")]
    pub selection: Selection,
    /// Keys of the selected rows, when the app wants to set them. The shell
    /// keeps its own selection between changes to this.
    #[serde(skip_serializing_if = "is_default")]
    pub selected: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Empty {
    #[serde(skip_serializing_if = "is_default")]
    pub title: String,
    #[serde(skip_serializing_if = "is_default")]
    pub detail: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Date {
    /// `YYYY-MM-DD`, or `YYYY-MM-DDTHH:MM`, in the viewer's local time.
    #[serde(skip_serializing_if = "is_default")]
    pub value: String,
    /// Shown in the danger tone once it is past (a date-only value at the
    /// end of its day), by the viewer's clock.
    #[serde(skip_serializing_if = "is_default")]
    pub flag_past: bool,
}

/// A bar from empty to full. Only the bar: the words beside it ("12 min
/// left") are a [`Text`], so they read the same to a screen reader and in
/// `ui.snapshot` as anywhere else.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Progress {
    /// From 0.0 to 1.0, clamped by the shell. `None` while nobody knows how
    /// far along it is, drawn as a bar that moves without a position.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value: Option<f64>,
    /// The accessible name: what is progressing ("Encoding trip.mov").
    #[serde(skip_serializing_if = "is_default")]
    pub label: String,
}

macro_rules! widget_from {
    ($($t:ident),*) => {
        $(impl From<$t> for Widget {
            fn from(w: $t) -> Widget {
                Widget::$t(w)
            }
        })*
    };
}
widget_from!(
    Stack, Row, Scroll, Text, Heading, Button, Checkbox, TextField, Select, List, Empty, Date,
    Progress
);

impl Widget {
    /// The wire name of this widget's kind.
    pub fn kind(&self) -> &'static str {
        match self {
            Widget::Stack(_) => "stack",
            Widget::Row(_) => "row",
            Widget::Scroll(_) => "scroll",
            Widget::Text(_) => "text",
            Widget::Heading(_) => "heading",
            Widget::Button(_) => "button",
            Widget::Checkbox(_) => "checkbox",
            Widget::TextField(_) => "text_field",
            Widget::Select(_) => "select",
            Widget::List(_) => "list",
            Widget::Empty(_) => "empty",
            Widget::Date(_) => "date",
            Widget::Progress(_) => "progress",
        }
    }
}

// ---------------------------------------------------------------------------
// Actions and events
// ---------------------------------------------------------------------------

/// A bus command a widget dispatches, as the app's window.
///
/// The shell builds the parameters at the moment the action fires, from
/// three sources in order: `params` as written, then each [`Action::fields`]
/// entry read from that field's *current* local value, then the widget's own
/// value under [`Action::value`]. A blank text field is sent as `null`, so an
/// optional parameter is absent-like rather than an empty string.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Action {
    pub command: String,
    #[serde(skip_serializing_if = "Map::is_empty")]
    pub params: Map<String, Value>,
    /// Parameter name to field name.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub fields: BTreeMap<String, String>,
    /// The parameter that receives this widget's own value (a checkbox's
    /// new state).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
    /// Fields that must not be blank. The shell disables the widget, and
    /// the palette entry, while any is, with no round trip.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub required: Vec<String>,
    /// Fields the shell clears after the command succeeds, back to the
    /// value the tree gives them. Only in the window that fired it, and only
    /// a field that still says what was sent, so text typed while the
    /// command ran is kept. (A field revision would clear every window
    /// showing the field, whoever ran the command.)
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub clear: Vec<String>,
}

impl Action {
    pub fn command(command: impl Into<String>) -> Action {
        Action {
            command: command.into(),
            ..Action::default()
        }
    }

    pub fn param(mut self, name: impl Into<String>, value: impl Into<Value>) -> Action {
        self.params.insert(name.into(), value.into());
        self
    }

    pub fn field(mut self, param: impl Into<String>, field: impl Into<String>) -> Action {
        self.fields.insert(param.into(), field.into());
        self
    }

    pub fn value_as(mut self, param: impl Into<String>) -> Action {
        self.value = Some(param.into());
        self
    }

    pub fn require(mut self, field: impl Into<String>) -> Action {
        self.required.push(field.into());
        self
    }

    pub fn clear(mut self, field: impl Into<String>) -> Action {
        self.clear.push(field.into());
        self
    }
}

/// What happened, semantically.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventKind {
    /// A field's value: debounced while typing, and always sent on blur and
    /// before a submit, so the app never acts on stale text.
    Changed,
    /// A button with no action was pressed, or a list row was opened.
    Activated,
    /// A list's selection, as the keys of the selected rows.
    Selected,
    /// Enter in a field with no `submit` action.
    Submitted,
    /// The window closed. The last event an instance receives.
    Closed,
    /// A browser showed the window again and was served the core's copy of
    /// the tree, without the app being asked for it. An app whose data can
    /// change behind its back (files edited elsewhere) reads it again here.
    /// Sent by the core, not the shell.
    Opened,
}

/// One event from the shell to the app's `update`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Event {
    /// The widget, by path from the root.
    pub path: Path,
    pub kind: EventKind,
    #[serde(default)]
    pub value: Value,
    /// The shell's number for this event, echoed in the next patch frame's
    /// `ack` so the shell knows when the app has caught up.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seq: Option<u64>,
}

impl Event {
    /// The widget's own key: the last element of its path.
    pub fn key(&self) -> &str {
        self.path.last().map(String::as_str).unwrap_or("")
    }
}

// ---------------------------------------------------------------------------
// Patches
// ---------------------------------------------------------------------------

/// One keyed change to a tree.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Patch {
    /// Put `node` at `path`. At the root this is a whole new tree, which is
    /// also how a window's first frame and every resync are sent.
    Replace { path: Path, node: Node },
    /// The node at `path` keeps its kind and children; its widget becomes
    /// this.
    Props { path: Path, widget: Widget },
    /// Insert `node` as the `index`th child of the node at `path`.
    Insert {
        path: Path,
        index: usize,
        node: Node,
    },
    /// Remove the child keyed `key` from the node at `path`.
    Remove { path: Path, key: String },
    /// Take the child keyed `key` out of the node at `path` and put it back
    /// as the `index`th child.
    Move {
        path: Path,
        key: String,
        index: usize,
    },
}

/// Why a patch could not be applied. Always a bug on one side or a frame
/// lost on the way; the remedy either way is a fresh root `Replace`.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ApplyError {
    #[error("no node at {0:?}")]
    NoNode(Path),
    #[error("no child {key:?} under {path:?}")]
    NoChild { path: Path, key: String },
    #[error("a child {key:?} already exists under {path:?}")]
    Duplicate { path: Path, key: String },
    #[error("index {index} is past the end of {path:?}")]
    Index { path: Path, index: usize },
    #[error("a replace at {path:?} carries a node keyed {key:?}")]
    KeyChanged { path: Path, key: String },
}

// ---------------------------------------------------------------------------
// Limits
// ---------------------------------------------------------------------------

/// The deepest a window's tree may be, root included. A real window is a
/// handful of levels (Tasks is five); the cap exists because the core keeps
/// every window's tree and walks it recursively (clone, drop, serialize),
/// and a tree an app grew a few levels per frame could otherwise overflow
/// the core's stack and take every shell and app down with it.
pub const MAX_DEPTH: usize = 64;

/// The most nodes a window's tree may hold. Ten thousand Tasks rows are
/// about 45,000.
pub const MAX_NODES: usize = 250_000;

/// How much work one frame's patches may cost to apply, in sibling visits:
/// every child scanned to find a key on a path, plus the children shifted or
/// checked by an insert, remove or move. About 30 ms of a core's time. A
/// frame over it is answered by asking the app for the whole tree, which
/// costs nothing to apply.
pub const APPLY_BUDGET: usize = 8_000_000;

/// The most work [`diff`] spends placing one node's children (placements
/// times the length of the list) before it sends that node whole instead.
const DIFF_BUDGET: usize = 1_000_000;

/// Why [`apply_bounded`] refused a frame.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Refused {
    /// A patch does not fit the tree: a lost frame or a bug.
    #[error(transparent)]
    Apply(#[from] ApplyError),
    /// The frame would cost more than [`APPLY_BUDGET`] to apply.
    #[error("the patches would cost more than {APPLY_BUDGET} sibling visits to apply")]
    Budget,
    /// The tree would be deeper than [`MAX_DEPTH`].
    #[error("the tree would be {0} levels deep; the limit is {MAX_DEPTH}")]
    Depth(usize),
    /// The tree would hold more than [`MAX_NODES`] nodes.
    #[error("the tree would hold {0} nodes; the limit is {MAX_NODES}")]
    Nodes(usize),
}

/// [`apply`] for a tree that came from somebody else: refused, before any
/// work it cannot afford, when a patch would make the tree deeper than
/// [`MAX_DEPTH`], cost more than [`APPLY_BUDGET`] or leave more than
/// [`MAX_NODES`] nodes. On an error the tree is left part-applied and must be
/// thrown away; the core does, and asks for the whole tree again.
pub fn apply_bounded(tree: &mut Node, patches: &[Patch]) -> Result<(), Refused> {
    let mut spent = 0usize;
    let mut grew = false;
    for patch in patches {
        // How deep the tree becomes where this patch lands: a replaced node
        // sits at its path, an inserted one a level below its parent's.
        let depth = match patch {
            Patch::Replace { path, node } => {
                grew = true;
                path.len() + node.depth()
            }
            Patch::Insert { path, node, .. } => {
                grew = true;
                path.len() + 1 + node.depth()
            }
            Patch::Props { path, .. } | Patch::Remove { path, .. } | Patch::Move { path, .. } => {
                path.len() + 1
            }
        };
        if depth > MAX_DEPTH {
            return Err(Refused::Depth(depth));
        }
        spent = spent.saturating_add(apply_cost(tree, patch));
        if spent > APPLY_BUDGET {
            return Err(Refused::Budget);
        }
        apply_one(tree, patch)?;
    }
    if grew {
        let nodes = tree.count();
        if nodes > MAX_NODES {
            return Err(Refused::Nodes(nodes));
        }
    }
    Ok(())
}

/// What one patch costs to apply to `tree`, in sibling visits: the children
/// scanned at each level of its path, and the target's children for a patch
/// that searches or shifts them. A path that leaves the tree costs what was
/// scanned before it did.
pub fn apply_cost(tree: &Node, patch: &Patch) -> usize {
    let path = match patch {
        Patch::Replace { path, .. }
        | Patch::Props { path, .. }
        | Patch::Insert { path, .. }
        | Patch::Remove { path, .. }
        | Patch::Move { path, .. } => path,
    };
    let mut cost = 0usize;
    let mut node = tree;
    for key in path {
        cost = cost.saturating_add(node.children.len());
        match node.children.iter().find(|c| &c.key == key) {
            Some(child) => node = child,
            None => return cost,
        }
    }
    match patch {
        Patch::Insert { .. } | Patch::Remove { .. } | Patch::Move { .. } => {
            cost.saturating_add(node.children.len())
        }
        Patch::Replace { .. } | Patch::Props { .. } => cost.max(1),
    }
}

/// The patches that turn `old` into `new`.
///
/// Keyed: children are matched by key, never by position. Unchanged
/// subtrees produce nothing; a node whose kind changed is replaced whole;
/// otherwise a changed widget is one [`Patch::Props`] and children are
/// removed, inserted and moved by key. Siblings must have unique keys
/// ([`normalize_keys`] makes them so).
pub fn diff(old: &Node, new: &Node) -> Vec<Patch> {
    let mut out = Vec::new();
    if old.key != new.key {
        out.push(Patch::Replace {
            path: Vec::new(),
            node: new.clone(),
        });
    } else {
        diff_node(&mut Vec::new(), old, new, &mut out);
    }
    out
}

fn diff_node(path: &mut Path, old: &Node, new: &Node, out: &mut Vec<Patch>) {
    if std::mem::discriminant(&old.widget) != std::mem::discriminant(&new.widget) {
        out.push(Patch::Replace {
            path: path.clone(),
            node: new.clone(),
        });
        return;
    }
    let mut children = Vec::new();
    if !diff_children(path, &old.children, &new.children, &mut children) {
        out.push(Patch::Replace {
            path: path.clone(),
            node: new.clone(),
        });
        return;
    }
    if old.widget != new.widget {
        out.push(Patch::Props {
            path: path.clone(),
            widget: new.widget.clone(),
        });
    }
    out.extend(children);
}

fn diff_children(path: &mut Path, old: &[Node], new: &[Node], out: &mut Vec<Patch>) -> bool {
    if old.is_empty() && new.is_empty() {
        return true;
    }
    // Maps, built once: every lookup below is by key, so the diff is linear
    // in the number of children apart from the moves it emits.
    let old_by_key: HashMap<&str, &Node> = old.iter().map(|n| (n.key.as_str(), n)).collect();
    let new_index: HashMap<&str, usize> = new
        .iter()
        .enumerate()
        .map(|(i, n)| (n.key.as_str(), i))
        .collect();
    // The children as `apply` will see them, kept in step with every patch
    // emitted so each index is right at the moment it is applied.
    let mut working: Vec<&str> = Vec::with_capacity(old.len().max(new.len()));
    let mut removes = Vec::new();
    for child in old {
        if new_index.contains_key(child.key.as_str()) {
            working.push(&child.key);
        } else {
            removes.push(Patch::Remove {
                path: path.clone(),
                key: child.key.clone(),
            });
        }
    }
    // The kept children that are already in the right order relative to
    // each other — the longest run of them whose new positions increase —
    // stay where they are. Every other child is moved (or inserted) once,
    // so moving one row to the end is one `Move`, not one per row it passed.
    let positions: Vec<usize> = working.iter().map(|k| new_index[k]).collect();
    let stay: HashSet<&str> = longest_increasing(&positions)
        .into_iter()
        .map(|i| working[i])
        .collect();
    let placed = new.len() - stay.len();
    // Each placement looks its anchor up in the working list. When that
    // would be quadratic in a long list, sending the node whole is cheaper
    // for every side, the app's diff, the core's mirror and the shell.
    if placed.saturating_mul(working.len().max(new.len())) > DIFF_BUDGET {
        return false;
    }
    out.extend(removes);
    // Right to left, each child that does not stay goes immediately before
    // the one that follows it in the new order (or at the end), which is
    // already where it belongs.
    for i in (0..new.len()).rev() {
        let key = new[i].key.as_str();
        if stay.contains(key) {
            continue;
        }
        let existed = old_by_key.contains_key(key);
        if existed {
            let at = working
                .iter()
                .position(|k| *k == key)
                .expect("a kept key is in the working list");
            working.remove(at);
        }
        let index = match new.get(i + 1) {
            Some(next) => working
                .iter()
                .position(|k| *k == next.key)
                .expect("the following child was placed already"),
            None => working.len(),
        };
        working.insert(index, key);
        out.push(if existed {
            Patch::Move {
                path: path.clone(),
                key: key.to_string(),
                index,
            }
        } else {
            Patch::Insert {
                path: path.clone(),
                index,
                node: new[i].clone(),
            }
        });
    }
    for child in new {
        if let Some(previous) = old_by_key.get(child.key.as_str()) {
            path.push(child.key.clone());
            diff_node(path, previous, child, out);
            path.pop();
        }
    }
    true
}

/// Indices into `seq` of one longest strictly increasing subsequence.
fn longest_increasing(seq: &[usize]) -> Vec<usize> {
    // Patience sorting: `tails[l]` is the index of the smallest tail of an
    // increasing run of length l + 1, `prev` links each element to the one
    // before it in its run.
    let mut tails: Vec<usize> = Vec::new();
    let mut prev: Vec<Option<usize>> = vec![None; seq.len()];
    for (i, &v) in seq.iter().enumerate() {
        let l = tails.partition_point(|&t| seq[t] < v);
        if l > 0 {
            prev[i] = Some(tails[l - 1]);
        }
        if l == tails.len() {
            tails.push(i);
        } else {
            tails[l] = i;
        }
    }
    let mut out = Vec::with_capacity(tails.len());
    let mut at = tails.last().copied();
    while let Some(i) = at {
        out.push(i);
        at = prev[i];
    }
    out.reverse();
    out
}

/// Play `patches` onto `tree`, in order. Stops at the first that does not
/// fit, leaving the tree with the patches before it applied.
pub fn apply(tree: &mut Node, patches: &[Patch]) -> Result<(), ApplyError> {
    for patch in patches {
        apply_one(tree, patch)?;
    }
    Ok(())
}

fn apply_one(tree: &mut Node, patch: &Patch) -> Result<(), ApplyError> {
    match patch {
        Patch::Replace { path, node } => {
            if path.is_empty() {
                *tree = node.clone();
                return Ok(());
            }
            if path.last() != Some(&node.key) {
                return Err(ApplyError::KeyChanged {
                    path: path.clone(),
                    key: node.key.clone(),
                });
            }
            let target = tree
                .find_mut(path)
                .ok_or_else(|| ApplyError::NoNode(path.clone()))?;
            *target = node.clone();
        }
        Patch::Props { path, widget } => {
            let target = tree
                .find_mut(path)
                .ok_or_else(|| ApplyError::NoNode(path.clone()))?;
            target.widget = widget.clone();
        }
        Patch::Insert { path, index, node } => {
            let parent = tree
                .find_mut(path)
                .ok_or_else(|| ApplyError::NoNode(path.clone()))?;
            if parent.children.iter().any(|c| c.key == node.key) {
                return Err(ApplyError::Duplicate {
                    path: path.clone(),
                    key: node.key.clone(),
                });
            }
            if *index > parent.children.len() {
                return Err(ApplyError::Index {
                    path: path.clone(),
                    index: *index,
                });
            }
            parent.children.insert(*index, node.clone());
        }
        Patch::Remove { path, key } => {
            let parent = tree
                .find_mut(path)
                .ok_or_else(|| ApplyError::NoNode(path.clone()))?;
            let at = parent
                .children
                .iter()
                .position(|c| &c.key == key)
                .ok_or_else(|| ApplyError::NoChild {
                    path: path.clone(),
                    key: key.clone(),
                })?;
            parent.children.remove(at);
        }
        Patch::Move { path, key, index } => {
            let parent = tree
                .find_mut(path)
                .ok_or_else(|| ApplyError::NoNode(path.clone()))?;
            let at = parent
                .children
                .iter()
                .position(|c| &c.key == key)
                .ok_or_else(|| ApplyError::NoChild {
                    path: path.clone(),
                    key: key.clone(),
                })?;
            let node = parent.children.remove(at);
            if *index > parent.children.len() {
                parent.children.insert(at, node);
                return Err(ApplyError::Index {
                    path: path.clone(),
                    index: *index,
                });
            }
            parent.children.insert(*index, node);
        }
    }
    Ok(())
}

/// Make sibling keys unique, in place, by suffixing repeats (`a`, `a~2`,
/// `a~3`). Returns how many keys it changed.
///
/// A view that gives two rows the same key is an app bug, but the price of
/// it should be one confused row and not a diff that cannot be applied. The
/// SDK runs this on every render and logs a nonzero count.
pub fn normalize_keys(node: &mut Node) -> usize {
    let mut changed = 0;
    let mut seen: HashSet<String> = HashSet::with_capacity(node.children.len());
    for child in &mut node.children {
        if !seen.insert(child.key.clone()) {
            let mut n = 2;
            let key = loop {
                let candidate = format!("{}~{n}", child.key);
                if !seen.contains(&candidate) {
                    break candidate;
                }
                n += 1;
            };
            seen.insert(key.clone());
            child.key = key;
            changed += 1;
        }
        changed += normalize_keys(child);
    }
    changed
}

/// The tree with every secret field's value removed, for `ui.snapshot`.
///
/// A redacted field keeps its place and its name, so a reader can see that
/// there is a password field and that it was withheld, but not what it says.
pub fn redacted(node: &Node) -> Node {
    let mut out = node.clone();
    redact_in_place(&mut out);
    out
}

fn redact_in_place(node: &mut Node) {
    if let Widget::TextField(field) = &mut node.widget
        && field.secret
    {
        field.value = String::new();
    }
    for child in &mut node.children {
        redact_in_place(child);
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn text(key: &str, s: &str) -> Node {
        Node::new(
            key,
            Text {
                text: s.to_string(),
                ..Text::default()
            },
        )
    }

    fn stack(key: &str, children: Vec<Node>) -> Node {
        Node::new(key, Stack::default()).with(children)
    }

    fn roundtrip(a: &Node, b: &Node) -> Vec<Patch> {
        let patches = diff(a, b);
        let mut applied = a.clone();
        apply(&mut applied, &patches)
            .unwrap_or_else(|e| panic!("{e}\n a = {a:#?}\n b = {b:#?}\n patches = {patches:#?}"));
        assert_eq!(&applied, b, "patches = {patches:#?}");
        // And through the wire: the shell and the core read the JSON, not
        // the Rust values.
        let wire: Vec<Patch> =
            serde_json::from_str(&serde_json::to_string(&patches).unwrap()).unwrap();
        let mut applied = a.clone();
        apply(&mut applied, &wire).unwrap();
        assert_eq!(&applied, b);
        patches
    }

    #[test]
    fn identical_trees_produce_no_patches() {
        let a = stack("root", vec![text("a", "one"), text("b", "two")]);
        assert!(diff(&a, &a.clone()).is_empty());
    }

    #[test]
    fn one_changed_text_is_one_props_patch_at_its_key() {
        let a = stack("root", vec![text("a", "one"), text("b", "two")]);
        let b = stack("root", vec![text("a", "one"), text("b", "three")]);
        let patches = roundtrip(&a, &b);
        assert_eq!(patches.len(), 1, "{patches:#?}");
        assert!(
            matches!(&patches[0], Patch::Props { path, .. } if path == &vec!["b".to_string()]),
            "{patches:#?}"
        );
    }

    #[test]
    fn a_row_inserted_at_the_top_is_one_insert_and_no_moves() {
        // The whole reason for keys: the rows below shift without a patch.
        let rows: Vec<Node> = (0..50).map(|i| text(&format!("t{i}"), "x")).collect();
        let a = stack("root", rows.clone());
        let mut more = vec![text("new", "y")];
        more.extend(rows);
        let b = stack("root", more);
        let patches = roundtrip(&a, &b);
        assert_eq!(patches.len(), 1, "{patches:#?}");
        assert!(matches!(&patches[0], Patch::Insert { index: 0, .. }));
    }

    #[test]
    fn a_removed_row_is_one_remove() {
        let rows: Vec<Node> = (0..50).map(|i| text(&format!("t{i}"), "x")).collect();
        let a = stack("root", rows.clone());
        let mut fewer = rows;
        fewer.remove(17);
        let patches = roundtrip(&a, &stack("root", fewer));
        assert_eq!(
            patches,
            vec![Patch::Remove {
                path: vec![],
                key: "t17".into()
            }]
        );
    }

    #[test]
    fn a_changed_kind_replaces_the_node_and_keeps_its_place() {
        let a = stack("root", vec![text("a", "one"), text("b", "two")]);
        let b = stack(
            "root",
            vec![
                text("a", "one"),
                Node::new("b", Heading { text: "two".into() }),
            ],
        );
        let patches = roundtrip(&a, &b);
        assert!(
            matches!(&patches[..], [Patch::Replace { path, .. }] if path == &vec!["b".to_string()])
        );
    }

    #[test]
    fn a_new_root_key_replaces_the_whole_tree() {
        let a = stack("one", vec![text("a", "x")]);
        let b = stack("two", vec![text("a", "x")]);
        let patches = roundtrip(&a, &b);
        assert!(matches!(&patches[..], [Patch::Replace { path, .. }] if path.is_empty()));
    }

    /// Every ordered subset of four keys, against every other: 65 lists, so
    /// 4,225 pairs, which covers every insert, remove and move a list of up
    /// to four can need, in every combination.
    #[test]
    fn every_pair_of_small_keyed_lists_roundtrips() {
        fn lists(keys: &[&str]) -> Vec<Vec<String>> {
            let mut out = vec![vec![]];
            for k in 1..=keys.len() {
                permutations_of_size(keys, k, &mut Vec::new(), &mut out);
            }
            out
        }
        fn permutations_of_size(
            keys: &[&str],
            k: usize,
            prefix: &mut Vec<String>,
            out: &mut Vec<Vec<String>>,
        ) {
            if prefix.len() == k {
                out.push(prefix.clone());
                return;
            }
            for key in keys {
                if !prefix.iter().any(|p| p == key) {
                    prefix.push(key.to_string());
                    permutations_of_size(keys, k, prefix, out);
                    prefix.pop();
                }
            }
        }
        let all = lists(&["a", "b", "c", "d"]);
        assert_eq!(all.len(), 65);
        for left in &all {
            for right in &all {
                // The text depends on which side, so a matched row also
                // needs a props patch: moves and props together.
                let a = stack(
                    "root",
                    left.iter().map(|k| text(k, &format!("L{k}"))).collect(),
                );
                let b = stack(
                    "root",
                    right
                        .iter()
                        .map(|k| text(k, if k == "b" { "Lb" } else { k }))
                        .collect(),
                );
                roundtrip(&a, &b);
            }
        }
    }

    /// A small deterministic generator, so a failure prints a seed that
    /// reproduces it rather than a case nobody can get back.
    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            // xorshift64*
            self.0 ^= self.0 >> 12;
            self.0 ^= self.0 << 25;
            self.0 ^= self.0 >> 27;
            self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
        }
        fn below(&mut self, n: u64) -> u64 {
            self.next() % n
        }
    }

    fn random_widget(rng: &mut Rng) -> Widget {
        match rng.below(6) {
            0 => Stack::default().into(),
            1 => Row {
                divider: rng.below(2) == 0,
                ..Row::default()
            }
            .into(),
            2 => Text {
                text: format!("t{}", rng.below(4)),
                strike: rng.below(2) == 0,
                ..Text::default()
            }
            .into(),
            3 => Checkbox {
                checked: rng.below(2) == 0,
                ..Checkbox::default()
            }
            .into(),
            4 => Button {
                label: format!("b{}", rng.below(3)),
                action: (rng.below(2) == 0).then(|| Action::command("demo.go").param("id", 1)),
                ..Button::default()
            }
            .into(),
            _ => TextField {
                name: "f".into(),
                value: format!("v{}", rng.below(3)),
                revision: rng.below(2),
                ..TextField::default()
            }
            .into(),
        }
    }

    fn random_tree(rng: &mut Rng, key: String, depth: u32) -> Node {
        let mut node = Node::new(key, random_widget(rng));
        if depth > 0 {
            // Keys from a small pool, so two random trees share many of them
            // and the diff has matches, moves and kind changes to find.
            let mut pool: Vec<String> = (0..8).map(|i| format!("k{i}")).collect();
            let n = rng.below(6) as usize;
            for _ in 0..n {
                let at = rng.below(pool.len() as u64) as usize;
                let key = pool.swap_remove(at);
                node.children.push(random_tree(rng, key, depth - 1));
            }
        }
        node
    }

    /// Edit a tree the way a view changes between renders: a few rows in,
    /// a few out, a few moved, a few props changed.
    fn mutate(rng: &mut Rng, node: &mut Node, depth: u32) {
        if rng.below(4) == 0 {
            node.widget = random_widget(rng);
        }
        let n = node.children.len();
        if n > 1 && rng.below(3) == 0 {
            let from = rng.below(n as u64) as usize;
            let to = rng.below(n as u64) as usize;
            let c = node.children.remove(from);
            node.children.insert(to, c);
        }
        if n > 0 && rng.below(4) == 0 {
            let at = rng.below(n as u64) as usize;
            node.children.remove(at);
        }
        if depth > 0 && rng.below(3) == 0 {
            let key = format!("n{}", rng.next() % 1000);
            if !node.children.iter().any(|c| c.key == key) {
                let at = rng.below(node.children.len() as u64 + 1) as usize;
                let child = random_tree(rng, key, depth - 1);
                node.children.insert(at, child);
            }
        }
        for child in &mut node.children {
            mutate(rng, child, depth.saturating_sub(1));
        }
    }

    #[test]
    fn random_trees_roundtrip() {
        for seed in 1..=3_000u64 {
            let mut rng = Rng(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1);
            let a = random_tree(&mut rng, "root".into(), 3);
            // Half the time an unrelated tree, half an edit of this one.
            let b = if seed % 2 == 0 {
                random_tree(&mut rng, "root".into(), 3)
            } else {
                let mut b = a.clone();
                mutate(&mut rng, &mut b, 3);
                b
            };
            let patches = diff(&a, &b);
            let mut applied = a.clone();
            apply(&mut applied, &patches)
                .unwrap_or_else(|e| panic!("seed {seed}: {e}\npatches = {patches:#?}"));
            assert_eq!(applied, b, "seed {seed}");
        }
    }

    #[test]
    fn a_patch_that_does_not_fit_is_refused_with_where() {
        let mut tree = stack("root", vec![text("a", "x")]);
        let e = apply(
            &mut tree,
            &[Patch::Remove {
                path: vec![],
                key: "zz".into(),
            }],
        )
        .unwrap_err();
        assert_eq!(
            e,
            ApplyError::NoChild {
                path: vec![],
                key: "zz".into()
            }
        );
        let e = apply(
            &mut tree,
            &[Patch::Insert {
                path: vec![],
                index: 0,
                node: text("a", "again"),
            }],
        )
        .unwrap_err();
        assert!(matches!(e, ApplyError::Duplicate { .. }));
        let e = apply(
            &mut tree,
            &[Patch::Props {
                path: vec!["nope".into()],
                widget: Heading::default().into(),
            }],
        )
        .unwrap_err();
        assert!(matches!(e, ApplyError::NoNode(_)));
        let e = apply(
            &mut tree,
            &[Patch::Replace {
                path: vec!["a".into()],
                node: text("b", "renamed"),
            }],
        )
        .unwrap_err();
        assert!(matches!(e, ApplyError::KeyChanged { .. }));
    }

    #[test]
    fn duplicate_sibling_keys_are_made_unique_and_then_diff_cleanly() {
        let mut a = stack(
            "root",
            vec![
                text("a", "1"),
                text("a", "2"),
                text("a~2", "3"),
                text("a", "4"),
            ],
        );
        assert_eq!(normalize_keys(&mut a), 3);
        let keys: Vec<&str> = a.children.iter().map(|c| c.key.as_str()).collect();
        assert_eq!(keys, ["a", "a~2", "a~2~2", "a~3"]);
        let mut b = a.clone();
        b.children.reverse();
        roundtrip(&a, &b);
    }

    #[test]
    fn a_snapshot_withholds_secret_values_and_nothing_else() {
        let tree = stack(
            "root",
            vec![
                Node::new(
                    "pw",
                    TextField {
                        name: "pw".into(),
                        value: "hunter2".into(),
                        secret: true,
                        ..TextField::default()
                    },
                ),
                Node::new(
                    "user",
                    TextField {
                        name: "user".into(),
                        value: "kz".into(),
                        ..TextField::default()
                    },
                ),
            ],
        );
        let shown = serde_json::to_string(&redacted(&tree)).unwrap();
        assert!(!shown.contains("hunter2"), "{shown}");
        assert!(shown.contains("\"kz\""), "{shown}");
        assert!(shown.contains("\"pw\""), "{shown}");
    }

    #[test]
    fn the_wire_shape_is_tagged_by_kind_and_op() {
        // The shell's TypeScript reads exactly this; a serde attribute
        // changed by accident would break every window silently.
        let node = Node::new(
            "go",
            Button {
                label: "Go".into(),
                action: Some(Action::command("demo.go").field("name", "who")),
                ..Button::default()
            },
        );
        let v = serde_json::to_value(&node).unwrap();
        assert_eq!(v["key"], "go");
        assert_eq!(v["widget"]["kind"], "button");
        assert_eq!(v["widget"]["action"]["command"], "demo.go");
        assert_eq!(v["widget"]["action"]["fields"]["name"], "who");
        assert!(v.get("children").is_none());
        let p = serde_json::to_value(Patch::Remove {
            path: vec!["a".into()],
            key: "b".into(),
        })
        .unwrap();
        assert_eq!(
            p,
            serde_json::json!({"op": "remove", "path": ["a"], "key": "b"})
        );
        let field = serde_json::to_value(Widget::from(TextField::default())).unwrap();
        assert_eq!(field["kind"], "text_field");
        assert_eq!(Widget::from(TextField::default()).kind(), "text_field");
    }

    #[test]
    fn a_progress_bar_says_how_far_or_that_nobody_knows() {
        // What `UiNode.vue` reads: `value` absent is the indeterminate bar,
        // which is not the same as 0 ("not started").
        let known = serde_json::to_value(Widget::from(Progress {
            value: Some(0.25),
            label: "Encoding a.mov".into(),
        }))
        .unwrap();
        assert_eq!(
            known,
            serde_json::json!({"kind": "progress", "value": 0.25, "label": "Encoding a.mov"})
        );
        let started = serde_json::to_value(Widget::from(Progress {
            value: Some(0.0),
            label: String::new(),
        }))
        .unwrap();
        assert_eq!(
            started,
            serde_json::json!({"kind": "progress", "value": 0.0})
        );
        let unknown = serde_json::to_value(Widget::from(Progress::default())).unwrap();
        assert_eq!(unknown, serde_json::json!({"kind": "progress"}));
        let back: Widget = serde_json::from_value(unknown).unwrap();
        assert_eq!(back, Widget::Progress(Progress::default()));
    }

    #[test]
    fn a_row_moved_from_the_top_to_the_bottom_is_one_move() {
        // A finished task sorting to the end of the list: one row moved,
        // not one move for every row it passed.
        let rows: Vec<Node> = (0..50).map(|i| text(&format!("t{i}"), "x")).collect();
        let a = stack("root", rows.clone());
        let mut moved = rows;
        let first = moved.remove(0);
        moved.push(first);
        let patches = roundtrip(&a, &stack("root", moved));
        assert_eq!(
            patches,
            vec![Patch::Move {
                path: vec![],
                key: "t0".into(),
                index: 49
            }]
        );
    }

    #[test]
    fn rows_reordered_in_two_places_are_two_moves() {
        let keys = |ks: &[&str]| stack("root", ks.iter().map(|k| text(k, "x")).collect());
        let a = keys(&["a", "b", "c", "d", "e", "f"]);
        let b = keys(&["b", "c", "a", "d", "f", "e"]);
        let patches = roundtrip(&a, &b);
        assert_eq!(patches.len(), 2, "{patches:#?}");
        assert!(patches.iter().all(|p| matches!(p, Patch::Move { .. })));
    }

    #[test]
    fn the_longest_increasing_run_is_found() {
        assert_eq!(longest_increasing(&[]), Vec::<usize>::new());
        assert_eq!(longest_increasing(&[3, 1, 2]), vec![1, 2]);
        let seq = [0, 8, 4, 12, 2, 10, 6, 14, 1, 9, 5, 13, 3, 11, 7, 15];
        let run = longest_increasing(&seq);
        assert_eq!(run.len(), 6);
        assert!(run.windows(2).all(|w| w[0] < w[1] && seq[w[0]] < seq[w[1]]));
    }

    #[test]
    fn a_reorder_too_costly_to_place_row_by_row_sends_the_list_whole() {
        // Reversing two thousand rows would be 1,999 moves, each found by a
        // scan of the list: the list is sent as one node instead.
        let rows: Vec<Node> = (0..2_000).map(|i| text(&format!("t{i}"), "x")).collect();
        let list = |rows: Vec<Node>| Node::new("list", List::default()).with(rows);
        let a = stack("root", vec![list(rows.clone())]);
        let mut reversed = rows;
        reversed.reverse();
        let b = stack("root", vec![list(reversed)]);
        let patches = roundtrip(&a, &b);
        assert!(
            matches!(&patches[..], [Patch::Replace { path, .. }] if path == &vec!["list".to_string()]),
            "{} patches",
            patches.len()
        );
        // A few rows moved in the same list is still a few moves.
        let mut nudged: Vec<Node> = (0..2_000).map(|i| text(&format!("t{i}"), "x")).collect();
        nudged.swap(10, 1_500);
        let patches = roundtrip(&a, &stack("root", vec![list(nudged)]));
        assert_eq!(patches.len(), 2, "{patches:#?}");
    }

    fn chain(depth: usize) -> Node {
        let mut node = Node::new("k", Stack::default());
        for _ in 1..depth {
            node = Node::new("k", Stack::default()).with([node]);
        }
        node
    }

    #[test]
    fn a_bounded_apply_refuses_a_tree_past_the_depth_limit_before_applying() {
        let mut tree = stack("root", vec![chain(MAX_DEPTH - 1)]);
        assert_eq!(tree.depth(), MAX_DEPTH);
        // One more level under the deepest node is refused, and the tree is
        // not touched.
        let path = vec!["k".to_string(); MAX_DEPTH - 1];
        let before = tree.clone();
        let e = apply_bounded(
            &mut tree,
            &[Patch::Insert {
                path: path.clone(),
                index: 0,
                node: text("x", "deep"),
            }],
        )
        .unwrap_err();
        assert_eq!(e, Refused::Depth(MAX_DEPTH + 1));
        assert_eq!(tree, before);
        // A whole tree past it, likewise; one at it is accepted.
        let e = apply_bounded(
            &mut tree,
            &[Patch::Replace {
                path: vec![],
                node: chain(MAX_DEPTH + 1),
            }],
        )
        .unwrap_err();
        assert_eq!(e, Refused::Depth(MAX_DEPTH + 1));
        apply_bounded(
            &mut tree,
            &[Patch::Replace {
                path: vec![],
                node: chain(MAX_DEPTH),
            }],
        )
        .unwrap();
    }

    #[test]
    fn a_bounded_apply_refuses_too_many_nodes_and_too_much_work() {
        let mut tree = stack("root", vec![]);
        let many = stack(
            "root",
            (0..MAX_NODES).map(|i| text(&format!("t{i}"), "")).collect(),
        );
        let e = apply_bounded(
            &mut tree,
            &[Patch::Replace {
                path: vec![],
                node: many,
            }],
        )
        .unwrap_err();
        assert_eq!(e, Refused::Nodes(MAX_NODES + 1));
        // Five thousand inserts at the front cost about twelve million
        // sibling visits; the same rows as one replace cost nothing.
        let mut tree = stack("root", vec![]);
        let flood: Vec<Patch> = (0..5_000)
            .map(|i| Patch::Insert {
                path: vec![],
                index: 0,
                node: text(&format!("n{i}"), ""),
            })
            .collect();
        assert_eq!(apply_bounded(&mut tree, &flood), Err(Refused::Budget));
        let mut tree = stack("root", vec![]);
        let rows = stack(
            "root",
            (0..5_000).map(|i| text(&format!("n{i}"), "")).collect(),
        );
        apply_bounded(
            &mut tree,
            &[Patch::Replace {
                path: vec![],
                node: rows,
            }],
        )
        .unwrap();
        // And an ordinary edit is well inside the budget.
        let patches = diff(&tree, &{
            let mut t = tree.clone();
            t.children.remove(2_500);
            t
        });
        apply_bounded(&mut tree, &patches).unwrap();
        assert_eq!(tree.children.len(), 4_999);
    }

    #[test]
    fn counting_and_measuring_a_tree_do_not_recurse() {
        // Far deeper than any stack would allow a recursive walk.
        let mut node = Node::new("k", Stack::default());
        for _ in 1..200_000 {
            node = Node::new("k", Stack::default()).with([node]);
        }
        assert_eq!(node.depth(), 200_000);
        assert_eq!(node.count(), 200_000);
        // Dropping it would recurse, so take it apart by hand.
        let mut next = Some(node);
        while let Some(mut n) = next {
            next = n.children.pop();
        }
    }
}
