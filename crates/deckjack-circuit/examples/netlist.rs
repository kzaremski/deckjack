//! Print a project's netlist in KiCad's S-expression format, or compare it
//! with a reference netlist.
//!
//! ```text
//! cargo run -p deckjack-circuit --example netlist -- PROJECT.kicad_pro
//! cargo run -p deckjack-circuit --example netlist -- PROJECT.kicad_pro --compare REFERENCE.net
//! ```
//!
//! `PROJECT` may also be the root `.kicad_sch`. With `--compare`, the output
//! is a parity report and the exit status is 0 only on parity.

use std::path::PathBuf;
use std::process::ExitCode;

use deckjack_circuit::{Project, compute, netlist};

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(path) = args.first() else {
        eprintln!("usage: netlist PROJECT.kicad_pro [--compare REFERENCE.net]");
        return ExitCode::from(2);
    };
    let project = match Project::open(&PathBuf::from(path)) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::FAILURE;
        }
    };
    for w in &project.warnings {
        eprintln!("warning: {w}");
    }
    let conn = compute(&project);
    for a in &conn.approximate {
        eprintln!("approximate: {a}");
    }
    let nl = netlist::build(&project, &conn);
    match (args.get(1).map(String::as_str), args.get(2)) {
        (Some("--compare"), Some(reference)) => {
            let bytes = match std::fs::read(reference) {
                Ok(b) => b,
                Err(e) => {
                    eprintln!("cannot read {reference}: {e}");
                    return ExitCode::FAILURE;
                }
            };
            let golden = match netlist::parse_kicad(&bytes) {
                Ok(g) => g,
                Err(e) => {
                    eprintln!("{reference}: {e}");
                    return ExitCode::FAILURE;
                }
            };
            let p = netlist::compare(&golden, &nl);
            println!(
                "components {}/{}  pins {}/{}  nets identical {}/{} (ours {})",
                p.test_components,
                p.golden_components,
                p.pins_matched,
                p.pins_compared,
                p.nets_identical,
                p.golden_nets,
                p.test_nets
            );
            for m in &p.missing_components {
                println!("missing component {m}");
            }
            for m in &p.extra_components {
                println!("extra component {m}");
            }
            for m in &p.mismatches {
                println!("{m}");
            }
            if p.matches() {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            }
        }
        _ => {
            print!("{}", netlist::write_kicad(&nl));
            ExitCode::SUCCESS
        }
    }
}
