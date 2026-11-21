//! Thumbnails for Files: `files.thumbnail`.
//!
//! A small picture of an image file, made on demand and kept in a cache the
//! operator may delete at any time. What this module promises:
//!
//! - **The same boundary as every file command.** The source is named by a
//!   [`NodePath`] and resolved through [`Namespace::resolve`], so the
//!   working-directory boundary and the `.deckjack/` refusal are enforced by
//!   the code that already enforces them. Nothing here joins strings into a
//!   host path.
//! - **The cache is a cache.** It lives under `<install>/cache/files/`
//!   ([`cache_dir`]), never in the hidden state. An entry is keyed by the file's address, its modification
//!   time to the nanosecond, its length and the requested size, so an edited
//!   photo gets a new thumbnail rather than the old one. Deleting the whole
//!   directory, even while the server runs, costs a regeneration and nothing
//!   else. A cache that cannot be written is logged and skipped: the
//!   thumbnail is still returned.
//! - **The cache directory is not trusted.** By default the working
//!   directory *is* the install directory, so `cache/files` is listable and
//!   replaceable through the Write-level file commands. The cache therefore
//!   refuses to use a `cache` or `cache/files` that is a symlink or not a
//!   directory, never follows a symlink in place of an entry, and counts and
//!   evicts only files named like its own entries (64 hex digits), so a
//!   ReadOnly thumbnail request can never delete or read anything it did not
//!   write. (A swap between the check and the use is not closed: the checks
//!   are path-based, not descriptor-based. Eviction's name rule is what
//!   bounds that window to files named like cache entries.)
//! - **Bounded.** The cache has a byte cap ([`DEFAULT_CACHE_CAP`]); past it
//!   the least recently used entries go. A source over [`MAX_SOURCE`] is
//!   refused before it is read, decoding has allocation and dimension limits,
//!   all decodes together share one memory budget ([`DEFAULT_DECODE_BUDGET`],
//!   reserved from the size the image's header declares before its pixels are
//!   allocated), and only a few decodes run at once, because a folder of two hundred
//!   photos asks for two hundred thumbnails in the same second and each
//!   decode of a phone photo holds tens of megabytes.
//!
//! **Why the decoding is in the core.** The decoders are the `image` crate's
//! pure-Rust ones (JPEG, PNG, GIF, WebP, BMP): memory-safe, with no C library
//! behind them, and a panic inside one is caught by the blocking task and
//! reported rather than taking the server down. Video is not thumbnailed:
//! that needs ffmpeg, and running ffmpeg on an untrusted file in the
//! unconfined core is the wrong place for it. It belongs in a confined
//! helper, and until one exists a video row shows its icon.

use std::io::{self, Cursor};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use thiserror::Error;

use crate::command::{Command, CommandError, CommandId, Context, RiskLevel};
use crate::files::{FileError, Namespace, NodePath, Root};

/// The cache's directory name under `<install>/cache/`.
pub const CACHE_NAME: &str = "files";

/// Where Files keeps its thumbnails for an install directory.
pub fn cache_dir(install: &Path) -> PathBuf {
    install.join(crate::apps::CACHE_SUBDIR).join(CACHE_NAME)
}

/// Bytes the thumbnail cache may hold before the oldest entries go. At about
/// 15 KB a thumbnail that is four thousand pictures, which is several
/// folders of photos and not a dent in any disk.
pub const DEFAULT_CACHE_CAP: u64 = 64 * 1024 * 1024;

/// The largest file a thumbnail is made from. A 50-megapixel JPEG is about
/// 25 MB; anything over this is a RAW or a scan, which these decoders do not
/// read anyway.
pub const MAX_SOURCE: u64 = 48 * 1024 * 1024;

/// The sizes a thumbnail is made at. A request is rounded up to one of these
/// so a resized window does not fill the cache with near-duplicates.
pub const SIZES: [u32; 3] = [128, 256, 512];

/// The size when the request does not say.
pub const DEFAULT_SIZE: u32 = 256;

/// Decoding limits: a picture claiming a larger canvas than this is refused
/// before its pixels are allocated.
const MAX_DIMENSION: u32 = 16_384;
const MAX_DECODE_ALLOC: u64 = 384 * 1024 * 1024;

/// Bytes of decoded pixels every decode in flight may hold together. A
/// decode reserves its image's declared size from this before allocating,
/// and waits while others hold it; one that alone exceeds it is refused. So
/// four tiny files that each declare a 360 MB canvas take turns rather than
/// pinning well over a gigabyte at once.
pub const DEFAULT_DECODE_BUDGET: u64 = MAX_DECODE_ALLOC;

/// What a thumbnail is made of, by extension. The extension is only the
/// gate that decides whether to read the file at all; the decoder then goes
/// by the bytes, so a PNG named `.jpg` still works.
const IMAGE_EXTENSIONS: [&str; 6] = ["jpg", "jpeg", "png", "gif", "webp", "bmp"];

/// Whether Files can make a thumbnail of a file with this name.
pub fn is_thumbnailable(name: &str) -> bool {
    let Some((_, ext)) = name.rsplit_once('.') else {
        return false;
    };
    IMAGE_EXTENSIONS
        .iter()
        .any(|known| known.eq_ignore_ascii_case(ext))
}

/// Round a requested edge length up to one the cache keeps.
pub fn bucket(requested: Option<u32>) -> u32 {
    let want = requested.unwrap_or(DEFAULT_SIZE);
    SIZES
        .iter()
        .copied()
        .find(|&s| s >= want)
        .unwrap_or(SIZES[SIZES.len() - 1])
}

#[derive(Debug, Error)]
pub enum ThumbnailError {
    #[error(transparent)]
    File(#[from] FileError),

    #[error(
        "{path} is not an image Files makes thumbnails of; it reads JPEG, PNG, GIF, WebP and BMP"
    )]
    NotAnImage { path: NodePath },

    #[error("{path} is {len} bytes, over the {limit} byte limit for a thumbnail")]
    TooLarge {
        path: NodePath,
        len: u64,
        limit: u64,
    },

    #[error(
        "{path} would need {needed} bytes to decode, over the {limit} byte budget for thumbnails"
    )]
    TooManyPixels {
        path: NodePath,
        needed: u64,
        limit: u64,
    },

    #[error("{path} could not be read as an image: {message}")]
    Undecodable { path: NodePath, message: String },
}

/// One thumbnail, as `files.thumbnail` returns it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Thumbnail {
    pub path: NodePath,
    /// `image/jpeg` for an opaque picture, `image/png` for one with
    /// transparency, which JPEG would fill with black.
    pub mime: String,
    /// The encoded picture, standard padded base64: ready for a `data:` URL.
    pub content: String,
    pub width: u32,
    pub height: u32,
    /// Whether it came from the cache rather than from decoding the file.
    pub cached: bool,
}

/// The thumbnail cache directory and its byte cap.
pub struct ThumbnailCache {
    dir: PathBuf,
    cap: u64,
    /// What the directory is believed to hold, in bytes. `None` until the
    /// first write scans it. A belief, not a ledger: the operator may delete
    /// files under it at any time, and the next eviction rescans.
    used: Mutex<Option<u64>>,
}

impl ThumbnailCache {
    pub fn new(dir: PathBuf, cap: u64) -> ThumbnailCache {
        ThumbnailCache {
            dir,
            cap,
            used: Mutex::new(None),
        }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn entry(&self, key: &str) -> PathBuf {
        self.dir.join(key)
    }

    /// A cached picture, and its last-used time moved to now so eviction
    /// keeps what is being looked at.
    ///
    /// The entry is opened without following a symlink: one planted under an
    /// entry's name would otherwise hand out, and touch, a file outside the
    /// working directory.
    fn get(&self, key: &str) -> Option<Vec<u8>> {
        use std::io::Read as _;
        self.check_dirs(false).ok()?;
        let mut options = std::fs::File::options();
        options.read(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.custom_flags(libc::O_NOFOLLOW);
        }
        let mut file = options.open(self.entry(key)).ok()?;
        if !file.metadata().ok()?.is_file() {
            return None;
        }
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes).ok()?;
        let _ = file.set_modified(SystemTime::now());
        Some(bytes)
    }

    /// Refuse a cache directory, or its parent, that is a symlink or not a
    /// directory. With `must_exist` false a missing one passes, because it is
    /// about to be created.
    fn check_dirs(&self, must_exist: bool) -> io::Result<()> {
        for dir in [self.dir.parent(), Some(self.dir.as_path())]
            .into_iter()
            .flatten()
        {
            match std::fs::symlink_metadata(dir) {
                Ok(meta) if meta.file_type().is_symlink() || !meta.is_dir() => {
                    return Err(io::Error::other(format!(
                        "{} is a symlink or not a directory; it is not used as a cache",
                        dir.display()
                    )));
                }
                Ok(_) => {}
                Err(e) if !must_exist && e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }

    /// Store a picture. Failure is the caller's to log, never to report: a
    /// cache that cannot be written is a slower Files, not a broken one.
    fn put(&self, key: &str, bytes: &[u8]) -> io::Result<()> {
        use std::io::Write as _;
        self.check_dirs(false)?;
        create_private_dir(&self.dir)?;
        self.check_dirs(true)?;
        let mut tmp = tempfile::NamedTempFile::new_in(&self.dir)?;
        tmp.write_all(bytes)?;
        tmp.persist(self.entry(key)).map_err(|e| e.error)?;

        let mut used = self.used.lock().unwrap_or_else(|e| e.into_inner());
        let now = match *used {
            Some(n) => n + bytes.len() as u64,
            None => self.scan().iter().map(|(_, len, _)| len).sum(),
        };
        *used = Some(if now > self.cap { self.evict() } else { now });
        Ok(())
    }

    /// Every entry: path, length, last used. Only regular files named like
    /// an entry count: anything else in the directory is not the cache's to
    /// measure or delete.
    fn scan(&self) -> Vec<(PathBuf, u64, SystemTime)> {
        if self.check_dirs(true).is_err() {
            return Vec::new();
        }
        let Ok(dir) = std::fs::read_dir(&self.dir) else {
            return Vec::new();
        };
        dir.filter_map(Result::ok)
            .filter(|e| is_entry_name(&e.file_name().to_string_lossy()))
            .filter_map(|e| {
                // Not following symlinks: a link is never an entry.
                let meta = e.metadata().ok()?;
                meta.is_file()
                    .then(|| (e.path(), meta.len(), meta.modified().unwrap_or(UNIX_EPOCH)))
            })
            .collect()
    }

    /// Remove the least recently used entries until the cache is at three
    /// quarters of its cap, so the next few writes do not each evict again.
    /// Returns what is left.
    fn evict(&self) -> u64 {
        let mut entries = self.scan();
        let mut total: u64 = entries.iter().map(|(_, len, _)| len).sum();
        entries.sort_by_key(|(_, _, used)| *used);
        let target = self.cap / 4 * 3;
        for (path, len, _) in entries {
            if total <= target {
                break;
            }
            if std::fs::remove_file(&path).is_ok() {
                total -= len;
            }
        }
        total
    }
}

/// Whether `name` is one the cache writes: a SHA-256 in lowercase hex.
fn is_entry_name(name: &str) -> bool {
    name.len() == 64 && name.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

fn create_private_dir(dir: &Path) -> io::Result<()> {
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt as _;
        builder.mode(0o700);
    }
    builder.create(dir)
}

/// Makes thumbnails: the namespace they come from and the cache they go to.
///
/// Both fields are required, so there is no way to build one that cannot
/// reach either.
pub struct Thumbnailer {
    files: Arc<Namespace>,
    cache: ThumbnailCache,
    budget: DecodeBudget,
}

impl Thumbnailer {
    pub fn new(files: Arc<Namespace>, cache: ThumbnailCache) -> Thumbnailer {
        Thumbnailer {
            files,
            cache,
            budget: DecodeBudget::new(DEFAULT_DECODE_BUDGET),
        }
    }

    /// The same, with a decode memory budget other than the default.
    pub fn with_decode_budget(self, bytes: u64) -> Thumbnailer {
        Thumbnailer {
            budget: DecodeBudget::new(bytes),
            ..self
        }
    }

    pub fn cache(&self) -> &ThumbnailCache {
        &self.cache
    }

    /// A thumbnail of `address` no larger than `size` on either edge.
    /// Blocking: decoding is CPU work and reading is disk work.
    pub fn make(&self, address: &NodePath, size: Option<u32>) -> Result<Thumbnail, ThumbnailError> {
        let edge = bucket(size);
        let resolved = self.files.resolve(address)?;
        let path = self.files.qualify(resolved.path());
        let name = resolved.path().name().unwrap_or_default();
        if !is_thumbnailable(name) {
            return Err(ThumbnailError::NotAnImage { path });
        }
        // A picture of the thumbnail cache, when the working directory is
        // the install directory and the cache is therefore listable, would
        // add an entry to the cache for every entry in it.
        if self.inside_cache(resolved.real_path()) {
            return Err(ThumbnailError::NotAnImage { path });
        }

        let before = Stamp::of(self.files.root(), resolved.real_path(), &path)?;
        if before.len > MAX_SOURCE {
            return Err(ThumbnailError::TooLarge {
                path,
                len: before.len,
                limit: MAX_SOURCE,
            });
        }
        let key = before.key(&path, edge);
        if let Some(bytes) = self.cache.get(&key)
            && let Ok(thumb) = describe(&path, &bytes, true)
        {
            return Ok(thumb);
        }

        let source = self.files.read(address, MAX_SOURCE)?;
        let encoded = render(&path, &source, edge, &self.budget)?;
        // Cached only if the file did not change while it was being read, or
        // the entry would carry the new picture under the old key.
        if Stamp::of(self.files.root(), resolved.real_path(), &path)
            .ok()
            .as_ref()
            == Some(&before)
            && let Err(e) = self.cache.put(&key, &encoded)
        {
            tracing::warn!(
                cache = %self.cache.dir.display(),
                error = %e,
                "cannot write a thumbnail to the cache; it will be made again next time"
            );
        }
        describe(&path, &encoded, false)
    }

    fn inside_cache(&self, real: &Path) -> bool {
        std::fs::canonicalize(&self.cache.dir).is_ok_and(|cache| real.starts_with(cache))
    }
}

/// What identifies one version of a file for the cache.
#[derive(Debug, PartialEq, Eq)]
struct Stamp {
    len: u64,
    modified_ns: u128,
}

impl Stamp {
    fn of(root: &Root, real: &Path, path: &NodePath) -> Result<Stamp, FileError> {
        let meta = std::fs::metadata(real).map_err(|source| root.map_io(&path.path, source))?;
        if !meta.is_file() {
            return Err(FileError::NotAFile {
                path: path.path.clone(),
            });
        }
        let modified_ns = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        Ok(Stamp {
            len: meta.len(),
            modified_ns,
        })
    }

    fn key(&self, path: &NodePath, edge: u32) -> String {
        let mut hash = Sha256::new();
        // The version is in the key so a change to how thumbnails are made
        // retires every old one without anyone deleting anything.
        hash.update(b"thumbnail-v1\0");
        hash.update(path.to_string().as_bytes());
        hash.update(b"\0");
        hash.update(self.modified_ns.to_le_bytes());
        hash.update(self.len.to_le_bytes());
        hash.update(edge.to_le_bytes());
        let digest = hash.finalize();
        digest.iter().map(|b| format!("{b:02x}")).collect()
    }
}

/// Memory every decode in flight shares: a count of reserved bytes, and a
/// wait for room.
struct DecodeBudget {
    cap: u64,
    used: Mutex<u64>,
    freed: std::sync::Condvar,
}

/// Bytes held from a [`DecodeBudget`] until dropped.
struct Reservation<'a> {
    budget: &'a DecodeBudget,
    bytes: u64,
}

impl DecodeBudget {
    fn new(cap: u64) -> DecodeBudget {
        DecodeBudget {
            cap,
            used: Mutex::new(0),
            freed: std::sync::Condvar::new(),
        }
    }

    /// Wait until `bytes` fit, then hold them. `None` if they never could.
    fn reserve(&self, bytes: u64) -> Option<Reservation<'_>> {
        if bytes > self.cap {
            return None;
        }
        let mut used = self.used.lock().unwrap_or_else(|e| e.into_inner());
        while *used + bytes > self.cap {
            used = self.freed.wait(used).unwrap_or_else(|e| e.into_inner());
        }
        *used += bytes;
        Some(Reservation {
            budget: self,
            bytes,
        })
    }

    /// Hold `bytes` if they fit now, without waiting.
    #[cfg(test)]
    fn try_reserve(&self, bytes: u64) -> Option<Reservation<'_>> {
        let mut used = self.used.lock().unwrap_or_else(|e| e.into_inner());
        if *used + bytes > self.cap {
            return None;
        }
        *used += bytes;
        Some(Reservation {
            budget: self,
            bytes,
        })
    }
}

impl Drop for Reservation<'_> {
    fn drop(&mut self) {
        let mut used = self.budget.used.lock().unwrap_or_else(|e| e.into_inner());
        *used -= self.bytes;
        self.budget.freed.notify_all();
    }
}

/// Decode, shrink, orient and encode.
fn render(
    path: &NodePath,
    source: &[u8],
    edge: u32,
    budget: &DecodeBudget,
) -> Result<Vec<u8>, ThumbnailError> {
    use image::{DynamicImage, ImageDecoder as _, ImageFormat, ImageReader, Limits};

    let undecodable = |e: &dyn std::fmt::Display| ThumbnailError::Undecodable {
        path: path.clone(),
        message: e.to_string(),
    };

    let mut reader = ImageReader::new(Cursor::new(source))
        .with_guessed_format()
        .map_err(|e| undecodable(&e))?;
    let mut limits = Limits::default();
    limits.max_image_width = Some(MAX_DIMENSION);
    limits.max_image_height = Some(MAX_DIMENSION);
    limits.max_alloc = Some(MAX_DECODE_ALLOC);
    reader.limits(limits);

    let mut decoder = reader.into_decoder().map_err(|e| undecodable(&e))?;
    // A phone stores a portrait photo sideways and says so in its EXIF; a
    // thumbnail that ignores that shows every portrait lying down.
    let orientation = decoder.orientation().ok();
    // Held until the full-size picture is gone, at the end of this function.
    let needed = decoder.total_bytes();
    let _held = budget
        .reserve(needed)
        .ok_or_else(|| ThumbnailError::TooManyPixels {
            path: path.clone(),
            needed,
            limit: budget.cap,
        })?;
    let mut picture = DynamicImage::from_decoder(decoder).map_err(|e| undecodable(&e))?;

    // Never enlarged: a 64-pixel icon stays 64 pixels.
    if picture.width() > edge || picture.height() > edge {
        picture = picture.thumbnail(edge, edge);
    }
    // Oriented after shrinking, so the rotation copies a thumbnail rather
    // than a second full-size picture. The result is the same: the bound is
    // square, so it fits either way round.
    if let Some(orientation) = orientation {
        picture.apply_orientation(orientation);
    }

    let mut out = Cursor::new(Vec::new());
    if picture.color().has_alpha() {
        picture
            .write_to(&mut out, ImageFormat::Png)
            .map_err(|e| undecodable(&e))?;
    } else {
        let rgb = DynamicImage::ImageRgb8(picture.to_rgb8());
        let encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, 82);
        rgb.write_with_encoder(encoder)
            .map_err(|e| undecodable(&e))?;
    }
    Ok(out.into_inner())
}

/// The response for encoded bytes, from the cache or freshly made. The size
/// is read back from the encoded header, so the two cannot disagree.
fn describe(path: &NodePath, bytes: &[u8], cached: bool) -> Result<Thumbnail, ThumbnailError> {
    let reader = image::ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|e| ThumbnailError::Undecodable {
            path: path.clone(),
            message: e.to_string(),
        })?;
    let mime = match reader.format() {
        Some(image::ImageFormat::Png) => "image/png",
        Some(image::ImageFormat::Jpeg) => "image/jpeg",
        _ => {
            return Err(ThumbnailError::Undecodable {
                path: path.clone(),
                message: "the cached thumbnail is neither PNG nor JPEG".into(),
            });
        }
    };
    let (width, height) = reader
        .into_dimensions()
        .map_err(|e| ThumbnailError::Undecodable {
            path: path.clone(),
            message: e.to_string(),
        })?;
    Ok(Thumbnail {
        path: path.clone(),
        mime: mime.to_string(),
        content: base64::engine::general_purpose::STANDARD.encode(bytes),
        width,
        height,
        cached,
    })
}

// ---------------------------------------------------------------------------
// The command
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ThumbnailParams {
    /// The image file.
    pub path: NodePath,
    /// The longest edge wanted, in pixels. Rounded up to 128, 256 or 512;
    /// 256 when left out.
    #[serde(default)]
    pub size: Option<u32>,
}

pub struct MakeThumbnail {
    thumbnailer: Arc<Thumbnailer>,
    /// How many decodes may run at once.
    permits: Arc<tokio::sync::Semaphore>,
}

impl MakeThumbnail {
    pub fn new(thumbnailer: Arc<Thumbnailer>) -> MakeThumbnail {
        let cores = std::thread::available_parallelism().map_or(2, |n| n.get());
        MakeThumbnail {
            thumbnailer,
            permits: Arc::new(tokio::sync::Semaphore::new((cores / 2).clamp(1, 4))),
        }
    }
}

fn to_command_error(command: CommandId, error: ThumbnailError) -> CommandError {
    match error {
        ThumbnailError::File(e) => crate::files::to_command_error(command, e),
        // Not this node failing: a kind of file there is no thumbnail for.
        // The UI shows the file's icon, and the bus has a word for that.
        e @ (ThumbnailError::NotAnImage { .. }
        | ThumbnailError::TooLarge { .. }
        | ThumbnailError::TooManyPixels { .. }) => CommandError::Unsupported {
            command,
            reason: e.to_string(),
        },
        e @ ThumbnailError::Undecodable { .. } => CommandError::Failed(anyhow::Error::new(e)),
    }
}

impl Command for MakeThumbnail {
    const ID: &'static str = "files.thumbnail";
    const TITLE: &'static str = "Thumbnail";
    /// Read: it reads one file inside the working directory. What it writes
    /// is a cache nothing depends on. By default that cache is inside the
    /// working directory (working directory = install directory), which is
    /// why [`ThumbnailCache`] refuses a symlinked directory, never follows a
    /// symlinked entry and evicts only its own entry names: a ReadOnly
    /// request must not be able to delete or read anything it did not write.
    const RISK: RiskLevel = RiskLevel::ReadOnly;

    type Params = ThumbnailParams;
    type Output = Thumbnail;

    async fn run(&self, params: ThumbnailParams, cx: Context) -> Result<Thumbnail, CommandError> {
        // Held across the decode. A closed semaphore cannot happen: nothing
        // closes it.
        let _permit = Arc::clone(&self.permits)
            .acquire_owned()
            .await
            .map_err(|e| CommandError::Failed(anyhow::anyhow!("{e}")))?;
        let thumbnailer = Arc::clone(&self.thumbnailer);
        let command = cx.command.clone();
        match tokio::task::spawn_blocking(move || thumbnailer.make(&params.path, params.size)).await
        {
            Ok(result) => result.map_err(|e| to_command_error(command, e)),
            Err(joined) => Err(CommandError::Failed(anyhow::anyhow!(
                "making the thumbnail did not finish: {joined}"
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_decode_waits_while_others_hold_the_budget_and_goes_when_they_finish() {
        let budget = Arc::new(DecodeBudget::new(400));
        let first = budget.try_reserve(300).unwrap();
        assert!(budget.try_reserve(200).is_none(), "over the budget");

        let started = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let waiter = {
            let budget = Arc::clone(&budget);
            let started = Arc::clone(&started);
            std::thread::spawn(move || {
                let _held = budget.reserve(200).unwrap();
                started.store(true, std::sync::atomic::Ordering::SeqCst);
            })
        };
        std::thread::sleep(std::time::Duration::from_millis(100));
        assert!(
            !started.load(std::sync::atomic::Ordering::SeqCst),
            "a decode went ahead while the budget was spent"
        );
        drop(first);
        waiter.join().unwrap();
        assert!(started.load(std::sync::atomic::Ordering::SeqCst));
        // Both released: the whole budget is free again.
        assert!(budget.try_reserve(400).is_some());
    }

    #[test]
    fn a_decode_larger_than_the_whole_budget_is_refused_not_left_waiting() {
        let budget = DecodeBudget::new(400);
        assert!(budget.reserve(401).is_none());
        assert!(budget.reserve(400).is_some());
    }
}
