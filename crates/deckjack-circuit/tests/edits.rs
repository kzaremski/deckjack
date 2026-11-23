//! Edit operations, round-tripped through the file: apply, save, reopen from
//! disk, and check both the bytes (only the intended change) and the result
//! (the netlist shows it). One project per format era, KiCad 6 development
//! files through KiCad 10.
//!
//! When `kicad-cli` is installed, each edited project is also exported by
//! KiCad itself and compared with this engine's netlist, which shows KiCad
//! parses the edited files and agrees on what they mean.

mod common;

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use deckjack_circuit::edit::{self, EditError, Flag, LabelScope, Op, PinRef};
use deckjack_circuit::model::Pt;
use deckjack_circuit::netlist::{self, Netlist};
use deckjack_circuit::{Project, compute};

/// One fixture per format era: (fixture, schematic version).
const ERAS: [(&str, u32); 7] = [
    ("complex_hierarchy", 20200512),
    ("legacy_power", 20211123),
    ("issue14494", 20230121),
    ("issue16915", 20231120),
    ("multinetclasses", 20240716),
    ("jumpers", 20250227),
    ("issue24220", 20260306),
];

struct Work {
    _tmp: tempfile::TempDir,
    root: PathBuf,
}

fn work(name: &str) -> Work {
    let fx = common::fixtures()
        .into_iter()
        .find(|f| f.name == name)
        .expect("fixture");
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join(name);
    common::copy_dir(&fx.dir, &dir);
    let root = dir.join(fx.root.file_name().unwrap());
    Work { _tmp: tmp, root }
}

/// A working copy with every reference annotated first, so a test's
/// baseline has no `?` references (their net names change on annotation).
fn prepared(name: &str) -> Work {
    let w = work(name);
    let mut project = Project::open(&w.root).unwrap();
    edit::apply(&mut project, &Op::Annotate { reset: false }).unwrap();
    save(&project);
    w
}

fn save(project: &Project) {
    for (rel, text) in project.changed_files() {
        std::fs::write(project.dir.join(rel), text).unwrap();
    }
}

fn netlist_of(project: &Project) -> Netlist {
    netlist::build(project, &compute(project))
}

/// `(ref, pin) -> net name`.
fn pin_nets(nl: &Netlist) -> BTreeMap<(String, String), String> {
    nl.nets
        .iter()
        .flat_map(|n| {
            n.nodes
                .iter()
                .map(move |nd| ((nd.reference.clone(), nd.pin.clone()), n.name.clone()))
        })
        .collect()
}

/// Lines of `b` not in `a` and of `a` not in `b`, as multisets.
fn line_changes(a: &str, b: &str) -> (usize, usize) {
    let count = |s: &str| {
        let mut m: BTreeMap<String, i64> = BTreeMap::new();
        for l in s.lines() {
            *m.entry(l.to_string()).or_default() += 1;
        }
        m
    };
    let (ma, mb) = (count(a), count(b));
    let keys: BTreeSet<&String> = ma.keys().chain(mb.keys()).collect();
    let (mut removed, mut added) = (0, 0);
    for k in keys {
        let d = mb.get(k).copied().unwrap_or(0) - ma.get(k).copied().unwrap_or(0);
        if d > 0 {
            added += d as usize;
        } else {
            removed += (-d) as usize;
        }
    }
    (removed, added)
}

/// The first annotated, non-power symbol in the root sheet whose library
/// symbol has at least two pins, as (uuid, reference, cache key).
fn target(project: &Project) -> (String, String, String) {
    let f = &project.files[&project.root_file];
    for sym in &f.model.symbols {
        let r = project.resolve_instance(0, sym);
        let Some(lib) = f.model.lib_symbol_for(sym) else {
            continue;
        };
        // Not a power symbol, and not a jumper (whose pins are joined inside
        // the part, so two labels on it would short two nets).
        let jumper = lib.duplicate_pin_numbers_are_jumpers || !lib.jumper_pin_groups.is_empty();
        if r.reference.starts_with('#')
            || r.reference.ends_with('?')
            || lib.power.is_some()
            || jumper
        {
            continue;
        }
        if f.model.placed_pins(sym).len() >= 2 {
            return (sym.uuid.clone(), r.reference, sym.cache_key().to_string());
        }
    }
    panic!("no target symbol in {}", project.root_file);
}

fn kicad_cli() -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|d| d.join("kicad-cli"))
        .find(|p| p.is_file())
}

/// With KiCad installed, its netlist of the edited project must equal ours.
fn check_with_kicad(root: &Path, ours: &Netlist) {
    let Some(cli) = kicad_cli() else {
        eprintln!("SKIPPED kicad-cli check: KiCad is not installed");
        return;
    };
    let tmp = tempfile::tempdir().unwrap();
    let out = tmp.path().join("out.net");
    let o = std::process::Command::new(cli)
        .env("HOME", tmp.path())
        .args(["sch", "export", "netlist", "-o"])
        .arg(&out)
        .arg(root)
        .output()
        .unwrap();
    assert!(
        o.status.success() && out.is_file(),
        "kicad-cli failed on the edited project: {}",
        String::from_utf8_lossy(&o.stderr)
    );
    let golden = netlist::parse_kicad(&std::fs::read(&out).unwrap()).unwrap();
    let p = netlist::compare(&golden, ours);
    assert!(
        p.matches() && p.nets_identical(),
        "kicad-cli disagrees after the edit: {:?} {:?}",
        p.missing_components,
        p.mismatches
    );
}

#[test]
fn set_field_value_is_a_one_line_change() {
    for (name, version) in ERAS {
        let w = prepared(name);
        let mut project = Project::open(&w.root).unwrap();
        assert_eq!(
            project.files[&project.root_file].model.version, version,
            "{name}"
        );
        let (uuid, reference, _) = target(&project);
        let before = project.files[&project.root_file].to_text();
        let file = project.root_file.clone();
        edit::apply(
            &mut project,
            &Op::SetField {
                file: file.clone(),
                symbol: uuid.clone(),
                name: "Value".into(),
                value: "4k7 \"q\"".into(),
            },
        )
        .unwrap();
        let after = project.files[&file].to_text();
        let (removed, added) = line_changes(&before, &after);
        // One line for the field; KiCad 6 files may also carry the value in
        // the root's symbol_instances table (one more line).
        assert!(
            removed == added && (1..=2).contains(&removed),
            "{name}: {removed} removed, {added} added"
        );
        save(&project);
        let reopened = Project::open(&w.root).unwrap();
        assert_eq!(
            reopened.files[&file].to_text(),
            after,
            "{name}: the saved file reads back unchanged"
        );
        let nl = netlist_of(&reopened);
        let comp = nl
            .components
            .iter()
            .find(|c| c.reference == reference)
            .unwrap();
        assert_eq!(comp.value, "4k7 \"q\"", "{name}");
        check_with_kicad(&w.root, &nl);
    }
}

#[test]
fn add_and_remove_a_field_and_set_dnp() {
    for (name, version) in ERAS {
        let w = prepared(name);
        let mut project = Project::open(&w.root).unwrap();
        let (uuid, reference, _) = target(&project);
        let file = project.root_file.clone();
        let original = project.files[&file].to_text();
        edit::apply(
            &mut project,
            &Op::SetField {
                file: file.clone(),
                symbol: uuid.clone(),
                name: "DJ_TEST_FIELD".into(),
                value: "RC0603".into(),
            },
        )
        .unwrap();
        let sym = project.files[&file]
            .model
            .symbols
            .iter()
            .find(|s| s.uuid == uuid)
            .unwrap();
        assert_eq!(sym.field("DJ_TEST_FIELD"), Some("RC0603"), "{name}");
        edit::apply(
            &mut project,
            &Op::RemoveField {
                file: file.clone(),
                symbol: uuid.clone(),
                name: "DJ_TEST_FIELD".into(),
            },
        )
        .unwrap();
        let now = project.files[&file].to_text();
        if now != original {
            let (a, b) = common::changed_region(&original, &now);
            panic!("{name}: add then remove is not a no-op: -{a:?} +{b:?}");
        }
        let dnp = edit::apply(
            &mut project,
            &Op::SetFlag {
                file: file.clone(),
                symbol: uuid.clone(),
                flag: Flag::Dnp,
                value: true,
            },
        );
        if version < 20221206 {
            assert!(
                matches!(dnp, Err(EditError::NoDnpInFormat(_))),
                "{name}: {dnp:?}"
            );
            assert_eq!(
                project.files[&file].to_text(),
                original,
                "{name}: a refused edit changes nothing"
            );
            continue;
        }
        dnp.unwrap();
        save(&project);
        let reopened = Project::open(&w.root).unwrap();
        let nl = netlist_of(&reopened);
        let comp = nl
            .components
            .iter()
            .find(|c| c.reference == reference)
            .unwrap();
        assert!(
            comp.properties
                .iter()
                .any(|(n, v)| n == "dnp" && v.is_none()),
            "{name}: {:?}",
            comp.properties
        );
        check_with_kicad(&w.root, &nl);
        // An added field, saved, as KiCad reads it.
        let mut project = Project::open(&w.root).unwrap();
        edit::apply(
            &mut project,
            &Op::SetField {
                file: file.clone(),
                symbol: uuid.clone(),
                name: "DJ_TEST_FIELD".into(),
                value: "RC0603".into(),
            },
        )
        .unwrap();
        save(&project);
        let reopened = Project::open(&w.root).unwrap();
        let nl = netlist_of(&reopened);
        let comp = nl
            .components
            .iter()
            .find(|c| c.reference == reference)
            .unwrap();
        assert!(
            comp.properties
                .iter()
                .any(|(n, v)| n == "DJ_TEST_FIELD" && v.as_deref() == Some("RC0603")),
            "{name}: {:?}",
            comp.properties
        );
        check_with_kicad(&w.root, &nl);
    }
}

/// The brief's exit flow on the model: place a part from the cache, connect
/// it by label to a local and a global net, annotate, save; the netlist
/// gains exactly that and loses nothing.
#[test]
fn place_connect_annotate() {
    for (name, _) in ERAS {
        let w = prepared(name);
        let mut project = Project::open(&w.root).unwrap();
        let before_nl = netlist_of(&project);
        let (_, _, lib) = target(&project);
        let file = project.root_file.clone();
        let original = project.files[&file].to_text();
        let place = edit::apply(
            &mut project,
            &Op::PlaceSymbol {
                file: file.clone(),
                lib: lib.clone(),
                at: Pt::new(2_540_000, 2_540_000),
                rotation: 0,
                unit: 1,
            },
        )
        .unwrap();
        let new_uuid = place.created[0].clone();
        let sym = project.files[&file]
            .model
            .symbols
            .iter()
            .find(|s| s.uuid == new_uuid)
            .unwrap()
            .clone();
        let pins: Vec<String> = project.files[&file]
            .model
            .placed_pins(&sym)
            .into_iter()
            .map(|p| p.pin.number)
            .collect();
        let (p1, p2) = (pins[0].clone(), pins[1].clone());
        edit::apply_all(
            &mut project,
            &[
                Op::ConnectPinToNet {
                    file: file.clone(),
                    pin: PinRef {
                        symbol: new_uuid.clone(),
                        pin: p1.clone(),
                    },
                    net: "DJ_LOCAL".into(),
                    scope: LabelScope::Local,
                    stub: 0,
                },
                Op::ConnectPinToNet {
                    file: file.clone(),
                    pin: PinRef {
                        symbol: new_uuid.clone(),
                        pin: p2.clone(),
                    },
                    net: "DJ_GLOBAL".into(),
                    scope: LabelScope::Global,
                    stub: 0,
                },
                Op::Annotate { reset: false },
            ],
        )
        .unwrap();
        // Only additions in the edited sheet, apart from annotation of any
        // references the fixture left unannotated, and KiCad 6's tables.
        let after = project.files[&file].to_text();
        let (removed, added) = line_changes(&original, &after);
        assert!(added > 3, "{name}");
        assert!(removed <= 4, "{name}: {removed} lines removed");
        save(&project);
        let reopened = Project::open(&w.root).unwrap();
        assert!(
            reopened.warnings.is_empty(),
            "{name}: {:?}",
            reopened.warnings
        );
        let sym = reopened.files[&file]
            .model
            .symbols
            .iter()
            .find(|s| s.uuid == new_uuid)
            .unwrap();
        let reference = reopened.resolve_instance(0, sym).reference;
        assert!(
            !reference.ends_with('?'),
            "{name}: annotated, got {reference}"
        );
        let nl = netlist_of(&reopened);
        let nets = pin_nets(&nl);
        assert_eq!(
            nets.get(&(reference.clone(), p1)).map(String::as_str),
            Some("/DJ_LOCAL"),
            "{name}"
        );
        assert_eq!(
            nets.get(&(reference.clone(), p2)).map(String::as_str),
            Some("DJ_GLOBAL"),
            "{name}"
        );
        // Nothing that was connected before changed nets.
        let old = pin_nets(&before_nl);
        for (k, v) in &old {
            if k.0.ends_with('?') {
                continue;
            }
            assert_eq!(nets.get(k), Some(v), "{name}: {k:?} moved");
        }
        assert_eq!(
            nl.components.len(),
            before_nl.components.len() + 1,
            "{name}"
        );
        check_with_kicad(&w.root, &nl);
    }
}

#[test]
fn copy_and_wire_two_pins_and_no_connect() {
    for (name, _) in ERAS {
        let w = prepared(name);
        let mut project = Project::open(&w.root).unwrap();
        let (uuid, _, _) = target(&project);
        let file = project.root_file.clone();
        let copy = edit::apply(
            &mut project,
            &Op::CopySymbol {
                file: file.clone(),
                symbol: uuid.clone(),
                at: Pt::new(5_080_000, 3_810_000),
            },
        )
        .unwrap();
        let a = copy.created[0].clone();
        let copy2 = edit::apply(
            &mut project,
            &Op::CopySymbol {
                file: file.clone(),
                symbol: uuid.clone(),
                at: Pt::new(6_350_000, 4_445_000),
            },
        )
        .unwrap();
        let b = copy2.created[0].clone();
        let m = &project.files[&file].model;
        let pa = m.placed_pins(m.symbols.iter().find(|s| s.uuid == a).unwrap());
        let pb = m.placed_pins(m.symbols.iter().find(|s| s.uuid == b).unwrap());
        let nc_at = pb[1].at;
        edit::apply_all(
            &mut project,
            &[
                Op::ConnectPins {
                    file: file.clone(),
                    a: PinRef {
                        symbol: a.clone(),
                        pin: pa[0].pin.number.clone(),
                    },
                    b: PinRef {
                        symbol: b.clone(),
                        pin: pb[0].pin.number.clone(),
                    },
                },
                Op::AddNoConnect {
                    file: file.clone(),
                    at: nc_at,
                },
                Op::Annotate { reset: false },
            ],
        )
        .unwrap();
        save(&project);
        let reopened = Project::open(&w.root).unwrap();
        let conn = compute(&reopened);
        let find = |u: &str, n: &str| {
            let s = conn.symbols.iter().position(|s| s.uuid == u).unwrap();
            let p = conn.symbols[s]
                .pins
                .iter()
                .copied()
                .find(|&p| conn.pins[p].number == n)
                .unwrap();
            conn.pin_net[&p]
        };
        assert_eq!(
            find(&a, &pa[0].pin.number),
            find(&b, &pb[0].pin.number),
            "{name}: the wire joins the pins"
        );
        let nc_net = &conn.nets[find(&b, &pb[1].pin.number)];
        assert!(nc_net.has_no_connect, "{name}: the flag is on the pin");
        check_with_kicad(&w.root, &netlist_of(&reopened));
    }
}

#[test]
fn refusals_change_nothing() {
    let w = work("issue24220");
    let mut project = Project::open(&w.root).unwrap();
    let (uuid, _, _) = target(&project);
    let file = project.root_file.clone();
    let original = project.files[&file].to_text();
    let cases = [
        Op::RemoveField {
            file: file.clone(),
            symbol: uuid.clone(),
            name: "Value".into(),
        },
        Op::SetField {
            file: file.clone(),
            symbol: uuid.clone(),
            name: "Reference".into(),
            value: "R9".into(),
        },
        Op::SetField {
            file: file.clone(),
            symbol: "nope".into(),
            name: "Value".into(),
            value: "x".into(),
        },
        Op::SetField {
            file: "missing.kicad_sch".into(),
            symbol: uuid.clone(),
            name: "Value".into(),
            value: "x".into(),
        },
        Op::PlaceSymbol {
            file: file.clone(),
            lib: "No:Such".into(),
            at: Pt::default(),
            rotation: 0,
            unit: 1,
        },
        Op::AddWire {
            file: file.clone(),
            points: vec![Pt::new(1, 1), Pt::new(1, 1)],
        },
        Op::ConnectPinToNet {
            file: file.clone(),
            pin: PinRef {
                symbol: uuid.clone(),
                pin: "999".into(),
            },
            net: "X".into(),
            scope: LabelScope::Local,
            stub: 0,
        },
    ];
    for op in &cases {
        assert!(
            edit::apply(&mut project, op).is_err(),
            "{op:?} should be refused"
        );
        assert_eq!(project.files[&file].to_text(), original);
    }
    // A batch with a bad op in the middle leaves nothing behind.
    let batch = [
        Op::SetField {
            file: file.clone(),
            symbol: uuid.clone(),
            name: "Value".into(),
            value: "ok".into(),
        },
        cases[0].clone(),
    ];
    assert!(edit::apply_all(&mut project, &batch).is_err());
    assert!(project.changed_files().is_empty());
}

#[test]
fn newer_files_are_read_only() {
    let w = work("issue24220");
    let text = std::fs::read_to_string(&w.root).unwrap().replacen(
        "(version 20260306)",
        "(version 20991231)",
        1,
    );
    std::fs::write(&w.root, text).unwrap();
    let mut project = Project::open(&w.root).unwrap();
    assert!(project.files[&project.root_file].is_newer_than_known());
    let (uuid, _, _) = target(&project);
    let file = project.root_file.clone();
    let r = edit::apply(
        &mut project,
        &Op::SetField {
            file,
            symbol: uuid,
            name: "Value".into(),
            value: "x".into(),
        },
    );
    assert!(matches!(r, Err(EditError::ReadOnly { .. })), "{r:?}");
    assert!(matches!(
        edit::apply(&mut project, &Op::Annotate { reset: false }),
        Err(EditError::ReadOnly { .. })
    ));
}

#[test]
fn annotate_fills_unannotated_references() {
    for name in [
        "test_multiunit_reannotate_4",
        "issue16915",
        "complex_hierarchy_shared",
    ] {
        let w = work(name);
        let mut project = Project::open(&w.root).unwrap();
        let out = edit::apply(&mut project, &Op::Annotate { reset: false }).unwrap();
        assert!(!out.renamed.is_empty(), "{name}");
        save(&project);
        let reopened = Project::open(&w.root).unwrap();
        let conn = compute(&reopened);
        let unannotated: Vec<&str> = conn
            .symbols
            .iter()
            .map(|s| s.reference.as_str())
            .filter(|r| r.ends_with('?'))
            .collect();
        assert!(unannotated.is_empty(), "{name}: still {unannotated:?}");
        check_with_kicad(&w.root, &netlist_of(&reopened));
        // Annotating twice does nothing more.
        let mut again = Project::open(&w.root).unwrap();
        assert!(
            edit::apply(&mut again, &Op::Annotate { reset: false })
                .unwrap()
                .renamed
                .is_empty(),
            "{name}"
        );
        assert!(again.changed_files().is_empty(), "{name}");
    }
}

/// The symbol node with this UUID in a loaded file, for a test to corrupt.
fn symbol_node<'a>(
    project: &'a mut Project,
    file: &str,
    uuid: &str,
) -> &'a mut deckjack_circuit::sexpr::List {
    project
        .files
        .get_mut(file)
        .unwrap()
        .doc
        .root
        .items
        .iter_mut()
        .filter_map(|n| n.as_list_mut())
        .find(|l| l.is("symbol") && l.value_of("uuid").as_deref() == Some(uuid))
        .unwrap()
}

/// KiCad loads a symbol only at a right angle; anything else is refused,
/// and a negative right angle is written as its positive equivalent.
#[test]
fn place_symbol_rotation_must_be_a_right_angle() {
    let w = prepared("issue16915");
    let mut project = Project::open(&w.root).unwrap();
    let (_, _, lib) = target(&project);
    let file = project.root_file.clone();
    let original = project.files[&file].to_text();
    let place = |rotation| Op::PlaceSymbol {
        file: file.clone(),
        lib: lib.clone(),
        at: Pt::new(50_800_000, 50_800_000),
        rotation,
        unit: 1,
    };
    for bad in [45, 1, -45, 359] {
        let r = edit::apply(&mut project, &place(bad));
        assert!(
            matches!(r, Err(EditError::BadRotation(b)) if b == bad),
            "{bad}: {r:?}"
        );
        assert_eq!(project.files[&file].to_text(), original);
    }
    let out = edit::apply(&mut project, &place(-90)).unwrap();
    let sym = project.files[&file]
        .model
        .symbols
        .iter()
        .find(|s| s.uuid == out.created[0])
        .unwrap();
    assert_eq!(sym.rotation, 270);
    save(&project);
    check_with_kicad(&w.root, &netlist_of(&Project::open(&w.root).unwrap()));
}

/// A placement takes the default body style, not the De Morgan style of the
/// symbol it clones its layout from.
#[test]
fn place_symbol_does_not_copy_a_de_morgan_body_style() {
    for (name, version) in [("issue16915", 20231120), ("issue24220", 20260306)] {
        let w = prepared(name);
        let mut project = Project::open(&w.root).unwrap();
        let (uuid, _, lib) = target(&project);
        let file = project.root_file.clone();
        assert_eq!(project.files[&file].model.version, version);
        // Put the clone source in its De Morgan form, under both spellings.
        let node = symbol_node(&mut project, &file, &uuid);
        let unit = node
            .items
            .iter()
            .position(|n| n.as_list().is_some_and(|l| l.is("unit")))
            .unwrap();
        for head in ["convert", "body_style"] {
            let mut l = deckjack_circuit::sexpr::parse_list(&format!("({head} 2)")).unwrap();
            l.lead = " ".into();
            node.items
                .insert(unit + 1, deckjack_circuit::sexpr::Node::List(l));
        }
        let out = edit::apply(
            &mut project,
            &Op::PlaceSymbol {
                file: file.clone(),
                lib: lib.clone(),
                at: Pt::new(50_800_000, 50_800_000),
                rotation: 0,
                unit: 1,
            },
        )
        .unwrap();
        let placed = symbol_node(&mut project, &file, &out.created[0]);
        for head in ["convert", "body_style"] {
            assert_eq!(placed.value_of(head).as_deref(), Some("1"), "{name} {head}");
        }
        let sym = project.files[&file]
            .model
            .symbols
            .iter()
            .find(|s| s.uuid == out.created[0])
            .unwrap();
        assert_eq!(sym.body_style, 1, "{name}");
        assert!(
            project.files[&file].model.placed_pins(sym).len() >= 2,
            "{name}: the placed symbol has its pins"
        );
    }
}

/// A clone source whose `instances` block lacks a `project` or a `path`
/// (malformed, but it loads) is an error-free edit, not a panic.
#[test]
fn malformed_instances_blocks_do_not_panic() {
    for broken in ["(instances)", "(instances (project \"p\"))"] {
        let w = prepared("issue16915");
        let mut project = Project::open(&w.root).unwrap();
        let (uuid, _, lib) = target(&project);
        let file = project.root_file.clone();
        let node = symbol_node(&mut project, &file, &uuid);
        let i = node
            .items
            .iter()
            .position(|n| n.as_list().is_some_and(|l| l.is("instances")))
            .unwrap();
        let mut repl = deckjack_circuit::sexpr::parse_list(broken).unwrap();
        repl.lead = node.items[i].lead().to_string();
        node.items[i] = deckjack_circuit::sexpr::Node::List(repl);
        for op in [
            Op::CopySymbol {
                file: file.clone(),
                symbol: uuid.clone(),
                at: Pt::new(50_800_000, 50_800_000),
            },
            Op::PlaceSymbol {
                file: file.clone(),
                lib: lib.clone(),
                at: Pt::new(76_200_000, 50_800_000),
                rotation: 0,
                unit: 1,
            },
        ] {
            let out = edit::apply(&mut project, &op).unwrap_or_else(|e| panic!("{broken}: {e}"));
            let placed = symbol_node(&mut project, &file, &out.created[0]);
            let path = placed
                .find("instances")
                .and_then(|i| i.find("project"))
                .and_then(|p| p.find("path"));
            assert!(
                path.is_some(),
                "{broken}: the new symbol has an instance path"
            );
        }
    }
}

/// A new field is hidden the way its file's format hides fields: a bare
/// `hide` in `effects` before KiCad 8, `(hide yes)` in `effects` in KiCad 8
/// (whose parser refuses it on the property), and `(hide yes)` on the
/// property from KiCad 9's format on.
#[test]
fn a_new_field_is_hidden_in_the_files_own_syntax() {
    for (name, version) in ERAS {
        let w = prepared(name);
        let mut project = Project::open(&w.root).unwrap();
        let (uuid, _, _) = target(&project);
        let file = project.root_file.clone();
        edit::apply(
            &mut project,
            &Op::SetField {
                file: file.clone(),
                symbol: uuid.clone(),
                name: "MPN".into(),
                value: "ABC123".into(),
            },
        )
        .unwrap();
        let sym = symbol_node(&mut project, &file, &uuid);
        let prop = sym
            .lists()
            .find(|l| l.is("property") && l.atom(1).as_deref() == Some("MPN"))
            .unwrap();
        let effects = prop.find("effects").unwrap();
        let on_property = prop.find("hide").is_some() || prop.has_flag("hide");
        let in_effects_list = effects.find("hide").is_some();
        let in_effects_bare = effects.has_flag("hide");
        let expected = if version >= 20241004 {
            (true, false, false)
        } else if version >= 20231120 {
            (false, true, false)
        } else {
            (false, false, true)
        };
        assert_eq!(
            (on_property, in_effects_list, in_effects_bare),
            expected,
            "{name} ({version}): {prop:?}"
        );
        save(&project);
        let reopened = Project::open(&w.root).unwrap();
        let sym = reopened.files[&file]
            .model
            .symbols
            .iter()
            .find(|s| s.uuid == uuid)
            .unwrap();
        assert_eq!(sym.field("MPN"), Some("ABC123"), "{name}");
        check_with_kicad(&w.root, &netlist_of(&reopened));
    }
}

/// `(sheet path, reference, unit)` of every placed symbol instance.
fn instances(project: &Project) -> Vec<(usize, String, String, u32, Pt)> {
    let conn = compute(project);
    conn.symbols
        .iter()
        .map(|s| {
            let sym = &project.files[&project.sheets[s.sheet].file].model.symbols[s.symbol];
            (s.sheet, s.uuid.clone(), s.reference.clone(), s.unit, sym.at)
        })
        .collect()
}

/// `Annotate { reset: true }` renumbers every part, and the units of a
/// multi-unit part that shared a number still share one: the BOM keeps its
/// package count.
#[test]
fn annotate_reset_keeps_multi_unit_packages_together() {
    for name in ["test_multiunit_reannotate", "test_multiunit_reannotate_2"] {
        let w = work(name);
        let mut project = Project::open(&w.root).unwrap();
        let before = instances(&project);
        let before_comps = netlist_of(&project).components.len();
        edit::apply(&mut project, &Op::Annotate { reset: true }).unwrap();
        save(&project);
        let reopened = Project::open(&w.root).unwrap();
        let after = instances(&reopened);
        let new_ref: BTreeMap<(usize, String), String> = after
            .iter()
            .map(|(s, u, r, _, _)| ((*s, u.clone()), r.clone()))
            .collect();
        // Old reference to the new references its units got.
        let mut groups: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        for (s, u, r, _, _) in &before {
            if !r.ends_with('?') {
                groups
                    .entry(r.clone())
                    .or_default()
                    .insert(new_ref[&(*s, u.clone())].clone());
            }
        }
        for (old, new) in &groups {
            assert_eq!(new.len(), 1, "{name}: the units of {old} became {new:?}");
        }
        let distinct: BTreeSet<&String> = groups.values().flatten().collect();
        assert_eq!(distinct.len(), groups.len(), "{name}: {groups:?}");
        assert_eq!(
            netlist_of(&reopened).components.len(),
            before_comps,
            "{name}: the package count changed"
        );
        check_with_kicad(&w.root, &netlist_of(&reopened));
    }
}

/// What annotation promises, checked on its result: references unique per
/// symbol instance (the units of one part aside), each instance of a shared
/// sheet numbered separately, and per prefix the first free numbers handed
/// out in hierarchy order, then X, then Y.
#[test]
fn annotate_numbers_in_order_and_per_instance() {
    for (name, reset) in [
        ("complex_hierarchy_shared", true),
        ("complex_hierarchy_shared", false),
        ("test_multiunit_reannotate_4", false),
        ("issue16915", true),
    ] {
        let w = work(name);
        let mut project = Project::open(&w.root).unwrap();
        edit::apply(&mut project, &Op::Annotate { reset }).unwrap();
        save(&project);
        let reopened = Project::open(&w.root).unwrap();
        let conn = compute(&reopened);
        let at = |s: &deckjack_circuit::connectivity::SymInst| {
            reopened.files[&reopened.sheets[s.sheet].file].model.symbols[s.symbol].at
        };
        // Unique per instance, except the units of one multi-unit part.
        let mut seen: BTreeMap<&str, (usize, String)> = BTreeMap::new();
        for s in &conn.symbols {
            assert!(!s.reference.ends_with('?'), "{name}: {}", s.reference);
            if let Some((count, uuid)) = seen.get(s.reference.as_str()) {
                assert!(
                    s.unit_count > 1 && *count > 1 && *uuid != s.uuid,
                    "{name} (reset {reset}): {} is given twice",
                    s.reference
                );
            }
            seen.insert(&s.reference, (s.unit_count as usize, s.uuid.clone()));
        }
        // A shared sheet's symbol has a different reference per instance.
        let mut by_uuid: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
        let mut count: BTreeMap<&str, usize> = BTreeMap::new();
        for s in &conn.symbols {
            by_uuid.entry(&s.uuid).or_default().insert(&s.reference);
            *count.entry(&s.uuid).or_default() += 1;
        }
        for (uuid, refs) in &by_uuid {
            assert_eq!(refs.len(), count[uuid], "{name}: {uuid} has {refs:?}");
        }
        if name == "complex_hierarchy_shared" {
            assert!(count.values().any(|&c| c > 1), "{name} has a shared sheet");
        }
        if !reset {
            continue;
        }
        // With every number handed out afresh: per prefix, in (sheet, X, Y)
        // order, the numbers first appear as 1, 2, 3, ...
        let mut order: Vec<&deckjack_circuit::connectivity::SymInst> =
            conn.symbols.iter().collect();
        order.sort_by_key(|s| (s.sheet, at(s).x, at(s).y));
        let mut next: BTreeMap<String, u64> = BTreeMap::new();
        let mut given: BTreeSet<String> = BTreeSet::new();
        for s in order {
            if !given.insert(s.reference.clone()) {
                continue;
            }
            let digits = s
                .reference
                .chars()
                .rev()
                .take_while(char::is_ascii_digit)
                .count();
            let (prefix, num) = s.reference.split_at(s.reference.len() - digits);
            let n = next.entry(prefix.to_string()).or_insert(1);
            assert_eq!(
                num.parse::<u64>().unwrap(),
                *n,
                "{name}: {} out of order",
                s.reference
            );
            *n += 1;
        }
        check_with_kicad(&w.root, &netlist_of(&reopened));
    }
}

/// Footprint (and KiCad 6's per-instance copy of it), and the in-BOM and
/// on-board flags, as the netlist and KiCad see them.
#[test]
fn footprint_and_bom_and_board_flags() {
    for (name, version) in ERAS {
        let w = prepared(name);
        let project = Project::open(&w.root).unwrap();
        let (uuid, reference, _) = target(&project);
        let file = project.root_file.clone();
        // A KiCad 6 file's per-instance footprint copy wins over the field
        // when it is not empty; give it a stale one, which the edit must
        // update too.
        let text = std::fs::read_to_string(&w.root).unwrap();
        let mut stale = 0;
        let lines: Vec<String> = text
            .split_inclusive('\n')
            .scan(false, |in_entry, line| {
                if line.contains("(path \"") {
                    *in_entry = line.contains(&format!("/{uuid}\""));
                }
                let mut line = line.to_string();
                if *in_entry && line.contains("(footprint \"\")") {
                    line = line.replace("(footprint \"\")", "(footprint \"Stale:Footprint\")");
                    stale += 1;
                }
                Some(line)
            })
            .collect();
        std::fs::write(&w.root, lines.concat()).unwrap();
        if version < 20221002 && name == "legacy_power" {
            assert!(stale > 0, "{name}: the table has the symbol's entry");
        }
        let mut project = Project::open(&w.root).unwrap();
        if stale > 0 {
            let nl = netlist_of(&project);
            let comp = nl
                .components
                .iter()
                .find(|c| c.reference == reference)
                .unwrap();
            assert_eq!(comp.footprint, "Stale:Footprint", "{name}");
        }
        edit::apply(
            &mut project,
            &Op::SetField {
                file: file.clone(),
                symbol: uuid.clone(),
                name: "Footprint".into(),
                value: "Resistor_SMD:R_0402_1005Metric".into(),
            },
        )
        .unwrap();
        save(&project);
        let reopened = Project::open(&w.root).unwrap();
        let nl = netlist_of(&reopened);
        let comp = nl
            .components
            .iter()
            .find(|c| c.reference == reference)
            .unwrap();
        assert_eq!(comp.footprint, "Resistor_SMD:R_0402_1005Metric", "{name}");
        check_with_kicad(&w.root, &nl);

        let mut project = Project::open(&w.root).unwrap();
        edit::apply(
            &mut project,
            &Op::SetFlag {
                file: file.clone(),
                symbol: uuid.clone(),
                flag: Flag::InBom,
                value: false,
            },
        )
        .unwrap();
        save(&project);
        let reopened = Project::open(&w.root).unwrap();
        let nl = netlist_of(&reopened);
        let comp = nl
            .components
            .iter()
            .find(|c| c.reference == reference)
            .unwrap();
        assert!(
            comp.properties
                .iter()
                .any(|(n, v)| n == "exclude_from_bom" && v.is_none()),
            "{name} ({version}): {:?}",
            comp.properties
        );
        check_with_kicad(&w.root, &nl);

        let mut project = Project::open(&w.root).unwrap();
        edit::apply(
            &mut project,
            &Op::SetFlag {
                file: file.clone(),
                symbol: uuid.clone(),
                flag: Flag::OnBoard,
                value: false,
            },
        )
        .unwrap();
        save(&project);
        let reopened = Project::open(&w.root).unwrap();
        let nl = netlist_of(&reopened);
        assert!(
            !nl.components.iter().any(|c| c.reference == reference),
            "{name}: an off-board part is not in the netlist"
        );
        check_with_kicad(&w.root, &nl);
    }
}
