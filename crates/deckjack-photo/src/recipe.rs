//! The edit recipe: what the operator did to a photo, as named physical
//! values.
//!
//! This is the one structure the sidecar stores, the LUT bake reads, the
//! export renders and an agent writes. Every field has a default that means
//! "no change", so `{}` is a valid recipe and the identity edit, and a recipe
//! written by a later version with a field this one does not know is refused
//! rather than half-applied (`deny_unknown_fields`). The math behind each
//! field is in `docs/formats/photo-edit-v1.md`.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// At most this many local adjustments. Each one is a texture lookup and a
/// mask in the frame's shader, and the brief sets the v1 budget at four.
pub const MAX_MASKS: usize = 4;

/// At most this many points in one tone curve, endpoints included.
pub const MAX_CURVE_POINTS: usize = 16;

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct Recipe {
    /// Exposure in stops (EV), applied in linear light. -5 to +5.
    pub exposure_ev: f32,
    pub white_balance: WhiteBalance,
    pub levels: Levels,
    pub curves: Curves,
    /// -1 (gray) to +1 (twice the chroma). 0 is no change.
    pub saturation: f32,
    pub geometry: Geometry,
    /// Local adjustments, applied in order after everything global.
    pub masks: Vec<Local>,
}

/// A relative white-balance shift on top of what the file already has (the
/// camera's as-shot balance, for a RAW). Positive temperature is warmer,
/// positive tint is more magenta.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct WhiteBalance {
    /// -100 to +100. At 100 red gains half a stop and blue loses half.
    pub temperature: f32,
    /// -100 to +100. At 100 green loses half a stop.
    pub tint: f32,
}

/// Input and output levels, for all channels and then for each.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct Levels {
    pub master: Level,
    pub red: Level,
    pub green: Level,
    pub blue: Level,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct Level {
    /// Input black point, 0 to 1 in display (sRGB-encoded) values.
    pub black: f32,
    /// Input white point, above `black`.
    pub white: f32,
    /// Midtone gamma, 0.1 to 10. Above 1 brightens.
    pub gamma: f32,
    /// Output range.
    pub out_black: f32,
    pub out_white: f32,
}

impl Default for Level {
    fn default() -> Level {
        Level {
            black: 0.0,
            white: 1.0,
            gamma: 1.0,
            out_black: 0.0,
            out_white: 1.0,
        }
    }
}

/// Tone curves as control points `[input, output]` in display values. An
/// empty list is the identity. Interpolation is monotone cubic
/// (Fritsch-Carlson), so a curve never overshoots between its points.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct Curves {
    pub master: Vec<[f32; 2]>,
    pub red: Vec<[f32; 2]>,
    pub green: Vec<[f32; 2]>,
    pub blue: Vec<[f32; 2]>,
}

/// Orientation, straightening and crop. Applied in that order, all of it on
/// top of the file's own EXIF orientation, which is applied on decode.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct Geometry {
    /// Clockwise quarter turns, 0 to 3.
    pub quarter_turns: u8,
    pub flip_h: bool,
    pub flip_v: bool,
    /// Degrees, -45 to +45. Positive turns the picture clockwise.
    pub straighten_deg: f32,
    /// The kept rectangle, normalized to the turned image: x, y, width,
    /// height, each 0 to 1.
    pub crop: Rect,
}

impl Default for Geometry {
    fn default() -> Geometry {
        Geometry {
            quarter_turns: 0,
            flip_h: false,
            flip_v: false,
            straighten_deg: 0.0,
            crop: Rect::FULL,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl Rect {
    pub const FULL: Rect = Rect {
        x: 0.0,
        y: 0.0,
        w: 1.0,
        h: 1.0,
    };
}

/// One local adjustment: what changes, and where.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Local {
    pub mask: Mask,
    /// Inside instead of outside, or the other way round.
    #[serde(default)]
    pub invert: bool,
    /// Stops, -5 to +5, applied to the display value in linear light.
    #[serde(default)]
    pub exposure_ev: f32,
    /// -1 to +1, as the global one.
    #[serde(default)]
    pub saturation: f32,
}

/// Where a local adjustment applies. Coordinates are normalized to the
/// photo as decoded (after EXIF orientation, before any recipe geometry), so
/// cropping or straightening never moves a mask off what it was put on.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Mask {
    /// An ellipse. Full strength inside `1 - feather` of its radius, none
    /// outside it, smoothstep between.
    Radial {
        /// Center, 0 to 1 across and down.
        cx: f32,
        cy: f32,
        /// Radii as fractions of the photo's long edge, 0.001 to 2.
        rx: f32,
        ry: f32,
        /// Degrees, clockwise.
        #[serde(default)]
        angle_deg: f32,
        /// 0 (hard) to 1 (soft all the way to the center).
        feather: f32,
    },
}

/// Why a recipe was refused. One message per problem, naming the field, so
/// an agent writing recipes can correct itself.
#[derive(Debug, thiserror::Error, PartialEq)]
#[error("{field}: {problem}")]
pub struct Invalid {
    pub field: String,
    pub problem: String,
}

fn range(field: &str, value: f32, lo: f32, hi: f32) -> Result<(), Invalid> {
    if value.is_finite() && value >= lo && value <= hi {
        Ok(())
    } else {
        Err(Invalid {
            field: field.to_string(),
            problem: format!("{value} is outside {lo} to {hi}"),
        })
    }
}

impl Recipe {
    /// Check every value against its documented range.
    pub fn validate(&self) -> Result<(), Invalid> {
        range("exposure_ev", self.exposure_ev, -5.0, 5.0)?;
        range(
            "white_balance.temperature",
            self.white_balance.temperature,
            -100.0,
            100.0,
        )?;
        range("white_balance.tint", self.white_balance.tint, -100.0, 100.0)?;
        range("saturation", self.saturation, -1.0, 1.0)?;
        for (name, level) in [
            ("master", &self.levels.master),
            ("red", &self.levels.red),
            ("green", &self.levels.green),
            ("blue", &self.levels.blue),
        ] {
            level.validate(&format!("levels.{name}"))?;
        }
        for (name, curve) in [
            ("master", &self.curves.master),
            ("red", &self.curves.red),
            ("green", &self.curves.green),
            ("blue", &self.curves.blue),
        ] {
            validate_curve(&format!("curves.{name}"), curve)?;
        }
        self.geometry.validate()?;
        if self.masks.len() > MAX_MASKS {
            return Err(Invalid {
                field: "masks".to_string(),
                problem: format!("{} masks, at most {MAX_MASKS}", self.masks.len()),
            });
        }
        for (i, local) in self.masks.iter().enumerate() {
            local.validate(&format!("masks[{i}]"))?;
        }
        Ok(())
    }
}

impl Level {
    fn validate(&self, field: &str) -> Result<(), Invalid> {
        range(&format!("{field}.black"), self.black, 0.0, 1.0)?;
        range(&format!("{field}.white"), self.white, 0.0, 1.0)?;
        range(&format!("{field}.gamma"), self.gamma, 0.1, 10.0)?;
        range(&format!("{field}.out_black"), self.out_black, 0.0, 1.0)?;
        range(&format!("{field}.out_white"), self.out_white, 0.0, 1.0)?;
        // A white point at or below the black point divides by zero or
        // inverts the image; neither is what a levels control means.
        if self.white - self.black < 0.004 {
            return Err(Invalid {
                field: format!("{field}.white"),
                problem: "must be above black".to_string(),
            });
        }
        Ok(())
    }
}

fn validate_curve(field: &str, points: &[[f32; 2]]) -> Result<(), Invalid> {
    if points.is_empty() {
        return Ok(());
    }
    if points.len() < 2 || points.len() > MAX_CURVE_POINTS {
        return Err(Invalid {
            field: field.to_string(),
            problem: format!(
                "{} points, a curve has 2 to {MAX_CURVE_POINTS}",
                points.len()
            ),
        });
    }
    for (i, [x, y]) in points.iter().enumerate() {
        range(&format!("{field}[{i}][0]"), *x, 0.0, 1.0)?;
        range(&format!("{field}[{i}][1]"), *y, 0.0, 1.0)?;
    }
    for i in 1..points.len() {
        if points[i][0] <= points[i - 1][0] {
            return Err(Invalid {
                field: format!("{field}[{i}][0]"),
                problem: "inputs must strictly increase".to_string(),
            });
        }
    }
    Ok(())
}

impl Geometry {
    fn validate(&self) -> Result<(), Invalid> {
        if self.quarter_turns > 3 {
            return Err(Invalid {
                field: "geometry.quarter_turns".to_string(),
                problem: format!("{} is outside 0 to 3", self.quarter_turns),
            });
        }
        range("geometry.straighten_deg", self.straighten_deg, -45.0, 45.0)?;
        let c = self.crop;
        range("geometry.crop.x", c.x, 0.0, 1.0)?;
        range("geometry.crop.y", c.y, 0.0, 1.0)?;
        range("geometry.crop.w", c.w, 0.01, 1.0)?;
        range("geometry.crop.h", c.h, 0.01, 1.0)?;
        // A small tolerance, because a crop dragged to the edge in the
        // browser arrives as 0.30000001 + 0.70000005.
        if c.x + c.w > 1.0001 || c.y + c.h > 1.0001 {
            return Err(Invalid {
                field: "geometry.crop".to_string(),
                problem: "reaches past the edge of the photo".to_string(),
            });
        }
        Ok(())
    }
}

impl Local {
    fn validate(&self, field: &str) -> Result<(), Invalid> {
        range(&format!("{field}.exposure_ev"), self.exposure_ev, -5.0, 5.0)?;
        range(&format!("{field}.saturation"), self.saturation, -1.0, 1.0)?;
        match self.mask {
            Mask::Radial {
                cx,
                cy,
                rx,
                ry,
                angle_deg,
                feather,
            } => {
                range(&format!("{field}.mask.cx"), cx, -1.0, 2.0)?;
                range(&format!("{field}.mask.cy"), cy, -1.0, 2.0)?;
                range(&format!("{field}.mask.rx"), rx, 0.001, 2.0)?;
                range(&format!("{field}.mask.ry"), ry, 0.001, 2.0)?;
                range(&format!("{field}.mask.angle_deg"), angle_deg, -360.0, 360.0)?;
                range(&format!("{field}.mask.feather"), feather, 0.0, 1.0)?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_object_is_the_identity_recipe() {
        // The sidecar of a photo nobody has touched, and the reset button,
        // are both `{}`. If a default were not "no change", opening a photo
        // would edit it.
        let r: Recipe = serde_json::from_str("{}").unwrap();
        assert_eq!(r, Recipe::default());
        assert_eq!(r.geometry.crop, Rect::FULL);
        assert_eq!(r.levels.master.white, 1.0);
        assert_eq!(r.levels.master.gamma, 1.0);
        r.validate().unwrap();
    }

    #[test]
    fn an_unknown_field_is_refused_not_ignored() {
        // A recipe from a later version, or an agent's typo ("exposure"),
        // silently dropped would render something other than what was asked.
        let e = serde_json::from_str::<Recipe>(r#"{"exposure": 1}"#).unwrap_err();
        assert!(e.to_string().contains("exposure"), "{e}");
    }

    #[test]
    fn each_out_of_range_value_is_named() {
        let mut r = Recipe {
            exposure_ev: 6.0,
            ..Recipe::default()
        };
        assert_eq!(r.validate().unwrap_err().field, "exposure_ev");
        r.exposure_ev = f32::NAN;
        assert_eq!(r.validate().unwrap_err().field, "exposure_ev");
        r.exposure_ev = 0.0;
        r.levels.red.white = 0.0;
        assert_eq!(r.validate().unwrap_err().field, "levels.red.white");
        r.levels.red.white = 1.0;
        r.curves.blue = vec![[0.0, 0.0], [0.5, 0.6], [0.5, 1.0]];
        assert_eq!(r.validate().unwrap_err().field, "curves.blue[2][0]");
        r.curves.blue = vec![[0.0, 0.0]];
        assert_eq!(r.validate().unwrap_err().field, "curves.blue");
        r.curves.blue.clear();
        r.geometry.crop = Rect {
            x: 0.5,
            y: 0.0,
            w: 0.6,
            h: 1.0,
        };
        assert_eq!(r.validate().unwrap_err().field, "geometry.crop");
        r.geometry.crop = Rect::FULL;
        r.geometry.quarter_turns = 4;
        assert_eq!(r.validate().unwrap_err().field, "geometry.quarter_turns");
        r.geometry.quarter_turns = 3;
        let radial = Local {
            mask: Mask::Radial {
                cx: 0.5,
                cy: 0.5,
                rx: 0.2,
                ry: 0.2,
                angle_deg: 0.0,
                feather: 0.5,
            },
            invert: false,
            exposure_ev: 0.7,
            saturation: 0.0,
        };
        r.masks = vec![radial; 5];
        assert_eq!(r.validate().unwrap_err().field, "masks");
        r.masks = vec![radial; 4];
        r.validate().unwrap();
        r.masks[3].mask = Mask::Radial {
            cx: 0.5,
            cy: 0.5,
            rx: 0.0,
            ry: 0.2,
            angle_deg: 0.0,
            feather: 0.5,
        };
        assert_eq!(r.validate().unwrap_err().field, "masks[3].mask.rx");
    }

    #[test]
    fn a_mask_is_tagged_by_kind_in_json() {
        // The format spec documents `"kind": "radial"`; the brush and the
        // linear gradient will be further kinds under the same tag.
        let local: Local = serde_json::from_str(
            r#"{"mask":{"kind":"radial","cx":0.5,"cy":0.4,"rx":0.1,"ry":0.2,"feather":0.3},"exposure_ev":0.7}"#,
        )
        .unwrap();
        assert!(matches!(local.mask, Mask::Radial { cy, .. } if cy == 0.4));
        assert!(!local.invert);
    }
}
