//! Input from the browser, made safe and native.
//!
//! Everything here is a pure function, so the rules are tested without a
//! display: which DOM buttons become which evdev buttons, which key codes
//! are accepted, how a wheel notch is scaled, and how far a size or a point
//! may go.

use deckjack_protocol::stream::{OUTPUT_HEIGHT, OUTPUT_WIDTH};

/// evdev button codes (`linux/input-event-codes.h`).
pub const BTN_LEFT: u32 = 0x110;
pub const BTN_RIGHT: u32 = 0x111;
pub const BTN_MIDDLE: u32 = 0x112;
pub const BTN_SIDE: u32 = 0x113;
pub const BTN_EXTRA: u32 = 0x114;

/// The highest evdev key code (`KEY_MAX`).
pub const KEY_MAX: u32 = 0x2ff;

/// xkb key codes are evdev codes plus 8, an offset from X11's history.
pub const XKB_OFFSET: u32 = 8;

/// The smallest window an app is asked for.
pub const MIN_SIZE: u32 = 32;

/// A DOM `MouseEvent.button` as an evdev button. Anything else is dropped.
pub fn evdev_button(dom: u8) -> Option<u32> {
    match dom {
        0 => Some(BTN_LEFT),
        1 => Some(BTN_MIDDLE),
        2 => Some(BTN_RIGHT),
        3 => Some(BTN_SIDE),
        4 => Some(BTN_EXTRA),
        _ => None,
    }
}

/// An evdev key code from the browser as an xkb key code, or `None` for a
/// code no keyboard has (zero, or past `KEY_MAX`): the browser maps
/// `KeyboardEvent.code`, and a code outside the table is a bug or a
/// forgery, not a key.
pub fn xkb_keycode(evdev: u32) -> Option<u32> {
    (1..=KEY_MAX).contains(&evdev).then_some(evdev + XKB_OFFSET)
}

/// One browser wheel delta (pixels, positive down or right) as a Wayland
/// axis value and a v120 step count.
///
/// Browsers report a notch of a mouse wheel as about 100 pixels; Wayland
/// clients expect a notch to be 120 v120 units and, by convention, 15 of
/// axis value (XWayland turns every 10 of axis value into one X11 wheel
/// click, so passing the browser's 100 through would scroll ten lines a
/// notch). A trackpad's small deltas come through as fractions of a notch.
pub fn wheel(delta_px: f64) -> (f64, i32) {
    let notches = (delta_px / 100.0).clamp(-10.0, 10.0);
    (notches * 15.0, (notches * 120.0).round() as i32)
}

/// A requested window size, within the one output and above the minimum.
pub fn clamp_size(width: u32, height: u32) -> (u32, u32) {
    (
        width.clamp(MIN_SIZE, OUTPUT_WIDTH),
        height.clamp(MIN_SIZE, OUTPUT_HEIGHT),
    )
}

/// The smallest picture libopenh264 encodes in either direction: below 16
/// pixels it refuses every picture (measured, docs/lanes/8-stream.md).
pub const MIN_ENCODED: u32 = 16;

/// What of a window is shown: its geometry, at least one pixel and never
/// more than the output. A client may set any window geometry it likes; a
/// window bigger than the one output it was told about is not one the
/// compositor allocates a canvas for.
pub fn visible_size(width: i32, height: i32) -> (u32, u32) {
    (
        width.clamp(1, OUTPUT_WIDTH as i32) as u32,
        height.clamp(1, OUTPUT_HEIGHT as i32) as u32,
    )
}

/// The size a window's picture is drawn and encoded at, for a visible
/// size: rounded up to even (H.264 4:2:0) and at least [`MIN_ENCODED`]. The
/// extra is black padding past the visible size, which the browser does not
/// draw: the picture's header carries the visible size.
pub fn encoded_size(visible: (u32, u32)) -> (u32, u32) {
    let even = |n: u32| n.saturating_add(n & 1);
    (
        even(visible.0).max(MIN_ENCODED),
        even(visible.1).max(MIN_ENCODED),
    )
}

/// The ids, `root` first, of a window and everything placed against it
/// (its popups, their popups), from a snapshot of who each window's parent
/// is. Taken before any of them is removed: once a parent is gone, a
/// popup's parent can no longer be looked up.
pub fn with_descendants(root: u32, parents: &[(u32, Option<u32>)]) -> Vec<u32> {
    let mut out = vec![root];
    let mut i = 0;
    while i < out.len() {
        let id = out[i];
        for &(child, parent) in parents {
            if parent == Some(id) && !out.contains(&child) {
                out.push(child);
            }
        }
        i += 1;
    }
    out
}

/// A pointer position inside a window of `width` x `height`. The browser
/// computes it from a scaled picture, so rounding can put it a fraction
/// outside; a non-finite value (`NaN` from a zero-sized panel) becomes 0.
pub fn clamp_point(x: f64, y: f64, width: i32, height: i32) -> (f64, f64) {
    let fix = |v: f64, max: i32| {
        if v.is_finite() {
            v.clamp(0.0, (max.max(1) - 1) as f64)
        } else {
            0.0
        }
    };
    (fix(x, width), fix(y, height))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_window_shows_at_most_the_output_and_at_least_a_pixel() {
        assert_eq!(visible_size(300, 252), (300, 252));
        assert_eq!(visible_size(20_000, 20_000), (OUTPUT_WIDTH, OUTPUT_HEIGHT));
        assert_eq!(visible_size(0, -5), (1, 1));
        assert_eq!(visible_size(1920, 1081), (1920, 1080));
    }

    /// libopenh264 refuses anything under 16 pixels either way (and, past
    /// the output, the visible size already stops the picture growing).
    #[test]
    fn a_picture_is_encoded_even_and_never_under_sixteen_pixels() {
        assert_eq!(encoded_size((1, 1)), (16, 16));
        assert_eq!(encoded_size((200, 12)), (200, 16));
        assert_eq!(encoded_size((14, 1080)), (16, 1080));
        assert_eq!(encoded_size((16, 17)), (16, 18));
        assert_eq!(encoded_size((299, 251)), (300, 252));
        assert_eq!(encoded_size((1920, 1080)), (1920, 1080));
    }

    #[test]
    fn a_window_goes_with_its_popups_and_theirs_and_nothing_else() {
        // 1 is a toplevel with a menu 2, whose submenu is 3; 4 is another
        // toplevel with its own menu 5.
        let parents = [
            (1, None),
            (2, Some(1)),
            (3, Some(2)),
            (4, None),
            (5, Some(4)),
        ];
        assert_eq!(with_descendants(1, &parents), vec![1, 2, 3]);
        assert_eq!(with_descendants(2, &parents), vec![2, 3]);
        assert_eq!(with_descendants(4, &parents), vec![4, 5]);
        assert_eq!(with_descendants(3, &parents), vec![3]);
        // A cycle (an X11 window transient for its own child) ends.
        assert_eq!(
            with_descendants(7, &[(7, Some(8)), (8, Some(7))]),
            vec![7, 8]
        );
    }

    #[test]
    fn dom_buttons_map_to_evdev_and_unknown_ones_are_dropped() {
        assert_eq!(evdev_button(0), Some(BTN_LEFT));
        assert_eq!(evdev_button(1), Some(BTN_MIDDLE));
        assert_eq!(evdev_button(2), Some(BTN_RIGHT));
        assert_eq!(evdev_button(3), Some(BTN_SIDE));
        assert_eq!(evdev_button(4), Some(BTN_EXTRA));
        assert_eq!(evdev_button(5), None);
    }

    #[test]
    fn key_codes_are_offset_for_xkb_and_bounded() {
        // KEY_A is 30 in evdev, 38 in xkb.
        assert_eq!(xkb_keycode(30), Some(38));
        assert_eq!(xkb_keycode(KEY_MAX), Some(KEY_MAX + 8));
        assert_eq!(xkb_keycode(0), None);
        assert_eq!(xkb_keycode(KEY_MAX + 1), None);
    }

    #[test]
    fn a_wheel_notch_is_one_v120_step_and_a_huge_delta_is_capped() {
        assert_eq!(wheel(100.0), (15.0, 120));
        assert_eq!(wheel(-50.0), (-7.5, -60));
        assert_eq!(wheel(1e9), (150.0, 1200));
    }

    #[test]
    fn sizes_stay_on_the_output_and_points_inside_the_window() {
        assert_eq!(clamp_size(0, 0), (MIN_SIZE, MIN_SIZE));
        assert_eq!(clamp_size(5000, 5000), (OUTPUT_WIDTH, OUTPUT_HEIGHT));
        assert_eq!(clamp_size(800, 600), (800, 600));
        assert_eq!(clamp_point(-3.0, 900.0, 800, 600), (0.0, 599.0));
        assert_eq!(clamp_point(f64::NAN, f64::INFINITY, 800, 600), (0.0, 0.0));
        assert_eq!(clamp_point(10.5, 20.25, 800, 600), (10.5, 20.25));
    }
}
