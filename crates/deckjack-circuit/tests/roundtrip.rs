//! Load and save with no edits is byte-identical, for every fixture file.

mod common;

use deckjack_circuit::model::SheetModel;
use deckjack_circuit::project_file::ProjectFile;
use deckjack_circuit::{Project, sexpr};

#[test]
fn every_schematic_round_trips_byte_identical() {
    let files = common::files_with(&["kicad_sch"]);
    assert!(
        files.len() >= 60,
        "expected the whole corpus, found {}",
        files.len()
    );
    for path in &files {
        let bytes = std::fs::read(path).unwrap();
        let doc = sexpr::parse(&bytes).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
        assert!(
            doc.to_text().as_bytes() == bytes.as_slice(),
            "{} did not round-trip",
            path.display()
        );
        SheetModel::read(&doc).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    }
}

#[test]
fn every_project_file_round_trips_byte_identical() {
    let files = common::files_with(&["kicad_pro"]);
    assert!(
        files.len() >= 25,
        "expected the whole corpus, found {}",
        files.len()
    );
    for path in &files {
        let bytes = std::fs::read(path).unwrap();
        let pf = ProjectFile::parse(&bytes).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
        assert!(
            pf.text().as_bytes() == bytes.as_slice(),
            "{} did not round-trip",
            path.display()
        );
    }
}

#[test]
fn every_reference_netlist_parses() {
    for path in common::files_with(&["net"]) {
        let bytes = std::fs::read(&path).unwrap();
        let nl = deckjack_circuit::netlist::parse_kicad(&bytes)
            .unwrap_or_else(|e| panic!("{}: {e}", path.display()));
        assert!(!nl.nets.is_empty(), "{} has no nets", path.display());
    }
}

#[test]
fn every_project_opens_whole_and_unchanged() {
    for fx in common::fixtures() {
        let project = Project::open(&fx.root).unwrap_or_else(|e| panic!("{}: {e}", fx.name));
        assert!(
            project.warnings.is_empty(),
            "{}: {:?}",
            fx.name,
            project.warnings
        );
        assert!(
            project.changed_files().is_empty(),
            "{}: unedited files report changes",
            fx.name
        );
        // Every sheet file in the directory tree is reached by the hierarchy.
        let on_disk = common::files_with(&["kicad_sch"])
            .into_iter()
            .filter(|p| p.starts_with(&fx.dir))
            .count();
        assert_eq!(
            project.files.len(),
            on_disk,
            "{}: not every sheet was loaded",
            fx.name
        );
        assert!(!project.sheets.is_empty());
    }
}
