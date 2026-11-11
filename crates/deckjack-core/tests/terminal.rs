//! What a terminal session promises, exercised through the public API only.
//!
//! The claims under test are the ones a reattaching browser depends on: a
//! cursor hands back exactly what followed it, a bounded scrollback says when
//! it dropped something, a shell that has exited still serves its last line
//! and then reports how it died, and closing one session's terminals touches
//! nothing else.
//!
//! Every test drives a fake [`PtyBackend`] and a fake clock rather than a
//! real shell. That is not only for speed: a real `/bin/sh` cannot be made to
//! emit a chosen byte at a chosen moment, exit with a chosen status, or wedge
//! on input, and those are precisely the paths that break. Nothing here
//! sleeps — the fake's output stream is a channel, so ordering comes from the
//! channel and from `wait_for_exit`, never from a timer.

use std::collections::{HashMap, VecDeque};
use std::ffi::OsString;
use std::io::{self, Read, Write};
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use deckjack_core::auth::session::{SessionHash, SessionId};
use deckjack_core::terminal::{
    Env, Hangup, Limits, Opener, Output, Program, PtyBackend, PtyProcess, Registry, SessionSpec,
    TerminalError, TerminalId, TerminalSession, WinSize,
};

// ---- the injected world -------------------------------------------------

/// A clock the test moves by hand, so an exit can be placed in the past and
/// `prune_exited` can be asked about it without waiting for real seconds.
struct TestClock(Mutex<u64>);

impl TestClock {
    fn at(t: u64) -> Arc<TestClock> {
        Arc::new(TestClock(Mutex::new(t)))
    }

    fn set(&self, t: u64) {
        *self.0.lock().unwrap() = t;
    }
}

impl Env for TestClock {
    fn now(&self) -> u64 {
        *self.0.lock().unwrap()
    }
}

/// How one fake shell should behave. Defaults to a well-behaved one: it
/// echoes nothing, accepts all input, and exits 0 when the test closes its
/// output stream.
#[derive(Clone, Copy)]
struct Plan {
    /// The backend refuses to open a pty at all.
    fail_open: bool,
    /// The pty opens but its output stream cannot be taken.
    fail_reader: bool,
    /// Writes to the pty block forever, standing in for a program that has
    /// stopped reading its input.
    wedged_input: bool,
    /// `wait` blocks until the test releases it, so the moment between the
    /// last byte of output and the recorded exit can be inspected.
    gate_wait: bool,
    /// `wait` cannot reap the child, as if something else already had.
    unreapable: bool,
    /// The kernel refuses the window-size change.
    fail_resize: bool,
    exit_code: u32,
    pid: Option<u32>,
}

impl Default for Plan {
    fn default() -> Plan {
        Plan {
            fail_open: false,
            fail_reader: false,
            wedged_input: false,
            gate_wait: false,
            unreapable: false,
            fail_resize: false,
            exit_code: 0,
            pid: Some(4242),
        }
    }
}

/// The test's end of one fake shell.
#[derive(Clone)]
struct Shell {
    /// Dropping this is end-of-file on the pty, which is how a shell exits.
    out: Arc<Mutex<Option<Sender<Vec<u8>>>>>,
    /// Releases a gated `wait`.
    gate: Arc<Mutex<Option<Sender<()>>>>,
    /// Chunks the input thread actually wrote to the pty, in order.
    typed: Arc<Mutex<Receiver<Vec<u8>>>>,
    signals: Arc<Mutex<Vec<Hangup>>>,
    resizes: Arc<Mutex<Vec<WinSize>>>,
    /// The size the backend was handed when the pty was opened.
    opened_with: WinSize,
}

impl Shell {
    /// Emit one chunk of output, as a pty read would deliver it.
    fn says(&self, bytes: &[u8]) {
        self.out
            .lock()
            .unwrap()
            .as_ref()
            .expect("the shell's output stream is still open")
            .send(bytes.to_vec())
            .expect("the output pump should still be reading");
    }

    /// End the output stream: the shell has exited.
    fn ends(&self) {
        self.out.lock().unwrap().take();
    }

    fn release_wait(&self) {
        if let Some(gate) = self.gate.lock().unwrap().take() {
            let _ = gate.send(());
        }
    }

    /// Block until the input thread has written the next chunk. A channel,
    /// not a sleep: if the chunk never arrives the test hangs rather than
    /// passing by luck.
    fn next_typed(&self) -> Vec<u8> {
        self.typed
            .lock()
            .unwrap()
            .recv()
            .expect("the input thread should have written a chunk")
    }

    fn signals(&self) -> Vec<Hangup> {
        self.signals.lock().unwrap().clone()
    }

    fn resizes(&self) -> Vec<WinSize> {
        self.resizes.lock().unwrap().clone()
    }
}

/// A pty output stream fed by the test over a channel. All senders dropped
/// is end-of-file, which is what the pump sees when the last slave closes.
struct ChannelReader {
    rx: Receiver<Vec<u8>>,
    left: Vec<u8>,
}

impl Read for ChannelReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        while self.left.is_empty() {
            match self.rx.recv() {
                Ok(chunk) => self.left = chunk,
                Err(_) => return Ok(0),
            }
        }
        let n = self.left.len().min(buf.len());
        buf[..n].copy_from_slice(&self.left[..n]);
        self.left.drain(..n);
        Ok(n)
    }
}

/// Records what was typed, and hands each chunk to the test.
struct RecordingWriter(Sender<Vec<u8>>);

impl Write for RecordingWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let _ = self.0.send(buf.to_vec());
        Ok(buf.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// A pty whose child has stopped reading: every write blocks forever. The
/// sender is kept alive by the [`Shell`], so this parks rather than erroring.
struct WedgedWriter(Receiver<()>);

impl Write for WedgedWriter {
    fn write(&mut self, _buf: &[u8]) -> io::Result<usize> {
        let _ = self.0.recv();
        Ok(0)
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

struct FakePty {
    plan: Plan,
    /// Keeps the wedged writer's channel alive for as long as the pty, so a
    /// blocked write parks instead of erroring out.
    _hold: Sender<()>,
    reader: Mutex<Option<Box<dyn Read + Send>>>,
    writer: Mutex<Option<Box<dyn Write + Send>>>,
    gate: Mutex<Option<Receiver<()>>>,
    signals: Arc<Mutex<Vec<Hangup>>>,
    resizes: Arc<Mutex<Vec<WinSize>>>,
}

impl PtyProcess for FakePty {
    fn resize(&self, size: WinSize) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        if self.plan.fail_resize {
            return Err("the kernel refused TIOCSWINSZ".into());
        }
        self.resizes.lock().unwrap().push(size);
        Ok(())
    }

    fn reader(&self) -> Result<Box<dyn Read + Send>, Box<dyn std::error::Error + Send + Sync>> {
        if self.plan.fail_reader {
            return Err("the output stream is already taken".into());
        }
        Ok(self
            .reader
            .lock()
            .unwrap()
            .take()
            .expect("the output stream is taken once"))
    }

    fn writer(&self) -> Result<Box<dyn Write + Send>, Box<dyn std::error::Error + Send + Sync>> {
        Ok(self
            .writer
            .lock()
            .unwrap()
            .take()
            .expect("the input stream is taken once"))
    }

    fn pid(&self) -> Option<u32> {
        self.plan.pid
    }

    fn hangup(&self, how: Hangup) -> Result<(), nix::Error> {
        self.signals.lock().unwrap().push(how);
        Ok(())
    }

    fn wait(&self) -> Option<(u32, Option<String>)> {
        if let Some(gate) = self.gate.lock().unwrap().take() {
            let _ = gate.recv();
        }
        if self.plan.unreapable {
            return None;
        }
        Some((self.plan.exit_code, None))
    }
}

/// Opens fake ptys, keeping the test's end of each one.
struct FakeBackend {
    plans: Mutex<VecDeque<Plan>>,
    shells: Mutex<HashMap<TerminalId, Shell>>,
}

impl FakeBackend {
    fn new() -> Arc<FakeBackend> {
        Arc::new(FakeBackend {
            plans: Mutex::new(VecDeque::new()),
            shells: Mutex::new(HashMap::new()),
        })
    }

    /// Queue the plan for the next `open`. Unqueued opens get the default.
    fn next(&self, plan: Plan) {
        self.plans.lock().unwrap().push_back(plan);
    }

    fn shell(&self, id: &TerminalId) -> Shell {
        self.shells
            .lock()
            .unwrap()
            .get(id)
            .cloned()
            .expect("the backend opened a pty for this terminal")
    }
}

impl PtyBackend for FakeBackend {
    fn open(
        &self,
        spec: &SessionSpec,
        id: &TerminalId,
    ) -> Result<Box<dyn PtyProcess>, TerminalError> {
        let plan = self.plans.lock().unwrap().pop_front().unwrap_or_default();
        if plan.fail_open {
            return Err(TerminalError::OpenPty {
                source: "no pty devices left".into(),
            });
        }

        let (out_tx, out_rx) = channel::<Vec<u8>>();
        let (typed_tx, typed_rx) = channel::<Vec<u8>>();
        let (gate_tx, gate_rx) = channel::<()>();
        let (hold_tx, hold_rx) = channel::<()>();

        let writer: Box<dyn Write + Send> = if plan.wedged_input {
            Box::new(WedgedWriter(hold_rx))
        } else {
            Box::new(RecordingWriter(typed_tx))
        };

        let signals = Arc::new(Mutex::new(Vec::new()));
        let resizes = Arc::new(Mutex::new(Vec::new()));

        self.shells.lock().unwrap().insert(
            id.clone(),
            Shell {
                out: Arc::new(Mutex::new(Some(out_tx))),
                gate: Arc::new(Mutex::new(Some(gate_tx))),
                typed: Arc::new(Mutex::new(typed_rx)),
                signals: Arc::clone(&signals),
                resizes: Arc::clone(&resizes),
                opened_with: spec.size,
            },
        );
        Ok(Box::new(FakePty {
            plan,
            _hold: hold_tx,
            reader: Mutex::new(Some(Box::new(ChannelReader {
                rx: out_rx,
                left: Vec::new(),
            }))),
            writer: Mutex::new(Some(writer)),
            gate: Mutex::new(plan.gate_wait.then_some(gate_rx)),
            signals,
            resizes,
        }))
    }
}

// ---- helpers ------------------------------------------------------------

struct Rig {
    registry: Registry,
    backend: Arc<FakeBackend>,
    clock: Arc<TestClock>,
}

fn rig(limits: Limits) -> Rig {
    let clock = TestClock::at(1_000);
    let backend = FakeBackend::new();
    Rig {
        registry: Registry::new(
            Arc::clone(&clock) as Arc<dyn Env>,
            Arc::clone(&backend) as Arc<dyn PtyBackend>,
            limits,
        ),
        backend,
        clock,
    }
}

/// A rig whose scrollback is small enough that a test can overflow it by
/// hand. The byte count is the only thing that differs from production.
fn small_rig(scrollback_bytes: usize) -> Rig {
    rig(Limits {
        max_sessions: 8,
        scrollback_bytes,
    })
}

fn shell_spec() -> SessionSpec {
    SessionSpec::login_shell("/srv/deckjack")
}

fn client(session: &SessionHash) -> SessionSpec {
    shell_spec().opened_by(Opener::Client(*session))
}

/// The registry records who opened a terminal by the hash of their session
/// id, never the id itself: the registry outlives any one request and is
/// dumped into logs and lockdown reports, and a session id is a credential.
fn an_id() -> SessionHash {
    SessionId::generate().unwrap().hash()
}

/// Follow a terminal from `cursor` until it reports an exit, exactly as the
/// WebSocket handler does, and return everything it delivered plus the total
/// it admitted to skipping.
async fn follow_to_exit(term: &TerminalSession, mut cursor: u64) -> (Vec<u8>, u64) {
    let mut seen = Vec::new();
    let mut skipped = 0;
    loop {
        let out: Output = term.output_after(cursor).await;
        assert!(
            out.cursor >= cursor,
            "a cursor must never move backwards: {cursor} -> {}",
            out.cursor
        );
        cursor = out.cursor;
        skipped += out.skipped;
        seen.extend_from_slice(&out.bytes);
        if out.exit.is_some() {
            return (seen, skipped);
        }
    }
}

/// A deadlock guard, not a synchronization device: every await here resolves
/// on a notification, so the happy path never spends this time. It exists so
/// a regression that loses a wakeup fails the suite instead of hanging it.
async fn within<T>(f: impl std::future::Future<Output = T>) -> T {
    tokio::time::timeout(Duration::from_secs(10), f)
        .await
        .expect("a terminal wakeup was lost: this should have resolved immediately")
}

// ---- the scrollback cursor ----------------------------------------------

#[tokio::test]
async fn following_from_a_cursor_delivers_exactly_what_followed_it() {
    let r = rig(Limits::default());
    let term = r.registry.open(shell_spec()).unwrap();
    let shell = r.backend.shell(term.id());

    for chunk in [&b"one"[..], b"two", b"three"] {
        shell.says(chunk);
    }
    shell.ends();

    let (seen, skipped) = within(follow_to_exit(&term, 0)).await;
    // The whole stream, in order, once each: no gap and no repeat is the
    // entire contract a reattaching browser rests on.
    assert_eq!(seen, b"onetwothree");
    assert_eq!(skipped, 0);
}

#[tokio::test]
async fn a_reattaching_client_sees_the_scrollback_then_only_what_follows() {
    let r = rig(Limits::default());
    let term = r.registry.open(shell_spec()).unwrap();
    let shell = r.backend.shell(term.id());

    shell.says(b"before reload\n");
    // Wait for the pump to have it, the way a browser does.
    let first = within(term.output_after(0)).await;
    assert_eq!(first.bytes, b"before reload\n");

    // A reload: the client throws its state away and takes a snapshot.
    let snap = term.snapshot();
    assert_eq!(snap.bytes, b"before reload\n");
    assert_eq!(snap.skipped, 0);

    shell.says(b"after reload\n");
    shell.ends();
    let (seen, skipped) = within(follow_to_exit(&term, snap.cursor)).await;
    assert_eq!(
        seen, b"after reload\n",
        "the reattached client replayed output"
    );
    assert_eq!(skipped, 0);
}

#[tokio::test]
async fn a_full_scrollback_drops_the_oldest_bytes_and_says_how_many() {
    let r = small_rig(8);
    let term = r.registry.open(shell_spec()).unwrap();
    let shell = r.backend.shell(term.id());

    shell.says(b"abcd");
    shell.says(b"efgh");
    shell.says(b"ij");
    shell.ends();
    within(term.wait_for_exit()).await;

    let out = term.read_from(0);
    // 10 bytes were produced and 8 are kept, so the two oldest are gone and
    // the client is told so rather than handed a silently truncated stream.
    assert_eq!(out.bytes, b"cdefghij");
    assert_eq!(out.skipped, 2);
    assert_eq!(out.cursor, 10, "the cursor counts every byte ever produced");
}

#[tokio::test]
async fn a_write_larger_than_the_whole_scrollback_keeps_its_tail() {
    let r = small_rig(4);
    let term = r.registry.open(shell_spec()).unwrap();
    let shell = r.backend.shell(term.id());

    shell.says(b"0123456789");
    shell.ends();
    within(term.wait_for_exit()).await;

    let out = term.read_from(0);
    assert_eq!(
        out.bytes, b"6789",
        "the newest bytes are the ones worth keeping"
    );
    assert_eq!(out.skipped, 6);
    assert_eq!(out.cursor, 10);
}

#[tokio::test]
async fn a_client_behind_the_scrollback_is_told_before_it_is_handed_bytes() {
    let r = small_rig(4);
    let term = r.registry.open(shell_spec()).unwrap();
    let shell = r.backend.shell(term.id());

    shell.says(b"aaaa");
    let first = within(term.output_after(0)).await;
    assert_eq!(first.cursor, 4);
    shell.says(b"bbbb");
    shell.says(b"cccc");
    shell.ends();
    within(term.wait_for_exit()).await;

    // The client was at 4 and 8 bytes have fallen off the front since, so it
    // has a hole: it must reset its emulator, because the first byte it gets
    // may land mid escape sequence.
    let out = term.read_from(4);
    assert_eq!(out.skipped, 4);
    assert_eq!(out.bytes, b"cccc");
}

#[tokio::test]
async fn a_cursor_past_the_end_of_the_stream_is_treated_as_caught_up() {
    let r = rig(Limits::default());
    let term = r.registry.open(shell_spec()).unwrap();
    let shell = r.backend.shell(term.id());
    shell.says(b"hi");
    within(term.output_after(0)).await;

    // A client whose stored cursor outlived a server restart. Refusing it
    // would strand the window; panicking would take the server down.
    let out = term.read_from(9_999);
    assert!(out.bytes.is_empty());
    assert_eq!(out.skipped, 0);
    assert_eq!(out.cursor, 2);
}

#[tokio::test]
async fn a_follower_waiting_on_an_idle_terminal_is_woken_by_output() {
    let r = rig(Limits::default());
    let term = r.registry.open(shell_spec()).unwrap();
    let shell = r.backend.shell(term.id());

    // Park a follower on a terminal that has produced nothing at all, then
    // produce something. If the wait and the read were ordered the other way
    // round in the module, this is the call that would hang forever.
    let following = tokio::spawn({
        let term = Arc::clone(&term);
        async move { term.output_after(0).await }
    });
    shell.says(b"woke up");

    let out = within(following).await.unwrap();
    assert_eq!(out.bytes, b"woke up");
    assert!(out.exit.is_none());
}

// ---- exit ---------------------------------------------------------------

#[tokio::test]
async fn an_exited_shell_serves_its_last_line_and_then_reports_the_exit() {
    let r = rig(Limits::default());
    r.backend.next(Plan {
        exit_code: 3,
        ..Plan::default()
    });
    let term = r.registry.open(shell_spec()).unwrap();
    let shell = r.backend.shell(term.id());

    shell.says(b"goodbye\n");
    shell.ends();

    let (seen, _) = within(follow_to_exit(&term, 0)).await;
    assert_eq!(seen, b"goodbye\n", "the last line survives the exit");
    let exit = term.exit().expect("the shell has been reaped");
    assert_eq!(exit.code, 3);
    assert_eq!(exit.at, 1_000, "the exit is timed by the injected clock");
    // And the record stays readable for a browser that reconnects after it.
    let after = term.read_from(0);
    assert_eq!(after.bytes, b"goodbye\n");
    assert_eq!(after.exit.map(|e| e.code), Some(3));
}

#[tokio::test]
async fn the_last_output_is_readable_before_the_exit_is_recorded() {
    // The pump pushes every byte it read and only then reaps, so output is
    // never held back waiting on a status. A reader that samples the status
    // after the bytes could otherwise report an exit with the final chunk
    // still missing.
    let r = rig(Limits::default());
    r.backend.next(Plan {
        gate_wait: true,
        ..Plan::default()
    });
    let term = r.registry.open(shell_spec()).unwrap();
    let shell = r.backend.shell(term.id());

    shell.says(b"final\n");
    shell.ends();

    let out = within(term.output_after(0)).await;
    assert_eq!(out.bytes, b"final\n");
    assert!(
        out.exit.is_none(),
        "the shell has not been reaped yet, so there is no status to report"
    );

    shell.release_wait();
    let exit = within(term.wait_for_exit()).await;
    assert_eq!(exit.code, 0);
}

#[tokio::test]
async fn a_shell_that_cannot_be_reaped_is_still_reported_as_gone() {
    // Something else reaped the child. Its output stream has ended either
    // way, and a session left looking alive forever is the worse answer.
    let r = rig(Limits::default());
    r.backend.next(Plan {
        unreapable: true,
        ..Plan::default()
    });
    let term = r.registry.open(shell_spec()).unwrap();
    r.backend.shell(term.id()).ends();

    let exit = within(term.wait_for_exit()).await;
    assert_ne!(exit.code, 0, "an unknown status is reported as a failure");
    assert!(term.exit().is_some());
}

#[tokio::test]
async fn typing_into_an_exited_terminal_is_refused_not_swallowed() {
    let r = rig(Limits::default());
    let term = r.registry.open(shell_spec()).unwrap();
    r.backend.shell(term.id()).ends();
    within(term.wait_for_exit()).await;

    let err = term.write(b"ls\r").unwrap_err();
    // Accepting it would leave the operator typing into a dead shell with no
    // sign that nothing is listening.
    assert!(
        matches!(err, TerminalError::Exited { .. }),
        "expected Exited, got {err}"
    );
}

// ---- input --------------------------------------------------------------

#[test]
fn keystrokes_reach_the_shell_in_the_order_they_were_sent() {
    let r = rig(Limits::default());
    let term = r.registry.open(shell_spec()).unwrap();
    let shell = r.backend.shell(term.id());

    term.write(b"echo hi").unwrap();
    term.write(b"\r").unwrap();

    assert_eq!(shell.next_typed(), b"echo hi");
    assert_eq!(shell.next_typed(), b"\r");
}

#[test]
fn input_to_a_wedged_shell_fails_fast_instead_of_blocking_the_caller() {
    let r = rig(Limits::default());
    r.backend.next(Plan {
        wedged_input: true,
        ..Plan::default()
    });
    let term = r.registry.open(shell_spec()).unwrap();

    // A program that has stopped reading must not let the browser grow the
    // server's memory, and must not stall the handler that took the
    // keystroke. The queue is bounded, so this loop ends.
    let mut accepted = 0;
    let err = loop {
        match term.write(b"x") {
            Ok(()) => {
                accepted += 1;
                assert!(
                    accepted < 10_000,
                    "the input queue is unbounded: {accepted} chunks accepted by a shell \
                     that has never read one"
                );
            }
            Err(e) => break e,
        }
    };
    assert!(
        matches!(err, TerminalError::InputBacklog { .. }),
        "expected InputBacklog, got {err}"
    );
    assert!(accepted > 0, "a wedged shell still accepts a little typing");
}

#[test]
fn an_empty_write_is_not_an_error() {
    // xterm.js sends a zero-length payload on some paste and focus paths;
    // refusing it would surface as a spurious error toast.
    let r = rig(Limits::default());
    let term = r.registry.open(shell_spec()).unwrap();
    assert!(term.write(b"").is_ok());
}

// ---- resize -------------------------------------------------------------

#[test]
fn resize_clamps_absurd_sizes_rather_than_passing_them_on() {
    let r = rig(Limits::default());
    let term = r.registry.open(shell_spec()).unwrap();
    let shell = r.backend.shell(term.id());

    // A browser mid-layout reports zero rows; a hostile client reports the
    // largest u16 it can. Programs allocate rows * cols cells.
    term.resize(WinSize::new(0, 0)).unwrap();
    term.resize(WinSize::new(u16::MAX, u16::MAX)).unwrap();

    let seen = shell.resizes();
    assert_eq!(seen.len(), 2);
    assert_eq!((seen[0].rows, seen[0].cols), (1, 1));
    assert!(
        seen[1].rows <= 1_000 && seen[1].cols <= 2_000,
        "an unbounded window size reached the pty: {:?}",
        seen[1]
    );
    assert_eq!(
        term.size(),
        seen[1],
        "the size the session reports is the size the pty was given"
    );
}

#[test]
fn the_size_a_session_opens_with_is_already_clamped() {
    let r = rig(Limits::default());
    let term = r
        .registry
        .open(shell_spec().with_size(WinSize::new(0, u16::MAX)))
        .unwrap();
    let shell = r.backend.shell(term.id());

    assert_eq!(term.size().rows, 1);
    assert!(term.size().cols <= 2_000);
    assert_eq!(
        shell.opened_with,
        term.size(),
        "the backend was handed a size the session does not admit to"
    );
    assert_eq!(term.info().size, term.size());
}

#[test]
fn a_resize_to_the_size_already_held_is_not_sent_to_the_shell_again() {
    // Called on every drag frame. Raising SIGWINCH for a size the child
    // already has makes full-screen programs redraw for nothing.
    let r = rig(Limits::default());
    let term = r
        .registry
        .open(shell_spec().with_size(WinSize::new(40, 100)))
        .unwrap();
    let shell = r.backend.shell(term.id());

    term.resize(WinSize::new(40, 100)).unwrap();
    term.resize(WinSize::new(41, 100)).unwrap();
    term.resize(WinSize::new(41, 100)).unwrap();

    assert_eq!(shell.resizes().len(), 1);
    assert_eq!(term.size(), WinSize::new(41, 100));
}

#[test]
fn a_refused_resize_leaves_the_recorded_size_alone() {
    // The session's size is what the UI and the next reattach believe. If
    // the kernel refused the change, recording it anyway would have the
    // server and the pty disagree with nothing to resync them.
    let r = rig(Limits::default());
    r.backend.next(Plan {
        fail_resize: true,
        ..Plan::default()
    });
    let term = r
        .registry
        .open(shell_spec().with_size(WinSize::new(24, 80)))
        .unwrap();

    let err = term.resize(WinSize::new(50, 120)).unwrap_err();
    assert!(matches!(err, TerminalError::Pty { .. }), "got {err}");
    assert_eq!(term.size(), WinSize::new(24, 80));
}

// ---- limits -------------------------------------------------------------

#[tokio::test]
async fn max_sessions_is_enforced_and_an_exited_session_does_not_count() {
    let r = rig(Limits {
        max_sessions: 2,
        scrollback_bytes: 1024,
    });
    let first = r.registry.open(shell_spec()).unwrap();
    let _second = r.registry.open(shell_spec()).unwrap();

    let err = r.registry.open(shell_spec()).unwrap_err();
    assert!(
        matches!(err, TerminalError::TooMany { live, max } if live == 2 && max == 2),
        "got {err}"
    );

    // An exited shell holds no pty and no threads, so it must not hold a
    // slot either, or a few crashed shells wedge the terminal app.
    r.backend.shell(first.id()).ends();
    within(first.wait_for_exit()).await;
    r.registry
        .open(shell_spec())
        .expect("an exited session should not occupy a slot");
}

#[test]
fn a_backend_that_cannot_open_a_pty_does_not_consume_a_slot() {
    let r = rig(Limits {
        max_sessions: 1,
        scrollback_bytes: 1024,
    });
    r.backend.next(Plan {
        fail_open: true,
        ..Plan::default()
    });

    let err = r.registry.open(shell_spec()).unwrap_err();
    assert!(matches!(err, TerminalError::OpenPty { .. }), "got {err}");
    assert!(
        r.registry.list().is_empty(),
        "a failed open left a session behind"
    );
    r.registry
        .open(shell_spec())
        .expect("the slot a failed open never used is still free");
}

#[test]
fn a_pty_whose_output_stream_cannot_be_taken_is_not_registered() {
    let r = rig(Limits {
        max_sessions: 1,
        scrollback_bytes: 1024,
    });
    r.backend.next(Plan {
        fail_reader: true,
        ..Plan::default()
    });

    let err = r.registry.open(shell_spec()).unwrap_err();
    assert!(matches!(err, TerminalError::Pty { .. }), "got {err}");
    // A terminal with no output pump would look alive and never say a word.
    assert!(r.registry.list().is_empty());
    r.registry
        .open(shell_spec())
        .expect("the slot is still free");
}

// ---- close, scoping and lockdown ---------------------------------------

#[test]
fn close_for_session_closes_that_sessions_terminals_and_nothing_else() {
    let r = rig(Limits::default());
    let signing_out = an_id();
    let staying = an_id();

    let doomed = r.registry.open(client(&signing_out)).unwrap();
    let other_client = r.registry.open(client(&staying)).unwrap();
    let at_the_keyboard = r.registry.open(shell_spec()).unwrap();

    let doomed_shell = r.backend.shell(doomed.id());
    let other_shell = r.backend.shell(other_client.id());
    let host_shell = r.backend.shell(at_the_keyboard.id());

    assert_eq!(r.registry.close_for_session(&signing_out), 1);

    // The one that signed out is gone and was hung up.
    assert_eq!(doomed_shell.signals(), vec![Hangup::Polite]);
    assert!(matches!(
        r.registry.attach(doomed.id()).unwrap_err(),
        TerminalError::NoSuchTerminal { .. }
    ));

    // Nobody else was signaled: sign-out must not kill another browser's
    // shell or the one the operator is sitting at.
    assert!(other_shell.signals().is_empty());
    assert!(host_shell.signals().is_empty());
    assert!(r.registry.attach(other_client.id()).is_ok());
    assert!(r.registry.attach(at_the_keyboard.id()).is_ok());
    assert_eq!(r.registry.list().len(), 2);
}

#[test]
fn signing_out_a_session_with_no_terminals_closes_nothing() {
    let r = rig(Limits::default());
    let mine = r.registry.open(client(&an_id())).unwrap();

    assert_eq!(r.registry.close_for_session(&an_id()), 0);
    assert!(r.backend.shell(mine.id()).signals().is_empty());
    assert!(r.registry.attach(mine.id()).is_ok());
}

#[tokio::test]
async fn lockdown_kills_every_live_shell_without_a_signal_it_can_trap() {
    let r = rig(Limits::default());
    let one = r.registry.open(shell_spec()).unwrap();
    let two = r.registry.open(client(&an_id())).unwrap();
    let already_gone = r.registry.open(shell_spec()).unwrap();

    r.backend.shell(already_gone.id()).ends();
    within(already_gone.wait_for_exit()).await;

    assert_eq!(r.registry.kill_all(), 2, "only live shells are killed");
    assert_eq!(r.backend.shell(one.id()).signals(), vec![Hangup::Now]);
    assert_eq!(r.backend.shell(two.id()).signals(), vec![Hangup::Now]);
    // A kill switch a shell can trap is not a kill switch.
    assert!(
        !r.backend
            .shell(one.id())
            .signals()
            .contains(&Hangup::Polite)
    );
    assert!(
        r.registry.list().is_empty(),
        "lockdown forgets every terminal, exited ones included"
    );
}

#[test]
fn closing_a_terminal_hangs_it_up_politely_and_forgets_it() {
    let r = rig(Limits::default());
    let term = r.registry.open(shell_spec()).unwrap();
    let shell = r.backend.shell(term.id());

    r.registry.close(term.id()).unwrap();
    assert_eq!(shell.signals(), vec![Hangup::Polite]);
    assert!(matches!(
        r.registry.close(term.id()).unwrap_err(),
        TerminalError::NoSuchTerminal { .. }
    ));
}

#[test]
fn attaching_to_a_terminal_that_is_not_here_is_refused() {
    let r = rig(Limits::default());
    let gone = TerminalId::parse("nothing-by-this-name").unwrap();
    assert!(matches!(
        r.registry.attach(&gone).unwrap_err(),
        TerminalError::NoSuchTerminal { .. }
    ));
}

#[tokio::test]
async fn an_exited_terminal_can_still_be_attached_to() {
    // The browser reconnects after the shell died and must learn how it
    // died, not that the terminal never existed.
    let r = rig(Limits::default());
    r.backend.next(Plan {
        exit_code: 130,
        ..Plan::default()
    });
    let term = r.registry.open(shell_spec()).unwrap();
    let shell = r.backend.shell(term.id());
    shell.says(b"^C\n");
    shell.ends();
    within(term.wait_for_exit()).await;

    let again = r.registry.attach(term.id()).unwrap();
    let out = again.read_from(0);
    assert_eq!(out.bytes, b"^C\n");
    assert_eq!(out.exit.map(|e| e.code), Some(130));
}

// ---- bookkeeping --------------------------------------------------------

#[tokio::test]
async fn prune_forgets_only_the_sessions_that_exited_before_the_cutoff() {
    let r = rig(Limits::default());
    let old = r.registry.open(shell_spec()).unwrap();
    let recent = r.registry.open(shell_spec()).unwrap();
    let running = r.registry.open(shell_spec()).unwrap();

    r.clock.set(1_000);
    r.backend.shell(old.id()).ends();
    within(old.wait_for_exit()).await;

    r.clock.set(2_000);
    r.backend.shell(recent.id()).ends();
    within(recent.wait_for_exit()).await;

    assert_eq!(r.registry.prune_exited(1_500), 1);
    assert!(matches!(
        r.registry.attach(old.id()).unwrap_err(),
        TerminalError::NoSuchTerminal { .. }
    ));
    // A shell that is still running is never pruned, whatever the cutoff.
    assert!(r.registry.attach(recent.id()).is_ok());
    assert!(r.registry.attach(running.id()).is_ok());
    assert_eq!(r.registry.prune_exited(u64::MAX), 1);
    assert!(r.registry.attach(running.id()).is_ok());
}

#[test]
fn list_is_oldest_first_so_a_tab_strip_can_be_rebuilt() {
    let r = rig(Limits::default());
    r.clock.set(10);
    let first = r.registry.open(shell_spec()).unwrap();
    r.clock.set(20);
    let second = r.registry.open(shell_spec()).unwrap();
    r.clock.set(30);
    let third = r.registry.open(shell_spec()).unwrap();

    let listed = r.registry.list();
    let ids: Vec<&str> = listed.iter().map(|i| i.id.as_str()).collect();
    assert_eq!(
        ids,
        vec![
            first.id().as_str(),
            second.id().as_str(),
            third.id().as_str()
        ]
    );
    let listed = &listed[0];
    assert_eq!(listed.opened_at, 10);
    assert_eq!(listed.pid, Some(4242));
    assert_eq!(listed.exit, None);
}

#[test]
fn a_command_terminal_is_labeled_by_the_program_not_its_path() {
    let r = rig(Limits::default());
    let term = r
        .registry
        .open(SessionSpec {
            program: Program::Command(vec![
                OsString::from("/usr/bin/git"),
                OsString::from("status"),
            ]),
            cwd: PathBuf::from("/srv/deckjack"),
            size: WinSize::default(),
            env: Vec::new(),
            env_remove: Vec::new(),
            opened_by: Opener::Host,
        })
        .unwrap();

    assert_eq!(term.info().label, "git");
    assert_eq!(term.opened_by(), &Opener::Host);
}

#[test]
fn two_terminals_never_share_an_id() {
    let r = rig(Limits::default());
    let a = r.registry.open(shell_spec()).unwrap();
    let b = r.registry.open(shell_spec()).unwrap();
    assert_ne!(a.id(), b.id());
    // Handing one client another's terminal is the failure this guards.
    let attached = r.registry.attach(a.id()).unwrap();
    assert!(Arc::ptr_eq(&attached, &a));
}

// ---- ids off the wire ---------------------------------------------------

#[test]
fn an_id_from_the_wire_is_refused_unless_it_is_shaped_like_one() {
    // These reach `parse` straight off a WebSocket frame, before anything
    // puts them in a log line, a thread name or a map lookup.
    for bad in [
        "",
        "../../etc/passwd",
        "has space",
        "has\nnewline",
        "tab\there",
        "semi;colon",
        "quote\"mark",
        "percent%2e%2e",
        "nul\0byte",
        &"x".repeat(33),
    ] {
        assert!(
            TerminalId::parse(bad).is_none(),
            "{bad:?} was accepted as a terminal id"
        );
    }
}

#[test]
fn a_generated_id_survives_the_round_trip_a_client_puts_it_through() {
    let id = TerminalId::generate().unwrap();
    let back = TerminalId::parse(id.as_str()).expect("a generated id must parse");
    assert_eq!(id, back);
    assert_eq!(back.to_string(), id.as_str());
    assert_ne!(
        id,
        TerminalId::generate().unwrap(),
        "ids must not be guessable by incrementing"
    );
}

// ---- what a terminal may print about itself ----------------------------

#[test]
fn a_terminal_never_prints_the_session_that_opened_it() {
    // Every open writes a log line that includes `opened_by`. A session id is
    // a bearer cookie: in a log file it is a way back into the server. The
    // registry never holds one — `Opener::Client` carries the hash — so this
    // is now structurally true rather than a property of a `Debug` impl, and
    // the test says so by starting from a real cookie.
    let cookie = SessionId::generate().unwrap();
    let r = rig(Limits::default());
    let term = r.registry.open(client(&cookie.hash())).unwrap();

    let printed = format!("{term:?} {:?}", term.opened_by());
    assert!(
        !printed.contains(&cookie.encode()),
        "a session id reached a Debug line: {printed}"
    );
    // The digest does appear, and is meant to: matching a log line against a
    // revocation is the one thing `opened_by` is for.
    assert!(printed.contains(&cookie.hash().hex()), "got {printed}");
}

#[test]
fn a_refused_keystroke_is_not_quoted_back_in_the_error() {
    // The error from a write ends up in a response and an audit record. The
    // bytes it refused may be what was typed at a password prompt.
    let r = rig(Limits::default());
    r.backend.next(Plan {
        wedged_input: true,
        ..Plan::default()
    });
    let term = r.registry.open(shell_spec()).unwrap();

    let secret = "hunter2-correct-horse";
    let err = loop {
        match term.write(secret.as_bytes()) {
            Ok(()) => continue,
            Err(e) => break e,
        }
    };
    let shown = format!("{err} {err:?}");
    assert!(
        !shown.contains("hunter2"),
        "the error quoted the input: {shown}"
    );
}
