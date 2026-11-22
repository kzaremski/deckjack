//! Adding a comment, or a reply, as a surgical patch of the package.
//!
//! Only the parts a comment needs are touched:
//!
//! - `document.xml`: the range markers and the reference run are spliced in
//!   at byte offsets the scanner recorded. Everything else in the part, down
//!   to its whitespace and attribute order, is the original bytes. A run is
//!   split only where a selection starts or ends inside it, and each piece
//!   carries the run's original opening tag and properties.
//! - `comments.xml`: the new comment is appended (the part is created when
//!   the file had none).
//! - `commentsExtended.xml`: for a reply only, the thread link.
//! - `document.xml.rels` and `[Content_Types].xml`: only when a part had to
//!   be created.
//!
//! Every other entry is copied raw ([`Package::rewrite`]). The result is then
//! checked, not trusted: the package diff must be exactly the parts above,
//! and the new document must scan to the same paragraphs with the comment
//! covering exactly the selected text. Any other outcome fails the
//! operation and nothing is written.

use std::collections::{BTreeMap, BTreeSet};

use quick_xml::events::Event;
use quick_xml::reader::Reader;

use super::docx::{Docx, DocxError};
use super::package::{self, Package, PackageError};
use super::parts;
use super::scan::{ChildKind, Inline, ParaPos, Run, Span};

const W14_NS: &str = "http://schemas.microsoft.com/office/word/2010/wordml";
const W15_NS: &str = "http://schemas.microsoft.com/office/word/2012/wordml";
const MC_NS: &str = "http://schemas.openxmlformats.org/markup-compatibility/2006";
const RELS_NS: &str = "http://schemas.openxmlformats.org/package/2006/relationships";
const COMMENTS_REL: &str =
    "http://schemas.openxmlformats.org/officeDocument/2006/relationships/comments";
const EXTENDED_REL: &str =
    "http://schemas.microsoft.com/office/2011/relationships/commentsExtended";
const COMMENTS_TYPE: &str =
    "application/vnd.openxmlformats-officedocument.wordprocessingml.comments+xml";
const EXTENDED_TYPE: &str =
    "application/vnd.openxmlformats-officedocument.wordprocessingml.commentsExtended+xml";

/// The longest comment accepted, in characters.
pub const MAX_COMMENT: usize = 20_000;
pub const MAX_AUTHOR: usize = 100;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum PatchError {
    #[error(transparent)]
    Docx(#[from] DocxError),
    #[error(transparent)]
    Package(#[from] PackageError),
    #[error(
        "this document is digitally signed, and adding a comment would invalidate the \
         signature. Office does not change signed documents"
    )]
    Signed,
    #[error("this document is in ISO Strict form, which the comment patcher does not write")]
    Strict,
    #[error(
        "this document binds WordprocessingML as its default namespace, which the comment \
         patcher does not write"
    )]
    DefaultNamespace,
    #[error("there is no paragraph {0} in this document")]
    NoParagraph(String),
    #[error(
        "the selection {start}..{end} is not inside paragraph {anchor}, which is {len} characters long"
    )]
    BadRange {
        anchor: String,
        start: usize,
        end: usize,
        len: usize,
    },
    #[error("a comment needs some text")]
    EmptyText,
    #[error("a comment is limited to {MAX_COMMENT} characters")]
    TooLong,
    #[error("a comment needs an author name of 1 to {MAX_AUTHOR} characters")]
    BadAuthor,
    #[error("there is no comment {0} to reply to")]
    NoParent(String),
    #[error("comment {0} has no place in the document body to attach a reply to")]
    UnanchoredParent(String),
    #[error(
        "the selection starts or ends inside text this file stores as CDATA; select a slightly \
         different range"
    )]
    Cdata,
    #[error("{0} could not be updated: {1}")]
    Part(String, String),
    #[error("the patched document failed its own check, so nothing was written: {0}")]
    Verify(String),
}

/// A comment to add.
#[derive(Debug, Clone)]
pub struct NewComment {
    /// `p/N`. Ignored for a reply.
    pub anchor: String,
    /// UTF-16 offsets into the paragraph's visible text. Ignored for a reply.
    pub start: usize,
    pub end: usize,
    pub text: String,
    pub author: String,
    /// ISO 8601, UTC.
    pub date: String,
    pub reply_to: Option<String>,
}

/// The patched package and what the patch did.
#[derive(Debug)]
pub struct Patched {
    pub bytes: Vec<u8>,
    pub id: String,
    pub changed: BTreeSet<String>,
    pub added: BTreeSet<String>,
}

pub fn add_comment(doc: &mut Docx, new: &NewComment) -> Result<Patched, PatchError> {
    if doc.report.signed {
        return Err(PatchError::Signed);
    }
    if doc.scan.strict {
        return Err(PatchError::Strict);
    }
    if doc.scan.prefix.is_empty() {
        return Err(PatchError::DefaultNamespace);
    }
    let text = clean_text(&new.text);
    if text.trim().is_empty() {
        return Err(PatchError::EmptyText);
    }
    if text.chars().count() > MAX_COMMENT {
        return Err(PatchError::TooLong);
    }
    let author = clean_text(&new.author).replace('\n', " ");
    let author = author.trim().to_string();
    if author.is_empty() || author.chars().count() > MAX_AUTHOR {
        return Err(PatchError::BadAuthor);
    }

    let p = doc.scan.prefix.clone();
    let id = next_id(doc);

    // Where the new comment goes in the body, and what it covers.
    let (edits, expected) = match &new.reply_to {
        None => {
            let (_, para) = doc
                .scan
                .paragraph(&new.anchor)
                .ok_or_else(|| PatchError::NoParagraph(new.anchor.clone()))?;
            if new.start >= new.end || new.end > para.units {
                return Err(PatchError::BadRange {
                    anchor: new.anchor.clone(),
                    start: new.start,
                    end: new.end,
                    len: para.units,
                });
            }
            let edits = range_edits(&doc.xml, para, new.start, new.end, &p, &id)?;
            let visible = doc.paragraph_text(&new.anchor).unwrap_or_default();
            let selected = utf16_slice(&visible, new.start, new.end);
            (edits, Some((new.anchor.clone(), selected)))
        }
        Some(parent) => {
            if !doc.comments.iter().any(|c| &c.id == parent) {
                return Err(PatchError::NoParent(parent.clone()));
            }
            (reply_edits(doc, parent, &p, &id)?, None)
        }
    };
    let document = apply(&doc.xml, edits).map_err(|e| PatchError::Part(doc.main.clone(), e))?;

    // The comments part, and the thread link for a reply.
    let mut replace: BTreeMap<String, Vec<u8>> = BTreeMap::new();
    let mut add: Vec<(String, Vec<u8>)> = Vec::new();
    let mut new_rels: Vec<(&str, String)> = Vec::new();
    let mut new_types: Vec<(String, &str)> = Vec::new();

    let mut taken_para_ids = existing_para_ids(doc);
    let para_id = fresh_para_id(&mut taken_para_ids, &id, &new.date, &text);
    let reply_parent_para = match &new.reply_to {
        Some(parent) => Some(thread_root_para_id(doc, parent, &mut taken_para_ids)?),
        None => None,
    };

    let (comments_name, comments_xml) = match (&doc.comments_part, &doc.comments_xml) {
        (Some(name), Some(xml)) => {
            let c = root_prefix(xml, scan_main_ns()).unwrap_or_else(|| "w".to_string());
            let (mut xml, w14) = ensure_ns(xml, W14_NS, "w14", reply_parent_para.is_some())
                .map_err(|e| PatchError::Part(name.clone(), e))?;
            if let Some((parent_id, Some(pid))) = &reply_parent_para {
                xml = set_para_id(&xml, parent_id, pid)
                    .map_err(|e| PatchError::Part(name.clone(), e))?;
            }
            let element = comment_xml(&c, w14.as_deref(), &id, &author, &new.date, &text, &para_id);
            let xml = insert_before_root_end(&xml, &element)
                .map_err(|e| PatchError::Part(name.clone(), e))?;
            replace.insert(name.clone(), xml.into_bytes());
            (name.clone(), None)
        }
        _ => {
            let name = sibling(&doc.main, "comments.xml");
            let element = comment_xml("w", Some("w14"), &id, &author, &new.date, &text, &para_id);
            let xml = format!(
                "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\r\n\
                 <w:comments xmlns:w=\"{}\" xmlns:w14=\"{W14_NS}\" xmlns:mc=\"{MC_NS}\" \
                 mc:Ignorable=\"w14\">{element}</w:comments>",
                scan_main_ns()
            );
            new_rels.push((COMMENTS_REL, relative_target(&doc.main, &name)));
            new_types.push((name.clone(), COMMENTS_TYPE));
            (name.clone(), Some(xml))
        }
    };
    if let Some(xml) = comments_xml {
        add.push((comments_name, xml.into_bytes()));
    }

    if let Some((_, parent_para)) = &reply_parent_para {
        let parent_para = parent_para
            .clone()
            .ok_or_else(|| PatchError::Verify("the parent comment has no paragraph id".into()))?;
        match (&doc.extended_part, &doc.extended_xml) {
            (Some(name), Some(xml)) => {
                let (xml, w15) = ensure_ns(xml, W15_NS, "w15", true)
                    .map_err(|e| PatchError::Part(name.clone(), e))?;
                let w15 = w15.unwrap_or_else(|| "w15".into());
                let entry = format!(
                    "<{w15}:commentEx {w15}:paraId=\"{para_id}\" {w15}:paraIdParent=\"{parent_para}\" {w15}:done=\"0\"/>"
                );
                let xml = insert_before_root_end(&xml, &entry)
                    .map_err(|e| PatchError::Part(name.clone(), e))?;
                replace.insert(name.clone(), xml.into_bytes());
            }
            _ => {
                let name = sibling(&doc.main, "commentsExtended.xml");
                let xml = format!(
                    "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\r\n\
                     <w15:commentsEx xmlns:w15=\"{W15_NS}\" xmlns:mc=\"{MC_NS}\" mc:Ignorable=\"w15\">\
                     <w15:commentEx w15:paraId=\"{para_id}\" w15:paraIdParent=\"{parent_para}\" w15:done=\"0\"/>\
                     </w15:commentsEx>"
                );
                new_rels.push((EXTENDED_REL, relative_target(&doc.main, &name)));
                new_types.push((name.clone(), EXTENDED_TYPE));
                add.push((name, xml.into_bytes()));
            }
        }
    }

    let rels_name = parts::rels_part(&doc.main);
    if !new_rels.is_empty() {
        let base = doc.rels_xml.clone();
        let mut xml = base.clone().unwrap_or_else(|| {
            format!(
                "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\r\n<Relationships xmlns=\"{RELS_NS}\"></Relationships>"
            )
        });
        let mut taken: BTreeSet<String> = parts::rels(&xml).into_iter().map(|r| r.id).collect();
        for (kind, target) in &new_rels {
            let rid = (1..)
                .map(|n| format!("rId{n}"))
                .find(|r| !taken.contains(r))
                .unwrap_or_default();
            taken.insert(rid.clone());
            let element = format!(
                "<Relationship Id=\"{rid}\" Type=\"{kind}\" Target=\"{}\"/>",
                escape(target)
            );
            xml = insert_before_root_end(&xml, &element)
                .map_err(|e| PatchError::Part(rels_name.clone(), e))?;
        }
        if base.is_some() {
            replace.insert(rels_name.clone(), xml.into_bytes());
        } else {
            add.push((rels_name.clone(), xml.into_bytes()));
        }
    }
    if !new_types.is_empty() {
        let mut xml = doc.content_types.clone();
        for (part, kind) in &new_types {
            let element = format!(
                "<Override PartName=\"/{}\" ContentType=\"{kind}\"/>",
                escape(part)
            );
            xml = insert_before_root_end(&xml, &element)
                .map_err(|e| PatchError::Part("[Content_Types].xml".into(), e))?;
        }
        replace.insert("[Content_Types].xml".to_string(), xml.into_bytes());
    }
    replace.insert(doc.main.clone(), document.into_bytes());

    let bytes = doc.package.rewrite(&replace, &add)?;
    let changed: BTreeSet<String> = replace.keys().cloned().collect();
    let added: BTreeSet<String> = add.iter().map(|(n, _)| n.clone()).collect();
    verify(doc, &bytes, &changed, &added, &id, expected.as_ref())?;
    Ok(Patched {
        bytes,
        id,
        changed,
        added,
    })
}

fn scan_main_ns() -> &'static str {
    super::scan::MAIN_NS
}

/// The survival check: exactly the expected parts differ, every other entry
/// is byte-identical, and the comment landed on the selected words.
pub(crate) fn verify(
    doc: &mut Docx,
    bytes: &[u8],
    changed: &BTreeSet<String>,
    added: &BTreeSet<String>,
    id: &str,
    expected: Option<&(String, String)>,
) -> Result<(), PatchError> {
    let mut after = Package::open(bytes.to_vec())?;
    let diff = package::diff(&mut doc.package, &mut after)?;
    // A replaced part may come out byte-identical in principle; never in
    // practice, since each gains an element. Anything changed must be
    // expected, and nothing may vanish.
    if !diff.changed.is_subset(changed) || diff.added != *added || !diff.removed.is_empty() {
        return Err(PatchError::Verify(format!(
            "expected to change {changed:?} and add {added:?}; changed {:?}, added {:?}, removed {:?}",
            diff.changed, diff.added, diff.removed
        )));
    }
    let reopened = Docx::open(bytes.to_vec()).map_err(|e| PatchError::Verify(e.to_string()))?;
    if reopened.scan.paragraphs.len() != doc.scan.paragraphs.len() {
        return Err(PatchError::Verify("the paragraph count changed".into()));
    }
    for (index, (a, b)) in doc
        .scan
        .paragraphs
        .iter()
        .zip(&reopened.scan.paragraphs)
        .enumerate()
    {
        let anchor = format!("p/{index}");
        if a.units != b.units || doc.paragraph_text(&anchor) != reopened.paragraph_text(&anchor) {
            return Err(PatchError::Verify(format!("the text of {anchor} changed")));
        }
    }
    if !reopened.comments.iter().any(|c| c.id == id) {
        return Err(PatchError::Verify(
            "the new comment is not in the comments part".into(),
        ));
    }
    if !reopened.scan.comment_ids.contains(id) {
        return Err(PatchError::Verify(
            "the new comment is not anchored in the body".into(),
        ));
    }
    if let Some((anchor, selected)) = expected {
        let covered = covered_text(&reopened, anchor, id);
        if &covered != selected {
            return Err(PatchError::Verify(format!(
                "the comment covers {covered:?}, not the selected {selected:?}"
            )));
        }
    }
    Ok(())
}

/// The text in paragraph `anchor` that comment `id` covers.
pub fn covered_text(doc: &Docx, anchor: &str, id: &str) -> String {
    fn find<'b>(blocks: &'b [super::scan::Block], anchor: &str) -> Option<&'b super::scan::Para> {
        for block in blocks {
            match block {
                super::scan::Block::Paragraph(p) if p.anchor == anchor => return Some(p),
                super::scan::Block::Paragraph(_) => {}
                super::scan::Block::Table(t) => {
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
    find(&doc.scan.blocks, anchor)
        .map(|p| {
            p.inlines
                .iter()
                .filter_map(|i| match i {
                    Inline::Text { text, comments, .. } if comments.iter().any(|c| c == id) => {
                        Some(text.as_str())
                    }
                    _ => None,
                })
                .collect()
        })
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------
// The body
// ---------------------------------------------------------------------------

/// A replacement of `span` with `text`; an empty span is an insertion.
type Edit = (Span, String);

fn range_edits(
    xml: &str,
    para: &ParaPos,
    start: usize,
    end: usize,
    p: &str,
    id: &str,
) -> Result<Vec<Edit>, PatchError> {
    let start_marker = format!("<{p}:commentRangeStart {p}:id=\"{id}\"/>");
    let end_marker = format!("<{p}:commentRangeEnd {p}:id=\"{id}\"/>");
    let reference = format!("<{p}:r><{p}:commentReference {p}:id=\"{id}\"/></{p}:r>");

    // The run holding the first selected unit, and the one holding the last.
    let first = para
        .runs
        .iter()
        .position(|r| r.units > 0 && r.offset <= start && start < r.offset + r.units)
        .ok_or_else(|| PatchError::Verify("no run holds the selection's start".into()))?;
    let last = para
        .runs
        .iter()
        .position(|r| r.units > 0 && r.offset < end && end <= r.offset + r.units)
        .ok_or_else(|| PatchError::Verify("no run holds the selection's end".into()))?;

    let end_run = &para.runs[last];
    // The reference run cannot go inside a deletion (it would read as
    // deleted) or inside a hyperlink or field; it goes after the outermost
    // wrapper of the end, at paragraph level.
    let outer = end_run.wrappers.first().map(|w| para.wrappers[*w]);
    let end_text = match outer {
        None => format!("{end_marker}{reference}"),
        Some(_) => end_marker,
    };

    let mut edits = Vec::new();
    let mut cuts: BTreeMap<usize, Vec<(usize, String)>> = BTreeMap::new();
    cuts.entry(first)
        .or_default()
        .push((start - para.runs[first].offset, start_marker));
    cuts.entry(last)
        .or_default()
        .push((end - end_run.offset, end_text));
    for (index, cuts) in cuts {
        let run = &para.runs[index];
        edits.push((run.span, split_run(xml, run, &cuts, p)?));
    }
    if let Some(w) = outer {
        edits.push((
            Span {
                start: w.span.end,
                end: w.span.end,
            },
            reference,
        ));
    }
    Ok(edits)
}

/// A run, with markup inserted at each local UTF-16 offset. A cut at 0 goes
/// before the run and a cut at its length after it; a cut inside splits it.
fn split_run(
    xml: &str,
    run: &Run,
    cuts: &[(usize, String)],
    p: &str,
) -> Result<String, PatchError> {
    let src = |s: Span| &xml[s.start..s.end];
    let mut cuts: Vec<(usize, String)> = cuts.to_vec();
    cuts.sort_by_key(|c| c.0);
    let mut before = String::new();
    let mut after = String::new();
    let mut inner: Vec<(usize, String)> = Vec::new();
    for (at, text) in cuts {
        if at == 0 {
            before.push_str(&text);
        } else if at >= run.units {
            after.push_str(&text);
        } else {
            inner.push((at, text));
        }
    }
    if inner.is_empty() {
        return Ok(format!("{before}{}{after}", src(run.span)));
    }

    let open = src(run.open);
    let props = run.props.map(src).unwrap_or("");
    let close = format!("</{p}:r>");
    let mut out = before;
    out.push_str(open);
    out.push_str(props);
    let mut pos = 0usize;
    let mut pending = inner.into_iter().peekable();
    for child in &run.children {
        // Cuts that fall exactly here go before this child, so zero-width
        // children (field characters, references) stay with what follows.
        while let Some((at, _)) = pending.peek() {
            if *at == pos {
                let (_, text) = pending.next().unwrap_or_default();
                out.push_str(&close);
                out.push_str(&text);
                out.push_str(open);
                out.push_str(props);
            } else {
                break;
            }
        }
        let child_end = pos + child.units;
        let inside: Vec<(usize, String)> = {
            let mut v = Vec::new();
            while let Some((at, _)) = pending.peek() {
                if *at > pos && *at < child_end {
                    v.push(pending.next().unwrap_or_default());
                } else {
                    break;
                }
            }
            v
        };
        if inside.is_empty() {
            out.push_str(src(child.span));
        } else {
            let ChildKind::Text {
                tag,
                content,
                cdata,
            } = &child.kind
            else {
                return Err(PatchError::Verify(
                    "a cut fell inside a non-text run child".into(),
                ));
            };
            if *cdata {
                return Err(PatchError::Cdata);
            }
            let raw = src(*content);
            let mut from = 0usize;
            for (at, text) in &inside {
                let byte = unit_to_byte(raw, at - pos)
                    .ok_or_else(|| PatchError::Verify("a cut fell inside a character".into()))?;
                out.push_str(&format!(
                    "<{p}:{tag} xml:space=\"preserve\">{}</{p}:{tag}>",
                    &raw[from..byte]
                ));
                out.push_str(&close);
                out.push_str(text);
                out.push_str(open);
                out.push_str(props);
                from = byte;
            }
            out.push_str(&format!(
                "<{p}:{tag} xml:space=\"preserve\">{}</{p}:{tag}>",
                &raw[from..]
            ));
        }
        pos = child_end;
    }
    // Every cut must have found its place; one left over would be a marker
    // silently dropped.
    if pending.next().is_some() {
        return Err(PatchError::Verify(
            "a cut fell outside the run's children".into(),
        ));
    }
    out.push_str(&close);
    out.push_str(&after);
    Ok(out)
}

/// The byte offset in escaped XML text `raw` of the boundary after `units`
/// UTF-16 units of its unescaped text. An entity counts as the one character
/// it stands for. `None` if the offset falls inside a surrogate pair.
pub fn unit_to_byte(raw: &str, units: usize) -> Option<usize> {
    let bytes = raw.as_bytes();
    let mut i = 0usize;
    let mut count = 0usize;
    while count < units {
        if i >= bytes.len() {
            return None;
        }
        if bytes[i] == b'&' {
            let semi = raw[i..].find(';')? + i;
            let name = &raw[i + 1..semi];
            let width = if let Some(num) = name.strip_prefix('#') {
                let code =
                    if let Some(hex) = num.strip_prefix('x').or_else(|| num.strip_prefix('X')) {
                        u32::from_str_radix(hex, 16).ok()?
                    } else {
                        num.parse::<u32>().ok()?
                    };
                char::from_u32(code)?.len_utf16()
            } else {
                1
            };
            count += width;
            i = semi + 1;
        } else {
            let c = raw[i..].chars().next()?;
            count += c.len_utf16();
            i += c.len_utf8();
        }
    }
    (count == units).then_some(i)
}

fn reply_edits(doc: &Docx, parent: &str, p: &str, id: &str) -> Result<Vec<Edit>, PatchError> {
    let markers = doc
        .scan
        .markers
        .get(parent)
        .ok_or_else(|| PatchError::UnanchoredParent(parent.to_string()))?;
    let reference = format!("<{p}:r><{p}:commentReference {p}:id=\"{id}\"/></{p}:r>");
    let mut edits = Vec::new();
    let at = |s: Span| Span {
        start: s.end,
        end: s.end,
    };
    if let Some(s) = markers.range_start {
        edits.push((at(s), format!("<{p}:commentRangeStart {p}:id=\"{id}\"/>")));
    }
    match (markers.range_end, markers.reference_run) {
        (Some(e), Some(r)) => {
            edits.push((at(e), format!("<{p}:commentRangeEnd {p}:id=\"{id}\"/>")));
            edits.push((at(r), reference));
        }
        (Some(e), None) => {
            edits.push((
                at(e),
                format!("<{p}:commentRangeEnd {p}:id=\"{id}\"/>{reference}"),
            ));
        }
        (None, Some(r)) => edits.push((at(r), reference)),
        (None, None) => return Err(PatchError::UnanchoredParent(parent.to_string())),
    }
    Ok(edits)
}

/// Apply non-overlapping edits to `xml`.
fn apply(xml: &str, mut edits: Vec<Edit>) -> Result<String, String> {
    // Stable: two insertions at one offset keep the order they were made in.
    edits.sort_by_key(|(s, _)| (s.start, s.end));
    let mut out = String::with_capacity(xml.len() + 512);
    let mut at = 0usize;
    for (span, text) in edits {
        if span.start < at || span.end < span.start || span.end > xml.len() {
            return Err("two edits overlap".to_string());
        }
        out.push_str(&xml[at..span.start]);
        out.push_str(&text);
        at = span.end;
    }
    out.push_str(&xml[at..]);
    Ok(out)
}

// ---------------------------------------------------------------------------
// Ids and small parts
// ---------------------------------------------------------------------------

fn next_id(doc: &Docx) -> String {
    let max = doc
        .comments
        .iter()
        .map(|c| c.id.as_str())
        .chain(doc.scan.comment_ids.iter().map(String::as_str))
        .filter_map(|id| id.parse::<i64>().ok())
        .max()
        .unwrap_or(-1);
    (max + 1).max(0).to_string()
}

fn existing_para_ids(doc: &Docx) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for xml in [Some(doc.xml.as_str()), doc.comments_xml.as_deref()]
        .into_iter()
        .flatten()
    {
        for piece in xml.split("paraId=\"").skip(1) {
            if let Some(id) = piece.get(..8) {
                out.insert(id.to_ascii_uppercase());
            }
        }
    }
    out
}

/// An 8-hex-digit paragraph id below 0x80000000, as Word requires, unused in
/// this document. Derived from the comment so a test can predict it.
fn fresh_para_id(taken: &mut BTreeSet<String>, id: &str, date: &str, text: &str) -> String {
    use sha2::{Digest as _, Sha256};
    let mut salt = 0u32;
    loop {
        let digest = Sha256::digest(format!("{id}\0{date}\0{text}\0{salt}"));
        let n = u32::from_be_bytes([digest[0], digest[1], digest[2], digest[3]]) & 0x7FFF_FFFF;
        let candidate = format!("{n:08X}");
        if n != 0 && taken.insert(candidate.clone()) {
            return candidate;
        }
        salt += 1;
    }
}

/// The thread a reply joins: its root comment's id and paragraph id. Word's
/// threads are flat, so a reply to a reply hangs off the root. When the root
/// has no paragraph id one is minted, and the caller writes it in.
fn thread_root_para_id(
    doc: &Docx,
    parent: &str,
    taken: &mut BTreeSet<String>,
) -> Result<(String, Option<String>), PatchError> {
    let mut at = parent.to_string();
    for _ in 0..64 {
        let c = doc
            .comments
            .iter()
            .find(|c| c.id == at)
            .ok_or_else(|| PatchError::NoParent(at.clone()))?;
        match &c.reply_to {
            Some(up) if up != &at => at = up.clone(),
            _ => break,
        }
    }
    let root = doc
        .comments
        .iter()
        .find(|c| c.id == at)
        .ok_or_else(|| PatchError::NoParent(at.clone()))?;
    let para = match &root.para_id {
        Some(p) => Some(p.clone()),
        None => Some(fresh_para_id(taken, &root.id, "root", &root.text)),
    };
    Ok((root.id.clone(), para))
}

/// Give comment `id`'s last paragraph the paragraph id `para_id`, if it has
/// none. A no-op when it already has one.
fn set_para_id(xml: &str, id: &str, para_id: &str) -> Result<String, String> {
    let mut r = Reader::from_str(xml);
    let mut in_target = false;
    let mut last_p: Option<(usize, bool)> = None;
    loop {
        let start = r.buffer_position() as usize;
        match r.read_event().map_err(|e| e.to_string())? {
            Event::Start(e) | Event::Empty(e) => {
                let name = e.name().as_ref().to_vec();
                let local = name.rsplit(|b| *b == b':').next().unwrap_or(&name).to_vec();
                if local == b"comment" {
                    in_target = parts::attr(&e, "id").as_deref() == Some(id);
                } else if in_target && local == b"p" {
                    last_p = Some((start, parts::attr(&e, "paraId").is_some()));
                }
            }
            Event::End(e) => {
                let name = e.name().as_ref().to_vec();
                let local = name.rsplit(|b| *b == b':').next().unwrap_or(&name).to_vec();
                if local == b"comment" && in_target {
                    break;
                }
            }
            Event::Eof => break,
            _ => {}
        }
    }
    let Some((at, has)) = last_p else {
        return Err(format!("comment {id} has no paragraph to give an id"));
    };
    if has {
        return Ok(xml.to_string());
    }
    let w14 = root_prefix(xml, W14_NS).ok_or("the w14 namespace is not declared")?;
    // `<w:p` is followed by whitespace, `>` or `/`; insert the attribute
    // right after the element name.
    let name_end = xml[at..]
        .find(|c: char| c.is_whitespace() || c == '>' || c == '/')
        .map(|n| at + n)
        .ok_or("a malformed paragraph tag")?;
    Ok(format!(
        "{} {w14}:paraId=\"{para_id}\"{}",
        &xml[..name_end],
        &xml[name_end..]
    ))
}

fn comment_xml(
    c: &str,
    w14: Option<&str>,
    id: &str,
    author: &str,
    date: &str,
    text: &str,
    para_id: &str,
) -> String {
    let initials: String = author
        .split_whitespace()
        .filter_map(|w| w.chars().next())
        .take(4)
        .collect();
    let mut out = format!(
        "<{c}:comment {c}:id=\"{id}\" {c}:author=\"{}\" {c}:date=\"{}\" {c}:initials=\"{}\">",
        escape(author),
        escape(date),
        escape(&initials)
    );
    let lines: Vec<&str> = text.split('\n').collect();
    for (i, line) in lines.iter().enumerate() {
        let last = i + 1 == lines.len();
        match (last, w14) {
            (true, Some(w14)) => out.push_str(&format!(
                "<{c}:p {w14}:paraId=\"{para_id}\" {w14}:textId=\"77777777\">"
            )),
            _ => out.push_str(&format!("<{c}:p>")),
        }
        if i == 0 {
            out.push_str(&format!("<{c}:r><{c}:annotationRef/></{c}:r>"));
        }
        out.push_str(&format!("<{c}:r>"));
        for (j, piece) in line.split('\t').enumerate() {
            if j > 0 {
                out.push_str(&format!("<{c}:tab/>"));
            }
            if !piece.is_empty() {
                out.push_str(&format!(
                    "<{c}:t xml:space=\"preserve\">{}</{c}:t>",
                    escape(piece)
                ));
            }
        }
        out.push_str(&format!("</{c}:r></{c}:p>"));
    }
    out.push_str(&format!("</{c}:comment>"));
    out
}

/// The prefix the root element binds to `ns`, if any.
fn root_prefix(xml: &str, ns: &str) -> Option<String> {
    let mut r = Reader::from_str(xml);
    loop {
        match r.read_event().ok()? {
            Event::Start(e) | Event::Empty(e) => {
                return e.attributes().with_checks(false).flatten().find_map(|a| {
                    let key = a.key.as_ref();
                    (a.value.as_ref() == ns.as_bytes())
                        .then(|| key.strip_prefix(b"xmlns:"))
                        .flatten()
                        .map(|p| String::from_utf8_lossy(p).into_owned())
                });
            }
            Event::Eof => return None,
            _ => {}
        }
    }
}

/// Make sure the root declares `ns`, adding `xmlns:<preferred>` when `need`
/// and it is missing. Returns the XML and the prefix in use, if any.
fn ensure_ns(
    xml: &str,
    ns: &str,
    preferred: &str,
    need: bool,
) -> Result<(String, Option<String>), String> {
    if let Some(p) = root_prefix(xml, ns) {
        return Ok((xml.to_string(), Some(p)));
    }
    if !need {
        return Ok((xml.to_string(), None));
    }
    let (start, name_end) = root_tag(xml)?;
    // A prefix already bound to something else is not reused.
    let mut prefix = preferred.to_string();
    while xml[start..].contains(&format!("xmlns:{prefix}=")) {
        prefix.push('x');
    }
    Ok((
        format!(
            "{} xmlns:{prefix}=\"{ns}\"{}",
            &xml[..name_end],
            &xml[name_end..]
        ),
        Some(prefix),
    ))
}

/// The root element's start offset and the offset just after its name.
fn root_tag(xml: &str) -> Result<(usize, usize), String> {
    let mut r = Reader::from_str(xml);
    loop {
        let start = r.buffer_position() as usize;
        match r.read_event().map_err(|e| e.to_string())? {
            Event::Start(e) | Event::Empty(e) => {
                let len = e.name().as_ref().len();
                return Ok((start, start + 1 + len));
            }
            Event::Eof => return Err("no root element".into()),
            _ => {}
        }
    }
}

/// Insert `element` as the root's last child, expanding a self-closed root.
fn insert_before_root_end(xml: &str, element: &str) -> Result<String, String> {
    let mut r = Reader::from_str(xml);
    let mut depth = 0usize;
    loop {
        let start = r.buffer_position() as usize;
        let event = r.read_event().map_err(|e| e.to_string())?;
        let end = r.buffer_position() as usize;
        match event {
            Event::Empty(e) if depth == 0 => {
                let name = String::from_utf8_lossy(e.name().as_ref()).into_owned();
                let tag = &xml[start..end];
                let open = tag.strip_suffix("/>").ok_or("a malformed root")?;
                return Ok(format!(
                    "{}{}>{element}</{name}>{}",
                    &xml[..start],
                    open.trim_end(),
                    &xml[end..]
                ));
            }
            Event::Start(_) => depth += 1,
            Event::End(_) => {
                depth -= 1;
                if depth == 0 {
                    return Ok(format!("{}{element}{}", &xml[..start], &xml[start..]));
                }
            }
            Event::Eof => return Err("the part has no root element".into()),
            _ => {}
        }
    }
}

fn sibling(part: &str, name: &str) -> String {
    match part.rsplit_once('/') {
        Some((dir, _)) => format!("{dir}/{name}"),
        None => name.to_string(),
    }
}

fn relative_target(source: &str, target: &str) -> String {
    let dir = source.rsplit_once('/').map(|(d, _)| d).unwrap_or("");
    target
        .strip_prefix(dir)
        .and_then(|t| t.strip_prefix('/'))
        .map(str::to_string)
        .unwrap_or_else(|| format!("/{target}"))
}

fn escape(s: &str) -> String {
    quick_xml::escape::escape(s).into_owned()
}

/// Line endings normalized, and every character XML 1.0 cannot carry
/// dropped (a stray control character would make the part unreadable).
fn clean_text(s: &str) -> String {
    s.replace("\r\n", "\n")
        .replace('\r', "\n")
        .chars()
        .filter(|c| matches!(c, '\n' | '\t') || !c.is_control())
        .filter(|c| !matches!(*c as u32, 0xFFFE | 0xFFFF))
        .collect()
}

pub fn utf16_slice(s: &str, start: usize, end: usize) -> String {
    let units: Vec<u16> = s.encode_utf16().collect();
    let end = end.min(units.len());
    let start = start.min(end);
    String::from_utf16_lossy(&units[start..end])
}

/// Now, as Word writes it: `2026-11-21T09:30:00Z`.
pub fn now_iso() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    iso_from_unix(secs)
}

pub fn iso_from_unix(secs: i64) -> String {
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    // Howard Hinnant's civil_from_days.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}
