//! Node registry and placement, through the public API only.
//!
//! Everything here runs against a clock passed in as `now` and a `NodeStore`
//! held in memory, so there is no sleeping, no SQLite and no spawning: the
//! same shape as `lock.rs`, for the same reason.
//!
//! What these tests are trying to catch is the class of defect that only
//! shows up when two halves meet: a node identified by something transient,
//! a wire format one side invented, a requirement that is quietly dropped
//! instead of refusing, a node evicted because its link blinked.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Mutex;

use deckjack_core::node;
use deckjack_core::registry::{
    Arch, BackendKind, BackendRecord, Capabilities, CodecNeed, CodecSupport, Cpu, Gpu, GpuApi,
    GpuNeed, HEARTBEAT_SECS, Lifecycle, Liveness, MeshAddress, NoPersistence, NodeHealth,
    NodeStore, OFFLINE_AFTER_SECS, Os, PROTOCOL_VERSION, PersistedNode, PlacementError,
    RUNTIME_GLIBC, RUNTIME_NODE, Registered, Registration, Registry, RegistryError, Requirements,
    Storage, Target, Unfit, WorkingDirNeed,
};
use deckjack_core::store::StoreError;

// A fixed unix second. Any value works; a real-looking one makes a failure
// message readable.
const T0: u64 = 1_700_000_000;

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

/// A store that keeps rows in memory, or fails every call when asked to.
///
/// `Mutex` rather than `RefCell` so the store is `Sync`, which is what the
/// server needs to hand it to an async handler.
struct MemStore {
    rows: Mutex<BTreeMap<String, PersistedNode>>,
    broken: bool,
}

impl MemStore {
    fn new() -> MemStore {
        MemStore {
            rows: Mutex::new(BTreeMap::new()),
            broken: false,
        }
    }

    fn broken() -> MemStore {
        MemStore {
            rows: Mutex::new(BTreeMap::new()),
            broken: true,
        }
    }

    fn len(&self) -> usize {
        self.rows.lock().unwrap().len()
    }

    fn row(&self, node_id: &str) -> Option<PersistedNode> {
        self.rows.lock().unwrap().get(node_id).cloned()
    }

    /// Put a row in directly, as a previous run of the server would have.
    fn seed(&self, row: PersistedNode) {
        self.rows.lock().unwrap().insert(row.node_id.clone(), row);
    }
}

/// Any `StoreError` will do: what is under test is that the registry keeps
/// working when the database does not.
fn store_is_down() -> StoreError {
    StoreError::FromTheFuture {
        path: std::path::PathBuf::from("/nonexistent/.deckjack/state.db"),
        found: 99,
        supported: 1,
    }
}

impl NodeStore for MemStore {
    fn upsert_node(&self, node: &PersistedNode) -> Result<(), StoreError> {
        if self.broken {
            return Err(store_is_down());
        }
        self.rows
            .lock()
            .unwrap()
            .insert(node.node_id.clone(), node.clone());
        Ok(())
    }

    fn load_nodes(&self) -> Result<Vec<PersistedNode>, StoreError> {
        if self.broken {
            return Err(store_is_down());
        }
        Ok(self.rows.lock().unwrap().values().cloned().collect())
    }

    fn delete_node(&self, node_id: &str) -> Result<(), StoreError> {
        if self.broken {
            return Err(store_is_down());
        }
        self.rows.lock().unwrap().remove(node_id);
        Ok(())
    }
}

fn addr(host: &str) -> MeshAddress {
    MeshAddress::new(host, 7878).expect("a tailnet name is a usable address")
}

fn bare_caps(arch: Arch, os: Os) -> Capabilities {
    Capabilities {
        target: Target { arch, os },
        cpu: Cpu::default(),
        gpus: Vec::new(),
        codecs: Vec::new(),
        storage: Storage::default(),
        runtimes: BTreeSet::new(),
    }
}

fn linux_x86() -> Capabilities {
    bare_caps(Arch::X86_64, Os::Linux)
}

fn gpu(vram_bytes: Option<u64>) -> Gpu {
    Gpu {
        vendor: "NVIDIA".to_string(),
        model: "Tesla P40".to_string(),
        vram_bytes,
        apis: vec![GpuApi::Cuda, GpuApi::Vulkan],
    }
}

fn codec(name: &str, encode: bool, hardware: bool) -> CodecSupport {
    CodecSupport {
        name: name.to_string(),
        decode: true,
        encode,
        hardware,
    }
}

fn backend(id: &str, commands: &[&str], lifecycle: Lifecycle) -> BackendRecord {
    BackendRecord {
        id: id.to_string(),
        kind: BackendKind::RustService,
        commands: commands.iter().map(|c| c.to_string()).collect(),
        lifecycle,
    }
}

fn registration(node_id: &str, caps: Capabilities, backends: Vec<BackendRecord>) -> Registration {
    Registration {
        node_id: node_id.to_string(),
        host_name: format!("host-{node_id}"),
        address: addr(node_id),
        protocol: PROTOCOL_VERSION,
        capabilities: caps,
        backends,
    }
}

/// A registry whose shell node is not one of the registered nodes, so the
/// local-node bonus does not quietly decide a comparison under test.
fn registry_of(nodes: Vec<Registration>, store: &dyn NodeStore) -> Registry {
    let mut registry = Registry::new("shell-node-elsewhere".to_string());
    for node in nodes {
        registry
            .register(node, T0, store)
            .expect("a current-protocol registration is accepted");
    }
    registry
}

// ---------------------------------------------------------------------------
// Identity
// ---------------------------------------------------------------------------

/// The design's rule is that a machine is the thing with the identity, not
/// the path to it. A node that moves to a new tailnet address, or is renamed,
/// is the same node; forgetting that would strand its running work and leave
/// a ghost in the registry.
#[test]
fn a_node_is_keyed_by_its_identity_not_by_how_it_is_reached() {
    let store = MemStore::new();
    let mut registry = Registry::new("shell".to_string());

    registry
        .register(registration("r620", linux_x86(), vec![]), T0, &store)
        .unwrap();

    let moved = Registration {
        host_name: "r620-renamed".to_string(),
        address: MeshAddress::new("100.101.102.103", 9999).unwrap(),
        ..registration("r620", linux_x86(), vec![])
    };
    let outcome = registry.register(moved, T0 + 60, &store).unwrap();

    assert_eq!(
        outcome,
        Registered::Known {
            capabilities_changed: false
        }
    );
    assert_eq!(registry.nodes().count(), 1, "one machine, one record");
    assert_eq!(store.len(), 1, "and one row");

    let record = registry.get("r620").expect("still the same node");
    assert_eq!(record.address.to_string(), "100.101.102.103:9999");
    assert_eq!(record.host_name, "r620-renamed");
    assert_eq!(
        record.first_seen, T0,
        "first_seen is when the machine was first seen, not when it last moved"
    );
}

/// Two machines can easily answer to the same name — a restored laptop, two
/// hosts both called `localhost` behind the mesh. Only the node id separates
/// them, and placement must treat them as two candidates.
#[test]
fn two_machines_sharing_a_host_name_and_address_are_still_two_nodes() {
    let store = MemStore::new();
    let mut registry = Registry::new("shell".to_string());

    for node_id in ["node-aaa", "node-bbb"] {
        let reg = Registration {
            host_name: "deckjack".to_string(),
            address: addr("deckjack"),
            ..registration(node_id, linux_x86(), vec![])
        };
        registry.register(reg, T0, &store).unwrap();
    }

    assert_eq!(registry.nodes().count(), 2);
    assert_eq!(store.len(), 2);
}

/// A daemon reconnects constantly: restarts, link flaps, upgrades. Each one
/// is an update, and the second one has to be reported as a known node or
/// anything keyed on "new node" fires every few minutes.
#[test]
fn re_registering_the_same_node_updates_it_rather_than_duplicating_it() {
    let store = MemStore::new();
    let mut registry = Registry::new("shell".to_string());

    let first = registry
        .register(registration("fold", linux_x86(), vec![]), T0, &store)
        .unwrap();
    assert_eq!(first, Registered::New);

    let again = registry
        .register(registration("fold", linux_x86(), vec![]), T0 + 5, &store)
        .unwrap();
    assert_eq!(
        again,
        Registered::Known {
            capabilities_changed: false
        }
    );

    // The Fold with a dock attached is a different machine for scheduling.
    let docked = {
        let mut caps = linux_x86();
        caps.gpus.push(gpu(Some(8 << 30)));
        registration("fold", caps, vec![])
    };
    let changed = registry.register(docked, T0 + 10, &store).unwrap();
    assert_eq!(
        changed,
        Registered::Known {
            capabilities_changed: true
        }
    );

    assert_eq!(registry.nodes().count(), 1);
    assert_eq!(store.len(), 1);
}

/// A node whose protocol number this build does not speak is refused whole.
/// Half-understanding it is how a field means one thing on one machine and
/// another on the next, so the refusal must also leave nothing behind.
#[test]
fn a_node_speaking_another_protocol_version_is_refused_and_not_remembered() {
    let store = MemStore::new();
    let mut registry = Registry::new("shell".to_string());

    let newer = Registration {
        protocol: PROTOCOL_VERSION + 1,
        ..registration("r620", linux_x86(), vec![])
    };
    let err = registry.register(newer, T0, &store).unwrap_err();

    match err {
        RegistryError::IncompatibleProtocol {
            node_id,
            theirs,
            ours,
        } => {
            assert_eq!(node_id, "r620");
            assert_eq!(theirs, PROTOCOL_VERSION + 1);
            assert_eq!(ours, PROTOCOL_VERSION);
            // The message has to name both sides, or the operator cannot tell
            // which machine to upgrade.
            let message = RegistryError::IncompatibleProtocol {
                node_id: "r620".to_string(),
                theirs,
                ours,
            }
            .to_string();
            assert!(message.contains("r620"), "{message}");
        }
        other => panic!("expected a protocol refusal, got {other:?}"),
    }

    assert_eq!(registry.nodes().count(), 0);
    assert_eq!(store.len(), 0);
    assert!(matches!(
        registry.place(&Requirements::default(), T0),
        Err(PlacementError::NoNodes)
    ));
}

/// The shell node is a node. If it registered through some side door, "run it
/// here" would be a special case in the scheduler rather than a placement,
/// and `local` would be guesswork.
#[test]
fn the_shell_node_registers_through_the_same_path_as_every_other_node() {
    let install = tempfile::tempdir().unwrap();
    let me = node::id(install.path()).unwrap();
    let store = MemStore::new();

    let mut registry = Registry::new(me.clone());
    registry
        .register(
            Registration::for_this_node(
                install.path(),
                addr("gpd"),
                Capabilities::for_this_build(),
                vec![backend("files", &["files.list"], Lifecycle::LongRunning)],
            )
            .unwrap(),
            T0,
            &store,
        )
        .unwrap();
    registry
        .register(registration("other", linux_x86(), vec![]), T0, &store)
        .unwrap();

    let mut need = Requirements::for_command("files.list");
    need.working_dir = WorkingDirNeed::Required;
    let placed = registry.place(&need, T0).unwrap();

    assert_eq!(placed.node_id, me);
    assert!(placed.local, "no hop and nothing to sign on the shell node");
    assert_eq!(placed.backend.as_deref(), Some("files"));
}

// ---------------------------------------------------------------------------
// The wire
// ---------------------------------------------------------------------------

/// The registration a node daemon sends, written out by hand rather than
/// produced by these same Rust types. A serde attribute change that renames
/// a field or reshapes the address stops the mesh dead, and round-tripping
/// through `Registration` would not notice.
#[test]
fn a_registration_as_the_node_daemon_sends_it_parses_as_written() {
    let wire = r#"{
        "node_id": "cmV2OTk",
        "host_name": "r620",
        "address": "r620:7878",
        "protocol": 1,
        "capabilities": {
            "target": { "arch": "x86_64", "os": "linux" },
            "cpu": { "cores": 24, "avx2": false },
            "gpus": [
                {
                    "vendor": "NVIDIA",
                    "model": "Tesla P40",
                    "vram_bytes": 25769803776,
                    "apis": ["cuda", "vulkan"]
                }
            ],
            "codecs": [
                { "name": "h264", "decode": true, "encode": true, "hardware": true }
            ],
            "storage": { "free_bytes": 1099511627776, "total_bytes": 2199023255552 },
            "runtimes": ["glibc", "node"]
        },
        "backends": [
            {
                "id": "files",
                "kind": "rust_service",
                "commands": ["files.list", "files.read"],
                "lifecycle": "long_running"
            }
        ]
    }"#;

    let reg: Registration = serde_json::from_str(wire).expect("the daemon's own shape");

    assert_eq!(reg.protocol, PROTOCOL_VERSION);
    assert_eq!(reg.address.host(), "r620");
    assert_eq!(reg.address.port(), 7878);
    assert_eq!(reg.capabilities.target.arch, Arch::X86_64);
    assert_eq!(reg.capabilities.target.os, Os::Linux);
    assert_eq!(reg.capabilities.cpu.cores, 24);
    assert_eq!(reg.capabilities.best_vram(), Some(24 << 30));
    assert!(
        reg.capabilities.codec("H264").is_some(),
        "manifests say H264 and ffmpeg says h264"
    );
    assert!(reg.capabilities.runtimes.contains("node"));
    let b = &reg.backends[0];
    assert_eq!(b.kind, BackendKind::RustService);
    assert_eq!(b.lifecycle, Lifecycle::LongRunning);
    assert!(b.provides("files.read"));
    assert!(!b.provides("files.write"));
}

/// Fields are meant to be additive: an older shell node has to keep accepting
/// a newer daemon's registration, and a node that reports nothing simply
/// never wins work that needs hardware it did not claim.
#[test]
fn a_newer_daemon_can_send_fields_this_build_does_not_know() {
    let wire = r#"{
        "node_id": "fold",
        "host_name": "fold",
        "address": "fold:7878",
        "protocol": 1,
        "capabilities": {
            "target": { "arch": "aarch64", "os": "android" },
            "npu": { "tops": 40 }
        },
        "backends": []
    }"#;

    let reg: Registration = serde_json::from_str(wire).expect("unknown fields are ignored");
    assert_eq!(reg.capabilities.target.os, Os::Android);
    assert_eq!(reg.capabilities.cpu.cores, 0, "unreported, not zero CPUs");
    assert!(reg.capabilities.gpus.is_empty());

    let store = MemStore::new();
    let registry = registry_of(vec![reg], &store);

    let need = Requirements {
        gpu: GpuNeed::Required {
            min_vram_bytes: None,
        },
        ..Requirements::default()
    };
    match registry.place(&need, T0) {
        Err(PlacementError::NothingFits { rejections }) => {
            assert_eq!(rejections[0].reason, Unfit::NoGpu);
        }
        other => panic!("a node that claimed no GPU must not win GPU work: {other:?}"),
    }
}

/// No node ever opens a public port, so an address that is reachable from the
/// internet is a misconfiguration to refuse, not to register. The check has
/// to happen on the wire too: a registration arriving as JSON is the only way
/// this is ever reached in production.
#[test]
fn a_publicly_routable_address_is_refused_in_process_and_on_the_wire() {
    for private in [
        "127.0.0.1",
        "192.168.1.10",
        "10.0.0.4",
        "100.101.102.103", // Tailscale's CGNAT range
        "fd7a:115c:a1e0::1",
        "::1",
    ] {
        assert!(
            MeshAddress::new(private, 7878).is_ok(),
            "{private} is on the mesh"
        );
    }

    for public in ["8.8.8.8", "93.184.216.34", "2606:4700:4700::1111"] {
        assert!(
            MeshAddress::new(public, 7878).is_err(),
            "{public} is reachable from the internet"
        );
    }

    let wire = r#"{
        "node_id": "vps",
        "host_name": "vps",
        "address": "8.8.8.8:7878",
        "protocol": 1,
        "capabilities": { "target": { "arch": "x86_64", "os": "linux" } },
        "backends": []
    }"#;
    assert!(
        serde_json::from_str::<Registration>(wire).is_err(),
        "deserializing must not be a way around the address check"
    );
}

/// An address with no usable port is refused rather than defaulted: guessing
/// 7878 would turn a typo into a connection to whatever is on that port.
#[test]
fn an_address_without_a_usable_port_is_refused_rather_than_guessed() {
    for bad in ["r620", "r620:", "r620:99999", "r620:http", ":7878", ""] {
        assert!(
            MeshAddress::try_from(bad.to_string()).is_err(),
            "{bad:?} is not an address"
        );
    }
}

/// The store keeps the address as one readable string, so the string has to
/// parse back to the same address — including an IPv6 literal, where losing
/// the brackets turns the last group into the port.
#[test]
fn an_address_survives_the_form_it_is_stored_as() {
    for (host, port) in [
        ("r620", 7878u16),
        ("100.101.102.103", 9999),
        ("fd7a:115c:a1e0::1", 7878),
        ("::1", 1),
    ] {
        let built = MeshAddress::new(host, port).unwrap();
        let stored = built.to_string();
        let parsed = MeshAddress::try_from(stored.clone())
            .unwrap_or_else(|e| panic!("{stored} did not parse back: {e}"));
        assert_eq!(parsed, built, "{stored}");
        assert_eq!(parsed.host(), host);
        assert_eq!(parsed.port(), port);
    }
}

// ---------------------------------------------------------------------------
// Liveness
// ---------------------------------------------------------------------------

/// The Fold is on LTE and misses beats. Dropping a node for that would lose
/// the operator's view of a machine they own, and the design says a remembered
/// node that is offline should read as "offline", not as missing.
#[test]
fn a_stale_heartbeat_does_not_evict_a_node() {
    let store = MemStore::new();
    let mut registry = Registry::new("shell".to_string());
    registry
        .register(registration("fold", linux_x86(), vec![]), T0, &store)
        .unwrap();

    let long_gone = T0 + OFFLINE_AFTER_SECS * 100;

    assert_eq!(registry.nodes().count(), 1, "still a known machine");
    assert_eq!(registry.online(long_gone).count(), 0, "but not a candidate");
    assert_eq!(store.len(), 1, "and not deleted from the database");

    let record = registry.get("fold").expect("not evicted");
    assert_eq!(
        record.liveness(long_gone, registry.offline_after_secs()),
        Liveness::Stale {
            age_secs: OFFLINE_AFTER_SECS * 100
        }
    );

    // Coming back is a heartbeat, not a re-registration: the daemon never
    // went away, the link did.
    let before = registry.heartbeat("fold", None, long_gone + 1).unwrap();
    assert!(matches!(before, Liveness::Stale { .. }), "{before:?}");
    assert!(
        registry
            .get("fold")
            .unwrap()
            .is_online(long_gone + 1, OFFLINE_AFTER_SECS)
    );
    assert_eq!(registry.nodes().count(), 1);
}

/// The threshold has to leave room for three missed beats, and the Fold on
/// LTE misses them routinely. Asserted at compile time because it is a
/// relationship between two published constants, not a runtime outcome.
const _: () = assert!(OFFLINE_AFTER_SECS > HEARTBEAT_SECS * 3);

/// The threshold is three missed beats plus slack, and it is a documented
/// constant, so both sides of it are worth pinning: a node one second inside
/// it still takes work, and one second outside it does not.
#[test]
fn a_node_is_a_candidate_up_to_the_offline_threshold_and_not_past_it() {
    let store = MemStore::new();
    let mut registry = Registry::new("shell".to_string());
    registry
        .register(registration("r620", linux_x86(), vec![]), T0, &store)
        .unwrap();

    assert!(
        registry
            .place(&Requirements::default(), T0 + OFFLINE_AFTER_SECS)
            .is_ok()
    );
    match registry.place(&Requirements::default(), T0 + OFFLINE_AFTER_SECS + 1) {
        Err(PlacementError::NothingFits { rejections }) => assert!(
            matches!(
                rejections[0].reason,
                Unfit::Offline {
                    liveness: Liveness::Stale { .. }
                }
            ),
            "{:?}",
            rejections[0].reason
        ),
        other => panic!("a node past the threshold is not a candidate: {other:?}"),
    }

    // A worse link gets a longer rope, and the rope is what decides.
    let patient = Registry::new("shell".to_string()).with_offline_after(OFFLINE_AFTER_SECS * 10);
    assert_eq!(patient.offline_after_secs(), OFFLINE_AFTER_SECS * 10);
}

/// A beat for a node id nobody registered must not invent a node: a record
/// with no capabilities and no address would be a candidate the scheduler
/// could pick and then be unable to reach.
#[test]
fn a_heartbeat_from_an_unregistered_node_is_refused_and_creates_nothing() {
    let mut registry = Registry::new("shell".to_string());

    let err = registry.heartbeat("stranger", None, T0).unwrap_err();
    assert!(
        matches!(err, RegistryError::UnknownNode { ref node_id } if node_id == "stranger"),
        "{err:?}"
    );
    assert_eq!(registry.nodes().count(), 0);
    assert!(registry.get("stranger").is_none());
}

/// Liveness is only ever earned by a beat arriving at *this* process. A row
/// in the database says a machine exists, never that it is reachable, so a
/// restarted server must not hand work to a node it has not heard from.
#[test]
fn a_node_remembered_from_the_database_is_never_live_until_it_beats() {
    let store = MemStore::new();
    {
        let mut first_run = Registry::new("shell".to_string());
        first_run
            .register(registration("r620", linux_x86(), vec![]), T0, &store)
            .unwrap();
    }

    let mut restarted = Registry::load("shell".to_string(), &store).unwrap();
    let record = restarted.get("r620").expect("remembered");
    assert_eq!(record.last_heartbeat, None);
    assert_eq!(record.health, None);
    assert_eq!(
        record.liveness(T0, OFFLINE_AFTER_SECS),
        Liveness::NeverSeen,
        "not seen since this server started"
    );
    assert_eq!(
        record.first_seen, T0,
        "remembered from when it first appeared"
    );

    match restarted.place(&Requirements::default(), T0) {
        Err(PlacementError::NothingFits { rejections }) => assert_eq!(
            rejections[0].reason,
            Unfit::Offline {
                liveness: Liveness::NeverSeen
            }
        ),
        other => panic!("a remembered node is not a candidate: {other:?}"),
    }

    // Registering is itself proof of life, so one handshake is enough.
    restarted
        .register(registration("r620", linux_x86(), vec![]), T0 + 1, &store)
        .unwrap();
    assert!(restarted.place(&Requirements::default(), T0 + 1).is_ok());
}

/// A daemon that cannot read its own load is still reachable. Blanking the
/// figures on such a beat would make a node look memory-starved and bounce
/// work off it.
#[test]
fn a_beat_without_health_keeps_the_last_figures_rather_than_blanking_them() {
    let store = MemStore::new();
    let mut registry = Registry::new("shell".to_string());
    registry
        .register(registration("r620", linux_x86(), vec![]), T0, &store)
        .unwrap();

    let health = NodeHealth {
        free_memory_bytes: Some(48 << 30),
        cpu_load_permille: Some(120),
    };
    registry.heartbeat("r620", Some(health), T0 + 1).unwrap();
    registry.heartbeat("r620", None, T0 + 2).unwrap();

    assert_eq!(registry.get("r620").unwrap().health, Some(health));

    let need = Requirements {
        min_free_memory_bytes: Some(32 << 30),
        ..Requirements::default()
    };
    assert!(
        registry.place(&need, T0 + 2).is_ok(),
        "the node still has the memory it reported"
    );
}

/// Two machines never agree on the clock to the second. A beat stamped
/// slightly in the future is NTP skew, not a negative age, and must not read
/// as a node that is suddenly offline.
#[test]
fn a_beat_from_a_node_with_a_fast_clock_does_not_age_backwards() {
    let store = MemStore::new();
    let mut registry = Registry::new("shell".to_string());
    registry
        .register(registration("r620", linux_x86(), vec![]), T0 + 30, &store)
        .unwrap();

    let record = registry.get("r620").unwrap();
    assert_eq!(record.heartbeat_age(T0), Some(0));
    assert_eq!(
        record.liveness(T0, OFFLINE_AFTER_SECS),
        Liveness::Online { age_secs: 0 }
    );
    assert!(registry.place(&Requirements::default(), T0).is_ok());
}

// ---------------------------------------------------------------------------
// Placement refuses rather than nearly fits
// ---------------------------------------------------------------------------

/// A GPU too small for the job is not a smaller GPU to use anyway. The
/// refusal has to carry both numbers, since "no node fits" tells the operator
/// nothing about which machine to change.
#[test]
fn a_node_that_only_nearly_fits_is_refused_and_says_by_how_much() {
    let store = MemStore::new();
    let small = {
        let mut caps = linux_x86();
        caps.gpus.push(gpu(Some(4 << 30)));
        registration("small", caps, vec![])
    };
    let registry = registry_of(vec![small], &store);

    let need = Requirements {
        gpu: GpuNeed::Required {
            min_vram_bytes: Some(8 << 30),
        },
        ..Requirements::default()
    };

    match registry.place(&need, T0) {
        Err(e @ PlacementError::NothingFits { .. }) => {
            let PlacementError::NothingFits { ref rejections } = e else {
                unreachable!()
            };
            assert_eq!(
                rejections[0].reason,
                Unfit::NotEnoughVram {
                    have: 4 << 30,
                    need: 8 << 30
                }
            );
            let message = e.to_string();
            assert!(message.contains("4.0 GiB"), "{message}");
            assert!(message.contains("8.0 GiB"), "{message}");
            assert!(message.contains("host-small"), "{message}");
        }
        other => panic!("4 GiB does not satisfy 8 GiB: {other:?}"),
    }

    // A GPU whose driver never reported its size is still a GPU: rejecting it
    // would make placement depend on how chatty a driver is.
    let quiet = {
        let mut caps = linux_x86();
        caps.gpus.push(gpu(None));
        registration("quiet", caps, vec![])
    };
    let store2 = MemStore::new();
    assert!(registry_of(vec![quiet], &store2).place(&need, T0).is_ok());
}

/// A software AV1 encode on the R620 is technically a yes and practically an
/// overnight job, which is the whole reason `hardware` is tracked apart.
#[test]
fn a_software_only_codec_never_satisfies_a_hardware_requirement() {
    let store = MemStore::new();
    let soft = {
        let mut caps = linux_x86();
        caps.codecs.push(codec("av1", true, false));
        registration("soft", caps, vec![])
    };
    let registry = registry_of(vec![soft], &store);

    let need = Requirements {
        codecs: vec![CodecNeed::encode("av1").in_hardware()],
        ..Requirements::default()
    };
    match registry.place(&need, T0) {
        Err(PlacementError::NothingFits { rejections }) => assert_eq!(
            rejections[0].reason,
            Unfit::SoftwareCodecOnly {
                codec: "av1".to_string()
            }
        ),
        other => panic!("software-only is not hardware: {other:?}"),
    }

    // Decoding it is a different question from encoding it, and the common
    // shortage is encode.
    let decode_only = {
        let mut caps = linux_x86();
        caps.codecs.push(codec("av1", false, true));
        registration("decoder", caps, vec![])
    };
    let store2 = MemStore::new();
    let registry = registry_of(vec![decode_only], &store2);

    let encode = Requirements {
        codecs: vec![CodecNeed::encode("av1")],
        ..Requirements::default()
    };
    assert!(registry.place(&encode, T0).is_err(), "cannot encode av1");

    let decode = Requirements {
        codecs: vec![CodecNeed::decode("av1")],
        ..Requirements::default()
    };
    assert!(registry.place(&decode, T0).is_ok(), "can decode av1");
}

/// Placement is how a command is routed without the caller knowing which app
/// owns it. A command nothing provides has to be a refusal; picking a node
/// anyway would dispatch to a backend that is not there.
#[test]
fn a_command_no_backend_provides_is_refused_rather_than_placed_anywhere() {
    let store = MemStore::new();
    let node = registration(
        "r620",
        linux_x86(),
        vec![backend("files", &["files.list"], Lifecycle::LongRunning)],
    );
    let registry = registry_of(vec![node], &store);

    match registry.place(&Requirements::for_command("filmcraft.export"), T0) {
        Err(PlacementError::NothingFits { rejections }) => assert_eq!(
            rejections[0].reason,
            Unfit::MissingCommand {
                command: "filmcraft.export".to_string()
            }
        ),
        other => panic!("unprovided commands are refused: {other:?}"),
    }

    // Naming a backend that exists but does not own the command is the same
    // refusal, not a silent fallback to the backend that was named.
    let mut pinned = Requirements::for_backend("files");
    pinned.command = Some("filmcraft.export".to_string());
    assert!(registry.place(&pinned, T0).is_err());

    // And a backend nobody runs.
    match registry.place(&Requirements::for_backend("filmcraft"), T0) {
        Err(PlacementError::NothingFits { rejections }) => assert_eq!(
            rejections[0].reason,
            Unfit::MissingBackend {
                backend: "filmcraft".to_string()
            }
        ),
        other => panic!("{other:?}"),
    }

    assert!(!registry.is_available(&Requirements::for_command("filmcraft.export"), T0));
    assert!(registry.is_available(&Requirements::for_command("files.list"), T0));
}

/// Work that has to run where the files are means the node holding the
/// working directory, which is derived from identity. A node cannot claim a
/// working directory it does not have, and an idle machine with 64 cores must
/// not be allowed to take that work.
#[test]
fn work_that_needs_the_working_directory_never_leaves_the_shell_node() {
    let store = MemStore::new();
    let mut registry = Registry::new("shell".to_string());
    registry
        .register(registration("shell", linux_x86(), vec![]), T0, &store)
        .unwrap();

    let beefy = {
        let mut caps = linux_x86();
        caps.cpu = Cpu {
            cores: 64,
            avx2: true,
        };
        caps.gpus.push(gpu(Some(24 << 30)));
        registration("beefy", caps, vec![])
    };
    registry.register(beefy, T0, &store).unwrap();

    let need = Requirements {
        working_dir: WorkingDirNeed::Required,
        ..Requirements::default()
    };
    let placed = registry.place(&need, T0).unwrap();
    assert_eq!(placed.node_id, "shell");
    assert!(placed.local);

    // With the shell node offline there is nowhere to run it, and the reason
    // is the requirement, not the hardware.
    let later = T0 + OFFLINE_AFTER_SECS + 1;
    registry.heartbeat("beefy", None, later).unwrap();
    match registry.place(&need, later) {
        Err(PlacementError::NothingFits { rejections }) => {
            let beefy = rejections.iter().find(|r| r.node_id == "beefy").unwrap();
            assert_eq!(beefy.reason, Unfit::NotWorkingDirHost);
        }
        other => panic!("the working directory is on one machine only: {other:?}"),
    }
}

/// App binaries are per target, and the Fold is `aarch64-linux-android` with
/// no glibc and no Node. Placing an x86-64 binary there, or a Node app on a
/// machine without Node, produces a process that cannot start.
#[test]
fn a_node_is_refused_for_a_target_or_runtime_it_does_not_have() {
    let store = MemStore::new();
    let fold = {
        let mut caps = bare_caps(Arch::Aarch64, Os::Android);
        caps.cpu = Cpu {
            cores: 8,
            avx2: false,
        };
        registration("fold", caps, vec![])
    };
    let registry = registry_of(vec![fold], &store);

    let x86_only = Requirements {
        arch: Some(Arch::X86_64),
        ..Requirements::default()
    };
    match registry.place(&x86_only, T0) {
        Err(PlacementError::NothingFits { rejections }) => assert_eq!(
            rejections[0].reason,
            Unfit::WrongArch {
                have: Arch::Aarch64,
                want: Arch::X86_64
            }
        ),
        other => panic!("{other:?}"),
    }

    let needs_node = Requirements {
        runtimes: vec![RUNTIME_NODE.to_string()],
        ..Requirements::default()
    };
    match registry.place(&needs_node, T0) {
        Err(PlacementError::NothingFits { rejections }) => assert_eq!(
            rejections[0].reason,
            Unfit::MissingRuntime {
                runtime: "node".to_string()
            }
        ),
        other => panic!("Termux has no Node: {other:?}"),
    }

    let needs_avx2 = Requirements {
        needs_avx2: true,
        ..Requirements::default()
    };
    match registry.place(&needs_avx2, T0) {
        Err(PlacementError::NothingFits { rejections }) => {
            assert_eq!(rejections[0].reason, Unfit::NoAvx2)
        }
        other => panic!("{other:?}"),
    }
}

/// "Nothing fits" is unactionable; "the R620 has no GPU and the Fold is
/// offline" is a decision the operator can make. Every node that was passed
/// over has to appear, with its own reason.
#[test]
fn a_refusal_names_every_node_that_was_passed_over_and_why() {
    let store = MemStore::new();
    let mut registry = Registry::new("shell".to_string());
    registry
        .register(registration("r620", linux_x86(), vec![]), T0, &store)
        .unwrap();
    let fold = {
        let mut caps = bare_caps(Arch::Aarch64, Os::Android);
        caps.gpus.push(gpu(Some(4 << 30)));
        registration("fold", caps, vec![])
    };
    registry.register(fold, T0, &store).unwrap();

    // The Fold goes quiet; the R620 keeps beating.
    let later = T0 + OFFLINE_AFTER_SECS + 5;
    registry.heartbeat("r620", None, later).unwrap();

    let need = Requirements {
        gpu: GpuNeed::Required {
            min_vram_bytes: None,
        },
        ..Requirements::default()
    };

    match registry.place(&need, later) {
        Err(e @ PlacementError::NothingFits { .. }) => {
            let PlacementError::NothingFits { ref rejections } = e else {
                unreachable!()
            };
            assert_eq!(rejections.len(), 2, "both nodes accounted for");
            let r620 = rejections.iter().find(|r| r.node_id == "r620").unwrap();
            assert_eq!(r620.reason, Unfit::NoGpu);
            let fold = rejections.iter().find(|r| r.node_id == "fold").unwrap();
            assert!(
                matches!(fold.reason, Unfit::Offline { .. }),
                "{:?}",
                fold.reason
            );

            let message = e.to_string();
            assert!(message.contains("host-r620"), "{message}");
            assert!(message.contains("host-fold"), "{message}");
            assert!(message.contains("no GPU"), "{message}");
        }
        other => panic!("{other:?}"),
    }
}

/// Two refusals that look the same to a scheduler mean different things to a
/// person: nothing is registered yet, versus nothing registered can do this.
#[test]
fn an_empty_registry_refuses_differently_from_one_where_nothing_fits() {
    let store = MemStore::new();
    let empty = Registry::new("shell".to_string());
    match empty.place(&Requirements::default(), T0) {
        Err(e @ PlacementError::NoNodes) => {
            let message = e.to_string();
            assert!(message.contains("no nodes are registered"), "{message}");
        }
        other => panic!("{other:?}"),
    }

    let registry = registry_of(vec![registration("r620", linux_x86(), vec![])], &store);
    assert!(matches!(
        registry.place(&Requirements::for_command("nope"), T0),
        Err(PlacementError::NothingFits { .. })
    ));
}

// ---------------------------------------------------------------------------
// Preference, not coin-flip
// ---------------------------------------------------------------------------

/// Load and core count are tiebreakers. A busy machine with the right
/// hardware is still the right machine: a GPU-preferred placement that lands
/// on a GPU-less node because that node has spare cores defeats the point of
/// expressing the preference at all.
#[test]
fn a_hardware_preference_is_not_outranked_by_spare_cores() {
    let store = MemStore::new();

    // The one GPU in the fleet, and it is busy.
    let gpu_node = {
        let mut caps = linux_x86();
        caps.gpus.push(gpu(Some(24 << 30)));
        caps.cpu = Cpu {
            cores: 4,
            avx2: true,
        };
        registration("a-gpu", caps, vec![])
    };
    // A big idle CPU box with no GPU.
    let cpu_node = {
        let mut caps = linux_x86();
        caps.cpu = Cpu {
            cores: 64,
            avx2: true,
        };
        registration("b-cpu", caps, vec![])
    };

    let mut registry = registry_of(vec![gpu_node, cpu_node], &store);
    registry
        .heartbeat(
            "a-gpu",
            Some(NodeHealth {
                free_memory_bytes: Some(1 << 30),
                cpu_load_permille: Some(1_000),
            }),
            T0,
        )
        .unwrap();
    registry
        .heartbeat(
            "b-cpu",
            Some(NodeHealth {
                free_memory_bytes: Some(64 << 30),
                cpu_load_permille: Some(0),
            }),
            T0,
        )
        .unwrap();

    let need = Requirements {
        gpu: GpuNeed::Preferred,
        ..Requirements::default()
    };
    assert_eq!(
        registry.place(&need, T0).unwrap().node_id,
        "a-gpu",
        "a GPU preference must outrank idle cores"
    );
}

/// A service already running and supervised there starts instantly; an
/// on-demand backend pays a cold start. That is a real difference, and it must
/// not be erased by whichever machine happens to be idle.
#[test]
fn a_running_service_is_preferred_over_a_cold_start_on_an_idle_node() {
    let store = MemStore::new();
    let running = registration(
        "a-running",
        linux_x86(),
        vec![backend(
            "filmcraft",
            &["filmcraft.export"],
            Lifecycle::LongRunning,
        )],
    );
    let cold = {
        let mut caps = linux_x86();
        caps.cpu = Cpu {
            cores: 64,
            avx2: true,
        };
        registration(
            "b-cold",
            caps,
            vec![backend(
                "filmcraft",
                &["filmcraft.export"],
                Lifecycle::OnDemand,
            )],
        )
    };

    let mut registry = registry_of(vec![running, cold], &store);
    registry
        .heartbeat(
            "a-running",
            Some(NodeHealth {
                free_memory_bytes: Some(1 << 30),
                cpu_load_permille: Some(1_000),
            }),
            T0,
        )
        .unwrap();
    registry
        .heartbeat(
            "b-cold",
            Some(NodeHealth {
                free_memory_bytes: Some(64 << 30),
                cpu_load_permille: Some(0),
            }),
            T0,
        )
        .unwrap();

    let placed = registry
        .place(&Requirements::for_command("filmcraft.export"), T0)
        .unwrap();
    assert_eq!(placed.node_id, "a-running");
    assert_eq!(placed.backend.as_deref(), Some("filmcraft"));
}

/// Repeated placement of the same command has to land on the same machine, or
/// follow-up work walks away from the state it needs. With nothing to choose
/// between two nodes, the answer is the same one every time.
#[test]
fn placement_is_stable_so_repeated_work_lands_on_the_same_node() {
    let store = MemStore::new();
    let backends = vec![backend("files", &["files.list"], Lifecycle::LongRunning)];
    let registry = registry_of(
        vec![
            registration("node-bbb", linux_x86(), backends.clone()),
            registration("node-aaa", linux_x86(), backends.clone()),
        ],
        &store,
    );

    let need = Requirements::for_command("files.list");
    let first = registry.place(&need, T0).unwrap();
    for _ in 0..5 {
        assert_eq!(
            registry.place(&need, T0 + 1).unwrap().node_id,
            first.node_id
        );
    }
    assert_eq!(first.node_id, "node-aaa", "ties keep the lowest node id");
    assert!(!first.local, "neither of these is the shell node");
}

// ---------------------------------------------------------------------------
// Persistence
// ---------------------------------------------------------------------------

/// Capabilities are what placement decides on, so a restart that garbles them
/// routes work to the wrong machine. Every arm that carries data has to come
/// back as it went in — including an architecture this build has never heard
/// of, which is exactly the field most likely to be lossy.
#[test]
fn capabilities_round_trip_through_the_store() {
    let caps = Capabilities {
        target: Target {
            arch: Arch::Other("riscv64".to_string()),
            os: Os::Other("illumos".to_string()),
        },
        cpu: Cpu {
            cores: 24,
            avx2: true,
        },
        gpus: vec![
            gpu(Some(24 << 30)),
            Gpu {
                vendor: "Intel".to_string(),
                model: "Arc".to_string(),
                vram_bytes: None,
                apis: vec![GpuApi::Vulkan, GpuApi::Other("oneapi".to_string())],
            },
        ],
        codecs: vec![codec("h264", true, true), codec("av1", false, false)],
        storage: Storage {
            free_bytes: Some(1 << 40),
            total_bytes: None,
        },
        runtimes: BTreeSet::from([RUNTIME_NODE.to_string(), RUNTIME_GLIBC.to_string()]),
    };

    let store = MemStore::new();
    let mut registry = Registry::new("shell".to_string());
    let backends = vec![
        backend(
            "files",
            &["files.list", "files.read"],
            Lifecycle::LongRunning,
        ),
        BackendRecord {
            kind: BackendKind::CliAdapter,
            ..backend("git", &["git.status"], Lifecycle::OnDemand)
        },
        BackendRecord {
            kind: BackendKind::NodeApp,
            ..backend("agentmancer", &["agent.session"], Lifecycle::OnDemand)
        },
    ];
    registry
        .register(
            registration("r620", caps.clone(), backends.clone()),
            T0,
            &store,
        )
        .unwrap();

    let restored = Registry::load("shell".to_string(), &store).unwrap();
    let record = restored.get("r620").expect("remembered");
    assert_eq!(record.capabilities, caps, "capabilities survived the store");
    assert_eq!(record.backends, backends, "and so did the backends");
    assert_eq!(record.address, addr("r620"));
    assert_eq!(record.protocol, PROTOCOL_VERSION);

    // Because they survived exactly, the first registration after a restart
    // is not reported as a hardware change — anything caching a placement
    // keys on that.
    let mut restored = restored;
    let outcome = restored
        .register(registration("r620", caps, backends), T0 + 1, &store)
        .unwrap();
    assert_eq!(
        outcome,
        Registered::Known {
            capabilities_changed: false
        }
    );
}

/// A node that is demonstrably there is not refused because the state
/// database could not be written, exactly as a sign-in is not refused because
/// the session could not be stored.
#[test]
fn a_store_that_cannot_be_written_does_not_refuse_a_node_that_is_there() {
    let broken = MemStore::broken();
    let mut registry = Registry::new("shell".to_string());

    let outcome = registry
        .register(
            registration(
                "r620",
                linux_x86(),
                vec![backend("files", &["files.list"], Lifecycle::LongRunning)],
            ),
            T0,
            &broken,
        )
        .expect("a write failure is logged, not fatal");
    assert_eq!(outcome, Registered::New);

    let placed = registry
        .place(&Requirements::for_command("files.list"), T0)
        .expect("the node is reachable whatever the database says");
    assert_eq!(placed.node_id, "r620");

    // Loading, by contrast, is where a broken store is the caller's problem:
    // it cannot be papered over with an empty registry that silently forgets
    // every machine.
    assert!(Registry::load("shell".to_string(), &broken).is_err());
}

/// A row that cannot be read is a node that will re-register the moment its
/// daemon connects, so dropping it costs nothing and refusing to start costs
/// the whole server.
#[test]
fn an_unreadable_row_is_skipped_rather_than_failing_startup() {
    let store = MemStore::new();
    let good = {
        let mut registry = Registry::new("shell".to_string());
        registry
            .register(registration("good", linux_x86(), vec![]), T0, &store)
            .unwrap();
        store.row("good").unwrap()
    };

    store.seed(PersistedNode {
        node_id: "bad-json".to_string(),
        capabilities: "{not json".to_string(),
        ..good.clone()
    });
    store.seed(PersistedNode {
        node_id: "bad-address".to_string(),
        // Hand-edited, or written by a build that did not check: either way
        // no node is ever reached off the mesh.
        address: "8.8.8.8:7878".to_string(),
        ..good.clone()
    });

    let registry = Registry::load("shell".to_string(), &store).unwrap();
    assert_eq!(registry.nodes().count(), 1);
    assert!(registry.get("good").is_some());
    assert!(registry.get("bad-json").is_none());
    assert!(registry.get("bad-address").is_none());
}

/// Forgetting a node is deliberate and total: leaving the row behind means it
/// reappears at the next restart, which is the opposite of what was asked.
#[test]
fn forgetting_a_node_removes_it_from_memory_and_from_the_database() {
    let store = MemStore::new();
    let mut registry = Registry::new("shell".to_string());
    registry
        .register(registration("r620", linux_x86(), vec![]), T0, &store)
        .unwrap();

    assert!(registry.forget("r620", &store).unwrap());
    assert_eq!(registry.nodes().count(), 0);
    assert_eq!(store.len(), 0);
    assert_eq!(
        Registry::load("shell".to_string(), &store)
            .unwrap()
            .nodes()
            .count(),
        0
    );

    assert!(
        !registry.forget("r620", &store).unwrap(),
        "forgetting what is not there is false, not an error"
    );

    // A node that comes back is new again, not a known one with a history.
    assert_eq!(
        registry
            .register(registration("r620", linux_x86(), vec![]), T0 + 10, &store)
            .unwrap(),
        Registered::New
    );
    assert_eq!(registry.get("r620").unwrap().first_seen, T0 + 10);
}

/// The stopgap store has to behave like a store, or every test above passes
/// against a thing the server never uses.
#[test]
fn without_persistence_a_node_is_unknown_until_it_registers_again() {
    let mut registry = Registry::new("shell".to_string());
    registry
        .register(
            registration("r620", linux_x86(), vec![]),
            T0,
            &NoPersistence,
        )
        .unwrap();
    assert_eq!(registry.nodes().count(), 1);

    let next_run = Registry::load("shell".to_string(), &NoPersistence).unwrap();
    assert_eq!(next_run.nodes().count(), 0);
}

/// The registry is server state and the store is reached from async handlers,
/// where the bus requires `Send` futures. A trait object that is not `Send +
/// Sync` cannot be shared with a spawned task, which is a wall at integration
/// rather than a bug with a test.
#[test]
fn the_registry_and_its_store_can_be_shared_with_the_async_bus() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<Registry>();
    assert_send_sync::<std::sync::Arc<dyn NodeStore>>();
    assert_send_sync::<Registration>();
}
