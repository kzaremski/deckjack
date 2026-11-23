//! `deckjack-stream`: spawned by the core, confined, for one native app.
//!
//! The core passes the app as JSON in `DECKJACK_STREAM_SPEC` and the link,
//! one end of a socket pair, as an inherited descriptor whose number is in
//! `DECKJACK_STREAM_FD`. See `deckjack_protocol::stream`.

#[cfg(target_os = "linux")]
fn main() -> anyhow::Result<()> {
    use anyhow::Context as _;
    use deckjack_protocol::stream::{ENV_LINK_FD, ENV_SPEC, LaunchSpec};
    use std::os::fd::{FromRawFd as _, OwnedFd};

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .with_writer(std::io::stderr)
        .init();

    let spec: LaunchSpec = serde_json::from_str(
        &std::env::var(ENV_SPEC).with_context(|| format!("{ENV_SPEC} is not set"))?,
    )
    .with_context(|| format!("{ENV_SPEC} is not a launch spec"))?;
    let fd: i32 = std::env::var(ENV_LINK_FD)
        .with_context(|| format!("{ENV_LINK_FD} is not set"))?
        .parse()
        .with_context(|| format!("{ENV_LINK_FD} is not a descriptor number"))?;
    // SAFETY: fstat and fcntl on a descriptor number; nothing is assumed
    // about it until fstat says it is an open socket.
    unsafe {
        let mut st: libc::stat = std::mem::zeroed();
        if fd < 3 || libc::fstat(fd, &mut st) != 0 || st.st_mode & libc::S_IFMT != libc::S_IFSOCK {
            anyhow::bail!("{ENV_LINK_FD}={fd} is not an open socket inherited from the core");
        }
        // The app and Xwayland are this process's children: neither may
        // inherit the link and speak to the core as the compositor.
        let flags = libc::fcntl(fd, libc::F_GETFD);
        if flags < 0 || libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC) != 0 {
            anyhow::bail!(
                "cannot keep the link from the app: {}",
                std::io::Error::last_os_error()
            );
        }
    }
    // SAFETY: an open socket this process inherited and nothing else owns.
    let core = std::os::unix::net::UnixStream::from(unsafe { OwnedFd::from_raw_fd(fd) });
    deckjack_stream::compositor::become_supervisor()?;
    let runtime = deckjack_stream::compositor::runtime_dir()?;
    // SAFETY: no other thread exists yet. The Wayland socket is created in
    // XDG_RUNTIME_DIR, which the core does not set (an app inherits nothing
    // of the operator's), so it is pointed at the private runtime directory.
    unsafe { std::env::set_var("XDG_RUNTIME_DIR", &runtime) };
    let result = deckjack_stream::compositor::run(spec, core, runtime.clone());
    // This instance's sockets and X cookie go with it.
    let _ = std::fs::remove_dir_all(&runtime);
    result
}

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("deckjack-stream runs on Linux only: it is a Wayland compositor");
    std::process::exit(1);
}
