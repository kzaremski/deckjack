//! Edit operations: the changes the circuit app's v1 makes to a schematic,
//! as changes to the lossless tree.
//!
//! Every operation works on the tree in place and touches only the nodes it
//! is about, so the saved file differs from the original by exactly the edit
//! (a changed atom, or a block of new lines). The typed model of each touched
//! file is re-read afterwards.
//!
//! **How new nodes get their syntax.** KiCad's schematic syntax changed in
//! small ways across versions 6 to 10 (quoted UUIDs, `(hide yes)` in place of
//! a bare `hide`, new flags such as `dnp` and `exclude_from_sim`, tabs in
//! place of two spaces). The rule is to write at the file's own version and
//! never upgrade it, so a new node is made by **cloning a node of the same
//! kind already in the file** and changing its position, text and UUID. Only
//! when the file has no such node is it built from a template for the file's
//! version. A new symbol's properties are cloned from the symbol's own entry
//! in the sheet's `lib_symbols` cache, which is written in the file's syntax
//! too.
//!
//! **What is refused.** A file newer than this code knows is read-only. A
//! mandatory field cannot be removed, the Reference field is set only through
//! [`Op::Annotate`] (it lives in the per-instance tables as well), and a flag
//! the file's format has no syntax for (DNP before KiCad 7) is an error, not
//! a silent upgrade.

use std::collections::{BTreeMap, BTreeSet};

use crate::model::{self, IU_PER_MM, ModelError, Pt, SheetModel, Symbol, fmt_mm};
use crate::project::{Project, SheetPath};
use crate::sexpr::{self, Atom, List, Node, Style};

/// The first format with `(dnp ...)`: KiCad 7.
const FIRST_DNP_VERSION: u32 = 20221206;
/// The first format with per-symbol `instances` blocks rather than KiCad 6's
/// root `symbol_instances` table.
const FIRST_INSTANCES_VERSION: u32 = 20221002;
/// The first format KiCad 8 wrote: tabs, quoted UUIDs, `(hide yes)`.
const FIRST_KICAD8_VERSION: u32 = 20231120;
/// The first format with a field's `(hide yes)` on the property itself
/// rather than inside its `effects`. KiCad 8.0's field parser refuses a
/// property-level `hide`. This version is inferred, not read from KiCad's
/// history: the fixtures show 20240819 still hiding inside `effects` and
/// 20250227 at the property, and 20241004 ("use booleans for 'hide' in
/// symbols") is the only format change between them about `hide`.
const FIRST_FIELD_HIDE_VERSION: u32 = 20241004;

/// Fields every symbol has, which can be changed but not removed.
pub const MANDATORY_FIELDS: [&str; 5] = [
    "Reference",
    "Value",
    "Footprint",
    "Datasheet",
    "Description",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LabelScope {
    Local,
    Global,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Flag {
    /// Do not populate.
    Dnp,
    InBom,
    OnBoard,
}

/// A pin of a placed symbol, by the symbol's UUID and the pin's number.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PinRef {
    pub symbol: String,
    pub pin: String,
}

/// One edit. `file` is a sheet file relative to the project directory, as in
/// [`Project::files`]; symbols are named by UUID.
#[derive(Debug, Clone, PartialEq)]
pub enum Op {
    /// Set a field's value, adding the field (hidden) when the symbol does
    /// not have it. Value and Footprint also update KiCad 6's per-instance
    /// copies where the file has them.
    SetField {
        file: String,
        symbol: String,
        name: String,
        value: String,
    },
    /// Remove a non-mandatory field.
    RemoveField {
        file: String,
        symbol: String,
        name: String,
    },
    SetFlag {
        file: String,
        symbol: String,
        flag: Flag,
        value: bool,
    },
    /// Place a symbol from the sheet's `lib_symbols` cache (`lib` is the
    /// cache key, such as `Device:R`), unannotated (`R?`).
    PlaceSymbol {
        file: String,
        lib: String,
        at: Pt,
        rotation: i32,
        unit: u32,
    },
    /// Copy a symbol already on the sheet to a new position, unannotated.
    CopySymbol {
        file: String,
        symbol: String,
        at: Pt,
    },
    /// Wire segments through the given points (at least two).
    AddWire {
        file: String,
        points: Vec<Pt>,
    },
    /// A wire between two pins: straight when they line up, otherwise one
    /// elbow, leaving the first pin horizontally or vertically as its
    /// direction says.
    ConnectPins {
        file: String,
        a: PinRef,
        b: PinRef,
    },
    /// Connect a pin to a net by name: a short stub wire leaving the pin, and
    /// a local or global label at its end. `stub` is the stub's length in
    /// internal units (0 for 2.54 mm).
    ConnectPinToNet {
        file: String,
        pin: PinRef,
        net: String,
        scope: LabelScope,
        stub: i64,
    },
    AddNoConnect {
        file: String,
        at: Pt,
    },
    /// Give every unannotated reference (`R?`) the first free number for its
    /// prefix, across the whole project, in hierarchy order and by position
    /// (X, then Y) within a sheet. With `reset`, every reference is
    /// renumbered; the units of a multi-unit part that shared a number keep
    /// sharing one. An unannotated unit (`U?`) always gets a number of its
    /// own.
    Annotate {
        reset: bool,
    },
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Outcome {
    /// UUIDs of the items created (a symbol first). Items in formats that
    /// carry no UUID on that kind of node are created without one and are
    /// not listed.
    pub created: Vec<String>,
    /// Files changed, relative to the project directory.
    pub files: BTreeSet<String>,
    /// For [`Op::Annotate`]: `(sheet path, old reference, new reference)`.
    pub renamed: Vec<(String, String, String)>,
}

#[derive(Debug, thiserror::Error)]
pub enum EditError {
    #[error("{0} is not a sheet of this project")]
    NoFile(String),
    #[error("{file} is format {version}, newer than this app understands; it is read-only")]
    ReadOnly { file: String, version: u32 },
    #[error("no symbol with UUID {0}")]
    NoSymbol(String),
    #[error("\"{0}\" is not in the sheet's lib_symbols cache")]
    NoLibSymbol(String),
    #[error("symbol {symbol} has no pin {pin}")]
    NoPin { symbol: String, pin: String },
    #[error("the symbol has no field \"{0}\"")]
    NoField(String),
    #[error("\"{0}\" is a mandatory field and cannot be removed")]
    Mandatory(String),
    #[error("the Reference is set by annotation, not as a field")]
    ReferenceField,
    #[error("this file's format ({0}) has no DNP flag; open and save it once in KiCad 7 or later")]
    NoDnpInFormat(u32),
    #[error("a wire needs at least two distinct points")]
    BadWire,
    #[error("a net name must not be empty")]
    EmptyName,
    #[error("a symbol's rotation must be 0, 90, 180 or 270 degrees, not {0}")]
    BadRotation(i32),
    #[error("unit {unit} does not exist (the symbol has {count})")]
    NoUnit { unit: u32, count: u32 },
    #[error("the edited file no longer reads as a schematic: {0}")]
    Model(#[from] ModelError),
    #[error("internal template error: {0}")]
    Template(#[from] sexpr::Error),
}

/// Apply one edit to a loaded project. On error nothing has been changed.
pub fn apply(project: &mut Project, op: &Op) -> Result<Outcome, EditError> {
    // Work on a copy of the touched files so a failure leaves the project as
    // it was.
    let before = project.files.clone();
    let result = apply_inner(project, op);
    match result {
        Ok(out) => {
            for f in &out.files {
                if let Some(file) = project.files.get_mut(f)
                    && let Err(e) = file.refresh()
                {
                    project.files = before;
                    return Err(e.into());
                }
            }
            Ok(out)
        }
        Err(e) => {
            project.files = before;
            Err(e)
        }
    }
}

/// Apply several edits in order, all or nothing.
pub fn apply_all(project: &mut Project, ops: &[Op]) -> Result<Outcome, EditError> {
    let before = project.files.clone();
    let mut total = Outcome::default();
    for op in ops {
        match apply(project, op) {
            Ok(out) => {
                total.created.extend(out.created);
                total.files.extend(out.files);
                total.renamed.extend(out.renamed);
            }
            Err(e) => {
                project.files = before;
                return Err(e);
            }
        }
    }
    Ok(total)
}

fn apply_inner(project: &mut Project, op: &Op) -> Result<Outcome, EditError> {
    match op {
        Op::SetField {
            file,
            symbol,
            name,
            value,
        } => set_field(project, file, symbol, name, value),
        Op::RemoveField { file, symbol, name } => remove_field(project, file, symbol, name),
        Op::SetFlag {
            file,
            symbol,
            flag,
            value,
        } => set_flag(project, file, symbol, *flag, *value),
        Op::PlaceSymbol {
            file,
            lib,
            at,
            rotation,
            unit,
        } => place_symbol(project, file, lib, *at, *rotation, *unit),
        Op::CopySymbol { file, symbol, at } => copy_symbol(project, file, symbol, *at),
        Op::AddWire { file, points } => {
            writable(project, file)?;
            let created = add_wire(project, file, points)?;
            Ok(Outcome {
                created,
                files: BTreeSet::from([file.clone()]),
                ..Default::default()
            })
        }
        Op::ConnectPins { file, a, b } => connect_pins(project, file, a, b),
        Op::ConnectPinToNet {
            file,
            pin,
            net,
            scope,
            stub,
        } => connect_pin_to_net(project, file, pin, net, *scope, *stub),
        Op::AddNoConnect { file, at } => {
            writable(project, file)?;
            let created = add_no_connect(project, file, *at)?.into_iter().collect();
            Ok(Outcome {
                created,
                files: BTreeSet::from([file.clone()]),
                ..Default::default()
            })
        }
        Op::Annotate { reset } => annotate(project, *reset),
    }
}

// ---- helpers -----------------------------------------------------------------

/// A fresh random (version 4) UUID.
pub fn new_uuid() -> String {
    use rand::TryRng as _;
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let mut bytes = [0u8; 16];
    // The system generator (a syscall, so it works confined). If it fails,
    // the clock plus a counter still gives distinct values in one process.
    if rand::rngs::SysRng.try_fill_bytes(&mut bytes).is_err() {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let n = now ^ ((COUNTER.fetch_add(1, Ordering::Relaxed) as u128) << 64);
        bytes.copy_from_slice(&n.to_le_bytes());
    }
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    )
}

fn writable(project: &Project, file: &str) -> Result<(), EditError> {
    let f = project
        .files
        .get(file)
        .ok_or_else(|| EditError::NoFile(file.to_string()))?;
    if f.is_newer_than_known() {
        return Err(EditError::ReadOnly {
            file: file.to_string(),
            version: f.model.version,
        });
    }
    Ok(())
}

fn version(project: &Project, file: &str) -> u32 {
    project
        .files
        .get(file)
        .map(|f| f.model.version)
        .unwrap_or(0)
}

fn root_mut<'a>(project: &'a mut Project, file: &str) -> Result<&'a mut List, EditError> {
    project
        .files
        .get_mut(file)
        .map(|f| &mut f.doc.root)
        .ok_or_else(|| EditError::NoFile(file.to_string()))
}

fn style_of(project: &Project, file: &str) -> Style {
    project
        .files
        .get(file)
        .map(|f| Style::detect(&f.doc))
        .unwrap_or_default()
}

/// The index among the root's children of the symbol with this UUID.
fn symbol_index(root: &List, uuid: &str) -> Option<usize> {
    root.items.iter().position(|n| {
        n.as_list()
            .is_some_and(|l| l.is("symbol") && l.value_of("uuid").as_deref() == Some(uuid))
    })
}

fn symbol_model<'a>(
    project: &'a Project,
    file: &str,
    uuid: &str,
) -> Result<(&'a SheetModel, &'a Symbol), EditError> {
    let f = project
        .files
        .get(file)
        .ok_or_else(|| EditError::NoFile(file.to_string()))?;
    let sym = f
        .model
        .symbols
        .iter()
        .find(|s| s.uuid == uuid)
        .ok_or_else(|| EditError::NoSymbol(uuid.to_string()))?;
    Ok((&f.model, sym))
}

fn list_at(root: &mut List, index: usize) -> &mut List {
    root.items[index]
        .as_list_mut()
        .expect("index points at a list")
}

/// Set the atom at `index` of a list, keeping its quoting; append it when
/// the list is shorter.
fn set_atom(list: &mut List, index: usize, text: &str) {
    if let Some(Node::Atom(a)) = list.items.get_mut(index) {
        a.set_text(text);
        return;
    }
    let mut a = Atom::bare(text);
    a.set_text(text);
    a.lead = " ".into();
    if index <= list.items.len() {
        list.items
            .insert(index.min(list.items.len()), Node::Atom(a));
    }
}

/// Set a string atom, quoted whatever the old atom was (names and values
/// are always strings in KiCad's files).
fn set_string(list: &mut List, index: usize, text: &str) {
    match list.items.get_mut(index) {
        Some(Node::Atom(a)) => a.raw = sexpr::quote(text),
        _ => {
            let mut a = Atom::quoted(text);
            a.lead = " ".into();
            list.items
                .insert(index.min(list.items.len()), Node::Atom(a));
        }
    }
}

fn set_at(list: &mut List, p: Pt, rotation: Option<i32>) {
    if let Some(at) = list.find_mut("at") {
        set_atom(at, 1, &fmt_mm(p.x));
        set_atom(at, 2, &fmt_mm(p.y));
        if let Some(r) = rotation {
            set_atom(at, 3, &r.rem_euclid(360).to_string());
        }
    }
}

/// Set the node's UUID. Returns false when the node has none: KiCad 6
/// development formats wrote no UUID on wires and labels (KiCad assigns
/// one when it loads them), and a clone of such a node stays that way.
fn set_uuid(list: &mut List, uuid: &str) -> bool {
    match list.find_mut("uuid") {
        Some(u) => {
            set_atom(u, 1, uuid);
            true
        }
        None => false,
    }
}

/// Set `(name value)` where the child exists.
fn set_child_value(list: &mut List, head: &str, value: &str) -> bool {
    match list.find_mut(head) {
        Some(c) => {
            set_atom(c, 1, value);
            true
        }
        None => false,
    }
}

/// Remove `count` indentation units from every line break inside a node, for
/// a node moved to a shallower depth.
fn dedent(node: &mut List, count: usize, style: &Style) {
    let strip = style.indent.repeat(count);
    fn fix(ws: &mut String, strip: &str) {
        if let Some(nl) = ws.rfind('\n') {
            let (head, indent) = ws.split_at(nl + 1);
            if let Some(rest) = indent.strip_prefix(strip) {
                *ws = format!("{head}{rest}");
            }
        }
    }
    fn walk(l: &mut List, strip: &str) {
        fix(&mut l.lead, strip);
        fix(&mut l.tail, strip);
        for item in &mut l.items {
            match item {
                Node::List(c) => walk(c, strip),
                Node::Atom(a) => fix(&mut a.lead, strip),
            }
        }
    }
    walk(node, &strip);
}

/// Insert a node among the root's children: after the last child with the
/// same keyword, else before the first of `before`, else at the end. The
/// node's leading whitespace is set to a line break at depth 1.
fn insert_root(root: &mut List, mut node: List, style: &Style, before: &[&str]) -> usize {
    let head = node.head().unwrap_or("").to_string();
    node.lead = format!("{}{}", style.newline, style.indent);
    let last_same = root
        .items
        .iter()
        .rposition(|n| n.as_list().is_some_and(|l| l.is(&head)));
    let at = match last_same {
        Some(i) => i + 1,
        None => root
            .items
            .iter()
            .position(|n| {
                n.as_list()
                    .is_some_and(|l| l.head().is_some_and(|h| before.contains(&h)))
            })
            .unwrap_or(root.items.len()),
    };
    root.items.insert(at, Node::List(node));
    at
}

/// Where new items go when the file has none of their kind: before the
/// trailing tables, as KiCad orders them.
const TRAILERS: [&str; 4] = [
    "sheet_instances",
    "symbol_instances",
    "embedded_fonts",
    "embedded_files",
];

/// A template for the file's version, laid out the way KiCad lays it out.
fn template(snippet: &str, version: u32, style: &Style) -> Result<List, EditError> {
    let mut l = sexpr::parse_list(snippet)?;
    if version >= FIRST_KICAD8_VERSION {
        sexpr::reformat(&mut l, 1, style);
    }
    Ok(l)
}

fn uuid_atom(version: u32, uuid: &str) -> String {
    if version >= FIRST_KICAD8_VERSION {
        sexpr::quote(uuid)
    } else {
        uuid.to_string()
    }
}

/// A clone of the first root child with this keyword, if any.
fn clone_first(root: &List, head: &str, pred: impl Fn(&List) -> bool) -> Option<List> {
    root.lists().find(|l| l.is(head) && pred(l)).cloned()
}

// ---- fields and flags ----------------------------------------------------------

fn set_field(
    project: &mut Project,
    file: &str,
    symbol: &str,
    name: &str,
    value: &str,
) -> Result<Outcome, EditError> {
    writable(project, file)?;
    if name == "Reference" {
        return Err(EditError::ReferenceField);
    }
    let version = version(project, file);
    let style = style_of(project, file);
    let (paths, root_file) = instance_paths(project, file, symbol);
    let root = root_mut(project, file)?;
    let idx = symbol_index(root, symbol).ok_or_else(|| EditError::NoSymbol(symbol.to_string()))?;
    let sym = list_at(root, idx);
    let existing = sym
        .items
        .iter_mut()
        .filter_map(Node::as_list_mut)
        .find(|l| l.is("property") && l.atom(1).as_deref() == Some(name));
    match existing {
        Some(p) => set_string(p, 2, value),
        None => {
            let prop = new_property(sym, name, value, version, &style)?;
            let after = sym
                .items
                .iter()
                .rposition(|n| n.as_list().is_some_and(|l| l.is("property")))
                .map_or(sym.items.len(), |i| i + 1);
            sym.items.insert(after, Node::List(prop));
        }
    }
    // KiCad 6 (and 7 development files) keep per-instance Value and
    // Footprint copies, which win over the field when present.
    if matches!(name, "Value" | "Footprint") {
        let key = name.to_lowercase();
        if let Some(inst) = sym.find_mut("instances") {
            for project_node in inst
                .items
                .iter_mut()
                .filter_map(Node::as_list_mut)
                .filter(|l| l.is("project"))
            {
                for path in project_node
                    .items
                    .iter_mut()
                    .filter_map(Node::as_list_mut)
                    .filter(|l| l.is("path"))
                {
                    if let Some(c) = path.find_mut(&key) {
                        set_string(c, 1, value);
                    }
                }
            }
        }
        let mut touched_root = false;
        if let Some(rf) = project.files.get_mut(&root_file) {
            for node in rf.doc.root.items.iter_mut() {
                let Some(table) = node.as_list_mut().filter(|l| l.is("symbol_instances")) else {
                    continue;
                };
                for path in table
                    .items
                    .iter_mut()
                    .filter_map(Node::as_list_mut)
                    .filter(|l| l.is("path"))
                {
                    if path.atom(1).is_some_and(|p| paths.contains(&p))
                        && let Some(c) = path.find_mut(&key)
                    {
                        set_string(c, 1, value);
                        touched_root = true;
                    }
                }
            }
        }
        let mut files = BTreeSet::from([file.to_string()]);
        if touched_root {
            files.insert(root_file);
        }
        return Ok(Outcome {
            files,
            ..Default::default()
        });
    }
    Ok(Outcome {
        files: BTreeSet::from([file.to_string()]),
        ..Default::default()
    })
}

/// KiCad 6 table paths (`/sheet-uuids.../symbol-uuid`) of every instance of
/// a symbol in this file, and the root file's name.
fn instance_paths(project: &Project, file: &str, symbol: &str) -> (Vec<String>, String) {
    let paths = project
        .sheets
        .iter()
        .filter(|s| s.file == file)
        .map(|s| format!("{}/{symbol}", s.legacy_prefix()))
        .collect();
    (paths, project.root_file.clone())
}

/// A new hidden property, cloned from one the symbol already has.
fn new_property(
    sym: &List,
    name: &str,
    value: &str,
    version: u32,
    style: &Style,
) -> Result<List, EditError> {
    let at = sym
        .find("at")
        .and_then(|a| Some(Pt::new(model::mm(&a.atom(1)?)?, model::mm(&a.atom(2)?)?)))
        .unwrap_or_default();
    let next_id = sym
        .find_all("property")
        .filter_map(|p| p.value_of("id").and_then(|i| i.parse::<i64>().ok()))
        .max()
        .map(|m| m + 1);
    let base = sym
        .lists()
        .filter(|l| l.is("property"))
        .find(|l| l.atom(1).as_deref() == Some("Footprint"))
        .or_else(|| sym.find("property"))
        .cloned();
    let mut prop = match base {
        Some(p) => p,
        None => {
            // `hide_property` below hides it the way the version wants.
            let snippet = if version >= FIRST_KICAD8_VERSION {
                "(property \"\" \"\" (at 0 0 0) (effects (font (size 1.27 1.27))))".to_string()
            } else {
                "(property \"\" \"\" (id 0) (at 0 0 0) (effects (font (size 1.27 1.27)) hide))"
                    .to_string()
            };
            let mut p = sexpr::parse_list(&snippet)?;
            sexpr::reformat(&mut p, 2, style);
            p
        }
    };
    set_string(&mut prop, 1, name);
    set_string(&mut prop, 2, value);
    set_at(&mut prop, at, Some(0));
    if let (Some(id), Some(c)) = (next_id, prop.find_mut("id")) {
        set_atom(c, 1, &id.to_string());
    }
    hide_property(&mut prop, version, style);
    Ok(prop)
}

fn hide_property(prop: &mut List, version: u32, style: &Style) {
    if (FIRST_KICAD8_VERSION..FIRST_FIELD_HIDE_VERSION).contains(&version) {
        // KiCad 8: `(hide yes)` inside `effects`, after the font.
        if let Some(eff) = prop.find_mut("effects")
            && !set_child_value(eff, "hide", "yes")
            && !eff.has_flag("hide")
        {
            let lead = eff
                .items
                .iter()
                .skip(1)
                .find_map(|n| n.as_list().map(|l| l.lead.clone()))
                .filter(|l| l.contains('\n'))
                .unwrap_or_else(|| format!("{}{}", style.newline, style.indent.repeat(4)));
            let mut hide = sexpr::parse_list("(hide yes)").expect("constant parses");
            hide.lead = lead;
            eff.items.push(Node::List(hide));
        }
        return;
    }
    if version >= FIRST_FIELD_HIDE_VERSION && !(prop.has_flag("hide")) {
        if !set_child_value(prop, "hide", "yes") {
            // `(hide yes)` goes after `(at ...)`, on its own line.
            let pos = prop
                .items
                .iter()
                .position(|n| n.as_list().is_some_and(|l| l.is("at")))
                .map_or(prop.items.len(), |i| i + 1);
            let lead = prop
                .items
                .get(pos.saturating_sub(1))
                .map(|n| n.lead().to_string())
                .unwrap_or_else(|| " ".into());
            let mut hide = sexpr::parse_list("(hide yes)").expect("constant parses");
            hide.lead = if lead.contains('\n') {
                lead
            } else {
                format!("{}{}", style.newline, style.indent.repeat(3))
            };
            prop.items.insert(pos, Node::List(hide));
        }
        return;
    }
    // KiCad 6 and 7: a bare `hide` inside `effects`.
    if let Some(eff) = prop.find_mut("effects")
        && !eff.has_flag("hide")
    {
        let mut a = Atom::bare("hide");
        a.lead = " ".into();
        eff.items.push(Node::Atom(a));
    }
}

fn remove_field(
    project: &mut Project,
    file: &str,
    symbol: &str,
    name: &str,
) -> Result<Outcome, EditError> {
    writable(project, file)?;
    if MANDATORY_FIELDS.contains(&name) {
        return Err(EditError::Mandatory(name.to_string()));
    }
    let root = root_mut(project, file)?;
    let idx = symbol_index(root, symbol).ok_or_else(|| EditError::NoSymbol(symbol.to_string()))?;
    let sym = list_at(root, idx);
    let pos = sym
        .items
        .iter()
        .position(|n| {
            n.as_list()
                .is_some_and(|l| l.is("property") && l.atom(1).as_deref() == Some(name))
        })
        .ok_or_else(|| EditError::NoField(name.to_string()))?;
    sym.items.remove(pos);
    Ok(Outcome {
        files: BTreeSet::from([file.to_string()]),
        ..Default::default()
    })
}

fn set_flag(
    project: &mut Project,
    file: &str,
    symbol: &str,
    flag: Flag,
    value: bool,
) -> Result<Outcome, EditError> {
    writable(project, file)?;
    let version = version(project, file);
    let root = root_mut(project, file)?;
    let idx = symbol_index(root, symbol).ok_or_else(|| EditError::NoSymbol(symbol.to_string()))?;
    let sym = list_at(root, idx);
    let head = match flag {
        Flag::Dnp => "dnp",
        Flag::InBom => "in_bom",
        Flag::OnBoard => "on_board",
    };
    let yes_no = if value { "yes" } else { "no" };
    if !set_child_value(sym, head, yes_no) {
        if flag == Flag::Dnp && version < FIRST_DNP_VERSION {
            return Err(EditError::NoDnpInFormat(version));
        }
        // After the last of the flags KiCad writes before it.
        let pos = sym
            .items
            .iter()
            .rposition(|n| {
                n.as_list().is_some_and(|l| {
                    matches!(
                        l.head(),
                        Some(
                            "unit"
                                | "body_style"
                                | "convert"
                                | "exclude_from_sim"
                                | "in_bom"
                                | "on_board"
                                | "in_pos_files"
                        )
                    )
                })
            })
            .map_or(1, |i| i + 1);
        let lead = sym
            .items
            .get(pos - 1)
            .map(|n| n.lead().to_string())
            .unwrap_or_else(|| " ".into());
        let mut node = sexpr::parse_list(&format!("({head} {yes_no})"))?;
        node.lead = if lead.is_empty() { " ".into() } else { lead };
        sym.items.insert(pos, Node::List(node));
    }
    Ok(Outcome {
        files: BTreeSet::from([file.to_string()]),
        ..Default::default()
    })
}

// ---- placing symbols -------------------------------------------------------------

fn place_symbol(
    project: &mut Project,
    file: &str,
    lib: &str,
    at: Pt,
    rotation: i32,
    unit: u32,
) -> Result<Outcome, EditError> {
    writable(project, file)?;
    // KiCad refuses to load a symbol at any other angle.
    let rotation = match rotation.rem_euclid(360) {
        r @ (0 | 90 | 180 | 270) => r,
        _ => return Err(EditError::BadRotation(rotation)),
    };
    let f = &project.files[file];
    let model = &f.model;
    let ls = model
        .lib_symbols
        .get(lib)
        .ok_or_else(|| EditError::NoLibSymbol(lib.to_string()))?;
    let count = ls.unit_count();
    let unit = unit.max(1);
    if unit > count {
        return Err(EditError::NoUnit { unit, count });
    }
    let lib_node = f
        .doc
        .root
        .find("lib_symbols")
        .and_then(|l| l.items.get(ls.index))
        .and_then(Node::as_list)
        .cloned()
        .ok_or_else(|| EditError::NoLibSymbol(lib.to_string()))?;
    let prefix = ls.property("Reference").unwrap_or("U").to_string();
    let base = clone_first(&f.doc.root, "symbol", |l| {
        l.value_of("lib_id").as_deref() == Some(lib)
    })
    .or_else(|| clone_first(&f.doc.root, "symbol", |_| true));
    let version = model.version;
    let style = Style::detect(&f.doc);
    let mut node = match base {
        Some(n) => n,
        None => symbol_template(version, &style)?,
    };
    // Library identity: the cache key is the lib_id unless it was renamed.
    set_child_value(&mut node, "lib_id", lib);
    if let Some(i) = node
        .items
        .iter()
        .position(|n| n.as_list().is_some_and(|l| l.is("lib_name")))
    {
        node.items.remove(i);
    }
    set_at(&mut node, at, Some(rotation));
    // Nothing of the cloned symbol's own state carries over: its mirroring,
    // KiCad 7 development `default_instance`, or KiCad 10 variant overrides.
    node.items.retain(|n| {
        !n.as_list().is_some_and(|l| {
            matches!(
                l.head(),
                Some("mirror" | "default_instance" | "variant" | "variants")
            )
        })
    });
    set_child_value(&mut node, "unit", &unit.to_string());
    // The default body style, not the clone's (a De Morgan clone would
    // otherwise place a De Morgan symbol, possibly of a library symbol that
    // has none). The pins below are the default body style's.
    for (head, v) in [
        ("convert", "1"),
        ("body_style", "1"),
        ("in_bom", "yes"),
        ("on_board", "yes"),
        ("dnp", "no"),
        ("exclude_from_sim", "no"),
    ] {
        set_child_value(&mut node, head, v);
    }
    // Properties from the library entry, re-based onto the placement.
    let first_prop = node
        .items
        .iter()
        .position(|n| n.as_list().is_some_and(|l| l.is("property")));
    node.items
        .retain(|n| !n.as_list().is_some_and(|l| l.is("property")));
    let t = model::Transform::for_symbol(rotation, false, false);
    let mut props: Vec<Node> = Vec::new();
    for p in lib_node.find_all("property") {
        let name = p.atom(1).unwrap_or_default();
        if name.starts_with("ki_") {
            continue;
        }
        let mut p = p.clone();
        dedent(&mut p, 1, &style);
        if let Some(a) = p.find("at") {
            let off = Pt::new(
                a.atom(1).and_then(|v| model::mm(&v)).unwrap_or(0),
                -a.atom(2).and_then(|v| model::mm(&v)).unwrap_or(0),
            );
            let off = t.apply(off);
            let rot = a.atom(3).and_then(|r| r.parse::<i32>().ok()).unwrap_or(0);
            set_at(&mut p, Pt::new(at.x + off.x, at.y + off.y), Some(rot));
        }
        if name == "Reference" {
            set_string(&mut p, 2, &format!("{prefix}?"));
        }
        props.push(Node::List(p));
    }
    let insert_at = first_prop.unwrap_or_else(|| {
        node.items
            .iter()
            .position(|n| n.as_list().is_some_and(|l| l.is("uuid")))
            .map_or(node.items.len(), |i| i + 1)
    });
    let insert_at = insert_at.min(node.items.len());
    for (k, p) in props.into_iter().enumerate() {
        node.items.insert(insert_at + k, p);
    }
    // Pins of the placed unit, each with its own UUID.
    let pin_numbers: Vec<String> = {
        let mut seen = BTreeSet::new();
        ls.pins_for(unit, 1)
            .filter(|p| seen.insert(p.number.clone()))
            .map(|p| p.number.clone())
            .collect()
    };
    finish_new_symbol(
        project,
        file,
        node,
        Some(&pin_numbers),
        &format!("{prefix}?"),
        unit,
        &style,
    )
}

fn copy_symbol(
    project: &mut Project,
    file: &str,
    symbol: &str,
    at: Pt,
) -> Result<Outcome, EditError> {
    writable(project, file)?;
    let (_, sym) = symbol_model(project, file, symbol)?;
    let prefix = {
        let r = sym.field("Reference").unwrap_or("U");
        r.trim_end_matches(|c: char| c.is_ascii_digit() || c == '?')
            .to_string()
    };
    let unit = sym.unit;
    let dx = at.x - sym.at.x;
    let dy = at.y - sym.at.y;
    let f = &project.files[file];
    let style = Style::detect(&f.doc);
    let idx =
        symbol_index(&f.doc.root, symbol).ok_or_else(|| EditError::NoSymbol(symbol.to_string()))?;
    let mut node = f.doc.root.items[idx]
        .as_list()
        .cloned()
        .expect("symbol is a list");
    // Move the symbol and every field with it.
    shift_at(&mut node, dx, dy);
    for p in node
        .items
        .iter_mut()
        .filter_map(Node::as_list_mut)
        .filter(|l| l.is("property"))
    {
        shift_at(p, dx, dy);
        if p.atom(1).as_deref() == Some("Reference") {
            set_string(p, 2, &format!("{prefix}?"));
        }
    }
    finish_new_symbol(
        project,
        file,
        node,
        None,
        &format!("{prefix}?"),
        unit,
        &style,
    )
}

fn shift_at(l: &mut List, dx: i64, dy: i64) {
    if let Some(a) = l.find_mut("at")
        && let (Some(x), Some(y)) = (
            a.atom(1).and_then(|v| model::mm(&v)),
            a.atom(2).and_then(|v| model::mm(&v)),
        )
    {
        set_atom(a, 1, &fmt_mm(x + dx));
        set_atom(a, 2, &fmt_mm(y + dy));
    }
}

/// Give a new symbol node its UUID, pin UUIDs and instance entries, and
/// insert it.
fn finish_new_symbol(
    project: &mut Project,
    file: &str,
    mut node: List,
    pins: Option<&[String]>,
    reference: &str,
    unit: u32,
    style: &Style,
) -> Result<Outcome, EditError> {
    let version = version(project, file);
    let uuid = new_uuid();
    // A placed symbol always has a UUID; a template without one gets it.
    if !set_uuid(&mut node, &uuid) {
        let mut u = sexpr::parse_list(&format!("(uuid {})", uuid_atom(version, &uuid)))?;
        u.lead = node
            .items
            .last()
            .map(|n| n.lead().to_string())
            .filter(|l| l.contains('\n'))
            .unwrap_or_else(|| " ".into());
        node.items.push(Node::List(u));
    }
    // A copy keeps its pin entries (and any alternate pin functions chosen
    // on them), each with a fresh UUID.
    let Some(pins) = pins else {
        for p in node
            .items
            .iter_mut()
            .filter_map(Node::as_list_mut)
            .filter(|l| l.is("pin"))
        {
            set_uuid(p, &new_uuid());
        }
        return finish_instances(project, file, node, reference, unit, style, version);
    };
    // Pins: replace whatever the clone had, keeping its layout for one entry.
    let pin_template = node.lists().find(|l| l.is("pin")).cloned();
    let first_pin = node
        .items
        .iter()
        .position(|n| n.as_list().is_some_and(|l| l.is("pin")));
    node.items
        .retain(|n| !n.as_list().is_some_and(|l| l.is("pin")));
    let pin_template = match pin_template {
        // A fresh placement uses each pin's default function.
        Some(mut p) => {
            p.items
                .retain(|n| !n.as_list().is_some_and(|l| l.is("alternate")));
            p
        }
        None => {
            let mut p =
                sexpr::parse_list(&format!("(pin \"1\" (uuid {}))", uuid_atom(version, "x")))?;
            if version >= FIRST_KICAD8_VERSION {
                sexpr::reformat(&mut p, 2, style);
            } else {
                p.lead = format!("{}{}", style.newline, style.indent.repeat(2));
            }
            p
        }
    };
    let pin_at = first_pin
        .unwrap_or_else(|| {
            node.items
                .iter()
                .position(|n| n.as_list().is_some_and(|l| l.is("instances")))
                .unwrap_or(node.items.len())
        })
        .min(node.items.len());
    for (k, num) in pins.iter().enumerate() {
        let mut p = pin_template.clone();
        set_string(&mut p, 1, num);
        set_uuid(&mut p, &new_uuid());
        node.items.insert(pin_at + k, Node::List(p));
    }
    finish_instances(project, file, node, reference, unit, style, version)
}

/// Give a new symbol node its instance entries and insert it.
fn finish_instances(
    project: &mut Project,
    file: &str,
    mut node: List,
    reference: &str,
    unit: u32,
    style: &Style,
    version: u32,
) -> Result<Outcome, EditError> {
    let uuid = node.value_of("uuid").unwrap_or_default();
    // Instances: KiCad 7+ in the symbol, KiCad 6 in the root's table.
    let sheets: Vec<SheetPath> = project
        .sheets
        .iter()
        .filter(|s| s.file == file)
        .cloned()
        .collect();
    let root_uuid = project.files[&project.root_file]
        .model
        .uuid
        .clone()
        .unwrap_or_default();
    let mut files = BTreeSet::from([file.to_string()]);
    if version >= FIRST_INSTANCES_VERSION || node.find("instances").is_some() {
        let block = instances_block(
            node.find("instances"),
            &project.name,
            &sheets,
            &root_uuid,
            reference,
            unit,
            version,
            style,
        )?;
        match node
            .items
            .iter()
            .position(|n| n.as_list().is_some_and(|l| l.is("instances")))
        {
            Some(i) => node.items[i] = Node::List(block),
            None => node.items.push(Node::List(block)),
        }
    } else {
        let root_file = project.root_file.clone();
        let rf = project
            .files
            .get_mut(&root_file)
            .expect("root file is loaded");
        let rstyle = Style::detect(&rf.doc);
        let table_idx = rf
            .doc
            .root
            .items
            .iter()
            .position(|n| n.as_list().is_some_and(|l| l.is("symbol_instances")));
        if let Some(ti) = table_idx {
            let table = rf.doc.root.items[ti]
                .as_list_mut()
                .expect("table is a list");
            let tmpl = table.lists().find(|l| l.is("path")).cloned();
            for sp in &sheets {
                let path = format!("{}/{uuid}", sp.legacy_prefix());
                let mut entry = match &tmpl {
                    Some(t) => t.clone(),
                    None => {
                        let mut e = sexpr::parse_list(
                            "(path \"\" (reference \"\") (unit 1) (value \"\") (footprint \"\"))",
                        )?;
                        e.lead = format!("{}{}", rstyle.newline, rstyle.indent.repeat(2));
                        e
                    }
                };
                set_string(&mut entry, 1, &path);
                if let Some(c) = entry.find_mut("reference") {
                    set_string(c, 1, reference);
                }
                set_child_value(&mut entry, "unit", &unit.to_string());
                if let Some(c) = entry.find_mut("value") {
                    set_string(c, 1, "");
                }
                if let Some(c) = entry.find_mut("footprint") {
                    set_string(c, 1, "");
                }
                table.items.push(Node::List(entry));
            }
            files.insert(root_file);
        }
    }
    let root = root_mut(project, file)?;
    insert_root(root, node, style, &TRAILERS);
    Ok(Outcome {
        created: vec![uuid],
        files,
        ..Default::default()
    })
}

#[allow(clippy::too_many_arguments)]
fn instances_block(
    existing: Option<&List>,
    project_name: &str,
    sheets: &[SheetPath],
    root_uuid: &str,
    reference: &str,
    unit: u32,
    version: u32,
    style: &Style,
) -> Result<List, EditError> {
    let path_tmpl = existing
        .and_then(|i| i.find("project"))
        .and_then(|p| p.find("path"))
        .cloned();
    let project_tmpl = existing.and_then(|i| i.find("project")).cloned();
    let mut default =
        sexpr::parse_list("(instances (project \"\" (path \"\" (reference \"\") (unit 1))))")?;
    if version >= FIRST_KICAD8_VERSION {
        sexpr::reformat(&mut default, 2, style);
    }
    let default_project = default
        .find("project")
        .cloned()
        .expect("constant has a project");
    let default_path = default_project
        .find("path")
        .cloned()
        .expect("constant has a path");
    // An existing block (from the symbol being cloned) lends its layout,
    // as far as it has one: a malformed `(instances)` without a project or
    // path falls back to the default for the missing part.
    let mut block = existing.cloned().unwrap_or(default);
    let mut proj = project_tmpl.unwrap_or(default_project);
    let path_tmpl = path_tmpl
        .or_else(|| proj.find("path").cloned())
        .unwrap_or(default_path);
    set_string(&mut proj, 1, project_name);
    proj.items
        .retain(|n| !n.as_list().is_some_and(|l| l.is("path")));
    for sp in sheets {
        let mut p = path_tmpl.clone();
        set_string(&mut p, 1, &sp.kiid_path(root_uuid));
        if let Some(c) = p.find_mut("reference") {
            set_string(c, 1, reference);
        }
        set_child_value(&mut p, "unit", &unit.to_string());
        for k in ["value", "footprint"] {
            if let Some(c) = p.find_mut(k) {
                set_string(c, 1, "");
            }
        }
        proj.items.push(Node::List(p));
    }
    block
        .items
        .retain(|n| !n.as_list().is_some_and(|l| l.is("project")));
    block.items.push(Node::List(proj));
    Ok(block)
}

fn symbol_template(version: u32, style: &Style) -> Result<List, EditError> {
    let u = uuid_atom(version, "00000000-0000-0000-0000-000000000000");
    let snippet = if version >= FIRST_KICAD8_VERSION {
        format!(
            "(symbol (lib_id \"\") (at 0 0 0) (unit 1) (exclude_from_sim no) (in_bom yes) (on_board yes) (dnp no) (uuid {u}))"
        )
    } else if version >= FIRST_DNP_VERSION {
        format!(
            "(symbol (lib_id \"\") (at 0 0 0) (unit 1)\n    (in_bom yes) (on_board yes) (dnp no)\n    (uuid {u})\n  )"
        )
    } else {
        format!(
            "(symbol (lib_id \"\") (at 0 0 0) (unit 1)\n    (in_bom yes) (on_board yes)\n    (uuid {u})\n  )"
        )
    };
    template(&snippet, version, style)
}

// ---- wires, labels, flags ------------------------------------------------------

fn add_wire(project: &mut Project, file: &str, points: &[Pt]) -> Result<Vec<String>, EditError> {
    let mut pts: Vec<Pt> = points.to_vec();
    pts.dedup();
    if pts.len() < 2 {
        return Err(EditError::BadWire);
    }
    let version = version(project, file);
    let style = style_of(project, file);
    let base = project.files[file]
        .doc
        .root
        .lists()
        .find(|l| l.is("wire"))
        .cloned();
    let mut created = Vec::new();
    for w in pts.windows(2) {
        let mut node = match &base {
            Some(b) => b.clone(),
            None => {
                let u = uuid_atom(version, "x");
                let snippet = if version >= FIRST_KICAD8_VERSION {
                    format!(
                        "(wire (pts (xy 0 0) (xy 0 0)) (stroke (width 0) (type default)) (uuid {u}))"
                    )
                } else {
                    format!(
                        "(wire (pts (xy 0 0) (xy 0 0))\n    (stroke (width 0) (type default) (color 0 0 0 0))\n    (uuid {u})\n  )"
                    )
                };
                template(&snippet, version, &style)?
            }
        };
        if let Some(p) = node.find_mut("pts") {
            let mut xys = p
                .items
                .iter_mut()
                .filter_map(Node::as_list_mut)
                .filter(|l| l.is("xy"));
            for pt in [w[0], w[1]] {
                if let Some(xy) = xys.next() {
                    set_atom(xy, 1, &fmt_mm(pt.x));
                    set_atom(xy, 2, &fmt_mm(pt.y));
                }
            }
        }
        let uuid = new_uuid();
        let has_uuid = set_uuid(&mut node, &uuid);
        insert_root(root_mut(project, file)?, node, &style, &TRAILERS);
        if has_uuid {
            created.push(uuid);
        }
    }
    Ok(created)
}

fn add_no_connect(project: &mut Project, file: &str, at: Pt) -> Result<Option<String>, EditError> {
    let version = version(project, file);
    let style = style_of(project, file);
    let base = project.files[file]
        .doc
        .root
        .lists()
        .find(|l| l.is("no_connect"))
        .cloned();
    let mut node = match base {
        Some(b) => b,
        None => template(
            &format!("(no_connect (at 0 0) (uuid {}))", uuid_atom(version, "x")),
            version,
            &style,
        )?,
    };
    set_at(&mut node, at, None);
    let uuid = new_uuid();
    let has_uuid = set_uuid(&mut node, &uuid);
    insert_root(root_mut(project, file)?, node, &style, &TRAILERS);
    Ok(has_uuid.then_some(uuid))
}

fn add_label(
    project: &mut Project,
    file: &str,
    text: &str,
    at: Pt,
    angle: i32,
    scope: LabelScope,
) -> Result<Option<String>, EditError> {
    let version = version(project, file);
    let style = style_of(project, file);
    let head = match scope {
        LabelScope::Local => "label",
        LabelScope::Global => "global_label",
    };
    let base = project.files[file]
        .doc
        .root
        .lists()
        .find(|l| l.is(head))
        .cloned();
    let mut node = match base {
        Some(b) => b,
        None => {
            let u = uuid_atom(version, "x");
            let snippet = match scope {
                LabelScope::Local => {
                    format!(
                        "(label \"\" (at 0 0 0) (effects (font (size 1.27 1.27)) (justify left bottom)) (uuid {u}))"
                    )
                }
                LabelScope::Global if version >= FIRST_KICAD8_VERSION => format!(
                    "(global_label \"\" (shape bidirectional) (at 0 0 0) (fields_autoplaced yes) (effects (font (size 1.27 1.27)) (justify left)) (uuid {u}) (property \"Intersheetrefs\" \"${{INTERSHEET_REFS}}\" (at 0 0 0) (effects (font (size 1.27 1.27)) (justify left) (hide yes))))"
                ),
                LabelScope::Global => format!(
                    "(global_label \"\" (shape bidirectional) (at 0 0 0) (fields_autoplaced)\n    (effects (font (size 1.27 1.27)) (justify left))\n    (uuid {u})\n  )"
                ),
            };
            template(&snippet, version, &style)?
        }
    };
    set_string(&mut node, 1, text);
    set_at(&mut node, at, Some(angle));
    // A connection made by name has no direction to claim.
    set_child_value(&mut node, "shape", "bidirectional");
    // Text runs away from the wire end: left-justified at 0 and 90 degrees,
    // right-justified at 180 and 270.
    let side = if matches!(angle.rem_euclid(360), 180 | 270) {
        "right"
    } else {
        "left"
    };
    if let Some(j) = node.find_mut("effects").and_then(|e| e.find_mut("justify")) {
        set_atom(j, 1, side);
    }
    for p in node
        .items
        .iter_mut()
        .filter_map(Node::as_list_mut)
        .filter(|l| l.is("property"))
    {
        set_at(p, at, Some(0));
    }
    let uuid = new_uuid();
    let has_uuid = set_uuid(&mut node, &uuid);
    insert_root(root_mut(project, file)?, node, &style, &TRAILERS);
    Ok(has_uuid.then_some(uuid))
}

fn pin_geometry(
    project: &Project,
    file: &str,
    pin: &PinRef,
) -> Result<model::PlacedPin, EditError> {
    let (model, sym) = symbol_model(project, file, &pin.symbol)?;
    if model.lib_symbol_for(sym).is_none() {
        return Err(EditError::NoLibSymbol(sym.cache_key().to_string()));
    }
    model
        .placed_pins(sym)
        .into_iter()
        .find(|p| p.pin.number == pin.pin)
        .ok_or_else(|| EditError::NoPin {
            symbol: pin.symbol.clone(),
            pin: pin.pin.clone(),
        })
}

fn connect_pins(
    project: &mut Project,
    file: &str,
    a: &PinRef,
    b: &PinRef,
) -> Result<Outcome, EditError> {
    writable(project, file)?;
    let pa = pin_geometry(project, file, a)?;
    let pb = pin_geometry(project, file, b)?;
    let points = if pa.at.x == pb.at.x || pa.at.y == pb.at.y {
        vec![pa.at, pb.at]
    } else if pa.outward.x != 0 {
        // Leave the first pin horizontally.
        vec![pa.at, Pt::new(pb.at.x, pa.at.y), pb.at]
    } else {
        vec![pa.at, Pt::new(pa.at.x, pb.at.y), pb.at]
    };
    let created = add_wire(project, file, &points)?;
    Ok(Outcome {
        created,
        files: BTreeSet::from([file.to_string()]),
        ..Default::default()
    })
}

fn connect_pin_to_net(
    project: &mut Project,
    file: &str,
    pin: &PinRef,
    net: &str,
    scope: LabelScope,
    stub: i64,
) -> Result<Outcome, EditError> {
    writable(project, file)?;
    if net.trim().is_empty() {
        return Err(EditError::EmptyName);
    }
    let p = pin_geometry(project, file, pin)?;
    let len = if stub > 0 {
        stub
    } else {
        (2.54 * IU_PER_MM) as i64
    };
    let end = Pt::new(p.at.x + p.outward.x * len, p.at.y + p.outward.y * len);
    let mut created = add_wire(project, file, &[p.at, end])?;
    let angle = match (p.outward.x, p.outward.y) {
        (1, _) => 0,
        (-1, _) => 180,
        (_, -1) => 90,
        _ => 270,
    };
    created.extend(add_label(project, file, net, end, angle, scope)?);
    Ok(Outcome {
        created,
        files: BTreeSet::from([file.to_string()]),
        ..Default::default()
    })
}

// ---- annotation ----------------------------------------------------------------

/// Split `R12` into `("R", Some(12))` and `R?` into `("R", None)`.
fn split_reference(r: &str) -> (String, Option<u64>) {
    let digits = r.chars().rev().take_while(|c| c.is_ascii_digit()).count();
    let (prefix, num) = r.split_at(r.len() - digits);
    if let Some(p) = prefix.strip_suffix('?') {
        return (p.to_string(), None);
    }
    (
        prefix.to_string(),
        if num.is_empty() {
            None
        } else {
            num.parse().ok()
        },
    )
}

fn annotate(project: &mut Project, reset: bool) -> Result<Outcome, EditError> {
    for (name, f) in &project.files {
        if f.is_newer_than_known() {
            return Err(EditError::ReadOnly {
                file: name.clone(),
                version: f.model.version,
            });
        }
    }
    // Every placed symbol instance: (sheet index, symbol uuid, file, current
    // reference, position).
    struct Inst {
        sheet: usize,
        file: String,
        uuid: String,
        reference: String,
        at: Pt,
        /// For a unit of a multi-unit part that already has a number: the
        /// package it belongs to (library symbol and reference). Every
        /// unit of one package gets the same new number.
        package: Option<(String, String)>,
    }
    let mut all: Vec<Inst> = Vec::new();
    for (si, sp) in project.sheets.iter().enumerate() {
        let f = &project.files[&sp.file];
        for sym in &f.model.symbols {
            let r = project.resolve_instance(si, sym);
            let multi_unit = f
                .model
                .lib_symbol_for(sym)
                .is_some_and(|l| l.unit_count() > 1);
            let package = (multi_unit && split_reference(&r.reference).1.is_some())
                .then(|| (sym.lib_id.clone(), r.reference.clone()));
            all.push(Inst {
                sheet: si,
                file: sp.file.clone(),
                uuid: sym.uuid.clone(),
                reference: r.reference,
                at: sym.at,
                package,
            });
        }
    }
    let mut used: BTreeMap<String, BTreeSet<u64>> = BTreeMap::new();
    let mut todo: Vec<usize> = Vec::new();
    for (i, inst) in all.iter().enumerate() {
        let (prefix, num) = split_reference(&inst.reference);
        match num {
            Some(n) if !reset => {
                used.entry(prefix).or_default().insert(n);
            }
            _ => todo.push(i),
        }
    }
    // Hierarchy order, then X, then Y (KiCad's default "sort by X position").
    todo.sort_by_key(|&i| (all[i].sheet, all[i].at.x, all[i].at.y));
    let root_uuid = project.files[&project.root_file]
        .model
        .uuid
        .clone()
        .unwrap_or_default();
    let mut out = Outcome::default();
    // Package to the number its first unit (in the order above) was given.
    let mut package_number: BTreeMap<(String, String), u64> = BTreeMap::new();
    for i in todo {
        let inst = &all[i];
        let (prefix, _) = split_reference(&inst.reference);
        let set = used.entry(prefix.clone()).or_default();
        let n = match inst.package.as_ref().and_then(|p| package_number.get(p)) {
            Some(&n) => n,
            None => {
                let mut n = 1;
                while set.contains(&n) {
                    n += 1;
                }
                set.insert(n);
                if let Some(p) = &inst.package {
                    package_number.insert(p.clone(), n);
                }
                n
            }
        };
        let new_ref = format!("{prefix}{n}");
        let sp = project.sheets[inst.sheet].clone();
        let kiid = sp.kiid_path(&root_uuid);
        let legacy = format!("{}/{}", sp.legacy_prefix(), inst.uuid);
        let mut done = false;
        {
            let root = root_mut(project, &inst.file)?;
            let idx = symbol_index(root, &inst.uuid)
                .ok_or_else(|| EditError::NoSymbol(inst.uuid.clone()))?;
            let sym = list_at(root, idx);
            if let Some(block) = sym.find_mut("instances") {
                for proj in block
                    .items
                    .iter_mut()
                    .filter_map(Node::as_list_mut)
                    .filter(|l| l.is("project"))
                {
                    for path in proj
                        .items
                        .iter_mut()
                        .filter_map(Node::as_list_mut)
                        .filter(|l| l.is("path"))
                    {
                        if path.atom(1).as_deref() == Some(kiid.as_str())
                            && let Some(c) = path.find_mut("reference")
                        {
                            set_string(c, 1, &new_ref);
                            done = true;
                        }
                    }
                }
            }
            if let Some(p) = sym
                .items
                .iter_mut()
                .filter_map(Node::as_list_mut)
                .find(|l| l.is("property") && l.atom(1).as_deref() == Some("Reference"))
            {
                set_string(p, 2, &new_ref);
            }
        }
        out.files.insert(inst.file.clone());
        if !done {
            let root_file = project.root_file.clone();
            let rf = project
                .files
                .get_mut(&root_file)
                .expect("root file is loaded");
            for table in rf
                .doc
                .root
                .items
                .iter_mut()
                .filter_map(Node::as_list_mut)
                .filter(|l| l.is("symbol_instances"))
            {
                for path in table
                    .items
                    .iter_mut()
                    .filter_map(Node::as_list_mut)
                    .filter(|l| l.is("path"))
                {
                    if path.atom(1).as_deref() == Some(legacy.as_str())
                        && let Some(c) = path.find_mut("reference")
                    {
                        set_string(c, 1, &new_ref);
                        done = true;
                    }
                }
            }
            if done {
                out.files.insert(root_file);
            }
        }
        out.renamed
            .push((sp.human(), inst.reference.clone(), new_ref));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uuids_are_v4_and_distinct() {
        let a = new_uuid();
        let b = new_uuid();
        assert_ne!(a, b);
        assert_eq!(a.len(), 36);
        assert_eq!(&a[14..15], "4");
        assert!(matches!(&a[19..20], "8" | "9" | "a" | "b"));
    }

    #[test]
    fn references_split() {
        assert_eq!(split_reference("R12"), ("R".into(), Some(12)));
        assert_eq!(split_reference("R?"), ("R".into(), None));
        assert_eq!(split_reference("#PWR?"), ("#PWR".into(), None));
        assert_eq!(split_reference("U"), ("U".into(), None));
    }

    #[test]
    fn dedent_strips_one_level() {
        let mut l = sexpr::parse_list("(a\n\t\t\t(b 1)\n\t\t)").unwrap();
        dedent(&mut l, 1, &Style::default());
        let doc = sexpr::Document {
            root: l,
            trailing: String::new(),
        };
        assert_eq!(doc.to_text(), "(a\n\t\t(b 1)\n\t)");
    }
}
