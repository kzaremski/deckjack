//! Deckjack's circuit library: KiCad schematics read and written without
//! damage, their connectivity, and their netlist.
//!
//! The pieces, bottom up:
//!
//! * [`sexpr`]: a lossless S-expression tree. Load and save with no edits is
//!   byte-identical; an edit changes only the bytes it touches.
//! * [`project_file`]: the same guarantee for the JSON `.kicad_pro`.
//! * [`model`]: a typed view of one `.kicad_sch` over that tree.
//! * [`project`]: a project, its sheet files (confined to the project
//!   directory) and the sheet hierarchy.
//! * [`connectivity`]: nets and net names, following KiCad's connection
//!   graph.
//! * [`netlist`]: KiCad's S-expression netlist format, written, read and
//!   compared.
//! * [`edit`]: the edit operations the circuit app's v1 needs, as changes to
//!   the tree.
//!
//! The scoping brief and what is built are in `docs/lanes/7b-circuit.md`.

pub mod conn;
pub mod connectivity;
pub mod edit;
pub mod model;
pub mod netlist;
pub mod project;
pub mod project_file;
pub mod sexpr;

pub use connectivity::{Connectivity, compute};
pub use project::Project;

/// Open a project and compute its KiCad netlist text in one call.
pub fn netlist_text(path: &std::path::Path) -> Result<String, project::ProjectError> {
    let project = Project::open(path)?;
    let conn = compute(&project);
    Ok(netlist::write_kicad(&netlist::build(&project, &conn)))
}
