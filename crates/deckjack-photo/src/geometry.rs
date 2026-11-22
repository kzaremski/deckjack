//! Orientation, straightening and crop, as one affine map.
//!
//! Every geometric step in a recipe is affine, so the whole of it composes
//! into one 2x3 matrix from an output position (normalized, 0 to 1 across
//! the cropped result) to a source position (normalized, 0 to 1 across the
//! photo as decoded). The export samples the source through it and the
//! frame's shader does the same with the same six numbers, which is why the
//! preview needs no round trip to show a crop.
//!
//! The frame computes this map itself while a crop is being dragged
//! (`ui/geometry.js`); the two are held to the same numbers by their tests.

use crate::recipe::Geometry;

/// `[a, b, c, d, e, f]`: `u = a*x + b*y + c`, `v = d*x + e*y + f`.
pub type Affine = [f64; 6];

fn compose(outer: &Affine, inner: &Affine) -> Affine {
    // outer(inner(p))
    let [a, b, c, d, e, f] = *outer;
    let [g, h, i, j, k, l] = *inner;
    [
        a * g + b * j,
        a * h + b * k,
        a * i + b * l + c,
        d * g + e * j,
        d * h + e * k,
        d * i + e * l + f,
    ]
}

/// The turned image's size, for a source of `width` x `height`.
pub fn turned_size(g: &Geometry, width: f64, height: f64) -> (f64, f64) {
    if g.quarter_turns % 2 == 1 {
        (height, width)
    } else {
        (width, height)
    }
}

/// The cropped result's size in pixels, at the source's full resolution.
pub fn output_size(g: &Geometry, width: u32, height: u32) -> (u32, u32) {
    let (tw, th) = turned_size(g, f64::from(width), f64::from(height));
    let w = (f64::from(g.crop.w) * tw).round().max(1.0);
    let h = (f64::from(g.crop.h) * th).round().max(1.0);
    (w as u32, h as u32)
}

/// Output uv to source uv, for a source of `width` x `height` (any unit;
/// only the aspect ratio matters).
pub fn output_to_source(g: &Geometry, width: f64, height: f64) -> Affine {
    let (tw, th) = turned_size(g, width, height);
    let c = g.crop;
    // 1. Output uv to straightened pixels.
    let crop: Affine = [
        f64::from(c.w) * tw,
        0.0,
        f64::from(c.x) * tw,
        0.0,
        f64::from(c.h) * th,
        f64::from(c.y) * th,
    ];
    // 2. Undo the straighten: rotate by -angle about the center. With y
    //    down, a positive angle turns the picture clockwise, so the inverse
    //    is [[cos, sin], [-sin, cos]].
    let a = f64::from(g.straighten_deg).to_radians();
    let (s, co) = a.sin_cos();
    let (cx, cy) = (tw / 2.0, th / 2.0);
    let unrotate: Affine = [co, s, cx - co * cx - s * cy, -s, co, cy + s * cx - co * cy];
    // 3. Turned pixels to turned uv.
    let to_uv: Affine = [1.0 / tw, 0.0, 0.0, 0.0, 1.0 / th, 0.0];
    // 4. Undo the flips, which happen after the turn.
    let unflip: Affine = [
        if g.flip_h { -1.0 } else { 1.0 },
        0.0,
        if g.flip_h { 1.0 } else { 0.0 },
        0.0,
        if g.flip_v { -1.0 } else { 1.0 },
        if g.flip_v { 1.0 } else { 0.0 },
    ];
    // 5. Undo the clockwise quarter turns, in uv: one clockwise turn takes
    //    source (s, t) to (1 - t, s), so its inverse is s = b, t = 1 - a.
    let unturn: Affine = match g.quarter_turns % 4 {
        1 => [0.0, 1.0, 0.0, -1.0, 0.0, 1.0],
        2 => [-1.0, 0.0, 1.0, 0.0, -1.0, 1.0],
        3 => [0.0, -1.0, 1.0, 1.0, 0.0, 0.0],
        _ => [1.0, 0.0, 0.0, 0.0, 1.0, 0.0],
    };
    let m = compose(&unrotate, &crop);
    let m = compose(&to_uv, &m);
    let m = compose(&unflip, &m);
    compose(&unturn, &m)
}

pub fn apply(m: &Affine, x: f64, y: f64) -> (f64, f64) {
    (m[0] * x + m[1] * y + m[2], m[3] * x + m[4] * y + m[5])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::recipe::Rect;

    fn close(a: (f64, f64), b: (f64, f64)) -> bool {
        (a.0 - b.0).abs() < 1e-9 && (a.1 - b.1).abs() < 1e-9
    }

    #[test]
    fn the_identity_geometry_is_the_identity_map() {
        let m = output_to_source(&Geometry::default(), 4000.0, 3000.0);
        for p in [(0.0, 0.0), (1.0, 1.0), (0.3, 0.7)] {
            assert!(close(apply(&m, p.0, p.1), p));
        }
        assert_eq!(output_size(&Geometry::default(), 4000, 3000), (4000, 3000));
    }

    #[test]
    fn a_crop_selects_its_rectangle() {
        let g = Geometry {
            crop: Rect {
                x: 0.25,
                y: 0.5,
                w: 0.5,
                h: 0.25,
            },
            ..Geometry::default()
        };
        let m = output_to_source(&g, 4000.0, 3000.0);
        assert!(close(apply(&m, 0.0, 0.0), (0.25, 0.5)));
        assert!(close(apply(&m, 1.0, 1.0), (0.75, 0.75)));
        assert_eq!(output_size(&g, 4000, 3000), (2000, 750));
    }

    #[test]
    fn a_clockwise_turn_puts_the_left_edge_on_top() {
        let g = Geometry {
            quarter_turns: 1,
            ..Geometry::default()
        };
        let m = output_to_source(&g, 4000.0, 3000.0);
        // Turned clockwise, the output's top-left came from the source's
        // bottom-left, and the output's top-right from the source's top-left.
        assert!(close(apply(&m, 0.0, 0.0), (0.0, 1.0)));
        assert!(close(apply(&m, 1.0, 0.0), (0.0, 0.0)));
        assert!(close(apply(&m, 1.0, 1.0), (1.0, 0.0)));
        assert_eq!(output_size(&g, 4000, 3000), (3000, 4000));
        let half = output_to_source(
            &Geometry {
                quarter_turns: 2,
                ..Geometry::default()
            },
            4000.0,
            3000.0,
        );
        assert!(close(apply(&half, 0.0, 0.0), (1.0, 1.0)));
        let ccw = output_to_source(
            &Geometry {
                quarter_turns: 3,
                ..Geometry::default()
            },
            4000.0,
            3000.0,
        );
        // Counter-clockwise: the top-left came from the top-right.
        assert!(close(apply(&ccw, 0.0, 0.0), (1.0, 0.0)));
        assert!(close(apply(&ccw, 0.0, 1.0), (0.0, 0.0)));
    }

    #[test]
    fn flips_mirror_after_the_turn() {
        let g = Geometry {
            flip_h: true,
            ..Geometry::default()
        };
        let m = output_to_source(&g, 100.0, 100.0);
        assert!(close(apply(&m, 0.0, 0.25), (1.0, 0.25)));
        let g = Geometry {
            flip_v: true,
            quarter_turns: 1,
            ..Geometry::default()
        };
        let m = output_to_source(&g, 100.0, 100.0);
        // Turned clockwise then flipped top to bottom: the output's
        // top-left is the turned image's bottom-left, which came from the
        // source's bottom-right.
        assert!(close(apply(&m, 0.0, 0.0), (1.0, 1.0)));
    }

    #[test]
    fn straightening_turns_the_picture_clockwise_about_the_center() {
        let g = Geometry {
            straighten_deg: 90.0f32.min(45.0),
            ..Geometry::default()
        };
        // The center never moves.
        let m = output_to_source(&g, 200.0, 100.0);
        assert!(close(apply(&m, 0.5, 0.5), (0.5, 0.5)));
        // Turned clockwise by 45 degrees, the output point straight right of
        // the center came from up and to the right of it in the source
        // (y down), at the same pixel distance.
        let (u, v) = apply(&m, 0.75, 0.5); // 50 px right of center
        let (dx, dy) = ((u - 0.5) * 200.0, (v - 0.5) * 100.0);
        let r = 50.0 / 2f64.sqrt();
        assert!((dx - r).abs() < 1e-9 && (dy + r).abs() < 1e-9, "{dx} {dy}");
    }
}
