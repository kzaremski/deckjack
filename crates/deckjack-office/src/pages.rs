//! Pages: a document laid out by LibreOffice as a PDF, then one page at a
//! time rasterized by poppler's `pdftoppm`.
//!
//! The PDF is cached under the app's private `HOME`, keyed by the SHA-256 of
//! the document's bytes and the engine's version, so a page view opened
//! twice converts once, and a LibreOffice upgrade (which changes layout)
//! renders afresh. Page images are cached beside it by page and width.
//! Everything here is safe to delete.
//!
//! `pdftoppm` and `pdfinfo` run as children of this app, so the sandbox
//! confines them too, with the same wall-clock limit as a conversion.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::Serialize;
use tokio::sync::watch;

use crate::worker::{Engine, JobError};

pub const PDFTOPPM: &str = "/usr/bin/pdftoppm";
pub const PDFINFO: &str = "/usr/bin/pdfinfo";

/// Narrowest and widest page image, in pixels. The top is what a phone at
/// 3x needs zoomed in twice on a 400 px column, and it keeps a dense page's
/// JPEG well under the 8 MiB frame.
pub const MIN_WIDTH: u32 = 200;
pub const MAX_WIDTH: u32 = 2400;

/// The cache's ceiling. Oldest documents go first.
pub const CACHE_BYTES: u64 = 1 << 30;

const TOOL_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Debug, thiserror::Error)]
pub enum PageError {
    #[error(transparent)]
    Job(#[from] JobError),
    #[error("{0} is not installed on this node, so pages cannot be drawn")]
    MissingTool(&'static str),
    #[error("{tool} failed: {reason}")]
    Tool { tool: &'static str, reason: String },
    #[error("there is no page {page}; the document has {pages}")]
    NoPage { page: u32, pages: u32 },
    #[error("{0}")]
    Io(#[from] std::io::Error),
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct PageSize {
    /// Points (1/72 inch).
    pub width: f32,
    pub height: f32,
}

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, serde::Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "lowercase")]
pub enum ImageFormat {
    Jpeg,
    Png,
}

impl ImageFormat {
    fn ext(self) -> &'static str {
        match self {
            ImageFormat::Jpeg => "jpg",
            ImageFormat::Png => "png",
        }
    }
    pub fn mime(self) -> &'static str {
        match self {
            ImageFormat::Jpeg => "image/jpeg",
            ImageFormat::Png => "image/png",
        }
    }
}

pub struct Renders {
    pub root: PathBuf,
    pub engine: Arc<Engine>,
    pub version: String,
    /// One conversion per document at a time: a page view asks for several
    /// pages at once, and each must not start its own LibreOffice.
    locks: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
}

impl Renders {
    pub fn new(root: PathBuf, engine: Arc<Engine>, version: String) -> Renders {
        Renders {
            root,
            engine,
            version,
            locks: Mutex::new(HashMap::new()),
        }
    }

    /// The cache key for a document's bytes on this engine.
    pub async fn key(&self, source: &Path) -> Result<String, PageError> {
        let source = source.to_path_buf();
        let digest = tokio::task::spawn_blocking(move || -> std::io::Result<String> {
            use sha2::{Digest as _, Sha256};
            use std::io::Read as _;
            let mut file = std::fs::File::open(&source)?;
            let mut hash = Sha256::new();
            let mut buf = vec![0u8; 1 << 16];
            loop {
                let n = file.read(&mut buf)?;
                if n == 0 {
                    break;
                }
                hash.update(&buf[..n]);
            }
            Ok(hash
                .finalize()
                .iter()
                .take(16)
                .map(|b| format!("{b:02x}"))
                .collect())
        })
        .await
        .map_err(|e| std::io::Error::other(e.to_string()))??;
        Ok(format!("{digest}-{}", self.version))
    }

    /// The document as a PDF, converting it if this engine has not yet.
    pub async fn pdf(
        &self,
        source: &Path,
        cancel: watch::Receiver<bool>,
    ) -> Result<(String, PathBuf), PageError> {
        let key = self.key(source).await?;
        let dir = self.root.join(&key);
        let pdf = dir.join("document.pdf");
        let lock = {
            let mut locks = self
                .locks
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            Arc::clone(locks.entry(key.clone()).or_default())
        };
        let _held = lock.lock().await;
        if pdf.is_file() {
            touch(&dir);
            return Ok((key, pdf));
        }
        std::fs::create_dir_all(&dir)?;
        let is_pdf = source
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| e.eq_ignore_ascii_case("pdf"));
        let partial = dir.join("document.pdf.partial");
        if is_pdf {
            std::fs::copy(source, &partial)?;
        } else {
            self.engine.convert(source, "pdf", &partial, cancel).await?;
        }
        std::fs::rename(&partial, &pdf)?;
        self.prune(&key);
        Ok((key, pdf))
    }

    /// Page sizes, in points, in order.
    pub async fn layout(&self, pdf: &Path) -> Result<Vec<PageSize>, PageError> {
        let info = run_tool(
            PDFINFO,
            "pdfinfo",
            &[
                "-f".into(),
                "1".into(),
                "-l".into(),
                "100000".into(),
                path_arg(pdf),
            ],
            &self.engine,
        )
        .await?;
        Ok(parse_pdfinfo(&String::from_utf8_lossy(&info)))
    }

    /// One page as an image, `width` pixels wide.
    pub async fn page(
        &self,
        key: &str,
        pdf: &Path,
        page: u32,
        width: u32,
        format: ImageFormat,
    ) -> Result<Vec<u8>, PageError> {
        let width = width.clamp(MIN_WIDTH, MAX_WIDTH);
        let dir = self.root.join(key);
        let out = dir.join(format!("p{page}-w{width}.{}", format.ext()));
        if let Ok(bytes) = std::fs::read(&out) {
            touch(&dir);
            return Ok(bytes);
        }
        let prefix = dir.join(format!("p{page}-w{width}-partial"));
        let mut args = vec![
            "-f".to_string(),
            page.to_string(),
            "-l".to_string(),
            page.to_string(),
            "-scale-to-x".to_string(),
            width.to_string(),
            "-scale-to-y".to_string(),
            "-1".to_string(),
            "-singlefile".to_string(),
        ];
        match format {
            ImageFormat::Jpeg => {
                args.push("-jpeg".into());
                args.push("-jpegopt".into());
                args.push("quality=82".into());
            }
            ImageFormat::Png => args.push("-png".into()),
        }
        args.push(path_arg(pdf));
        args.push(path_arg(&prefix));
        run_tool(PDFTOPPM, "pdftoppm", &args, &self.engine).await?;
        let made = dir.join(format!("p{page}-w{width}-partial.{}", format.ext()));
        std::fs::rename(&made, &out)?;
        Ok(std::fs::read(&out)?)
    }

    /// Keep the cache under its ceiling, oldest document first, never the
    /// one just made.
    fn prune(&self, keep: &str) {
        let Ok(entries) = std::fs::read_dir(&self.root) else {
            return;
        };
        let mut dirs: Vec<(std::time::SystemTime, u64, PathBuf)> = entries
            .flatten()
            .filter(|e| e.file_name() != keep)
            .filter_map(|e| {
                let meta = e.metadata().ok()?;
                let size: u64 = std::fs::read_dir(e.path())
                    .ok()?
                    .flatten()
                    .filter_map(|f| f.metadata().ok().map(|m| m.len()))
                    .sum();
                Some((meta.modified().ok()?, size, e.path()))
            })
            .collect();
        let mut total: u64 = dirs.iter().map(|d| d.1).sum();
        dirs.sort();
        for (_, size, path) in dirs {
            if total <= CACHE_BYTES {
                break;
            }
            if std::fs::remove_dir_all(&path).is_ok() {
                total = total.saturating_sub(size);
            }
        }
    }
}

fn touch(dir: &Path) {
    // The directory's mtime is the cache's recency; creating and removing a
    // marker updates it without a utime call.
    let marker = dir.join(".used");
    let _ = std::fs::write(&marker, b"");
    let _ = std::fs::remove_file(&marker);
}

fn path_arg(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

/// Run a poppler tool confined (it inherits this app's sandbox), with a
/// clean environment and a wall-clock limit.
async fn run_tool(
    program: &'static str,
    name: &'static str,
    args: &[String],
    engine: &Engine,
) -> Result<Vec<u8>, PageError> {
    if !Path::new(program).is_file() {
        return Err(PageError::MissingTool(name));
    }
    let child = tokio::process::Command::new(program)
        .args(args)
        .env_clear()
        .env("HOME", &engine.home)
        .env("TMPDIR", &engine.tmp)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;
    let output = match tokio::time::timeout(TOOL_TIMEOUT, child.wait_with_output()).await {
        Ok(output) => output?,
        Err(_) => {
            return Err(PageError::Tool {
                tool: name,
                reason: format!("it took longer than {TOOL_TIMEOUT:?} and was stopped"),
            });
        }
    };
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(PageError::Tool {
            tool: name,
            reason: stderr.lines().last().unwrap_or("it failed").to_string(),
        });
    }
    Ok(output.stdout)
}

/// `Page    3 size: 595.304 x 841.89 pts (A4)` lines, in page order.
pub fn parse_pdfinfo(text: &str) -> Vec<PageSize> {
    let mut sized: Vec<(u32, PageSize)> = Vec::new();
    let mut single: Option<PageSize> = None;
    let mut pages: Option<u32> = None;
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("Pages:") {
            pages = rest.trim().parse().ok();
        } else if let Some(rest) = line.strip_prefix("Page size:") {
            single = parse_size(rest);
        } else if let Some(rest) = line.strip_prefix("Page") {
            let rest = rest.trim_start();
            if let Some((num, size)) = rest.split_once(" size:")
                && let (Ok(n), Some(s)) = (num.trim().parse::<u32>(), parse_size(size))
            {
                sized.push((n, s));
            }
        }
    }
    if !sized.is_empty() {
        sized.sort_by_key(|(n, _)| *n);
        return sized.into_iter().map(|(_, s)| s).collect();
    }
    match (pages, single) {
        (Some(n), Some(s)) => vec![s; n.min(100_000) as usize],
        _ => Vec::new(),
    }
}

fn parse_size(text: &str) -> Option<PageSize> {
    let mut parts = text.split_whitespace();
    let width: f32 = parts.next()?.parse().ok()?;
    if parts.next()? != "x" {
        return None;
    }
    let height: f32 = parts.next()?.parse().ok()?;
    (width > 0.0 && height > 0.0).then_some(PageSize { width, height })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn page_sizes_are_read_from_pdfinfo_per_page_and_for_a_single_page() {
        let many = "Pages:           3\nPage    1 size: 595.304 x 841.89 pts (A4)\nPage    2 size: 841.89 x 595.304 pts\nPage    3 size: 612 x 792 pts (letter)\n";
        let sizes = parse_pdfinfo(many);
        assert_eq!(sizes.len(), 3);
        assert_eq!(
            sizes[1],
            PageSize {
                width: 841.89,
                height: 595.304
            }
        );
        let one = "Pages:           2\nPage size:       793.757 x 446.513 pts\n";
        assert_eq!(
            parse_pdfinfo(one),
            vec![
                PageSize {
                    width: 793.757,
                    height: 446.513
                };
                2
            ]
        );
        assert!(parse_pdfinfo("garbage").is_empty());
    }
}
