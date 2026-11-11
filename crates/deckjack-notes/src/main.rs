//! `deckjack-notes`: the Notes app.
//!
//! A note is a Markdown file under `Notes/` in the working directory. That is
//! the entire data model, and it is the design's rule for user data rather
//! than a shortcut: "user data never does — that stays in plain files and
//! open formats, so if Deckjack disappeared the files would still work with
//! other tools."
//!
//! This file is only the seam between the bus and [`notes`]. Every rule about
//! names, titles and atomic writes is in that module, where it is tested
//! without a socket.

mod notes;

use std::path::PathBuf;

use deckjack_app::{App, Ctx};
use serde::Deserialize;
use serde_json::{Value, json};

#[derive(rust_embed::RustEmbed)]
#[folder = "ui"]
struct Ui;

/// The working directory is the process's cwd: the core launches an app there,
/// which is what "apps see the working directory as their filesystem root"
/// means in practice.
fn working_dir() -> PathBuf {
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
}

/// Every handler starts here, so a missing `Notes/` is one error in one place.
fn root() -> Result<PathBuf, String> {
    notes::root(&working_dir()).map_err(|e| e.to_string())
}

#[derive(Deserialize, schemars::JsonSchema)]
struct NoParams {}

#[derive(Deserialize, schemars::JsonSchema)]
struct Name {
    /// The note's filename, as `notes.list` reports it.
    name: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct Save {
    name: String,
    /// The note's full Markdown text. Replaces what is there.
    text: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct Create {
    /// What to call it. The filename is derived from this.
    title: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct Rename {
    name: String,
    /// A new title, or a new filename ending in `.md`.
    to: String,
}

fn main() -> anyhow::Result<()> {
    App::new(include_str!("../app.toml"))?
        .ui::<Ui>()
        .command("notes.list", |_p: NoParams, _cx: Ctx| async move {
            let notes = notes::list(&root()?).map_err(|e| e.to_string())?;
            Ok(json!({ "notes": notes }))
        })
        .command("notes.read", |p: Name, _cx: Ctx| async move {
            let text = notes::read(&root()?, &p.name).map_err(|e| e.to_string())?;
            Ok(json!({ "name": p.name, "text": text }))
        })
        .command("notes.write", |p: Save, _cx: Ctx| async move {
            notes::write(&root()?, &p.name, &p.text).map_err(|e| e.to_string())?;
            Ok(json!({ "name": p.name, "bytes": p.text.len() }))
        })
        .command("notes.create", |p: Create, _cx: Ctx| async move {
            let note = notes::create(&root()?, &p.title).map_err(|e| e.to_string())?;
            Ok(serde_json::to_value(note).unwrap_or(Value::Null))
        })
        .command("notes.rename", |p: Rename, _cx: Ctx| async move {
            let note = notes::rename(&root()?, &p.name, &p.to).map_err(|e| e.to_string())?;
            Ok(serde_json::to_value(note).unwrap_or(Value::Null))
        })
        .command("notes.delete", |p: Name, _cx: Ctx| async move {
            notes::delete(&root()?, &p.name).map_err(|e| e.to_string())?;
            Ok(json!({ "deleted": p.name }))
        })
        .run()
}
