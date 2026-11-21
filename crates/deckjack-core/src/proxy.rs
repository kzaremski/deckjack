//! The proxy socket: where `tailscale serve unix:/...` hands requests over.
//!
//! D2 in docs/APP-UI-ISOLATION.md. Behind `tailscale serve`, the client's
//! address and scheme arrive as `X-Forwarded-For` and `X-Forwarded-Proto`,
//! and believing them is what lets the per-address lockout count the right
//! device and the session cookie be marked `Secure`. Believing them from
//! loopback TCP — the old way, `trusted_proxies = ["127.0.0.1"]` — means
//! believing any local process that can open a TCP connection, and an app
//! granted the network is such a process: it could choose its own address
//! and claim HTTPS.
//!
//! A Unix socket fixes that by making the door itself the credential, in two
//! layers:
//!
//! 1. **The file.** Mode 0600, in a directory created owner-only, so no
//!    other user can connect. tailscaled runs as root, and root's
//!    `CAP_DAC_OVERRIDE` bypasses both the socket's mode and the directory's,
//!    so it still can.
//! 2. **The peer's uid**, read with `SO_PEERCRED` on every accepted
//!    connection ([`Filtered`]). The file mode alone is not enough, because
//!    an app runs as the same user as Deckjack, and Landlock up to the ABI
//!    this build asks for (v4) does not restrict `connect` on a pathname Unix
//!    socket at all — an app already connects to its own socket in the same
//!    directory. So a connection is served only if its peer is the uid named
//!    by `server.unix_socket_peer_uid`, root by default: what tailscaled runs
//!    as, and what no app runs as.
//!
//! Requests off this socket are marked with [`SOCKET_PEER`], and
//! [`crate::http::Ctx`] trusts that mark and nothing else without an address
//! being listed.
//!
//! **Host.** `tailscale serve` sends `Host: localhost` to a Unix-socket
//! backend (it rewrites the outbound host to the target URL's, which for a
//! socket is `http://localhost`) and puts the browser's host in
//! `X-Forwarded-Host`. Every Origin check in the core compares `Origin`
//! against `Host`, so sign-in and the bus would refuse every request.
//! [`router`] restores `Host` from `X-Forwarded-Host`, on this socket only.
//!
//! What tailscale sends, from `ipn/ipnlocal/serve.go`: it proxies with Go's
//! `httputil.ReverseProxy` and a `Rewrite` function, which drops any
//! `X-Forwarded-*` the client sent before tailscale sets its own:
//! `X-Forwarded-For` from the connection's source address, `X-Forwarded-Host`
//! always, and `X-Forwarded-Proto: https` only when the browser's connection
//! was TLS. So a client cannot smuggle either header through it.

use std::net::{IpAddr, Ipv6Addr, SocketAddr};
use std::os::unix::fs::{DirBuilderExt as _, MetadataExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};

use axum::Router;
use axum::extract::{ConnectInfo, Request};
use axum::http::{HeaderValue, header};
use axum::middleware::{self, Next};
use axum::response::Response;
use thiserror::Error;

/// The peer address given to every request that arrived on the proxy socket.
///
/// `[::]:0`. No TCP connection can have it: the unspecified address is never
/// a source address and port 0 is never a source port, so a request carrying
/// it came off this socket and nowhere else. A sentinel rather than a new
/// extractor type, so every handler that already takes
/// `ConnectInfo<SocketAddr>` keeps working unchanged and cannot forget to
/// look somewhere else.
///
/// It is also the address a request is attributed to when the proxy sent no
/// `X-Forwarded-For`, which `tailscale serve` always sends; the lockout then
/// counts every such request together, which errs toward locking rather than
/// toward letting any of them through uncounted.
pub const SOCKET_PEER: SocketAddr = SocketAddr::new(IpAddr::V6(Ipv6Addr::UNSPECIFIED), 0);

/// `sun_path` holds 108 bytes on Linux, one of them the terminating NUL.
const MAX_SOCKET_PATH: usize = 107;

/// The same application, as the proxy socket serves it.
///
/// Wraps `app` outermost, so every route, fallback and gate inside it sees
/// the marked peer and the restored `Host`.
pub fn router(app: Router) -> Router {
    app.layer(middleware::from_fn(mark))
}

async fn mark(mut request: Request, next: Next) -> Response {
    // Inserted, not merely defaulted: whatever an inner layer would have
    // found here is replaced, so there is exactly one answer to "where did
    // this come from" for a request off this socket.
    request.extensions_mut().insert(ConnectInfo(SOCKET_PEER));
    let forwarded_host = request
        .headers()
        .get("x-forwarded-host")
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|v| !v.is_empty() && !v.contains(','))
        .and_then(|v| HeaderValue::from_str(v).ok());
    if let Some(host) = forwarded_host {
        request.headers_mut().insert(header::HOST, host);
    }
    next.run(request).await
}

#[derive(Debug, Error)]
pub enum BindError {
    #[error(
        "{path} is {len} bytes, and a Unix socket path can be at most {MAX_SOCKET_PATH}: \
         set server.unix_socket to a shorter path"
    )]
    TooLong { path: PathBuf, len: usize },

    #[error(
        "{path} is already being served by another process. Two nodes are configured with \
         the same server.unix_socket; give each its own"
    )]
    InUse { path: PathBuf },

    #[error(
        "{path} exists and is not a socket; refusing to replace it. Move it, or choose \
             another server.unix_socket"
    )]
    NotASocket { path: PathBuf },

    #[error("cannot open the proxy socket at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

/// A bound proxy socket. Dropping it removes the socket file, if the file at
/// that path is still the one this bound.
pub struct Socket {
    listener: Filtered,
    guard: Remove,
}

impl Socket {
    pub fn path(&self) -> &Path {
        &self.guard.path
    }

    /// The listener to serve, and the guard that removes the file when it is
    /// dropped. Kept apart so the guard can outlive the server future.
    pub fn into_parts(self) -> (Filtered, Remove) {
        (self.listener, self.guard)
    }
}

/// The proxy socket's listener: hands axum only the connections whose peer
/// is the allowed uid, and drops the rest at accept, before a byte of HTTP is
/// read. See the module doc for why the file mode is not enough.
pub struct Filtered {
    inner: tokio::net::UnixListener,
    peer_uid: u32,
}

impl axum::serve::Listener for Filtered {
    type Io = tokio::net::UnixStream;
    type Addr = tokio::net::unix::SocketAddr;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        loop {
            let (stream, addr) = axum::serve::Listener::accept(&mut self.inner).await;
            match stream.peer_cred() {
                Ok(cred) if cred.uid() == self.peer_uid => return (stream, addr),
                Ok(cred) => tracing::warn!(
                    uid = cred.uid(),
                    pid = ?cred.pid(),
                    allowed = self.peer_uid,
                    "proxy socket: refused a connection from a uid that is not the proxy's"
                ),
                Err(e) => tracing::warn!(error = %e,
                    "proxy socket: refused a connection whose peer could not be identified"),
            }
        }
    }

    fn local_addr(&self) -> std::io::Result<Self::Addr> {
        self.inner.local_addr()
    }
}

/// Removes the socket file on drop — clean shutdown — but only if the path
/// still names the inode this process bound. A node that crashed leaves its
/// file behind, which [`bind`] handles; a node shutting down must not delete
/// a successor's.
pub struct Remove {
    path: PathBuf,
    dev: u64,
    ino: u64,
    /// The exclusive lock on `<path>.lock`, held for as long as this node
    /// serves the path, and released by the kernel however it exits. Dropped
    /// after the file is removed, because fields drop after `drop` runs.
    _lock: nix::fcntl::Flock<std::fs::File>,
}

impl Drop for Remove {
    fn drop(&mut self) {
        match std::fs::symlink_metadata(&self.path) {
            Ok(meta) if meta.dev() == self.dev && meta.ino() == self.ino => {
                if let Err(e) = std::fs::remove_file(&self.path) {
                    tracing::warn!(path = %self.path.display(), error = %e,
                        "cannot remove the proxy socket");
                }
            }
            _ => {}
        }
    }
}

/// Bind the proxy socket at `path`.
///
/// - A missing parent directory is created owner-only. An existing one is
///   left as it is: it is the operator's.
/// - An exclusive `flock` on `<path>.lock` is taken first and held for the
///   node's life. The working directory's lock does not settle who owns the
///   path, because it is under the install directory, which several working
///   directories share; without this lock two nodes starting together could
///   both find the path empty and both rename into it, and the first would
///   be silently orphaned. Held by another node: [`BindError::InUse`]. The
///   lock file is never removed (removing it would reopen the race).
/// - A socket already at `path` is probed. If something answers, it belongs
///   to a live node and is left alone ([`BindError::InUse`]). If nothing
///   answers, it is a crashed node's leftover and is replaced.
/// - Anything at `path` that is not a socket is never deleted.
/// - The socket is bound under a temporary name, set to 0600, and renamed
///   into place, so it never exists at `path` with a wider mode.
///
/// `peer_uid` is the only uid whose connections are served; see
/// [`Filtered`].
///
/// Must be called inside a tokio runtime.
pub fn bind(path: &Path, peer_uid: u32) -> Result<Socket, BindError> {
    let io = |source: std::io::Error| BindError::Io {
        path: path.to_path_buf(),
        source,
    };
    let parent = path.parent().unwrap_or(Path::new("/"));
    let staging = parent.join(format!(".proxy-{}.sock", std::process::id()));
    for candidate in [path, staging.as_path()] {
        let len = candidate.as_os_str().len();
        if len > MAX_SOCKET_PATH {
            return Err(BindError::TooLong {
                path: candidate.to_path_buf(),
                len,
            });
        }
    }

    if !parent.exists() {
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(parent)
            .map_err(io)?;
    }

    let lock = {
        use std::os::unix::fs::OpenOptionsExt as _;
        let mut lock_name = path.as_os_str().to_owned();
        lock_name.push(".lock");
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .mode(0o600)
            .open(PathBuf::from(lock_name))
            .map_err(io)?;
        match nix::fcntl::Flock::lock(file, nix::fcntl::FlockArg::LockExclusiveNonblock) {
            Ok(lock) => lock,
            Err((_, nix::errno::Errno::EWOULDBLOCK)) => {
                return Err(BindError::InUse {
                    path: path.to_path_buf(),
                });
            }
            Err((_, errno)) => return Err(io(errno.into())),
        }
    };

    match std::fs::symlink_metadata(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(io(e)),
        Ok(meta) => {
            use std::os::unix::fs::FileTypeExt as _;
            if !meta.file_type().is_socket() {
                return Err(BindError::NotASocket {
                    path: path.to_path_buf(),
                });
            }
            match std::os::unix::net::UnixStream::connect(path) {
                Ok(_) => {
                    return Err(BindError::InUse {
                        path: path.to_path_buf(),
                    });
                }
                // Nobody listening: a leftover. Replaced by the rename below,
                // atomically, rather than removed first.
                Err(e) if e.kind() == std::io::ErrorKind::ConnectionRefused => {
                    tracing::info!(path = %path.display(), "replacing a stale proxy socket");
                }
                Err(e) => return Err(io(e)),
            }
        }
    }

    match std::fs::remove_file(&staging) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(io(e)),
    }
    let listener = std::os::unix::net::UnixListener::bind(&staging).map_err(io)?;
    let placed = std::fs::set_permissions(&staging, std::fs::Permissions::from_mode(0o600))
        .and_then(|()| std::fs::rename(&staging, path));
    if let Err(e) = placed {
        let _ = std::fs::remove_file(&staging);
        return Err(io(e));
    }
    let meta = std::fs::symlink_metadata(path).map_err(io)?;
    let guard = Remove {
        path: path.to_path_buf(),
        dev: meta.dev(),
        ino: meta.ino(),
        _lock: lock,
    };
    listener.set_nonblocking(true).map_err(io)?;
    let inner = tokio::net::UnixListener::from_std(listener).map_err(io)?;
    Ok(Socket {
        listener: Filtered { inner, peer_uid },
        guard,
    })
}

/// The apps whose stored approval includes the network, by name: hosts named
/// in `needs.network` or ports in `needs.connect_tcp`.
pub fn network_grant_holders(grants: &[crate::apps::Grant]) -> Vec<String> {
    grants
        .iter()
        .filter(|g| !g.needs.network.is_empty() || !g.needs.connect_tcp.is_empty())
        .map(|g| g.app.clone())
        .collect()
}

/// The startup banner's warning when `trusted_proxies` believes an address
/// local processes can arrive from while an approved app holds a network
/// grant, or `None`.
///
/// Local means loopback, in either spelling (`::ffff:127.0.0.1` is what a
/// dual-stack core on `[::]` sees for an IPv4 loopback client), or the
/// core's own listen address, which is where a TCP `tailscale serve` pointed
/// at the node's tailnet address arrives from.
///
/// Approval and launch already refuse a grant that reaches the core
/// ([`crate::apps::reaches_core`]), so this is the second line: it says out
/// loud that the configuration depends on that refusal, and what to switch
/// to so it does not.
pub fn local_trust_warning(
    trusted: &[IpAddr],
    listen: SocketAddr,
    network_apps: &[String],
) -> Option<String> {
    let own = listen.ip().to_canonical();
    let local: Vec<String> = trusted
        .iter()
        .filter(|ip| {
            let ip = ip.to_canonical();
            ip.is_loopback() || ip == own
        })
        .map(IpAddr::to_string)
        .collect();
    if local.is_empty() || network_apps.is_empty() {
        return None;
    }
    Some(format!(
        "WARNING: trusted_proxies believes an address local processes connect from ({}) \
         while {} hold{} a network grant. Any local process that can reach this port could \
         claim any address and claim HTTPS. Use server.unix_socket for `tailscale serve` and \
         empty trusted_proxies.",
        local.join(", "),
        network_apps.join(", "),
        if network_apps.len() == 1 { "s" } else { "" },
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_warning_needs_both_local_trust_and_a_network_grant() {
        let ip = |s: &str| -> IpAddr { s.parse().unwrap() };
        let on_loopback: SocketAddr = "127.0.0.1:7878".parse().unwrap();
        let on_tailnet: SocketAddr = "100.64.0.1:7878".parse().unwrap();
        let apps = vec!["pim".to_string()];

        let w = local_trust_warning(&[ip("127.0.0.1")], on_loopback, &apps)
            .expect("loopback plus a grant warns");
        assert!(w.contains("127.0.0.1") && w.contains("pim") && w.contains("unix_socket"));
        assert!(local_trust_warning(&[ip("::1")], on_loopback, &apps).is_some());
        // The spelling a dual-stack core sees for IPv4 loopback.
        assert!(local_trust_warning(&[ip("::ffff:127.0.0.1")], on_loopback, &apps).is_some());
        // A TCP `tailscale serve` to the node's own address arrives from it.
        assert!(local_trust_warning(&[ip("100.64.0.1")], on_tailnet, &apps).is_some());
        // Controls: each half alone is quiet, and an address that is neither
        // loopback nor the core's own is a remote proxy.
        assert_eq!(
            local_trust_warning(&[ip("127.0.0.1")], on_loopback, &[]),
            None
        );
        assert_eq!(
            local_trust_warning(&[ip("100.64.0.1")], on_loopback, &apps),
            None
        );
        assert_eq!(local_trust_warning(&[], on_loopback, &apps), None);
    }

    #[test]
    fn network_grant_holders_are_read_back_from_the_store() {
        use crate::apps::{Grant, Needs};
        let store = crate::store::Store::in_memory().unwrap();
        let grant = |app: &str, needs: Needs| Grant {
            app: app.to_string(),
            fingerprint: format!("fp-{app}"),
            needs,
            granted_at: 1,
        };
        store
            .record_app_grant(&grant(
                "pim",
                Needs {
                    network: vec!["cloud.example.net".to_string()],
                    connect_tcp: vec![443],
                    ..Needs::default()
                },
            ))
            .unwrap();
        store
            .record_app_grant(&grant(
                "paint",
                Needs {
                    gpu: true,
                    ..Needs::default()
                },
            ))
            .unwrap();
        store
            .record_app_grant(&grant(
                "feeds",
                Needs {
                    network: vec!["example.org".to_string()],
                    ..Needs::default()
                },
            ))
            .unwrap();

        let grants = store.app_grants().unwrap();
        assert_eq!(grants.len(), 3, "every stored approval is listed");
        assert_eq!(network_grant_holders(&grants), vec!["feeds", "pim"]);
    }

    #[test]
    fn the_sentinel_is_not_an_address_a_tcp_peer_can_have() {
        assert!(SOCKET_PEER.ip().is_unspecified());
        assert_eq!(SOCKET_PEER.port(), 0);
    }
}
