//! The node registry and capability-based placement.
//!
//! The shell node keeps this. Every other machine running the node daemon
//! registers here with its address on the private mesh, the backends it can
//! run, and what it is physically able to do — GPU, codecs, storage,
//! architecture — then heartbeats to say it is still there.
//!
//! Placement answers one question: given what a command needs, which node
//! should run it? The answer never leaves the server. A panel is handed
//! results, never a node, so [`Placement`] has no `Serialize` impl. That is
//! the cheapest way to make "location doesn't matter to the UI" true rather
//! than merely intended; a future type that does serialize a node to the
//! browser has to be written on purpose.
//!
//! Three decisions in here are deliberate:
//!
//! * **Time is an argument, never a read.** Everything that cares about
//!   liveness takes `now` as unix seconds, the same units [`crate::lock`]
//!   writes, so every rule is exercised without sleeping. The cost is that a
//!   wall-clock jump ages heartbeats; the worst outcome is a node looking
//!   offline for one interval, and the next beat corrects it.
//! * **Persistence is injected, and partial.** Registration is durable,
//!   because a node is a machine the operator owns: remembering it lets the
//!   first panel after a restart say "the R620 is offline" instead of
//!   pretending that machine never existed. Heartbeats are not written at
//!   all — with `synchronous = FULL` that would be a disk write per node
//!   every few seconds for a value that is meaningless the moment this
//!   process dies.
//! * **A remembered node is never a live node.** Liveness is only ever
//!   earned by a heartbeat that arrives at *this* process, so a registry
//!   loaded from the database starts with every node offline.
//!
//! # Use
//!
//! ```text
//! let mut registry = Registry::load(node::id(&install)?, &store)?;
//!
//! // The shell node registers itself like any other node.
//! registry.register(
//!     Registration::for_this_node(
//!         &install,
//!         MeshAddress::new("gpd", 7878)?,
//!         Capabilities::for_this_build(),
//!         vec![files_backend()],
//!     )?,
//!     now,
//!     &store,
//! )?;
//!
//! // Where should an AV1 export run?
//! let mut need = Requirements::for_command("filmcraft.export");
//! need.gpu = GpuNeed::Preferred;
//! need.codecs = vec![CodecNeed::encode("av1")];
//! let placed = registry.place(&need, now)?;   // internal; not sent to the browser
//! ```

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::io;
use std::net::IpAddr;
use std::path::Path;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::store::StoreError;

/// Version of the registration handshake. Bumped when a field stops being
/// additive. A node that speaks a different number is refused with a message
/// naming both sides, rather than half-understood: the design's rule for the
/// app protocol applies to the node protocol for the same reason.
pub const PROTOCOL_VERSION: u32 = 1;

/// How often a node daemon is expected to beat.
pub const HEARTBEAT_SECS: u64 = 10;

/// When a node stops being a placement candidate.
///
/// Three missed beats plus slack. Deliberately generous: the Fold is on LTE
/// and misses a beat routinely, and treating that as a failure would bounce
/// long-running work between machines for no reason.
pub const OFFLINE_AFTER_SECS: u64 = 35;

/// Runtime keys used by [`Capabilities::runtimes`]. Spelled once here so a
/// daemon and a manifest cannot disagree by a capital letter.
pub const RUNTIME_NODE: &str = "node";
pub const RUNTIME_GLIBC: &str = "glibc";

// ---------------------------------------------------------------------------
// Mesh address
// ---------------------------------------------------------------------------

#[derive(Debug, Error, PartialEq, Eq)]
pub enum AddressError {
    #[error(
        "a node address cannot be empty; give the node's tailnet name or its 100.x address with a port"
    )]
    Empty,

    #[error("{input} has no port; node addresses look like `r620:7878`")]
    NoPort { input: String },

    #[error("{input} does not have a usable port number")]
    BadPort { input: String },

    #[error(
        "{ip} is a public address; Deckjack nodes are reached over the private mesh only, so \
         register the node's tailnet address instead and leave it bound to loopback"
    )]
    Public { ip: IpAddr },
}

/// Where the shell node reaches a node, on the private mesh.
///
/// Stored and sent as one `host:port` string so the validation below cannot be
/// skipped by deserializing the struct directly: a registration arriving over
/// the wire is checked exactly as one built in process.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct MeshAddress {
    host: String,
    port: u16,
}

impl MeshAddress {
    /// Build an address, refusing one that is not on a private network.
    ///
    /// Only a literal IP can be judged here. A name is accepted as given,
    /// because tailnet names resolve through split DNS that is a property of
    /// the client's network: resolving it in the registry would make
    /// registration depend on whichever resolver this process happens to
    /// have, and fail on the laptop that is offline with a hosts-file entry.
    pub fn new(host: impl Into<String>, port: u16) -> Result<MeshAddress, AddressError> {
        let host = host.into();
        let host = host.trim().trim_start_matches('[').trim_end_matches(']');
        if host.is_empty() {
            return Err(AddressError::Empty);
        }
        if let Ok(ip) = host.parse::<IpAddr>()
            && !is_mesh_ip(ip)
        {
            return Err(AddressError::Public { ip });
        }
        Ok(MeshAddress {
            host: host.to_string(),
            port,
        })
    }

    pub fn host(&self) -> &str {
        &self.host
    }

    pub fn port(&self) -> u16 {
        self.port
    }
}

impl fmt::Display for MeshAddress {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Bracket a literal IPv6 so the result parses back.
        if self.host.contains(':') {
            write!(f, "[{}]:{}", self.host, self.port)
        } else {
            write!(f, "{}:{}", self.host, self.port)
        }
    }
}

impl TryFrom<String> for MeshAddress {
    type Error = AddressError;

    fn try_from(value: String) -> Result<MeshAddress, AddressError> {
        let (host, port) = value.rsplit_once(':').ok_or(AddressError::NoPort {
            input: value.clone(),
        })?;
        let port: u16 = port.parse().map_err(|_| AddressError::BadPort {
            input: value.clone(),
        })?;
        MeshAddress::new(host, port)
    }
}

impl From<MeshAddress> for String {
    fn from(value: MeshAddress) -> String {
        value.to_string()
    }
}

/// Loopback, RFC1918, link-local, IPv6 unique-local, or the CGNAT range
/// `100.64.0.0/10` that Tailscale hands out. Anything else is reachable from
/// the internet, and no node is ever supposed to be.
fn is_mesh_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let [a, b, _, _] = v4.octets();
            v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || (a == 100 && (64..128).contains(&b))
        }
        IpAddr::V6(v6) => {
            let first = v6.segments()[0];
            // fc00::/7 unique-local (Tailscale's fd7a:115c:a1e0::/48 included)
            // and fe80::/10 link-local.
            v6.is_loopback() || (first & 0xfe00) == 0xfc00 || (first & 0xffc0) == 0xfe80
        }
    }
}

// ---------------------------------------------------------------------------
// Capabilities
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Arch {
    X86_64,
    Aarch64,
    Other(String),
}

impl Arch {
    pub fn current() -> Arch {
        match std::env::consts::ARCH {
            "x86_64" => Arch::X86_64,
            "aarch64" => Arch::Aarch64,
            other => Arch::Other(other.to_string()),
        }
    }
}

impl fmt::Display for Arch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Arch::X86_64 => f.write_str("x86_64"),
            Arch::Aarch64 => f.write_str("aarch64"),
            Arch::Other(s) => f.write_str(s),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Os {
    Linux,
    MacOs,
    /// Termux on the Fold. Kept distinct from Linux on purpose: there is no
    /// glibc, Android's seccomp filter refuses syscalls that work everywhere
    /// else, and anything needing Node cannot be installed here at all.
    Android,
    Other(String),
}

impl Os {
    pub fn current() -> Os {
        match std::env::consts::OS {
            "linux" => Os::Linux,
            "macos" => Os::MacOs,
            "android" => Os::Android,
            other => Os::Other(other.to_string()),
        }
    }
}

impl fmt::Display for Os {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Os::Linux => f.write_str("linux"),
            Os::MacOs => f.write_str("macos"),
            Os::Android => f.write_str("android"),
            Os::Other(s) => f.write_str(s),
        }
    }
}

/// What a node's binaries are built for. Placement cares because app binaries
/// are per target: a prebuilt x86-64 service is not a candidate for the Fold.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Target {
    pub arch: Arch,
    pub os: Os,
}

impl Target {
    pub fn current() -> Target {
        Target {
            arch: Arch::current(),
            os: Os::current(),
        }
    }
}

impl fmt::Display for Target {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}-{}", self.arch, self.os)
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Cpu {
    /// Hardware threads. `0` means the node did not report, not a node with
    /// no CPUs, so it is only ever used as a tiebreaker.
    pub cores: u16,
    /// Whether this node's own binaries may use AVX2.
    ///
    /// Reported from the build, not probed: the R620's Sandy Bridge Xeons
    /// lack it, builds there use `-C target-cpu=native`, and what placement
    /// needs to know is whether a binary compiled for this node can run, not
    /// what the silicon theoretically supports.
    pub avx2: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GpuApi {
    Vulkan,
    Metal,
    Cuda,
    Rocm,
    OpenCl,
    Other(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Gpu {
    pub vendor: String,
    pub model: String,
    /// `None` means the driver did not say. Treated as "a GPU that will not
    /// tell you its size", never as zero.
    pub vram_bytes: Option<u64>,
    pub apis: Vec<GpuApi>,
}

/// One codec, and what this node can actually do with it. Encode and decode
/// are tracked apart because the common shortage is encode, and `hardware`
/// apart again because a software AV1 encode on the R620 is technically a yes
/// and practically an overnight job.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodecSupport {
    pub name: String,
    pub decode: bool,
    pub encode: bool,
    pub hardware: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Storage {
    /// Free bytes where this node keeps caches and scratch. `None` means
    /// unreported.
    pub free_bytes: Option<u64>,
    pub total_bytes: Option<u64>,
}

/// What a node is able to do. Changes only when hardware or installed
/// software changes, so it is sent at registration rather than with every
/// beat.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Capabilities {
    pub target: Target,
    #[serde(default)]
    pub cpu: Cpu,
    #[serde(default)]
    pub gpus: Vec<Gpu>,
    #[serde(default)]
    pub codecs: Vec<CodecSupport>,
    #[serde(default)]
    pub storage: Storage,
    /// Runtimes present on the node; see [`RUNTIME_NODE`]. The optional Node
    /// app suite installs only where Node and glibc exist, which is a fact
    /// about the machine and therefore belongs here.
    #[serde(default)]
    pub runtimes: BTreeSet<String>,
}

impl Capabilities {
    /// What this build of this binary can say about itself without probing
    /// anything.
    ///
    /// TODO: `gpus`, `codecs` and `storage` are left empty, and `runtimes`
    /// unpopulated. Filling them means probing — Vulkan or Metal enumeration,
    /// an `ffmpeg -codecs` parse, `statvfs` on the cache directory, a PATH
    /// lookup for `node` — and none of those probes exist in this crate yet.
    /// Inventing numbers here would make placement confidently wrong, so the
    /// node daemon sets these fields explicitly until the probes land, and a
    /// node that reports nothing simply does not win GPU or codec work.
    pub fn for_this_build() -> Capabilities {
        Capabilities {
            target: Target::current(),
            cpu: Cpu {
                cores: std::thread::available_parallelism()
                    .map(|n| u16::try_from(n.get()).unwrap_or(u16::MAX))
                    .unwrap_or(0),
                avx2: cfg!(target_feature = "avx2"),
            },
            gpus: Vec::new(),
            codecs: Vec::new(),
            storage: Storage::default(),
            runtimes: BTreeSet::new(),
        }
    }

    /// Codec support by name, matched case-insensitively because manifests
    /// say `H264` and ffmpeg says `h264`.
    pub fn codec(&self, name: &str) -> Option<&CodecSupport> {
        self.codecs
            .iter()
            .find(|c| c.name.eq_ignore_ascii_case(name))
    }

    /// The largest reported VRAM across this node's GPUs.
    pub fn best_vram(&self) -> Option<u64> {
        self.gpus.iter().filter_map(|g| g.vram_bytes).max()
    }
}

// ---------------------------------------------------------------------------
// Backends and registration
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BackendKind {
    RustService,
    CliAdapter,
    NodeApp,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Lifecycle {
    /// Started when a window opens, stopped when idle.
    OnDemand,
    /// Already running and supervised on that node.
    LongRunning,
}

/// A backend a node offers, as its `app.toml` describes it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackendRecord {
    pub id: String,
    pub kind: BackendKind,
    /// Command ids this backend provides. Placement matches against these, so
    /// a command can be routed without the caller knowing which app owns it.
    pub commands: Vec<String>,
    pub lifecycle: Lifecycle,
}

impl BackendRecord {
    pub fn provides(&self, command_id: &str) -> bool {
        self.commands.iter().any(|c| c == command_id)
    }
}

/// What a node daemon sends to join.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Registration {
    pub node_id: String,
    pub host_name: String,
    pub address: MeshAddress,
    pub protocol: u32,
    pub capabilities: Capabilities,
    pub backends: Vec<BackendRecord>,
}

impl Registration {
    /// Build this machine's own registration from its node identity.
    ///
    /// The shell node registers itself through the same path as every other
    /// node, so placement has one kind of candidate rather than a special
    /// case for "here".
    pub fn for_this_node(
        install_dir: &Path,
        address: MeshAddress,
        capabilities: Capabilities,
        backends: Vec<BackendRecord>,
    ) -> io::Result<Registration> {
        Ok(Registration {
            node_id: crate::node::id(install_dir)?,
            host_name: crate::node::host_name(),
            address,
            protocol: PROTOCOL_VERSION,
            capabilities,
            backends,
        })
    }
}

/// What a node reports with a heartbeat: the parts that change minute to
/// minute, and only the parts placement actually uses as a tiebreaker.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct NodeHealth {
    pub free_memory_bytes: Option<u64>,
    /// CPU in use, in thousandths of the node's total capacity, so 1000 is
    /// every core busy. An integer on purpose: placement scores must order
    /// identically on every node, and floats invite a comparison that does
    /// not.
    pub cpu_load_permille: Option<u16>,
}

/// A node as the registry holds it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeRecord {
    /// See [`crate::node::id`]: generated once per install directory, so it
    /// survives a restore onto new hardware and never travels with the
    /// working directory.
    pub node_id: String,
    /// For showing a person which machine; never used to decide anything.
    pub host_name: String,
    pub address: MeshAddress,
    pub protocol: u32,
    pub capabilities: Capabilities,
    pub backends: Vec<BackendRecord>,
    pub first_seen: u64,
    /// Unix seconds of the last beat received **by this process**. `None`
    /// means the node is remembered from the database and has not checked in
    /// since this server started.
    pub last_heartbeat: Option<u64>,
    pub health: Option<NodeHealth>,
}

/// How alive a node looks. `Stale` and `NeverSeen` are both unusable for
/// placement but say different things to an operator, so they stay apart.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Liveness {
    Online { age_secs: u64 },
    Stale { age_secs: u64 },
    NeverSeen,
}

impl fmt::Display for Liveness {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Liveness::Online { age_secs } => write!(f, "online, last beat {age_secs}s ago"),
            Liveness::Stale { age_secs } => write!(f, "no beat for {age_secs}s"),
            Liveness::NeverSeen => f.write_str("not seen since this server started"),
        }
    }
}

impl NodeRecord {
    pub fn heartbeat_age(&self, now: u64) -> Option<u64> {
        // A beat from the future is a clock disagreement between two machines,
        // not a negative age. Clamping to zero keeps the node usable; the
        // alternative is dropping a healthy node over a few seconds of NTP.
        self.last_heartbeat.map(|at| now.saturating_sub(at))
    }

    pub fn liveness(&self, now: u64, offline_after_secs: u64) -> Liveness {
        match self.heartbeat_age(now) {
            None => Liveness::NeverSeen,
            Some(age) if age <= offline_after_secs => Liveness::Online { age_secs: age },
            Some(age) => Liveness::Stale { age_secs: age },
        }
    }

    pub fn is_online(&self, now: u64, offline_after_secs: u64) -> bool {
        matches!(
            self.liveness(now, offline_after_secs),
            Liveness::Online { .. }
        )
    }

    pub fn backend(&self, id: &str) -> Option<&BackendRecord> {
        self.backends.iter().find(|b| b.id == id)
    }

    pub fn backend_for_command(&self, command_id: &str) -> Option<&BackendRecord> {
        self.backends.iter().find(|b| b.provides(command_id))
    }
}

// ---------------------------------------------------------------------------
// Requirements
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GpuNeed {
    #[default]
    Ignored,
    /// Runs anywhere, but a GPU node is worth preferring.
    Preferred,
    Required {
        min_vram_bytes: Option<u64>,
    },
}

/// Whether the command has to run where the files are.
///
/// "Where the files are" is the node holding the working directory, which is
/// the node holding the lock, which is the shell node. Derived from identity
/// rather than reported as a capability, so a node cannot claim a working
/// directory it does not have.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkingDirNeed {
    #[default]
    Ignored,
    Preferred,
    Required,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodecNeed {
    pub name: String,
    /// `true` to encode, `false` to decode.
    pub encode: bool,
    /// Refuse a node that can only do it in software.
    pub hardware: bool,
}

impl CodecNeed {
    pub fn decode(name: impl Into<String>) -> CodecNeed {
        CodecNeed {
            name: name.into(),
            encode: false,
            hardware: false,
        }
    }

    pub fn encode(name: impl Into<String>) -> CodecNeed {
        CodecNeed {
            name: name.into(),
            encode: true,
            hardware: false,
        }
    }

    pub fn in_hardware(mut self) -> CodecNeed {
        self.hardware = true;
        self
    }
}

/// What a command needs of a node.
///
/// Built from the command definition and the owning app's manifest, not
/// guessed by the UI — the same reason risk levels are declared by the
/// backend: an adapter bug must not be able to relax a requirement.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Requirements {
    /// Command id, matched against each backend's `commands`.
    pub command: Option<String>,
    /// A specific backend, when the caller already knows which one it wants.
    pub backend: Option<String>,
    pub gpu: GpuNeed,
    pub codecs: Vec<CodecNeed>,
    pub free_storage_bytes: Option<u64>,
    pub min_free_memory_bytes: Option<u64>,
    /// Set when only a binary for one architecture exists.
    pub arch: Option<Arch>,
    pub needs_avx2: bool,
    pub working_dir: WorkingDirNeed,
    /// See [`RUNTIME_NODE`].
    pub runtimes: Vec<String>,
}

impl Requirements {
    pub fn for_command(command_id: impl Into<String>) -> Requirements {
        Requirements {
            command: Some(command_id.into()),
            ..Requirements::default()
        }
    }

    pub fn for_backend(backend_id: impl Into<String>) -> Requirements {
        Requirements {
            backend: Some(backend_id.into()),
            ..Requirements::default()
        }
    }
}

// ---------------------------------------------------------------------------
// Placement
// ---------------------------------------------------------------------------

/// Where a command will run.
///
/// **Server-internal, and intentionally not serializable.** A panel never
/// knows which node its backend runs on; it gets results over the one
/// WebSocket it already has. The command bus uses this to decide whether to
/// run locally or sign and forward.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Placement {
    pub node_id: String,
    pub host_name: String,
    pub address: MeshAddress,
    /// The backend that will serve it, when the requirements named one or a
    /// command matched one.
    pub backend: Option<String>,
    /// This is the shell node, so there is no hop and nothing to sign.
    pub local: bool,
}

/// Why one node was passed over. Collected for the error message: "nothing
/// fits" is unactionable, "the R620 has no GPU and the Fold is offline" is a
/// decision the operator can make.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Unfit {
    Offline { liveness: Liveness },
    MissingBackend { backend: String },
    MissingCommand { command: String },
    WrongArch { have: Arch, want: Arch },
    NoAvx2,
    MissingRuntime { runtime: String },
    NoGpu,
    NotEnoughVram { have: u64, need: u64 },
    MissingCodec { codec: String, encode: bool },
    SoftwareCodecOnly { codec: String },
    NotEnoughStorage { free: u64, need: u64 },
    NotEnoughMemory { free: u64, need: u64 },
    NotWorkingDirHost,
}

impl fmt::Display for Unfit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Unfit::Offline { liveness } => write!(f, "{liveness}"),
            Unfit::MissingBackend { backend } => write!(f, "does not run {backend}"),
            Unfit::MissingCommand { command } => {
                write!(f, "no backend there provides {command}")
            }
            Unfit::WrongArch { have, want } => write!(f, "is {have}, not {want}"),
            Unfit::NoAvx2 => f.write_str("has no AVX2"),
            Unfit::MissingRuntime { runtime } => write!(f, "has no {runtime}"),
            Unfit::NoGpu => f.write_str("has no GPU"),
            Unfit::NotEnoughVram { have, need } => {
                write!(f, "has {} of VRAM, needs {}", gib(*have), gib(*need))
            }
            Unfit::MissingCodec { codec, encode } => write!(
                f,
                "cannot {} {codec}",
                if *encode { "encode" } else { "decode" }
            ),
            Unfit::SoftwareCodecOnly { codec } => {
                write!(f, "has only a software {codec} codec")
            }
            Unfit::NotEnoughStorage { free, need } => {
                write!(f, "has {} free, needs {}", gib(*free), gib(*need))
            }
            Unfit::NotEnoughMemory { free, need } => {
                write!(f, "has {} of memory free, needs {}", gib(*free), gib(*need))
            }
            Unfit::NotWorkingDirHost => f.write_str("is not holding the working directory"),
        }
    }
}

/// One node and why it was passed over.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rejection {
    pub node_id: String,
    pub host_name: String,
    pub reason: Unfit,
}

#[derive(Debug, Error)]
pub enum PlacementError {
    #[error(
        "no nodes are registered; start `deckjack` on at least one machine, or register this one \
         with itself before dispatching commands"
    )]
    NoNodes,

    #[error("no node can run this: {}", summarize(.rejections))]
    NothingFits { rejections: Vec<Rejection> },
}

fn summarize(rejections: &[Rejection]) -> String {
    rejections
        .iter()
        .map(|r| format!("{} ({}) {}", r.host_name, r.node_id, r.reason))
        .collect::<Vec<_>>()
        .join("; ")
}

fn gib(bytes: u64) -> String {
    const GIB: u64 = 1 << 30;
    const MIB: u64 = 1 << 20;
    if bytes >= GIB {
        format!("{:.1} GiB", bytes as f64 / GIB as f64)
    } else {
        format!("{} MiB", bytes / MIB)
    }
}

// ---------------------------------------------------------------------------
// Persistence
// ---------------------------------------------------------------------------

/// One row of the `nodes` table.
///
/// Capabilities and backends arrive already encoded, so the store stays a
/// store: it moves rows, and the shape of a node is this module's business.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PersistedNode {
    pub node_id: String,
    pub host_name: String,
    /// `host:port`, human-readable in `sqlite3` on purpose: the first thing
    /// anyone debugging a mesh wants to see is the address.
    pub address: String,
    pub protocol: u32,
    pub capabilities: String,
    pub backends: String,
    pub first_seen: u64,
    /// When this node last completed a registration handshake. Heartbeats are
    /// deliberately not written (see the module notes), so this is the newest
    /// durable evidence of the machine, not the last beat: shown as "last
    /// registered", and never treated as liveness.
    pub last_seen: u64,
}

#[derive(Debug, Error)]
pub enum RowError {
    #[error("stored capabilities or backends are not valid JSON: {0}")]
    Json(#[from] serde_json::Error),

    #[error("stored node address is unusable: {0}")]
    Address(#[from] AddressError),
}

impl PersistedNode {
    pub fn encode(record: &NodeRecord, last_seen: u64) -> Result<PersistedNode, serde_json::Error> {
        Ok(PersistedNode {
            node_id: record.node_id.clone(),
            host_name: record.host_name.clone(),
            address: record.address.to_string(),
            protocol: record.protocol,
            capabilities: serde_json::to_string(&record.capabilities)?,
            backends: serde_json::to_string(&record.backends)?,
            first_seen: record.first_seen,
            last_seen,
        })
    }

    /// Rebuild a record. `last_heartbeat` and `health` come back empty: a
    /// remembered node is a known machine, not a reachable one.
    pub fn decode(&self) -> Result<NodeRecord, RowError> {
        Ok(NodeRecord {
            node_id: self.node_id.clone(),
            host_name: self.host_name.clone(),
            address: MeshAddress::try_from(self.address.clone())?,
            protocol: self.protocol,
            capabilities: serde_json::from_str(&self.capabilities)?,
            backends: serde_json::from_str(&self.backends)?,
            first_seen: self.first_seen,
            last_heartbeat: None,
            health: None,
        })
    }
}

/// The durable half of the registry, injected rather than reached for, so the
/// placement rules are testable without SQLite and so the single [`
/// crate::store::Store`] the server owns can be lent in under its own lock.
pub trait NodeStore: Send + Sync {
    fn upsert_node(&self, node: &PersistedNode) -> Result<(), StoreError>;
    fn load_nodes(&self) -> Result<Vec<PersistedNode>, StoreError>;
    fn delete_node(&self, node_id: &str) -> Result<(), StoreError>;
}

/// A registry that forgets its nodes when the process ends.
///
/// Stopgap, though not for the reason this said until now. It claimed the
/// `nodes` table did not exist; it does — `store.rs` creates it in
/// `MIGRATIONS[1]` and it is in every live database. The missing half is the
/// other one: `Store` does not implement [`NodeStore`], no SQL anywhere reads
/// or writes that table, and the only implementors are this type and the test
/// file's `MemStore`.
///
/// Correct for now rather than a defect. Nothing registers a node because no
/// node daemon exists, and `Bus::resolve_node` refuses a non-local node
/// instead of consulting a registry — see Phase 2 in `docs/DESIGN.md`, where
/// the inter-node transport is still an open question. Wiring persistence
/// today would store exactly one node that nothing queries.
///
/// The pointer to "this module's integration notes" is gone with it: there is
/// no such section in this file.
pub struct NoPersistence;

impl NodeStore for NoPersistence {
    fn upsert_node(&self, _node: &PersistedNode) -> Result<(), StoreError> {
        Ok(())
    }

    fn load_nodes(&self) -> Result<Vec<PersistedNode>, StoreError> {
        Ok(Vec::new())
    }

    fn delete_node(&self, _node_id: &str) -> Result<(), StoreError> {
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Registry
// ---------------------------------------------------------------------------

#[derive(Debug, Error)]
pub enum RegistryError {
    #[error(
        "node {node_id} speaks registry protocol {theirs}, this shell node speaks {ours}; \
         update the deckjack binary on whichever machine is older"
    )]
    IncompatibleProtocol {
        node_id: String,
        theirs: u32,
        ours: u32,
    },

    #[error(
        "node {node_id} sent a heartbeat without registering; it will be accepted once its \
         daemon reconnects, so check that machine's deckjack log if this repeats"
    )]
    UnknownNode { node_id: String },

    #[error(transparent)]
    Store(#[from] StoreError),
}

/// How a registration landed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Registered {
    /// A node id never seen in this working directory before.
    New,
    /// A node already known, from the database or an earlier connection.
    /// `capabilities_changed` matters to anything that cached a placement:
    /// the Fold with a dock attached is a different machine for scheduling.
    Known { capabilities_changed: bool },
}

/// Every node, and which one should run what.
///
/// Held by the shell node. Keyed in a `BTreeMap` rather than a `HashMap` so
/// iteration order is the node id order on every run: placement ties then
/// break the same way each time, which is the difference between a scheduler
/// you can reason about and one that moves work for no visible reason.
#[derive(Debug)]
pub struct Registry {
    shell_node_id: String,
    nodes: BTreeMap<String, NodeRecord>,
    offline_after_secs: u64,
}

impl Registry {
    pub fn new(shell_node_id: String) -> Registry {
        Registry {
            shell_node_id,
            nodes: BTreeMap::new(),
            offline_after_secs: OFFLINE_AFTER_SECS,
        }
    }

    /// For a mesh where beats are slower or the link is worse than the
    /// default assumes.
    pub fn with_offline_after(mut self, secs: u64) -> Registry {
        self.offline_after_secs = secs;
        self
    }

    /// Restore the nodes this working directory already knows about, all of
    /// them offline until they beat.
    pub fn load(shell_node_id: String, nodes: &dyn NodeStore) -> Result<Registry, RegistryError> {
        let mut registry = Registry::new(shell_node_id);
        for row in nodes.load_nodes()? {
            match row.decode() {
                Ok(record) => {
                    registry.nodes.insert(record.node_id.clone(), record);
                }
                // A row we cannot read is a node that will re-register the
                // moment its daemon connects, so dropping it costs nothing
                // and refusing to start costs the whole server.
                Err(e) => tracing::warn!(
                    node = %row.node_id,
                    error = %e,
                    "skipping unreadable node row"
                ),
            }
        }
        Ok(registry)
    }

    pub fn shell_node_id(&self) -> &str {
        &self.shell_node_id
    }

    pub fn offline_after_secs(&self) -> u64 {
        self.offline_after_secs
    }

    /// Accept a node, or refuse it over the protocol version.
    pub fn register(
        &mut self,
        registration: Registration,
        now: u64,
        nodes: &dyn NodeStore,
    ) -> Result<Registered, RegistryError> {
        if registration.protocol != PROTOCOL_VERSION {
            return Err(RegistryError::IncompatibleProtocol {
                node_id: registration.node_id,
                theirs: registration.protocol,
                ours: PROTOCOL_VERSION,
            });
        }

        let existing = self.nodes.get(&registration.node_id);
        let outcome = match existing {
            None => Registered::New,
            Some(previous) => Registered::Known {
                capabilities_changed: previous.capabilities != registration.capabilities,
            },
        };
        let first_seen = existing.map_or(now, |previous| previous.first_seen);

        let record = NodeRecord {
            node_id: registration.node_id,
            host_name: registration.host_name,
            address: registration.address,
            protocol: registration.protocol,
            capabilities: registration.capabilities,
            backends: registration.backends,
            first_seen,
            // Registering is itself proof of life, so the node is a candidate
            // immediately rather than after its first beat.
            last_heartbeat: Some(now),
            // Health is dropped on purpose: a node registers again because
            // its daemon restarted, and last week's load figure would be a
            // confident lie.
            health: None,
        };

        // Persistence failing is not a reason to refuse a node that is
        // demonstrably there, exactly as a sign-in is not refused because the
        // session could not be written. It is logged loudly, because a state
        // database that cannot be written is a real problem somewhere else.
        match PersistedNode::encode(&record, now) {
            Ok(row) => {
                if let Err(e) = nodes.upsert_node(&row) {
                    tracing::warn!(node = %row.node_id, error = %e, "node not persisted");
                }
            }
            Err(e) => tracing::warn!(node = %record.node_id, error = %e, "node not encodable"),
        }

        tracing::info!(
            node = %record.node_id,
            host = %record.host_name,
            address = %record.address,
            target = %record.capabilities.target,
            backends = record.backends.len(),
            "node registered"
        );
        self.nodes.insert(record.node_id.clone(), record);
        Ok(outcome)
    }

    /// Record that a node is still there.
    ///
    /// Returns how it looked *before* this beat, so a caller can log a node
    /// coming back without keeping its own copy of the previous state.
    pub fn heartbeat(
        &mut self,
        node_id: &str,
        health: Option<NodeHealth>,
        now: u64,
    ) -> Result<Liveness, RegistryError> {
        let offline_after = self.offline_after_secs;
        let node = self
            .nodes
            .get_mut(node_id)
            .ok_or_else(|| RegistryError::UnknownNode {
                node_id: node_id.to_string(),
            })?;

        let before = node.liveness(now, offline_after);
        node.last_heartbeat = Some(now);
        // A beat without health keeps the last figures rather than blanking
        // them: a daemon that cannot read its own load is still reachable.
        if let Some(health) = health {
            node.health = Some(health);
        }
        Ok(before)
    }

    /// Drop a node, from memory and from the database. `false` if it was not
    /// known here.
    pub fn forget(&mut self, node_id: &str, nodes: &dyn NodeStore) -> Result<bool, RegistryError> {
        nodes.delete_node(node_id)?;
        Ok(self.nodes.remove(node_id).is_some())
    }

    pub fn get(&self, node_id: &str) -> Option<&NodeRecord> {
        self.nodes.get(node_id)
    }

    /// Every node, live or not, in node id order.
    pub fn nodes(&self) -> impl Iterator<Item = &NodeRecord> {
        self.nodes.values()
    }

    pub fn online(&self, now: u64) -> impl Iterator<Item = &NodeRecord> {
        let offline_after = self.offline_after_secs;
        self.nodes
            .values()
            .filter(move |n| n.is_online(now, offline_after))
    }

    /// Whether any online node could run this, without committing to one.
    /// What greys out a command in the palette.
    pub fn is_available(&self, requirements: &Requirements, now: u64) -> bool {
        self.place(requirements, now).is_ok()
    }

    /// Pick the node that fits.
    ///
    /// Hard requirements filter; everything else is a preference expressed as
    /// a score, so a node that merely fits still loses to one that fits well.
    /// Ties keep the lowest node id, which makes repeated placement of the
    /// same command land on the same machine.
    pub fn place(
        &self,
        requirements: &Requirements,
        now: u64,
    ) -> Result<Placement, PlacementError> {
        if self.nodes.is_empty() {
            return Err(PlacementError::NoNodes);
        }

        let mut best: Option<(i64, Placement)> = None;
        let mut rejections = Vec::new();

        for node in self.nodes.values() {
            match self.fit(node, requirements, now) {
                Ok((score, backend)) => {
                    if best
                        .as_ref()
                        .is_none_or(|(best_score, _)| score > *best_score)
                    {
                        best = Some((
                            score,
                            Placement {
                                node_id: node.node_id.clone(),
                                host_name: node.host_name.clone(),
                                address: node.address.clone(),
                                backend,
                                local: node.node_id == self.shell_node_id,
                            },
                        ));
                    }
                }
                Err(reason) => rejections.push(Rejection {
                    node_id: node.node_id.clone(),
                    host_name: node.host_name.clone(),
                    reason,
                }),
            }
        }

        match best {
            Some((_, placement)) => Ok(placement),
            None => Err(PlacementError::NothingFits { rejections }),
        }
    }

    /// Hard checks, then a score. `Err` is why this node is out.
    fn fit(
        &self,
        node: &NodeRecord,
        requirements: &Requirements,
        now: u64,
    ) -> Result<(i64, Option<String>), Unfit> {
        let liveness = node.liveness(now, self.offline_after_secs);
        if !matches!(liveness, Liveness::Online { .. }) {
            return Err(Unfit::Offline { liveness });
        }

        let caps = &node.capabilities;
        let mut score: i64 = 0;

        if let Some(want) = &requirements.arch
            && &caps.target.arch != want
        {
            return Err(Unfit::WrongArch {
                have: caps.target.arch.clone(),
                want: want.clone(),
            });
        }

        if requirements.needs_avx2 && !caps.cpu.avx2 {
            return Err(Unfit::NoAvx2);
        }

        for runtime in &requirements.runtimes {
            if !caps.runtimes.contains(runtime) {
                return Err(Unfit::MissingRuntime {
                    runtime: runtime.clone(),
                });
            }
        }

        // A named backend wins over a command lookup: the caller that names
        // one is usually pinning follow-up work to the backend that already
        // holds the state for it.
        let backend = match (&requirements.backend, &requirements.command) {
            (Some(id), command) => {
                let backend = node.backend(id).ok_or_else(|| Unfit::MissingBackend {
                    backend: id.clone(),
                })?;
                if let Some(command) = command
                    && !backend.provides(command)
                {
                    return Err(Unfit::MissingCommand {
                        command: command.clone(),
                    });
                }
                Some(backend)
            }
            (None, Some(command)) => {
                Some(
                    node.backend_for_command(command)
                        .ok_or_else(|| Unfit::MissingCommand {
                            command: command.clone(),
                        })?,
                )
            }
            // Neither named: the caller is placing raw capability work, such
            // as a transcode that any capable node may take.
            (None, None) => None,
        };

        match requirements.gpu {
            GpuNeed::Ignored => {}
            GpuNeed::Preferred => {
                if !caps.gpus.is_empty() {
                    score += 150;
                }
            }
            GpuNeed::Required { min_vram_bytes } => {
                if caps.gpus.is_empty() {
                    return Err(Unfit::NoGpu);
                }
                match (min_vram_bytes, caps.best_vram()) {
                    (Some(need), Some(have)) if have < need => {
                        return Err(Unfit::NotEnoughVram { have, need });
                    }
                    // A GPU whose driver never reported its size is still a
                    // GPU. Rejecting it would make the answer depend on how
                    // chatty a driver is, so it scores lower and stays in.
                    (Some(_), None) => score += 50,
                    _ => score += 200,
                }
            }
        }

        for need in &requirements.codecs {
            let Some(support) = caps.codec(&need.name) else {
                return Err(Unfit::MissingCodec {
                    codec: need.name.clone(),
                    encode: need.encode,
                });
            };
            let direction_ok = if need.encode {
                support.encode
            } else {
                support.decode
            };
            if !direction_ok {
                return Err(Unfit::MissingCodec {
                    codec: need.name.clone(),
                    encode: need.encode,
                });
            }
            if need.hardware && !support.hardware {
                return Err(Unfit::SoftwareCodecOnly {
                    codec: need.name.clone(),
                });
            }
            if support.hardware {
                score += 60;
            }
        }

        // Unreported free space is not zero free space. A node that does not
        // measure its disk is a weaker candidate, not a disqualified one, so
        // the check only bites when there is a number to compare.
        if let Some(need) = requirements.free_storage_bytes
            && let Some(free) = caps.storage.free_bytes
            && free < need
        {
            return Err(Unfit::NotEnoughStorage { free, need });
        }

        if let Some(need) = requirements.min_free_memory_bytes
            && let Some(free) = node.health.and_then(|h| h.free_memory_bytes)
            && free < need
        {
            return Err(Unfit::NotEnoughMemory { free, need });
        }

        let is_working_dir_host = node.node_id == self.shell_node_id;
        match requirements.working_dir {
            WorkingDirNeed::Ignored => {}
            WorkingDirNeed::Preferred => {
                if is_working_dir_host {
                    score += 250;
                }
            }
            WorkingDirNeed::Required => {
                if !is_working_dir_host {
                    return Err(Unfit::NotWorkingDirHost);
                }
                score += 250;
            }
        }

        // Staying on the shell node costs no hop and no signature, so it wins
        // all else being equal.
        if is_working_dir_host {
            score += 100;
        }

        // A service already running and supervised there starts instantly;
        // an on-demand backend pays a cold start.
        if let Some(backend) = backend
            && backend.lifecycle == Lifecycle::LongRunning
        {
            score += 80;
        }

        // Tiebreakers only. Bounded as a *group*: the whole block spans
        // -20..=+32, which is narrower than the smallest preference above
        // (+60 for a hardware codec), so no amount of spare capacity can
        // outrank a node that actually has the hardware. Bounding them one by
        // one is not enough — a full-width core count and a full-width memory
        // figure used to sum to +128 against a loaded GPU node's 150-100=50,
        // which sent GPU-preferred work to the machine with no GPU.
        score += i64::from(caps.cpu.cores.min(64)) / 4;
        if let Some(health) = node.health {
            if let Some(load) = health.cpu_load_permille {
                score -= i64::from(load.min(1_000)) / 50;
            }
            if let Some(free) = health.free_memory_bytes {
                score += ((free >> 30) as i64).min(64) / 4;
            }
        }

        Ok((score, backend.map(|b| b.id.clone())))
    }
}
