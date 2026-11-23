//! Parity with `kicad-cli sch export netlist`, run live when KiCad is
//! installed (`pacman -S kicad` on Arch). Without it the test passes with a
//! message saying it was skipped, so the gate never claims a comparison it
//! did not make.
//!
//! `DJ_KICAD_CLI` overrides the binary's path.

mod common;

use std::path::PathBuf;
use std::process::Command;

use deckjack_circuit::netlist;
use deckjack_circuit::{Project, compute};

fn kicad_cli() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("DJ_KICAD_CLI") {
        return Some(PathBuf::from(p));
    }
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|d| d.join("kicad-cli"))
        .find(|p| p.is_file())
}

#[test]
fn netlists_match_kicad_cli() {
    let Some(cli) = kicad_cli() else {
        eprintln!(
            "SKIPPED: kicad-cli is not installed; install KiCad (pacman -S kicad) to run live netlist parity"
        );
        return;
    };
    let version = Command::new(&cli)
        .arg("--version")
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string());
    eprintln!("kicad-cli {}", version.unwrap_or_default());
    let mut failures = Vec::new();
    let (mut projects, mut codes_differ, mut classes_differ) = (0, 0, 0);
    let mut components = 0;
    for fx in common::fixtures() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join(&fx.name);
        common::copy_dir(&fx.dir, &dir);
        let root = dir.join(fx.root.file_name().unwrap());
        let out = tmp.path().join("out.net");
        // KiCad writes its config under HOME; keep it out of the operator's.
        // From the project directory: a project without a `.kicad_pro`
        // takes the working directory as its own, and KiCad writes each
        // component's Sheetfile relative to it.
        let status = Command::new(&cli)
            .current_dir(&dir)
            .env("HOME", tmp.path())
            .args(["sch", "export", "netlist", "--format", "kicadsexpr", "-o"])
            .arg(&out)
            .arg(&root)
            .output()
            .unwrap();
        if !status.status.success() || !out.is_file() {
            failures.push(format!(
                "{}: kicad-cli failed: {}",
                fx.name,
                String::from_utf8_lossy(&status.stderr)
            ));
            continue;
        }
        let golden = netlist::parse_kicad(&std::fs::read(&out).unwrap()).unwrap();
        let project = Project::open(&root).unwrap();
        let ours = netlist::build(&project, &compute(&project));
        let p = netlist::compare(&golden, &ours);
        eprintln!(
            "{:34} pins {}/{} nets {}/{}",
            fx.name, p.pins_matched, p.pins_compared, p.nets_identical, p.golden_nets
        );
        if !p.matches() || !p.nets_identical() {
            failures.push(format!(
                "{}: {:?} {:?}",
                fx.name, p.missing_components, p.mismatches
            ));
        }
        // Stricter: the nets section record for record (name, every node
        // with its pin function and type). Net codes and net classes are
        // known gaps (see the lane document) and are counted, not failed.
        let strip = |n: &netlist::NetRecord| (n.name.clone(), n.nodes.clone());
        let (gs, os): (Vec<_>, Vec<_>) = (
            golden.nets.iter().map(strip).collect(),
            ours.nets.iter().map(strip).collect(),
        );
        if gs != os {
            let diff: Vec<_> = gs.iter().filter(|g| !os.contains(g)).collect();
            failures.push(format!("{}: nets section differs: {diff:?}", fx.name));
        }
        codes_differ += golden
            .nets
            .iter()
            .zip(&ours.nets)
            .filter(|(g, o)| g.code != o.code)
            .count();
        classes_differ += golden
            .nets
            .iter()
            .zip(&ours.nets)
            .filter(|(g, o)| g.class != o.class)
            .count();
        // Components: reference, value, footprint, sheet path, the set of
        // UUIDs and the primary (last) UUID. The order of a multi-unit
        // symbol's other UUIDs follows KiCad's R-tree iteration and is not
        // reproduced.
        let key = |c: &netlist::Component| {
            let mut set = c.tstamps.clone();
            set.sort();
            (
                c.reference.clone(),
                c.value.clone(),
                c.footprint.clone(),
                c.sheet_names.clone(),
                c.sheet_tstamps.clone(),
                set,
                c.tstamps.last().cloned(),
            )
        };
        let (mut g, mut o): (Vec<_>, Vec<_>) = (
            golden.components.iter().map(key).collect(),
            ours.components.iter().map(key).collect(),
        );
        g.sort();
        o.sort();
        if g != o {
            let only_g: Vec<_> = g.iter().filter(|c| !o.contains(c)).collect();
            let only_o: Vec<_> = o.iter().filter(|c| !g.contains(c)).collect();
            failures.push(format!(
                "{}: components differ: kicad {only_g:?} ours {only_o:?}",
                fx.name
            ));
        }
        // Component properties (Sheetname, Sheetfile, the placing sheet's
        // user fields, the symbol's user fields, dnp, exclude_from_bom,
        // ki_keywords, ki_fp_filters), which PCB import copies onto
        // footprints. Every difference is a failure. The root sheet's
        // name (a root-sheet component's Sheetname) follows a rule inferred
        // from these fixtures (`netlist::root_sheet_name`); this is what
        // checks it.
        let props = |c: &netlist::Component| -> Vec<(String, Option<String>)> {
            let mut v: Vec<_> = c.properties.to_vec();
            v.sort();
            v
        };
        components += golden.components.len();
        for gc in &golden.components {
            let Some(oc) = ours
                .components
                .iter()
                .find(|o| o.reference == gc.reference && o.sheet_tstamps == gc.sheet_tstamps)
            else {
                continue;
            };
            if props(gc) != props(oc) {
                failures.push(format!(
                    "{}: {} properties differ: kicad {:?} ours {:?}",
                    fx.name,
                    gc.reference,
                    props(gc),
                    props(oc)
                ));
            }
        }
        projects += 1;
    }
    eprintln!(
        "{projects} projects and {components} components compared; known gaps: {codes_differ} net codes and {classes_differ} net classes differ"
    );
    assert!(
        failures.is_empty(),
        "kicad-cli parity failures:\n{}",
        failures.join("\n")
    );
}

/// KiCad 9's local power symbols: the shared amplifier sheet's power
/// symbols made local, so each of its two instances has its own `+12V`,
/// `GND` and so on, named under the sheet path, while the root's global
/// ones keep their plain names. Checked by hand always, and against
/// `kicad-cli` when it is installed.
#[test]
fn local_power_is_per_sheet_instance() {
    let fx = common::fixtures()
        .into_iter()
        .find(|f| f.name == "complex_hierarchy_shared")
        .unwrap();
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join(&fx.name);
    common::copy_dir(&fx.dir, &dir);
    let sheet = dir.join("ampli_ht/ampli_ht.kicad_sch");
    let text = std::fs::read_to_string(&sheet).unwrap();
    assert_eq!(text.matches("(power)").count(), 4);
    std::fs::write(&sheet, text.replace("(power)", "(power local)")).unwrap();
    let root = dir.join(fx.root.file_name().unwrap());
    let project = Project::open(&root).unwrap();
    let ours = netlist::build(&project, &compute(&project));
    let names: Vec<&str> = ours.nets.iter().map(|n| n.name.as_str()).collect();
    for instance in ["/ampli_ht_vertical1/", "/ampli_ht_horizontal1/"] {
        for power in ["+12V", "-VAA", "GND"] {
            let local = format!("{instance}{power}");
            assert!(names.contains(&local.as_str()), "no {local} in {names:?}");
        }
    }
    // The root sheet's power symbols are still global.
    assert!(names.contains(&"GND") && names.contains(&"HT"), "{names:?}");

    let Some(cli) = kicad_cli() else {
        eprintln!("SKIPPED kicad-cli check: KiCad is not installed");
        return;
    };
    let out = tmp.path().join("out.net");
    let status = Command::new(&cli)
        .current_dir(&dir)
        .env("HOME", tmp.path())
        .args(["sch", "export", "netlist", "--format", "kicadsexpr", "-o"])
        .arg(&out)
        .arg(&root)
        .output()
        .unwrap();
    assert!(
        status.status.success(),
        "{}",
        String::from_utf8_lossy(&status.stderr)
    );
    let golden = netlist::parse_kicad(&std::fs::read(&out).unwrap()).unwrap();
    let p = netlist::compare(&golden, &ours);
    assert!(
        p.matches() && p.nets_identical(),
        "{:?} {:?}",
        p.missing_components,
        p.mismatches
    );
    let golden_names: Vec<&str> = golden.nets.iter().map(|n| n.name.as_str()).collect();
    assert_eq!(golden_names.len(), names.len());
    for n in &names {
        assert!(golden_names.contains(n), "KiCad has no net {n}");
    }
}
