//! Just enough iCalendar to read and write a `VTODO`.
//!
//! Not a general iCalendar library, and the gap is worth stating because the
//! format is large and this is a corner of it. What is here is content lines
//! with folding and the text escapes, the handful of `VTODO` properties the
//! app sets, and nothing else: no recurrence expansion, no timezone
//! components, no alarms, no participants.
//!
//! Two rules shape it, both from RFC 5545:
//!
//! * **Lines are folded at 75 octets** and unfolded by removing CRLF followed
//!   by one space or tab. A writer that ignores folding produces a file other
//!   clients may reject; a reader that ignores it reads a summary that stops
//!   mid-word. Both halves are here because both halves have bitten this
//!   format before.
//! * **Unknown properties are preserved.** A task written by Deckjack and
//!   edited by a phone comes back carrying properties this app has never
//!   heard of, and dropping them on the next save would quietly delete
//!   someone else's data. So a parse keeps every line it did not understand
//!   and a write puts them back.

use std::fmt::Write as _;

/// Lines are folded after this many octets, not characters: a multi-byte
/// character must not be split across the fold.
const FOLD_OCTETS: usize = 75;

/// One `NAME;PARAM=value:value` line, split up.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Line {
    pub name: String,
    /// In the order they appeared, because some clients care and round-trips
    /// should be boring.
    pub params: Vec<(String, String)>,
    /// Still escaped. [`unescape_text`] is applied by the property readers,
    /// not here, because escaping is per value type: `TEXT` has escapes and
    /// `DATE-TIME` does not.
    pub value: String,
}

impl Line {
    /// One parameter, case-insensitively. Used by the tests that prove a
    /// quoted parameter value containing a colon does not cut the line in the
    /// wrong place — the property readers here want no parameters, which is
    /// why nothing else calls it yet.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn param(&self, name: &str) -> Option<&str> {
        self.params
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

/// Unfold and split a calendar object into content lines.
pub fn parse_lines(text: &str) -> Vec<Line> {
    let mut out = Vec::new();
    for raw in unfold(text) {
        if raw.trim().is_empty() {
            continue;
        }
        if let Some(line) = parse_line(&raw) {
            out.push(line);
        }
    }
    out
}

/// Join continuation lines. A folded line is CRLF (or LF) then one space or
/// tab, and the whitespace is part of the fold rather than the value.
fn unfold(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for line in text.split('\n') {
        let line = line.strip_suffix('\r').unwrap_or(line);
        match line.strip_prefix([' ', '\t']) {
            Some(rest) => {
                if let Some(last) = out.last_mut() {
                    last.push_str(rest);
                } else {
                    // A file that opens with a continuation has no line to
                    // continue. Kept as its own line rather than dropped, so
                    // a malformed file is visible instead of silently short.
                    out.push(rest.to_string());
                }
            }
            None => out.push(line.to_string()),
        }
    }
    out
}

fn parse_line(raw: &str) -> Option<Line> {
    // The value starts at the first colon that is not inside a quoted
    // parameter value: `ATTENDEE;CN="Smith, J:r":mailto:x` is one line with
    // one value, and splitting on the first colon would cut it in the name.
    let bytes = raw.as_bytes();
    let mut in_quotes = false;
    let mut colon = None;
    for (i, b) in bytes.iter().enumerate() {
        match b {
            b'"' => in_quotes = !in_quotes,
            b':' if !in_quotes => {
                colon = Some(i);
                break;
            }
            _ => {}
        }
    }
    let colon = colon?;
    let (head, value) = (&raw[..colon], &raw[colon + 1..]);

    let mut parts = split_params(head);
    let name = parts.remove(0).to_ascii_uppercase();
    let params = parts
        .into_iter()
        .filter_map(|p| {
            let (k, v) = p.split_once('=')?;
            Some((k.to_ascii_uppercase(), v.trim_matches('"').to_string()))
        })
        .collect();

    Some(Line {
        name,
        params,
        value: value.to_string(),
    })
}

/// Split `NAME;A=1;B="x;y"` on semicolons outside quotes.
fn split_params(head: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut start = 0;
    let mut in_quotes = false;
    for (i, c) in head.char_indices() {
        match c {
            '"' => in_quotes = !in_quotes,
            ';' if !in_quotes => {
                out.push(&head[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    out.push(&head[start..]);
    out
}

/// Write one content line, folded, with CRLF endings.
///
/// CRLF and not LF: RFC 5545 says CRLF, and the clients that are strict about
/// it are the ones a file has to survive.
pub fn write_line(out: &mut String, name: &str, params: &[(&str, &str)], value: &str) {
    let mut line = String::with_capacity(name.len() + value.len() + 8);
    line.push_str(name);
    for (k, v) in params {
        let _ = write!(line, ";{k}={v}");
    }
    line.push(':');
    line.push_str(value);
    fold_into(out, &line);
}

fn fold_into(out: &mut String, line: &str) {
    let mut octets = 0;
    let mut first = true;
    for c in line.chars() {
        let width = c.len_utf8();
        // The continuation's leading space counts toward the next line's
        // budget, so the limit after a fold is one octet lower.
        let budget = if first { FOLD_OCTETS } else { FOLD_OCTETS - 1 };
        if octets + width > budget {
            out.push_str("\r\n ");
            octets = 0;
            first = false;
        }
        out.push(c);
        octets += width;
    }
    out.push_str("\r\n");
}

/// Escape a `TEXT` value: backslash, semicolon, comma and newline.
pub fn escape_text(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for c in value.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            ';' => out.push_str("\\;"),
            ',' => out.push_str("\\,"),
            '\n' => out.push_str("\\n"),
            // A bare CR would end the content line. Dropped rather than
            // escaped: CRLF has already become \n by the time text reaches
            // here, so a lone CR is a stray.
            '\r' => {}
            c => out.push(c),
        }
    }
    out
}

pub fn unescape_text(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut chars = value.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n') | Some('N') => out.push('\n'),
            Some('\\') => out.push('\\'),
            Some(';') => out.push(';'),
            Some(',') => out.push(','),
            // An escape this does not know is kept as written, both
            // characters, rather than swallowing the backslash. Guessing
            // would corrupt a value some other client wrote on purpose.
            Some(other) => {
                out.push('\\');
                out.push(other);
            }
            None => out.push('\\'),
        }
    }
    out
}

/// A `VCALENDAR` holding one `VTODO`, as this app reads and writes it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Todo {
    pub uid: String,
    pub summary: String,
    /// `COMPLETED` in iCalendar terms; `NEEDS-ACTION` otherwise.
    pub done: bool,
    /// 1 is highest, 9 lowest, 0 means unset — iCalendar's own scale.
    pub priority: u8,
    /// `DUE`, as written: `20260110` for a date or `20260110T150000Z` for a
    /// moment. Kept as text because this app does not do date arithmetic and
    /// a parsed-then-reserialized date is a chance to change someone's data.
    pub due: Option<String>,
    pub created: Option<String>,
    pub completed_at: Option<String>,
    /// Properties inside the `VTODO` this app does not model, in order, so a
    /// task edited on a phone does not lose what the phone set.
    pub extra: Vec<Line>,
    /// `VCALENDAR`-level properties other than `VERSION` and `PRODID`.
    pub calendar_extra: Vec<Line>,
}

/// Properties this app owns. Anything else inside a `VTODO` is `extra`.
const KNOWN: &[&str] = &[
    "UID",
    "SUMMARY",
    "STATUS",
    "PRIORITY",
    "DUE",
    "CREATED",
    "DTSTAMP",
    "LAST-MODIFIED",
    "COMPLETED",
    "PERCENT-COMPLETE",
];

#[derive(Debug, thiserror::Error)]
pub enum IcalError {
    /// Not a calendar object at all. Told apart from [`IcalError::NoTodo`]
    /// because the two deserve different treatment: a `.ics` holding only a
    /// `VEVENT` is a calendar file that is not a task, and a Calendar app may
    /// well put one in the same folder, so skipping it is right. A file that
    /// is not a calendar is a file something has corrupted, and the operator
    /// should be told rather than have it quietly disappear from the list.
    #[error("this file is not a calendar object: it has no BEGIN:VCALENDAR")]
    NotACalendar,

    #[error("this file has no VTODO in it, so it is not a task")]
    NoTodo,

    #[error("this VTODO has no UID, which is the one property it must have")]
    NoUid,
}

/// Read the first `VTODO` in a calendar object.
pub fn parse_todo(text: &str) -> Result<Todo, IcalError> {
    let lines = parse_lines(text);
    if !lines
        .iter()
        .any(|l| l.name == "BEGIN" && l.value.eq_ignore_ascii_case("VCALENDAR"))
    {
        return Err(IcalError::NotACalendar);
    }
    let mut todo = Todo::default();
    let mut depth_todo = false;
    let mut found = false;
    let mut other_component = 0usize;

    for line in lines {
        match (line.name.as_str(), line.value.to_ascii_uppercase().as_str()) {
            ("BEGIN", "VTODO") if !found => {
                depth_todo = true;
                found = true;
                continue;
            }
            ("END", "VTODO") if depth_todo => {
                depth_todo = false;
                continue;
            }
            // A VALARM or VTIMEZONE inside the VTODO: skipped as a unit, and
            // kept, because an alarm is what makes a reminder fire and
            // dropping it would silence it.
            ("BEGIN", _) if depth_todo => {
                other_component += 1;
                todo.extra.push(line);
                continue;
            }
            ("END", _) if depth_todo && other_component > 0 => {
                other_component -= 1;
                todo.extra.push(line);
                continue;
            }
            _ => {}
        }

        if other_component > 0 {
            todo.extra.push(line);
            continue;
        }

        if !depth_todo {
            if !matches!(
                line.name.as_str(),
                "BEGIN" | "END" | "VERSION" | "PRODID" | "CALSCALE" | "METHOD"
            ) {
                todo.calendar_extra.push(line);
            }
            continue;
        }

        match line.name.as_str() {
            "UID" => todo.uid = line.value.clone(),
            "SUMMARY" => todo.summary = unescape_text(&line.value),
            "STATUS" => todo.done = line.value.eq_ignore_ascii_case("COMPLETED"),
            "PRIORITY" => todo.priority = line.value.trim().parse().unwrap_or(0),
            "DUE" => todo.due = Some(line.value.clone()),
            "CREATED" => todo.created = Some(line.value.clone()),
            "COMPLETED" => todo.completed_at = Some(line.value.clone()),
            // DTSTAMP, LAST-MODIFIED and PERCENT-COMPLETE are rewritten on
            // every save, so they are not carried over.
            name if KNOWN.contains(&name) => {}
            _ => todo.extra.push(line),
        }
    }

    if !found {
        return Err(IcalError::NoTodo);
    }
    if todo.uid.trim().is_empty() {
        return Err(IcalError::NoUid);
    }
    Ok(todo)
}

/// Write a calendar object holding this one task.
///
/// `stamp` is `DTSTAMP` and `LAST-MODIFIED`, passed in rather than read from a
/// clock so the output is a pure function of its input and every rule here is
/// testable without a fixed-clock trick.
pub fn write_todo(todo: &Todo, stamp: &str) -> String {
    let mut out = String::with_capacity(512);
    write_line(&mut out, "BEGIN", &[], "VCALENDAR");
    write_line(&mut out, "VERSION", &[], "2.0");
    // Identifies what wrote the file, which is the first thing anyone
    // debugging a sync conflict wants to know.
    write_line(&mut out, "PRODID", &[], "-//Deckjack//Tasks 0.1//EN");
    for line in &todo.calendar_extra {
        write_raw(&mut out, line);
    }
    write_line(&mut out, "BEGIN", &[], "VTODO");
    write_line(&mut out, "UID", &[], &todo.uid);
    write_line(&mut out, "DTSTAMP", &[], stamp);
    if let Some(created) = &todo.created {
        write_line(&mut out, "CREATED", &[], created);
    }
    write_line(&mut out, "LAST-MODIFIED", &[], stamp);
    write_line(&mut out, "SUMMARY", &[], &escape_text(&todo.summary));
    write_line(
        &mut out,
        "STATUS",
        &[],
        if todo.done {
            "COMPLETED"
        } else {
            "NEEDS-ACTION"
        },
    );
    // Both are written when done, because clients disagree about which one
    // they read: some show a task as open if PERCENT-COMPLETE is missing even
    // with STATUS:COMPLETED.
    if todo.done {
        write_line(&mut out, "PERCENT-COMPLETE", &[], "100");
        write_line(
            &mut out,
            "COMPLETED",
            &[],
            todo.completed_at.as_deref().unwrap_or(stamp),
        );
    }
    if todo.priority > 0 {
        write_line(&mut out, "PRIORITY", &[], &todo.priority.to_string());
    }
    if let Some(due) = &todo.due {
        // A date-only DUE must say so, or a client reads `20260110` as an
        // invalid date-time and drops it.
        let params: Vec<(&str, &str)> = if due.len() == 8 {
            vec![("VALUE", "DATE")]
        } else {
            Vec::new()
        };
        write_line(&mut out, "DUE", &params, due);
    }
    for line in &todo.extra {
        write_raw(&mut out, line);
    }
    write_line(&mut out, "END", &[], "VTODO");
    write_line(&mut out, "END", &[], "VCALENDAR");
    out
}

/// Put back a line this app did not understand, exactly as it arrived.
fn write_raw(out: &mut String, line: &Line) {
    let params: Vec<(&str, &str)> = line
        .params
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    write_line(out, &line.name, &params, &line.value);
}

#[cfg(test)]
mod tests {
    use super::*;

    const STAMP: &str = "20260110T120000Z";

    #[test]
    fn a_folded_line_is_rejoined() {
        let text = "SUMMARY:a very long\r\n  summary\r\n";
        let lines = parse_lines(text);
        assert_eq!(lines.len(), 1);
        // The fold's own space is removed; the one that was in the value
        // stays. Getting this backwards eats a space per fold.
        assert_eq!(lines[0].value, "a very long summary");
    }

    #[test]
    fn folding_respects_octets_not_characters() {
        let mut out = String::new();
        // Each é is two octets, so a character-counting folder would put the
        // fold in the wrong place and could split one in half.
        write_line(&mut out, "SUMMARY", &[], &"é".repeat(80));
        for line in out.split("\r\n") {
            assert!(line.len() <= FOLD_OCTETS, "{} octets: {line}", line.len());
        }
        // And it still round-trips, which is what the octet rule is for.
        let back = parse_lines(&out);
        assert_eq!(back[0].value, "é".repeat(80));
    }

    #[test]
    fn a_colon_inside_a_quoted_parameter_does_not_end_the_name() {
        let lines = parse_lines("ATTENDEE;CN=\"Smith, J:r\":mailto:x@example.com\r\n");
        assert_eq!(lines[0].name, "ATTENDEE");
        assert_eq!(lines[0].param("CN"), Some("Smith, J:r"));
        assert_eq!(lines[0].value, "mailto:x@example.com");
    }

    #[test]
    fn text_escapes_round_trip() {
        for value in [
            "plain",
            "semi;colon",
            "comma,separated",
            "back\\slash",
            "two\nlines",
            "all\\of;them,at\nonce",
        ] {
            assert_eq!(unescape_text(&escape_text(value)), value, "{value:?}");
        }
    }

    #[test]
    fn an_unknown_escape_is_left_alone() {
        // `\q` means nothing, and swallowing the backslash would change a
        // value some other client wrote deliberately.
        assert_eq!(unescape_text("a\\qb"), "a\\qb");
        assert_eq!(unescape_text("trailing\\"), "trailing\\");
    }

    #[test]
    fn a_task_round_trips() {
        let todo = Todo {
            uid: "abc-123".into(),
            summary: "Pay the; invoice, twice".into(),
            done: false,
            priority: 2,
            due: Some("20260110".into()),
            created: Some("20260101T090000Z".into()),
            ..Todo::default()
        };
        let text = write_todo(&todo, STAMP);
        let back = parse_todo(&text).unwrap();

        assert_eq!(back.uid, "abc-123");
        assert_eq!(back.summary, "Pay the; invoice, twice");
        assert_eq!(back.priority, 2);
        assert_eq!(back.due.as_deref(), Some("20260110"));
        assert_eq!(back.created.as_deref(), Some("20260101T090000Z"));
        assert!(!back.done);
    }

    #[test]
    fn a_date_only_due_says_it_is_a_date() {
        let todo = Todo {
            uid: "u".into(),
            due: Some("20260110".into()),
            ..Todo::default()
        };
        let text = write_todo(&todo, STAMP);
        assert!(text.contains("DUE;VALUE=DATE:20260110"), "{text}");

        // A date-time carries no VALUE parameter, which is the default.
        let todo = Todo {
            uid: "u".into(),
            due: Some("20260110T150000Z".into()),
            ..Todo::default()
        };
        assert!(write_todo(&todo, STAMP).contains("DUE:20260110T150000Z"));
    }

    #[test]
    fn done_writes_every_property_a_client_might_read() {
        let todo = Todo {
            uid: "u".into(),
            done: true,
            ..Todo::default()
        };
        let text = write_todo(&todo, STAMP);
        assert!(text.contains("STATUS:COMPLETED"), "{text}");
        assert!(text.contains("PERCENT-COMPLETE:100"), "{text}");
        assert!(text.contains(&format!("COMPLETED:{STAMP}")), "{text}");
        assert!(parse_todo(&text).unwrap().done);
    }

    #[test]
    fn properties_this_app_does_not_know_survive_a_save() {
        // The case this matters in: a task edited on a phone comes back with
        // categories, a location and an alarm. Saving it from Deckjack must
        // not be how those disappear.
        let from_phone = "\
BEGIN:VCALENDAR\r\n\
VERSION:2.0\r\n\
PRODID:-//Some Phone//EN\r\n\
BEGIN:VTODO\r\n\
UID:phone-1\r\n\
SUMMARY:Buy milk\r\n\
STATUS:NEEDS-ACTION\r\n\
CATEGORIES:Errands,Home\r\n\
LOCATION:The shop\r\n\
X-APPLE-SORT-ORDER:12\r\n\
BEGIN:VALARM\r\n\
ACTION:DISPLAY\r\n\
TRIGGER:-PT15M\r\n\
END:VALARM\r\n\
END:VTODO\r\n\
END:VCALENDAR\r\n";

        let todo = parse_todo(from_phone).unwrap();
        assert_eq!(todo.summary, "Buy milk");

        let saved = write_todo(&todo, STAMP);
        for kept in [
            "CATEGORIES:Errands,Home",
            "LOCATION:The shop",
            "X-APPLE-SORT-ORDER:12",
            "BEGIN:VALARM",
            "ACTION:DISPLAY",
            "TRIGGER:-PT15M",
            "END:VALARM",
        ] {
            assert!(saved.contains(kept), "lost {kept}:\n{saved}");
        }
    }

    #[test]
    fn a_file_with_no_todo_is_refused() {
        let event = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nBEGIN:VEVENT\r\nUID:e\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
        assert!(matches!(parse_todo(event), Err(IcalError::NoTodo)));
    }

    #[test]
    fn a_todo_with_no_uid_is_refused() {
        let text = "BEGIN:VCALENDAR\r\nBEGIN:VTODO\r\nSUMMARY:x\r\nEND:VTODO\r\nEND:VCALENDAR\r\n";
        assert!(matches!(parse_todo(text), Err(IcalError::NoUid)));
    }

    #[test]
    fn a_summary_with_a_newline_survives_the_round_trip() {
        // The one that breaks naive writers: a raw newline would end the
        // content line and the rest would parse as a property.
        let todo = Todo {
            uid: "u".into(),
            summary: "first line\nsecond line".into(),
            ..Todo::default()
        };
        let text = write_todo(&todo, STAMP);
        assert!(
            !text.contains("first line\r\nsecond"),
            "wrote a real newline"
        );
        assert_eq!(
            parse_todo(&text).unwrap().summary,
            "first line\nsecond line"
        );
    }

    #[test]
    fn every_line_ends_with_crlf() {
        let todo = Todo {
            uid: "u".into(),
            summary: "x".into(),
            ..Todo::default()
        };
        let text = write_todo(&todo, STAMP);
        assert!(!text.contains('\n') || text.contains("\r\n"));
        for line in text.split("\r\n").filter(|l| !l.is_empty()) {
            assert!(!line.contains('\n'), "bare LF in {line:?}");
        }
    }
}
