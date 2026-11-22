//! Export: render the recipe at full quality into a new file.
//!
//! Three promises, each tested:
//!
//! 1. **It never overwrites.** The name is the photo's stem plus `-edit`,
//!    numbered `-edit (2)` and so on when taken, and the final step is a
//!    rename that refuses to replace (`renameat2(RENAME_NOREPLACE)`, or a
//!    hard link where the filesystem lacks it), so a file appearing between
//!    the check and the rename is not replaced either.
//! 2. **A cancelled or failed export leaves nothing.** The file is written
//!    under a hidden partial name only after the whole image is encoded in
//!    memory, and that name is removed on every path that does not end in
//!    the rename.
//! 3. **The original's bytes are never touched**, by construction (nothing
//!    here opens it for writing) and by test.
//!
//! The output is sRGB with an embedded sRGB ICC profile (moxcms), oriented
//! (orientation baked in, none recorded). EXIF is not copied yet.

use std::ffi::CString;
use std::io::Cursor;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use image::ImageEncoder;
use serde::Deserialize;

use crate::render::{Cancelled, Rendered};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Format {
    /// 8-bit JPEG.
    Jpeg,
    /// 16 bits per channel, uncompressed TIFF.
    Tiff16,
}

impl Format {
    fn extension(self) -> &'static str {
        match self {
            Format::Jpeg => "jpg",
            Format::Tiff16 => "tif",
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ExportError {
    #[error("export cancelled; nothing was written")]
    Cancelled,
    #[error("could not encode the export: {0}")]
    Encode(String),
    #[error("could not write the export into {dir}: {source}")]
    Io {
        dir: String,
        #[source]
        source: std::io::Error,
    },
    #[error("no free name for the export in {0}")]
    NoName(String),
}

impl From<Cancelled> for ExportError {
    fn from(_: Cancelled) -> ExportError {
        ExportError::Cancelled
    }
}

/// The ICC profile every export carries: plain sRGB.
pub fn srgb_icc() -> Vec<u8> {
    moxcms::ColorProfile::new_srgb()
        .encode()
        .unwrap_or_default()
}

/// The bytes of an export, in memory.
pub fn encode(r: &Rendered, format: Format, quality: u8) -> Result<Vec<u8>, ExportError> {
    let icc = srgb_icc();
    match format {
        Format::Jpeg => {
            let mut out = Vec::new();
            let mut enc =
                image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, quality.clamp(1, 100));
            if !icc.is_empty() {
                let _ = enc.set_icc_profile(icc);
            }
            enc.write_image(
                &r.to_rgb8(),
                r.width,
                r.height,
                image::ExtendedColorType::Rgb8,
            )
            .map_err(|e| ExportError::Encode(e.to_string()))?;
            Ok(out)
        }
        Format::Tiff16 => {
            let mut out = Cursor::new(Vec::new());
            let err = |e: tiff::TiffError| ExportError::Encode(e.to_string());
            let mut enc = tiff::encoder::TiffEncoder::new(&mut out).map_err(err)?;
            let mut image = enc
                .new_image::<tiff::encoder::colortype::RGB16>(r.width, r.height)
                .map_err(err)?;
            if !icc.is_empty() {
                image
                    .encoder()
                    .write_tag(tiff::tags::Tag::IccProfile, Undefined(&icc))
                    .map_err(err)?;
            }
            image.write_data(&r.to_rgb16()).map_err(err)?;
            Ok(out.into_inner())
        }
    }
}

/// Bytes as TIFF type UNDEFINED, which is what readers expect the ICC
/// profile tag to be.
struct Undefined<'a>(&'a [u8]);

impl tiff::encoder::TiffValue for Undefined<'_> {
    const BYTE_LEN: u8 = 1;
    const FIELD_TYPE: tiff::tags::Type = tiff::tags::Type::UNDEFINED;
    fn count(&self) -> usize {
        self.0.len()
    }
    fn data(&self) -> std::borrow::Cow<'_, [u8]> {
        std::borrow::Cow::Borrowed(self.0)
    }
}

/// `IMG_1-edit.jpg`, then `IMG_1-edit (2).jpg`, and so on.
pub fn candidate(stem: &str, format: Format, n: u32) -> String {
    if n <= 1 {
        format!("{stem}-edit.{}", format.extension())
    } else {
        format!("{stem}-edit ({n}).{}", format.extension())
    }
}

/// Removes the partial file unless the export reached its final name.
struct Partial {
    path: PathBuf,
    done: bool,
}

impl Drop for Partial {
    fn drop(&mut self) {
        if !self.done {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

static SERIAL: AtomicU64 = AtomicU64::new(0);

/// Rename `from` to `to` unless `to` exists. `Ok(false)` when it exists.
fn rename_noreplace(from: &Path, to: &Path) -> std::io::Result<bool> {
    let c = |p: &Path| {
        CString::new(p.as_os_str().as_bytes())
            .map_err(|_| std::io::Error::from(std::io::ErrorKind::InvalidInput))
    };
    let (f, t) = (c(from)?, c(to)?);
    // SAFETY: both are valid NUL-terminated paths for the duration of the
    // call, and AT_FDCWD resolves them as given.
    let r = unsafe {
        libc::renameat2(
            libc::AT_FDCWD,
            f.as_ptr(),
            libc::AT_FDCWD,
            t.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    };
    if r == 0 {
        return Ok(true);
    }
    let e = std::io::Error::last_os_error();
    match e.raw_os_error() {
        Some(libc::EEXIST) => Ok(false),
        // A filesystem without RENAME_NOREPLACE: a hard link also refuses
        // to replace, and then the partial name is dropped.
        Some(libc::EINVAL) | Some(libc::ENOSYS) => match std::fs::hard_link(from, to) {
            Ok(()) => {
                let _ = std::fs::remove_file(from);
                Ok(true)
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(false),
            Err(e) => Err(e),
        },
        _ => Err(e),
    }
}

/// Write `bytes` into `dir` under the first free export name for `stem`.
/// `cancelled` is checked once more before anything touches the disk.
pub fn write_new(
    dir: &Path,
    stem: &str,
    format: Format,
    bytes: &[u8],
    cancelled: &dyn Fn() -> bool,
) -> Result<PathBuf, ExportError> {
    let io = |source| ExportError::Io {
        dir: dir.display().to_string(),
        source,
    };
    if cancelled() {
        return Err(ExportError::Cancelled);
    }
    let partial = Partial {
        path: dir.join(format!(
            ".{stem}-edit.partial-{}-{}",
            std::process::id(),
            SERIAL.fetch_add(1, Ordering::Relaxed)
        )),
        done: false,
    };
    let mut partial = partial;
    {
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&partial.path)
            .map_err(io)?;
        f.write_all(bytes).map_err(io)?;
        f.sync_all().map_err(io)?;
    }
    if cancelled() {
        return Err(ExportError::Cancelled);
    }
    for n in 1..=999 {
        let target = dir.join(candidate(stem, format, n));
        if target.exists() {
            continue;
        }
        if rename_noreplace(&partial.path, &target).map_err(io)? {
            partial.done = true;
            return Ok(target);
        }
    }
    Err(ExportError::NoName(dir.display().to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tiny() -> Rendered {
        Rendered {
            width: 4,
            height: 2,
            pixels: (0..24).map(|i| i as f32 / 24.0).collect(),
        }
    }

    fn never() -> bool {
        false
    }

    #[test]
    fn exports_take_the_next_free_name_and_never_replace() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("IMG-edit.jpg"), b"someone's file").unwrap();
        let bytes = encode(&tiny(), Format::Jpeg, 90).unwrap();
        let a = write_new(d.path(), "IMG", Format::Jpeg, &bytes, &never).unwrap();
        assert_eq!(a.file_name().unwrap(), "IMG-edit (2).jpg");
        let b = write_new(d.path(), "IMG", Format::Jpeg, &bytes, &never).unwrap();
        assert_eq!(b.file_name().unwrap(), "IMG-edit (3).jpg");
        assert_eq!(
            std::fs::read(d.path().join("IMG-edit.jpg")).unwrap(),
            b"someone's file"
        );
    }

    #[test]
    fn a_rename_onto_an_existing_name_is_refused_even_after_the_check() {
        // The race the existence check cannot close: the name appears after
        // it was found free. The rename itself must refuse.
        let d = tempfile::tempdir().unwrap();
        let from = d.path().join("a");
        let to = d.path().join("b");
        std::fs::write(&from, b"new").unwrap();
        std::fs::write(&to, b"old").unwrap();
        assert!(!rename_noreplace(&from, &to).unwrap());
        assert_eq!(std::fs::read(&to).unwrap(), b"old");
        std::fs::remove_file(&to).unwrap();
        assert!(rename_noreplace(&from, &to).unwrap());
        assert_eq!(std::fs::read(&to).unwrap(), b"new");
    }

    #[test]
    fn a_cancelled_export_leaves_no_file_at_all() {
        let d = tempfile::tempdir().unwrap();
        let bytes = encode(&tiny(), Format::Tiff16, 0).unwrap();
        // Cancelled after the partial was written, before the rename.
        let calls = std::cell::Cell::new(0);
        let second_check = || {
            calls.set(calls.get() + 1);
            calls.get() >= 2
        };
        let e = write_new(d.path(), "IMG", Format::Tiff16, &bytes, &second_check).unwrap_err();
        assert!(matches!(e, ExportError::Cancelled));
        assert_eq!(calls.get(), 2);
        assert_eq!(std::fs::read_dir(d.path()).unwrap().count(), 0);
    }

    #[test]
    fn a_tiff_export_is_16_bit_rgb_with_an_icc_profile() {
        let bytes = encode(&tiny(), Format::Tiff16, 0).unwrap();
        let mut dec = tiff::decoder::Decoder::new(Cursor::new(bytes)).unwrap();
        assert_eq!(dec.colortype().unwrap(), tiff::ColorType::RGB(16));
        assert_eq!(dec.dimensions().unwrap(), (4, 2));
        let icc = dec.get_tag_u8_vec(tiff::tags::Tag::IccProfile).unwrap();
        assert_eq!(&icc[36..40], b"acsp");
        let tiff::decoder::DecodingResult::U16(v) = dec.read_image().unwrap() else {
            panic!("not 16-bit");
        };
        assert_eq!(v[23], (23.0f32 / 24.0 * 65535.0).round() as u16);
    }

    #[test]
    fn a_jpeg_export_carries_an_srgb_profile() {
        let bytes = encode(&tiny(), Format::Jpeg, 90).unwrap();
        let mut dec = image::codecs::jpeg::JpegDecoder::new(Cursor::new(&bytes)).unwrap();
        use image::ImageDecoder;
        let icc = dec.icc_profile().unwrap().expect("no ICC profile");
        assert_eq!(&icc[36..40], b"acsp");
    }
}
