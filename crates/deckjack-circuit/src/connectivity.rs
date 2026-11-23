//! Connectivity: which pins are on which net, and what each net is called.
//!
//! The algorithm follows KiCad's `CONNECTION_GRAPH` (eeschema
//! `connection_graph.cpp`, KiCad 10), step for step where net names depend
//! on it, because a netlist that differs from KiCad's in a single name is a
//! netlist a board house will build differently. It is written from a reading
//! of that code, not translated from it.
//!
//! 1. **Items per sheet instance.** Pins (at their world positions, after the
//!    symbol's rotation and mirroring), wires, buses, junctions, no-connect
//!    flags, labels, sheet pins and bus entries. Items connect where their
//!    connection points coincide exactly, with KiCad's exceptions: wires do
//!    not join buses, a no-connect pin joins nothing, a junction joins wires
//!    that pass through it, a label joins two or more lines crossing under
//!    it, and a bus entry's bus end joins only the bus.
//! 2. **Subgraphs.** Connected components, one sheet instance each. Each picks
//!    a driver by priority (global label, global power pin, local power pin,
//!    local label, hierarchical label, sheet pin, ordinary pin) and then by
//!    KiCad's tie-breaks.
//! 3. **Same-sheet merging** of strongly driven subgraphs whose names match,
//!    including secondary labels on the same wire; renaming of weakly driven
//!    subgraphs whose names collide (`_1`, `_2`); creation of virtual
//!    subgraphs for bus members.
//! 4. **Global names**: subgraphs joined by shared global label or power
//!    names take one name.
//! 5. **Hierarchy**: sheet pins and hierarchical labels link subgraphs across
//!    sheet instances; the best driver of the chain names all of it. Bus
//!    members propagate by position (vector buses) or name (groups).
//! 6. **Final names**: a weakly driven subgraph with one pin is
//!    `unconnected-(...)`. A net is every subgraph with the same final name.
//!
//! Not modeled, and the effect: text variables in labels (`${VAR}`) are not
//! expanded; hit-testing is exact rather than
//! within a wire's pen width; jumper pin groups are modeled, KiCad 10
//! variants are not.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::rc::Rc;

use crate::conn::{
    self, Aliases, Conn, DriverKind, Kind, match_bus_member, member_at, member_at_mut,
};
use crate::model::{LabelKind, LibSymbol, PinType, Power, Pt, SheetModel};
use crate::project::{Project, unit_letters};

/// Recursion guard for hierarchy propagation, far past any real design.
const MAX_PROPAGATION_DEPTH: usize = 256;

/// At most this many pins, items and bus members across the whole project,
/// counting every sheet instance separately. A shared sheet placed thousands
/// of times, or one library symbol with thousands of pins placed thousands
/// of times, multiplies a small file into far more work than its size
/// suggests; past this budget the rest of the project is left out and the
/// result is marked approximate.
pub const MAX_PROJECT_ITEMS: usize = 2_000_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Priority {
    Invalid = -1,
    None = 0,
    Pin,
    SheetPin,
    HierLabel,
    LocalLabel,
    LocalPowerPin,
    GlobalPowerPin,
    Global,
}

/// A symbol placed in one sheet instance.
#[derive(Debug, Clone)]
pub struct SymInst {
    pub sheet: usize,
    /// Index into the file model's `symbols`.
    pub symbol: usize,
    pub uuid: String,
    pub reference: String,
    pub reference_with_unit: String,
    pub unit: u32,
    pub unit_count: u32,
    pub value: String,
    pub footprint: String,
    pub lib: Option<Rc<LibSymbol>>,
    pub power: Option<Power>,
    pub on_board: bool,
    pub in_bom: bool,
    pub dnp: bool,
    /// The library symbol's Reference field starts with `#` (a power or
    /// virtual symbol).
    pub lib_ref_hash: bool,
    pub pins: Vec<usize>,
}

impl SymInst {
    pub fn in_netlist(&self) -> bool {
        !self.reference.starts_with('#')
    }
}

/// A pin of a placed symbol.
#[derive(Debug, Clone)]
pub struct PinInst {
    pub sym: usize,
    pub number: String,
    /// The library pin's raw name.
    pub lib_name: String,
    /// The library pin's shown name (empty for `~`).
    pub lib_shown_name: String,
    /// The shown name: the alternate's when one is selected.
    pub shown_name: String,
    pub pin_type: PinType,
    pub hidden: bool,
    pub pos: Pt,
    /// Default net names: `[normal, forced no-connect]`.
    pub default_names: [String; 2],
}

#[derive(Debug, Clone, PartialEq)]
enum ItemKind {
    Pin(usize),
    Wire,
    Bus,
    Junction,
    NoConnect,
    Label(LabelKind, String),
    SheetPin {
        name: String,
        shape: String,
        child: Option<usize>,
    },
    WireEntry,
    BusBusEntry,
}

#[derive(Debug, Clone)]
struct Item {
    kind: ItemKind,
    sheet: usize,
    points: Vec<Pt>,
}

impl Item {
    fn is_driver(&self) -> bool {
        matches!(
            self.kind,
            ItemKind::Pin(_)
                | ItemKind::SheetPin { .. }
                | ItemKind::Label(
                    LabelKind::Local | LabelKind::Global | LabelKind::Hierarchical,
                    _
                )
        )
    }
    fn is_line(&self) -> bool {
        matches!(self.kind, ItemKind::Wire | ItemKind::Bus)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ConnRef {
    Item(usize),
    Implicit(usize),
}

/// A bus member snapshot used as a key, and the subgraphs linked through it.
type Links = Vec<(Conn, BTreeSet<usize>)>;

fn link(links: &mut Links, key: &Conn, sg: usize) {
    if let Some((_, set)) = links.iter_mut().find(|(k, _)| {
        k.name == key.name
            && k.vector_index == key.vector_index
            && k.local_name == key.local_name
            && k.prefix == key.prefix
    }) {
        set.insert(sg);
    } else {
        links.push((key.clone(), BTreeSet::from([sg])));
    }
}

#[derive(Debug, Clone, Default)]
struct Subgraph {
    sheet: usize,
    items: Vec<usize>,
    drivers: Vec<usize>,
    driver: Option<usize>,
    conn: Option<ConnRef>,
    strong: bool,
    local_driver: bool,
    multiple: bool,
    no_connect: Option<usize>,
    hier_pins: Vec<usize>,
    hier_ports: Vec<usize>,
    absorbed: bool,
    absorbed_by: Option<usize>,
    absorbed_list: Vec<usize>,
    is_bus_member: bool,
    dirty: bool,
    bus_neighbors: Links,
    bus_parents: Links,
}

/// One net: its name and its pins.
#[derive(Debug, Clone, PartialEq)]
pub struct Net {
    pub name: String,
    /// `(sheet instance, pin index)` for every pin, before netlist filtering.
    pub pins: Vec<usize>,
    /// A no-connect flag sits on this net.
    pub has_no_connect: bool,
    /// This is a bus (`D[0..7]`, `USB{DP DN}`), not a net. Buses carry no
    /// pins; they are listed because KiCad's net numbering counts them.
    pub bus: bool,
}

/// The result: symbols, pins and nets of a whole project.
#[derive(Debug, Clone)]
pub struct Connectivity {
    pub paths: Vec<String>,
    pub symbols: Vec<SymInst>,
    pub pins: Vec<PinInst>,
    pub nets: Vec<Net>,
    /// Pin index to net index.
    pub pin_net: HashMap<usize, usize>,
    /// Why the result may differ from KiCad's, if it may.
    pub approximate: Vec<String>,
}

impl Connectivity {
    pub fn net_of_pin(&self, pin: usize) -> Option<&Net> {
        self.pin_net.get(&pin).map(|&n| &self.nets[n])
    }
}

struct Graph<'a> {
    project: &'a Project,
    paths: Vec<String>,
    aliases: Aliases,
    items: Vec<Item>,
    syms: Vec<SymInst>,
    pins: Vec<PinInst>,
    item_conn: Vec<Option<Conn>>,
    implicit: Vec<Conn>,
    sgs: Vec<Subgraph>,
    driver_sgs: Vec<usize>,
    name_map: HashMap<String, Vec<usize>>,
    sheet_sgs: HashMap<usize, Vec<usize>>,
    approximate: BTreeSet<String>,
    depth: usize,
    /// Work counted against [`MAX_PROJECT_ITEMS`].
    work: usize,
}

/// Compute connectivity for a loaded project.
pub fn compute(project: &Project) -> Connectivity {
    let mut g = Graph {
        project,
        paths: project.sheets.iter().map(|s| s.human()).collect(),
        aliases: Aliases::new(),
        items: Vec::new(),
        syms: Vec::new(),
        pins: Vec::new(),
        item_conn: Vec::new(),
        implicit: Vec::new(),
        sgs: Vec::new(),
        driver_sgs: Vec::new(),
        name_map: HashMap::new(),
        sheet_sgs: HashMap::new(),
        approximate: BTreeSet::new(),
        depth: 0,
        work: 0,
    };
    g.collect_aliases();
    g.build_items();
    g.build_subgraphs();
    g.resolve_all();
    g.collect_driver_values();
    g.generate_bus_alias_members();
    g.process_subgraphs();
    g.finish();
    g.into_result()
}

fn on_segment(p: Pt, a: Pt, b: Pt) -> bool {
    let cross =
        (b.x - a.x) as i128 * (p.y - a.y) as i128 - (b.y - a.y) as i128 * (p.x - a.x) as i128;
    cross == 0
        && p.x >= a.x.min(b.x)
        && p.x <= a.x.max(b.x)
        && p.y >= a.y.min(b.y)
        && p.y <= a.y.max(b.y)
}

/// The lines of one sheet instance, indexed to answer "which lines pass
/// through this point" without testing every line. Horizontal and vertical
/// lines are grouped by their fixed coordinate and found by interval
/// stabbing; diagonal lines are found by their X range, then tested exactly.
struct LineIndex {
    horizontal: HashMap<i64, Stabber>,
    vertical: HashMap<i64, Stabber>,
    points: HashMap<Pt, Vec<usize>>,
    diagonal: Stabber,
    diagonal_ends: HashMap<usize, (Pt, Pt)>,
}

impl LineIndex {
    fn new(lines: impl Iterator<Item = (usize, Pt, Pt)>) -> LineIndex {
        let mut h: HashMap<i64, Vec<(i64, i64, usize)>> = HashMap::new();
        let mut v: HashMap<i64, Vec<(i64, i64, usize)>> = HashMap::new();
        let mut points: HashMap<Pt, Vec<usize>> = HashMap::new();
        let mut d = Vec::new();
        let mut diagonal_ends = HashMap::new();
        for (id, a, b) in lines {
            if a == b {
                points.entry(a).or_default().push(id);
            } else if a.y == b.y {
                h.entry(a.y)
                    .or_default()
                    .push((a.x.min(b.x), a.x.max(b.x), id));
            } else if a.x == b.x {
                v.entry(a.x)
                    .or_default()
                    .push((a.y.min(b.y), a.y.max(b.y), id));
            } else {
                d.push((a.x.min(b.x), a.x.max(b.x), id));
                diagonal_ends.insert(id, (a, b));
            }
        }
        LineIndex {
            horizontal: h.into_iter().map(|(k, iv)| (k, Stabber::new(iv))).collect(),
            vertical: v.into_iter().map(|(k, iv)| (k, Stabber::new(iv))).collect(),
            points,
            diagonal: Stabber::new(d),
            diagonal_ends,
        }
    }

    /// The lines `p` lies on (`on_segment`), ascending.
    fn through(&self, p: Pt) -> Vec<usize> {
        let mut out = Vec::new();
        if let Some(s) = self.horizontal.get(&p.y) {
            s.stab(p.x, &mut out);
        }
        if let Some(s) = self.vertical.get(&p.x) {
            s.stab(p.y, &mut out);
        }
        if let Some(v) = self.points.get(&p) {
            out.extend(v.iter().copied());
        }
        let mut diag = Vec::new();
        self.diagonal.stab(p.x, &mut diag);
        out.extend(diag.into_iter().filter(|id| {
            self.diagonal_ends
                .get(id)
                .is_some_and(|&(a, b)| on_segment(p, a, b))
        }));
        out.sort_unstable();
        out
    }
}

/// Closed intervals `[lo, hi]`, sorted by `lo`, under a segment tree of the
/// largest `hi`, so the intervals containing a value are found in time
/// proportional to how many there are (times a logarithm), not to how many
/// intervals exist.
struct Stabber {
    iv: Vec<(i64, i64, usize)>,
    max_hi: Vec<i64>,
}

impl Stabber {
    fn new(mut iv: Vec<(i64, i64, usize)>) -> Stabber {
        iv.sort_unstable();
        let mut s = Stabber {
            max_hi: vec![i64::MIN; 4 * iv.len().max(1)],
            iv,
        };
        if !s.iv.is_empty() {
            s.build(1, 0, s.iv.len());
        }
        s
    }

    fn build(&mut self, node: usize, l: usize, r: usize) -> i64 {
        let m = if r - l == 1 {
            self.iv[l].1
        } else {
            let mid = (l + r) / 2;
            self.build(2 * node, l, mid)
                .max(self.build(2 * node + 1, mid, r))
        };
        self.max_hi[node] = m;
        m
    }

    fn stab(&self, x: i64, out: &mut Vec<usize>) {
        // Only intervals starting at or before `x` can contain it.
        let end = self.iv.partition_point(|&(lo, _, _)| lo <= x);
        if end > 0 {
            self.query(1, 0, self.iv.len(), end, x, out);
        }
    }

    fn query(&self, node: usize, l: usize, r: usize, end: usize, x: i64, out: &mut Vec<usize>) {
        if l >= end || self.max_hi[node] < x {
            return;
        }
        if r - l == 1 {
            out.push(self.iv[l].2);
            return;
        }
        let mid = (l + r) / 2;
        self.query(2 * node, l, mid, end, x, out);
        self.query(2 * node + 1, mid, r, end, x, out);
    }
}

/// Expand KiCad 10's stacked pin notation (`[1,3-5]`) into numbers.
pub fn stacked_numbers(number: &str) -> Vec<String> {
    let Some(inner) = number.strip_prefix('[').and_then(|s| s.strip_suffix(']')) else {
        return vec![number.to_string()];
    };
    let mut out = Vec::new();
    for part in inner.split(',') {
        let part = part.trim();
        if let Some((a, b)) = part.split_once('-')
            && let (Ok(a), Ok(b)) = (a.trim().parse::<i64>(), b.trim().parse::<i64>())
            && a <= b
            && b - a < 1024
        {
            out.extend((a..=b).map(|n| n.to_string()));
            continue;
        }
        if !part.is_empty() {
            out.push(part.to_string());
        }
    }
    if out.is_empty() {
        vec![number.to_string()]
    } else {
        out
    }
}

fn effective_pad_number(number: &str) -> String {
    let nums = stacked_numbers(number);
    if nums.len() <= 1 {
        return number.to_string();
    }
    // The smallest, numerically where possible.
    nums.iter()
        .min_by(|a, b| match (a.parse::<i64>(), b.parse::<i64>()) {
            (Ok(x), Ok(y)) => x.cmp(&y),
            _ => a.cmp(b),
        })
        .cloned()
        .unwrap_or_else(|| number.to_string())
}

/// The low 32 bits of KiCad's `KIID::Hash`, which is Boost's
/// `uuids::hash_value`. This is the formula of Boost 1.86 and later (KiCad
/// 10 as Arch builds it, against Boost 1.92, checked against `kicad-cli`
/// 10.0.7). A KiCad built against an older Boost hashes differently, and its
/// `unconnected-(U?-xxxxxxxx-PadN)` names will not match these.
pub fn kiid_hash32(uuid: &str) -> u32 {
    let hex: Vec<u8> = uuid.bytes().filter(u8::is_ascii_hexdigit).collect();
    let mut bytes = [0u8; 16];
    if hex.len() == 32 {
        for (i, b) in bytes.iter_mut().enumerate() {
            let s = std::str::from_utf8(&hex[2 * i..2 * i + 2]).unwrap_or("00");
            *b = u8::from_str_radix(s, 16).unwrap_or(0);
        }
    }
    let mx = |x: u64| {
        let x = x.wrapping_mul(0xD96A_AA55);
        x ^ (x >> 16)
    };
    let mut r: u64 = 0;
    for i in [0, 4, 8, 12] {
        let w = u32::from_le_bytes([bytes[i], bytes[i + 1], bytes[i + 2], bytes[i + 3]]);
        r = mx(r.wrapping_add(w as u64));
    }
    let r = r.wrapping_mul(0x7DF9_54AB);
    (r ^ (r >> 16)) as u32
}

impl Graph<'_> {
    fn collect_aliases(&mut self) {
        // Bus aliases are schematic-wide in KiCad 7+ (and per sheet before);
        // a name defined twice keeps the first definition.
        for sp in &self.project.sheets {
            let f = &self.project.files[&sp.file];
            for a in &f.model.bus_aliases {
                self.aliases
                    .entry(a.name.clone())
                    .or_insert_with(|| a.members.clone());
            }
        }
        if let Some(pf) = &self.project.project_file
            && let Some(crate::project_file::Spanned {
                value: crate::project_file::Value::Object(entries),
                ..
            }) = pf.get(&["schematic", "bus_aliases"])
        {
            for (name, v) in entries {
                if let crate::project_file::Value::Array(items) = &v.value {
                    let members = items
                        .iter()
                        .filter_map(|m| m.as_str().map(str::to_string))
                        .collect();
                    self.aliases.entry(name.clone()).or_insert(members);
                }
            }
        }
    }

    fn add_item(&mut self, kind: ItemKind, sheet: usize, points: Vec<Pt>) -> usize {
        let conn = match &kind {
            ItemKind::Pin(p) => {
                let mut c = Conn::new(sheet);
                c.kind = Kind::Net;
                c.name = self.pins[*p].default_names[0].clone();
                Some(c)
            }
            ItemKind::Label(k, text) if *k != LabelKind::Directive => {
                Some(self.label_conn(sheet, text))
            }
            ItemKind::SheetPin { name, .. } => Some(self.label_conn(sheet, name)),
            _ => None,
        };
        self.work += conn.as_ref().map_or(1, Conn::tree_size);
        self.items.push(Item {
            kind,
            sheet,
            points,
        });
        self.item_conn.push(conn);
        self.items.len() - 1
    }

    /// A label's or sheet pin's connection, noting a bus cut short.
    fn label_conn(&mut self, sheet: usize, text: &str) -> Conn {
        let mut c = Conn::new(sheet);
        if !c.configure_from_label(&conn::escape_netname(text), &self.aliases) {
            self.approximate.insert(format!(
                "bus labels that refer to themselves through an alias, nest deeper than 8 groups or expand to more than {} members were cut off",
                conn::MAX_LABEL_MEMBERS
            ));
        }
        c
    }

    fn over_budget(&mut self) -> bool {
        if self.work <= MAX_PROJECT_ITEMS {
            return false;
        }
        self.approximate.insert(format!(
            "the project has more than {MAX_PROJECT_ITEMS} pins, items and bus members across its sheet instances; the rest were left out"
        ));
        true
    }

    fn build_items(&mut self) {
        let project = self.project;
        let (sep, first) = project.subpart_settings();
        let root_version = project.files[&project.root_file].model.version;
        // (parent instance, sheet item index) -> child instance
        let mut child_of: HashMap<(usize, usize), usize> = HashMap::new();
        for (i, sp) in project.sheets.iter().enumerate() {
            if let (Some(p), Some(item)) = (sp.parent, sp.sheet_item) {
                child_of.insert((p, item), i);
            }
        }
        // Each file's library symbols, flattened once rather than once per
        // placement.
        let mut libs: HashMap<(&str, &str, &str), Option<Rc<LibSymbol>>> = HashMap::new();
        'sheets: for (si, sp) in project.sheets.iter().enumerate() {
            if self.over_budget() {
                break;
            }
            let file = &project.files[&sp.file];
            let m = &file.model;
            // Symbols and their pins.
            for (sym_idx, sym) in m.symbols.iter().enumerate() {
                if self.over_budget() {
                    break 'sheets;
                }
                let inst = project.resolve_instance(si, sym);
                let lib = libs
                    .entry((sp.file.as_str(), sym.cache_key(), sym.lib_id.as_str()))
                    .or_insert_with(|| m.lib_symbol_for(sym).map(Rc::new))
                    .clone();
                let unit_count = lib.as_ref().map(|l| l.unit_count()).unwrap_or(1);
                let mut reference_with_unit = inst.reference.clone();
                if unit_count > 1 {
                    let display = lib.as_ref().and_then(|l| {
                        l.units
                            .iter()
                            .find(|u| u.unit == inst.unit)
                            .and_then(|u| u.unit_name.clone())
                    });
                    if let Some(sep) = sep {
                        reference_with_unit.push(sep);
                    }
                    match display {
                        Some(d) if !d.is_empty() => reference_with_unit.push_str(&d),
                        _ => reference_with_unit.push_str(&unit_letters(inst.unit, first)),
                    }
                }
                // KiCad's `FixLegacyPowerSymbolMismatches`, run on load for
                // projects older than 20230221: a global power symbol with
                // an invisible power pin takes the pin's name as its value.
                let mut value = inst.value.clone();
                if root_version < 20230221
                    && let Some(l) = lib.as_ref()
                    && l.power == Some(Power::Global)
                    && let Some((_, first)) = l.all_pins().next()
                    && first.pin_type == PinType::PowerIn
                    && first.hidden
                {
                    value = first.name.clone();
                }
                let sym_i = self.syms.len();
                self.syms.push(SymInst {
                    sheet: si,
                    symbol: sym_idx,
                    uuid: sym.uuid.clone(),
                    reference: inst.reference.clone(),
                    reference_with_unit,
                    unit: inst.unit,
                    unit_count,
                    value,
                    footprint: inst.footprint.clone(),
                    power: lib.as_ref().and_then(|l| l.power),
                    lib_ref_hash: lib
                        .as_ref()
                        .and_then(|l| l.property("Reference"))
                        .is_some_and(|r| r.starts_with('#')),
                    lib: lib.clone(),
                    on_board: sym.on_board,
                    in_bom: sym.in_bom,
                    dnp: sym.dnp,
                    pins: Vec::new(),
                });
                if lib.is_none() {
                    self.approximate.insert(format!(
                        "symbol {} uses \"{}\", which is not in the sheet's lib_symbols cache; it has no pins",
                        inst.reference,
                        sym.cache_key()
                    ));
                    continue;
                }
                let mut pin_ids = Vec::new();
                let placed_pins = lib
                    .as_deref()
                    .map(|l| SheetModel::placed_pins_with(l, sym, inst.unit))
                    .unwrap_or_default();
                self.work += placed_pins.len();
                for placed in placed_pins {
                    let lp = &placed.pin;
                    let alt = sym
                        .alternates
                        .get(&lp.number)
                        .and_then(|a| lp.alternates.iter().find(|(n, _)| n == a));
                    let (shown_name, pin_type) = match alt {
                        Some((n, t)) => (n.clone(), *t),
                        None => (lp.shown_name().to_string(), lp.pin_type),
                    };
                    self.pins.push(PinInst {
                        sym: sym_i,
                        number: lp.number.clone(),
                        lib_name: lp.name.clone(),
                        lib_shown_name: lp.shown_name().to_string(),
                        shown_name,
                        pin_type,
                        hidden: lp.hidden,
                        pos: placed.at,
                        default_names: [String::new(), String::new()],
                    });
                    pin_ids.push(self.pins.len() - 1);
                }
                self.syms[sym_i].pins = pin_ids.clone();
                let shared = self.shared_names(&pin_ids);
                for &p in &pin_ids {
                    self.pins[p].default_names = [
                        self.default_net_name(p, false, &shared),
                        self.default_net_name(p, true, &shared),
                    ];
                }
            }
            // Items in file order: symbols' pins first, as KiCad adds them
            // when it meets the symbol, then everything else.
            let mut ordered: Vec<(usize, ItemKind, Vec<Pt>)> = Vec::new();
            for sym in self.syms.iter().filter(|s| s.sheet == si) {
                let idx = m.symbols[sym.symbol].index;
                for &p in &sym.pins {
                    ordered.push((idx, ItemKind::Pin(p), vec![self.pins[p].pos]));
                }
            }
            for w in &m.wires {
                ordered.push((w.index, ItemKind::Wire, vec![w.a, w.b]));
            }
            for b in &m.buses {
                ordered.push((b.index, ItemKind::Bus, vec![b.a, b.b]));
            }
            for (i, &j) in m.junctions.iter().enumerate() {
                ordered.push((
                    usize::MAX - 2 * m.junctions.len() + i,
                    ItemKind::Junction,
                    vec![j],
                ));
            }
            for (i, &n) in m.no_connects.iter().enumerate() {
                ordered.push((
                    usize::MAX - m.no_connects.len() + i,
                    ItemKind::NoConnect,
                    vec![n],
                ));
            }
            for l in &m.labels {
                ordered.push((l.index, ItemKind::Label(l.kind, l.text.clone()), vec![l.at]));
            }
            for (item_idx, sh) in m.sheets.iter().enumerate() {
                let child = child_of.get(&(si, item_idx)).copied();
                for p in &sh.pins {
                    ordered.push((
                        sh.index,
                        ItemKind::SheetPin {
                            name: p.name.clone(),
                            shape: p.shape.clone(),
                            child,
                        },
                        vec![p.at],
                    ));
                }
            }
            for e in &m.wire_entries {
                ordered.push((e.index, ItemKind::WireEntry, vec![e.at, e.end]));
            }
            for e in &m.bus_bus_entries {
                ordered.push((e.index, ItemKind::BusBusEntry, vec![e.at, e.end]));
            }
            ordered.sort_by_key(|(idx, _, _)| *idx);
            for (_, kind, pts) in ordered {
                if self.over_budget() {
                    break 'sheets;
                }
                self.add_item(kind, si, pts);
            }
        }
    }

    /// For one symbol's pins: per (shown name, is a no-connect pin), the
    /// first pin number seen and whether a second, different number shares
    /// them. Computed once per symbol so that naming its pins is linear in
    /// the pin count, not quadratic.
    fn shared_names(&self, pins: &[usize]) -> HashMap<(String, bool), (String, bool)> {
        let mut out: HashMap<(String, bool), (String, bool)> = HashMap::new();
        for &o in pins {
            let op = &self.pins[o];
            let key = (op.shown_name.clone(), op.pin_type == PinType::NoConnect);
            match out.get_mut(&key) {
                Some((first, multiple)) => *multiple |= *first != op.number,
                None => {
                    out.insert(key, (op.number.clone(), false));
                }
            }
        }
        out
    }

    /// `SCH_PIN::GetDefaultNetName`.
    fn default_net_name(
        &self,
        p: usize,
        force_nc: bool,
        shared: &HashMap<(String, bool), (String, bool)>,
    ) -> String {
        let pin = &self.pins[p];
        let sym = &self.syms[pin.sym];
        if self.pin_is_global_power(p) || self.pin_is_local_power(p) {
            return if sym.power.is_some() {
                conn::escape_netname(&sym.value)
            } else {
                conn::escape_netname(&pin.lib_name)
            };
        }
        let unconnected = force_nc || pin.pin_type == PinType::NoConnect;
        let mut name = String::from(if unconnected {
            "unconnected-("
        } else {
            "Net-("
        });
        // Another pin with the same shown name and a different number.
        let has_multiple = shared
            .get(&(pin.shown_name.clone(), unconnected))
            .is_some_and(|(first, multiple)| *multiple || *first != pin.number);
        let pad = effective_pad_number(&pin.number);
        if sym.reference.ends_with('?') {
            // KiCad stands a short hash of the symbol's UUID in for the
            // missing number.
            name.push_str(&format!(
                "{}-{:08x}-Pad{pad})",
                sym.reference,
                kiid_hash32(&sym.uuid)
            ));
        } else if !pin.lib_shown_name.is_empty() && pin.lib_shown_name != pin.number {
            name.push_str(&sym.reference_with_unit);
            name.push('-');
            name.push_str(&conn::escape_netname(&pin.lib_shown_name));
            if unconnected || has_multiple {
                name.push_str("-Pad");
                name.push_str(&conn::escape_netname(&pad));
            }
            name.push(')');
        } else {
            name.push_str(&sym.reference);
            name.push_str("-Pad");
            name.push_str(&conn::escape_netname(&pad));
            name.push(')');
        }
        name
    }

    fn pin_is_global_power(&self, p: usize) -> bool {
        let pin = &self.pins[p];
        if pin.pin_type != PinType::PowerIn {
            return false;
        }
        match self.syms[pin.sym].power {
            Some(Power::Global) => true,
            Some(Power::Local) => false,
            None => pin.hidden,
        }
    }

    fn pin_is_local_power(&self, p: usize) -> bool {
        let pin = &self.pins[p];
        pin.pin_type == PinType::PowerIn && self.syms[pin.sym].power == Some(Power::Local)
    }

    fn pin_is_power(&self, p: usize) -> bool {
        self.pin_is_global_power(p) || self.pin_is_local_power(p)
    }

    fn priority(&self, item: Option<usize>) -> Priority {
        let Some(item) = item else {
            return Priority::None;
        };
        match &self.items[item].kind {
            ItemKind::SheetPin { .. } => Priority::SheetPin,
            ItemKind::Label(LabelKind::Hierarchical, _) => Priority::HierLabel,
            ItemKind::Label(LabelKind::Local, _) => Priority::LocalLabel,
            ItemKind::Label(LabelKind::Global, _) => Priority::Global,
            ItemKind::Pin(p) => {
                let sym = &self.syms[self.pins[*p].sym];
                if self.pin_is_global_power(*p) {
                    Priority::GlobalPowerPin
                } else if self.pin_is_local_power(*p) {
                    Priority::LocalPowerPin
                } else if !sym.on_board || sym.lib_ref_hash {
                    Priority::None
                } else {
                    Priority::Pin
                }
            }
            _ => Priority::None,
        }
    }

    fn driver_kind(&self, item: usize) -> DriverKind {
        match &self.items[item].kind {
            ItemKind::Label(LabelKind::Global, _) => DriverKind::GlobalLabel,
            ItemKind::Pin(p) => DriverKind::Pin {
                local_power: self.pin_is_local_power(*p),
            },
            _ => DriverKind::Other,
        }
    }

    /// `GetNameForDriver`.
    fn name_for(&self, sg: usize, item: usize) -> String {
        match &self.items[item].kind {
            ItemKind::Pin(p) => {
                self.pins[*p].default_names[usize::from(self.sgs[sg].no_connect.is_some())].clone()
            }
            ItemKind::Label(_, text) => conn::escape_netname(text),
            ItemKind::SheetPin { name, .. } => conn::escape_netname(name),
            _ => String::new(),
        }
    }

    fn conn_ref(&self, r: ConnRef) -> &Conn {
        match r {
            ConnRef::Item(i) => self.item_conn[i]
                .as_ref()
                .expect("driver items have connections"),
            ConnRef::Implicit(i) => &self.implicit[i],
        }
    }

    fn conn_ref_mut(&mut self, r: ConnRef) -> &mut Conn {
        match r {
            ConnRef::Item(i) => self.item_conn[i]
                .as_mut()
                .expect("driver items have connections"),
            ConnRef::Implicit(i) => &mut self.implicit[i],
        }
    }

    fn conn(&self, sg: usize) -> Option<&Conn> {
        self.sgs[sg].conn.map(|r| self.conn_ref(r))
    }

    fn conn_mut(&mut self, sg: usize) -> Option<&mut Conn> {
        let r = self.sgs[sg].conn?;
        Some(self.conn_ref_mut(r))
    }

    fn full(&self, c: &Conn) -> String {
        c.full_name(&self.paths)
    }

    fn sg_name(&self, sg: usize) -> String {
        self.conn(sg).map(|c| self.full(c)).unwrap_or_default()
    }

    fn sg_name_short(&self, sg: usize) -> String {
        self.conn(sg).map(Conn::short_name).unwrap_or_default()
    }

    // ---- 1. items to subgraphs ------------------------------------------

    fn build_subgraphs(&mut self) {
        let n = self.items.len();
        let mut parent: Vec<usize> = (0..n).collect();
        fn find(parent: &mut [usize], mut x: usize) -> usize {
            while parent[x] != x {
                parent[x] = parent[parent[x]];
                x = parent[x];
            }
            x
        }
        let union = |parent: &mut Vec<usize>, a: usize, b: usize| {
            let (ra, rb) = (find(parent, a), find(parent, b));
            if ra != rb {
                parent[ra.max(rb)] = ra.min(rb);
            }
        };
        // Per sheet instance.
        let mut by_sheet: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
        for (i, it) in self.items.iter().enumerate() {
            by_sheet.entry(it.sheet).or_default().push(i);
        }
        for ids in by_sheet.values() {
            let lines: Vec<usize> = ids
                .iter()
                .copied()
                .filter(|&i| self.items[i].is_line())
                .collect();
            let buses: Vec<usize> = lines
                .iter()
                .copied()
                .filter(|&i| self.items[i].kind == ItemKind::Bus)
                .collect();
            let seg = |i: usize| (i, self.items[i].points[0], self.items[i].points[1]);
            let line_index = LineIndex::new(lines.iter().map(|&i| seg(i)));
            let bus_index = LineIndex::new(buses.iter().map(|&i| seg(i)));
            let mut map: BTreeMap<Pt, Vec<usize>> = BTreeMap::new();
            let mut extra: Vec<(usize, usize)> = Vec::new();
            for &i in ids {
                for &p in &self.items[i].points {
                    map.entry(p).or_default().push(i);
                }
            }
            for &i in ids {
                let it = &self.items[i];
                let p = it.points[0];
                match &it.kind {
                    ItemKind::Label(..) => {
                        let hits = line_index.through(p);
                        if hits.len() >= 2 {
                            map.entry(p).or_default().extend(hits.iter().copied());
                        }
                        // `SCH_LABEL_BASE::UpdateDanglingState`: a label also
                        // joins the first bus, else the first wire, passing
                        // under it, unless a pin or no-connect flag sits at
                        // its position.
                        let pin_or_nc = map.get(&p).is_some_and(|v| {
                            v.iter().any(|&o| {
                                matches!(self.items[o].kind, ItemKind::Pin(_) | ItemKind::NoConnect)
                            })
                        });
                        if !pin_or_nc {
                            let first = hits
                                .iter()
                                .copied()
                                .find(|&l| self.items[l].kind == ItemKind::Bus)
                                .or_else(|| {
                                    hits.iter()
                                        .copied()
                                        .find(|&l| self.items[l].kind == ItemKind::Wire)
                                });
                            if let Some(l) = first {
                                extra.push((i, l));
                            }
                        }
                    }
                    ItemKind::Junction => {
                        let mids: Vec<usize> = line_index
                            .through(p)
                            .into_iter()
                            .filter(|&l| {
                                let (a, b) = (self.items[l].points[0], self.items[l].points[1]);
                                p != a && p != b
                            })
                            .collect();
                        map.entry(p).or_default().extend(mids);
                    }
                    _ => {}
                }
            }
            for (a, b) in extra {
                union(&mut parent, a, b);
            }
            for (pt, vec) in map.iter_mut() {
                vec.sort_unstable();
                vec.dedup();
                let bus_line = bus_index.through(*pt).first().copied();
                // Which items at one point join is decided by their kinds
                // alone (`ConnectionPropagatesTo` both ways, and the bus
                // entry rules), so instead of testing every pair, items of
                // a kind are chained and the kinds joined through one
                // representative each. Linear in the items at the point.
                // Wires join wires, buses join buses, and everything else
                // (pins, labels, junctions, flags, sheet pins, bus-bus
                // entries) joins all of those. A wire entry joins wires,
                // other entries and everything else when no bus passes the
                // point, and only bus-bus entries when one does (a wire
                // entry's net does not run into the bus it touches: A[7..0]
                // to A7 is a change of connection, not a join). A no-connect
                // pin joins nothing. This was checked against the pairwise
                // `ConnectionPropagatesTo` test on every fixture and on the
                // randomized corruption corpus.
                let (mut wires, mut bus_lines, mut entries, mut others, mut bus_entries) =
                    (Vec::new(), Vec::new(), Vec::new(), Vec::new(), Vec::new());
                for &ci in vec.iter() {
                    match &self.items[ci].kind {
                        ItemKind::Pin(p) if self.pins[*p].pin_type == PinType::NoConnect => {}
                        ItemKind::Wire => wires.push(ci),
                        ItemKind::Bus => bus_lines.push(ci),
                        ItemKind::WireEntry => entries.push(ci),
                        ItemKind::BusBusEntry => {
                            if let Some(b) = bus_line {
                                union(&mut parent, ci, b);
                            }
                            bus_entries.push(ci);
                            others.push(ci);
                        }
                        _ => others.push(ci),
                    }
                }
                let chain = |parent: &mut Vec<usize>, v: &[usize]| {
                    for w in v.windows(2) {
                        union(parent, w[0], w[1]);
                    }
                };
                chain(&mut parent, &wires);
                chain(&mut parent, &bus_lines);
                chain(&mut parent, &others);
                if let Some(&o) = others.first() {
                    for v in [&wires, &bus_lines] {
                        if let Some(&x) = v.first() {
                            union(&mut parent, o, x);
                        }
                    }
                }
                if bus_line.is_none() {
                    chain(&mut parent, &entries);
                    if let Some(&e) = entries.first() {
                        for v in [&wires, &others] {
                            if let Some(&x) = v.first() {
                                union(&mut parent, e, x);
                            }
                        }
                    }
                } else if let Some(&b) = bus_entries.first() {
                    for &e in &entries {
                        union(&mut parent, e, b);
                    }
                }
            }
        }
        // Jumpers: pins a library symbol declares internally connected.
        let mut pin_item: HashMap<usize, usize> = HashMap::new();
        for (i, it) in self.items.iter().enumerate() {
            if let ItemKind::Pin(p) = it.kind {
                pin_item.insert(p, i);
            }
        }
        for sym in &self.syms {
            let Some(lib) = &sym.lib else { continue };
            let mut groups: Vec<Vec<usize>> = Vec::new();
            if lib.duplicate_pin_numbers_are_jumpers {
                let mut by_num: BTreeMap<&str, Vec<usize>> = BTreeMap::new();
                for &p in &sym.pins {
                    by_num
                        .entry(self.pins[p].number.as_str())
                        .or_default()
                        .push(p);
                }
                groups.extend(by_num.into_values());
            }
            for grp in &lib.jumper_pin_groups {
                let members: Vec<usize> = sym
                    .pins
                    .iter()
                    .copied()
                    .filter(|&p| {
                        let num = &self.pins[p].number;
                        grp.contains(num) || stacked_numbers(num).iter().any(|n| grp.contains(n))
                    })
                    .collect();
                groups.push(members);
            }
            for grp in groups {
                for w in grp.windows(2) {
                    if let (Some(&a), Some(&b)) = (pin_item.get(&w[0]), pin_item.get(&w[1])) {
                        union(&mut parent, a, b);
                    }
                }
            }
        }
        // Components, in item order.
        let mut sg_of_root: HashMap<usize, usize> = HashMap::new();
        for i in 0..n {
            let r = find(&mut parent, i);
            let sg = *sg_of_root.entry(r).or_insert_with(|| {
                self.sgs.push(Subgraph {
                    sheet: self.items[i].sheet,
                    dirty: true,
                    ..Default::default()
                });
                self.sgs.len() - 1
            });
            self.sg_add_item(sg, i);
            if self.items[i].kind == ItemKind::NoConnect {
                self.sgs[sg].no_connect = Some(i);
            }
        }
    }

    fn sg_add_item(&mut self, sg: usize, item: usize) {
        let s = &mut self.sgs[sg];
        s.items.push(item);
        let it = &self.items[item];
        // Items are partitioned among subgraphs (an absorbed subgraph is
        // never absorbed again), so an item cannot already be a driver here;
        // no `contains` scan, which was quadratic in a net's labels.
        if it.is_driver() {
            s.drivers.push(item);
        }
        match it.kind {
            ItemKind::SheetPin { .. } => s.hier_pins.push(item),
            ItemKind::Label(LabelKind::Hierarchical, _) => s.hier_ports.push(item),
            _ => {}
        }
    }

    // ---- 2. drivers -------------------------------------------------------

    fn resolve_all(&mut self) {
        for sg in 0..self.sgs.len() {
            for &item in &self.sgs[sg].items.clone() {
                match &self.items[item].kind {
                    ItemKind::NoConnect => self.sgs[sg].no_connect = Some(item),
                    ItemKind::Pin(p) if self.pins[*p].pin_type == PinType::NoConnect => {
                        self.sgs[sg].no_connect = Some(item)
                    }
                    _ => {}
                }
            }
            self.resolve_drivers(sg);
            self.sgs[sg].dirty = false;
        }
        self.driver_sgs = (0..self.sgs.len())
            .filter(|&s| self.sgs[s].driver.is_some())
            .collect();
    }

    /// KiCad's candidate ranking: is `a` a better driver than `b`?
    fn better_driver(&self, sg: usize, a: usize, b: usize) -> bool {
        let (ac, bc) = (self.item_conn[a].as_ref(), self.item_conn[b].as_ref());
        if let (Some(ac), Some(bc)) = (ac, bc)
            && ac.is_bus()
            && bc.is_bus()
        {
            if bc.is_subset_of(ac) && !ac.is_subset_of(bc) {
                return true;
            }
            if !bc.is_subset_of(ac) && ac.is_subset_of(bc) {
                return false;
            }
        }
        if let (ItemKind::Pin(pa), ItemKind::Pin(pb)) = (&self.items[a].kind, &self.items[b].kind) {
            let ap = self.syms[self.pins[*pa].sym].power;
            let bp = self.syms[self.pins[*pb].sym].power;
            let (ag, bg) = (ap == Some(Power::Global), bp == Some(Power::Global));
            if ag != bg {
                return ag;
            }
            let (al, bl) = (ap == Some(Power::Local), bp == Some(Power::Local));
            if al != bl {
                return al;
            }
        }
        if let (ItemKind::SheetPin { shape: sa, .. }, ItemKind::SheetPin { shape: sb, .. }) =
            (&self.items[a].kind, &self.items[b].kind)
            && sa != sb
        {
            if sa == "output" {
                return true;
            }
            if sb == "output" {
                return false;
            }
        }
        let (an, bn) = (self.name_for(sg, a), self.name_for(sg, b));
        let (al, bl) = (an.contains("-Pad"), bn.contains("-Pad"));
        if al != bl {
            return !al;
        }
        an < bn
    }

    /// `CONNECTION_SUBGRAPH::ResolveDrivers`.
    fn resolve_drivers(&mut self, sg: usize) -> bool {
        let mut highest = Priority::Invalid;
        let mut candidates: Vec<usize> = Vec::new();
        let mut strong: Vec<usize> = Vec::new();
        for &d in &self.sgs[sg].drivers {
            let p = self.priority(Some(d));
            if p == Priority::Pin
                && let ItemKind::Pin(pi) = self.items[d].kind
                && !self.syms[self.pins[pi].sym].in_netlist()
            {
                continue;
            }
            if p >= Priority::HierLabel {
                strong.push(d);
            }
            if p > highest {
                candidates.clear();
                candidates.push(d);
                highest = p;
            } else if !candidates.is_empty() && p == highest {
                candidates.push(d);
            }
        }
        if highest >= Priority::HierLabel {
            self.sgs[sg].strong = true;
        }
        self.sgs[sg].local_driver = highest < Priority::GlobalPowerPin;
        candidates.sort_by(|&a, &b| {
            if self.better_driver(sg, a, b) {
                std::cmp::Ordering::Less
            } else if self.better_driver(sg, b, a) {
                std::cmp::Ordering::Greater
            } else {
                std::cmp::Ordering::Equal
            }
        });
        let driver = candidates.first().copied();
        self.sgs[sg].driver = driver;
        if strong.len() > 1 {
            self.sgs[sg].multiple = true;
        }
        if self.sgs[sg].strong {
            self.sgs[sg].drivers = strong;
        }
        if let Some(d) = driver {
            let name = self.name_for(sg, d);
            let kind = self.driver_kind(d);
            let aliases = std::mem::take(&mut self.aliases);
            if let Some(c) = self.item_conn[d].as_mut() {
                c.configure_from_label(&name, &aliases);
                c.set_driver(Some(kind));
            }
            self.aliases = aliases;
            self.sgs[sg].conn = Some(ConnRef::Item(d));
        } else if !self.sgs[sg].is_bus_member {
            self.sgs[sg].conn = None;
        }
        driver.is_some()
    }

    fn collect_driver_values(&mut self) {
        for &sg in &self.driver_sgs {
            let Some(c) = self.conn(sg) else { continue };
            let full = self.full(c);
            let is_vector = c.kind == Kind::Bus;
            self.name_map.entry(full.clone()).or_default().push(sg);
            if is_vector {
                let prefix_only = format!("{}[]", full.split('[').next().unwrap_or(""));
                self.name_map.entry(prefix_only).or_default().push(sg);
            }
        }
        for &sg in &self.driver_sgs.clone() {
            self.sgs[sg].dirty = true;
        }
    }

    fn generate_bus_alias_members(&mut self) {
        let mut new_sgs = Vec::new();
        for &sg in &self.driver_sgs.clone() {
            let labels: Vec<usize> = self.sgs[sg]
                .drivers
                .iter()
                .copied()
                .filter(|&d| {
                    matches!(self.items[d].kind, ItemKind::Label(..))
                        && self.item_conn[d].as_ref().is_some_and(Conn::is_bus)
                })
                .collect();
            for item in labels {
                let ItemKind::Label(_, text) = &self.items[item].kind else {
                    continue;
                };
                let sheet = self.sgs[sg].sheet;
                let mut dummy = Conn::new(sheet);
                dummy.configure_from_label(text, &self.aliases);
                for m in dummy.members.iter().filter(|m| m.is_net()) {
                    let name = m.full_local_name();
                    let mut c = Conn::new(sheet);
                    c.name = name.clone();
                    c.kind = Kind::Net;
                    self.implicit.push(c);
                    let r = ConnRef::Implicit(self.implicit.len() - 1);
                    self.sgs.push(Subgraph {
                        sheet,
                        conn: Some(r),
                        is_bus_member: true,
                        strong: true,
                        dirty: true,
                        ..Default::default()
                    });
                    let new_sg = self.sgs.len() - 1;
                    self.name_map.entry(name).or_default().push(new_sg);
                    new_sgs.push(new_sg);
                }
            }
        }
        self.driver_sgs.extend(new_sgs);
    }

    // ---- 3. same-sheet processing -----------------------------------------

    fn default_connection(&self, item: usize, sg: usize) -> Option<Conn> {
        let ok = match &self.items[item].kind {
            ItemKind::Pin(p) => self.pin_is_power(*p),
            ItemKind::Label(k, _) => *k != LabelKind::Directive,
            _ => false,
        };
        if !ok {
            return None;
        }
        let mut c = Conn::new(self.sgs[sg].sheet);
        c.configure_from_label(&self.name_for(sg, item), &self.aliases);
        Some(c)
    }

    fn connections_to_check(&self, sg: usize, out: &mut Vec<Conn>) {
        let Some(dc) = self.conn(sg) else { return };
        for &item in &self.sgs[sg].items {
            if Some(item) == self.sgs[sg].driver {
                continue;
            }
            if let Some(c) = self.default_connection(item, sg) {
                if c.kind != dc.kind || c.short_name() == dc.short_name() {
                    continue;
                }
                out.push(c);
            }
        }
    }

    fn absorb(&mut self, into: usize, other: usize) {
        for item in self.sgs[other].items.clone() {
            self.sg_add_item(into, item);
        }
        let mut absorbed = vec![other];
        absorbed.extend(self.sgs[other].absorbed_list.clone());
        self.sgs[into]
            .absorbed_list
            .extend(absorbed.iter().copied());
        for (k, set) in self.sgs[other].bus_neighbors.clone() {
            for s in set {
                link(&mut self.sgs[into].bus_neighbors, &k, s);
            }
        }
        for (k, set) in self.sgs[other].bus_parents.clone() {
            for s in set {
                link(&mut self.sgs[into].bus_parents, &k, s);
            }
        }
        self.sgs[into].multiple |= self.sgs[other].multiple;
        let o = &mut self.sgs[other];
        o.absorbed = true;
        o.dirty = false;
        o.driver = None;
        o.conn = None;
        for a in absorbed {
            self.sgs[a].absorbed_by = Some(into);
        }
    }

    fn root_of(&self, mut sg: usize) -> usize {
        let mut guard = 0;
        while let Some(by) = self.sgs[sg].absorbed_by {
            if !self.sgs[sg].absorbed || guard > self.sgs.len() {
                break;
            }
            sg = by;
            guard += 1;
        }
        sg
    }

    fn rename_weak(&mut self, sg: usize, base_full: &str) -> String {
        let mut suffix = 1u32;
        loop {
            let aliases = std::mem::take(&mut self.aliases);
            let c = self.conn_mut(sg).expect("driver subgraph has a connection");
            if c.kind == Kind::Group {
                let mut prefix = c.bus_prefix.clone();
                let plen = c.bus_prefix.chars().count();
                if prefix.is_empty() {
                    prefix = "BUS".into();
                }
                let members: String = base_full.chars().skip(plen).collect();
                let new = format!("{prefix}_{suffix}{members}");
                c.configure_from_label(&new, &aliases);
            } else {
                c.suffix = format!("_{suffix}");
            }
            self.aliases = aliases;
            suffix += 1;
            let name = self.sg_name(sg);
            if !self.name_map.contains_key(&name) || suffix > 10_000 {
                return name;
            }
        }
    }

    fn promote_sheet_pin_driver(&mut self, sg: usize) {
        let Some(d) = self.sgs[sg].driver else { return };
        if !matches!(self.items[d].kind, ItemKind::SheetPin { .. }) {
            return;
        }
        let global = self.conn(sg).map(Conn::short_name).unwrap_or_default();
        if let Some(v) = self.name_map.get(&global)
            && v.iter().any(|&c| self.sgs[c].sheet == self.sgs[sg].sheet)
        {
            return;
        }
        self.sgs[sg].strong = true;
    }

    /// The names a subgraph can be matched by in [`Self::process_subgraphs`]:
    /// its connection's short name, and the names of the drivers among
    /// `items` (a superset of what the match tests; the match itself is then
    /// tested exactly).
    fn match_names(&self, sg: usize, items: &[usize]) -> Vec<String> {
        let mut out: Vec<String> = self.conn(sg).map(Conn::short_name).into_iter().collect();
        for &d in items {
            match &self.items[d].kind {
                ItemKind::Pin(p) if self.pin_is_power(*p) => {
                    out.push(self.pins[*p].default_names[0].clone())
                }
                ItemKind::Label(..) => out.push(self.name_for(sg, d)),
                _ => {}
            }
        }
        out
    }

    fn process_subgraphs(&mut self) {
        self.sheet_sgs.clear();
        for &sg in &self.driver_sgs {
            self.sheet_sgs
                .entry(self.sgs[sg].sheet)
                .or_default()
                .push(sg);
        }
        // (sheet, name) to the subgraphs that may match that name, so that
        // each bus member or label looks up its few candidates instead of
        // testing every subgraph on the sheet. Entries are only ever added;
        // a stale one costs an exact test that fails.
        let mut by_name: HashMap<(usize, String), Vec<usize>> = HashMap::new();
        for &sg in &self.driver_sgs {
            let sheet = self.sgs[sg].sheet;
            for n in self.match_names(sg, &self.sgs[sg].drivers) {
                by_name.entry((sheet, n)).or_default().push(sg);
            }
        }
        let mut invalidated: Vec<usize> = Vec::new();
        let mut invalidated_set: HashSet<usize> = HashSet::new();
        for sg in self.driver_sgs.clone() {
            if self.sgs[sg].absorbed {
                continue;
            }
            let Some(c) = self.conn(sg) else { continue };
            let mut name = self.full(c);
            let is_vector = c.kind == Kind::Bus;
            if !self.sgs[sg].strong {
                let mut key = name.clone();
                let mut count = self.name_map.get(&key).map_or(0, Vec::len);
                if count <= 1 && is_vector {
                    let prefix_only = format!("{}[]", name.split('[').next().unwrap_or(""));
                    if let Some(v) = self.name_map.get(&prefix_only) {
                        key = prefix_only;
                        count = v.len();
                    }
                }
                if count > 1 {
                    let new_name = self.rename_weak(sg, &name);
                    if let Some(v) = self.name_map.get_mut(&key) {
                        v.retain(|&x| x != sg);
                    }
                    self.name_map.entry(new_name.clone()).or_default().push(sg);
                    by_name
                        .entry((self.sgs[sg].sheet, self.sg_name_short(sg)))
                        .or_default()
                        .push(sg);
                    name = new_name;
                    self.promote_sheet_pin_driver(sg);
                } else if self.sgs[sg].driver.is_some() {
                    self.promote_sheet_pin_driver(sg);
                }
            }
            let _ = name;
            self.sgs[sg].dirty = true;
            if !self.sgs[sg].strong {
                continue;
            }
            let sheet = self.sgs[sg].sheet;
            let mut to_check: Vec<Conn> = vec![self.conn(sg).cloned().expect("checked above")];
            self.connections_to_check(sg, &mut to_check);
            let mut i = 0;
            while i < to_check.len() {
                let member = to_check[i].clone();
                i += 1;
                if to_check.len() > 100_000 {
                    self.approximate
                        .insert("bus expansion was cut off at 100,000 members".into());
                    break;
                }
                if member.is_bus() {
                    to_check.extend(member.members.iter().cloned());
                }
                let test_name = member.short_name();
                // The strong, unabsorbed subgraphs on this sheet that may
                // match, in subgraph order (the order `driver_sgs` lists
                // them, which is ascending).
                let mut candidates: Vec<usize> = by_name
                    .get(&(sheet, test_name.clone()))
                    .map(|v| v.iter().copied().filter(|&c| self.sgs[c].strong).collect())
                    .unwrap_or_default();
                candidates.sort_unstable();
                candidates.dedup();
                for cand in candidates {
                    if self.sgs[cand].absorbed || cand == sg {
                        continue;
                    }
                    let Some(cc) = self.conn(cand) else { continue };
                    let mut matched = cc.short_name() == test_name;
                    if !matched && self.sgs[cand].multiple {
                        for &d in &self.sgs[cand].drivers {
                            if Some(d) == self.sgs[cand].driver {
                                continue;
                            }
                            match &self.items[d].kind {
                                ItemKind::SheetPin { .. } => continue,
                                ItemKind::Pin(p) => {
                                    if self.pin_is_power(*p)
                                        && self.pins[*p].default_names[0] == test_name
                                    {
                                        matched = true;
                                        break;
                                    }
                                }
                                _ => {
                                    if self.name_for(sg, d) == test_name {
                                        matched = true;
                                        break;
                                    }
                                }
                            }
                        }
                    }
                    if !matched {
                        continue;
                    }
                    let mine = self.conn(sg).expect("strong subgraph has a connection");
                    let theirs = self.conn(cand).expect("candidate has a connection");
                    if mine.is_bus() && theirs.is_net() {
                        link(&mut self.sgs[sg].bus_neighbors, &member, cand);
                        link(&mut self.sgs[cand].bus_parents, &member, sg);
                    } else if (!mine.is_bus() && !theirs.is_bus()) || mine.kind == theirs.kind {
                        self.connections_to_check(cand, &mut to_check);
                        let moved: Vec<usize> = self.sgs[cand]
                            .items
                            .iter()
                            .copied()
                            .filter(|&i| self.items[i].is_driver())
                            .collect();
                        for n in self.match_names(cand, &moved) {
                            by_name.entry((sheet, n)).or_default().push(sg);
                        }
                        self.absorb(sg, cand);
                        if invalidated_set.insert(sg) {
                            invalidated.push(sg);
                        }
                    }
                }
            }
        }
        for sg in invalidated {
            if self.sgs[sg].absorbed {
                continue;
            }
            self.resolve_drivers(sg);
        }
    }

    // ---- 4 and 5. global names and the hierarchy ---------------------------

    fn recache(&mut self, sg: usize, old: &str) {
        if let Some(v) = self.name_map.get_mut(old) {
            v.retain(|&x| x != sg);
        }
        let new = self.sg_name(sg);
        self.name_map.entry(new).or_default().push(sg);
    }

    fn clone_conn_into(&mut self, sg: usize, src: &Conn) {
        if let Some(c) = self.conn_mut(sg) {
            c.clone_from_conn(src);
        }
    }

    fn finish(&mut self) {
        self.driver_sgs.retain(|&s| !self.sgs[s].absorbed);
        let global: Vec<usize> = self
            .driver_sgs
            .iter()
            .copied()
            .filter(|&s| !self.sgs[s].local_driver)
            .collect();
        self.sheet_sgs.clear();
        for &sg in &self.driver_sgs {
            self.sheet_sgs
                .entry(self.sgs[sg].sheet)
                .or_default()
                .push(sg);
        }

        // Global equivalence classes over shared global names.
        if !global.is_empty() {
            let mut root: HashMap<usize, usize> = HashMap::new();
            fn find(root: &mut HashMap<usize, usize>, mut x: usize) -> usize {
                while let Some(&p) = root.get(&x) {
                    if p == x {
                        break;
                    }
                    x = p;
                }
                x
            }
            let mut by_name: BTreeMap<String, Vec<usize>> = BTreeMap::new();
            for &sg in &global {
                for &d in &self.sgs[sg].drivers {
                    if self.priority(Some(d)) < Priority::GlobalPowerPin {
                        continue;
                    }
                    let v = by_name.entry(self.name_for(sg, d)).or_default();
                    if !v.contains(&sg) {
                        v.push(sg);
                    }
                }
            }
            for sgs in by_name.values() {
                if sgs.len() < 2 {
                    continue;
                }
                for &other in &sgs[1..] {
                    root.entry(sgs[0]).or_insert(sgs[0]);
                    root.entry(other).or_insert(other);
                    let (ra, rb) = (find(&mut root, sgs[0]), find(&mut root, other));
                    if ra == rb {
                        continue;
                    }
                    let (pa, pb) = (
                        self.priority(self.sgs[ra].driver),
                        self.priority(self.sgs[rb].driver),
                    );
                    let a_wins = if pa != pb {
                        pa > pb
                    } else {
                        self.sg_name(ra) < self.sg_name(rb)
                    };
                    if a_wins {
                        root.insert(rb, ra);
                    } else {
                        root.insert(ra, rb);
                    }
                }
            }
            let members: Vec<usize> = root.keys().copied().collect();
            for sg in members {
                let r = find(&mut root, sg);
                if r == sg || self.sg_name(sg) == self.sg_name(r) {
                    continue;
                }
                let src = self
                    .conn(r)
                    .cloned()
                    .expect("global subgraph has a connection");
                self.clone_conn_into(sg, &src);
            }
        }

        for sg in self.driver_sgs.clone() {
            if !self.sgs[sg].dirty {
                continue;
            }
            if !self.sgs[sg].local_driver && self.sgs[sg].multiple {
                for d in self.sgs[sg].drivers.clone() {
                    if Some(d) == self.sgs[sg].driver {
                        continue;
                    }
                    let secondary = self.name_for(sg, d);
                    if secondary == self.sg_name(sg) {
                        continue;
                    }
                    let secondary_global = self.priority(Some(d)) >= Priority::GlobalPowerPin;
                    for &cand in &global {
                        if cand == sg {
                            continue;
                        }
                        if !secondary_global && self.sgs[cand].sheet != self.sgs[sg].sheet {
                            continue;
                        }
                        let hit = self.sgs[cand]
                            .drivers
                            .iter()
                            .any(|&cd| self.name_for(cand, cd) == secondary);
                        if hit {
                            let src = self
                                .conn(sg)
                                .cloned()
                                .expect("driver subgraph has a connection");
                            self.clone_conn_into(cand, &src);
                            self.sgs[cand].dirty = false;
                            self.propagate(cand, false);
                        }
                    }
                }
            }
            self.propagate(sg, false);
        }
        for sg in self.driver_sgs.clone() {
            if self.sgs[sg].dirty {
                self.propagate(sg, true);
            }
        }
        self.link_multiple_bus_parents();

        // Final names.
        for sg in self.driver_sgs.clone() {
            let s = &self.sgs[sg];
            if !s.strong
                && s.drivers.len() == 1
                && let Some(d) = s.driver
                && let ItemKind::Pin(p) = self.items[d].kind
            {
                let name = self.pins[p].default_names[1].clone();
                let aliases = std::mem::take(&mut self.aliases);
                if let Some(c) = self.conn_mut(sg) {
                    c.configure_from_label(&name, &aliases);
                }
                self.aliases = aliases;
            }
            self.sgs[sg].dirty = false;
        }
    }

    fn visit(&mut self, parent: usize, visited: &HashSet<usize>, search: &mut Vec<usize>) {
        for pin in self.sgs[parent].hier_pins.clone() {
            let ItemKind::SheetPin {
                child: Some(child), ..
            } = self.items[pin].kind
            else {
                continue;
            };
            let pin_name = self.name_for(parent, pin);
            let Some(cands) = self.sheet_sgs.get(&child).cloned() else {
                continue;
            };
            for cand in cands {
                if !self.sgs[cand].strong
                    || self.sgs[cand].hier_ports.is_empty()
                    || visited.contains(&cand)
                {
                    continue;
                }
                let hit = self.sgs[cand]
                    .hier_ports
                    .iter()
                    .any(|&l| self.name_for(cand, l) == pin_name);
                if hit {
                    search.push(cand);
                }
            }
        }
        let parent_sheet = self.project.sheets[self.sgs[parent].sheet].parent;
        for label in self.sgs[parent].hier_ports.clone() {
            let Some(up) = parent_sheet else { continue };
            let label_name = self.name_for(parent, label);
            let Some(cands) = self.sheet_sgs.get(&up).cloned() else {
                continue;
            };
            let parent_kind = self.conn(parent).map(|c| c.kind);
            for cand in cands {
                if self.sgs[cand].hier_pins.is_empty()
                    || visited.contains(&cand)
                    || self.conn(cand).map(|c| c.kind) != parent_kind
                {
                    continue;
                }
                let hit = self.sgs[cand].hier_pins.iter().any(|&p| {
                    matches!(self.items[p].kind, ItemKind::SheetPin { child: Some(c), .. } if c == self.sgs[parent].sheet)
                        && self.name_for(cand, p) == label_name
                });
                if hit {
                    search.push(cand);
                }
            }
        }
    }

    /// `propagateToNeighbors`.
    fn propagate(&mut self, sg: usize, force: bool) {
        if self.depth >= MAX_PROPAGATION_DEPTH {
            self.approximate
                .insert("hierarchy propagation was cut off (too deep)".into());
            return;
        }
        self.depth += 1;
        self.propagate_inner(sg, force);
        self.depth -= 1;
    }

    fn propagate_inner(&mut self, sg: usize, force: bool) {
        let Some(conn) = self.conn(sg).cloned() else {
            self.sgs[sg].dirty = false;
            return;
        };
        let mut stale: Vec<(usize, Vec<usize>)> = Vec::new();
        if conn.is_bus() {
            self.propagate_bus_neighbors(sg, force, &mut stale);
        }
        let (ports, pins) = (
            self.sgs[sg].hier_ports.is_empty(),
            self.sgs[sg].hier_pins.is_empty(),
        );
        if !force && !ports && !pins {
            return;
        } else if ports && pins {
            self.sgs[sg].dirty = false;
            return;
        }
        let mut visited_set: HashSet<usize> = HashSet::new();
        let mut visited: Vec<usize> = Vec::new();
        visited_set.insert(sg);
        visited.push(sg);
        let mut search = Vec::new();
        self.visit(sg, &visited_set, &mut search);
        let mut i = 0;
        while i < search.len() {
            let child = search[i];
            i += 1;
            if visited_set.insert(child) {
                visited.push(child);
                self.visit(child, &visited_set, &mut search);
            }
            self.sgs[child].dirty = false;
        }

        let mut best = sg;
        let mut highest = self.priority(self.sgs[sg].driver);
        let mut best_strong = highest >= Priority::HierLabel;
        let mut best_name = self.sg_name(sg);
        if highest < Priority::GlobalPowerPin {
            for &other in &visited {
                if other == sg {
                    continue;
                }
                let p = self.priority(self.sgs[other].driver);
                let strong = p >= Priority::HierLabel;
                let name = self.sg_name(other);
                let depth = |s: usize| self.project.sheets[self.sgs[s].sheet].uuids.len();
                let shorter = depth(other) < depth(best);
                let as_good = depth(other) <= depth(best);
                if p >= Priority::GlobalPowerPin
                    || (!best_strong && strong)
                    || (p > highest && strong)
                    || (p == highest && strong && shorter)
                    || (best_strong == strong && as_good && p == highest && name < best_name)
                {
                    best = other;
                    highest = p;
                    best_strong = strong;
                    best_name = name;
                }
            }
        }
        let src = self
            .conn(best)
            .cloned()
            .expect("visited subgraphs have connections");
        for &v in &visited {
            let old = self.sg_name(v);
            self.clone_conn_into(v, &src);
            if old != self.full(&src) {
                self.recache(v, &old);
            }
            if src.is_bus() {
                self.propagate_bus_neighbors(v, force, &mut stale);
            }
        }
        if src.is_bus() && !stale.is_empty() {
            let cached = stale.clone();
            let mut scratch = Vec::new();
            for (owner, path) in cached {
                let Some(stale_member) =
                    self.conn(owner).and_then(|c| member_at(c, &path)).cloned()
                else {
                    continue;
                };
                for &v in &visited {
                    let Some(vc) = self.conn(v) else { continue };
                    let Some(mp) = match_bus_member(vc, &stale_member) else {
                        continue;
                    };
                    if let Some(m) = self.conn_mut(v).and_then(|c| member_at_mut(c, &mp)) {
                        m.clone_from_conn(&stale_member);
                    }
                    self.propagate_bus_neighbors(v, force, &mut scratch);
                }
            }
        }
        self.sgs[sg].dirty = false;
    }

    fn propagate_bus_neighbors(
        &mut self,
        parent: usize,
        force: bool,
        stale: &mut Vec<(usize, Vec<usize>)>,
    ) {
        let mut keys: Vec<(Conn, BTreeSet<usize>)> = self.sgs[parent].bus_neighbors.clone();
        keys.sort_by_key(|(k, _)| self.full(k));
        for (key, neighbors) in keys {
            for neighbor in neighbors.iter().copied() {
                let n = self.root_of(neighbor);
                let Some(pc) = self.conn(parent).cloned() else {
                    return;
                };
                let mut path = match_bus_member(&pc, &key);
                if path.is_none() {
                    'outer: for &s2 in &neighbors {
                        if self.sgs[s2].multiple {
                            for &d in &self.sgs[s2].drivers {
                                if let Some(c) = self.default_connection(d, s2)
                                    && let Some(p) = match_bus_member(&pc, &c)
                                {
                                    path = Some(p);
                                    break 'outer;
                                }
                            }
                        }
                    }
                }
                let Some(path) = path else { continue };
                let Some(member) = member_at(&pc, &path).cloned() else {
                    continue;
                };
                let Some(nc) = self.conn(n).cloned() else {
                    continue;
                };
                let neighbor_name = self.full(&nc);
                if neighbor_name == self.full(&member) {
                    continue;
                }
                if nc.sheet != self.sgs[n].sheet {
                    if nc.sheet != pc.sheet {
                        continue;
                    }
                    if pc.members.iter().any(|m| self.full(m) == neighbor_name) {
                        continue;
                    }
                }
                if !nc.is_net() {
                    continue;
                }
                if self.priority(self.sgs[n].driver) >= Priority::GlobalPowerPin {
                    if let Some(m) = self.conn_mut(parent).and_then(|c| member_at_mut(c, &path)) {
                        m.clone_from_conn(&nc);
                    }
                    stale.push((parent, path));
                } else {
                    self.clone_conn_into(n, &member);
                    self.recache(n, &neighbor_name);
                    self.sgs[n].dirty = true;
                    self.propagate(n, force);
                    let now = self.conn(n).cloned();
                    let member_now = self.conn(parent).and_then(|c| member_at(c, &path)).cloned();
                    if let (Some(now), Some(mn)) = (now, member_now)
                        && self.full(&now) != self.full(&mn)
                    {
                        if let Some(m) = self.conn_mut(parent).and_then(|c| member_at_mut(c, &path))
                        {
                            m.clone_from_conn(&now);
                        }
                        stale.push((parent, path));
                    }
                }
            }
        }
    }

    fn link_multiple_bus_parents(&mut self) {
        for sg in self.driver_sgs.clone() {
            // KiCad keys `m_bus_parents` by member object, one per parent
            // bus; `link` folds equal members together, so count parents.
            if self.sgs[sg]
                .bus_parents
                .iter()
                .map(|(_, set)| set.len())
                .sum::<usize>()
                < 2
            {
                continue;
            }
            let Some(conn) = self.conn(sg).cloned() else {
                continue;
            };
            if !conn.is_net() {
                continue;
            }
            for (link_member, parents) in self.sgs[sg].bus_parents.clone() {
                for parent in parents {
                    let parent = self.root_of(parent);
                    let Some(pc) = self.conn(parent).cloned() else {
                        continue;
                    };
                    let pname = self.full(&pc);
                    let mut bus_sgs = vec![parent];
                    if let Some(v) = self.name_map.get(&pname) {
                        for &c in v {
                            let c = self.root_of(c);
                            if c != parent
                                && self
                                    .conn(c)
                                    .is_some_and(|cc| cc.is_bus() && self.full(cc) == pname)
                                && !bus_sgs.contains(&c)
                            {
                                bus_sgs.push(c);
                            }
                        }
                    }
                    for bus_sg in bus_sgs {
                        let Some(bc) = self.conn(bus_sg).cloned() else {
                            continue;
                        };
                        let Some(mp) = match_bus_member(&bc, &link_member) else {
                            continue;
                        };
                        let Some(m) = member_at(&bc, &mp) else {
                            continue;
                        };
                        if self.full(&conn) == self.full(m) {
                            continue;
                        }
                        let old_name = self.full(m);
                        let mut stale_sgs = Vec::new();
                        for (k, nbs) in &self.sgs[bus_sg].bus_neighbors {
                            if match_bus_member(&bc, k).as_ref() != Some(&mp) {
                                continue;
                            }
                            for &nb in nbs {
                                if self.priority(self.sgs[nb].driver) < Priority::GlobalPowerPin {
                                    stale_sgs.push(nb);
                                }
                            }
                        }
                        if let Some(m) = self.conn_mut(bus_sg).and_then(|c| member_at_mut(c, &mp)) {
                            m.clone_from_conn(&conn);
                        }
                        if let Some(v) = self.name_map.get(&old_name) {
                            stale_sgs.extend(v.iter().copied());
                        }
                        for old in stale_sgs {
                            let old = self.root_of(old);
                            let before = self.sg_name(old);
                            self.clone_conn_into(old, &conn);
                            if before != self.sg_name(old) {
                                self.recache(old, &before);
                            }
                        }
                    }
                }
            }
        }
    }

    fn into_result(self) -> Connectivity {
        let mut by_name: BTreeMap<String, usize> = BTreeMap::new();
        let mut nets: Vec<Net> = Vec::new();
        let mut pin_net = HashMap::new();
        let mut any_bus = false;
        for &sg in &self.driver_sgs {
            let s = &self.sgs[sg];
            // Every driven subgraph, virtual bus members and buses included:
            // KiCad numbers its netlist's nets over all of these.
            let name = self.sg_name(sg);
            let bus = self.conn(sg).is_some_and(Conn::is_bus);
            any_bus |= bus;
            let idx = *by_name.entry(name.clone()).or_insert_with(|| {
                nets.push(Net {
                    name,
                    pins: Vec::new(),
                    has_no_connect: false,
                    bus,
                });
                nets.len() - 1
            });
            if s.no_connect
                .is_some_and(|n| self.items[n].kind == ItemKind::NoConnect)
            {
                nets[idx].has_no_connect = true;
            }
            for &item in &s.items {
                if let ItemKind::Pin(p) = self.items[item].kind {
                    nets[idx].pins.push(p);
                    pin_net.insert(p, idx);
                }
            }
        }
        let mut approximate: Vec<String> = self.approximate.into_iter().collect();
        if any_bus {
            approximate.push(
                "the design uses buses; bus resolution follows KiCad's algorithm and matches KiCad 10 on every bus \
                 fixture tested, but has had less real-world exposure than the rest of the engine"
                    .into(),
            );
        }
        Connectivity {
            paths: self.paths,
            symbols: self.syms,
            pins: self.pins,
            nets,
            pin_net,
            approximate,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stacked() {
        assert_eq!(stacked_numbers("1"), vec!["1"]);
        assert_eq!(stacked_numbers("[1,3-5]"), vec!["1", "3", "4", "5"]);
        assert_eq!(effective_pad_number("[7,2-3]"), "2");
        assert_eq!(stacked_numbers("[A1,B2]"), vec!["A1", "B2"]);
    }

    #[test]
    fn kiid_hash_matches_kicad() {
        // From kicad-cli 10.0.7's netlist of test_multiunit_reannotate_4.
        assert_eq!(
            kiid_hash32("549455c3-ab6e-454e-94b0-5ca9e521ae0b"),
            0xd986_524f
        );
    }

    #[test]
    fn segments() {
        assert!(on_segment(Pt::new(5, 0), Pt::new(0, 0), Pt::new(10, 0)));
        assert!(!on_segment(Pt::new(5, 1), Pt::new(0, 0), Pt::new(10, 0)));
        assert!(!on_segment(Pt::new(11, 0), Pt::new(0, 0), Pt::new(10, 0)));
    }
}
