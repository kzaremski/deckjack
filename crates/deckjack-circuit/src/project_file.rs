//! The `.kicad_pro` project file, read and written without damage.
//!
//! A `.kicad_pro` is JSON. Rather than parse it into a map and serialize the
//! map back (which reorders keys, renormalizes numbers and loses whatever
//! formatting KiCad chose), this keeps the original text and a tree of byte
//! spans over it. Reading a value reads its span; setting one replaces just
//! that span's bytes. An unmodified load and save is therefore byte-identical,
//! and a single setting change is a one-line diff.
//!
//! The parser accepts standard JSON (RFC 8259) only, with the same hostile-
//! input bounds as the S-expression reader: a size cap, a depth cap, and
//! errors rather than panics.

use std::collections::BTreeMap;

/// Refuse project files larger than this. Real ones are tens of KB.
pub const MAX_BYTES: usize = 16 * 1024 * 1024;
/// Refuse nesting deeper than this. KiCad's files nest about six deep.
pub const MAX_DEPTH: usize = 128;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    #[error("the project file is {0} bytes, over the limit")]
    TooLarge(usize),
    #[error("the project file is not valid UTF-8")]
    NotUtf8,
    #[error("the project file is not valid JSON at byte {0}")]
    Syntax(usize),
    #[error("the project file nests deeper than {MAX_DEPTH} levels")]
    TooDeep,
    #[error("no value at {0}")]
    NoSuchKey(String),
}

/// A JSON value with the byte span it occupies in the text.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Null,
    Bool(bool),
    Number(String),
    String(String),
    Array(Vec<Spanned>),
    Object(Vec<(String, Spanned)>),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Spanned {
    pub value: Value,
    pub start: usize,
    pub end: usize,
}

impl Spanned {
    pub fn get(&self, key: &str) -> Option<&Spanned> {
        match &self.value {
            Value::Object(entries) => entries.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }
    pub fn as_str(&self) -> Option<&str> {
        match &self.value {
            Value::String(s) => Some(s),
            _ => None,
        }
    }
    pub fn as_i64(&self) -> Option<i64> {
        match &self.value {
            Value::Number(n) => n.parse().ok(),
            _ => None,
        }
    }
}

/// A loaded project file.
#[derive(Debug, Clone)]
pub struct ProjectFile {
    text: String,
    root: Spanned,
}

impl ProjectFile {
    pub fn parse(bytes: &[u8]) -> Result<ProjectFile, Error> {
        if bytes.len() > MAX_BYTES {
            return Err(Error::TooLarge(bytes.len()));
        }
        let text = std::str::from_utf8(bytes)
            .map_err(|_| Error::NotUtf8)?
            .to_string();
        let root = Parser {
            b: text.as_bytes(),
            pos: 0,
            depth: 0,
        }
        .document()?;
        Ok(ProjectFile { text, root })
    }

    /// The file's text, which is the input until something is set.
    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn root(&self) -> &Spanned {
        &self.root
    }

    /// The value at a path of object keys, such as `["meta", "filename"]`.
    pub fn get(&self, path: &[&str]) -> Option<&Spanned> {
        let mut at = &self.root;
        for key in path {
            at = at.get(key)?;
        }
        Some(at)
    }

    /// Replace the value at `path` with `json` (already valid JSON text, such
    /// as `"\"x\""` or `3`). Only that value's bytes change.
    pub fn set_raw(&mut self, path: &[&str], json: &str) -> Result<(), Error> {
        let at = self
            .get(path)
            .ok_or_else(|| Error::NoSuchKey(path.join(".")))?;
        let (start, end) = (at.start, at.end);
        let mut text = String::with_capacity(self.text.len() + json.len());
        text.push_str(&self.text[..start]);
        text.push_str(json);
        text.push_str(&self.text[end..]);
        *self = ProjectFile::parse(text.as_bytes())?;
        Ok(())
    }

    /// Set a string value, quoting it as JSON.
    pub fn set_string(&mut self, path: &[&str], value: &str) -> Result<(), Error> {
        self.set_raw(path, &json_quote(value))
    }

    /// `text_variables` as a map, for `${VAR}` expansion.
    pub fn text_variables(&self) -> BTreeMap<String, String> {
        let mut out = BTreeMap::new();
        if let Some(Spanned {
            value: Value::Object(entries),
            ..
        }) = self.get(&["text_variables"])
        {
            for (k, v) in entries {
                if let Some(s) = v.as_str() {
                    out.insert(k.clone(), s.to_string());
                }
            }
        }
        out
    }
}

/// Quote a string as JSON.
pub fn json_quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

struct Parser<'a> {
    b: &'a [u8],
    pos: usize,
    depth: usize,
}

impl Parser<'_> {
    fn document(mut self) -> Result<Spanned, Error> {
        self.ws();
        let v = self.value()?;
        self.ws();
        if self.pos != self.b.len() {
            return Err(Error::Syntax(self.pos));
        }
        Ok(v)
    }

    fn ws(&mut self) {
        while self.pos < self.b.len() && matches!(self.b[self.pos], b' ' | b'\t' | b'\n' | b'\r') {
            self.pos += 1;
        }
    }

    fn err<T>(&self) -> Result<T, Error> {
        Err(Error::Syntax(self.pos))
    }

    fn value(&mut self) -> Result<Spanned, Error> {
        let start = self.pos;
        let value = match self.b.get(self.pos) {
            Some(b'{') => self.object()?,
            Some(b'[') => self.array()?,
            Some(b'"') => Value::String(self.string()?),
            Some(b't') => self.literal("true", Value::Bool(true))?,
            Some(b'f') => self.literal("false", Value::Bool(false))?,
            Some(b'n') => self.literal("null", Value::Null)?,
            Some(c) if *c == b'-' || c.is_ascii_digit() => self.number()?,
            _ => return self.err(),
        };
        Ok(Spanned {
            value,
            start,
            end: self.pos,
        })
    }

    fn literal(&mut self, word: &str, v: Value) -> Result<Value, Error> {
        if self.b[self.pos..].starts_with(word.as_bytes()) {
            self.pos += word.len();
            Ok(v)
        } else {
            self.err()
        }
    }

    fn number(&mut self) -> Result<Value, Error> {
        let start = self.pos;
        if self.b.get(self.pos) == Some(&b'-') {
            self.pos += 1;
        }
        let digits = |p: &mut Self| {
            let s = p.pos;
            while p.pos < p.b.len() && p.b[p.pos].is_ascii_digit() {
                p.pos += 1;
            }
            p.pos > s
        };
        if !digits(self) {
            return self.err();
        }
        if self.b.get(self.pos) == Some(&b'.') {
            self.pos += 1;
            if !digits(self) {
                return self.err();
            }
        }
        if matches!(self.b.get(self.pos), Some(b'e' | b'E')) {
            self.pos += 1;
            if matches!(self.b.get(self.pos), Some(b'+' | b'-')) {
                self.pos += 1;
            }
            if !digits(self) {
                return self.err();
            }
        }
        // The slice is ASCII digits and signs, so always UTF-8.
        Ok(Value::Number(
            String::from_utf8_lossy(&self.b[start..self.pos]).into_owned(),
        ))
    }

    fn string(&mut self) -> Result<String, Error> {
        self.pos += 1; // opening quote
        let mut out = String::new();
        loop {
            let Some(&c) = self.b.get(self.pos) else {
                return self.err();
            };
            match c {
                b'"' => {
                    self.pos += 1;
                    return Ok(out);
                }
                b'\\' => {
                    self.pos += 1;
                    let Some(&e) = self.b.get(self.pos) else {
                        return self.err();
                    };
                    self.pos += 1;
                    match e {
                        b'"' => out.push('"'),
                        b'\\' => out.push('\\'),
                        b'/' => out.push('/'),
                        b'b' => out.push('\u{8}'),
                        b'f' => out.push('\u{c}'),
                        b'n' => out.push('\n'),
                        b'r' => out.push('\r'),
                        b't' => out.push('\t'),
                        b'u' => {
                            let hi = self.hex4()?;
                            let code = if (0xd800..0xdc00).contains(&hi) {
                                if self.b.get(self.pos) != Some(&b'\\')
                                    || self.b.get(self.pos + 1) != Some(&b'u')
                                {
                                    return self.err();
                                }
                                self.pos += 2;
                                let lo = self.hex4()?;
                                if !(0xdc00..0xe000).contains(&lo) {
                                    return self.err();
                                }
                                0x10000 + ((hi - 0xd800) << 10) + (lo - 0xdc00)
                            } else {
                                hi
                            };
                            match char::from_u32(code) {
                                Some(ch) => out.push(ch),
                                None => return self.err(),
                            }
                        }
                        _ => return self.err(),
                    }
                }
                c if c < 0x20 => return self.err(),
                _ => {
                    // Copy one UTF-8 character. The input is valid UTF-8,
                    // so the width from the lead byte is in bounds.
                    let width = utf8_width(c);
                    let end = (self.pos + width).min(self.b.len());
                    out.push_str(&String::from_utf8_lossy(&self.b[self.pos..end]));
                    self.pos = end;
                }
            }
        }
    }

    fn hex4(&mut self) -> Result<u32, Error> {
        let Some(slice) = self.b.get(self.pos..self.pos + 4) else {
            return self.err();
        };
        let s = std::str::from_utf8(slice).map_err(|_| Error::Syntax(self.pos))?;
        let v = u32::from_str_radix(s, 16).map_err(|_| Error::Syntax(self.pos))?;
        self.pos += 4;
        Ok(v)
    }

    fn enter(&mut self) -> Result<(), Error> {
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            Err(Error::TooDeep)
        } else {
            Ok(())
        }
    }

    fn array(&mut self) -> Result<Value, Error> {
        self.enter()?;
        self.pos += 1;
        let mut items = Vec::new();
        self.ws();
        if self.b.get(self.pos) == Some(&b']') {
            self.pos += 1;
            self.depth -= 1;
            return Ok(Value::Array(items));
        }
        loop {
            self.ws();
            items.push(self.value()?);
            self.ws();
            match self.b.get(self.pos) {
                Some(b',') => self.pos += 1,
                Some(b']') => {
                    self.pos += 1;
                    self.depth -= 1;
                    return Ok(Value::Array(items));
                }
                _ => return self.err(),
            }
        }
    }

    fn object(&mut self) -> Result<Value, Error> {
        self.enter()?;
        self.pos += 1;
        let mut entries = Vec::new();
        self.ws();
        if self.b.get(self.pos) == Some(&b'}') {
            self.pos += 1;
            self.depth -= 1;
            return Ok(Value::Object(entries));
        }
        loop {
            self.ws();
            if self.b.get(self.pos) != Some(&b'"') {
                return self.err();
            }
            let key = self.string()?;
            self.ws();
            if self.b.get(self.pos) != Some(&b':') {
                return self.err();
            }
            self.pos += 1;
            self.ws();
            let v = self.value()?;
            entries.push((key, v));
            self.ws();
            match self.b.get(self.pos) {
                Some(b',') => self.pos += 1,
                Some(b'}') => {
                    self.pos += 1;
                    self.depth -= 1;
                    return Ok(Value::Object(entries));
                }
                _ => return self.err(),
            }
        }
    }
}

fn utf8_width(lead: u8) -> usize {
    match lead {
        0xf0..=0xff => 4,
        0xe0..=0xef => 3,
        0xc0..=0xdf => 2,
        _ => 1,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn get_and_set_keep_everything_else() {
        let src = "{\n  \"meta\": {\n    \"filename\": \"a.kicad_pro\",\n    \"version\": 1\n  },\n  \"x\": [1, 2.5e3, true, null]\n}\n";
        let mut p = ProjectFile::parse(src.as_bytes()).unwrap();
        assert_eq!(p.text(), src);
        assert_eq!(
            p.get(&["meta", "filename"]).unwrap().as_str(),
            Some("a.kicad_pro")
        );
        assert_eq!(p.get(&["meta", "version"]).unwrap().as_i64(), Some(1));
        p.set_string(&["meta", "filename"], "b \"q\".kicad_pro")
            .unwrap();
        assert_eq!(
            p.text(),
            src.replace("\"a.kicad_pro\"", "\"b \\\"q\\\".kicad_pro\"")
        );
        assert!(p.set_raw(&["nope"], "1").is_err());
    }

    #[test]
    fn rejects_bad_json() {
        for bad in [
            "",
            "{",
            "{\"a\" 1}",
            "[1,]",
            "\"\\x\"",
            "01x",
            "{\"a\":1}}",
            "\"\u{1}\"",
        ] {
            assert!(ProjectFile::parse(bad.as_bytes()).is_err(), "{bad:?}");
        }
        let deep = "[".repeat(200) + &"]".repeat(200);
        assert_eq!(
            ProjectFile::parse(deep.as_bytes()).unwrap_err(),
            Error::TooDeep
        );
        assert_eq!(
            ProjectFile::parse("\"\\ud83d\\ude00\"".as_bytes())
                .unwrap()
                .root()
                .as_str(),
            Some("😀")
        );
    }
}
