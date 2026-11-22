//! Spike 2 (lane brief 5a): does an 8-bit proxy band under a hard edit?
//!
//! A synthetic sky: a smooth linear-light gradient two stops under, pushed
//! +2 EV through a strong S-curve, which stretches the gradient's codes the
//! most a v1 recipe can. Each proxy variant is quantized with the
//! production `render::quantize`, then developed with the production
//! pipeline evaluated directly (no LUT, so only the proxy's precision is
//! measured), and compared with the same pipeline on the unquantized f32.
//!
//! Run: `cargo run --release -p deckjack-photo --example banding`

use deckjack_photo::color::{Encoding, Global};
use deckjack_photo::decode::{Format, Info, Source};
use deckjack_photo::recipe::Recipe;
use deckjack_photo::render::quantize;
use image::ImageEncoder;

const W: u32 = 2048;
const H: u32 = 256;

fn png8(codes: &[u8]) -> usize {
    let mut out = Vec::new();
    image::codecs::png::PngEncoder::new_with_quality(
        &mut out,
        image::codecs::png::CompressionType::Fast,
        image::codecs::png::FilterType::Adaptive,
    )
    .write_image(codes, W, H, image::ExtendedColorType::Rgb8)
    .unwrap();
    out.len()
}

fn png16(values: &[f32], encoding: Encoding) -> usize {
    let words: Vec<u8> = values
        .iter()
        .flat_map(|v| ((encoding.encode(*v) * 65535.0).round() as u16).to_be_bytes())
        .collect();
    let mut out = Vec::new();
    image::codecs::png::PngEncoder::new_with_quality(
        &mut out,
        image::codecs::png::CompressionType::Fast,
        image::codecs::png::FilterType::Adaptive,
    )
    .write_image(&words, W, H, image::ExtendedColorType::Rgb16)
    .unwrap();
    out.len()
}

/// Output 8-bit codes for every pixel, through `global`.
fn develop(inputs: &[f32], global: &Global) -> Vec<f32> {
    inputs
        .as_chunks::<3>()
        .0
        .iter()
        .flat_map(|p| global.apply(*p).map(|v| (v * 255.0).round()))
        .collect()
}

/// Green channel, averaged down each column: what the eye integrates.
fn column_means(out: &[f32]) -> Vec<f64> {
    (0..W as usize)
        .map(|x| {
            (0..H as usize)
                .map(|y| f64::from(out[(y * W as usize + x) * 3 + 1]))
                .sum::<f64>()
                / f64::from(H)
        })
        .collect()
}

/// The longest run of one output code along the middle row, in pixels: a
/// band's width.
fn longest_run(out: &[f32]) -> usize {
    let y = H as usize / 2;
    let row: Vec<f32> = (0..W as usize)
        .map(|x| out[(y * W as usize + x) * 3 + 1])
        .collect();
    let (mut best, mut run) = (1, 1);
    for x in 1..row.len() {
        if row[x] == row[x - 1] {
            run += 1;
            best = best.max(run);
        } else {
            run = 1;
        }
    }
    best
}

fn encode_png(codes: &[u8], w: u32, h: u32, color: image::ExtendedColorType) -> usize {
    let mut png = Vec::new();
    image::codecs::png::PngEncoder::new_with_quality(
        &mut png,
        image::codecs::png::CompressionType::Fast,
        image::codecs::png::FilterType::Adaptive,
    )
    .write_image(codes, w, h, color)
    .unwrap();
    png.len()
}

/// The same comparison on a real RAW, downscaled to a 2048 px proxy: writes
/// the f32 reference and each 8-bit variant's developed result for
/// `spike/compare_raw.py`, and prints the proxy sizes.
fn real(path: &str, out: &str, recipe: &Recipe) {
    let src = deckjack_photo::decode::decode(std::path::Path::new(path)).expect("decode");
    let small = deckjack_photo::render::downscale(&src, 2048);
    let (w, h) = (small.width(), small.height());
    std::fs::create_dir_all(out).unwrap();
    let write = |name: &str, v: &[f32]| {
        let bytes: Vec<u8> = v.iter().flat_map(|x| (x / 255.0).to_le_bytes()).collect();
        std::fs::write(format!("{out}/{name}.f32"), bytes).unwrap();
    };
    write(
        "reference",
        &develop(&small.pixels, &Global::new(recipe, Encoding::Linear)),
    );
    for (name, dither) in [("log8", false), ("log8-dither", true)] {
        let codes = quantize(&small, Encoding::Log, dither);
        let inputs: Vec<f32> = codes.iter().map(|c| f32::from(*c) / 255.0).collect();
        write(name, &develop(&inputs, &Global::new(recipe, Encoding::Log)));
        let size = encode_png(&codes, w, h, image::ExtendedColorType::Rgb8);
        println!("{name}: {w}x{h} PNG {size} bytes");
    }
    let words: Vec<u8> = small
        .pixels
        .iter()
        .flat_map(|v| ((Encoding::Log.encode(*v) * 65535.0).round() as u16).to_be_bytes())
        .collect();
    let size = encode_png(&words, w, h, image::ExtendedColorType::Rgb16);
    println!("log16: {w}x{h} PNG {size} bytes");
    std::fs::write(format!("{out}/size"), format!("{w} {h}")).unwrap();
}

fn main() {
    let recipe: Recipe = serde_json::from_str(
        r#"{"exposure_ev": 2.0,
            "curves": {"master": [[0,0],[0.25,0.1],[0.5,0.5],[0.75,0.92],[1,1]]}}"#,
    )
    .unwrap();
    let args: Vec<String> = std::env::args().skip(1).collect();
    if let [path, out] = args.as_slice() {
        real(path, out, &recipe);
        return;
    }
    // Sky: 0.03 to 0.12 linear across, a slight blue cast, 2 EV under.
    let mut pixels = Vec::with_capacity((W * H * 3) as usize);
    for _y in 0..H {
        for x in 0..W {
            let t = x as f32 / (W - 1) as f32;
            let l = 0.03 + 0.09 * t;
            pixels.extend([l * 0.8, l * 0.95, l * 1.2]);
        }
    }
    let sky = Source {
        format: Format::Raw,
        info: Info {
            width: W,
            height: H,
            ..Info::default()
        },
        pixels,
    };

    let reference = develop(&sky.pixels, &Global::new(&recipe, Encoding::Linear));
    let ref_means = column_means(&reference);
    let ref_run = longest_run(&reference);
    let ref_codes = {
        let mut c: Vec<f32> = (0..W as usize)
            .map(|x| reference[(H as usize / 2 * W as usize + x) * 3 + 1])
            .collect();
        c.dedup();
        c.len()
    };
    println!(
        "reference (f32): {ref_codes} runs of one code along the middle row, longest run {ref_run} px; 16-bit PNG proxy {} bytes (log) / {} bytes (sRGB)",
        png16(&sky.pixels, Encoding::Log),
        png16(&sky.pixels, Encoding::Srgb)
    );
    for (encoding, dither) in [
        (Encoding::Srgb, false),
        (Encoding::Srgb, true),
        (Encoding::Log, false),
        (Encoding::Log, true),
    ] {
        let codes = quantize(&sky, encoding, dither);
        let inputs: Vec<f32> = codes.iter().map(|c| f32::from(*c) / 255.0).collect();
        let out = develop(&inputs, &Global::new(&recipe, encoding));
        let means = column_means(&out);
        let mean_err = means
            .iter()
            .zip(&ref_means)
            .map(|(a, b)| (a - b).abs())
            .fold(0.0, f64::max);
        let mut row: Vec<f32> = (0..W as usize)
            .map(|x| out[(H as usize / 2 * W as usize + x) * 3 + 1])
            .collect();
        let max_step = row
            .windows(2)
            .map(|w| (w[1] - w[0]).abs())
            .fold(0.0, f32::max);
        row.dedup();
        println!(
            "{encoding:?}{}: {} runs of one code on the middle row, longest run {} px, largest step {} codes, column-mean error max {:.2} codes, PNG {} bytes",
            if dither { " + dither" } else { "" },
            row.len(),
            longest_run(&out),
            max_step,
            mean_err,
            png8(&codes)
        );
    }
}
