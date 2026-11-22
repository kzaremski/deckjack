//! The preview matches the export (lane brief exit check 4): mean CIEDE2000
//! under 1.5 at the same scale, measured, not eyeballed.
//!
//! The preview is modeled here exactly as the frame's shader computes it
//! (`ui/gl.js`): the 8-bit proxy sampled bilinearly through the geometry
//! map, the global LUT applied trilinearly, each local LUT blended in under
//! its mask. Spike 1 measured the real shader against this model on
//! SwiftShader and a Radeon 890M and found them within 0.02 mean ΔE2000 of
//! each other, so a model that passes here stands for the GPU.

mod common;

use base64::Engine as _;
use deckjack_photo::color::{self, Global};
use deckjack_photo::decode::{Format, Info, Source};
use deckjack_photo::engine::Engine;
use deckjack_photo::geometry;
use deckjack_photo::lut::Lut;
use deckjack_photo::recipe::Recipe;
use deckjack_photo::render;

use common::dng::dng_from;
use common::{Workdir, test_card, write_png};

/// CIEDE2000 between two sRGB colors in 0..1.
fn de2000(a: [f32; 3], b: [f32; 3]) -> f64 {
    de2000_lab(lab(a), lab(b))
}

/// sRGB (D65) to CIELAB.
fn lab(c: [f32; 3]) -> [f64; 3] {
    {
        let l = c.map(|v| f64::from(color::srgb_decode(v)));
        let x = (0.4124564 * l[0] + 0.3575761 * l[1] + 0.1804375 * l[2]) / 0.95047;
        let y = 0.2126729 * l[0] + 0.7151522 * l[1] + 0.0721750 * l[2];
        let z = (0.0193339 * l[0] + 0.1191920 * l[1] + 0.9503041 * l[2]) / 1.08883;
        let f = |t: f64| {
            if t > (6.0f64 / 29.0).powi(3) {
                t.cbrt()
            } else {
                t / (3.0 * (6.0f64 / 29.0).powi(2)) + 4.0 / 29.0
            }
        };
        [
            116.0 * f(y) - 16.0,
            500.0 * (f(x) - f(y)),
            200.0 * (f(y) - f(z)),
        ]
    }
}

fn de2000_lab([l1, a1, b1]: [f64; 3], [l2, a2, b2]: [f64; 3]) -> f64 {
    let c1 = a1.hypot(b1);
    let c2 = a2.hypot(b2);
    let cm = (c1 + c2) / 2.0;
    let g = 0.5 * (1.0 - (cm.powi(7) / (cm.powi(7) + 25f64.powi(7))).sqrt());
    let (a1p, a2p) = ((1.0 + g) * a1, (1.0 + g) * a2);
    let (c1p, c2p) = (a1p.hypot(b1), a2p.hypot(b2));
    let hue = |b: f64, a: f64| b.atan2(a).to_degrees().rem_euclid(360.0);
    let (h1, h2) = (hue(b1, a1p), hue(b2, a2p));
    let dl = l2 - l1;
    let dc = c2p - c1p;
    let mut dh = h2 - h1;
    if c1p * c2p == 0.0 {
        dh = 0.0;
    } else if dh > 180.0 {
        dh -= 360.0;
    } else if dh < -180.0 {
        dh += 360.0;
    }
    let dhh = 2.0 * (c1p * c2p).sqrt() * (dh / 2.0).to_radians().sin();
    let lm = (l1 + l2) / 2.0;
    let cpm = (c1p + c2p) / 2.0;
    let hm = if c1p * c2p == 0.0 {
        h1 + h2
    } else if (h1 - h2).abs() <= 180.0 {
        (h1 + h2) / 2.0
    } else if h1 + h2 < 360.0 {
        (h1 + h2 + 360.0) / 2.0
    } else {
        (h1 + h2 - 360.0) / 2.0
    };
    let t = 1.0 - 0.17 * (hm - 30.0).to_radians().cos()
        + 0.24 * (2.0 * hm).to_radians().cos()
        + 0.32 * (3.0 * hm + 6.0).to_radians().cos()
        - 0.20 * (4.0 * hm - 63.0).to_radians().cos();
    let dtheta = 30.0 * (-((hm - 275.0) / 25.0).powi(2)).exp();
    let rc = 2.0 * (cpm.powi(7) / (cpm.powi(7) + 25f64.powi(7))).sqrt();
    let sl = 1.0 + 0.015 * (lm - 50.0).powi(2) / (20.0 + (lm - 50.0).powi(2)).sqrt();
    let sc = 1.0 + 0.045 * cpm;
    let sh = 1.0 + 0.015 * cpm * t;
    let rt = -(2.0 * dtheta).to_radians().sin() * rc;
    ((dl / sl).powi(2) + (dc / sc).powi(2) + (dhh / sh).powi(2) + rt * (dc / sc) * (dhh / sh))
        .sqrt()
}

#[test]
fn the_ciede2000_here_matches_the_published_test_pairs() {
    // Sharma, Wu and Dalal (2005), pairs 1, 7 and 25. A metric that is
    // wrong here would pass or fail the parity test for the wrong reason.
    for (a, b, want) in [
        ([50.0, 2.6772, -79.7751], [50.0, 0.0, -82.7485], 2.0425),
        ([50.0, 0.0, 0.0], [50.0, -1.0, 2.0], 2.3669),
        (
            [60.2574, -34.0099, 36.2677],
            [60.4626, -34.1751, 39.4387],
            1.2644,
        ),
    ] {
        let got = de2000_lab(a, b);
        assert!((got - want).abs() < 1e-4, "{got} != {want}");
    }
    // And through sRGB: black against white is 100.
    assert!((de2000([0.0; 3], [1.0; 3]) - 100.0).abs() < 1e-3);
}

/// The frame's preview of `recipe`, modeled per pixel.
fn preview(
    proxy_png: &[u8],
    encoding_is_log: bool,
    recipe: &Recipe,
    size: usize,
    out: (u32, u32),
) -> Vec<[f32; 3]> {
    let codes = image::load_from_memory(proxy_png).unwrap().into_rgb8();
    let (pw, ph) = codes.dimensions();
    // The proxy as a texture: its codes as values, sampled bilinearly.
    let tex = Source {
        format: Format::Png,
        info: Info {
            width: pw,
            height: ph,
            ..Info::default()
        },
        pixels: codes
            .as_raw()
            .iter()
            .map(|c| f32::from(*c) / 255.0)
            .collect(),
    };
    let encoding = if encoding_is_log {
        color::Encoding::Log
    } else {
        color::Encoding::Srgb
    };
    let g = Global::new(recipe, encoding);
    let global = Lut::bake(size, |c| g.apply(c));
    let locals: Vec<(Lut, color::MaskOp)> = color::locals(recipe, pw as f32, ph as f32)
        .into_iter()
        .map(|(op, m)| (Lut::bake(size, |c| op.apply(c)), m))
        .collect();
    let map = geometry::output_to_source(&recipe.geometry, f64::from(pw), f64::from(ph));
    let mut px = Vec::new();
    for j in 0..out.1 {
        for i in 0..out.0 {
            let (u, v) = geometry::apply(
                &map,
                (f64::from(i) + 0.5) / f64::from(out.0),
                (f64::from(j) + 0.5) / f64::from(out.1),
            );
            if !(0.0..=1.0).contains(&u) || !(0.0..=1.0).contains(&v) {
                px.push([0.0; 3]);
                continue;
            }
            let code = tex.sample(u * f64::from(pw), v * f64::from(ph));
            let mut y = global.apply(code);
            for (l, m) in &locals {
                let k = m.at(u as f32, v as f32);
                if k > 0.0 {
                    y = color::blend(y, l.apply(y), k);
                }
            }
            // The canvas is 8 bits.
            px.push(y.map(|c| (c.clamp(0.0, 1.0) * 255.0).round() / 255.0));
        }
    }
    px
}

fn exit_check_recipe() -> Recipe {
    // Exit check 2: straighten 3°, crop 4:5, levels, a two-point S-curve,
    // and a radial mask brightening a face by +0.7 EV.
    serde_json::from_value(serde_json::json!({
        "white_balance": { "temperature": 20, "tint": -5 },
        "levels": { "master": { "black": 0.03, "white": 0.95, "gamma": 1.1, "out_black": 0, "out_white": 1 } },
        "curves": { "master": [[0, 0], [0.25, 0.2], [0.75, 0.82], [1, 1]] },
        "geometry": { "straighten_deg": 3.0, "crop": { "x": 0.2, "y": 0.05, "w": 0.5625, "h": 0.9 } },
        "masks": [{ "mask": { "kind": "radial", "cx": 0.45, "cy": 0.4, "rx": 0.12, "ry": 0.16, "feather": 0.6 },
                    "exposure_ev": 0.7 }]
    }))
    .unwrap()
}

fn score(w: &Workdir, name: &str, recipe: &Recipe, size: usize) -> (f64, f64) {
    let e = Engine::new(w.path().to_path_buf());
    let path = format!("Photos/{name}");
    let proxy = e.proxy(&path, Some(640)).unwrap();
    let png = base64::engine::general_purpose::STANDARD
        .decode(proxy["png"].as_str().unwrap())
        .unwrap();
    let log = proxy["encoding"] == "log";
    // The export at the preview's scale.
    let a = deckjack_photo::paths::resolve(w.path(), &path).unwrap();
    let src = e.source(&a).unwrap();
    let small = render::downscale(&src, 640);
    let direct = render::render(&small, recipe, None, &|| false, &|_| {}).unwrap();
    let shown = preview(&png, log, recipe, size, (direct.width, direct.height));
    let mut d: Vec<f64> = direct
        .pixels
        .as_chunks::<3>()
        .0
        .iter()
        .zip(&shown)
        .map(|(x, y)| de2000(*x, *y))
        .collect();
    d.sort_by(f64::total_cmp);
    let mean = d.iter().sum::<f64>() / d.len() as f64;
    (mean, d[(d.len() as f64 * 0.99) as usize])
}

#[test]
fn the_preview_matches_the_export_within_the_exit_check() {
    let w = Workdir::new();
    write_png(&w.photos().join("card.png"), &test_card(1200, 900));
    let r = exit_check_recipe();
    r.validate().unwrap();
    let (mean, p99) = score(&w, "card.png", &r, 33);
    assert!(mean < 1.5, "mean ΔE2000 {mean:.3} (p99 {p99:.3})");
    // The 17^3 LUT used while dragging is allowed to be worse, but not by
    // much on an ordinary edit.
    let (mean17, _) = score(&w, "card.png", &r, 17);
    assert!(mean17 < 1.5, "17^3 mean ΔE2000 {mean17:.3}");
    eprintln!("preview vs export: 33^3 mean {mean:.3} p99 {p99:.3}; 17^3 mean {mean17:.3}");
}

/// The test card as a Bayer RAW: each photosite takes its own channel of
/// the card in linear light, so the decoder demosaics it back.
fn card_dng(w: u32, h: u32) -> Vec<u8> {
    let card = test_card(w, h);
    dng_from(
        w,
        h,
        |x, y| {
            let c = match (x % 2, y % 2) {
                (0, 0) => 0,
                (1, 1) => 2,
                _ => 1,
            };
            let v = f32::from(card.get_pixel(x, y).0[c]) / 255.0;
            (color::srgb_decode(v) * 65535.0).round() as u16
        },
        1,
        32803,
    )
}

/// The RAW path is the exit check's main case and goes another way: an
/// 8-bit log proxy with dither, and LUTs indexed by log codes. Scored
/// separately so a regression there fails a test of its own.
#[test]
fn a_raw_preview_matches_its_export_within_the_exit_check() {
    let w = Workdir::new();
    std::fs::write(w.photos().join("card.dng"), card_dng(1200, 900)).unwrap();
    let r = exit_check_recipe();
    let e = Engine::new(w.path().to_path_buf());
    assert_eq!(
        e.proxy("Photos/card.dng", Some(640)).unwrap()["encoding"],
        "log"
    );
    let (mean, p99) = score(&w, "card.dng", &r, 33);
    let (mean17, p99_17) = score(&w, "card.dng", &r, 17);
    eprintln!(
        "RAW preview vs export: 33^3 mean {mean:.3} p99 {p99:.3}; 17^3 mean {mean17:.3} p99 {p99_17:.3}"
    );
    assert!(mean < 1.5, "mean ΔE2000 {mean:.3} (p99 {p99:.3})");
    assert!(mean17 < 1.5, "17^3 mean ΔE2000 {mean17:.3}");
    // Not the exit check's bar: this card measured p99 2.3 when written,
    // and real RAWs measure 4.4 to 4.6 (resampling at edges and texture,
    // see the lane brief). The bound is here so that log codes and LUT
    // indices drifting apart, or dither grown past a code, fail.
    assert!(p99 < 3.5, "p99 ΔE2000 {p99:.3}");
}
