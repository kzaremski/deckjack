//! Configuration.
//!
//! One TOML file in the working directory. The defaults below are the file:
//! [`DEFAULT_TOML`] is what `deckjack init` writes and what every comment in
//! it documents, so there is one place to read rather than a struct and a
//! separate example drifting apart.
//!
//! Anything missing from the file takes its default, so an old file keeps
//! working when a key is added. Anything unknown is an error rather than a
//! shrug, because a typo in a security setting that silently does nothing is
//! worse than a failed start.

use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Where the config lives, relative to the working directory. Beside the lock
/// and the state database, inside the hidden directory that is never served.
pub const CONFIG_PATH: &str = ".deckjack/config.toml";

pub const DEFAULT_TOML: &str = r##"# Deckjack configuration.
#
# Written on first start. Keys left out take their default, so this file can
# be trimmed to only what you change. An unknown key is an error.

[server]
# Address to serve on. The default is loopback on purpose: Deckjack is reached
# through a private mesh (Tailscale or equivalent) with its HTTPS
# certificates, not by opening a port. See docs/DESIGN.md, Security.
listen = "127.0.0.1:7878"

# Peers whose X-Forwarded-For and X-Forwarded-Proto headers are believed.
#
# A reverse proxy is the only thing that may tell Deckjack who a request came
# from. Without this list every request is attributed to the socket it arrived
# on, which behind a proxy is the proxy itself: one address for everyone, so
# one person's wrong codes lock out everybody.
#
# List only addresses you control. Prefer `unix_socket` below for
# `tailscale serve`: trusting loopback here means believing any local process
# that can open a TCP connection, which includes any app granted the network,
# and the startup banner warns when that is the case.
trusted_proxies = []

# A Unix socket for `tailscale serve` to proxy to, in addition to `listen`.
# Relative paths are under the install directory (~/.deckjack, or
# $DECKJACK_HOME). Requests that arrive on it are the ONLY ones whose
# X-Forwarded-For, X-Forwarded-Proto and X-Forwarded-Host are believed: no
# app and no other user can open it (mode 0600, in an owner-only directory),
# while tailscaled runs as root and can. Empty means no socket. Then:
#
#   unix_socket = "proxy/http.sock"
#   sudo tailscale serve --bg unix:/home/YOU/.deckjack/proxy/http.sock
#
# Not under run/: that directory is the app sockets', named after the apps,
# and a node refuses to start with its proxy socket there.
#
# `listen` stays up beside it, on loopback, for `deckjack unlock` and for a
# browser on this machine; nothing that arrives there is believed.
unix_socket = ""

# The uid a connection to unix_socket must come from (checked with
# SO_PEERCRED). Root, which is what tailscaled runs as. The file's mode keeps
# other users out but not apps, which run as you; this keeps apps out. Change
# it only if your tailscaled runs as another user, and if that user is you,
# understand that an app could then pose as the proxy.
unix_socket_peer_uid = 0

[auth]
# Wrong codes from one address before it is blocked.
per_ip_attempts = 3
# Wrong codes in total before every sign-in is refused until restart.
global_attempts = 10
# How long a session lasts.
session_max_hours = 24
# Sessions kept at once; signing in again drops the oldest.
max_sessions = 4
# Pause after a wrong code. Long enough to make guessing pointless, short
# enough not to notice when you mistype once.
failure_delay_ms = 750

[passkey]
# The name the browser is asked to bind a passkey to, and the origin it must
# be loaded from. Both empty means loopback: `localhost` over the listen port,
# which WebAuthn accepts as a secure context with no certificate, so a first
# run works out of the box.
#
# Set these BEFORE enrolling the first passkey. A credential is bound to the
# rp_id it was created under and cannot be moved: changing rp_id afterwards
# does not break sign-in temporarily, it orphans every passkey already
# enrolled, permanently.
#
# rp_id is a registrable domain — no scheme, no port, and NOT an IP address,
# which WebAuthn does not accept. origin is the full URL the browser loads,
# and must be https:// for anything but localhost. For a tailnet name:
#
#   rp_id = "pocket.tail1234.ts.net"
#   origin = "https://pocket.tail1234.ts.net"
#
# Left empty on a node that listens on a non-loopback address, passkeys are
# switched off with a warning rather than enrolled against a name the browser
# will later reject.
rp_id = ""
origin = ""

[agents]
# Agent sessions this node opens a door for. Each listed id gets one socket
# under ~/.deckjack/run/agents/, in a directory only this user can read, and
# an agent attaches by connecting to it:
#
#   sessions = ["assistant"]
#
# Then point an MCP client at it:
#
#   command = "deckjack", args = ["agent", "assistant", "--dir", "/path/to/workdir"]
#
# Listing a session here does NOT grant it anything. The socket is the door;
# the grant issued to the same id is the authority, and a session with no
# grant attaches successfully and is offered no tools at all. Issue grants
# with system.grant.issue, from the Inbox, and revoke them there.
#
# Whoever can open the socket IS that session, so the two are separate knobs
# on purpose: you can revoke an agent's authority without taking its door
# away, and see every door in this one list.
sessions = []
"##;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
#[derive(Default)]
pub struct Config {
    pub server: Server,
    pub auth: Auth,
    pub passkey: Passkey,
    pub agents: Agents,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Server {
    pub listen: SocketAddr,
    pub trusted_proxies: Vec<IpAddr>,
    /// Where to listen for `tailscale serve unix:/...`. Empty means nowhere.
    /// See [`Server::unix_socket_path`].
    pub unix_socket: PathBuf,
    /// The only uid whose connections to the proxy socket are served.
    pub unix_socket_peer_uid: u32,
}

/// The install directory's folder for the proxy socket, listed in
/// [`crate::node::INSTALL_LAYOUT`]. See [`Server::unix_socket_path`].
pub const PROXY_SUBDIR: &str = "proxy";

/// `path` with `.` dropped and each `..` taking off the component before it,
/// without touching the disk.
fn lexically_normal(path: &Path) -> PathBuf {
    use std::path::Component;
    let mut out = PathBuf::new();
    for part in path.components() {
        match part {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other),
        }
    }
    out
}

impl Server {
    /// The proxy socket's path, resolved: a relative `unix_socket` is under
    /// `install_dir`, an absolute one is taken as written, an empty one means
    /// no socket.
    ///
    /// Refused inside the app socket directory (`<install>/run`): app
    /// sockets there are named `<app>.sock`, and launching an app binds its
    /// socket by removing whatever is at that path, so an app named after
    /// the proxy socket would silently take it over.
    ///
    /// Anywhere else inside the install directory it must be under
    /// [`PROXY_SUBDIR`], the one place [`crate::node::INSTALL_LAYOUT`] keeps
    /// out of the file layer and out of app grants. Elsewhere the socket
    /// would be an ordinary file in the default working directory, which an
    /// app granted that folder could move aside and replace with its own;
    /// tailscaled would then forward every session to the app.
    ///
    /// The checks are lexical, after resolving `.` and `..`; they are there
    /// to stop the natural mistake, not a determined one.
    pub fn unix_socket_path(&self, install_dir: &Path) -> Result<Option<PathBuf>, String> {
        if self.unix_socket.as_os_str().is_empty() {
            return Ok(None);
        }
        let path = install_dir.join(&self.unix_socket);
        let lexical = lexically_normal(&path);
        let run = install_dir.join(crate::apps::RUN_SUBDIR);
        if lexical.starts_with(&run) {
            return Err(format!(
                "server.unix_socket = {:?} is inside {}, where app sockets live and an app \
                 could replace it. Put it elsewhere, e.g. \"proxy/http.sock\"",
                self.unix_socket,
                run.display()
            ));
        }
        let proxy = install_dir.join(PROXY_SUBDIR);
        if lexical.starts_with(install_dir) && !lexical.starts_with(&proxy) {
            return Err(format!(
                "server.unix_socket = {:?} is inside the install directory {} but not under \
                 proxy/, so Files and app grants could reach it and an app could replace it. \
                 Use \"proxy/http.sock\", or a path outside the install directory",
                self.unix_socket,
                install_dir.display()
            ));
        }
        Ok(Some(path))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Auth {
    pub per_ip_attempts: u32,
    pub global_attempts: u32,
    pub session_max_hours: u32,
    pub max_sessions: u32,
    pub failure_delay_ms: u64,
}

/// Which agent sessions this node opens a door for.
///
/// Listing a session here creates its socket; it does **not** grant it
/// anything. The two are separate on purpose. Deriving the sockets from the
/// grant store instead would mean issuing a grant silently opened a door and
/// revoking one silently removed it, so the operator could never see the
/// node's agent surface in one place — and a door that appears as a
/// side effect of a permission change is exactly the kind of thing nobody
/// audits.
///
/// An empty list means no agent can attach, which is the default.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields, default)]
pub struct Agents {
    /// Agent session ids. Each gets one socket under the hidden directory,
    /// and whatever grant the operator issued to that same id.
    pub sessions: Vec<String>,
}

/// What a passkey ceremony is bound to.
///
/// Both empty means "derive loopback settings from `server.listen`", which is
/// the only thing that can be derived correctly: WebAuthn needs a secure
/// context and a registrable domain, and neither can be guessed from an IP
/// address. See [`DEFAULT_TOML`] for why these must be set before the first
/// enrollment rather than after.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields, default)]
pub struct Passkey {
    /// Registrable domain: no scheme, no port, not an IP.
    pub rp_id: String,
    /// The full URL the browser loads.
    pub origin: String,
}

impl Passkey {
    /// Whether the operator stated both halves.
    ///
    /// Both or neither: an `rp_id` with no `origin` (or the reverse) is a
    /// configuration that produces ceremonies the browser rejects, and
    /// guessing the missing half is how you bind credentials to a name
    /// nobody chose.
    pub fn is_set(&self) -> bool {
        !self.rp_id.is_empty() && !self.origin.is_empty()
    }

    /// Set, half-set or unset — the three cases a caller has to tell apart.
    pub fn half_set(&self) -> bool {
        self.rp_id.is_empty() != self.origin.is_empty()
    }
}

impl Default for Server {
    fn default() -> Server {
        Server {
            listen: SocketAddr::from(([127, 0, 0, 1], 7878)),
            trusted_proxies: Vec::new(),
            unix_socket: PathBuf::new(),
            unix_socket_peer_uid: 0,
        }
    }
}

impl Default for Auth {
    fn default() -> Auth {
        Auth {
            per_ip_attempts: 3,
            global_attempts: 10,
            session_max_hours: 24,
            max_sessions: 4,
            failure_delay_ms: 750,
        }
    }
}

impl Auth {
    pub fn failure_delay(&self) -> Duration {
        Duration::from_millis(self.failure_delay_ms)
    }

    pub fn limits(&self) -> crate::auth::state::Limits {
        crate::auth::state::Limits {
            per_ip_attempts: self.per_ip_attempts,
            global_attempts: self.global_attempts,
            session_max: Duration::from_secs(u64::from(self.session_max_hours) * 3600),
            max_sessions: self.max_sessions as usize,
        }
    }
}

/// Smallest values that still mean something. A zero attempt limit would lock
/// the door before the first try; a zero session length would expire a session
/// as it was created.
pub const MIN_ATTEMPTS: u32 = 1;
pub const MIN_SESSION_HOURS: u32 = 1;
pub const MIN_SESSIONS: u32 = 1;

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("cannot read {path}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("{path} is not valid: {source}")]
    Parse {
        path: PathBuf,
        #[source]
        source: toml::de::Error,
    },
    #[error("{path}: {message}")]
    Invalid { path: PathBuf, message: String },
    #[error("cannot write {path}: {source}")]
    Write {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

impl Config {
    /// Load the config for a working directory, writing the default file the
    /// first time so there is something to edit.
    pub fn load_or_init(working_dir: &Path) -> Result<Config, ConfigError> {
        let path = working_dir.join(CONFIG_PATH);

        if !path.exists() {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).map_err(|source| ConfigError::Write {
                    path: path.clone(),
                    source,
                })?;
            }
            std::fs::write(&path, DEFAULT_TOML).map_err(|source| ConfigError::Write {
                path: path.clone(),
                source,
            })?;
        }

        Config::load(&path)
    }

    pub fn load(path: &Path) -> Result<Config, ConfigError> {
        let text = std::fs::read_to_string(path).map_err(|source| ConfigError::Read {
            path: path.to_path_buf(),
            source,
        })?;
        let cfg: Config = toml::from_str(&text).map_err(|source| ConfigError::Parse {
            path: path.to_path_buf(),
            source,
        })?;
        cfg.validate(path)?;
        Ok(cfg)
    }

    fn validate(&self, path: &Path) -> Result<(), ConfigError> {
        let bad = |message: String| ConfigError::Invalid {
            path: path.to_path_buf(),
            message,
        };

        if self.auth.per_ip_attempts < MIN_ATTEMPTS {
            return Err(bad(format!(
                "auth.per_ip_attempts must be at least {MIN_ATTEMPTS}"
            )));
        }
        if self.auth.global_attempts < self.auth.per_ip_attempts {
            return Err(bad(
                "auth.global_attempts must not be below auth.per_ip_attempts, or one \
                 address could lock everybody out before it was blocked itself"
                    .to_string(),
            ));
        }
        if self.auth.session_max_hours < MIN_SESSION_HOURS {
            return Err(bad(format!(
                "auth.session_max_hours must be at least {MIN_SESSION_HOURS}"
            )));
        }
        if self.auth.max_sessions < MIN_SESSIONS {
            return Err(bad(format!(
                "auth.max_sessions must be at least {MIN_SESSIONS}"
            )));
        }
        for session in &self.agents.sessions {
            if let Err(e) = crate::mcpserve::check_session_name(session) {
                return Err(bad(format!("agents.sessions has an unusable id: {e}")));
            }
        }
        {
            // A duplicate would bind the same socket twice: the second mount
            // would fail at startup with "address in use" long after the
            // config was read, which is a confusing way to learn about a
            // typo in a list.
            let mut seen = std::collections::BTreeSet::new();
            for session in &self.agents.sessions {
                if !seen.insert(session) {
                    return Err(bad(format!(
                        "agents.sessions lists {session:?} more than once"
                    )));
                }
            }
        }
        if self.passkey.half_set() {
            return Err(bad(
                "passkey.rp_id and passkey.origin must both be set or both be empty: one \
                 without the other produces ceremonies the browser rejects, and \
                 guessing the other half would bind every enrolled credential to \
                 a name nobody chose"
                    .to_string(),
            ));
        }
        // An rp_id is a registrable domain. Caught here rather than at the
        // first ceremony, because by then a credential may already be bound
        // to it and a credential cannot be rebound.
        if !self.passkey.rp_id.is_empty() {
            let bad_rp = self.passkey.rp_id.contains("://")
                || self.passkey.rp_id.contains(':')
                || self.passkey.rp_id.contains('/')
                || self.passkey.rp_id.parse::<IpAddr>().is_ok();
            if bad_rp {
                return Err(bad(format!(
                    "passkey.rp_id must be a bare domain with no scheme, no port and no path, \
                     and cannot be an IP address: {:?} is not one",
                    self.passkey.rp_id
                )));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_file_parses_to_the_defaults() {
        // The embedded file and the Default impls are two statements of the
        // same thing; this is what keeps them from drifting.
        let parsed: Config = toml::from_str(DEFAULT_TOML).unwrap();
        assert_eq!(parsed, Config::default());
    }

    #[test]
    fn missing_keys_take_defaults() {
        let cfg: Config = toml::from_str("[auth]\nmax_sessions = 9\n").unwrap();
        assert_eq!(cfg.auth.max_sessions, 9);
        assert_eq!(cfg.auth.per_ip_attempts, 3);
        assert_eq!(cfg.server.listen, Config::default().server.listen);
    }

    #[test]
    fn unknown_keys_are_an_error() {
        let e = toml::from_str::<Config>("[auth]\nper_ip_attemps = 3\n").unwrap_err();
        assert!(e.to_string().contains("per_ip_attemps"), "{e}");
    }

    #[test]
    fn trusted_proxies_is_empty_by_default() {
        // Trusting a proxy means believing a header about who the caller is.
        // That must be opted into, never inherited.
        assert!(Config::default().server.trusted_proxies.is_empty());
    }

    #[test]
    fn the_proxy_socket_is_off_by_default_and_relative_to_the_install_directory() {
        let install = Path::new("/home/someone/.deckjack");
        assert_eq!(Config::default().server.unix_socket_path(install), Ok(None));

        let cfg: Config = toml::from_str("[server]\nunix_socket = \"proxy/http.sock\"\n").unwrap();
        assert_eq!(
            cfg.server.unix_socket_path(install),
            Ok(Some(PathBuf::from(
                "/home/someone/.deckjack/proxy/http.sock"
            )))
        );
        let cfg: Config = toml::from_str("[server]\nunix_socket = \"/srv/dj/p.sock\"\n").unwrap();
        assert_eq!(
            cfg.server.unix_socket_path(install),
            Ok(Some(PathBuf::from("/srv/dj/p.sock")))
        );
    }

    #[test]
    fn the_proxy_socket_is_refused_where_app_sockets_live() {
        // An app named "proxy" binds <install>/run/proxy.sock by removing
        // whatever is there. Relative or absolute, run/ is refused.
        let install = Path::new("/home/someone/.deckjack");
        for spelled in ["run/proxy.sock", "/home/someone/.deckjack/run/proxy.sock"] {
            let cfg: Config =
                toml::from_str(&format!("[server]\nunix_socket = \"{spelled}\"\n")).unwrap();
            let err = cfg.server.unix_socket_path(install).unwrap_err();
            assert!(err.contains("app sockets"), "{spelled}: {err}");
        }
        // Control: a sibling under proxy/ whose name starts with "run" is fine.
        let cfg: Config =
            toml::from_str("[server]\nunix_socket = \"proxy/runner.sock\"\n").unwrap();
        assert!(cfg.server.unix_socket_path(install).is_ok());
    }

    #[test]
    fn inside_the_install_directory_the_proxy_socket_must_be_under_proxy() {
        // Only `proxy/` is in INSTALL_LAYOUT. Anywhere else in the install
        // directory the socket would be a file the operator's Files window,
        // and any app granted that folder, could move aside and replace.
        let install = Path::new("/home/someone/.deckjack");
        for spelled in [
            "http.sock",
            "sockets/http.sock",
            "/home/someone/.deckjack/http.sock",
            "proxy/../http.sock",
        ] {
            let cfg: Config =
                toml::from_str(&format!("[server]\nunix_socket = \"{spelled}\"\n")).unwrap();
            let err = cfg.server.unix_socket_path(install).unwrap_err();
            assert!(err.contains("proxy/"), "{spelled}: {err}");
        }
        for fine in [
            "proxy/http.sock",
            "/home/someone/.deckjack/proxy/http.sock",
            "/srv/dj/p.sock",
        ] {
            let cfg: Config =
                toml::from_str(&format!("[server]\nunix_socket = \"{fine}\"\n")).unwrap();
            assert!(cfg.server.unix_socket_path(install).is_ok(), "{fine}");
        }
        assert!(crate::node::INSTALL_LAYOUT.contains(&PROXY_SUBDIR));
    }

    #[test]
    fn global_below_per_ip_is_refused() {
        let toml_text = "[auth]\nper_ip_attempts = 5\nglobal_attempts = 2\n";
        let cfg: Config = toml::from_str(toml_text).unwrap();
        let err = cfg.validate(Path::new("test.toml")).unwrap_err();
        assert!(err.to_string().contains("global_attempts"), "{err}");
    }

    #[test]
    fn zero_attempts_is_refused() {
        let cfg: Config = toml::from_str("[auth]\nper_ip_attempts = 0\n").unwrap();
        assert!(cfg.validate(Path::new("test.toml")).is_err());
    }

    #[test]
    fn load_or_init_writes_then_reads_the_default() {
        let dir = tempfile::tempdir().unwrap();
        let first = Config::load_or_init(dir.path()).unwrap();
        assert_eq!(first, Config::default());
        assert!(dir.path().join(CONFIG_PATH).exists());

        // Second call reads what was written rather than rewriting it.
        std::fs::write(dir.path().join(CONFIG_PATH), "[auth]\nmax_sessions = 7\n").unwrap();
        let second = Config::load_or_init(dir.path()).unwrap();
        assert_eq!(second.auth.max_sessions, 7);
    }

    #[test]
    fn config_lives_in_the_hidden_directory() {
        assert!(Path::new(CONFIG_PATH).starts_with(".deckjack"));
    }

    #[test]
    fn a_half_written_passkey_section_is_refused() {
        // Not defaulted, not guessed. An rp_id with no origin produces
        // ceremonies the browser rejects; filling in the missing half would
        // bind every credential enrolled afterwards to a name the operator
        // never chose, and a credential cannot be rebound.
        for text in [
            "[passkey]\nrp_id = \"pocket.ts.net\"\n",
            "[passkey]\norigin = \"https://pocket.ts.net\"\n",
        ] {
            let cfg: Config = toml::from_str(text).unwrap();
            let err = cfg.validate(Path::new("test.toml")).unwrap_err();
            assert!(err.to_string().contains("both"), "{err}");
        }
    }

    #[test]
    fn an_rp_id_that_is_not_a_registrable_domain_is_refused() {
        // Caught at startup rather than at the first ceremony, because by
        // then a credential may already be bound to it.
        for bad_id in [
            "https://pocket.ts.net",
            "pocket.ts.net:7878",
            "pocket.ts.net/deckjack",
            "192.168.1.10",
            "::1",
        ] {
            let text =
                format!("[passkey]\nrp_id = \"{bad_id}\"\norigin = \"https://pocket.ts.net\"\n");
            let cfg: Config = toml::from_str(&text).unwrap();
            let err = match cfg.validate(Path::new("test.toml")) {
                Err(e) => e.to_string(),
                Ok(()) => panic!("{bad_id} should be refused as an rp_id"),
            };
            assert!(err.contains("rp_id"), "{err}");
        }
    }

    #[test]
    fn a_whole_passkey_section_is_accepted_and_an_empty_one_is_the_default() {
        let cfg: Config = toml::from_str(
            "[passkey]\nrp_id = \"pocket.ts.net\"\norigin = \"https://pocket.ts.net\"\n",
        )
        .unwrap();
        assert!(cfg.validate(Path::new("test.toml")).is_ok());
        assert!(cfg.passkey.is_set());

        // The default is both-empty, which means "derive loopback settings",
        // and it must stay valid or a fresh install would not start.
        let fresh = Config::default();
        assert!(!fresh.passkey.is_set() && !fresh.passkey.half_set());
        assert!(fresh.validate(Path::new("test.toml")).is_ok());
    }

    #[test]
    fn limits_convert_hours_to_a_duration() {
        let a = Auth {
            session_max_hours: 24,
            ..Auth::default()
        };
        assert_eq!(a.limits().session_max, Duration::from_secs(86_400));
    }
}
