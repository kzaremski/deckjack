//! One pass over `word/document.xml`: the reading view's blocks, and where
//! every run sits in the bytes.
//!
//! The reader and the patcher share this pass on purpose. A comment is
//! anchored by the reading view (paragraph `p/N`, UTF-16 offsets into its
//! visible text) and placed by the patcher; if the two counted paragraphs or
//! characters differently, a comment would land on the wrong words. With one
//! scanner there is one definition of both.
//!
//! **Paragraph numbering.** Every `w:p` in the body is numbered in document
//! order, including those inside tables and block content controls, and
//! excluding those inside text boxes and other drawings (whose subtrees are
//! skipped whole). `p/N` is the Nth. That is flatter than the brief's
//! `tbl/2/r3/c1/p/0` and just as stable for an unchanged file.
//!
//! **Visible text.** `w:t` and `w:delText` (deleted text is shown, struck
//! through, never silently accepted), a tab or a break as one character,
//! a non-breaking hyphen and a symbol as one character. Field codes
//! (`w:instrText`) are not visible; a field's result is. Drawings and
//! equations are placeholders with no text.
//!
//! **What is skipped.** Any element outside the main WordprocessingML
//! namespace (`mc:AlternateContent`, `m:oMath`, `w14:*`, ...) is skipped with
//! its subtree; inside a run it becomes an object placeholder. Unknown
//! elements in the main namespace are descended into, so content inside a
//! wrapper this file has never heard of is still read.

use std::collections::{BTreeMap, BTreeSet};

use quick_xml::events::{BytesStart, Event};
use quick_xml::reader::Reader;
use serde::Serialize;

pub const MAIN_NS: &str = "http://schemas.openxmlformats.org/wordprocessingml/2006/main";
pub const STRICT_NS: &str = "http://purl.oclc.org/ooxml/wordprocessingml/main";

/// Deeper than any real document nests, and shallow enough that the
/// recursion below cannot exhaust the stack.
const MAX_DEPTH: usize = 200;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ScanError {
    #[error("the document is not well-formed XML: {0}")]
    Xml(String),
    #[error("the document declares a DTD, which a Word document never does; refusing it")]
    Dtd,
    #[error("the document nests elements more than {MAX_DEPTH} deep")]
    TooDeep,
    #[error("the document does not use the WordprocessingML namespace")]
    NotWord,
}

// ---------------------------------------------------------------------------
// The reading view
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Block {
    Paragraph(Para),
    Table(Table),
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Para {
    /// `p/N`.
    pub anchor: String,
    pub style: Option<String>,
    /// 1-9, when the paragraph is a heading.
    pub heading: Option<u8>,
    /// The list it is an item of, when it is one.
    pub list: Option<ListItem>,
    pub inlines: Vec<Inline>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ListItem {
    pub num_id: String,
    pub level: u8,
    /// The marker to show: `•`, `3.`, `b)`. Filled in by the numbering pass.
    pub marker: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Inline {
    Text {
        text: String,
        /// Index into [`Scan::changes`].
        #[serde(skip_serializing_if = "Option::is_none")]
        change: Option<usize>,
        /// Ids of the comments whose range covers this text.
        #[serde(skip_serializing_if = "Vec::is_empty")]
        comments: Vec<String>,
    },
    /// A picture, chart, text box or equation, shown as a placeholder.
    Object { what: String },
    /// Where a comment is attached, for a comment with no range.
    CommentMark { id: String },
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Table {
    pub rows: Vec<Vec<Cell>>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Cell {
    /// Columns spanned (`w:gridSpan`), 1 for an ordinary cell.
    pub span: u32,
    pub blocks: Vec<Block>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ChangeKind {
    Insertion,
    Deletion,
    MoveFrom,
    MoveTo,
}

/// A tracked change, as the reading view and `office.text` list it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Change {
    pub id: String,
    pub kind: ChangeKind,
    pub author: Option<String>,
    pub date: Option<String>,
    /// The paragraph it is in, `p/N`.
    pub anchor: String,
    pub text: String,
}

// ---------------------------------------------------------------------------
// Where things are, for the patcher
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Span {
    pub start: usize,
    pub end: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChildKind {
    /// `w:t` or `w:delText`: `content` is the escaped text between its tags.
    Text {
        tag: String,
        content: Span,
        /// CDATA inside: cannot be cut by offset arithmetic.
        cdata: bool,
    },
    /// Anything else in a run: a tab is 1 unit, a field character 0.
    Atom,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Child {
    pub span: Span,
    pub units: usize,
    pub kind: ChildKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Run {
    /// The whole `<w:r>…</w:r>`.
    pub span: Span,
    /// The opening tag, copied onto each piece when the run is split.
    pub open: Span,
    /// `<w:rPr>…</w:rPr>`, copied onto each piece too.
    pub props: Option<Span>,
    /// What follows the properties, in order.
    pub children: Vec<Child>,
    /// Where the run's text starts in the paragraph, UTF-16 units.
    pub offset: usize,
    pub units: usize,
    /// Indices into [`ParaPos::wrappers`], outermost first.
    pub wrappers: Vec<usize>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Wrapper {
    /// From its opening tag to just past its closing tag.
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParaPos {
    pub span: Span,
    /// `false` for `<w:p/>`.
    pub has_body: bool,
    pub runs: Vec<Run>,
    pub wrappers: Vec<Wrapper>,
    /// Visible length, UTF-16 units.
    pub units: usize,
}

/// Where one comment's markers are in the body.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Markers {
    pub range_start: Option<Span>,
    pub range_end: Option<Span>,
    /// The run holding `w:commentReference`.
    pub reference_run: Option<Span>,
}

/// Paragraph style facts the scanner needs, from `styles.xml`.
#[derive(Debug, Clone, Default)]
pub struct Styles {
    /// style id → (heading level, list numbering)
    pub paragraph: BTreeMap<String, StyleFacts>,
}

#[derive(Debug, Clone, Default)]
pub struct StyleFacts {
    pub heading: Option<u8>,
    pub num: Option<(String, u8)>,
}

/// Everything one pass learns.
#[derive(Debug, Clone)]
pub struct Scan {
    pub blocks: Vec<Block>,
    pub changes: Vec<Change>,
    pub paragraphs: Vec<ParaPos>,
    pub markers: BTreeMap<String, Markers>,
    /// Every comment id seen in the body.
    pub comment_ids: BTreeSet<String>,
    /// The prefix bound to the main namespace (`w` in every Word file).
    pub prefix: String,
    /// Strict OOXML (ISO 29500 strict) rather than transitional.
    pub strict: bool,
}

impl Scan {
    pub fn paragraph(&self, anchor: &str) -> Option<(usize, &ParaPos)> {
        let index: usize = anchor.strip_prefix("p/")?.parse().ok()?;
        self.paragraphs.get(index).map(|p| (index, p))
    }
}

// ---------------------------------------------------------------------------
// The pass
// ---------------------------------------------------------------------------

pub fn scan(xml: &str, styles: &Styles) -> Result<Scan, ScanError> {
    let mut s = Scanner {
        r: Reader::from_str(xml),
        prefix: Vec::new(),
        strict: false,
        depth: 0,
        styles,
        out: Scan {
            blocks: Vec::new(),
            changes: Vec::new(),
            paragraphs: Vec::new(),
            markers: BTreeMap::new(),
            comment_ids: BTreeSet::new(),
            prefix: String::new(),
            strict: false,
        },
        open_comments: BTreeSet::new(),
    };
    s.r.config_mut().check_end_names = true;
    s.document()?;
    s.out.prefix = String::from_utf8_lossy(&s.prefix).into_owned();
    s.out.strict = s.strict;
    Ok(s.out)
}

struct Scanner<'a, 's> {
    r: Reader<&'a [u8]>,
    prefix: Vec<u8>,
    strict: bool,
    depth: usize,
    styles: &'s Styles,
    out: Scan,
    open_comments: BTreeSet<String>,
}

/// The run-level context a run inherits from the wrappers around it.
#[derive(Clone, Default)]
struct Context {
    change: Option<usize>,
    wrappers: Vec<usize>,
}

/// A paragraph being read.
struct ParaBuild {
    para: Para,
    pos: ParaPos,
    units: usize,
}

type Ev<'a> = (Event<'a>, Span);

impl<'a> Scanner<'a, '_> {
    fn next(&mut self) -> Result<Ev<'a>, ScanError> {
        let start = self.r.buffer_position() as usize;
        let event = self
            .r
            .read_event()
            .map_err(|e| ScanError::Xml(e.to_string()))?;
        let end = self.r.buffer_position() as usize;
        if let Event::DocType(_) = event {
            return Err(ScanError::Dtd);
        }
        Ok((event, Span { start, end }))
    }

    /// The local name if `e` is in the main namespace, else `None`.
    fn w<'e>(&self, name: &'e [u8]) -> Option<&'e [u8]> {
        if self.prefix.is_empty() {
            return (!name.contains(&b':')).then_some(name);
        }
        let rest = name.strip_prefix(self.prefix.as_slice())?;
        rest.strip_prefix(b":")
    }

    fn attr(&self, e: &BytesStart<'_>, local: &str) -> Option<String> {
        for a in e.attributes().with_checks(false).flatten() {
            let key = a.key.as_ref();
            let matches = match self.w(key) {
                Some(l) => l == local.as_bytes(),
                None => key == local.as_bytes(),
            };
            if matches {
                return a
                    .normalized_value(quick_xml::XmlVersion::Implicit1_0)
                    .ok()
                    .map(|v| v.into_owned());
            }
        }
        None
    }

    /// Skip the subtree an already-read start tag opened.
    fn skip(&mut self, e: &BytesStart<'_>) -> Result<Span, ScanError> {
        let end = self
            .r
            .read_to_end(e.name())
            .map_err(|err| ScanError::Xml(err.to_string()))?;
        Ok(Span {
            start: end.start as usize,
            end: self.r.buffer_position() as usize,
        })
    }

    fn enter(&mut self) -> Result<(), ScanError> {
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            return Err(ScanError::TooDeep);
        }
        Ok(())
    }

    fn leave(&mut self) {
        self.depth -= 1;
    }

    fn document(&mut self) -> Result<(), ScanError> {
        // The root: bind the main prefix.
        loop {
            let (event, _) = self.next()?;
            match event {
                Event::Start(e) => {
                    let mut bound = false;
                    for a in e.attributes().with_checks(false).flatten() {
                        let key = a.key.as_ref();
                        let value = a.value.as_ref();
                        let strict = if value == MAIN_NS.as_bytes() {
                            false
                        } else if value == STRICT_NS.as_bytes() {
                            true
                        } else {
                            continue;
                        };
                        if let Some(p) = key.strip_prefix(b"xmlns:") {
                            self.prefix = p.to_vec();
                        } else if key == b"xmlns" {
                            self.prefix.clear();
                        } else {
                            continue;
                        }
                        self.strict = strict;
                        bound = true;
                    }
                    if !bound || self.w(e.name().as_ref()) != Some(b"document") {
                        return Err(ScanError::NotWord);
                    }
                    break;
                }
                Event::Eof => return Err(ScanError::NotWord),
                _ => {}
            }
        }
        loop {
            let (event, _) = self.next()?;
            match event {
                Event::Start(e) if self.w(e.name().as_ref()) == Some(b"body") => {
                    self.enter()?;
                    let blocks = self.blocks()?;
                    self.leave();
                    self.out.blocks = blocks;
                }
                Event::Start(e) => {
                    self.skip(&e)?;
                }
                Event::End(_) | Event::Eof => break,
                _ => {}
            }
        }
        Ok(())
    }

    /// Block content until the end tag of the current container.
    fn blocks(&mut self) -> Result<Vec<Block>, ScanError> {
        let mut out = Vec::new();
        loop {
            let (event, span) = self.next()?;
            match event {
                Event::Start(e) => {
                    let name = e.name().as_ref().to_vec();
                    match self.w(&name) {
                        Some(b"p") => {
                            self.enter()?;
                            let para = self.paragraph(span, true)?;
                            self.leave();
                            out.push(Block::Paragraph(para));
                        }
                        Some(b"tbl") => {
                            self.enter()?;
                            let table = self.table()?;
                            self.leave();
                            out.push(Block::Table(table));
                        }
                        Some(b"sectPr") | Some(b"sdtPr") | Some(b"sdtEndPr") | Some(b"tblPr")
                        | Some(b"customXmlPr") => {
                            self.skip(&e)?;
                        }
                        // sdt, sdtContent, customXml, and anything unknown in
                        // the main namespace: its blocks are the document's.
                        Some(_) => {
                            self.enter()?;
                            let inner = self.blocks()?;
                            self.leave();
                            out.extend(inner);
                        }
                        None => {
                            self.skip(&e)?;
                        }
                    }
                }
                Event::Empty(e) => {
                    let name = e.name().as_ref().to_vec();
                    if self.w(&name) == Some(b"p") {
                        let para = self.paragraph(span, false)?;
                        out.push(Block::Paragraph(para));
                    } else {
                        self.marker(&e, span);
                    }
                }
                Event::End(_) => return Ok(out),
                Event::Eof => return Err(ScanError::Xml("unexpected end of document".into())),
                _ => {}
            }
        }
    }

    fn table(&mut self) -> Result<Table, ScanError> {
        let mut table = Table::default();
        loop {
            let (event, _) = self.next()?;
            match event {
                Event::Start(e) => match self.w(e.name().as_ref()) {
                    Some(b"tr") => {
                        self.enter()?;
                        let row = self.row()?;
                        self.leave();
                        table.rows.push(row);
                    }
                    _ => {
                        self.skip(&e)?;
                    }
                },
                Event::End(_) => return Ok(table),
                Event::Eof => return Err(ScanError::Xml("unexpected end of document".into())),
                _ => {}
            }
        }
    }

    fn row(&mut self) -> Result<Vec<Cell>, ScanError> {
        let mut cells = Vec::new();
        loop {
            let (event, span) = self.next()?;
            match event {
                Event::Start(e) => match self.w(e.name().as_ref()) {
                    Some(b"tc") => {
                        self.enter()?;
                        let cell = self.cell()?;
                        self.leave();
                        cells.push(cell);
                    }
                    // A row-level content control around cells.
                    Some(b"sdt") | Some(b"sdtContent") | Some(b"customXml") => {
                        self.enter()?;
                        let inner = self.row()?;
                        self.leave();
                        cells.extend(inner);
                    }
                    _ => {
                        self.skip(&e)?;
                    }
                },
                Event::Empty(e) => {
                    self.marker(&e, span);
                }
                Event::End(_) => return Ok(cells),
                Event::Eof => return Err(ScanError::Xml("unexpected end of document".into())),
                _ => {}
            }
        }
    }

    fn cell(&mut self) -> Result<Cell, ScanError> {
        let mut cell = Cell {
            span: 1,
            blocks: Vec::new(),
        };
        loop {
            let (event, span) = self.next()?;
            match event {
                Event::Start(e) => {
                    let name = e.name().as_ref().to_vec();
                    match self.w(&name) {
                        Some(b"tcPr") => {
                            self.enter()?;
                            cell.span = self.cell_props()?;
                            self.leave();
                        }
                        Some(b"p") => {
                            self.enter()?;
                            let para = self.paragraph(span, true)?;
                            self.leave();
                            cell.blocks.push(Block::Paragraph(para));
                        }
                        Some(b"tbl") => {
                            self.enter()?;
                            let table = self.table()?;
                            self.leave();
                            cell.blocks.push(Block::Table(table));
                        }
                        Some(b"sdtPr") | Some(b"sdtEndPr") => {
                            self.skip(&e)?;
                        }
                        Some(_) => {
                            self.enter()?;
                            let inner = self.blocks()?;
                            self.leave();
                            cell.blocks.extend(inner);
                        }
                        None => {
                            self.skip(&e)?;
                        }
                    }
                }
                Event::Empty(e) => {
                    if self.w(e.name().as_ref()) == Some(b"p") {
                        let para = self.paragraph(span, false)?;
                        cell.blocks.push(Block::Paragraph(para));
                    } else {
                        self.marker(&e, span);
                    }
                }
                Event::End(_) => return Ok(cell),
                Event::Eof => return Err(ScanError::Xml("unexpected end of document".into())),
                _ => {}
            }
        }
    }

    fn cell_props(&mut self) -> Result<u32, ScanError> {
        let mut span = 1;
        loop {
            let (event, _) = self.next()?;
            match event {
                Event::Empty(e) if self.w(e.name().as_ref()) == Some(b"gridSpan") => {
                    span = self
                        .attr(&e, "val")
                        .and_then(|v| v.parse().ok())
                        .filter(|n| (1..=64).contains(n))
                        .unwrap_or(1);
                }
                Event::Start(e) => {
                    self.skip(&e)?;
                }
                Event::End(_) => return Ok(span),
                Event::Eof => return Err(ScanError::Xml("unexpected end of document".into())),
                _ => {}
            }
        }
    }

    /// A comment range marker or anything else empty at block or inline
    /// level. Records range markers and keeps the open set current.
    fn marker(&mut self, e: &BytesStart<'_>, span: Span) {
        let name = e.name().as_ref().to_vec();
        match self.w(&name) {
            Some(b"commentRangeStart") => {
                if let Some(id) = self.attr(e, "id") {
                    self.out.comment_ids.insert(id.clone());
                    self.out.markers.entry(id.clone()).or_default().range_start = Some(span);
                    self.open_comments.insert(id);
                }
            }
            Some(b"commentRangeEnd") => {
                if let Some(id) = self.attr(e, "id") {
                    self.out.comment_ids.insert(id.clone());
                    self.out.markers.entry(id.clone()).or_default().range_end = Some(span);
                    self.open_comments.remove(&id);
                }
            }
            _ => {}
        }
    }

    fn paragraph(&mut self, span: Span, has_body: bool) -> Result<Para, ScanError> {
        let index = self.out.paragraphs.len();
        // Reserve the index now: a nested paragraph (none should exist
        // outside skipped drawings, but a hostile file can try) gets a later
        // one instead of colliding.
        self.out.paragraphs.push(ParaPos {
            span,
            has_body,
            runs: Vec::new(),
            wrappers: Vec::new(),
            units: 0,
        });
        let mut build = ParaBuild {
            para: Para {
                anchor: format!("p/{index}"),
                ..Para::default()
            },
            pos: ParaPos {
                span,
                has_body,
                runs: Vec::new(),
                wrappers: Vec::new(),
                units: 0,
            },
            units: 0,
        };
        if has_body {
            self.inline(&mut build, &Context::default())?;
            build.pos.span.end = self.r.buffer_position() as usize;
        }
        build.pos.units = build.units;
        // Style-level heading and numbering, unless the paragraph says
        // otherwise.
        if let Some(style) = build
            .para
            .style
            .as_ref()
            .and_then(|s| self.styles.paragraph.get(s))
        {
            if build.para.heading.is_none() {
                build.para.heading = style.heading;
            }
            if build.para.list.is_none()
                && let Some((num, level)) = &style.num
            {
                build.para.list = Some(ListItem {
                    num_id: num.clone(),
                    level: *level,
                    marker: String::new(),
                });
            }
        }
        if build.para.list.as_ref().is_some_and(|l| l.num_id == "0") {
            build.para.list = None;
        }
        self.out.paragraphs[index] = build.pos;
        Ok(build.para)
    }

    fn para_props(&mut self, build: &mut ParaBuild) -> Result<(), ScanError> {
        loop {
            let (event, _) = self.next()?;
            match event {
                Event::Empty(e) => match self.w(e.name().as_ref()) {
                    Some(b"pStyle") => build.para.style = self.attr(&e, "val"),
                    Some(b"outlineLvl") => {
                        build.para.heading = self
                            .attr(&e, "val")
                            .and_then(|v| v.parse::<u8>().ok())
                            .filter(|l| *l < 9)
                            .map(|l| l + 1);
                    }
                    _ => {}
                },
                Event::Start(e) => match self.w(e.name().as_ref()) {
                    Some(b"numPr") => {
                        self.enter()?;
                        let (num, level) = self.num_props()?;
                        self.leave();
                        if let Some(num) = num {
                            build.para.list = Some(ListItem {
                                num_id: num,
                                level: level.unwrap_or(0),
                                marker: String::new(),
                            });
                        }
                    }
                    _ => {
                        self.skip(&e)?;
                    }
                },
                Event::End(_) => return Ok(()),
                Event::Eof => return Err(ScanError::Xml("unexpected end of document".into())),
                _ => {}
            }
        }
    }

    fn num_props(&mut self) -> Result<(Option<String>, Option<u8>), ScanError> {
        let (mut num, mut level) = (None, None);
        loop {
            let (event, _) = self.next()?;
            match event {
                Event::Empty(e) => match self.w(e.name().as_ref()) {
                    Some(b"numId") => num = self.attr(&e, "val"),
                    Some(b"ilvl") => {
                        level = self
                            .attr(&e, "val")
                            .and_then(|v| v.parse().ok())
                            .filter(|l| *l < 9)
                    }
                    _ => {}
                },
                Event::Start(e) => {
                    self.skip(&e)?;
                }
                Event::End(_) => return Ok((num, level)),
                Event::Eof => return Err(ScanError::Xml("unexpected end of document".into())),
                _ => {}
            }
        }
    }

    /// Paragraph-level content until the end of the current element.
    fn inline(&mut self, build: &mut ParaBuild, cx: &Context) -> Result<(), ScanError> {
        loop {
            let (event, span) = self.next()?;
            match event {
                Event::Start(e) => {
                    let name = e.name().as_ref().to_vec();
                    match self.w(&name) {
                        Some(b"pPr") => {
                            self.enter()?;
                            self.para_props(build)?;
                            self.leave();
                        }
                        Some(b"r") => {
                            self.enter()?;
                            self.run(build, cx, span, true)?;
                            self.leave();
                        }
                        Some(b"sdtPr") | Some(b"sdtEndPr") | Some(b"customXmlPr")
                        | Some(b"smartTagPr") => {
                            self.skip(&e)?;
                        }
                        Some(local) => {
                            let kind = match local {
                                b"ins" => Some(ChangeKind::Insertion),
                                b"del" => Some(ChangeKind::Deletion),
                                b"moveFrom" => Some(ChangeKind::MoveFrom),
                                b"moveTo" => Some(ChangeKind::MoveTo),
                                _ => None,
                            };
                            let mut inner = cx.clone();
                            if let Some(kind) = kind {
                                self.out.changes.push(Change {
                                    id: self.attr(&e, "id").unwrap_or_default(),
                                    kind,
                                    author: self.attr(&e, "author"),
                                    date: self.attr(&e, "date"),
                                    anchor: build.para.anchor.clone(),
                                    text: String::new(),
                                });
                                inner.change = Some(self.out.changes.len() - 1);
                            }
                            let wrapper = build.pos.wrappers.len();
                            build.pos.wrappers.push(Wrapper { span });
                            inner.wrappers.push(wrapper);
                            self.enter()?;
                            self.inline(build, &inner)?;
                            self.leave();
                            build.pos.wrappers[wrapper].span.end =
                                self.r.buffer_position() as usize;
                        }
                        None => {
                            // An equation or alternate content at paragraph
                            // level: shown as a placeholder.
                            let what = object_name(&name);
                            self.skip(&e)?;
                            build.para.inlines.push(Inline::Object { what });
                        }
                    }
                }
                Event::Empty(e) => {
                    let name = e.name().as_ref().to_vec();
                    match self.w(&name) {
                        Some(b"r") => {
                            self.run(build, cx, span, false)?;
                        }
                        Some(b"commentRangeStart") | Some(b"commentRangeEnd") => {
                            self.marker(&e, span);
                        }
                        _ => {}
                    }
                }
                Event::End(_) => return Ok(()),
                Event::Eof => return Err(ScanError::Xml("unexpected end of document".into())),
                _ => {}
            }
        }
    }

    fn run(
        &mut self,
        build: &mut ParaBuild,
        cx: &Context,
        span: Span,
        has_body: bool,
    ) -> Result<(), ScanError> {
        let mut run = Run {
            span,
            open: span,
            props: None,
            children: Vec::new(),
            offset: build.units,
            units: 0,
            wrappers: cx.wrappers.clone(),
        };
        let mut text = String::new();
        // The comments this run references, so closing the run touches only
        // their markers rather than every marker in the document.
        let mut referenced: Vec<String> = Vec::new();
        if has_body {
            loop {
                let (event, cspan) = self.next()?;
                match event {
                    Event::Start(e) => {
                        let name = e.name().as_ref().to_vec();
                        match self.w(&name) {
                            Some(b"rPr") => {
                                let skipped = self.skip(&e)?;
                                run.props = Some(Span {
                                    start: cspan.start,
                                    end: skipped.end,
                                });
                            }
                            Some(local @ (b"t" | b"delText")) => {
                                let tag = String::from_utf8_lossy(local).into_owned();
                                let (content, piece, cdata) = self.text_content()?;
                                let units = piece.encode_utf16().count();
                                text.push_str(&piece);
                                run.children.push(Child {
                                    span: Span {
                                        start: cspan.start,
                                        end: self.r.buffer_position() as usize,
                                    },
                                    units,
                                    kind: ChildKind::Text {
                                        tag,
                                        content,
                                        cdata,
                                    },
                                });
                            }
                            Some(local @ (b"drawing" | b"pict" | b"object")) => {
                                let what = if local == b"object" {
                                    "embedded object"
                                } else {
                                    "picture"
                                };
                                let what =
                                    self.object_title(&e).unwrap_or_else(|| what.to_string());
                                let skipped = self.skip(&e)?;
                                flush(build, cx, &mut text, &self.open_comments);
                                build.para.inlines.push(Inline::Object { what });
                                run.children.push(atom(cspan.start, skipped.end, 0));
                            }
                            Some(local) => {
                                let units = atom_units(local);
                                let piece = atom_text(local, None);
                                let skipped = self.skip(&e)?;
                                text.push_str(&piece);
                                run.children.push(atom(cspan.start, skipped.end, units));
                            }
                            None => {
                                let what = object_name(&name);
                                let skipped = self.skip(&e)?;
                                flush(build, cx, &mut text, &self.open_comments);
                                build.para.inlines.push(Inline::Object { what });
                                run.children.push(atom(cspan.start, skipped.end, 0));
                            }
                        }
                    }
                    Event::Empty(e) => {
                        let name = e.name().as_ref().to_vec();
                        let local = self.w(&name).map(<[u8]>::to_vec);
                        match local.as_deref() {
                            Some(b"commentReference") => {
                                if let Some(id) = self.attr(&e, "id") {
                                    self.out.comment_ids.insert(id.clone());
                                    // Filled in with the run's span below.
                                    self.out
                                        .markers
                                        .entry(id.clone())
                                        .or_default()
                                        .reference_run = Some(Span {
                                        start: span.start,
                                        end: 0,
                                    });
                                    flush(build, cx, &mut text, &self.open_comments);
                                    referenced.push(id.clone());
                                    if self.out.markers[&id].range_start.is_none() {
                                        build.para.inlines.push(Inline::CommentMark { id });
                                    }
                                }
                                run.children.push(atom(cspan.start, cspan.end, 0));
                            }
                            Some(l) => {
                                let sym = if l == b"sym" {
                                    self.attr(&e, "char")
                                } else {
                                    None
                                };
                                let units = atom_units(l);
                                text.push_str(&atom_text(l, sym.as_deref()));
                                run.children.push(atom(cspan.start, cspan.end, units));
                            }
                            None => run.children.push(atom(cspan.start, cspan.end, 0)),
                        }
                    }
                    Event::End(_) => break,
                    Event::Eof => {
                        return Err(ScanError::Xml("unexpected end of document".into()));
                    }
                    _ => {}
                }
            }
            run.span.end = self.r.buffer_position() as usize;
            run.open = Span {
                start: span.start,
                end: span.end,
            };
        }
        run.units = run.children.iter().map(|c| c.units).sum();
        build.units += run.units;
        flush(build, cx, &mut text, &self.open_comments);
        // A commentReference recorded above points at this run.
        let run_span = run.span;
        for id in &referenced {
            if let Some(r) = self
                .out
                .markers
                .get_mut(id)
                .and_then(|m| m.reference_run.as_mut())
                && r.start == run_span.start
                && r.end == 0
            {
                r.end = run_span.end;
            }
        }
        build.pos.runs.push(run);
        Ok(())
    }

    /// The text inside `w:t`/`w:delText`, its raw byte span, and whether it
    /// held CDATA.
    fn text_content(&mut self) -> Result<(Span, String, bool), ScanError> {
        let start = self.r.buffer_position() as usize;
        let mut text = String::new();
        let mut cdata = false;
        loop {
            let (event, span) = self.next()?;
            match event {
                Event::Text(t) => {
                    text.push_str(&t.decode().map_err(|e| ScanError::Xml(e.to_string()))?)
                }
                Event::GeneralRef(r) => {
                    let name = r.decode().map_err(|e| ScanError::Xml(e.to_string()))?;
                    match r.resolve_char_ref() {
                        Ok(Some(c)) => text.push(c),
                        _ => match quick_xml::escape::resolve_predefined_entity(&name) {
                            Some(s) => text.push_str(s),
                            None => {
                                return Err(ScanError::Xml(format!("unknown entity &{name};")));
                            }
                        },
                    }
                }
                Event::CData(c) => {
                    cdata = true;
                    text.push_str(&String::from_utf8_lossy(&c.into_inner()));
                }
                Event::End(_) => {
                    return Ok((
                        Span {
                            start,
                            end: span.start,
                        },
                        text,
                        cdata,
                    ));
                }
                Event::Start(e) => {
                    self.skip(&e)?;
                }
                Event::Eof => return Err(ScanError::Xml("unexpected end of document".into())),
                _ => {}
            }
        }
    }

    /// `wp:docPr name="Picture 1"` is buried in the drawing; reading it means
    /// parsing the subtree, which `skip` does not. The generic word is fine.
    fn object_title(&self, _e: &BytesStart<'_>) -> Option<String> {
        None
    }
}

fn atom(start: usize, end: usize, units: usize) -> Child {
    Child {
        span: Span { start, end },
        units,
        kind: ChildKind::Atom,
    }
}

/// The visible length of a non-text run child.
fn atom_units(local: &[u8]) -> usize {
    match local {
        b"tab" | b"ptab" | b"br" | b"cr" | b"noBreakHyphen" | b"sym" => 1,
        _ => 0,
    }
}

fn atom_text(local: &[u8], sym: Option<&str>) -> String {
    match local {
        b"tab" | b"ptab" => "\t".into(),
        b"br" | b"cr" => "\n".into(),
        b"noBreakHyphen" => "\u{2011}".into(),
        b"sym" => sym
            .and_then(|hex| u32::from_str_radix(hex, 16).ok())
            // Symbol-font code points (F0xx) mean nothing outside that font.
            .and_then(|code| {
                if (0xF000..=0xF0FF).contains(&code) {
                    None
                } else {
                    char::from_u32(code)
                }
            })
            .filter(|c| c.len_utf16() == 1)
            .unwrap_or('\u{FFFD}')
            .to_string(),
        _ => String::new(),
    }
}

fn object_name(name: &[u8]) -> String {
    let local = name.rsplit(|b| *b == b':').next().unwrap_or(name);
    match local {
        b"oMath" | b"oMathPara" => "equation".into(),
        _ => "object".into(),
    }
}

/// Close the text accumulated so far into an inline.
fn flush(build: &mut ParaBuild, cx: &Context, text: &mut String, open: &BTreeSet<String>) {
    if text.is_empty() {
        return;
    }
    let piece = std::mem::take(text);
    let comments: Vec<String> = open.iter().cloned().collect();
    // Merge with the previous inline when nothing about it differs, so a
    // paragraph is not a hundred one-word spans because Word split its runs.
    if let Some(Inline::Text {
        text: prev,
        change: prev_change,
        comments: prev_comments,
    }) = build.para.inlines.last_mut()
        && *prev_change == cx.change
        && *prev_comments == comments
    {
        prev.push_str(&piece);
        return;
    }
    build.para.inlines.push(Inline::Text {
        text: piece,
        change: cx.change,
        comments,
    });
}

/// Fill each change's text from the inlines that carry it.
pub fn collect_change_text(scan: &mut Scan) {
    fn walk(blocks: &[Block], changes: &mut [Change]) {
        for block in blocks {
            match block {
                Block::Paragraph(p) => {
                    for inline in &p.inlines {
                        if let Inline::Text {
                            text,
                            change: Some(i),
                            ..
                        } = inline
                            && let Some(c) = changes.get_mut(*i)
                        {
                            c.text.push_str(text);
                        }
                    }
                }
                Block::Table(t) => {
                    for row in &t.rows {
                        for cell in row {
                            walk(&cell.blocks, changes);
                        }
                    }
                }
            }
        }
    }
    let blocks = std::mem::take(&mut scan.blocks);
    walk(&blocks, &mut scan.changes);
    scan.blocks = blocks;
}
