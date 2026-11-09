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
# List only addresses you control. `tailscale serve` proxies from loopback, so
# loopback is the usual answer; it is NOT a default, because trusting loopback
# on a machine where anyone can open a socket means anyone can claim any
# address.
trusted_proxies = []

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
"##;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
#[derive(Default)]
pub struct Config {
    pub server: Server,
    pub auth: Auth,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Server {
    pub listen: SocketAddr,
    pub trusted_proxies: Vec<IpAddr>,
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

impl Default for Server {
    fn default() -> Server {
        Server {
            listen: SocketAddr::from(([127, 0, 0, 1], 7878)),
            trusted_proxies: Vec::new(),
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
    fn limits_convert_hours_to_a_duration() {
        let a = Auth {
            session_max_hours: 24,
            ..Auth::default()
        };
        assert_eq!(a.limits().session_max, Duration::from_secs(86_400));
    }
}
