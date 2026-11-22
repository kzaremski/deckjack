//! `deckjack-office`: read, comment on and export office documents.
//!
//! Two lives in one binary. Launched by the core it is the app: it serves
//! the commands in `app.toml` and its UI. Launched by itself with
//! `--worker <dir>` it is a LibreOffice worker for one job and exits; see
//! `worker.rs` for why the engine never runs in the app process.

use std::sync::Arc;

use deckjack_app::{App, Ctx};
use deckjack_office::app::{
    CancelParams, CommentParams, ExportParams, Office, PagesParams, PathParams, RenderParams,
    SaveInPlaceParams, TextParams,
};
use deckjack_office::worker;
use serde_json::json;

#[derive(rust_embed::RustEmbed)]
#[folder = "ui"]
struct Ui;

const MANIFEST: &str = include_str!("../app.toml");

fn main() -> anyhow::Result<()> {
    let args: Vec<std::ffi::OsString> = std::env::args_os().collect();
    if args.get(1).is_some_and(|a| a == worker::WORKER_FLAG) {
        let dir = args
            .get(2)
            .map(std::path::PathBuf::from)
            .unwrap_or_default();
        std::process::exit(worker::worker_main(&dir));
    }

    // The folders this app may open are the installed manifest's: the copy
    // the core read and the kernel's grant was built from, so editing it and
    // approving Office again widens both together.
    let installed = std::env::current_exe().ok().and_then(|exe| {
        std::fs::read_to_string(exe.with_file_name(deckjack_protocol::MANIFEST_FILE)).ok()
    });
    let folders = deckjack_office::paths::folder_names(installed.as_deref(), MANIFEST)?;
    let office = Arc::new(Office::from_env(folders)?);

    let o = Arc::clone(&office);
    let open = move |p: PathParams, _cx: Ctx| {
        let o = Arc::clone(&o);
        async move { o.open(p).await }
    };
    let o = Arc::clone(&office);
    let pages = move |p: PagesParams, cx: Ctx| {
        let o = Arc::clone(&o);
        async move { o.pages(p, &cx).await }
    };
    let o = Arc::clone(&office);
    let render = move |p: RenderParams, _cx: Ctx| {
        let o = Arc::clone(&o);
        async move { o.render(p).await }
    };
    let o = Arc::clone(&office);
    let text = move |p: TextParams, _cx: Ctx| {
        let o = Arc::clone(&o);
        async move { o.text(p).await }
    };
    let o = Arc::clone(&office);
    let cancel = move |p: CancelParams, _cx: Ctx| {
        let o = Arc::clone(&o);
        async move { Ok(json!({ "job": p.job, "cancelled": o.cancel(&p.job) })) }
    };
    let o = Arc::clone(&office);
    let comment = move |p: CommentParams, _cx: Ctx| {
        let o = Arc::clone(&o);
        async move { o.comment(p).await }
    };
    let o = Arc::clone(&office);
    let save = move |p: SaveInPlaceParams, _cx: Ctx| {
        let o = Arc::clone(&o);
        async move { o.save_in_place(p).await }
    };
    let o = Arc::clone(&office);
    let export = move |p: ExportParams, cx: Ctx| {
        let o = Arc::clone(&o);
        async move { o.export_pdf(p, &cx).await }
    };

    App::new(MANIFEST)?
        .ui::<Ui>()
        .command("office.open", open)
        .command("office.pages", pages)
        .command("office.page.render", render)
        .command("office.text", text)
        .command("office.cancel", cancel)
        .command("office.comment.add", comment)
        .command("office.save.inplace", save)
        .command("office.export_pdf", export)
        .run()
}

#[cfg(test)]
mod tests {
    use super::{MANIFEST, Ui};
    use deckjack_protocol::{Manifest, Risk};

    fn manifest() -> Manifest {
        Manifest::parse(std::path::Path::new("app.toml"), MANIFEST).expect("a valid manifest")
    }

    /// The brief's manifest: the two folders documents arrive in, written,
    /// and nothing else — no network, no GPU, no bus.
    #[test]
    fn the_manifest_asks_for_the_document_folders_and_nothing_else() {
        let m = manifest();
        assert_eq!(m.app.name, "office");
        assert_eq!(m.title(), "Office");
        let dirs: Vec<(&str, bool)> = m
            .needs
            .working_dir
            .iter()
            .map(|d| (d.path.as_str(), d.write))
            .collect();
        assert_eq!(dirs, [("Documents", true), ("Downloads", true)]);
        assert!(!m.needs.gpu);
        assert!(m.needs.network.is_empty());
        assert!(!m.needs.invoke_commands);
    }

    /// Only writing a file beside the document is `write`; nothing is
    /// destructive, because nothing here replaces or deletes a file.
    #[test]
    fn only_the_commands_that_write_a_file_are_write() {
        let m = manifest();
        let writes: Vec<&str> = m
            .commands
            .iter()
            .filter(|c| c.risk != Risk::Read)
            .map(|c| c.id.as_str())
            .collect();
        assert_eq!(
            writes,
            [
                "office.comment.add",
                "office.save.inplace",
                "office.export_pdf"
            ]
        );
        assert!(m.commands.iter().all(|c| c.risk != Risk::Destructive));
    }

    /// Every core command the UI names is declared in `[ui] invoke`, and
    /// nothing more; and every `office.*` command it names is one the app
    /// registers.
    #[test]
    fn the_ui_declares_exactly_the_core_commands_it_names() {
        let m = manifest();
        let script = include_str!("../ui/office.js");
        let mut declared = m.needs.ui.invoke.clone();
        declared.sort();
        let mut named = quoted_ids(script, "files.");
        named.sort();
        named.dedup();
        assert_eq!(declared, named);
        let own: Vec<String> = m.commands.iter().map(|c| c.id.clone()).collect();
        for id in quoted_ids(script, "office.") {
            assert!(
                own.contains(&id),
                "the UI calls {id}, which the app does not register"
            );
        }
    }

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

    /// Built against the isolated frame from day one (APP-UI-ISOLATION §4):
    /// no inline script, nothing fetched (`connect-src 'none'`), no
    /// `/content` route, no wasm.
    #[test]
    fn the_ui_is_embedded_and_needs_nothing_the_isolated_frame_forbids() {
        for path in ["index.html", "office.js", "main.js"] {
            let file = <Ui as rust_embed::RustEmbed>::get(path)
                .unwrap_or_else(|| panic!("{path} is not embedded"));
            assert!(!file.data.is_empty(), "{path} is empty");
        }
        let index = String::from_utf8_lossy(
            &<Ui as rust_embed::RustEmbed>::get("index.html")
                .unwrap()
                .data,
        )
        .into_owned();
        assert!(
            !index.contains("<script>") && !index.contains("<script type=\"module\">"),
            "an inline script: the isolated frame's CSP has no 'unsafe-inline' for scripts"
        );
        assert!(index.contains("src=\"./main.js\""));
        for path in ["office.js", "main.js", "index.html"] {
            let text =
                String::from_utf8_lossy(&<Ui as rust_embed::RustEmbed>::get(path).unwrap().data)
                    .into_owned();
            for forbidden in [
                "fetch(",
                "XMLHttpRequest",
                "/content",
                "WebAssembly",
                "new WebSocket",
                "EventSource",
            ] {
                assert!(!text.contains(forbidden), "{path} uses {forbidden}");
            }
        }
    }
}
