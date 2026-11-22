//! A Word document, opened: its package, its body scanned into blocks, its
//! comments threaded, and what editing it would keep.

use serde::Serialize;

use super::package::{Package, PackageError};
use super::parts::{self, Comment};
use super::scan::{self, Block, Change, Scan, ScanError};

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum DocxError {
    #[error(transparent)]
    Package(#[from] PackageError),
    #[error(transparent)]
    Scan(#[from] ScanError),
    #[error("the package names no main document part, so it is not a Word document")]
    NoDocument,
    #[error("the main document part is {0:?}, and that is not where a Word document's body lives")]
    NotWordprocessing(String),
}

/// What a file holds that an editing path could drop, computed from the
/// package rather than asserted. The comment patcher keeps all of it; the
/// report is for the operator's "what survives" and for agents.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Report {
    pub parts: usize,
    pub comments: usize,
    pub tracked_changes: usize,
    /// A digital signature, which any change invalidates.
    pub signed: bool,
    /// A VBA project. Never run here; kept as bytes.
    pub macros: bool,
    pub embedded_objects: usize,
    pub charts: usize,
    pub images: usize,
    pub custom_xml: bool,
    /// ISO 29500 Strict rather than Transitional.
    pub strict: bool,
}

pub struct Docx {
    pub package: Package,
    /// The main part, `word/document.xml` in every file Word writes.
    pub main: String,
    pub xml: String,
    pub scan: Scan,
    pub comments: Vec<Comment>,
    /// The comments part, when there is one.
    pub comments_part: Option<String>,
    pub extended_part: Option<String>,
    pub comments_xml: Option<String>,
    pub extended_xml: Option<String>,
    /// `document.xml.rels`, when there is one.
    pub rels_xml: Option<String>,
    pub content_types: String,
    pub report: Report,
}

impl Docx {
    pub fn open(bytes: Vec<u8>) -> Result<Docx, DocxError> {
        let mut package = Package::open(bytes)?;
        let root_rels = package.read_text("_rels/.rels")?;
        let main = parts::main_part(&root_rels).ok_or(DocxError::NoDocument)?;
        if !package.has(&main) {
            return Err(DocxError::NoDocument);
        }
        let content_types = package.read_text("[Content_Types].xml")?;
        let rels_name = parts::rels_part(&main);
        let rels_xml = package.read_text_opt(&rels_name)?;
        let rels = rels_xml.as_deref().map(parts::rels).unwrap_or_default();
        let target = |kind: &str| {
            rels.iter()
                .find(|r| parts::rel_is(r, kind) && !r.external)
                .and_then(|r| parts::resolve(&main, &r.target))
        };

        let styles = match target("styles") {
            Some(name) => package.read_text_opt(&name)?.map(|x| parts::styles(&x)),
            None => None,
        }
        .unwrap_or_default();
        let numbering = match target("numbering") {
            Some(name) => package.read_text_opt(&name)?.map(|x| parts::numbering(&x)),
            None => None,
        }
        .unwrap_or_default();

        let xml = package.read_text(&main)?;
        let mut scan = scan::scan(&xml, &styles)?;
        scan::collect_change_text(&mut scan);
        parts::number_lists(&mut scan.blocks, &numbering);

        let comments_part = target("comments").filter(|n| package.has(n));
        let extended_part = target("commentsExtended").filter(|n| package.has(n));
        let comments_xml = match &comments_part {
            Some(name) => Some(package.read_text(name)?),
            None => None,
        };
        let extended_xml = match &extended_part {
            Some(name) => Some(package.read_text(name)?),
            None => None,
        };
        let mut comments = comments_xml
            .as_deref()
            .map(parts::comments)
            .unwrap_or_default();
        if let Some(extended) = extended_xml.as_deref() {
            parts::thread(&mut comments, &parts::comments_extended(extended));
        }

        let names = package.names();
        let signed = names.iter().any(|n| n.starts_with("_xmlsignatures/"))
            || parts::rels(&root_rels)
                .iter()
                .any(|r| r.kind.ends_with("/digital-signature/origin"));
        let report = Report {
            parts: names.len(),
            comments: comments.len(),
            tracked_changes: scan.changes.len(),
            signed,
            macros: names.iter().any(|n| n.ends_with("vbaProject.bin")),
            embedded_objects: names.iter().filter(|n| n.contains("/embeddings/")).count(),
            charts: names
                .iter()
                .filter(|n| n.contains("/charts/") && n.ends_with(".xml") && !n.contains("/_rels/"))
                .count(),
            images: names.iter().filter(|n| n.contains("/media/")).count(),
            custom_xml: names.iter().any(|n| n.starts_with("customXml/")),
            strict: scan.strict,
        };

        Ok(Docx {
            package,
            main,
            xml,
            scan,
            comments,
            comments_part,
            extended_part,
            comments_xml,
            extended_xml,
            rels_xml,
            content_types,
            report,
        })
    }

    pub fn blocks(&self) -> &[Block] {
        &self.scan.blocks
    }

    pub fn changes(&self) -> &[Change] {
        &self.scan.changes
    }

    /// The visible text of paragraph `anchor`, exactly as anchors count it.
    pub fn paragraph_text(&self, anchor: &str) -> Option<String> {
        fn find<'b>(blocks: &'b [Block], anchor: &str) -> Option<&'b scan::Para> {
            for block in blocks {
                match block {
                    Block::Paragraph(p) if p.anchor == anchor => return Some(p),
                    Block::Paragraph(_) => {}
                    Block::Table(t) => {
                        for row in &t.rows {
                            for cell in row {
                                if let Some(p) = find(&cell.blocks, anchor) {
                                    return Some(p);
                                }
                            }
                        }
                    }
                }
            }
            None
        }
        let para = find(&self.scan.blocks, anchor)?;
        Some(
            para.inlines
                .iter()
                .filter_map(|i| match i {
                    scan::Inline::Text { text, .. } => Some(text.as_str()),
                    _ => None,
                })
                .collect(),
        )
    }
}
