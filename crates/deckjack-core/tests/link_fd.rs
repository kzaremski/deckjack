//! An app whose link to the core already sits at descriptor 3 in the core
//! still inherits it.
//!
//! `OsEnv::spawn` moves the app's end of the link to [`apps::LINK_FD`] with
//! `dup2`, which clears close-on-exec on the copy. When the socket pair's
//! app end is already at 3 there is no copy, and the flag has to be cleared
//! by hand; this is the only test that puts the pair there.
//!
//! It rearranges this process's descriptor table (0 and 3) to do it, which is
//! why it is alone in its own test binary: no other test thread is running
//! to have a descriptor moved from under it.

use std::io::Read as _;
use std::os::fd::AsRawFd as _;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use deckjack_core::apps::{self, Confinement, Env as _, LaunchPlan, OsEnv};
use deckjack_core::sandbox;

fn is_open(fd: i32) -> bool {
    // SAFETY: F_GETFD on a plain integer reads a flag and nothing else.
    unsafe { libc::fcntl(fd, libc::F_GETFD) >= 0 }
}

/// Move whatever is at `fd` out of the way, close-on-exec, returning where.
fn stash(fd: i32) -> Option<i32> {
    if !is_open(fd) {
        return None;
    }
    // SAFETY: duplicates and closes a descriptor this test then restores.
    unsafe {
        let saved = libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 64);
        assert!(saved >= 0, "cannot stash descriptor {fd}");
        libc::close(fd);
        Some(saved)
    }
}

fn restore(fd: i32, saved: Option<i32>) {
    if let Some(saved) = saved {
        // SAFETY: puts back what `stash` moved, and closes the stash.
        unsafe {
            assert!(libc::dup2(saved, fd) >= 0, "cannot restore descriptor {fd}");
            libc::close(saved);
        }
    }
}

#[test]
fn a_link_that_lands_on_descriptor_3_survives_into_the_app() {
    let env = OsEnv::new();
    let mut links = env.links().attend("at-three");
    let plan = LaunchPlan {
        app: "at-three".to_string(),
        program: PathBuf::from("/bin/sh"),
        args: vec!["-c".to_string(), format!("echo hello >&{}", apps::LINK_FD)],
        cwd: PathBuf::from("/"),
        core_link: true,
        pass_fds: Vec::new(),
        env: Vec::new(),
        sandbox: apps::Sandbox {
            gpu: false,
            network_hosts: Vec::new(),
            denied: Vec::new(),
            policy: sandbox::Policy::nothing(),
        },
        // Unconfined, so the test is about descriptors and not this kernel.
        confinement: Confinement::unconfined_for_development(),
    };

    // A socket pair takes the two lowest free descriptors, core end first.
    // With 0 and 3 free and 1, 2 held, that is 0 for the core and 3 for the
    // app.
    let saved3 = stash(3);
    let saved0 = stash(0);
    let spawned = env.spawn(&plan);
    restore(3, saved3.filter(|_| !is_open(3)));
    let core = links.try_recv().expect("the launch made a link");
    let core_fd = core.as_raw_fd();
    let pid = spawned.expect("the app starts");

    let mut core = core;
    core.set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    let mut heard = String::new();
    let _ = core.read_to_string(&mut heard);
    drop(core);
    restore(0, saved0);

    let deadline = Instant::now() + Duration::from_secs(10);
    while env.is_running(pid) {
        assert!(Instant::now() < deadline, "the app hung");
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(
        core_fd, 0,
        "the pair did not land where this test needs it (core end at {core_fd}), so it \
         proves nothing about descriptor 3"
    );
    assert_eq!(heard, "hello\n", "the app lost its link at exec");
}
