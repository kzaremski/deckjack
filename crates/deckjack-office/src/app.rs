//! The commands, as the bus sees them. Each is a thin seam over the engine
//! (`worker`, `pages`) or the OOXML code (`ooxml`); the rules live there.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use base64::Engine as _;
use deckjack_app::Ctx;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::watch;

use crate::ooxml::docx::Docx;
use crate::ooxml::{patch, text};
use crate::pages::{ImageFormat, Renders};
use crate::paths::{self, Folders, PathError};
use crate::worker::{self, Engine, Limits};

/// Word packages: the files the reading view and comments work on.
pub const WORD_EXTS: &[&str] = &["docx", "docm"];

/// The largest document Office opens, any kind.
pub const MAX_DOCUMENT: u64 = 256 << 20;

/// The largest result a command returns, serialized. The core reads an app
/// frame of at most [`deckjack_protocol::MAX_FRAME_BYTES`] and drops the
/// whole connection, every call in flight with it, on a longer one; the
/// headroom is for the frame around the result. A result over this is
/// turned into an error that says what to ask for instead.
pub const MAX_RESULT_BYTES: usize = deckjack_protocol::MAX_FRAME_BYTES - (64 << 10);

/// `value`, if it serializes within [`MAX_RESULT_BYTES`]; otherwise an
/// error naming its size and `instead`.
pub fn bounded(value: Value, instead: &str) -> Result<Value, String> {
    struct Count(usize);
    impl std::io::Write for Count {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0 += buf.len();
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut count = Count(0);
    serde_json::to_writer(&mut count, &value).map_err(err)?;
    if count.0 > MAX_RESULT_BYTES {
        return Err(format!(
            "the result is {} bytes, over the {MAX_RESULT_BYTES}-byte limit for one reply; {instead}",
            count.0
        ));
    }
    Ok(value)
}

pub struct Office {
    pub folders: Folders,
    pub renders: Renders,
    jobs: Mutex<HashMap<String, watch::Sender<bool>>>,
}

impl Office {
    pub fn new(folders: Folders, engine: Engine) -> Office {
        let version = worker::engine_version(&engine.install);
        let root = engine.home.join("renders");
        Office {
            folders,
            renders: Renders::new(root, Arc::new(engine), version),
            jobs: Mutex::new(HashMap::new()),
        }
    }

    /// Built from the process the core launched: the working directory is
    /// the cwd, `HOME` and `TMPDIR` are the app's private ones, and this
    /// binary re-executes itself as the worker.
    pub fn from_env(folder_names: Vec<String>) -> anyhow::Result<Office> {
        let root = std::env::current_dir()?;
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        let tmp = std::env::var_os("TMPDIR")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        let exe = std::env::current_exe()?;
        let install = [
            crate::lok::DEFAULT_INSTALL,
            "/usr/lib64/libreoffice/program",
        ]
        .iter()
        .map(PathBuf::from)
        .find(|p| p.join(crate::lok::LIBRARY).is_file())
        .unwrap_or_else(|| PathBuf::from(crate::lok::DEFAULT_INSTALL));
        let engine = Engine::new(install, exe, home, tmp, Limits::default());
        Ok(Office::new(Folders::new(root, folder_names), engine))
    }

    /// A cancel signal for `job`, registered so `office.cancel` can fire it.
    fn job(&self, job: Option<&str>) -> (watch::Receiver<bool>, Option<JobGuard<'_>>) {
        let (tx, rx) = watch::channel(false);
        let Some(id) = job.filter(|j| !j.is_empty() && j.len() <= 64) else {
            return (rx, None);
        };
        self.lock_jobs().insert(id.to_string(), tx);
        (
            rx,
            Some(JobGuard {
                office: self,
                id: id.to_string(),
            }),
        )
    }

    fn lock_jobs(&self) -> std::sync::MutexGuard<'_, HashMap<String, watch::Sender<bool>>> {
        self.jobs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    pub fn cancel(&self, job: &str) -> bool {
        match self.lock_jobs().get(job) {
            Some(tx) => tx.send(true).is_ok(),
            None => false,
        }
    }
}

struct JobGuard<'a> {
    office: &'a Office,
    id: String,
}

impl Drop for JobGuard<'_> {
    fn drop(&mut self) {
        self.office.lock_jobs().remove(&self.id);
    }
}

fn err(e: impl std::fmt::Display) -> String {
    e.to_string()
}

fn ext_of(path: &Path) -> String {
    path.extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase()
}

/// What kind of document a name is, for the UI and for agents.
pub fn kind_of(path: &Path) -> &'static str {
    match ext_of(path).as_str() {
        "docx" | "docm" | "dotx" | "doc" | "odt" | "rtf" | "wpd" | "fodt" => "document",
        "xlsx" | "xlsm" | "xls" | "ods" | "csv" | "fods" => "spreadsheet",
        "pptx" | "pptm" | "ppt" | "odp" | "fodp" | "key" => "presentation",
        "pdf" => "pdf",
        "odg" | "vsd" | "vsdx" => "drawing",
        _ => "other",
    }
}

fn check_size(path: &Path, address: &str) -> Result<u64, String> {
    let size = std::fs::metadata(path).map_err(err)?.len();
    if size > MAX_DOCUMENT {
        return Err(format!(
            "{address} is {size} bytes; Office opens documents up to {MAX_DOCUMENT} bytes"
        ));
    }
    Ok(size)
}

async fn read_docx(path: PathBuf) -> Result<Docx, String> {
    tokio::task::spawn_blocking(move || {
        let bytes = std::fs::read(&path).map_err(err)?;
        Docx::open(bytes).map_err(err)
    })
    .await
    .map_err(err)?
}

// ---------------------------------------------------------------------------
// Parameters
// ---------------------------------------------------------------------------

#[derive(Deserialize, schemars::JsonSchema)]
pub struct PathParams {
    /// A working-directory path inside Documents/ or Downloads/, e.g.
    /// `/Documents/Report.docx`.
    pub path: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct PagesParams {
    pub path: String,
    /// A name for this job, so `office.cancel` can stop it.
    #[serde(default)]
    pub job: Option<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct RenderParams {
    pub path: String,
    /// 1-based.
    pub page: u32,
    /// Pixels; clamped to 200-2400.
    pub width: u32,
    #[serde(default)]
    pub format: Option<ImageFormat>,
    #[serde(default)]
    pub job: Option<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct TextParams {
    /// A working-directory path to a Word document.
    pub path: String,
    /// Only the CriticMarkup text (`text`), or only the structure the
    /// reading view draws (`blocks`). Both when left out; a long document
    /// may only fit one reply as one or the other.
    #[serde(default)]
    pub only: Option<TextPart>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TextPart {
    Text,
    Blocks,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct CancelParams {
    pub job: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct CommentParams {
    /// The Word document to comment on.
    pub path: String,
    /// The paragraph, as `office.text` names it: `p/12`. Not needed for a
    /// reply.
    #[serde(default)]
    pub anchor: Option<String>,
    /// UTF-16 offsets into that paragraph's text, as `office.text`'s blocks
    /// give it; `end` is exclusive. Not needed for a reply.
    #[serde(default)]
    pub start: Option<usize>,
    #[serde(default)]
    pub end: Option<usize>,
    pub text: String,
    pub author: String,
    /// Reply to this comment id instead of anchoring a new one.
    #[serde(default)]
    pub reply_to: Option<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct SaveInPlaceParams {
    /// The commented copy `office.comment.add` wrote.
    pub prepared: String,
    /// The name it should take. Must not exist: move the original to the
    /// trash first (`files.trash`).
    pub path: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct ExportParams {
    pub path: String,
    #[serde(default)]
    pub job: Option<String>,
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

impl Office {
    pub async fn open(&self, p: PathParams) -> Result<Value, String> {
        let file = self.folders.file(&p.path).map_err(err)?;
        let size = check_size(&file.path, &file.address)?;
        let word = WORD_EXTS.contains(&ext_of(&file.path).as_str());
        let (report, comment) = if word {
            match read_docx(file.path.clone()).await {
                Ok(doc) => {
                    let can = !doc.report.signed && !doc.report.strict;
                    let note = if doc.report.signed {
                        Some(
                            "This document is signed; adding a comment would break the signature, so commenting is off.",
                        )
                    } else if doc.report.strict {
                        Some(
                            "This document is in ISO Strict form; it can be read but not commented on here.",
                        )
                    } else {
                        None
                    };
                    (json!(doc.report), json!({ "ok": can, "note": note }))
                }
                Err(e) => (Value::Null, json!({ "ok": false, "note": e })),
            }
        } else {
            (
                Value::Null,
                json!({ "ok": false, "note": "Comments are written into Word (.docx) files only." }),
            )
        };
        Ok(json!({
            "path": file.address,
            "name": file.path.file_name().map(|n| n.to_string_lossy().into_owned()),
            "kind": kind_of(&file.path),
            "bytes": size,
            "reading": word,
            "engine": self.renders.engine.available(),
            "report": report,
            "comment": comment,
            // Every part a comment does not touch is copied byte for byte.
            "keeps": "A comment rewrites only document.xml, comments.xml and, for a reply, commentsExtended.xml (plus the package's index when the file had no comments). Every other part is copied byte for byte; tracked changes stay pending.",
        }))
    }

    pub async fn pages(&self, p: PagesParams, cx: &Ctx) -> Result<Value, String> {
        let file = self.folders.file(&p.path).map_err(err)?;
        check_size(&file.path, &file.address)?;
        let (cancel, _guard) = self.job(p.job.as_deref());
        cx.progress(None, "Laying out pages");
        let (key, pdf) = self.renders.pdf(&file.path, cancel).await.map_err(err)?;
        let sizes = self.renders.layout(&pdf).await.map_err(err)?;
        Ok(json!({ "path": file.address, "key": key, "pages": sizes.len(), "sizes": sizes }))
    }

    pub async fn render(&self, p: RenderParams) -> Result<Value, String> {
        let file = self.folders.file(&p.path).map_err(err)?;
        check_size(&file.path, &file.address)?;
        let (cancel, _guard) = self.job(p.job.as_deref());
        let (key, pdf) = self.renders.pdf(&file.path, cancel).await.map_err(err)?;
        let sizes = self.renders.layout(&pdf).await.map_err(err)?;
        let pages = sizes.len() as u32;
        if p.page == 0 || p.page > pages {
            return Err(crate::pages::PageError::NoPage {
                page: p.page,
                pages,
            }
            .to_string());
        }
        let format = p.format.unwrap_or(ImageFormat::Jpeg);
        // Rounded up to a step so nearby widths share a cached image.
        let width = p
            .width
            .clamp(crate::pages::MIN_WIDTH, crate::pages::MAX_WIDTH)
            .div_ceil(100)
            * 100;
        let width = width.min(crate::pages::MAX_WIDTH);
        let bytes = self
            .renders
            .page(&key, &pdf, p.page, width, format)
            .await
            .map_err(err)?;
        let size = sizes[(p.page - 1) as usize];
        bounded(
            json!({
                "page": p.page,
                "pages": pages,
                "width": width,
                "size": size,
                "mime": format.mime(),
                "data": base64::engine::general_purpose::STANDARD.encode(&bytes),
            }),
            "ask for format \"jpeg\" or a smaller width",
        )
    }

    pub async fn text(&self, p: TextParams) -> Result<Value, String> {
        let file = self.folders.file(&p.path).map_err(err)?;
        check_size(&file.path, &file.address)?;
        if !WORD_EXTS.contains(&ext_of(&file.path).as_str()) {
            return Err(format!(
                "{} is not a Word document; office.text reads .docx files. Use office.export_pdf \
                 for a PDF of other formats",
                file.address
            ));
        }
        let path = file.path.clone();
        let address = file.address.clone();
        // Opening, rendering and serializing are all CPU on untrusted input,
        // so none of it runs on the runtime's own threads.
        tokio::task::spawn_blocking(move || {
            let bytes = std::fs::read(&path).map_err(err)?;
            let doc = Docx::open(bytes).map_err(err)?;
            text_result(&address, &doc, p.only)
        })
        .await
        .map_err(err)?
    }

    pub async fn comment(&self, p: CommentParams) -> Result<Value, String> {
        let file = self.folders.file(&p.path).map_err(err)?;
        check_size(&file.path, &file.address)?;
        if !WORD_EXTS.contains(&ext_of(&file.path).as_str()) {
            return Err(format!(
                "{} is not a Word document; comments are written into .docx files only",
                file.address
            ));
        }
        let new = patch::NewComment {
            anchor: p.anchor.clone().unwrap_or_default(),
            start: p.start.unwrap_or(0),
            end: p.end.unwrap_or(0),
            text: p.text,
            author: p.author,
            date: patch::now_iso(),
            reply_to: p.reply_to,
        };
        if new.reply_to.is_none() && (p.anchor.is_none() || p.start.is_none() || p.end.is_none()) {
            return Err(
                "a new comment needs anchor, start and end (or reply_to for a reply)".into(),
            );
        }
        let source = file.path.clone();
        let dir = source
            .parent()
            .ok_or("the document has no folder")?
            .to_path_buf();
        let name = source
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .ok_or("the document has no name")?;
        let (written, id, changed, added) = tokio::task::spawn_blocking(move || {
            let bytes = std::fs::read(&source).map_err(err)?;
            let mut doc = Docx::open(bytes).map_err(err)?;
            let patched = patch::add_comment(&mut doc, &new).map_err(err)?;
            let written =
                paths::write_first_free(&dir, paths::commented_names(&name), &patched.bytes)
                    .map_err(err)?;
            Ok::<_, String>((written, patched.id, patched.changed, patched.added))
        })
        .await
        .map_err(err)??;
        Ok(json!({
            "path": self.folders.address_of(&written),
            "source": file.address,
            "id": id,
            "changed": changed,
            "added": added,
        }))
    }

    pub async fn save_in_place(&self, p: SaveInPlaceParams) -> Result<Value, String> {
        let prepared = self.folders.file(&p.prepared).map_err(err)?;
        let target = self.folders.resolve(&p.path).map_err(err)?;
        if prepared.path.parent() != target.path.parent() {
            return Err("the commented copy and the original must be in the same folder".into());
        }
        if ext_of(&prepared.path) != ext_of(&target.path) {
            return Err("the commented copy and the original must have the same extension".into());
        }
        if prepared.path == target.path {
            return Err("the copy already has that name".into());
        }
        if target.path.symlink_metadata().is_ok() {
            return Err(PathError::Exists(format!(
                "{} (move the original to the trash first, so the save can be undone)",
                target.address
            ))
            .to_string());
        }
        paths::take_name(&prepared.path, &target.path).map_err(|e| {
            if e.kind() == std::io::ErrorKind::AlreadyExists {
                PathError::Exists(target.address.clone()).to_string()
            } else {
                e.to_string()
            }
        })?;
        Ok(json!({ "path": target.address, "from": prepared.address }))
    }

    pub async fn export_pdf(&self, p: ExportParams, cx: &Ctx) -> Result<Value, String> {
        let file = self.folders.file(&p.path).map_err(err)?;
        check_size(&file.path, &file.address)?;
        if ext_of(&file.path) == "pdf" {
            return Err(format!("{} is already a PDF", file.address));
        }
        let (cancel, _guard) = self.job(p.job.as_deref());
        cx.progress(None, "Exporting with LibreOffice");
        let (_, pdf) = self.renders.pdf(&file.path, cancel).await.map_err(err)?;
        let dir = file
            .path
            .parent()
            .ok_or("the document has no folder")?
            .to_path_buf();
        let name = file
            .path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .ok_or("the document has no name")?;
        let written = tokio::task::spawn_blocking(move || {
            let bytes = std::fs::read(&pdf).map_err(err)?;
            paths::write_first_free(&dir, paths::export_names(&name, "pdf"), &bytes).map_err(err)
        })
        .await
        .map_err(err)??;
        Ok(json!({ "path": self.folders.address_of(&written), "source": file.address }))
    }
}

/// `office.text`'s answer for an opened document, within the reply limit.
pub fn text_result(address: &str, doc: &Docx, only: Option<TextPart>) -> Result<Value, String> {
    let mut out = json!({
        "path": address,
        "comments": doc.comments,
        "anchored": crate::ooxml::parts::anchored_order(doc.blocks()),
        "changes": doc.changes(),
        "report": doc.report,
    });
    if only != Some(TextPart::Blocks) {
        out["text"] = json!(text::render(doc.blocks(), &doc.comments, doc.changes()));
    }
    if only != Some(TextPart::Text) {
        out["blocks"] = json!(doc.blocks());
    }
    let instead = match only {
        None => "ask for only \"text\" or only \"blocks\" (the `only` parameter)",
        Some(_) => "this document is too long to read here; office.export_pdf makes a PDF of it",
    };
    bounded(out, instead)
}
