//! `cargo xtask`: the parts of a Deckjack build that are not Rust.
//!
//! `crate::shell` bakes `web/dist` into the core binary with `rust-embed`, so
//! the Vite bundle is an *input* to the Rust build, not an output of it. A
//! missing or stale `web/dist` therefore does not fail loudly — it ships a
//! binary with no shell, or last week's shell — and the only thing standing
//! between that and the operator is remembering `cd web && npm run build`.
//! That is what this crate replaces.
//!
//! # The constraint that shapes all of it
//!
//! DESIGN.md: "An xtask runs the web build before cargo build, so Rust builds
//! need no Node (including inside Termux)."
//!
//! Read in the obvious way those halves contradict each other. They don't:
//! the point is that a node with no JavaScript toolchain can still compile
//! the binary, *because the bundle it needs is already on disk*. So npm is
//! never a hard requirement of a build:
//!
//! - no npm, bundle present  -> normal successful build, nothing to say but a
//!   note that the bundle was taken as-is;
//! - no npm, no bundle       -> the only real error, and the message says how
//!   to get a bundle onto the machine;
//! - npm present, bundle stale or missing -> rebuild it, then build Rust.
//!
//! A machine with Node gets a reproducible web build. A machine without one
//! is not punished for being what Deckjack is designed to run on.
//!
//! # Freshness
//!
//! Staleness is decided by modification times: the newest mtime among the web
//! build's inputs against the *oldest* mtime in `web/dist`. Oldest, not
//! newest, because a build killed halfway leaves some fresh files beside old
//! ones, and the honest reading of that directory is the age of its worst
//! file.
//!
//! Directory mtimes count as inputs, which is what catches a *deleted* or
//! renamed source file: the file's own mtime is gone, but its parent
//! directory's moved. See [`Freshness`] for what this check still cannot see.

use std::env;
use std::ffi::OsString;
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::time::{Duration, SystemTime};

use thiserror::Error;

/// Paths of the web build, resolved once and passed down, so no function here
/// re-derives where the repository is from ambient state.
struct Workspace {
    root: PathBuf,
    web: PathBuf,
    dist: PathBuf,
}

impl Workspace {
    /// Inputs to the web build, relative to `web/`.
    ///
    /// An explicit list rather than "everything under `web/`" on purpose:
    /// `node_modules` is enormous and reinstalling it is not a reason to
    /// rebuild, `dist` is the output, and `generated/` is written *during* a
    /// build from `src/tokens`, so counting it as an input would make every
    /// bundle stale the moment it was built. Entries that do not exist are
    /// skipped, so dropping a `tsconfig` variant does not break the check.
    const INPUTS: &'static [&'static str] = &[
        "src",
        "public",
        "scripts",
        "index.html",
        "package.json",
        "package-lock.json",
        "vite.config.ts",
        "tsconfig.json",
        "tsconfig.app.json",
        "tsconfig.node.json",
        "env.d.ts",
    ];

    fn new(root: PathBuf) -> Workspace {
        let web = root.join("web");
        let dist = web.join("dist");
        Workspace { root, web, dist }
    }

    /// The entry document. Its presence, not the directory's, is what decides
    /// whether a bundle exists: an empty `dist/` left behind by a failed build
    /// or a `rm -rf` of its contents is not a bundle, and `shell::is_embedded`
    /// agrees — it also asks for `index.html`.
    fn dist_index(&self) -> PathBuf {
        self.dist.join(shell_index())
    }

    /// Display a path relative to the root when possible. Operators think in
    /// `web/src/App.vue`, not in absolute paths.
    fn show(&self, path: &Path) -> String {
        path.strip_prefix(&self.root)
            .unwrap_or(path)
            .display()
            .to_string()
    }
}

/// Kept as a function rather than importing `deckjack_core` for one constant:
/// the xtask must compile when the core does not, which is exactly the
/// situation it exists to diagnose.
fn shell_index() -> &'static str {
    "index.html"
}

/// External programs, resolved once at startup.
///
/// Resolution is injected rather than looked up at each call site so that a
/// caller can point the task at a different npm (`DECKJACK_NPM`) and so the
/// "is Node here at all" question is answered in exactly one place.
struct Tools {
    /// `None` means no JavaScript toolchain on this machine, which is a
    /// supported state, not a failure.
    npm: Option<PathBuf>,
    /// Cargo invokes us with `CARGO` set; honoring it keeps a `+nightly` or
    /// rustup-shimmed toolchain consistent between the outer and inner build.
    cargo: OsString,
}

impl Tools {
    fn detect() -> Tools {
        let npm = match env::var_os("DECKJACK_NPM") {
            Some(explicit) => Some(PathBuf::from(explicit)),
            None => find_on_path("npm"),
        };
        let cargo = env::var_os("CARGO").unwrap_or_else(|| OsString::from("cargo"));
        Tools { npm, cargo }
    }

    fn npm(&self) -> Result<&Path, XtaskError> {
        self.npm.as_deref().ok_or(XtaskError::NpmMissing)
    }
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

#[derive(Debug, Error)]
enum XtaskError {
    #[error(
        "cannot find the Deckjack repository: no directory at or above {from} \
         holds both Cargo.toml and web/package.json. Run this from inside a \
         checkout, or `cargo xtask ...` so cargo passes its manifest path."
    )]
    NoRoot { from: PathBuf },

    #[error(
        "npm is not on PATH, so the web bundle cannot be built here.\n  \
         With Node: install Node >= 22.6.0 (web/package.json, engines) and run \
         `cargo xtask web`.\n  Without Node: build the bundle on a machine that \
         has it and copy web/dist across — once it exists, the Rust build needs \
         no Node. Set DECKJACK_NPM to point at an npm somewhere unusual."
    )]
    NpmMissing,

    #[error(
        "no web bundle at {dist} and npm is not on PATH, so one cannot be built \
         here.\n  Copy a built web/dist from a machine with Node, or install \
         Node >= 22.6.0 and run `cargo xtask web`.\n  Without it the binary \
         would ship with no browser shell at all."
    )]
    NoBundleNoNpm { dist: String },

    #[error("no web bundle at {dist}. Run `cargo xtask web` to build one.")]
    NoBundle { dist: String },

    #[error(
        "the web bundle is stale: {input} is {age} newer than {dist}. Run \
         `cargo xtask web` to rebuild it, or `cargo xtask build`, which does \
         the web build first."
    )]
    Stale {
        input: String,
        dist: String,
        age: String,
    },

    #[error(
        "`{command}` failed in {dir}: {status}. Its output is above — fix what \
         it reports, then run the same command again."
    )]
    Tool {
        command: String,
        dir: String,
        status: String,
    },

    #[error("cannot run `{command}`: {source}. Check that it is installed and executable.")]
    Spawn {
        command: String,
        #[source]
        source: io::Error,
    },

    #[error("cannot read {path}: {source}")]
    Read {
        path: String,
        #[source]
        source: io::Error,
    },

    #[error(
        "{path} has no `name = \"...\"` under [app], so the app it describes has no \
         name to build or install it under"
    )]
    NoAppName { path: String },

    #[error("unknown task `{0}`. Run `cargo xtask help` for the list.")]
    UnknownTask(String),

    #[error("`cargo xtask {task}` does not take `{option}`. Run `cargo xtask help`.")]
    UnknownOption { task: &'static str, option: String },

    #[error("`{option}` needs a value: {what}.")]
    MissingValue {
        option: &'static str,
        what: &'static str,
    },

    #[error("`{value}` is not a port number.")]
    BadPort { value: String },

    #[error(
        "neither DECKJACK_HOME nor HOME is set, so there is no default install \
         directory. Pass --prefix (install) or --home (dev)."
    )]
    NoHome,

    #[error("cannot install {what} to {to}: {source}")]
    Install {
        what: String,
        to: String,
        #[source]
        source: io::Error,
    },

    #[error("{0} was not built: expected it at {1}. The cargo build above should have made it.")]
    NotBuilt(String, String),
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

fn main() -> ExitCode {
    let args: Vec<String> = env::args().skip(1).collect();
    match run(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            // Printed, not panicked: a stale bundle is a thing the operator
            // has to act on, and a backtrace is noise on top of the one
            // sentence that tells them what to do.
            eprintln!("\nxtask: {error}");
            let mut source = std::error::Error::source(&error);
            while let Some(inner) = source {
                eprintln!("       caused by: {inner}");
                source = inner.source();
            }
            ExitCode::FAILURE
        }
    }
}

fn run(args: &[String]) -> Result<(), XtaskError> {
    let (task, rest) = args
        .split_first()
        .map(|(t, r)| (t.as_str(), r))
        .unwrap_or(("help", &[]));

    if matches!(task, "help" | "-h" | "--help") {
        print_help();
        return Ok(());
    }

    let workspace = Workspace::new(locate_root()?);
    let tools = Tools::detect();

    match task {
        "web" => {
            let force = parse_flags(rest, "web", &["--force"])?.contains(&"--force");
            task_web(&workspace, &tools, force)
        }
        "check" => {
            parse_flags(rest, "check", &[])?;
            task_check(&workspace)
        }
        "build" => task_build(&workspace, &tools, rest),
        "install" => {
            let opts =
                InstallOptions::parse(rest, env::var_os("DECKJACK_HOME"), env::var_os("HOME"))?;
            task_install(&workspace, &tools, &opts)
        }
        "dev" => {
            let opts = DevOptions::parse(rest, env::var_os("HOME"))?;
            task_dev(&workspace, &tools, &opts)
        }
        other => Err(XtaskError::UnknownTask(other.to_string())),
    }
}

fn print_help() {
    print!(
        "\
cargo xtask — Deckjack build tasks

  web [--force]     Build the browser shell into web/dist with npm. Skipped as
                    already current unless --force or something under web/
                    changed since the last build. Needs Node.

  check             Report whether web/dist exists and is newer than its
                    sources. Exits non-zero when it is missing or stale, so CI
                    can refuse to ship a binary with a stale shell. Strict on
                    purpose: it answers the question honestly even on a machine
                    that could not rebuild.

  build [args...]   The whole build: the web bundle first (when it needs it and
                    npm is available), then `cargo build`. Unrecognized
                    arguments go to cargo, so `cargo xtask build --release`
                    works. --no-web skips the web step.

  install [--debug] [--prefix DIR]
                    Build (release by default, web bundle first) and install
                    into DIR, default $DECKJACK_HOME or ~/.deckjack: the core
                    as bin/deckjack and every app as apps/<name>/ with its
                    app.toml. Binaries are replaced by rename, so a running
                    server is not disturbed; restart it to pick up the new
                    core. An app picks up its new binary the next time it is
                    started.

  dev [--port N] [--home DIR]
                    The core and Vite together, in the foreground. Builds a
                    debug core and the apps, installs the apps into DIR
                    (default ~/.deckjack-dev, its own working directory so it
                    never contends with a real ~/.deckjack server), starts
                    `deckjack --debug` on 127.0.0.1:N (default 8777) and
                    `npm run dev` pointed at it. Open http://localhost:5273.
                    Ctrl+C stops both; if either exits, the other is stopped.

A machine without Node is not an error. If web/dist is already present it is
used as it is, with a note; only a missing bundle that cannot be built is
fatal. DECKJACK_NPM overrides where npm is found.
"
    );
}

/// Flags for the tasks that take a fixed, tiny set of them. Anything else is
/// refused rather than ignored, because a silently dropped `--force` leaves
/// the operator staring at a bundle that did not rebuild.
fn parse_flags<'a>(
    args: &'a [String],
    task: &'static str,
    allowed: &[&'static str],
) -> Result<Vec<&'a str>, XtaskError> {
    let mut seen = Vec::new();
    for arg in args {
        match allowed.iter().find(|a| **a == arg.as_str()) {
            Some(flag) => seen.push(*flag),
            None => {
                return Err(XtaskError::UnknownOption {
                    task,
                    option: arg.clone(),
                });
            }
        }
    }
    Ok(seen)
}

// ---------------------------------------------------------------------------
// Tasks
// ---------------------------------------------------------------------------

fn task_web(workspace: &Workspace, tools: &Tools, force: bool) -> Result<(), XtaskError> {
    let freshness = freshness(workspace)?;
    if !force && matches!(freshness, Freshness::Fresh { .. }) {
        step(format!(
            "web bundle at {} is current; nothing to do (--force rebuilds)",
            workspace.show(&workspace.dist)
        ));
        return Ok(());
    }

    // Asked for explicitly, so a missing npm *is* an error here: the operator
    // wants a bundle built, and this machine cannot build one.
    let npm = tools.npm()?;
    build_web(workspace, npm)
}

fn task_check(workspace: &Workspace) -> Result<(), XtaskError> {
    match freshness(workspace)? {
        Freshness::Fresh { newest_input, .. } => {
            step(format!(
                "web bundle is current: {} built after {}",
                workspace.show(&workspace.dist),
                workspace.show(&newest_input)
            ));
            Ok(())
        }
        Freshness::Missing => Err(XtaskError::NoBundle {
            dist: workspace.show(&workspace.dist),
        }),
        Freshness::Stale {
            newest_input,
            input_time,
            dist_time,
        } => Err(XtaskError::Stale {
            input: workspace.show(&newest_input),
            dist: workspace.show(&workspace.dist),
            age: format_gap(input_time, dist_time),
        }),
    }
}

fn task_build(workspace: &Workspace, tools: &Tools, args: &[String]) -> Result<(), XtaskError> {
    let mut cargo_args: Vec<&str> = Vec::new();
    let mut skip_web = false;
    for arg in args {
        if arg == "--no-web" {
            skip_web = true;
        } else {
            cargo_args.push(arg);
        }
    }

    if skip_web {
        step("skipping the web build (--no-web)");
    } else {
        web_before_cargo(workspace, tools)?;
    }

    let mut cargo = Command::new(&tools.cargo);
    cargo
        .arg("build")
        .args(&cargo_args)
        .current_dir(&workspace.root);
    exec(cargo, &workspace.root)
}

/// The half of the design sentence that is easy to get wrong.
///
/// Every outcome except "no bundle and no npm" continues to the Rust build.
/// The warnings are loud because shipping a stale shell is the failure this
/// crate exists to prevent, and silent is how it happens.
fn web_before_cargo(workspace: &Workspace, tools: &Tools) -> Result<(), XtaskError> {
    let freshness = freshness(workspace)?;

    match (&freshness, tools.npm.as_deref()) {
        (Freshness::Fresh { .. }, _) => {
            step("web bundle is current; going straight to cargo");
            Ok(())
        }
        (_, Some(npm)) => {
            if let Freshness::Stale { newest_input, .. } = &freshness {
                step(format!(
                    "web bundle is older than {}; rebuilding",
                    workspace.show(newest_input)
                ));
            } else {
                step("no web bundle yet; building it");
            }
            build_web(workspace, npm)
        }
        (Freshness::Missing, None) => Err(XtaskError::NoBundleNoNpm {
            dist: workspace.show(&workspace.dist),
        }),
        (
            Freshness::Stale {
                newest_input,
                input_time,
                dist_time,
            },
            None,
        ) => {
            // Deliberately not an error. This is the Termux case: the bundle
            // was built elsewhere and copied in, and its mtimes are whatever
            // the copy gave them. Refusing to build would make the whole
            // Node-free story a lie.
            warn(format!(
                "web bundle may be stale — {} is {} newer than {} — and npm is \
                 not on PATH, so it cannot be rebuilt here. Building Rust with \
                 the bundle as it is.",
                workspace.show(newest_input),
                format_gap(*input_time, *dist_time),
                workspace.show(&workspace.dist),
            ));
            Ok(())
        }
    }
}

/// Run the npm side of a build.
fn build_web(workspace: &Workspace, npm: &Path) -> Result<(), XtaskError> {
    if !workspace.web.join("node_modules").exists() {
        // Done here rather than left to the operator because `npm run build`
        // on a fresh checkout fails with an error about vite, not about
        // dependencies, which sends people looking in the wrong place.
        let lock = workspace.web.join("package-lock.json");
        let subcommand = if lock.exists() { "ci" } else { "install" };
        step(format!(
            "web/node_modules is missing; running npm {subcommand}"
        ));
        let mut install = Command::new(npm);
        install.arg(subcommand).current_dir(&workspace.web);
        exec(install, &workspace.web)?;
    }

    if tokens_needed(workspace)? {
        step("design tokens are missing or older than web/src/tokens; regenerating");
        let mut tokens = Command::new(npm);
        tokens.args(["run", "tokens"]).current_dir(&workspace.web);
        exec(tokens, &workspace.web)?;
    }

    step("building the web bundle (npm run build)");
    let mut build = Command::new(npm);
    build.args(["run", "build"]).current_dir(&workspace.web);
    exec(build, &workspace.web)?;

    // Trust nothing: a zero exit from a build tool that wrote no entry
    // document still leaves `rust-embed` with nothing to bake in, and that
    // failure would otherwise surface as a blank page at runtime.
    if !workspace.dist_index().exists() {
        return Err(XtaskError::NoBundle {
            dist: workspace.show(&workspace.dist),
        });
    }
    step(format!(
        "web bundle written to {}",
        workspace.show(&workspace.dist)
    ));
    Ok(())
}

/// Whether `npm run tokens` has to run before the bundle.
///
/// `web/generated` is not checked in (it is a build product of
/// `scripts/emit-tokens.ts`), so a fresh clone has no tokens at all and
/// `vite build` fails on an import it cannot resolve. The script is detected
/// by looking for its name in `package.json` instead of parsing the file,
/// because a task whose job is to explain broken builds should not need a
/// JSON parser — and the cost of guessing wrong is one clear npm error.
fn tokens_needed(workspace: &Workspace) -> Result<bool, XtaskError> {
    let manifest = workspace.web.join("package.json");
    let declared = read_to_string(&manifest, workspace)?.contains("\"tokens\"");
    if !declared {
        return Ok(false);
    }

    let generated = workspace.web.join("generated");
    if !generated.exists() {
        return Ok(true);
    }

    let sources = workspace.web.join("src").join("tokens");
    if !sources.exists() {
        return Ok(false);
    }

    let newest_source = newest_mtime(&sources, workspace)?;
    let oldest_generated = oldest_mtime(&generated, workspace)?;
    match (newest_source, oldest_generated) {
        (Some((_, source)), Some(generated)) => Ok(source > generated),
        // No generated files inside the directory counts as nothing generated.
        (Some(_), None) => Ok(true),
        _ => Ok(false),
    }
}

// ---------------------------------------------------------------------------
// Freshness
// ---------------------------------------------------------------------------

/// What the mtime comparison concluded.
///
/// # What this check cannot see
///
/// It is a timestamp comparison, so it is cheap and it is approximate:
///
/// - **A deleted source file** is caught only through its parent directory's
///   mtime, which is included for exactly that reason. On a filesystem that
///   does not update directory mtimes (some network and overlay mounts) a
///   deletion is invisible, and the bundle keeps the removed module.
/// - **A dependency change without a lockfile change.** `npm install` of a
///   floating version rewrites `node_modules`, which is not an input here —
///   including it would mean walking tens of thousands of files to decide
///   whether to rebuild.
/// - **Content.** `touch web/src/App.vue` forces a rebuild of an unchanged
///   tree; a hash of inputs would not, and would cost a full read every time.
/// - **Build settings outside the tree**: `NODE_ENV`, a vite mode, a different
///   npm version. The same sources can legitimately produce a different
///   bundle.
/// - **Integrity.** A truncated or hand-edited `dist` with recent mtimes looks
///   perfectly fresh. The oldest-file rule catches an *interrupted* build, not
///   a corrupt one.
/// - **Clock skew.** A source file with a future mtime (a bad clock, an
///   archive extracted with preserved timestamps) reads as permanently stale.
///
/// Every one of those errs toward rebuilding or toward warning, which is the
/// right direction: a wasted `npm run build` costs seconds, a stale shell
/// baked into a release costs an afternoon of confusion.
enum Freshness {
    /// No `web/dist/index.html`.
    Missing,
    /// Every input is older than the whole bundle.
    Fresh { newest_input: PathBuf },
    /// Something under `web/` is newer than the oldest file in the bundle.
    Stale {
        newest_input: PathBuf,
        input_time: SystemTime,
        dist_time: SystemTime,
    },
}

fn freshness(workspace: &Workspace) -> Result<Freshness, XtaskError> {
    if !workspace.dist_index().exists() {
        return Ok(Freshness::Missing);
    }

    let dist_time = match oldest_mtime(&workspace.dist, workspace)? {
        Some(time) => time,
        // `index.html` exists but nothing has a readable mtime: treat it as
        // unknowable rather than fresh.
        None => {
            return Ok(Freshness::Stale {
                newest_input: workspace.dist_index(),
                input_time: SystemTime::now(),
                dist_time: SystemTime::UNIX_EPOCH,
            });
        }
    };

    let mut newest: Option<(PathBuf, SystemTime)> = None;
    for input in Workspace::INPUTS {
        let path = workspace.web.join(input);
        if let Some(found) = newest_mtime(&path, workspace)?
            && newest.as_ref().is_none_or(|(_, best)| found.1 > *best)
        {
            newest = Some(found);
        }
    }

    let Some((newest_input, input_time)) = newest else {
        // No inputs on disk at all. Odd, but not a reason to rebuild.
        return Ok(Freshness::Fresh {
            newest_input: workspace.web.clone(),
        });
    };

    if input_time > dist_time {
        Ok(Freshness::Stale {
            newest_input,
            input_time,
            dist_time,
        })
    } else {
        Ok(Freshness::Fresh { newest_input })
    }
}

/// Newest mtime at or under `path`, with the file that carries it.
///
/// Directories count: their mtime is the only local record that a file was
/// deleted or renamed inside them.
fn newest_mtime(
    path: &Path,
    workspace: &Workspace,
) -> Result<Option<(PathBuf, SystemTime)>, XtaskError> {
    let mut best: Option<(PathBuf, SystemTime)> = None;
    walk(path, workspace, &mut |entry, time| {
        if best.as_ref().is_none_or(|(_, best_time)| time > *best_time) {
            best = Some((entry.to_path_buf(), time));
        }
    })?;
    Ok(best)
}

/// Oldest mtime at or under `path`, ignoring directories.
///
/// Directories are skipped here because a build that writes a new file into
/// `dist/assets` bumps that directory, and the question for an output tree is
/// how old its contents are, not when it was last touched.
fn oldest_mtime(path: &Path, workspace: &Workspace) -> Result<Option<SystemTime>, XtaskError> {
    let mut best: Option<SystemTime> = None;
    walk(path, workspace, &mut |entry, time| {
        if entry.is_dir() {
            return;
        }
        if best.is_none_or(|best_time| time < best_time) {
            best = Some(time);
        }
    })?;
    Ok(best)
}

/// Recursive walk that reports each path with its mtime.
///
/// A missing `path` is not an error: the input list is allowed to name files
/// that a given checkout does not have.
fn walk(
    path: &Path,
    workspace: &Workspace,
    visit: &mut dyn FnMut(&Path, SystemTime),
) -> Result<(), XtaskError> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(source) => {
            return Err(XtaskError::Read {
                path: workspace.show(path),
                source,
            });
        }
    };

    if let Ok(time) = metadata.modified() {
        visit(path, time);
    }

    // Symlinks are reported by their own mtime and not followed: a link into
    // node_modules or out of the tree would otherwise drag an unrelated
    // subtree into the comparison, and a cycle would hang the walk.
    if !metadata.is_dir() {
        return Ok(());
    }

    let entries = fs::read_dir(path).map_err(|source| XtaskError::Read {
        path: workspace.show(path),
        source,
    })?;
    for entry in entries {
        let entry = entry.map_err(|source| XtaskError::Read {
            path: workspace.show(path),
            source,
        })?;
        let child = entry.path();
        if is_ignored(&child) {
            continue;
        }
        walk(&child, workspace, visit)?;
    }
    Ok(())
}

/// Paths that never mean a rebuild even when they sit inside an input.
fn is_ignored(path: &Path) -> bool {
    match path.file_name().and_then(|name| name.to_str()) {
        // Reinstalling dependencies is not a source change, and walking it
        // would dominate the cost of the check.
        Some("node_modules") => true,
        // `vue-tsc --build` writes these *after* the bundle, so counting them
        // would leave every bundle looking stale the moment it finished.
        Some(name) if name.ends_with(".tsbuildinfo") => true,
        _ => false,
    }
}

// ---------------------------------------------------------------------------
// Plumbing
// ---------------------------------------------------------------------------

/// Find the repository root.
///
/// `cargo xtask` sets `CARGO_MANIFEST_DIR` to `<root>/xtask`, which is the
/// cheap answer. It is still verified rather than trusted, and a walk up from
/// the working directory is the fallback, so running the binary directly from
/// anywhere inside a checkout works too.
fn locate_root() -> Result<PathBuf, XtaskError> {
    if let Some(manifest_dir) = env::var_os("CARGO_MANIFEST_DIR") {
        let candidate = PathBuf::from(manifest_dir);
        if let Some(parent) = candidate.parent()
            && is_root(parent)
        {
            return Ok(parent.to_path_buf());
        }
    }

    let from = env::current_dir().map_err(|source| XtaskError::Read {
        path: "the current directory".to_string(),
        source,
    })?;
    let mut at = from.as_path();
    loop {
        if is_root(at) {
            return Ok(at.to_path_buf());
        }
        match at.parent() {
            Some(parent) => at = parent,
            None => return Err(XtaskError::NoRoot { from }),
        }
    }
}

/// Both markers, not just `Cargo.toml`: the xtask's own directory has one of
/// those, and so does every crate under `crates/`.
fn is_root(path: &Path) -> bool {
    path.join("Cargo.toml").is_file() && path.join("web").join("package.json").is_file()
}

fn find_on_path(program: &str) -> Option<PathBuf> {
    let path = env::var_os("PATH")?;
    env::split_paths(&path)
        .map(|dir| dir.join(program))
        .find(|candidate| is_executable(candidate))
}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    // `metadata` follows symlinks, which is what is wanted: npm is almost
    // always a link into a Node installation.
    fs::metadata(path).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn is_executable(path: &Path) -> bool {
    path.is_file()
}

/// Run a child process, inheriting its stdio.
///
/// Inherited rather than captured so npm's and cargo's own progress and error
/// output reach the operator as it happens; this task has nothing better to
/// say about a TypeScript error than TypeScript does.
fn exec(mut command: Command, dir: &Path) -> Result<(), XtaskError> {
    let shown = show_command(&command);
    let status = command.status().map_err(|source| XtaskError::Spawn {
        command: shown.clone(),
        source,
    })?;
    if !status.success() {
        return Err(XtaskError::Tool {
            command: shown,
            dir: dir.display().to_string(),
            status: describe_status(&status),
        });
    }
    Ok(())
}

fn show_command(command: &Command) -> String {
    let mut shown = command.get_program().to_string_lossy().to_string();
    for arg in command.get_args() {
        shown.push(' ');
        shown.push_str(&arg.to_string_lossy());
    }
    shown
}

fn describe_status(status: &std::process::ExitStatus) -> String {
    match status.code() {
        Some(code) => format!("exit status {code}"),
        // A signal death (a killed npm, an OOM) has no exit code, and
        // "exit status unknown" would hide the most useful fact about it.
        None => format!("{status}"),
    }
}

/// Read a file, reporting the path the operator knows it by rather than the
/// absolute one.
fn read_to_string(path: &Path, workspace: &Workspace) -> Result<String, XtaskError> {
    fs::read_to_string(path).map_err(|source| XtaskError::Read {
        path: workspace.show(path),
        source,
    })
}

fn step(message: impl fmt::Display) {
    println!("xtask: {message}");
}

fn warn(message: impl fmt::Display) {
    eprintln!("xtask: warning: {message}");
}

/// How much newer `later` is than `earlier`, in the coarsest unit that still
/// says something useful.
fn format_gap(later: SystemTime, earlier: SystemTime) -> String {
    let gap = later
        .duration_since(earlier)
        .unwrap_or(Duration::from_secs(0));
    let seconds = gap.as_secs();
    match seconds {
        0 => "less than a second".to_string(),
        1 => "1 second".to_string(),
        s if s < 90 => format!("{s} seconds"),
        s if s < 90 * 60 => format!("{} minutes", s / 60),
        s if s < 48 * 3600 => format!("{} hours", s / 3600),
        s => format!("{} days", s / 86_400),
    }
}

// ---------------------------------------------------------------------------
// install and dev
// ---------------------------------------------------------------------------

/// The default install directory: `$DECKJACK_HOME`, else `$HOME/.deckjack`.
/// The same rule as `deckjack_core::node::install_dir`, restated because the
/// xtask must not depend on the core.
fn deckjack_home(
    deckjack_home: Option<OsString>,
    home: Option<OsString>,
) -> Result<PathBuf, XtaskError> {
    if let Some(dir) = deckjack_home.filter(|d| !d.is_empty()) {
        return Ok(PathBuf::from(dir));
    }
    home.filter(|h| !h.is_empty())
        .map(|h| PathBuf::from(h).join(".deckjack"))
        .ok_or(XtaskError::NoHome)
}

#[derive(Debug, PartialEq, Eq)]
struct InstallOptions {
    release: bool,
    prefix: PathBuf,
}

impl InstallOptions {
    fn parse(
        args: &[String],
        deckjack_home_var: Option<OsString>,
        home: Option<OsString>,
    ) -> Result<InstallOptions, XtaskError> {
        let mut release = true;
        let mut prefix = None;
        let mut it = args.iter();
        while let Some(arg) = it.next() {
            match arg.as_str() {
                "--debug" => release = false,
                "--prefix" => {
                    let value = it.next().ok_or(XtaskError::MissingValue {
                        option: "--prefix",
                        what: "the directory to install into",
                    })?;
                    prefix = Some(PathBuf::from(value));
                }
                other => {
                    return Err(XtaskError::UnknownOption {
                        task: "install",
                        option: other.to_string(),
                    });
                }
            }
        }
        let prefix = match prefix {
            Some(p) => p,
            None => deckjack_home(deckjack_home_var, home)?,
        };
        Ok(InstallOptions { release, prefix })
    }
}

#[derive(Debug, PartialEq, Eq)]
struct DevOptions {
    port: u16,
    home: PathBuf,
}

/// The dev core's port. Matches `web/vite.config.ts`'s default.
const DEV_PORT: u16 = 8777;

impl DevOptions {
    fn parse(args: &[String], home: Option<OsString>) -> Result<DevOptions, XtaskError> {
        let mut port = DEV_PORT;
        let mut dir = None;
        let mut it = args.iter();
        while let Some(arg) = it.next() {
            match arg.as_str() {
                "--port" => {
                    let value = it.next().ok_or(XtaskError::MissingValue {
                        option: "--port",
                        what: "the port for the dev core",
                    })?;
                    port = value.parse().map_err(|_| XtaskError::BadPort {
                        value: value.clone(),
                    })?;
                }
                "--home" => {
                    let value = it.next().ok_or(XtaskError::MissingValue {
                        option: "--home",
                        what: "the dev install and working directory",
                    })?;
                    dir = Some(PathBuf::from(value));
                }
                other => {
                    return Err(XtaskError::UnknownOption {
                        task: "dev",
                        option: other.to_string(),
                    });
                }
            }
        }
        // Deliberately not $DECKJACK_HOME: that names the real install, and
        // the dev core must never take its lock or its port.
        let home = match dir {
            Some(d) => d,
            None => home
                .filter(|h| !h.is_empty())
                .map(|h| PathBuf::from(h).join(".deckjack-dev"))
                .ok_or(XtaskError::NoHome)?,
        };
        Ok(DevOptions { port, home })
    }
}

/// One app crate in the repository.
#[derive(Debug, Clone, PartialEq, Eq)]
struct AppCrate {
    /// The app's name, from its `app.toml`: what the binary is called
    /// (`deckjack-<name>`) and where it installs (`apps/<name>/`).
    name: String,
    /// The crate's directory, which `deckjack install` reads the manifest
    /// and the UI from.
    dir: PathBuf,
}

/// Every app in the repository: a `crates/deckjack-*/` with an `app.toml`,
/// which is exactly what makes a crate an app. Sorted by app name, so the
/// output is the same every run.
///
/// The name comes from the manifest, not the directory. The two usually
/// agree; they do not have to, and where they part it is the manifest that
/// the core holds the binary to. `crates/deckjack-files` is the Files app,
/// whose name is `filer` because `files` is the core's own namespace.
fn find_apps(root: &Path) -> Result<Vec<AppCrate>, XtaskError> {
    let crates = root.join("crates");
    let entries = fs::read_dir(&crates).map_err(|source| XtaskError::Read {
        path: crates.display().to_string(),
        source,
    })?;
    let mut apps = Vec::new();
    for entry in entries.flatten() {
        let dir_name = entry.file_name().to_string_lossy().into_owned();
        let manifest = entry.path().join("app.toml");
        if dir_name.starts_with("deckjack-") && manifest.is_file() {
            let text = fs::read_to_string(&manifest).map_err(|source| XtaskError::Read {
                path: manifest.display().to_string(),
                source,
            })?;
            let name = manifest_app_name(&text).ok_or_else(|| XtaskError::NoAppName {
                path: manifest.display().to_string(),
            })?;
            apps.push(AppCrate {
                name,
                dir: entry.path(),
            });
        }
    }
    apps.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(apps)
}

/// `name` under `[app]` in an `app.toml`, read without a TOML parser because
/// this crate has none (see Cargo.toml). Only the plain `name = "x"` form the
/// in-tree manifests use; the core's `deckjack install` parses the manifest
/// properly and refuses anything this reads wrong.
fn manifest_app_name(text: &str) -> Option<String> {
    let mut in_app = false;
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_app = line == "[app]";
            continue;
        }
        if !in_app {
            continue;
        }
        let Some(rest) = line.strip_prefix("name") else {
            continue;
        };
        let Some(value) = rest.trim_start().strip_prefix('=') else {
            continue;
        };
        let value = value.trim();
        let value = value.split('#').next().unwrap_or(value).trim();
        return value
            .strip_prefix('"')
            .and_then(|v| v.strip_suffix('"'))
            .filter(|v| !v.is_empty())
            .map(str::to_string);
    }
    None
}

/// Where cargo puts its output: `$CARGO_TARGET_DIR`, else `target/`.
fn target_dir(root: &Path, from_env: Option<OsString>) -> PathBuf {
    match from_env.filter(|d| !d.is_empty()) {
        Some(dir) => {
            let dir = PathBuf::from(dir);
            if dir.is_absolute() {
                dir
            } else {
                root.join(dir)
            }
        }
        None => root.join("target"),
    }
}

/// Copy `from` to `to` by writing beside it and renaming over it.
///
/// A rename, not a write in place, because the destination may be a binary
/// that is running: Linux refuses to open a running executable for writing
/// ("text file busy"), and a rename leaves the running process its old file.
fn install_file(from: &Path, to: &Path, mode: u32) -> Result<(), XtaskError> {
    let fail = |source: io::Error| XtaskError::Install {
        what: from.display().to_string(),
        to: to.display().to_string(),
        source,
    };
    let dir = to.parent().unwrap_or(Path::new("."));
    fs::create_dir_all(dir).map_err(fail)?;
    let name = to
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let tmp = dir.join(format!(".{name}.xtask-new"));
    let _ = fs::remove_file(&tmp);
    fs::copy(from, &tmp).map_err(fail)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(&tmp, fs::Permissions::from_mode(mode)).map_err(fail)?;
    }
    #[cfg(not(unix))]
    let _ = mode;
    fs::rename(&tmp, to).map_err(|e| {
        let _ = fs::remove_file(&tmp);
        fail(e)
    })
}

/// Install the built apps (and, with `core`, the core) into `prefix`.
fn install_into(
    workspace: &Workspace,
    built: &Path,
    prefix: &Path,
    core: bool,
) -> Result<Vec<String>, XtaskError> {
    let mut installed = Vec::new();
    if core {
        let bin = built.join("deckjack");
        if !bin.is_file() {
            return Err(XtaskError::NotBuilt(
                "deckjack".into(),
                bin.display().to_string(),
            ));
        }
        install_file(&bin, &prefix.join("bin").join("deckjack"), 0o755)?;
        installed.push("bin/deckjack".to_string());
    }
    // Apps go through the core's own `deckjack install`, from the build just
    // made, rather than a second copy of its rules here: it validates the
    // manifest, checks the binary's architecture, swaps the directory
    // atomically and says whether the app needs approving again. Linking the
    // core into this crate to call it directly would cost the dependency tree
    // this crate exists to avoid.
    let core_bin = built.join("deckjack");
    for AppCrate { name: app, dir } in find_apps(&workspace.root)? {
        let binary = format!("deckjack-{app}");
        let from = built.join(&binary);
        if !from.is_file() {
            return Err(XtaskError::NotBuilt(binary, from.display().to_string()));
        }
        if !core_bin.is_file() {
            return Err(XtaskError::NotBuilt(
                "deckjack".into(),
                core_bin.display().to_string(),
            ));
        }
        let mut install = Command::new(&core_bin);
        install
            .arg("install")
            .arg(&dir)
            .arg("--binary")
            .arg(&from)
            .env("DECKJACK_HOME", prefix);
        exec(install, &workspace.root)?;
        installed.push(format!("apps/{app}/"));
        // An app installed under a name it has since given up: remove the old
        // directory through the core's own `deckjack uninstall`, or it stays
        // behind as a manifest that never validates again.
        for (old, _) in RENAMED_APPS.iter().filter(|(_, new)| *new == app) {
            if !prefix.join("apps").join(old).join("app.toml").is_file() {
                continue;
            }
            let mut uninstall = Command::new(&core_bin);
            uninstall
                .arg("uninstall")
                .arg(old)
                .env("DECKJACK_HOME", prefix);
            exec(uninstall, &workspace.root)?;
            installed.push(format!("removed apps/{old}/ (now {app})"));
        }
    }
    Ok(installed)
}

/// Apps that were renamed, old name first. `files` and `editor` became
/// namespaces only the core may use (docs/APP-UI-ISOLATION.md §4.6), so the
/// Files and Editor apps took new names; their old install directories are
/// removed when the new ones are installed.
const RENAMED_APPS: &[(&str, &str)] = &[("files", "filer"), ("editor", "scribe")];

fn cargo_build(workspace: &Workspace, tools: &Tools, release: bool) -> Result<(), XtaskError> {
    let mut cargo = Command::new(&tools.cargo);
    cargo.arg("build").current_dir(&workspace.root);
    if release {
        cargo.arg("--release");
    }
    exec(cargo, &workspace.root)
}

fn task_install(
    workspace: &Workspace,
    tools: &Tools,
    opts: &InstallOptions,
) -> Result<(), XtaskError> {
    web_before_cargo(workspace, tools)?;
    cargo_build(workspace, tools, opts.release)?;
    let profile = if opts.release { "release" } else { "debug" };
    let built = target_dir(&workspace.root, env::var_os("CARGO_TARGET_DIR")).join(profile);
    let installed = install_into(workspace, &built, &opts.prefix, true)?;
    step(format!(
        "installed a {profile} build into {}: {}",
        opts.prefix.display(),
        installed.join(", ")
    ));
    // The binary finds its home from DECKJACK_HOME or ~/.deckjack, not from
    // where it was installed, so a custom prefix has to be named when it runs.
    let bin = opts.prefix.join("bin").join("deckjack");
    let default = deckjack_home(env::var_os("DECKJACK_HOME"), env::var_os("HOME")).ok();
    let start = if default.as_deref() == Some(opts.prefix.as_path()) {
        format!("`{}`", bin.display())
    } else {
        format!(
            "`DECKJACK_HOME={} {}`",
            opts.prefix.display(),
            bin.display()
        )
    };
    step(format!(
        "start it with {start}; a server already running keeps its old binary \
         until it is restarted"
    ));
    Ok(())
}

fn task_dev(workspace: &Workspace, tools: &Tools, opts: &DevOptions) -> Result<(), XtaskError> {
    let npm = tools.npm()?.to_path_buf();
    if !workspace.web.join("node_modules").exists() {
        step("web/node_modules is missing; running npm ci");
        let mut install = Command::new(&npm);
        install.arg("ci").current_dir(&workspace.web);
        exec(install, &workspace.web)?;
    }
    // The core embeds web/dist, so it has to exist to compile, even though
    // Vite serves the shell in development.
    web_before_cargo(workspace, tools)?;
    cargo_build(workspace, tools, false)?;
    let built = target_dir(&workspace.root, env::var_os("CARGO_TARGET_DIR")).join("debug");
    let installed = install_into(workspace, &built, &opts.home, false)?;
    step(format!(
        "dev apps installed into {}: {}",
        opts.home.display(),
        installed.join(", ")
    ));

    let listen = format!("127.0.0.1:{}", opts.port);
    let mut core_cmd = Command::new(built.join("deckjack"));
    core_cmd
        .args(["--debug", "--listen", &listen, "--dir"])
        .arg(&opts.home)
        .env("DECKJACK_HOME", &opts.home)
        .current_dir(&opts.home);
    let mut core = core_cmd.spawn().map_err(|source| XtaskError::Spawn {
        command: show_command(&core_cmd),
        source,
    })?;

    let mut vite_cmd = Command::new(&npm);
    vite_cmd
        .args(["run", "dev"])
        .env("DECKJACK_CORE", format!("http://{listen}"))
        .current_dir(&workspace.web);
    let mut vite = match vite_cmd.spawn() {
        Ok(child) => child,
        Err(source) => {
            stop(&mut core);
            return Err(XtaskError::Spawn {
                command: show_command(&vite_cmd),
                source,
            });
        }
    };

    step(format!(
        "dev running: shell at http://localhost:5273 (sign in with the code the \
         core printed), core at http://{listen}, debug session cookie in {}. \
         Ctrl+C stops both.",
        opts.home.join("debug-session.cookie").display()
    ));

    // Whichever exits first takes the other down with it: half a dev setup
    // is a shell that cannot reach its core, or a core nothing is viewing.
    loop {
        if let Ok(Some(status)) = core.try_wait() {
            step(format!(
                "the core exited ({}); stopping Vite",
                describe_status(&status)
            ));
            stop(&mut vite);
            return Ok(());
        }
        if let Ok(Some(status)) = vite.try_wait() {
            step(format!(
                "Vite exited ({}); stopping the core",
                describe_status(&status)
            ));
            stop(&mut core);
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

/// SIGTERM, then a few seconds' grace, then SIGKILL. TERM first because the
/// core releases its lock and checkpoints its database on a clean shutdown;
/// a kill leaves the next start to recover from an unclean one.
fn stop(child: &mut std::process::Child) {
    let _ = Command::new("kill")
        .args(["-TERM", &child.id().to_string()])
        .status();
    for _ in 0..50 {
        if let Ok(Some(_)) = child.try_wait() {
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let _ = child.kill();
    let _ = child.wait();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn the_install_directory_is_deckjack_home_then_dot_deckjack() {
        assert_eq!(
            deckjack_home(Some("/opt/dj".into()), Some("/home/u".into())).unwrap(),
            PathBuf::from("/opt/dj")
        );
        assert_eq!(
            deckjack_home(None, Some("/home/u".into())).unwrap(),
            PathBuf::from("/home/u/.deckjack")
        );
        // Set but empty is unset, not "the current directory".
        assert_eq!(
            deckjack_home(Some("".into()), Some("/home/u".into())).unwrap(),
            PathBuf::from("/home/u/.deckjack")
        );
        assert!(matches!(deckjack_home(None, None), Err(XtaskError::NoHome)));
    }

    #[test]
    fn install_is_a_release_into_the_default_home_unless_told_otherwise() {
        let home = || Some(OsString::from("/home/u"));
        assert_eq!(
            InstallOptions::parse(&[], None, home()).unwrap(),
            InstallOptions {
                release: true,
                prefix: "/home/u/.deckjack".into()
            }
        );
        assert_eq!(
            InstallOptions::parse(&s(&["--debug", "--prefix", "/tmp/x"]), None, home()).unwrap(),
            InstallOptions {
                release: false,
                prefix: "/tmp/x".into()
            }
        );
        assert!(matches!(
            InstallOptions::parse(&s(&["--prefix"]), None, home()),
            Err(XtaskError::MissingValue { .. })
        ));
        assert!(matches!(
            InstallOptions::parse(&s(&["--relase"]), None, home()),
            Err(XtaskError::UnknownOption { .. })
        ));
    }

    #[test]
    fn dev_never_defaults_to_the_real_install() {
        // DECKJACK_HOME is the real install; dev takes no notice of it.
        let opts = DevOptions::parse(&[], Some("/home/u".into())).unwrap();
        assert_eq!(
            opts,
            DevOptions {
                port: 8777,
                home: "/home/u/.deckjack-dev".into()
            }
        );
        assert_ne!(opts.home, PathBuf::from("/home/u/.deckjack"));
        let opts = DevOptions::parse(&s(&["--port", "9000", "--home", "/tmp/d"]), None).unwrap();
        assert_eq!(
            opts,
            DevOptions {
                port: 9000,
                home: "/tmp/d".into()
            }
        );
        assert!(matches!(
            DevOptions::parse(&s(&["--port", "x"]), Some("/h".into())),
            Err(XtaskError::BadPort { .. })
        ));
    }

    #[test]
    fn the_dev_port_matches_the_vite_proxy_default() {
        let vite =
            fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("../web/vite.config.ts"))
                .unwrap();
        assert!(
            vite.contains(&format!("http://127.0.0.1:{DEV_PORT}")),
            "web/vite.config.ts no longer proxies to the dev core's port {DEV_PORT}"
        );
    }

    fn scratch(name: &str) -> PathBuf {
        let dir = env::temp_dir().join(format!("xtask-test-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn apps_are_the_crates_with_a_manifest_named_by_the_manifest() {
        let root = scratch("apps");
        for (dir, manifest) in [
            ("deckjack-notes", Some("notes")),
            // The directory and the app's name part: the manifest decides.
            ("deckjack-files", Some("filer")),
            ("deckjack-core", None),
            ("deckjack-protocol", None),
            ("other", Some("other")),
        ] {
            fs::create_dir_all(root.join("crates").join(dir)).unwrap();
            if let Some(name) = manifest {
                fs::write(
                    root.join("crates").join(dir).join("app.toml"),
                    format!("# an app\n[app]\nname = \"{name}\" # its name\nversion = \"1\"\n"),
                )
                .unwrap();
            }
        }
        let found = find_apps(&root).unwrap();
        let names: Vec<&str> = found.iter().map(|a| a.name.as_str()).collect();
        assert_eq!(names, vec!["filer", "notes"]);
        assert_eq!(found[0].dir, root.join("crates").join("deckjack-files"));
        // And the real repository's set, so a new app is noticed here.
        let real = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
        let names: Vec<String> = find_apps(&real)
            .unwrap()
            .into_iter()
            .map(|a| a.name)
            .collect();
        assert_eq!(
            names,
            vec![
                "convert", "filer", "notes", "office", "photo", "scribe", "tasks", "video"
            ]
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn an_app_name_is_read_from_the_app_table_only() {
        assert_eq!(
            manifest_app_name("[package]\nname = \"x\"\n[app]\nname = \"y\"\n"),
            Some("y".to_string())
        );
        assert_eq!(manifest_app_name("[app]\nversion = \"1\"\n"), None);
        assert_eq!(manifest_app_name("[app]\nnamespace = \"z\"\n"), None);
    }

    #[test]
    fn the_target_directory_follows_cargo_target_dir() {
        let root = Path::new("/repo");
        assert_eq!(target_dir(root, None), PathBuf::from("/repo/target"));
        assert_eq!(target_dir(root, Some("/t".into())), PathBuf::from("/t"));
        assert_eq!(
            target_dir(root, Some("out".into())),
            PathBuf::from("/repo/out")
        );
    }

    /// An executable shell script at `to`. Written by `install(1)` in its
    /// own process, so no descriptor open for writing can leak into a fork
    /// made by another test thread and make the exec fail "text file busy".
    fn script(to: &Path, body: &str) {
        let src = to.with_extension("src");
        fs::write(&src, body).unwrap();
        let status = Command::new("install")
            .args(["-m", "755"])
            .arg(&src)
            .arg(to)
            .status()
            .unwrap();
        assert!(status.success());
    }

    #[test]
    fn apps_are_installed_by_the_freshly_built_core_into_the_prefix() {
        // `make install` and `xtask dev` hand each app to the core's own
        // `deckjack install`. A stand-in core records how it was called, which
        // is the whole contract: the crate directory holding app.toml, the
        // built binary via --binary, and the prefix as DECKJACK_HOME so the
        // apps land beside the core being installed rather than in ~/.deckjack.
        let dir = scratch("via-core");
        let root = dir.join("ws");
        fs::create_dir_all(root.join("crates/deckjack-clock")).unwrap();
        fs::write(
            root.join("crates/deckjack-clock/app.toml"),
            "[app]\nname = \"clock\"\n",
        )
        .unwrap();
        // A crate without a manifest is not an app.
        fs::create_dir_all(root.join("crates/deckjack-core")).unwrap();
        let built = dir.join("built");
        fs::create_dir_all(&built).unwrap();
        fs::write(built.join("deckjack-clock"), "").unwrap();
        let log = dir.join("calls.log");
        script(
            &built.join("deckjack"),
            &format!(
                "#!/bin/sh\nprintf '%s\\n' \"$@\" \"home=$DECKJACK_HOME\" >> '{}'\n",
                log.display()
            ),
        );
        let prefix = dir.join("prefix");

        let installed =
            install_into(&Workspace::new(root.clone()), &built, &prefix, false).unwrap();
        assert_eq!(installed, s(&["apps/clock/"]));
        let calls = fs::read_to_string(&log).unwrap();
        let expected = [
            "install".to_string(),
            root.join("crates/deckjack-clock").display().to_string(),
            "--binary".to_string(),
            built.join("deckjack-clock").display().to_string(),
            format!("home={}", prefix.display()),
        ];
        assert_eq!(calls.lines().collect::<Vec<_>>(), expected);

        // A core that refuses the app fails the whole install, loudly.
        script(&built.join("deckjack"), "#!/bin/sh\nexit 3\n");
        assert!(matches!(
            install_into(&Workspace::new(root), &built, &prefix, false),
            Err(XtaskError::Tool { .. })
        ));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn installing_removes_an_app_left_under_its_old_name() {
        // Files became `filer` and Editor became `scribe` when their old
        // names were reserved for the core's namespaces. A node that had
        // the old ones installed would otherwise keep `apps/files/` and
        // `apps/editor/` as manifests that never validate again.
        let dir = scratch("renamed");
        let root = dir.join("ws");
        fs::create_dir_all(root.join("crates/deckjack-files")).unwrap();
        fs::write(
            root.join("crates/deckjack-files/app.toml"),
            "[app]\nname = \"filer\"\n",
        )
        .unwrap();
        let built = dir.join("built");
        fs::create_dir_all(&built).unwrap();
        fs::write(built.join("deckjack-filer"), "").unwrap();
        let log = dir.join("calls.log");
        script(
            &built.join("deckjack"),
            &format!(
                "#!/bin/sh\nprintf '%s ' \"$@\" >> '{}'\necho >> '{}'\n",
                log.display(),
                log.display()
            ),
        );
        let prefix = dir.join("prefix");
        fs::create_dir_all(prefix.join("apps/files")).unwrap();
        fs::write(
            prefix.join("apps/files/app.toml"),
            "[app]\nname = \"files\"\n",
        )
        .unwrap();

        let installed =
            install_into(&Workspace::new(root.clone()), &built, &prefix, false).unwrap();
        let calls = fs::read_to_string(&log).unwrap();
        let calls: Vec<&str> = calls.lines().map(str::trim_end).collect();
        assert_eq!(calls.len(), 2, "{calls:?}");
        assert!(calls[0].starts_with("install "), "{calls:?}");
        assert_eq!(calls[1], "uninstall files", "{calls:?}");
        assert!(
            installed.iter().any(|i| i.contains("apps/files/")),
            "{installed:?}"
        );

        // The control: with nothing under the old name, nothing is removed,
        // and `editor` (never installed here) is not asked about.
        fs::remove_file(&log).unwrap();
        fs::remove_dir_all(prefix.join("apps/files")).unwrap();
        install_into(&Workspace::new(root), &built, &prefix, false).unwrap();
        let calls = fs::read_to_string(&log).unwrap();
        assert_eq!(calls.lines().count(), 1, "{calls}");
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn installing_replaces_a_binary_that_is_running() {
        // A running executable cannot be opened for writing on Linux, so a
        // copy in place would fail with "text file busy". Run one and
        // replace it while it runs.
        use std::os::unix::fs::PermissionsExt as _;
        let dir = scratch("busy");
        let target = dir.join("prog");
        // Copied by `install(1)` in its own process, as `script` does: a
        // descriptor this thread held open for writing could be inherited by
        // another test's fork and make the spawn below fail "text file busy".
        let status = Command::new("install")
            .args(["-m", "755", "/bin/sleep"])
            .arg(&target)
            .status()
            .unwrap();
        assert!(status.success());
        let mut running = Command::new(&target).arg("5").spawn().unwrap();

        let new = dir.join("new");
        fs::write(&new, b"#!/bin/sh\necho new\n").unwrap();
        install_file(&new, &target, 0o755).unwrap();

        assert_eq!(fs::read(&target).unwrap(), b"#!/bin/sh\necho new\n");
        let mode = fs::metadata(&target).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o755);
        assert!(
            running.try_wait().unwrap().is_none(),
            "the running program was disturbed"
        );
        let _ = running.kill();
        let _ = running.wait();
        // No temporary file is left beside it.
        let names: Vec<String> = fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert!(!names.iter().any(|n| n.contains("xtask-new")), "{names:?}");
        let _ = fs::remove_dir_all(dir);
    }
}
