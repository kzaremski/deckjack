//! `deckjack install`, from the outside, against a real filesystem.
//!
//! The claims these tests defend, in order of what would hurt most:
//!
//! 1. An install never widens what the operator approved: an update whose
//!    needs changed is refused at launch until it is approved again, by the
//!    same approval model the host already enforces.
//! 2. Nothing lands that the core would refuse, and a refused install leaves
//!    the previous version exactly as it was.
//! 3. The swap is safe while the old binary is running.
//! 4. The CLI really calls all of this (the "built but never constructed"
//!    defect class).

use std::fs;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};

use deckjack_core::apps::{self, Grant, GrantError, Grants, Host, HostError};
use deckjack_core::install::{
    self, Approval, Because, ElfError, InstallError, Machine, Source, check_elf,
};
use deckjack_core::store::{Store, StoreGrants};

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

/// A small ELF executable for this machine. The install checks the header,
/// so the binary has to be genuine; `true` is on every Linux.
fn real_binary(name: &str) -> PathBuf {
    for dir in ["/usr/bin", "/bin"] {
        let path = Path::new(dir).join(name);
        if path.is_file() {
            return path;
        }
    }
    panic!("no {name} binary on this machine to use as a fixture");
}

fn manifest_text(name: &str, version: &str, needs: &str) -> String {
    format!(
        "[app]\nname = \"{name}\"\nversion = \"{version}\"\nprotocol = {}\n\n\
         [[commands]]\nid = \"{name}.list\"\nsummary = \"List things\"\nrisk = \"read\"\n{needs}",
        apps::PROTOCOL_VERSION
    )
}

/// A built app directory, the shape `deckjack install` takes.
fn built_app(root: &Path, dir: &str, name: &str, manifest: &str, binary: &[u8]) -> PathBuf {
    let dir = root.join(dir);
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("app.toml"), manifest).unwrap();
    let bin = dir.join(format!("deckjack-{name}"));
    fs::write(&bin, binary).unwrap();
    fs::set_permissions(&bin, fs::Permissions::from_mode(0o755)).unwrap();
    dir
}

fn true_bytes() -> Vec<u8> {
    fs::read(real_binary("true")).unwrap()
}

fn source(dir: &Path) -> Source {
    Source::from_dir(dir, None).unwrap()
}

/// A 64-byte ELF header with the given identity.
fn elf_header(machine: u16, class: u8, data: u8, e_type: u16) -> Vec<u8> {
    let mut h = vec![0u8; 64];
    h[..4].copy_from_slice(&[0x7f, b'E', b'L', b'F']);
    h[4] = class;
    h[5] = data;
    h[6] = 1;
    let (t, m) = if data == 2 {
        (e_type.to_be_bytes(), machine.to_be_bytes())
    } else {
        (e_type.to_le_bytes(), machine.to_le_bytes())
    };
    h[16..18].copy_from_slice(&t);
    h[18..20].copy_from_slice(&m);
    h
}

/// An architecture that is not this one, and its name.
fn foreign_machine() -> (u16, &'static str) {
    if Machine::this().machine == 62 {
        (183, "aarch64")
    } else {
        (62, "x86_64")
    }
}

/// Spawn a binary this process just wrote. Another test thread forking at
/// the moment the file was open for writing holds that descriptor until its
/// own exec, and the kernel refuses to run a file open for writing ("text
/// file busy"). That is the test harness, not the install, so wait it out.
fn spawn_retrying(command: &mut Command) -> std::process::Child {
    for _ in 0..100 {
        match command.spawn() {
            Ok(child) => return child,
            Err(e) if e.kind() == std::io::ErrorKind::ExecutableFileBusy => {
                std::thread::sleep(std::time::Duration::from_millis(10))
            }
            Err(e) => panic!("cannot spawn {command:?}: {e}"),
        }
    }
    panic!("{command:?} stayed busy");
}

/// Approvals in the working directory's real state database: where the
/// server records them and where `install` reads them to say whether an app
/// will run. Cloned handles share one connection, as the server's do.
#[derive(Clone)]
struct StoredGrants {
    store: Arc<Mutex<Store>>,
}

impl StoredGrants {
    fn open(work: &Path) -> StoredGrants {
        StoredGrants {
            store: Arc::new(Mutex::new(Store::open(work).unwrap())),
        }
    }
}

impl Grants for StoredGrants {
    fn grant(&self, app: &str) -> Result<Option<Grant>, GrantError> {
        StoreGrants::new(Arc::clone(&self.store)).grant(app)
    }
    fn record(&self, grant: &Grant) -> Result<(), GrantError> {
        StoreGrants::new(Arc::clone(&self.store)).record(grant)
    }
    fn revoke(&self, app: &str) -> Result<(), GrantError> {
        StoreGrants::new(Arc::clone(&self.store)).revoke(app)
    }
}

/// A host over this install, and the receivers standing in for the core's
/// attendants: a launch hands the core its end of the app's link and is
/// refused when nothing is waiting for it. Hold the receivers while the
/// host launches.
fn host(
    install_dir: &Path,
    work: &Path,
    grants: StoredGrants,
) -> (Host<apps::OsEnv>, Vec<apps::LinkReceiver>) {
    let policy = apps::Policy {
        // The launch itself is not what these tests are about, and an
        // unconfined `true` exits harmlessly.
        confinement: apps::Confinement::unconfined_for_development(),
        ..apps::Policy::default()
    };
    let env = apps::OsEnv::new();
    let links = apps::discover(install_dir, &env)
        .apps
        .iter()
        .map(|app| env.links().attend(&app.name))
        .collect();
    let host = Host::new(
        install_dir.to_path_buf(),
        work.to_path_buf(),
        // A core on its tailnet address: these manifests use the legacy
        // whole-network grant, which a core on loopback refuses (D2, tested in
        // tests/apps.rs). What is tested here is approval across updates.
        apps::CoreDoor::new("100.64.0.1:7878".parse().unwrap(), vec![]),
        env,
        policy,
        Box::new(grants),
    );
    (host, links)
}

const NEEDS_NETWORK: &str = "\n[needs]\nnetwork = [\"cloud.example.net\"]\n";
const NEEDS_MORE: &str = "\n[needs]\nnetwork = [\"cloud.example.net\", \"ads.example.com\"]\n";

// ---------------------------------------------------------------------------
// Installing
// ---------------------------------------------------------------------------

#[test]
fn an_installed_app_is_one_discovery_will_run() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    // The source directory's name is irrelevant; the manifest names the app.
    let dir = built_app(
        tmp.path(),
        "build-output",
        "clock",
        &manifest_text("clock", "1.0.0", ""),
        &true_bytes(),
    );

    let report = install::install(&home, &source(&dir)).expect("a valid app installs");
    assert_eq!(report.name, "clock");
    assert_eq!(report.dir, home.join("apps/clock"));
    assert_eq!(report.approval, Approval::NotNeeded);

    let found = apps::discover(&home, &apps::OsEnv::new());
    let problems: Vec<String> = found.problems.iter().map(|p| p.error.to_string()).collect();
    assert!(problems.is_empty(), "{problems:?}");
    let names: Vec<&str> = found.apps.iter().map(|a| a.name.as_str()).collect();
    assert_eq!(names, ["clock"]);
    assert_eq!(
        fs::read(home.join("apps/clock/deckjack-clock")).unwrap(),
        true_bytes()
    );
    // Nothing staged is left behind beside the apps.
    let entries: Vec<String> = fs::read_dir(home.join("apps"))
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(entries, ["clock"]);
}

#[test]
fn a_new_app_that_asks_for_something_reports_that_it_needs_approval() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let dir = built_app(
        tmp.path(),
        "src",
        "pim",
        &manifest_text("pim", "1.0.0", NEEDS_NETWORK),
        &true_bytes(),
    );
    // Named explicitly, as `--dir` does: a working directory where no server
    // has recorded anything yet has approved nothing.
    let report = install::install_in(&home, &home, &source(&dir)).unwrap();
    match &report.approval {
        Approval::Required { because, summary } => {
            assert_eq!(*because, Because::NewApp);
            assert!(summary.contains("cloud.example.net"), "{summary}");
        }
        other => panic!("expected approval to be required, got {other:?}"),
    }
    assert!(
        report.render().contains("must approve"),
        "{}",
        report.render()
    );
}

#[test]
fn an_update_that_asks_for_more_is_not_covered_by_the_old_approval() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let work = tmp.path().join("work");
    fs::create_dir_all(&work).unwrap();

    let v1 = built_app(
        tmp.path(),
        "v1",
        "pim",
        &manifest_text("pim", "1.0.0", NEEDS_NETWORK),
        &true_bytes(),
    );
    install::install_in(&home, &work, &source(&v1)).unwrap();

    // The operator approves version 1.
    let grants = StoredGrants::open(&work);
    let (mut before, _links) = host(&home, &work, grants.clone());
    before.refresh();
    before.approve("pim").unwrap();

    let v2 = built_app(
        tmp.path(),
        "v2",
        "pim",
        &manifest_text("pim", "2.0.0", NEEDS_MORE),
        &true_bytes(),
    );
    let report = install::install_in(&home, &work, &source(&v2)).unwrap();
    assert_eq!(report.previous_version.as_deref(), Some("1.0.0"));
    match &report.approval {
        Approval::Required { because, summary } => {
            assert_eq!(*because, Because::Changed);
            assert!(summary.contains("ads.example.com"), "{summary}");
        }
        other => panic!("expected re-approval, got {other:?}"),
    }

    // And the host, reading what the install put on disk with the grant the
    // operator gave version 1, refuses to launch it.
    let (mut after, _links) = host(&home, &work, grants);
    after.refresh();
    match after.open_window("pim") {
        Err(HostError::GrantStale { summary, .. }) => {
            assert!(summary.contains("ads.example.com"), "{summary}")
        }
        other => panic!("expected a stale grant, got {other:?}"),
    }
}

#[test]
fn an_update_that_asks_for_the_same_keeps_its_approval() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let work = tmp.path().join("work");
    fs::create_dir_all(&work).unwrap();

    let v1 = built_app(
        tmp.path(),
        "v1",
        "pim",
        &manifest_text("pim", "1.0.0", NEEDS_NETWORK),
        &true_bytes(),
    );
    install::install_in(&home, &work, &source(&v1)).unwrap();
    let grants = StoredGrants::open(&work);
    let (mut before, _links) = host(&home, &work, grants.clone());
    before.refresh();
    before.approve("pim").unwrap();

    let v2 = built_app(
        tmp.path(),
        "v2",
        "pim",
        &manifest_text("pim", "1.0.1", NEEDS_NETWORK),
        &true_bytes(),
    );
    let report = install::install_in(&home, &work, &source(&v2)).unwrap();
    assert_eq!(report.approval, Approval::Approved);

    let (mut after, _links) = host(&home, &work, grants);
    after.refresh();
    after
        .open_window("pim")
        .expect("the approval given to 1.0.0 covers 1.0.1, which asks for the same");
    after.stop_all();
}

#[test]
fn the_swap_does_not_disturb_the_running_old_binary() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let sleep = fs::read(real_binary("sleep")).unwrap();
    let v1 = built_app(
        tmp.path(),
        "v1",
        "clock",
        &manifest_text("clock", "1.0.0", ""),
        &sleep,
    );
    install::install(&home, &source(&v1)).unwrap();

    let installed = home.join("apps/clock/deckjack-clock");
    let mut running = spawn_retrying(Command::new(&installed).arg("30"));

    let v2 = built_app(
        tmp.path(),
        "v2",
        "clock",
        &manifest_text("clock", "2.0.0", ""),
        &true_bytes(),
    );
    let swapped = install::install(&home, &source(&v2));
    let still_running = running.try_wait().unwrap().is_none();
    // Killed by the handle this test spawned, before any assertion can fail.
    running.kill().unwrap();
    running.wait().unwrap();

    let report = swapped.expect("replacing a running binary succeeds");
    assert_eq!(report.previous_version.as_deref(), Some("1.0.0"));
    assert!(
        still_running,
        "the old process survived the swap of its own binary"
    );
    assert_eq!(fs::read(&installed).unwrap(), true_bytes());
    assert!(
        fs::read_to_string(home.join("apps/clock/app.toml"))
            .unwrap()
            .contains("2.0.0")
    );
}

// ---------------------------------------------------------------------------
// Refusals
// ---------------------------------------------------------------------------

#[test]
fn a_binary_for_another_architecture_is_refused_naming_both() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let (machine, name) = foreign_machine();
    let dir = built_app(
        tmp.path(),
        "src",
        "clock",
        &manifest_text("clock", "1.0.0", ""),
        &elf_header(machine, 2, 1, 3),
    );

    let err = install::install(&home, &source(&dir)).unwrap_err();
    let text = err.to_string();
    assert!(
        matches!(err, InstallError::Binary(ElfError::WrongArch { .. })),
        "{err:?}"
    );
    assert!(text.contains(name), "{text}");
    assert!(text.contains(&Machine::this().describe()), "{text}");
    assert!(!home.join("apps/clock").exists());
}

#[test]
fn a_refused_update_leaves_the_installed_version_as_it_was() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let v1 = built_app(
        tmp.path(),
        "v1",
        "clock",
        &manifest_text("clock", "1.0.0", ""),
        &true_bytes(),
    );
    install::install(&home, &source(&v1)).unwrap();

    let v2 = built_app(
        tmp.path(),
        "v2",
        "clock",
        &manifest_text("clock", "2.0.0", ""),
        b"#!/bin/sh\necho not elf\n",
    );
    let err = install::install(&home, &source(&v2)).unwrap_err();
    assert!(
        matches!(err, InstallError::Binary(ElfError::NotElf { .. })),
        "{err:?}"
    );

    assert!(
        fs::read_to_string(home.join("apps/clock/app.toml"))
            .unwrap()
            .contains("1.0.0")
    );
    assert_eq!(
        fs::read(home.join("apps/clock/deckjack-clock")).unwrap(),
        true_bytes()
    );
    let entries: Vec<String> = fs::read_dir(home.join("apps"))
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(entries, ["clock"], "the refused staging copy is gone");
}

#[test]
fn a_manifest_the_protocol_refuses_installs_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");

    // A name that would escape apps/.
    let escape = built_app(
        tmp.path(),
        "escape",
        "x",
        &manifest_text("../../evil", "1.0.0", ""),
        &true_bytes(),
    );
    let err = install::install(&home, &source_unchecked(&escape, "x")).unwrap_err();
    assert!(matches!(err, InstallError::Manifest(_)), "{err:?}");

    // A protocol this core does not speak.
    let future = built_app(
        tmp.path(),
        "future",
        "clock",
        &format!(
            "[app]\nname = \"clock\"\nversion = \"1.0.0\"\nprotocol = {}\n",
            apps::PROTOCOL_VERSION + 1
        ),
        &true_bytes(),
    );
    let err = install::install(&home, &source_unchecked(&future, "clock")).unwrap_err();
    assert!(matches!(err, InstallError::Protocol(_)), "{err:?}");

    assert!(!tmp.path().join("evil").exists());
    assert!(!home.join("apps/clock").exists());
}

fn source_unchecked(dir: &Path, name: &str) -> Source {
    Source {
        manifest: dir.join("app.toml"),
        binary: dir.join(format!("deckjack-{name}")),
    }
}

#[test]
fn uninstall_refuses_a_name_that_would_escape_apps() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let victim = home.join("precious");
    fs::create_dir_all(&victim).unwrap();
    fs::write(victim.join("app.toml"), "keep me").unwrap();
    fs::create_dir_all(home.join("apps")).unwrap();

    for name in ["../precious", "..", ".staging", "/etc", "a/b"] {
        let err = install::uninstall(&home, name).unwrap_err();
        assert!(
            matches!(err, InstallError::BadName { .. }),
            "{name}: {err:?}"
        );
    }
    assert!(victim.join("app.toml").exists());
}

#[test]
fn uninstall_removes_an_installed_app_and_nothing_else() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    for name in ["clock", "notes"] {
        let dir = built_app(
            tmp.path(),
            name,
            name,
            &manifest_text(name, "1.0.0", ""),
            &true_bytes(),
        );
        install::install(&home, &source(&dir)).unwrap();
    }
    install::uninstall(&home, "clock").unwrap();

    let found = apps::discover(&home, &apps::OsEnv::new());
    let names: Vec<&str> = found.apps.iter().map(|a| a.name.as_str()).collect();
    assert_eq!(names, ["notes"]);
    assert!(found.problems.is_empty());
    assert!(matches!(
        install::uninstall(&home, "clock"),
        Err(InstallError::NotInstalled { .. })
    ));
}

#[test]
fn a_stray_file_where_the_app_would_go_is_not_replaced() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    fs::create_dir_all(home.join("apps")).unwrap();
    fs::write(home.join("apps/clock"), "the operator's file").unwrap();
    let dir = built_app(
        tmp.path(),
        "src",
        "clock",
        &manifest_text("clock", "1.0.0", ""),
        &true_bytes(),
    );
    let err = install::install(&home, &source(&dir)).unwrap_err();
    assert!(matches!(err, InstallError::InTheWay { .. }), "{err:?}");
    assert_eq!(
        fs::read_to_string(home.join("apps/clock")).unwrap(),
        "the operator's file"
    );
}

// ---------------------------------------------------------------------------
// The ELF check
// ---------------------------------------------------------------------------

#[test]
fn the_elf_check_accepts_this_machine_and_refuses_every_other_identity() {
    let this = Machine::this();
    let path = Path::new("deckjack-x");
    // Both an executable and a position-independent executable.
    assert_eq!(
        check_elf(
            path,
            &elf_header(this.machine, this.class, this.data, 2),
            this
        ),
        Ok(())
    );
    assert_eq!(
        check_elf(
            path,
            &elf_header(this.machine, this.class, this.data, 3),
            this
        ),
        Ok(())
    );

    let wrong_class = if this.class == 2 { 1 } else { 2 };
    let wrong_data = if this.data == 1 { 2 } else { 1 };
    let (foreign, _) = foreign_machine();
    for header in [
        elf_header(foreign, this.class, this.data, 3),
        elf_header(this.machine, wrong_class, this.data, 3),
        elf_header(this.machine, this.class, wrong_data, 3),
    ] {
        assert!(
            matches!(
                check_elf(path, &header, this),
                Err(ElfError::WrongArch { .. })
            ),
            "{header:?}"
        );
    }

    // An object file is not something to run.
    assert!(matches!(
        check_elf(
            path,
            &elf_header(this.machine, this.class, this.data, 1),
            this
        ),
        Err(ElfError::NotExecutable { .. })
    ));
    // Truncated or not ELF at all.
    assert!(matches!(
        check_elf(
            path,
            &elf_header(this.machine, this.class, this.data, 3)[..10],
            this
        ),
        Err(ElfError::NotElf { .. })
    ));
    assert!(matches!(
        check_elf(path, b"#!/bin/sh\nexit 0\n\0\0\0\0\0\0", this),
        Err(ElfError::NotElf { .. })
    ));
}

#[test]
fn this_machine_is_the_one_the_test_binary_was_built_for() {
    // The test binary was built by the same compiler for the same target, so
    // its own header is the ground truth for `Machine::this`.
    let me = std::env::current_exe().unwrap();
    let header = fs::read(&me).unwrap();
    assert_eq!(check_elf(&me, &header[..64], Machine::this()), Ok(()));
}

// ---------------------------------------------------------------------------
// The CLI
// ---------------------------------------------------------------------------

fn deckjack(home: &Path, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_deckjack"))
        .args(args)
        .env("DECKJACK_HOME", home)
        .env_remove("RUST_LOG")
        .output()
        .unwrap()
}

#[test]
fn the_cli_installs_lists_and_uninstalls_through_the_same_code() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let dir = built_app(
        tmp.path(),
        "src",
        "pim",
        &manifest_text("pim", "1.0.0", NEEDS_NETWORK),
        &true_bytes(),
    );

    // A server has run in the install directory (the default working
    // directory) and approved nothing, so the report is definite.
    fs::create_dir_all(&home).unwrap();
    drop(Store::open(&home).unwrap());
    let out = deckjack(&home, &["install", dir.to_str().unwrap()]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "{stdout}{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(stdout.contains("installed pim 1.0.0"), "{stdout}");
    assert!(stdout.contains("must approve"), "{stdout}");
    assert!(home.join("apps/pim/deckjack-pim").is_file());

    let out = deckjack(&home, &["apps"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success());
    assert!(stdout.contains("pim 1.0.0"), "{stdout}");
    assert!(stdout.contains("cloud.example.net"), "{stdout}");

    let out = deckjack(&home, &["uninstall", "pim"]);
    assert!(out.status.success());
    assert!(!home.join("apps/pim").exists());
}

#[test]
fn the_cli_install_takes_the_binary_from_elsewhere_and_fails_loudly() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    // The repository's layout: the manifest in the crate, the binary in
    // target/. Only the manifest is in the directory given.
    let crate_dir = tmp.path().join("crates/deckjack-clock");
    fs::create_dir_all(&crate_dir).unwrap();
    fs::write(
        crate_dir.join("app.toml"),
        manifest_text("clock", "1.0.0", ""),
    )
    .unwrap();

    let out = deckjack(
        &home,
        &[
            "install",
            crate_dir.to_str().unwrap(),
            "--binary",
            real_binary("true").to_str().unwrap(),
        ],
    );
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        fs::read(home.join("apps/clock/deckjack-clock")).unwrap(),
        true_bytes()
    );

    // Without --binary there is nothing beside the manifest, and the error
    // says what to do about it.
    let out = deckjack(&home, &["install", crate_dir.to_str().unwrap()]);
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("--binary"), "{stderr}");

    // A foreign binary: a failing exit, and the reason on stderr.
    let (machine, name) = foreign_machine();
    let foreign = tmp.path().join("foreign");
    fs::write(&foreign, elf_header(machine, 2, 1, 3)).unwrap();
    let out = deckjack(
        &home,
        &[
            "install",
            crate_dir.to_str().unwrap(),
            "--binary",
            foreign.to_str().unwrap(),
        ],
    );
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains(name), "{stderr}");
}

// ---------------------------------------------------------------------------
// Review fixes: each test below was red before its fix
// ---------------------------------------------------------------------------

#[test]
fn the_approval_fingerprint_is_a_sha256_digest_of_what_was_approved() {
    // Install reports `Unchanged`, and the host launches on an old grant,
    // when two fingerprints are equal. An app author writes both manifests,
    // so equality has to mean "the same request", which a 64-bit
    // non-cryptographic hash cannot promise: a colliding pair costs about
    // 2^32 tries. The expected value was computed outside Rust, with
    // `printf '%s' '<canonical JSON>' | sha256sum`, over
    // {"commands":[["pim.list","read"]],"needs":{"gpu":false,
    // "network":["cloud.example.net"],"working_dir":[]}}.
    let manifest = apps::Manifest::parse(
        Path::new("app.toml"),
        &manifest_text("pim", "1.0.0", NEEDS_NETWORK),
    )
    .unwrap();
    assert_eq!(
        manifest.fingerprint(),
        "sha256:60b789c37eba2fed5ca5b35b447173613f35f3432ed8e1016915f3b71a17203d"
    );
}

#[test]
fn a_directory_that_is_not_an_app_is_never_replaced_by_an_install() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let theirs = home.join("apps/notes");
    fs::create_dir_all(&theirs).unwrap();
    fs::write(theirs.join("my-notes.txt"), "precious").unwrap();
    let dir = built_app(
        tmp.path(),
        "src",
        "notes",
        &manifest_text("notes", "1.0.0", ""),
        &true_bytes(),
    );

    let err = install::install(&home, &source(&dir)).unwrap_err();
    assert!(matches!(err, InstallError::InTheWay { .. }), "{err:?}");
    assert_eq!(
        fs::read_to_string(theirs.join("my-notes.txt")).unwrap(),
        "precious"
    );
    assert!(!theirs.join("app.toml").exists());
}

#[test]
fn an_update_over_a_broken_install_is_judged_by_the_stored_approval() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let broken = home.join("apps/pim");
    fs::create_dir_all(&broken).unwrap();
    fs::write(broken.join("app.toml"), "this is not a manifest").unwrap();
    let dir = built_app(
        tmp.path(),
        "src",
        "pim",
        &manifest_text("pim", "1.0.0", NEEDS_NETWORK),
        &true_bytes(),
    );

    let report = install::install_in(&home, &home, &source(&dir)).unwrap();
    assert_eq!(report.previous_version, None);
    match &report.approval {
        Approval::Required { because, .. } => {
            assert_eq!(*because, Because::NotApproved)
        }
        other => panic!("expected approval to be required, got {other:?}"),
    }
    let text = report.render();
    assert!(text.contains("never been approved"), "{text}");
}

#[test]
fn uninstall_refuses_what_is_not_an_app_and_deletes_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let theirs = home.join("apps/clock");
    fs::create_dir_all(&theirs).unwrap();
    fs::write(theirs.join("my-notes.txt"), "precious").unwrap();
    fs::write(home.join("apps/notes"), "a stray file").unwrap();

    for name in ["clock", "notes"] {
        let err = install::uninstall(&home, name).unwrap_err();
        assert!(
            matches!(err, InstallError::InTheWay { .. }),
            "{name}: {err:?}"
        );
    }
    assert_eq!(
        fs::read_to_string(theirs.join("my-notes.txt")).unwrap(),
        "precious"
    );
    assert_eq!(
        fs::read_to_string(home.join("apps/notes")).unwrap(),
        "a stray file"
    );
}

#[test]
fn an_update_never_shows_a_missing_or_mixed_app_to_a_reader() {
    // A reader (a server launching the app) opens apps/<name>/ at any
    // instant during updates. It must always find an app, and the manifest
    // and binary it finds through one open directory must be one version.
    // Opening the directory once and reading through /proc/self/fd is what
    // "one directory" means here: the files resolve against the inode that
    // was opened, not against the name.
    use std::sync::atomic::{AtomicBool, Ordering};

    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let small = true_bytes();
    let large = fs::read(real_binary("sleep")).unwrap();
    assert_ne!(small.len(), large.len());
    let v1 = built_app(
        tmp.path(),
        "v1",
        "clock",
        &manifest_text("clock", "1.0.0", ""),
        &small,
    );
    let v2 = built_app(
        tmp.path(),
        "v2",
        "clock",
        &manifest_text("clock", "2.0.0", ""),
        &large,
    );
    install::install(&home, &source(&v1)).unwrap();

    let target = home.join("apps/clock");
    let done = Arc::new(AtomicBool::new(false));
    let reader = {
        let done = done.clone();
        let (small, large) = (small.len() as u64, large.len() as u64);
        std::thread::spawn(move || -> (u64, Vec<String>) {
            use std::os::fd::AsRawFd as _;
            let mut looks = 0;
            let mut seen = Vec::new();
            while !done.load(Ordering::Relaxed) {
                looks += 1;
                let Ok(dir) = fs::File::open(&target) else {
                    seen.push("apps/clock was missing".to_string());
                    continue;
                };
                let via = PathBuf::from(format!("/proc/self/fd/{}", dir.as_raw_fd()));
                let manifest = fs::read_to_string(via.join("app.toml"));
                let binary = fs::metadata(via.join("deckjack-clock")).map(|m| m.len());
                // A directory already swapped out is the retired version,
                // being deleted; what it holds by then is nobody's concern.
                {
                    use std::os::unix::fs::MetadataExt as _;
                    let held = dir.metadata().map(|m| (m.dev(), m.ino())).ok();
                    match fs::metadata(&target) {
                        Err(_) => {
                            seen.push("apps/clock was missing".to_string());
                            continue;
                        }
                        Ok(now) if held != Some((now.dev(), now.ino())) => continue,
                        Ok(_) => {}
                    }
                }
                match (manifest, binary) {
                    (Ok(m), Ok(len)) => {
                        let ok = (m.contains("1.0.0") && len == small)
                            || (m.contains("2.0.0") && len == large);
                        if !ok {
                            seen.push(format!("mixed: {len} bytes beside {m:?}"));
                        }
                    }
                    (m, b) => seen.push(format!("incomplete: {:?} {:?}", m.is_ok(), b.is_ok())),
                }
            }
            (looks, seen)
        })
    };

    for i in 0..150 {
        let from = if i % 2 == 0 { &v2 } else { &v1 };
        install::install(&home, &source(from)).unwrap();
    }
    done.store(true, Ordering::Relaxed);
    let (looks, seen) = reader.join().unwrap();
    assert!(looks > 150, "the reader barely ran ({looks} looks)");
    assert!(
        seen.is_empty(),
        "{} of {looks}: {:?}",
        seen.len(),
        &seen[..seen.len().min(5)]
    );
}

#[test]
fn installs_and_uninstalls_running_at_once_do_not_trip_over_each_other() {
    // Every operation stages under the shared apps/.staging, and each one
    // tidies that directory away when it finishes. One finishing must never
    // pull it out from under another that is about to stage.
    let tmp = tempfile::tempdir().unwrap();
    let home = Arc::new(tmp.path().join("home"));
    let bytes = true_bytes();
    let sources: Vec<PathBuf> = (0..6)
        .map(|i| {
            let name = format!("app{i}");
            built_app(
                tmp.path(),
                &name,
                &name,
                &manifest_text(&name, "1.0.0", ""),
                &bytes,
            )
        })
        .collect();

    let workers: Vec<_> = sources
        .into_iter()
        .enumerate()
        .map(|(i, dir)| {
            let home = home.clone();
            std::thread::spawn(move || -> Vec<String> {
                let mut failures = Vec::new();
                for _ in 0..60 {
                    if let Err(e) = install::install(&home, &source(&dir)) {
                        failures.push(format!("install: {e}"));
                    }
                    if let Err(e) = install::uninstall(&home, &format!("app{i}")) {
                        failures.push(format!("uninstall: {e}"));
                    }
                }
                failures
            })
        })
        .collect();
    let failures: Vec<String> = workers
        .into_iter()
        .flat_map(|w| w.join().unwrap())
        .collect();
    assert!(
        failures.is_empty(),
        "{} failed: {:?}",
        failures.len(),
        &failures[..failures.len().min(5)]
    );
}

#[test]
fn the_cli_listing_names_what_will_not_run_and_why() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let dir = built_app(
        tmp.path(),
        "src",
        "clock",
        &manifest_text("clock", "1.0.0", ""),
        &true_bytes(),
    );
    install::install(&home, &source(&dir)).unwrap();
    // A manifest with no binary beside it.
    let orphan = home.join("apps/notes");
    fs::create_dir_all(&orphan).unwrap();
    fs::write(orphan.join("app.toml"), manifest_text("notes", "1.0.0", "")).unwrap();

    let out = deckjack(&home, &["apps"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success());
    assert!(stdout.contains("clock 1.0.0"), "{stdout}");
    let line = stdout
        .lines()
        .find(|l| l.starts_with("notes"))
        .unwrap_or_else(|| panic!("notes is not listed: {stdout}"));
    assert!(line.contains("NOT RUNNABLE"), "{line}");
    assert!(line.contains("deckjack-notes"), "{line}");
}

// ---------------------------------------------------------------------------
// The report is about the stored approval, not the previous manifest
// ---------------------------------------------------------------------------

/// Record an approval in the working directory's state database, the one
/// the server reads, as the server records it.
fn store_grant(work: &Path, app: &str, fingerprint: &str, needs: &str) {
    let manifest =
        apps::Manifest::parse(Path::new("app.toml"), &manifest_text(app, "1.0.0", needs)).unwrap();
    let store = deckjack_core::store::Store::open(work).unwrap();
    store
        .record_app_grant(&Grant {
            app: app.to_string(),
            fingerprint: fingerprint.to_string(),
            needs: manifest.needs,
            granted_at: 1,
        })
        .unwrap();
}

#[test]
fn a_reinstall_over_a_grant_from_before_the_sha256_switch_says_it_must_be_approved_again() {
    // An operator upgrading from a build that fingerprinted with fnv1a64:
    // the manifest is unchanged, but the server compares the stored
    // `fnv1a64:` value with a `sha256:` one and refuses to launch. The
    // install report used to compare the old manifest with the new one —
    // both hashed by the new code — and promise the approval still applied.
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let dir = built_app(
        tmp.path(),
        "src",
        "notes",
        &manifest_text("notes", "1.0.0", NEEDS_NETWORK),
        &true_bytes(),
    );
    install::install(&home, &source(&dir)).unwrap();
    // The working directory is the install directory, as by default.
    store_grant(&home, "notes", "fnv1a64:0123456789abcdef", NEEDS_NETWORK);

    let report = install::install(&home, &source(&dir)).unwrap();
    match &report.approval {
        Approval::Required { because, .. } => assert_eq!(*because, Because::OlderFingerprint),
        other => panic!("expected approval to be required, got {other:?}"),
    }
    let text = report.render();
    assert!(!text.contains("still applies"), "{text}");
    assert!(text.contains("Approve notes"), "it must say how: {text}");
    assert!(text.contains("SHA-256"), "and why: {text}");

    // The server agrees, and its refusal points at the same way out rather
    // than at a command that does not exist.
    let (mut server, _links) = host(&home, &home, StoredGrants::open(&home));
    server.refresh();
    match server.open_window("notes") {
        Err(e @ HostError::GrantStale { .. }) => {
            let message = e.to_string();
            assert!(message.contains("Approve notes"), "{message}");
            assert!(!message.contains("deckjack approve"), "{message}");
        }
        other => panic!("expected a stale grant, got {other:?}"),
    }

    // And once approved again, the next install says so.
    server.approve("notes").unwrap();
    let report = install::install(&home, &source(&dir)).unwrap();
    assert_eq!(report.approval, Approval::Approved);
}

#[test]
fn a_reinstall_of_an_app_never_approved_does_not_claim_an_approval() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let dir = built_app(
        tmp.path(),
        "src",
        "notes",
        &manifest_text("notes", "1.0.0", NEEDS_NETWORK),
        &true_bytes(),
    );
    install::install(&home, &source(&dir)).unwrap();
    // A server has run here and recorded approvals, none of them for notes.
    drop(Store::open(&home).unwrap());

    let report = install::install(&home, &source(&dir)).unwrap();
    match &report.approval {
        Approval::Required { because, .. } => assert_eq!(*because, Because::NotApproved),
        other => panic!("expected approval to be required, got {other:?}"),
    }
    let text = report.render();
    assert!(!text.contains("still applies"), "{text}");
    assert!(text.contains("must approve"), "{text}");
}

#[test]
fn the_cli_reports_against_the_working_directory_it_is_given() {
    // `deckjack install --dir` for a server started with `--dir`: the
    // approval that matters is the one in that working directory.
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let work = tmp.path().join("work");
    fs::create_dir_all(&work).unwrap();
    let dir = built_app(
        tmp.path(),
        "src",
        "notes",
        &manifest_text("notes", "1.0.0", NEEDS_NETWORK),
        &true_bytes(),
    );
    let manifest = apps::Manifest::parse(
        Path::new("app.toml"),
        &manifest_text("notes", "1.0.0", NEEDS_NETWORK),
    )
    .unwrap();
    store_grant(&work, "notes", &manifest.fingerprint(), NEEDS_NETWORK);

    let args = [
        "install",
        dir.to_str().unwrap(),
        "--dir",
        work.to_str().unwrap(),
    ];
    let out = deckjack(&home, &args);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "{stdout}{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(stdout.contains("covers exactly"), "{stdout}");

    // Without --dir it is the install directory's. No server has recorded
    // approvals there, so this may simply be the wrong directory (`make
    // install` never passes --dir): it must not claim the app is unapproved.
    let out = deckjack(&home, &["install", dir.to_str().unwrap()]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{stdout}");
    assert!(!stdout.contains("must approve"), "{stdout}");
    assert!(!stdout.contains("never been approved"), "{stdout}");
    assert!(
        stdout.contains("--dir"),
        "it must say how to point it: {stdout}"
    );

    // Once a server has recorded approvals there, none of them for notes,
    // the install directory's answer is definite.
    drop(Store::open(&home).unwrap());
    let out = deckjack(&home, &["install", dir.to_str().unwrap()]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("must approve"), "{stdout}");
}

#[test]
fn without_a_working_directory_named_no_approvals_found_is_not_reported_as_unapproved() {
    // `deckjack install` without --dir guesses the server's working
    // directory. Where that guess holds no state database at all, the
    // server is somewhere else (or has never run): whether the app is
    // approved is not known, and saying "never approved" would be false for
    // an operator whose server runs with --dir.
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let dir = built_app(
        tmp.path(),
        "src",
        "notes",
        &manifest_text("notes", "1.0.0", NEEDS_NETWORK),
        &true_bytes(),
    );
    install::install(&home, &source(&dir)).unwrap();
    let report = install::install(&home, &source(&dir)).unwrap();
    match &report.approval {
        Approval::Unknown { reason, summary } => {
            assert!(reason.contains("--dir"), "{reason}");
            assert!(summary.contains("cloud.example.net"), "{summary}");
        }
        other => panic!("expected an unknown approval, got {other:?}"),
    }
    let text = report.render();
    assert!(!text.contains("never been approved"), "{text}");
    assert!(!text.contains("It is new"), "{text}");
}

#[test]
fn a_stored_approval_the_server_cannot_read_is_not_reported_as_covering_the_app() {
    // The fingerprint matches, but the recorded needs do not parse. The
    // server treats that row as no approval and asks again; the install
    // report must say the same, not "runs without asking again".
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let work = tmp.path().join("work");
    fs::create_dir_all(&work).unwrap();
    let dir = built_app(
        tmp.path(),
        "src",
        "notes",
        &manifest_text("notes", "1.0.0", NEEDS_NETWORK),
        &true_bytes(),
    );
    install::install_in(&home, &work, &source(&dir)).unwrap();
    let grants = StoredGrants::open(&work);
    let (mut server, _links) = host(&home, &work, grants.clone());
    server.refresh();
    server.approve("notes").unwrap();

    // The control: while the row reads, the approval covers the reinstall.
    let report = install::install_in(&home, &work, &source(&dir)).unwrap();
    assert_eq!(report.approval, Approval::Approved);

    rusqlite::Connection::open(work.join(deckjack_core::store::DB_PATH))
        .unwrap()
        .execute(
            "UPDATE app_grants SET needs = 'not json' WHERE app = 'notes'",
            [],
        )
        .unwrap();

    let report = install::install_in(&home, &work, &source(&dir)).unwrap();
    match &report.approval {
        Approval::Required { because, .. } => assert_eq!(*because, Because::Unreadable),
        other => panic!("expected approval to be required, got {other:?}"),
    }
    let text = report.render();
    assert!(!text.contains("runs without asking again"), "{text}");
    assert!(text.contains("Approve notes"), "{text}");

    // And the server agrees: it will not launch on that row.
    let (mut server, _links) = host(&home, &work, grants);
    server.refresh();
    assert!(
        server.open_window("notes").is_err(),
        "the server must refuse to launch on an unreadable approval"
    );
}

// ---------------------------------------------------------------------------
// What `deckjack apps` says on a node that launches no app
// ---------------------------------------------------------------------------

#[test]
fn the_apps_listing_says_when_none_of_the_apps_will_launch_and_why() {
    let manifest =
        apps::Manifest::parse(Path::new("app.toml"), &manifest_text("notes", "1.0.0", "")).unwrap();
    let one = apps::Discovery {
        apps: vec![apps::Installed {
            name: "notes".to_string(),
            dir: PathBuf::from("/nowhere/apps/notes"),
            binary: PathBuf::from("/nowhere/apps/notes/deckjack-notes"),
            manifest,
        }],
        problems: Vec::new(),
    };
    let none = apps::Discovery {
        apps: Vec::new(),
        problems: Vec::new(),
    };
    let old_kernel = deckjack_core::sandbox::Availability::Enforcing {
        landlock_abi: 6,
        network_rules: true,
        ipc_scoping: false,
    };
    let no_landlock = deckjack_core::sandbox::Availability::Unavailable {
        reason: "Landlock is not enabled".to_string(),
    };
    let new_kernel = deckjack_core::sandbox::Availability::Enforcing {
        landlock_abi: 10,
        network_rules: true,
        ipc_scoping: true,
    };

    let said = install::render_node_refusal(&one, &old_kernel);
    assert!(
        said.starts_with("\nNONE OF THESE WILL LAUNCH ON THIS NODE: "),
        "{said:?}"
    );
    assert!(said.contains("Landlock ABI 6"), "{said:?}");
    assert!(said.contains(apps::MIN_KERNEL), "{said:?}");

    let said = install::render_node_refusal(&one, &no_landlock);
    assert!(said.contains("NONE OF THESE WILL LAUNCH"), "{said:?}");
    assert!(said.contains("Landlock is not enabled"), "{said:?}");

    // Nothing to say when apps can launch, or when nothing is listed.
    assert_eq!(install::render_node_refusal(&one, &new_kernel), "");
    assert_eq!(install::render_node_refusal(&none, &old_kernel), "");
}
