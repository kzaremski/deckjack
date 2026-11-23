//! A KiCad project: the root schematic, the sheets it pulls in, and the
//! sheet hierarchy.
//!
//! **Confinement.** Sheet file names come from the files themselves, which
//! are untrusted. A sheet is loaded only when its file, after resolving `..`
//! and symbolic links, lies inside the project directory. Anything else (an
//! absolute path elsewhere, `../../etc/passwd`, a symlink pointing out) is
//! refused with a warning and the sheet is left empty, never read.
//!
//! **Hierarchy.** One file can be placed several times (KiCad's "shared"
//! sheets). Each placement is a separate [`SheetPath`], with its own
//! references and its own local nets. A sheet that contains itself, directly
//! or through descendants, is refused with a warning rather than recursing.
//! The number of sheet instances and their depth are both capped.
//!
//! **Order.** Sheet paths are listed as KiCad lists them for its netlist:
//! depth first, then sorted by page number, ties kept in depth-first order.

use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};

use crate::model::{Sheet, SheetModel, Symbol};
use crate::netlist::str_num_cmp;
use crate::project_file::ProjectFile;
use crate::sexpr::{self, Document};

/// At most this many sheet instances in one project.
pub const MAX_SHEET_INSTANCES: usize = 4096;
/// At most this many levels of sheets.
pub const MAX_SHEET_DEPTH: usize = 64;

#[derive(Debug, thiserror::Error)]
pub enum ProjectError {
    #[error("cannot read {path}: {source}")]
    Io {
        path: String,
        source: std::io::Error,
    },
    #[error("{path}: {source}")]
    Parse { path: String, source: sexpr::Error },
    #[error("{path}: {source}")]
    Model {
        path: String,
        source: crate::model::ModelError,
    },
    #[error("{path}: {source}")]
    ProjectFile {
        path: String,
        source: crate::project_file::Error,
    },
    #[error("{0} is not inside the project directory")]
    OutsideProject(String),
}

enum Bounded {
    Read(Vec<u8>),
    /// The file's size, over the limit; nothing was read.
    TooLarge(usize),
}

/// Read a file of at most `limit` bytes. A larger file is refused from its
/// size before any of it is read, and the read itself stops one byte past
/// the limit, so a file that grows after the check is bounded too.
fn read_bounded(path: &Path, limit: usize) -> std::io::Result<Bounded> {
    use std::io::Read;
    let f = std::fs::File::open(path)?;
    let size = f.metadata()?.len();
    if size > limit as u64 {
        return Ok(Bounded::TooLarge(
            usize::try_from(size).unwrap_or(usize::MAX),
        ));
    }
    let mut bytes = Vec::with_capacity(size as usize);
    f.take(limit as u64 + 1).read_to_end(&mut bytes)?;
    if bytes.len() > limit {
        return Ok(Bounded::TooLarge(bytes.len()));
    }
    Ok(Bounded::Read(bytes))
}

/// One loaded `.kicad_sch` file.
#[derive(Debug, Clone)]
pub struct SchFile {
    /// The bytes as read, for the unmodified-save check.
    pub original: Vec<u8>,
    pub doc: Document,
    pub model: SheetModel,
}

impl SchFile {
    pub fn parse(bytes: Vec<u8>, path: &str) -> Result<SchFile, ProjectError> {
        let doc = sexpr::parse(&bytes).map_err(|source| ProjectError::Parse {
            path: path.into(),
            source,
        })?;
        let model = SheetModel::read(&doc).map_err(|source| ProjectError::Model {
            path: path.into(),
            source,
        })?;
        Ok(SchFile {
            original: bytes,
            doc,
            model,
        })
    }

    /// Re-derive the typed view after the tree was edited.
    pub fn refresh(&mut self) -> Result<(), crate::model::ModelError> {
        self.model = SheetModel::read(&self.doc)?;
        Ok(())
    }

    /// The text to save.
    pub fn to_text(&self) -> String {
        self.doc.to_text()
    }

    /// Whether this file is newer than any format this code knows, so must
    /// be opened read-only.
    pub fn is_newer_than_known(&self) -> bool {
        self.model.version > crate::model::NEWEST_KNOWN_VERSION
    }
}

/// One placement of a sheet in the hierarchy.
#[derive(Debug, Clone, PartialEq)]
pub struct SheetPath {
    /// The file, relative to the project directory, with `/` separators.
    pub file: String,
    /// The UUIDs of the sheet items from the root down (the root itself is
    /// not a sheet item and is not included).
    pub uuids: Vec<String>,
    /// The sheet names along the way, for the human-readable path.
    pub names: Vec<String>,
    pub page: String,
    pub excluded_from_board: bool,
    pub excluded_from_bom: bool,
    pub dnp: bool,
    /// Index of the parent in [`Project::sheets`], `None` for the root.
    pub parent: Option<usize>,
    /// For a child, the index of its sheet item in the parent file's model.
    pub sheet_item: Option<usize>,
}

impl SheetPath {
    /// KiCad's `PathHumanReadable` with escaped sheet names: `/` for the
    /// root, `/a/b/` below it.
    pub fn human(&self) -> String {
        let mut s = String::from("/");
        for n in &self.names {
            s.push_str(&n.replace('/', "{slash}"));
            s.push('/');
        }
        s
    }

    /// The UUID path KiCad 7+ writes in `instances`: `/root-uuid/a/b`.
    pub fn kiid_path(&self, root_uuid: &str) -> String {
        let mut s = format!("/{root_uuid}");
        for u in &self.uuids {
            s.push('/');
            s.push_str(u);
        }
        s
    }

    /// The path without the root UUID, as KiCad 6's tables write it.
    pub fn legacy_prefix(&self) -> String {
        let mut s = String::new();
        for u in &self.uuids {
            s.push('/');
            s.push_str(u);
        }
        s
    }

    /// The `tstamps` value of a netlist's `sheetpath`: `/a/b/`.
    pub fn tstamps(&self) -> String {
        let mut s = self.legacy_prefix();
        s.push('/');
        s
    }
}

/// A loaded project.
#[derive(Debug, Clone)]
pub struct Project {
    pub dir: PathBuf,
    pub name: String,
    pub project_file: Option<ProjectFile>,
    pub files: BTreeMap<String, SchFile>,
    pub root_file: String,
    pub sheets: Vec<SheetPath>,
    pub warnings: Vec<String>,
}

/// A placed symbol's reference, unit and value in one sheet instance.
#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedInstance {
    pub reference: String,
    pub unit: u32,
    pub value: String,
    pub footprint: String,
}

impl Project {
    /// Open a project from its `.kicad_pro` or its root `.kicad_sch`.
    pub fn open(path: &Path) -> Result<Project, ProjectError> {
        let path_str = path.display().to_string();
        let abs = std::fs::canonicalize(path).map_err(|source| ProjectError::Io {
            path: path_str.clone(),
            source,
        })?;
        let dir = abs
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("/"));
        let stem = abs
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        let pro_path = dir.join(format!("{stem}.kicad_pro"));
        let pro_error = |source| ProjectError::ProjectFile {
            path: pro_path.display().to_string(),
            source,
        };
        let project_file = match read_bounded(&pro_path, crate::project_file::MAX_BYTES) {
            Ok(Bounded::Read(bytes)) => Some(ProjectFile::parse(&bytes).map_err(pro_error)?),
            Ok(Bounded::TooLarge(size)) => {
                return Err(pro_error(crate::project_file::Error::TooLarge(size)));
            }
            Err(_) => None,
        };
        let root_file = format!("{stem}.kicad_sch");
        let mut project = Project {
            dir,
            name: stem,
            project_file,
            files: BTreeMap::new(),
            root_file: root_file.clone(),
            sheets: Vec::new(),
            warnings: Vec::new(),
        };
        project.load_file(&root_file)?;
        project.build_hierarchy();
        Ok(project)
    }

    fn load_file(&mut self, rel: &str) -> Result<(), ProjectError> {
        if self.files.contains_key(rel) {
            return Ok(());
        }
        let full = self.dir.join(rel);
        let limit = sexpr::Limits::default().max_bytes;
        let bytes = match read_bounded(&full, limit) {
            Ok(Bounded::Read(bytes)) => bytes,
            Ok(Bounded::TooLarge(size)) => {
                return Err(ProjectError::Parse {
                    path: rel.into(),
                    source: sexpr::Error::TooLarge { size, limit },
                });
            }
            Err(source) => {
                return Err(ProjectError::Io {
                    path: rel.into(),
                    source,
                });
            }
        };
        let file = SchFile::parse(bytes, rel)?;
        self.files.insert(rel.to_string(), file);
        Ok(())
    }

    /// Resolve a sheet's file name, relative to the file that places it, to
    /// a path relative to the project directory; `None` when it would leave
    /// the directory.
    fn resolve_sheet_file(&self, parent_rel: &str, name: &str) -> Option<String> {
        if name.is_empty() || name.contains('\0') {
            return None;
        }
        let name = name.replace('\\', "/");
        let parent_dir = Path::new(parent_rel).parent().unwrap_or(Path::new(""));
        let candidates = [parent_dir.join(&name), PathBuf::from(&name)];
        for cand in candidates {
            if cand.is_absolute() {
                continue;
            }
            // Lexical normalization first: no component may climb out.
            let mut parts: Vec<String> = Vec::new();
            let mut ok = true;
            for c in cand.components() {
                match c {
                    Component::Normal(p) => parts.push(p.to_string_lossy().into_owned()),
                    Component::CurDir => {}
                    Component::ParentDir => {
                        if parts.pop().is_none() {
                            ok = false;
                            break;
                        }
                    }
                    _ => {
                        ok = false;
                        break;
                    }
                }
            }
            if !ok || parts.is_empty() {
                continue;
            }
            let rel = parts.join("/");
            // Then the real path: a symlink must not lead outside either.
            let Ok(real) = std::fs::canonicalize(self.dir.join(&rel)) else {
                continue;
            };
            if !real.starts_with(&self.dir) || !real.is_file() {
                continue;
            }
            return Some(rel);
        }
        None
    }

    fn root_uuid(&self) -> String {
        self.files
            .get(&self.root_file)
            .and_then(|f| f.model.uuid.clone())
            .unwrap_or_default()
    }

    fn build_hierarchy(&mut self) {
        let root_page = self
            .files
            .get(&self.root_file)
            .and_then(|f| {
                f.model
                    .legacy_sheet_pages
                    .iter()
                    .find(|(p, _)| p == "/")
                    .map(|(_, pg)| pg.clone())
            })
            .unwrap_or_else(|| "1".into());
        let mut sheets = vec![SheetPath {
            file: self.root_file.clone(),
            uuids: Vec::new(),
            names: Vec::new(),
            page: root_page,
            excluded_from_board: false,
            excluded_from_bom: false,
            dnp: false,
            parent: None,
            sheet_item: None,
        }];
        // Depth first, children in file order.
        let mut order: Vec<usize> = Vec::new();
        let mut stack = vec![0usize];
        let root_uuid = self.root_uuid();
        while let Some(idx) = stack.pop() {
            order.push(idx);
            let sp = sheets[idx].clone();
            if sp.uuids.len() >= MAX_SHEET_DEPTH {
                self.warnings.push(format!(
                    "sheets nested deeper than {MAX_SHEET_DEPTH} levels were not loaded"
                ));
                continue;
            }
            let Some(file) = self.files.get(&sp.file) else {
                continue;
            };
            let children: Vec<Sheet> = file.model.sheets.clone();
            let mut new_children = Vec::new();
            for (item_idx, sheet) in children.iter().enumerate() {
                if sheets.len() >= MAX_SHEET_INSTANCES {
                    self.warnings.push(format!(
                        "more than {MAX_SHEET_INSTANCES} sheet instances; the rest were not loaded"
                    ));
                    break;
                }
                let Some(rel) = self.resolve_sheet_file(&sp.file, &sheet.file) else {
                    self.warnings.push(format!(
                        "sheet \"{}\" names \"{}\", which is missing or outside the project directory; it was not loaded",
                        sheet.name, sheet.file
                    ));
                    continue;
                };
                // A sheet that contains itself.
                let mut anc = Some(idx);
                let mut recursive = false;
                while let Some(a) = anc {
                    if sheets[a].file == rel {
                        recursive = true;
                        break;
                    }
                    anc = sheets[a].parent;
                }
                if recursive {
                    self.warnings.push(format!(
                        "sheet \"{}\" contains itself; the recursion was not followed",
                        sheet.name
                    ));
                    continue;
                }
                if let Err(e) = self.load_file(&rel) {
                    self.warnings
                        .push(format!("sheet \"{}\" could not be loaded: {e}", sheet.name));
                    continue;
                }
                let mut uuids = sp.uuids.clone();
                uuids.push(sheet.uuid.clone());
                let mut names = sp.names.clone();
                names.push(sheet.name.clone());
                let parent_path = sp.kiid_path(&root_uuid);
                let legacy = {
                    let mut s = sp.legacy_prefix();
                    s.push('/');
                    s.push_str(&sheet.uuid);
                    s
                };
                let page = sheet
                    .instances
                    .iter()
                    .find(|i| i.path == parent_path && i.project == self.name)
                    .or_else(|| sheet.instances.iter().find(|i| i.path == parent_path))
                    .and_then(|i| i.page.clone())
                    .or_else(|| {
                        self.files.get(&self.root_file).and_then(|f| {
                            f.model
                                .legacy_sheet_pages
                                .iter()
                                .find(|(p, _)| *p == legacy)
                                .map(|(_, pg)| pg.clone())
                        })
                    })
                    .unwrap_or_default();
                sheets.push(SheetPath {
                    file: rel,
                    uuids,
                    names,
                    page,
                    excluded_from_board: sp.excluded_from_board || !sheet.on_board,
                    excluded_from_bom: sp.excluded_from_bom || !sheet.in_bom,
                    dnp: sp.dnp || sheet.dnp,
                    parent: Some(idx),
                    sheet_item: Some(item_idx),
                });
                new_children.push(sheets.len() - 1);
            }
            // Push in reverse so the first child is visited first.
            stack.extend(new_children.into_iter().rev());
        }
        // Sort by page number, ties in depth-first order. Parent indices
        // are remapped afterwards.
        let mut ranked: Vec<(usize, usize)> = order
            .iter()
            .enumerate()
            .map(|(rank, &i)| (rank, i))
            .collect();
        ranked
            .sort_by(|a, b| compare_page(&sheets[a.1].page, &sheets[b.1].page).then(a.0.cmp(&b.0)));
        let mut remap = vec![0usize; sheets.len()];
        for (new, (_, old)) in ranked.iter().enumerate() {
            remap[*old] = new;
        }
        let mut sorted: Vec<SheetPath> =
            ranked.iter().map(|(_, old)| sheets[*old].clone()).collect();
        for s in &mut sorted {
            s.parent = s.parent.map(|p| remap[p]);
        }
        self.sheets = sorted;
    }

    /// The file of a sheet instance.
    pub fn file_of(&self, sheet: usize) -> &SchFile {
        &self.files[&self.sheets[sheet].file]
    }

    /// A symbol's reference, unit, value and footprint in one sheet
    /// instance, following KiCad's rules: the symbol's own `instances` entry
    /// for this path, else KiCad 6's root `symbol_instances` table, else
    /// the Reference field.
    pub fn resolve_instance(&self, sheet: usize, sym: &Symbol) -> ResolvedInstance {
        let sp = &self.sheets[sheet];
        let root_uuid = self.root_uuid();
        let full = sp.kiid_path(&root_uuid);
        let mut reference = None;
        let mut unit = None;
        let mut value = None;
        let mut footprint = None;
        let inst = sym
            .instances
            .iter()
            .find(|i| i.path == full && i.project == self.name)
            .or_else(|| sym.instances.iter().find(|i| i.path == full))
            .or_else(|| {
                // A root without a UUID (early KiCad 6 files): match on
                // everything below the root.
                if root_uuid.is_empty() {
                    let tail = sp.legacy_prefix();
                    sym.instances.iter().find(|i| {
                        i.path
                            .split_once('/')
                            .is_some_and(|_| strip_first(&i.path) == tail)
                    })
                } else {
                    None
                }
            });
        if let Some(i) = inst {
            reference = i.reference.clone();
            unit = i.unit;
            value = i.value.clone().filter(|v| !v.is_empty());
            footprint = i.footprint.clone().filter(|v| !v.is_empty());
        }
        if reference.is_none() {
            let legacy_path = format!("{}/{}", sp.legacy_prefix(), sym.uuid);
            if let Some(root) = self.files.get(&self.root_file)
                && let Some(li) = root
                    .model
                    .legacy_symbol_instances
                    .iter()
                    .find(|l| l.path == legacy_path)
            {
                reference = Some(li.reference.clone());
                unit = li.unit;
                value = li.value.clone().filter(|v| !v.is_empty());
                footprint = li.footprint.clone().filter(|v| !v.is_empty());
            }
        }
        ResolvedInstance {
            reference: reference
                .filter(|r| !r.is_empty())
                .unwrap_or_else(|| sym.field("Reference").unwrap_or("?").to_string()),
            unit: unit.unwrap_or(sym.unit),
            value: value.unwrap_or_else(|| sym.field("Value").unwrap_or("").to_string()),
            footprint: footprint
                .unwrap_or_else(|| sym.field("Footprint").unwrap_or("").to_string()),
        }
    }

    /// The sub-part separator and first letter from the project file
    /// (`schematic.subpart_id_separator` and `subpart_first_id`).
    pub fn subpart_settings(&self) -> (Option<char>, char) {
        let pf = self.project_file.as_ref();
        let sep = pf
            .and_then(|p| p.get(&["schematic", "subpart_id_separator"]))
            .and_then(|v| v.as_i64())
            .and_then(|c| {
                if c == 0 {
                    None
                } else {
                    char::from_u32(c as u32)
                }
            });
        let first = pf
            .and_then(|p| p.get(&["schematic", "subpart_first_id"]))
            .and_then(|v| v.as_i64())
            .and_then(|c| char::from_u32(c as u32))
            .unwrap_or('A');
        (sep, first)
    }

    /// Text for each file that changed since it was read, keyed by its path
    /// relative to the project directory.
    pub fn changed_files(&self) -> Vec<(String, String)> {
        self.files
            .iter()
            .filter_map(|(k, f)| {
                let text = f.to_text();
                (text.as_bytes() != f.original.as_slice()).then(|| (k.clone(), text))
            })
            .collect()
    }
}

fn strip_first(path: &str) -> String {
    // "/a/b/c" -> "/b/c"
    let rest = path.strip_prefix('/').unwrap_or(path);
    match rest.split_once('/') {
        Some((_, tail)) => format!("/{tail}"),
        None => String::new(),
    }
}

/// KiCad's `SCH_SHEET::ComparePageNum`: numbers numerically, otherwise a
/// natural string comparison, with an empty page last.
fn compare_page(a: &str, b: &str) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    match (a.is_empty(), b.is_empty()) {
        (true, true) => return Ordering::Equal,
        (true, false) => return Ordering::Greater,
        (false, true) => return Ordering::Less,
        _ => {}
    }
    match (a.parse::<i64>(), b.parse::<i64>()) {
        (Ok(x), Ok(y)) => x.cmp(&y),
        _ => str_num_cmp(a, b, true),
    }
}

/// The letter suffix KiCad gives a unit: 1 is A, 26 is Z, 27 is AA.
pub fn unit_letters(unit: u32, first: char) -> String {
    if unit == 0 {
        return String::new();
    }
    let base = first as u32;
    let mut n = unit;
    let mut out = Vec::new();
    while n > 0 {
        let rem = (n - 1) % 26;
        out.push(char::from_u32(base + rem).unwrap_or('?'));
        n = (n - 1) / 26;
    }
    out.iter().rev().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn letters() {
        assert_eq!(unit_letters(1, 'A'), "A");
        assert_eq!(unit_letters(26, 'A'), "Z");
        assert_eq!(unit_letters(27, 'A'), "AA");
        assert_eq!(unit_letters(2, 'a'), "b");
    }

    #[test]
    fn human_paths() {
        let sp = SheetPath {
            file: "a.kicad_sch".into(),
            uuids: vec!["u1".into(), "u2".into()],
            names: vec!["top".into(), "a/b".into()],
            page: "2".into(),
            excluded_from_board: false,
            excluded_from_bom: false,
            dnp: false,
            parent: None,
            sheet_item: None,
        };
        assert_eq!(sp.human(), "/top/a{slash}b/");
        assert_eq!(sp.kiid_path("r"), "/r/u1/u2");
        assert_eq!(sp.legacy_prefix(), "/u1/u2");
        assert_eq!(strip_first("/r/u1/u2"), "/u1/u2");
    }
}
