//! The small parts around `document.xml`: relationships, styles, numbering
//! and comments. Each is read by local name and attribute, with anything
//! unexpected passed over rather than refused: these parts decide how text
//! is labeled, not what it says, so a part this file misreads costs a
//! heading level or a list marker, never a word.

use std::collections::{BTreeMap, HashSet};

use quick_xml::events::{BytesStart, Event};
use quick_xml::reader::Reader;
use serde::Serialize;

use super::scan::{Block, Inline, StyleFacts, Styles};

/// A relationship from a `.rels` part.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rel {
    pub id: String,
    pub kind: String,
    pub target: String,
    pub external: bool,
}

/// The local part of a qualified name.
fn local(name: &[u8]) -> &[u8] {
    name.rsplit(|b| *b == b':').next().unwrap_or(name)
}

/// An attribute by local name, ignoring its prefix.
pub fn attr(e: &BytesStart<'_>, name: &str) -> Option<String> {
    e.attributes()
        .with_checks(false)
        .flatten()
        .find(|a| local(a.key.as_ref()) == name.as_bytes() && !a.key.as_ref().starts_with(b"xmlns"))
        .and_then(|a| {
            a.normalized_value(quick_xml::XmlVersion::Implicit1_0)
                .ok()
                .map(|v| v.into_owned())
        })
}

/// Every start or empty element, as (local name, element, is a start), in
/// order. Fine for the flat parts this module reads; never used on
/// `document.xml`.
fn walk(xml: &str, mut each: impl FnMut(&[u8], &BytesStart<'_>, bool)) {
    let mut r = Reader::from_str(xml);
    loop {
        match r.read_event() {
            Ok(Event::Start(e)) => each(local(e.name().as_ref()), &e, true),
            Ok(Event::Empty(e)) => each(local(e.name().as_ref()), &e, false),
            Ok(Event::Eof) | Ok(Event::DocType(_)) | Err(_) => break,
            _ => {}
        }
    }
}

pub fn rels(xml: &str) -> Vec<Rel> {
    let mut out = Vec::new();
    walk(xml, |name, e, _| {
        if name == b"Relationship" {
            out.push(Rel {
                id: attr(e, "Id").unwrap_or_default(),
                kind: attr(e, "Type").unwrap_or_default(),
                target: attr(e, "Target").unwrap_or_default(),
                external: attr(e, "TargetMode").as_deref() == Some("External"),
            });
        }
    });
    out
}

/// Relationship types, transitional and strict, by their final segment.
pub fn rel_is(rel: &Rel, kind: &str) -> bool {
    rel.kind.rsplit('/').next() == Some(kind)
}

/// The part a relationship in `source`'s `.rels` points at, as a package
/// part name. `None` for an external target or one that climbs out.
pub fn resolve(source: &str, target: &str) -> Option<String> {
    let mut parts: Vec<&str> = if let Some(absolute) = target.strip_prefix('/') {
        return normalize(absolute.split('/').collect());
    } else {
        source.split('/').collect()
    };
    parts.pop();
    parts.extend(target.split('/'));
    normalize(parts)
}

fn normalize(parts: Vec<&str>) -> Option<String> {
    let mut out: Vec<&str> = Vec::new();
    for part in parts {
        match part {
            "" | "." => {}
            ".." => {
                out.pop()?;
            }
            other => out.push(other),
        }
    }
    (!out.is_empty()).then(|| out.join("/"))
}

/// The `.rels` part that belongs to `part`.
pub fn rels_part(part: &str) -> String {
    match part.rsplit_once('/') {
        Some((dir, file)) => format!("{dir}/_rels/{file}.rels"),
        None => format!("_rels/{part}.rels"),
    }
}

/// The document part `_rels/.rels` names.
pub fn main_part(root_rels: &str) -> Option<String> {
    rels(root_rels)
        .into_iter()
        .find(|r| rel_is(r, "officeDocument") && !r.external)
        .and_then(|r| resolve("", &r.target))
}

// ---------------------------------------------------------------------------
// Styles
// ---------------------------------------------------------------------------

pub fn styles(xml: &str) -> Styles {
    #[derive(Default)]
    struct Raw {
        name: Option<String>,
        based_on: Option<String>,
        outline: Option<u8>,
        num: Option<String>,
        level: Option<u8>,
    }
    let mut raw: BTreeMap<String, Raw> = BTreeMap::new();
    let mut current: Option<String> = None;
    walk(xml, |name, e, open| match name {
        b"style" if open => {
            current = (attr(e, "type").as_deref() == Some("paragraph"))
                .then(|| attr(e, "styleId"))
                .flatten();
            if let Some(id) = &current {
                raw.entry(id.clone()).or_default();
            }
        }
        _ => {
            let Some(entry) = current.as_ref().and_then(|id| raw.get_mut(id)) else {
                return;
            };
            match name {
                b"name" => entry.name = attr(e, "val"),
                b"basedOn" => entry.based_on = attr(e, "val"),
                b"outlineLvl" => {
                    entry.outline = attr(e, "val")
                        .and_then(|v| v.parse().ok())
                        .filter(|l| *l < 9)
                }
                b"numId" => entry.num = attr(e, "val"),
                b"ilvl" => {
                    entry.level = attr(e, "val")
                        .and_then(|v| v.parse().ok())
                        .filter(|l| *l < 9)
                }
                _ => {}
            }
        }
    });
    let mut out = Styles::default();
    for id in raw.keys() {
        // Follow basedOn a few steps for what the style does not say itself.
        let mut heading = None;
        let mut num = None;
        let mut at = Some(id.clone());
        for _ in 0..8 {
            let Some(here) = at.as_ref().and_then(|i| raw.get(i)) else {
                break;
            };
            if heading.is_none() {
                heading = here.outline.map(|l| l + 1).or_else(|| {
                    let name = here.name.as_deref()?.to_ascii_lowercase();
                    if name == "title" {
                        return Some(1);
                    }
                    name.strip_prefix("heading ")?
                        .parse::<u8>()
                        .ok()
                        .filter(|l| (1..=9).contains(l))
                });
            }
            if num.is_none()
                && let Some(n) = &here.num
            {
                num = Some((n.clone(), here.level.unwrap_or(0)));
            }
            at = here.based_on.clone();
        }
        out.paragraph
            .insert(id.clone(), StyleFacts { heading, num });
    }
    out
}

// ---------------------------------------------------------------------------
// Numbering
// ---------------------------------------------------------------------------

/// The largest `w:start` honored. Word's user interface stops at 32767; a
/// larger value is clamped rather than counted from, so a hostile part
/// cannot ask for a marker billions of letters long.
const MAX_START: u32 = 32_767;

/// The longest marker produced, in characters. A real one is a few
/// characters ("iv.", "1.2.3"); this is a bound on hostile input, not a
/// style rule.
pub const MAX_MARKER: usize = 64;

#[derive(Debug, Clone, Default)]
struct Level {
    format: String,
    text: String,
    start: u32,
}

#[derive(Debug, Default)]
pub struct Numbering {
    /// numId → abstractNumId
    nums: BTreeMap<String, String>,
    /// abstractNumId → levels
    abstracts: BTreeMap<String, BTreeMap<u8, Level>>,
}

pub fn numbering(xml: &str) -> Numbering {
    let mut out = Numbering::default();
    let mut abstract_id: Option<String> = None;
    let mut num_id: Option<String> = None;
    let mut level: Option<u8> = None;
    walk(xml, |name, e, open| match name {
        b"abstractNum" if open => {
            abstract_id = attr(e, "abstractNumId");
            num_id = None;
        }
        b"num" if open => {
            num_id = attr(e, "numId");
            abstract_id = None;
        }
        b"abstractNumId" => {
            if let (Some(n), Some(a)) = (&num_id, attr(e, "val")) {
                out.nums.insert(n.clone(), a);
            }
        }
        b"lvl" if open => {
            level = attr(e, "ilvl")
                .and_then(|v| v.parse().ok())
                .filter(|l| *l < 9);
            if let (Some(a), Some(l)) = (&abstract_id, level) {
                out.abstracts.entry(a.clone()).or_default().insert(
                    l,
                    Level {
                        start: 1,
                        ..Level::default()
                    },
                );
            }
        }
        b"start" | b"numFmt" | b"lvlText" => {
            let (Some(a), Some(l)) = (&abstract_id, level) else {
                return;
            };
            let Some(entry) = out.abstracts.get_mut(a).and_then(|m| m.get_mut(&l)) else {
                return;
            };
            let val = attr(e, "val").unwrap_or_default();
            match name {
                b"start" => entry.start = val.parse().unwrap_or(1).min(MAX_START),
                b"numFmt" => entry.format = val,
                _ => entry.text = val,
            }
        }
        _ => {}
    });
    out
}

impl Numbering {
    fn level(&self, num: &str, level: u8) -> Option<&Level> {
        self.abstracts.get(self.nums.get(num)?)?.get(&level)
    }
}

/// Fill in every list item's marker, counting in document order.
pub fn number_lists(blocks: &mut [Block], numbering: &Numbering) {
    let mut counters: BTreeMap<String, [u32; 9]> = BTreeMap::new();
    fn walk(
        blocks: &mut [Block],
        numbering: &Numbering,
        counters: &mut BTreeMap<String, [u32; 9]>,
    ) {
        for block in blocks {
            match block {
                Block::Paragraph(p) => {
                    let Some(item) = p.list.as_mut() else {
                        continue;
                    };
                    let level = item.level.min(8) as usize;
                    let counts = counters.entry(item.num_id.clone()).or_insert([0; 9]);
                    let start = |l: usize| {
                        numbering
                            .level(&item.num_id, l as u8)
                            .map(|lv| lv.start)
                            .unwrap_or(1)
                    };
                    counts[level] = if counts[level] == 0 {
                        start(level)
                    } else {
                        counts[level].saturating_add(1)
                    };
                    for deeper in counts.iter_mut().skip(level + 1) {
                        *deeper = 0;
                    }
                    item.marker = match numbering.level(&item.num_id, level as u8) {
                        Some(lv) if lv.format == "bullet" || lv.format.is_empty() => {
                            ["•", "◦", "▪"][level % 3].to_string()
                        }
                        Some(lv) if lv.format == "none" => String::new(),
                        Some(lv) => {
                            let number = |l: usize| {
                                let count = counts[l];
                                let n = if count == 0 { start(l) } else { count };
                                let fmt = numbering
                                    .level(&item.num_id, l as u8)
                                    .map(|x| x.format.as_str())
                                    .unwrap_or_default();
                                format_number(n, fmt)
                            };
                            marker(&lv.text, level, number)
                        }
                        None => ["•", "◦", "▪"][level % 3].to_string(),
                    };
                }
                Block::Table(t) => {
                    for row in &mut t.rows {
                        for cell in row {
                            walk(&mut cell.blocks, numbering, counters);
                        }
                    }
                }
            }
        }
    }
    walk(blocks, numbering, &mut counters);
}

/// Expand `%1` … `%9` in `template` in one pass, stopping at
/// [`MAX_MARKER`] characters. `%N` deeper than `level` is left as written,
/// as before; the output never grows with the template's repetitions past
/// the bound.
fn marker(template: &str, level: usize, mut number: impl FnMut(usize) -> String) -> String {
    let mut out = String::new();
    let mut len = 0usize;
    let mut push = |out: &mut String, s: &str| -> bool {
        for ch in s.chars() {
            if len == MAX_MARKER {
                return false;
            }
            out.push(ch);
            len += 1;
        }
        true
    };
    let mut chars = template.chars().peekable();
    while let Some(ch) = chars.next() {
        let digit = (ch == '%')
            .then(|| chars.peek().and_then(|d| d.to_digit(10)))
            .flatten()
            .filter(|d| (1..=9).contains(d) && (*d as usize) <= level + 1);
        let more = match digit {
            Some(d) => {
                chars.next();
                push(&mut out, &number(d as usize - 1))
            }
            None => push(&mut out, ch.encode_utf8(&mut [0; 4])),
        };
        if !more {
            break;
        }
    }
    out
}

fn format_number(n: u32, format: &str) -> String {
    match format {
        "lowerLetter" => letters(n, b'a'),
        "upperLetter" => letters(n, b'A'),
        "lowerRoman" => roman(n).to_lowercase(),
        "upperRoman" => roman(n),
        _ => n.to_string(),
    }
}

/// "zzzz" is item 104; past that, digits.
const MAX_LETTER_REPEATS: usize = 4;

fn letters(n: u32, base: u8) -> String {
    if n == 0 {
        return "0".into();
    }
    // Word repeats the letter: a … z, aa … zz. Past a few repeats the
    // number is written in digits instead, so the marker stays bounded.
    let repeats = ((n - 1) / 26 + 1) as usize;
    if repeats > MAX_LETTER_REPEATS {
        return n.to_string();
    }
    let letter = (base + ((n - 1) % 26) as u8) as char;
    letter.to_string().repeat(repeats)
}

fn roman(mut n: u32) -> String {
    if n == 0 || n > 3999 {
        return n.to_string();
    }
    let table = [
        (1000, "M"),
        (900, "CM"),
        (500, "D"),
        (400, "CD"),
        (100, "C"),
        (90, "XC"),
        (50, "L"),
        (40, "XL"),
        (10, "X"),
        (9, "IX"),
        (5, "V"),
        (4, "IV"),
        (1, "I"),
    ];
    let mut out = String::new();
    for (value, digits) in table {
        while n >= value {
            out.push_str(digits);
            n -= value;
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Comments
// ---------------------------------------------------------------------------

/// One comment, from `comments.xml`, with its thread from
/// `commentsExtended.xml`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Comment {
    pub id: String,
    pub author: Option<String>,
    pub initials: Option<String>,
    pub date: Option<String>,
    pub text: String,
    /// `w14:paraId` of the comment's last paragraph: how replies point at it.
    #[serde(skip)]
    pub para_id: Option<String>,
    /// The comment this one replies to.
    pub reply_to: Option<String>,
    pub done: bool,
}

pub fn comments(xml: &str) -> Vec<Comment> {
    let mut out: Vec<Comment> = Vec::new();
    let mut in_comment = false;
    let mut in_text = false;
    let mut paragraphs = 0usize;
    let mut r = Reader::from_str(xml);
    while let Ok(event) = r.read_event() {
        match event {
            Event::Start(e) | Event::Empty(e) if local(e.name().as_ref()) == b"comment" => {
                out.push(Comment {
                    id: attr(&e, "id").unwrap_or_default(),
                    author: attr(&e, "author"),
                    initials: attr(&e, "initials"),
                    date: attr(&e, "date"),
                    text: String::new(),
                    para_id: None,
                    reply_to: None,
                    done: false,
                });
                in_comment = true;
                paragraphs = 0;
            }
            Event::Start(e) if in_comment => match local(e.name().as_ref()) {
                b"p" => {
                    if let Some(c) = out.last_mut() {
                        if paragraphs > 0 {
                            c.text.push('\n');
                        }
                        if let Some(id) = attr(&e, "paraId") {
                            c.para_id = Some(id);
                        }
                    }
                    paragraphs += 1;
                }
                b"t" => in_text = true,
                _ => {}
            },
            Event::Empty(e) if in_comment => match local(e.name().as_ref()) {
                b"p" => {
                    if let Some(c) = out.last_mut() {
                        if paragraphs > 0 {
                            c.text.push('\n');
                        }
                        if let Some(id) = attr(&e, "paraId") {
                            c.para_id = Some(id);
                        }
                    }
                    paragraphs += 1;
                }
                b"tab" => {
                    if let Some(c) = out.last_mut() {
                        c.text.push('\t');
                    }
                }
                b"br" => {
                    if let Some(c) = out.last_mut() {
                        c.text.push('\n');
                    }
                }
                _ => {}
            },
            Event::Text(t) if in_text => {
                if let (Some(c), Ok(s)) = (out.last_mut(), t.decode()) {
                    c.text.push_str(&s);
                }
            }
            Event::GeneralRef(g) if in_text => {
                if let Some(c) = out.last_mut() {
                    if let Ok(Some(ch)) = g.resolve_char_ref() {
                        c.text.push(ch);
                    } else if let Ok(name) = g.decode()
                        && let Some(s) = quick_xml::escape::resolve_predefined_entity(&name)
                    {
                        c.text.push_str(s);
                    }
                }
            }
            Event::End(e) => match local(e.name().as_ref()) {
                b"t" => in_text = false,
                b"comment" => in_comment = false,
                _ => {}
            },
            Event::Eof | Event::DocType(_) => break,
            _ => {}
        }
    }
    out
}

/// `w15:commentEx` entries: paraId → (parent paraId, done).
pub fn comments_extended(xml: &str) -> BTreeMap<String, (Option<String>, bool)> {
    let mut out = BTreeMap::new();
    walk(xml, |name, e, _| {
        if name == b"commentEx"
            && let Some(id) = attr(e, "paraId")
        {
            let parent = attr(e, "paraIdParent");
            let done = attr(e, "done").as_deref() == Some("1");
            out.insert(id, (parent, done));
        }
    });
    out
}

/// Thread comments: fill `reply_to` and `done` from the extended part.
pub fn thread(comments: &mut [Comment], extended: &BTreeMap<String, (Option<String>, bool)>) {
    let by_para: BTreeMap<String, String> = comments
        .iter()
        .filter_map(|c| Some((c.para_id.clone()?, c.id.clone())))
        .collect();
    for c in comments.iter_mut() {
        let Some((parent, done)) = c.para_id.as_ref().and_then(|p| extended.get(p)) else {
            continue;
        };
        c.done = *done;
        c.reply_to = parent.as_ref().and_then(|p| by_para.get(p)).cloned();
    }
}

/// The comment ids the body actually anchors text to, in reading order.
pub fn anchored_order(blocks: &[Block]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    fn walk(blocks: &[Block], out: &mut Vec<String>, seen: &mut HashSet<String>) {
        for block in blocks {
            match block {
                Block::Paragraph(p) => {
                    for inline in &p.inlines {
                        let ids: Vec<&String> = match inline {
                            Inline::Text { comments, .. } => comments.iter().collect(),
                            Inline::CommentMark { id } => vec![id],
                            Inline::Object { .. } => vec![],
                        };
                        for id in ids {
                            if seen.insert(id.clone()) {
                                out.push(id.clone());
                            }
                        }
                    }
                }
                Block::Table(t) => {
                    for row in &t.rows {
                        for cell in row {
                            walk(&cell.blocks, out, seen);
                        }
                    }
                }
            }
        }
    }
    walk(blocks, &mut out, &mut seen);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_relationship_target_resolves_against_its_source_and_never_climbs_out() {
        assert_eq!(
            resolve("word/document.xml", "comments.xml").as_deref(),
            Some("word/comments.xml")
        );
        assert_eq!(
            resolve("word/document.xml", "../customXml/item1.xml").as_deref(),
            Some("customXml/item1.xml")
        );
        assert_eq!(
            resolve("", "word/document.xml").as_deref(),
            Some("word/document.xml")
        );
        assert_eq!(
            resolve("word/document.xml", "/word/styles.xml").as_deref(),
            Some("word/styles.xml")
        );
        assert_eq!(resolve("word/document.xml", "../../etc/passwd"), None);
        assert_eq!(
            rels_part("word/document.xml"),
            "word/_rels/document.xml.rels"
        );
    }

    #[test]
    fn list_markers_count_per_level_and_restart_below_a_shallower_item() {
        assert_eq!(format_number(4, "lowerRoman"), "iv");
        assert_eq!(format_number(28, "lowerLetter"), "bb");
        assert_eq!(format_number(1994, "upperRoman"), "MCMXCIV");
    }
}
