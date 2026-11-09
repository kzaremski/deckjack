//! Window manager server state.
//!
//! Windows live here, on the server, not in a browser tab. The set of open
//! windows is one thing: sign in from the phone and the same work is open
//! that the desktop left open, because a window *is* an app instance the
//! server holds, and the browser only draws it.
//!
//! The layouts are deliberately **not** one thing, and that asymmetry is the
//! reason this module exists. A [`DesktopLayout`] (frames, z-order, snapping,
//! tiling) and a [`CompactLayout`] (focus order, an optional split pair)
//! describe the same [`WindowId`]s. Rearranging windows on a phone writes
//! only the compact layout, so the desktop arrangement is still there when
//! you sit back down. Focus is the one thing that leaks, in one direction
//! only: see [`WindowSet::focus`].
//!
//! [`Mode`] follows the viewport and its input rather than the user agent,
//! because the same device is both: a Fold is compact folded and desktop
//! unfolded, and a phone in DeX drives a monitor with a mouse. A manual
//! override exists for the cases the rule gets wrong (a large tablet with a
//! keyboard case) and is stored with the window state, not sniffed.
//!
//! Nothing here reads the clock, the screen or the filesystem. The one time
//! value a window needs is passed in through [`Clock`], and the work area is
//! an argument to every operation that needs one, because the server does not
//! own the viewport — the client reports it with the command. That keeps the
//! whole module decidable from its inputs.
//!
//! ```ignore
//! let mut set = WindowSet::new();
//! let work = Rect { x: 0, y: 32, width: 2560, height: 1408 };
//! let id = set.open(
//!     OpenSpec {
//!         app: AppInstance::new("files", "01J9"),
//!         title: "Files — /srv/media".into(),
//!         node_id: "r620".into(),
//!         min_width: 420,
//!         size: Size { width: 1100, height: 760 },
//!     },
//!     work,
//!     &SystemClock,
//! );
//! set.snap(id, Snap::Left, work)?;          // desktop layout only
//! set.focus(id, Mode::Compact)?;            // compact layout only
//! let rows = set.snapshot();                // explicit rows for the store
//! ```

use std::collections::BTreeMap;
use std::fmt;
use std::str::FromStr;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Narrower than this is compact mode whatever is plugged in. Chosen because
/// two side-by-side panels at the design system's compact density stop being
/// usable below it, not because any particular device is this wide.
pub const DESKTOP_MIN_WIDTH: u32 = 900;

/// Fallback minimum width for an app that declares none. Narrow enough for a
/// phone's logical width, so a sloppy app contract never makes a window
/// unplaceable.
pub const DEFAULT_MIN_WIDTH: u32 = 320;

/// Shortest a window may be made. A title bar plus one row of content.
pub const MIN_HEIGHT: u32 = 120;

/// How much of a window must stay inside the work area. Enough to grab the
/// title bar with a mouse or a thumb; without it a window can be dragged
/// somewhere no pointer can reach it again, which on a server-held layout
/// would follow you to every device.
pub const KEEP_VISIBLE: u32 = 80;

/// Diagonal offset between cascaded new windows.
pub const CASCADE_STEP: i32 = 28;

/// New windows cascade through this many steps before starting over, so the
/// twentieth window is not off the edge.
const CASCADE_WRAP: u32 = 8;

/// Caps every width and height. No display is anywhere near this, and the cap
/// is what lets the rest of the module do its geometry in `i32` without
/// worrying about a hostile or corrupt number overflowing it.
pub const MAX_EXTENT: u32 = 32_768;

/// Where a split divider sits when nothing better is known.
pub const DEFAULT_DIVIDER: f32 = 0.5;

/// A divider is clamped into this range rather than refused: it is dragged,
/// and a drag that overshoots should stop at the end, not fail. Outside it one
/// pane is too narrow to be worth rendering.
pub const DIVIDER_RANGE: (f32, f32) = (0.15, 0.85);

/// Used to place a window that comes back from the database without a
/// placement row. Restore has no viewport to ask — the browser has not
/// connected yet — so the window gets a plausible frame that the first real
/// move corrects, rather than being dropped.
const FALLBACK_WORK_AREA: Rect = Rect {
    x: 0,
    y: 0,
    width: 1280,
    height: 800,
};

/// The clock, injected. Only [`WindowSet::open`] needs it, and it needs it
/// for one field, but reading `SystemTime::now()` inside would make every
/// test of window ordering depend on the wall clock.
pub trait Clock {
    /// Unix seconds now.
    fn now(&self) -> u64;
}

/// The real one.
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0)
    }
}

// ---- identity -----------------------------------------------------------

/// Handle for an open window.
///
/// Counted, not random. A window id is not a capability — the session cookie
/// is the gate, and every window belongs to the one operator — so there is
/// nothing to guess, and a small number is far easier to read in a log line,
/// a pop-out URL or a keyboard shortcut.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct WindowId(u64);

impl WindowId {
    pub fn from_u64(n: u64) -> WindowId {
        WindowId(n)
    }

    pub fn as_u64(self) -> u64 {
        self.0
    }
}

impl fmt::Display for WindowId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl FromStr for WindowId {
    type Err = std::num::ParseIntError;

    fn from_str(s: &str) -> Result<WindowId, Self::Err> {
        s.parse().map(WindowId)
    }
}

/// Which app, and which of that app's instances, a window shows.
///
/// TODO(apps): the app registry does not exist yet, so these are strings. The
/// window manager only ever stores and compares them, so swapping in the
/// registry's own types later touches this struct and nothing below it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppInstance {
    /// Installed app name, as the dock and the command bus know it.
    pub app: String,
    /// Opaque handle the app's backend assigned to this instance.
    pub instance: String,
}

impl AppInstance {
    pub fn new(app: impl Into<String>, instance: impl Into<String>) -> AppInstance {
        AppInstance {
            app: app.into(),
            instance: instance.into(),
        }
    }
}

/// What is open, independent of how any device is showing it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Window {
    pub app: AppInstance,
    /// What the dock, the switcher and the title bar show. The app owns it.
    pub title: String,
    /// The node running this instance's backend, as [`crate::node::id`]
    /// reports it. A panel never cares which node it is; this is here so the
    /// dock can show it and so a node going away can close its windows
    /// instead of leaving ones that answer nothing.
    pub node_id: String,
    /// From the app contract. A resize clamps to it rather than refusing,
    /// because the operator dragging an edge should always get *something*.
    pub min_width: u32,
    pub opened_at: u64,
}

// ---- geometry -----------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Size {
    pub width: u32,
    pub height: u32,
}

/// CSS pixels in the shell's coordinate space, origin at the top left of the
/// viewport. The server never converts to device pixels; the browser does.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

impl Rect {
    pub fn new(x: i32, y: i32, width: u32, height: u32) -> Rect {
        Rect {
            x,
            y,
            width: width.min(MAX_EXTENT),
            height: height.min(MAX_EXTENT),
        }
    }

    pub fn right(&self) -> i32 {
        self.x.saturating_add(self.width.min(MAX_EXTENT) as i32)
    }

    pub fn bottom(&self) -> i32 {
        self.y.saturating_add(self.height.min(MAX_EXTENT) as i32)
    }

    pub fn size(&self) -> Size {
        Size {
            width: self.width,
            height: self.height,
        }
    }
}

/// Clamp a frame into a work area, honoring an app's minimum width.
///
/// A work area narrower than the app's minimum wins: the window gets the
/// whole width and the panel deals with it, because a window placed too
/// narrow is a panel that scrolls, while a window refused is work the
/// operator cannot reach at all.
fn fit(frame: Rect, work: Rect, min_width: u32) -> Rect {
    let max_width = work.width.clamp(1, MAX_EXTENT);
    let max_height = work.height.clamp(1, MAX_EXTENT);

    let width = frame
        .width
        .clamp(1, max_width)
        .max(min_width.min(max_width));
    let height = frame
        .height
        .clamp(1, max_height)
        .max(MIN_HEIGHT.min(max_height));

    // A window may hang off the right or bottom edge, but never so far that
    // the grab area goes with it, and never above the work area's top, where
    // the title bar would be under the shell's own bar.
    let keep_x = KEEP_VISIBLE.min(width) as i32;
    let min_x = work.x - (width as i32 - keep_x);
    let max_x = (work.right() - keep_x).max(min_x);
    let keep_y = KEEP_VISIBLE.min(height) as i32;
    let max_y = (work.bottom() - keep_y).max(work.y);

    Rect {
        x: frame.x.clamp(min_x, max_x),
        y: frame.y.clamp(work.y, max_y),
        width,
        height,
    }
}

// ---- desktop layout -----------------------------------------------------

/// Where a window sits relative to the work area.
///
/// `Floating` is a frame the operator placed. The edge and corner zones are
/// what snapping and the tiling keys produce; they are stored as the *zone*
/// rather than the resulting frame so the window re-snaps correctly when the
/// work area changes (a monitor unplugged, the dock moved) instead of keeping
/// a half that is no longer half of anything.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Snap {
    Floating,
    Maximized,
    Left,
    Right,
    Top,
    Bottom,
    TopLeft,
    TopRight,
    BottomLeft,
    BottomRight,
    /// Placed by [`WindowSet::tile`]. Distinct from `Floating` so a tiled
    /// window can be told apart from one the operator dropped there, and
    /// distinct from the zones because the tiler owns the frame.
    Tiled,
}

impl Snap {
    /// The text in the database. Kept equal to the serde name so one window
    /// has one spelling everywhere.
    pub fn as_str(self) -> &'static str {
        match self {
            Snap::Floating => "floating",
            Snap::Maximized => "maximized",
            Snap::Left => "left",
            Snap::Right => "right",
            Snap::Top => "top",
            Snap::Bottom => "bottom",
            Snap::TopLeft => "top_left",
            Snap::TopRight => "top_right",
            Snap::BottomLeft => "bottom_left",
            Snap::BottomRight => "bottom_right",
            Snap::Tiled => "tiled",
        }
    }

    pub fn parse(s: &str) -> Option<Snap> {
        Some(match s {
            "floating" => Snap::Floating,
            "maximized" => Snap::Maximized,
            "left" => Snap::Left,
            "right" => Snap::Right,
            "top" => Snap::Top,
            "bottom" => Snap::Bottom,
            "top_left" => Snap::TopLeft,
            "top_right" => Snap::TopRight,
            "bottom_left" => Snap::BottomLeft,
            "bottom_right" => Snap::BottomRight,
            "tiled" => Snap::Tiled,
            _ => return None,
        })
    }

    /// The frame this zone means inside `work`, or `None` for the two states
    /// that carry their own frame.
    pub fn frame(self, work: Rect) -> Option<Rect> {
        let half_w = work.width / 2;
        let half_h = work.height / 2;
        let right_w = work.width - half_w;
        let bottom_h = work.height - half_h;
        let mid_x = work.x + half_w as i32;
        let mid_y = work.y + half_h as i32;

        Some(match self {
            Snap::Floating | Snap::Tiled => return None,
            Snap::Maximized => work,
            Snap::Left => Rect::new(work.x, work.y, half_w, work.height),
            Snap::Right => Rect::new(mid_x, work.y, right_w, work.height),
            Snap::Top => Rect::new(work.x, work.y, work.width, half_h),
            Snap::Bottom => Rect::new(work.x, mid_y, work.width, bottom_h),
            Snap::TopLeft => Rect::new(work.x, work.y, half_w, half_h),
            Snap::TopRight => Rect::new(mid_x, work.y, right_w, half_h),
            Snap::BottomLeft => Rect::new(work.x, mid_y, half_w, bottom_h),
            Snap::BottomRight => Rect::new(mid_x, mid_y, right_w, bottom_h),
        })
    }
}

/// One window's place in the desktop layout.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct DesktopPlacement {
    pub frame: Rect,
    pub snap: Snap,
    /// The floating frame to go back to when the window is unsnapped or
    /// untiled. Set when leaving `Floating`, cleared on the way back, so
    /// snapping twice does not forget where the window came from.
    pub restore: Option<Rect>,
    pub minimized: bool,
    /// Showing in its own browser window, for a second monitor. It keeps its
    /// frame and its place in z-order, so closing the pop-out puts it back
    /// where it was rather than on top of everything.
    pub popped_out: bool,
}

/// Positions, sizes, z-order and snapping: everything that is true of the
/// windows only on a wide screen with a pointer.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DesktopLayout {
    places: BTreeMap<WindowId, DesktopPlacement>,
    /// Back to front. Every window in the set appears exactly once.
    order: Vec<WindowId>,
    focus: Option<WindowId>,
}

impl DesktopLayout {
    pub fn placement(&self, id: WindowId) -> Option<&DesktopPlacement> {
        self.places.get(&id)
    }

    /// Back to front: paint in this order, and the last one is on top.
    pub fn stacking(&self) -> &[WindowId] {
        &self.order
    }

    pub fn focus(&self) -> Option<WindowId> {
        self.focus
    }

    /// Topmost window that is actually on the desktop, which is what focus
    /// falls back to when the focused window closes.
    fn topmost_visible(&self) -> Option<WindowId> {
        self.order
            .iter()
            .rev()
            .copied()
            .find(|id| self.places.get(id).is_some_and(|p| !p.minimized))
    }

    fn insert(&mut self, id: WindowId, placement: DesktopPlacement) {
        self.places.insert(id, placement);
        self.order.retain(|&other| other != id);
        self.order.push(id);
    }

    fn remove(&mut self, id: WindowId) {
        self.places.remove(&id);
        self.order.retain(|&other| other != id);
        if self.focus == Some(id) {
            self.focus = self.topmost_visible();
        }
    }

    fn raise(&mut self, id: WindowId) {
        if self.places.contains_key(&id) {
            self.order.retain(|&other| other != id);
            self.order.push(id);
        }
    }
}

// ---- compact layout -----------------------------------------------------

/// Where a split's divider sits, as a fraction of the long axis.
///
/// Clamped on construction, never refused: this value arrives from a drag.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Divider(f32);

impl Divider {
    pub fn new(fraction: f32) -> Divider {
        // NaN compares false against everything, so it would survive a plain
        // clamp and then poison the layout. It becomes the default instead.
        if !fraction.is_finite() {
            return Divider(DEFAULT_DIVIDER);
        }
        Divider(fraction.clamp(DIVIDER_RANGE.0, DIVIDER_RANGE.1))
    }

    pub fn fraction(self) -> f32 {
        self.0
    }
}

impl Default for Divider {
    fn default() -> Divider {
        Divider(DEFAULT_DIVIDER)
    }
}

/// Two windows sharing a compact screen.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Split {
    /// The leading pane: left in landscape, top in portrait.
    pub primary: WindowId,
    pub secondary: WindowId,
    pub divider: Divider,
}

/// Focus order and the split pair: everything that is true of the windows
/// only on a narrow or touch-first screen.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct CompactLayout {
    /// Most recently focused first. This is the switcher's order, and its
    /// head is the window filling the screen.
    order: Vec<WindowId>,
    split: Option<Split>,
}

impl CompactLayout {
    /// Most recently focused first.
    pub fn switcher(&self) -> &[WindowId] {
        &self.order
    }

    /// The window filling the screen, or the split's primary.
    pub fn front(&self) -> Option<WindowId> {
        match self.split {
            Some(split) => Some(split.primary),
            None => self.order.first().copied(),
        }
    }

    pub fn split(&self) -> Option<Split> {
        self.split
    }

    fn insert_front(&mut self, id: WindowId) {
        self.order.retain(|&other| other != id);
        self.order.insert(0, id);
    }

    fn remove(&mut self, id: WindowId) {
        self.order.retain(|&other| other != id);
        // A split with a closed member is not a split. The survivor stays
        // open and simply fills the screen.
        if let Some(split) = self.split
            && (split.primary == id || split.secondary == id)
        {
            self.split = None;
        }
    }
}

// ---- mode ---------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    Desktop,
    Compact,
}

impl Mode {
    pub fn as_str(self) -> &'static str {
        match self {
            Mode::Desktop => "desktop",
            Mode::Compact => "compact",
        }
    }

    pub fn parse(s: &str) -> Option<Mode> {
        match s {
            "desktop" => Some(Mode::Desktop),
            "compact" => Some(Mode::Compact),
            _ => None,
        }
    }
}

/// The finest pointer the viewport has, as the browser's `pointer` media
/// query reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Pointer {
    /// Mouse, trackpad or stylus: `pointer: fine`.
    Fine,
    /// Touch only: `pointer: coarse`.
    Coarse,
    /// Keyboard or remote only: `pointer: none`.
    None,
}

impl Pointer {
    pub fn is_fine(self) -> bool {
        matches!(self, Pointer::Fine)
    }
}

/// What the client reports about the surface it is drawing on. Reported, not
/// sniffed: the user agent is useless here, because one device is several
/// viewports.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Viewport {
    /// CSS pixels.
    pub width: u32,
    pub height: u32,
    pub pointer: Pointer,
    /// Where a fold or hinge crosses the viewport, as a fraction of the long
    /// axis, from the Viewport Segments API. `None` on a flat screen.
    pub hinge: Option<f32>,
}

impl Viewport {
    /// Where a new split's divider goes: the hinge if the screen has one, so
    /// an unfolded Fold splits along the crease instead of across it.
    pub fn default_divider(&self) -> Divider {
        match self.hinge {
            Some(fraction) => Divider::new(fraction),
            None => Divider::default(),
        }
    }
}

/// Wide screen with a pointer gets desktop mode; narrow or touch-first gets
/// compact.
///
/// Width alone is not enough: a 10-inch tablet in landscape is wide and still
/// touch-first, where overlapping windows and a 2-pixel resize edge are worse
/// than one window at a time. A pointer alone is not enough either, since a
/// mouse paired to a phone does not make the phone a desktop. Both, or
/// compact — and the operator can overrule it.
pub fn automatic_mode(viewport: &Viewport) -> Mode {
    if viewport.width >= DESKTOP_MIN_WIDTH && viewport.pointer.is_fine() {
        Mode::Desktop
    } else {
        Mode::Compact
    }
}

/// The automatic rule plus the operator's override.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModePolicy {
    manual: Option<Mode>,
}

impl ModePolicy {
    pub fn resolve(&self, viewport: &Viewport) -> Mode {
        self.manual.unwrap_or_else(|| automatic_mode(viewport))
    }

    /// `None` goes back to following the viewport.
    pub fn set(&mut self, mode: Option<Mode>) {
        self.manual = mode;
    }

    pub fn manual(&self) -> Option<Mode> {
        self.manual
    }
}

// ---- errors -------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum WindowError {
    #[error(
        "no window {0}: it was closed, possibly from another device; \
         reload the window list before acting on it again"
    )]
    NoSuchWindow(WindowId),

    #[error("window {0} cannot be split with itself; pick a second window from the switcher")]
    SplitWithSelf(WindowId),

    #[error("there is no split to adjust; split two windows first")]
    NotSplit,
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum RestoreError {
    #[error(
        "the stored layout mentions window {0}, which has no window row; \
         the state database is inconsistent, so delete the layout rows for \
         that id or restore the database from a backup"
    )]
    OrphanLayout(WindowId),

    #[error(
        "the stored desktop layout has snap zone {0:?}, which this build does \
         not know; it was written by a newer Deckjack, so upgrade rather than \
         reinterpreting the layout"
    )]
    UnknownSnap(String),

    #[error(
        "the stored window mode {0:?} is not one this build knows; upgrade \
         Deckjack, or clear the mode override to go back to automatic"
    )]
    UnknownMode(String),
}

// ---- the set ------------------------------------------------------------

/// What [`WindowSet::open`] is told. Everything here comes from the app
/// contract and the command that opened the window; nothing is guessed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OpenSpec {
    pub app: AppInstance,
    pub title: String,
    pub node_id: String,
    /// The app contract's minimum. Zero means it declares none.
    pub min_width: u32,
    /// The app contract's preferred wide size. Clamped into the work area.
    pub size: Size,
}

/// Every open window, with both layouts over it and the mode policy.
///
/// This is the authoritative copy; the browser holds a projection of it and
/// the store holds a [`Snapshot`] of it. All three describe the same windows.
#[derive(Debug, Clone, PartialEq)]
pub struct WindowSet {
    windows: BTreeMap<WindowId, Window>,
    desktop: DesktopLayout,
    compact: CompactLayout,
    /// Next id to hand out. Ids are never reused within a run; after a
    /// restart the counter resumes above the highest id that survived, which
    /// may reuse the id of a window closed before the restart. Nothing holds
    /// a reference to a closed window across a restart, so that is harmless.
    next_id: u64,
    mode: ModePolicy,
}

impl Default for WindowSet {
    fn default() -> WindowSet {
        WindowSet::new()
    }
}

impl WindowSet {
    pub fn new() -> WindowSet {
        WindowSet {
            windows: BTreeMap::new(),
            desktop: DesktopLayout::default(),
            compact: CompactLayout::default(),
            next_id: 1,
            mode: ModePolicy::default(),
        }
    }

    // ---- reading --------------------------------------------------------

    pub fn len(&self) -> usize {
        self.windows.len()
    }

    pub fn is_empty(&self) -> bool {
        self.windows.is_empty()
    }

    pub fn window(&self, id: WindowId) -> Option<&Window> {
        self.windows.get(&id)
    }

    /// By id, which is open order. The dock uses this; the switcher and the
    /// stack have their own orders.
    pub fn windows(&self) -> impl Iterator<Item = (WindowId, &Window)> {
        self.windows.iter().map(|(&id, w)| (id, w))
    }

    pub fn desktop(&self) -> &DesktopLayout {
        &self.desktop
    }

    pub fn compact(&self) -> &CompactLayout {
        &self.compact
    }

    pub fn mode(&self, viewport: &Viewport) -> Mode {
        self.mode.resolve(viewport)
    }

    pub fn mode_policy(&self) -> &ModePolicy {
        &self.mode
    }

    /// `None` goes back to following the viewport.
    pub fn set_mode_override(&mut self, mode: Option<Mode>) {
        self.mode.set(mode);
    }

    fn require(&self, id: WindowId) -> Result<(), WindowError> {
        if self.windows.contains_key(&id) {
            Ok(())
        } else {
            Err(WindowError::NoSuchWindow(id))
        }
    }

    // ---- the set itself -------------------------------------------------

    /// Open a window. It enters **both** layouts, because a new window has to
    /// be somewhere on every device, and it takes focus in both, because the
    /// operator asked for it just now wherever they are sitting.
    pub fn open(&mut self, spec: OpenSpec, work_area: Rect, clock: &dyn Clock) -> WindowId {
        let id = WindowId(self.next_id);
        self.next_id = self.next_id.saturating_add(1);

        let min_width = if spec.min_width == 0 {
            DEFAULT_MIN_WIDTH
        } else {
            spec.min_width
        };

        // Cascade from the work area's origin, stepping by how many windows
        // are already placed, so two windows opened back to back do not land
        // exactly on top of each other.
        let step = (self.windows.len() as u32 % CASCADE_WRAP) as i32 * CASCADE_STEP;
        let wanted = Rect {
            x: work_area.x + step,
            y: work_area.y + step,
            width: spec.size.width,
            height: spec.size.height,
        };

        self.windows.insert(
            id,
            Window {
                app: spec.app,
                title: spec.title,
                node_id: spec.node_id,
                min_width,
                opened_at: clock.now(),
            },
        );
        self.desktop.insert(
            id,
            DesktopPlacement {
                frame: fit(wanted, work_area, min_width),
                snap: Snap::Floating,
                restore: None,
                minimized: false,
                popped_out: false,
            },
        );
        self.desktop.focus = Some(id);
        self.compact.insert_front(id);
        id
    }

    /// Close a window and take it out of both layouts. Returns what was
    /// closed so the caller can tell the app instance to shut down.
    pub fn close(&mut self, id: WindowId) -> Result<Window, WindowError> {
        let window = self
            .windows
            .remove(&id)
            .ok_or(WindowError::NoSuchWindow(id))?;
        self.desktop.remove(id);
        self.compact.remove(id);
        Ok(window)
    }

    /// Close every window whose backend runs on a node, for when that node
    /// goes away. The windows would otherwise sit there answering nothing.
    pub fn close_all_on_node(&mut self, node_id: &str) -> Vec<(WindowId, Window)> {
        let doomed: Vec<WindowId> = self
            .windows
            .iter()
            .filter(|(_, w)| w.node_id == node_id)
            .map(|(&id, _)| id)
            .collect();

        doomed
            .into_iter()
            .filter_map(|id| self.close(id).ok().map(|w| (id, w)))
            .collect()
    }

    /// The app owns its title; the shell only displays it.
    pub fn set_title(&mut self, id: WindowId, title: impl Into<String>) -> Result<(), WindowError> {
        let window = self
            .windows
            .get_mut(&id)
            .ok_or(WindowError::NoSuchWindow(id))?;
        window.title = title.into();
        Ok(())
    }

    /// Focus a window, in the layouts the given mode is allowed to touch.
    ///
    /// This is the one place the two layouts are not independent, and the
    /// coupling runs one way only. Focusing on a desktop also moves the
    /// window to the head of the compact focus order, so picking up the phone
    /// shows the switcher ordered by what you were just doing. Focusing in
    /// compact mode never restacks the desktop, because the desktop
    /// arrangement is the thing that must survive using the phone.
    pub fn focus(&mut self, id: WindowId, mode: Mode) -> Result<(), WindowError> {
        self.require(id)?;
        self.compact.insert_front(id);

        if mode == Mode::Desktop {
            // A focused window cannot stay minimized; raising it is the whole
            // point of clicking it in the dock.
            if let Some(placement) = self.desktop.places.get_mut(&id) {
                placement.minimized = false;
            }
            self.desktop.raise(id);
            self.desktop.focus = Some(id);
        }
        Ok(())
    }

    // ---- desktop layout -------------------------------------------------

    /// Move and resize in one step: this is what a drag sends. The frame is
    /// clamped into the work area and to the app's minimum width, and the
    /// window stops being snapped or tiled, since the operator has just
    /// placed it by hand.
    pub fn place(
        &mut self,
        id: WindowId,
        frame: Rect,
        work_area: Rect,
    ) -> Result<DesktopPlacement, WindowError> {
        let min_width = self
            .windows
            .get(&id)
            .ok_or(WindowError::NoSuchWindow(id))?
            .min_width;
        let placement = self
            .desktop
            .places
            .get_mut(&id)
            .ok_or(WindowError::NoSuchWindow(id))?;

        placement.frame = fit(frame, work_area, min_width);
        placement.snap = Snap::Floating;
        placement.restore = None;
        Ok(*placement)
    }

    /// Move without resizing.
    pub fn move_to(
        &mut self,
        id: WindowId,
        x: i32,
        y: i32,
        work_area: Rect,
    ) -> Result<DesktopPlacement, WindowError> {
        let current = self.placement_of(id)?.frame;
        self.place(
            id,
            Rect {
                x,
                y,
                width: current.width,
                height: current.height,
            },
            work_area,
        )
    }

    /// Resize without moving the top left corner.
    pub fn resize(
        &mut self,
        id: WindowId,
        size: Size,
        work_area: Rect,
    ) -> Result<DesktopPlacement, WindowError> {
        let current = self.placement_of(id)?.frame;
        self.place(
            id,
            Rect {
                x: current.x,
                y: current.y,
                width: size.width,
                height: size.height,
            },
            work_area,
        )
    }

    /// Snap to an edge or corner, or maximize. The floating frame is
    /// remembered so [`WindowSet::unsnap`] can put the window back.
    pub fn snap(
        &mut self,
        id: WindowId,
        snap: Snap,
        work_area: Rect,
    ) -> Result<DesktopPlacement, WindowError> {
        let min_width = self
            .windows
            .get(&id)
            .ok_or(WindowError::NoSuchWindow(id))?
            .min_width;
        let placement = self
            .desktop
            .places
            .get_mut(&id)
            .ok_or(WindowError::NoSuchWindow(id))?;

        match snap.frame(work_area) {
            Some(frame) => {
                if placement.snap == Snap::Floating {
                    placement.restore = Some(placement.frame);
                }
                placement.frame = fit(frame, work_area, min_width);
                placement.snap = snap;
            }
            // `Floating` and `Tiled` carry no zone of their own; asking to
            // snap to either means "stop being snapped".
            None => {
                if let Some(restore) = placement.restore.take() {
                    placement.frame = fit(restore, work_area, min_width);
                }
                placement.snap = Snap::Floating;
            }
        }
        Ok(*placement)
    }

    /// Back to the floating frame the window had before it was snapped.
    pub fn unsnap(
        &mut self,
        id: WindowId,
        work_area: Rect,
    ) -> Result<DesktopPlacement, WindowError> {
        self.snap(id, Snap::Floating, work_area)
    }

    /// Lay every window that is on the desktop into a grid: the
    /// keyboard-driven tiling, applied to the whole screen.
    ///
    /// Minimized and popped-out windows are left out, the first because it is
    /// not on the desktop and the second because it is on another monitor
    /// where this work area does not apply.
    pub fn tile(&mut self, work_area: Rect) -> Vec<WindowId> {
        let tiled: Vec<WindowId> = self
            .desktop
            .order
            .iter()
            .copied()
            .filter(|id| {
                self.desktop
                    .places
                    .get(id)
                    .is_some_and(|p| !p.minimized && !p.popped_out)
            })
            .collect();

        if tiled.is_empty() {
            return tiled;
        }

        // Start from a square-ish grid, then drop columns until every cell
        // clears the widest minimum in the set. Overlapping is better than
        // cells too narrow for the panels in them.
        let widest_min = tiled
            .iter()
            .filter_map(|id| self.windows.get(id))
            .map(|w| w.min_width)
            .max()
            .unwrap_or(DEFAULT_MIN_WIDTH);

        let mut cols = (tiled.len() as f64).sqrt().ceil().max(1.0) as u32;
        while cols > 1 && work_area.width / cols < widest_min {
            cols -= 1;
        }
        let rows = (tiled.len() as u32).div_ceil(cols);

        let cell_w = work_area.width / cols;
        let cell_h = work_area.height / rows;

        for (index, &id) in tiled.iter().enumerate() {
            let col = index as u32 % cols;
            let row = index as u32 / cols;
            // The last column and row absorb the integer-division remainder,
            // so the grid reaches the edges with no dead strip.
            let width = if col + 1 == cols {
                work_area.width - cell_w * col
            } else {
                cell_w
            };
            let height = if row + 1 == rows {
                work_area.height - cell_h * row
            } else {
                cell_h
            };
            let frame = Rect::new(
                work_area.x + (cell_w * col) as i32,
                work_area.y + (cell_h * row) as i32,
                width,
                height,
            );
            let min_width = self
                .windows
                .get(&id)
                .map(|w| w.min_width)
                .unwrap_or(DEFAULT_MIN_WIDTH);

            if let Some(placement) = self.desktop.places.get_mut(&id) {
                if placement.snap == Snap::Floating {
                    placement.restore = Some(placement.frame);
                }
                placement.frame = fit(frame, work_area, min_width);
                placement.snap = Snap::Tiled;
            }
        }
        tiled
    }

    pub fn minimize(&mut self, id: WindowId) -> Result<(), WindowError> {
        let placement = self
            .desktop
            .places
            .get_mut(&id)
            .ok_or(WindowError::NoSuchWindow(id))?;
        placement.minimized = true;
        if self.desktop.focus == Some(id) {
            self.desktop.focus = self.desktop.topmost_visible();
        }
        Ok(())
    }

    /// Unminimize and raise, which is what clicking the dock entry does.
    pub fn unminimize(&mut self, id: WindowId) -> Result<(), WindowError> {
        self.require(id)?;
        self.focus(id, Mode::Desktop)
    }

    /// Hand a window to its own browser window, or take it back.
    ///
    /// Only the flag lives here. Which browser window is showing it is the
    /// client's business: the server cannot tell one tab from another and
    /// should not pretend to.
    pub fn set_popped_out(&mut self, id: WindowId, popped_out: bool) -> Result<(), WindowError> {
        let placement = self
            .desktop
            .places
            .get_mut(&id)
            .ok_or(WindowError::NoSuchWindow(id))?;
        placement.popped_out = popped_out;
        Ok(())
    }

    pub fn placement_of(&self, id: WindowId) -> Result<DesktopPlacement, WindowError> {
        self.desktop
            .places
            .get(&id)
            .copied()
            .ok_or(WindowError::NoSuchWindow(id))
    }

    // ---- compact layout -------------------------------------------------

    /// Move a window to a position in the switcher. Hand-ordering the
    /// switcher is the compact equivalent of arranging windows, so it is
    /// stored rather than recomputed from focus the next time.
    pub fn reorder(&mut self, id: WindowId, index: usize) -> Result<(), WindowError> {
        self.require(id)?;
        self.compact.order.retain(|&other| other != id);
        let index = index.min(self.compact.order.len());
        self.compact.order.insert(index, id);
        Ok(())
    }

    /// Put two windows side by side, primary first.
    ///
    /// Both move to the head of the switcher: they are what is on screen, and
    /// leaving the split pair buried in the order would make the switcher
    /// disagree with what the operator is looking at.
    pub fn split(
        &mut self,
        primary: WindowId,
        secondary: WindowId,
        divider: Divider,
    ) -> Result<Split, WindowError> {
        self.require(primary)?;
        self.require(secondary)?;
        if primary == secondary {
            return Err(WindowError::SplitWithSelf(primary));
        }

        self.compact.insert_front(secondary);
        self.compact.insert_front(primary);
        let split = Split {
            primary,
            secondary,
            divider,
        };
        self.compact.split = Some(split);
        Ok(split)
    }

    /// Drag the divider.
    pub fn set_divider(&mut self, divider: Divider) -> Result<Split, WindowError> {
        let split = self.compact.split.as_mut().ok_or(WindowError::NotSplit)?;
        split.divider = divider;
        Ok(*split)
    }

    /// Swap the panes, so the secondary leads.
    pub fn swap_split(&mut self) -> Result<Split, WindowError> {
        let split = self.compact.split.as_mut().ok_or(WindowError::NotSplit)?;
        std::mem::swap(&mut split.primary, &mut split.secondary);
        let swapped = *split;
        self.compact.insert_front(swapped.secondary);
        self.compact.insert_front(swapped.primary);
        Ok(swapped)
    }

    /// Back to one window filling the screen. Both windows stay open; the
    /// primary is the one left in front.
    pub fn unsplit(&mut self) -> Option<Split> {
        let split = self.compact.split.take()?;
        self.compact.insert_front(split.primary);
        Some(split)
    }

    // ---- persistence ----------------------------------------------------

    /// Everything the store needs, as plain rows.
    ///
    /// Serialization is spelled out rather than derived from the structs, so
    /// a field added here cannot silently change the on-disk shape, and the
    /// SQL in the migration can be read against this function.
    pub fn snapshot(&self) -> Snapshot {
        let windows = self
            .windows
            .iter()
            .map(|(&id, w)| WindowRow {
                id: id.0,
                app: w.app.app.clone(),
                instance: w.app.instance.clone(),
                title: w.title.clone(),
                node_id: w.node_id.clone(),
                min_width: w.min_width,
                opened_at: w.opened_at,
            })
            .collect();

        let desktop = self
            .desktop
            .order
            .iter()
            .enumerate()
            .filter_map(|(z, &id)| {
                let p = self.desktop.places.get(&id)?;
                Some(DesktopRow {
                    window_id: id.0,
                    x: p.frame.x,
                    y: p.frame.y,
                    width: p.frame.width,
                    height: p.frame.height,
                    snap: p.snap.as_str().to_string(),
                    restore_x: p.restore.map(|r| r.x),
                    restore_y: p.restore.map(|r| r.y),
                    restore_width: p.restore.map(|r| r.width),
                    restore_height: p.restore.map(|r| r.height),
                    minimized: p.minimized,
                    popped_out: p.popped_out,
                    z: z as u32,
                })
            })
            .collect();

        let compact = self
            .compact
            .order
            .iter()
            .enumerate()
            .map(|(rank, &id)| CompactRow {
                window_id: id.0,
                focus_rank: rank as u32,
            })
            .collect();

        Snapshot {
            windows,
            desktop,
            compact,
            split: self.compact.split.map(|s| SplitRow {
                primary: s.primary.0,
                secondary: s.secondary.0,
                divider: s.divider.fraction() as f64,
            }),
            shell: ShellRow {
                desktop_focus: self.desktop.focus.map(|id| id.0),
                mode_override: self.mode.manual().map(|m| m.as_str().to_string()),
            },
        }
    }

    /// Rebuild from stored rows.
    ///
    /// Strict about structure — a layout row for a window that does not exist
    /// means the database is inconsistent and guessing would hide it — and
    /// forgiving about gaps: a window with no layout row gets a default
    /// place, because losing a window is worse than placing it badly, and a
    /// split naming a window that is gone is simply dropped, since a split is
    /// a convenience worth one tap, not data.
    pub fn restore(snapshot: Snapshot) -> Result<WindowSet, RestoreError> {
        let mut set = WindowSet::new();

        for row in snapshot.windows {
            set.windows.insert(
                WindowId(row.id),
                Window {
                    app: AppInstance {
                        app: row.app,
                        instance: row.instance,
                    },
                    title: row.title,
                    node_id: row.node_id,
                    min_width: if row.min_width == 0 {
                        DEFAULT_MIN_WIDTH
                    } else {
                        row.min_width
                    },
                    opened_at: row.opened_at,
                },
            );
        }
        set.next_id = set
            .windows
            .keys()
            .map(|id| id.0)
            .max()
            .map_or(1, |highest| highest.saturating_add(1));

        let mut stacked: Vec<(u32, WindowId)> = Vec::with_capacity(snapshot.desktop.len());
        for row in snapshot.desktop {
            let id = WindowId(row.window_id);
            if !set.windows.contains_key(&id) {
                return Err(RestoreError::OrphanLayout(id));
            }
            let snap = Snap::parse(&row.snap)
                .ok_or_else(|| RestoreError::UnknownSnap(row.snap.clone()))?;
            let restore = match (
                row.restore_x,
                row.restore_y,
                row.restore_width,
                row.restore_height,
            ) {
                (Some(x), Some(y), Some(width), Some(height)) => {
                    Some(Rect::new(x, y, width, height))
                }
                // A half-written restore frame is no restore frame; the
                // window simply keeps the frame it has.
                _ => None,
            };
            set.desktop.places.insert(
                id,
                DesktopPlacement {
                    frame: Rect::new(row.x, row.y, row.width, row.height),
                    snap,
                    restore,
                    minimized: row.minimized,
                    popped_out: row.popped_out,
                },
            );
            stacked.push((row.z, id));
        }
        // Ties broken by id so the stack is the same on every restore.
        stacked.sort_by_key(|&(z, id)| (z, id));
        set.desktop.order = stacked.into_iter().map(|(_, id)| id).collect();

        let mut ranked: Vec<(u32, WindowId)> = Vec::with_capacity(snapshot.compact.len());
        for row in snapshot.compact {
            let id = WindowId(row.window_id);
            if !set.windows.contains_key(&id) {
                return Err(RestoreError::OrphanLayout(id));
            }
            ranked.push((row.focus_rank, id));
        }
        ranked.sort_by_key(|&(rank, id)| (rank, id));
        set.compact.order = ranked.into_iter().map(|(_, id)| id).collect();

        // Fill the gaps. Both layouts must cover every window, or the shell
        // has an open window it cannot show.
        let ids: Vec<WindowId> = set.windows.keys().copied().collect();
        for id in ids {
            if !set.desktop.places.contains_key(&id) {
                let min_width = set
                    .windows
                    .get(&id)
                    .map_or(DEFAULT_MIN_WIDTH, |w| w.min_width);
                let step = (set.desktop.order.len() as u32 % CASCADE_WRAP) as i32 * CASCADE_STEP;
                let wanted = Rect {
                    x: FALLBACK_WORK_AREA.x + step,
                    y: FALLBACK_WORK_AREA.y + step,
                    width: FALLBACK_WORK_AREA.width / 2,
                    height: FALLBACK_WORK_AREA.height / 2,
                };
                tracing::warn!(window = %id, "no stored desktop placement; placing it by default");
                set.desktop.insert(
                    id,
                    DesktopPlacement {
                        frame: fit(wanted, FALLBACK_WORK_AREA, min_width),
                        snap: Snap::Floating,
                        restore: None,
                        minimized: false,
                        popped_out: false,
                    },
                );
            }
            if !set.compact.order.contains(&id) {
                tracing::warn!(window = %id, "no stored switcher position; appending it");
                set.compact.order.push(id);
            }
        }

        set.desktop.focus = match snapshot.shell.desktop_focus.map(WindowId) {
            // Focus pointing at a window that is gone is not worth refusing
            // a start over; the topmost window takes it.
            Some(id) if set.windows.contains_key(&id) => Some(id),
            Some(_) | None => set.desktop.topmost_visible(),
        };

        if let Some(text) = snapshot.shell.mode_override {
            let mode = Mode::parse(&text).ok_or(RestoreError::UnknownMode(text))?;
            set.mode.set(Some(mode));
        }

        if let Some(row) = snapshot.split {
            let primary = WindowId(row.primary);
            let secondary = WindowId(row.secondary);
            let both_open =
                set.windows.contains_key(&primary) && set.windows.contains_key(&secondary);
            if both_open && primary != secondary {
                set.compact.split = Some(Split {
                    primary,
                    secondary,
                    divider: Divider::new(row.divider as f32),
                });
            } else {
                tracing::warn!("stored split names a window that is not open; dropping the split");
            }
        }

        Ok(set)
    }
}

// ---- stored rows --------------------------------------------------------

/// One row per `windows` row in the state database.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WindowRow {
    pub id: u64,
    pub app: String,
    pub instance: String,
    pub title: String,
    pub node_id: String,
    pub min_width: u32,
    pub opened_at: u64,
}

/// One row per `window_desktop` row. `z` is 0 at the back.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DesktopRow {
    pub window_id: u64,
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
    pub snap: String,
    pub restore_x: Option<i32>,
    pub restore_y: Option<i32>,
    pub restore_width: Option<u32>,
    pub restore_height: Option<u32>,
    pub minimized: bool,
    pub popped_out: bool,
    pub z: u32,
}

/// One row per `window_compact` row. `focus_rank` is 0 for the most recently
/// focused window.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompactRow {
    pub window_id: u64,
    pub focus_rank: u32,
}

/// The single `window_split` row, if there is a split.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct SplitRow {
    pub primary: u64,
    pub secondary: u64,
    pub divider: f64,
}

/// The single `window_shell` row: the bits of state that belong to the shell
/// rather than to any one window.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ShellRow {
    pub desktop_focus: Option<u64>,
    /// `None` is automatic mode.
    pub mode_override: Option<String>,
}

/// A whole [`WindowSet`] as rows. Written in one transaction, read in one go.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct Snapshot {
    pub windows: Vec<WindowRow>,
    pub desktop: Vec<DesktopRow>,
    pub compact: Vec<CompactRow>,
    pub split: Option<SplitRow>,
    pub shell: ShellRow,
}
