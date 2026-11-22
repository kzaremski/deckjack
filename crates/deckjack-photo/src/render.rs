//! From a decoded photo to pixels: the proxy the frame previews from, and
//! the full pipeline the export (and `photo.render`) evaluates directly.

use rayon::prelude::*;

use crate::color::{self, Encoding, Global};
use crate::decode::Source;
use crate::geometry;
use crate::recipe::Recipe;

/// Longest proxy edge. A 2048 px PNG is 2-6 MB, and as base64 inside one
/// JSON frame it must stay under the socket's 8 MiB frame limit.
pub const MAX_PROXY_EDGE: u32 = 2048;

/// Box-average `src` down so its long edge is at most `long_edge`. Done in
/// linear light, which is where averaging is physically right. A source
/// already small enough is copied.
pub fn downscale(src: &Source, long_edge: u32) -> Source {
    let (w, h) = (src.width(), src.height());
    let long = w.max(h);
    if long <= long_edge {
        return Source {
            format: src.format,
            info: src.info.clone(),
            pixels: src.pixels.clone(),
        };
    }
    let scale = f64::from(long) / f64::from(long_edge);
    let ow = ((f64::from(w) / scale).round() as u32).max(1);
    let oh = ((f64::from(h) / scale).round() as u32).max(1);
    let sx = f64::from(w) / f64::from(ow);
    let sy = f64::from(h) / f64::from(oh);
    let mut out = vec![0.0f32; (ow * oh * 3) as usize];
    out.par_chunks_mut((ow * 3) as usize)
        .enumerate()
        .for_each(|(j, row)| {
            let y0 = (j as f64 * sy).floor() as u32;
            let y1 = (((j + 1) as f64 * sy).floor() as u32).clamp(y0 + 1, h);
            for i in 0..ow as usize {
                let x0 = (i as f64 * sx).floor() as u32;
                let x1 = (((i + 1) as f64 * sx).floor() as u32).clamp(x0 + 1, w);
                let mut acc = [0.0f64; 3];
                for y in y0..y1 {
                    let base = ((y * w + x0) * 3) as usize;
                    for p in src.pixels[base..base + ((x1 - x0) * 3) as usize]
                        .as_chunks::<3>()
                        .0
                    {
                        acc[0] += f64::from(p[0]);
                        acc[1] += f64::from(p[1]);
                        acc[2] += f64::from(p[2]);
                    }
                }
                let n = f64::from((y1 - y0) * (x1 - x0));
                row[i * 3] = (acc[0] / n) as f32;
                row[i * 3 + 1] = (acc[1] / n) as f32;
                row[i * 3 + 2] = (acc[2] / n) as f32;
            }
        });
    let mut info = src.info.clone();
    info.width = ow;
    info.height = oh;
    Source {
        format: src.format,
        info,
        pixels: out,
    }
}

/// A small, deterministic hash for dither noise: the same proxy every time,
/// so a cached proxy and a fresh one are byte-identical.
fn hash(x: u32, y: u32, c: u32) -> f32 {
    let mut h = x
        .wrapping_mul(0x8da6_b343)
        .wrapping_add(y.wrapping_mul(0xd816_3841))
        .wrapping_add(c.wrapping_mul(0xcb1a_b31f));
    h ^= h >> 15;
    h = h.wrapping_mul(0x2c1b_3c6d);
    h ^= h >> 12;
    h = h.wrapping_mul(0x297a_2d39);
    h ^= h >> 15;
    (h >> 8) as f32 / (1u32 << 24) as f32
}

/// Quantize linear pixels to 8-bit codes in `encoding`. With `dither`,
/// triangular noise of one code peak to peak is added first, which trades
/// visible steps in a smooth gradient for fine grain (spike 2).
pub fn quantize(src: &Source, encoding: Encoding, dither: bool) -> Vec<u8> {
    let w = src.width();
    src.pixels
        .par_iter()
        .enumerate()
        .map(|(i, &v)| {
            let code = encoding.encode(v) * 255.0;
            let noise = if dither {
                let p = (i / 3) as u32;
                let (x, y, c) = (p % w, p / w, (i % 3) as u32);
                hash(x, y, c) + hash(x, y, c + 3) - 1.0
            } else {
                0.0
            };
            (code + noise).round().clamp(0.0, 255.0) as u8
        })
        .collect()
}

/// The proxy encoding for a photo: sRGB for display formats, whose 8-bit
/// values are what they already were, and log for RAW (spike 2).
pub fn proxy_encoding(src: &Source) -> Encoding {
    match src.format {
        crate::decode::Format::Raw => Encoding::Log,
        _ => Encoding::Srgb,
    }
}

/// The proxy: downscaled, quantized and PNG-encoded, unadjusted.
pub struct Proxy {
    pub width: u32,
    pub height: u32,
    pub encoding: Encoding,
    pub png: Vec<u8>,
}

pub fn proxy(src: &Source, long_edge: u32) -> Result<Proxy, image::ImageError> {
    let small = downscale(src, long_edge.clamp(16, MAX_PROXY_EDGE));
    let encoding = proxy_encoding(src);
    let codes = quantize(&small, encoding, encoding == Encoding::Log);
    let mut png = Vec::new();
    image::codecs::png::PngEncoder::new_with_quality(
        &mut png,
        image::codecs::png::CompressionType::Fast,
        image::codecs::png::FilterType::Adaptive,
    )
    .write_image(
        &codes,
        small.width(),
        small.height(),
        image::ExtendedColorType::Rgb8,
    )?;
    Ok(Proxy {
        width: small.width(),
        height: small.height(),
        encoding,
        png,
    })
}

use image::ImageEncoder;

/// A rendered result: display (sRGB-encoded) values 0..1, RGB, row-major.
pub struct Rendered {
    pub width: u32,
    pub height: u32,
    pub pixels: Vec<f32>,
}

/// Why a render stopped early.
#[derive(Debug, PartialEq, Eq)]
pub struct Cancelled;

/// Rows rendered between checks for cancellation and progress reports.
const BAND: usize = 64;

/// The full pipeline, evaluated directly: geometry, then every global and
/// local operation per pixel. `long_edge` limits the result's long edge;
/// the source is box-downscaled first when the result is at least half its
/// size, so a small export is not an aliased one.
pub fn render(
    src: &Source,
    recipe: &Recipe,
    long_edge: Option<u32>,
    cancelled: &(dyn Fn() -> bool + Sync),
    progress: &dyn Fn(f32),
) -> Result<Rendered, Cancelled> {
    let (fw, fh) = geometry::output_size(&recipe.geometry, src.width(), src.height());
    let (ow, oh) = match long_edge {
        Some(l) if l < fw.max(fh) => {
            let s = f64::from(l) / f64::from(fw.max(fh));
            (
                ((f64::from(fw) * s).round() as u32).max(1),
                ((f64::from(fh) * s).round() as u32).max(1),
            )
        }
        _ => (fw, fh),
    };
    // Pre-shrink so each output pixel samples about one source pixel.
    let ratio = f64::from(fw.max(fh)) / f64::from(ow.max(oh));
    let shrunk;
    let src = if ratio >= 2.0 {
        let long = (f64::from(src.width().max(src.height())) / ratio).round() as u32;
        shrunk = downscale(src, long.max(1));
        &shrunk
    } else {
        src
    };
    let (sw, sh) = (f64::from(src.width()), f64::from(src.height()));
    let map = geometry::output_to_source(&recipe.geometry, sw, sh);
    let global = Global::new(recipe, Encoding::Linear);
    let locals = color::locals(recipe, src.width() as f32, src.height() as f32);
    let row_len = (ow * 3) as usize;
    let mut pixels = vec![0.0f32; row_len * oh as usize];
    let bands = pixels.chunks_mut(row_len * BAND).enumerate();
    let total = oh as f32;
    for (b, band) in bands {
        if cancelled() {
            return Err(Cancelled);
        }
        band.par_chunks_mut(row_len)
            .enumerate()
            .for_each(|(r, row)| {
                let j = b * BAND + r;
                let v = (j as f64 + 0.5) / f64::from(oh);
                for i in 0..ow as usize {
                    let u = (i as f64 + 0.5) / f64::from(ow);
                    let (su, sv) = geometry::apply(&map, u, v);
                    let lin = src.sample(su * sw, sv * sh);
                    let mut y = global.apply(lin);
                    for (op, mask) in &locals {
                        let m = mask.at(su as f32, sv as f32);
                        if m > 0.0 {
                            y = color::blend(y, op.apply(y), m);
                        }
                    }
                    row[i * 3..i * 3 + 3].copy_from_slice(&y);
                }
            });
        progress(((b + 1) * BAND) as f32 / total);
    }
    Ok(Rendered {
        width: ow,
        height: oh,
        pixels,
    })
}

impl Rendered {
    pub fn to_rgb8(&self) -> Vec<u8> {
        self.pixels
            .iter()
            .map(|v| (v.clamp(0.0, 1.0) * 255.0).round() as u8)
            .collect()
    }

    pub fn to_rgb16(&self) -> Vec<u16> {
        self.pixels
            .iter()
            .map(|v| (v.clamp(0.0, 1.0) * 65535.0).round() as u16)
            .collect()
    }

    /// A JPEG of this render, for `photo.render` and the export.
    pub fn jpeg(&self, quality: u8) -> Result<Vec<u8>, image::ImageError> {
        let mut out = Vec::new();
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, quality.clamp(1, 100))
            .write_image(
                &self.to_rgb8(),
                self.width,
                self.height,
                image::ExtendedColorType::Rgb8,
            )?;
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decode::{Format, Info};
    use crate::recipe::{Geometry, Local, Mask, Rect};

    pub(crate) fn flat(w: u32, h: u32, f: impl Fn(u32, u32) -> [f32; 3]) -> Source {
        let mut pixels = Vec::with_capacity((w * h * 3) as usize);
        for y in 0..h {
            for x in 0..w {
                pixels.extend(f(x, y));
            }
        }
        Source {
            format: Format::Png,
            info: Info {
                width: w,
                height: h,
                ..Info::default()
            },
            pixels,
        }
    }

    fn never() -> bool {
        false
    }

    #[test]
    fn downscaling_averages_in_linear_light() {
        // A 1-pixel checker of 0 and 1 averages to 0.5 linear, not 0.5 in
        // sRGB code values.
        let src = flat(4, 2, |x, y| [((x + y) % 2) as f32; 3]);
        let small = downscale(&src, 2);
        assert_eq!((small.width(), small.height()), (2, 1));
        assert!(small.pixels.iter().all(|&v| (v - 0.5).abs() < 1e-6));
    }

    #[test]
    fn the_identity_render_of_a_photo_is_the_photo_encoded() {
        let src = flat(8, 4, |x, _| [x as f32 / 8.0, 0.25, 0.0]);
        let out = render(&src, &Recipe::default(), None, &never, &|_| {}).unwrap();
        assert_eq!((out.width, out.height), (8, 4));
        for x in 0..8 {
            let v = out.pixels[(8 + x) * 3];
            assert!((v - color::srgb_encode(x as f32 / 8.0)).abs() < 1e-5, "{x}");
        }
    }

    #[test]
    fn a_crop_and_a_turn_change_the_output_shape() {
        let src = flat(40, 20, |x, _| [if x < 20 { 0.0 } else { 1.0 }; 3]);
        let r = Recipe {
            geometry: Geometry {
                quarter_turns: 1,
                crop: Rect {
                    x: 0.0,
                    y: 0.0,
                    w: 1.0,
                    h: 0.5,
                },
                ..Geometry::default()
            },
            ..Recipe::default()
        };
        let out = render(&src, &r, None, &never, &|_| {}).unwrap();
        // Turned, 20 x 40; the top half kept: 20 x 20. A clockwise turn
        // puts the source's left (dark) half on top.
        assert_eq!((out.width, out.height), (20, 20));
        assert!(out.pixels.iter().all(|&v| v < 0.01));
    }

    #[test]
    fn a_long_edge_limit_scales_the_result() {
        let src = flat(400, 200, |_, _| [0.18; 3]);
        let out = render(&src, &Recipe::default(), Some(100), &never, &|_| {}).unwrap();
        assert_eq!((out.width, out.height), (100, 50));
        let want = color::srgb_encode(0.18);
        assert!(out.pixels.iter().all(|&v| (v - want).abs() < 1e-4));
        // A limit above the size changes nothing.
        let same = render(&src, &Recipe::default(), Some(4000), &never, &|_| {}).unwrap();
        assert_eq!((same.width, same.height), (400, 200));
    }

    #[test]
    fn a_radial_mask_brightens_only_under_it() {
        let src = flat(100, 100, |_, _| [0.18; 3]);
        let r = Recipe {
            masks: vec![Local {
                mask: Mask::Radial {
                    cx: 0.5,
                    cy: 0.5,
                    rx: 0.2,
                    ry: 0.2,
                    angle_deg: 0.0,
                    feather: 0.1,
                },
                invert: false,
                exposure_ev: 1.0,
                saturation: 0.0,
            }],
            ..Recipe::default()
        };
        let out = render(&src, &r, None, &never, &|_| {}).unwrap();
        let at = |x: usize, y: usize| out.pixels[(y * 100 + x) * 3];
        assert!((at(50, 50) - color::srgb_encode(0.36)).abs() < 1e-4);
        assert!((at(2, 2) - color::srgb_encode(0.18)).abs() < 1e-4);
    }

    #[test]
    fn a_render_stops_when_cancelled_and_reports_progress_until_then() {
        let src = flat(10, 300, |_, _| [0.5; 3]);
        let calls = std::sync::atomic::AtomicUsize::new(0);
        let cancel_after_two = || calls.load(std::sync::atomic::Ordering::SeqCst) >= 2;
        let progress = |_f: f32| {
            calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        };
        let r = render(&src, &Recipe::default(), None, &cancel_after_two, &progress);
        assert!(matches!(r, Err(Cancelled)));
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 2);
        let ok = render(&src, &Recipe::default(), None, &never, &|_| {});
        assert!(ok.is_ok());
    }

    #[test]
    fn quantizing_is_deterministic_and_dither_only_moves_one_code() {
        let src = flat(64, 64, |x, _| [x as f32 / 64.0 * 0.3; 3]);
        let plain = quantize(&src, Encoding::Log, false);
        let a = quantize(&src, Encoding::Log, true);
        let b = quantize(&src, Encoding::Log, true);
        assert_eq!(a, b);
        assert_ne!(a, plain);
        assert!(
            a.iter()
                .zip(&plain)
                .all(|(x, y)| (i16::from(*x) - i16::from(*y)).abs() <= 1)
        );
    }
}
