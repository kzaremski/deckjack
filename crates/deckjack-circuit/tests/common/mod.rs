//! Shared helpers for the fixture-driven tests.
#![allow(dead_code)]

use std::path::{Path, PathBuf};

pub fn fixtures_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/kicad-qa")
}

/// One fixture project: its directory, root schematic and reference
/// netlist (if upstream ships one).
#[derive(Debug, Clone)]
pub struct Fixture {
    pub name: String,
    pub dir: PathBuf,
    pub root: PathBuf,
    pub reference: Option<PathBuf>,
}

/// Every fixture project, sorted by name. The root schematic is the one
/// named after the `.kicad_pro` (or the `.net`, or the directory).
pub fn fixtures() -> Vec<Fixture> {
    let mut out = Vec::new();
    let mut dirs: Vec<PathBuf> = std::fs::read_dir(fixtures_dir())
        .expect("fixtures directory")
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect();
    dirs.sort();
    for dir in dirs {
        let name = dir.file_name().unwrap().to_string_lossy().into_owned();
        let with_ext = |ext: &str| -> Option<PathBuf> {
            let mut v: Vec<PathBuf> = std::fs::read_dir(&dir)
                .ok()?
                .filter_map(Result::ok)
                .map(|e| e.path())
                .filter(|p| p.extension().is_some_and(|e| e == ext))
                .collect();
            v.sort();
            v.into_iter().next()
        };
        let pro = with_ext("kicad_pro");
        let net = with_ext("net");
        let stem = pro
            .as_ref()
            .or(net.as_ref())
            .map(|p| p.file_stem().unwrap().to_string_lossy().into_owned())
            .unwrap_or_else(|| name.clone());
        let root = dir.join(format!("{stem}.kicad_sch"));
        assert!(
            root.is_file(),
            "fixture {name} has no root schematic {}",
            root.display()
        );
        out.push(Fixture {
            name,
            dir,
            root,
            reference: net,
        });
    }
    out
}

/// Every file under the fixtures with one of these extensions.
pub fn files_with(exts: &[&str]) -> Vec<PathBuf> {
    fn walk(dir: &Path, exts: &[&str], out: &mut Vec<PathBuf>) {
        for e in std::fs::read_dir(dir).unwrap().filter_map(Result::ok) {
            let p = e.path();
            if p.is_dir() {
                walk(&p, exts, out);
            } else if p.extension().is_some_and(|x| exts.iter().any(|e| x == *e)) {
                out.push(p);
            }
        }
    }
    let mut out = Vec::new();
    walk(&fixtures_dir(), exts, &mut out);
    out.sort();
    out
}

/// Copy a directory tree (fixture projects are small and flat or one level
/// deep).
pub fn copy_dir(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for e in std::fs::read_dir(from).unwrap().filter_map(Result::ok) {
        let p = e.path();
        let dest = to.join(e.file_name());
        if p.is_dir() {
            copy_dir(&p, &dest);
        } else {
            std::fs::copy(&p, &dest).unwrap();
        }
    }
}

/// The lines removed from `a` and added in `b` by the single changed region
/// between their common prefix and common suffix.
pub fn changed_region(a: &str, b: &str) -> (Vec<String>, Vec<String>) {
    let a: Vec<&str> = a.split_inclusive('\n').collect();
    let b: Vec<&str> = b.split_inclusive('\n').collect();
    let mut pre = 0;
    while pre < a.len() && pre < b.len() && a[pre] == b[pre] {
        pre += 1;
    }
    let mut suf = 0;
    while suf < a.len() - pre && suf < b.len() - pre && a[a.len() - 1 - suf] == b[b.len() - 1 - suf]
    {
        suf += 1;
    }
    (
        a[pre..a.len() - suf]
            .iter()
            .map(|s| s.to_string())
            .collect(),
        b[pre..b.len() - suf]
            .iter()
            .map(|s| s.to_string())
            .collect(),
    )
}

/// A small deterministic generator (xorshift64*), so the randomized tests
/// are reproducible from their seed without a dependency.
pub struct Rng(pub u64);

impl Rng {
    pub fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }
    pub fn below(&mut self, n: usize) -> usize {
        if n == 0 {
            0
        } else {
            (self.next() % n as u64) as usize
        }
    }
}
