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

    #[error("unknown task `{0}`. Run `cargo xtask help` for the list.")]
    UnknownTask(String),

    #[error("`cargo xtask {task}` takes no options, but got `{option}`.")]
    UnknownOption { task: &'static str, option: String },
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
