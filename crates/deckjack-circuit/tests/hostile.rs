//! Hostile input: randomized corruption of every fixture, hand-made worst
//! cases, and sheet files that try to leave the project directory. The rule
//! under test is "errors and warnings, never a panic, never a read outside
//! the project".
//!
//! The randomized test is seeded (`DJ_FUZZ_SEED`, `DJ_FUZZ_ROUNDS` to vary
//! it) so a failure reproduces exactly.

mod common;

use std::path::Path;

use common::Rng;
use deckjack_circuit::model::SheetModel;
use deckjack_circuit::project_file::ProjectFile;
use deckjack_circuit::{Project, compute, netlist, sexpr};

fn mutate(rng: &mut Rng, input: &[u8]) -> Vec<u8> {
    let mut v = input.to_vec();
    let rounds = 1 + rng.below(4);
    for _ in 0..rounds {
        let len = v.len().max(1);
        match rng.below(9) {
            0 => {
                // Flip a byte.
                let i = rng.below(len);
                if let Some(b) = v.get_mut(i) {
                    *b ^= 1 << rng.below(8);
                }
            }
            1 => {
                // Delete a range.
                let i = rng.below(len);
                let n = rng.below(64).min(v.len().saturating_sub(i));
                v.drain(i..i + n);
            }
            2 => {
                // Duplicate a range somewhere else.
                let i = rng.below(len);
                let n = rng.below(256).min(v.len().saturating_sub(i));
                let chunk: Vec<u8> = v[i..i + n].to_vec();
                let at = rng.below(v.len() + 1);
                v.splice(at..at, chunk);
            }
            3 => {
                // Insert structure characters.
                let at = rng.below(v.len() + 1);
                let c = b"()\"\\ \n"[rng.below(6)];
                v.insert(at, c);
            }
            4 => {
                // Truncate.
                let at = rng.below(len);
                v.truncate(at);
            }
            5 => {
                // Swap a number for an extreme one.
                let at = rng.below(v.len() + 1);
                let n = [
                    "-1",
                    "1e308",
                    "nan",
                    "99999999999999999999",
                    "0",
                    "-0.00001",
                    "inf",
                ][rng.below(7)];
                v.splice(at..at, n.bytes());
            }
            6 => {
                // Replace a token with a keyword from elsewhere.
                let at = rng.below(v.len() + 1);
                let k = [
                    "(symbol",
                    "(sheet",
                    "(pin",
                    "(at",
                    "(xy",
                    "(uuid",
                    "(instances",
                    "(lib_symbols",
                    "(bus_alias \"a\" (members \"{a}\"))",
                ][rng.below(9)];
                v.splice(at..at, k.bytes());
            }
            7 => {
                // A run of open parentheses.
                let at = rng.below(v.len() + 1);
                let n = 1 + rng.below(400);
                v.splice(at..at, std::iter::repeat_n(b'(', n));
            }
            _ => {
                // Invalid UTF-8.
                let at = rng.below(v.len() + 1);
                v.insert(at, 0xff);
            }
        }
    }
    v
}

/// The `k`th list of a tree in preorder.
fn nth_list<'a>(l: &'a mut sexpr::List, k: &mut usize) -> Option<&'a mut sexpr::List> {
    if *k == 0 {
        return Some(l);
    }
    *k -= 1;
    for item in &mut l.items {
        if let sexpr::Node::List(c) = item
            && let Some(found) = nth_list(c, k)
        {
            return Some(found);
        }
    }
    None
}

/// Structure-preserving corruption: the result always parses, so it reaches
/// the typed model and the connectivity engine with values and shapes they
/// do not expect.
fn mutate_tree(rng: &mut Rng, doc: &mut sexpr::Document) {
    fn count(l: &sexpr::List) -> usize {
        1 + l.lists().map(count).sum::<usize>()
    }
    let tokens = [
        "0",
        "-1",
        "1e9",
        "1e300",
        "NaN",
        "\"\"",
        "\"~\"",
        "\"/\"",
        "\"../../etc/passwd\"",
        "\"{a b}\"",
        "\"D[0..4096]\"",
        "\"X[0..2]{Y}\"",
        "hide",
        "yes",
        "no",
        "local",
        "power_in",
        "no_connect",
        "\"#PWR?\"",
        "\"R?\"",
        "\"[1,3-1000000]\"",
        "\"${VAR}\"",
        "90",
        "45",
        "-270",
        "\"\\\"\"",
    ];
    let rounds = 1 + rng.below(6);
    for _ in 0..rounds {
        let mut k = rng.below(count(&doc.root));
        let target = nth_list(&mut doc.root, &mut k).expect("k is below the count");
        match rng.below(5) {
            0 => {
                // Replace an atom.
                let atoms: Vec<usize> = (0..target.items.len())
                    .filter(|&i| matches!(target.items[i], sexpr::Node::Atom(_)))
                    .collect();
                if let Some(&i) = atoms.get(rng.below(atoms.len().max(1)))
                    && let sexpr::Node::Atom(a) = &mut target.items[i]
                {
                    a.raw = tokens[rng.below(tokens.len())].to_string();
                }
            }
            1 => {
                // Delete a child.
                if target.items.len() > 1 {
                    let i = 1 + rng.below(target.items.len() - 1);
                    target.items.remove(i);
                }
            }
            2 => {
                // Duplicate a child.
                if !target.items.is_empty() {
                    let i = rng.below(target.items.len());
                    let c = target.items[i].clone();
                    target.items.insert(i, c);
                }
            }
            3 => {
                // Swap two children.
                if target.items.len() > 2 {
                    let a = 1 + rng.below(target.items.len() - 1);
                    let b = 1 + rng.below(target.items.len() - 1);
                    target.items.swap(a, b);
                }
            }
            _ => {
                // Append an atom.
                let mut a = sexpr::Atom::bare("x");
                a.raw = tokens[rng.below(tokens.len())].to_string();
                a.lead = " ".into();
                target.items.push(sexpr::Node::Atom(a));
            }
        }
    }
}

#[test]
fn structural_corruption_never_panics() {
    let seed = env_u64("DJ_FUZZ_SEED", 0x0007_eec1_c017);
    let rounds = env_u64("DJ_FUZZ_ROUNDS", 24) as usize;
    let mut rng = Rng(seed | 1);
    let tmp = tempfile::tempdir().unwrap();
    let (mut modeled, mut ran) = (0, 0);
    for path in common::files_with(&["kicad_sch"]) {
        let original = sexpr::parse(&std::fs::read(&path).unwrap()).unwrap();
        for _ in 0..rounds {
            let mut doc = original.clone();
            mutate_tree(&mut rng, &mut doc);
            let text = doc.to_text();
            let Ok(back) = sexpr::parse(text.as_bytes()) else {
                continue;
            };
            assert_eq!(back.to_text(), text);
            if SheetModel::read(&back).is_err() {
                continue;
            }
            modeled += 1;
            // Run it in place of its own file, so hierarchies stay intact.
            let dir = tmp.path().join(format!("r{ran}"));
            common::copy_dir(path.parent().unwrap(), &dir);
            let target = dir.join(path.file_name().unwrap());
            std::fs::write(&target, &text).unwrap();
            if let Ok(project) = Project::open(&target) {
                let conn = compute(&project);
                let _ = netlist::write_kicad(&netlist::build(&project, &conn));
            }
            ran += 1;
            let _ = std::fs::remove_dir_all(&dir);
        }
    }
    eprintln!(
        "seed {seed}: {modeled} corrupted trees read as schematics, {ran} ran the whole pipeline"
    );
    assert!(ran > 100);
}

fn env_u64(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(default)
}

#[test]
fn randomized_corruption_never_panics() {
    let seed = env_u64("DJ_FUZZ_SEED", 0x005e_edc1_c017);
    let rounds = env_u64("DJ_FUZZ_ROUNDS", 24) as usize;
    let mut rng = Rng(seed | 1);
    let tmp = tempfile::tempdir().unwrap();
    let mut parsed = 0;
    let mut projects = 0;
    let schematics = common::files_with(&["kicad_sch"]);
    for path in &schematics {
        let bytes = std::fs::read(path).unwrap();
        for round in 0..rounds {
            let m = mutate(&mut rng, &bytes);
            let Ok(doc) = sexpr::parse(&m) else { continue };
            parsed += 1;
            assert!(
                doc.to_text().as_bytes() == m.as_slice(),
                "seed {seed}: a parsed document must print back as read"
            );
            let Ok(_model) = SheetModel::read(&doc) else {
                continue;
            };
            // Every few rounds, run the whole pipeline on the corrupted sheet
            // as a one-sheet project.
            if round % 4 == 0 {
                let p = tmp.path().join("fuzz.kicad_sch");
                std::fs::write(&p, &m).unwrap();
                if let Ok(project) = Project::open(&p) {
                    let conn = compute(&project);
                    let _ = netlist::write_kicad(&netlist::build(&project, &conn));
                    projects += 1;
                }
            }
        }
    }
    for path in common::files_with(&["kicad_pro", "net"]) {
        let bytes = std::fs::read(&path).unwrap();
        for _ in 0..rounds {
            let m = mutate(&mut rng, &bytes);
            if path.extension().is_some_and(|e| e == "kicad_pro") {
                if let Ok(pf) = ProjectFile::parse(&m) {
                    assert!(pf.text().as_bytes() == m.as_slice());
                }
            } else {
                let _ = netlist::parse_kicad(&m);
            }
        }
    }
    eprintln!(
        "seed {seed}: {parsed} corrupted schematics still parsed, {projects} ran the whole pipeline"
    );
    assert!(
        parsed > 0 && projects > 0,
        "the mutations should leave some inputs parseable"
    );
}

#[test]
fn worst_cases_are_bounded() {
    // Nesting just under and over the limit.
    let ok = "(".repeat(200) + &")".repeat(200);
    assert!(sexpr::parse(ok.as_bytes()).is_ok());
    let deep = "(".repeat(100_000);
    assert!(matches!(
        sexpr::parse(deep.as_bytes()),
        Err(sexpr::Error::TooDeep { .. })
    ));
    // A huge string and a huge atom.
    let big = format!("(a \"{}\" {})", "x".repeat(1 << 20), "y".repeat(1 << 20));
    assert!(sexpr::parse(big.as_bytes()).is_ok());
    // Deeply nested JSON.
    let json = "[".repeat(10_000);
    assert!(ProjectFile::parse(json.as_bytes()).is_err());
}

const ROOT_UUID: &str = "11111111-1111-4111-8111-111111111111";

fn sheet_item(name: &str, file: &str, uuid: &str, page: &str) -> String {
    format!(
        "(sheet (at 10 10) (size 20 20) (uuid \"{uuid}\")
    (property \"Sheetname\" \"{name}\" (at 10 10 0))
    (property \"Sheetfile\" \"{file}\" (at 10 30 0))
    (instances (project \"p\" (path \"/{ROOT_UUID}\" (page \"{page}\"))))
  )"
    )
}

fn schematic(uuid: &str, body: &str) -> String {
    format!(
        "(kicad_sch (version 20231120) (generator \"eeschema\") (uuid \"{uuid}\")\n  (lib_symbols)\n  {body}\n)\n"
    )
}

/// A hostile root whose labels expand without bound if nothing stops them.
#[test]
fn hostile_buses_and_aliases_are_bounded() {
    let tmp = tempfile::tempdir().unwrap();
    let body = "(bus_alias \"a\" (members \"{a}\" \"{b}\"))
  (bus_alias \"b\" (members \"{a}\"))
  (label \"{a}\" (at 0 0 0) (uuid \"21111111-1111-4111-8111-111111111111\"))
  (label \"D[0..999999999]\" (at 10 0 0) (uuid \"31111111-1111-4111-8111-111111111111\"))
  (label \"X{a b c d e f g h}\" (at 20 0 0) (uuid \"41111111-1111-4111-8111-111111111111\"))
  (bus (pts (xy 0 0) (xy 20 0)) (uuid \"51111111-1111-4111-8111-111111111111\"))";
    let p = tmp.path().join("p.kicad_sch");
    std::fs::write(&p, schematic(ROOT_UUID, body)).unwrap();
    let project = Project::open(&p).unwrap();
    let conn = compute(&project);
    let _ = netlist::build(&project, &conn);
}

#[test]
fn sheets_cannot_leave_the_project_directory() {
    let tmp = tempfile::tempdir().unwrap();
    let outside = tmp.path().join("outside.kicad_sch");
    std::fs::write(
        &outside,
        schematic("22222222-2222-4222-8222-222222222222", ""),
    )
    .unwrap();
    let proj = tmp.path().join("proj");
    std::fs::create_dir(&proj).unwrap();
    std::os::unix::fs::symlink(&outside, proj.join("link.kicad_sch")).unwrap();
    std::fs::write(
        proj.join("ok.kicad_sch"),
        schematic("33333333-3333-4333-8333-333333333333", ""),
    )
    .unwrap();
    let body = [
        sheet_item(
            "up",
            "../outside.kicad_sch",
            "a1111111-1111-4111-8111-111111111111",
            "2",
        ),
        sheet_item(
            "abs",
            outside.to_str().unwrap(),
            "a2222222-1111-4111-8111-111111111111",
            "3",
        ),
        sheet_item(
            "link",
            "link.kicad_sch",
            "a3333333-1111-4111-8111-111111111111",
            "4",
        ),
        sheet_item(
            "missing",
            "nope.kicad_sch",
            "a4444444-1111-4111-8111-111111111111",
            "5",
        ),
        sheet_item(
            "self",
            "p.kicad_sch",
            "a5555555-1111-4111-8111-111111111111",
            "6",
        ),
        sheet_item(
            "ok",
            "ok.kicad_sch",
            "a6666666-1111-4111-8111-111111111111",
            "7",
        ),
        // A real NUL byte inside the quoted file name.
        sheet_item("nul", "a\0b", "a7777777-1111-4111-8111-111111111111", "8"),
    ]
    .join("\n  ");
    let root = proj.join("p.kicad_sch");
    std::fs::write(&root, schematic(ROOT_UUID, &body)).unwrap();
    let project = Project::open(&root).unwrap();
    let loaded: Vec<&String> = project.files.keys().collect();
    assert_eq!(
        loaded,
        vec!["ok.kicad_sch", "p.kicad_sch"],
        "warnings: {:?}",
        project.warnings
    );
    assert_eq!(project.sheets.len(), 2);
    // The NUL survives parsing and reaches the file-name check (where
    // `resolve_sheet_file` refuses it; the standard library would refuse
    // the path too, so that guard is defense in depth).
    assert!(
        project.warnings.iter().any(|w| w.contains("\"a\0b\"")),
        "{:?}",
        project.warnings
    );
    for name in ["up", "abs", "link", "missing", "self", "nul"] {
        assert!(
            project
                .warnings
                .iter()
                .any(|w| w.contains(&format!("\"{name}\""))),
            "{name}: {:?}",
            project.warnings
        );
    }
}

#[test]
fn mutual_recursion_is_refused() {
    let tmp = tempfile::tempdir().unwrap();
    let a = schematic(
        ROOT_UUID,
        &sheet_item(
            "b",
            "b.kicad_sch",
            "b1111111-1111-4111-8111-111111111111",
            "2",
        ),
    );
    let b = schematic(
        "44444444-4444-4444-8444-444444444444",
        &sheet_item(
            "a",
            "a.kicad_sch",
            "b2222222-1111-4111-8111-111111111111",
            "3",
        ),
    );
    std::fs::write(tmp.path().join("a.kicad_sch"), a).unwrap();
    std::fs::write(tmp.path().join("b.kicad_sch"), b).unwrap();
    let project = Project::open(&tmp.path().join("a.kicad_sch")).unwrap();
    assert_eq!(project.sheets.len(), 2);
    assert!(
        project
            .warnings
            .iter()
            .any(|w| w.contains("contains itself")),
        "{:?}",
        project.warnings
    );
}

/// A shared sheet placed many times in a chain grows exponentially; the
/// instance cap stops it.
#[test]
fn exponential_hierarchies_are_capped() {
    let tmp = tempfile::tempdir().unwrap();
    let levels = 16;
    for i in 0..levels {
        let next = format!("l{}.kicad_sch", i + 1);
        let body = if i + 1 < levels {
            format!(
                "{}\n  {}",
                sheet_item(
                    "x",
                    &next,
                    &format!("c{i:07}-1111-4111-8111-111111111111"),
                    "2"
                ),
                sheet_item(
                    "y",
                    &next,
                    &format!("d{i:07}-1111-4111-8111-111111111111"),
                    "3"
                )
            )
        } else {
            String::new()
        };
        let uuid = if i == 0 {
            ROOT_UUID.to_string()
        } else {
            format!("e{i:07}-1111-4111-8111-111111111111")
        };
        std::fs::write(
            tmp.path().join(format!("l{i}.kicad_sch")),
            schematic(&uuid, &body),
        )
        .unwrap();
    }
    let project = Project::open(Path::new(&tmp.path().join("l0.kicad_sch"))).unwrap();
    assert!(project.sheets.len() <= deckjack_circuit::project::MAX_SHEET_INSTANCES);
    assert!(
        project
            .warnings
            .iter()
            .any(|w| w.contains("sheet instances")),
        "{:?}",
        project.warnings
    );
    let _ = compute(&project);
}

/// Write `p.kicad_sch` (and optionally `p.kicad_pro`) in a fresh directory,
/// open it and compute its connectivity, failing if that takes longer than
/// `limit`. The limits are generous for a debug build; the inputs are sized
/// so that the quadratic or exponential behavior these tests guard against
/// takes minutes or exhausts memory.
fn compute_within(
    body: &str,
    pro: Option<&str>,
    limit: std::time::Duration,
) -> deckjack_circuit::Connectivity {
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path().join("p.kicad_sch");
    std::fs::write(&p, schematic(ROOT_UUID, body)).unwrap();
    if let Some(pro) = pro {
        std::fs::write(tmp.path().join("p.kicad_pro"), pro).unwrap();
    }
    let start = std::time::Instant::now();
    let project = Project::open(&p).unwrap();
    let conn = compute(&project);
    let _ = netlist::build(&project, &conn);
    let took = start.elapsed();
    assert!(took < limit, "took {took:?}, over {limit:?}");
    conn
}

fn uuid(n: usize) -> String {
    format!("{n:08x}-1111-4111-8111-111111111111")
}

/// A bus alias whose members name the alias itself, six wide: without a
/// stack of the aliases being expanded this grows as 6^9 connections and
/// aborts on allocation failure.
#[test]
fn self_referencing_bus_aliases_are_cut_off() {
    let label = format!("(label \"{{B}}\" (at 0 0 0) (uuid \"{}\"))", uuid(1));
    let limit = std::time::Duration::from_secs(20);
    let body = format!("(bus_alias \"B\" (members \"{{B B B B B B}}\"))\n  {label}");
    let conn = compute_within(&body, None, limit);
    assert!(
        conn.approximate.iter().any(|a| a.contains("cut off")),
        "{:?}",
        conn.approximate
    );
    // The same alias from the project file.
    let pro = r#"{"schematic": {"bus_aliases": {"B": ["{B B B B B B}"]}}}"#;
    let conn = compute_within(&label, Some(pro), limit);
    assert!(
        conn.approximate.iter().any(|a| a.contains("cut off")),
        "{:?}",
        conn.approximate
    );
}

/// One label grouping a thousand 4001-member vectors: each vector is under
/// the per-vector cap, but together they are four million connections per
/// label item.
#[test]
fn a_group_of_many_vectors_is_cut_off() {
    let members: Vec<String> = (1..=1000).map(|i| format!("A{i}[0..4000]")).collect();
    let body = format!(
        "(label \"{{{}}}\" (at 0 0 0) (uuid \"{}\"))\n  (label \"{{{}}}\" (at 10 0 0) (uuid \"{}\"))\n  (bus (pts (xy 0 0) (xy 10 0)) (uuid \"{}\"))",
        members.join(" "),
        uuid(1),
        members.join(" "),
        uuid(2),
        uuid(3)
    );
    let conn = compute_within(&body, None, std::time::Duration::from_secs(60));
    assert!(
        conn.approximate.iter().any(|a| a.contains("cut off")),
        "{:?}",
        conn.approximate
    );
}

/// A chain of files each placing the next twice multiplies the leaf's
/// wires by the instance count; the project-wide item budget stops it.
#[test]
fn sheet_instances_times_sheet_size_is_capped() {
    let tmp = tempfile::tempdir().unwrap();
    let levels = 12;
    let leaf_wires = 1500;
    for i in 0..levels {
        let body = if i + 1 < levels {
            let next = format!("l{}.kicad_sch", i + 1);
            format!(
                "{}\n  {}",
                sheet_item("x", &next, &uuid(0xc000_0000 + i), "2"),
                sheet_item("y", &next, &uuid(0xd000_0000 + i), "3")
            )
        } else {
            (0..leaf_wires)
                .map(|w| {
                    format!(
                        "(wire (pts (xy {w} 0) (xy {w} 1)) (uuid \"{}\"))",
                        uuid(0x1000_0000 + w)
                    )
                })
                .collect::<Vec<_>>()
                .join("\n  ")
        };
        let u = if i == 0 {
            ROOT_UUID.to_string()
        } else {
            uuid(0xe000_0000 + i)
        };
        std::fs::write(
            tmp.path().join(format!("l{i}.kicad_sch")),
            schematic(&u, &body),
        )
        .unwrap();
    }
    let start = std::time::Instant::now();
    let project = Project::open(&tmp.path().join("l0.kicad_sch")).unwrap();
    let conn = compute(&project);
    assert!(
        conn.approximate
            .iter()
            .any(|a| a.contains("pins, items and bus members")),
        "{:?}",
        conn.approximate
    );
    let took = start.elapsed();
    assert!(took < std::time::Duration::from_secs(120), "took {took:?}");
}

/// One library symbol with many pins placed many times: the pin instances
/// count against the project-wide budget, and naming a symbol's pins is
/// linear in its pin count.
#[test]
fn library_pins_times_placements_is_capped() {
    let pins_per_symbol = 5000;
    let placements = 500;
    let pins: String = (0..pins_per_symbol)
        .map(|n| {
            format!(
                "(pin passive line (at 0 {} 0) (length 1) (name \"P\") (number \"{n}\"))",
                n * 2
            )
        })
        .collect::<Vec<_>>()
        .join(" ");
    let lib = format!(
        "(symbol \"L:BIG\" (property \"Reference\" \"U\" (at 0 0 0)) (symbol \"BIG_1_1\" {pins}))"
    );
    let syms: String = (0..placements)
        .map(|s| {
            format!(
                "(symbol (lib_id \"L:BIG\") (at {} 0 0) (unit 1) (uuid \"{}\")
    (property \"Reference\" \"U{s}\" (at 0 0 0)) (property \"Value\" \"BIG\" (at 0 0 0))
    (instances (project \"p\" (path \"/{ROOT_UUID}\" (reference \"U{s}\") (unit 1)))))",
                s * 10,
                uuid(s)
            )
        })
        .collect::<Vec<_>>()
        .join("\n  ");
    let body = syms;
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path().join("p.kicad_sch");
    let text =
        schematic(ROOT_UUID, &body).replace("(lib_symbols)", &format!("(lib_symbols {lib})"));
    std::fs::write(&p, text).unwrap();
    let start = std::time::Instant::now();
    let project = Project::open(&p).unwrap();
    let conn = compute(&project);
    let took = start.elapsed();
    let max = deckjack_circuit::connectivity::MAX_PROJECT_ITEMS;
    assert!(
        conn.pins.len() <= max + pins_per_symbol,
        "{}",
        conn.pins.len()
    );
    assert!(
        conn.approximate
            .iter()
            .any(|a| a.contains("pins, items and bus members")),
        "{:?}",
        conn.approximate
    );
    assert!(took < std::time::Duration::from_secs(60), "took {took:?}");
}

/// Many items at one point, and many labels each on its own wire: both
/// were quadratic (every pair at a point; every label against every line;
/// every strong subgraph against every other on the sheet).
#[test]
fn connectivity_is_not_quadratic_in_items() {
    let n = 20_000;
    let coincident: String = (0..2 * n)
        .map(|i| format!("(wire (pts (xy 0 0) (xy 0 0)) (uuid \"{}\"))", uuid(i)))
        .collect::<Vec<_>>()
        .join("\n  ");
    let limit = std::time::Duration::from_secs(30);
    compute_within(&coincident, None, limit);
    let labeled: String = (0..n)
        .map(|i| {
            format!(
                "(wire (pts (xy {x} 0) (xy {x} 10)) (uuid \"{}\"))\n  (label \"N{i}\" (at {x} 5 0) (uuid \"{}\"))",
                uuid(2 * i),
                uuid(2 * i + 1),
                x = i * 20
            )
        })
        .collect::<Vec<_>>()
        .join("\n  ");
    let conn = compute_within(&labeled, None, limit);
    assert!(conn.nets.len() >= n, "{}", conn.nets.len());
}

/// A sheet or project file far over the size limit is refused from its size,
/// not read whole first. The files are sparse (8 GiB that take no disk), and
/// the check runs in a child of this test binary under `ulimit -v`, so a
/// whole-file read fails with "out of memory" there instead of reaching the
/// size check, and cannot exhaust this machine's memory either.
#[test]
fn oversized_files_are_refused_before_reading() {
    const CHILD: &str = "DJ_OVERSIZED_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let exe = std::env::current_exe().unwrap();
        let out = std::process::Command::new("sh")
            .arg("-c")
            .arg("ulimit -v 2000000 && exec \"$0\" \"$@\"")
            .arg(exe)
            .args([
                "--exact",
                "oversized_files_are_refused_before_reading",
                "--nocapture",
                "--test-threads=1",
            ])
            .env(CHILD, "1")
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "child failed:\n{}\n{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        return;
    }
    let sparse = |path: &Path| {
        std::fs::File::create(path)
            .unwrap()
            .set_len(8 << 30)
            .unwrap();
    };
    let tmp = tempfile::tempdir().unwrap();
    sparse(&tmp.path().join("big.kicad_sch"));
    let root = tmp.path().join("p.kicad_sch");
    std::fs::write(
        &root,
        schematic(
            ROOT_UUID,
            &sheet_item("big", "big.kicad_sch", &uuid(0xb16), "2"),
        ),
    )
    .unwrap();
    let project = Project::open(&root).unwrap();
    assert!(
        project
            .warnings
            .iter()
            .any(|w| w.contains("\"big\"") && w.contains("over the 67108864-byte limit")),
        "{:?}",
        project.warnings
    );
    sparse(&tmp.path().join("p.kicad_pro"));
    match Project::open(&root) {
        Err(deckjack_circuit::project::ProjectError::ProjectFile { source, .. }) => {
            assert!(matches!(
                source,
                deckjack_circuit::project_file::Error::TooLarge(n) if n == 8 << 30
            ));
        }
        other => panic!("expected a too-large project file, got {other:?}"),
    }
}

/// A linear chain of 70 distinct sheet files stops at 64 levels.
#[test]
fn sheet_depth_is_capped() {
    let tmp = tempfile::tempdir().unwrap();
    let levels = 70;
    for i in 0..levels {
        let body = if i + 1 < levels {
            sheet_item(
                "x",
                &format!("l{}.kicad_sch", i + 1),
                &uuid(0xa000_0000 + i),
                "2",
            )
        } else {
            String::new()
        };
        let u = if i == 0 {
            ROOT_UUID.to_string()
        } else {
            uuid(0xf000_0000 + i)
        };
        std::fs::write(
            tmp.path().join(format!("l{i}.kicad_sch")),
            schematic(&u, &body),
        )
        .unwrap();
    }
    let project = Project::open(&tmp.path().join("l0.kicad_sch")).unwrap();
    let deepest = project.sheets.iter().map(|s| s.uuids.len()).max().unwrap();
    assert_eq!(deepest, deckjack_circuit::project::MAX_SHEET_DEPTH);
    assert_eq!(
        project.sheets.len(),
        deckjack_circuit::project::MAX_SHEET_DEPTH + 1
    );
    assert!(
        project
            .warnings
            .iter()
            .any(|w| w.contains("nested deeper than 64 levels")),
        "{:?}",
        project.warnings
    );
    let _ = compute(&project);
}
