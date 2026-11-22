//! Program monitor stills, as command results.
//!
//! Under the isolated frame (docs/APP-UI-ISOLATION.md) the window has
//! `connect-src 'none'` and no `/content`, so a still travels in an ordinary
//! brokered invoke as base64 JPEG, and the window wraps it in a `blob:` URL.
//! That is why the size is bounded here: a still is well under the broker's
//! limits and the 8 MiB app frame cap.

use base64::Engine as _;
use image::ImageEncoder;
use serde_json::{Value, json};

use crate::engine::{Project, Result, VideoError};

pub const DEFAULT_WIDTH: u32 = 640;
pub const MIN_WIDTH: u32 = 64;
pub const MAX_WIDTH: u32 = 1920;

/// JPEG quality for scrub stills: small enough to scrub with over a
/// tailnet, sharp enough to find a cut point on.
const QUALITY: u8 = 80;

/// Render `frame` at about `width` pixels and encode it.
pub fn render(project: &Project, frame: i64, width: u32) -> Result<Value> {
    let width = width.clamp(MIN_WIDTH, MAX_WIDTH);
    let (rgba, w, h) = project.render(frame, width)?;
    let jpeg = encode(&rgba, w, h)?;
    Ok(json!({
        "frame": frame.max(0),
        "width": w,
        "height": h,
        "mime": "image/jpeg",
        "data": base64::engine::general_purpose::STANDARD.encode(&jpeg),
    }))
}

/// RGBA (already composited over black) to JPEG.
pub fn encode(rgba: &[u8], w: u32, h: u32) -> Result<Vec<u8>> {
    if rgba.len() != (w as usize) * (h as usize) * 4 || w == 0 || h == 0 {
        return Err(VideoError::Engine(format!(
            "the renderer returned {} bytes for a {w}x{h} frame",
            rgba.len()
        )));
    }
    let rgb: Vec<u8> = rgba
        .as_chunks::<4>()
        .0
        .iter()
        .flat_map(|p| [p[0], p[1], p[2]])
        .collect();
    let mut out = Vec::new();
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, QUALITY)
        .write_image(&rgb, w, h, image::ExtendedColorType::Rgb8)
        .map_err(|e| VideoError::Engine(format!("could not encode the still: {e}")))?;
    Ok(out)
}
