//! Reading a photo into linear light.
//!
//! Every input ends up the same shape: an RGB image of `f32` in linear light
//! with sRGB (Rec.709) primaries, EXIF orientation already applied. A JPEG,
//! PNG or WebP is taken to be sRGB; an embedded ICC profile is not read yet
//! (see the lane brief, "Deferred"). A TIFF is read with the `tiff` crate:
//! 8 or 16 bits or float, chunky or planar. A camera RAW goes through
//! rawler: demosaiced, white-balanced as shot and converted with the
//! decoder's own camera matrix, so its color is neutral rather than matched
//! to the camera's JPEG.
//!
//! A RAW file is untrusted input to a parser. rawler is memory-safe but can
//! panic on a malformed file, so each call into it runs under
//! `catch_unwind`, and a panic is reported as "could not decode" rather
//! than taking the app down with every other command in flight.

use std::io::BufReader;
use std::path::Path;

use image::{DynamicImage, ImageDecoder, ImageReader, Rgb32FImage, metadata::Orientation};
use serde::Serialize;

use crate::color::srgb_decode;

/// Largest image decoded, in pixels: a 45 MP RAW is 540 MB as `f32` RGB,
/// and a display file at the cap peaks near 1.8 GB while it decodes (its
/// bytes plus the `f32` copy). The engine runs one such decode at a time
/// (`Engine::heavy`); this is the bound on each.
pub const MAX_PIXELS: u64 = 120_000_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Format {
    Jpeg,
    Png,
    Webp,
    Tiff,
    Raw,
}

/// Whether this build's RAW decoder claims an extension (any case, no dot).
/// rawler lists its extensions in upper case.
pub fn is_raw_extension(ext: &str) -> bool {
    rawler::decoders::supported_extensions()
        .iter()
        .any(|e| e.eq_ignore_ascii_case(ext))
}

/// What a file is, by its name. `None` is a file this app does not open.
pub fn format_of(name: &str) -> Option<Format> {
    let ext = Path::new(name).extension()?.to_str()?.to_ascii_lowercase();
    match ext.as_str() {
        "jpg" | "jpeg" => Some(Format::Jpeg),
        "png" => Some(Format::Png),
        "webp" => Some(Format::Webp),
        "tif" | "tiff" => Some(Format::Tiff),
        e if is_raw_extension(e) => Some(Format::Raw),
        _ => None,
    }
}

/// What `photo.open` reports about a file, beside its pixels.
#[derive(Debug, Clone, Default, Serialize, PartialEq)]
pub struct Info {
    /// After orientation.
    pub width: u32,
    pub height: u32,
    /// The EXIF orientation the file carried, 1 to 8, now applied.
    pub orientation: u8,
    /// Bits per channel in the file.
    pub bits: u8,
    pub camera: Option<String>,
    pub lens: Option<String>,
    pub exposure: Option<String>,
    pub taken: Option<String>,
    /// `"image <version>"`, `"tiff"` or `"rawler <version>"`.
    pub decoder: String,
}

/// A decoded photo.
pub struct Source {
    pub format: Format,
    pub info: Info,
    /// Linear RGB, row-major, `width * height * 3`.
    pub pixels: Vec<f32>,
}

impl Source {
    pub fn width(&self) -> u32 {
        self.info.width
    }
    pub fn height(&self) -> u32 {
        self.info.height
    }

    /// Bilinear sample at a pixel-space position (pixel centers at +0.5).
    /// Outside the image is black: the corners a straighten exposes are
    /// shown as what they are, not smeared edge pixels.
    pub fn sample(&self, x: f64, y: f64) -> [f32; 3] {
        let (w, h) = (self.info.width as i64, self.info.height as i64);
        let fx = x - 0.5;
        let fy = y - 0.5;
        if fx < -0.5 || fy < -0.5 || fx > (w as f64) - 0.5 || fy > (h as f64) - 0.5 {
            return [0.0; 3];
        }
        let x0 = fx.floor() as i64;
        let y0 = fy.floor() as i64;
        let tx = (fx - x0 as f64) as f32;
        let ty = (fy - y0 as f64) as f32;
        let px = |x: i64, y: i64| -> [f32; 3] {
            let x = x.clamp(0, w - 1) as usize;
            let y = y.clamp(0, h - 1) as usize;
            let i = (y * w as usize + x) * 3;
            [self.pixels[i], self.pixels[i + 1], self.pixels[i + 2]]
        };
        let (a, b, c, d) = (
            px(x0, y0),
            px(x0 + 1, y0),
            px(x0, y0 + 1),
            px(x0 + 1, y0 + 1),
        );
        let mut out = [0.0; 3];
        for k in 0..3 {
            let top = a[k] + (b[k] - a[k]) * tx;
            let bottom = c[k] + (d[k] - c[k]) * tx;
            out[k] = top + (bottom - top) * ty;
        }
        out
    }
}

#[derive(Debug, thiserror::Error)]
pub enum DecodeError {
    #[error("{0} is not a photo this app opens (JPEG, PNG, WebP, TIFF or camera RAW)")]
    NotAPhoto(String),
    #[error("this camera's RAW is not supported by the decoder: {0}")]
    UnsupportedCamera(String),
    #[error("could not decode {name}: {why}")]
    Corrupt { name: String, why: String },
    #[error("{name} is {pixels} pixels, more than the {MAX_PIXELS} this app decodes")]
    TooLarge { name: String, pixels: u64 },
    #[error("cannot read {name}: {source}")]
    Io {
        name: String,
        #[source]
        source: std::io::Error,
    },
}

fn name_of(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

fn corrupt(path: &Path, why: impl ToString) -> DecodeError {
    DecodeError::Corrupt {
        name: name_of(path),
        why: why.to_string(),
    }
}

fn check_size(path: &Path, w: u32, h: u32) -> Result<(), DecodeError> {
    let pixels = u64::from(w) * u64::from(h);
    if pixels > MAX_PIXELS {
        return Err(DecodeError::TooLarge {
            name: name_of(path),
            pixels,
        });
    }
    Ok(())
}

/// Decode a photo to linear light, oriented.
pub fn decode(path: &Path) -> Result<Source, DecodeError> {
    let name = name_of(path);
    let format = format_of(&name).ok_or_else(|| DecodeError::NotAPhoto(name.clone()))?;
    match format {
        Format::Raw => decode_raw(path),
        Format::Tiff => decode_tiff(path),
        _ => decode_display(path, format),
    }
}

/// Display-referred image data, 0..1 sRGB-encoded, to linear light.
fn linearize(img: Rgb32FImage) -> Vec<f32> {
    let mut v = img.into_raw();
    for c in &mut v {
        *c = srgb_decode(c.clamp(0.0, 1.0));
    }
    v
}

fn oriented(mut img: DynamicImage, exif: u8) -> DynamicImage {
    if let Some(o) = Orientation::from_exif(exif) {
        img.apply_orientation(o);
    }
    img
}

fn orientation_number(o: Orientation) -> u8 {
    o.to_exif()
}

fn decode_display(path: &Path, format: Format) -> Result<Source, DecodeError> {
    let file = std::fs::File::open(path).map_err(|source| DecodeError::Io {
        name: name_of(path),
        source,
    })?;
    let reader = ImageReader::new(BufReader::new(file))
        .with_guessed_format()
        .map_err(|e| corrupt(path, e))?;
    let mut decoder = reader.into_decoder().map_err(|e| corrupt(path, e))?;
    let (w, h) = decoder.dimensions();
    check_size(path, w, h)?;
    let orientation = decoder.orientation().map(orientation_number).unwrap_or(1);
    let bits = (decoder.color_type().bits_per_pixel()
        / u16::from(decoder.color_type().channel_count())) as u8;
    let img = DynamicImage::from_decoder(decoder).map_err(|e| corrupt(path, e))?;
    let img = oriented(img, orientation);
    let (width, height) = (img.width(), img.height());
    Ok(Source {
        format,
        info: Info {
            width,
            height,
            orientation,
            bits,
            decoder: "image 0.25".to_string(),
            ..Info::default()
        },
        pixels: linearize(img.into_rgb32f()),
    })
}

/// The `tiff` crate's decoding limits, raised to this app's own cap. Its
/// default buffer limit is 256 MiB, which refuses a 16-bit RGB TIFF over
/// about 44.7 MP, this app's own `tiff16` export of a 45 MP RAW included.
/// The pixel count is already checked against [`MAX_PIXELS`] before the
/// buffer is allocated, so the cap that matters is that one; this only has
/// to admit the largest sample layout read here (four `f32` channels).
fn tiff_limits() -> tiff::decoder::Limits {
    let mut limits = tiff::decoder::Limits::default();
    limits.decoding_buffer_size = usize::try_from(MAX_PIXELS)
        .unwrap_or(usize::MAX)
        .saturating_mul(4 * 4);
    limits
}

/// Interleaved RGB from a TIFF's samples, chunky (RGBRGB...) or planar
/// (RRR... GGG... BBB...). `None` when there are fewer samples than the
/// dimensions say.
fn tiff_rgb<T: Copy>(
    v: &[T],
    pixels: usize,
    channels: usize,
    planar: bool,
    f: impl Fn(T) -> f32,
) -> Option<Vec<f32>> {
    if v.len() < pixels.checked_mul(channels)? {
        return None;
    }
    let at = |i: usize, c: usize| {
        if planar {
            f(v[c * pixels + i])
        } else {
            f(v[i * channels + c])
        }
    };
    let mut rgb = Vec::with_capacity(pixels * 3);
    for i in 0..pixels {
        if channels < 3 {
            rgb.extend([at(i, 0); 3]);
        } else {
            rgb.extend([at(i, 0), at(i, 1), at(i, 2)]);
        }
    }
    Some(rgb)
}

fn decode_tiff(path: &Path) -> Result<Source, DecodeError> {
    use tiff::decoder::{Decoder, DecodingResult};
    use tiff::tags::Tag;
    let file = std::fs::File::open(path).map_err(|source| DecodeError::Io {
        name: name_of(path),
        source,
    })?;
    let mut decoder = Decoder::new(BufReader::new(file))
        .map_err(|e| corrupt(path, e))?
        .with_limits(tiff_limits());
    let (w, h) = decoder.dimensions().map_err(|e| corrupt(path, e))?;
    check_size(path, w, h)?;
    let color = decoder.colortype().map_err(|e| corrupt(path, e))?;
    let orientation = decoder
        .get_tag_u32(Tag::Orientation)
        .ok()
        .and_then(|o| u8::try_from(o).ok())
        .filter(|o| (1..=8).contains(o))
        .unwrap_or(1);
    // PlanarConfiguration 2: each channel stored as its own plane.
    let planar = decoder.get_tag_u32(Tag::PlanarConfiguration).ok() == Some(2);
    let channels = match color {
        tiff::ColorType::Gray(_) => 1,
        tiff::ColorType::GrayA(_) => 2,
        tiff::ColorType::RGB(_) => 3,
        tiff::ColorType::RGBA(_) => 4,
        other => {
            return Err(corrupt(
                path,
                format!("unsupported TIFF color type {other:?}"),
            ));
        }
    };
    // `read_image` reads only a planar file's first plane; this reads them
    // all (within the limits above).
    let mut samples = DecodingResult::U8(Vec::new());
    decoder
        .read_image_to_buffer(&mut samples)
        .map_err(|e| corrupt(path, e))?;
    let pixels = (w as usize) * (h as usize);
    let (rgb, bits) = match &samples {
        DecodingResult::U8(v) => (
            tiff_rgb(v, pixels, channels, planar, |x| f32::from(x) / 255.0),
            8,
        ),
        DecodingResult::U16(v) => (
            tiff_rgb(v, pixels, channels, planar, |x| f32::from(x) / 65535.0),
            16,
        ),
        DecodingResult::F32(v) => (tiff_rgb(v, pixels, channels, planar, |x| x), 32),
        _ => return Err(corrupt(path, "unsupported TIFF sample format")),
    };
    drop(samples);
    let rgb = rgb.ok_or_else(|| corrupt(path, "short image data"))?;
    let img = Rgb32FImage::from_raw(w, h, rgb).ok_or_else(|| corrupt(path, "bad dimensions"))?;
    let img = oriented(DynamicImage::ImageRgb32F(img), orientation);
    let (width, height) = (img.width(), img.height());
    Ok(Source {
        format: Format::Tiff,
        info: Info {
            width,
            height,
            orientation,
            bits,
            decoder: "tiff 0.11".to_string(),
            ..Info::default()
        },
        pixels: linearize(img.into_rgb32f()),
    })
}

/// Run a rawler call, turning a panic into a decode error.
fn guarded<T>(path: &Path, f: impl FnOnce() -> Result<T, DecodeError>) -> Result<T, DecodeError> {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)) {
        Ok(result) => result,
        Err(_) => Err(corrupt(path, "the RAW decoder failed on this file")),
    }
}

fn raw_error(path: &Path, e: rawler::RawlerError) -> DecodeError {
    match e {
        rawler::RawlerError::Unsupported { make, model, .. } => {
            DecodeError::UnsupportedCamera(format!("{make} {model}").trim().to_string())
        }
        other => corrupt(path, other),
    }
}

fn rational(r: &rawler::formats::tiff::Rational) -> Option<f64> {
    (r.d != 0).then(|| f64::from(r.n) / f64::from(r.d))
}

fn raw_info(md: &rawler::decoders::RawMetadata) -> Info {
    let e = &md.exif;
    let camera = format!("{} {}", md.make, md.model).trim().to_string();
    let lens = e
        .lens_model
        .clone()
        .or_else(|| md.lens.as_ref().map(|l| l.lens_model.clone()));
    let mut parts = Vec::new();
    if let Some(t) = e.exposure_time.as_ref().and_then(rational) {
        parts.push(if t < 1.0 && t > 0.0 {
            format!("1/{:.0} s", 1.0 / t)
        } else {
            format!("{t} s")
        });
    }
    if let Some(f) = e.fnumber.as_ref().and_then(rational) {
        parts.push(format!("f/{f:.1}"));
    }
    if let Some(iso) = e.iso_speed_ratings {
        parts.push(format!("ISO {iso}"));
    }
    if let Some(fl) = e.focal_length.as_ref().and_then(rational) {
        parts.push(format!("{fl:.0} mm"));
    }
    Info {
        camera: (!camera.is_empty()).then_some(camera),
        lens,
        exposure: (!parts.is_empty()).then(|| parts.join(", ")),
        taken: e.date_time_original.clone(),
        decoder: "rawler 0.8.0".to_string(),
        ..Info::default()
    }
}

fn decode_raw(path: &Path) -> Result<Source, DecodeError> {
    use rawler::decoders::RawDecodeParams;
    use rawler::imgop::develop::{Intermediate, ProcessingStep, RawDevelop};
    guarded(path, || {
        let src = rawler::rawsource::RawSource::new(path).map_err(|source| DecodeError::Io {
            name: name_of(path),
            source,
        })?;
        let decoder = rawler::get_decoder(&src).map_err(|e| raw_error(path, e))?;
        let params = RawDecodeParams::default();
        let md = decoder
            .raw_metadata(&src, &params)
            .map_err(|e| raw_error(path, e))?;
        let raw = decoder
            .raw_image(&src, &params, false)
            .map_err(|e| raw_error(path, e))?;
        check_size(path, raw.width as u32, raw.height as u32)?;
        // Linear output: everything rawler's default development does except
        // the final sRGB gamma, which is this pipeline's step 3.
        let develop = RawDevelop::new_with(&[
            ProcessingStep::Rescale,
            ProcessingStep::Demosaic,
            ProcessingStep::FujiRotate,
            ProcessingStep::CropActiveArea,
            ProcessingStep::WhiteBalance,
            ProcessingStep::Calibrate,
            ProcessingStep::CropDefault,
        ]);
        let developed = develop
            .develop_intermediate(&raw)
            .map_err(|e| raw_error(path, e))?;
        let (w, h, rgb) = match developed {
            Intermediate::ThreeColor(px) => {
                let (w, h) = (px.dim().w as u32, px.dim().h as u32);
                (w, h, px.data.into_iter().flatten().collect::<Vec<f32>>())
            }
            Intermediate::Monochrome(px) => {
                let (w, h) = (px.dim().w as u32, px.dim().h as u32);
                (w, h, px.data.into_iter().flat_map(|v| [v; 3]).collect())
            }
            Intermediate::FourColor(_) => {
                return Err(corrupt(
                    path,
                    "a four-color sensor this decoder left undeveloped",
                ));
            }
        };
        // rawler's own `orientation` field reads Normal for files whose EXIF
        // says otherwise (spike 3: a portrait CR2 with Orientation 8), so the
        // EXIF value is the one used.
        let orientation = md
            .exif
            .orientation
            .and_then(|o| u8::try_from(o).ok())
            .filter(|o| (1..=8).contains(o))
            .unwrap_or(1);
        let img =
            Rgb32FImage::from_raw(w, h, rgb).ok_or_else(|| corrupt(path, "bad dimensions"))?;
        let img = oriented(DynamicImage::ImageRgb32F(img), orientation);
        let mut info = raw_info(&md);
        info.width = img.width();
        info.height = img.height();
        info.orientation = orientation;
        info.bits = raw.bps as u8;
        let DynamicImage::ImageRgb32F(img) = img else {
            return Err(corrupt(path, "unexpected pixel layout"));
        };
        Ok(Source {
            format: Format::Raw,
            info,
            pixels: img.into_raw(),
        })
    })
}

/// The JPEG a camera embeds in its RAW, decoded, oriented and scaled to at
/// most `max_edge`. For Quick Look and Files thumbnails: fast, and the
/// camera's own rendering. Non-RAW photos are decoded and scaled instead.
pub fn embedded_preview(path: &Path, max_edge: u32) -> Result<DynamicImage, DecodeError> {
    use rawler::decoders::RawDecodeParams;
    let name = name_of(path);
    let format = format_of(&name).ok_or_else(|| DecodeError::NotAPhoto(name.clone()))?;
    let img = if format == Format::Raw {
        guarded(path, || {
            let src =
                rawler::rawsource::RawSource::new(path).map_err(|source| DecodeError::Io {
                    name: name.clone(),
                    source,
                })?;
            let decoder = rawler::get_decoder(&src).map_err(|e| raw_error(path, e))?;
            let params = RawDecodeParams::default();
            let md = decoder
                .raw_metadata(&src, &params)
                .map_err(|e| raw_error(path, e))?;
            let preview = decoder
                .preview_image(&src, &params)
                .ok()
                .flatten()
                .or_else(|| decoder.thumbnail_image(&src, &params).ok().flatten())
                .ok_or_else(|| corrupt(path, "this RAW carries no embedded preview"))?;
            let o = md
                .exif
                .orientation
                .and_then(|o| u8::try_from(o).ok())
                .unwrap_or(1);
            Ok(oriented(preview, o))
        })?
    } else {
        let s = decode(path)?;
        let mut out = image::RgbImage::new(s.width(), s.height());
        for (i, p) in out.pixels_mut().enumerate() {
            for c in 0..3 {
                p.0[c] = (crate::color::srgb_encode(s.pixels[i * 3 + c]) * 255.0).round() as u8;
            }
        }
        DynamicImage::ImageRgb8(out)
    };
    Ok(if img.width().max(img.height()) > max_edge {
        img.resize(max_edge, max_edge, image::imageops::FilterType::Triangle)
    } else {
        img
    })
}
