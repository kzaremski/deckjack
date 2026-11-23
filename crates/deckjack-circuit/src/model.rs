//! A typed view of one `.kicad_sch` file.
//!
//! The model is read *from* the lossless tree in `sexpr.rs` and never written
//! back as a whole. Every item remembers the index of its node among the root
//! list's children, and edits (`edit.rs`) change those nodes in place. So the
//! typed side can ignore whatever it does not understand without that ever
//! costing a byte on save: unknown and newer constructs simply stay in the
//! tree.
//!
//! Coordinates are integers in KiCad's schematic internal unit, 100 nm
//! (10,000 per millimeter), so that "the pin lands on the wire end" is an
//! exact comparison, as it is in KiCad.
//!
//! Formats covered: KiCad 6 through 10 (`version` 20200310 through the
//! newest token in [`NEWEST_KNOWN_VERSION`]), including the KiCad 6-era
//! `symbol_instances`/`sheet_instances` tables and the per-item `instances`
//! blocks that replaced them in KiCad 7.

use std::collections::BTreeMap;

use crate::sexpr::{Document, List, Node};

/// The newest schematic format token this code has been checked against
/// (KiCad 10.0, from a 10.0-saved file in the fixture corpus).
pub const NEWEST_KNOWN_VERSION: u32 = 20260306;
/// The oldest S-expression schematic token. Earlier files are KiCad 5's
/// legacy format, which is a different syntax altogether.
pub const OLDEST_KNOWN_VERSION: u32 = 20200310;

/// Internal units per millimeter.
pub const IU_PER_MM: f64 = 10_000.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct Pt {
    pub x: i64,
    pub y: i64,
}

impl Pt {
    pub fn new(x: i64, y: i64) -> Pt {
        Pt { x, y }
    }
}

/// Parse a millimeter value into internal units.
pub fn mm(text: &str) -> Option<i64> {
    let v: f64 = text.parse().ok()?;
    if !v.is_finite() || v.abs() > 1e9 {
        return None;
    }
    Some((v * IU_PER_MM).round() as i64)
}

/// Format internal units as KiCad writes millimeters: up to four decimals,
/// trailing zeros dropped.
pub fn fmt_mm(iu: i64) -> String {
    let neg = iu < 0;
    let a = iu.unsigned_abs();
    let whole = a / 10_000;
    let frac = a % 10_000;
    let mut s = String::new();
    if neg && (whole != 0 || frac != 0) {
        s.push('-');
    }
    s.push_str(&whole.to_string());
    if frac != 0 {
        let f = format!("{frac:04}");
        s.push('.');
        s.push_str(f.trim_end_matches('0'));
    }
    s
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PinType {
    Input,
    Output,
    Bidirectional,
    TriState,
    Passive,
    Free,
    Unspecified,
    PowerIn,
    PowerOut,
    OpenCollector,
    OpenEmitter,
    NoConnect,
}

impl PinType {
    pub fn parse(s: &str) -> PinType {
        match s {
            "input" => PinType::Input,
            "output" => PinType::Output,
            "bidirectional" => PinType::Bidirectional,
            "tri_state" => PinType::TriState,
            "passive" => PinType::Passive,
            "free" => PinType::Free,
            "power_in" => PinType::PowerIn,
            "power_out" => PinType::PowerOut,
            "open_collector" => PinType::OpenCollector,
            "open_emitter" => PinType::OpenEmitter,
            "no_connect" | "unconnected" => PinType::NoConnect,
            _ => PinType::Unspecified,
        }
    }

    /// The name KiCad's netlist uses (`GetCanonicalElectricalTypeName`).
    pub fn canonical(self) -> &'static str {
        match self {
            PinType::Input => "input",
            PinType::Output => "output",
            PinType::Bidirectional => "bidirectional",
            PinType::TriState => "tri_state",
            PinType::Passive => "passive",
            PinType::Free => "free",
            PinType::Unspecified => "unspecified",
            PinType::PowerIn => "power_in",
            PinType::PowerOut => "power_out",
            PinType::OpenCollector => "open_collector",
            PinType::OpenEmitter => "open_emitter",
            PinType::NoConnect => "no_connect",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Power {
    Global,
    /// KiCad 9's `(power local)`: a power symbol whose net is sheet-local.
    Local,
}

/// A pin in a library symbol, in the symbol's own coordinates with the Y
/// axis pointing down (the file stores it up; it is flipped on read, as
/// KiCad does).
#[derive(Debug, Clone, PartialEq)]
pub struct LibPin {
    pub number: String,
    pub name: String,
    pub pin_type: PinType,
    pub pos: Pt,
    /// The pin's orientation in the library, in degrees: the direction it
    /// extends from its connection point toward the symbol body (0 is to the
    /// right, 90 up, in the library's Y-up axes).
    pub angle: i32,
    pub hidden: bool,
    pub alternates: Vec<(String, PinType)>,
}

impl LibPin {
    /// KiCad's `GetShownName`: a name of `~` means no name.
    pub fn shown_name(&self) -> &str {
        if self.name == "~" { "" } else { &self.name }
    }
}

/// One `(symbol "NAME_U_B" ...)` body inside a library symbol.
#[derive(Debug, Clone, PartialEq)]
pub struct LibUnit {
    /// 0 means common to every unit.
    pub unit: u32,
    /// 0 means common to every body style.
    pub body_style: u32,
    pub pins: Vec<LibPin>,
    pub unit_name: Option<String>,
}

/// A symbol from the sheet's embedded `lib_symbols` cache.
#[derive(Debug, Clone, PartialEq)]
pub struct LibSymbol {
    /// Index of the node among the `lib_symbols` list's children.
    pub index: usize,
    pub name: String,
    pub extends: Option<String>,
    pub power: Option<Power>,
    pub properties: Vec<(String, String)>,
    pub units: Vec<LibUnit>,
    pub duplicate_pin_numbers_are_jumpers: bool,
    pub jumper_pin_groups: Vec<Vec<String>>,
    pub footprint_filters: Vec<String>,
}

impl LibSymbol {
    pub fn property(&self, name: &str) -> Option<&str> {
        self.properties
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
    }

    /// The number of units (A, B, ...), at least 1.
    pub fn unit_count(&self) -> u32 {
        self.units.iter().map(|u| u.unit).max().unwrap_or(0).max(1)
    }

    /// The pins a placed symbol of this unit and body style has.
    pub fn pins_for(&self, unit: u32, body_style: u32) -> impl Iterator<Item = &LibPin> {
        self.units
            .iter()
            .filter(move |u| {
                (u.unit == 0 || u.unit == unit) && (u.body_style == 0 || u.body_style == body_style)
            })
            .flat_map(|u| u.pins.iter())
    }

    /// Every pin in every unit.
    pub fn all_pins(&self) -> impl Iterator<Item = (&LibUnit, &LibPin)> {
        self.units
            .iter()
            .flat_map(|u| u.pins.iter().map(move |p| (u, p)))
    }
}

/// A field of a placed symbol or sheet.
#[derive(Debug, Clone, PartialEq)]
pub struct Field {
    pub name: String,
    pub value: String,
}

/// One entry of a symbol's (or sheet's) `instances` block.
#[derive(Debug, Clone, PartialEq)]
pub struct Instance {
    pub project: String,
    /// The sheet path, `/root-uuid/sheet-uuid/...`.
    pub path: String,
    pub reference: Option<String>,
    pub unit: Option<u32>,
    pub page: Option<String>,
    pub value: Option<String>,
    pub footprint: Option<String>,
}

/// A symbol placed on the sheet.
#[derive(Debug, Clone, PartialEq)]
pub struct Symbol {
    /// Index among the root list's children.
    pub index: usize,
    pub lib_id: String,
    pub lib_name: Option<String>,
    pub at: Pt,
    pub rotation: i32,
    pub mirror_x: bool,
    pub mirror_y: bool,
    pub unit: u32,
    pub body_style: u32,
    pub in_bom: bool,
    pub on_board: bool,
    pub dnp: bool,
    pub uuid: String,
    pub fields: Vec<Field>,
    /// Pin number to alternate function name, from the instance's `(pin ...)`
    /// entries.
    pub alternates: BTreeMap<String, String>,
    pub instances: Vec<Instance>,
}

impl Symbol {
    /// The key into `lib_symbols`.
    pub fn cache_key(&self) -> &str {
        self.lib_name.as_deref().unwrap_or(&self.lib_id)
    }
    pub fn field(&self, name: &str) -> Option<&str> {
        self.fields
            .iter()
            .find(|f| f.name == name)
            .map(|f| f.value.as_str())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LabelKind {
    Local,
    Global,
    Hierarchical,
    /// A netclass flag (KiCad 7's directive label). Connectable, never a
    /// driver.
    Directive,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Label {
    pub index: usize,
    pub kind: LabelKind,
    pub text: String,
    pub at: Pt,
    pub shape: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Line {
    pub index: usize,
    pub a: Pt,
    pub b: Pt,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SheetPin {
    pub name: String,
    pub shape: String,
    pub at: Pt,
}

/// A hierarchical sheet placed on this sheet.
#[derive(Debug, Clone, PartialEq)]
pub struct Sheet {
    pub index: usize,
    pub uuid: String,
    pub name: String,
    pub file: String,
    pub pins: Vec<SheetPin>,
    pub instances: Vec<Instance>,
    pub fields: Vec<Field>,
    pub on_board: bool,
    pub in_bom: bool,
    pub dnp: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct BusEntry {
    pub index: usize,
    pub at: Pt,
    pub end: Pt,
}

#[derive(Debug, Clone, PartialEq)]
pub struct BusAlias {
    pub name: String,
    pub members: Vec<String>,
}

/// KiCad 6's root-level `symbol_instances` entry.
#[derive(Debug, Clone, PartialEq)]
pub struct LegacySymbolInstance {
    /// `/sheet-uuid/.../symbol-uuid`, without the root's UUID.
    pub path: String,
    pub reference: String,
    pub unit: Option<u32>,
    pub value: Option<String>,
    pub footprint: Option<String>,
}

/// A typed view of one schematic file.
#[derive(Debug, Clone, Default)]
pub struct SheetModel {
    pub version: u32,
    pub uuid: Option<String>,
    pub lib_symbols: BTreeMap<String, LibSymbol>,
    pub symbols: Vec<Symbol>,
    pub wires: Vec<Line>,
    pub buses: Vec<Line>,
    pub junctions: Vec<Pt>,
    pub no_connects: Vec<Pt>,
    pub labels: Vec<Label>,
    pub sheets: Vec<Sheet>,
    pub wire_entries: Vec<BusEntry>,
    pub bus_bus_entries: Vec<BusEntry>,
    pub bus_aliases: Vec<BusAlias>,
    pub legacy_symbol_instances: Vec<LegacySymbolInstance>,
    /// KiCad 6's `sheet_instances`: path (without the root UUID) to page.
    pub legacy_sheet_pages: Vec<(String, String)>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ModelError {
    #[error("not a KiCad schematic (the root is not kicad_sch)")]
    NotSchematic,
    #[error(
        "schematic format {0} is older than KiCad 6; open it once in KiCad 6 or later to convert it"
    )]
    TooOld(u32),
    #[error("a {what} has no valid position")]
    BadGeometry { what: &'static str },
}

fn at_of(l: &List) -> Option<(Pt, i32)> {
    let at = l.find("at")?;
    let x = mm(&at.atom(1)?)?;
    let y = mm(&at.atom(2)?)?;
    let rot = at
        .atom(3)
        .and_then(|r| r.parse::<f64>().ok())
        .unwrap_or(0.0);
    Some((Pt::new(x, y), rot.round() as i32))
}

fn xy(l: &List) -> Option<Pt> {
    Some(Pt::new(mm(&l.atom(1)?)?, mm(&l.atom(2)?)?))
}

fn fields_of(l: &List) -> Vec<Field> {
    l.find_all("property")
        .map(|p| Field {
            name: p.atom(1).unwrap_or_default(),
            value: p.atom(2).unwrap_or_default(),
        })
        .collect()
}

fn instances_of(l: &List) -> Vec<Instance> {
    let mut out = Vec::new();
    let Some(inst) = l.find("instances") else {
        return out;
    };
    for project in inst.find_all("project") {
        let name = project.atom(1).unwrap_or_default();
        for path in project.find_all("path") {
            out.push(Instance {
                project: name.clone(),
                path: path.atom(1).unwrap_or_default(),
                reference: path.value_of("reference"),
                unit: path.value_of("unit").and_then(|u| u.parse().ok()),
                page: path.value_of("page"),
                value: path.value_of("value"),
                footprint: path.value_of("footprint"),
            });
        }
    }
    out
}

/// A flag that KiCad 6-8 write bare (`hide`) and KiCad 9+ as `(hide yes)`.
fn flag(l: &List, name: &str) -> bool {
    l.has_flag(name) || l.yes_no(name).unwrap_or(false)
}

fn parse_lib_symbol(index: usize, l: &List) -> LibSymbol {
    let name = l.atom(1).unwrap_or_default();
    let power = l.find("power").map(|p| {
        if p.atom(1).as_deref() == Some("local") {
            Power::Local
        } else {
            Power::Global
        }
    });
    let mut units = Vec::new();
    for sub in l.find_all("symbol") {
        let sub_name = sub.atom(1).unwrap_or_default();
        // "NAME_U_B": the last two underscore-separated numbers.
        let mut parts = sub_name.rsplitn(3, '_');
        let body_style = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);
        let unit = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);
        let mut pins = Vec::new();
        for p in sub.find_all("pin") {
            let pin_type = PinType::parse(&p.atom(1).unwrap_or_default());
            let Some((pos, angle)) = at_of(p) else {
                continue;
            };
            let alternates = p
                .find_all("alternate")
                .map(|a| {
                    (
                        a.atom(1).unwrap_or_default(),
                        PinType::parse(&a.atom(2).unwrap_or_default()),
                    )
                })
                .collect();
            pins.push(LibPin {
                number: p.find("number").and_then(|n| n.atom(1)).unwrap_or_default(),
                name: p.find("name").and_then(|n| n.atom(1)).unwrap_or_default(),
                pin_type,
                // The library's Y axis points up.
                pos: Pt::new(pos.x, -pos.y),
                angle,
                hidden: flag(p, "hide"),
                alternates,
            });
        }
        units.push(LibUnit {
            unit,
            body_style,
            pins,
            unit_name: sub.value_of("unit_name"),
        });
    }
    let jumper_pin_groups = l
        .find("jumper_pin_groups")
        .map(|g| {
            g.lists()
                .map(|grp| {
                    grp.items
                        .iter()
                        .filter_map(Node::as_atom)
                        .map(|a| a.text())
                        .collect()
                })
                .collect()
        })
        .unwrap_or_default();
    let properties: Vec<(String, String)> = l
        .find_all("property")
        .map(|p| (p.atom(1).unwrap_or_default(), p.atom(2).unwrap_or_default()))
        .collect();
    let footprint_filters = properties
        .iter()
        .find(|(n, _)| n == "ki_fp_filters")
        .map(|(_, v)| v.split_whitespace().map(str::to_string).collect())
        .unwrap_or_default();
    LibSymbol {
        index,
        name,
        extends: l.value_of("extends"),
        power,
        properties,
        units,
        duplicate_pin_numbers_are_jumpers: l
            .yes_no("duplicate_pin_numbers_are_jumpers")
            .unwrap_or(false),
        jumper_pin_groups,
        footprint_filters,
    }
}

impl SheetModel {
    /// Build the typed view of a parsed schematic.
    pub fn read(doc: &Document) -> Result<SheetModel, ModelError> {
        let root = &doc.root;
        if !root.is("kicad_sch") {
            return Err(ModelError::NotSchematic);
        }
        let version = root
            .value_of("version")
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);
        if version < OLDEST_KNOWN_VERSION {
            return Err(ModelError::TooOld(version));
        }
        let mut m = SheetModel {
            version,
            uuid: root.value_of("uuid"),
            ..Default::default()
        };
        for (index, node) in root.items.iter().enumerate() {
            let Some(l) = node.as_list() else { continue };
            match l.head().unwrap_or("") {
                "lib_symbols" => {
                    for (i, s) in l.items.iter().enumerate() {
                        if let Some(s) = s.as_list().filter(|s| s.is("symbol")) {
                            let ls = parse_lib_symbol(i, s);
                            m.lib_symbols.insert(ls.name.clone(), ls);
                        }
                    }
                }
                "symbol" => {
                    let (at, rotation) =
                        at_of(l).ok_or(ModelError::BadGeometry { what: "symbol" })?;
                    let mirror = l.find("mirror").and_then(|m| m.atom(1)).unwrap_or_default();
                    let mut alternates = BTreeMap::new();
                    for p in l.find_all("pin") {
                        if let (Some(num), Some(alt)) = (p.atom(1), p.value_of("alternate")) {
                            alternates.insert(num, alt);
                        }
                    }
                    m.symbols.push(Symbol {
                        index,
                        lib_id: l.value_of("lib_id").unwrap_or_default(),
                        lib_name: l.value_of("lib_name"),
                        at,
                        rotation,
                        mirror_x: mirror.contains('x'),
                        mirror_y: mirror.contains('y'),
                        unit: l.value_of("unit").and_then(|u| u.parse().ok()).unwrap_or(1),
                        body_style: l
                            .value_of("body_style")
                            .or_else(|| l.value_of("convert"))
                            .and_then(|u| u.parse().ok())
                            .unwrap_or(1),
                        in_bom: l.yes_no("in_bom").unwrap_or(true),
                        on_board: l.yes_no("on_board").unwrap_or(true),
                        dnp: l.yes_no("dnp").unwrap_or(false),
                        uuid: l.value_of("uuid").unwrap_or_default(),
                        fields: fields_of(l),
                        alternates,
                        instances: instances_of(l),
                    });
                }
                "wire" | "bus" => {
                    let pts: Vec<Pt> = l
                        .find("pts")
                        .map(|p| p.find_all("xy").filter_map(xy).collect())
                        .unwrap_or_default();
                    if pts.len() < 2 {
                        return Err(ModelError::BadGeometry { what: "wire" });
                    }
                    // A KiCad line has two points; a longer polyline is
                    // read as consecutive segments.
                    for w in pts.windows(2) {
                        let line = Line {
                            index,
                            a: w[0],
                            b: w[1],
                        };
                        if l.is("wire") {
                            m.wires.push(line)
                        } else {
                            m.buses.push(line)
                        }
                    }
                }
                "junction" => m.junctions.push(
                    at_of(l)
                        .ok_or(ModelError::BadGeometry { what: "junction" })?
                        .0,
                ),
                "no_connect" => m.no_connects.push(
                    at_of(l)
                        .ok_or(ModelError::BadGeometry { what: "no_connect" })?
                        .0,
                ),
                head @ ("label" | "global_label" | "hierarchical_label" | "netclass_flag"
                | "directive_label") => {
                    let kind = match head {
                        "label" => LabelKind::Local,
                        "global_label" => LabelKind::Global,
                        "hierarchical_label" => LabelKind::Hierarchical,
                        _ => LabelKind::Directive,
                    };
                    let (at, _) = at_of(l).ok_or(ModelError::BadGeometry { what: "label" })?;
                    m.labels.push(Label {
                        index,
                        kind,
                        text: l.atom(1).unwrap_or_default(),
                        at,
                        shape: l.value_of("shape"),
                    });
                }
                head @ ("bus_entry" | "bus_bus_entry") => {
                    let (at, _) = at_of(l).ok_or(ModelError::BadGeometry { what: "bus entry" })?;
                    let size = l.find("size").and_then(xy).unwrap_or_default();
                    let e = BusEntry {
                        index,
                        at,
                        end: Pt::new(at.x + size.x, at.y + size.y),
                    };
                    if head == "bus_entry" {
                        m.wire_entries.push(e)
                    } else {
                        m.bus_bus_entries.push(e)
                    }
                }
                "bus_alias" => m.bus_aliases.push(BusAlias {
                    name: l.atom(1).unwrap_or_default(),
                    members: l
                        .find("members")
                        .map(|mm| {
                            mm.items
                                .iter()
                                .skip(1)
                                .filter_map(Node::as_atom)
                                .map(|a| a.text())
                                .collect()
                        })
                        .unwrap_or_default(),
                }),
                "sheet" => {
                    let fields = fields_of(l);
                    let get = |a: &str, b: &str| {
                        fields
                            .iter()
                            .find(|f| f.name == a || f.name == b)
                            .map(|f| f.value.clone())
                            .unwrap_or_default()
                    };
                    let pins = l
                        .find_all("pin")
                        .filter_map(|p| {
                            Some(SheetPin {
                                name: p.atom(1)?,
                                shape: p.atom(2).unwrap_or_default(),
                                at: at_of(p)?.0,
                            })
                        })
                        .collect();
                    m.sheets.push(Sheet {
                        index,
                        uuid: l.value_of("uuid").unwrap_or_default(),
                        name: get("Sheetname", "Sheet name"),
                        file: get("Sheetfile", "Sheet file"),
                        pins,
                        instances: instances_of(l),
                        fields: fields.clone(),
                        on_board: l.yes_no("on_board").unwrap_or(true),
                        in_bom: l.yes_no("in_bom").unwrap_or(true),
                        dnp: l.yes_no("dnp").unwrap_or(false),
                    });
                }
                "symbol_instances" => {
                    for p in l.find_all("path") {
                        m.legacy_symbol_instances.push(LegacySymbolInstance {
                            path: p.atom(1).unwrap_or_default(),
                            reference: p.value_of("reference").unwrap_or_default(),
                            unit: p.value_of("unit").and_then(|u| u.parse().ok()),
                            value: p.value_of("value"),
                            footprint: p.value_of("footprint"),
                        });
                    }
                }
                "sheet_instances" => {
                    for p in l.find_all("path") {
                        m.legacy_sheet_pages.push((
                            p.atom(1).unwrap_or_default(),
                            p.value_of("page").unwrap_or_default(),
                        ));
                    }
                }
                _ => {}
            }
        }
        Ok(m)
    }

    /// The library symbol a placed symbol uses, with `extends` resolved.
    pub fn lib_symbol_for(&self, sym: &Symbol) -> Option<LibSymbol> {
        let base = self
            .lib_symbols
            .get(sym.cache_key())
            .or_else(|| self.lib_symbols.get(&sym.lib_id))?;
        Some(self.flatten(base, 0))
    }

    fn flatten(&self, ls: &LibSymbol, depth: usize) -> LibSymbol {
        let mut out = ls.clone();
        if let Some(parent) = ls.extends.as_ref().and_then(|p| {
            // `extends` names the parent by item name; in the cache the key
            // carries the library nickname.
            let lib = ls.name.split_once(':').map(|(l, _)| l);
            self.lib_symbols
                .get(p)
                .or_else(|| lib.and_then(|lib| self.lib_symbols.get(&format!("{lib}:{p}"))))
        }) && depth < 8
        {
            let parent = self.flatten(parent, depth + 1);
            if out.units.iter().all(|u| u.pins.is_empty()) {
                out.units = parent.units;
            }
            if out.power.is_none() {
                out.power = parent.power;
            }
        }
        out
    }
}

/// A pin of a placed symbol, where it is on the sheet.
#[derive(Debug, Clone, PartialEq)]
pub struct PlacedPin {
    pub pin: LibPin,
    /// The connection point, in sheet coordinates (Y down).
    pub at: Pt,
    /// A unit vector (one of the four axis directions, Y down) pointing away
    /// from the symbol body: the way a wire leaves this pin.
    pub outward: Pt,
}

impl SheetModel {
    /// The pins a placed symbol has on the sheet, after its unit, body
    /// style, rotation and mirroring. This is the single place pin geometry
    /// is computed; the connectivity engine and the edit operations use it,
    /// and a UI must too.
    pub fn placed_pins(&self, sym: &Symbol) -> Vec<PlacedPin> {
        self.placed_pins_for_unit(sym, sym.unit)
    }

    /// As [`SheetModel::placed_pins`], for a given unit (a sheet instance can
    /// select a different unit than the symbol's own `unit` field).
    pub fn placed_pins_for_unit(&self, sym: &Symbol, unit: u32) -> Vec<PlacedPin> {
        match self.lib_symbol_for(sym) {
            Some(lib) => Self::placed_pins_with(&lib, sym, unit),
            None => Vec::new(),
        }
    }

    /// As [`SheetModel::placed_pins_for_unit`], with the library symbol
    /// already resolved (by [`SheetModel::lib_symbol_for`]), so a caller
    /// placing many symbols of one library symbol flattens it once.
    pub fn placed_pins_with(lib: &LibSymbol, sym: &Symbol, unit: u32) -> Vec<PlacedPin> {
        let t = Transform::for_symbol(sym.rotation, sym.mirror_x, sym.mirror_y);
        lib.pins_for(unit, sym.body_style)
            .map(|p| {
                let off = t.apply(p.pos);
                // Toward the body in library axes (Y up), flipped to Y down,
                // then reversed to point away from the body.
                let (dx, dy) = match p.angle.rem_euclid(360) {
                    90 => (0, -1),
                    180 => (-1, 0),
                    270 => (0, 1),
                    _ => (1, 0),
                };
                let out = t.apply(Pt::new(-dx, -dy));
                PlacedPin {
                    pin: p.clone(),
                    at: Pt::new(sym.at.x + off.x, sym.at.y + off.y),
                    outward: out,
                }
            })
            .collect()
    }
}

/// The 2x2 transform of a placed symbol: rotation first, then mirroring, the
/// same composition KiCad's `SCH_SYMBOL::SetOrientation` builds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Transform {
    pub x1: i64,
    pub y1: i64,
    pub x2: i64,
    pub y2: i64,
}

impl Transform {
    pub const IDENTITY: Transform = Transform {
        x1: 1,
        y1: 0,
        x2: 0,
        y2: 1,
    };

    /// `self` followed by `t`.
    fn then(self, t: Transform) -> Transform {
        Transform {
            x1: self.x1 * t.x1 + self.x2 * t.y1,
            y1: self.y1 * t.x1 + self.y2 * t.y1,
            x2: self.x1 * t.x2 + self.x2 * t.y2,
            y2: self.y1 * t.x2 + self.y2 * t.y2,
        }
    }

    pub fn for_symbol(rotation: i32, mirror_x: bool, mirror_y: bool) -> Transform {
        const CCW: Transform = Transform {
            x1: 0,
            y1: 1,
            x2: -1,
            y2: 0,
        };
        const CW: Transform = Transform {
            x1: 0,
            y1: -1,
            x2: 1,
            y2: 0,
        };
        let mut t = Transform::IDENTITY;
        match rotation.rem_euclid(360) {
            90 => t = t.then(CCW),
            180 => t = t.then(CCW).then(CCW),
            270 => t = t.then(CW),
            _ => {}
        }
        if mirror_x {
            t = t.then(Transform {
                x1: 1,
                y1: 0,
                x2: 0,
                y2: -1,
            });
        }
        if mirror_y {
            t = t.then(Transform {
                x1: -1,
                y1: 0,
                x2: 0,
                y2: 1,
            });
        }
        t
    }

    pub fn apply(self, p: Pt) -> Pt {
        Pt::new(self.x1 * p.x + self.y1 * p.y, self.x2 * p.x + self.y2 * p.y)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mm_round_trip() {
        assert_eq!(mm("12.7"), Some(127_000));
        assert_eq!(mm("-0.0001"), Some(-1));
        assert_eq!(fmt_mm(127_000), "12.7");
        assert_eq!(fmt_mm(-1), "-0.0001");
        assert_eq!(fmt_mm(0), "0");
        assert_eq!(fmt_mm(254_000), "25.4");
        assert_eq!(mm("nan"), None);
    }

    #[test]
    fn transform_rotates_ccw_on_screen() {
        // A pin to the right of the origin, rotated 90 degrees, points up
        // (negative Y on screen).
        let t = Transform::for_symbol(90, false, false);
        assert_eq!(t.apply(Pt::new(1, 0)), Pt::new(0, -1));
        let t = Transform::for_symbol(0, false, true);
        assert_eq!(t.apply(Pt::new(1, 2)), Pt::new(-1, 2));
        let t = Transform::for_symbol(0, true, false);
        assert_eq!(t.apply(Pt::new(1, 2)), Pt::new(1, -2));
        // Rotation is applied before mirroring.
        let t = Transform::for_symbol(90, true, false);
        assert_eq!(t.apply(Pt::new(1, 0)), Pt::new(0, 1));
    }
}
