//! The reader and the patcher against Word-shaped documents built here.
//!
//! Built rather than checked in so each test states the XML it depends on:
//! a run split across three `w:r` elements, a deletion with an author, a
//! comment thread linked through `commentsExtended.xml`. The shapes follow
//! what Word 365 writes (namespaces, `w14:paraId`, `w15:commentEx`).

use std::collections::BTreeSet;

use super::docx::Docx;
use super::package::{self, Package, tests::zip_of};
use super::patch::{self, NewComment, PatchError};
use super::scan::{Block, ChangeKind, Inline, ScanError};
use super::text;

const W: &str = "http://schemas.openxmlformats.org/wordprocessingml/2006/main";
const W14: &str = "http://schemas.microsoft.com/office/word/2010/wordml";
const W15: &str = "http://schemas.microsoft.com/office/word/2012/wordml";

const STYLES: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:styles xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">
<w:style w:type="paragraph" w:default="1" w:styleId="Normal"><w:name w:val="Normal"/></w:style>
<w:style w:type="paragraph" w:styleId="Heading1"><w:name w:val="heading 1"/><w:basedOn w:val="Normal"/><w:pPr><w:outlineLvl w:val="0"/></w:pPr></w:style>
<w:style w:type="paragraph" w:styleId="berschrift2"><w:name w:val="heading 2"/><w:basedOn w:val="Normal"/></w:style>
<w:style w:type="paragraph" w:styleId="ListBullet"><w:name w:val="List Bullet"/><w:pPr><w:numPr><w:numId w:val="1"/></w:numPr></w:pPr></w:style>
</w:styles>"#;

const NUMBERING: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:numbering xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">
<w:abstractNum w:abstractNumId="0"><w:lvl w:ilvl="0"><w:start w:val="1"/><w:numFmt w:val="bullet"/><w:lvlText w:val=""/></w:lvl></w:abstractNum>
<w:abstractNum w:abstractNumId="1"><w:lvl w:ilvl="0"><w:start w:val="1"/><w:numFmt w:val="decimal"/><w:lvlText w:val="%1."/></w:lvl><w:lvl w:ilvl="1"><w:start w:val="1"/><w:numFmt w:val="lowerLetter"/><w:lvlText w:val="%2)"/></w:lvl></w:abstractNum>
<w:num w:numId="1"><w:abstractNumId w:val="0"/></w:num>
<w:num w:numId="2"><w:abstractNumId w:val="1"/></w:num>
</w:numbering>"#;

const PNG: &[u8] = &[
    0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 0, 0, 0, 13, 7, 7, 7,
];

/// What goes into a test document besides its body.
#[derive(Default)]
struct Extra<'a> {
    comments: Option<&'a str>,
    extended: Option<&'a str>,
    /// In place of [`NUMBERING`].
    numbering: Option<&'a str>,
    signed: bool,
}

fn document(body: &str) -> String {
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\r\n\
         <w:document xmlns:w=\"{W}\" xmlns:w14=\"{W14}\" xmlns:r=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships\"><w:body>{body}<w:sectPr/></w:body></w:document>"
    )
}

fn docx(body: &str, extra: Extra<'_>) -> Vec<u8> {
    let mut types = String::from(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Default Extension="xml" ContentType="application/xml"/><Default Extension="png" ContentType="image/png"/><Override PartName="/word/document.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml"/>"#,
    );
    let mut rels = String::from(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/styles" Target="styles.xml"/><Relationship Id="rId2" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/numbering" Target="numbering.xml"/><Relationship Id="rId3" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/image" Target="media/image1.png"/>"#,
    );
    if extra.comments.is_some() {
        types.push_str(r#"<Override PartName="/word/comments.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.comments+xml"/>"#);
        rels.push_str(r#"<Relationship Id="rId4" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/comments" Target="comments.xml"/>"#);
    }
    if extra.extended.is_some() {
        types.push_str(r#"<Override PartName="/word/commentsExtended.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.commentsExtended+xml"/>"#);
        rels.push_str(r#"<Relationship Id="rId5" Type="http://schemas.microsoft.com/office/2011/relationships/commentsExtended" Target="commentsExtended.xml"/>"#);
    }
    types.push_str("</Types>");
    rels.push_str("</Relationships>");
    let mut root_rels = String::from(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="word/document.xml"/>"#,
    );
    if extra.signed {
        root_rels.push_str(r#"<Relationship Id="rId9" Type="http://schemas.openxmlformats.org/package/2006/relationships/digital-signature/origin" Target="_xmlsignatures/origin.sigs"/>"#);
    }
    root_rels.push_str("</Relationships>");
    let doc = document(body);
    let mut entries: Vec<(&str, Vec<u8>)> = vec![
        ("[Content_Types].xml", types.into_bytes()),
        ("_rels/.rels", root_rels.into_bytes()),
        ("word/document.xml", doc.into_bytes()),
        ("word/_rels/document.xml.rels", rels.into_bytes()),
        ("word/styles.xml", STYLES.as_bytes().to_vec()),
        (
            "word/numbering.xml",
            extra.numbering.unwrap_or(NUMBERING).as_bytes().to_vec(),
        ),
        ("word/media/image1.png", PNG.to_vec()),
        (
            "docProps/core.xml",
            b"<cp:coreProperties xmlns:cp=\"x\"/>".to_vec(),
        ),
    ];
    if let Some(c) = extra.comments {
        entries.push(("word/comments.xml", c.as_bytes().to_vec()));
    }
    if let Some(c) = extra.extended {
        entries.push(("word/commentsExtended.xml", c.as_bytes().to_vec()));
    }
    if extra.signed {
        entries.push(("_xmlsignatures/origin.sigs", Vec::new()));
        entries.push(("_xmlsignatures/sig1.xml", b"<Signature/>".to_vec()));
    }
    let refs: Vec<(&str, &[u8])> = entries.iter().map(|(n, b)| (*n, b.as_slice())).collect();
    zip_of(&refs)
}

/// A report as Word writes one: heading, a paragraph split over runs with
/// an existing comment thread, tracked changes by two people, a list, a
/// table, a picture and a hyperlink.
fn report_body() -> String {
    r#"<w:p w14:paraId="1A2B3C4D"><w:pPr><w:pStyle w:val="Heading1"/></w:pPr><w:r><w:t>Quarterly report</w:t></w:r></w:p>
<w:p><w:r><w:rPr><w:b/></w:rPr><w:t xml:space="preserve">Revenue </w:t></w:r><w:commentRangeStart w:id="0"/><w:r><w:t>grew</w:t></w:r><w:r><w:t xml:space="preserve"> by 12% &amp; costs fell.</w:t></w:r><w:commentRangeEnd w:id="0"/><w:r><w:rPr><w:rStyle w:val="CommentReference"/></w:rPr><w:commentReference w:id="0"/></w:r><w:commentRangeStart w:id="1"/><w:commentRangeEnd w:id="1"/><w:r><w:commentReference w:id="1"/></w:r></w:p>
<w:p><w:r><w:t xml:space="preserve">We </w:t></w:r><w:del w:id="10" w:author="Bob Ray" w:date="2026-11-02T10:00:00Z"><w:r><w:delText>might</w:delText></w:r></w:del><w:ins w:id="11" w:author="Ann Lee" w:date="2026-11-03T11:00:00Z"><w:r><w:t>will</w:t></w:r></w:ins><w:r><w:t xml:space="preserve"> hire.</w:t></w:r></w:p>
<w:p><w:pPr><w:pStyle w:val="ListBullet"/></w:pPr><w:r><w:t>Bullet one</w:t></w:r></w:p>
<w:p><w:pPr><w:numPr><w:ilvl w:val="0"/><w:numId w:val="2"/></w:numPr></w:pPr><w:r><w:t>First</w:t></w:r></w:p>
<w:p><w:pPr><w:numPr><w:ilvl w:val="1"/><w:numId w:val="2"/></w:numPr></w:pPr><w:r><w:t>Sub</w:t></w:r></w:p>
<w:p><w:pPr><w:numPr><w:ilvl w:val="0"/><w:numId w:val="2"/></w:numPr></w:pPr><w:r><w:t>Second</w:t></w:r></w:p>
<w:tbl><w:tblPr/><w:tblGrid/><w:tr><w:tc><w:tcPr><w:gridSpan w:val="2"/></w:tcPr><w:p><w:r><w:t>Region</w:t></w:r></w:p></w:tc></w:tr><w:tr><w:tc><w:p><w:r><w:t>North</w:t></w:r></w:p></w:tc><w:tc><w:p><w:r><w:t>40</w:t></w:r></w:p></w:tc></w:tr></w:tbl>
<w:p><w:r><w:t xml:space="preserve">Chart: </w:t></w:r><w:r><w:drawing><wp:inline xmlns:wp="x"><w:p><w:r><w:t>not body text</w:t></w:r></w:p></wp:inline></w:drawing></w:r><w:r><w:t xml:space="preserve"> see </w:t></w:r><w:hyperlink r:id="rId3"><w:r><w:t>the site</w:t></w:r></w:hyperlink><w:r><w:fldChar w:fldCharType="begin"/></w:r><w:r><w:instrText>PAGE</w:instrText></w:r><w:r><w:fldChar w:fldCharType="separate"/></w:r><w:r><w:t>7</w:t></w:r><w:r><w:fldChar w:fldCharType="end"/></w:r><w:r><w:tab/><w:t>😀 end</w:t></w:r></w:p>
<w:p><w:pPr><w:numPr><w:ilvl w:val="1"/><w:numId w:val="2"/></w:numPr></w:pPr><w:r><w:t>Sub again</w:t></w:r></w:p>
<w:p/>"#
        .to_string()
}

fn thread_comments() -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:comments xmlns:w="{W}" xmlns:w14="{W14}"><w:comment w:id="0" w:author="Ann Lee" w:date="2026-11-01T09:00:00Z" w:initials="AL"><w:p w14:paraId="00000A01"><w:r><w:annotationRef/></w:r><w:r><w:t>Source for 12%?</w:t></w:r></w:p></w:comment><w:comment w:id="1" w:author="Bob Ray" w:date="2026-11-01T10:00:00Z" w:initials="BR"><w:p w14:paraId="00000A02"><w:r><w:annotationRef/></w:r><w:r><w:t>Finance deck, slide 4.</w:t></w:r></w:p></w:comment></w:comments>"#
    )
}

fn thread_extended() -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w15:commentsEx xmlns:w15="{W15}"><w15:commentEx w15:paraId="00000A01" w15:done="0"/><w15:commentEx w15:paraId="00000A02" w15:paraIdParent="00000A01" w15:done="0"/></w15:commentsEx>"#
    )
}

fn report() -> Vec<u8> {
    let comments = thread_comments();
    let extended = thread_extended();
    docx(
        &report_body(),
        Extra {
            comments: Some(&comments),
            extended: Some(&extended),
            ..Extra::default()
        },
    )
}

fn paragraphs(blocks: &[Block]) -> Vec<&super::scan::Para> {
    let mut out = Vec::new();
    for b in blocks {
        match b {
            Block::Paragraph(p) => out.push(p),
            Block::Table(t) => {
                for row in &t.rows {
                    for cell in row {
                        out.extend(paragraphs(&cell.blocks));
                    }
                }
            }
        }
    }
    out
}

fn comment(anchor: &str, start: usize, end: usize, text: &str) -> NewComment {
    NewComment {
        anchor: anchor.into(),
        start,
        end,
        text: text.into(),
        author: "Kim Reviewer".into(),
        date: "2026-11-21T12:00:00Z".into(),
        reply_to: None,
    }
}

// ---------------------------------------------------------------------------
// Reading
// ---------------------------------------------------------------------------

#[test]
fn the_reading_view_has_headings_lists_tables_and_placeholders() {
    let doc = Docx::open(report()).unwrap();
    let paras = paragraphs(doc.blocks());
    assert_eq!(paras[0].heading, Some(1), "Heading1 by its outline level");
    assert_eq!(doc.paragraph_text("p/0").unwrap(), "Quarterly report");

    // Lists: a style's numbering, then a decimal list with a sublevel that
    // does not disturb the outer count.
    let markers: Vec<&str> = paras[3..7]
        .iter()
        .map(|p| p.list.as_ref().unwrap().marker.as_str())
        .collect();
    assert_eq!(markers, ["•", "1.", "a)", "2."]);
    // A sublevel under the next outer item counts from the start again.
    let again = paras.iter().find(|p| p.anchor == "p/11").unwrap();
    assert_eq!(again.list.as_ref().unwrap().marker, "a)");

    let Block::Table(table) = &doc.blocks()[7] else {
        panic!("the table is not where it was: {:?}", doc.blocks()[7]);
    };
    assert_eq!(table.rows.len(), 2);
    assert_eq!(table.rows[0][0].span, 2);
    assert_eq!(doc.paragraph_text("p/8").unwrap(), "North");

    // The drawing is a placeholder and its inner paragraph is not counted;
    // the field shows its result, not its code; the tab is one character.
    let chart = paras.iter().find(|p| p.anchor == "p/10").unwrap();
    assert!(
        chart
            .inlines
            .iter()
            .any(|i| matches!(i, Inline::Object { .. }))
    );
    assert_eq!(
        doc.paragraph_text("p/10").unwrap(),
        "Chart:  see the site7\t😀 end"
    );
    assert!(
        !doc.paragraph_text("p/10")
            .unwrap()
            .contains("not body text")
    );
    assert!(!doc.paragraph_text("p/10").unwrap().contains("PAGE"));
    assert_eq!(
        doc.scan.paragraphs.len(),
        13,
        "the empty last paragraph counts too"
    );
}

#[test]
fn every_paragraph_s_visible_text_is_as_long_as_its_anchor_count() {
    // The invariant the patcher depends on: the reading view's text and the
    // scanner's unit count are the same length, paragraph by paragraph.
    let doc = Docx::open(report()).unwrap();
    for (i, pos) in doc.scan.paragraphs.iter().enumerate() {
        let text = doc.paragraph_text(&format!("p/{i}")).unwrap_or_default();
        assert_eq!(text.encode_utf16().count(), pos.units, "p/{i}: {text:?}");
    }
}

#[test]
fn existing_comments_and_changes_are_shown_with_authors_and_never_accepted() {
    let doc = Docx::open(report()).unwrap();
    assert_eq!(doc.comments.len(), 2);
    assert_eq!(doc.comments[0].author.as_deref(), Some("Ann Lee"));
    assert_eq!(doc.comments[0].text, "Source for 12%?");
    assert_eq!(
        doc.comments[1].reply_to.as_deref(),
        Some("0"),
        "the thread link"
    );

    let changes = doc.changes();
    assert_eq!(changes.len(), 2);
    assert_eq!(changes[0].kind, ChangeKind::Deletion);
    assert_eq!(changes[0].author.as_deref(), Some("Bob Ray"));
    assert_eq!(changes[0].text, "might");
    assert_eq!(changes[1].kind, ChangeKind::Insertion);
    assert_eq!(changes[1].date.as_deref(), Some("2026-11-03T11:00:00Z"));
    // Both texts stay in the paragraph: nothing is resolved.
    assert_eq!(doc.paragraph_text("p/2").unwrap(), "We mightwill hire.");
    // The comment covers exactly its range.
    assert_eq!(
        patch::covered_text(&doc, "p/1", "0"),
        "grew by 12% & costs fell."
    );
}

/// A numbering part with one level, `start` and `lvl_text` as given.
fn one_level_numbering(start: &str, format: &str, lvl_text: &str) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:numbering xmlns:w="{W}"><w:abstractNum w:abstractNumId="0"><w:lvl w:ilvl="0"><w:start w:val="{start}"/><w:numFmt w:val="{format}"/><w:lvlText w:val="{lvl_text}"/></w:lvl></w:abstractNum><w:num w:numId="1"><w:abstractNumId w:val="0"/></w:num></w:numbering>"#
    )
}

fn list_markers(numbering: &str, items: usize) -> Vec<String> {
    let body: String = (0..items)
        .map(|_| r#"<w:p><w:pPr><w:numPr><w:ilvl w:val="0"/><w:numId w:val="1"/></w:numPr></w:pPr><w:r><w:t>x</w:t></w:r></w:p>"#)
        .collect();
    let doc = Docx::open(docx(
        &body,
        Extra {
            numbering: Some(numbering),
            ..Extra::default()
        },
    ))
    .unwrap();
    paragraphs(doc.blocks())
        .iter()
        .map(|p| p.list.as_ref().unwrap().marker.clone())
        .collect()
}

/// A hostile numbering part cannot make a marker large: the review's bomb
/// (`w:start` 4,000,000,000 in letters, a template of thirty `%1`) opened
/// to a 153 MB marker or an abort. Each bound is checked on its own.
#[test]
fn a_hostile_list_numbering_gives_a_short_marker_and_never_overflows() {
    // The start is clamped, and letters past "zzzz" turn into digits.
    let huge = one_level_numbering("4000000000", "lowerLetter", "%1");
    let markers = list_markers(&huge, 2);
    assert_eq!(markers, ["32767", "32768"]);
    // A start at the top of u32 counts on without overflowing (a debug
    // build panicked on the second item).
    let top = one_level_numbering("4294967295", "decimal", "%1.");
    assert_eq!(list_markers(&top, 2), ["32767.", "32768."]);
    // A template repeating %1 is cut at the marker bound.
    let repeated = one_level_numbering("1", "decimal", &"%1".repeat(30));
    let markers = list_markers(&repeated, 1);
    assert_eq!(markers[0], "1".repeat(30));
    let long = one_level_numbering("30000", "decimal", &"%1".repeat(30));
    let markers = list_markers(&long, 1);
    assert_eq!(markers[0].chars().count(), super::parts::MAX_MARKER);
    assert!(markers[0].starts_with("300003000030000"));
    // Real markers are unchanged: "%1." and letters up to the bound.
    let letters = one_level_numbering("103", "lowerLetter", "(%1)");
    assert_eq!(list_markers(&letters, 3), ["(yyyy)", "(zzzz)", "(105)"]);
}

#[test]
fn the_agent_text_marks_changes_and_comments() {
    let doc = Docx::open(report()).unwrap();
    let text = text::render(doc.blocks(), &doc.comments, doc.changes());
    assert!(text.starts_with("# Quarterly report\n"), "{text}");
    assert!(
        text.contains("Revenue {==grew by 12% & costs fell.==}[c0]"),
        "{text}"
    );
    assert!(text.contains("We {--might--}{++will++} hire."), "{text}");
    assert!(text.contains("  a) Sub"), "{text}");
    assert!(text.contains("| North | 40 |"), "{text}");
    assert!(
        text.contains("[c0] Ann Lee, 2026-11-01T09:00:00Z: Source for 12%?"),
        "{text}"
    );
    assert!(text.contains("  reply [c1] Bob Ray"), "{text}");
    assert!(
        text.contains("- Bob Ray deleted \"might\" in p/2"),
        "{text}"
    );
}

#[test]
fn a_document_with_a_dtd_or_absurd_nesting_is_refused() {
    let dtd = format!(
        "<?xml version=\"1.0\"?><!DOCTYPE x [<!ENTITY a \"aaaaaaaa\">]><w:document xmlns:w=\"{W}\"><w:body/></w:document>"
    );
    assert_eq!(
        super::scan::scan(&dtd, &Default::default()).err(),
        Some(ScanError::Dtd)
    );
    let deep = format!(
        "<w:document xmlns:w=\"{W}\"><w:body>{}{}</w:body></w:document>",
        "<w:sdt>".repeat(500),
        "</w:sdt>".repeat(500)
    );
    assert_eq!(
        super::scan::scan(&deep, &Default::default()).err(),
        Some(ScanError::TooDeep)
    );
    let not_word = "<doc xmlns=\"urn:other\"><body/></doc>";
    assert_eq!(
        super::scan::scan(not_word, &Default::default()).err(),
        Some(ScanError::NotWord)
    );
}

// ---------------------------------------------------------------------------
// Commenting
// ---------------------------------------------------------------------------

/// Patch, then check what every comment test cares about: exactly the
/// expected parts changed, every other entry byte-identical, and the new
/// comment covers the selected text.
fn patched(original: Vec<u8>, new: &NewComment) -> (Docx, patch::Patched) {
    let mut doc = Docx::open(original.clone()).unwrap();
    let result = patch::add_comment(&mut doc, new).unwrap_or_else(|e| panic!("{e}"));
    let mut before = Package::open(original).unwrap();
    let mut after = Package::open(result.bytes.clone()).unwrap();
    let diff = package::diff(&mut before, &mut after).unwrap();
    assert_eq!(diff.changed, result.changed, "changed parts");
    assert_eq!(diff.added, result.added, "added parts");
    assert!(diff.removed.is_empty());
    for name in before.names().to_vec() {
        if !result.changed.contains(&name) {
            assert_eq!(
                before.raw(&name).unwrap(),
                after.raw(&name).unwrap(),
                "{name} is not byte-identical"
            );
            assert_eq!(
                before.read(&name).unwrap(),
                after.read(&name).unwrap(),
                "{name}"
            );
        }
    }
    (Docx::open(result.bytes.clone()).unwrap(), result)
}

#[test]
fn a_comment_inside_one_run_splits_it_and_covers_exactly_the_selection() {
    // "Quarterly report": select "report" (offsets 10..16).
    let (after, result) = patched(report(), &comment("p/0", 10, 16, "Which quarter?"));
    assert_eq!(result.id, "2", "one past the highest id in use");
    assert_eq!(patch::covered_text(&after, "p/0", "2"), "report");
    assert_eq!(after.paragraph_text("p/0").unwrap(), "Quarterly report");
    let new = after.comments.iter().find(|c| c.id == "2").unwrap();
    assert_eq!(new.text, "Which quarter?");
    assert_eq!(new.author.as_deref(), Some("Kim Reviewer"));
    assert_eq!(new.date.as_deref(), Some("2026-11-21T12:00:00Z"));
    assert_eq!(
        result.changed,
        BTreeSet::from([
            "word/comments.xml".to_string(),
            "word/document.xml".to_string()
        ])
    );
    assert!(result.added.is_empty());
    // The heading is still a heading: the split runs kept their place.
    assert_eq!(paragraphs(after.blocks())[0].heading, Some(1));
}

#[test]
fn a_comment_across_runs_and_an_entity_lands_on_the_right_characters() {
    // p/1 is "Revenue grew by 12% & costs fell." over three runs, with
    // `&amp;` in the third. Select "by 12% & costs" (13..27).
    let visible = Docx::open(report()).unwrap().paragraph_text("p/1").unwrap();
    assert_eq!(&visible[13..27], "by 12% & costs");
    let (after, _) = patched(report(), &comment("p/1", 13, 27, "Check"));
    assert_eq!(patch::covered_text(&after, "p/1", "2"), "by 12% & costs");
    // The existing comment still covers its own range.
    assert_eq!(
        patch::covered_text(&after, "p/1", "0"),
        "grew by 12% & costs fell."
    );
    // The bold run's properties went onto nothing new: "Revenue " is whole.
    assert!(
        after
            .xml
            .contains("<w:rPr><w:b/></w:rPr><w:t xml:space=\"preserve\">Revenue </w:t>")
    );
}

#[test]
fn a_comment_after_an_emoji_counts_utf16_units_like_the_browser() {
    // p/10 ends "\t😀 end"; the emoji is two UTF-16 units. "end" is the last
    // three units of the paragraph.
    let doc = Docx::open(report()).unwrap();
    let units = doc.scan.paragraphs[10].units;
    let (after, _) = patched(report(), &comment("p/10", units - 3, units, "fine"));
    assert_eq!(patch::covered_text(&after, "p/10", "2"), "end");
}

#[test]
fn a_comment_starting_between_two_children_of_one_run_splits_it_there() {
    // `<w:r><w:tab/><w:t>😀 end</w:t></w:r>`: a selection starting right
    // after the tab splits the run between its children, not inside text.
    let doc = Docx::open(report()).unwrap();
    let text = doc.paragraph_text("p/10").unwrap();
    let units = doc.scan.paragraphs[10].units;
    let start = text[..text.find('\t').unwrap()].encode_utf16().count() + 1;
    let (after, _) = patched(report(), &comment("p/10", start, units, "x"));
    assert_eq!(patch::covered_text(&after, "p/10", "2"), "😀 end");
    assert!(
        after
            .xml
            .contains("<w:r><w:tab/></w:r><w:commentRangeStart w:id=\"2\"/><w:r><w:t>😀 end</w:t>")
    );
}

#[test]
fn an_author_with_markup_characters_is_escaped_and_read_back_whole() {
    let mut new = comment("p/0", 0, 3, "x");
    new.author = "Ann & \"Bo\" <x>".into();
    let (after, _) = patched(report(), &new);
    let c = after.comments.iter().find(|c| c.id == "2").unwrap();
    assert_eq!(c.author.as_deref(), Some("Ann & \"Bo\" <x>"));
}

#[test]
fn a_comment_ending_inside_a_deletion_puts_its_reference_outside_it() {
    // p/2 is "We mightwill hire."; select "We migh" (0..7), which ends inside
    // the w:del. The reference run must not be deleted text.
    let (after, _) = patched(report(), &comment("p/2", 0, 7, "Tone"));
    assert_eq!(patch::covered_text(&after, "p/2", "2"), "We migh");
    let del_end = after.xml.find("</w:del>").unwrap();
    let reference = after.xml.find("<w:commentReference w:id=\"2\"/>").unwrap();
    assert!(
        reference > del_end,
        "the reference run is inside the deletion"
    );
    // The changes are still pending, with their authors.
    assert_eq!(after.changes().len(), 2);
    assert_eq!(after.changes()[0].author.as_deref(), Some("Bob Ray"));
    assert_eq!(after.paragraph_text("p/2").unwrap(), "We mightwill hire.");
}

#[test]
fn a_comment_on_hyperlinked_text_keeps_the_link_whole() {
    let doc = Docx::open(report()).unwrap();
    let text = doc.paragraph_text("p/10").unwrap();
    let start = text.find("the site").unwrap();
    let (after, _) = patched(report(), &comment("p/10", start, start + 3, "Which?"));
    assert_eq!(patch::covered_text(&after, "p/10", "2"), "the");
    assert_eq!(after.xml.matches("<w:hyperlink").count(), 1);
    assert_eq!(after.xml.matches("</w:hyperlink>").count(), 1);
}

#[test]
fn a_comment_in_a_table_cell_and_on_a_whole_run_needs_no_split() {
    let (after, _) = patched(report(), &comment("p/8", 0, 5, "North of what?"));
    assert_eq!(patch::covered_text(&after, "p/8", "2"), "North");
    // A whole-run selection is markers around the untouched run.
    assert!(after.xml.contains(
        "<w:commentRangeStart w:id=\"2\"/><w:r><w:t>North</w:t></w:r><w:commentRangeEnd w:id=\"2\"/>"
    ));
}

#[test]
fn a_document_without_comments_gains_the_part_its_relationship_and_its_type() {
    let original = docx(
        &report_body()
            .replace("w:id=\"0\"", "w:id=\"90\"")
            .replace("w:id=\"1\"", "w:id=\"91\""),
        Extra::default(),
    );
    let (after, result) = patched(original, &comment("p/0", 0, 9, "Good title"));
    assert_eq!(
        result.changed,
        BTreeSet::from([
            "[Content_Types].xml".to_string(),
            "word/_rels/document.xml.rels".to_string(),
            "word/document.xml".to_string()
        ])
    );
    assert_eq!(
        result.added,
        BTreeSet::from(["word/comments.xml".to_string()])
    );
    assert_eq!(after.comments_part.as_deref(), Some("word/comments.xml"));
    assert_eq!(after.comments.len(), 1);
    assert_eq!(result.id, "92", "above the ids the body already uses");
    assert!(
        after
            .content_types
            .contains("PartName=\"/word/comments.xml\"")
    );
}

#[test]
fn a_reply_joins_the_thread_through_comments_extended() {
    let mut new = comment("", 0, 0, "Agreed, cite it.");
    new.reply_to = Some("0".into());
    let (after, result) = patched(report(), &new);
    assert_eq!(
        result.changed,
        BTreeSet::from([
            "word/comments.xml".to_string(),
            "word/commentsExtended.xml".to_string(),
            "word/document.xml".to_string()
        ])
    );
    let reply = after.comments.iter().find(|c| c.id == "2").unwrap();
    assert_eq!(reply.reply_to.as_deref(), Some("0"));
    // Anchored on the same text as its parent.
    assert_eq!(
        patch::covered_text(&after, "p/1", "2"),
        "grew by 12% & costs fell."
    );
    // The original thread is intact.
    assert_eq!(
        after
            .comments
            .iter()
            .find(|c| c.id == "1")
            .unwrap()
            .reply_to
            .as_deref(),
        Some("0")
    );
}

#[test]
fn a_reply_to_a_reply_hangs_off_the_thread_root() {
    let mut new = comment("", 0, 0, "Me too.");
    new.reply_to = Some("1".into());
    let (after, _) = patched(report(), &new);
    let reply = after.comments.iter().find(|c| c.id == "2").unwrap();
    assert_eq!(reply.reply_to.as_deref(), Some("0"));
}

#[test]
fn a_reply_to_a_comment_with_no_paragraph_id_gives_the_parent_one() {
    // LibreOffice and older Word write comments without w14:paraId and with
    // no commentsExtended part at all.
    let comments = format!(
        r#"<w:comments xmlns:w="{W}"><w:comment w:id="0" w:author="Ann"><w:p><w:r><w:t>Old</w:t></w:r></w:p></w:comment></w:comments>"#
    );
    let original = docx(
        &report_body(),
        Extra {
            comments: Some(&comments),
            ..Extra::default()
        },
    );
    let mut new = comment("", 0, 0, "Reply");
    new.reply_to = Some("0".into());
    let (after, result) = patched(original, &new);
    assert!(result.added.contains("word/commentsExtended.xml"));
    let reply = after.comments.iter().find(|c| c.id == "2").unwrap();
    assert_eq!(reply.reply_to.as_deref(), Some("0"));
    assert!(
        after.comments[0].para_id.is_some(),
        "the parent was given a paragraph id"
    );
}

#[test]
fn a_signed_document_is_refused() {
    let original = docx(
        &report_body(),
        Extra {
            signed: true,
            ..Extra::default()
        },
    );
    let mut doc = Docx::open(original).unwrap();
    assert!(doc.report.signed);
    assert_eq!(
        patch::add_comment(&mut doc, &comment("p/0", 0, 3, "x")).err(),
        Some(PatchError::Signed)
    );
}

#[test]
fn a_range_outside_the_paragraph_and_an_empty_comment_are_refused() {
    let mut doc = Docx::open(report()).unwrap();
    assert!(matches!(
        patch::add_comment(&mut doc, &comment("p/0", 3, 99, "x")).err(),
        Some(PatchError::BadRange { len: 16, .. })
    ));
    assert!(matches!(
        patch::add_comment(&mut doc, &comment("p/0", 3, 3, "x")).err(),
        Some(PatchError::BadRange { .. })
    ));
    assert_eq!(
        patch::add_comment(&mut doc, &comment("p/0", 0, 3, "  ")).err(),
        Some(PatchError::EmptyText)
    );
    assert_eq!(
        patch::add_comment(&mut doc, &comment("p/99", 0, 3, "x")).err(),
        Some(PatchError::NoParagraph("p/99".into()))
    );
    let mut reply = comment("", 0, 0, "x");
    reply.reply_to = Some("77".into());
    assert_eq!(
        patch::add_comment(&mut doc, &reply).err(),
        Some(PatchError::NoParent("77".into()))
    );
}

#[test]
fn comment_text_is_escaped_and_split_into_paragraphs() {
    let (after, _) = patched(
        report(),
        &comment("p/0", 0, 3, "a < b & \"c\"\nsecond\tline\u{7}"),
    );
    let new = after.comments.iter().find(|c| c.id == "2").unwrap();
    assert_eq!(new.text, "a < b & \"c\"\nsecond\tline");
}

#[test]
fn utf16_offsets_map_through_entities_and_surrogate_pairs() {
    let raw = "a&amp;b😀&#x1F600;c";
    assert_eq!(patch::unit_to_byte(raw, 1), Some(1));
    assert_eq!(patch::unit_to_byte(raw, 2), Some(6));
    assert_eq!(patch::unit_to_byte(raw, 3), Some(7));
    assert_eq!(patch::unit_to_byte(raw, 4), None, "inside the emoji's pair");
    assert_eq!(patch::unit_to_byte(raw, 5), Some(11));
    assert_eq!(patch::unit_to_byte(raw, 7), Some(20));
    assert_eq!(patch::unit_to_byte(raw, 8), Some(21));
}

#[test]
fn dates_are_written_the_way_word_writes_them() {
    assert_eq!(patch::iso_from_unix(0), "1970-01-01T00:00:00Z");
    assert_eq!(patch::iso_from_unix(1_795_253_400), "2026-11-21T09:30:00Z");
    assert_eq!(patch::iso_from_unix(951_782_400), "2000-02-29T00:00:00Z");
}

#[test]
fn the_patch_check_refuses_a_comment_on_the_wrong_words_or_an_unexpected_part() {
    // The check that runs before anything is written, fed a correct patch
    // with the wrong expectations.
    let original = report();
    let mut doc = Docx::open(original.clone()).unwrap();
    let result = patch::add_comment(&mut doc, &comment("p/0", 10, 16, "x")).unwrap();
    let mut fresh = Docx::open(original.clone()).unwrap();
    assert!(
        patch::verify(
            &mut fresh,
            &result.bytes,
            &result.changed,
            &result.added,
            &result.id,
            Some(&("p/0".to_string(), "report".to_string())),
        )
        .is_ok()
    );
    let wrong_words = patch::verify(
        &mut fresh,
        &result.bytes,
        &result.changed,
        &result.added,
        &result.id,
        Some(&("p/0".to_string(), "Quarterly".to_string())),
    );
    assert!(
        matches!(wrong_words, Err(PatchError::Verify(_))),
        "{wrong_words:?}"
    );
    let fewer: BTreeSet<String> = BTreeSet::from(["word/document.xml".to_string()]);
    let unexpected = patch::verify(
        &mut fresh,
        &result.bytes,
        &fewer,
        &result.added,
        &result.id,
        None,
    );
    assert!(
        matches!(unexpected, Err(PatchError::Verify(_))),
        "{unexpected:?}"
    );
}

// ---------------------------------------------------------------------------
// Bounds on what a hostile or merely long document costs
// ---------------------------------------------------------------------------

/// Text that does not compress past the package's ratio cap.
fn varied_words(seed: &mut u64, chars: usize) -> String {
    let mut out = String::with_capacity(chars);
    while out.len() < chars {
        *seed = seed
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        let len = 2 + (*seed >> 60) as usize;
        for i in 0..len {
            out.push((b'a' + ((*seed >> (i * 5 % 55)) % 26) as u8) as char);
        }
        out.push(' ');
    }
    out
}

/// The review measured a 20,000-paragraph book giving a 9 MB `office.text`
/// result, over the core's 8 MiB frame limit, which drops the app's whole
/// connection. The result is now refused by name instead, and each half
/// asked for alone fits.
#[test]
fn a_text_result_over_the_frame_limit_is_an_error_and_each_half_alone_fits() {
    let mut seed = 7;
    let body: String = (0..1_100)
        .map(|_| {
            format!(
                "<w:p><w:r><w:t>{}</w:t></w:r></w:p>",
                varied_words(&mut seed, 4_000)
            )
        })
        .collect();
    let doc = Docx::open(docx(&body, Extra::default())).unwrap();
    let both = crate::app::text_result("/Documents/book.docx", &doc, None).unwrap_err();
    assert!(both.contains("over the") && both.contains("only"), "{both}");
    for only in [crate::app::TextPart::Text, crate::app::TextPart::Blocks] {
        let value = crate::app::text_result("/Documents/book.docx", &doc, Some(only)).unwrap();
        let size = serde_json::to_vec(&value).unwrap().len();
        assert!(size <= crate::app::MAX_RESULT_BYTES, "{only:?}: {size}");
        assert_eq!(
            value.get("text").is_some(),
            only == crate::app::TextPart::Text
        );
        assert_eq!(
            value.get("blocks").is_some(),
            only == crate::app::TextPart::Blocks
        );
        assert!(value["comments"].is_array() && value["report"].is_object());
    }
    // A short document still gets both, as before.
    let short =
        crate::app::text_result("/Documents/r.docx", &Docx::open(report()).unwrap(), None).unwrap();
    assert!(short["text"].is_string() && short["blocks"].is_array());
}

#[test]
fn a_result_is_bounded_at_the_frame_limit_less_its_headroom() {
    let fits = "x".repeat(crate::app::MAX_RESULT_BYTES - 2);
    assert!(crate::app::bounded(serde_json::json!(fits), "smaller").is_ok());
    let over = "x".repeat(crate::app::MAX_RESULT_BYTES - 1);
    let e = crate::app::bounded(serde_json::json!(over), "ask for less").unwrap_err();
    assert!(e.ends_with("ask for less"), "{e}");
}

/// The review measured 200,000 empty comments rendering in 66 s, growing
/// with the square; the scanner had the same shape in comment references
/// (every run's end walked every marker: 50,000 references took 35 s to
/// open in a debug build). Linear, opening and rendering 50,000 of each
/// takes well under a second even in a debug build.
#[test]
fn opening_and_rendering_many_comments_is_linear_not_quadratic() {
    let mut comments = format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><w:comments xmlns:w="{W}">"#
    );
    for i in 0..50_000 {
        comments.push_str(&format!(r#"<w:comment w:id="{i}"/>"#));
    }
    comments.push_str("</w:comments>");
    // Every one also referenced from the body, so the anchored order has
    // 50,000 distinct ids to keep in first-seen order.
    let refs: String = (0..50_000)
        .map(|i| format!(r#"<w:r><w:commentReference w:id="{i}"/></w:r>"#))
        .collect();
    let body = format!("<w:p><w:r><w:t>x</w:t></w:r>{refs}</w:p>");
    let file = docx(
        &body,
        Extra {
            comments: Some(&comments),
            ..Extra::default()
        },
    );
    let budget = std::time::Duration::from_secs(3);

    let started = std::time::Instant::now();
    let doc = Docx::open(file).unwrap();
    let opened = started.elapsed();
    assert_eq!(doc.comments.len(), 50_000);
    assert!(opened < budget, "opening took {opened:?}");

    let started = std::time::Instant::now();
    let text = text::render(doc.blocks(), &doc.comments, doc.changes());
    let order = super::parts::anchored_order(doc.blocks());
    let rendered = started.elapsed();
    assert!(text.contains("[c49999] someone"));
    assert_eq!(order.len(), 50_000);
    assert_eq!(order[49_999], "49999");
    assert!(rendered < budget, "rendering took {rendered:?}");
}

/// A reply listed under each of many roots that share its parent's id was
/// the same square again; each id's replies are listed once.
#[test]
fn replies_under_a_repeated_root_id_are_listed_once() {
    let mut a = super::parts::Comment {
        id: "0".into(),
        author: None,
        initials: None,
        date: None,
        text: "root".into(),
        para_id: None,
        reply_to: None,
        done: false,
    };
    let mut reply = a.clone();
    reply.id = "1".into();
    reply.text = "reply".into();
    reply.reply_to = Some("0".into());
    let dup = a.clone();
    a.text = "root".into();
    let text = text::render(&[], &[a, dup, reply], &[]);
    assert_eq!(text.matches("reply [c1]").count(), 1, "{text}");
    assert_eq!(text.matches("[c0]").count(), 2, "{text}");
}
