//! `deckjack-scribe`: the Editor app. Its name is `scribe` because `editor`
//! is the core's namespace; see `app.toml`.
//!
//! All UI, like Files. See `app.toml` for why it has no backend of its own:
//! the editor's commands are the core's (`crates/deckjack-core/src/editor.rs`),
//! because the core is the one process that may open the working directory.

use deckjack_app::App;

#[derive(rust_embed::RustEmbed)]
#[folder = "ui"]
struct Ui;

fn main() -> anyhow::Result<()> {
    App::new(include_str!("../app.toml"))?.ui::<Ui>().run()
}

#[cfg(test)]
mod tests {
    use super::Ui;
    use deckjack_protocol::Manifest;

    /// The manifest parses and validates, and puts the editor in the dock
    /// under its own name: the shell's open-app contract addresses it as
    /// `scribe` (`EDITOR_APP` in `web/src/shell/quicklook.ts`), and the dock
    /// is built from these fields.
    #[test]
    fn the_manifest_is_valid_and_reaches_the_dock() {
        let manifest = Manifest::parse(
            std::path::Path::new("app.toml"),
            include_str!("../app.toml"),
        )
        .expect("a valid manifest");
        assert_eq!(manifest.app.name, "scribe");
        assert_eq!(manifest.title(), "Editor");
        assert_eq!(manifest.app.tag.as_deref(), Some("Ed"));
        assert!(manifest.app.summary.is_some());
        assert!(manifest.app.default_width.is_some() && manifest.app.default_height.is_some());
        // Its own process asks for nothing and provides nothing; its window
        // calls the core's commands, and those are its whole approval.
        assert!(manifest.commands.is_empty());
        assert!(!manifest.needs.gpu && manifest.needs.working_dir.is_empty());
        assert!(manifest.needs.network.is_empty() && !manifest.needs.invoke_commands);
    }

    /// Every core command the UI names is declared in `[ui] invoke`, and
    /// nothing more. Read from `editor.js` itself, as every quoted
    /// `"editor.…"` or `"files.…"` string (a command held in a variable is
    /// still a quoted literal somewhere), so a call added there without the
    /// manifest is a red test here rather than a refusal in the operator's
    /// window once app windows are held to their scope.
    #[test]
    fn the_ui_declares_exactly_the_core_commands_it_names() {
        let manifest = Manifest::parse(
            std::path::Path::new("app.toml"),
            include_str!("../app.toml"),
        )
        .expect("a valid manifest");
        let mut declared = manifest.needs.ui.invoke.clone();
        declared.sort();
        let script = include_str!("../ui/editor.js");
        let mut named = quoted_ids(script, "editor.");
        named.extend(quoted_ids(script, "files."));
        named.sort();
        named.dedup();
        assert_eq!(declared, named);
    }

    /// Every `"<prefix>…"` string literal in a script.
    fn quoted_ids(script: &str, prefix: &str) -> Vec<String> {
        let opening = format!("\"{prefix}");
        script
            .split(opening.as_str())
            .skip(1)
            .filter_map(|rest| rest.split('"').next())
            .filter(|tail| {
                tail.chars()
                    .all(|c| c.is_ascii_lowercase() || c == '.' || c == '_')
            })
            .map(|tail| format!("{prefix}{tail}"))
            .collect()
    }

    /// What the binary serves is the UI and the vendored editor, embedded.
    /// A missing bundle would be a blank window with a module error in a
    /// console nobody on a phone can open.
    #[test]
    fn the_ui_and_its_vendored_editor_are_embedded() {
        for path in ["index.html", "editor.js", "vendor/codemirror.js"] {
            let file = <Ui as rust_embed::RustEmbed>::get(path)
                .unwrap_or_else(|| panic!("{path} is not embedded"));
            assert!(!file.data.is_empty(), "{path} is empty");
        }
        let index = <Ui as rust_embed::RustEmbed>::get("index.html").unwrap();
        let index = String::from_utf8_lossy(&index.data);
        // Relative, so it resolves under /apps/editor/ and never to a CDN.
        assert!(index.contains("./editor.js"));
        let bundle = <Ui as rust_embed::RustEmbed>::get("vendor/codemirror.js").unwrap();
        let bundle = String::from_utf8_lossy(&bundle.data);
        for remote in ["from\"http", "from \"http", "import(\"http", "import(`http"] {
            assert!(
                !bundle.contains(remote),
                "the vendored bundle loads code from the network ({remote})"
            );
        }
    }
}
