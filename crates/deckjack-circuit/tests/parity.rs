//! Netlist parity with the reference netlists KiCad's own repository ships
//! (`qa/data/eeschema/netlists`), compared the way KiCad's regression test
//! compares them, and stricter: every net identical by name and members.

mod common;

use std::collections::BTreeSet;

use deckjack_circuit::netlist::{self, Netlist};
use deckjack_circuit::{Project, compute};

/// References written by a KiCad older than 7, which named a net after a pin
/// number (`Net-(D1-Pad2)`) where KiCad 7 and later use the pin name
/// (`Net-(D1-A)`). KiCad's own netlist test does not use these. For them,
/// the nets must partition the pins identically; names may differ by that
/// rule only.
const PRE_KICAD7_REFERENCES: [&str; 1] = ["legacy_power"];

fn partition(nl: &Netlist) -> BTreeSet<BTreeSet<(String, String)>> {
    nl.nets
        .iter()
        .map(|n| {
            n.nodes
                .iter()
                .map(|nd| (nd.reference.clone(), nd.pin.clone()))
                .collect()
        })
        .collect()
}

#[test]
fn netlists_match_kicad_references() {
    let mut report = Vec::new();
    let mut failures = Vec::new();
    let mut compared = 0;
    for fx in common::fixtures() {
        let Some(reference) = &fx.reference else {
            continue;
        };
        compared += 1;
        let golden = netlist::parse_kicad(&std::fs::read(reference).unwrap()).unwrap();
        let project = Project::open(&fx.root).unwrap();
        let conn = compute(&project);
        let ours = netlist::build(&project, &conn);
        let p = netlist::compare(&golden, &ours);
        report.push(format!(
            "{:34} pins {:3}/{:3}  nets {:2}/{:2}",
            fx.name, p.pins_matched, p.pins_compared, p.nets_identical, p.golden_nets
        ));
        if PRE_KICAD7_REFERENCES.contains(&fx.name.as_str()) {
            if partition(&golden) != partition(&ours) || !p.missing_components.is_empty() {
                failures.push(format!("{}: pin partition differs", fx.name));
            }
            for m in &p.mismatches {
                // The only allowed difference is the pin-number net name.
                if !m.contains("-Pad") {
                    failures.push(format!("{}: {m}", fx.name));
                }
            }
            continue;
        }
        if !p.matches() || !p.nets_identical() {
            failures.push(format!(
                "{}: missing {:?} extra {:?} {:?}",
                fx.name, p.missing_components, p.extra_components, p.mismatches
            ));
        }
    }
    eprintln!("{}", report.join("\n"));
    assert!(
        compared >= 22,
        "expected 22 reference netlists, found {compared}"
    );
    assert!(
        failures.is_empty(),
        "parity failures:\n{}",
        failures.join("\n")
    );
}

/// The written netlist reads back to the same nets, so the export is a
/// faithful carrier of the engine's result.
#[test]
fn written_netlists_read_back() {
    for fx in common::fixtures() {
        let project = Project::open(&fx.root).unwrap();
        let conn = compute(&project);
        let ours = netlist::build(&project, &conn);
        let text = netlist::write_kicad(&ours);
        let back =
            netlist::parse_kicad(text.as_bytes()).unwrap_or_else(|e| panic!("{}: {e}", fx.name));
        assert_eq!(back.nets, ours.nets, "{}", fx.name);
        assert_eq!(back.components.len(), ours.components.len(), "{}", fx.name);
    }
}

/// Computing twice gives the same netlist: nothing depends on hash order.
#[test]
fn netlists_are_deterministic() {
    for fx in common::fixtures() {
        let project = Project::open(&fx.root).unwrap();
        let a = netlist::write_kicad(&netlist::build(&project, &compute(&project)));
        let b = netlist::write_kicad(&netlist::build(&project, &compute(&project)));
        assert_eq!(a, b, "{}", fx.name);
    }
}
