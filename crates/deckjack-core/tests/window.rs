//! Window manager invariants, exercised through the public API only.
//!
//! The claim this module exists to make is that the two layouts over one set
//! of windows are independent: using the phone must not scramble the desktop
//! arrangement, and arranging the desktop must not scramble the switcher
//! beyond the one documented leak (focus). Most of what follows is aimed at
//! that, at the failure paths of `restore`, and at the geometry clamps that
//! keep a server-held layout reachable from every device.

use deckjack_core::window::{
    AppInstance, Clock, DEFAULT_MIN_WIDTH, DESKTOP_MIN_WIDTH, DIVIDER_RANGE, Divider, KEEP_VISIBLE,
    MIN_HEIGHT, Mode, ModePolicy, OpenSpec, Pointer, Rect, RestoreError, Size, Snap, Snapshot,
    Viewport, Window, WindowError, WindowId, WindowSet, automatic_mode,
};

// ---- harness ------------------------------------------------------------

/// No test may depend on the wall clock, so the one time value a window
/// carries is injected. It also advances only when a test says so, which is
/// what makes `opened_at` assertable at all.
struct TestClock(std::cell::Cell<u64>);

impl TestClock {
    fn at(secs: u64) -> TestClock {
        TestClock(std::cell::Cell::new(secs))
    }

    fn advance(&self, secs: u64) {
        self.0.set(self.0.get() + secs);
    }
}

impl Clock for TestClock {
    fn now(&self) -> u64 {
        self.0.get()
    }
}

/// A realistic desktop work area: not at the origin, because a shell bar sits
/// above it, and a work area whose `y` is 0 hides every off-by-`work.y` bug.
fn work() -> Rect {
    Rect::new(0, 32, 2560, 1408)
}

/// A phone-sized work area.
fn small_work() -> Rect {
    Rect::new(0, 0, 412, 915)
}

fn spec(app: &str, min_width: u32) -> OpenSpec {
    OpenSpec {
        app: AppInstance::new(app, format!("{app}-1")),
        title: format!("{app} — test"),
        node_id: "node-a".to_string(),
        min_width,
        size: Size {
            width: 1100,
            height: 760,
        },
    }
}

fn open(set: &mut WindowSet, app: &str) -> WindowId {
    set.open(spec(app, 420), work(), &TestClock::at(1_000))
}

fn viewport(width: u32, pointer: Pointer) -> Viewport {
    Viewport {
        width,
        height: 1440,
        pointer,
        hinge: None,
    }
}

// ---- the invariant worth the most: the two layouts are independent ------

/// Everything the compact layout can be told to do, applied to a desktop
/// arrangement that took work to build. The desktop layout must come out bit
/// for bit the same: that is the whole reason the module keeps two layouts.
#[test]
fn rearranging_the_compact_layout_never_alters_the_desktop_layout() {
    let mut set = WindowSet::new();
    let a = open(&mut set, "files");
    let b = open(&mut set, "terminal");
    let c = open(&mut set, "mail");

    // A desktop arrangement with every kind of placement in it.
    set.snap(a, Snap::Left, work()).unwrap();
    set.place(b, Rect::new(700, 200, 900, 600), work()).unwrap();
    set.snap(c, Snap::BottomRight, work()).unwrap();
    set.minimize(b).unwrap();
    set.focus(c, Mode::Desktop).unwrap();
    let before = set.desktop().clone();

    set.focus(a, Mode::Compact).unwrap();
    set.reorder(b, 0).unwrap();
    set.reorder(c, 2).unwrap();
    let split = set.split(b, a, Divider::new(0.4)).unwrap();
    set.set_divider(Divider::new(0.7)).unwrap();
    set.swap_split().unwrap();
    set.unsplit().unwrap();
    set.focus(b, Mode::Compact).unwrap();

    assert_eq!(
        set.desktop(),
        &before,
        "a phone rearranging its own layout moved the desktop"
    );
    // And the compact side really did change, so the comparison above is not
    // passing because nothing happened.
    assert_ne!(split.divider.fraction(), 0.7);
    assert_eq!(set.compact().switcher().first().copied(), Some(b));
}

/// The mirror image, for every desktop operation that is not focus. Focus is
/// the one documented leak and has its own test below.
#[test]
fn rearranging_the_desktop_never_alters_the_compact_layout() {
    let mut set = WindowSet::new();
    let a = open(&mut set, "files");
    let b = open(&mut set, "terminal");
    let c = open(&mut set, "mail");

    set.split(a, c, Divider::new(0.6)).unwrap();
    set.reorder(b, 1).unwrap();
    let before = set.compact().clone();

    set.place(a, Rect::new(10, 40, 800, 500), work()).unwrap();
    set.move_to(b, 1200, 300, work()).unwrap();
    set.resize(
        c,
        Size {
            width: 500,
            height: 400,
        },
        work(),
    )
    .unwrap();
    set.snap(a, Snap::Maximized, work()).unwrap();
    set.unsnap(a, work()).unwrap();
    set.minimize(b).unwrap();
    set.set_popped_out(c, true).unwrap();
    set.tile(work());

    assert_eq!(
        set.compact(),
        &before,
        "arranging the desktop reordered the phone's switcher"
    );
}

/// The one coupling, and its direction. Focusing on a desktop promotes the
/// window in the switcher, so picking up the phone shows what you were just
/// doing; focusing in compact mode leaves the desktop stack alone, because
/// the desktop arrangement is the thing that must survive the phone.
#[test]
fn desktop_focus_promotes_in_the_switcher_but_compact_focus_never_restacks() {
    let mut set = WindowSet::new();
    let a = open(&mut set, "files");
    let b = open(&mut set, "terminal");
    let c = open(&mut set, "mail");

    let stack_before = set.desktop().stacking().to_vec();
    set.focus(a, Mode::Compact).unwrap();
    assert_eq!(set.compact().switcher().first().copied(), Some(a));
    assert_eq!(
        set.desktop().stacking(),
        stack_before.as_slice(),
        "compact focus restacked the desktop"
    );
    assert_eq!(
        set.desktop().focus(),
        Some(c),
        "compact focus moved desktop focus"
    );

    set.focus(b, Mode::Desktop).unwrap();
    assert_eq!(set.desktop().stacking().last().copied(), Some(b));
    assert_eq!(
        set.compact().switcher().first().copied(),
        Some(b),
        "desktop focus must promote the window in the switcher"
    );
}

/// Minimizing is a desktop-only state. A minimized window is still perfectly
/// usable on a phone, which has no such concept, so it keeps its place in the
/// switcher and can still be the front window there.
#[test]
fn a_minimized_window_is_still_available_in_compact_mode() {
    let mut set = WindowSet::new();
    let a = open(&mut set, "files");
    let _b = open(&mut set, "terminal");

    set.minimize(a).unwrap();
    set.focus(a, Mode::Compact).unwrap();

    assert_eq!(set.compact().front(), Some(a));
    assert!(
        set.placement_of(a).unwrap().minimized,
        "compact focus must not silently unminimize on the desktop"
    );
}

// ---- open, ids ----------------------------------------------------------

/// A new window has to be somewhere on every device, so it enters both
/// layouts and takes focus in both.
#[test]
fn a_new_window_enters_both_layouts_and_takes_focus() {
    let mut set = WindowSet::new();
    let a = open(&mut set, "files");

    assert_eq!(set.desktop().focus(), Some(a));
    assert_eq!(set.desktop().stacking(), &[a]);
    assert!(set.desktop().placement(a).is_some());
    assert_eq!(set.compact().switcher(), &[a]);
    assert_eq!(set.compact().front(), Some(a));
}

/// Ids are handles other state refers to (a pop-out URL, a log line, the
/// Inbox). Within one run a closed window's id must never come back, or those
/// references would silently point at different work.
#[test]
fn ids_are_never_reused_within_a_run() {
    let mut set = WindowSet::new();
    let mut seen = Vec::new();

    for round in 0..5 {
        let id = open(&mut set, "files");
        assert!(!seen.contains(&id), "id {id} handed out twice in one run");
        seen.push(id);
        // Close everything every other round, so the set is empty again and a
        // counter that restarted from the set's length would collide.
        if round % 2 == 1 {
            for &old in &seen {
                let _ = set.close(old);
            }
        }
    }
}

/// An app that declares no minimum still has to be placeable; a sloppy app
/// contract must not produce a window nobody can use.
#[test]
fn an_app_declaring_no_minimum_width_gets_the_default() {
    let mut set = WindowSet::new();
    let id = set.open(spec("sloppy", 0), work(), &TestClock::at(1));

    assert_eq!(set.window(id).unwrap().min_width, DEFAULT_MIN_WIDTH);
    assert!(set.placement_of(id).unwrap().frame.width >= DEFAULT_MIN_WIDTH);
}

/// Two windows opened back to back must not land exactly on top of each
/// other, and the cascade must wrap rather than walking off the screen.
#[test]
fn cascaded_windows_never_coincide_and_never_leave_the_work_area() {
    let mut set = WindowSet::new();
    let mut frames = Vec::new();
    for n in 0..20 {
        let id = open(&mut set, &format!("app{n}"));
        let frame = set.placement_of(id).unwrap().frame;
        // Within one wrap of the cascade no two windows share an origin.
        if let Some(previous) = frames.last() {
            assert_ne!(
                (frame.x, frame.y),
                *previous,
                "window {n} landed exactly on its predecessor"
            );
        }
        frames.push((frame.x, frame.y));
        assert!(
            frame.x <= work().right() - KEEP_VISIBLE as i32 && frame.y >= work().y,
            "window {n} opened out of reach at {frame:?}"
        );
    }
}

/// The clock is injected, and `opened_at` is whatever it said — not the wall
/// clock, and not the same value for every window.
#[test]
fn opened_at_comes_from_the_injected_clock() {
    let clock = TestClock::at(1_700_000_000);
    let mut set = WindowSet::new();
    let a = set.open(spec("files", 420), work(), &clock);
    clock.advance(90);
    let b = set.open(spec("mail", 420), work(), &clock);

    assert_eq!(set.window(a).unwrap().opened_at, 1_700_000_000);
    assert_eq!(set.window(b).unwrap().opened_at, 1_700_000_090);
}

// ---- close --------------------------------------------------------------

/// Closing takes the window out of both layouts and out of the split. A
/// layout still naming a closed window would be a window the shell tries to
/// draw and cannot.
#[test]
fn closing_a_window_removes_it_from_both_layouts_and_from_the_split() {
    let mut set = WindowSet::new();
    let a = open(&mut set, "files");
    let b = open(&mut set, "terminal");
    let c = open(&mut set, "mail");
    set.split(a, b, Divider::default()).unwrap();

    let closed = set.close(a).unwrap();
    assert_eq!(closed.app.app, "files");

    assert!(set.window(a).is_none());
    assert!(
        set.desktop().placement(a).is_none(),
        "desktop placement survived"
    );
    assert!(!set.desktop().stacking().contains(&a), "z-order survived");
    assert!(
        !set.compact().switcher().contains(&a),
        "switcher entry survived"
    );
    assert!(
        set.compact().split().is_none(),
        "split kept a closed member"
    );
    // The survivor of the broken split is still open and now fills the screen.
    assert_eq!(set.compact().front(), Some(b));
    assert!(set.window(c).is_some());
}

/// Focus must land on something a device can actually show. After the focused
/// window closes it goes to the topmost window still on the desktop, skipping
/// minimized ones.
#[test]
fn closing_the_focused_window_passes_focus_to_the_topmost_visible_window() {
    let mut set = WindowSet::new();
    let a = open(&mut set, "files");
    let b = open(&mut set, "terminal");
    let c = open(&mut set, "mail");

    set.minimize(b).unwrap();
    set.focus(c, Mode::Desktop).unwrap();
    assert_eq!(set.desktop().focus(), Some(c));

    set.close(c).unwrap();
    assert_eq!(
        set.desktop().focus(),
        Some(a),
        "focus fell to a minimized window instead of a visible one"
    );

    set.close(a).unwrap();
    assert_eq!(
        set.desktop().focus(),
        None,
        "focus named a minimized window with nothing visible left"
    );
}

/// A node going away closes the windows whose backends it ran, and nothing
/// else: the windows on other nodes are still answering.
#[test]
fn losing_a_node_closes_only_that_nodes_windows() {
    let mut set = WindowSet::new();
    let here = set.open(spec("files", 420), work(), &TestClock::at(1));
    let there = set.open(
        OpenSpec {
            node_id: "node-b".to_string(),
            ..spec("mail", 420)
        },
        work(),
        &TestClock::at(1),
    );

    let closed = set.close_all_on_node("node-b");
    assert_eq!(closed.len(), 1);
    assert_eq!(closed[0].0, there);
    assert!(
        set.window(here).is_some(),
        "a healthy node's window was closed"
    );
    assert!(set.close_all_on_node("node-nonexistent").is_empty());
}

/// Every operation names a window, and that window can have been closed from
/// another device a moment ago. None of them may panic; all of them say which
/// window is gone.
#[test]
fn acting_on_a_closed_window_is_refused_not_panicked() {
    let mut set = WindowSet::new();
    let a = open(&mut set, "files");
    let b = open(&mut set, "terminal");
    set.close(a).unwrap();

    let gone = WindowError::NoSuchWindow(a);
    assert_eq!(set.close(a).unwrap_err(), gone);
    assert_eq!(set.set_title(a, "x").unwrap_err(), gone);
    assert_eq!(set.focus(a, Mode::Desktop).unwrap_err(), gone);
    assert_eq!(
        set.place(a, Rect::new(0, 0, 10, 10), work()).unwrap_err(),
        gone
    );
    assert_eq!(set.move_to(a, 5, 5, work()).unwrap_err(), gone);
    assert_eq!(
        set.resize(
            a,
            Size {
                width: 10,
                height: 10
            },
            work()
        )
        .unwrap_err(),
        gone
    );
    assert_eq!(set.snap(a, Snap::Left, work()).unwrap_err(), gone);
    assert_eq!(set.unsnap(a, work()).unwrap_err(), gone);
    assert_eq!(set.minimize(a).unwrap_err(), gone);
    assert_eq!(set.unminimize(a).unwrap_err(), gone);
    assert_eq!(set.set_popped_out(a, true).unwrap_err(), gone);
    assert_eq!(set.placement_of(a).unwrap_err(), gone);
    assert_eq!(set.reorder(a, 0).unwrap_err(), gone);
    assert_eq!(set.split(a, b, Divider::default()).unwrap_err(), gone);
    assert_eq!(set.split(b, a, Divider::default()).unwrap_err(), gone);

    // And the refusals left the live window untouched.
    assert_eq!(set.len(), 1);
    assert_eq!(set.compact().switcher(), &[b]);
}

// ---- desktop geometry ---------------------------------------------------

/// A server-held layout follows you to every device, so a window dragged
/// somewhere no pointer can reach would be unreachable everywhere. Enough of
/// it always stays inside the work area to grab.
#[test]
fn a_window_can_never_be_dragged_out_of_reach() {
    let mut set = WindowSet::new();
    let id = open(&mut set, "files");
    let w = work();

    for (x, y) in [
        (-100_000, -100_000),
        (100_000, 100_000),
        (i32::MIN / 2, i32::MAX / 2),
        (0, -1),
    ] {
        let frame = set.move_to(id, x, y, w).unwrap().frame;
        let keep = KEEP_VISIBLE as i32;
        assert!(
            frame.right() >= w.x + keep,
            "dragged off the left edge: {frame:?}"
        );
        assert!(
            frame.x <= w.right() - keep,
            "dragged off the right edge: {frame:?}"
        );
        assert!(
            frame.y >= w.y,
            "title bar ended up above the work area: {frame:?}"
        );
        assert!(
            frame.y <= w.bottom() - keep,
            "dragged off the bottom edge: {frame:?}"
        );
    }
}

/// The operator dragging an edge should always get something. A resize past
/// the app's minimum, or past the work area, clamps rather than failing.
#[test]
fn a_resize_past_the_limits_clamps_rather_than_refusing() {
    let mut set = WindowSet::new();
    let id = set.open(spec("files", 600), work(), &TestClock::at(1));

    let tiny = set
        .resize(
            id,
            Size {
                width: 10,
                height: 10,
            },
            work(),
        )
        .unwrap()
        .frame;
    assert_eq!(
        tiny.width, 600,
        "resize below the app minimum was not clamped"
    );
    assert_eq!(tiny.height, MIN_HEIGHT);

    let huge = set
        .resize(
            id,
            Size {
                width: u32::MAX,
                height: u32::MAX,
            },
            work(),
        )
        .unwrap()
        .frame;
    assert_eq!(huge.width, work().width);
    assert_eq!(huge.height, work().height);
}

/// A work area narrower than the app's minimum gives the window the whole
/// width: a panel that scrolls beats work the operator cannot reach.
#[test]
fn a_work_area_narrower_than_the_app_minimum_still_places_the_window() {
    let mut set = WindowSet::new();
    let id = set.open(spec("editor", 1_200), small_work(), &TestClock::at(1));
    let frame = set.placement_of(id).unwrap().frame;

    assert_eq!(frame.width, small_work().width);
    assert_eq!(frame.x, small_work().x);
}

/// Snapping remembers where the window was floating, and snapping again does
/// not forget it: two snaps then an unsnap must land back on the hand-placed
/// frame, not on the first snap's half of the screen.
#[test]
fn snapping_twice_still_remembers_the_floating_frame() {
    let mut set = WindowSet::new();
    let id = open(&mut set, "files");
    let placed = set
        .place(id, Rect::new(300, 150, 900, 640), work())
        .unwrap()
        .frame;
    assert_eq!(set.placement_of(id).unwrap().restore, None);

    set.snap(id, Snap::Left, work()).unwrap();
    set.snap(id, Snap::BottomRight, work()).unwrap();
    let back = set.unsnap(id, work()).unwrap();

    assert_eq!(back.frame, placed, "unsnap forgot the original frame");
    assert_eq!(back.snap, Snap::Floating);
    assert_eq!(back.restore, None, "the restore frame was not consumed");
}

/// Placing by hand ends any snap or tile: the operator just said where it
/// goes, so there is nothing to go back to.
#[test]
fn placing_by_hand_ends_the_snap_and_drops_the_restore_frame() {
    let mut set = WindowSet::new();
    let id = open(&mut set, "files");
    set.snap(id, Snap::Maximized, work()).unwrap();
    assert!(set.placement_of(id).unwrap().restore.is_some());

    let placed = set.place(id, Rect::new(40, 60, 700, 500), work()).unwrap();
    assert_eq!(placed.snap, Snap::Floating);
    assert_eq!(placed.restore, None);
    assert_eq!(placed.frame, Rect::new(40, 60, 700, 500));
}

/// The zone is stored rather than the frame, so the window re-snaps correctly
/// when the work area changes under it — a monitor unplugged, the dock moved.
/// Every zone must tile the work area with no gap and no overlap between
/// opposite halves.
#[test]
fn every_snap_zone_stays_inside_the_work_area_and_halves_meet_exactly() {
    let w = work();
    let zones = [
        Snap::Maximized,
        Snap::Left,
        Snap::Right,
        Snap::Top,
        Snap::Bottom,
        Snap::TopLeft,
        Snap::TopRight,
        Snap::BottomLeft,
        Snap::BottomRight,
    ];
    for zone in zones {
        let frame = zone.frame(w).expect("a zone must have a frame");
        assert!(
            frame.x >= w.x
                && frame.y >= w.y
                && frame.right() <= w.right()
                && frame.bottom() <= w.bottom(),
            "{zone:?} leaves the work area: {frame:?}"
        );
    }
    // Left and Right together are exactly the work area, whatever the parity
    // of its width: an odd width must not leave a one-pixel seam.
    for width in [2560, 2561] {
        let w = Rect::new(0, 32, width, 1408);
        let left = Snap::Left.frame(w).unwrap();
        let right = Snap::Right.frame(w).unwrap();
        assert_eq!(left.right(), right.x, "a seam between the halves");
        assert_eq!(right.right(), w.right(), "the right half misses the edge");
    }
    // The two frame-carrying states have no zone of their own.
    assert_eq!(Snap::Floating.frame(w), None);
    assert_eq!(Snap::Tiled.frame(w), None);
}

/// Tiling is the keyboard-driven layout for the screen in front of you. A
/// minimized window is not on that screen and a popped-out one is on another,
/// so neither is dragged into the grid.
#[test]
fn tiling_skips_minimized_and_popped_out_windows() {
    let mut set = WindowSet::new();
    let a = open(&mut set, "files");
    let b = open(&mut set, "terminal");
    let c = open(&mut set, "mail");
    let d = open(&mut set, "notes");

    let b_frame = set.placement_of(b).unwrap().frame;
    let c_frame = set.placement_of(c).unwrap().frame;
    set.minimize(b).unwrap();
    set.set_popped_out(c, true).unwrap();

    let tiled = set.tile(work());
    assert_eq!(
        tiled,
        vec![a, d],
        "tiling touched a window not on this screen"
    );
    assert_eq!(set.placement_of(b).unwrap().frame, b_frame);
    assert_eq!(set.placement_of(c).unwrap().frame, c_frame);
    assert_eq!(set.placement_of(a).unwrap().snap, Snap::Tiled);
    assert_eq!(set.placement_of(d).unwrap().snap, Snap::Tiled);

    // Nothing to tile is not an error.
    let mut empty = WindowSet::new();
    assert!(empty.tile(work()).is_empty());
}

/// Overlapping beats cells too narrow for the panels in them, so the grid
/// drops columns until every cell clears the widest minimum in the set.
#[test]
fn tiled_cells_clear_the_widest_app_minimum() {
    let mut set = WindowSet::new();
    let a = set.open(spec("files", 420), work(), &TestClock::at(1));
    let b = set.open(spec("mail", 420), work(), &TestClock::at(1));
    let wide = set.open(spec("editor", 1_300), work(), &TestClock::at(1));

    let tiled = set.tile(work());
    assert_eq!(tiled.len(), 3);
    for id in [a, b, wide] {
        let frame = set.placement_of(id).unwrap().frame;
        assert!(
            frame.width >= 1_300,
            "cell for {id} is {}px wide, under the widest minimum in the set",
            frame.width
        );
        assert!(frame.width <= work().width, "cell wider than the screen");
    }
}

/// Tiling rearranges frames. It is not a focus or raise gesture, so the stack
/// and the focused window come out of it unchanged.
#[test]
fn tiling_preserves_the_stack_and_the_focus() {
    let mut set = WindowSet::new();
    let a = open(&mut set, "files");
    let b = open(&mut set, "terminal");
    set.focus(a, Mode::Desktop).unwrap();

    let stack = set.desktop().stacking().to_vec();
    set.tile(work());

    assert_eq!(set.desktop().stacking(), stack.as_slice());
    assert_eq!(set.desktop().focus(), Some(a));
    assert_eq!(set.desktop().stacking().last().copied(), Some(a));
    let _ = b;
}

/// Untiling puts the window back where it was floating, which means tiling
/// has to remember that frame even though it does not use a snap zone.
#[test]
fn untiling_returns_the_window_to_its_floating_frame() {
    let mut set = WindowSet::new();
    let id = open(&mut set, "files");
    let placed = set
        .place(id, Rect::new(220, 90, 840, 620), work())
        .unwrap()
        .frame;

    set.tile(work());
    assert_eq!(set.placement_of(id).unwrap().snap, Snap::Tiled);
    let back = set.unsnap(id, work()).unwrap();

    assert_eq!(back.frame, placed);
    assert_eq!(back.snap, Snap::Floating);
}

/// Clicking the dock entry of a minimized window has to actually show it:
/// unminimize both clears the flag and raises the window to the top.
#[test]
fn unminimizing_shows_and_raises_the_window() {
    let mut set = WindowSet::new();
    let a = open(&mut set, "files");
    let _b = open(&mut set, "terminal");
    set.minimize(a).unwrap();

    set.unminimize(a).unwrap();
    assert!(!set.placement_of(a).unwrap().minimized);
    assert_eq!(set.desktop().stacking().last().copied(), Some(a));
    assert_eq!(set.desktop().focus(), Some(a));
}

/// A popped-out window is on a second monitor; closing the pop-out must put
/// it back where it was rather than on top of everything.
#[test]
fn popping_out_and_back_keeps_the_frame_and_the_stacking() {
    let mut set = WindowSet::new();
    let a = open(&mut set, "files");
    let b = open(&mut set, "terminal");
    let frame = set.placement_of(a).unwrap().frame;
    let stack = set.desktop().stacking().to_vec();

    set.set_popped_out(a, true).unwrap();
    set.set_popped_out(a, false).unwrap();

    assert_eq!(set.placement_of(a).unwrap().frame, frame);
    assert_eq!(set.desktop().stacking(), stack.as_slice());
    assert_eq!(set.desktop().stacking().last().copied(), Some(b));
}

// ---- compact layout -----------------------------------------------------

/// A split is two windows on one screen. Both belong at the head of the
/// switcher, or the switcher disagrees with what the operator is looking at.
#[test]
fn a_split_puts_both_panes_at_the_head_of_the_switcher() {
    let mut set = WindowSet::new();
    let a = open(&mut set, "files");
    let b = open(&mut set, "terminal");
    let c = open(&mut set, "mail");
    set.focus(c, Mode::Compact).unwrap();

    let split = set.split(a, b, Divider::new(0.35)).unwrap();
    assert_eq!(split.primary, a);
    assert_eq!(set.compact().switcher()[..2], [a, b]);
    assert_eq!(set.compact().front(), Some(a), "the primary pane leads");

    let swapped = set.swap_split().unwrap();
    assert_eq!((swapped.primary, swapped.secondary), (b, a));
    assert_eq!(swapped.divider, split.divider, "swapping moved the divider");
    assert_eq!(set.compact().front(), Some(b));

    let dropped = set.unsplit().unwrap();
    assert_eq!(dropped.primary, b);
    assert!(set.compact().split().is_none());
    assert_eq!(set.compact().front(), Some(b), "the primary stays in front");
    assert!(set.window(a).is_some() && set.window(b).is_some());
}

/// Splitting a window with itself is not a layout, it is a mistake, and it
/// has to be refused rather than silently producing a one-pane split.
#[test]
fn a_window_cannot_be_split_with_itself() {
    let mut set = WindowSet::new();
    let a = open(&mut set, "files");

    assert_eq!(
        set.split(a, a, Divider::default()).unwrap_err(),
        WindowError::SplitWithSelf(a)
    );
    assert!(set.compact().split().is_none());
}

/// Adjusting a split that does not exist is told, not guessed at.
#[test]
fn adjusting_a_split_that_does_not_exist_is_refused() {
    let mut set = WindowSet::new();
    let _a = open(&mut set, "files");

    assert_eq!(
        set.set_divider(Divider::new(0.5)).unwrap_err(),
        WindowError::NotSplit
    );
    assert_eq!(set.swap_split().unwrap_err(), WindowError::NotSplit);
    assert_eq!(set.unsplit(), None);
}

/// The divider arrives from a drag, so an overshoot stops at the end of its
/// travel instead of failing, and a value that is not a number at all becomes
/// the default rather than poisoning the layout.
#[test]
fn a_divider_is_clamped_never_refused() {
    assert_eq!(Divider::new(-5.0).fraction(), DIVIDER_RANGE.0);
    assert_eq!(Divider::new(99.0).fraction(), DIVIDER_RANGE.1);
    assert_eq!(
        Divider::new(f32::NAN).fraction(),
        Divider::default().fraction()
    );
    assert_eq!(
        Divider::new(f32::INFINITY).fraction(),
        Divider::default().fraction()
    );
    assert_eq!(Divider::new(0.33).fraction(), 0.33);

    let mut set = WindowSet::new();
    let a = open(&mut set, "files");
    let b = open(&mut set, "terminal");
    set.split(a, b, Divider::new(f32::NAN)).unwrap();
    let fraction = set.compact().split().unwrap().divider.fraction();
    assert!(fraction.is_finite(), "a NaN divider reached the layout");
}

/// Hand-ordering the switcher is the compact equivalent of arranging windows,
/// so it is stored, and an index past the end lands at the end rather than
/// panicking on an out-of-range insert.
#[test]
fn reordering_the_switcher_is_stored_and_an_index_past_the_end_is_clamped() {
    let mut set = WindowSet::new();
    let a = open(&mut set, "files");
    let b = open(&mut set, "terminal");
    let c = open(&mut set, "mail");
    assert_eq!(set.compact().switcher(), &[c, b, a]);

    set.reorder(a, 0).unwrap();
    assert_eq!(set.compact().switcher(), &[a, c, b]);

    set.reorder(a, usize::MAX).unwrap();
    assert_eq!(set.compact().switcher(), &[c, b, a]);
    assert_eq!(
        set.compact().switcher().len(),
        3,
        "reorder duplicated a window"
    );
}

// ---- mode ---------------------------------------------------------------

/// Mode follows the viewport and its pointer, not the user agent. Width alone
/// is not enough (a touch-first tablet in landscape is wide) and a pointer
/// alone is not enough (a mouse paired to a phone).
#[test]
fn desktop_mode_needs_both_a_wide_viewport_and_a_fine_pointer() {
    assert_eq!(
        automatic_mode(&viewport(DESKTOP_MIN_WIDTH, Pointer::Fine)),
        Mode::Desktop
    );
    assert_eq!(
        automatic_mode(&viewport(DESKTOP_MIN_WIDTH - 1, Pointer::Fine)),
        Mode::Compact
    );
    assert_eq!(
        automatic_mode(&viewport(2560, Pointer::Coarse)),
        Mode::Compact
    );
    assert_eq!(
        automatic_mode(&viewport(2560, Pointer::None)),
        Mode::Compact
    );
    assert_eq!(
        automatic_mode(&viewport(412, Pointer::Coarse)),
        Mode::Compact
    );
}

/// The automatic rule gets some cases wrong (a large tablet with a keyboard
/// case), so the operator can overrule it — and the override wins until it is
/// cleared, whatever the viewport then reports.
#[test]
fn a_manual_mode_override_wins_until_it_is_cleared() {
    let mut set = WindowSet::new();
    let desktop_viewport = viewport(2560, Pointer::Fine);
    let phone_viewport = viewport(412, Pointer::Coarse);
    assert_eq!(set.mode(&desktop_viewport), Mode::Desktop);

    set.set_mode_override(Some(Mode::Compact));
    assert_eq!(
        set.mode(&desktop_viewport),
        Mode::Compact,
        "the override lost"
    );
    assert_eq!(set.mode_policy().manual(), Some(Mode::Compact));

    set.set_mode_override(Some(Mode::Desktop));
    assert_eq!(set.mode(&phone_viewport), Mode::Desktop);

    set.set_mode_override(None);
    assert_eq!(
        set.mode(&phone_viewport),
        Mode::Compact,
        "clearing did not restore automatic"
    );
    assert_eq!(set.mode(&desktop_viewport), Mode::Desktop);
    assert_eq!(set.mode_policy().manual(), None);

    // A fresh policy follows the viewport.
    assert_eq!(
        ModePolicy::default().resolve(&phone_viewport),
        Mode::Compact
    );
}

/// An unfolded Fold splits along the crease rather than across it, and a
/// hinge the browser reports as nonsense falls back to the middle.
#[test]
fn a_split_defaults_to_the_hinge_when_the_screen_has_one() {
    let folded = Viewport {
        hinge: Some(0.5),
        ..viewport(1_812, Pointer::Coarse)
    };
    assert_eq!(folded.default_divider().fraction(), 0.5);

    let off_center = Viewport {
        hinge: Some(0.62),
        ..viewport(1_812, Pointer::Coarse)
    };
    assert_eq!(off_center.default_divider().fraction(), 0.62);

    let nonsense = Viewport {
        hinge: Some(f32::NAN),
        ..viewport(1_812, Pointer::Coarse)
    };
    assert_eq!(
        nonsense.default_divider().fraction(),
        Divider::default().fraction()
    );

    let flat = viewport(2560, Pointer::Fine);
    assert_eq!(
        flat.default_divider().fraction(),
        Divider::default().fraction()
    );
}

// ---- persistence --------------------------------------------------------

/// Build a set with something of everything in it, so the round trip below
/// has every field to lose.
fn furnished() -> (WindowSet, Vec<WindowId>) {
    let clock = TestClock::at(1_700_000_000);
    let mut set = WindowSet::new();
    let a = set.open(spec("files", 420), work(), &clock);
    clock.advance(5);
    let b = set.open(spec("terminal", 0), work(), &clock);
    clock.advance(5);
    let c = set.open(spec("editor", 1_300), work(), &clock);
    clock.advance(5);
    let d = set.open(spec("mail", 500), work(), &clock);

    set.snap(a, Snap::Left, work()).unwrap();
    set.place(b, Rect::new(640, 300, 900, 620), work()).unwrap();
    set.snap(c, Snap::TopRight, work()).unwrap();
    set.minimize(d).unwrap();
    set.set_popped_out(c, true).unwrap();
    set.set_title(b, "terminal — ~/src").unwrap();
    set.focus(b, Mode::Desktop).unwrap();
    set.split(c, a, Divider::new(0.37)).unwrap();
    set.reorder(d, 1).unwrap();
    set.set_mode_override(Some(Mode::Compact));
    (set, vec![a, b, c, d])
}

/// The store's copy and the server's copy must describe the same windows.
/// Anything the snapshot drops comes back as a window in the wrong place on
/// whichever device the operator next picks up.
#[test]
fn snapshot_then_restore_is_identity() {
    let (set, _) = furnished();
    let restored =
        WindowSet::restore(set.snapshot()).expect("a snapshot of a live set must restore");

    assert_eq!(restored, set, "the round trip through the store lost state");
    // Named explicitly as well, so a failure says which half broke.
    assert_eq!(restored.desktop(), set.desktop());
    assert_eq!(restored.compact(), set.compact());
    assert_eq!(restored.snapshot(), set.snapshot());
}

/// Both layouts have to cover every window after a restore, or the shell has
/// an open window one of the devices cannot show. Losing a window is worse
/// than placing it badly, so a missing layout row is filled in, not fatal.
#[test]
fn a_window_with_no_layout_rows_is_placed_rather_than_lost() {
    let (set, ids) = furnished();
    let mut snapshot = set.snapshot();
    let orphan = ids[0].as_u64();
    snapshot.desktop.retain(|row| row.window_id != orphan);
    snapshot.compact.retain(|row| row.window_id != orphan);

    let restored = WindowSet::restore(snapshot).expect("a gap must not be fatal");

    assert_eq!(restored.len(), ids.len());
    for &id in &ids {
        assert!(
            restored.desktop().placement(id).is_some(),
            "{id} has no desktop placement"
        );
        assert!(
            restored.compact().switcher().contains(&id),
            "{id} is missing from the switcher"
        );
    }
    let frame = restored.placement_of(ids[0]).unwrap().frame;
    assert!(
        frame.width > 0 && frame.height >= MIN_HEIGHT,
        "default frame is unusable"
    );
}

/// Every window appears exactly once in each layout. A duplicate would be
/// painted twice and would make the stack ambiguous.
#[test]
fn a_restored_layout_names_every_window_exactly_once() {
    let (set, ids) = furnished();
    let restored = WindowSet::restore(set.snapshot()).unwrap();

    let mut stack = restored.desktop().stacking().to_vec();
    stack.sort();
    let mut expected = ids.clone();
    expected.sort();
    assert_eq!(stack, expected, "the restored stack is not the window set");

    let mut switcher = restored.compact().switcher().to_vec();
    switcher.sort();
    assert_eq!(
        switcher, expected,
        "the restored switcher is not the window set"
    );
}

/// A layout row for a window that does not exist means the database is
/// inconsistent. Guessing would hide that, so it is refused.
#[test]
fn a_layout_row_for_a_missing_window_is_refused() {
    let (set, _) = furnished();
    let ghost = WindowId::from_u64(9_999);

    let mut snapshot = set.snapshot();
    let mut row = snapshot.desktop[0].clone();
    row.window_id = ghost.as_u64();
    snapshot.desktop.push(row);
    assert_eq!(
        WindowSet::restore(snapshot).unwrap_err(),
        RestoreError::OrphanLayout(ghost)
    );

    let mut snapshot = set.snapshot();
    snapshot.compact.push(deckjack_core::window::CompactRow {
        window_id: ghost.as_u64(),
        focus_rank: 7,
    });
    assert_eq!(
        WindowSet::restore(snapshot).unwrap_err(),
        RestoreError::OrphanLayout(ghost)
    );
}

/// A snap zone this build does not know was written by a newer Deckjack. The
/// layout is refused whole rather than reinterpreted: a window silently
/// treated as floating is a rearranged desktop, and the operator would never
/// know the downgrade did it.
#[test]
fn a_snapshot_from_a_newer_schema_is_refused_rather_than_half_read() {
    let (set, _) = furnished();

    let mut snapshot = set.snapshot();
    snapshot.desktop[0].snap = "quarter_left".to_string();
    match WindowSet::restore(snapshot) {
        Err(RestoreError::UnknownSnap(zone)) => assert_eq!(zone, "quarter_left"),
        other => panic!("expected a refusal, got {other:?}"),
    }

    let mut snapshot = set.snapshot();
    snapshot.shell.mode_override = Some("tv".to_string());
    match WindowSet::restore(snapshot) {
        Err(RestoreError::UnknownMode(mode)) => assert_eq!(mode, "tv"),
        other => panic!("expected a refusal, got {other:?}"),
    }

    // The known spellings do round trip, so the refusals above are about an
    // unknown value and not about the parser rejecting everything.
    for zone in [
        Snap::Floating,
        Snap::Maximized,
        Snap::Left,
        Snap::Right,
        Snap::Top,
        Snap::Bottom,
        Snap::TopLeft,
        Snap::TopRight,
        Snap::BottomLeft,
        Snap::BottomRight,
        Snap::Tiled,
    ] {
        assert_eq!(
            Snap::parse(zone.as_str()),
            Some(zone),
            "{zone:?} does not round trip"
        );
    }
    for mode in [Mode::Desktop, Mode::Compact] {
        assert_eq!(Mode::parse(mode.as_str()), Some(mode));
    }
    assert_eq!(Snap::parse("Maximize"), None);
    assert_eq!(Mode::parse(""), None);
}

/// A split is a convenience worth one tap, not data, so a stored split naming
/// a window that is gone is dropped rather than refused — unlike a layout row,
/// which is structure.
#[test]
fn a_stored_split_naming_a_closed_window_is_dropped_not_refused() {
    let (set, ids) = furnished();

    let mut snapshot = set.snapshot();
    snapshot.split.as_mut().unwrap().secondary = 9_999;
    let restored = WindowSet::restore(snapshot).expect("a stale split must not refuse a start");
    assert!(restored.compact().split().is_none());
    assert_eq!(
        restored.len(),
        ids.len(),
        "dropping the split cost a window"
    );

    // Nor may a degenerate split survive the trip.
    let mut snapshot = set.snapshot();
    let row = snapshot.split.as_mut().unwrap();
    row.secondary = row.primary;
    assert!(
        WindowSet::restore(snapshot)
            .unwrap()
            .compact()
            .split()
            .is_none()
    );
}

/// A half-written restore frame is no restore frame. The window keeps the
/// frame it has rather than jumping to a corner assembled from defaults.
#[test]
fn a_half_written_restore_frame_is_ignored_not_assembled() {
    let (set, ids) = furnished();
    let snapped = ids[0].as_u64();

    let mut snapshot = set.snapshot();
    let row = snapshot
        .desktop
        .iter_mut()
        .find(|row| row.window_id == snapped)
        .unwrap();
    assert!(
        row.restore_x.is_some(),
        "fixture must have a snapped window"
    );
    let frame_before = (row.x, row.y, row.width, row.height);
    row.restore_width = None;

    let restored = WindowSet::restore(snapshot).unwrap();
    let placement = restored.placement_of(ids[0]).unwrap();
    assert_eq!(placement.restore, None);
    assert_eq!(
        (
            placement.frame.x,
            placement.frame.y,
            placement.frame.width,
            placement.frame.height
        ),
        frame_before,
        "the window moved because its restore frame was incomplete"
    );
}

/// Focus pointing at a window that is gone is not worth refusing a start
/// over, but it must not be kept either: it lands on the topmost window that
/// is actually on the desktop.
#[test]
fn restored_focus_never_names_a_window_that_cannot_be_shown() {
    let (set, ids) = furnished();

    let mut snapshot = set.snapshot();
    snapshot.shell.desktop_focus = Some(9_999);
    let restored = WindowSet::restore(snapshot).expect("stale focus must not refuse a start");
    let focus = restored
        .desktop()
        .focus()
        .expect("something should have focus");
    assert!(restored.window(focus).is_some());
    assert!(
        !restored.placement_of(focus).unwrap().minimized,
        "focus landed on a minimized window"
    );

    // Same when the stored row simply has no focus.
    let mut snapshot = set.snapshot();
    snapshot.shell.desktop_focus = None;
    let restored = WindowSet::restore(snapshot).unwrap();
    assert_eq!(restored.desktop().focus(), topmost_visible(&set));
    let _ = ids;
}

/// The topmost window that is actually on the desktop, read off the public
/// stacking order: what focus is supposed to fall back to.
fn topmost_visible(set: &WindowSet) -> Option<WindowId> {
    set.desktop()
        .stacking()
        .iter()
        .rev()
        .copied()
        .find(|&id| !set.placement_of(id).unwrap().minimized)
}

/// An empty set round trips too: first start, and the state after closing
/// everything, are the same shape.
#[test]
fn an_empty_snapshot_restores_an_empty_set() {
    let restored = WindowSet::restore(Snapshot::default()).unwrap();

    assert!(restored.is_empty());
    assert_eq!(restored.desktop().focus(), None);
    assert!(restored.compact().switcher().is_empty());
    assert_eq!(restored.compact().front(), None);
    assert_eq!(restored.mode_policy().manual(), None);
    assert_eq!(restored, WindowSet::new());
}

/// Ids resume above the highest window that survived the restart, so a
/// restored set never hands out an id one of its own windows already has.
#[test]
fn a_restored_set_hands_out_ids_above_every_window_it_restored() {
    let (set, ids) = furnished();
    let mut restored = WindowSet::restore(set.snapshot()).unwrap();

    let fresh = open(&mut restored, "notes");
    assert!(
        ids.iter().all(|&old| old != fresh),
        "a restored set reused a live window's id"
    );
    assert!(fresh.as_u64() > ids.iter().map(|id| id.as_u64()).max().unwrap());
}

/// The snapshot is the store's view, and `z` is what orders the stack in SQL.
/// It has to come back in the same order even when the rows arrive shuffled,
/// since nothing promises a SELECT without an ORDER BY.
#[test]
fn a_shuffled_snapshot_restores_the_same_stack_and_switcher() {
    let (set, _) = furnished();
    let mut snapshot = set.snapshot();
    snapshot.desktop.reverse();
    snapshot.compact.reverse();
    snapshot.windows.reverse();

    let restored = WindowSet::restore(snapshot).unwrap();
    assert_eq!(restored.desktop().stacking(), set.desktop().stacking());
    assert_eq!(restored.compact().switcher(), set.compact().switcher());
}

/// The window id's text form is what a pop-out URL and a log line carry, so
/// it has to survive the trip through a string.
#[test]
fn a_window_id_round_trips_through_its_text_form() {
    let id = WindowId::from_u64(42);
    assert_eq!(id.to_string(), "42");
    assert_eq!("42".parse::<WindowId>().unwrap(), id);
    assert!("-1".parse::<WindowId>().is_err());
    assert!("".parse::<WindowId>().is_err());
    assert!("1e3".parse::<WindowId>().is_err());
}

/// The app owns its title; the shell only displays it. Nothing else about the
/// window may move when it changes.
#[test]
fn a_title_change_touches_nothing_but_the_title() {
    let mut set = WindowSet::new();
    let a = open(&mut set, "files");
    let _b = open(&mut set, "terminal");
    let desktop = set.desktop().clone();
    let compact = set.compact().clone();
    let window: Window = set.window(a).unwrap().clone();

    set.set_title(a, "files — /srv/media").unwrap();

    assert_eq!(set.window(a).unwrap().title, "files — /srv/media");
    assert_eq!(set.window(a).unwrap().opened_at, window.opened_at);
    assert_eq!(set.window(a).unwrap().app, window.app);
    assert_eq!(set.desktop(), &desktop, "a title change moved the desktop");
    assert_eq!(set.compact(), &compact, "a title change moved the switcher");
}

// ---- hostile geometry ---------------------------------------------------

/// The work area is reported by the client with the command, so it is input,
/// not something the server controls. An absurd one must come out as a bad
/// layout, not as a panicked server.
#[test]
fn an_absurd_work_area_does_not_panic_the_window_manager() {
    for w in [
        Rect::new(i32::MAX, i32::MAX, 1920, 1080),
        Rect::new(i32::MIN, i32::MIN, 1920, 1080),
        Rect::new(0, 0, 0, 0),
        Rect::new(-1, -1, u32::MAX, u32::MAX),
    ] {
        let mut set = WindowSet::new();
        let id = set.open(spec("files", 420), w, &TestClock::at(1));
        set.move_to(id, i32::MAX, i32::MIN, w).unwrap();
        set.resize(
            id,
            Size {
                width: u32::MAX,
                height: 1,
            },
            w,
        )
        .unwrap();
        set.snap(id, Snap::BottomRight, w).unwrap();
        set.unsnap(id, w).unwrap();
        set.tile(w);
        assert_eq!(set.len(), 1, "work area {w:?} lost the window");
    }
}

/// Focus names the window the desktop is showing, and the rest of the module
/// keeps that true: minimizing the focused window hands focus on, and
/// focusing a minimized one unminimizes it. A stored row must not be able to
/// contradict that, or the shell comes up focused on a window that is not on
/// the desktop and the operator's keystrokes have nowhere to go.
#[test]
fn restored_focus_is_never_a_minimized_window() {
    let (set, ids) = furnished();
    let minimized = ids[3];
    assert!(
        set.placement_of(minimized).unwrap().minimized,
        "fixture must have a minimized window"
    );

    let mut snapshot = set.snapshot();
    snapshot.shell.desktop_focus = Some(minimized.as_u64());
    let restored = WindowSet::restore(snapshot).unwrap();

    match restored.desktop().focus() {
        None => panic!("there were visible windows to focus"),
        Some(focus) => assert!(
            !restored.placement_of(focus).unwrap().minimized,
            "came up focused on a minimized window"
        ),
    }
}
