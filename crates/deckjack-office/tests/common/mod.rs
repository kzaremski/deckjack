//! An office app installed the way `deckjack install` lays it out, with the
//! launch plan the core would build for it.

#![allow(dead_code)]

use std::path::{Path, PathBuf};

use deckjack_core::apps::{self, Confinement, LaunchPlan, OsEnv};
use deckjack_core::sandbox;

pub struct Rig {
    pub env: OsEnv,
    pub install: PathBuf,
    pub work: PathBuf,
    /// The plan's private HOME and TMPDIR.
    pub home: PathBuf,
    pub tmp: PathBuf,
    /// Where the core's end of each launch's link arrives, as it would at
    /// the core's attendant for this app.
    pub links: std::sync::Mutex<apps::LinkReceiver>,
    installed: apps::Installed,
    _dir: tempfile::TempDir,
}

pub fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

pub fn have_libreoffice() -> bool {
    let there = Path::new("/usr/lib/libreoffice/program/libsofficeapp.so").is_file()
        && Path::new("/usr/bin/pdftoppm").is_file()
        && Path::new("/usr/bin/pdfinfo").is_file();
    if !there {
        eprintln!("skipped: LibreOffice or poppler is not installed on this host");
    }
    there
}

impl Rig {
    /// `None`, having said why, on a host that cannot confine.
    pub fn new() -> Option<Rig> {
        if !sandbox::availability().is_enforcing() {
            eprintln!(
                "skipped: this host cannot confine ({:?})",
                sandbox::availability()
            );
            return None;
        }
        let dir = tempfile::tempdir().unwrap();
        let install = dir.path().join("install");
        let work = dir.path().join("work");
        std::fs::create_dir_all(&work).unwrap();
        let app_dir = install.join("apps").join("office");
        std::fs::create_dir_all(&app_dir).unwrap();
        std::fs::write(app_dir.join("app.toml"), include_str!("../../app.toml")).unwrap();
        // Copied, not linked: Landlock checks the target of a link, and the
        // build directory is granted to nothing.
        std::fs::copy(
            env!("CARGO_BIN_EXE_deckjack-office"),
            app_dir.join("deckjack-office"),
        )
        .unwrap();

        let env = OsEnv::new();
        let links = std::sync::Mutex::new(env.links().attend("office"));
        let found = apps::discover(&install, &env);
        let installed = found
            .apps
            .into_iter()
            .find(|app| app.name == "office")
            .unwrap_or_else(|| panic!("office was not discovered: {:?}", found.problems));
        let plan = LaunchPlan::build(&installed, &install, &work, Confinement::Enforced, &env)
            .expect("a plan for a real install");
        let var = |key: &str| {
            PathBuf::from(
                &plan
                    .env
                    .iter()
                    .find(|(k, _)| k == key)
                    .unwrap_or_else(|| panic!("the plan sets no {key}"))
                    .1,
            )
        };
        let home = var("HOME");
        let tmp = var("TMPDIR");
        Some(Rig {
            env,
            install,
            work,
            home,
            tmp,
            links,
            installed,
            _dir: dir,
        })
    }

    /// A fresh plan, exactly as the core builds it.
    pub fn plan(&self) -> LaunchPlan {
        LaunchPlan::build(
            &self.installed,
            &self.install,
            &self.work,
            Confinement::Enforced,
            &self.env,
        )
        .expect("a plan for a real install")
    }
}
