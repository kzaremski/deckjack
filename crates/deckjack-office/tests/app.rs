//! The office app end to end, confined: launched by the core's own launcher
//! from a real install, and driven over its link with the core's frames.
//! Every command runs in the confined process; conversions run in the
//! worker it spawns, which inherits the confinement.
//!
//! This test plays the core: it takes the core's end of the link the
//! launcher handed the app, says hello, reads the registration and sends
//! calls. Nothing in the app is faked.

mod common;

use std::io::{BufRead as _, BufReader, Write as _};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::{Duration, Instant};

use base64::Engine as _;
use common::{Rig, have_libreoffice};
use deckjack_core::apps::Env as _;
use deckjack_office::ooxml::{docx::Docx, package, patch};
use deckjack_protocol::{AppFrame, CoreFrame, Hello, PROTOCOL_VERSION, decode_frame, encode_frame};
use serde_json::{Value, json};

struct Core {
    reader: BufReader<UnixStream>,
    writer: UnixStream,
    next: u64,
    pid: u32,
}

impl Core {
    fn start(rig: &Rig) -> Core {
        let plan = rig.plan();
        let pid = rig.env.spawn(&plan).expect("the confined app starts");
        // Made by the launcher before the spawn, so it is already here.
        let stream = rig
            .links
            .lock()
            .unwrap()
            .try_recv()
            .expect("the launcher handed the core its end of the app's link");
        stream
            .set_read_timeout(Some(Duration::from_secs(180)))
            .unwrap();
        let writer = stream.try_clone().unwrap();
        let mut core = Core {
            reader: BufReader::new(stream),
            writer,
            next: 0,
            pid,
        };
        core.send(&CoreFrame::Hello(Hello {
            protocol: PROTOCOL_VERSION,
            core_version: "test".into(),
            app: "office".into(),
        }));
        loop {
            match core.frame() {
                AppFrame::Register(r) => {
                    let ids: Vec<&str> = r.commands.iter().map(|c| c.id.as_str()).collect();
                    assert!(ids.contains(&"office.comment.add"), "{ids:?}");
                    break;
                }
                AppFrame::Log { level, message } => {
                    assert_ne!(
                        level, "error",
                        "the app logged an error at startup: {message}"
                    )
                }
                other => panic!("expected a registration, got {other:?}"),
            }
        }
        core
    }

    fn send(&mut self, frame: &CoreFrame) {
        self.writer
            .write_all(encode_frame(frame).unwrap().as_bytes())
            .unwrap();
    }

    fn frame(&mut self) -> AppFrame {
        let mut line = String::new();
        let n = self
            .reader
            .read_line(&mut line)
            .expect("a frame from the app");
        assert!(n > 0, "the app closed its socket");
        decode_frame(line.trim_end()).unwrap()
    }

    /// Call a command and wait for its result, skipping progress and logs.
    fn call(&mut self, command: &str, params: Value) -> Result<Value, String> {
        self.next += 1;
        let id = self.next;
        self.send(&CoreFrame::Call {
            id,
            command: command.into(),
            params,
        });
        loop {
            match self.frame() {
                AppFrame::Result {
                    id: got,
                    result,
                    error,
                } if got == id => {
                    return match error {
                        Some(e) => Err(e),
                        None => Ok(result.unwrap_or(Value::Null)),
                    };
                }
                _ => continue,
            }
        }
    }

    fn stop(mut self, rig: &Rig) {
        self.send(&CoreFrame::Stop);
        let deadline = Instant::now() + Duration::from_secs(10);
        while rig.env.is_running(self.pid) {
            assert!(Instant::now() < deadline, "the app ignored stop");
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

/// Every process whose command line mentions `needle`, by reading /proc.
fn processes_mentioning(needle: &str) -> Vec<u32> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir("/proc").into_iter().flatten().flatten() {
        let Ok(pid) = entry.file_name().to_string_lossy().parse::<u32>() else {
            continue;
        };
        if let Ok(cmd) = std::fs::read(entry.path().join("cmdline"))
            && String::from_utf8_lossy(&cmd).contains(needle)
        {
            out.push(pid);
        }
    }
    out
}

fn find_paragraph(blocks: &Value, needle: &str) -> Option<(String, String)> {
    for block in blocks.as_array()? {
        match block["type"].as_str()? {
            "paragraph" => {
                let text: String = block["inlines"]
                    .as_array()?
                    .iter()
                    .filter_map(|i| i["text"].as_str())
                    .collect();
                if text.contains(needle) {
                    return Some((block["anchor"].as_str()?.to_string(), text));
                }
            }
            "table" => {
                for row in block["rows"].as_array()? {
                    for cell in row.as_array()? {
                        if let Some(found) = find_paragraph(&cell["blocks"], needle) {
                            return Some(found);
                        }
                    }
                }
            }
            _ => {}
        }
    }
    None
}

fn utf16_index(text: &str, needle: &str) -> usize {
    let byte = text.find(needle).unwrap();
    text[..byte].encode_utf16().count()
}

#[test]
fn the_confined_app_shows_pages_reads_comments_and_exports() {
    let Some(rig) = Rig::new() else { return };
    if !have_libreoffice() {
        return;
    }
    let docs = rig.work.join("Documents");
    std::fs::create_dir_all(&docs).unwrap();
    std::fs::create_dir_all(rig.work.join("Projects")).unwrap();
    std::fs::copy(common::fixture("report.docx"), docs.join("report.docx")).unwrap();
    std::fs::copy(common::fixture("deck.pptx"), docs.join("deck.pptx")).unwrap();
    std::fs::copy(
        common::fixture("report.docx"),
        rig.work.join("Projects/x.docx"),
    )
    .unwrap();
    let original = std::fs::read(docs.join("report.docx")).unwrap();

    let mut core = Core::start(&rig);

    // Describe it.
    let open = core
        .call(
            "office.open",
            json!({ "path": "r620:/Documents/report.docx" }),
        )
        .unwrap();
    assert_eq!(open["kind"], "document");
    assert_eq!(open["reading"], true);
    assert_eq!(
        open["engine"], true,
        "LibreOffice was not found from inside the sandbox"
    );
    assert_eq!(open["comment"]["ok"], true, "{open}");

    // Pages: a PDF laid out by the confined worker, then one page image.
    let pages = core
        .call("office.pages", json!({ "path": "/Documents/report.docx" }))
        .unwrap();
    assert!(pages["pages"].as_u64().unwrap() >= 1, "{pages}");
    let started = Instant::now();
    let page = core
        .call(
            "office.page.render",
            json!({ "path": "/Documents/report.docx", "page": 1, "width": 1080 }),
        )
        .unwrap();
    let image = base64::engine::general_purpose::STANDARD
        .decode(page["data"].as_str().unwrap())
        .unwrap();
    assert_eq!(&image[..2], [0xFF, 0xD8], "not a JPEG");
    assert_eq!(page["width"], 1100, "rounded up to the cache step");
    eprintln!(
        "S6: page 1 at 1100 px is {} KiB as JPEG, rendered in {:?}",
        image.len() / 1024,
        started.elapsed()
    );
    assert!(
        core.call(
            "office.page.render",
            json!({ "path": "/Documents/report.docx", "page": 99, "width": 400 })
        )
        .unwrap_err()
        .contains("no page 99")
    );
    // A presentation pages too.
    let deck = core
        .call("office.pages", json!({ "path": "/Documents/deck.pptx" }))
        .unwrap();
    assert_eq!(deck["pages"], 2);

    // Reading view, then a comment on a few words of it.
    let text = core
        .call("office.text", json!({ "path": "/Documents/report.docx" }))
        .unwrap();
    assert!(
        text["text"].as_str().unwrap().contains("Quarterly report"),
        "{}",
        text["text"]
    );
    let (anchor, para) = find_paragraph(&text["blocks"], "first paragraph").unwrap();
    let start = utf16_index(&para, "first paragraph");
    let added = core
        .call(
            "office.comment.add",
            json!({
                "path": "/Documents/report.docx",
                "anchor": anchor,
                "start": start,
                "end": start + "first paragraph".encode_utf16().count(),
                "text": "Is it, though?",
                "author": "Kim Reviewer",
            }),
        )
        .unwrap();
    assert_eq!(added["path"], "/Documents/report (commented).docx");
    let commented = std::fs::read(docs.join("report (commented).docx")).unwrap();
    let doc = Docx::open(commented.clone()).unwrap();
    let id = added["id"].as_str().unwrap();
    assert_eq!(patch::covered_text(&doc, &anchor, id), "first paragraph");
    assert_eq!(doc.comments[0].author.as_deref(), Some("Kim Reviewer"));
    // Only the expected parts differ from the original; the rest are its
    // bytes.
    let diff = package::diff(
        &mut package::Package::open(original.clone()).unwrap(),
        &mut package::Package::open(commented).unwrap(),
    )
    .unwrap();
    assert!(diff.removed.is_empty());
    assert_eq!(
        diff.changed.iter().map(String::as_str).collect::<Vec<_>>(),
        [
            "[Content_Types].xml",
            "word/_rels/document.xml.rels",
            "word/document.xml"
        ]
    );
    assert_eq!(
        diff.added.iter().map(String::as_str).collect::<Vec<_>>(),
        ["word/comments.xml"]
    );
    // The original is untouched.
    assert_eq!(std::fs::read(docs.join("report.docx")).unwrap(), original);

    // A reply goes into another new file, threaded on the first comment.
    let reply = core
        .call(
            "office.comment.add",
            json!({
                "path": "/Documents/report (commented).docx",
                "text": "Yes.",
                "author": "Ann",
                "reply_to": id,
            }),
        )
        .unwrap();
    assert_eq!(reply["path"], "/Documents/report (commented 2).docx");
    let doc = Docx::open(std::fs::read(docs.join("report (commented 2).docx")).unwrap()).unwrap();
    assert_eq!(doc.comments[1].reply_to.as_deref(), Some(id));
    libreoffice_reads_the_thread(&docs.join("report (commented 2).docx"));

    // Save in place: refused while the original is there; once it has gone
    // to the trash (the core's files.trash, played here by a move out of the
    // folder), the commented copy takes its name.
    let refused = core
        .call(
            "office.save.inplace",
            json!({ "prepared": "/Documents/report (commented).docx", "path": "/Documents/report.docx" }),
        )
        .unwrap_err();
    assert!(
        refused.contains("already exists") && refused.contains("trash"),
        "{refused}"
    );
    std::fs::rename(docs.join("report.docx"), rig.work.join("trashed.docx")).unwrap();
    core.call(
        "office.save.inplace",
        json!({ "prepared": "/Documents/report (commented).docx", "path": "/Documents/report.docx" }),
    )
    .unwrap();
    assert!(!docs.join("report (commented).docx").exists());
    let saved = Docx::open(std::fs::read(docs.join("report.docx")).unwrap()).unwrap();
    assert_eq!(saved.comments.len(), 1);

    // Export: a PDF beside the document, never over anything.
    let exported = core
        .call(
            "office.export_pdf",
            json!({ "path": "/Documents/deck.pptx" }),
        )
        .unwrap();
    assert_eq!(exported["path"], "/Documents/deck.pdf");
    assert!(
        std::fs::read(docs.join("deck.pdf"))
            .unwrap()
            .starts_with(b"%PDF-")
    );
    let again = core
        .call(
            "office.export_pdf",
            json!({ "path": "/Documents/deck.pptx" }),
        )
        .unwrap();
    assert_eq!(again["path"], "/Documents/deck (2).pdf");

    // A document in a folder Office was not given is refused by name.
    let outside = core
        .call("office.open", json!({ "path": "/Projects/x.docx" }))
        .unwrap_err();
    assert!(outside.contains("\"Projects\""), "{outside}");

    // Nothing LibreOffice touched is in the operator's folders: no lock
    // files, nothing but what the commands wrote.
    let mut names: Vec<String> = std::fs::read_dir(&docs)
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    assert_eq!(
        names,
        [
            "deck (2).pdf",
            "deck.pdf",
            "deck.pptx",
            "report (commented 2).docx",
            "report.docx"
        ]
    );

    core.stop(&rig);
    assert!(
        processes_mentioning(&rig.tmp.to_string_lossy()).is_empty(),
        "a worker outlived the app"
    );
}

/// The brief's S3, on the one other implementation on this machine: the
/// patched file, converted by LibreOffice to ODT, holds both comments with
/// their authors, the reply threaded on its parent, and the range closing
/// right after the selected words.
fn libreoffice_reads_the_thread(path: &Path) {
    use deckjack_office::worker::{Engine, Limits};
    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::new(
        "/usr/lib/libreoffice/program".into(),
        env!("CARGO_BIN_EXE_deckjack-office").into(),
        dir.path().join("home"),
        dir.path().join("tmp"),
        Limits::default(),
    );
    std::fs::create_dir_all(dir.path().join("tmp")).unwrap();
    let odt = dir.path().join("out.odt");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let (_tx, rx) = tokio::sync::watch::channel(false);
    runtime
        .block_on(engine.convert(path, "odt", &odt, rx))
        .expect("LibreOffice opens the commented file");
    let mut odt = package::Package::open(std::fs::read(&odt).unwrap()).unwrap();
    let content = odt.read_text("content.xml").unwrap();
    let annotation = |author: &str| {
        let at = content
            .find(&format!("<dc:creator>{author}</dc:creator>"))
            .unwrap_or_else(|| panic!("no comment by {author} in LibreOffice's reading"));
        let open = content[..at].rfind("<office:annotation ").unwrap();
        content[open..at].to_string()
    };
    let parent = annotation("Kim Reviewer");
    let reply = annotation("Ann");
    let name = parent
        .split("office:name=\"")
        .nth(1)
        .and_then(|s| s.split('"').next())
        .unwrap();
    assert!(
        reply.contains(&format!("loext:parent-name=\"{name}\"")),
        "LibreOffice did not thread the reply: {reply}"
    );
    let end = content
        .find(&format!("<office:annotation-end office:name=\"{name}\"/>"))
        .unwrap();
    assert!(
        content[..end].ends_with("first paragraph"),
        "the range does not end after the selection: …{}",
        &content[end.saturating_sub(60)..end]
    );
}

/// The LibreOffice worker's lifetime is the job's: a cancel or a timeout
/// kills it, and no process is left behind. Driven through the engine
/// directly, so the kill can be aimed at a worker that is known to be
/// running.
#[tokio::test]
async fn a_cancelled_or_timed_out_conversion_kills_its_worker() {
    use deckjack_office::worker::{Engine, JobError, Limits};
    if !have_libreoffice() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    let tmp = dir.path().join("tmp");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&tmp).unwrap();
    let source = common::fixture("sheet.xlsx");
    let engine = std::sync::Arc::new(Engine::new(
        "/usr/lib/libreoffice/program".into(),
        env!("CARGO_BIN_EXE_deckjack-office").into(),
        home.clone(),
        tmp.clone(),
        Limits::default(),
    ));
    let jobs = tmp.join("jobs").to_string_lossy().into_owned();

    // Cancel, once the worker is running.
    let (tx, rx) = tokio::sync::watch::channel(false);
    let task = tokio::spawn({
        let engine = std::sync::Arc::clone(&engine);
        let source = source.clone();
        let out = dir.path().join("a.pdf");
        async move { engine.convert(&source, "pdf", &out, rx).await }
    });
    let deadline = Instant::now() + Duration::from_secs(20);
    while processes_mentioning(&jobs).is_empty() {
        assert!(Instant::now() < deadline, "the worker never started");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let cancelled = Instant::now();
    tx.send(true).unwrap();
    let result = task.await.unwrap();
    assert!(matches!(result, Err(JobError::Cancelled)), "{result:?}");
    // Killed, not waited out: LibreOffice alone takes several hundred
    // milliseconds to boot, and the cancel landed as the worker appeared.
    assert!(
        cancelled.elapsed() < Duration::from_millis(200),
        "the cancel took {:?}: the worker ran to the end instead of being killed",
        cancelled.elapsed()
    );
    assert!(
        processes_mentioning(&jobs).is_empty(),
        "the cancelled worker is still running"
    );
    assert!(!dir.path().join("a.pdf").exists());

    // Time out.
    let quick = Engine::new(
        "/usr/lib/libreoffice/program".into(),
        env!("CARGO_BIN_EXE_deckjack-office").into(),
        home,
        tmp,
        Limits {
            timeout: Duration::from_millis(50),
            ..Limits::default()
        },
    );
    let (_tx, rx) = tokio::sync::watch::channel(false);
    let started = Instant::now();
    let result = quick
        .convert(&source, "pdf", &dir.path().join("b.pdf"), rx)
        .await;
    assert!(matches!(result, Err(JobError::TimedOut(_))), "{result:?}");
    assert!(
        started.elapsed() < Duration::from_millis(250),
        "the timeout took {:?}: the worker was not killed at 50 ms",
        started.elapsed()
    );
    assert!(
        processes_mentioning(&jobs).is_empty(),
        "the timed-out worker is still running"
    );
    assert!(!Path::new(&dir.path().join("b.pdf")).exists());
}
