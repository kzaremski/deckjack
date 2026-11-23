//! The netlist: built from [`Connectivity`], written in KiCad's S-expression
//! netlist format (`kicad-cli sch export netlist`, format version "E"), read
//! back, and compared.
//!
//! **What is KiCad's and what is approximate.** The `nets` section follows
//! KiCad's exporter exactly in what it includes and how it orders and names
//! things: nets sorted by a natural-number comparison of their names, nodes by
//! reference then pin number, duplicates removed, power and virtual symbols
//! (references starting with `#`) left out, `+no_connect` on single-pin nets
//! with a no-connect flag. The `components` section carries what PCB import
//! reads (reference, value, footprint, library source, properties, sheet path
//! and UUIDs) but is not field-for-field identical to KiCad 10's (no
//! `fields` block, no variants, no component classes, and no `libraries`
//! section, which needs the symbol library tables).
//!
//! [`compare`] is the parity check. It compares what KiCad's own regression
//! test compares (`qa/tests/eeschema/test_netlist_exporter_kicad.cpp`): the
//! component set, and for each component pin its net name, pin function and
//! pin type. It also reports whether the nets match as whole sets.

use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt::Write as _;

use crate::connectivity::{Connectivity, stacked_numbers};
use crate::model::LibSymbol;
use crate::project::Project;
use crate::sexpr::{self, List, quote};

/// KiCad's `StrNumCmp`, quirks included: runs of digits compare as numbers,
/// and after a run the following characters are compared before both sides
/// advance.
pub fn str_num_cmp(a: &str, b: &str, ignore_case: bool) -> Ordering {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let (mut i, mut j) = (0, 0);
    while i < a.len() && j < b.len() {
        let mut c1 = a[i];
        let mut c2 = b[j];
        if c1.is_ascii_digit() && c2.is_ascii_digit() {
            let (mut n1, mut n2): (i64, i64) = (0, 0);
            while i < a.len() && a[i].is_ascii_digit() {
                n1 = n1
                    .saturating_mul(10)
                    .saturating_add(a[i] as i64 - '0' as i64);
                i += 1;
            }
            while j < b.len() && b[j].is_ascii_digit() {
                n2 = n2
                    .saturating_mul(10)
                    .saturating_add(b[j] as i64 - '0' as i64);
                j += 1;
            }
            match n1.cmp(&n2) {
                Ordering::Equal => {}
                o => return o,
            }
            c1 = a.get(i).copied().unwrap_or('\0');
            c2 = b.get(j).copied().unwrap_or('\0');
        }
        let (x, y) = if ignore_case {
            (
                c1.to_uppercase().next().unwrap_or(c1),
                c2.to_uppercase().next().unwrap_or(c2),
            )
        } else {
            (c1, c2)
        };
        match x.cmp(&y) {
            Ordering::Equal => {}
            o => return o,
        }
        if i < a.len() {
            i += 1;
        }
        if j < b.len() {
            j += 1;
        }
    }
    match (i >= a.len(), j >= b.len()) {
        (true, false) => Ordering::Less,
        (false, true) => Ordering::Greater,
        _ => Ordering::Equal,
    }
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Component {
    pub reference: String,
    pub value: String,
    pub footprint: String,
    pub datasheet: String,
    pub description: String,
    pub lib: String,
    pub part: String,
    /// Name and value; a flag property such as `dnp` has no value.
    pub properties: Vec<(String, Option<String>)>,
    pub sheet_names: String,
    pub sheet_tstamps: String,
    pub tstamps: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Default)]
pub struct Node {
    pub reference: String,
    pub pin: String,
    pub pin_function: Option<String>,
    pub pin_type: String,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct NetRecord {
    pub code: String,
    pub name: String,
    pub class: Option<String>,
    pub nodes: Vec<Node>,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct LibPart {
    pub lib: String,
    pub part: String,
    pub description: String,
    pub docs: String,
    pub footprints: Vec<String>,
    pub fields: Vec<(String, String)>,
    pub pins: Vec<(String, String, String)>,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Netlist {
    pub source: String,
    pub tool: String,
    pub sheets: Vec<(String, String, String)>,
    pub components: Vec<Component>,
    pub libparts: Vec<LibPart>,
    pub nets: Vec<NetRecord>,
}

fn split_lib_id(id: &str) -> (String, String) {
    match id.split_once(':') {
        Some((l, p)) => (l.to_string(), p.to_string()),
        None => (String::new(), id.to_string()),
    }
}

const MANDATORY: [&str; 5] = [
    "Reference",
    "Value",
    "Footprint",
    "Datasheet",
    "Description",
];

/// Build the netlist KiCad's exporter would write for a PCB.
/// The name KiCad 10 gives the root sheet, which it writes as a root-sheet
/// component's Sheetname. Inferred from what `kicad-cli` 10.0.7 writes for
/// every fixture, not read from KiCad's code: a file older than the flat
/// hierarchy format (20251012) takes the project's name; a newer one takes
/// the root's entry in the project file's `sheets` table; and either falls
/// back to "Root" (no project file, or an empty entry).
pub fn root_sheet_name(project: &Project) -> String {
    let Some(pf) = &project.project_file else {
        return "Root".into();
    };
    let root = &project.files[&project.root_file].model;
    let name = if root.version < 20251012 {
        Some(project.name.clone())
    } else {
        let uuid = root.uuid.clone().unwrap_or_default();
        match pf.get(&["sheets"]).map(|s| &s.value) {
            Some(crate::project_file::Value::Array(rows)) => rows.iter().find_map(|row| {
                let crate::project_file::Value::Array(cells) = &row.value else {
                    return None;
                };
                (cells.first()?.as_str()? == uuid)
                    .then(|| cells.get(1)?.as_str().map(str::to_string))?
            }),
            _ => None,
        }
    };
    name.filter(|n| !n.is_empty())
        .unwrap_or_else(|| "Root".into())
}

pub fn build(project: &Project, conn: &Connectivity) -> Netlist {
    let mut nl = Netlist {
        source: project.root_file.clone(),
        tool: format!("deckjack-circuit {}", env!("CARGO_PKG_VERSION")),
        ..Default::default()
    };
    for (i, sp) in project.sheets.iter().enumerate() {
        let _ = i;
        nl.sheets.push((sp.page.clone(), sp.human(), sp.tstamps()));
    }

    // Components: per sheet in order, by reference, one per reference.
    let mut seen_refs: BTreeSet<String> = BTreeSet::new();
    let mut used_libs: BTreeMap<String, LibSymbol> = BTreeMap::new();
    // Symbols by sheet instance, and by (sheet instance, reference), in
    // symbol order: looked up per sheet and per component rather than
    // scanned, which was quadratic in the symbol count.
    let mut by_sheet: Vec<Vec<usize>> = vec![Vec::new(); project.sheets.len()];
    let mut by_ref: HashMap<(usize, &str), Vec<usize>> = HashMap::new();
    for (i, s) in conn.symbols.iter().enumerate() {
        if let Some(v) = by_sheet.get_mut(s.sheet) {
            v.push(i);
        }
        by_ref
            .entry((s.sheet, s.reference.as_str()))
            .or_default()
            .push(i);
    }
    for (si, sp) in project.sheets.iter().enumerate() {
        let file = &project.files[&sp.file];
        let mut syms: Vec<usize> = by_sheet[si].clone();
        // Several units of one reference on a sheet: keep the lowest UUID.
        syms.sort_by(|&a, &b| {
            let (sa, sb) = (&conn.symbols[a], &conn.symbols[b]);
            str_num_cmp(&sa.reference, &sb.reference, true).then_with(|| sa.uuid.cmp(&sb.uuid))
        });
        syms.dedup_by(|b, a| {
            str_num_cmp(
                &conn.symbols[*a].reference,
                &conn.symbols[*b].reference,
                true,
            ) == Ordering::Equal
        });
        for s in syms {
            let si_ = &conn.symbols[s];
            if si_.reference.starts_with('#') {
                continue;
            }
            let Some(lib) = si_.lib.as_ref() else {
                continue;
            };
            if lib.unit_count() > 1 && !seen_refs.insert(si_.reference.clone()) {
                continue;
            }
            if sp.excluded_from_board || !si_.on_board {
                continue;
            }
            let sym = &file.model.symbols[si_.symbol];
            let (lib_name, part) = split_lib_id(&sym.lib_id);
            used_libs
                .entry(sym.lib_id.clone())
                .or_insert_with(|| (**lib).clone());
            let mut properties: Vec<(String, Option<String>)> = sym
                .fields
                .iter()
                .filter(|f| !MANDATORY.contains(&f.name.as_str()) && !f.name.starts_with("ki_"))
                .map(|f| (f.name.clone(), Some(f.value.clone())))
                .collect();
            let sheet_name = sp
                .names
                .last()
                .cloned()
                .unwrap_or_else(|| root_sheet_name(project));
            properties.push(("Sheetname".into(), Some(sheet_name)));
            // The sheet item that places this sheet instance, if any.
            let placing = sp.parent.zip(sp.sheet_item).and_then(|(parent, item)| {
                project
                    .sheets
                    .get(parent)
                    .and_then(|pp| project.files.get(&pp.file))
                    .and_then(|f| f.model.sheets.get(item))
            });
            // KiCad writes the file name as that sheet item names it
            // (relative to the file placing it), the root's as it is.
            let sheet_file = placing.map_or_else(|| sp.file.clone(), |s| s.file.clone());
            properties.push(("Sheetfile".into(), Some(sheet_file)));
            // Then the sheet item's user fields, which KiCad 10 copies onto
            // each component of the instance.
            if let Some(sheet) = placing {
                properties.extend(
                    sheet
                        .fields
                        .iter()
                        .filter(|f| {
                            !matches!(
                                f.name.as_str(),
                                "Sheetname" | "Sheetfile" | "Sheet name" | "Sheet file"
                            )
                        })
                        .map(|f| (f.name.clone(), Some(f.value.clone()))),
                );
            }
            // KiCad 10 writes the library's keywords and footprint filters
            // (the filters joined by single spaces), not `ki_description`,
            // which it reports as the component's description instead.
            if let Some(v) = lib.property("ki_keywords").filter(|v| !v.is_empty()) {
                properties.push(("ki_keywords".into(), Some(v.to_string())));
            }
            if let Some(v) = lib
                .property("ki_fp_filters")
                .filter(|v| !v.trim().is_empty())
            {
                let joined = v.split_whitespace().collect::<Vec<_>>().join(" ");
                properties.push(("ki_fp_filters".into(), Some(joined)));
            }
            if !si_.in_bom || sp.excluded_from_bom {
                properties.push(("exclude_from_bom".into(), None));
            }
            if si_.dnp || sp.dnp {
                properties.push(("dnp".into(), None));
            }
            let description = sym
                .field("Description")
                .map(str::to_string)
                .or_else(|| lib.property("Description").map(str::to_string))
                .or_else(|| lib.property("ki_description").map(str::to_string))
                .unwrap_or_default();
            // Every UUID that shares the reference on this sheet, in KiCad's
            // order: the other units as its ordered set displaced them (file
            // order, a unit moving aside when one with a lower UUID arrives),
            // then the lowest UUID last.
            let tstamps = {
                let mut primary: Option<&str> = None;
                let mut extra: Vec<String> = Vec::new();
                for o in by_ref
                    .get(&(si, si_.reference.as_str()))
                    .into_iter()
                    .flatten()
                    .map(|&o| &conn.symbols[o])
                {
                    match primary {
                        None => primary = Some(&o.uuid),
                        Some(p) if p > o.uuid.as_str() => {
                            extra.push(p.to_string());
                            primary = Some(&o.uuid);
                        }
                        Some(_) => extra.push(o.uuid.clone()),
                    }
                }
                extra.extend(primary.map(str::to_string));
                extra
            };
            nl.components.push(Component {
                reference: si_.reference.clone(),
                // KiCad always writes a value, `~` for an empty one.
                value: if si_.value.is_empty() {
                    "~".into()
                } else {
                    si_.value.clone()
                },
                footprint: si_.footprint.clone(),
                datasheet: sym.field("Datasheet").unwrap_or("").to_string(),
                description,
                lib: lib_name,
                part,
                properties,
                sheet_names: sp.human(),
                sheet_tstamps: sp.tstamps(),
                tstamps,
            });
        }
    }
    for (id, lib) in used_libs {
        let (l, p) = split_lib_id(&id);
        let mut pins: Vec<(String, String, String)> = Vec::new();
        for (_, pin) in lib.all_pins() {
            if !pins.iter().any(|(n, _, _)| *n == pin.number) {
                pins.push((
                    pin.number.clone(),
                    pin.shown_name().to_string(),
                    pin.pin_type.canonical().to_string(),
                ));
            }
        }
        pins.sort_by(|a, b| str_num_cmp(&a.0, &b.0, true));
        nl.libparts.push(LibPart {
            lib: l,
            part: p,
            description: lib
                .property("Description")
                .or(lib.property("ki_description"))
                .unwrap_or("")
                .to_string(),
            docs: lib.property("Datasheet").unwrap_or("").to_string(),
            footprints: lib.footprint_filters.clone(),
            fields: lib
                .properties
                .iter()
                .filter(|(n, _)| MANDATORY[..4].contains(&n.as_str()))
                .cloned()
                .collect(),
            pins,
        });
    }

    // Nets.
    let mut order: Vec<usize> = (0..conn.nets.len()).collect();
    order.sort_by(|&a, &b| str_num_cmp(&conn.nets[a].name, &conn.nets[b].name, false));
    for (i, &ni) in order.iter().enumerate() {
        let net = &conn.nets[ni];
        let mut pins: Vec<usize> = net
            .pins
            .iter()
            .copied()
            .filter(|&p| {
                let s = &conn.symbols[conn.pins[p].sym];
                s.on_board && !project.sheets[s.sheet].excluded_from_board
            })
            .collect();
        let key = |p: usize| {
            (
                conn.symbols[conn.pins[p].sym].reference.clone(),
                conn.pins[p].number.clone(),
            )
        };
        pins.sort_by_key(|&p| key(p));
        pins.dedup_by_key(|p| key(*p));
        let all_stacked = pins.len() <= 1
            || pins.iter().all(|&p| {
                let (a, b) = (&conn.pins[pins[0]], &conn.pins[p]);
                a.sym == b.sym && a.pos == b.pos && a.lib_name == b.lib_name
            });
        let mut rec = NetRecord {
            code: (i + 1).to_string(),
            name: net.name.clone(),
            class: Some("Default".into()),
            nodes: Vec::new(),
        };
        for &p in &pins {
            let pin = &conn.pins[p];
            let sym = &conn.symbols[pin.sym];
            if sym.reference.starts_with('#') {
                continue;
            }
            let nums = stacked_numbers(&pin.number);
            for num in &nums {
                let base = &pin.shown_name;
                let function = if base.is_empty() {
                    num.clone()
                } else {
                    format!("{base}_{num}")
                };
                let mut pin_type = pin.pin_type.canonical().to_string();
                if net.has_no_connect && (pins.len() == 1 || all_stacked) {
                    pin_type.push_str("+no_connect");
                }
                rec.nodes.push(Node {
                    reference: sym.reference.clone(),
                    pin: num.clone(),
                    // KiCad 10 writes `NAME_NUMBER` (KiCad 9 wrote `NAME`).
                    pin_function: (!base.is_empty() || nums.len() > 1).then_some(function),
                    pin_type,
                });
            }
        }
        if !rec.nodes.is_empty() {
            nl.nets.push(rec);
        }
    }
    nl
}

/// Write a netlist in KiCad's S-expression format, laid out as KiCad 10
/// lays it out (one list per line, tab indentation), so a `diff` against
/// `kicad-cli sch export netlist` output lines up.
pub fn write_kicad(nl: &Netlist) -> String {
    let compact = write_kicad_compact(nl);
    match sexpr::parse(compact.as_bytes()) {
        Ok(mut doc) => {
            sexpr::reformat(&mut doc.root, 0, &sexpr::Style::default());
            doc.root.lead.clear();
            doc.trailing = "\n".into();
            doc.to_text()
        }
        // The compact text is ours and always parses; this is unreachable
        // in practice and still not worth a panic.
        Err(_) => compact,
    }
}

/// The same netlist in the compact layout KiCad 6 to 9 wrote.
pub fn write_kicad_compact(nl: &Netlist) -> String {
    let mut o = String::new();
    o.push_str("(export (version \"E\")\n");
    let _ = write!(
        o,
        "  (design\n    (source {})\n    (tool {})",
        quote(&nl.source),
        quote(&nl.tool)
    );
    for (num, name, ts) in &nl.sheets {
        let _ = write!(
            o,
            "\n    (sheet (number {}) (name {}) (tstamps {}))",
            quote(num),
            quote(name),
            quote(ts)
        );
    }
    o.push(')');
    o.push_str("\n  (components");
    for c in &nl.components {
        let _ = write!(o, "\n    (comp (ref {})", quote(&c.reference));
        let _ = write!(o, "\n      (value {})", quote(&c.value));
        if !c.footprint.is_empty() {
            let _ = write!(o, "\n      (footprint {})", quote(&c.footprint));
        }
        if !c.datasheet.is_empty() {
            let _ = write!(o, "\n      (datasheet {})", quote(&c.datasheet));
        }
        if !c.description.is_empty() {
            let _ = write!(o, "\n      (description {})", quote(&c.description));
        }
        let _ = write!(
            o,
            "\n      (libsource (lib {}) (part {}) (description {}))",
            quote(&c.lib),
            quote(&c.part),
            quote(&c.description)
        );
        for (n, v) in &c.properties {
            match v {
                Some(v) => {
                    let _ = write!(
                        o,
                        "\n      (property (name {}) (value {}))",
                        quote(n),
                        quote(v)
                    );
                }
                None => {
                    let _ = write!(o, "\n      (property (name {}))", quote(n));
                }
            }
        }
        let _ = write!(
            o,
            "\n      (sheetpath (names {}) (tstamps {}))",
            quote(&c.sheet_names),
            quote(&c.sheet_tstamps)
        );
        o.push_str("\n      (tstamps");
        for t in &c.tstamps {
            let _ = write!(o, " {}", quote(t));
        }
        o.push_str("))");
    }
    o.push(')');
    o.push_str("\n  (libparts");
    for lp in &nl.libparts {
        let _ = write!(
            o,
            "\n    (libpart (lib {}) (part {})",
            quote(&lp.lib),
            quote(&lp.part)
        );
        if !lp.description.is_empty() {
            let _ = write!(o, "\n      (description {})", quote(&lp.description));
        }
        if !lp.docs.is_empty() {
            let _ = write!(o, "\n      (docs {})", quote(&lp.docs));
        }
        if !lp.footprints.is_empty() {
            o.push_str("\n      (footprints");
            for f in &lp.footprints {
                let _ = write!(o, "\n        (fp {})", quote(f));
            }
            o.push(')');
        }
        o.push_str("\n      (fields");
        for (n, v) in &lp.fields {
            let _ = write!(o, "\n        (field (name {}) {})", quote(n), quote(v));
        }
        o.push(')');
        if !lp.pins.is_empty() {
            o.push_str("\n      (pins");
            for (num, name, t) in &lp.pins {
                let _ = write!(
                    o,
                    "\n        (pin (num {}) (name {}) (type {}))",
                    quote(num),
                    quote(name),
                    quote(t)
                );
            }
            o.push(')');
        }
        o.push(')');
    }
    o.push(')');
    o.push_str("\n  (nets");
    for n in &nl.nets {
        let _ = write!(
            o,
            "\n    (net (code {}) (name {})",
            quote(&n.code),
            quote(&n.name)
        );
        if let Some(c) = &n.class {
            let _ = write!(o, " (class {})", quote(c));
        }
        for node in &n.nodes {
            let _ = write!(
                o,
                "\n      (node (ref {}) (pin {})",
                quote(&node.reference),
                quote(&node.pin)
            );
            if let Some(f) = &node.pin_function {
                let _ = write!(o, " (pinfunction {})", quote(f));
            }
            let _ = write!(o, " (pintype {}))", quote(&node.pin_type));
        }
        o.push(')');
    }
    o.push_str("))\n");
    o
}

/// Read a KiCad S-expression netlist (any version that has the `E` layout:
/// KiCad 6 through 10).
pub fn parse_kicad(bytes: &[u8]) -> Result<Netlist, sexpr::Error> {
    let doc = sexpr::parse(bytes)?;
    let root = &doc.root;
    let mut nl = Netlist::default();
    let get = |l: &List, k: &str| l.value_of(k).unwrap_or_default();
    if let Some(design) = root.find("design") {
        nl.source = get(design, "source");
        nl.tool = get(design, "tool");
    }
    if let Some(comps) = root.find("components") {
        for c in comps.find_all("comp") {
            let libsource = c.find("libsource");
            let sheetpath = c.find("sheetpath");
            nl.components.push(Component {
                reference: get(c, "ref"),
                value: get(c, "value"),
                footprint: get(c, "footprint"),
                datasheet: get(c, "datasheet"),
                description: get(c, "description"),
                lib: libsource.map(|l| get(l, "lib")).unwrap_or_default(),
                part: libsource.map(|l| get(l, "part")).unwrap_or_default(),
                properties: c
                    .find_all("property")
                    .map(|p| (get(p, "name"), p.value_of("value")))
                    .collect(),
                sheet_names: sheetpath.map(|s| get(s, "names")).unwrap_or_default(),
                sheet_tstamps: sheetpath.map(|s| get(s, "tstamps")).unwrap_or_default(),
                tstamps: c.find("tstamps").map(List::args).unwrap_or_default(),
            });
        }
    }
    if let Some(nets) = root.find("nets") {
        for n in nets.find_all("net") {
            nl.nets.push(NetRecord {
                code: get(n, "code"),
                name: get(n, "name"),
                class: n.value_of("class"),
                nodes: n
                    .find_all("node")
                    .map(|nd| Node {
                        reference: get(nd, "ref"),
                        pin: get(nd, "pin"),
                        pin_function: nd.value_of("pinfunction"),
                        pin_type: get(nd, "pintype"),
                    })
                    .collect(),
            });
        }
    }
    Ok(nl)
}

/// How two netlists differ.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Parity {
    pub golden_components: usize,
    pub test_components: usize,
    pub missing_components: Vec<String>,
    pub extra_components: Vec<String>,
    /// Pins compared the way KiCad's regression test compares them.
    pub pins_compared: usize,
    pub pins_matched: usize,
    /// One line per mismatched pin.
    pub mismatches: Vec<String>,
    pub golden_nets: usize,
    pub test_nets: usize,
    /// Nets present in both with the same name and the same members.
    pub nets_identical: usize,
}

impl Parity {
    /// Parity as KiCad's own test defines it.
    pub fn matches(&self) -> bool {
        self.golden_components == self.test_components
            && self.missing_components.is_empty()
            && self.mismatches.is_empty()
    }
    /// Every net identical by name and membership.
    pub fn nets_identical(&self) -> bool {
        self.golden_nets == self.test_nets && self.nets_identical == self.golden_nets
    }
}

type PinMap = BTreeMap<(String, String), (String, Option<String>, String)>;

fn pin_map(nl: &Netlist) -> PinMap {
    let mut m = BTreeMap::new();
    for n in &nl.nets {
        for nd in &n.nodes {
            m.insert(
                (nd.reference.clone(), nd.pin.clone()),
                (n.name.clone(), nd.pin_function.clone(), nd.pin_type.clone()),
            );
        }
    }
    m
}

/// Compare a netlist against a reference (`golden`).
pub fn compare(golden: &Netlist, test: &Netlist) -> Parity {
    let mut p = Parity {
        golden_components: golden.components.len(),
        test_components: test.components.len(),
        golden_nets: golden.nets.len(),
        test_nets: test.nets.len(),
        ..Default::default()
    };
    let test_refs: BTreeSet<&str> = test
        .components
        .iter()
        .map(|c| c.reference.as_str())
        .collect();
    let golden_refs: BTreeSet<&str> = golden
        .components
        .iter()
        .map(|c| c.reference.as_str())
        .collect();
    p.missing_components = golden_refs
        .difference(&test_refs)
        .map(|s| s.to_string())
        .collect();
    p.extra_components = test_refs
        .difference(&golden_refs)
        .map(|s| s.to_string())
        .collect();
    let (gm, tm) = (pin_map(golden), pin_map(test));
    // KiCad 9 writes a pin function as `NAME`, KiCad 10 as `NAME_NUMBER`;
    // compare the name part.
    let same_func = |a: &Option<String>, b: &Option<String>, pin: &str| match (a, b) {
        (Some(a), Some(b)) => a == b || *a == format!("{b}_{pin}") || *b == format!("{a}_{pin}"),
        (None, None) => true,
        (Some(x), None) | (None, Some(x)) => x == pin,
    };
    for (key, (gname, gfunc, gtype)) in &gm {
        // KiCad's test skips these: their auto-generated names are
        // disambiguated nondeterministically.
        if gfunc.as_deref().is_some_and(|f| f.contains("RESERVED")) {
            continue;
        }
        p.pins_compared += 1;
        match tm.get(key) {
            Some((tname, tfunc, ttype))
                if tname == gname && same_func(tfunc, gfunc, &key.1) && ttype == gtype =>
            {
                p.pins_matched += 1
            }
            Some((tname, tfunc, ttype)) => p.mismatches.push(format!(
                "{} pin {}: expected {gname:?} {gfunc:?} {gtype:?}, got {tname:?} {tfunc:?} {ttype:?}",
                key.0, key.1
            )),
            None => p.mismatches.push(format!("{} pin {}: expected on {gname:?}, missing", key.0, key.1)),
        }
    }
    let members = |nl: &Netlist| -> BTreeMap<String, BTreeSet<(String, String)>> {
        nl.nets
            .iter()
            .map(|n| {
                (
                    n.name.clone(),
                    n.nodes
                        .iter()
                        .map(|nd| (nd.reference.clone(), nd.pin.clone()))
                        .collect(),
                )
            })
            .collect()
    };
    let (gn, tn) = (members(golden), members(test));
    p.nets_identical = gn.iter().filter(|(k, v)| tn.get(*k) == Some(v)).count();
    p
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn natural_order() {
        assert_eq!(str_num_cmp("R2", "R10", false), Ordering::Less);
        assert_eq!(str_num_cmp("R10", "R10", false), Ordering::Equal);
        assert_eq!(str_num_cmp("a", "B", true), Ordering::Less);
        assert_eq!(str_num_cmp("a", "B", false), Ordering::Greater);
        assert_eq!(
            str_num_cmp("Net-(U1-Pad2)", "Net-(U1-Pad10)", false),
            Ordering::Less
        );
        assert_eq!(str_num_cmp("A1", "A1B", false), Ordering::Less);
        assert_eq!(
            str_num_cmp("99999999999999999999999", "1", false),
            Ordering::Greater
        );
    }

    #[test]
    fn write_then_read() {
        let nl = Netlist {
            source: "x.kicad_sch".into(),
            tool: "t".into(),
            components: vec![Component {
                reference: "R1".into(),
                value: "1k".into(),
                ..Default::default()
            }],
            nets: vec![NetRecord {
                code: "1".into(),
                name: "/A".into(),
                class: Some("Default".into()),
                nodes: vec![Node {
                    reference: "R1".into(),
                    pin: "1".into(),
                    pin_function: None,
                    pin_type: "passive".into(),
                }],
            }],
            ..Default::default()
        };
        let text = write_kicad(&nl);
        let back = parse_kicad(text.as_bytes()).unwrap();
        assert_eq!(back.nets, nl.nets);
        assert_eq!(back.components[0].reference, "R1");
        assert!(compare(&nl, &back).matches());
    }
}
