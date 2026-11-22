//! A working directory with a `Photos/` folder and a synthetic photo in it.
//! Each test binary uses part of this, hence the `dead_code` allowance.
#![allow(dead_code)]

pub mod dng;

use std::path::{Path, PathBuf};

use image::{ImageEncoder, Rgb, RgbImage};

pub struct Workdir {
    pub dir: tempfile::TempDir,
}

impl Workdir {
    pub fn new() -> Workdir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("Photos")).unwrap();
        Workdir { dir }
    }

    pub fn path(&self) -> &Path {
        self.dir.path()
    }

    pub fn photos(&self) -> PathBuf {
        self.dir.path().join("Photos")
    }

    /// Files in `Photos/`, sorted.
    pub fn names(&self) -> Vec<String> {
        let mut v: Vec<String> = std::fs::read_dir(self.photos())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        v.sort();
        v
    }
}

/// A photo-like test card: smooth gradients, saturated patches and a
/// little deterministic noise, so a LUT is exercised across its whole cube.
pub fn test_card(w: u32, h: u32) -> RgbImage {
    let mut seed = 0x1234_5678u32;
    let mut noise = move || {
        seed ^= seed << 13;
        seed ^= seed >> 17;
        seed ^= seed << 5;
        (seed % 7) as i32 - 3
    };
    RgbImage::from_fn(w, h, |x, y| {
        let u = x as f32 / w as f32;
        let v = y as f32 / h as f32;
        let base = if v < 0.5 {
            // Sky-like gradient over the top half.
            [0.25 + 0.5 * u, 0.4 + 0.4 * u, 0.85 - 0.2 * v]
        } else {
            // Patches: hue across, value down.
            let hue = (u * 6.0).floor() / 6.0;
            let val = 0.2 + 0.8 * ((v - 0.5) * 2.0);
            let (r, g, b) = match (hue * 6.0) as u32 {
                0 => (1.0, 0.3, 0.2),
                1 => (0.9, 0.8, 0.2),
                2 => (0.3, 0.8, 0.3),
                3 => (0.2, 0.7, 0.8),
                4 => (0.3, 0.3, 0.9),
                _ => (0.8, 0.4, 0.7),
            };
            [r * val, g * val, b * val]
        };
        let px = base.map(|c| ((c * 255.0) as i32 + noise()).clamp(0, 255) as u8);
        Rgb(px)
    })
}

pub fn write_jpeg(path: &Path, img: &RgbImage) {
    let mut out = Vec::new();
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, 95)
        .write_image(
            img.as_raw(),
            img.width(),
            img.height(),
            image::ExtendedColorType::Rgb8,
        )
        .unwrap();
    std::fs::write(path, out).unwrap();
}

pub fn write_png(path: &Path, img: &RgbImage) {
    img.save(path).unwrap();
}
