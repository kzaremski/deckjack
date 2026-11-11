//! `deckjack-files`: the Files app.
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
