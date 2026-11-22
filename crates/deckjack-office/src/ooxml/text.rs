//! The reading view as plain text, for agents (`office.text`).
//!
//! Markup follows CriticMarkup, which models already read: `{++inserted++}`,
//! `{--deleted--}`, and `{==commented text==}[c3]` with the comment itself
//! listed at the end under `[c3]`. Headings are `#` lines and list items are
//! indented with their own marker, so the structure survives as Markdown.
//! Tracked changes are listed at the end too, with author and date.

use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;

use super::parts::Comment;
use super::scan::{Block, Change, ChangeKind, Inline, Para};

pub fn render(blocks: &[Block], comments: &[Comment], changes: &[Change]) -> String {
    let mut out = String::new();
    render_blocks(blocks, changes, &mut out);
    let out_trimmed = out.trim_end().to_string();
    let mut out = out_trimmed;
    if !comments.is_empty() {
        out.push_str("\n\n---\nComments:\n");
        // Indexed once, so a part with millions of comments is linear, not
        // quadratic: replies by parent id, and every id there is.
        let ids: HashSet<&str> = comments.iter().map(|c| c.id.as_str()).collect();
        let mut replies: HashMap<&str, Vec<&Comment>> = HashMap::new();
        for r in comments {
            if let Some(parent) = r.reply_to.as_deref() {
                replies.entry(parent).or_default().push(r);
            }
        }
        // Each id's replies are listed once, under its first root.
        let mut listed: HashSet<&str> = HashSet::new();
        for c in comments.iter().filter(|c| c.reply_to.is_none()) {
            write_comment(&mut out, c, "");
            if listed.insert(c.id.as_str()) {
                for r in replies.get(c.id.as_str()).into_iter().flatten() {
                    write_comment(&mut out, r, "  reply ");
                }
            }
        }
        // A reply whose parent is missing is still listed.
        for r in comments
            .iter()
            .filter(|r| r.reply_to.as_deref().is_some_and(|p| !ids.contains(p)))
        {
            write_comment(&mut out, r, "  reply ");
        }
    }
    if !changes.is_empty() {
        out.push_str("\n\n---\nTracked changes (not accepted):\n");
        for c in changes {
            let kind = match c.kind {
                ChangeKind::Insertion => "inserted",
                ChangeKind::Deletion => "deleted",
                ChangeKind::MoveFrom => "moved from here",
                ChangeKind::MoveTo => "moved here",
            };
            let _ = writeln!(
                out,
                "- {} {kind} {:?} in {} ({})",
                c.author.as_deref().unwrap_or("someone"),
                c.text,
                c.anchor,
                c.date.as_deref().unwrap_or("no date")
            );
        }
        out = out.trim_end().to_string();
    }
    out.push('\n');
    out
}

fn write_comment(out: &mut String, c: &Comment, lead: &str) {
    let done = if c.done { " (resolved)" } else { "" };
    let _ = writeln!(
        out,
        "{lead}[c{}] {}{}{done}: {}",
        c.id,
        c.author.as_deref().unwrap_or("someone"),
        c.date
            .as_deref()
            .map(|d| format!(", {d}"))
            .unwrap_or_default(),
        c.text.replace('\n', " / ")
    );
}

fn render_blocks(blocks: &[Block], changes: &[Change], out: &mut String) {
    for block in blocks {
        match block {
            Block::Paragraph(p) => {
                out.push_str(&paragraph(p, changes));
                out.push_str("\n\n");
            }
            Block::Table(t) => {
                for row in &t.rows {
                    let cells: Vec<String> = row
                        .iter()
                        .map(|cell| {
                            let mut inner = String::new();
                            render_blocks(&cell.blocks, changes, &mut inner);
                            inner
                                .split("\n\n")
                                .map(str::trim)
                                .filter(|s| !s.is_empty())
                                .collect::<Vec<_>>()
                                .join(" / ")
                                .replace('|', "\\|")
                        })
                        .collect();
                    out.push_str("| ");
                    out.push_str(&cells.join(" | "));
                    out.push_str(" |\n");
                }
                out.push('\n');
            }
        }
    }
}

pub fn paragraph(p: &Para, changes: &[Change]) -> String {
    let mut line = String::new();
    if let Some(level) = p.heading {
        line.push_str(&"#".repeat(level as usize));
        line.push(' ');
    } else if let Some(item) = &p.list {
        line.push_str(&"  ".repeat(item.level as usize));
        if item.marker.is_empty() {
            line.push_str("- ");
        } else {
            line.push_str(&item.marker);
            line.push(' ');
        }
    }
    let mut i = 0;
    while i < p.inlines.len() {
        let ids = match &p.inlines[i] {
            Inline::Text { comments, .. } => comments.clone(),
            _ => Vec::new(),
        };
        if ids.is_empty() {
            line.push_str(&inline(&p.inlines[i], changes));
            i += 1;
            continue;
        }
        // Consecutive inlines under the same comments read as one range.
        let mut j = i;
        let mut inner = String::new();
        while j < p.inlines.len() {
            match &p.inlines[j] {
                Inline::Text { comments, .. } if *comments == ids => {
                    inner.push_str(&inline(&p.inlines[j], changes));
                    j += 1;
                }
                _ => break,
            }
        }
        let labels: Vec<String> = ids.iter().map(|id| format!("c{id}")).collect();
        let _ = write!(line, "{{=={inner}==}}[{}]", labels.join(","));
        i = j;
    }
    line
}

fn inline(i: &Inline, changes: &[Change]) -> String {
    match i {
        Inline::Text { text, change, .. } => {
            match change.and_then(|c| changes.get(c)).map(|c| c.kind) {
                Some(ChangeKind::Insertion) | Some(ChangeKind::MoveTo) => format!("{{++{text}++}}"),
                Some(ChangeKind::Deletion) | Some(ChangeKind::MoveFrom) => {
                    format!("{{--{text}--}}")
                }
                None => text.clone(),
            }
        }
        Inline::Object { what } => format!("[{what}]"),
        Inline::CommentMark { id } => format!("[c{id}]"),
    }
}
