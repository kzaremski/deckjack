//! `deckjack-filer`: the Files app. Its name is `filer` because `files` is
//! the core's namespace; see `app.toml`.
//!
//! All UI. See `app.toml` for why it has no backend: the file commands are
//! the core's, because they are what keeps an address inside the working
//! directory, and one implementation of that boundary is the right number.

use deckjack_app::App;

#[derive(rust_embed::RustEmbed)]
#[folder = "ui"]
struct Ui;

fn main() -> anyhow::Result<()> {
    App::new(include_str!("../app.toml"))?.ui::<Ui>().run()
}

#[cfg(test)]
mod tests {
    use deckjack_protocol::Manifest;

    fn manifest() -> Manifest {
        Manifest::parse(
            std::path::Path::new("app.toml"),
            include_str!("../app.toml"),
        )
        .expect("a valid manifest")
    }

    /// Renamed off the core's namespace, still called Files where the
    /// operator reads it, and it needs approving: its window calls the
    /// core's file commands, which is a need (D1).
    #[test]
    fn the_app_is_filer_titled_files_and_needs_approving() {
        let manifest = manifest();
        assert_eq!(manifest.app.name, "filer");
        assert_eq!(manifest.title(), "Files");
        assert!(manifest.commands.is_empty());
        assert!(!manifest.needs.is_empty());
    }

    /// Every core command the UI names is declared in `[ui] invoke`, and
    /// nothing more. Read from `files.js` itself as every quoted `"files.…"`
    /// string, which catches `files.copy` and `files.move` too: those are
    /// picked into a variable before the call.
    #[test]
    fn the_ui_declares_exactly_the_core_commands_it_names() {
        let mut declared = manifest().needs.ui.invoke;
        declared.sort();
        let opening = "\"files.";
        let mut named: Vec<String> = include_str!("../ui/files.js")
            .split(opening)
            .skip(1)
            .filter_map(|rest| rest.split('"').next())
            .filter(|tail| {
                tail.chars()
                    .all(|c| c.is_ascii_lowercase() || c == '.' || c == '_')
            })
            .map(|tail| format!("files.{tail}"))
            .collect();
        named.sort();
        named.dedup();
        assert_eq!(declared, named);
    }
}
