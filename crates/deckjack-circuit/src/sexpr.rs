//! A lossless S-expression tree for KiCad files.
//!
//! KiCad's schematic, symbol and board files are S-expressions. This module
//! reads one into a concrete syntax tree that keeps every byte: each node
//! carries the whitespace that came before it, each list the whitespace before
//! its closing parenthesis, and each atom its raw token text (quotes and
//! escapes included). Printing the tree concatenates those pieces, so a file
//! that is read and written without edits comes back byte-identical, and an
//! edit to one atom changes only that atom's bytes.
//!
//! What it does not do is interpret anything. Whether `(at 1 2 0)` is a
//! position is the typed model's business (`model.rs`); here it is a list of
//! four atoms.
//!
//! **Hostile input.** The parser is iterative, so nesting cannot overflow the
//! stack, and it refuses input over [`Limits::max_bytes`] or nested deeper than
//! [`Limits::max_depth`]. Every failure is an [`Error`]; nothing panics on
//! input.

use std::fmt;

/// Bounds on what [`parse`] accepts.
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    /// Refuse a document larger than this many bytes.
    pub max_bytes: usize,
    /// Refuse lists nested deeper than this. KiCad's own files nest about a
    /// dozen deep; the default leaves a wide margin.
    pub max_depth: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            // A large real schematic sheet is a few MiB. 64 MiB is far past
            // anything KiCad writes and still bounded.
            max_bytes: 64 * 1024 * 1024,
            max_depth: 256,
        }
    }
}

/// Why a document did not parse. Offsets are byte offsets into the input.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    #[error("the file is {size} bytes, over the {limit}-byte limit")]
    TooLarge { size: usize, limit: usize },
    #[error("the file is not valid UTF-8 (at byte {0})")]
    NotUtf8(usize),
    #[error("nesting deeper than {limit} levels at byte {offset}")]
    TooDeep { offset: usize, limit: usize },
    #[error("unexpected ')' at byte {0}")]
    UnexpectedClose(usize),
    #[error("unterminated string starting at byte {0}")]
    UnterminatedString(usize),
    #[error("the file ended inside a list opened at byte {0}")]
    UnclosedList(usize),
    #[error("the file does not start with a list")]
    NoRootList,
    #[error("unexpected content after the root list at byte {0}")]
    TrailingContent(usize),
}

/// One node: a list or an atom.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Node {
    List(List),
    Atom(Atom),
}

/// `(` items `)`, with the whitespace around it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct List {
    /// Whitespace before the opening parenthesis.
    pub lead: String,
    pub items: Vec<Node>,
    /// Whitespace before the closing parenthesis.
    pub tail: String,
}

/// A bare symbol or number, or a quoted string, as written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Atom {
    /// Whitespace before the token.
    pub lead: String,
    /// The token exactly as it appears in the file, quotes included.
    pub raw: String,
}

/// A whole file: the root list and whatever whitespace follows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Document {
    pub root: List,
    pub trailing: String,
}

impl Document {
    /// Print the document. Unedited, this is the input byte for byte.
    pub fn to_text(&self) -> String {
        let mut out = String::new();
        write_list(&self.root, &mut out);
        out.push_str(&self.trailing);
        out
    }
}

impl fmt::Display for Document {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_text())
    }
}

fn write_list(list: &List, out: &mut String) {
    // Recursion depth is bounded by `Limits::max_depth` for parsed trees, and
    // by construction for trees built in code.
    out.push_str(&list.lead);
    out.push('(');
    for item in &list.items {
        match item {
            Node::List(l) => write_list(l, out),
            Node::Atom(a) => {
                out.push_str(&a.lead);
                out.push_str(&a.raw);
            }
        }
    }
    out.push_str(&list.tail);
    out.push(')');
}

impl Node {
    pub fn as_list(&self) -> Option<&List> {
        match self {
            Node::List(l) => Some(l),
            Node::Atom(_) => None,
        }
    }
    pub fn as_list_mut(&mut self) -> Option<&mut List> {
        match self {
            Node::List(l) => Some(l),
            Node::Atom(_) => None,
        }
    }
    pub fn as_atom(&self) -> Option<&Atom> {
        match self {
            Node::Atom(a) => Some(a),
            Node::List(_) => None,
        }
    }
    pub fn lead_mut(&mut self) -> &mut String {
        match self {
            Node::List(l) => &mut l.lead,
            Node::Atom(a) => &mut a.lead,
        }
    }
    pub fn lead(&self) -> &str {
        match self {
            Node::List(l) => &l.lead,
            Node::Atom(a) => &a.lead,
        }
    }
    /// Print this node alone, including its leading whitespace.
    pub fn to_text(&self) -> String {
        let mut out = String::new();
        match self {
            Node::List(l) => write_list(l, &mut out),
            Node::Atom(a) => {
                out.push_str(&a.lead);
                out.push_str(&a.raw);
            }
        }
        out
    }
}

impl Atom {
    /// A bare token (a keyword or a number). The caller is responsible for it
    /// containing no whitespace, parentheses or quotes.
    pub fn bare(text: &str) -> Atom {
        Atom {
            lead: String::new(),
            raw: text.to_string(),
        }
    }

    /// A quoted string, escaped the way KiCad's writer escapes it.
    pub fn quoted(text: &str) -> Atom {
        Atom {
            lead: String::new(),
            raw: quote(text),
        }
    }

    pub fn is_quoted(&self) -> bool {
        self.raw.starts_with('"')
    }

    /// The token's value: unquoted and unescaped for a string, as-is for a
    /// bare token.
    pub fn text(&self) -> String {
        if self.is_quoted() {
            unquote(&self.raw)
        } else {
            self.raw.clone()
        }
    }

    /// Replace the value, keeping the leading whitespace and the quoting
    /// style: a quoted atom stays quoted, a bare one stays bare when the new
    /// value can be written bare.
    pub fn set_text(&mut self, text: &str) {
        if self.is_quoted() || !can_be_bare(text) {
            self.raw = quote(text);
        } else {
            self.raw = text.to_string();
        }
    }
}

fn can_be_bare(text: &str) -> bool {
    !text.is_empty()
        && !text
            .chars()
            .any(|c| c.is_whitespace() || matches!(c, '(' | ')' | '"' | '\\'))
}

/// Quote a string as KiCad's `OUTPUTFORMATTER::Quotew` does.
pub fn quote(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Undo [`quote`], and the other escapes KiCad's lexer accepts.
pub fn unquote(raw: &str) -> String {
    let inner = raw.strip_prefix('"').unwrap_or(raw);
    let inner = inner.strip_suffix('"').unwrap_or(inner);
    let mut out = String::with_capacity(inner.len());
    let mut chars = inner.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('r') => out.push('\r'),
            Some('t') => out.push('\t'),
            Some('a') => out.push('\u{7}'),
            Some('b') => out.push('\u{8}'),
            Some('f') => out.push('\u{c}'),
            Some('v') => out.push('\u{b}'),
            Some(other) => out.push(other),
            None => out.push('\\'),
        }
    }
    out
}

impl List {
    /// The list's keyword: its first item when that is a bare atom.
    pub fn head(&self) -> Option<&str> {
        match self.items.first() {
            Some(Node::Atom(a)) if !a.is_quoted() => Some(a.raw.as_str()),
            _ => None,
        }
    }

    pub fn is(&self, head: &str) -> bool {
        self.head() == Some(head)
    }

    /// Child lists, skipping atoms.
    pub fn lists(&self) -> impl Iterator<Item = &List> {
        self.items.iter().filter_map(Node::as_list)
    }

    /// The first child list with this keyword.
    pub fn find(&self, head: &str) -> Option<&List> {
        self.lists().find(|l| l.is(head))
    }

    pub fn find_mut(&mut self, head: &str) -> Option<&mut List> {
        self.items
            .iter_mut()
            .filter_map(Node::as_list_mut)
            .find(|l| l.is(head))
    }

    /// Every child list with this keyword.
    pub fn find_all<'a>(&'a self, head: &'a str) -> impl Iterator<Item = &'a List> + 'a {
        self.lists().filter(move |l| l.is(head))
    }

    /// The atom at `index` (0 is the keyword), as text.
    pub fn atom(&self, index: usize) -> Option<String> {
        self.items
            .get(index)
            .and_then(Node::as_atom)
            .map(Atom::text)
    }

    /// Atoms after the keyword, as text, stopping at the first list.
    pub fn args(&self) -> Vec<String> {
        self.items
            .iter()
            .skip(1)
            .map_while(|n| n.as_atom().map(Atom::text))
            .collect()
    }

    /// Whether a bare flag atom (such as `hide`) appears among the items.
    pub fn has_flag(&self, flag: &str) -> bool {
        self.items
            .iter()
            .skip(1)
            .any(|n| matches!(n, Node::Atom(a) if !a.is_quoted() && a.raw == flag))
    }

    /// A child `(name value)` read as text.
    pub fn value_of(&self, head: &str) -> Option<String> {
        self.find(head).and_then(|l| l.atom(1))
    }

    /// A yes/no child such as `(in_bom yes)`; a bare `(hide)` counts as yes.
    pub fn yes_no(&self, head: &str) -> Option<bool> {
        let l = self.find(head)?;
        match l.atom(1).as_deref() {
            None | Some("yes") | Some("true") => Some(true),
            Some(_) => Some(false),
        }
    }
}

/// Parse with the default [`Limits`].
pub fn parse(input: &[u8]) -> Result<Document, Error> {
    parse_with(input, Limits::default())
}

/// Parse a KiCad S-expression document.
pub fn parse_with(input: &[u8], limits: Limits) -> Result<Document, Error> {
    if input.len() > limits.max_bytes {
        return Err(Error::TooLarge {
            size: input.len(),
            limit: limits.max_bytes,
        });
    }
    let text = std::str::from_utf8(input).map_err(|e| Error::NotUtf8(e.valid_up_to()))?;
    let bytes = text.as_bytes();

    // Open lists, innermost last, each with the offset of its '('.
    let mut stack: Vec<(List, usize)> = Vec::new();
    let mut root: Option<List> = None;
    let mut pos = 0;

    loop {
        let ws_start = pos;
        while pos < bytes.len() && is_space(bytes[pos]) {
            pos += 1;
        }
        // A UTF-8 byte-order mark before the root is kept as leading text.
        if stack.is_empty() && root.is_none() && text[pos..].starts_with('\u{feff}') {
            pos += '\u{feff}'.len_utf8();
            while pos < bytes.len() && is_space(bytes[pos]) {
                pos += 1;
            }
        }
        let lead = &text[ws_start..pos];
        if pos >= bytes.len() {
            if let Some((_, open)) = stack.last() {
                return Err(Error::UnclosedList(*open));
            }
            return match root {
                Some(root) => Ok(Document {
                    root,
                    trailing: lead.to_string(),
                }),
                None => Err(Error::NoRootList),
            };
        }
        if root.is_some() {
            return Err(Error::TrailingContent(pos));
        }
        match bytes[pos] {
            b'(' => {
                if stack.len() >= limits.max_depth {
                    return Err(Error::TooDeep {
                        offset: pos,
                        limit: limits.max_depth,
                    });
                }
                stack.push((
                    List {
                        lead: lead.to_string(),
                        items: Vec::new(),
                        tail: String::new(),
                    },
                    pos,
                ));
                pos += 1;
            }
            b')' => {
                let Some((mut list, _)) = stack.pop() else {
                    return Err(Error::UnexpectedClose(pos));
                };
                list.tail = lead.to_string();
                pos += 1;
                match stack.last_mut() {
                    Some((parent, _)) => parent.items.push(Node::List(list)),
                    None => root = Some(list),
                }
            }
            b'"' => {
                let start = pos;
                pos += 1;
                loop {
                    match bytes.get(pos) {
                        None => return Err(Error::UnterminatedString(start)),
                        Some(b'\\') => pos += 2,
                        Some(b'"') => {
                            pos += 1;
                            break;
                        }
                        Some(_) => pos += 1,
                    }
                }
                let pos_end = pos.min(bytes.len());
                if pos > bytes.len() {
                    return Err(Error::UnterminatedString(start));
                }
                let atom = Atom {
                    lead: lead.to_string(),
                    raw: text[start..pos_end].to_string(),
                };
                match stack.last_mut() {
                    Some((parent, _)) => parent.items.push(Node::Atom(atom)),
                    None => return Err(Error::NoRootList),
                }
            }
            _ => {
                let start = pos;
                while pos < bytes.len()
                    && !is_space(bytes[pos])
                    && !matches!(bytes[pos], b'(' | b')' | b'"')
                {
                    pos += 1;
                }
                let atom = Atom {
                    lead: lead.to_string(),
                    raw: text[start..pos].to_string(),
                };
                match stack.last_mut() {
                    Some((parent, _)) => parent.items.push(Node::Atom(atom)),
                    None => return Err(Error::NoRootList),
                }
            }
        }
    }
}

fn is_space(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\n' | b'\r' | b'\x0c' | b'\x0b')
}

/// Parse a snippet that is a single list, such as a template for a new node.
pub fn parse_list(snippet: &str) -> Result<List, Error> {
    let doc = parse(snippet.as_bytes())?;
    Ok(doc.root)
}

/// How new nodes are laid out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Style {
    /// One level of indentation: a tab in KiCad 8 and later, two spaces in
    /// KiCad 6 and 7.
    pub indent: String,
    /// The line ending the file uses.
    pub newline: String,
}

impl Default for Style {
    fn default() -> Self {
        Style {
            indent: "\t".into(),
            newline: "\n".into(),
        }
    }
}

impl Style {
    /// Read the indentation and line ending from an existing document, from
    /// the whitespace before the root's first child list.
    pub fn detect(doc: &Document) -> Style {
        let mut style = Style::default();
        let sample = doc
            .root
            .items
            .iter()
            .skip(1)
            .map(Node::lead)
            .find(|l| l.contains('\n'));
        if let Some(lead) = sample {
            if lead.contains("\r\n") {
                style.newline = "\r\n".into();
            }
            let after = lead.rsplit('\n').next().unwrap_or("");
            if !after.is_empty() {
                style.indent = after.to_string();
            }
        }
        style
    }

    fn line(&self, depth: usize) -> String {
        let mut s = self.newline.clone();
        for _ in 0..depth {
            s.push_str(&self.indent);
        }
        s
    }
}

/// Lay out a list the way KiCad 8's writer does, as a child at `depth`
/// (the root's children are at depth 1): the keyword and leading atoms on
/// the opening line, each child list on its own line one level deeper, runs
/// of `xy` points on one line, and the closing parenthesis on its own line
/// when the list has child lists.
pub fn reformat(list: &mut List, depth: usize, style: &Style) {
    list.lead = style.line(depth);
    let mut has_child_list = false;
    let mut prev_was_xy = false;
    let mut first = true;
    for item in &mut list.items {
        match item {
            Node::Atom(a) => {
                a.lead = if first { String::new() } else { " ".into() };
                prev_was_xy = false;
            }
            Node::List(child) => {
                let is_xy = child.is("xy");
                reformat(child, depth + 1, style);
                if is_xy && prev_was_xy {
                    child.lead = " ".into();
                }
                has_child_list = true;
                prev_was_xy = is_xy;
            }
        }
        first = false;
    }
    list.tail = if has_child_list {
        style.line(depth)
    } else {
        String::new()
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rt(s: &str) {
        let doc = parse(s.as_bytes()).unwrap();
        assert_eq!(doc.to_text(), s);
    }

    #[test]
    fn round_trips_whitespace_and_strings() {
        rt("(a)");
        rt("  (a b \"c d\" (e 1.5) )\n");
        rt("(kicad_sch\r\n\t(version 20231120)\r\n\t(x \"q\\\"uote\\\\\")\r\n)\r\n");
        rt("\u{feff}(a)");
        rt("(a\"b\"c)");
    }

    #[test]
    fn errors_not_panics() {
        assert_eq!(parse(b"(a"), Err(Error::UnclosedList(0)));
        assert_eq!(parse(b"a)"), Err(Error::NoRootList));
        assert_eq!(parse(b"(a))"), Err(Error::TrailingContent(3)));
        assert_eq!(parse(b"(a \"b)"), Err(Error::UnterminatedString(3)));
        assert_eq!(parse(b"(a \"b\\"), Err(Error::UnterminatedString(3)));
        assert_eq!(parse(b""), Err(Error::NoRootList));
        assert_eq!(parse(b")"), Err(Error::UnexpectedClose(0)));
        assert!(matches!(parse(&[b'(', 0xff, b')']), Err(Error::NotUtf8(1))));
        let deep = "(".repeat(300) + &")".repeat(300);
        assert!(matches!(parse(deep.as_bytes()), Err(Error::TooDeep { .. })));
        let limits = Limits {
            max_bytes: 4,
            max_depth: 10,
        };
        assert!(matches!(
            parse_with(b"(abcd)", limits),
            Err(Error::TooLarge { .. })
        ));
    }

    #[test]
    fn atom_text_and_set() {
        let mut a = Atom::quoted("a \"b\"\n\\");
        assert_eq!(a.text(), "a \"b\"\n\\");
        a.set_text("x");
        assert_eq!(a.raw, "\"x\"");
        let mut b = Atom::bare("12");
        b.set_text("1 2");
        assert_eq!(b.raw, "\"1 2\"");
    }

    #[test]
    fn reformat_matches_kicad8_layout() {
        let mut l = parse_list(
            "(wire (pts (xy 1 2) (xy 3 4)) (stroke (width 0) (type default)) (uuid \"u\"))",
        )
        .unwrap();
        reformat(&mut l, 1, &Style::default());
        let mut s = String::new();
        write_list(&l, &mut s);
        assert_eq!(
            s,
            "\n\t(wire\n\t\t(pts\n\t\t\t(xy 1 2) (xy 3 4)\n\t\t)\n\t\t(stroke\n\t\t\t(width 0)\n\t\t\t(type default)\n\t\t)\n\t\t(uuid \"u\")\n\t)"
        );
    }
}
