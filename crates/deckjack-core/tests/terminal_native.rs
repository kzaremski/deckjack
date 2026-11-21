//! What the kill switch leaves behind on a real host, asked of the kernel.
//!
//! `tests/terminal.rs` drives a fake pty, which can say whether `wait` was
//! called but not whether the kernel still holds a zombie, nor whether a job
//! the shell put in its own process group survived. Those are claims about
//! processes, so these tests start a real shell through [`NativeBackend`] and
//! read `/proc` for the answer — the same place `ps` reads it, which is where
//! the operator would look.
//!
//! Linux only: `/proc/<pid>/stat` is the observable, and the session-wide
//! kill being tested is the Linux and Android implementation.

#![cfg(any(target_os = "linux", target_os = "android"))]

use std::sync::Arc;
use std::time::{Duration, Instant};

use deckjack_core::terminal::{
    Limits, NativeBackend, OsEnv, Program, Registry, SessionSpec, TerminalSession,
};

/// The process state letter from `/proc/<pid>/stat`, or `None` once the pid
/// is gone entirely (reaped, and not yet reused).
fn state_of(pid: u32) -> Option<char> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // The command name is parenthesized and may itself contain spaces or
    // parentheses, so the fields are counted from the last `)`.
    let rest = &stat[stat.rfind(')')? + 1..];
    rest.split_whitespace().next()?.chars().next()
}

/// Whether `pid` is a live process: present and not a zombie.
fn alive(pid: u32) -> bool {
    matches!(state_of(pid), Some(s) if s != 'Z')
}

/// Poll until `done` holds or the deadline passes. A failure deadline: the
/// happy path returns on the first or second look.
fn eventually(deadline: Duration, mut done: impl FnMut() -> bool) -> bool {
    let until = Instant::now() + deadline;
    while Instant::now() < until {
        if done() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    done()
}

/// Read the terminal until a line `<tag>=<number>` appears, and return the
/// number. How the test learns a pid that exists only inside the shell.
async fn read_tagged(term: &TerminalSession, tag: &str) -> u32 {
    let mut seen = String::new();
    let mut cursor = 0;
    let found = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let out = term.output_after(cursor).await;
            cursor = out.cursor;
            seen.push_str(&String::from_utf8_lossy(&out.bytes));
            let needle = format!("{tag}=");
            if let Some(at) = seen.find(&needle) {
                let digits: String = seen[at + needle.len()..]
                    .chars()
                    .take_while(char::is_ascii_digit)
                    .collect();
                // A number cut off by a chunk boundary: wait for the rest.
                if !digits.is_empty() && seen[at + needle.len() + digits.len()..].contains('\n') {
                    return digits.parse::<u32>().expect("a pid");
                }
            }
            assert!(out.exit.is_none(), "the shell exited first: {seen}");
        }
    })
    .await;
    found.unwrap_or_else(|_| panic!("the shell never printed {tag}: {seen:?}"))
}

fn registry() -> Registry {
    Registry::new(Arc::new(OsEnv), Arc::new(NativeBackend), Limits::default())
}

fn sh(script: &str, cwd: &std::path::Path) -> SessionSpec {
    let mut spec = SessionSpec::login_shell(cwd);
    spec.program = Program::Command(vec!["/bin/sh".into(), "-c".into(), script.into()]);
    spec
}

/// The kill switch leaves no zombie, and takes the shell's background jobs
/// with it.
///
/// Two defects, one test, because they hid each other. A job the shell put in
/// its own process group (`set -m`, which every interactive shell does) is
/// not reached by signaling the shell's group, so it survived the lockdown
/// running as the host user. And because it still held the pty open, the
/// output stream never ended, so even a pump that reaped correctly would
/// never have reached `wait`: the shell stayed a zombie.
#[tokio::test]
async fn the_kill_switch_reaps_the_shell_and_kills_its_background_jobs() {
    let dir = tempfile::tempdir().unwrap();
    let reg = registry();
    let term = reg
        .open(sh(
            "set -m; sleep 600 & echo job=$!; echo ready=1; wait",
            dir.path(),
        ))
        .expect("a shell starts");
    let shell_pid = term.info().pid.expect("a native pty reports its pid");
    let job_pid = read_tagged(&term, "job").await;
    read_tagged(&term, "ready").await;
    assert!(
        alive(shell_pid),
        "the shell is not running, so nothing is under test"
    );
    assert!(
        alive(job_pid),
        "the background job is not running, so nothing is under test"
    );
    drop(term);

    assert_eq!(reg.kill_all(), 1);

    assert!(
        eventually(Duration::from_secs(10), || !alive(job_pid)),
        "the shell's background job {job_pid} survived the kill switch: state {:?}",
        state_of(job_pid)
    );
    assert!(
        eventually(Duration::from_secs(10), || state_of(shell_pid).is_none()),
        "the killed shell {shell_pid} was never reaped: state {:?} (Z is a zombie)",
        state_of(shell_pid)
    );
}

/// An ordinary close reaps the shell too, after the registry has let go of it.
#[tokio::test]
async fn a_closed_shell_leaves_no_zombie() {
    let dir = tempfile::tempdir().unwrap();
    let reg = registry();
    let term = reg
        .open(sh("echo ready=1; exec sleep 600", dir.path()))
        .expect("a shell starts");
    let pid = term.info().pid.expect("a native pty reports its pid");
    read_tagged(&term, "ready").await;
    let id = term.id().clone();
    drop(term);

    reg.close(&id).expect("the terminal closes");

    assert!(
        eventually(Duration::from_secs(10), || state_of(pid).is_none()),
        "the closed shell {pid} was never reaped: state {:?} (Z is a zombie)",
        state_of(pid)
    );
}

/// The kill switch reaches jobs a shell left behind when it exited.
///
/// `nohup job >/dev/null &` and then `exit`: the pty closes, the pump reaps
/// the shell, and the session reads as exited — but the job is still in the
/// shell's session, running as the host user. Lockdown skipped exited
/// sessions, and a reaped shell's hangup was a no-op, so the job outlived
/// the kill switch. Safe to reach: the kernel does not hand out a pid while
/// any process still carries it as a session id.
#[tokio::test]
async fn the_kill_switch_reaches_a_job_the_exited_shell_left_behind() {
    let dir = tempfile::tempdir().unwrap();
    let reg = registry();
    let term = reg
        .open(sh(
            "set -m; nohup sleep 600 >/dev/null 2>&1 </dev/null & echo job=$!",
            dir.path(),
        ))
        .expect("a shell starts");
    let shell_pid = term.info().pid.expect("a native pty reports its pid");
    let job_pid = read_tagged(&term, "job").await;
    tokio::time::timeout(Duration::from_secs(10), term.wait_for_exit())
        .await
        .expect("the shell exits once it has started the job");
    assert!(
        eventually(Duration::from_secs(10), || state_of(shell_pid).is_none()),
        "the shell was not reaped, so the reaped case is not under test"
    );
    assert!(
        alive(job_pid),
        "the left-behind job is not running, so nothing is under test"
    );
    drop(term);

    assert_eq!(reg.kill_all(), 0, "no shell was running to be killed");

    let survived = !eventually(Duration::from_secs(10), || !alive(job_pid));
    if survived {
        let _ = nix::sys::signal::kill(
            nix::unistd::Pid::from_raw(job_pid as i32),
            nix::sys::signal::Signal::SIGKILL,
        );
    }
    assert!(
        !survived,
        "a job the exited shell left behind survived the kill switch"
    );
}

/// The kill switch reaches a closed terminal's jobs that ignored the hangup.
///
/// `close` sends `SIGHUP` and forgets the session. A job that ignores it and
/// keeps the pty open lives on, with the pump still draining its output —
/// and a later lockdown, walking only the terminals still listed, never
/// found it.
#[tokio::test]
async fn the_kill_switch_reaches_jobs_of_a_terminal_already_closed() {
    let dir = tempfile::tempdir().unwrap();
    let reg = registry();
    let term = reg
        .open(sh(
            "set -m; (trap '' HUP; exec sleep 600) & echo job=$!; echo ready=1; wait",
            dir.path(),
        ))
        .expect("a shell starts");
    let job_pid = read_tagged(&term, "job").await;
    read_tagged(&term, "ready").await;
    let id = term.id().clone();
    drop(term);

    reg.close(&id).expect("the terminal closes");
    // Given the hangup time to land: the job is meant to survive it.
    std::thread::sleep(Duration::from_millis(200));
    assert!(
        alive(job_pid),
        "the job did not survive the close, so nothing is under test"
    );

    reg.kill_all();

    let survived = !eventually(Duration::from_secs(10), || !alive(job_pid));
    if survived {
        let _ = nix::sys::signal::kill(
            nix::unistd::Pid::from_raw(job_pid as i32),
            nix::sys::signal::Signal::SIGKILL,
        );
    }
    assert!(
        !survived,
        "a closed terminal's job that ignored the hangup survived the kill switch"
    );
}
