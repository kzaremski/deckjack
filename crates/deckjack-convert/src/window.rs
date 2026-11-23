//! The Convert window, described with the UI protocol and drawn by the shell.
//!
//! Three steps in one column, which is what works at 320 px: **Pick** files,
//! **Choose** a preset (and measure it), watch the **Queue**. Every action
//! that does something is a bus command bound to a widget (`convert.estimate`,
//! `convert.job.add`, `convert.job.cancel`, ...), so the palette and an agent
//! can do exactly what the window does. Only moving between steps, filtering
//! the file list and selecting rows are the window's own business.
//!
//! The window follows work it did not start: a job advancing, or an estimate
//! an agent asked for, re-renders it through the app's notifier
//! ([`deckjack_app::ui::Program::notified`]), with no event behind it.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};

use deckjack_app::ui::{
    Action, Button, Checkbox, Choice, Empty, Event, EventKind, Gap, Heading, Input, List, Node,
    Notifier, Program, Progress, Row, Select, Selection, Stack, Text, TextField, Tone,
};
use serde_json::{Value, json};

use crate::estimate::Estimate;
use crate::files::{self, Entry};
use crate::presets::{self, PRESETS, PresetId};
use crate::queue::{Job, JobState, Queue, human_bytes};

/// What every window shares: the queue, and the latest estimate for each
/// file and preset, which `convert.estimate` writes whoever called it.
pub struct Shared {
    pub queue: Arc<Queue>,
    pub notifier: Notifier,
    pub scratch: PathBuf,
    pub estimates: Mutex<HashMap<(String, PresetId), Measuring>>,
    /// Estimates and stills take turns ([`crate::estimate::Gate`]).
    pub gate: crate::estimate::Gate,
}

/// One estimate's state, as the window shows it.
#[derive(Debug, Clone)]
pub enum Measuring {
    Running {
        fraction: f32,
        step: String,
    },
    /// The estimate, and the options it was measured with.
    Done(Box<Estimate>, presets::Options),
    Failed(String),
}

/// What a measurement covered, for the line under it: the window cannot see
/// the range and container fields the shell holds, so it says what was
/// measured rather than let a measurement of other options pass for this one.
fn measured_on(options: &presets::Options) -> String {
    let span = match options.range {
        Some(r) => format!("{} to {}", presets::clock(r.start), presets::clock(r.end)),
        None => "the whole file".into(),
    };
    let mut with = Vec::new();
    match options.container {
        Some(presets::Container::Mp4) => with.push("as MP4"),
        Some(presets::Container::Mkv) => with.push("as MKV"),
        None => {}
    }
    if options.keep_all_audio {
        with.push("with every audio track");
    }
    if options.allow_lossy_to_lossless {
        with.push("from lossy allowed");
    }
    let mut line = format!("Measured on {span}");
    for w in with {
        line.push_str(", ");
        line.push_str(w);
    }
    line.push('.');
    line
}

impl Shared {
    pub fn set_estimate(&self, path: &str, preset: PresetId, m: Measuring) {
        self.estimates
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert((path.to_string(), preset), m);
        self.notifier.notify();
    }

    fn estimate(&self, path: &str, preset: PresetId) -> Option<Measuring> {
        self.estimates
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&(path.to_string(), preset))
            .cloned()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    Pick,
    Choose,
    Queue,
}

pub struct Convert {
    shared: Arc<Shared>,
    step: Step,
    files: Vec<Entry>,
    truncated: bool,
    filter: String,
    selected: Vec<String>,
    preset: Option<PresetId>,
    show_command: bool,
    /// The option boxes. A box with no bound command is the app's to keep:
    /// the shell shows what the tree says once the app has answered the
    /// tick, so a box the model does not hold would untick itself.
    keep_all_audio: bool,
    allow_lossy: bool,
    jobs: Vec<Job>,
}

pub enum Msg {
    Go(Step),
    Filter(String),
    Select(Vec<String>),
    /// One row's tick box.
    Pick(String, bool),
    /// Every file the filter shows.
    PickAll,
    Preset(Option<PresetId>),
    ShowCommand(bool),
    KeepAllAudio(bool),
    AllowLossy(bool),
    /// Read the folders again.
    Rescan,
    /// The queue or an estimate changed: read the queue again.
    Refresh,
    /// This window's own batch was queued: show the queue.
    Queued,
}

/// The commands whose success changes the queue.
const JOB_COMMANDS: &[&str] = &[
    "convert.job.add",
    "convert.job.cancel",
    "convert.job.retry",
    "convert.job.clear",
    "convert.settings",
];

impl Convert {
    pub fn open(shared: Arc<Shared>) -> Convert {
        let mut window = Convert {
            jobs: shared.queue.list(),
            shared,
            step: Step::Pick,
            files: Vec::new(),
            truncated: false,
            filter: String::new(),
            selected: Vec::new(),
            preset: None,
            show_command: false,
            keep_all_audio: false,
            allow_lossy: false,
        };
        window.rescan();
        // A window opened while jobs exist starts where the work is.
        if window
            .jobs
            .iter()
            .any(|j| matches!(j.state, JobState::Queued | JobState::Running))
        {
            window.step = Step::Queue;
        }
        window
    }

    fn rescan(&mut self) {
        let (files, truncated) =
            files::browse(self.shared.queue.grants(), None).unwrap_or_default();
        self.files = files;
        self.truncated = truncated;
        // A selected file that is gone is no longer selected.
        let present: std::collections::HashSet<&str> =
            self.files.iter().map(|f| f.path.as_str()).collect();
        self.selected.retain(|p| present.contains(p.as_str()));
    }

    fn visible(&self) -> Vec<&Entry> {
        let needle = self.filter.trim().to_lowercase();
        self.files
            .iter()
            .filter(|f| needle.is_empty() || f.path.to_lowercase().contains(&needle))
            .collect()
    }

    fn steps_bar(&self) -> Node {
        let tab = |key: &str, label: &str, step: Step, enabled: bool| {
            Node::new(
                key,
                Button {
                    label: label.into(),
                    tone: if self.step == step {
                        Tone::Accent
                    } else {
                        Tone::Normal
                    },
                    enabled,
                    ..Button::default()
                },
            )
        };
        let active = self
            .jobs
            .iter()
            .filter(|j| matches!(j.state, JobState::Queued | JobState::Running))
            .count();
        Node::new(
            "bar",
            Row {
                bar: true,
                gap: Gap::Small,
                ..Row::default()
            },
        )
        .with([
            Node::new(
                "title",
                Heading {
                    text: "Convert".into(),
                },
            ),
            tab("pick", "Files", Step::Pick, true),
            tab("choose", "Preset", Step::Choose, !self.selected.is_empty()),
            tab(
                "queue",
                &if active > 0 {
                    format!("Queue ({active})")
                } else {
                    "Queue".to_string()
                },
                Step::Queue,
                true,
            ),
        ])
    }

    fn pick(&self) -> Vec<Node> {
        if let Err(e) = self.shared.queue.tools() {
            return vec![Node::new(
                "empty",
                Empty {
                    title: "ffmpeg is not installed on this node.".into(),
                    detail: Some(e),
                },
            )];
        }
        let shown = self.visible();
        let folders = self.shared.queue.grants().names();
        let mut out = vec![
            Node::new(
                "tools",
                Row {
                    gap: Gap::Small,
                    padded: true,
                    divider: true,
                    ..Row::default()
                },
            )
            .with([
                Node::new(
                    "filter",
                    TextField {
                        name: "filter".into(),
                        label: "Filter files".into(),
                        placeholder: "Filter".into(),
                        input: Input::Search,
                        grow: true,
                        ..TextField::default()
                    },
                ),
                // One button, not two, so the row fits at 320 px: All while
                // anything shown is unpicked, None once everything is.
                if shown.iter().any(|f| !self.selected.contains(&f.path)) {
                    Node::new(
                        "pick_all",
                        Button {
                            label: "All".into(),
                            enabled: !shown.is_empty(),
                            ..Button::default()
                        },
                    )
                } else {
                    Node::new(
                        "pick_none",
                        Button {
                            label: "None".into(),
                            enabled: !self.selected.is_empty(),
                            ..Button::default()
                        },
                    )
                },
                Node::new(
                    "rescan",
                    Button {
                        label: "Refresh".into(),
                        ..Button::default()
                    },
                ),
            ]),
        ];
        if shown.is_empty() {
            out.push(Node::new(
                "empty",
                Empty {
                    title: if self.files.is_empty() {
                        format!("No video or audio files in {folders}")
                    } else {
                        "Nothing matches the filter.".into()
                    },
                    detail: self
                        .files
                        .is_empty()
                        .then(|| "Put files there with Files, then refresh.".to_string()),
                },
            ));
        } else {
            out.push(
                Node::new(
                    "files",
                    // Picked with a tick box per row, not the list's own
                    // selection: the list selects several rows only with
                    // Ctrl-click, which a phone does not have (a tier-1
                    // phase-1 gap, see the lane doc).
                    List {
                        label: "Files".into(),
                        ..List::default()
                    },
                )
                .with(
                    shown
                        .iter()
                        .map(|f| file_row(f, self.selected.contains(&f.path))),
                ),
            );
        }
        let mut note = format!(
            "{} selected · {} file{} in {folders}",
            self.selected.len(),
            self.files.len(),
            if self.files.len() == 1 { "" } else { "s" }
        );
        if self.truncated {
            note.push_str(&format!(" (the first {})", files::BROWSE_LIMIT));
        }
        out.push(
            Node::new(
                "next",
                Row {
                    padded: true,
                    gap: Gap::Medium,
                    ..Row::default()
                },
            )
            .with([
                Node::new(
                    "note",
                    Text {
                        text: note,
                        tone: Tone::Muted,
                        small: true,
                        grow: true,
                        ..Text::default()
                    },
                ),
                Node::new(
                    "to_choose",
                    Button {
                        label: "Next: preset".into(),
                        enabled: !self.selected.is_empty(),
                        tone: Tone::Accent,
                        ..Button::default()
                    },
                ),
            ]),
        );
        out
    }

    fn choose(&self) -> Vec<Node> {
        let mut out = Vec::new();
        let first = self.selected.first().cloned().unwrap_or_default();
        let picked = match self.selected.len() {
            0 => "No files selected.".to_string(),
            1 => first.clone(),
            n => format!("{first} and {} more", n - 1),
        };
        out.push(
            Node::new(
                "picked",
                Row {
                    padded: true,
                    ..Row::default()
                },
            )
            .with([Node::new(
                "what",
                Text {
                    text: picked,
                    tone: Tone::Muted,
                    small: true,
                    grow: true,
                    ..Text::default()
                },
            )]),
        );
        out.push(
            Node::new(
                "presets",
                List {
                    label: "Presets".into(),
                    selection: Selection::Single,
                    selected: self.preset.map(|p| vec![p.name()]).unwrap_or_default(),
                },
            )
            .with(PRESETS.iter().map(|p| {
                Node::new(
                    p.id.name(),
                    Stack {
                        gap: Gap::None,
                        padded: true,
                        ..Stack::default()
                    },
                )
                .with([
                    Node::new(
                        "title",
                        Text {
                            text: p.title.into(),
                            ..Text::default()
                        },
                    ),
                    Node::new(
                        "quality",
                        Text {
                            text: p.quality.into(),
                            tone: Tone::Muted,
                            small: true,
                            ..Text::default()
                        },
                    ),
                ])
            })),
        );
        let Some(preset) = self.preset else {
            out.push(Node::new(
                "hint",
                Empty {
                    title: "Choose a preset.".into(),
                    detail: Some("Each says what it costs in quality before you commit.".into()),
                },
            ));
            return out;
        };
        out.push(self.options(preset));
        out.extend(self.measured(preset, &first));
        out.push(self.start(preset));
        out
    }

    fn options(&self, preset: PresetId) -> Node {
        let trim = preset == PresetId::Trim;
        let range = vec![
            Node::new(
                "start",
                TextField {
                    name: "start".into(),
                    grow: true,
                    label: if trim { "From" } else { "From (optional)" }.into(),
                    placeholder: if trim { "From, e.g. 0:30" } else { "From" }.into(),
                    ..TextField::default()
                },
            ),
            Node::new(
                "end",
                TextField {
                    name: "end".into(),
                    grow: true,
                    label: if trim { "To" } else { "To (optional)" }.into(),
                    placeholder: if trim { "To, e.g. 1:45" } else { "To" }.into(),
                    ..TextField::default()
                },
            ),
        ];
        // One control per line, so the window works at 320 px: a row does
        // not wrap.
        let mut fields = vec![
            Node::new(
                "range",
                Row {
                    gap: Gap::Small,
                    ..Row::default()
                },
            )
            .with(range),
        ];
        if preset == PresetId::Remux {
            fields.push(Node::new(
                "container",
                Select {
                    name: "container".into(),
                    label: "Container".into(),
                    options: vec![
                        choice(Value::Null, "Automatic"),
                        choice("mp4".into(), "MP4"),
                        choice("mkv".into(), "MKV"),
                    ],
                    value: Value::Null,
                    revision: 0,
                },
            ));
            fields.push(Node::new(
                "keep_all_audio",
                Checkbox {
                    label: "All audio tracks".into(),
                    name: Some("keep_all_audio".into()),
                    checked: self.keep_all_audio,
                    ..Checkbox::default()
                },
            ));
        }
        if preset == PresetId::AudioFlac {
            fields.push(Node::new(
                "allow_lossy",
                Checkbox {
                    label: "Allow from lossy".into(),
                    name: Some("allow_lossy_to_lossless".into()),
                    checked: self.allow_lossy,
                    ..Checkbox::default()
                },
            ));
        }
        let mut dests = vec![choice(Value::Null, "Beside each file")];
        for (dir, write) in &self.shared.queue.grants().dirs {
            if *write {
                dests.push(choice(Value::String(dir.clone()), &format!("Into {dir}/")));
            }
        }
        fields.push(Node::new(
            "dest",
            Select {
                name: "dest".into(),
                label: "Save to".into(),
                options: dests,
                value: Value::Null,
                revision: 0,
            },
        ));
        Node::new(
            "options",
            Stack {
                padded: true,
                gap: Gap::Small,
                ..Stack::default()
            },
        )
        .with(fields)
    }

    /// The fields every bound action reads, so an estimate measures exactly
    /// what Start would queue.
    fn with_option_fields(action: Action) -> Action {
        action
            .field("start", "start")
            .field("end", "end")
            .field("container", "container")
            .field("keep_all_audio", "keep_all_audio")
            .field("allow_lossy_to_lossless", "allow_lossy_to_lossless")
    }

    fn measured(&self, preset: PresetId, first: &str) -> Vec<Node> {
        let mut out = Vec::new();
        let mut measure = Convert::with_option_fields(
            Action::command("convert.estimate")
                .param("path", first)
                .param("preset", preset.name()),
        );
        if preset == PresetId::Trim {
            measure = measure.require("start").require("end");
        }
        let state = self.shared.estimate(first, preset);
        let running = matches!(state, Some(Measuring::Running { .. }));
        out.push(
            Node::new(
                "measure",
                Row {
                    padded: true,
                    gap: Gap::Medium,
                    ..Row::default()
                },
            )
            .with([
                Node::new(
                    "go",
                    Button {
                        label: if preset.preset().encodes {
                            "Measure on samples"
                        } else {
                            "Check"
                        }
                        .into(),
                        action: Some(measure),
                        enabled: !running && !first.is_empty(),
                        ..Button::default()
                    },
                ),
                Node::new(
                    "show_command",
                    Checkbox {
                        label: "Show command".into(),
                        name: Some("show_command".into()),
                        checked: self.show_command,
                        ..Checkbox::default()
                    },
                ),
            ]),
        );
        let mut lines: Vec<(String, Tone)> = Vec::new();
        let mut command = None;
        match &state {
            None => lines.push((
                format!(
                    "Not measured yet. Measuring encodes {} short samples of {}.",
                    crate::estimate::SAMPLE_AT.len(),
                    first.rsplit('/').next().unwrap_or(first)
                ),
                Tone::Muted,
            )),
            Some(Measuring::Running { fraction, step }) => {
                out.push(Node::new(
                    "measuring",
                    Progress {
                        value: Some(f64::from(*fraction)),
                        label: "Measuring".into(),
                    },
                ));
                lines.push((format!("{step}…"), Tone::Muted));
            }
            Some(Measuring::Failed(e)) => lines.push((e.clone(), Tone::Danger)),
            Some(Measuring::Done(_, measured))
                if (preset == PresetId::Remux
                    && measured.keep_all_audio != self.keep_all_audio)
                    || (preset == PresetId::AudioFlac
                        && measured.allow_lossy_to_lossless != self.allow_lossy) =>
            {
                lines.push((
                    "Measured with other options than these; measure again.".into(),
                    Tone::Muted,
                ));
            }
            Some(Measuring::Done(e, measured)) => {
                lines.push((size_line(e), Tone::Normal));
                if let (Some(secs), Some(speed)) = (e.predicted_secs, e.speed) {
                    lines.push((
                        format!(
                            "About {} for this file, at {speed:.1}× real time.",
                            duration_words(secs)
                        ),
                        Tone::Normal,
                    ));
                }
                lines.push((e.gloss.clone(), Tone::Normal));
                lines.push((measured_on(measured), Tone::Muted));
                for w in &e.warnings {
                    lines.push((w.clone(), Tone::Warning));
                }
                if self.selected.len() > 1 {
                    lines.push((
                        "Measured on the first file; the others may differ.".into(),
                        Tone::Muted,
                    ));
                }
                command = Some(e.command.clone());
            }
        }
        if self.show_command {
            let text = command.unwrap_or_else(|| {
                format!(
                    "Measure to see the exact command for this file. Every {} command starts: \
                     ffmpeg {}",
                    preset.tag(),
                    crate::tools::Tools::input_guard().join(" ")
                )
            });
            lines.push((text, Tone::Muted));
        }
        out.push(
            Node::new(
                "result",
                Stack {
                    padded: true,
                    gap: Gap::Small,
                    ..Stack::default()
                },
            )
            .with(lines.into_iter().enumerate().map(|(i, (text, tone))| {
                Node::new(
                    format!("l{i}"),
                    Text {
                        text,
                        tone,
                        small: true,
                        ..Text::default()
                    },
                )
            })),
        );
        out
    }

    fn start(&self, preset: PresetId) -> Node {
        let mut add = Convert::with_option_fields(
            Action::command("convert.job.add")
                .param("paths", json!(self.selected))
                .param("preset", preset.name()),
        )
        .field("dest", "dest");
        if preset == PresetId::Trim {
            add = add.require("start").require("end");
        }
        let n = self.selected.len();
        Node::new(
            "start",
            Row {
                padded: true,
                ..Row::default()
            },
        )
        .with([Node::new(
            "go",
            Button {
                label: format!("Convert {n} file{}", if n == 1 { "" } else { "s" }),
                action: Some(add),
                enabled: n > 0,
                tone: Tone::Accent,
                ..Button::default()
            },
        )])
    }

    fn queue(&self) -> Vec<Node> {
        let mut out = Vec::new();
        let parallel = self.shared.queue.parallel();
        out.push(
            Node::new(
                "settings",
                Row {
                    padded: true,
                    gap: Gap::Medium,
                    divider: true,
                    ..Row::default()
                },
            )
            .with([
                Node::new(
                    "two",
                    Checkbox {
                        label: "Two at once".into(),
                        checked: parallel == 2,
                        action: Some(Action::command("convert.settings").value_as("two_at_once")),
                        ..Checkbox::default()
                    },
                ),
                Node::new(
                    "clear",
                    Button {
                        label: "Clear finished".into(),
                        action: Some(Action::command("convert.job.clear").param("finished", true)),
                        enabled: self.jobs.iter().any(|j| j.state.finished()),
                        ..Button::default()
                    },
                ),
            ]),
        );
        if self.jobs.is_empty() {
            out.push(Node::new(
                "empty",
                Empty {
                    title: "Nothing queued.".into(),
                    detail: Some("Pick files, choose a preset, and convert.".into()),
                },
            ));
            return out;
        }
        out.push(
            Node::new(
                "jobs",
                List {
                    label: "Jobs".into(),
                    ..List::default()
                },
            )
            .with(self.jobs.iter().map(job_row)),
        );
        let count = |s: JobState| self.jobs.iter().filter(|j| j.state == s).count();
        out.push(
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
                    text: format!(
                        "{} running, {} queued, {} done, {} failed · jobs keep running with \
                         this window closed",
                        count(JobState::Running),
                        count(JobState::Queued),
                        count(JobState::Done),
                        count(JobState::Failed)
                    ),
                    tone: Tone::Muted,
                    small: true,
                    ..Text::default()
                },
            )]),
        );
        out
    }
}

fn choice(value: Value, label: &str) -> Choice {
    Choice {
        value,
        label: label.into(),
    }
}

fn file_row(f: &Entry, picked: bool) -> Node {
    let (name, folder) = match f.path.rsplit_once('/') {
        Some((dir, name)) => (name.to_string(), dir.to_string()),
        None => (f.path.clone(), String::new()),
    };
    Node::new(
        f.path.clone(),
        Row {
            gap: Gap::Medium,
            padded: true,
            ..Row::default()
        },
    )
    .with([
        Node::new(
            "pick",
            Checkbox {
                label: format!("Pick {name}"),
                show_label: false,
                checked: picked,
                ..Checkbox::default()
            },
        ),
        Node::new(
            "name",
            Text {
                text: name.clone(),
                grow: true,
                ..Text::default()
            },
        ),
        Node::new(
            "where",
            Text {
                text: format!("{folder}/ · {}", human_bytes(f.size)),
                tone: Tone::Muted,
                small: true,
                ..Text::default()
            },
        ),
    ])
}

fn size_line(e: &Estimate) -> String {
    if e.samples.is_empty() {
        return format!(
            "About {}, the same as the source.",
            human_bytes(e.predicted_bytes)
        );
    }
    format!(
        "About {} ({} to {}), {:.0}% of the source.",
        human_bytes(e.predicted_bytes),
        human_bytes(e.low_bytes),
        human_bytes(e.high_bytes),
        e.percent_of_source
    )
}

/// `12 min`, `1 h 5 min`, `40 s`: readable at arm's length.
pub fn duration_words(secs: f64) -> String {
    let s = secs.max(0.0).round() as u64;
    if s < 60 {
        format!("{s} s")
    } else if s < 3600 {
        format!("{} min", s.div_ceil(60))
    } else {
        let m = s.div_ceil(60);
        format!("{} h {} min", m / 60, m % 60)
    }
}

fn job_row(job: &Job) -> Node {
    let (state, tone) = match job.state {
        JobState::Queued => ("Queued", Tone::Muted),
        JobState::Running => ("Converting", Tone::Accent),
        JobState::Done => ("Done", Tone::Normal),
        JobState::Failed => ("Failed", Tone::Danger),
        JobState::Cancelled => ("Cancelled", Tone::Muted),
    };
    let title = presets::PRESETS
        .iter()
        .find(|p| p.id == job.preset)
        .map_or("", |p| p.title);
    let detail = match job.state {
        JobState::Running => {
            let mut parts = Vec::new();
            if let Some(f) = job.fraction {
                parts.push(format!("{:.0}%", f * 100.0));
            }
            if let Some(s) = job.speed {
                parts.push(format!("{s:.1}×"));
            }
            match job.eta_secs {
                Some(e) => parts.push(format!("{} left", duration_words(e))),
                None => parts.push("starting".into()),
            }
            parts.join(" · ")
        }
        JobState::Queued => format!("{title}, waiting its turn"),
        JobState::Done => job.output.clone().unwrap_or_default(),
        JobState::Failed => job.error.clone().unwrap_or_default(),
        JobState::Cancelled => "Cancelled; nothing was kept.".into(),
    };
    let mut children = vec![
        Node::new(
            "head",
            Row {
                gap: Gap::Medium,
                ..Row::default()
            },
        )
        .with([
            Node::new(
                "name",
                Text {
                    text: job.name().to_string(),
                    grow: true,
                    ..Text::default()
                },
            ),
            Node::new(
                "state",
                Text {
                    text: state.into(),
                    tone,
                    small: true,
                    ..Text::default()
                },
            ),
        ]),
    ];
    if job.state == JobState::Running {
        children.push(Node::new(
            "bar",
            Progress {
                value: job.fraction,
                label: format!("Converting {}", job.name()),
            },
        ));
    }
    let mut foot = vec![Node::new(
        "detail",
        Text {
            text: detail,
            tone: if job.state == JobState::Failed {
                Tone::Danger
            } else {
                Tone::Muted
            },
            small: true,
            grow: true,
            ..Text::default()
        },
    )];
    match job.state {
        JobState::Queued | JobState::Running => foot.push(Node::new(
            "cancel",
            Button {
                label: "Cancel".into(),
                action: Some(Action::command("convert.job.cancel").param("id", job.id.clone())),
                ..Button::default()
            },
        )),
        JobState::Failed | JobState::Cancelled => foot.push(Node::new(
            "retry",
            Button {
                label: "Retry".into(),
                action: Some(Action::command("convert.job.retry").param("id", job.id.clone())),
                ..Button::default()
            },
        )),
        JobState::Done => {}
    }
    children.push(
        Node::new(
            "foot",
            Row {
                gap: Gap::Medium,
                ..Row::default()
            },
        )
        .with(foot),
    );
    for (i, w) in job.warnings.iter().enumerate() {
        children.push(Node::new(
            format!("w{i}"),
            Text {
                text: w.clone(),
                tone: Tone::Warning,
                small: true,
                ..Text::default()
            },
        ));
    }
    Node::new(
        job.id.clone(),
        Stack {
            gap: Gap::Small,
            padded: true,
            ..Stack::default()
        },
    )
    .with(children)
}

impl Program for Convert {
    type Msg = Msg;

    fn view(&self) -> Node {
        let body = match self.step {
            Step::Pick => self.pick(),
            Step::Choose => self.choose(),
            Step::Queue => self.queue(),
        };
        let key = match self.step {
            Step::Pick => "pick",
            Step::Choose => "choose",
            Step::Queue => "queue",
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
            self.steps_bar(),
            Node::new(
                key,
                Stack {
                    gap: Gap::None,
                    grow: true,
                    ..Stack::default()
                },
            )
            .with(body),
        ])
    }

    fn update(&mut self, msg: Msg) {
        match msg {
            Msg::Go(step) => {
                if step != Step::Choose || !self.selected.is_empty() {
                    self.step = step;
                }
                if step == Step::Queue {
                    self.jobs = self.shared.queue.list();
                }
            }
            Msg::Filter(text) => self.filter = text,
            Msg::Select(keys) => self.selected = keys,
            Msg::Pick(path, on) => {
                self.selected.retain(|p| p != &path);
                if on && self.files.iter().any(|f| f.path == path) {
                    self.selected.push(path);
                }
            }
            Msg::PickAll => {
                let shown: Vec<String> = self.visible().iter().map(|f| f.path.clone()).collect();
                for path in shown {
                    if !self.selected.contains(&path) {
                        self.selected.push(path);
                    }
                }
            }
            Msg::Preset(p) => self.preset = p,
            Msg::ShowCommand(on) => self.show_command = on,
            Msg::KeepAllAudio(on) => self.keep_all_audio = on,
            Msg::AllowLossy(on) => self.allow_lossy = on,
            Msg::Rescan => self.rescan(),
            Msg::Refresh => self.jobs = self.shared.queue.list(),
            Msg::Queued => {
                self.jobs = self.shared.queue.list();
                self.selected.clear();
                self.step = Step::Queue;
            }
        }
    }

    fn event(&self, event: &Event) -> Option<Msg> {
        match (event.kind, event.key()) {
            (EventKind::Activated, "pick") => Some(Msg::Go(Step::Pick)),
            (EventKind::Activated, "choose" | "to_choose") => Some(Msg::Go(Step::Choose)),
            (EventKind::Activated, "queue") => Some(Msg::Go(Step::Queue)),
            (EventKind::Activated, "rescan") => Some(Msg::Rescan),
            (EventKind::Changed, "filter") => Some(Msg::Filter(event.value.as_str()?.to_string())),
            (EventKind::Changed, "show_command") => Some(Msg::ShowCommand(event.value.as_bool()?)),
            (EventKind::Changed, "keep_all_audio") => {
                Some(Msg::KeepAllAudio(event.value.as_bool()?))
            }
            (EventKind::Changed, "allow_lossy") => Some(Msg::AllowLossy(event.value.as_bool()?)),
            // A row's tick box: the row's key is the file's path.
            (EventKind::Changed, "pick") => {
                let path = event
                    .path
                    .len()
                    .checked_sub(2)
                    .map(|i| event.path[i].clone())?;
                Some(Msg::Pick(path, event.value.as_bool()?))
            }
            (EventKind::Activated, "pick_all") => Some(Msg::PickAll),
            (EventKind::Activated, "pick_none") => Some(Msg::Select(Vec::new())),
            (EventKind::Selected, "presets") => {
                Some(Msg::Preset(strings(&event.value).first().and_then(|k| {
                    serde_json::from_value(Value::String(k.clone())).ok()
                })))
            }
            (EventKind::Opened, _) => Some(Msg::Rescan),
            _ => None,
        }
    }

    fn command_ran(&self, command: &str, params: &Value) -> Option<Msg> {
        if command == "convert.job.add" && params["paths"] == json!(self.selected) {
            return Some(Msg::Queued);
        }
        JOB_COMMANDS.contains(&command).then_some(Msg::Refresh)
    }

    fn notified(&self) -> Option<Msg> {
        Some(Msg::Refresh)
    }
}

fn strings(v: &Value) -> Vec<String> {
    v.as_array()
        .map(|a| {
            a.iter()
                .filter_map(|s| s.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::files::Grants;
    use crate::tools::Tools;
    use deckjack_app::ui::Widget;

    struct Fixture {
        dir: tempfile::TempDir,
        shared: Arc<Shared>,
    }

    /// A window over a working directory with two videos and a song, and a
    /// queue loaded from `jobs` (as `queue.json` would hold them).
    fn fixture(tools: Result<Tools, String>, jobs: Value) -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        for f in ["Videos/a.mp4", "Videos/trip/b.mov", "Music/song.mp3"] {
            let p = dir.path().join(f);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, b"x").unwrap();
        }
        let home = dir.path().join("home");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(
            home.join("queue.json"),
            serde_json::to_vec(&json!({"next": 9, "parallel": 1, "jobs": jobs})).unwrap(),
        )
        .unwrap();
        let grants = Grants::from_manifest(include_str!("../app.toml"), dir.path().to_path_buf());
        let queue = Queue::open(grants, tools, &home, || {});
        let shared = Arc::new(Shared {
            queue,
            notifier: Notifier::default(),
            scratch: dir.path().join("tmp"),
            estimates: Mutex::default(),
            gate: Default::default(),
        });
        Fixture { dir, shared }
    }

    fn fake_tools() -> Result<Tools, String> {
        Ok(Tools {
            ffmpeg: "/nonexistent/ffmpeg".into(),
            ffprobe: "/nonexistent/ffprobe".into(),
            threads: 1,
        })
    }

    fn job(id: &str, state: &str) -> Value {
        let error = if state == "failed" {
            json!("ffmpeg stopped: boom")
        } else {
            Value::Null
        };
        let output = if state == "done" {
            json!("Videos/a (Share).mp4")
        } else {
            Value::Null
        };
        json!({"id": id, "path": "Videos/a.mp4", "preset": "share-h264", "state": state,
               "added": 1, "error": error, "output": output})
    }

    fn at<'a>(view: &'a Node, path: &[&str]) -> &'a Node {
        let path: Vec<String> = path.iter().map(|s| s.to_string()).collect();
        view.find(&path)
            .unwrap_or_else(|| panic!("nothing at {path:?} in {view:#?}"))
    }

    fn ev(path: &[&str], kind: EventKind, value: Value) -> Event {
        Event {
            path: path.iter().map(|s| s.to_string()).collect(),
            kind,
            value,
            seq: None,
        }
    }

    fn step(w: &mut Convert, e: Event) {
        let msg = w.event(&e).expect("the window handles it");
        w.update(msg);
    }

    /// Tick each file's box, as a tap does.
    fn pick(w: &mut Convert, paths: &[&str]) {
        for p in paths {
            step(
                w,
                ev(
                    &["pick", "files", p, "pick"],
                    EventKind::Changed,
                    json!(true),
                ),
            );
        }
    }

    fn button(node: &Node) -> &Button {
        match &node.widget {
            Widget::Button(b) => b,
            other => panic!("not a button: {other:?}"),
        }
    }

    #[test]
    fn picking_lists_the_granted_media_as_a_multi_select_list() {
        let f = fixture(fake_tools(), json!([]));
        let mut w = Convert::open(Arc::clone(&f.shared));
        let view = w.view();
        let list = at(&view, &["pick", "files"]);
        let Widget::List(l) = &list.widget else {
            panic!()
        };
        assert_eq!(
            l.selection,
            Selection::None,
            "picked by tick boxes, which a phone can tap"
        );
        let mut keys: Vec<&str> = list.children.iter().map(|r| r.key.as_str()).collect();
        keys.sort();
        assert_eq!(
            keys,
            ["Music/song.mp3", "Videos/a.mp4", "Videos/trip/b.mov"]
        );
        assert!(!button(at(&view, &["pick", "next", "to_choose"])).enabled);
        assert!(!button(at(&view, &["bar", "choose"])).enabled);
        assert!(button(at(&view, &["pick", "tools", "pick_all"])).enabled);

        pick(&mut w, &["Videos/a.mp4"]);
        let view = w.view();
        assert!(button(at(&view, &["pick", "next", "to_choose"])).enabled);
        let Widget::Checkbox(tick) = &at(&view, &["pick", "files", "Videos/a.mp4", "pick"]).widget
        else {
            panic!()
        };
        assert!(tick.checked);
        assert_eq!(tick.label, "Pick a.mp4");
        // Unticking takes it out again.
        step(
            &mut w,
            ev(
                &["pick", "files", "Videos/a.mp4", "pick"],
                EventKind::Changed,
                json!(false),
            ),
        );
        assert!(w.selected.is_empty());
        pick(&mut w, &["Videos/a.mp4"]);
        // Filtering is the window's own.
        step(
            &mut w,
            ev(
                &["pick", "tools", "filter"],
                EventKind::Changed,
                json!("trip"),
            ),
        );
        assert_eq!(at(&w.view(), &["pick", "files"]).children.len(), 1);
        // All picks what the filter shows, and keeps what was picked.
        step(
            &mut w,
            ev(
                &["pick", "tools", "pick_all"],
                EventKind::Activated,
                Value::Null,
            ),
        );
        assert_eq!(w.selected, ["Videos/a.mp4", "Videos/trip/b.mov"]);
        // Everything shown is picked: the one button now picks none.
        let view = w.view();
        assert!(button(at(&view, &["pick", "tools", "pick_none"])).enabled);
        assert!(
            view.find(&["pick".into(), "tools".into(), "pick_all".into()])
                .is_none()
        );
        // A path that is not a listed file is never picked.
        pick(&mut w, &["Documents/x.mp4"]);
        assert_eq!(w.selected.len(), 2);
        step(
            &mut w,
            ev(
                &["pick", "tools", "pick_none"],
                EventKind::Activated,
                Value::Null,
            ),
        );
        assert!(w.selected.is_empty());
        pick(&mut w, &["Videos/a.mp4"]);
        // A file that disappears stops being selected.
        std::fs::remove_file(f.dir.path().join("Videos/a.mp4")).unwrap();
        step(&mut w, ev(&[], EventKind::Opened, Value::Null));
        assert!(w.selected.is_empty());
    }

    #[test]
    fn start_queues_exactly_the_selection_with_the_fields_an_estimate_reads() {
        let f = fixture(fake_tools(), json!([]));
        let mut w = Convert::open(Arc::clone(&f.shared));
        pick(&mut w, &["Videos/a.mp4", "Music/song.mp3"]);
        step(
            &mut w,
            ev(
                &["pick", "next", "to_choose"],
                EventKind::Activated,
                Value::Null,
            ),
        );
        assert_eq!(w.step, Step::Choose);
        step(
            &mut w,
            ev(&["choose", "presets"], EventKind::Selected, json!(["trim"])),
        );
        assert_eq!(w.preset, Some(PresetId::Trim));
        let view = w.view();
        let start = button(at(&view, &["choose", "start", "go"]));
        let add = start.action.as_ref().unwrap();
        assert_eq!(add.command, "convert.job.add");
        assert_eq!(
            add.params["paths"],
            json!(["Videos/a.mp4", "Music/song.mp3"])
        );
        assert_eq!(add.params["preset"], "trim");
        assert_eq!(add.required, ["start", "end"], "a trim needs both ends");
        assert_eq!(add.fields["dest"], "dest");
        let measure = button(at(&view, &["choose", "measure", "go"]))
            .action
            .clone()
            .unwrap();
        assert_eq!(measure.command, "convert.estimate");
        assert_eq!(measure.params["path"], "Videos/a.mp4");
        // The estimate reads the same option fields Start sends.
        for field in [
            "start",
            "end",
            "container",
            "keep_all_audio",
            "allow_lossy_to_lossless",
        ] {
            assert_eq!(measure.fields.get(field), add.fields.get(field), "{field}");
        }
        // Share needs no range.
        step(
            &mut w,
            ev(
                &["choose", "presets"],
                EventKind::Selected,
                json!(["share-h264"]),
            ),
        );
        let view = w.view();
        let go = button(at(&view, &["choose", "start", "go"]));
        assert!(go.action.as_ref().unwrap().required.is_empty());
    }

    #[test]
    fn this_windows_batch_moves_it_to_the_queue_and_anyone_elses_only_refreshes() {
        let f = fixture(fake_tools(), json!([]));
        let mut w = Convert::open(Arc::clone(&f.shared));
        pick(&mut w, &["Videos/a.mp4"]);
        let other = w.command_ran("convert.job.add", &json!({"paths": ["Music/song.mp3"]}));
        assert!(matches!(other, Some(Msg::Refresh)));
        let mine = w
            .command_ran("convert.job.add", &json!({"paths": ["Videos/a.mp4"]}))
            .unwrap();
        w.update(mine);
        assert_eq!(w.step, Step::Queue);
        assert!(w.selected.is_empty());
        assert!(w.command_ran("convert.probe", &Value::Null).is_none());
        assert!(
            matches!(w.notified(), Some(Msg::Refresh)),
            "a job moving re-renders"
        );
    }

    #[test]
    fn the_queue_shows_each_job_with_its_own_bound_action() {
        let f = fixture(
            fake_tools(),
            json!([job("j1", "queued"), job("j2", "failed"), job("j3", "done")]),
        );
        let w = Convert::open(Arc::clone(&f.shared));
        assert_eq!(w.step, Step::Queue, "a window opens where the work is");
        let view = w.view();
        let cancel = button(at(&view, &["queue", "jobs", "j1", "foot", "cancel"]));
        assert_eq!(
            cancel.action.as_ref().unwrap().command,
            "convert.job.cancel"
        );
        assert_eq!(cancel.action.as_ref().unwrap().params["id"], "j1");
        let retry = button(at(&view, &["queue", "jobs", "j2", "foot", "retry"]));
        assert_eq!(retry.action.as_ref().unwrap().params["id"], "j2");
        let Widget::Text(err) = &at(&view, &["queue", "jobs", "j2", "foot", "detail"]).widget
        else {
            panic!()
        };
        assert_eq!(
            (err.text.as_str(), err.tone),
            ("ffmpeg stopped: boom", Tone::Danger)
        );
        assert!(button(at(&view, &["queue", "settings", "clear"])).enabled);
        assert_eq!(
            at(&view, &["queue", "jobs", "j3", "foot"]).children.len(),
            1,
            "a finished job has no action"
        );
    }

    #[test]
    fn a_running_job_shows_a_bar_and_the_time_left() {
        let running: Job = serde_json::from_value(json!({
            "id": "j4", "path": "Videos/trip/b.mov", "preset": "small-av1", "state": "running",
            "added": 1, "fraction": 0.42, "speed": 1.5, "eta_secs": 700.0
        }))
        .unwrap();
        let row = job_row(&running);
        let Widget::Progress(bar) = &at(&row, &["bar"]).widget else {
            panic!()
        };
        assert_eq!(bar.value, Some(0.42));
        assert_eq!(bar.label, "Converting b.mov");
        let Widget::Text(detail) = &at(&row, &["foot", "detail"]).widget else {
            panic!()
        };
        assert_eq!(detail.text, "42% · 1.5× · 12 min left");
        assert!(matches!(
            at(&row, &["foot", "cancel"]).widget,
            Widget::Button(_)
        ));
    }

    #[test]
    fn an_estimate_in_flight_shows_progress_and_its_result_shows_the_band() {
        let f = fixture(fake_tools(), json!([]));
        let mut w = Convert::open(Arc::clone(&f.shared));
        pick(&mut w, &["Videos/a.mp4"]);
        step(
            &mut w,
            ev(&["bar", "choose"], EventKind::Activated, Value::Null),
        );
        step(
            &mut w,
            ev(
                &["choose", "presets"],
                EventKind::Selected,
                json!(["share-h264"]),
            ),
        );
        f.shared.set_estimate(
            "Videos/a.mp4",
            PresetId::ShareH264,
            Measuring::Running {
                fraction: 0.5,
                step: "Scoring sample 2 with VMAF".into(),
            },
        );
        let view = w.view();
        assert!(matches!(
            at(&view, &["choose", "measuring"]).widget,
            Widget::Progress(_)
        ));
        assert!(
            !button(at(&view, &["choose", "measure", "go"])).enabled,
            "one at a time"
        );
        let e = Estimate {
            path: "Videos/a.mp4".into(),
            preset: PresetId::ShareH264,
            tag: "share-h264@1".into(),
            source_bytes: 100_000_000,
            predicted_bytes: 34_000_000,
            low_bytes: 30_000_000,
            high_bytes: 40_000_000,
            percent_of_source: 34.0,
            predicted_secs: Some(240.0),
            speed: Some(2.1),
            metric: Some("VMAF"),
            score: Some(96.0),
            gloss: "VMAF 96, measured on 3 samples of your file: hard to tell apart.".into(),
            samples: vec![crate::estimate::Sample {
                at: 1.0,
                seconds: 2.0,
                bytes: 1,
                wall_secs: 1.0,
                score: Some(96.0),
            }],
            warnings: vec![],
            command: "/usr/bin/ffmpeg -i file:x".into(),
            real_start: None,
        };
        f.shared.set_estimate(
            "Videos/a.mp4",
            PresetId::ShareH264,
            Measuring::Done(Box::new(e.clone()), Default::default()),
        );
        step(
            &mut w,
            ev(
                &["choose", "measure", "show_command"],
                EventKind::Changed,
                json!(true),
            ),
        );
        let mut texts = Vec::new();
        for n in &at(&w.view(), &["choose", "result"]).children {
            if let Widget::Text(t) = &n.widget {
                texts.push(t.text.clone());
            }
        }
        assert_eq!(texts[0], "About 34 MB (30 MB to 40 MB), 34% of the source.");
        assert_eq!(texts[1], "About 4 min for this file, at 2.1× real time.");
        assert!(texts[2].starts_with("VMAF 96, measured on 3 samples"));
        assert_eq!(texts[3], "Measured on the whole file.");
        assert_eq!(texts.last().unwrap(), "/usr/bin/ffmpeg -i file:x");

        // A measurement of ten seconds says so: the window cannot see the
        // range field, so it must not pass it off as the whole file's.
        let ten = crate::presets::Options {
            range: Some(crate::presets::Range {
                start: 0.0,
                end: 10.0,
            }),
            ..Default::default()
        };
        f.shared.set_estimate(
            "Videos/a.mp4",
            PresetId::ShareH264,
            Measuring::Done(Box::new(e.clone()), ten),
        );
        let result = texts_of(&w.view(), &["choose", "result"]);
        assert_eq!(result[3], "Measured on 0:00.0 to 0:10.0.", "{result:?}");

        // A remux measured with one audio track is not shown as the answer
        // once every track is asked for.
        step(
            &mut w,
            ev(
                &["choose", "presets"],
                EventKind::Selected,
                json!(["remux"]),
            ),
        );
        f.shared.set_estimate(
            "Videos/a.mp4",
            PresetId::Remux,
            Measuring::Done(Box::new(e), Default::default()),
        );
        assert!(texts_of(&w.view(), &["choose", "result"])[0].starts_with("About 34 MB"));
        step(
            &mut w,
            ev(
                &["choose", "options", "keep_all_audio"],
                EventKind::Changed,
                json!(true),
            ),
        );
        let stale = texts_of(&w.view(), &["choose", "result"]);
        assert_eq!(
            stale[0], "Measured with other options than these; measure again.",
            "{stale:?}"
        );
        assert!(
            !stale.iter().any(|t| t.starts_with("About 34 MB")),
            "{stale:?}"
        );
    }

    fn texts_of(view: &Node, path: &[&str]) -> Vec<String> {
        at(view, path)
            .children
            .iter()
            .filter_map(|n| match &n.widget {
                Widget::Text(t) => Some(t.text.clone()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn an_option_box_stays_as_ticked_because_the_model_holds_it() {
        // The shell drops its local tick once the app answers, and shows the
        // tree again: a box the model forgot would untick itself and Start
        // would send `false`.
        let f = fixture(fake_tools(), json!([]));
        let mut w = Convert::open(Arc::clone(&f.shared));
        pick(&mut w, &["Music/song.mp3"]);
        step(
            &mut w,
            ev(&["bar", "choose"], EventKind::Activated, Value::Null),
        );
        for (preset, key) in [("audio-flac", "allow_lossy"), ("remux", "keep_all_audio")] {
            step(
                &mut w,
                ev(&["choose", "presets"], EventKind::Selected, json!([preset])),
            );
            let path = ["choose", "options", key];
            step(&mut w, ev(&path, EventKind::Changed, json!(true)));
            let view = w.view();
            let Widget::Checkbox(tick) = &at(&view, &path).widget else {
                panic!()
            };
            assert!(tick.checked, "{key}");
        }
    }

    #[test]
    fn without_ffmpeg_the_window_says_how_to_install_it() {
        let f = fixture(
            Err("ffmpeg and ffprobe were not found; sudo pacman -S ffmpeg".into()),
            json!([]),
        );
        let view = Convert::open(Arc::clone(&f.shared)).view();
        let Widget::Empty(e) = &at(&view, &["pick", "empty"]).widget else {
            panic!()
        };
        assert_eq!(e.title, "ffmpeg is not installed on this node.");
        assert!(e.detail.as_deref().unwrap().contains("pacman"));
    }

    #[test]
    fn durations_read_at_arms_length() {
        assert_eq!(duration_words(40.0), "40 s");
        assert_eq!(duration_words(700.0), "12 min");
        assert_eq!(duration_words(3900.0), "1 h 5 min");
    }
}
