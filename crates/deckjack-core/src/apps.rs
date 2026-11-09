//! The app host.
//!
//! Apps are separate programs: one `deckjack-<name>` executable per app under
//! the install directory, each carrying its own backend, its own embedded web
//! UI and an `app.toml` manifest. This module is the core side of that
//! arrangement — the five steps in docs/DESIGN.md, "Installation and app
//! binaries":
//!
//! 1. **Discovery.** [`discover`] scans `<install>/apps/*/app.toml` at
//!    startup. A broken app becomes a [`Problem`], never a failed start.
//! 2. **Launch.** [`Host::start_services`] starts long-running apps;
//!    [`Host::open_window`] starts an on-demand app when a window asks for
//!    it, and [`Host::poll`] stops it again once it has been idle.
//! 3. **Handshake.** [`accept_handshake`] over a Unix socket: the core sends
//!    [`Hello`], the app answers [`Registration`] with its commands as JSON
//!    Schema, and [`accept_registration`] decides whether to believe it.
//! 4. **UI routing.** [`ui_router`], nested at [`UI_MOUNT`], serves each
//!    app's UI at `/apps/<name>/` on the core's own origin. The browser never
//!    holds an app's address and never opens a socket to one; sign-in,
//!    theming and the window manager stay unified because every byte goes
//!    through here.
//! 5. **Supervision.** [`Host::poll`] notices a dead child, restarts it with
//!    backoff, and reports [`HostEvent::Reconnecting`] so the window shows
//!    "reconnecting" rather than the desktop going down with the app.
//!
//! **Permissions are enforced, not advertised.** [`LaunchPlan::build`]
//! translates a manifest's `[needs]` into a [`sandbox::Policy`], and
//! [`OsEnv::spawn`] starts the child through [`sandbox::command`], which
//! confines it between fork and exec. A node whose kernel cannot enforce what
//! a manifest asked for refuses to launch the app rather than running it with
//! the operator's own access — see [`ConfinementError::Unenforceable`]. There
//! is exactly one way to run an app unconfined,
//! [`Confinement::unconfined_for_development`], it cannot be reached from a
//! manifest or a config file, and it logs a warning on every launch.
//!
//! **Protocol versioning is enforced from the first commit**, as the design
//! says to decide early: [`PROTOCOL_VERSION`] is what this core speaks, and an
//! app built against anything else is refused at discovery with a message
//! naming both numbers and what to do about it. Nothing is launched
//! optimistically to find out.
//!
//! Everything that touches the clock, the filesystem or processes goes
//! through [`Env`], so the whole state machine — crash loops, backoff, idle
//! stops, handshake timeouts — is exercised without spawning or sleeping.
//!
//! ```toml
//! # ~/.deckjack/apps/pim/app.toml
//! [app]
//! name = "pim"
//! version = "0.3.1"
//! protocol = 1
//! title = "Calendar and contacts"
//! lifecycle = "service"        # or "on-demand", the default
//!
//! [needs]
//! gpu = false
//! network = ["cloud.zaremski.net"]
//! working_dir = [{ path = "Calendars", write = true }]
//!
//! [[commands]]
//! id = "pim.event.create"
//! summary = "Create a calendar event"
//! risk = "write"
//! ```

use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::Router;
use axum::extract::{Path as UrlPath, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::get;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::sandbox;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// The app protocol this core speaks: handshake shape, frame shape and the
/// meaning of a command declaration, all as one number.
///
/// It is a single integer rather than a semver range because the only
/// question that matters at launch is "can these two talk". Additive changes
/// that an older app survives raise [`MIN_PROTOCOL_VERSION`] not at all;
/// anything an older app would misread raises both.
pub const PROTOCOL_VERSION: u32 = 1;

/// The oldest protocol this core still accepts.
pub const MIN_PROTOCOL_VERSION: u32 = 1;

/// Apps live here, relative to the install directory (never the working
/// directory: binaries are built for one machine and must not travel).
pub const APPS_SUBDIR: &str = "apps";

/// Runtime sockets live here, relative to the install directory. Also not the
/// working directory — a socket that got synced to another machine would be a
/// door to a process that was never there.
pub const RUN_SUBDIR: &str = "run";

/// Per-app scratch space lives here, relative to the install directory.
///
/// An app's durable state belongs on the bus, so a directory the operator may
/// delete at any time is the honest home for the rest of it.
pub const CACHE_SUBDIR: &str = "cache";

/// The manifest file inside each app directory.
pub const MANIFEST_FILE: &str = "app.toml";

/// Every app binary is named this plus the app name, discovered the way git
/// finds `git-*` subcommands.
pub const BINARY_PREFIX: &str = "deckjack-";

/// Where [`ui_router`] expects to be nested.
pub const UI_MOUNT: &str = "/apps";

/// What `/apps/<name>/` resolves to.
pub const UI_INDEX: &str = "index.html";

/// Longest line the core will accept from an app.
///
/// Frames are newline-delimited JSON, so one frame is one line. Apps are
/// local, installed and approved rather than remote peers, which is the only
/// reason the limit may be checked after the line has been read instead of
/// while reading it.
pub const MAX_FRAME_BYTES: usize = 8 * 1024 * 1024;

/// This core's version, sent in [`Hello`] so an app can log what it is
/// talking to. Never used to decide compatibility — [`PROTOCOL_VERSION`] is.
pub const CORE_VERSION: &str = env!("CARGO_PKG_VERSION");

/// The hidden directory no app may ever be granted a path into.
const HIDDEN_DIR: &str = ".deckjack";

// ---------------------------------------------------------------------------
// Manifest
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
    /// Nothing beyond the bus. Such an app has nothing for the operator to
    /// approve, so [`Host`] launches it without a grant rather than
    /// manufacturing a consent dialog with an empty list in it.
    pub fn is_empty(&self) -> bool {
        !self.gpu && self.network.is_empty() && self.working_dir.is_empty()
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
    /// Read and validate one `app.toml`.
    pub fn load(path: &Path, env: &dyn Env) -> Result<Manifest, ManifestError> {
        let text = env
            .read_to_string(path)
            .map_err(|source| ManifestError::Read {
                path: path.to_path_buf(),
                source,
            })?;
        let manifest: Manifest = toml::from_str(&text).map_err(|source| ManifestError::Parse {
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
    /// An app that gains a network host or turns a read command into a
    /// destructive one gets a different fingerprint and so needs approving
    /// again. Version and lifecycle are deliberately excluded: a patch
    /// release that asks for nothing new should not nag.
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
// Protocol
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

#[derive(Debug, Error)]
pub enum HandshakeError {
    #[error(transparent)]
    Protocol(#[from] ProtocolError),

    #[error(
        "the app on {expected}'s socket registered as \"{found}\". Something is listening \
         on another app's socket; nothing was registered"
    )]
    WrongApp { expected: String, found: String },

    #[error(
        "{app}'s binary reports version {binary}, its app.toml says {manifest}. An update \
         swapped one and not the other; reinstall with `deckjack install {app}`"
    )]
    VersionMismatch {
        app: String,
        binary: String,
        manifest: String,
    },

    #[error(
        "{app} registered command {command:?}, which its app.toml does not declare. The \
         manifest is what was approved, so the command was not registered"
    )]
    Undeclared { app: String, command: String },

    #[error(
        "{app} registered command {command:?} as {registered:?} risk where its app.toml \
         declares {declared:?}. Risk is part of what was approved"
    )]
    RiskChanged {
        app: String,
        command: String,
        declared: Risk,
        registered: Risk,
    },

    #[error("{app} registered command {command:?} twice")]
    Duplicate { app: String, command: String },

    #[error(
        "{app} registered command {command:?} with a parameter schema that is not a JSON \
         Schema object, so the bus has nothing to validate calls against"
    )]
    BadSchema { app: String, command: String },

    #[error("{app} did not complete the handshake within {seconds}s")]
    Timeout { app: String, seconds: u64 },
}

/// Decide whether to believe a [`Registration`].
///
/// The manifest, not the running process, is what the operator approved. So
/// the registration may narrow that surface (an app may discover at runtime
/// that it cannot offer something) but never widen it: a command the manifest
/// does not declare is refused even if everything else about the app is fine.
pub fn accept_registration(
    manifest: &Manifest,
    registration: &Registration,
) -> Result<Vec<RegisteredCommand>, HandshakeError> {
    check_protocol(&manifest.app.name, registration.protocol)?;

    if registration.app != manifest.app.name {
        return Err(HandshakeError::WrongApp {
            expected: manifest.app.name.clone(),
            found: registration.app.clone(),
        });
    }
    if registration.version != manifest.app.version {
        return Err(HandshakeError::VersionMismatch {
            app: manifest.app.name.clone(),
            binary: registration.version.clone(),
            manifest: manifest.app.version.clone(),
        });
    }

    let mut accepted: Vec<RegisteredCommand> = Vec::with_capacity(registration.commands.len());
    for command in &registration.commands {
        let declared = manifest
            .commands
            .iter()
            .find(|d| d.id == command.id)
            .ok_or_else(|| HandshakeError::Undeclared {
                app: manifest.app.name.clone(),
                command: command.id.clone(),
            })?;

        if declared.risk != command.risk {
            return Err(HandshakeError::RiskChanged {
                app: manifest.app.name.clone(),
                command: command.id.clone(),
                declared: declared.risk,
                registered: command.risk,
            });
        }
        if accepted.iter().any(|a| a.id == command.id) {
            return Err(HandshakeError::Duplicate {
                app: manifest.app.name.clone(),
                command: command.id.clone(),
            });
        }
        if !command.params.is_object() {
            return Err(HandshakeError::BadSchema {
                app: manifest.app.name.clone(),
                command: command.id.clone(),
            });
        }
        accepted.push(command.clone());
    }
    Ok(accepted)
}

// ---------------------------------------------------------------------------
// Transport
// ---------------------------------------------------------------------------

/// One app's connection to the core: newline-delimited JSON over a Unix
/// socket in the install directory.
///
/// Stdio is the documented alternative in the design, and the frames above
/// are identical over it. Only the socket is implemented here because it is
/// the one that survives the app being restarted while the core keeps
/// running.
pub struct Connection {
    app: String,
    stream: tokio::io::BufReader<tokio::net::UnixStream>,
}

impl Connection {
    pub fn app(&self) -> &str {
        &self.app
    }

    pub async fn send(&mut self, frame: &CoreFrame) -> Result<(), ProtocolError> {
        use tokio::io::AsyncWriteExt as _;
        let line = encode_frame(frame)?;
        self.stream.write_all(line.as_bytes()).await?;
        self.stream.flush().await?;
        Ok(())
    }

    pub async fn recv(&mut self) -> Result<AppFrame, ProtocolError> {
        use tokio::io::AsyncBufReadExt as _;
        let mut line = String::new();
        let read = self.stream.read_line(&mut line).await?;
        if read == 0 {
            return Err(ProtocolError::Closed);
        }
        decode_frame(line.trim_end())
    }
}

/// Where an app's socket lives.
pub fn socket_path(install_dir: &Path, app: &str) -> PathBuf {
    install_dir.join(RUN_SUBDIR).join(format!("{app}.sock"))
}

/// Bind the socket an app will connect back on.
///
/// The run directory is set to owner-only, and a leftover socket from a
/// crash is removed first: `bind` fails on an existing path, so without that
/// an unclean shutdown would make the app permanently unlaunchable. Removing
/// it is safe because the lock file has already established that no other
/// server is serving this directory.
pub async fn bind_socket(path: &Path) -> io::Result<tokio::net::UnixListener> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
        // The socket is an app's door to the command bus. Any local user who
        // could connect to it would be an unauthenticated client.
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))?;
    }
    match std::fs::remove_file(path) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    tokio::net::UnixListener::bind(path)
}

/// Accept one app's connection and complete the handshake.
///
/// The timeout is the whole exchange, not each read: an app that connects and
/// then says nothing is exactly as broken as one that never connects, and
/// [`Host`] treats both as a crash so the backoff and crash-loop rules apply
/// to a hung app too.
pub async fn accept_handshake(
    listener: &tokio::net::UnixListener,
    manifest: &Manifest,
    timeout: std::time::Duration,
) -> Result<(Connection, Vec<RegisteredCommand>), HandshakeError> {
    let app = manifest.app.name.clone();
    let seconds = timeout.as_secs();

    let exchange = async {
        let (stream, _addr) = listener.accept().await.map_err(ProtocolError::Io)?;
        let mut conn = Connection {
            app: app.clone(),
            stream: tokio::io::BufReader::new(stream),
        };

        conn.send(&CoreFrame::Hello(Hello {
            protocol: PROTOCOL_VERSION,
            core_version: CORE_VERSION.to_string(),
            app: app.clone(),
        }))
        .await?;

        // Anything before the registration is out of order. Logs are the one
        // exception: an app that wants to explain itself while starting up
        // should be able to.
        loop {
            match conn.recv().await? {
                AppFrame::Register(registration) => {
                    let commands = accept_registration(manifest, &registration)?;
                    return Ok((conn, commands));
                }
                AppFrame::Log { level, message } => {
                    tracing::debug!(app = %app, %level, %message, "app log during handshake");
                }
                other => {
                    return Err(HandshakeError::Protocol(ProtocolError::Malformed(format!(
                        "expected a registration frame first, got {other:?}"
                    ))));
                }
            }
        }
    };

    match tokio::time::timeout(timeout, exchange).await {
        Ok(result) => result,
        Err(_elapsed) => Err(HandshakeError::Timeout { app, seconds }),
    }
}

// ---------------------------------------------------------------------------
// Environment
// ---------------------------------------------------------------------------

/// What the host asks of the world, instead of reading it directly.
///
/// `Send + Sync` because the host sits behind one lock shared by every HTTP
/// handler.
pub trait Env: Send + Sync {
    /// Unix seconds. Used for backoff, idle timeouts and crash windows, all
    /// of which are coarse on purpose: a clock that jumps should not strand
    /// an app in `Reconnecting`.
    fn now(&self) -> u64;

    /// Immediate entries of a directory, sorted, so two nodes list the same
    /// apps in the same order.
    fn read_dir(&self, path: &Path) -> io::Result<Vec<PathBuf>>;

    fn read_to_string(&self, path: &Path) -> io::Result<String>;

    fn is_executable(&self, path: &Path) -> bool;

    /// Whether a path is a directory, following symlinks.
    ///
    /// Separate from [`Env::exists`] because Landlock refuses a
    /// directory-only right such as `ReadDir` on anything that is not a
    /// directory, and one such rule fails the whole ruleset. See
    /// [`PENDING_FILE_GRANTS`].
    fn is_dir(&self, path: &Path) -> bool;

    /// Resolve a path through symlinks.
    ///
    /// The launcher needs the real target rather than the manifest's words:
    /// Landlock grants the tree behind the descriptor it is handed, so a
    /// symlink inside the working directory pointing at `~/.ssh` would
    /// otherwise be a grant on `~/.ssh`.
    fn canonicalize(&self, path: &Path) -> io::Result<PathBuf>;

    /// Create a directory and everything above it, owner-only, and do nothing
    /// if it is already there.
    ///
    /// Separate from a plain `create_dir_all` because the mode matters:
    /// Landlock sits on top of ordinary permissions rather than replacing
    /// them, so a world-readable scratch directory stays world-readable to
    /// everything Landlock is not confining.
    fn create_private_dir(&self, path: &Path) -> io::Result<()>;

    /// Whether this node can confine a process at all.
    ///
    /// Injected rather than read from [`crate::sandbox::availability`]
    /// directly so the refusal is decided by the same thing that decides
    /// everything else here, and so a node's answer can be fixed in place
    /// without a kernel to match it.
    fn sandbox_availability(&self) -> sandbox::Availability;

    /// Whether a path is there at all. Separate from
    /// [`Env::is_executable`] so "no binary" and "not executable" stay
    /// different answers to the operator.
    fn exists(&self, path: &Path) -> bool;

    /// Start an app. Returns its pid.
    fn spawn(&self, plan: &LaunchPlan) -> io::Result<u32>;

    /// Whether a pid we spawned is still running.
    fn is_running(&self, pid: u32) -> bool;

    /// Ask a pid to stop. SIGTERM, not SIGKILL: an app has state to flush.
    fn terminate(&self, pid: u32) -> io::Result<()>;
}

/// The real one.
pub struct OsEnv {
    /// Children are kept so they can be reaped.
    ///
    /// Without this, a crashed child stays a zombie, `kill(pid, 0)` keeps
    /// succeeding, and the supervisor never notices the crash it exists to
    /// notice. `try_wait` in [`OsEnv::is_running`] is what reaps them.
    children: std::sync::Mutex<BTreeMap<u32, std::process::Child>>,
    /// Probed at most once. It is a property of the running kernel, and the
    /// probe builds a real Landlock ruleset, so asking per launch would spend
    /// syscalls to learn the same answer.
    sandbox: std::sync::OnceLock<sandbox::Availability>,
}

impl Default for OsEnv {
    fn default() -> OsEnv {
        OsEnv::new()
    }
}

impl OsEnv {
    pub fn new() -> OsEnv {
        OsEnv {
            children: std::sync::Mutex::new(BTreeMap::new()),
            sandbox: std::sync::OnceLock::new(),
        }
    }
}

impl Env for OsEnv {
    fn now(&self) -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0)
    }

    fn read_dir(&self, path: &Path) -> io::Result<Vec<PathBuf>> {
        let mut entries: Vec<PathBuf> = std::fs::read_dir(path)?
            .filter_map(|entry| entry.ok().map(|e| e.path()))
            .collect();
        entries.sort();
        Ok(entries)
    }

    fn read_to_string(&self, path: &Path) -> io::Result<String> {
        std::fs::read_to_string(path)
    }

    fn is_executable(&self, path: &Path) -> bool {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::metadata(path)
            .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
    }

    fn exists(&self, path: &Path) -> bool {
        std::fs::metadata(path).is_ok()
    }

    fn is_dir(&self, path: &Path) -> bool {
        std::fs::metadata(path).map(|m| m.is_dir()).unwrap_or(false)
    }

    fn canonicalize(&self, path: &Path) -> io::Result<PathBuf> {
        std::fs::canonicalize(path)
    }

    fn create_private_dir(&self, path: &Path) -> io::Result<()> {
        if std::fs::metadata(path).is_ok() {
            // Already there, so it is the operator's directory and not ours.
            // Tightening its mode here would silently change permissions on
            // part of the working directory that something else may share.
            return Ok(());
        }
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::create_dir_all(path)?;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
    }

    fn sandbox_availability(&self) -> sandbox::Availability {
        self.sandbox.get_or_init(sandbox::availability).clone()
    }

    fn spawn(&self, plan: &LaunchPlan) -> io::Result<u32> {
        use std::process::{Command, Stdio};

        // The confinement is applied by the `Command` itself, in the forked
        // child just before exec. It is done here rather than in [`Host`] so
        // that "launch an app" and "launch an app confined" are one operation
        // with one implementation: there is no code path in this module that
        // reaches an app binary without going through this match.
        //
        // `sandbox::command` refuses when the host cannot enforce, which makes
        // this the second gate after [`LaunchPlan::build`] — deliberately, so
        // a future caller that assembles a plan by hand still cannot spawn
        // something unconfined by forgetting a check.
        let mut command = match plan.confinement {
            Confinement::Enforced => sandbox::command(&plan.program, plan.sandbox.policy.clone())
                .map_err(|e| io::Error::other(e.to_string()))?,
            // Already logged at warn by [`Host::launch`].
            Confinement::Unconfined => Command::new(&plan.program),
        };
        command.args(&plan.args);
        // An app inherits nothing. The core's environment can hold API keys,
        // proxy credentials and the operator's shell history settings; an app
        // gets only what [`LaunchPlan::build`] decided to hand it.
        command.env_clear();
        for (key, value) in &plan.env {
            command.env(key, value);
        }
        command.current_dir(&plan.cwd);
        // No stdin: an app that reads from the terminal would block on the
        // core's. stdout and stderr are inherited so an app's panic lands in
        // the same journal as the core's.
        command.stdin(Stdio::null());

        let child = command.spawn()?;
        let pid = child.id();
        self.children
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(pid, child);
        Ok(pid)
    }

    fn is_running(&self, pid: u32) -> bool {
        let mut children = self
            .children
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(child) = children.get_mut(&pid) {
            return match child.try_wait() {
                Ok(None) => true,
                Ok(Some(status)) => {
                    tracing::info!(pid, %status, "app exited");
                    children.remove(&pid);
                    false
                }
                // A child we cannot ask about is a child we cannot supervise;
                // calling it gone gets it restarted, which is the recoverable
                // reading.
                Err(e) => {
                    tracing::warn!(pid, error = %e, "cannot check app process");
                    false
                }
            };
        }
        // Not ours: a pid read from somewhere else. Signal 0 does the
        // existence check without sending anything.
        use nix::sys::signal::kill;
        use nix::unistd::Pid;
        match kill(Pid::from_raw(pid as i32), None) {
            Ok(()) => true,
            Err(nix::errno::Errno::EPERM) => true,
            Err(_) => false,
        }
    }

    fn terminate(&self, pid: u32) -> io::Result<()> {
        use nix::sys::signal::{Signal, kill};
        use nix::unistd::Pid;
        kill(Pid::from_raw(pid as i32), Signal::SIGTERM)
            .map_err(|e| io::Error::other(e.to_string()))
    }
}

// ---------------------------------------------------------------------------
// Launching
// ---------------------------------------------------------------------------

/// Whether this node enforces what a manifest asked for, or deliberately does
/// not.
///
/// There is no `Deserialize`, no `From<bool>` and no `FromStr` here on
/// purpose. A manifest is written by the app, and a config file is one typo
/// away from a security setting that silently does nothing, so neither may
/// produce [`Confinement::Unconfined`]: the only way to it is to call
/// [`Confinement::unconfined_for_development`] by name, which reads like what
/// it is at the one call site that chose it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Confinement {
    /// Apply the manifest's policy, and refuse to launch on a node that
    /// cannot. The default, and the only setting that should ever reach a
    /// machine the operator is not sitting at.
    #[default]
    Enforced,
    /// Launch with the full access of the user running Deckjack.
    ///
    /// For developing an app against a kernel without Landlock, and nothing
    /// else. Every launch logs a warning naming the app and what it asked
    /// for, because a developer who left this on should trip over it rather
    /// than discover it from a security review.
    Unconfined,
}

impl Confinement {
    /// The escape hatch, spelled out. See [`Confinement::Unconfined`].
    pub fn unconfined_for_development() -> Confinement {
        Confinement::Unconfined
    }

    pub fn is_enforced(&self) -> bool {
        matches!(self, Confinement::Enforced)
    }
}

/// Read-only system directories every app is granted.
///
/// Not generosity: Landlock is an allowlist and it is applied before exec, so
/// without the loader and its libraries every app dies in `ld.so` with
/// `EACCES` before `main`. Nothing here is the operator's — the working
/// directory, the hidden directory and the home directory are all absent.
///
/// `/proc` is deliberately not granted as a whole. `/proc/<pid>/environ` of
/// the core process is readable by the same user, and the core's environment
/// is exactly where API keys and proxy credentials live, so granting `/proc`
/// would hand every app the secrets the sandbox exists to keep from it.
/// `/proc/self` is resolved in the forked child, so the descriptor is the
/// app's own directory and not the core's.
///
/// `/dev` is not granted as a whole either, and that one is not close: on a
/// systemd seat `/dev/input/event*` carries an ACL for the logged-in user, so
/// a read grant on `/dev` is a keylogger for any app that wants one.
const SYSTEM_READ_DIRS: &[&str] = &[
    "/usr",
    "/bin",
    "/sbin",
    "/lib",
    "/lib64",
    "/etc",
    "/sys",
    "/proc/self",
];

/// Directories every app may read and write.
///
/// `/dev/shm` is here because POSIX shared memory is how ordinary libraries
/// allocate large buffers. It is one tmpfs shared by every process of this
/// user, so two Deckjack apps can see each other's segments in it: Landlock
/// cannot draw a line inside a shared mount, and giving each app its own would
/// need the mount namespaces this launcher deliberately does without. A known
/// gap, not an oversight.
const DEV_READ_WRITE_DIRS: &[&str] = &["/dev/shm"];

/// Granted read and write only when the manifest asks for the GPU.
///
/// `/dev/dri` covers Mesa, Vulkan and VA-API. ROCm also wants `/dev/kfd`,
/// which is a character device and so is in [`PENDING_FILE_GRANTS`].
const GPU_READ_WRITE_DIRS: &[&str] = &["/dev/dri"];

/// Granted read only when the manifest allows the network.
///
/// `/etc/resolv.conf` is a symlink into one of these on most systems, and
/// Landlock checks the target rather than the link, so without them DNS fails
/// with an `EACCES` that looks nothing like a DNS problem. `/run` as a whole
/// is never granted: `/run/user/<uid>` holds the operator's keyring, agent
/// sockets and Wayland socket.
const RESOLVER_READ_DIRS: &[&str] = &[
    "/run/systemd/resolve",
    "/run/NetworkManager",
    "/run/resolvconf",
];

/// Paths an app needs that [`sandbox::Policy`] cannot express yet, kept here
/// so the gap is in the code rather than only in a release note.
///
/// Landlock refuses a directory-only access right — `ReadDir` is one — on a
/// path that is not a directory, and it fails the whole ruleset rather than
/// that one rule. [`sandbox::apply`] hands `AccessFs::from_read` to every
/// entry of [`sandbox::Policy::read`], so a file in that list makes every
/// launch fail. Verified on this machine: Landlock ABI 4 answers
/// `incompatible directory-only access-rights: ReadDir`.
///
/// So these are *not* granted, [`Sandbox::unexpressible`] carries them, and
/// [`Host::launch`] names them at warn on every launch. An app that opens
/// `/dev/null` gets `EACCES`, and the operator should learn that from the
/// launcher rather than from an app that looks broken. Closing it is one
/// field on `sandbox::Policy` for non-directory paths, granted with
/// `AccessFs::from_read(WANTED) & !AccessFs::ReadDir`.
const PENDING_FILE_GRANTS: &[&str] = &[
    "/dev/null",
    "/dev/zero",
    "/dev/full",
    "/dev/random",
    "/dev/urandom",
    "/proc/cpuinfo",
    "/proc/meminfo",
    "/proc/stat",
    "/proc/uptime",
    "/proc/sys/kernel/osrelease",
    "/proc/sys/vm/overcommit_memory",
];

/// The same gap as [`PENDING_FILE_GRANTS`], for the GPU need alone.
const PENDING_GPU_FILE_GRANTS: &[&str] = &["/dev/kfd"];

/// Everything needed to start an app, assembled before anything is spawned so
/// it can be logged, shown to the operator, or asserted on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchPlan {
    pub app: String,
    pub program: PathBuf,
    pub args: Vec<String>,
    /// The working directory, which is also the only tree the app may be
    /// granted paths in.
    pub cwd: PathBuf,
    pub socket: PathBuf,
    pub env: Vec<(String, String)>,
    pub sandbox: Sandbox,
    /// Decided by the node, never by the app. Carried on the plan so
    /// [`Env::spawn`] cannot reach a different answer than the one that was
    /// logged.
    pub confinement: Confinement,
}

/// The confinement an app will actually run under: its manifest's `[needs]`
/// resolved against this machine, plus the one denial that is never
/// negotiable.
///
/// The manifest's own words are kept beside the resolved policy because the
/// two are not the same shape — `needs.network` is a list of hosts and
/// [`sandbox::Policy::network`] is a boolean — and the approval dialog and the
/// logs have to be able to show what was asked for, not only what could be
/// enforced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sandbox {
    pub gpu: bool,
    /// The hosts the manifest named. See [`Sandbox::network_is_coarse`].
    pub network_hosts: Vec<String>,
    /// Trees no app is ever granted. Landlock is an allowlist, so this is not
    /// a rule that gets installed anywhere — it is a rule about what may go
    /// into `policy`, checked in [`LaunchPlan::build`] against every path a
    /// manifest asked for.
    pub denied: Vec<PathBuf>,
    /// Paths this launcher wanted to grant and could not. See
    /// [`PENDING_FILE_GRANTS`]: it is a limit of [`sandbox::Policy`]'s shape,
    /// not a decision about this app, and it is reported at every launch
    /// rather than left to surface as an `EACCES` inside the app.
    pub unexpressible: Vec<PathBuf>,
    /// Exactly what [`sandbox::apply`] will be given in the forked child.
    pub policy: sandbox::Policy,
}

impl Sandbox {
    /// True when the manifest named hosts and the whole network had to be
    /// opened to honor it.
    ///
    /// Landlock's network rules are per-port, not per-host, so an allowlist of
    /// names cannot be installed as written. Refusing the network outright
    /// would break an app the operator approved to reach a host, so the gap is
    /// granted and reported at every launch instead of being hidden; the
    /// design's answer for closing it is the allowlisting HTTPS proxy.
    pub fn network_is_coarse(&self) -> bool {
        self.policy.network && !self.network_hosts.is_empty()
    }
}

#[derive(Debug, Error)]
pub enum ConfinementError {
    #[error(
        "this node cannot confine a process ({reason}), and {app} asks for {summary}. \
         Deckjack will not launch an app it cannot hold to its manifest: run {app} on a node \
         with Landlock and seccomp (Linux 6.7 or newer, CONFIG_SECURITY_LANDLOCK=y), or start \
         this node in developer mode if you accept that apps run with your full access"
    )]
    Unenforceable {
        app: String,
        summary: String,
        reason: String,
    },

    #[error(
        "{app}'s manifest asks for no network, and this kernel's Landlock is ABI {abi} where \
         network rules arrived in ABI 4. Granting the network the manifest did not ask for is \
         not a downgrade Deckjack makes: run {app} on a node with Linux 6.7 or newer"
    )]
    NoNetworkRules { app: String, abi: i32 },

    #[error(
        "{app} asks to read {path}, which does not exist. Create it, or drop it from the app's \
         app.toml and approve {app} again — a path that is not there cannot be granted, and \
         granting its parent instead would be more than was approved"
    )]
    MissingPath { app: String, path: PathBuf },

    #[error("cannot prepare {path} for {app}: {source}")]
    Prepare {
        app: String,
        path: PathBuf,
        #[source]
        source: io::Error,
    },

    #[error(
        "{app} asks for {asked}, which resolves to {real}, outside the working directory \
         {working_dir}. Something in the working directory is a symlink pointing out of it; \
         Deckjack grants the tree the link leads to, not the name, so this was refused. Replace \
         the link with a real directory or point the manifest somewhere inside"
    )]
    Escapes {
        app: String,
        asked: PathBuf,
        real: PathBuf,
        working_dir: PathBuf,
    },

    #[error(
        "{app} asks for {path}, which is {denied} or part of it. No app is ever granted that \
         tree: the core is the only process that opens state.db, and apps reach state through \
         the bus"
    )]
    Denied {
        app: String,
        path: PathBuf,
        denied: PathBuf,
    },

    #[error(
        "{app} asks for {path}, which is not a directory. Landlock grants trees, not single \
         files, so name the directory the file is in and let the app open what it needs inside \
         it"
    )]
    NotADirectory { app: String, path: PathBuf },
}

impl LaunchPlan {
    /// Assemble a plan, or refuse.
    ///
    /// This returns a `Result` because a plan that cannot be honored must not
    /// exist: every reason an app may not run — a kernel that cannot enforce,
    /// a granted path that escapes the working directory, a path that reaches
    /// the hidden directory — is decided here, before a process exists, rather
    /// than being discovered by a child that has already started.
    pub fn build(
        installed: &Installed,
        install_dir: &Path,
        working_dir: &Path,
        confinement: Confinement,
        env: &dyn Env,
    ) -> Result<LaunchPlan, ConfinementError> {
        let app = installed.manifest.app.name.clone();
        let needs = &installed.manifest.needs;
        let socket = socket_path(install_dir, &app);

        // The manifest's network list is an allowlist and not a hint, so an
        // empty list is a denial: no hosts named means no network at all.
        let network = !needs.network.is_empty();

        // Asked before a directory is created or a path resolved. If this node
        // cannot enforce, nothing about this app should happen at all.
        if confinement.is_enforced() {
            match env.sandbox_availability() {
                sandbox::Availability::Unavailable { reason } => {
                    return Err(ConfinementError::Unenforceable {
                        app,
                        summary: summarize(needs),
                        reason,
                    });
                }
                // A kernel without Landlock's network rules can still confine
                // the filesystem, so an app that asked for network access
                // loses nothing. An app that asked for none cannot be held to
                // that, and a denial that is not enforced is worse than an
                // error, so it is an error.
                sandbox::Availability::Enforcing {
                    landlock_abi,
                    network_rules: false,
                } if !network => {
                    return Err(ConfinementError::NoNetworkRules {
                        app,
                        abi: landlock_abi,
                    });
                }
                sandbox::Availability::Enforcing { .. } => {}
            }
        }

        let working_root =
            env.canonicalize(working_dir)
                .map_err(|source| ConfinementError::Prepare {
                    app: app.clone(),
                    path: working_dir.to_path_buf(),
                    source,
                })?;
        // The one denial that is never negotiable, resolved the same way the
        // grants are so the comparisons below are between like and like.
        let hidden = working_root.join(HIDDEN_DIR);

        let mut policy = sandbox::Policy::nothing().with_network(network);

        for need in &needs.working_dir {
            let asked = working_root.join(&need.path);
            if need.write {
                // An app that asked to write a directory asked for that
                // directory to exist. Refusing a fresh working directory
                // because `Calendars/` has never been written to would make
                // every app's first launch a support question. A read need is
                // never created: there is nothing the core could put in it,
                // and an empty directory would hide the operator's mistake.
                env.create_private_dir(&asked)
                    .map_err(|source| ConfinementError::Prepare {
                        app: app.clone(),
                        path: asked.clone(),
                        source,
                    })?;
            } else if !env.exists(&asked) {
                return Err(ConfinementError::MissingPath { app, path: asked });
            }

            // Resolved rather than joined. Landlock grants the tree behind the
            // descriptor it is handed, so a symlink in the working directory
            // pointing at `~/.ssh` would be a grant on `~/.ssh`. The manifest
            // already cannot write `..`; this is the same rule applied to what
            // the filesystem says instead of to what the manifest says.
            //
            // It is a check and not a guarantee: the link could be swapped
            // between here and `PathFd::new` inside the child. That race is
            // why the hidden directory check below is not the only thing
            // keeping an app out of `.deckjack/` — the core also never writes
            // anything an app may read into a tree an app was granted.
            let real = env
                .canonicalize(&asked)
                .map_err(|source| ConfinementError::Prepare {
                    app: app.clone(),
                    path: asked.clone(),
                    source,
                })?;
            if !real.starts_with(&working_root) {
                return Err(ConfinementError::Escapes {
                    app,
                    asked,
                    real,
                    working_dir: working_root,
                });
            }
            // Both directions: a grant inside the hidden directory would read
            // state.db, and a grant containing it would reach state.db through
            // the tree beneath it.
            if real.starts_with(&hidden) || hidden.starts_with(&real) {
                return Err(ConfinementError::Denied {
                    app,
                    path: real,
                    denied: hidden,
                });
            }
            // Checked here rather than left to Landlock, which fails the whole
            // ruleset on a non-directory rule and does it inside the forked
            // child, where the message is reduced to `EINVAL` by the time the
            // parent sees it.
            if !env.is_dir(&real) {
                return Err(ConfinementError::NotADirectory { app, path: real });
            }

            if need.write {
                policy.read_write.push(real);
            } else {
                policy.read.push(real);
            }
        }

        // Everything from here down is the launcher's own, chosen in this file
        // rather than named by a manifest, which is why it does not go through
        // the denial check above. On a node whose working directory is the
        // home directory the run and cache directories are *inside*
        // `.deckjack/`, and an app has to be able to reach its own socket and
        // its own scratch space. Those two paths are granted by name; the tree
        // around them is not.
        policy.read.push(installed.dir.clone());

        // The run directory may not exist yet: `bind_socket` creates it, and
        // on a cold start a service app can be launched before anything has
        // bound. A missing path takes the whole ruleset down, so it is created
        // here rather than hoped for.
        let run = install_dir.join(RUN_SUBDIR);
        let private = install_dir.join(CACHE_SUBDIR).join(APPS_SUBDIR).join(&app);
        let home = private.join("home");
        let tmp = private.join("tmp");
        for dir in [&run, &home, &tmp] {
            env.create_private_dir(dir)
                .map_err(|source| ConfinementError::Prepare {
                    app: app.clone(),
                    path: dir.clone(),
                    source,
                })?;
        }
        // Read, not write: the app connects to a socket here, it does not get
        // to create or remove one.
        policy.read.push(run);
        policy.read_write.push(home.clone());
        policy.read_write.push(tmp.clone());

        let mut unexpressible = Vec::new();

        for path in SYSTEM_READ_DIRS {
            grant_dir(&mut policy.read, Path::new(path), env);
        }
        for path in DEV_READ_WRITE_DIRS {
            grant_dir(&mut policy.read_write, Path::new(path), env);
        }
        for path in PENDING_FILE_GRANTS {
            note_unexpressible(&mut unexpressible, Path::new(path), env);
        }
        if needs.gpu {
            for path in GPU_READ_WRITE_DIRS {
                grant_dir(&mut policy.read_write, Path::new(path), env);
            }
            for path in PENDING_GPU_FILE_GRANTS {
                note_unexpressible(&mut unexpressible, Path::new(path), env);
            }
        }
        if network {
            for path in RESOLVER_READ_DIRS {
                grant_dir(&mut policy.read, Path::new(path), env);
            }
        }

        Ok(LaunchPlan {
            app: app.clone(),
            program: installed.binary.clone(),
            args: Vec::new(),
            cwd: working_dir.to_path_buf(),
            socket: socket.clone(),
            env: vec![
                ("DECKJACK_APP".to_string(), app),
                (
                    "DECKJACK_APP_SOCKET".to_string(),
                    socket.to_string_lossy().into_owned(),
                ),
                (
                    "DECKJACK_APP_PROTOCOL".to_string(),
                    PROTOCOL_VERSION.to_string(),
                ),
                // The core's environment is cleared before exec, which leaves
                // an app with no HOME and no TMPDIR at all. A library that
                // falls back to `/` or to `/tmp` on that writes where nothing
                // granted it anything, and the failure looks like a bug in the
                // app rather than a missing grant. So each app gets its own,
                // inside the install directory's cache.
                ("HOME".to_string(), home.to_string_lossy().into_owned()),
                ("TMPDIR".to_string(), tmp.to_string_lossy().into_owned()),
                // So an app can state in its own logs what it is running
                // under, instead of inferring it from a syscall that failed.
                (
                    "DECKJACK_CONFINED".to_string(),
                    if confinement.is_enforced() { "1" } else { "0" }.to_string(),
                ),
            ],
            sandbox: Sandbox {
                gpu: needs.gpu,
                network_hosts: needs.network.clone(),
                denied: vec![hidden],
                unexpressible,
                policy,
            },
            confinement,
        })
    }
}

/// Add a system directory to a grant list, if it is there and is a directory.
///
/// Absent is not an error: `/lib64` is gone on a merged-/usr system, `/sys` is
/// absent in some containers, and `/run/systemd/resolve` only exists where
/// systemd-resolved runs. Each one missing is one fewer thing to grant, where
/// handing `PathFd` a path that is not there would fail the whole ruleset.
fn grant_dir(into: &mut Vec<PathBuf>, path: &Path, env: &dyn Env) {
    if env.is_dir(path) {
        into.push(path.to_path_buf());
    }
}

/// Record a path that exists and is needed but cannot be granted yet. See
/// [`PENDING_FILE_GRANTS`].
fn note_unexpressible(into: &mut Vec<PathBuf>, path: &Path, env: &dyn Env) {
    if env.exists(path) {
        into.push(path.to_path_buf());
    }
}

// ---------------------------------------------------------------------------
// Discovery
// ---------------------------------------------------------------------------

/// An app that is installed and whose manifest the core can act on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Installed {
    pub name: String,
    pub dir: PathBuf,
    pub binary: PathBuf,
    pub manifest: Manifest,
}

/// An app directory the core found but will not run, and why.
#[derive(Debug)]
pub struct Problem {
    pub dir: PathBuf,
    /// Present when the manifest parsed far enough to name the app, so the
    /// dock can show it greyed out with the reason instead of it vanishing.
    pub name: Option<String>,
    pub error: DiscoveryError,
}

#[derive(Debug, Error)]
pub enum DiscoveryError {
    #[error(transparent)]
    Manifest(#[from] ManifestError),

    #[error(transparent)]
    Protocol(#[from] ProtocolError),

    #[error(
        "app.toml in {dir} names the app {name:?}; the directory and the name must match, \
         because {APPS_SUBDIR}/<name>/ is how an app is addressed"
    )]
    NameMismatch { dir: PathBuf, name: String },

    #[error("{path} is missing; every app directory holds one {BINARY_PREFIX}<name> executable")]
    MissingBinary { path: PathBuf },

    #[error("{path} is not executable; `chmod +x` it or reinstall the app")]
    NotExecutable { path: PathBuf },

    #[error("cannot list {path}: {source}")]
    Unreadable {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
}

/// What discovery found.
#[derive(Debug, Default)]
pub struct Discovery {
    pub apps: Vec<Installed>,
    pub problems: Vec<Problem>,
}

pub fn apps_root(install_dir: &Path) -> PathBuf {
    install_dir.join(APPS_SUBDIR)
}

/// Scan `<install>/apps/*/app.toml`.
///
/// One unusable app is a [`Problem`], never an error: a desktop that refuses
/// to start because a video editor was built against last month's protocol is
/// a worse computer than one that starts with the video editor greyed out.
/// A missing apps directory is the same — a fresh install has no apps.
pub fn discover(install_dir: &Path, env: &dyn Env) -> Discovery {
    let root = apps_root(install_dir);
    let mut found = Discovery::default();

    let entries = match env.read_dir(&root) {
        Ok(entries) => entries,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return found,
        Err(source) => {
            found.problems.push(Problem {
                dir: root.clone(),
                name: None,
                error: DiscoveryError::Unreadable { path: root, source },
            });
            return found;
        }
    };

    for dir in entries {
        let manifest_path = dir.join(MANIFEST_FILE);
        // Caches and stray files live beside app directories; only a
        // directory with a manifest claims to be an app.
        if !env.exists(&manifest_path) {
            continue;
        }

        let manifest = match Manifest::load(&manifest_path, env) {
            Ok(manifest) => manifest,
            Err(error) => {
                found.problems.push(Problem {
                    dir,
                    name: None,
                    error: error.into(),
                });
                continue;
            }
        };
        let name = manifest.app.name.clone();

        if dir.file_name().map(|n| n != name.as_str()).unwrap_or(true) {
            found.problems.push(Problem {
                dir: dir.clone(),
                name: Some(name.clone()),
                error: DiscoveryError::NameMismatch { dir, name },
            });
            continue;
        }

        if let Err(error) = check_protocol(&name, manifest.app.protocol) {
            found.problems.push(Problem {
                dir,
                name: Some(name),
                error: error.into(),
            });
            continue;
        }

        let binary = dir.join(format!("{BINARY_PREFIX}{name}"));
        if !env.is_executable(&binary) {
            let error = if env.exists(&binary) {
                DiscoveryError::NotExecutable {
                    path: binary.clone(),
                }
            } else {
                DiscoveryError::MissingBinary {
                    path: binary.clone(),
                }
            };
            found.problems.push(Problem {
                dir,
                name: Some(name),
                error,
            });
            continue;
        }

        if found.apps.iter().any(|a| a.name == name) {
            // Two directories cannot both be one app; the first one listed
            // wins so the outcome does not depend on filesystem order.
            found.problems.push(Problem {
                dir: dir.clone(),
                name: Some(name.clone()),
                error: DiscoveryError::NameMismatch { dir, name },
            });
            continue;
        }

        found.apps.push(Installed {
            name,
            dir,
            binary,
            manifest,
        });
    }

    found
}

// ---------------------------------------------------------------------------
// Approvals
// ---------------------------------------------------------------------------

/// What the operator approved for one app.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Grant {
    pub app: String,
    /// [`Manifest::fingerprint`] at the time of approval. A different
    /// fingerprint means the app now asks for something else, and the grant
    /// no longer covers it.
    pub fingerprint: String,
    pub needs: Needs,
    pub granted_at: u64,
}

#[derive(Debug, Error)]
#[error("cannot reach app approvals in the state database: {0}")]
pub struct GrantError(String);

impl GrantError {
    pub fn new(message: impl Into<String>) -> GrantError {
        GrantError(message.into())
    }
}

/// Where approvals live. Implemented over [`crate::store::Store`] by the
/// server, so this module needs no database handle of its own and no second
/// opinion about where state belongs.
pub trait Grants: Send + Sync {
    fn grant(&self, app: &str) -> Result<Option<Grant>, GrantError>;
    fn record(&self, grant: &Grant) -> Result<(), GrantError>;
    fn revoke(&self, app: &str) -> Result<(), GrantError>;
}

/// Stop-gap until `store.rs` gains the three app-grant methods (see this
/// module's integration notes).
///
/// It answers "not approved" to every question and refuses to record, rather
/// than pretending an approval happened and forgetting it: an app that asks
/// for nothing still launches, and anything else fails loudly with something
/// the operator can act on.
pub struct UnwiredGrants;

impl Grants for UnwiredGrants {
    fn grant(&self, _app: &str) -> Result<Option<Grant>, GrantError> {
        Ok(None)
    }

    fn record(&self, _grant: &Grant) -> Result<(), GrantError> {
        Err(GrantError::new(
            "app approvals are not persisted in this build, so an app that needs the GPU, \
             the network or part of the working directory cannot be approved yet",
        ))
    }

    fn revoke(&self, _app: &str) -> Result<(), GrantError> {
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Supervision
// ---------------------------------------------------------------------------

/// Supervision thresholds.
///
/// TODO: these belong in `config.rs` as an `[apps]` table once this module is
/// wired, so an operator on a slow machine can raise the handshake timeout
/// without a rebuild. The defaults are the documented behavior until then.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Policy {
    /// Wait before the first restart, doubled per attempt.
    pub restart_backoff_secs: u64,
    pub max_backoff_secs: u64,
    /// Crashes inside this window that count toward [`Policy::max_crashes`].
    pub crash_window_secs: u64,
    /// After this many crashes in the window the app is left stopped.
    pub max_crashes: usize,
    /// How long an app has to connect back and register.
    pub handshake_secs: u64,
    /// How long an on-demand app with no windows is kept warm.
    pub idle_secs: u64,
    /// Whether this node holds apps to their manifests.
    ///
    /// It lives here, with the other launch decisions the node makes, rather
    /// than anywhere an app or a manifest can reach. [`Confinement::Enforced`]
    /// is the [`Default`], so a [`Policy`] assembled from a config file that
    /// says nothing about it enforces.
    pub confinement: Confinement,
}

impl Default for Policy {
    fn default() -> Policy {
        Policy {
            restart_backoff_secs: 1,
            max_backoff_secs: 30,
            crash_window_secs: 120,
            max_crashes: 5,
            handshake_secs: 15,
            // Long enough that closing and reopening a window is instant,
            // short enough that a forgotten window does not hold a GPU
            // context or a phone's battery all afternoon.
            idle_secs: 300,
            confinement: Confinement::Enforced,
        }
    }
}

/// Where one app is in its life.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "phase", rename_all = "kebab-case")]
pub enum Phase {
    Stopped,
    /// Spawned, waiting for the handshake.
    Starting {
        since: u64,
    },
    Ready {
        since: u64,
    },
    /// Crashed and waiting to be restarted. Windows show "reconnecting".
    Reconnecting {
        since: u64,
        attempt: u32,
    },
    /// Crashed too often. Left alone until the operator asks again, because
    /// an app that cannot start is a broken build, and restarting it forever
    /// hides that while burning a phone's battery.
    GaveUp {
        at: u64,
        crashes: usize,
    },
}

/// What a window should render. The window manager needs exactly three
/// answers, so this is three and not a projection of [`Phase`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum WindowStatus {
    /// The app is up; show the UI.
    Live,
    /// The app will be back; show "reconnecting" and keep the window.
    Reconnecting,
    /// Not installed, not runnable, or given up on; show why, and let the
    /// window be closed.
    Unavailable,
}

/// Something the desktop needs to hear about. Emitted by [`Host`] rather than
/// pushed, so the WebSocket layer decides how it reaches the browser.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "kebab-case")]
pub enum HostEvent {
    Ready {
        app: String,
        commands: usize,
    },
    Reconnecting {
        app: String,
        attempt: u32,
    },
    Stopped {
        app: String,
        /// True when it was stopped for being idle rather than for failing.
        idle: bool,
    },
    GaveUp {
        app: String,
        crashes: usize,
        reason: String,
    },
    Unavailable {
        app: String,
        reason: String,
    },
}

/// One app's runtime state.
#[derive(Debug, Clone)]
pub struct Runtime {
    pub name: String,
    pub phase: Phase,
    pub pid: Option<u32>,
    pub socket: PathBuf,
    /// Registered at handshake, cleared on exit: a crashed app's commands
    /// must leave the palette, or the palette offers what cannot run.
    pub commands: Vec<RegisteredCommand>,
    /// Open windows. The idle sweep only touches apps at zero.
    pub windows: u32,
    pub last_active: u64,
    crashes: Vec<u64>,
    attempt: u32,
    next_attempt_at: u64,
}

impl Runtime {
    fn new(name: String, socket: PathBuf, now: u64) -> Runtime {
        Runtime {
            name,
            phase: Phase::Stopped,
            pid: None,
            socket,
            commands: Vec::new(),
            windows: 0,
            last_active: now,
            crashes: Vec::new(),
            attempt: 0,
            next_attempt_at: 0,
        }
    }

    pub fn window_status(&self) -> WindowStatus {
        match self.phase {
            Phase::Ready { .. } => WindowStatus::Live,
            // Both of these end in the app being up, so the window waits.
            Phase::Starting { .. } | Phase::Reconnecting { .. } => WindowStatus::Reconnecting,
            // Stopped means nothing is running and nothing is scheduled: an
            // idle stop, a refused approval, a failed spawn. Calling that
            // "reconnecting" would spin a window forever on something that
            // is never coming; the caller of `open_window` has the reason.
            Phase::Stopped | Phase::GaveUp { .. } => WindowStatus::Unavailable,
        }
    }
}

#[derive(Debug, Error)]
pub enum HostError {
    #[error("no app named {0:?} is installed on this node; `deckjack install {0}` first")]
    NotInstalled(String),

    #[error("{app} is installed but not runnable: {reason}")]
    Unavailable { app: String, reason: String },

    #[error(
        "{app} has not been approved for what it asks ({summary}). Approve it in Settings \
         or with `deckjack approve {app}`"
    )]
    NotApproved { app: String, summary: String },

    #[error(
        "{app}'s approval covers an older manifest: it now asks for {summary}. Review and \
         approve the change with `deckjack approve {app}`"
    )]
    GrantStale { app: String, summary: String },

    #[error(transparent)]
    Grant(#[from] GrantError),

    #[error(transparent)]
    Confinement(#[from] ConfinementError),

    #[error("cannot start {app}: {source}")]
    Spawn {
        app: String,
        #[source]
        source: io::Error,
    },

    #[error(
        "{app} crashed {crashes} times in a row and has been left stopped. Check its logs; \
         `deckjack restart {app}` tries again"
    )]
    CrashLoop { app: String, crashes: usize },
}

/// The app host: discovery, launch, supervision and the state the window
/// manager reads.
///
/// It owns no sockets and no connections. [`accept_handshake`] and
/// [`Connection`] are separate on purpose: the host decides *that* an app
/// should run and *what* its state is, while the transport layer does the
/// talking and reports back through [`Host::on_registered`] and
/// [`Host::poll`]. That split is what lets every rule in here be tested
/// without a process.
pub struct Host<E: Env> {
    install_dir: PathBuf,
    working_dir: PathBuf,
    env: E,
    policy: Policy,
    grants: Box<dyn Grants>,
    installed: BTreeMap<String, Installed>,
    problems: Vec<Problem>,
    runtimes: BTreeMap<String, Runtime>,
}

impl<E: Env> Host<E> {
    pub fn new(
        install_dir: PathBuf,
        working_dir: PathBuf,
        env: E,
        policy: Policy,
        grants: Box<dyn Grants>,
    ) -> Host<E> {
        Host {
            install_dir,
            working_dir,
            env,
            policy,
            grants,
            installed: BTreeMap::new(),
            problems: Vec::new(),
            runtimes: BTreeMap::new(),
        }
    }

    /// Step 1: discovery. Called at startup and after an install.
    ///
    /// An app that disappeared from disk keeps its runtime entry if it is
    /// still running: killing a working process because its directory was
    /// renamed mid-session would lose whatever it was doing. The next restart
    /// is where it falls out.
    pub fn refresh(&mut self) -> Vec<HostEvent> {
        let found = discover(&self.install_dir, &self.env);
        let now = self.env.now();

        self.installed = found
            .apps
            .into_iter()
            .map(|app| (app.name.clone(), app))
            .collect();
        self.problems = found.problems;

        for name in self.installed.keys() {
            self.runtimes.entry(name.clone()).or_insert_with(|| {
                Runtime::new(name.clone(), socket_path(&self.install_dir, name), now)
            });
        }

        self.problems
            .iter()
            .map(|problem| HostEvent::Unavailable {
                app: problem
                    .name
                    .clone()
                    .unwrap_or_else(|| problem.dir.to_string_lossy().into_owned()),
                reason: problem.error.to_string(),
            })
            .collect()
    }

    pub fn installed(&self) -> impl Iterator<Item = &Installed> {
        self.installed.values()
    }

    pub fn problems(&self) -> &[Problem] {
        &self.problems
    }

    pub fn runtime(&self, app: &str) -> Option<&Runtime> {
        self.runtimes.get(app)
    }

    /// Whether this node holds apps to their manifests.
    ///
    /// The approval dialog needs it: approving `[needs]` on a node in
    /// developer mode approves rather less than it appears to, and the
    /// operator is entitled to be told which one they are looking at.
    pub fn confinement(&self) -> Confinement {
        self.policy.confinement
    }

    /// Whether this node *can* hold apps to their manifests, for Settings and
    /// for a `doctor` command. Asked through [`Env`], so it is the same answer
    /// [`Host::open_window`] will act on rather than a second opinion.
    pub fn sandbox_availability(&self) -> sandbox::Availability {
        self.env.sandbox_availability()
    }

    pub fn window_status(&self, app: &str) -> WindowStatus {
        match self.runtimes.get(app) {
            Some(runtime) if self.installed.contains_key(app) => runtime.window_status(),
            // Installed-but-unrunnable and not-installed are the same answer
            // to a window: there is nothing to reconnect to.
            _ => WindowStatus::Unavailable,
        }
    }

    /// Every command the palette, the CLI and the agent tools may offer right
    /// now. Only ready apps: a command that cannot be dispatched should not
    /// be findable.
    pub fn ready_commands(&self) -> Vec<(&str, &RegisteredCommand)> {
        self.runtimes
            .values()
            .filter(|runtime| matches!(runtime.phase, Phase::Ready { .. }))
            .flat_map(|runtime| {
                runtime
                    .commands
                    .iter()
                    .map(move |command| (runtime.name.as_str(), command))
            })
            .collect()
    }

    /// Step 2, startup half: long-running apps.
    ///
    /// A service that cannot start becomes an event and a log line. The
    /// desktop comes up either way — PIM sync being broken is not a reason to
    /// have no computer.
    pub fn start_services(&mut self) -> Vec<HostEvent> {
        let services: Vec<String> = self
            .installed
            .values()
            .filter(|app| app.manifest.app.lifecycle == Lifecycle::Service)
            .map(|app| app.name.clone())
            .collect();

        let mut events = Vec::new();
        for name in services {
            match self.launch(&name) {
                Ok(_plan) => {}
                Err(e) => {
                    tracing::warn!(app = %name, error = %e, "service app did not start");
                    events.push(HostEvent::Unavailable {
                        app: name,
                        reason: e.to_string(),
                    });
                }
            }
        }
        events
    }

    /// Step 2, on-demand half: a window opened.
    ///
    /// Returns what that window should render immediately. `Reconnecting`
    /// here is not a failure: it is the app starting, and the window shows
    /// the same chrome it would during a restart, so a cold start and a crash
    /// recovery look alike to the operator.
    pub fn open_window(&mut self, app: &str) -> Result<WindowStatus, HostError> {
        if !self.installed.contains_key(app) {
            return Err(self.why_unavailable(app));
        }

        let now = self.env.now();
        if let Some(runtime) = self.runtimes.get_mut(app) {
            runtime.windows = runtime.windows.saturating_add(1);
            runtime.last_active = now;
        }

        let needs_launch = match self.runtimes.get(app).map(|r| &r.phase) {
            Some(Phase::Ready { .. }) => false,
            Some(Phase::Starting { .. }) | Some(Phase::Reconnecting { .. }) => false,
            Some(Phase::GaveUp { crashes, .. }) => {
                return Err(HostError::CrashLoop {
                    app: app.to_string(),
                    crashes: *crashes,
                });
            }
            _ => true,
        };
        if needs_launch {
            self.launch(app)?;
        }
        Ok(self.window_status(app))
    }

    /// A window closed. The app stays warm until the idle sweep in
    /// [`Host::poll`] takes it, so closing and reopening is instant.
    pub fn close_window(&mut self, app: &str) {
        let now = self.env.now();
        if let Some(runtime) = self.runtimes.get_mut(app) {
            runtime.windows = runtime.windows.saturating_sub(1);
            runtime.last_active = now;
        }
    }

    /// A command ran or a UI asset was fetched: the app is in use, so the
    /// idle sweep should leave it alone even with no window open (an agent
    /// calling a command is use too).
    pub fn note_activity(&mut self, app: &str) {
        let now = self.env.now();
        if let Some(runtime) = self.runtimes.get_mut(app) {
            runtime.last_active = now;
        }
    }

    /// Step 3's outcome: the transport completed a handshake.
    pub fn on_registered(&mut self, app: &str, commands: Vec<RegisteredCommand>) -> Vec<HostEvent> {
        let now = self.env.now();
        let count = commands.len();
        if let Some(runtime) = self.runtimes.get_mut(app) {
            // Only an app the host is waiting on may register. A
            // registration arriving in any other phase is from a process the
            // host does not supervise — a leftover from a crash that still
            // holds the socket, or a second instance — and believing it would
            // put commands in the palette that route to nothing, or revive an
            // app the crash-loop rule deliberately gave up on.
            if !matches!(runtime.phase, Phase::Starting { .. }) {
                tracing::warn!(
                    app,
                    phase = ?runtime.phase,
                    "ignoring a registration the host did not ask for"
                );
                return Vec::new();
            }
            runtime.phase = Phase::Ready { since: now };
            runtime.commands = commands;
            runtime.last_active = now;
            // The attempt counter resets on a successful start, not on a
            // successful spawn: an app that crashes during its handshake
            // every time must keep backing off.
            runtime.attempt = 0;
            runtime.next_attempt_at = 0;
            return vec![HostEvent::Ready {
                app: app.to_string(),
                commands: count,
            }];
        }
        tracing::warn!(
            app,
            "registration for an app that is not installed; ignored"
        );
        Vec::new()
    }

    /// The transport saw the connection fail, or the handshake time out. Same
    /// treatment as a crash: the app is not usable and must be restarted.
    pub fn on_lost(&mut self, app: &str, reason: &str) -> Vec<HostEvent> {
        tracing::warn!(app, reason, "app connection lost");
        let Some(runtime) = self.runtimes.get_mut(app) else {
            return Vec::new();
        };
        if let Some(pid) = runtime.pid.take() {
            // The process may still be alive but unreachable; a stuck app
            // holding the socket would stop the restart from ever handshaking.
            if let Err(e) = self.env.terminate(pid) {
                tracing::warn!(app, pid, error = %e, "cannot signal a lost app");
            }
        }
        self.record_crash(app)
    }

    /// Step 5: supervision. Call this on a timer.
    ///
    /// It does three things in order — notice dead children, restart what is
    /// due, stop what has gone idle — and returns everything the desktop
    /// should hear about.
    pub fn poll(&mut self) -> Vec<HostEvent> {
        let now = self.env.now();
        let mut events = Vec::new();
        let names: Vec<String> = self.runtimes.keys().cloned().collect();

        for name in &names {
            let Some(runtime) = self.runtimes.get(name) else {
                continue;
            };

            // A child that exited, or one that was spawned and never
            // registered. Both leave a window with nothing behind it.
            let dead = match (runtime.pid, &runtime.phase) {
                (Some(pid), Phase::Ready { .. }) => !self.env.is_running(pid),
                (Some(pid), Phase::Starting { since }) => {
                    !self.env.is_running(pid)
                        || now.saturating_sub(*since) >= self.policy.handshake_secs
                }
                _ => false,
            };

            if dead {
                let stalled = matches!(runtime.phase, Phase::Starting { .. });
                if let Some(pid) = self.runtimes.get_mut(name).and_then(|r| r.pid.take())
                    && stalled
                {
                    // Nothing registered within the timeout: ask it to go
                    // before trying again, so two instances never race for
                    // the same socket.
                    if let Err(e) = self.env.terminate(pid) {
                        tracing::warn!(app = %name, pid, error = %e, "cannot signal a stalled app");
                    }
                }
                events.extend(self.record_crash(name));
            }
        }

        for name in &names {
            let due = match self.runtimes.get(name) {
                Some(runtime) => match runtime.phase {
                    Phase::Reconnecting { .. } => {
                        now >= runtime.next_attempt_at && self.wanted(name)
                    }
                    _ => false,
                },
                None => false,
            };
            if due && let Err(e) = self.launch(name) {
                tracing::warn!(app = %name, error = %e, "restart failed");
                events.extend(self.record_crash(name));
            }
        }

        for name in &names {
            let idle = match (self.installed.get(name), self.runtimes.get(name)) {
                (Some(app), Some(runtime)) => {
                    app.manifest.app.lifecycle == Lifecycle::OnDemand
                        && matches!(runtime.phase, Phase::Ready { .. })
                        && runtime.windows == 0
                        && now.saturating_sub(runtime.last_active) >= self.policy.idle_secs
                }
                _ => false,
            };
            if idle {
                self.stop(name, true);
                events.push(HostEvent::Stopped {
                    app: name.clone(),
                    idle: true,
                });
            }
        }

        events
    }

    /// Operator asked for another try after a crash loop.
    pub fn retry(&mut self, app: &str) -> Result<WindowStatus, HostError> {
        if !self.installed.contains_key(app) {
            return Err(self.why_unavailable(app));
        }
        if let Some(runtime) = self.runtimes.get_mut(app) {
            runtime.phase = Phase::Stopped;
            runtime.attempt = 0;
            runtime.next_attempt_at = 0;
            runtime.crashes.clear();
        }
        self.launch(app)?;
        Ok(self.window_status(app))
    }

    /// Stop one app. `idle` only changes how it is logged.
    pub fn stop(&mut self, app: &str, idle: bool) {
        let Some(runtime) = self.runtimes.get_mut(app) else {
            return;
        };
        let pid = runtime.pid.take();
        runtime.phase = Phase::Stopped;
        runtime.commands.clear();
        if let Some(pid) = pid {
            tracing::info!(app, pid, idle, "stopping app");
            if let Err(e) = self.env.terminate(pid) {
                tracing::warn!(app, pid, error = %e, "cannot signal app");
            }
        }
    }

    /// Stop everything, for a clean core shutdown. SIGTERM only: apps get the
    /// chance to flush, and the core's own exit is not held up by one that
    /// does not take it.
    pub fn stop_all(&mut self) {
        let names: Vec<String> = self.runtimes.keys().cloned().collect();
        for name in names {
            self.stop(&name, false);
        }
    }

    /// Record the operator's approval of what an app currently asks for.
    ///
    /// The fingerprint is taken from the manifest on disk at this moment, so
    /// approving an app approves the thing that was shown, not whatever the
    /// directory holds later.
    pub fn approve(&mut self, app: &str) -> Result<Grant, HostError> {
        let installed = self
            .installed
            .get(app)
            .ok_or_else(|| HostError::NotInstalled(app.to_string()))?;
        let grant = Grant {
            app: app.to_string(),
            fingerprint: installed.manifest.fingerprint(),
            needs: installed.manifest.needs.clone(),
            granted_at: self.env.now(),
        };
        self.grants.record(&grant)?;
        tracing::info!(app, fingerprint = %grant.fingerprint, "app approved");
        Ok(grant)
    }

    pub fn revoke(&mut self, app: &str) -> Result<(), HostError> {
        self.grants.revoke(app)?;
        self.stop(app, false);
        Ok(())
    }

    /// Spawn an app, after checking that it may run.
    fn launch(&mut self, app: &str) -> Result<LaunchPlan, HostError> {
        let installed = self
            .installed
            .get(app)
            .ok_or_else(|| HostError::NotInstalled(app.to_string()))?
            .clone();

        self.check_grant(&installed)?;

        // Refuses here rather than after a spawn, so an app that may not run
        // on this node never becomes a process that has to be killed.
        let plan = LaunchPlan::build(
            &installed,
            &self.install_dir,
            &self.working_dir,
            self.policy.confinement,
            &self.env,
        )?;

        match plan.confinement {
            Confinement::Enforced => {
                if plan.sandbox.network_is_coarse() {
                    // Said out loud at every launch, not once in a design
                    // note: the manifest's host list was approved as an
                    // allowlist and this node can only grant the network as a
                    // whole. See [`Sandbox::network_is_coarse`].
                    tracing::warn!(
                        app,
                        hosts = %plan.sandbox.network_hosts.join(", "),
                        "granting the whole network: Landlock has no per-host rules, so the \
                         manifest's host list is not enforced on this node"
                    );
                }
                if !plan.sandbox.unexpressible.is_empty() {
                    // Not a property of this app: `sandbox::Policy` has no
                    // list for non-directory paths, so these are skipped
                    // rather than granted. An app that opens one of them will
                    // see EACCES, and the operator hears it from the launcher
                    // instead of debugging the app.
                    tracing::warn!(
                        app,
                        paths = %plan
                            .sandbox
                            .unexpressible
                            .iter()
                            .map(|p| p.display().to_string())
                            .collect::<Vec<_>>()
                            .join(", "),
                        "not granted: Landlock cannot take a directory-only right on a file \
                         and sandbox::Policy has no list for files yet"
                    );
                }
                tracing::info!(
                    app,
                    program = %plan.program.display(),
                    socket = %plan.socket.display(),
                    gpu = plan.sandbox.gpu,
                    network = plan.sandbox.policy.network,
                    read = plan.sandbox.policy.read.len(),
                    read_write = plan.sandbox.policy.read_write.len(),
                    "launching app confined"
                );
            }
            Confinement::Unconfined => {
                tracing::warn!(
                    app,
                    program = %plan.program.display(),
                    needs = %summarize(&installed.manifest.needs),
                    "launching app UNCONFINED: this node is in developer mode, so the app runs \
                     with the full access of the user running Deckjack"
                );
            }
        }

        let pid = self.env.spawn(&plan).map_err(|source| HostError::Spawn {
            app: app.to_string(),
            source,
        })?;
        let now = self.env.now();
        if let Some(runtime) = self.runtimes.get_mut(app) {
            runtime.pid = Some(pid);
            runtime.phase = Phase::Starting { since: now };
            runtime.commands.clear();
            runtime.last_active = now;
        }
        Ok(plan)
    }

    /// An app may run if it asks for nothing, or if a stored grant matches
    /// what it currently asks for.
    fn check_grant(&self, installed: &Installed) -> Result<(), HostError> {
        if installed.manifest.needs.is_empty() {
            return Ok(());
        }
        let app = installed.name.clone();
        let summary = summarize(&installed.manifest.needs);

        match self.grants.grant(&app)? {
            Some(grant) if grant.fingerprint == installed.manifest.fingerprint() => Ok(()),
            Some(_) => Err(HostError::GrantStale { app, summary }),
            None => Err(HostError::NotApproved { app, summary }),
        }
    }

    /// Whether an app should be running at all: a service always, an
    /// on-demand app only while a window wants it. Without this, a crash loop
    /// in an app nobody has open would restart forever in the background.
    fn wanted(&self, app: &str) -> bool {
        match (self.installed.get(app), self.runtimes.get(app)) {
            (Some(installed), Some(runtime)) => {
                installed.manifest.app.lifecycle == Lifecycle::Service || runtime.windows > 0
            }
            _ => false,
        }
    }

    /// Move an app into `Reconnecting`, or give up if it has crashed too
    /// often inside the window.
    fn record_crash(&mut self, app: &str) -> Vec<HostEvent> {
        let now = self.env.now();
        let window = self.policy.crash_window_secs;
        let max_crashes = self.policy.max_crashes;
        let base = self.policy.restart_backoff_secs;
        let cap = self.policy.max_backoff_secs;

        let Some(runtime) = self.runtimes.get_mut(app) else {
            return Vec::new();
        };
        runtime.commands.clear();
        runtime.pid = None;
        runtime
            .crashes
            .retain(|at| now.saturating_sub(*at) <= window);
        runtime.crashes.push(now);
        runtime.attempt = runtime.attempt.saturating_add(1);

        if runtime.crashes.len() > max_crashes {
            let crashes = runtime.crashes.len();
            runtime.phase = Phase::GaveUp { at: now, crashes };
            tracing::error!(app, crashes, "app crash loop; left stopped");
            return vec![HostEvent::GaveUp {
                app: app.to_string(),
                crashes,
                reason: format!("crashed {crashes} times within {window}s"),
            }];
        }

        // Exponential, capped. The shift is bounded so a long-lived app with
        // a high attempt count cannot overflow it.
        let step = base.saturating_mul(1u64 << runtime.attempt.saturating_sub(1).min(16));
        let delay = step.min(cap);
        runtime.next_attempt_at = now.saturating_add(delay);
        runtime.phase = Phase::Reconnecting {
            since: now,
            attempt: runtime.attempt,
        };

        vec![HostEvent::Reconnecting {
            app: app.to_string(),
            attempt: runtime.attempt,
        }]
    }

    /// The reason an app cannot be opened, as the operator needs to read it.
    fn why_unavailable(&self, app: &str) -> HostError {
        if let Some(problem) = self
            .problems
            .iter()
            .find(|p| p.name.as_deref() == Some(app))
        {
            return HostError::Unavailable {
                app: app.to_string(),
                reason: problem.error.to_string(),
            };
        }
        HostError::NotInstalled(app.to_string())
    }
}

/// One line describing what an app asks for, for an approval prompt and for
/// the error when there is no approval.
pub fn summarize(needs: &Needs) -> String {
    let mut parts: Vec<String> = Vec::new();
    if needs.gpu {
        parts.push("the GPU".to_string());
    }
    if !needs.network.is_empty() {
        parts.push(format!("network access to {}", needs.network.join(", ")));
    }
    for need in &needs.working_dir {
        let how = if need.write { "read and write" } else { "read" };
        parts.push(format!("{how} {}", need.path));
    }
    if parts.is_empty() {
        "nothing beyond the command bus".to_string()
    } else {
        parts.join("; ")
    }
}

// ---------------------------------------------------------------------------
// UI routing
// ---------------------------------------------------------------------------

/// What [`ui_router`] needs from the server.
///
/// The asset itself comes from the app's own binary over its socket, so
/// fetching it means a round trip the HTTP layer cannot do by itself. The
/// server implements this over [`Host`] plus its connection table; keeping it
/// a trait is what stops this module from owning the transport.
pub trait UiHost: Send + Sync + 'static {
    fn window_status(&self, app: &str) -> WindowStatus;

    /// Fetch one asset from a ready app. Called on a blocking thread, so an
    /// implementation may wait on the app.
    fn ui_asset(&self, app: &str, path: &str) -> Result<Option<UiAsset>, UiError>;
}

#[derive(Debug, Error)]
pub enum UiError {
    #[error("{app} is not reachable: {reason}")]
    Unreachable { app: String, reason: String },

    #[error(transparent)]
    Protocol(#[from] ProtocolError),
}

/// The app UI mount, to be nested at [`UI_MOUNT`] **behind the sign-in gate**.
///
/// Serving apps from the core's own origin is the whole point: the browser
/// never learns an app's address, so sign-in, CSP and theming are the core's
/// to enforce and an app cannot be reached around them.
///
/// ```ignore
/// let apps = Router::new().nest(apps::UI_MOUNT, apps::ui_router(ui_host));
/// let app = crate::http::router(ctx).merge(require_session(apps));
/// ```
pub fn ui_router(host: Arc<dyn UiHost>) -> Router {
    Router::new()
        .route("/{app}", get(redirect_to_dir))
        .route("/{app}/", get(serve_index))
        .route("/{app}/{*path}", get(serve_path))
        .with_state(host)
}

/// Relative URLs inside an app's `index.html` resolve against the directory,
/// so the trailing slash is load-bearing rather than cosmetic.
async fn redirect_to_dir(UrlPath(app): UrlPath<String>) -> Response {
    if !valid_name(&app) {
        return not_found();
    }
    Redirect::permanent(&format!("{UI_MOUNT}/{app}/")).into_response()
}

async fn serve_index(
    State(host): State<Arc<dyn UiHost>>,
    UrlPath(app): UrlPath<String>,
) -> Response {
    serve(host, app, UI_INDEX.to_string()).await
}

async fn serve_path(
    State(host): State<Arc<dyn UiHost>>,
    UrlPath((app, path)): UrlPath<(String, String)>,
) -> Response {
    serve(host, app, path).await
}

async fn serve(host: Arc<dyn UiHost>, app: String, path: String) -> Response {
    // The name is checked before it is used in a path, a log line or a
    // response body. Once past this it is known to be `[a-z0-9-]`, which is
    // why echoing it into HTML below needs no escaping.
    if !valid_name(&app) {
        return not_found();
    }
    // An app resolves asset paths inside its own embedded tree, but the core
    // must not forward a traversal attempt for it to interpret.
    if path.split('/').any(|part| part == ".." || part == ".") {
        return not_found();
    }

    match host.window_status(&app) {
        WindowStatus::Live => {}
        WindowStatus::Reconnecting => return reconnecting(&app),
        WindowStatus::Unavailable => return not_found(),
    }

    let fetch = {
        let host = Arc::clone(&host);
        let app = app.clone();
        let path = path.clone();
        // The asset comes from another process over a socket. Waiting for
        // that on an async worker would stall every other request on this
        // thread, so it goes to the blocking pool.
        tokio::task::spawn_blocking(move || host.ui_asset(&app, &path)).await
    };

    match fetch {
        Ok(Ok(Some(asset))) => asset_response(asset),
        Ok(Ok(None)) => not_found(),
        Ok(Err(e)) => {
            tracing::warn!(app = %app, path = %path, error = %e, "app UI fetch failed");
            reconnecting(&app)
        }
        Err(e) => {
            tracing::error!(app = %app, error = %e, "app UI fetch task failed");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

fn asset_response(asset: UiAsset) -> Response {
    // Hashed files may be cached forever; the entry document never, or an
    // app update would be invisible until a hard reload.
    let cache = if asset.immutable {
        "public, max-age=31536000, immutable"
    } else {
        "no-store"
    };
    axum::http::Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, asset.content_type)
        .header(header::CACHE_CONTROL, cache)
        // The app, not the core, chose the content type; sniffing past it
        // would let a mislabeled asset run as script.
        .header(header::X_CONTENT_TYPE_OPTIONS, "nosniff")
        .body(axum::body::Body::from(asset.bytes))
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
}

/// 503, not 500: the app is coming back, and the window should say so rather
/// than close. The header is for the shell, the body for anyone who reached
/// the URL directly.
fn reconnecting(app: &str) -> Response {
    axum::http::Response::builder()
        .status(StatusCode::SERVICE_UNAVAILABLE)
        .header(header::CACHE_CONTROL, "no-store")
        .header(header::RETRY_AFTER, "1")
        .header("x-deckjack-app-status", "reconnecting")
        .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
        .body(axum::body::Body::from(format!(
            "<!doctype html><meta charset=utf-8><title>Reconnecting</title>\
<p>{app} is restarting.</p>"
        )))
        .unwrap_or_else(|_| StatusCode::SERVICE_UNAVAILABLE.into_response())
}

fn not_found() -> Response {
    (
        StatusCode::NOT_FOUND,
        [(header::CACHE_CONTROL, "no-store")],
        "no such app asset",
    )
        .into_response()
}
