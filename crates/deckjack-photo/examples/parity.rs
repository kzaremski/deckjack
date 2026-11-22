//! Spike 1 (lane brief 5a): does a baked 3D LUT applied on the GPU match the
//! Rust pipeline evaluated directly?
//!
//! This half writes, for one photo and one recipe, everything the browser
//! half (`spike/lut-parity.mjs`) needs, plus the two references:
//!
//! - `proxy.png`: the production proxy (`render::proxy`);
//! - `global-17.bin`, `global-33.bin`, `local-17.bin`, `local-33.bin`: the
//!   production LUT bake, as the wire carries it;
//! - `direct.f32`: the pipeline evaluated directly on the proxy's own codes,
//!   per pixel, as display values (the "truth" for the preview);
//! - `model-17.f32`, `model-33.f32`: the same LUTs applied by Rust trilinear
//!   lookup, which separates LUT interpolation error from GPU error;
//! - `spike.json`: the recipe and sizes.
//!
//! Run: `cargo run --release -p deckjack-photo --example parity -- <photo> <out-dir> [recipe.json]`

use std::path::PathBuf;

use deckjack_photo::color::{self, Global, LocalOp};
use deckjack_photo::decode;
use deckjack_photo::lut::Lut;
use deckjack_photo::recipe::Recipe;
use deckjack_photo::render;

fn write_f32(path: PathBuf, v: &[f32]) {
    let bytes: Vec<u8> = v.iter().flat_map(|x| x.to_le_bytes()).collect();
    std::fs::write(path, bytes).expect("write");
}

fn main() {
    let mut args = std::env::args().skip(1);
    let photo = PathBuf::from(args.next().expect("photo path"));
    let out = PathBuf::from(args.next().expect("output directory"));
    let recipe: Recipe = match args.next() {
        Some(p) => {
            serde_json::from_str(&std::fs::read_to_string(p).expect("recipe")).expect("json")
        }
        None => serde_json::from_str(include_str!("spike-recipe.json")).expect("json"),
    };
    recipe.validate().expect("valid recipe");
    std::fs::create_dir_all(&out).expect("mkdir");

    let src = decode::decode(&photo).expect("decode");
    let proxy = render::proxy(&src, 1024).expect("proxy");
    std::fs::write(out.join("proxy.png"), &proxy.png).expect("write");
    let codes = image::load_from_memory(&proxy.png)
        .expect("png")
        .into_rgb8();
    let (w, h) = (proxy.width, proxy.height);

    let global = Global::new(&recipe, proxy.encoding);
    let locals = color::locals(&recipe, w as f32, h as f32);
    for size in [17usize, 33] {
        let started = std::time::Instant::now();
        let g = Lut::bake(size, |c| global.apply(c));
        println!(
            "bake {size}^3 (global): {:.2} ms, {} bytes",
            started.elapsed().as_secs_f64() * 1000.0,
            g.to_bytes().len()
        );
        std::fs::write(out.join(format!("global-{size}.bin")), g.to_bytes()).expect("write");
        let local_luts: Vec<Lut> = locals
            .iter()
            .map(|(op, _)| Lut::bake(size, |c| op.apply(c)))
            .collect();
        for (i, l) in local_luts.iter().enumerate() {
            std::fs::write(out.join(format!("local{i}-{size}.bin")), l.to_bytes()).expect("write");
        }
        // The Rust model of what the shader does.
        let mut model = Vec::with_capacity((w * h * 3) as usize);
        for (x, y, p) in codes.enumerate_pixels() {
            let code = p.0.map(|c| f32::from(c) / 255.0);
            let mut v = g.apply(code);
            let (u, t) = ((x as f32 + 0.5) / w as f32, (y as f32 + 0.5) / h as f32);
            for (i, (_, mask)) in locals.iter().enumerate() {
                let m = mask.at(u, t);
                if m > 0.0 {
                    v = color::blend(v, local_luts[i].apply(v), m);
                }
            }
            model.extend(v);
        }
        write_f32(out.join(format!("model-{size}.f32")), &model);
    }

    let mut direct = Vec::with_capacity((w * h * 3) as usize);
    for (x, y, p) in codes.enumerate_pixels() {
        let code = p.0.map(|c| f32::from(c) / 255.0);
        let mut v = global.apply(code);
        let (u, t) = ((x as f32 + 0.5) / w as f32, (y as f32 + 0.5) / h as f32);
        for (op, mask) in &locals {
            let m = mask.at(u, t);
            if m > 0.0 {
                v = color::blend(v, LocalOp::apply(op, v), m);
            }
        }
        direct.extend(v);
    }
    write_f32(out.join("direct.f32"), &direct);
    let meta = serde_json::json!({
        "width": w,
        "height": h,
        "encoding": proxy.encoding,
        "proxy_bytes": proxy.png.len(),
        "recipe": recipe,
    });
    std::fs::write(out.join("spike.json"), meta.to_string()).expect("write");
    println!(
        "{w}x{h} proxy {} bytes, {:?}",
        proxy.png.len(),
        proxy.encoding
    );
}
