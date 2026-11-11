//! The Deckjack app protocol.
//!
//! Everything an app binary and the core must agree on, and nothing else: the
//! manifest, the frames, the version numbers. An app depends on this crate
//! and not on `deckjack-core`, which is the point — a core carries SQLite,
//! axum, Landlock and a web shell, and an app that linked all of it would pay
//! for a server it never runs. Nine apps paying for it is how a desktop stops
//! fitting on a phone.
//!
//! # License
//!
//! Permissive (MIT OR Apache-2.0) while the rest of Deckjack is AGPL-3.0.
//! That is deliberate and it is what docs/DESIGN.md says to do: "Separate
//! programs talking over a socket are generally separate works, but plugins
//! exchanging complex internal structures can be argued to be one combined
//! program, so publish the app protocol under a permissive license or add an
//! explicit exception if outside apps are expected." An app that links only
//! this crate is unambiguously its own work.
//!
//! # Compatibility
//!
//! [`PROTOCOL_VERSION`] is what this build speaks and
//! [`MIN_PROTOCOL_VERSION`] is the oldest it will talk to. The core checks
//! both at discovery, before anything is launched, and refuses a mismatch
//! with a message naming both numbers. Nothing is started optimistically to
//! find out.
//!
//! ## How protocol 1 keeps working at protocol 2
//!
//! Protocol 2 adds the invocation frames ([`AppFrame::Invoke`],
//! [`AppFrame::Commands`] and their answers) and the `invoke_commands`
//! capability in `[needs]`. Everything from protocol 1 is unchanged, and
//! three separate facts keep an app built against 1 running:
//!
//! 1. **The handshake is byte-identical.** [`Hello`] and [`Registration`]
//!    have the fields they had, so the exchange a protocol-1 binary performs
//!    is the exchange this core performs. [`MIN_PROTOCOL_VERSION`] is still
//!    1, so [`check_protocol`] accepts that app's manifest at discovery and
//!    its registration at handshake.
//! 2. **The core sends no new frame unprompted.** [`CoreFrame::Invoked`] and
//!    [`CoreFrame::Available`] are only ever answers to a frame the app sent
//!    first, and a protocol-1 binary has no variant to send. So a protocol-1
//!    app never has to decode a frame its `serde` enum does not know.
//! 3. **An old core refuses a new app before launching it.** An app that uses
//!    the new frames declares `protocol = 2`, which a protocol-1 core rejects
//!    at discovery with [`ProtocolError::Incompatible`] rather than launching
//!    it and dropping the connection on the first frame it cannot read. That
//!    is the reason the number is bumped instead of the frames being smuggled
//!    in under the old one.
//!
//! The one thing that does **not** follow automatically is the number in
//! [`Hello`]. An app validates it against its own compiled window, so a core
//! announcing 2 to a binary whose window is `1..=1` is refused *by the app*.
//! [`negotiated`] is what a core should send there: the highest version both
//! halves have in common, which is 1 for a protocol-1 app and 2 for a
//! protocol-2 one. A core that sends a bare [`PROTOCOL_VERSION`] in `Hello`
//! is announcing a version it cannot know the app accepts.

use std::io;
use std::path::{Path, PathBuf};

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// The app protocol this build speaks.
///
/// Bumped whenever a frame changes shape or a manifest key changes meaning.
/// 2 added the invocation frames and the `invoke_commands` capability; see
/// the crate doc for what makes a protocol-1 app keep working.
pub const PROTOCOL_VERSION: u32 = 2;

/// The protocol a manifest must declare to use [`AppFrame::Invoke`].
///
/// Checked while the manifest is read, so an app asking for a capability its
/// own frames cannot express is refused at discovery rather than at the
/// moment it sends a frame the core will not answer.
pub const INVOKE_PROTOCOL: u32 = 2;

/// The oldest protocol this build will talk to.
pub const MIN_PROTOCOL_VERSION: u32 = 1;

/// The highest protocol a core and one app have in common.
///
/// What belongs in [`Hello::protocol`], and the reason is asymmetric: the
/// core checks the app's number against a *range* it supports, while the app
/// checks the core's number against the range *it* was compiled with. So the
/// core is the half that has to speak down. `negotiated(1)` is 1, which is
/// exactly what a binary built against protocol 1 will accept.
///
/// Clamped up to [`MIN_PROTOCOL_VERSION`] rather than returning something
/// below it: an app declaring protocol 0 is refused by [`check_protocol`] at
/// discovery, and this should not be the function that invents a version
/// number nobody speaks.
pub fn negotiated(app_protocol: u32) -> u32 {
    // `clamp` panics when the bounds cross. They are two constants in this
    // file with MIN below MAX, so that is a compile-time fact rather than
    // something to guard at runtime.
    app_protocol.clamp(MIN_PROTOCOL_VERSION, PROTOCOL_VERSION)
}

/// A frame larger than this is refused before it is parsed, so one app cannot
/// make the core allocate without bound.
pub const MAX_FRAME_BYTES: usize = 8 * 1024 * 1024;

/// The manifest filename inside an app's directory.
pub const MANIFEST_FILE: &str = "app.toml";

/// Every app binary is `deckjack-<name>`, like a `git-*` subcommand.
pub const BINARY_PREFIX: &str = "deckjack-";

/// The asset an app serves for `/apps/<name>/`.
pub const UI_INDEX: &str = "index.html";

/// The working directory's hidden state directory.
///
/// Named here, not only in the core, because a manifest that asks for a path
/// reaching into it is refused while the manifest is being read — before any
/// process exists to be confined. The core's sandbox denies it again; this is
/// the earlier of the two checks, not a substitute for it.
pub const HIDDEN_DIR: &str = ".deckjack";

// ---------------------------------------------------------------------------
// The manifest
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------

/// `app.toml`: what an app is, what it provides and what it needs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub app: AppInfo,
    #[serde(default)]
    pub needs: Needs,
    #[serde(default)]
    pub commands: Vec<CommandDecl>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AppInfo {
    /// Directory name, binary suffix, URL segment and command namespace, all
    /// at once. Hence the narrow character set enforced in [`valid_name`].
    pub name: String,
    pub version: String,
    /// The app protocol the binary was built against.
    pub protocol: u32,
    /// What the dock and the window chrome show. Defaults to the name.
    #[serde(default)]
    pub title: Option<String>,
    /// Lifecycle lives here rather than under `[needs]` because it is not a
    /// permission the operator grants: changing it does not invalidate an
    /// approval, where changing `[needs]` does.
    #[serde(default)]
    pub lifecycle: Lifecycle,
    /// Narrowest width the app's compact layout works at, per the window
    /// manager's app contract.
    #[serde(default)]
    pub min_width: Option<u32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Lifecycle {
    /// Started when a window opens, stopped when it has been idle.
    #[default]
    OnDemand,
    /// Started at core startup and kept running: PIM sync, reminders, the
    /// assistant. Stopping these on idle would stop the work they exist for.
    Service,
}

/// What an app asked for. Approved as a whole, fingerprinted as a whole.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Needs {
    pub gpu: bool,
    /// Hosts the app may reach, as an allowlist and not a hint: empty means
    /// no network at all, and [`LaunchPlan::build`] enforces that.
    ///
    /// The host *names* are a different matter. Landlock's network rules are
    /// per-port, so an allowlist of names cannot be installed as written and
    /// a non-empty list opens the network as a whole — reported at warn on
    /// every launch, see [`Sandbox::network_is_coarse`].
    pub network: Vec<String>,
    /// Parts of the working directory the app may see.
    pub working_dir: Vec<DirNeed>,
    /// Whether the app may **call** commands on the bus, not merely serve
    /// them. Every app can serve; this is the other direction, and it is a
    /// capability rather than a given because the bus is the whole machine.
    ///
    /// An app confined to one folder that could dispatch anything on the bus
    /// would be confined only until it called `terminal.open`, which is a
    /// shell for a manifest that asked to read a Notes folder. So the frames
    /// and this bit landed in one change, and the core refuses
    /// [`AppFrame::Invoke`] from an app whose manifest does not set it — see
    /// [`INVOKE_CAPABILITY`].
    ///
    /// It is not an authorization: declaring it only lets the app *ask*. What
    /// each app may actually run is the agent grant for its session, checked
    /// by the bus on every dispatch.
    ///
    /// Skipped when false, unlike the other fields, and that is deliberate:
    /// [`Manifest::fingerprint`] serializes this struct whole, so writing
    /// `"invoke_commands":false` into the digest would have changed the
    /// fingerprint of every app already installed and re-asked the operator
    /// to approve three apps that had not changed what they want. Absent and
    /// `false` are the same statement; turning it on still changes the digest,
    /// which is the case the fingerprint exists for.
    #[serde(skip_serializing_if = "is_false")]
    pub invoke_commands: bool,
}

/// `[needs]` key for [`Needs::invoke_commands`], named once so a refusal can
/// quote the thing the operator has to add.
pub const INVOKE_CAPABILITY: &str = "invoke_commands";

fn is_false(b: &bool) -> bool {
    !*b
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DirNeed {
    /// Relative to the working directory.
    pub path: String,
    #[serde(default)]
    pub write: bool,
}

impl Needs {
    /// Nothing beyond serving the bus. Such an app has nothing for the
    /// operator to approve, so [`Host`] launches it without a grant rather
    /// than manufacturing a consent dialog with an empty list in it.
    ///
    /// [`Needs::invoke_commands`] counts here, which is the point of putting
    /// it in this struct: an app with an otherwise empty `[needs]` launches
    /// with no approval at all, so a capability that did not make this
    /// `false` would be a capability nobody was ever asked about.
    pub fn is_empty(&self) -> bool {
        !self.gpu && !self.invoke_commands && self.network.is_empty() && self.working_dir.is_empty()
    }
}

/// A command as the manifest declares it. The authoritative parameter schema
/// arrives at handshake; this is the surface the operator approved.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommandDecl {
    /// Must start with `<app>.`, so an app cannot declare `system.shutdown`
    /// and have the palette offer it.
    pub id: String,
    pub summary: String,
    pub risk: Risk,
}

/// Risk level, declared by the backend and never inferred by the UI, so an
/// adapter bug cannot quietly downgrade a destructive command.
///
/// TODO: re-export the command bus's risk type once that module exists,
/// rather than keeping two enums that must agree. This one is defined here
/// because the app host has to parse `app.toml` whether or not the bus is
/// wired yet; the integrator should swap it for an import, not translate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Risk {
    /// Runs immediately, for anyone.
    Read,
    /// Runs under the caller's grant.
    Write,
    /// Waits in the Assistant's Inbox when an agent asks.
    Destructive,
    /// Leaves the machine; also Inbox-gated for agents.
    Outbound,
}

impl Risk {
    /// Whether an agent's call must wait for approval. The operator's own
    /// calls get an ordinary confirmation instead.
    pub fn needs_inbox(&self) -> bool {
        matches!(self, Risk::Destructive | Risk::Outbound)
    }
}

#[derive(Debug, Error)]
pub enum ManifestError {
    #[error("cannot read {path}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: io::Error,
    },

    #[error("{path} is not valid app.toml: {source}")]
    Parse {
        path: PathBuf,
        #[source]
        source: toml::de::Error,
    },

    #[error("{path}: {message}")]
    Invalid { path: PathBuf, message: String },
}

impl Manifest {
    /// Validate one `app.toml`'s text.
    ///
    /// `path` is only ever used to name the file in an error. Reading it is
    /// the core's job, because reading goes through its injected `Env` — see
    /// `deckjack_core::apps::load_manifest`.
    pub fn parse(path: &Path, text: &str) -> Result<Manifest, ManifestError> {
        let manifest: Manifest = toml::from_str(text).map_err(|source| ManifestError::Parse {
            path: path.to_path_buf(),
            source,
        })?;
        manifest.validate(path)?;
        Ok(manifest)
    }

    pub fn title(&self) -> &str {
        self.app.title.as_deref().unwrap_or(&self.app.name)
    }

    /// Everything here is a rule the rest of the module then gets to assume.
    fn validate(&self, path: &Path) -> Result<(), ManifestError> {
        let bad = |message: String| ManifestError::Invalid {
            path: path.to_path_buf(),
            message,
        };

        if !valid_name(&self.app.name) {
            return Err(bad(format!(
                "app.name {:?} is not usable: a name is 1-32 characters of a-z, 0-9 and \
                 '-', starting with a letter. It is the directory, the binary suffix, the \
                 URL segment and the command namespace, so it cannot be anything else",
                self.app.name
            )));
        }
        if self.app.version.trim().is_empty() {
            return Err(bad(
                "app.version is empty; set it to the build's version".into()
            ));
        }

        // The capability is useless without the frames that carry it, and an
        // app that declared one and not the other would be approved for bus
        // access and then refused on its first invocation — a permission the
        // operator granted and the app cannot use. Said here, while the
        // manifest is read, so it is a discovery error naming both numbers.
        if self.needs.invoke_commands && self.app.protocol < INVOKE_PROTOCOL {
            return Err(bad(format!(
                "needs.{INVOKE_CAPABILITY} requires app.protocol {INVOKE_PROTOCOL} or later, \
                 and this manifest says {}: the frames that invoke a command do not exist in \
                 protocol 1, so the capability could never be used",
                self.app.protocol
            )));
        }

        let mut seen: Vec<&str> = Vec::new();
        for command in &self.commands {
            let namespace = format!("{}.", self.app.name);
            if !command.id.starts_with(&namespace) {
                return Err(bad(format!(
                    "command {:?} must be named {namespace}<something>: an app may only \
                     declare commands in its own namespace, or installing it could shadow \
                     another app's command",
                    command.id
                )));
            }
            if command.id.len() == namespace.len() {
                return Err(bad(format!(
                    "command id {:?} has no name after the namespace",
                    command.id
                )));
            }
            if seen.contains(&command.id.as_str()) {
                return Err(bad(format!("command {:?} is declared twice", command.id)));
            }
            if command.summary.trim().is_empty() {
                return Err(bad(format!(
                    "command {:?} has no summary; it is what the palette shows",
                    command.id
                )));
            }
            seen.push(&command.id);
        }

        for need in &self.needs.working_dir {
            let as_path = Path::new(&need.path);
            if need.path.trim().is_empty() {
                return Err(bad("needs.working_dir has an empty path".into()));
            }
            if as_path.is_absolute() {
                return Err(bad(format!(
                    "needs.working_dir path {:?} is absolute; paths are relative to the \
                     working directory, which is the only thing an app may be granted",
                    need.path
                )));
            }
            if as_path
                .components()
                .any(|c| matches!(c, std::path::Component::ParentDir))
            {
                return Err(bad(format!(
                    "needs.working_dir path {:?} contains '..', which would reach outside \
                     the working directory",
                    need.path
                )));
            }
            if as_path.components().any(|c| c.as_os_str() == HIDDEN_DIR) {
                return Err(bad(format!(
                    "needs.working_dir path {:?} reaches into {HIDDEN_DIR}/. No app ever \
                     sees it: the core is the only process that opens state.db, and apps \
                     reach state through the bus",
                    need.path
                )));
            }
            // `.` and `./.` have no parent component and no hidden component,
            // so every check above passes while the grant is the whole
            // working directory — including the hidden directory inside it,
            // because Landlock grants the tree beneath a path. The launcher
            // refuses this again at build time; it is rejected here as well so
            // the operator is told while reading the manifest rather than
            // while failing to open a window.
            if !as_path
                .components()
                .any(|c| matches!(c, std::path::Component::Normal(_)))
            {
                return Err(bad(format!(
                    "needs.working_dir path {:?} is the working directory itself, which \
                     would grant its whole tree including {HIDDEN_DIR}/. Name the \
                     subdirectories the app actually needs",
                    need.path
                )));
            }
        }

        for host in &self.needs.network {
            if host.trim().is_empty() || host.contains('/') {
                return Err(bad(format!(
                    "needs.network entry {host:?} is not a host name; list hosts, not URLs"
                )));
            }
        }

        Ok(())
    }

    /// A stable digest of everything the operator approved: the `[needs]`
    /// table and the command surface with its risk levels.
    ///
    /// An app that gains a network host, turns a read command into a
    /// destructive one, or starts asking for [`Needs::invoke_commands`] gets
    /// a different fingerprint and so needs approving again — the whole
    /// `[needs]` table is hashed, so a capability added to it is inside the
    /// digest without this function naming it. Version and lifecycle are
    /// deliberately excluded: a patch release that asks for nothing new
    /// should not nag.
    ///
    /// Hashed with FNV-1a rather than [`std::hash::DefaultHasher`], whose
    /// output is explicitly not stable between Rust releases — a fingerprint
    /// that changed when the compiler did would revoke every grant on an
    /// upgrade.
    pub fn fingerprint(&self) -> String {
        let mut commands: Vec<(&str, Risk)> = self
            .commands
            .iter()
            .map(|c| (c.id.as_str(), c.risk))
            .collect();
        commands.sort();

        // These types contain no maps, floats or non-string keys, so
        // serialization cannot fail and the field order is the declaration
        // order. That is what makes the digest reproducible.
        let canonical = serde_json::json!({
            "needs": &self.needs,
            "commands": commands,
        });
        let text = serde_json::to_string(&canonical).unwrap_or_default();
        format!("fnv1a64:{:016x}", fnv1a64(text.as_bytes()))
    }
}

fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// The one place the character set for an app name is decided.
pub fn valid_name(name: &str) -> bool {
    if name.is_empty() || name.len() > 32 {
        return false;
    }
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !first.is_ascii_lowercase() {
        return false;
    }
    if name.ends_with('-') {
        return false;
    }
    name.chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

// ---------------------------------------------------------------------------
// Frames
// ---------------------------------------------------------------------------

/// Sent by the core as the first frame on a new connection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hello {
    pub protocol: u32,
    pub core_version: String,
    /// Which app the core thinks it is talking to. An app that finds its own
    /// name wrong here is looking at someone else's socket.
    pub app: String,
}

/// The app's answer: what it is and what it provides.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Registration {
    pub protocol: u32,
    pub app: String,
    pub version: String,
    pub commands: Vec<RegisteredCommand>,
}

/// A command as registered at handshake, with the JSON Schema the palette,
/// the generated TypeScript types and the agent tools are all built from.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RegisteredCommand {
    pub id: String,
    pub summary: String,
    pub risk: Risk,
    /// JSON Schema for the parameters. Must be a schema object; an app that
    /// sends a bare `true` or a string leaves the palette with nothing to
    /// render and the bus with nothing to validate against.
    pub params: serde_json::Value,
}

/// Core to app.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "frame", rename_all = "kebab-case")]
pub enum CoreFrame {
    Hello(Hello),
    /// Dispatch a command. `id` correlates the [`AppFrame::Result`].
    Call {
        id: u64,
        command: String,
        params: serde_json::Value,
    },
    Cancel {
        id: u64,
    },
    /// Fetch a UI asset. The browser's request for `/apps/<name>/<path>`
    /// becomes this; the app answers from its embedded assets.
    Ui {
        id: u64,
        path: String,
    },
    /// Shut down cleanly. Followed by SIGTERM if it is ignored.
    Stop,
    /// The answer to one [`AppFrame::Invoke`]. `call` is the app's number,
    /// echoed back; see [`AppFrame::Invoke`] for why it is not `id`.
    Invoked {
        call: u64,
        outcome: InvokeOutcome,
    },
    /// The answer to one [`AppFrame::Commands`]: what this app may call,
    /// already filtered by its grant.
    ///
    /// A `Commands` frame from an app with no capability is answered with
    /// [`CoreFrame::Invoked`] carrying [`InvokeOutcome::Refused`] instead. An
    /// empty list would read as "the bus has nothing for you" and send an app
    /// author hunting for missing commands rather than a missing capability.
    Available {
        call: u64,
        commands: Vec<AvailableCommand>,
    },
}

/// What came of one [`AppFrame::Invoke`].
///
/// Four answers rather than `Result`, because an app can act differently on
/// each: [`InvokeOutcome::Refused`] is an install-level problem to report to
/// the operator and never retry, [`InvokeOutcome::Queued`] means a person has
/// to decide and the app should carry on with other work, and
/// [`InvokeOutcome::Failed`] is the ordinary per-call error.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "kebab-case")]
pub enum InvokeOutcome {
    /// It ran. `value` is whatever the command returned, `null` for a command
    /// with no result.
    Ok { value: serde_json::Value },
    /// Nothing ran: it is waiting for the operator in the Inbox. Retrying is
    /// pointless and calling again only queues a second copy.
    Queued { approval: String },
    /// Not allowed, and asking again will not change it: the capability is
    /// not declared, the app's session has no grant, or the command is above
    /// the grant's ceiling.
    Refused { message: String },
    /// It ran and failed, or the parameters were not what the command takes.
    Failed { message: String },
}

/// One command an app may call, as the core reports it.
///
/// Generated from the live registry and filtered by the app's own grant, so
/// an app is not told about commands it would be refused for calling.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AvailableCommand {
    /// The dotted bus id to put in [`AppFrame::Invoke`].
    pub command: String,
    pub title: String,
    /// Everything the core has to say about it, including the sentence about
    /// waiting for approval when [`AvailableCommand::risk`] is gated.
    pub description: String,
    /// JSON Schema for the parameters.
    pub params: serde_json::Value,
    pub risk: CallRisk,
}

/// What calling a command costs, as the *bus* grades it.
///
/// Deliberately not [`Risk`]. `Risk` is what an app declares about commands
/// it serves, and the two sets are not the same: the bus has a cross-project
/// level that no manifest can declare, and collapsing it onto `Destructive`
/// here would tell an app the wrong reason for a wait.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CallRisk {
    /// Runs immediately. Reads state and changes nothing.
    ReadOnly,
    /// Runs immediately under the app's own grant.
    Write,
    /// Waits in the operator's Inbox because it removes or overwrites data.
    GatedDestructive,
    /// Waits because it leaves the machine.
    GatedOutbound,
    /// Waits because it reaches outside what the app's session was given.
    GatedCrossProject,
}

impl CallRisk {
    /// Whether calling this will wait for a person. Worth asking before
    /// building a plan around the answer arriving.
    pub fn waits_for_the_operator(self) -> bool {
        match self {
            CallRisk::ReadOnly | CallRisk::Write => false,
            CallRisk::GatedDestructive | CallRisk::GatedOutbound | CallRisk::GatedCrossProject => {
                true
            }
        }
    }
}

/// App to core.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "frame", rename_all = "kebab-case")]
pub enum AppFrame {
    Register(Registration),
    Result {
        id: u64,
        #[serde(default)]
        result: Option<serde_json::Value>,
        #[serde(default)]
        error: Option<String>,
    },
    Progress {
        id: u64,
        #[serde(default)]
        fraction: Option<f32>,
        message: String,
    },
    Ui {
        id: u64,
        /// Absent means the app has no such asset: a 404, not a failure.
        #[serde(default)]
        asset: Option<WireAsset>,
    },
    Log {
        level: String,
        message: String,
    },
    /// Run a bus command on the app's behalf. Answered with exactly one
    /// [`CoreFrame::Invoked`] carrying the same `call`.
    ///
    /// Refused unless the app's manifest declares
    /// [`Needs::invoke_commands`]: without that, adding this frame would have
    /// handed every installed app the whole bus, `terminal.open` included,
    /// on the strength of a manifest that asked to read one folder.
    ///
    /// # Why `call` and not `id`
    ///
    /// `id` numbers core-to-app calls and is allocated by the core's pump.
    /// `call` numbers app-to-core invocations and is allocated by the app.
    /// They are separate spaces on purpose, and the field name differs so a
    /// frame can never be read against the wrong one.
    ///
    /// Sharing one space would mean two allocators handing out numbers from
    /// it with no coordination: the core's `Call{id:1}` and the app's
    /// `Invoke{call:1}` would both be "operation 1" while being different
    /// operations in opposite directions. The first answer to arrive would be
    /// matched against whichever table was consulted first, so a command's
    /// result could be delivered to an invocation's waiter — a wrong value
    /// returned as if it were right, which is worse than an error. Keeping
    /// two spaces makes that unrepresentable rather than unlikely.
    Invoke {
        call: u64,
        command: String,
        params: serde_json::Value,
    },
    /// Ask what this app may call. Answered with one
    /// [`CoreFrame::Available`].
    ///
    /// Needs the same capability as [`AppFrame::Invoke`]: the list is built
    /// from the live registry, and an app with no business calling anything
    /// has no business enumerating the machine's commands either.
    Commands {
        call: u64,
    },
}

/// A UI asset on the wire. Frames are JSON, so the bytes are base64; the
/// app, which knows its own build, states the content type rather than
/// leaving the core to guess from an extension.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WireAsset {
    pub content_type: String,
    pub body: String,
    /// True for build-hashed files, which may be cached forever. `index.html`
    /// is never one of them.
    #[serde(default)]
    pub immutable: bool,
}

/// A UI asset once decoded, ready to be a response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UiAsset {
    pub content_type: String,
    pub bytes: Vec<u8>,
    pub immutable: bool,
}

impl WireAsset {
    /// Wrap bytes for the wire. The app states the content type, because the
    /// app is the half that knows its own build.
    pub fn new(content_type: impl Into<String>, bytes: &[u8], immutable: bool) -> WireAsset {
        use base64::Engine as _;
        WireAsset {
            content_type: content_type.into(),
            body: base64::engine::general_purpose::STANDARD.encode(bytes),
            immutable,
        }
    }

    pub fn decode(&self) -> Result<UiAsset, ProtocolError> {
        use base64::Engine as _;
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(self.body.as_bytes())
            .map_err(|e| ProtocolError::Malformed(format!("UI asset is not base64: {e}")))?;
        Ok(UiAsset {
            content_type: self.content_type.clone(),
            bytes,
            immutable: self.immutable,
        })
    }
}

#[derive(Debug, Error)]
pub enum ProtocolError {
    #[error(
        "app \"{app}\" was built against app protocol {found}, and this Deckjack speaks \
         {min}..={max}. Run `deckjack install {app}` to fetch a build for this core, or \
         upgrade Deckjack if the app is the newer half"
    )]
    Incompatible {
        app: String,
        found: u32,
        min: u32,
        max: u32,
    },

    #[error("a frame from the app was {size} bytes, over the {MAX_FRAME_BYTES}-byte limit")]
    FrameTooLong { size: usize },

    #[error("the app closed the connection")]
    Closed,

    #[error("the app sent something this core cannot read: {0}")]
    Malformed(String),

    #[error("socket error: {0}")]
    Io(#[from] io::Error),
}

/// Whether an app's protocol number can be talked to at all.
///
/// Called at discovery, before anything is launched: refusing after a spawn
/// would mean a process has already run with the operator's grants to learn
/// what could have been read off disk.
pub fn check_protocol(app: &str, found: u32) -> Result<(), ProtocolError> {
    if (MIN_PROTOCOL_VERSION..=PROTOCOL_VERSION).contains(&found) {
        Ok(())
    } else {
        Err(ProtocolError::Incompatible {
            app: app.to_string(),
            found,
            min: MIN_PROTOCOL_VERSION,
            max: PROTOCOL_VERSION,
        })
    }
}

pub fn encode_frame<T: Serialize>(frame: &T) -> Result<String, ProtocolError> {
    let mut line = serde_json::to_string(frame)
        .map_err(|e| ProtocolError::Malformed(format!("cannot encode frame: {e}")))?;
    line.push('\n');
    Ok(line)
}

pub fn decode_frame<T: DeserializeOwned>(line: &str) -> Result<T, ProtocolError> {
    if line.len() > MAX_FRAME_BYTES {
        return Err(ProtocolError::FrameTooLong { size: line.len() });
    }
    serde_json::from_str(line).map_err(|e| ProtocolError::Malformed(e.to_string()))
}
