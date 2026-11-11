//! Tasks on disk: one `VTODO` per `.ics` file under `Tasks/`.
//!
//! The file is the task. There is no index and no database, for the same
//! reason notes are Markdown: these files sync over CalDAV, so a task made
//! here appears on a phone through DAVx5 without Deckjack in the loop, and
//! survives Deckjack being deleted.
//!
//! A task's identity is its `UID`, and its filename is `<uid>.ics` — which is
//! also what CalDAV servers do, so a file written here can be PUT as-is.

use std::io;
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::ical::{self, Todo};

pub const TASKS_DIR: &str = "Tasks";
pub const EXTENSION: &str = "ics";

/// Longest summary accepted. A task is a line, not a document; a 10KB summary
/// is a paste accident and iCalendar folding would make the file absurd.
pub const MAX_SUMMARY: usize = 1000;

#[derive(Debug, thiserror::Error)]
pub enum TaskError {
    #[error("{0:?} is not a task id")]
    BadId(String),

    #[error("a task needs something to say")]
    EmptySummary,

    #[error("that summary is longer than {MAX_SUMMARY} characters")]
    SummaryTooLong,

    #[error("{0:?} is not a date (expected YYYY-MM-DD, or YYYY-MM-DDTHH:MM for a time)")]
    BadDue(String),

    #[error("priority {0} is outside 0-9; 1 is highest, 9 lowest, 0 unset")]
    BadPriority(u8),

    #[error("there is no task {0:?}")]
    NoSuchTask(String),

    #[error("cannot {doing} {id:?}: {source}")]
    Io {
        doing: &'static str,
        id: String,
        #[source]
        source: io::Error,
    },

    #[error("{id:?} is not a task file: {source}")]
    Malformed {
        id: String,
        #[source]
        source: ical::IcalError,
    },
}

/// One task, as the UI and the palette see it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Task {
    /// The `UID`, which is also the filename without `.ics`.
    pub id: String,
    pub summary: String,
    pub done: bool,
    /// 1 highest, 9 lowest, 0 unset.
    pub priority: u8,
    /// As iCalendar wrote it: `20260110` or `20260110T150000Z`.
    pub due: Option<String>,
}

impl Task {
    fn from_todo(todo: &Todo) -> Task {
        Task {
            id: todo.uid.clone(),
            summary: todo.summary.clone(),
            done: todo.done,
            priority: todo.priority,
            due: todo.due.clone(),
        }
    }
}

pub fn root(working_dir: &Path) -> Result<PathBuf, TaskError> {
    let root = working_dir.join(TASKS_DIR);
    std::fs::create_dir_all(&root).map_err(|source| TaskError::Io {
        doing: "create the tasks folder",
        id: TASKS_DIR.to_string(),
        source,
    })?;
    Ok(root)
}

/// A `UID` may be almost anything, but it becomes a filename, so what is
/// accepted here is narrower than what iCalendar allows.
///
/// A task written by another client with a `UID` containing a slash is read
/// (its filename came from that client, not from here) but cannot be
/// addressed by id through this app. That is the honest trade: the
/// alternative is deriving a filename from a hash and losing the property
/// that a Deckjack file can be PUT to a CalDAV server unchanged.
fn path_for(root: &Path, id: &str) -> Result<PathBuf, TaskError> {
    let trimmed = id.trim();
    let ok = !trimmed.is_empty()
        && trimmed.len() <= 200
        && !trimmed.starts_with('.')
        && trimmed
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '@' | '.'))
        && !trimmed.contains("..");
    if !ok {
        return Err(TaskError::BadId(trimmed.to_string()));
    }
    Ok(root.join(format!("{trimmed}.{EXTENSION}")))
}

/// Every task. Unreadable files are reported rather than hidden, because a
/// task that silently vanished from the list is worse than one shown as
/// broken.
pub fn list(root: &Path) -> Result<(Vec<Task>, Vec<String>), TaskError> {
    let entries = match std::fs::read_dir(root) {
        Ok(entries) => entries,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok((Vec::new(), Vec::new())),
        Err(source) => {
            return Err(TaskError::Io {
                doing: "read the tasks folder",
                id: TASKS_DIR.to_string(),
                source,
            });
        }
    };

    let mut tasks = Vec::new();
    let mut broken = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some(EXTENSION) {
            continue;
        }
        if !entry.file_type().is_ok_and(|t| t.is_file()) {
            continue;
        }
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default()
            .to_string();
        let Ok(text) = std::fs::read_to_string(&path) else {
            broken.push(name);
            continue;
        };
        match ical::parse_todo(&text) {
            Ok(todo) => tasks.push(Task::from_todo(&todo)),
            // A calendar file holding a VEVENT is not broken, it is just not a
            // task: the Calendar app's files may share this folder one day.
            // Anything that is not a calendar object, or a VTODO with no UID,
            // is a file something has damaged and is reported.
            Err(ical::IcalError::NoTodo) => {}
            Err(_) => broken.push(name),
        }
    }

    // Not done first, then by due date (undated last), then priority, then
    // summary. Decided here rather than in the UI so the palette, an agent
    // and the panel all see one order.
    tasks.sort_by(|a, b| {
        a.done
            .cmp(&b.done)
            .then_with(|| match (&a.due, &b.due) {
                (Some(x), Some(y)) => x.cmp(y),
                (Some(_), None) => std::cmp::Ordering::Less,
                (None, Some(_)) => std::cmp::Ordering::Greater,
                (None, None) => std::cmp::Ordering::Equal,
            })
            .then_with(|| rank(a.priority).cmp(&rank(b.priority)))
            .then_with(|| a.summary.cmp(&b.summary))
    });
    broken.sort();
    Ok((tasks, broken))
}

/// Priority for sorting: 1 is highest and 0 is unset, so 0 must sort after 9.
fn rank(priority: u8) -> u8 {
    if priority == 0 { 10 } else { priority }
}

fn load(root: &Path, id: &str) -> Result<Todo, TaskError> {
    let path = path_for(root, id)?;
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            return Err(TaskError::NoSuchTask(id.to_string()));
        }
        Err(source) => {
            return Err(TaskError::Io {
                doing: "read",
                id: id.to_string(),
                source,
            });
        }
    };
    ical::parse_todo(&text).map_err(|source| TaskError::Malformed {
        id: id.to_string(),
        source,
    })
}

/// Save a task, atomically: written beside the target and renamed over it, so
/// a crash leaves the old file rather than half of the new one.
fn save(root: &Path, todo: &Todo, stamp: &str) -> Result<(), TaskError> {
    let path = path_for(root, &todo.uid)?;
    let temp = path.with_extension("ics.saving");
    let text = ical::write_todo(todo, stamp);
    let io_err = |doing: &'static str| {
        let id = todo.uid.clone();
        move |source: io::Error| TaskError::Io { doing, id, source }
    };
    std::fs::write(&temp, text).map_err(io_err("save"))?;
    std::fs::rename(&temp, &path).map_err(io_err("save"))?;
    Ok(())
}

/// What a new or edited task may set.
#[derive(Debug, Clone, Default)]
pub struct Fields {
    pub summary: Option<String>,
    /// `Some(None)` clears the due date; `None` leaves it alone.
    pub due: Option<Option<String>>,
    pub priority: Option<u8>,
    pub done: Option<bool>,
}

pub fn add(root: &Path, fields: Fields, uid: String, stamp: &str) -> Result<Task, TaskError> {
    let summary = fields.summary.unwrap_or_default();
    check_summary(&summary)?;
    let due = match fields.due.flatten() {
        Some(text) => Some(parse_due(&text)?),
        None => None,
    };
    if let Some(p) = fields.priority
        && p > 9
    {
        return Err(TaskError::BadPriority(p));
    }

    let todo = Todo {
        uid,
        summary,
        done: fields.done.unwrap_or(false),
        priority: fields.priority.unwrap_or(0),
        due,
        created: Some(stamp.to_string()),
        completed_at: None,
        extra: Vec::new(),
        calendar_extra: Vec::new(),
    };
    save(root, &todo, stamp)?;
    Ok(Task::from_todo(&todo))
}

/// Change a task, keeping everything that was not mentioned — including the
/// properties this app does not model, which is what `ical`'s `extra` is for.
pub fn edit(root: &Path, id: &str, fields: Fields, stamp: &str) -> Result<Task, TaskError> {
    let mut todo = load(root, id)?;
    if let Some(summary) = fields.summary {
        check_summary(&summary)?;
        todo.summary = summary;
    }
    if let Some(due) = fields.due {
        todo.due = match due {
            Some(text) => Some(parse_due(&text)?),
            None => None,
        };
    }
    if let Some(priority) = fields.priority {
        if priority > 9 {
            return Err(TaskError::BadPriority(priority));
        }
        todo.priority = priority;
    }
    if let Some(done) = fields.done {
        set_done(&mut todo, done, stamp);
    }
    save(root, &todo, stamp)?;
    Ok(Task::from_todo(&todo))
}

pub fn complete(root: &Path, id: &str, done: bool, stamp: &str) -> Result<Task, TaskError> {
    let mut todo = load(root, id)?;
    set_done(&mut todo, done, stamp);
    save(root, &todo, stamp)?;
    Ok(Task::from_todo(&todo))
}

fn set_done(todo: &mut Todo, done: bool, stamp: &str) {
    todo.done = done;
    // Cleared when reopening: a `COMPLETED` time on an open task is a
    // contradiction other clients read in different ways.
    todo.completed_at = if done { Some(stamp.to_string()) } else { None };
}

pub fn delete(root: &Path, id: &str) -> Result<(), TaskError> {
    let path = path_for(root, id)?;
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Err(TaskError::NoSuchTask(id.to_string())),
        Err(source) => Err(TaskError::Io {
            doing: "delete",
            id: id.to_string(),
            source,
        }),
    }
}

fn check_summary(summary: &str) -> Result<(), TaskError> {
    if summary.trim().is_empty() {
        return Err(TaskError::EmptySummary);
    }
    if summary.chars().count() > MAX_SUMMARY {
        return Err(TaskError::SummaryTooLong);
    }
    Ok(())
}

/// `2026-01-10` or `2026-01-10T15:00` from a form, to iCalendar's own
/// spelling.
///
/// Validated by shape and by range, not by a date library: this app does no
/// date arithmetic, and pulling in a calendar crate to reject the 30th of
/// February would be the only thing it was used for. February 30th is
/// refused; a leap year is not checked, so 2026-02-29 is accepted and is
/// written as-is for a CalDAV client to resolve. That is a real limit and it
/// is better stated than hidden.
fn parse_due(text: &str) -> Result<String, TaskError> {
    let bad = || TaskError::BadDue(text.to_string());
    let (date, time) = match text.split_once('T') {
        Some((d, t)) => (d, Some(t)),
        None => (text, None),
    };
    let parts: Vec<&str> = date.split('-').collect();
    if parts.len() != 3 {
        return Err(bad());
    }
    let year: u32 = parts[0].parse().map_err(|_| bad())?;
    let month: u32 = parts[1].parse().map_err(|_| bad())?;
    let day: u32 = parts[2].parse().map_err(|_| bad())?;
    if parts[0].len() != 4 || parts[1].len() != 2 || parts[2].len() != 2 {
        return Err(bad());
    }
    if !(1..=12).contains(&month) || day == 0 {
        return Err(bad());
    }
    let longest = match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        _ => 29,
    };
    if day > longest || !(1970..=9999).contains(&year) {
        return Err(bad());
    }

    let Some(time) = time else {
        return Ok(format!("{year:04}{month:02}{day:02}"));
    };

    let clock: Vec<&str> = time.trim_end_matches('Z').split(':').collect();
    if clock.len() < 2 {
        return Err(bad());
    }
    let hour: u32 = clock[0].parse().map_err(|_| bad())?;
    let minute: u32 = clock[1].parse().map_err(|_| bad())?;
    let second: u32 = match clock.get(2) {
        Some(s) => s.parse().map_err(|_| bad())?,
        None => 0,
    };
    if hour > 23 || minute > 59 || second > 59 {
        return Err(bad());
    }
    // A floating local time: no `Z` and no `TZID`. The design says times
    // carry their zone and the scheduler expands them; until that service
    // exists, writing a floating time is honest — it means "this wall clock
    // time, wherever you are" — and a false `Z` would shift every due date by
    // the operator's offset.
    Ok(format!(
        "{year:04}{month:02}{day:02}T{hour:02}{minute:02}{second:02}"
    ))
}

/// A `UID` for a new task: random, and formatted as a URN so it is unambiguous
/// to every CalDAV client.
pub fn new_uid() -> String {
    use rand::TryRng as _;
    let mut bytes = [0u8; 16];
    // The system generator, as the core's own `os_random` uses: a syscall,
    // not `/dev/urandom`, which matters here because a confined app is not
    // granted that device — Landlock cannot take a directory-only right on a
    // single file, and the core reports it as unexpressible on every launch.
    //
    // A failure is survivable: a UID only has to be unique, so the fallback
    // is the clock rather than a panic in a command the operator is waiting
    // on.
    if rand::rngs::SysRng.try_fill_bytes(&mut bytes).is_err() {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        bytes.copy_from_slice(&now.to_le_bytes());
    }
    // Version 4, variant 1, so it is a well-formed UUID rather than something
    // that merely looks like one.
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const STAMP: &str = "20260110T120000Z";

    fn rig() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let root = root(dir.path()).unwrap();
        (dir, root)
    }

    fn fields(summary: &str) -> Fields {
        Fields {
            summary: Some(summary.to_string()),
            ..Fields::default()
        }
    }

    #[test]
    fn a_task_round_trips_through_a_file() {
        let (_d, root) = rig();
        let made = add(&root, fields("Pay the invoice"), new_uid(), STAMP).unwrap();
        let (tasks, broken) = list(&root).unwrap();
        assert!(broken.is_empty());
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].summary, "Pay the invoice");
        assert_eq!(tasks[0].id, made.id);

        // And it is a real calendar file, not a private format.
        let text = std::fs::read_to_string(root.join(format!("{}.ics", made.id))).unwrap();
        assert!(text.starts_with("BEGIN:VCALENDAR\r\n"));
        assert!(text.contains("BEGIN:VTODO"));
        assert!(text.contains(&format!("UID:{}", made.id)));
    }

    #[test]
    fn a_uid_is_a_well_formed_uuid_v4() {
        let uid = new_uid();
        assert_eq!(uid.len(), 36, "{uid}");
        let parts: Vec<&str> = uid.split('-').collect();
        assert_eq!(
            parts.iter().map(|p| p.len()).collect::<Vec<_>>(),
            vec![8, 4, 4, 4, 12]
        );
        assert!(parts[2].starts_with('4'), "version nibble: {uid}");
        assert!(
            matches!(parts[3].as_bytes()[0], b'8' | b'9' | b'a' | b'b'),
            "variant nibble: {uid}"
        );
        assert_ne!(new_uid(), new_uid());
    }

    #[test]
    fn an_empty_or_enormous_summary_is_refused() {
        let (_d, root) = rig();
        assert!(matches!(
            add(&root, fields("   "), new_uid(), STAMP),
            Err(TaskError::EmptySummary)
        ));
        let huge = "x".repeat(MAX_SUMMARY + 1);
        assert!(matches!(
            add(&root, fields(&huge), new_uid(), STAMP),
            Err(TaskError::SummaryTooLong)
        ));
    }

    #[test]
    fn completing_and_reopening_keeps_the_file_consistent() {
        let (_d, root) = rig();
        let made = add(&root, fields("Do it"), new_uid(), STAMP).unwrap();

        let done = complete(&root, &made.id, true, STAMP).unwrap();
        assert!(done.done);
        let text = std::fs::read_to_string(root.join(format!("{}.ics", made.id))).unwrap();
        assert!(text.contains("STATUS:COMPLETED"));
        assert!(text.contains("COMPLETED:"));

        let reopened = complete(&root, &made.id, false, STAMP).unwrap();
        assert!(!reopened.done);
        let text = std::fs::read_to_string(root.join(format!("{}.ics", made.id))).unwrap();
        assert!(text.contains("STATUS:NEEDS-ACTION"));
        // A completion time on an open task is a contradiction other clients
        // read in different ways.
        assert!(!text.contains("COMPLETED:"), "{text}");
        assert!(!text.contains("PERCENT-COMPLETE"), "{text}");
    }

    #[test]
    fn editing_keeps_what_it_was_not_told_about() {
        let (_d, root) = rig();
        let made = add(
            &root,
            Fields {
                summary: Some("Original".into()),
                due: Some(Some("2026-01-10".into())),
                priority: Some(3),
                done: None,
            },
            new_uid(),
            STAMP,
        )
        .unwrap();

        let edited = edit(
            &root,
            &made.id,
            Fields {
                summary: Some("Renamed".into()),
                ..Fields::default()
            },
            STAMP,
        )
        .unwrap();
        assert_eq!(edited.summary, "Renamed");
        assert_eq!(edited.due.as_deref(), Some("20260110"));
        assert_eq!(edited.priority, 3);
    }

    #[test]
    fn a_due_date_can_be_cleared_and_left_alone_distinctly() {
        let (_d, root) = rig();
        let made = add(
            &root,
            Fields {
                summary: Some("x".into()),
                due: Some(Some("2026-01-10".into())),
                ..Fields::default()
            },
            new_uid(),
            STAMP,
        )
        .unwrap();
        assert!(made.due.is_some());

        // `None` leaves it; `Some(None)` clears it. Collapsing those two is
        // how an edit silently drops a due date.
        let untouched = edit(&root, &made.id, Fields::default(), STAMP).unwrap();
        assert_eq!(untouched.due.as_deref(), Some("20260110"));

        let cleared = edit(
            &root,
            &made.id,
            Fields {
                due: Some(None),
                ..Fields::default()
            },
            STAMP,
        )
        .unwrap();
        assert!(cleared.due.is_none());
    }

    #[test]
    fn dates_and_times_are_accepted_in_the_forms_a_form_sends() {
        assert_eq!(parse_due("2026-01-10").unwrap(), "20260110");
        assert_eq!(parse_due("2026-01-10T15:00").unwrap(), "20260110T150000");
        assert_eq!(parse_due("2026-01-10T15:04:05").unwrap(), "20260110T150405");
        assert_eq!(parse_due("2026-12-31").unwrap(), "20261231");
    }

    #[test]
    fn a_date_that_is_not_one_is_refused() {
        for bad in [
            "",
            "tomorrow",
            "2026-13-01",
            "2026-00-10",
            "2026-02-30",
            "2026-04-31",
            "2026-01-32",
            "2026-01-00",
            "26-01-10",
            "2026-1-10",
            "2026-01-10T24:00",
            "2026-01-10T12:60",
            "2026-01-10T12",
            "1969-01-01",
        ] {
            assert!(parse_due(bad).is_err(), "{bad:?} was accepted");
        }
    }

    #[test]
    fn an_id_cannot_be_used_to_leave_the_folder() {
        let (_d, root) = rig();
        for bad in ["../escape", "a/b", "..", ".hidden", "", "x\0y"] {
            assert!(path_for(&root, bad).is_err(), "{bad:?} was accepted");
        }
    }

    #[test]
    fn a_priority_outside_the_scale_is_refused() {
        let (_d, root) = rig();
        assert!(matches!(
            add(
                &root,
                Fields {
                    summary: Some("x".into()),
                    priority: Some(10),
                    ..Fields::default()
                },
                new_uid(),
                STAMP
            ),
            Err(TaskError::BadPriority(10))
        ));
    }

    #[test]
    fn the_order_is_open_first_then_due_then_priority() {
        let (_d, root) = rig();
        let make = |summary: &str, due: Option<&str>, priority: u8, done: bool| {
            add(
                &root,
                Fields {
                    summary: Some(summary.to_string()),
                    due: Some(due.map(str::to_string)),
                    priority: Some(priority),
                    done: Some(done),
                },
                new_uid(),
                STAMP,
            )
            .unwrap()
        };

        make("done early", Some("2026-01-01"), 1, true);
        make("no date low", None, 9, false);
        make("later", Some("2026-02-01"), 5, false);
        make("soonest", Some("2026-01-05"), 5, false);
        make("no date high", None, 1, false);

        let (tasks, _) = list(&root).unwrap();
        let order: Vec<&str> = tasks.iter().map(|t| t.summary.as_str()).collect();
        assert_eq!(
            order,
            vec![
                "soonest",
                "later",
                "no date high",
                "no date low",
                "done early"
            ],
            "got {order:?}"
        );
    }

    #[test]
    fn an_unset_priority_sorts_after_the_lowest_one() {
        // 0 means unset, and 1 is the *highest*, so a naive numeric sort puts
        // every unprioritized task at the top.
        assert!(rank(0) > rank(9));
        assert!(rank(1) < rank(2));
    }

    #[test]
    fn a_broken_file_is_reported_and_the_rest_still_load() {
        let (_d, root) = rig();
        add(&root, fields("Fine"), new_uid(), STAMP).unwrap();
        std::fs::write(root.join("junk.ics"), "this is not a calendar").unwrap();
        // An event is not a task and is not broken either: a Calendar app may
        // share this folder.
        std::fs::write(
            root.join("event.ics"),
            "BEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\nUID:e\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n",
        )
        .unwrap();

        let (tasks, broken) = list(&root).unwrap();
        assert_eq!(tasks.len(), 1, "got {tasks:?}");
        assert_eq!(broken, vec!["junk.ics".to_string()]);
    }

    #[test]
    fn deleting_removes_the_file_and_says_so_when_it_was_not_there() {
        let (_d, root) = rig();
        let made = add(&root, fields("Doomed"), new_uid(), STAMP).unwrap();
        delete(&root, &made.id).unwrap();
        assert!(list(&root).unwrap().0.is_empty());
        assert!(matches!(
            delete(&root, &made.id),
            Err(TaskError::NoSuchTask(_))
        ));
    }

    #[test]
    fn saving_leaves_no_partial_file_behind() {
        let (_d, root) = rig();
        let made = add(&root, fields("One"), new_uid(), STAMP).unwrap();
        edit(
            &root,
            &made.id,
            Fields {
                summary: Some("Two".into()),
                ..Fields::default()
            },
            STAMP,
        )
        .unwrap();
        let strays: Vec<_> = std::fs::read_dir(&root)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.contains("saving"))
            .collect();
        assert!(strays.is_empty(), "left {strays:?}");
    }

    #[test]
    fn a_missing_tasks_folder_lists_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let (tasks, broken) = list(&dir.path().join("Tasks")).unwrap();
        assert!(tasks.is_empty() && broken.is_empty());
    }
}
