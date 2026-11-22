//! The color pipeline: every per-pixel operation, as plain functions.
//!
//! This module is the only definition of what a recipe does to a color. The
//! export evaluates it directly, pixel by pixel. The live preview evaluates
//! it too, once per lattice point of a 3D LUT that the frame then applies on
//! the GPU (`lut.rs`), so the preview and the export cannot disagree about
//! the math; they differ only by the LUT's interpolation, which spike 1
//! measured.
//!
//! The order, and the space each step works in, is the format spec's
//! (`docs/formats/photo-edit-v1.md`, "Pipeline"):
//!
//! 1. decode the input encoding to linear light (sRGB primaries);
//! 2. white balance and exposure, as per-channel gains in linear light;
//! 3. clip to 0..1 and encode with the sRGB transfer function ("display");
//! 4. levels, master then the channel's own;
//! 5. tone curves, master then the channel's own;
//! 6. saturation, around Rec.709 luma of the display values;
//! 7. each local adjustment: `y = y + (D(y) - y) * mask`, in display values.

use crate::recipe::{Level, Local, Mask, Recipe};

/// How an input color is encoded. The export works on linear values; the
/// proxy the frame holds is 8-bit, and for those the encoding matters.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Encoding {
    /// Already linear light. What the export renders from.
    Linear,
    /// The sRGB transfer function. A JPEG's or PNG's own values.
    Srgb,
    /// `log2`, for a RAW proxy, whose highlights go past 1.0 and whose
    /// shadows would band in 8 bits under sRGB's curve. See [`log_encode`].
    Log,
}

/// The darkest linear value the log encoding resolves, as a power of two.
pub const LOG_MIN_STOPS: f32 = -12.0;
/// Stops the log encoding covers. 2^-12 to 2^4: twelve below white and four
/// of highlight headroom above it.
pub const LOG_RANGE_STOPS: f32 = 16.0;

pub fn srgb_decode(v: f32) -> f32 {
    if v <= 0.04045 {
        v / 12.92
    } else {
        ((v + 0.055) / 1.055).powf(2.4)
    }
}

/// Clips to 0..1 first: display values are a closed range.
pub fn srgb_encode(v: f32) -> f32 {
    let v = v.clamp(0.0, 1.0);
    if v <= 0.003_130_8 {
        12.92 * v
    } else {
        1.055 * v.powf(1.0 / 2.4) - 0.055
    }
}

/// Linear light to a 0..1 log code. Zero maps to exactly 0.
pub fn log_encode(v: f32) -> f32 {
    let floor = LOG_MIN_STOPS.exp2();
    (((v.max(0.0) + floor).log2() - LOG_MIN_STOPS) / LOG_RANGE_STOPS).clamp(0.0, 1.0)
}

pub fn log_decode(p: f32) -> f32 {
    (p * LOG_RANGE_STOPS + LOG_MIN_STOPS).exp2() - LOG_MIN_STOPS.exp2()
}

impl Encoding {
    pub fn decode(self, v: f32) -> f32 {
        match self {
            Encoding::Linear => v,
            Encoding::Srgb => srgb_decode(v),
            Encoding::Log => log_decode(v),
        }
    }

    pub fn encode(self, v: f32) -> f32 {
        match self {
            Encoding::Linear => v,
            Encoding::Srgb => srgb_encode(v),
            Encoding::Log => log_encode(v),
        }
    }
}

/// The white balance and exposure gains, per channel, in linear light.
///
/// Temperature trades red against blue and tint takes from green, each at
/// half a stop at full scale. The three gains are then normalized so a
/// neutral gray keeps its luminance, which keeps white balance from also
/// being an exposure control.
pub fn gains(recipe: &Recipe) -> [f32; 3] {
    let t = recipe.white_balance.temperature / 100.0 * 0.5;
    let g = -recipe.white_balance.tint / 100.0 * 0.5;
    let raw = [t.exp2(), g.exp2(), (-t).exp2()];
    let luma = 0.2126 * raw[0] + 0.7152 * raw[1] + 0.0722 * raw[2];
    let exposure = recipe.exposure_ev.exp2();
    raw.map(|c| c / luma * exposure)
}

/// One levels control, compiled.
#[derive(Debug, Clone, Copy)]
pub struct LevelOp {
    black: f32,
    span: f32,
    inv_gamma: f32,
    out_black: f32,
    out_span: f32,
    identity: bool,
}

impl LevelOp {
    pub fn new(l: &Level) -> LevelOp {
        LevelOp {
            black: l.black,
            span: (l.white - l.black).max(1e-4),
            inv_gamma: 1.0 / l.gamma,
            out_black: l.out_black,
            out_span: l.out_white - l.out_black,
            identity: *l == Level::default(),
        }
    }

    /// `out_black + (out_white - out_black) * clamp((x - black) / (white - black))^(1/gamma)`
    pub fn apply(&self, x: f32) -> f32 {
        if self.identity {
            return x;
        }
        let t = ((x - self.black) / self.span).clamp(0.0, 1.0);
        self.out_black + self.out_span * t.powf(self.inv_gamma)
    }
}

/// A monotone cubic curve (Fritsch-Carlson), flat outside its end points.
#[derive(Debug, Clone, Default)]
pub struct Curve {
    xs: Vec<f32>,
    ys: Vec<f32>,
    ms: Vec<f32>,
}

impl Curve {
    /// From points already validated (strictly increasing inputs). Fewer
    /// than two points is the identity.
    pub fn new(points: &[[f32; 2]]) -> Curve {
        if points.len() < 2 {
            return Curve::default();
        }
        let xs: Vec<f32> = points.iter().map(|p| p[0]).collect();
        let ys: Vec<f32> = points.iter().map(|p| p[1]).collect();
        let n = xs.len();
        let d: Vec<f32> = (0..n - 1)
            .map(|k| (ys[k + 1] - ys[k]) / (xs[k + 1] - xs[k]))
            .collect();
        let mut ms = vec![0.0; n];
        ms[0] = d[0];
        ms[n - 1] = d[n - 2];
        for k in 1..n - 1 {
            ms[k] = if d[k - 1] * d[k] <= 0.0 {
                0.0
            } else {
                (d[k - 1] + d[k]) / 2.0
            };
        }
        for k in 0..n - 1 {
            if d[k] == 0.0 {
                ms[k] = 0.0;
                ms[k + 1] = 0.0;
                continue;
            }
            let a = ms[k] / d[k];
            let b = ms[k + 1] / d[k];
            let s = a * a + b * b;
            if s > 9.0 {
                let t = 3.0 / s.sqrt();
                ms[k] = t * a * d[k];
                ms[k + 1] = t * b * d[k];
            }
        }
        Curve { xs, ys, ms }
    }

    pub fn is_identity(&self) -> bool {
        self.xs.is_empty()
    }

    pub fn apply(&self, x: f32) -> f32 {
        let n = self.xs.len();
        if n == 0 {
            return x;
        }
        if x <= self.xs[0] {
            return self.ys[0];
        }
        if x >= self.xs[n - 1] {
            return self.ys[n - 1];
        }
        // Few points, so a linear scan beats a binary search's branches.
        let k = (0..n - 1).find(|&k| x < self.xs[k + 1]).unwrap_or(n - 2);
        let h = self.xs[k + 1] - self.xs[k];
        let t = (x - self.xs[k]) / h;
        let (t2, t3) = (t * t, t * t * t);
        let h00 = 2.0 * t3 - 3.0 * t2 + 1.0;
        let h10 = t3 - 2.0 * t2 + t;
        let h01 = -2.0 * t3 + 3.0 * t2;
        let h11 = t3 - t2;
        h00 * self.ys[k] + h10 * h * self.ms[k] + h01 * self.ys[k + 1] + h11 * h * self.ms[k + 1]
    }
}

/// Rec.709 luma weights, applied to display values for saturation.
const LUMA: [f32; 3] = [0.2126, 0.7152, 0.0722];

fn saturate(rgb: [f32; 3], amount: f32) -> [f32; 3] {
    if amount == 0.0 {
        return rgb;
    }
    let y = LUMA[0] * rgb[0] + LUMA[1] * rgb[1] + LUMA[2] * rgb[2];
    rgb.map(|c| (y + (c - y) * (1.0 + amount)).clamp(0.0, 1.0))
}

/// Everything global in a recipe, compiled for one input encoding.
#[derive(Debug, Clone)]
pub struct Global {
    input: Encoding,
    gains: [f32; 3],
    levels_master: LevelOp,
    levels: [LevelOp; 3],
    curve_master: Curve,
    curves: [Curve; 3],
    saturation: f32,
}

impl Global {
    pub fn new(recipe: &Recipe, input: Encoding) -> Global {
        Global {
            input,
            gains: gains(recipe),
            levels_master: LevelOp::new(&recipe.levels.master),
            levels: [
                LevelOp::new(&recipe.levels.red),
                LevelOp::new(&recipe.levels.green),
                LevelOp::new(&recipe.levels.blue),
            ],
            curve_master: Curve::new(&recipe.curves.master),
            curves: [
                Curve::new(&recipe.curves.red),
                Curve::new(&recipe.curves.green),
                Curve::new(&recipe.curves.blue),
            ],
            saturation: recipe.saturation,
        }
    }

    /// An input color, in this pipeline's input encoding, to display sRGB.
    pub fn apply(&self, input: [f32; 3]) -> [f32; 3] {
        let mut out = [0.0; 3];
        for c in 0..3 {
            let linear = self.input.decode(input[c]) * self.gains[c];
            let mut v = srgb_encode(linear);
            v = self.levels_master.apply(v);
            v = self.levels[c].apply(v);
            v = self.curve_master.apply(v);
            v = self.curves[c].apply(v);
            out[c] = v.clamp(0.0, 1.0);
        }
        saturate(out, self.saturation)
    }
}

/// What one local adjustment does to a display color, before its mask.
#[derive(Debug, Clone, Copy)]
pub struct LocalOp {
    gain: f32,
    saturation: f32,
}

impl LocalOp {
    pub fn new(local: &Local) -> LocalOp {
        LocalOp {
            gain: local.exposure_ev.exp2(),
            saturation: local.saturation,
        }
    }

    /// `D(y)`: display to display.
    pub fn apply(&self, rgb: [f32; 3]) -> [f32; 3] {
        let out = rgb.map(|c| srgb_encode(srgb_decode(c) * self.gain));
        saturate(out, self.saturation)
    }
}

/// `y + (d - y) * m`, the display-space blend every local adjustment uses.
pub fn blend(y: [f32; 3], d: [f32; 3], m: f32) -> [f32; 3] {
    [
        y[0] + (d[0] - y[0]) * m,
        y[1] + (d[1] - y[1]) * m,
        y[2] + (d[2] - y[2]) * m,
    ]
}

/// A mask, compiled for a photo of a given shape.
///
/// The same formula is in the frame's shader (`ui/gl.js`, `maskAt`); the two
/// are held together by `tests/parity.rs` and the UI test.
#[derive(Debug, Clone, Copy)]
pub struct MaskOp {
    cx: f32,
    cy: f32,
    /// Normalized x and y to long-edge units.
    sx: f32,
    sy: f32,
    cos: f32,
    sin: f32,
    rx: f32,
    ry: f32,
    inner: f32,
    invert: bool,
}

/// The softest edge a mask has, so `feather = 0` is a crisp edge rather than
/// a division by zero.
pub const MIN_FEATHER: f32 = 0.01;

impl MaskOp {
    /// `width` and `height` of the photo as decoded, in any unit.
    pub fn new(local: &Local, width: f32, height: f32) -> MaskOp {
        let long = width.max(height);
        let Mask::Radial {
            cx,
            cy,
            rx,
            ry,
            angle_deg,
            feather,
        } = local.mask;
        let a = angle_deg.to_radians();
        MaskOp {
            cx,
            cy,
            sx: width / long,
            sy: height / long,
            cos: a.cos(),
            sin: a.sin(),
            rx,
            ry,
            inner: 1.0 - feather.max(MIN_FEATHER),
            invert: local.invert,
        }
    }

    /// Mask strength at a normalized source position, 0 to 1.
    pub fn at(&self, u: f32, v: f32) -> f32 {
        let dx = (u - self.cx) * self.sx;
        let dy = (v - self.cy) * self.sy;
        // Into the ellipse's own frame: rotate by -angle.
        let x = dx * self.cos + dy * self.sin;
        let y = -dx * self.sin + dy * self.cos;
        let d = ((x / self.rx).powi(2) + (y / self.ry).powi(2)).sqrt();
        let t = ((d - self.inner) / (1.0 - self.inner)).clamp(0.0, 1.0);
        let m = 1.0 - t * t * (3.0 - 2.0 * t);
        if self.invert { 1.0 - m } else { m }
    }
}

/// The local adjustments of a recipe, compiled.
pub fn locals(recipe: &Recipe, width: f32, height: f32) -> Vec<(LocalOp, MaskOp)> {
    recipe
        .masks
        .iter()
        .map(|l| (LocalOp::new(l), MaskOp::new(l, width, height)))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::recipe::{Curves, Levels, WhiteBalance};

    fn close(a: f32, b: f32, tol: f32) -> bool {
        (a - b).abs() <= tol
    }

    #[test]
    fn srgb_round_trips_and_matches_known_values() {
        // 0.5 encoded is 0.7354 (IEC 61966-2-1); 0.214 decodes from 0.5.
        assert!(close(srgb_encode(0.5), 0.735_357, 1e-5));
        assert!(close(srgb_decode(0.5), 0.214_041, 1e-5));
        for i in 0..=100 {
            let v = i as f32 / 100.0;
            assert!(close(srgb_decode(srgb_encode(v)), v, 1e-5), "{v}");
        }
        // The linear toe, both ways.
        assert!(close(srgb_encode(0.002), 0.002 * 12.92, 1e-7));
        assert!(close(srgb_decode(0.03), 0.03 / 12.92, 1e-7));
        assert!(close(srgb_encode(2.0), 1.0, 1e-6));
        assert_eq!(srgb_encode(-1.0), 0.0);
    }

    #[test]
    fn the_log_encoding_spans_sixteen_stops_with_zero_at_zero() {
        assert_eq!(log_encode(0.0), 0.0);
        assert!(close(log_decode(0.0), 0.0, 1e-9));
        // White (1.0) sits 12 stops up the 16: 0.75.
        assert!(close(log_encode(1.0), 0.75, 1e-4));
        assert!(close(log_decode(1.0), 16.0 - 2f32.powi(-12), 1e-3));
        for v in [0.001_f32, 0.01, 0.18, 1.0, 4.0, 12.0] {
            assert!(close(log_decode(log_encode(v)) / v, 1.0, 1e-4), "{v}");
        }
    }

    #[test]
    fn white_balance_keeps_gray_luminance_and_exposure_doubles() {
        let mut r = Recipe {
            white_balance: WhiteBalance {
                temperature: 100.0,
                tint: -40.0,
            },
            ..Recipe::default()
        };
        let g = gains(&r);
        // Warmer: red up, blue down.
        assert!(g[0] > 1.0 && g[2] < 1.0, "{g:?}");
        // Half a stop either way, before normalization.
        assert!(close(g[0] / g[2], 2.0, 1e-5), "{g:?}");
        let luma = 0.2126 * g[0] + 0.7152 * g[1] + 0.0722 * g[2];
        assert!(close(luma, 1.0, 1e-5));
        r.exposure_ev = 1.0;
        let g2 = gains(&r);
        for c in 0..3 {
            assert!(close(g2[c], g[c] * 2.0, 1e-5));
        }
    }

    #[test]
    fn levels_map_black_and_white_points_and_bend_midtones_by_gamma() {
        let op = LevelOp::new(&Level {
            black: 0.1,
            white: 0.9,
            gamma: 2.0,
            out_black: 0.05,
            out_white: 0.95,
        });
        assert!(close(op.apply(0.1), 0.05, 1e-6));
        assert!(close(op.apply(0.0), 0.05, 1e-6));
        assert!(close(op.apply(0.9), 0.95, 1e-6));
        assert!(close(op.apply(1.0), 0.95, 1e-6));
        // Midpoint: t = 0.5, 0.5^(1/2) = 0.7071, scaled into 0.05..0.95.
        assert!(close(op.apply(0.5), 0.05 + 0.9 * 0.5_f32.sqrt(), 1e-5));
    }

    #[test]
    fn a_curve_passes_through_its_points_and_never_overshoots() {
        let pts = [[0.0, 0.0], [0.25, 0.15], [0.75, 0.85], [1.0, 1.0]];
        let c = Curve::new(&pts);
        for p in pts {
            assert!(close(c.apply(p[0]), p[1], 1e-6), "{p:?}");
        }
        // Monotone: an S-curve through increasing points never decreases.
        let mut last = -1.0;
        for i in 0..=1000 {
            let y = c.apply(i as f32 / 1000.0);
            assert!(y >= last - 1e-6, "dip at {i}");
            last = y;
        }
        // Flat steps stay flat between equal outputs: the case a plain
        // cubic spline overshoots.
        let flat = Curve::new(&[[0.0, 0.0], [0.4, 0.5], [0.6, 0.5], [1.0, 1.0]]);
        for i in 40..=60 {
            assert!(close(flat.apply(i as f32 / 100.0), 0.5, 1e-6), "{i}");
        }
        // Known value: the S-curve's midpoint by symmetry.
        assert!(close(c.apply(0.5), 0.5, 1e-6));
        // Outside the points it holds the end values.
        let short = Curve::new(&[[0.2, 0.1], [0.8, 0.9]]);
        assert_eq!(short.apply(0.0), 0.1);
        assert_eq!(short.apply(1.0), 0.9);
        assert!(Curve::new(&[]).is_identity());
        // A steep rise into a shoulder: the averaged tangent at the knee is
        // 5, and only the Fritsch-Carlson limit keeps the next segment from
        // bulging above its end point.
        let knee = Curve::new(&[[0.0, 0.0], [0.1, 0.9], [0.2, 1.0], [1.0, 1.0]]);
        for i in 100..=200 {
            let y = knee.apply(i as f32 / 1000.0);
            assert!(y <= 1.0 + 1e-6, "{y} at {i}");
        }
    }

    #[test]
    fn the_identity_recipe_returns_its_input() {
        let g = Global::new(&Recipe::default(), Encoding::Srgb);
        for v in [0.0, 0.1, 0.5, 0.73, 1.0] {
            let out = g.apply([v, v * 0.5, 1.0 - v]);
            assert!(close(out[0], v, 1e-5));
            assert!(close(out[1], v * 0.5, 1e-5));
            assert!(close(out[2], 1.0 - v, 1e-5));
        }
    }

    #[test]
    fn the_global_pipeline_runs_in_the_documented_order() {
        // +1 EV on linear 0.2 is 0.4 linear, which displays as 0.6654; then
        // the master curve maps through its points, then saturation 0 is a
        // no-op. Exposure applied after the curve would give another number.
        let r = Recipe {
            exposure_ev: 1.0,
            curves: Curves {
                master: vec![[0.0, 0.0], [0.6654, 0.5], [1.0, 1.0]],
                ..Curves::default()
            },
            ..Recipe::default()
        };
        let g = Global::new(&r, Encoding::Linear);
        let out = g.apply([0.2, 0.2, 0.2]);
        assert!(close(out[0], 0.5, 1e-3), "{out:?}");
        // Levels come before curves. With a black point at 0.4 the two
        // orders give different answers (about 0.33 against 0.17), so the
        // expected value is levels first, then the curve.
        let level = Level {
            black: 0.4,
            ..Level::default()
        };
        let r2 = Recipe {
            levels: Levels {
                master: level,
                ..Levels::default()
            },
            ..r.clone()
        };
        let out2 = Global::new(&r2, Encoding::Linear).apply([0.2, 0.2, 0.2]);
        let curve = Curve::new(&r.curves.master);
        let levels_first = curve.apply(LevelOp::new(&level).apply(srgb_encode(0.4)));
        let curves_first = LevelOp::new(&level).apply(curve.apply(srgb_encode(0.4)));
        assert!((levels_first - curves_first).abs() > 0.1);
        assert!(
            close(out2[0], levels_first, 1e-5),
            "{out2:?} {levels_first}"
        );
    }

    #[test]
    fn saturation_moves_chroma_around_luma() {
        let gray = saturate([0.4, 0.4, 0.4], 1.0);
        assert!(gray.iter().all(|&c| close(c, 0.4, 1e-6)));
        let mono = saturate([0.8, 0.2, 0.1], -1.0);
        assert!(close(mono[0], mono[1], 1e-6) && close(mono[1], mono[2], 1e-6));
        let more = saturate([0.6, 0.4, 0.4], 0.5);
        assert!(more[0] > 0.6 && more[1] < 0.4);
    }

    fn radial(feather: f32, invert: bool) -> Local {
        Local {
            mask: Mask::Radial {
                cx: 0.5,
                cy: 0.5,
                rx: 0.25,
                ry: 0.125,
                angle_deg: 0.0,
                feather,
            },
            invert,
            exposure_ev: 1.0,
            saturation: 0.0,
        }
    }

    #[test]
    fn a_radial_mask_is_full_inside_empty_outside_and_soft_between() {
        // A 2:1 photo: radii are in long-edge units, so rx = 0.25 is a
        // quarter of the width and ry = 0.125 is a quarter of the height.
        let m = MaskOp::new(&radial(0.5, false), 2000.0, 1000.0);
        assert_eq!(m.at(0.5, 0.5), 1.0);
        // On the x axis: d = |u - 0.5| / 0.25. Inner edge at d = 0.5.
        assert_eq!(m.at(0.5 + 0.12, 0.5), 1.0);
        assert_eq!(m.at(0.5 + 0.26, 0.5), 0.0);
        // Halfway through the feather (d = 0.75): smoothstep(0.5) = 0.5.
        assert!(close(m.at(0.5 + 0.1875, 0.5), 0.5, 1e-5));
        // In y, the same distance in long-edge units is twice as far in
        // normalized height.
        assert!(close(m.at(0.5, 0.5 + 0.1875 * 2.0 * 0.5), 0.5, 1e-5));
        // And on a portrait photo the same holds across: half the width is
        // a quarter of the long edge.
        let tall = MaskOp::new(&radial(0.5, false), 1000.0, 2000.0);
        assert!(close(tall.at(0.5 + 0.375, 0.5), 0.5, 1e-5));
        let inv = MaskOp::new(&radial(0.5, true), 2000.0, 1000.0);
        assert_eq!(inv.at(0.5, 0.5), 0.0);
        assert_eq!(inv.at(0.0, 0.0), 1.0);
    }

    #[test]
    fn a_rotated_mask_turns_clockwise() {
        // Turned 45 degrees clockwise (y down), the long radius points down
        // and to the right, not up and to the right. 90 degrees could not
        // tell the two directions apart: an ellipse is symmetric under 180.
        let mut l = radial(0.5, false);
        let Mask::Radial { angle_deg, .. } = &mut l.mask;
        *angle_deg = 45.0;
        let m = MaskOp::new(&l, 1000.0, 1000.0);
        assert!(m.at(0.5 + 0.15, 0.5 + 0.15) > 0.0);
        assert_eq!(m.at(0.5 + 0.15, 0.5 - 0.15), 0.0);
    }

    #[test]
    fn a_hard_edged_mask_still_has_a_finite_edge() {
        let m = MaskOp::new(&radial(0.0, false), 1000.0, 1000.0);
        assert_eq!(m.at(0.5 + 0.24, 0.5), 1.0);
        assert_eq!(m.at(0.5 + 0.2501, 0.5), 0.0);
        assert!(m.at(0.5 + 0.249, 0.5).is_finite());
    }

    #[test]
    fn a_local_brightens_in_linear_light_and_blends_by_the_mask() {
        let d = LocalOp::new(&radial(0.5, false)).apply([0.5, 0.5, 0.5]);
        assert!(close(d[0], srgb_encode(srgb_decode(0.5) * 2.0), 1e-6));
        let y = blend([0.5; 3], d, 0.25);
        assert!(close(y[0], 0.5 + (d[0] - 0.5) * 0.25, 1e-6));
        assert_eq!(blend([0.5; 3], d, 0.0), [0.5; 3]);
    }
}
