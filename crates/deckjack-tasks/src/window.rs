//! The Tasks window, described with the UI protocol.
//!
//! This used to be `ui/index.html`, 235 lines of HTML and JavaScript that
//! called the bus from an iframe. It is now a model and a view: the shell
//! draws the widgets, and every button is the same bus command the palette
//! and an agent call (`tasks.add`, `tasks.complete`, `tasks.delete`), sent as
//! this app's window.
//!
//! The model re-reads the folder whenever one of this app's write commands
//! succeeds, whoever called it, which is what the page did after each of its
//! own calls — and now also after the palette's and an agent's — and
//! whenever a browser shows the window again (`opened`), which is when the
//! page re-read it too: a reload, or another device opening it, picks up
//! files that changed on disk (CalDAV sync, an edit by hand).

use deckjack_app::ui::{
    Action, Button, Checkbox, Choice, Date, Empty, Event, EventKind, Gap, Heading, Input, List,
    Node, Program, Row, Select, Stack, Text, TextField, Tone,
};
use serde_json::Value;

use crate::tasks::Task;

/// One Tasks window.
pub struct Tasks {
    tasks: Vec<Task>,
    unreadable: Vec<String>,
    /// Why the folder could not be read, when it could not.
    failed: Option<String>,
    show_done: bool,
}

pub enum Msg {
    ShowDone(bool),
    /// One of this app's write commands ran, or the window was shown again:
    /// read the folder again.
    Reload,
}

/// The commands whose success changes what the window shows.
const WRITES: &[&str] = &["tasks.add", "tasks.complete", "tasks.edit", "tasks.delete"];

impl Tasks {
    /// A new window, with the folder as it is now.
    pub fn open() -> Tasks {
        let mut window = Tasks {
            tasks: Vec::new(),
            unreadable: Vec::new(),
            failed: None,
            show_done: false,
        };
        window.reload();
        window
    }

    fn reload(&mut self) {
        match crate::root().and_then(|root| crate::tasks::list(&root).map_err(|e| e.to_string())) {
            Ok((tasks, unreadable)) => {
                self.tasks = tasks;
                self.unreadable = unreadable;
                self.failed = None;
            }
            Err(e) => self.failed = Some(e),
        }
    }

    /// The model over a list already read, for the tests.
    #[cfg(test)]
    fn with(tasks: Vec<Task>) -> Tasks {
        Tasks {
            tasks,
            unreadable: Vec::new(),
            failed: None,
            show_done: false,
        }
    }

    /// `tasks.add` with whatever the add row says at the moment it fires.
    /// The add row clears once the task is added, in this window only and
    /// only where it still says what was sent, as the page's form did after
    /// its own submit.
    fn add_action() -> Action {
        Action::command("tasks.add")
            .field("summary", "summary")
            .field("due", "due")
            .field("priority", "priority")
            .require("summary")
            .clear("summary")
            .clear("due")
            .clear("priority")
    }

    fn add_row(&self) -> Node {
        Node::new(
            "add",
            Row {
                gap: Gap::Medium,
                padded: true,
                divider: true,
                ..Row::default()
            },
        )
        .with([
            Node::new(
                "summary",
                TextField {
                    name: "summary".into(),
                    placeholder: "What needs doing?".into(),
                    label: "New task".into(),
                    submit: Some(Tasks::add_action()),
                    grow: true,
                    ..TextField::default()
                },
            ),
            Node::new(
                "due",
                TextField {
                    name: "due".into(),
                    label: "Due date".into(),
                    input: Input::Date,
                    // Enter here adds, as it did in the page's form.
                    submit: Some(Tasks::add_action()),
                    ..TextField::default()
                },
            ),
            Node::new(
                "priority",
                Select {
                    name: "priority".into(),
                    label: "Priority".into(),
                    options: [(0, "—"), (1, "High"), (5, "Medium"), (9, "Low")]
                        .into_iter()
                        .map(|(value, label)| Choice {
                            value: value.into(),
                            label: label.into(),
                        })
                        .collect(),
                    value: 0.into(),
                    revision: 0,
                },
            ),
            Node::new(
                "add",
                Button {
                    label: "Add".into(),
                    action: Some(Tasks::add_action()),
                    ..Button::default()
                },
            ),
        ])
    }

    fn row(task: &Task) -> Node {
        let mut cells = vec![
            Node::new(
                "done",
                Checkbox {
                    label: "Done".into(),
                    show_label: false,
                    checked: task.done,
                    action: Some(
                        Action::command("tasks.complete")
                            .param("id", task.id.clone())
                            .value_as("done"),
                    ),
                    ..Checkbox::default()
                },
            ),
            Node::new(
                "what",
                Text {
                    text: task.summary.clone(),
                    tone: if task.done { Tone::Muted } else { Tone::Normal },
                    strike: task.done,
                    grow: true,
                    ..Text::default()
                },
            ),
        ];
        if task.priority > 0 {
            let marks = match task.priority {
                1..=3 => 3,
                4..=6 => 2,
                _ => 1,
            };
            cells.push(Node::new(
                "pri",
                Text {
                    text: "!".repeat(marks),
                    tone: if task.priority <= 3 {
                        Tone::Warning
                    } else {
                        Tone::Muted
                    },
                    small: true,
                    ..Text::default()
                },
            ));
        }
        if let Some(value) = task.due.as_deref().and_then(due_value) {
            cells.push(Node::new(
                "due",
                Date {
                    value,
                    // A finished task is not late, whatever its date says.
                    flag_past: !task.done,
                },
            ));
        }
        cells.push(Node::new(
            "drop",
            Button {
                label: "×".into(),
                // No confirmation here: `tasks.delete` is declared
                // destructive, so the shell asks, in the bus's own words.
                action: Some(Action::command("tasks.delete").param("id", task.id.clone())),
                quiet: true,
                title: Some("Delete".into()),
                ..Button::default()
            },
        ));
        Node::new(
            task.id.clone(),
            Row {
                gap: Gap::Medium,
                padded: true,
                divider: true,
                ..Row::default()
            },
        )
        .with(cells)
    }

    fn status(&self) -> Node {
        let (text, tone) = match &self.failed {
            Some(e) => (format!("Tasks/ could not be read: {e}"), Tone::Danger),
            None => {
                let open = self.tasks.iter().filter(|t| !t.done).count();
                let mut note = format!("{open} open, {} done", self.tasks.len() - open);
                if !self.unreadable.is_empty() {
                    let n = self.unreadable.len();
                    note.push_str(&format!(
                        " · {n} file{} in Tasks/ could not be read: {}",
                        if n == 1 { "" } else { "s" },
                        self.unreadable.join(", ")
                    ));
                }
                (note, Tone::Muted)
            }
        };
        Node::new(
            "status",
            Row {
                padded: true,
                ..Row::default()
            },
        )
        .with([Node::new(
            "note",
            Text {
                text,
                tone,
                small: true,
                ..Text::default()
            },
        )])
    }
}

/// `20260110` or `20260110T150000[Z]`, as the file wrote it, to the
/// `YYYY-MM-DD[THH:MM]` the shell formats. Read as the viewer's local time,
/// which is what the page did with the same value.
fn due_value(ical: &str) -> Option<String> {
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    let date = ical.get(0..8).filter(|d| digits(d))?;
    let day = format!("{}-{}-{}", &date[0..4], &date[4..6], &date[6..8]);
    if ical.len() == 8 {
        return Some(day);
    }
    let time = ical
        .get(8..9)
        .filter(|t| *t == "T")
        .and_then(|_| ical.get(9..13))
        .filter(|t| digits(t))?;
    Some(format!("{day}T{}:{}", &time[0..2], &time[2..4]))
}

impl Program for Tasks {
    type Msg = Msg;

    fn view(&self) -> Node {
        let visible: Vec<&Task> = self
            .tasks
            .iter()
            .filter(|t| self.show_done || !t.done)
            .collect();
        let body = if let (Some(e), true) = (&self.failed, self.tasks.is_empty()) {
            // Not "Nothing to do.": an unreadable folder is not an empty one,
            // and the page put the error in place of the list too.
            Node::new(
                "empty",
                Empty {
                    title: "Tasks/ could not be read.".into(),
                    detail: Some(e.clone()),
                },
            )
        } else if visible.is_empty() {
            Node::new(
                "empty",
                Empty {
                    title: "Nothing to do.".into(),
                    detail: None,
                },
            )
        } else {
            Node::new(
                "tasks",
                List {
                    label: "Tasks".into(),
                    ..List::default()
                },
            )
            .with(visible.into_iter().map(Tasks::row))
        };
        Node::new(
            "root",
            Stack {
                gap: Gap::None,
                grow: true,
                ..Stack::default()
            },
        )
        .with([
            Node::new(
                "bar",
                Row {
                    bar: true,
                    ..Row::default()
                },
            )
            .with([
                Node::new(
                    "title",
                    Heading {
                        text: "Tasks".into(),
                    },
                ),
                Node::new(
                    "show_done",
                    Checkbox {
                        label: "Show done".into(),
                        name: Some("show_done".into()),
                        checked: self.show_done,
                        ..Checkbox::default()
                    },
                ),
            ]),
            self.add_row(),
            body,
            self.status(),
        ])
    }

    fn update(&mut self, msg: Msg) {
        match msg {
            Msg::ShowDone(on) => self.show_done = on,
            Msg::Reload => self.reload(),
        }
    }

    fn event(&self, event: &Event) -> Option<Msg> {
        match (event.kind, event.key()) {
            (EventKind::Changed, "show_done") => Some(Msg::ShowDone(event.value.as_bool()?)),
            (EventKind::Opened, _) => Some(Msg::Reload),
            _ => None,
        }
    }

    fn command_ran(&self, command: &str, _params: &Value) -> Option<Msg> {
        WRITES.contains(&command).then_some(Msg::Reload)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use deckjack_app::ui::{Widget, diff};

    fn task(id: &str, summary: &str, done: bool, priority: u8, due: Option<&str>) -> Task {
        Task {
            id: id.into(),
            summary: summary.into(),
            done,
            priority,
            due: due.map(str::to_string),
        }
    }

    fn texts(node: &Node, out: &mut Vec<String>) {
        if let Widget::Text(t) = &node.widget {
            out.push(t.text.clone());
        }
        for c in &node.children {
            texts(c, out);
        }
    }

    fn rows(view: &Node) -> Vec<String> {
        view.find(&["tasks".to_string()])
            .map(|list| list.children.iter().map(|r| r.key.clone()).collect())
            .unwrap_or_default()
    }

    #[test]
    fn done_tasks_are_hidden_until_show_done_is_ticked() {
        let mut w = Tasks::with(vec![
            task("a", "Open one", false, 0, None),
            task("b", "Done one", true, 0, None),
        ]);
        assert_eq!(rows(&w.view()), ["a"]);
        let msg = w
            .event(&Event {
                path: vec!["bar".into(), "show_done".into()],
                kind: EventKind::Changed,
                value: true.into(),
                seq: None,
            })
            .expect("the box is the app's to handle");
        w.update(msg);
        assert_eq!(rows(&w.view()), ["a", "b"]);
    }

    #[test]
    fn nothing_visible_shows_the_empty_state_and_the_counts() {
        let w = Tasks::with(vec![task("b", "Done one", true, 0, None)]);
        let view = w.view();
        assert!(view.find(&["empty".to_string()]).is_some());
        assert!(view.find(&["tasks".to_string()]).is_none());
        let mut all = Vec::new();
        texts(&view, &mut all);
        assert!(all.contains(&"0 open, 1 done".to_string()), "{all:?}");
    }

    #[test]
    fn a_row_carries_the_commands_the_page_called() {
        let w = Tasks::with(vec![task("t1", "Water plants", false, 2, Some("20260110"))]);
        let view = w.view();
        let row = view.find(&["tasks".to_string(), "t1".to_string()]).unwrap();
        let Widget::Checkbox(tick) = &row.children[0].widget else {
            panic!("{row:?}")
        };
        let action = tick.action.as_ref().unwrap();
        assert_eq!(action.command, "tasks.complete");
        assert_eq!(action.params["id"], "t1");
        assert_eq!(action.value.as_deref(), Some("done"));
        let Widget::Button(drop) = &row.children.last().unwrap().widget else {
            panic!()
        };
        assert_eq!(drop.action.as_ref().unwrap().command, "tasks.delete");
        // High priority is three marks in the warning tone.
        let pri = row.find(&["pri".to_string()]).unwrap();
        let Widget::Text(pri) = &pri.widget else {
            panic!()
        };
        assert_eq!((pri.text.as_str(), pri.tone), ("!!!", Tone::Warning));
        let due = row.find(&["due".to_string()]).unwrap();
        assert_eq!(
            due.widget,
            Widget::Date(Date {
                value: "2026-01-10".into(),
                flag_past: true
            })
        );
    }

    #[test]
    fn the_add_button_reads_the_fields_and_needs_a_summary() {
        let w = Tasks::with(vec![]);
        let view = w.view();
        let add = view.find(&["add".to_string(), "add".to_string()]).unwrap();
        let Widget::Button(b) = &add.widget else {
            panic!()
        };
        let action = b.action.as_ref().unwrap();
        assert_eq!(action.command, "tasks.add");
        assert_eq!(action.fields["summary"], "summary");
        assert_eq!(action.fields["priority"], "priority");
        assert_eq!(action.required, ["summary"]);
    }

    #[test]
    fn no_write_from_anywhere_touches_the_add_row() {
        // The add row is the operator's: a task added by an agent, the
        // palette or another window reloads the list and leaves a half-typed
        // summary alone. Clearing after this window's own add is the
        // action's `clear`, done by the shell in that window only.
        let w = Tasks::with(vec![]);
        let before = w.view();
        for command in WRITES {
            let msg = w
                .command_ran(command, &Value::Null)
                .expect("a write reloads");
            assert!(matches!(msg, Msg::Reload));
        }
        assert!(w.command_ran("tasks.list", &Value::Null).is_none());
        let after = Tasks::with(vec![task("t1", "Added elsewhere", false, 0, None)]).view();
        let patches = diff(&before, &after);
        let touched_add_row = patches.iter().any(|p| match p {
            deckjack_app::ui::Patch::Props { path, .. }
            | deckjack_app::ui::Patch::Replace { path, .. }
            | deckjack_app::ui::Patch::Insert { path, .. }
            | deckjack_app::ui::Patch::Remove { path, .. }
            | deckjack_app::ui::Patch::Move { path, .. } => {
                path.first().is_some_and(|k| k == "add")
            }
        });
        assert!(!touched_add_row, "{patches:#?}");
        let add = before
            .find(&["add".to_string(), "add".to_string()])
            .unwrap();
        let Widget::Button(b) = &add.widget else {
            panic!()
        };
        assert_eq!(
            b.action.as_ref().unwrap().clear,
            ["summary", "due", "priority"]
        );
    }

    #[test]
    fn enter_in_the_summary_or_the_due_date_adds() {
        let view = Tasks::with(vec![]).view();
        for field in ["summary", "due"] {
            let node = view.find(&["add".to_string(), field.to_string()]).unwrap();
            let Widget::TextField(f) = &node.widget else {
                panic!("{node:?}")
            };
            assert_eq!(
                f.submit.as_ref().map(|a| a.command.as_str()),
                Some("tasks.add"),
                "{field}"
            );
        }
    }

    #[test]
    fn showing_the_window_again_reads_the_folder_again() {
        let w = Tasks::with(vec![]);
        let msg = w.event(&Event {
            path: vec![],
            kind: EventKind::Opened,
            value: Value::Null,
            seq: None,
        });
        assert!(matches!(msg, Some(Msg::Reload)));
    }

    #[test]
    fn an_unreadable_folder_says_so_in_place_of_the_list() {
        let mut w = Tasks::with(vec![]);
        w.failed = Some("permission denied".into());
        let view = w.view();
        let Widget::Empty(e) = &view.find(&["empty".to_string()]).unwrap().widget else {
            panic!()
        };
        assert_eq!(e.title, "Tasks/ could not be read.");
        assert_eq!(e.detail.as_deref(), Some("permission denied"));
        // A folder that was read once and fails later keeps the list it had,
        // with the error on the status line.
        let mut w = Tasks::with(vec![task("a", "Kept", false, 0, None)]);
        w.failed = Some("gone".into());
        assert_eq!(rows(&w.view()), ["a"]);
    }

    #[test]
    fn completing_one_task_is_a_small_patch() {
        let mut list: Vec<Task> = (0..200)
            .map(|i| task(&format!("t{i}"), "Something", false, 0, None))
            .collect();
        let mut before = Tasks::with(list.clone());
        before.show_done = true;
        list[50].done = true;
        let mut after = Tasks::with(list);
        after.show_done = true;
        let patches = diff(&before.view(), &after.view());
        // The tick, the summary's strike and the status line.
        assert_eq!(patches.len(), 3, "{patches:#?}");
    }

    #[test]
    fn priority_marks_and_lateness_follow_the_pages_rules() {
        // The page: 1-3 is three marks in the warning tone, 4-6 two, 7-9
        // one, both muted; a finished task is never shown as late.
        let w = Tasks::with(vec![
            task("p3", "a", false, 3, None),
            task("p4", "b", false, 4, None),
            task("p9", "c", false, 9, None),
            task("done", "d", true, 1, Some("20200101")),
        ]);
        let mut w = w;
        w.show_done = true;
        let view = w.view();
        let cell = |id: &str, key: &str| {
            view.find(&["tasks".to_string(), id.to_string(), key.to_string()])
                .unwrap()
                .widget
                .clone()
        };
        let marks = |id: &str| match cell(id, "pri") {
            Widget::Text(t) => (t.text, t.tone),
            other => panic!("{other:?}"),
        };
        assert_eq!(marks("p3"), ("!!!".to_string(), Tone::Warning));
        assert_eq!(marks("p4"), ("!!".to_string(), Tone::Muted));
        assert_eq!(marks("p9"), ("!".to_string(), Tone::Muted));
        assert_eq!(
            cell("done", "due"),
            Widget::Date(Date {
                value: "2020-01-01".into(),
                flag_past: false
            })
        );
    }

    #[test]
    fn due_dates_convert_as_the_page_read_them() {
        assert_eq!(due_value("20260110").as_deref(), Some("2026-01-10"));
        assert_eq!(
            due_value("20260110T150000").as_deref(),
            Some("2026-01-10T15:00")
        );
        assert_eq!(
            due_value("20260110T150000Z").as_deref(),
            Some("2026-01-10T15:00")
        );
        assert_eq!(due_value("2026-01-10"), None);
        assert_eq!(due_value("20260110X1500"), None);
        assert_eq!(due_value(""), None);
    }

    #[test]
    fn ten_thousand_tasks_fit_in_one_frame() {
        // A window's first frame is its whole tree, and a frame over the
        // protocol's limit is refused by the core, which would leave the
        // window blank. Ten thousand open tasks is the size the phase-0 list
        // test scrolls.
        let list: Vec<Task> = (0..10_000)
            .map(|i| {
                task(
                    &format!("{i:08x}-0000-4000-8000-000000000000"),
                    &format!("Task number {i}"),
                    false,
                    if i % 3 == 0 { 5 } else { 0 },
                    (i % 2 == 0).then_some("20260110"),
                )
            })
            .collect();
        let view = Tasks::with(list).view();
        let frame = deckjack_protocol::encode_frame(&deckjack_protocol::AppFrame::UiPatch {
            window: "w1".into(),
            rev: 1,
            ack: None,
            patches: vec![deckjack_app::ui::Patch::Replace {
                path: vec![],
                node: view,
            }],
        })
        .unwrap();
        eprintln!("10,000-task first frame: {} bytes", frame.len());
        assert!(
            frame.len() < deckjack_protocol::MAX_FRAME_BYTES,
            "{} bytes",
            frame.len()
        );
    }
}
