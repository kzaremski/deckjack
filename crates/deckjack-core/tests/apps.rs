//! The app host, from the outside.
//!
//! Everything here goes through `deckjack_core::apps`'s public surface. The
//! world is injected through a fake [`Env`], so every rule that depends on
//! time, the filesystem or a process — protocol refusals, path denials,
//! crash loops, backoff, idle stops — is asserted without spawning anything
//! and without a single sleep.
//!
//! The claims these tests exist to defend, in order of what would hurt most:
//!
//! 1. An app cannot widen what the operator approved, at any point: not in
//!    its manifest, not at handshake, not by turning its own sandbox off.
//! 2. A node that cannot confine an app does not run it.
//! 3. One broken app is a greyed-out icon, never a desktop that will not
//!    start and never a desktop that goes down with it.
//! 4. The wire format is pinned to the bytes the other half must write.

use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use deckjack_core::apps::{
    self, AppFrame, Confinement, ConfinementError, CoreFrame, DiscoveryError, Env, Grant,
    GrantError, Grants, Host, HostError, HostEvent, Installed, LaunchPlan, Lifecycle, Manifest,
    ManifestError, Phase, RegisteredCommand, Registration, Risk, WindowStatus, WireAsset,
};
use deckjack_core::sandbox;

// ---------------------------------------------------------------------------
// The injected world
// ---------------------------------------------------------------------------

const INSTALL: &str = "/install";
const WORK: &str = "/work";

/// Where the core listens, for the hosts most tests build: a node bound to
/// its tailnet address, where a legacy whole-network grant does not reach the
/// core on loopback. The D2 tests below build their own hosts on loopback.
const CORE_LISTEN: std::net::SocketAddr = std::net::SocketAddr::new(
    std::net::IpAddr::V4(std::net::Ipv4Addr::new(100, 64, 0, 1)),
    7878,
);

/// A filesystem, a clock, a process table and a kernel, all in memory.
///
/// Cloneable because [`Host`] takes its `Env` by value and gives it back
/// through no accessor: the test keeps the other handle and inspects what the
/// host did to the world.
#[derive(Clone)]
struct FakeEnv {
    inner: Arc<World>,
}

#[derive(Default)]
struct World {
    now: Mutex<u64>,
    dirs: Mutex<BTreeSet<PathBuf>>,
    files: Mutex<BTreeMap<PathBuf, String>>,
    exec: Mutex<BTreeSet<PathBuf>>,
    /// Where a path really leads, for the symlink rules.
    real: Mutex<BTreeMap<PathBuf, PathBuf>>,
    availability: Mutex<Option<sandbox::Availability>>,
    spawned: Mutex<Vec<LaunchPlan>>,
    spawn_fails: Mutex<bool>,
    running: Mutex<BTreeSet<u32>>,
    pids: Mutex<BTreeMap<String, u32>>,
    next_pid: Mutex<u32>,
    signaled: Mutex<Vec<u32>>,
    created: Mutex<Vec<PathBuf>>,
    /// Pairs of spellings that are one file, as on a case-folding mount.
    same: Mutex<Vec<(PathBuf, PathBuf)>>,
}

impl FakeEnv {
    fn new() -> FakeEnv {
        let env = FakeEnv {
            inner: Arc::new(World::default()),
        };
        *env.inner.now.lock().unwrap() = 1_000;
        *env.inner.next_pid.lock().unwrap() = 500;
        *env.inner.availability.lock().unwrap() = Some(sandbox::Availability::Enforcing {
            landlock_abi: 5,
            network_rules: true,
        });
        env.mkdir(INSTALL);
        env.mkdir(WORK);
        env
    }

    fn mkdir(&self, path: &str) -> PathBuf {
        let path = PathBuf::from(path);
        let mut dirs = self.inner.dirs.lock().unwrap();
        let mut cursor = Some(path.as_path());
        while let Some(current) = cursor {
            dirs.insert(current.to_path_buf());
            cursor = current.parent();
        }
        path
    }

    fn write(&self, path: &str, contents: &str) {
        let path = PathBuf::from(path);
        if let Some(parent) = path.parent() {
            self.mkdir(&parent.to_string_lossy());
        }
        self.inner
            .files
            .lock()
            .unwrap()
            .insert(path, contents.into());
    }

    fn executable(&self, path: &str) {
        let path = PathBuf::from(path);
        if let Some(parent) = path.parent() {
            self.mkdir(&parent.to_string_lossy());
        }
        self.inner.exec.lock().unwrap().insert(path);
    }

    /// `at` exists and is a directory, but leads to `target`.
    fn symlink_dir(&self, at: &str, target: &str) {
        self.mkdir(at);
        self.mkdir(target);
        self.inner
            .real
            .lock()
            .unwrap()
            .insert(PathBuf::from(at), PathBuf::from(target));
    }

    /// `a` and `b` are one file under two names, the way `BIN` is `bin` on a
    /// filesystem that folds case (and `realpath` answers as written).
    fn same_file_as(&self, a: &str, b: &str) {
        self.mkdir(a);
        self.mkdir(b);
        self.inner
            .same
            .lock()
            .unwrap()
            .push((PathBuf::from(a), PathBuf::from(b)));
    }

    fn advance(&self, secs: u64) {
        *self.inner.now.lock().unwrap() += secs;
    }

    fn set_availability(&self, availability: sandbox::Availability) {
        *self.inner.availability.lock().unwrap() = Some(availability);
    }

    fn fail_spawns(&self, fails: bool) {
        *self.inner.spawn_fails.lock().unwrap() = fails;
    }

    fn spawn_count(&self) -> usize {
        self.inner.spawned.lock().unwrap().len()
    }

    fn pid_of(&self, app: &str) -> u32 {
        *self
            .inner
            .pids
            .lock()
            .unwrap()
            .get(app)
            .unwrap_or_else(|| panic!("{app} was never spawned"))
    }

    /// The child exited on its own: a crash.
    fn crash(&self, app: &str) {
        let pid = self.pid_of(app);
        self.inner.running.lock().unwrap().remove(&pid);
    }

    fn signaled(&self) -> Vec<u32> {
        self.inner.signaled.lock().unwrap().clone()
    }
}

impl Env for FakeEnv {
    fn now(&self) -> u64 {
        *self.inner.now.lock().unwrap()
    }

    fn read_dir(&self, path: &Path) -> io::Result<Vec<PathBuf>> {
        if !self.inner.dirs.lock().unwrap().contains(path) {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("{} is not a directory", path.display()),
            ));
        }
        let mut entries: BTreeSet<PathBuf> = BTreeSet::new();
        for candidate in self.inner.dirs.lock().unwrap().iter() {
            if candidate.parent() == Some(path) {
                entries.insert(candidate.clone());
            }
        }
        for candidate in self.inner.files.lock().unwrap().keys() {
            if candidate.parent() == Some(path) {
                entries.insert(candidate.clone());
            }
        }
        for candidate in self.inner.exec.lock().unwrap().iter() {
            if candidate.parent() == Some(path) {
                entries.insert(candidate.clone());
            }
        }
        Ok(entries.into_iter().collect())
    }

    fn read_to_string(&self, path: &Path) -> io::Result<String> {
        self.inner
            .files
            .lock()
            .unwrap()
            .get(path)
            .cloned()
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no such file"))
    }

    fn is_executable(&self, path: &Path) -> bool {
        self.inner.exec.lock().unwrap().contains(path)
    }

    fn is_dir(&self, path: &Path) -> bool {
        self.inner.dirs.lock().unwrap().contains(path)
    }

    fn canonicalize(&self, path: &Path) -> io::Result<PathBuf> {
        if let Some(real) = self.inner.real.lock().unwrap().get(path) {
            return Ok(real.clone());
        }
        if self.exists(path) {
            return Ok(path.to_path_buf());
        }
        Err(io::Error::new(io::ErrorKind::NotFound, "no such path"))
    }

    fn same_file(&self, a: &Path, b: &Path) -> io::Result<bool> {
        let (a, b) = (self.canonicalize(a)?, self.canonicalize(b)?);
        Ok(a == b
            || self
                .inner
                .same
                .lock()
                .unwrap()
                .iter()
                .any(|(x, y)| (x == &a && y == &b) || (x == &b && y == &a)))
    }

    fn create_private_dir(&self, path: &Path) -> io::Result<()> {
        self.inner.created.lock().unwrap().push(path.to_path_buf());
        self.mkdir(&path.to_string_lossy());
        Ok(())
    }

    fn sandbox_availability(&self) -> sandbox::Availability {
        self.inner
            .availability
            .lock()
            .unwrap()
            .clone()
            .expect("availability was set in FakeEnv::new")
    }

    fn exists(&self, path: &Path) -> bool {
        self.inner.dirs.lock().unwrap().contains(path)
            || self.inner.files.lock().unwrap().contains_key(path)
            || self.inner.exec.lock().unwrap().contains(path)
    }

    fn spawn(&self, plan: &LaunchPlan) -> io::Result<u32> {
        if *self.inner.spawn_fails.lock().unwrap() {
            return Err(io::Error::other("exec format error"));
        }
        let pid = {
            let mut next = self.inner.next_pid.lock().unwrap();
            *next += 1;
            *next
        };
        self.inner.running.lock().unwrap().insert(pid);
        self.inner
            .pids
            .lock()
            .unwrap()
            .insert(plan.app.clone(), pid);
        self.inner.spawned.lock().unwrap().push(plan.clone());
        Ok(pid)
    }

    fn is_running(&self, pid: u32) -> bool {
        self.inner.running.lock().unwrap().contains(&pid)
    }

    fn terminate(&self, pid: u32) -> io::Result<()> {
        self.inner.signaled.lock().unwrap().push(pid);
        self.inner.running.lock().unwrap().remove(&pid);
        Ok(())
    }
}

/// Approvals in memory. Cloned into the host, kept here to inspect.
#[derive(Clone, Default)]
struct FakeGrants {
    stored: Arc<Mutex<Option<Grant>>>,
}

impl Grants for FakeGrants {
    fn grant(&self, _app: &str) -> Result<Option<Grant>, GrantError> {
        Ok(self.stored.lock().unwrap().clone())
    }

    fn record(&self, grant: &Grant) -> Result<(), GrantError> {
        *self.stored.lock().unwrap() = Some(grant.clone());
        Ok(())
    }

    fn revoke(&self, _app: &str) -> Result<(), GrantError> {
        *self.stored.lock().unwrap() = None;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

fn manifest_text(name: &str, extra: &str) -> String {
    format!(
        "[app]\nname = \"{name}\"\nversion = \"1.0.0\"\nprotocol = {}\n{extra}",
        apps::PROTOCOL_VERSION
    )
}

fn command_block(id: &str, risk: &str) -> String {
    format!("\n[[commands]]\nid = \"{id}\"\nsummary = \"Do a thing\"\nrisk = \"{risk}\"\n")
}

/// Put an app on disk the way `deckjack install` would: a directory named
/// after the app, its manifest, and an executable `deckjack-<name>`.
fn install(env: &FakeEnv, name: &str, manifest: &str) {
    env.write(&format!("{INSTALL}/apps/{name}/app.toml"), manifest);
    env.executable(&format!("{INSTALL}/apps/{name}/deckjack-{name}"));
}

/// Load one manifest through the public API, by writing it where an app's
/// would be. The path is part of every error message, so it has to be real.
fn load_manifest(toml: &str) -> Result<Manifest, ManifestError> {
    let env = FakeEnv::new();
    let path = format!("{INSTALL}/apps/probe/app.toml");
    env.write(&path, toml);
    apps::load_manifest(Path::new(&path), &env)
}

fn discovered(env: &FakeEnv, name: &str) -> Installed {
    let found = apps::discover(Path::new(INSTALL), env);
    let problems: Vec<String> = found
        .problems
        .iter()
        .map(|problem| problem.error.to_string())
        .collect();
    found
        .apps
        .into_iter()
        .find(|app| app.name == name)
        .unwrap_or_else(|| panic!("{name} was not discovered; problems: {problems:?}"))
}

fn plan_for(env: &FakeEnv, name: &str) -> Result<LaunchPlan, ConfinementError> {
    let installed = discovered(env, name);
    LaunchPlan::build(
        &installed,
        Path::new(INSTALL),
        Path::new(WORK),
        Confinement::Enforced,
        env,
    )
}

fn host_with(env: &FakeEnv, policy: apps::Policy, grants: FakeGrants) -> Host<FakeEnv> {
    Host::new(
        PathBuf::from(INSTALL),
        PathBuf::from(WORK),
        apps::CoreDoor::new(CORE_LISTEN, vec![]),
        env.clone(),
        policy,
        Box::new(grants),
    )
}

fn host(env: &FakeEnv) -> Host<FakeEnv> {
    host_with(env, apps::Policy::default(), FakeGrants::default())
}

fn registered(id: &str, risk: Risk) -> RegisteredCommand {
    RegisteredCommand {
        id: id.to_string(),
        summary: "Do a thing".to_string(),
        risk,
        params: serde_json::json!({ "type": "object" }),
    }
}

fn reconnecting_for(events: &[HostEvent], app: &str) -> bool {
    events
        .iter()
        .any(|event| matches!(event, HostEvent::Reconnecting { app: a, .. } if a == app))
}

fn mentions(haystack: &str, needles: &[&str]) -> bool {
    needles.iter().all(|needle| haystack.contains(needle))
}

// ---------------------------------------------------------------------------
// Protocol compatibility: decided early, and before anything runs
// ---------------------------------------------------------------------------

#[test]
fn an_app_built_against_another_protocol_is_refused_before_anything_is_launched() {
    let env = FakeEnv::new();
    // Everything else about this app is perfect: right name, right
    // directory, executable binary. Only the protocol is wrong.
    install(
        &env,
        "filmcraft",
        &format!(
            "[app]\nname = \"filmcraft\"\nversion = \"2.0.0\"\nprotocol = {}\n",
            apps::PROTOCOL_VERSION + 1
        ),
    );

    let mut host = host(&env);
    let events = host.refresh();

    let reason = match events.as_slice() {
        [HostEvent::Unavailable { app, reason }] => {
            assert_eq!(app, "filmcraft");
            reason.clone()
        }
        other => panic!("expected one unavailable event, got {other:?}"),
    };
    // A clear message is the requirement, so the message is the assertion:
    // both version numbers and the one command that fixes it.
    assert!(
        mentions(
            &reason,
            &[
                "filmcraft",
                &(apps::PROTOCOL_VERSION + 1).to_string(),
                &format!(
                    "{}..={}",
                    apps::MIN_PROTOCOL_VERSION,
                    apps::PROTOCOL_VERSION
                ),
                "deckjack install",
            ]
        ),
        "refusal must name both versions and what to do: {reason}"
    );

    // And it is a refusal, not a launch that then fails: nothing was spawned
    // to find out what the app would have said.
    let opened = host.open_window("filmcraft");
    assert!(
        matches!(opened, Err(HostError::Unavailable { .. })),
        "{opened:?}"
    );
    assert_eq!(env.spawn_count(), 0, "an incompatible app must never run");
    assert_eq!(host.window_status("filmcraft"), WindowStatus::Unavailable);
}

#[test]
fn the_core_accepts_exactly_the_protocol_range_it_declares() {
    for version in apps::MIN_PROTOCOL_VERSION..=apps::PROTOCOL_VERSION {
        assert!(apps::check_protocol("pim", version).is_ok(), "{version}");
    }
    assert!(apps::check_protocol("pim", apps::MIN_PROTOCOL_VERSION - 1).is_err());
    assert!(apps::check_protocol("pim", apps::PROTOCOL_VERSION + 1).is_err());
    assert!(apps::check_protocol("pim", u32::MAX).is_err());
}

#[test]
fn an_incompatible_app_is_refused_at_handshake_as_well_as_at_discovery() {
    // Discovery is not the only gate: a binary swapped under a good manifest
    // reaches the handshake, and the handshake must not believe it either.
    let manifest = load_manifest(&manifest_text("pim", "")).unwrap();
    let registration = Registration {
        protocol: apps::PROTOCOL_VERSION + 1,
        app: "pim".to_string(),
        version: "1.0.0".to_string(),
        commands: Vec::new(),
    };
    assert!(apps::accept_registration(&manifest, &registration).is_err());
}

// ---------------------------------------------------------------------------
// A manifest may not turn its own sandbox off
// ---------------------------------------------------------------------------

#[test]
fn a_manifest_cannot_turn_its_own_sandbox_off() {
    // The behavioral half: `confinement` is not a key anywhere in app.toml,
    // wherever an app tries to put it.
    for placement in [
        "confinement = \"unconfined\"\n",
        "[app]\nconfinement = \"unconfined\"\n",
        "[needs]\nconfinement = \"unconfined\"\n",
        "[needs]\nsandbox = false\n",
    ] {
        let toml = format!("{}{placement}", manifest_text("pim", ""));
        let refused = load_manifest(&toml);
        assert!(
            refused.is_err(),
            "a manifest must not be able to ask for less confinement: {placement:?}"
        );
    }

    // The structural half: the type itself has no `Deserialize`, so no
    // future config or manifest field can reach `Unconfined` by accident.
    // Asserted against the source because that is where the absence lives.
    let source = include_str!("../src/apps.rs");
    let before = source
        .split("pub enum Confinement")
        .next()
        .expect("apps.rs declares Confinement");
    let derive = before
        .lines()
        .rev()
        .find(|line| line.trim_start().starts_with("#[derive("))
        .expect("Confinement has a derive attribute");
    assert!(
        !derive.contains("Deserialize"),
        "Confinement must not derive Deserialize: {derive}"
    );
    assert!(
        !source.contains("Deserialize<'de> for Confinement"),
        "Confinement must not implement Deserialize by hand either"
    );

    // And the default is the safe one, so a Policy built from a config file
    // that says nothing enforces.
    assert!(Confinement::default().is_enforced());
}

// ---------------------------------------------------------------------------
// Manifest paths
// ---------------------------------------------------------------------------

#[test]
fn a_manifest_naming_a_path_outside_the_working_directory_is_refused() {
    for path in [
        "../secrets",
        "../../etc",
        "Docs/../../etc",
        "/etc",
        "/",
        ".",
        "./.",
        "",
        "   ",
    ] {
        let toml = format!(
            "{}\n[needs]\nworking_dir = [{{ path = \"{path}\", write = true }}]\n",
            manifest_text("pim", "")
        );
        match load_manifest(&toml) {
            Err(ManifestError::Invalid { message, .. }) => {
                assert!(
                    message.contains("working_dir"),
                    "refusal should name the field: {message}"
                );
            }
            other => panic!("path {path:?} must be refused, got {other:?}"),
        }
    }
}

#[test]
fn a_manifest_reaching_the_hidden_directory_is_refused() {
    for path in [".deckjack", ".deckjack/state.db", "Docs/.deckjack/notes"] {
        let toml = format!(
            "{}\n[needs]\nworking_dir = [{{ path = \"{path}\" }}]\n",
            manifest_text("pim", "")
        );
        match load_manifest(&toml) {
            Err(ManifestError::Invalid { message, .. }) => assert!(
                message.contains(".deckjack"),
                "the operator must be told which directory: {message}"
            ),
            other => panic!("path {path:?} must be refused, got {other:?}"),
        }
    }
}

#[test]
fn a_path_whose_real_target_leaves_the_working_directory_is_not_granted() {
    // The manifest is clean; the filesystem is not. Landlock grants the tree
    // behind the descriptor, so a link is a grant on what it leads to.
    let env = FakeEnv::new();
    install(
        &env,
        "pim",
        &format!(
            "{}\n[needs]\nworking_dir = [{{ path = \"Calendars\" }}]\n",
            manifest_text("pim", "")
        ),
    );
    env.symlink_dir("/work/Calendars", "/home/konstantin/.ssh");

    match plan_for(&env, "pim") {
        Err(ConfinementError::Escapes { real, .. }) => {
            assert_eq!(real, PathBuf::from("/home/konstantin/.ssh"));
        }
        other => panic!("expected a refusal, got {other:?}"),
    }
}

#[test]
fn a_path_whose_real_target_is_the_hidden_directory_is_not_granted() {
    let env = FakeEnv::new();
    install(
        &env,
        "pim",
        &format!(
            "{}\n[needs]\nworking_dir = [{{ path = \"Calendars\" }}]\n",
            manifest_text("pim", "")
        ),
    );
    env.symlink_dir("/work/Calendars", "/work/.deckjack");

    match plan_for(&env, "pim") {
        Err(ConfinementError::Denied { path, denied, .. }) => {
            assert_eq!(path, PathBuf::from("/work/.deckjack"));
            assert_eq!(denied, PathBuf::from("/work/.deckjack"));
        }
        other => panic!("expected a refusal, got {other:?}"),
    }
}

#[test]
fn a_grant_containing_the_hidden_directory_is_refused_too() {
    // Not only a grant *inside* `.deckjack/`: a grant on a tree that
    // contains it reaches state.db through that tree.
    let env = FakeEnv::new();
    install(
        &env,
        "pim",
        &format!(
            "{}\n[needs]\nworking_dir = [{{ path = \"inner\" }}]\n",
            manifest_text("pim", "")
        ),
    );
    env.symlink_dir("/work/inner", "/work");

    match plan_for(&env, "pim") {
        Err(ConfinementError::Denied { .. }) => {}
        other => panic!("expected a refusal, got {other:?}"),
    }
}

#[test]
fn a_read_path_that_is_missing_is_refused_rather_than_granting_its_parent() {
    let env = FakeEnv::new();
    install(
        &env,
        "pim",
        &format!(
            "{}\n[needs]\nworking_dir = [{{ path = \"Calendars\" }}]\n",
            manifest_text("pim", "")
        ),
    );
    // `Calendars` is simply not there, and nothing creates a read need.
    match plan_for(&env, "pim") {
        Err(ConfinementError::MissingPath { path, .. }) => {
            assert_eq!(path, PathBuf::from("/work/Calendars"));
        }
        other => panic!("expected a refusal, got {other:?}"),
    }
}

#[test]
fn a_granted_path_that_is_not_a_directory_is_refused() {
    // Landlock fails the whole ruleset on a directory-only right applied to
    // a file, inside the forked child where the message is gone. So it is
    // refused here, with words.
    let env = FakeEnv::new();
    install(
        &env,
        "pim",
        &format!(
            "{}\n[needs]\nworking_dir = [{{ path = \"notes.md\" }}]\n",
            manifest_text("pim", "")
        ),
    );
    env.write("/work/notes.md", "# notes");

    match plan_for(&env, "pim") {
        Err(ConfinementError::NotADirectory { path, .. }) => {
            assert_eq!(path, PathBuf::from("/work/notes.md"));
        }
        other => panic!("expected a refusal, got {other:?}"),
    }
}

#[test]
fn a_plan_grants_only_what_the_manifest_asked_for_inside_the_working_directory() {
    let env = FakeEnv::new();
    install(
        &env,
        "pim",
        &format!(
            "{}\n[needs]\nnetwork = [\"cloud.example.net\"]\nworking_dir = [{{ path = \"Calendars\", write = true }}]\n",
            manifest_text("pim", "")
        ),
    );

    let plan = plan_for(&env, "pim").expect("a clean manifest must produce a plan");

    let granted: Vec<&PathBuf> = plan
        .sandbox
        .policy
        .read
        .iter()
        .chain(plan.sandbox.policy.read_write.iter())
        .collect();
    let in_working_dir: Vec<&&PathBuf> = granted
        .iter()
        .filter(|path| path.starts_with(WORK))
        .collect();
    assert_eq!(
        in_working_dir,
        vec![&&PathBuf::from("/work/Calendars")],
        "the only part of the working directory granted is the one asked for"
    );
    assert!(
        plan.sandbox
            .policy
            .read_write
            .contains(&PathBuf::from("/work/Calendars")),
        "a write need must be writable"
    );
    assert!(
        !granted
            .iter()
            .any(|path| path.starts_with("/work/.deckjack")),
        "no app is ever granted a path in .deckjack/"
    );
    assert_eq!(plan.sandbox.denied, vec![PathBuf::from("/work/.deckjack")]);

    // The child inherits nothing: HOME and TMPDIR are the app's own, inside
    // the install directory, and never the operator's.
    let env_vars: BTreeMap<&str, &str> = plan
        .env
        .iter()
        .map(|(key, value)| (key.as_str(), value.as_str()))
        .collect();
    assert_eq!(env_vars.get("DECKJACK_CONFINED"), Some(&"1"));
    assert_eq!(
        env_vars.get("DECKJACK_APP_PROTOCOL"),
        Some(&apps::PROTOCOL_VERSION.to_string().as_str())
    );
    for key in ["HOME", "TMPDIR"] {
        let value = env_vars.get(key).unwrap_or_else(|| panic!("{key} is set"));
        assert!(
            value.starts_with(INSTALL),
            "{key} must be the app's own, got {value}"
        );
    }
}

#[test]
fn a_gpu_app_is_granted_the_compute_device_and_others_are_not() {
    // `/dev/kfd` is a character device, not a directory. It used to be
    // carried as "unexpressible" and never granted, so `gpu = true` got the
    // approval prompt and no device. Now it has to land in the policy's file
    // list — and only for an app that asked.
    let env = FakeEnv::new();
    env.write("/dev/kfd", "");
    env.write("/dev/null", "");
    env.write("/dev/urandom", "");
    install(
        &env,
        "photo",
        &format!("{}\n[needs]\ngpu = true\n", manifest_text("photo", "")),
    );
    install(&env, "notes", &manifest_text("notes", ""));

    let gpu = plan_for(&env, "photo").expect("a gpu manifest must produce a plan");
    let plain = plan_for(&env, "notes").expect("a plain manifest must produce a plan");

    let kfd = PathBuf::from("/dev/kfd");
    assert!(
        gpu.sandbox.policy.read_write_files.contains(&kfd),
        "an app that asked for the GPU gets the compute device read-write: {:?}",
        gpu.sandbox.policy
    );
    assert!(
        !plain.sandbox.policy.read_write_files.contains(&kfd)
            && !plain.sandbox.policy.read_files.contains(&kfd),
        "an app that did not ask for the GPU never sees it: {:?}",
        plain.sandbox.policy
    );

    // Every app gets the null device to write to and the entropy device to
    // read, as files: in a directory list either one would fail the ruleset.
    for plan in [&gpu, &plain] {
        let policy = &plan.sandbox.policy;
        assert!(
            policy
                .read_write_files
                .contains(&PathBuf::from("/dev/null"))
        );
        assert!(policy.read_files.contains(&PathBuf::from("/dev/urandom")));
        for file in policy.read_files.iter().chain(&policy.read_write_files) {
            assert!(
                !policy.read.contains(file) && !policy.read_write.contains(file),
                "{} is a file and must not be in a directory list",
                file.display()
            );
        }
    }
}

#[test]
fn a_system_file_this_machine_lacks_is_not_granted() {
    // Handing Landlock a path that is not there fails the whole ruleset, so a
    // machine with no AMD GPU must plan no `/dev/kfd` rather than fail every
    // GPU app's launch.
    let env = FakeEnv::new();
    install(
        &env,
        "photo",
        &format!("{}\n[needs]\ngpu = true\n", manifest_text("photo", "")),
    );
    let plan = plan_for(&env, "photo").expect("plan");
    assert!(
        !plan
            .sandbox
            .policy
            .read_write_files
            .contains(&PathBuf::from("/dev/kfd")),
        "{:?}",
        plan.sandbox.policy
    );
}

// ---------------------------------------------------------------------------
// A node that cannot confine does not run apps
// ---------------------------------------------------------------------------

#[test]
fn a_node_that_cannot_confine_refuses_to_launch_rather_than_running_unconfined() {
    let env = FakeEnv::new();
    env.set_availability(sandbox::Availability::Unavailable {
        reason: "CONFIG_SECURITY_LANDLOCK is not set".to_string(),
    });
    install(
        &env,
        "pim",
        &format!(
            "{}\n[needs]\nnetwork = [\"cloud.example.net\"]\n",
            manifest_text("pim", "")
        ),
    );

    match plan_for(&env, "pim") {
        Err(ConfinementError::Unenforceable {
            app,
            reason,
            summary,
        }) => {
            assert_eq!(app, "pim");
            assert!(
                reason.contains("LANDLOCK"),
                "the kernel's reason survives: {reason}"
            );
            assert!(
                summary.contains("cloud.example.net"),
                "the operator is told what was being asked for: {summary}"
            );
        }
        other => panic!("expected a refusal, got {other:?}"),
    }

    // And the host refuses the same way, with no process started.
    let grants = FakeGrants::default();
    let mut host = host_with(&env, apps::Policy::default(), grants.clone());
    host.refresh();
    host.approve("pim").expect("the operator may still approve");
    let opened = host.open_window("pim");
    assert!(
        matches!(
            opened,
            Err(HostError::Confinement(
                ConfinementError::Unenforceable { .. }
            ))
        ),
        "{opened:?}"
    );
    assert_eq!(
        env.spawn_count(),
        0,
        "an app that cannot be confined must not be spawned at all"
    );
}

#[test]
fn an_app_that_asked_for_no_network_is_refused_where_that_cannot_be_enforced() {
    // Landlock gained network rules at ABI 4. Below that, `network = []`
    // cannot be honored, and granting the network an app did not ask for is
    // not a downgrade this host makes.
    let env = FakeEnv::new();
    env.set_availability(sandbox::Availability::Enforcing {
        landlock_abi: 3,
        network_rules: false,
    });
    install(&env, "pim", &manifest_text("pim", ""));
    match plan_for(&env, "pim") {
        Err(ConfinementError::NoNetworkRules { abi, .. }) => assert_eq!(abi, 3),
        other => panic!("expected a refusal, got {other:?}"),
    }

    // An app that *did* ask for the network loses nothing on that kernel, so
    // it runs — and the launcher knows the grant is coarser than the
    // manifest asked for.
    install(
        &env,
        "notes",
        &format!(
            "{}\n[needs]\nnetwork = [\"cloud.example.net\"]\n",
            manifest_text("notes", "")
        ),
    );
    let plan = plan_for(&env, "notes").expect("an app that asked for network still runs");
    assert!(plan.sandbox.policy.network.is_whole());
    assert!(
        plan.sandbox.network_is_coarse(),
        "a per-host allowlist that cannot be installed must be reported, not hidden"
    );
}

// ---------------------------------------------------------------------------
// Discovery survives bad apps
// ---------------------------------------------------------------------------

#[test]
fn discovery_ignores_a_malformed_app_toml_instead_of_failing_the_scan() {
    let env = FakeEnv::new();
    install(&env, "broken", "this is not toml at all {{{");
    install(
        &env,
        "half",
        "[app]\nname = \"half\"\n", // no version, no protocol
    );
    install(&env, "pim", &manifest_text("pim", ""));

    let found = apps::discover(Path::new(INSTALL), &env);

    assert_eq!(
        found
            .apps
            .iter()
            .map(|a| a.name.as_str())
            .collect::<Vec<_>>(),
        vec!["pim"],
        "the good app is still there"
    );
    assert_eq!(found.problems.len(), 2);
    for problem in &found.problems {
        assert!(
            matches!(problem.error, DiscoveryError::Manifest(_)),
            "{:?}",
            problem.error
        );
        assert!(
            problem.dir.starts_with("/install/apps"),
            "the problem names the directory so the dock can show it: {:?}",
            problem.dir
        );
    }
}

#[test]
fn an_app_whose_directory_does_not_match_its_name_is_refused() {
    // The directory is how an app is addressed: `apps/<name>/`, the binary
    // suffix, the URL segment. A mismatch would make two of those disagree.
    let env = FakeEnv::new();
    env.write(
        &format!("{INSTALL}/apps/calendar/app.toml"),
        &manifest_text("pim", ""),
    );
    env.executable(&format!("{INSTALL}/apps/calendar/deckjack-pim"));

    let found = apps::discover(Path::new(INSTALL), &env);
    assert!(found.apps.is_empty());
    match found.problems.as_slice() {
        [problem] => {
            assert_eq!(problem.name.as_deref(), Some("pim"));
            assert!(matches!(problem.error, DiscoveryError::NameMismatch { .. }));
        }
        other => panic!("expected one problem, got {other:?}"),
    }
}

#[test]
fn an_app_name_that_is_not_a_plain_path_segment_is_refused() {
    // The name is a directory, a URL segment and a command namespace. A name
    // that can traverse or collide is refused before any of those use it.
    for name in [
        "../evil",
        "pim/sub",
        "Pim",
        "9lives",
        "-pim",
        "pim-",
        "pim nice",
        "",
        "a-very-long-name-that-goes-well-past-the-thirty-two-character-limit",
    ] {
        let toml = manifest_text(name, "");
        match load_manifest(&toml) {
            Err(ManifestError::Invalid { message, .. }) => {
                assert!(message.contains("app.name"), "{message}");
            }
            other => panic!("name {name:?} must be refused, got {other:?}"),
        }
    }
}

#[test]
fn an_app_with_no_runnable_binary_is_a_problem_and_not_a_crash() {
    let env = FakeEnv::new();
    // Manifest, no binary at all.
    env.write(
        &format!("{INSTALL}/apps/pim/app.toml"),
        &manifest_text("pim", ""),
    );
    // Manifest, and a binary that is there but not executable.
    env.write(
        &format!("{INSTALL}/apps/notes/app.toml"),
        &manifest_text("notes", ""),
    );
    env.write(&format!("{INSTALL}/apps/notes/deckjack-notes"), "ELF");

    let found = apps::discover(Path::new(INSTALL), &env);
    assert!(found.apps.is_empty());
    let kinds: Vec<&DiscoveryError> = found.problems.iter().map(|p| &p.error).collect();
    assert!(
        kinds
            .iter()
            .any(|e| matches!(e, DiscoveryError::MissingBinary { .. })),
        "{kinds:?}"
    );
    assert!(
        kinds
            .iter()
            .any(|e| matches!(e, DiscoveryError::NotExecutable { .. })),
        "{kinds:?}"
    );
}

#[test]
fn a_node_with_no_apps_installed_is_not_a_problem() {
    // A fresh install has no apps directory at all. That is a computer with
    // no apps, not a computer with a fault.
    let env = FakeEnv::new();
    let found = apps::discover(Path::new(INSTALL), &env);
    assert!(found.apps.is_empty());
    assert!(
        found.problems.is_empty(),
        "a missing apps directory is not a problem: {:?}",
        found.problems.first().map(|p| p.error.to_string())
    );

    let mut host = host(&env);
    assert!(host.refresh().is_empty());
    assert!(host.start_services().is_empty());
    assert!(host.poll().is_empty());
}

#[test]
fn a_file_beside_the_app_directories_is_not_mistaken_for_an_app() {
    let env = FakeEnv::new();
    env.write(&format!("{INSTALL}/apps/README"), "notes to self");
    env.mkdir(&format!("{INSTALL}/apps/scratch"));
    install(&env, "pim", &manifest_text("pim", ""));

    let found = apps::discover(Path::new(INSTALL), &env);
    assert_eq!(found.apps.len(), 1);
    assert!(found.problems.is_empty());
}

// ---------------------------------------------------------------------------
// The manifest is what was approved; a handshake may not widen it
// ---------------------------------------------------------------------------

#[test]
fn a_registration_may_narrow_the_approved_surface_but_never_widen_it() {
    let declared = format!(
        "{}{}",
        manifest_text("pim", ""),
        command_block("pim.event.create", "write")
    );
    let manifest = load_manifest(&declared).unwrap();

    // Narrower: the app decided at runtime it cannot offer the command.
    let narrowed = Registration {
        protocol: apps::PROTOCOL_VERSION,
        app: "pim".to_string(),
        version: "1.0.0".to_string(),
        commands: Vec::new(),
    };
    assert_eq!(
        apps::accept_registration(&manifest, &narrowed)
            .unwrap()
            .len(),
        0
    );

    // Wider: a command nobody approved.
    let widened = Registration {
        commands: vec![
            registered("pim.event.create", Risk::Write),
            registered("pim.mail.send", Risk::Outbound),
        ],
        ..narrowed.clone()
    };
    match apps::accept_registration(&manifest, &widened) {
        Err(apps::HandshakeError::Undeclared { command, .. }) => {
            assert_eq!(command, "pim.mail.send");
        }
        other => panic!("expected a refusal, got {other:?}"),
    }
}

#[test]
fn a_registration_may_not_restate_a_commands_risk() {
    // The terminal.write lesson from the other direction: risk is declared
    // by the backend and approved by the operator, so the running process
    // cannot move a command between risk levels — in either direction. One
    // way would have the operator confirming keystrokes; the other would
    // slip a destructive command past the Inbox.
    let declared = format!(
        "{}{}",
        manifest_text("pim", ""),
        command_block("pim.event.delete", "destructive")
    );
    let manifest = load_manifest(&declared).unwrap();

    for risk in [Risk::Read, Risk::Write, Risk::Outbound] {
        let registration = Registration {
            protocol: apps::PROTOCOL_VERSION,
            app: "pim".to_string(),
            version: "1.0.0".to_string(),
            commands: vec![registered("pim.event.delete", risk)],
        };
        match apps::accept_registration(&manifest, &registration) {
            Err(apps::HandshakeError::RiskChanged {
                declared,
                registered,
                ..
            }) => {
                assert_eq!(declared, Risk::Destructive);
                assert_eq!(registered, risk);
            }
            other => panic!("expected a refusal for {risk:?}, got {other:?}"),
        }
    }
}

#[test]
fn a_command_with_no_schema_object_is_refused_so_the_bus_can_always_validate() {
    let declared = format!(
        "{}{}",
        manifest_text("pim", ""),
        command_block("pim.event.create", "write")
    );
    let manifest = load_manifest(&declared).unwrap();

    for params in [
        serde_json::Value::Bool(true),
        serde_json::Value::Null,
        serde_json::json!("object"),
        serde_json::json!([{ "type": "object" }]),
    ] {
        let mut command = registered("pim.event.create", Risk::Write);
        command.params = params.clone();
        let registration = Registration {
            protocol: apps::PROTOCOL_VERSION,
            app: "pim".to_string(),
            version: "1.0.0".to_string(),
            commands: vec![command],
        };
        assert!(
            matches!(
                apps::accept_registration(&manifest, &registration),
                Err(apps::HandshakeError::BadSchema { .. })
            ),
            "params {params} must be refused"
        );
    }
}

#[test]
fn a_process_on_another_apps_socket_registers_nothing() {
    let manifest = load_manifest(&manifest_text("pim", "")).unwrap();
    let registration = Registration {
        protocol: apps::PROTOCOL_VERSION,
        app: "filmcraft".to_string(),
        version: "1.0.0".to_string(),
        commands: vec![registered("filmcraft.render", Risk::Write)],
    };
    match apps::accept_registration(&manifest, &registration) {
        Err(apps::HandshakeError::WrongApp { expected, found }) => {
            assert_eq!(expected, "pim");
            assert_eq!(found, "filmcraft");
        }
        other => panic!("expected a refusal, got {other:?}"),
    }
}

#[test]
fn a_binary_that_disagrees_with_its_manifest_about_its_version_is_refused() {
    // An update that swapped the binary and not the manifest means the
    // approved surface describes a program that is no longer there.
    let manifest = load_manifest(&manifest_text("pim", "")).unwrap();
    let registration = Registration {
        protocol: apps::PROTOCOL_VERSION,
        app: "pim".to_string(),
        version: "1.1.0".to_string(),
        commands: Vec::new(),
    };
    assert!(matches!(
        apps::accept_registration(&manifest, &registration),
        Err(apps::HandshakeError::VersionMismatch { .. })
    ));
}

#[test]
fn a_command_outside_the_apps_namespace_is_refused_in_the_manifest() {
    // Otherwise installing an app could shadow `system.shutdown`.
    for id in ["system.shutdown", "pimx.event.create", "pim", "pim."] {
        let toml = format!("{}{}", manifest_text("pim", ""), command_block(id, "read"));
        assert!(
            matches!(load_manifest(&toml), Err(ManifestError::Invalid { .. })),
            "command {id:?} must be refused"
        );
    }
    // And a duplicate, which would make the palette's choice arbitrary.
    let toml = format!(
        "{}{}{}",
        manifest_text("pim", ""),
        command_block("pim.event.create", "read"),
        command_block("pim.event.create", "destructive")
    );
    assert!(matches!(
        load_manifest(&toml),
        Err(ManifestError::Invalid { .. })
    ));
}

#[test]
fn only_destructive_and_outbound_commands_wait_in_the_inbox() {
    // A command that fires on every keystroke must not be Inbox-gated, and a
    // command that leaves the machine must be. The spelling in app.toml is
    // part of this: a risk level nobody can write is a risk level nobody
    // declares.
    let cases = [
        ("read", Risk::Read, false),
        ("write", Risk::Write, false),
        ("destructive", Risk::Destructive, true),
        ("outbound", Risk::Outbound, true),
    ];
    for (spelling, risk, inbox) in cases {
        let toml = format!(
            "{}{}",
            manifest_text("pim", ""),
            command_block("pim.thing.do", spelling)
        );
        let manifest = load_manifest(&toml).unwrap_or_else(|e| panic!("{spelling}: {e}"));
        assert_eq!(manifest.commands[0].risk, risk, "{spelling}");
        assert_eq!(risk.needs_inbox(), inbox, "{spelling}");
    }
}

// ---------------------------------------------------------------------------
// Approvals
// ---------------------------------------------------------------------------

#[test]
fn an_app_that_asks_for_more_than_was_approved_does_not_run_on_the_old_grant() {
    let env = FakeEnv::new();
    let needs_one_host = format!(
        "{}\n[needs]\nnetwork = [\"cloud.example.net\"]\n",
        manifest_text("pim", "")
    );
    install(&env, "pim", &needs_one_host);

    let grants = FakeGrants::default();
    let mut host = host_with(&env, apps::Policy::default(), grants.clone());
    host.refresh();
    host.approve("pim").unwrap();
    host.open_window("pim").expect("an approved app runs");
    assert_eq!(env.spawn_count(), 1);

    // An update now asks for a second host. The grant describes the manifest
    // that was shown, not whatever the directory holds later.
    host.stop("pim", false);
    install(
        &env,
        "pim",
        &format!(
            "{}\n[needs]\nnetwork = [\"cloud.example.net\", \"ads.example.com\"]\n",
            manifest_text("pim", "")
        ),
    );
    host.refresh();

    match host.open_window("pim") {
        Err(HostError::GrantStale { summary, .. }) => assert!(
            summary.contains("ads.example.com"),
            "the operator is shown what changed: {summary}"
        ),
        other => panic!("expected a stale grant, got {other:?}"),
    }
    assert_eq!(
        env.spawn_count(),
        1,
        "nothing new was started on the old approval"
    );
}

#[test]
fn an_app_with_no_approval_is_not_launched_and_is_told_what_it_asked_for() {
    let env = FakeEnv::new();
    install(
        &env,
        "pim",
        &format!(
            "{}\n[needs]\ngpu = true\nnetwork = [\"cloud.example.net\"]\n",
            manifest_text("pim", "")
        ),
    );
    let mut host = host(&env);
    host.refresh();

    match host.open_window("pim") {
        Err(HostError::NotApproved { summary, .. }) => {
            assert!(
                mentions(&summary, &["GPU", "cloud.example.net"]),
                "{summary}"
            );
        }
        other => panic!("expected a refusal, got {other:?}"),
    }
    assert_eq!(env.spawn_count(), 0);
}

#[test]
fn an_app_that_asks_for_nothing_runs_without_manufacturing_an_approval() {
    let env = FakeEnv::new();
    install(&env, "clock", &manifest_text("clock", ""));
    let mut host = host(&env);
    host.refresh();
    host.open_window("clock")
        .expect("an app with nothing to approve needs no dialog");
    assert_eq!(env.spawn_count(), 1);
}

#[test]
fn a_fingerprint_changes_with_what_was_approved_and_not_with_a_patch_release() {
    let base = format!(
        "{}{}",
        manifest_text("pim", ""),
        command_block("pim.event.create", "write")
    );
    let original = load_manifest(&base).unwrap().fingerprint();

    // A patch release that asks for nothing new must not nag.
    let repackaged = load_manifest(
        &base
            .replace("1.0.0", "1.0.1")
            .replace("[app]", "[app]\ntitle = \"Calendar\""),
    )
    .unwrap()
    .fingerprint();
    assert_eq!(original, repackaged);

    // Anything the operator approved changes it.
    let riskier =
        load_manifest(&base.replace("risk = \"write\"", "risk = \"destructive\"")).unwrap();
    let networked =
        load_manifest(&format!("{base}\n[needs]\nnetwork = [\"example.net\"]\n")).unwrap();
    let extra_command = load_manifest(&format!(
        "{base}{}",
        command_block("pim.event.delete", "destructive")
    ))
    .unwrap();
    for changed in [riskier, networked, extra_command] {
        assert_ne!(
            original,
            changed.fingerprint(),
            "a changed approval surface must need approving again"
        );
    }

    // And it is stable: the same manifest hashes the same way twice, or an
    // upgrade would revoke every grant on the machine.
    assert_eq!(original, load_manifest(&base).unwrap().fingerprint());
}

// ---------------------------------------------------------------------------
// Supervision
// ---------------------------------------------------------------------------

/// Two service apps, both up and registered.
fn two_running_services(env: &FakeEnv) -> Host<FakeEnv> {
    for name in ["pim", "notes"] {
        install(
            env,
            name,
            &format!(
                "{}{}",
                manifest_text(name, "lifecycle = \"service\"\n"),
                command_block(&format!("{name}.thing.do"), "read")
            ),
        );
    }
    let mut host = host(env);
    host.refresh();
    assert!(host.start_services().is_empty(), "both services started");
    for name in ["pim", "notes"] {
        host.on_registered(
            name,
            vec![registered(&format!("{name}.thing.do"), Risk::Read)],
        );
        assert_eq!(host.window_status(name), WindowStatus::Live);
    }
    host
}

#[test]
fn a_crashed_app_restarts_and_does_not_take_the_desktop_with_it() {
    let env = FakeEnv::new();
    let mut host = two_running_services(&env);

    env.crash("pim");
    let events = host.poll();

    assert!(reconnecting_for(&events, "pim"), "{events:?}");
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, HostEvent::Reconnecting { app, .. } if app == "notes")),
        "one app crashing says nothing about another: {events:?}"
    );
    // The window waits rather than closing, and the rest of the desktop is
    // untouched.
    assert_eq!(host.window_status("pim"), WindowStatus::Reconnecting);
    assert_eq!(host.window_status("notes"), WindowStatus::Live);

    // A crashed app's commands leave the palette: offering what cannot be
    // dispatched is worse than offering less.
    let offered: Vec<&str> = host.ready_commands().iter().map(|(app, _)| *app).collect();
    assert_eq!(offered, vec!["notes"]);

    // It comes back.
    env.advance(apps::Policy::default().restart_backoff_secs);
    let events = host.poll();
    assert_eq!(env.spawn_count(), 3, "pim was restarted: {events:?}");
    assert!(matches!(
        host.runtime("pim").map(|r| r.phase.clone()),
        Some(Phase::Starting { .. })
    ));
    host.on_registered("pim", vec![registered("pim.thing.do", Risk::Read)]);
    assert_eq!(host.window_status("pim"), WindowStatus::Live);
    assert_eq!(host.ready_commands().len(), 2);
}

#[test]
fn nothing_is_relaunched_before_its_backoff_is_due_and_the_wait_grows() {
    let policy = apps::Policy {
        restart_backoff_secs: 2,
        max_backoff_secs: 600,
        crash_window_secs: 3_600,
        max_crashes: 50,
        ..apps::Policy::default()
    };
    let env = FakeEnv::new();
    install(
        &env,
        "pim",
        &manifest_text("pim", "lifecycle = \"service\"\n"),
    );
    let mut host = host_with(&env, policy.clone(), FakeGrants::default());
    host.refresh();
    host.start_services();
    host.on_registered("pim", Vec::new());

    // Crashing without ever registering again: an app that dies on startup
    // must be tried less and less often. (A start that *succeeds* resets the
    // counter on purpose, which is why nothing registers inside this loop.)
    let mut waits = Vec::new();
    for _ in 0..3 {
        let spawns_before = env.spawn_count();
        env.crash("pim");
        host.poll();

        // Nothing restarts until the backoff has passed, however often the
        // supervisor is polled.
        let mut waited = 0;
        while env.spawn_count() == spawns_before {
            assert!(waited < 10_000, "the app was never restarted");
            env.advance(1);
            waited += 1;
            host.poll();
        }
        waits.push(waited);
    }

    assert!(
        waits.windows(2).all(|pair| pair[1] > pair[0]),
        "each restart must wait longer than the last: {waits:?}"
    );
    assert!(
        waits[0] >= policy.restart_backoff_secs,
        "the first wait is at least the base backoff: {waits:?}"
    );

    // And a start that comes up clears the debt: an app that ran for a week
    // and then crashed once must come back at once, not after minutes.
    host.on_registered("pim", Vec::new());
    let spawns = env.spawn_count();
    env.crash("pim");
    host.poll();
    env.advance(policy.restart_backoff_secs);
    host.poll();
    assert_eq!(
        env.spawn_count(),
        spawns + 1,
        "a healthy app's first crash is not punished with the old backoff"
    );
}

#[test]
fn an_app_that_cannot_stop_crashing_is_left_stopped_rather_than_restarted_forever() {
    let policy = apps::Policy {
        restart_backoff_secs: 1,
        max_backoff_secs: 2,
        crash_window_secs: 10_000,
        max_crashes: 3,
        ..apps::Policy::default()
    };
    let env = FakeEnv::new();
    install(
        &env,
        "pim",
        &manifest_text("pim", "lifecycle = \"service\"\n"),
    );
    let mut host = host_with(&env, policy.clone(), FakeGrants::default());
    host.refresh();
    host.start_services();

    let mut gave_up = None;
    // Generous enough to let the exact threshold be the module's business,
    // strict enough that "restarted forever" fails here.
    for _ in 0..(policy.max_crashes + 3) {
        env.crash("pim");
        for event in host.poll() {
            if let HostEvent::GaveUp { app, crashes, .. } = event {
                gave_up = Some((app, crashes));
            }
        }
        env.advance(policy.max_backoff_secs + 1);
        host.poll();
        host.on_registered("pim", Vec::new());
    }

    let (app, crashes) = gave_up.expect("an app that only ever crashes must be left stopped");
    assert_eq!(app, "pim");
    assert!(crashes >= policy.max_crashes);

    // Left alone from then on, however long the timer runs.
    let spawns = env.spawn_count();
    for _ in 0..5 {
        env.advance(10_000);
        assert!(host.poll().is_empty());
    }
    assert_eq!(env.spawn_count(), spawns, "a given-up app is not restarted");
    assert_eq!(host.window_status("pim"), WindowStatus::Unavailable);
    assert!(matches!(
        host.open_window("pim"),
        Err(HostError::CrashLoop { .. })
    ));

    // And the operator can ask again.
    host.retry("pim").expect("retry starts it");
    assert_eq!(env.spawn_count(), spawns + 1);
}

#[test]
fn an_app_that_connects_and_says_nothing_is_treated_as_a_crash() {
    // A hung app is exactly as unusable as a dead one, and it still holds
    // the socket, so it is signaled before another instance is started.
    let env = FakeEnv::new();
    install(&env, "pim", &manifest_text("pim", ""));
    let mut host = host(&env);
    host.refresh();
    host.open_window("pim").unwrap();
    let pid = env.pid_of("pim");

    env.advance(apps::Policy::default().handshake_secs);
    let events = host.poll();

    assert!(reconnecting_for(&events, "pim"), "{events:?}");
    assert!(
        env.signaled().contains(&pid),
        "a stalled app must be asked to go, or two instances race for the socket"
    );
}

#[test]
fn an_idle_on_demand_app_is_stopped_and_a_service_is_not() {
    let env = FakeEnv::new();
    install(&env, "pim", &manifest_text("pim", ""));
    install(
        &env,
        "sync",
        &manifest_text("sync", "lifecycle = \"service\"\n"),
    );
    let policy = apps::Policy::default();
    let mut host = host(&env);
    host.refresh();
    host.start_services();
    host.on_registered("sync", Vec::new());
    host.open_window("pim").unwrap();
    host.on_registered("pim", vec![registered("pim.thing.do", Risk::Read)]);
    host.close_window("pim");

    // Still warm a second before the timeout: closing and reopening a window
    // must be instant.
    env.advance(policy.idle_secs - 1);
    assert!(host.poll().is_empty());
    assert_eq!(host.window_status("pim"), WindowStatus::Live);

    env.advance(1);
    let events = host.poll();
    assert!(
        events
            .iter()
            .any(|e| matches!(e, HostEvent::Stopped { app, idle: true } if app == "pim")),
        "{events:?}"
    );
    assert_eq!(host.window_status("pim"), WindowStatus::Unavailable);
    assert!(
        host.ready_commands().is_empty(),
        "a stopped app's commands leave the palette"
    );

    // A service exists to keep working with no window open.
    env.advance(policy.idle_secs * 100);
    let events = host.poll();
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, HostEvent::Stopped { app, .. } if app == "sync")),
        "a service must not be idle-stopped: {events:?}"
    );
}

#[test]
fn a_command_call_keeps_an_app_warm_with_no_window_open() {
    // An agent calling a command is use, even though nothing is on screen.
    let env = FakeEnv::new();
    install(&env, "pim", &manifest_text("pim", ""));
    let policy = apps::Policy::default();
    let mut host = host(&env);
    host.refresh();
    host.open_window("pim").unwrap();
    host.on_registered("pim", Vec::new());
    host.close_window("pim");

    for _ in 0..3 {
        env.advance(policy.idle_secs - 1);
        host.note_activity("pim");
        assert!(host.poll().is_empty());
    }
    assert_eq!(host.window_status("pim"), WindowStatus::Live);
}

#[test]
fn an_on_demand_app_nobody_has_open_is_not_restarted() {
    // A crash loop in an app with no window would restart forever in the
    // background, on a phone's battery.
    let env = FakeEnv::new();
    install(&env, "pim", &manifest_text("pim", ""));
    let mut host = host(&env);
    host.refresh();
    host.open_window("pim").unwrap();
    host.on_registered("pim", Vec::new());

    host.close_window("pim");
    env.crash("pim");
    host.poll();
    let spawns = env.spawn_count();

    for _ in 0..5 {
        env.advance(1_000);
        host.poll();
    }
    assert_eq!(
        env.spawn_count(),
        spawns,
        "nothing wants this app, so nothing restarts it"
    );
}

#[test]
fn a_window_that_was_refused_does_not_keep_an_app_alive() {
    // `open_window` returning an error means the desktop has no window, so
    // nothing is holding the app open. Counting a refused window would make
    // the app ineligible for the idle sweep for the rest of the session —
    // the GPU context and the battery drain the sweep exists to stop.
    let env = FakeEnv::new();
    install(&env, "pim", &manifest_text("pim", ""));
    let policy = apps::Policy::default();
    let mut host = host(&env);
    host.refresh();

    env.fail_spawns(true);
    for _ in 0..3 {
        assert!(matches!(
            host.open_window("pim"),
            Err(HostError::Spawn { .. })
        ));
    }
    env.fail_spawns(false);

    // Now it starts, with no window open anywhere.
    host.retry("pim").expect("a retry starts it");
    host.on_registered("pim", Vec::new());
    assert_eq!(
        host.runtime("pim").map(|runtime| runtime.windows),
        Some(0),
        "no window was ever opened"
    );

    env.advance(policy.idle_secs);
    let events = host.poll();
    assert!(
        events
            .iter()
            .any(|e| matches!(e, HostEvent::Stopped { app, idle: true } if app == "pim")),
        "an app nothing has open must still be idle-stopped: {events:?}"
    );
}

#[test]
fn a_stop_the_host_asked_for_is_not_counted_as_a_crash() {
    // Stopping an app closes its socket, and the transport reports that the
    // same way it reports a crash. If the host believed it, an idle stop
    // would leave the app `Reconnecting` with nothing scheduled, and every
    // crash the poll loop already recorded would be counted twice — halving
    // the crash budget before the app is given up on.
    let env = FakeEnv::new();
    install(&env, "pim", &manifest_text("pim", ""));
    let mut host = host(&env);
    host.refresh();
    host.open_window("pim").unwrap();
    host.on_registered("pim", Vec::new());

    host.stop("pim", true);
    let events = host.on_lost("pim", "the app closed the connection");

    assert!(
        events.is_empty(),
        "a deliberate stop is not a crash: {events:?}"
    );
    assert_eq!(
        host.window_status("pim"),
        WindowStatus::Unavailable,
        "a stopped app must not claim it is coming back"
    );

    // The same report arriving twice for one crash counts once.
    host.retry("pim").unwrap();
    host.on_registered("pim", Vec::new());
    env.crash("pim");
    let first = host.poll();
    assert!(reconnecting_for(&first, "pim"), "{first:?}");
    let attempt_after_poll = match host.runtime("pim").map(|r| r.phase.clone()) {
        Some(Phase::Reconnecting { attempt, .. }) => attempt,
        other => panic!("expected reconnecting, got {other:?}"),
    };
    let again = host.on_lost("pim", "the app closed the connection");
    assert!(again.is_empty(), "one crash, one record: {again:?}");
    assert!(matches!(
        host.runtime("pim").map(|r| r.phase.clone()),
        Some(Phase::Reconnecting { attempt, .. }) if attempt == attempt_after_poll
    ));
}

#[test]
fn a_registration_the_host_did_not_ask_for_is_ignored() {
    // A leftover process still holding the socket, or a second instance,
    // would otherwise put commands in the palette that route to nothing.
    let env = FakeEnv::new();
    install(&env, "pim", &manifest_text("pim", ""));
    let mut host = host(&env);
    host.refresh();

    // Never launched.
    assert!(
        host.on_registered("pim", vec![registered("pim.thing.do", Risk::Read)])
            .is_empty()
    );
    assert!(host.ready_commands().is_empty());
    assert_eq!(host.window_status("pim"), WindowStatus::Unavailable);

    // Up, then deliberately stopped: the dying process registers again.
    host.open_window("pim").unwrap();
    host.on_registered("pim", vec![registered("pim.thing.do", Risk::Read)]);
    host.stop("pim", false);
    assert!(
        host.on_registered("pim", vec![registered("pim.thing.do", Risk::Read)])
            .is_empty()
    );
    assert!(host.ready_commands().is_empty());

    // And an app that is not installed at all.
    assert!(host.on_registered("filmcraft", Vec::new()).is_empty());
}

#[test]
fn an_uninstalled_app_is_never_confused_with_one_that_is_coming_back() {
    let env = FakeEnv::new();
    let mut host = host(&env);
    host.refresh();
    assert_eq!(host.window_status("pim"), WindowStatus::Unavailable);
    assert!(matches!(
        host.open_window("pim"),
        Err(HostError::NotInstalled(_))
    ));
    assert!(matches!(host.retry("pim"), Err(HostError::NotInstalled(_))));
    // Stopping something that was never started is a no-op, not a panic.
    host.stop("pim", false);
    host.close_window("pim");
    host.note_activity("pim");
    host.stop_all();
}

#[test]
fn stopping_the_core_signals_every_app_it_started() {
    let env = FakeEnv::new();
    let mut host = two_running_services(&env);
    let pids = ["pim", "notes"].map(|app| env.pid_of(app));

    host.stop_all();

    for pid in pids {
        assert!(env.signaled().contains(&pid), "{pid} was not asked to stop");
    }
    for app in ["pim", "notes"] {
        assert_eq!(host.window_status(app), WindowStatus::Unavailable);
    }
    assert!(host.ready_commands().is_empty());
}

#[test]
fn a_revoked_approval_stops_the_app_it_covered() {
    let env = FakeEnv::new();
    install(
        &env,
        "pim",
        &format!(
            "{}\n[needs]\nnetwork = [\"cloud.example.net\"]\n",
            manifest_text("pim", "")
        ),
    );
    let grants = FakeGrants::default();
    let mut host = host_with(&env, apps::Policy::default(), grants.clone());
    host.refresh();
    host.approve("pim").unwrap();
    host.open_window("pim").unwrap();
    host.on_registered("pim", vec![registered("pim.thing.do", Risk::Read)]);
    let pid = env.pid_of("pim");

    host.revoke("pim").unwrap();

    assert!(env.signaled().contains(&pid), "the app must be stopped");
    assert!(host.ready_commands().is_empty());
    assert!(
        matches!(host.open_window("pim"), Err(HostError::NotApproved { .. })),
        "a revoked app does not run again on the way out"
    );
}

#[test]
fn approving_an_app_that_is_not_installed_is_refused() {
    let env = FakeEnv::new();
    let mut host = host(&env);
    host.refresh();
    assert!(matches!(
        host.approve("filmcraft"),
        Err(HostError::NotInstalled(_))
    ));
}

// ---------------------------------------------------------------------------
// The wire format, pinned to the bytes the other half must write
// ---------------------------------------------------------------------------

#[test]
fn the_wire_format_is_the_one_the_other_half_has_to_write() {
    // This is the bug class that cost the most: two halves inventing
    // different tag fields, different shapes and different correlation. So
    // the bytes are asserted literally, not round-tripped.
    let line = apps::encode_frame(&CoreFrame::Call {
        id: 7,
        command: "pim.event.create".to_string(),
        params: serde_json::json!({ "title": "Dentist" }),
    })
    .unwrap();
    assert!(line.ends_with('\n'), "frames are newline-delimited");
    assert_eq!(line.matches('\n').count(), 1, "one frame is one line");

    let value: serde_json::Value = serde_json::from_str(line.trim_end()).unwrap();
    assert_eq!(value["frame"], "call", "the tag field is `frame`");
    assert_eq!(value["id"], 7, "correlation is a numeric `id`");
    assert_eq!(value["command"], "pim.event.create");

    // Written by hand the way an app's own encoder would, parsed by the core.
    let registration: AppFrame = apps::decode_frame(
        r#"{"frame":"register","protocol":1,"app":"pim","version":"1.0.0","commands":[]}"#,
    )
    .expect("an app's registration must parse");
    assert!(matches!(registration, AppFrame::Register(_)));

    let result: AppFrame =
        apps::decode_frame(r#"{"frame":"result","id":7,"result":{"ok":true}}"#).unwrap();
    match result {
        AppFrame::Result { id, result, error } => {
            assert_eq!(id, 7, "the answer carries the call's id");
            assert!(result.is_some() && error.is_none());
        }
        other => panic!("expected a result, got {other:?}"),
    }

    // A frame with no tag, or someone else's tag, is not silently accepted.
    for bad in [
        r#"{"id":7,"result":{}}"#,
        r#"{"type":"result","id":7}"#,
        r#"{"frame":"results","id":7}"#,
        "not json at all",
        "",
    ] {
        assert!(
            apps::decode_frame::<AppFrame>(bad).is_err(),
            "{bad:?} must not parse"
        );
    }

    // And the names are kebab-case, as both halves' generated types assume.
    for (frame, tag) in [
        (CoreFrame::Stop, "stop"),
        (CoreFrame::Cancel { id: 1 }, "cancel"),
        (
            CoreFrame::Ui {
                id: 1,
                path: "index.html".to_string(),
            },
            "ui",
        ),
    ] {
        let encoded = apps::encode_frame(&frame).unwrap();
        let value: serde_json::Value = serde_json::from_str(encoded.trim_end()).unwrap();
        assert_eq!(value["frame"], tag);
    }
}

#[test]
fn a_frame_over_the_size_limit_is_refused_rather_than_parsed() {
    let mut oversized = String::with_capacity(apps::MAX_FRAME_BYTES + 2);
    oversized.push('"');
    oversized.extend(std::iter::repeat_n('a', apps::MAX_FRAME_BYTES));
    oversized.push('"');
    match apps::decode_frame::<AppFrame>(&oversized) {
        Err(apps::ProtocolError::FrameTooLong { size }) => {
            assert!(size > apps::MAX_FRAME_BYTES)
        }
        other => panic!("expected a refusal, got {other:?}"),
    }
}

#[test]
fn a_ui_asset_that_is_not_base64_is_refused_instead_of_served() {
    let bad = WireAsset {
        content_type: "text/html".to_string(),
        body: "not base64 %%%".to_string(),
        immutable: false,
    };
    assert!(matches!(
        bad.decode(),
        Err(apps::ProtocolError::Malformed(_))
    ));

    let good = WireAsset {
        content_type: "text/html; charset=utf-8".to_string(),
        body: "PCFkb2N0eXBlIGh0bWw+".to_string(),
        immutable: true,
    };
    let asset = good.decode().unwrap();
    assert_eq!(asset.bytes, b"<!doctype html>");
    assert!(asset.immutable, "a hashed asset stays cacheable");
}

/// An app's bytes are cached by the browser that fetched them and by nobody
/// else. `public` would authorize a shared cache to hand a hashed asset to a
/// client with no session (docs/APP-UI-ISOLATION.md §4.2).
#[tokio::test]
async fn an_apps_ui_assets_are_cached_privately_or_not_at_all() {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    struct OneApp;
    impl apps::UiHost for OneApp {
        fn window_status(&self, _app: &str) -> WindowStatus {
            WindowStatus::Live
        }
        fn ui_asset(&self, _app: &str, path: &str) -> Result<Option<apps::UiAsset>, apps::UiError> {
            Ok(Some(apps::UiAsset {
                content_type: "text/javascript".to_string(),
                bytes: b"export {}".to_vec(),
                // Only a content-hashed file is immutable.
                immutable: path.starts_with("assets/"),
            }))
        }
    }

    let app = axum::Router::new().nest(apps::UI_MOUNT, apps::ui_router(Arc::new(OneApp)));
    let tcp = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("a loopback port");
    let addr = tcp.local_addr().expect("the bound address");
    tokio::spawn(async move {
        let _ = axum::serve(tcp, app).await;
    });

    for (path, want) in [
        (
            "/apps/pim/assets/index-abc123.js",
            "private, max-age=31536000, immutable",
        ),
        ("/apps/pim/", "no-store"),
    ] {
        let mut stream = tokio::net::TcpStream::connect(addr).await.expect("connect");
        stream
            .write_all(
                format!("GET {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n")
                    .as_bytes(),
            )
            .await
            .expect("send");
        let mut raw = String::new();
        stream.read_to_string(&mut raw).await.expect("read");
        let cache = raw
            .lines()
            .find_map(|l| {
                l.split_once(':')
                    .filter(|(k, _)| k.eq_ignore_ascii_case("cache-control"))
                    .map(|(_, v)| v.trim().to_string())
            })
            .unwrap_or_default();
        assert!(raw.starts_with("HTTP/1.1 200"), "{path}: {raw}");
        assert_eq!(cache, want, "{path}");
        assert!(!cache.contains("public"), "{path}: {cache}");
    }
}

// ---------------------------------------------------------------------------
// Smaller claims the rest of the module rests on
// ---------------------------------------------------------------------------

#[test]
fn a_manifest_says_what_it_needs_and_nothing_else_is_assumed() {
    let manifest = load_manifest(&manifest_text("pim", "")).unwrap();
    assert_eq!(
        manifest.app.lifecycle,
        Lifecycle::OnDemand,
        "on demand by default"
    );
    assert!(
        manifest.needs.is_empty(),
        "an app is granted nothing by default"
    );
    assert!(!manifest.needs.gpu);
    assert!(
        manifest.needs.network.is_empty(),
        "no hosts named means no network at all"
    );
    assert_eq!(manifest.title(), "pim", "the title falls back to the name");
    assert_eq!(
        apps::summarize(&manifest.needs),
        "nothing beyond the command bus"
    );

    let full = load_manifest(&format!(
        "{}\n[needs]\ngpu = true\nnetwork = [\"a.example\"]\nworking_dir = [{{ path = \"Docs\", write = true }}]\n",
        manifest_text("pim", "title = \"Calendar\"\nlifecycle = \"service\"\n")
    ))
    .unwrap();
    assert_eq!(full.app.lifecycle, Lifecycle::Service);
    assert_eq!(full.title(), "Calendar");
    assert!(!full.needs.is_empty());
    assert!(mentions(
        &apps::summarize(&full.needs),
        &["GPU", "a.example", "read and write Docs"]
    ));
}

#[test]
fn a_network_entry_that_is_not_a_host_name_is_refused() {
    for host in ["https://example.net/path", "example.net/", "", " "] {
        let toml = format!(
            "{}\n[needs]\nnetwork = [\"{host}\"]\n",
            manifest_text("pim", "")
        );
        assert!(
            matches!(load_manifest(&toml), Err(ManifestError::Invalid { .. })),
            "{host:?} must be refused"
        );
    }
}

#[test]
fn an_app_socket_lives_in_the_install_directory_and_never_in_the_working_one() {
    // A socket that got synced to another machine would be a door to a
    // process that was never there.
    let path = apps::socket_path(Path::new(INSTALL), "pim");
    assert!(path.starts_with(INSTALL));
    assert!(!path.starts_with(WORK));
    assert!(path.to_string_lossy().contains("pim"));
}

// ---------------------------------------------------------------------------
// The dock fields: what the shell builds its app table from
// ---------------------------------------------------------------------------

#[test]
fn a_manifest_carries_the_dock_fields_the_shell_needs() {
    let manifest = load_manifest(&manifest_text(
        "photo",
        "title = \"Photos\"\ntag = \"Ph\"\nsummary = \"Edit and sort photographs\"\n\
         min_width = 360\ndefault_width = 900\ndefault_height = 600\ncompact = false\n",
    ))
    .expect("a manifest with every dock field");
    let app = &manifest.app;
    assert_eq!(app.tag.as_deref(), Some("Ph"));
    assert_eq!(app.summary.as_deref(), Some("Edit and sort photographs"));
    assert_eq!(
        (app.default_width, app.default_height),
        (Some(900), Some(600))
    );
    assert!(!app.compact, "an app may say it has no compact layout");
    assert!(app.wide);
}

#[test]
fn an_app_that_says_nothing_about_layouts_supports_both() {
    // The app contract says every app does both. Absent must mean that, or
    // every manifest written before these fields existed would vanish from
    // one of the two modes.
    let manifest = load_manifest(&manifest_text("plain", "")).unwrap();
    assert!(manifest.app.compact && manifest.app.wide);
    assert_eq!(manifest.app.tag, None);
    assert_eq!(manifest.app.summary, None);
}

#[test]
fn dock_fields_the_shell_could_not_render_are_refused_at_discovery() {
    for (extra, names) in [
        ("tag = \"Pho\"\n", "app.tag"),
        ("tag = \"\"\n", "app.tag"),
        ("tag = \"P!\"\n", "app.tag"),
        ("summary = \"\"\n", "app.summary"),
        ("summary = \"two\\nlines\"\n", "app.summary"),
        (
            &*format!("summary = \"{}\"\n", "x".repeat(121)),
            "app.summary",
        ),
        (
            "min_width = 400\ndefault_width = 300\n",
            "app.default_width",
        ),
        ("default_height = 0\n", "app.default_height"),
        ("compact = false\nwide = false\n", "app.compact"),
    ] {
        let refused = load_manifest(&manifest_text("bad", extra));
        match refused {
            Err(ManifestError::Invalid { message, .. }) => assert!(
                message.contains(names),
                "{extra:?} was refused, but the message does not name {names}: {message}"
            ),
            other => panic!("{extra:?} must be refused as invalid, got {other:?}"),
        }
    }
    // The edges that are fine, so the rules above are limits and not bans.
    for extra in [
        "tag = \"P\"\n",
        "tag = \"9z\"\n",
        &*format!("summary = \"{}\"\n", "x".repeat(120)),
        "min_width = 400\ndefault_width = 400\n",
        "wide = false\n",
    ] {
        load_manifest(&manifest_text("good", extra))
            .unwrap_or_else(|e| panic!("{extra:?} should be accepted: {e}"));
    }
}

// ---------------------------------------------------------------------------
// Network granularity: TCP ports through Landlock
// ---------------------------------------------------------------------------

fn with_needs(name: &str, needs: &str) -> String {
    format!("{}\n[needs]\n{needs}\n", manifest_text(name, ""))
}

#[test]
fn naming_tcp_ports_narrows_the_network_to_exactly_those_ports() {
    let env = FakeEnv::new();
    env.mkdir("/run/systemd/resolve");
    let resolver = Path::new("/run/systemd/resolve");

    install(
        &env,
        "pim",
        &with_needs(
            "pim",
            "network = [\"cloud.example.net\"]\nconnect_tcp = [443, 8443]",
        ),
    );
    let plan = plan_for(&env, "pim").expect("a ported manifest plans");
    assert_eq!(
        plan.sandbox.policy.network,
        sandbox::Network::Ports {
            connect_tcp: vec![443, 8443],
            bind_tcp: vec![],
        },
        "the ports the manifest named are the ones the kernel is told"
    );
    assert!(
        !plan.sandbox.network_is_coarse(),
        "a manifest narrowed to ports is not reported as the whole network"
    );
    assert!(
        plan.sandbox.policy.read.iter().any(|p| p == resolver),
        "an app that connects to hosts by name can read the resolver config"
    );

    // Listening only: no outbound port, so no hosts needed and no DNS.
    install(&env, "dev", &with_needs("dev", "bind_tcp = [8080]"));
    let plan = plan_for(&env, "dev").expect("a bind-only manifest plans");
    assert_eq!(
        plan.sandbox.policy.network,
        sandbox::Network::Ports {
            connect_tcp: vec![],
            bind_tcp: vec![8080],
        }
    );
    assert!(
        !plan.sandbox.policy.read.iter().any(|p| p == resolver),
        "an app that only listens has no use for the resolver config"
    );
}

#[test]
fn the_old_network_keys_mean_what_they_meant_when_they_were_approved() {
    let env = FakeEnv::new();
    env.mkdir("/run/systemd/resolve");
    let resolver = Path::new("/run/systemd/resolve");

    install(&env, "quiet", &manifest_text("quiet", ""));
    let plan = plan_for(&env, "quiet").unwrap();
    assert!(
        plan.sandbox.policy.network.is_denied(),
        "no hosts and no ports is no TCP at all: {:?}",
        plan.sandbox.policy.network
    );
    assert!(!plan.sandbox.policy.read.iter().any(|p| p == resolver));

    install(
        &env,
        "pim",
        &with_needs("pim", "network = [\"cloud.example.net\"]"),
    );
    let plan = plan_for(&env, "pim").unwrap();
    assert!(
        plan.sandbox.policy.network.is_whole(),
        "hosts with no ports keep the whole network, so an approved manifest does not break"
    );
    assert!(plan.sandbox.network_is_coarse());
    assert!(plan.sandbox.policy.read.iter().any(|p| p == resolver));
}

#[test]
fn ports_on_a_kernel_without_network_rules_are_refused_not_widened() {
    let env = FakeEnv::new();
    env.set_availability(sandbox::Availability::Enforcing {
        landlock_abi: 3,
        network_rules: false,
    });
    for (name, needs) in [
        (
            "pim",
            "network = [\"cloud.example.net\"]\nconnect_tcp = [443]",
        ),
        ("dev", "bind_tcp = [8080]"),
    ] {
        install(&env, name, &with_needs(name, needs));
        match plan_for(&env, name) {
            Err(ConfinementError::NoNetworkRules { abi, .. }) => assert_eq!(abi, 3),
            other => {
                panic!("{name}: a port list that cannot be installed must refuse, got {other:?}")
            }
        }
    }
}

#[test]
fn a_port_list_that_cannot_mean_what_it_says_is_refused() {
    for (needs, names) in [
        ("network = [\"a.example\"]\nconnect_tcp = [0]", "port 0"),
        ("bind_tcp = [0]", "port 0"),
        (
            "network = [\"a.example\"]\nconnect_tcp = [443, 443]",
            "443 twice",
        ),
        ("bind_tcp = [8080, 8080]", "8080 twice"),
        ("connect_tcp = [443]", "needs.network"),
        // Hosts are where the app reaches out to; once ports are named, no
        // connect port means no outbound TCP, so the host list would promise
        // the operator a grant the sandbox does not install.
        (
            "network = [\"cloud.example.net\"]\nbind_tcp = [8080]",
            "needs.connect_tcp",
        ),
    ] {
        match load_manifest(&with_needs("bad", needs)) {
            Err(ManifestError::Invalid { message, .. }) => assert!(
                message.contains(names),
                "{needs:?} was refused, but the message does not say {names:?}: {message}"
            ),
            other => panic!("{needs:?} must be refused as invalid, got {other:?}"),
        }
    }
    // Not a TCP port at all: toml says so before validation does.
    assert!(matches!(
        load_manifest(&with_needs("bad", "bind_tcp = [70000]")),
        Err(ManifestError::Parse { .. })
    ));
    // The edges that are fine.
    for needs in [
        "bind_tcp = [8080]",
        "network = [\"a.example\"]\nconnect_tcp = [443, 65535]",
        "network = [\"a.example\"]\nconnect_tcp = [443]\nbind_tcp = [443]",
    ] {
        load_manifest(&with_needs("good", needs))
            .unwrap_or_else(|e| panic!("{needs:?} should be accepted: {e}"));
    }
}

#[test]
fn a_listening_port_alone_is_still_something_the_operator_approves() {
    let env = FakeEnv::new();
    install(&env, "dev", &with_needs("dev", "bind_tcp = [8080]"));
    let mut host = host(&env);
    host.refresh();
    match host.open_window("dev") {
        Err(HostError::NotApproved { summary, .. }) => assert!(
            mentions(&summary, &["8080"]),
            "the operator is told which port it listens on: {summary}"
        ),
        other => panic!("a bind grant must need approval, got {other:?}"),
    }
    assert_eq!(env.spawn_count(), 0);
}

#[test]
fn the_approval_summary_names_the_ports() {
    let manifest = load_manifest(&with_needs(
        "pim",
        "network = [\"cloud.example.net\"]\nconnect_tcp = [443, 993]\nbind_tcp = [8080]",
    ))
    .unwrap();
    let summary = apps::summarize(&manifest.needs);
    assert!(
        mentions(&summary, &["cloud.example.net", "443", "993", "8080"]),
        "{summary}"
    );
}

#[test]
fn a_fingerprint_covers_the_port_grants_and_not_their_order() {
    let base = |needs: &str| {
        load_manifest(&format!(
            "{}\n[needs]\n{needs}\nworking_dir = [{{ path = \"Docs\", write = true }}]\n{}",
            manifest_text("pim", ""),
            command_block("pim.event.create", "write")
        ))
        .unwrap()
        .fingerprint()
    };
    let legacy = base("network = [\"cloud.example.net\"]");
    // A manifest naming no ports must hash as if the port fields did not
    // exist (they are skipped when empty). If this moves, every app already
    // approved with a host list is re-asked on upgrade for nothing. Pinned
    // to the SHA-256 scheme that replaced FNV-1a; that switch re-asks once.
    assert_eq!(
        legacy, "sha256:a987f93aaa9164be992ff62c6deb7b0c86cf55fbbd944cc9cf29e05385ce49fd",
        "a manifest naming no ports must hash as it did before ports existed"
    );

    let https = base("network = [\"cloud.example.net\"]\nconnect_tcp = [443, 993]");
    for (changed, what) in [
        (
            base("network = [\"cloud.example.net\"]\nconnect_tcp = [443]"),
            "a connect port added",
        ),
        (https.clone(), "connect ports named"),
        (
            base("network = [\"cloud.example.net\"]\nconnect_tcp = [443, 993]\nbind_tcp = [8080]"),
            "a bind port added",
        ),
        (
            base("network = [\"cloud.example.net\"]\nconnect_tcp = [993]\nbind_tcp = [443]"),
            "a port moved from connect to bind",
        ),
    ] {
        assert_ne!(legacy, changed, "{what} must need approving again");
    }
    assert_ne!(
        https,
        base("network = [\"cloud.example.net\"]\nconnect_tcp = [443, 994]"),
        "a different port must need approving again"
    );
    assert_ne!(
        https,
        base("network = [\"cloud.example.net\"]\nconnect_tcp = [993]\nbind_tcp = [443]"),
        "moving a port between connect and bind must need approving again"
    );
    assert_eq!(
        https,
        base("network = [\"cloud.example.net\"]\nconnect_tcp = [993, 443]"),
        "the same ports in another order are the same grant"
    );
    assert_eq!(
        base("network = [\"cloud.example.net\"]\nconnect_tcp = [443]\nbind_tcp = [8080, 9090]"),
        base("network = [\"cloud.example.net\"]\nconnect_tcp = [443]\nbind_tcp = [9090, 8080]"),
        "the same listening ports in another order are the same grant"
    );
}

// ---------------------------------------------------------------------------
// The install directory's own entries
// ---------------------------------------------------------------------------
//
// The working directory defaults to the install directory, so a manifest's
// `bin` is, by default, the directory holding the unconfined core binary, and
// `apps` holds every other app's binary. Neither the manifest nor the
// filesystem may lead a grant there.

fn plan_in(env: &FakeEnv, name: &str, working_dir: &str) -> Result<LaunchPlan, ConfinementError> {
    let installed = discovered(env, name);
    LaunchPlan::build(
        &installed,
        Path::new(INSTALL),
        Path::new(working_dir),
        Confinement::Enforced,
        env,
    )
}

#[test]
fn a_manifest_naming_an_install_directory_entry_is_refused() {
    for path in [
        "bin",
        "apps",
        "apps/notes",
        "run",
        "keys",
        "cache",
        "cache/apps/other/home",
        "debug-session.cookie",
        "server.log",
        "./bin",
    ] {
        let toml = format!(
            "{}\n[needs]\nworking_dir = [{{ path = \"{path}\", write = true }}]\n",
            manifest_text("pim", "")
        );
        match load_manifest(&toml) {
            Err(ManifestError::Invalid { message, .. }) => assert!(
                message.contains("install directory"),
                "{path}: the operator must be told why: {message}"
            ),
            other => panic!("path {path:?} must be refused, got {other:?}"),
        }
    }

    // The controls: ordinary names, including ones that merely contain or
    // sit below an entry's name somewhere else, and the operator's own
    // `Cache` or `Apps`: on a case-sensitive filesystem those are not the
    // entries, and only the launcher can tell a folding one (see
    // `a_grant_spelling_an_entry_in_another_case_is_refused_where_it_is_that_entry`).
    for path in [
        "Notes",
        "binaries",
        "Docs/bin",
        "Projects/apps",
        "Cache",
        "Apps",
        "BIN",
    ] {
        let toml = format!(
            "{}\n[needs]\nworking_dir = [{{ path = \"{path}\", write = true }}]\n",
            manifest_text("pim", "")
        );
        load_manifest(&toml).unwrap_or_else(|e| panic!("{path:?} must be accepted: {e}"));
    }
}

#[test]
fn a_grant_whose_real_path_is_an_install_directory_entry_is_not_granted() {
    // The working directory is the install directory, as by default, and the
    // manifest is clean: the filesystem leads it into `bin/`.
    let env = FakeEnv::new();
    install(
        &env,
        "pim",
        &format!(
            "{}\n[needs]\nworking_dir = [{{ path = \"Tools\", write = true }}]\n",
            manifest_text("pim", "")
        ),
    );
    env.symlink_dir("/install/Tools", "/install/bin");

    match plan_in(&env, "pim", INSTALL) {
        Err(ConfinementError::InstallDenied { path, denied, .. }) => {
            assert_eq!(path, PathBuf::from("/install/bin"));
            assert_eq!(denied, PathBuf::from("/install/bin"));
        }
        other => panic!("expected a refusal, got {other:?}"),
    }

    // Into another app's directory, the same.
    env.symlink_dir("/install/Tools", "/install/apps/other");
    assert!(
        matches!(
            plan_in(&env, "pim", INSTALL),
            Err(ConfinementError::InstallDenied { .. })
        ),
        "a grant on another app's directory must be refused"
    );
}

#[test]
fn a_grant_containing_the_install_directory_is_refused() {
    // A working directory above the install directory: a grant on the tree
    // holding it reaches every entry through that tree.
    let env = FakeEnv::new();
    install(
        &env,
        "pim",
        &format!(
            "{}\n[needs]\nworking_dir = [{{ path = \"install\" }}]\n",
            manifest_text("pim", "")
        ),
    );

    match plan_in(&env, "pim", "/") {
        Err(ConfinementError::InstallDenied { path, .. }) => {
            assert_eq!(path, PathBuf::from("/install"));
        }
        other => panic!("expected a refusal, got {other:?}"),
    }
}

#[test]
fn an_ordinary_grant_in_the_install_directory_is_still_granted() {
    // The control for the two above: with the working directory being the
    // install directory, the operator's own folders in it are grantable.
    let env = FakeEnv::new();
    install(
        &env,
        "pim",
        &format!(
            "{}\n[needs]\nworking_dir = [{{ path = \"Calendars\", write = true }}]\n",
            manifest_text("pim", "")
        ),
    );

    let plan = plan_in(&env, "pim", INSTALL).expect("an ordinary folder must be granted");
    assert!(
        plan.sandbox
            .policy
            .read_write
            .contains(&PathBuf::from("/install/Calendars")),
        "{:?}",
        plan.sandbox.policy.read_write
    );
}

#[test]
fn a_grant_spelling_an_entry_in_another_case_is_refused_where_it_is_that_entry() {
    // The manifest check lets `BIN` through: on a case-sensitive filesystem
    // it is the operator's folder. On a folding mount it is `bin/` itself,
    // and only the launcher, which can ask the filesystem, can tell.
    let env = FakeEnv::new();
    install(
        &env,
        "pim",
        &format!(
            "{}\n[needs]\nworking_dir = [{{ path = \"BIN\", write = true }}]\n",
            manifest_text("pim", "")
        ),
    );
    env.same_file_as("/install/BIN", "/install/bin");
    match plan_in(&env, "pim", INSTALL) {
        Err(ConfinementError::InstallDenied { path, denied, .. }) => {
            assert_eq!(path, PathBuf::from("/install/BIN"));
            assert_eq!(denied, PathBuf::from("/install/bin"));
        }
        other => panic!("expected a refusal, got {other:?}"),
    }

    // The control: where `Cache` is a folder of its own, it is granted.
    install(
        &env,
        "pim",
        &format!(
            "{}\n[needs]\nworking_dir = [{{ path = \"Cache\", write = true }}]\n",
            manifest_text("pim", "")
        ),
    );
    env.mkdir("/install/cache");
    let plan = plan_in(&env, "pim", INSTALL).expect("the operator's Cache is grantable");
    assert!(
        plan.sandbox
            .policy
            .read_write
            .contains(&PathBuf::from("/install/Cache")),
        "{:?}",
        plan.sandbox.policy.read_write
    );
}

#[test]
fn an_approval_from_before_the_sha256_fingerprint_is_reported_stale_and_can_be_given_again() {
    let env = FakeEnv::new();
    install(
        &env,
        "pim",
        &format!(
            "{}\n[needs]\nnetwork = [\"cloud.example.net\"]\n",
            manifest_text("pim", "")
        ),
    );
    let grants = FakeGrants::default();
    let mut host = host_with(&env, apps::Policy::default(), grants.clone());
    host.refresh();
    assert_eq!(host.approval("pim").unwrap(), apps::ApprovalState::Required);

    host.approve("pim").unwrap();
    assert_eq!(host.approval("pim").unwrap(), apps::ApprovalState::Approved);

    // What a grant recorded by a build that hashed with fnv1a64 looks like.
    grants.stored.lock().unwrap().as_mut().unwrap().fingerprint =
        "fnv1a64:0123456789abcdef".to_string();
    assert_eq!(host.approval("pim").unwrap(), apps::ApprovalState::Stale);
    match host.open_window("pim") {
        Err(e @ HostError::GrantStale { .. }) => {
            assert!(e.to_string().contains("Approve pim"), "{e}");
        }
        other => panic!("expected a stale grant, got {other:?}"),
    }

    host.approve("pim").unwrap();
    assert_eq!(host.approval("pim").unwrap(), apps::ApprovalState::Approved);
}

// ---------------------------------------------------------------------------
// D2: no app grant may reach the core's own front door
// ---------------------------------------------------------------------------

fn host_listening_on(env: &FakeEnv, listen: &str, grants: FakeGrants) -> Host<FakeEnv> {
    host_behind(env, listen, &[], grants)
}

/// A host whose core listens on `listen` and believes `trusted` over TCP.
fn host_behind(
    env: &FakeEnv,
    listen: &str,
    trusted: &[std::net::IpAddr],
    grants: FakeGrants,
) -> Host<FakeEnv> {
    Host::new(
        PathBuf::from(INSTALL),
        PathBuf::from(WORK),
        apps::CoreDoor::new(listen.parse().unwrap(), trusted.to_vec()),
        env.clone(),
        apps::Policy::default(),
        Box::new(grants),
    )
}

/// A grant stored before the rule existed: its fingerprint matches, so only
/// the launch-time check stands between it and a process.
fn grandfathered(env: &FakeEnv, name: &str) -> FakeGrants {
    let installed = discovered(env, name);
    let grants = FakeGrants::default();
    grants
        .record(&Grant {
            app: name.to_string(),
            fingerprint: installed.manifest.fingerprint(),
            needs: installed.manifest.needs.clone(),
            granted_at: 1,
        })
        .unwrap();
    grants
}

#[test]
fn a_grant_of_the_cores_port_is_refused_at_approval_and_at_launch() {
    let env = FakeEnv::new();
    install(
        &env,
        "probe",
        &with_needs(
            "probe",
            "network = [\"a.example\"]\nconnect_tcp = [443, 7878]",
        ),
    );

    // On every listen address: a Landlock port rule reaches every address
    // that serves the port.
    for listen in ["127.0.0.1:7878", "100.64.0.1:7878", "0.0.0.0:7878"] {
        let mut host = host_listening_on(&env, listen, FakeGrants::default());
        host.refresh();
        match host.approve("probe") {
            Err(HostError::ReachesCore {
                app, port, reason, ..
            }) => {
                assert_eq!(app, "probe");
                assert_eq!(port, 7878);
                assert!(reason.contains("7878"), "{reason}");
            }
            other => panic!("{listen}: approving the core's port must be refused, got {other:?}"),
        }

        let mut host = host_listening_on(&env, listen, grandfathered(&env, "probe"));
        host.refresh();
        let opened = host.open_window("probe");
        assert!(
            matches!(opened, Err(HostError::ReachesCore { .. })),
            "{listen}: {opened:?}"
        );
    }
    assert_eq!(
        env.spawn_count(),
        0,
        "nothing granted the core's port was spawned"
    );

    // Control: the same app on a core listening on another port launches.
    let mut host = host_listening_on(&env, "127.0.0.1:9999", FakeGrants::default());
    host.refresh();
    host.approve("probe")
        .expect("another port is not the core's");
    host.open_window("probe").expect("and it launches");
    assert_eq!(env.spawn_count(), 1);
}

#[test]
fn a_whole_network_grant_is_refused_while_the_core_listens_on_loopback() {
    let env = FakeEnv::new();
    install(
        &env,
        "pim",
        &with_needs("pim", "network = [\"cloud.example.net\"]"),
    );

    // Loopback, and the unspecified address, which accepts loopback too.
    for listen in ["127.0.0.1:7878", "[::1]:7878", "0.0.0.0:7878", "[::]:7878"] {
        let mut host = host_listening_on(&env, listen, FakeGrants::default());
        host.refresh();
        assert!(
            matches!(host.approve("pim"), Err(HostError::ReachesCore { .. })),
            "{listen}: a whole-network grant must not be approvable"
        );
        let mut host = host_listening_on(&env, listen, grandfathered(&env, "pim"));
        host.refresh();
        let opened = host.open_window("pim");
        match &opened {
            Err(e @ HostError::ReachesCore { .. }) => {
                // The operator is told what to change.
                assert!(e.to_string().contains("connect_tcp"), "{e}");
            }
            other => panic!("{listen}: expected a refusal, got {other:?}"),
        }
    }
    assert_eq!(env.spawn_count(), 0);

    // Control: a core bound only to its tailnet address, believing nothing
    // over TCP (the server.unix_socket setup), is not reachable from any
    // trusted address.
    let mut host = host_listening_on(&env, "100.64.0.1:7878", FakeGrants::default());
    host.refresh();
    host.approve("pim")
        .expect("not reachable from a trusted address");
    host.open_window("pim").expect("launches");
    assert_eq!(env.spawn_count(), 1);
}

#[test]
fn a_whole_network_grant_is_refused_while_any_address_is_trusted_over_tcp() {
    // A TCP `tailscale serve` pointed at the node's own tailnet address
    // arrives from that address, so the operator trusts it. An app under no
    // TCP rule can connect to the core from that same address and be
    // believed. Loopback and IPv4-mapped loopback are the same case.
    let env = FakeEnv::new();
    install(
        &env,
        "pim",
        &with_needs("pim", "network = [\"cloud.example.net\"]"),
    );
    for trusted in ["100.64.0.1", "127.0.0.1", "::ffff:127.0.0.1"] {
        let trusted: std::net::IpAddr = trusted.parse().unwrap();
        let mut host = host_behind(&env, "100.64.0.1:7878", &[trusted], FakeGrants::default());
        host.refresh();
        assert!(
            matches!(host.approve("pim"), Err(HostError::ReachesCore { .. })),
            "trusting {trusted}: a whole-network grant must not be approvable"
        );
        let mut host = host_behind(
            &env,
            "100.64.0.1:7878",
            &[trusted],
            grandfathered(&env, "pim"),
        );
        host.refresh();
        let opened = host.open_window("pim");
        match &opened {
            Err(e @ HostError::ReachesCore { .. }) => {
                assert!(e.to_string().contains("trusted_proxies"), "{e}");
            }
            other => panic!("trusting {trusted}: expected a refusal, got {other:?}"),
        }
    }
    assert_eq!(env.spawn_count(), 0);

    // Control: naming ports puts the app under Landlock's port rules, and a
    // port that is not the core's cannot reach it from any address.
    install(
        &env,
        "mail",
        &with_needs(
            "mail",
            "network = [\"imap.example.net\"]\nconnect_tcp = [993]",
        ),
    );
    let trusted: std::net::IpAddr = "100.64.0.1".parse().unwrap();
    let mut host = host_behind(&env, "100.64.0.1:7878", &[trusted], FakeGrants::default());
    host.refresh();
    host.approve("mail")
        .expect("ports other than the core's are fine");
    host.open_window("mail").expect("launches");
    assert_eq!(env.spawn_count(), 1);
}

#[test]
fn an_ipv4_mapped_loopback_listen_address_is_loopback() {
    let needs = apps::Needs {
        network: vec!["cloud.example.net".to_string()],
        ..apps::Needs::default()
    };
    assert!(
        apps::reaches_core(
            &needs,
            &apps::CoreDoor::new("[::ffff:127.0.0.1]:7878".parse().unwrap(), vec![])
        )
        .is_some()
    );
}

#[test]
fn naming_ports_that_are_not_the_cores_is_allowed_on_loopback() {
    let env = FakeEnv::new();
    install(
        &env,
        "pim",
        &with_needs(
            "pim",
            "network = [\"cloud.example.net\"]\nconnect_tcp = [443, 993]",
        ),
    );
    let mut host = host_listening_on(&env, "127.0.0.1:7878", FakeGrants::default());
    host.refresh();
    host.approve("pim")
        .expect("ports other than the core's are fine");
    host.open_window("pim").expect("launches");
    assert_eq!(env.spawn_count(), 1);

    // And the pure rule agrees, for the manifest the validator cannot judge.
    let needs = discovered(&env, "pim").manifest.needs;
    assert_eq!(
        apps::reaches_core(
            &needs,
            &apps::CoreDoor::new("127.0.0.1:7878".parse().unwrap(), vec![])
        ),
        None
    );
    assert!(
        apps::reaches_core(
            &needs,
            &apps::CoreDoor::new("127.0.0.1:993".parse().unwrap(), vec![])
        )
        .is_some()
    );
}

// ---------------------------------------------------------------------------
// The window's scope: `[ui]` (docs/APP-UI-ISOLATION.md §4.6, §4.8)
// ---------------------------------------------------------------------------

fn with_ui(name: &str, invoke: &[&str]) -> String {
    let list: Vec<String> = invoke.iter().map(|i| format!("{i:?}")).collect();
    format!(
        "{}\n[ui]\ninvoke = [{}]\n",
        manifest_text(name, ""),
        list.join(", ")
    )
}

#[test]
fn a_window_that_reaches_beyond_its_own_commands_is_not_launched_unapproved() {
    let env = FakeEnv::new();
    install(&env, "clock", &manifest_text("clock", ""));
    install(&env, "filer", &with_ui("filer", &["files.list"]));
    let mut host = host(&env);
    host.refresh();

    host.open_window("clock")
        .expect("the control: an app that asks for nothing launches");
    match host.open_window("filer") {
        Err(HostError::NotApproved { summary, .. }) => {
            assert!(summary.contains("files.list"), "{summary}")
        }
        other => panic!("expected a refusal, got {other:?}"),
    }
    assert_eq!(env.spawn_count(), 1, "only the control was started");
}

#[test]
fn changing_only_the_window_scope_blocks_launch_until_approved_again() {
    let env = FakeEnv::new();
    install(&env, "filer", &with_ui("filer", &["files.list"]));
    let mut host = host(&env);
    host.refresh();
    host.approve("filer").unwrap();
    host.open_window("filer")
        .expect("the control: approved, it runs");
    assert_eq!(env.spawn_count(), 1);
    host.stop("filer", false);

    // Nothing else changes: same needs, same commands, one more id in [ui].
    install(
        &env,
        "filer",
        &with_ui("filer", &["files.list", "files.trash"]),
    );
    host.refresh();
    match host.open_window("filer") {
        Err(HostError::GrantStale { summary, .. }) => {
            assert!(summary.contains("files.trash"), "{summary}")
        }
        other => panic!("expected a stale grant, got {other:?}"),
    }
    assert_eq!(env.spawn_count(), 1, "nothing ran on the old approval");

    host.approve("filer").unwrap();
    host.open_window("filer").expect("approved again, it runs");
    assert_eq!(env.spawn_count(), 2);
}

#[test]
fn the_approved_window_scope_is_the_stored_one_and_only_while_it_matches() {
    let env = FakeEnv::new();
    install(&env, "filer", &with_ui("filer", &["files.list"]));
    let mut host = host(&env);
    host.refresh();
    assert!(
        host.approved_ui_invoke("filer").is_empty(),
        "declared is not approved"
    );

    host.approve("filer").unwrap();
    assert_eq!(host.approved_ui_invoke("filer"), ["files.list"]);

    // The manifest on disk grows; the approval does not follow it.
    install(
        &env,
        "filer",
        &with_ui("filer", &["files.list", "files.read"]),
    );
    host.refresh();
    assert!(
        host.approved_ui_invoke("filer").is_empty(),
        "a manifest changed after approval is a request, not a grant"
    );
    assert!(host.approved_ui_invoke("ghost").is_empty());
}

struct KnownRisks;

impl apps::CommandRisks for KnownRisks {
    fn risk(&self, command: &str) -> Option<deckjack_core::command::RiskLevel> {
        use deckjack_core::command::RiskLevel;
        match command {
            "files.list" => Some(RiskLevel::ReadOnly),
            "files.write" => Some(RiskLevel::Write),
            "files.delete" => Some(RiskLevel::Destructive),
            _ => None,
        }
    }
}

#[test]
fn the_summary_leads_with_the_riskiest_thing_the_window_can_do() {
    let manifest = load_manifest(&with_ui(
        "probe",
        &["files.list", "files.write", "files.delete"],
    ))
    .unwrap();
    let summary = apps::summarize_with(&manifest.needs, &KnownRisks);
    let at = |needle: &str| {
        summary
            .find(needle)
            .unwrap_or_else(|| panic!("{needle:?} missing from {summary:?}"))
    };
    assert!(summary.contains("on your behalf"), "{summary}");
    assert!(at("files.delete") < at("files.write"), "{summary}");
    assert!(at("files.write") < at("files.list"), "{summary}");
    assert!(
        summary.contains("remove or overwrite data (files.delete)"),
        "{summary}"
    );

    // A command whose risk is unknown is not presented as harmless: it leads.
    let manifest = load_manifest(&with_ui("probe", &["files.list", "files.mystery"])).unwrap();
    let summary = apps::summarize_with(&manifest.needs, &KnownRisks);
    assert!(
        summary.find("files.mystery").unwrap() < summary.find("files.list").unwrap(),
        "{summary}"
    );

    // Without a registry to ask (the `deckjack install` CLI), the ids are
    // still all there, and still not presented as harmless.
    let summary = apps::summarize(&manifest.needs);
    assert!(
        mentions(&summary, &["files.list", "files.mystery"]),
        "{summary}"
    );
    assert!(summary.contains("does not know"), "{summary}");
}
