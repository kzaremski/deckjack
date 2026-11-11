//! `deckjack-tasks`: the Tasks app.
//!
//! A task is a `VTODO` in its own `.ics` file under `Tasks/`. Every rule about
//! the format is in [`ical`] and every rule about the folder is in [`tasks`];
//! this file is the seam between those and the bus.

mod ical;
mod tasks;

use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use deckjack_app::{App, Ctx};
use serde::Deserialize;
use serde_json::{Value, json};
use tasks::Fields;

#[derive(rust_embed::RustEmbed)]
#[folder = "ui"]
struct Ui;

fn root() -> Result<PathBuf, String> {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    tasks::root(&cwd).map_err(|e| e.to_string())
}

/// `DTSTAMP` and `LAST-MODIFIED`: now, in iCalendar's UTC form.
///
/// Computed from the unix clock rather than with a date library, which this
/// app otherwise does not need. The conversion is the civil-from-days
/// algorithm, which is exact for every date in range — not an approximation
/// with 365.25 in it.
fn stamp() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let (days, rem) = ((secs / 86_400) as i64, secs % 86_400);
    let (y, m, d) = civil_from_days(days);
    let (h, min, s) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    format!("{y:04}{m:02}{d:02}T{h:02}{min:02}{s:02}Z")
}

/// Days since 1970-01-01 to a calendar date. Howard Hinnant's algorithm,
/// which is exact and has no leap-year special cases to get wrong.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[derive(Deserialize, schemars::JsonSchema)]
struct NoParams {}

#[derive(Deserialize, schemars::JsonSchema)]
struct Add {
    /// What the task says. One line.
    summary: String,
    /// `YYYY-MM-DD`, or `YYYY-MM-DDTHH:MM` for a time of day.
    #[serde(default)]
    due: Option<String>,
    /// 1 highest, 9 lowest, 0 or absent for unset.
    #[serde(default)]
    priority: Option<u8>,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct Id {
    /// The task's id, as `tasks.list` reports it.
    id: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct Complete {
    id: String,
    /// False reopens it.
    #[serde(default = "yes")]
    done: bool,
}

fn yes() -> bool {
    true
}

#[derive(Deserialize, schemars::JsonSchema)]
struct Edit {
    id: String,
    #[serde(default)]
    summary: Option<String>,
    /// Absent leaves the due date alone; `null` clears it.
    #[serde(default, deserialize_with = "double_option")]
    due: Option<Option<String>>,
    #[serde(default)]
    priority: Option<u8>,
    #[serde(default)]
    done: Option<bool>,
}

/// Tell "absent" apart from "null".
///
/// Without this an edit cannot clear a due date: serde collapses both to
/// `None`, so "leave it" and "remove it" would be the same request.
fn double_option<'de, D>(deserializer: D) -> Result<Option<Option<String>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Deserialize::deserialize(deserializer).map(Some)
}

fn main() -> anyhow::Result<()> {
    App::new(include_str!("../app.toml"))?
        .ui::<Ui>()
        .command("tasks.list", |_p: NoParams, _cx: Ctx| async move {
            let (tasks, broken) = tasks::list(&root()?).map_err(|e| e.to_string())?;
            Ok(json!({ "tasks": tasks, "unreadable": broken }))
        })
        .command("tasks.add", |p: Add, _cx: Ctx| async move {
            let task = tasks::add(
                &root()?,
                Fields {
                    summary: Some(p.summary),
                    due: Some(p.due),
                    priority: p.priority,
                    done: None,
                },
                tasks::new_uid(),
                &stamp(),
            )
            .map_err(|e| e.to_string())?;
            Ok(serde_json::to_value(task).unwrap_or(Value::Null))
        })
        .command("tasks.complete", |p: Complete, _cx: Ctx| async move {
            let task =
                tasks::complete(&root()?, &p.id, p.done, &stamp()).map_err(|e| e.to_string())?;
            Ok(serde_json::to_value(task).unwrap_or(Value::Null))
        })
        .command("tasks.edit", |p: Edit, _cx: Ctx| async move {
            let task = tasks::edit(
                &root()?,
                &p.id,
                Fields {
                    summary: p.summary,
                    due: p.due,
                    priority: p.priority,
                    done: p.done,
                },
                &stamp(),
            )
            .map_err(|e| e.to_string())?;
            Ok(serde_json::to_value(task).unwrap_or(Value::Null))
        })
        .command("tasks.delete", |p: Id, _cx: Ctx| async move {
            tasks::delete(&root()?, &p.id).map_err(|e| e.to_string())?;
            Ok(json!({ "deleted": p.id }))
        })
        .run()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_stamp_is_a_utc_icalendar_date_time() {
        let s = stamp();
        assert_eq!(s.len(), 16, "{s}");
        assert!(s.ends_with('Z'));
        assert_eq!(&s[8..9], "T");
        assert!(s[..8].chars().all(|c| c.is_ascii_digit()), "{s}");
    }

    #[test]
    fn dates_convert_exactly_at_the_edges() {
        // Known values, including the two every hand-rolled converter gets
        // wrong: a leap day and a century that is not a leap year.
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(-1), (1969, 12, 31));
        assert_eq!(civil_from_days(19_723), (2024, 1, 1));
        // A leap day, and a century leap year, which is the pair every
        // hand-rolled converter gets wrong.
        assert_eq!(civil_from_days(19_782), (2024, 2, 29));
        assert_eq!(civil_from_days(11_016), (2000, 2, 29));
        // 2026 is not a leap year, so the 28th is followed by March.
        assert_eq!(civil_from_days(20_512), (2026, 2, 28));
        assert_eq!(civil_from_days(20_513), (2026, 3, 1));
    }
}
