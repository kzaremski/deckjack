//! The shell's TypeScript applies the patches this crate's `diff` makes, so
//! the two must agree on every case, not only on the ones each side's own
//! tests thought of.
//!
//! This test generates trees and edits deterministically, diffs them here,
//! and checks the result against `web/src/uiproto/patches.fixture.json`,
//! which `web/src/uiproto/tree.test.ts` replays through `applyPatches`. A
//! change to the diff or the wire shape fails here until the fixture is
//! regenerated (`DJ_UPDATE_FIXTURES=1 cargo test -p deckjack-protocol --test
//! ui_fixture`), and then fails in vitest if the shell no longer agrees.

use deckjack_protocol::ui::{self, Button, Checkbox, Node, Row, Stack, Text, Widget};
use serde_json::json;

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

fn widget(rng: &mut Rng) -> Widget {
    match rng.below(5) {
        0 => Stack::default().into(),
        1 => Row {
            divider: rng.below(2) == 0,
            ..Row::default()
        }
        .into(),
        2 => Text {
            text: format!("t{}", rng.below(4)),
            strike: rng.below(2) == 0,
            ..Text::default()
        }
        .into(),
        3 => Checkbox {
            checked: rng.below(2) == 0,
            ..Checkbox::default()
        }
        .into(),
        _ => Button {
            label: format!("b{}", rng.below(3)),
            ..Button::default()
        }
        .into(),
    }
}

fn tree(rng: &mut Rng, key: String, depth: u32) -> Node {
    let mut node = Node::new(key, widget(rng));
    if depth > 0 {
        let mut pool: Vec<String> = (0..7).map(|i| format!("k{i}")).collect();
        for _ in 0..rng.below(6) {
            let at = rng.below(pool.len() as u64) as usize;
            let key = pool.swap_remove(at);
            node.children.push(tree(rng, key, depth - 1));
        }
    }
    node
}

fn cases() -> serde_json::Value {
    let mut out = Vec::new();
    for seed in 1..=80u64 {
        let mut rng = Rng(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1);
        let a = tree(&mut rng, "root".into(), 3);
        let b = tree(&mut rng, "root".into(), 3);
        let patches = ui::diff(&a, &b);
        out.push(json!({ "seed": seed, "a": a, "b": b, "patches": patches }));
    }
    serde_json::Value::Array(out)
}

#[test]
fn the_shells_patch_fixture_is_what_this_diff_produces() {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../web/src/uiproto/patches.fixture.json");
    let fresh = serde_json::to_string(&cases()).unwrap() + "\n";
    if std::env::var("DJ_UPDATE_FIXTURES").as_deref() == Ok("1") {
        std::fs::write(&path, &fresh).unwrap();
    }
    let on_disk = std::fs::read_to_string(&path).unwrap_or_default();
    assert!(
        on_disk == fresh,
        "{} is stale: regenerate it with DJ_UPDATE_FIXTURES=1 cargo test -p deckjack-protocol \
         --test ui_fixture",
        path.display()
    );
}
