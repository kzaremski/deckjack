//! Lockdown: the kill switch, tested through its public API only.
//!
//! The claims under test are the ones the design makes, in its order:
//!
//! 1. the four steps happen in order, and a failure in one does not stop the
//!    rest — a kill switch an attacker can halt by causing one problem is
//!    not a kill switch;
//! 2. the locked state survives a restart, so every persistence test here
//!    reopens the store rather than trusting the live object;
//! 3. the asymmetry: triggering needs nothing but a session, unlocking needs
//!    the passkey plus a host action or an offline code;
//! 4. the automatic triggers fire at their thresholds and not before;
//! 5. while locked, every route serves the locked page and no handler runs.
//!
//! Nothing here sleeps. The clock, the random source, the children, the
//! signals, the outbound queues, the passkey verifier and the journal all
//! arrive through traits, so the grace period is "waited" by a fake clock
//! that only writes down what it was asked for.

use std::io;
use std::net::IpAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicUsize, Ordering::SeqCst};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use deckjack_core::auth::code::RandomError;
use deckjack_core::lockdown::{
    self, AutoPolicy, Burst, Child, ChildKind, Children, Clock, Deps, Engagement, FileJournal,
    Frozen, Identity, JOURNAL_PATH, Journal, JournalError, Lockdown, MemoryJournal, Observation,
    OsSignals, Outbound, OutboundError, PasskeyAssertion, PasskeyVerifier, PasskeysUnavailable,
    Policy, RECORD_SCHEMA, RECOVERY_CODE_LEN, Record, RecoveryCode, RecoveryError, Report,
    Response, SecondFactor, SessionKey, Sessions, Signal, SignalError, Signals, Surface, Trigger,
    TriggerKind, UnlockError, UnlockRequest, Verdict, Watchdog,
};

// ---- fakes --------------------------------------------------------------

/// Every fake writes one line here, so "the four steps happen in order" is a
/// single assertion over a single list rather than four assertions that each
/// prove only that something happened.
type Log = Arc<Mutex<Vec<String>>>;

fn say(log: &Log, line: impl Into<String>) {
    log.lock()
        .unwrap_or_else(|e| e.into_inner())
        .push(line.into());
}

fn lines(log: &Log) -> Vec<String> {
    log.lock().unwrap_or_else(|e| e.into_inner()).clone()
}

/// A fixed starting point, so a journal written in a test is comparable.
fn t0() -> SystemTime {
    UNIX_EPOCH + Duration::from_secs(1_800_000_000)
}

struct FakeClock {
    sleeps: Arc<Mutex<Vec<Duration>>>,
    now: Mutex<SystemTime>,
}

impl Clock for FakeClock {
    fn now(&self) -> SystemTime {
        *self.now.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Records the request and moves the fake clock forward. No test here
    /// waits for anything, so a grace period of two seconds costs nothing.
    fn sleep(&self, d: Duration) {
        self.sleeps
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(d);
        let mut now = self.now.lock().unwrap_or_else(|e| e.into_inner());
        *now += d;
    }
}

/// Distinct bytes on every call, so ten issued recovery codes are ten
/// different codes. A constant source would make "a code cannot be used
/// twice" pass for the wrong reason.
struct CountingRandom {
    next: Arc<AtomicU8>,
}

impl lockdown::Random for CountingRandom {
    fn fill(&self, buf: &mut [u8]) -> Result<(), RandomError> {
        for b in buf.iter_mut() {
            *b = self.next.fetch_add(1, SeqCst);
        }
        Ok(())
    }
}

struct FakeSessions {
    log: Log,
    count: usize,
    /// Set after the `Arc<Lockdown>` exists, so step 1 can ask whether the
    /// gate is already shut while it runs.
    watch: Arc<Mutex<Option<Weak<Lockdown>>>>,
}

impl Sessions for FakeSessions {
    fn revoke_all(&self) -> usize {
        let held = self
            .watch
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .and_then(Weak::upgrade);
        if let Some(ld) = held {
            say(&self.log, format!("gate_closed={}", ld.is_locked()));
        }
        say(&self.log, "revoke");
        self.count
    }

    fn adopt_signing_key(&self, key: SessionKey) {
        // Formatted deliberately: if `Debug` ever stopped redacting, the key
        // would land in this log and the secrets test below would catch it.
        say(&self.log, format!("rotate {key:?}"));
    }
}

struct FakeChildren {
    kids: Vec<Child>,
    /// How many `running()` calls still see the children. Lockdown polls, so
    /// "exited during the grace period" is a count of calls, not a duration.
    alive_calls: usize,
    calls: Arc<AtomicUsize>,
}

impl Children for FakeChildren {
    fn running(&self) -> Vec<Child> {
        if self.calls.fetch_add(1, SeqCst) < self.alive_calls {
            self.kids.clone()
        } else {
            Vec::new()
        }
    }
}

struct FakeSignals {
    log: Log,
    /// Pids whose signal fails outright, as a pid we may not signal would.
    fail: Vec<u32>,
    /// Pids already gone, which the trait reports as `Ok(false)`.
    gone: Vec<u32>,
}

impl Signals for FakeSignals {
    fn send(&self, pid: u32, signal: Signal) -> Result<bool, SignalError> {
        let name = match signal {
            Signal::Term => "term",
            Signal::Kill => "kill",
        };
        say(&self.log, format!("{name}:{pid}"));
        if self.fail.contains(&pid) {
            return Err(SignalError::Failed {
                pid,
                source: io::Error::other("fake: operation not permitted"),
            });
        }
        Ok(!self.gone.contains(&pid))
    }
}

struct FakeOutbound {
    log: Log,
    /// `None` makes step 3 fail.
    result: Option<Frozen>,
}

impl Outbound for FakeOutbound {
    fn freeze(&self) -> Result<Frozen, OutboundError> {
        say(&self.log, "freeze");
        match self.result {
            Some(frozen) => Ok(frozen),
            None => Err(OutboundError("the mail queue is unreachable".into())),
        }
    }
}

struct AcceptPasskey;

impl PasskeyVerifier for AcceptPasskey {
    fn verify(&self, _assertion: &PasskeyAssertion) -> Result<(), UnlockError> {
        Ok(())
    }
}

struct RejectPasskey;

impl PasskeyVerifier for RejectPasskey {
    fn verify(&self, _assertion: &PasskeyAssertion) -> Result<(), UnlockError> {
        Err(UnlockError::BadPasskey)
    }
}

/// A journal that can be made to fail after the fact, which is how the
/// "unlocked in memory but not on disk" path is reached.
struct FlakyJournal {
    /// Shared so a test can "restart" over the same store.
    inner: Arc<MemoryJournal>,
    failing: Arc<AtomicBool>,
}

impl Journal for FlakyJournal {
    fn load(&self) -> Result<Option<Record>, JournalError> {
        if self.failing.load(SeqCst) {
            return Err(JournalError::Read {
                path: PathBuf::from(JOURNAL_PATH),
                source: io::Error::other("fake: I/O error"),
            });
        }
        self.inner.load()
    }

    fn save(&self, record: &Record) -> Result<(), JournalError> {
        if self.failing.load(SeqCst) {
            return Err(JournalError::Write {
                path: PathBuf::from(JOURNAL_PATH),
                source: io::Error::other("fake: no space left on device"),
            });
        }
        self.inner.save(record)
    }
}

// ---- fixture ------------------------------------------------------------

enum Passkeys {
    /// What this build actually ships.
    Unavailable,
    Accept,
    Reject,
}

struct Fixture {
    log: Log,
    sleeps: Arc<Mutex<Vec<Duration>>>,
    random: Arc<AtomicU8>,
    watch: Arc<Mutex<Option<Weak<Lockdown>>>>,
    sessions: usize,
    kids: Vec<Child>,
    alive_calls: usize,
    fail_signals: Vec<u32>,
    gone: Vec<u32>,
    outbound: Option<Frozen>,
    passkeys: Passkeys,
    policy: Policy,
    node_id: String,
}

impl Fixture {
    fn new() -> Fixture {
        Fixture {
            log: Log::default(),
            sleeps: Arc::default(),
            random: Arc::default(),
            watch: Arc::default(),
            sessions: 3,
            kids: Vec::new(),
            alive_calls: 0,
            fail_signals: Vec::new(),
            gone: Vec::new(),
            outbound: Some(Frozen {
                mail_queued: 2,
                inbox_pending: 1,
                tasks_scheduled: 4,
            }),
            passkeys: Passkeys::Unavailable,
            policy: Policy {
                grace: Duration::from_millis(400),
                grace_checks: 4,
                ..Policy::default()
            },
            node_id: "node-test".to_string(),
        }
    }

    /// Fresh dependencies over a given journal. Called twice by the restart
    /// tests: the shared probes stay, the per-run counters do not.
    fn deps(&self, journal: Box<dyn Journal + Send + Sync>) -> Deps {
        Deps {
            clock: Box::new(FakeClock {
                sleeps: self.sleeps.clone(),
                now: Mutex::new(t0()),
            }),
            random: Box::new(CountingRandom {
                next: self.random.clone(),
            }),
            sessions: Box::new(FakeSessions {
                log: self.log.clone(),
                count: self.sessions,
                watch: self.watch.clone(),
            }),
            children: Box::new(FakeChildren {
                kids: self.kids.clone(),
                alive_calls: self.alive_calls,
                calls: Arc::new(AtomicUsize::new(0)),
            }),
            signals: Box::new(FakeSignals {
                log: self.log.clone(),
                fail: self.fail_signals.clone(),
                gone: self.gone.clone(),
            }),
            outbound: Box::new(FakeOutbound {
                log: self.log.clone(),
                result: self.outbound,
            }),
            passkeys: match self.passkeys {
                Passkeys::Unavailable => Box::new(PasskeysUnavailable),
                Passkeys::Accept => Box::new(AcceptPasskey),
                Passkeys::Reject => Box::new(RejectPasskey),
            },
            journal,
        }
    }

    fn restore(&self, journal: Box<dyn Journal + Send + Sync>) -> Lockdown {
        Lockdown::restore(self.deps(journal), self.policy, self.node_id.clone())
            .expect("the journal in this fixture is readable")
    }

    fn fresh(&self) -> Lockdown {
        self.restore(Box::new(MemoryJournal::new()))
    }
}

fn child(pid: u32, kind: ChildKind) -> Child {
    Child {
        pid,
        kind,
        label: format!("fake-{pid}"),
    }
}

fn ip(last: u8) -> IpAddr {
    IpAddr::from([100, 64, 0, last])
}

/// Everything a holder of a plain session could present. None of it unlocks.
fn assertion() -> PasskeyAssertion {
    PasskeyAssertion::new(b"not a real webauthn assertion".to_vec())
}

// ---- the four steps -----------------------------------------------------

#[test]
fn the_four_steps_run_in_the_order_the_design_states() {
    let mut f = Fixture::new();
    f.kids = vec![child(4242, ChildKind::Terminal)];
    f.alive_calls = 1; // seen once, then gone: SIGTERM was enough.
    let ld = f.fresh();

    ld.engage(Trigger::Operator {
        surface: Surface::Palette,
    });

    // Revoke and rotate, then kill, then freeze. A refactor that froze the
    // outbound queues before revoking sessions would leave a live cookie
    // able to queue more mail, and would fail here.
    let log: Vec<String> = lines(&f.log)
        .into_iter()
        .filter(|l| !l.starts_with("gate_closed"))
        .collect();
    assert_eq!(
        log,
        vec![
            "revoke".to_string(),
            "rotate SessionKey(<redacted>)".to_string(),
            "term:4242".to_string(),
            "freeze".to_string(),
        ]
    );
}

#[test]
fn the_gate_is_already_shut_while_the_earlier_steps_are_still_running() {
    // Step 4 is listed last because it is what the operator sees last, but a
    // request arriving while sessions are being revoked must already be
    // refused; otherwise lockdown has a window in which it is doing the work
    // and still serving the workspace.
    let f = Fixture::new();
    let ld = Arc::new(f.fresh());
    *f.watch.lock().unwrap() = Some(Arc::downgrade(&ld));

    ld.engage(Trigger::HostCommand);

    assert!(
        lines(&f.log).contains(&"gate_closed=true".to_string()),
        "the gate was still open during step 1: {:?}",
        lines(&f.log)
    );
}

#[test]
fn engage_revokes_every_session_and_rotates_the_signing_key() {
    let f = Fixture::new();
    let ld = f.fresh();

    let report = ld.engage(Trigger::HostCommand);

    assert_eq!(report.sessions_revoked, 3);
    assert!(report.key_rotated);
    assert!(ld.is_locked());
    // The generation is what tells a cookie minted before the incident from
    // one minted after, so it has to move.
    assert_eq!(ld.status().key_generation, 1);
}

#[test]
fn a_failure_in_one_step_does_not_stop_the_others() {
    // The pattern the module relies on: nothing in `engage` returns `Err`,
    // because a kill switch that stops at the first problem is one an
    // attacker can stop by causing one problem.
    let mut f = Fixture::new();
    f.kids = vec![child(11, ChildKind::Agent), child(12, ChildKind::Sync)];
    f.alive_calls = 1;
    f.fail_signals = vec![11]; // the first child cannot be signaled at all
    f.outbound = None; // and step 3 fails outright
    let ld = f.fresh();

    let report = ld.engage(Trigger::TetherTerminal);

    assert!(ld.is_locked(), "the gate must shut whatever else failed");
    assert_eq!(report.sessions_revoked, 3);
    assert!(report.key_rotated);
    // The second child was still signaled after the first one failed.
    assert!(lines(&f.log).contains(&"term:12".to_string()));
    assert_eq!(report.terminated, 1);
    assert_eq!(report.frozen, None);
    assert_eq!(
        report.errors.len(),
        2,
        "both failures must be reported, not swallowed: {:?}",
        report.errors
    );
}

#[test]
fn a_journal_that_cannot_be_written_still_locks_the_node_now() {
    // Persistence is for the restart case. Losing it is worth a loud log
    // line, not a refusal to lock.
    let f = Fixture::new();
    let failing = Arc::new(AtomicBool::new(false));
    let ld = f.restore(Box::new(FlakyJournal {
        inner: Arc::new(MemoryJournal::new()),
        failing: failing.clone(),
    }));
    failing.store(true, SeqCst); // the store goes away after startup

    let report = ld.engage(Trigger::HostCommand);

    assert!(ld.is_locked());
    assert!(report.key_rotated);
}

#[test]
fn the_report_does_not_claim_a_child_was_stopped_when_it_was_already_gone() {
    let mut f = Fixture::new();
    f.kids = vec![child(500, ChildKind::App)];
    f.alive_calls = 1;
    f.gone = vec![500];
    let ld = f.fresh();

    let report = ld.engage(Trigger::HostCommand);

    assert_eq!(report.children_seen, 1);
    assert_eq!(report.terminated, 0);
    assert_eq!(report.killed, 0);
    assert!(report.survived.is_empty());
    assert!(report.errors.is_empty());
}

#[test]
fn a_child_that_exits_during_the_grace_period_is_never_killed() {
    let mut f = Fixture::new();
    f.kids = vec![child(77, ChildKind::Terminal)];
    f.alive_calls = 2; // the snapshot before the first wait still sees it
    let ld = f.fresh();

    let report = ld.engage(Trigger::HostCommand);

    assert_eq!(report.terminated, 1);
    assert_eq!(report.killed, 0, "SIGKILL after a clean exit");
    assert!(report.survived.is_empty());
    assert!(
        !lines(&f.log).iter().any(|l| l.starts_with("kill:")),
        "nothing should have been SIGKILLed: {:?}",
        lines(&f.log)
    );
}

#[test]
fn a_child_that_ignores_sigterm_is_killed_and_one_that_survives_is_reported() {
    let mut f = Fixture::new();
    f.kids = vec![child(99, ChildKind::Agent)];
    f.alive_calls = usize::MAX; // never exits, not even after SIGKILL
    let ld = f.fresh();

    let report = ld.engage(Trigger::HostCommand);

    assert_eq!(report.terminated, 1);
    assert_eq!(report.killed, 1);
    assert_eq!(
        report.survived,
        vec![99],
        "a process still alive after SIGKILL must be named, not assumed dead"
    );
}

#[test]
fn the_kill_sweep_waits_in_slices_and_stays_inside_the_grace_period() {
    let mut f = Fixture::new();
    f.kids = vec![child(99, ChildKind::Agent)];
    f.alive_calls = usize::MAX;
    f.policy = Policy {
        grace: Duration::from_secs(2),
        grace_checks: 4,
        ..Policy::default()
    };
    let ld = f.fresh();

    ld.engage(Trigger::HostCommand);

    let slept = f.sleeps.lock().unwrap().clone();
    let slice = Duration::from_millis(500);
    assert!(
        slept.iter().all(|d| *d == slice),
        "the grace period must be polled in equal slices: {slept:?}"
    );
    let total: Duration = slept.iter().sum();
    // Four slices of grace, plus the one last look after SIGKILL.
    assert_eq!(total, f.policy.grace + slice);
}

#[test]
fn no_children_means_no_signals_and_no_waiting_at_all() {
    // The common case. A kill switch should not stand around for the grace
    // period when there is nothing to wait for.
    let f = Fixture::new();
    let ld = f.fresh();

    let report = ld.engage(Trigger::HostCommand);

    assert_eq!(report.children_seen, 0);
    assert!(f.sleeps.lock().unwrap().is_empty());
    assert!(
        !lines(&f.log).iter().any(|l| l.starts_with("term:")),
        "{:?}",
        lines(&f.log)
    );
}

#[test]
fn a_grace_check_count_of_zero_does_not_divide_by_zero() {
    let mut f = Fixture::new();
    f.kids = vec![child(1234, ChildKind::App)];
    f.alive_calls = usize::MAX;
    f.policy = Policy {
        grace: Duration::from_secs(2),
        grace_checks: 0,
        ..Policy::default()
    };
    let ld = f.fresh();

    let report = ld.engage(Trigger::HostCommand);

    assert_eq!(report.killed, 1);
}

#[test]
fn lockdown_never_signals_init_the_process_group_or_the_server_itself() {
    // Checked against the real implementation, because this is the one place
    // a bug in the child table could turn the kill switch into a way to take
    // down the machine. No real process is signaled: all three are refused
    // before any syscall.
    let signals = OsSignals::new();
    let own = std::process::id();

    for pid in [0, 1, own] {
        for signal in [Signal::Term, Signal::Kill] {
            let err = signals
                .send(pid, signal)
                .expect_err("pid {pid} must be refused");
            assert!(
                matches!(err, SignalError::Refused { .. }),
                "pid {pid} was not refused: {err:?}"
            );
        }
    }
}

// ---- the locked state survives a restart --------------------------------

#[test]
fn a_locked_node_comes_back_locked_after_a_restart() {
    let dir = tempfile::tempdir().unwrap();
    let f = Fixture::new();

    {
        let ld = f.restore(Box::new(FileJournal::new(dir.path())));
        ld.engage(Trigger::Operator {
            surface: Surface::Hotkey,
        });
        assert!(ld.is_locked());
    } // the process "dies" here

    // A supervisor or an attacker restarting the process is the exact case
    // this exists for, so the store is reopened rather than reused.
    let after = f.restore(Box::new(FileJournal::new(dir.path())));
    assert!(after.is_locked(), "a restart cleared the lockdown");

    let engaged = after.status().engaged.expect("the cause must survive too");
    assert_eq!(engaged.trigger, TriggerKind::Operator);
    assert_eq!(engaged.at, 1_800_000_000);
    assert_eq!(engaged.node_id, "node-test");
}

#[test]
fn an_unlock_also_survives_a_restart() {
    let dir = tempfile::tempdir().unwrap();
    let mut f = Fixture::new();
    f.passkeys = Passkeys::Accept;

    let code = {
        let ld = f.restore(Box::new(FileJournal::new(dir.path())));
        let codes = ld.issue_recovery_codes(3).unwrap();
        ld.engage(Trigger::HostCommand);
        ld.unlock(UnlockRequest {
            identity: Identity::HostConsole,
            second: SecondFactor::RecoveryCode(codes[0].reveal()),
        })
        .unwrap();
        assert!(!ld.is_locked());
        codes[0].reveal()
    };

    let after = f.restore(Box::new(FileJournal::new(dir.path())));
    assert!(!after.is_locked(), "a restart re-locked an unlocked node");
    // And the code that was spent is still spent on disk.
    assert_eq!(after.recovery_codes_left(), 2);
    assert_eq!(after.status().recovery_codes_spent, 1);
    after.engage(Trigger::HostCommand);
    assert!(matches!(
        after.unlock(UnlockRequest {
            identity: Identity::HostConsole,
            second: SecondFactor::RecoveryCode(code),
        }),
        Err(UnlockError::BadRecoveryCode)
    ));
    assert!(after.is_locked());
}

#[test]
fn re_engaging_while_locked_keeps_the_first_cause_and_counts_the_rest() {
    let dir = tempfile::tempdir().unwrap();
    let f = Fixture::new();

    {
        let ld = f.restore(Box::new(FileJournal::new(dir.path())));
        ld.engage(Trigger::FailedPasskeys {
            count: 5,
            within: Duration::from_secs(300),
        });
        ld.engage(Trigger::Operator {
            surface: Surface::Menu,
        });
        ld.engage(Trigger::HostCommand);
    }

    let after = f.restore(Box::new(FileJournal::new(dir.path())));
    let engaged = after.status().engaged.unwrap();
    assert_eq!(
        engaged.trigger,
        TriggerKind::FailedPasskeys,
        "the first trigger is the interesting one"
    );
    assert_eq!(engaged.reengaged, 2);
}

#[test]
fn a_crash_partway_through_lockdown_still_restores_as_locked() {
    // An engagement with no report is the fingerprint of a process that died
    // between step 1 and step 3. It must still read as locked.
    let dir = tempfile::tempdir().unwrap();
    let journal = FileJournal::new(dir.path());
    journal
        .save(&Record {
            schema: RECORD_SCHEMA,
            engaged: Some(Engagement {
                at: 1_800_000_000,
                trigger: TriggerKind::TetherTerminal,
                reason: "a terminal opened from a tether session".to_string(),
                node_id: "node-test".to_string(),
                reengaged: 0,
                report: None,
            }),
            key_generation: 7,
            recovery: Vec::new(),
            recovery_spent: 0,
        })
        .unwrap();

    let f = Fixture::new();
    let ld = f.restore(Box::new(FileJournal::new(dir.path())));

    assert!(ld.is_locked());
    assert!(ld.status().engaged.unwrap().report.is_none());
}

#[test]
fn a_corrupt_journal_refuses_the_start_rather_than_starting_open() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(JOURNAL_PATH);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, "{\"schema\":1,\"engaged\"").unwrap();

    let f = Fixture::new();
    // `Lockdown` has no `Debug` impl, by design: it holds the record. So the
    // refusal is matched rather than unwrapped.
    let Err(err) = Lockdown::restore(
        f.deps(Box::new(FileJournal::new(dir.path()))),
        f.policy,
        f.node_id.clone(),
    ) else {
        panic!("a journal nobody can read must not come up unlocked");
    };

    // The message has to tell the operator what to do on the host, because
    // there is nothing they can do from a browser.
    let text = err.to_string();
    assert!(text.contains(JOURNAL_PATH), "{text}");
}

#[test]
fn a_journal_from_the_future_is_refused_rather_than_guessed_at() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(JOURNAL_PATH);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    // Shape of a record this build understands, but a schema it does not. A
    // newer Deckjack may have added a field that decides whether the node is
    // locked, so "it parsed, so it is fine" is not available.
    std::fs::write(
        &path,
        format!(
            "{{\"schema\":{},\"engaged\":null,\"key_generation\":0,\
             \"recovery\":[],\"recovery_spent\":0}}",
            RECORD_SCHEMA + 1
        ),
    )
    .unwrap();

    let f = Fixture::new();
    assert!(
        Lockdown::restore(
            f.deps(Box::new(FileJournal::new(dir.path()))),
            f.policy,
            f.node_id.clone()
        )
        .is_err()
    );
}

#[test]
fn a_sealed_node_is_locked_says_why_and_cannot_be_unlocked_by_a_code() {
    // What a caller does with that refusal: serve the locked page instead of
    // coming up open. The digests went with the unreadable file, so there is
    // no code left to match.
    let f = Fixture::new();
    let ld = Lockdown::sealed(
        f.deps(Box::new(MemoryJournal::new())),
        f.policy,
        f.node_id.clone(),
    );

    assert!(ld.is_locked());
    assert!(ld.status().engaged.is_none());
    assert!(matches!(
        ld.unlock(UnlockRequest {
            identity: Identity::HostConsole,
            second: SecondFactor::RecoveryCode("0123-4567-89AB-CDEF-GHJK".to_string()),
        }),
        Err(UnlockError::NoRecoveryCodes)
    ));
    assert!(ld.is_locked());

    let page = lockdown::locked_page(&ld.status());
    assert!(
        page.contains("journal could not be read"),
        "the page must say why there is no detail to show: {page}"
    );
}

#[test]
fn the_journal_is_readable_only_by_its_owner_and_lives_in_the_hidden_directory() {
    let dir = tempfile::tempdir().unwrap();
    let f = Fixture::new();
    let ld = f.restore(Box::new(FileJournal::new(dir.path())));
    ld.engage(Trigger::HostCommand);

    let path = dir.path().join(JOURNAL_PATH);
    assert!(path.exists(), "nothing was persisted");
    assert!(PathBuf::from(JOURNAL_PATH).starts_with(".deckjack"));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "the journal says when and why we were locked");
    }
}

#[test]
fn the_journal_never_records_the_address_a_trigger_carried() {
    // `Engagement::reason` is written during an incident and read afterwards
    // by whoever is holding the machine. The address belongs in the audit
    // log, which ships off the node, not in a file that outlives the
    // incident. The most common cause of a new-device trigger is the
    // operator's own new laptop.
    let dir = tempfile::tempdir().unwrap();
    let f = Fixture::new();
    let ld = f.restore(Box::new(FileJournal::new(dir.path())));

    ld.engage(Trigger::NewDevice { ip: ip(37) });

    let on_disk = std::fs::read_to_string(dir.path().join(JOURNAL_PATH)).unwrap();
    assert!(
        !on_disk.contains("100.64.0.37"),
        "the journal recorded an address: {on_disk}"
    );
    // The cause is still legible, which is the point of keeping a reason.
    assert!(on_disk.contains("device that is not known"), "{on_disk}");
}

#[test]
fn the_journal_never_records_a_remote_source_name_verbatim() {
    // A signed ntfy or Matrix command carries a caller-supplied string. It
    // reaches the journal and, through the locked page, a browser.
    let dir = tempfile::tempdir().unwrap();
    let f = Fixture::new();
    let ld = f.restore(Box::new(FileJournal::new(dir.path())));
    ld.engage(Trigger::RemoteCommand {
        source: "<script>alert(1)</script>".to_string(),
    });

    let on_disk = std::fs::read_to_string(dir.path().join(JOURNAL_PATH)).unwrap();
    assert!(!on_disk.contains("script"), "{on_disk}");
}

// ---- the asymmetry ------------------------------------------------------

#[test]
fn triggering_needs_nothing_but_a_session_and_unlocking_needs_more() {
    // In an emergency friction is the enemy, and the cost of a false
    // positive is an inconvenience.
    assert!(lockdown::may_engage(true));
    assert!(!lockdown::may_engage(false));
}

#[test]
fn a_stolen_session_has_no_unlock_path_in_this_build() {
    // Everything reachable from a browser session: an assertion this build
    // cannot verify, with either second factor. `Identity::HostConsole` is
    // only constructible by the control-socket caller, and the test below
    // shows it is one factor on its own.
    let f = Fixture::new(); // PasskeysUnavailable, as shipped
    let ld = f.fresh();
    let codes = ld.issue_recovery_codes(2).unwrap();
    ld.engage(Trigger::Operator {
        surface: Surface::Palette,
    });

    for second in [
        SecondFactor::HostCommand,
        SecondFactor::RecoveryCode(codes[0].reveal()),
    ] {
        let err = ld
            .unlock(UnlockRequest {
                identity: Identity::Passkey(assertion()),
                second,
            })
            .expect_err("a session must not be able to unlock");
        assert!(matches!(err, UnlockError::PasskeysUnavailable), "{err:?}");
        assert!(ld.is_locked());
    }
    // And no code was burned by the attempts.
    assert_eq!(ld.recovery_codes_left(), 2);
}

#[test]
fn the_host_console_plus_the_host_command_is_one_factor_and_is_refused() {
    let f = Fixture::new();
    let ld = f.fresh();
    ld.engage(Trigger::HostCommand);

    let err = ld
        .unlock(UnlockRequest {
            identity: Identity::HostConsole,
            second: SecondFactor::HostCommand,
        })
        .expect_err("the same factor twice is not two factors");
    assert!(matches!(err, UnlockError::OneFactor), "{err:?}");
    assert!(ld.is_locked());
}

#[test]
fn the_host_console_with_an_offline_code_is_the_usable_path() {
    let f = Fixture::new();
    let ld = f.fresh();
    let codes = ld.issue_recovery_codes(2).unwrap();
    ld.engage(Trigger::TetherTerminal);

    ld.unlock(UnlockRequest {
        identity: Identity::HostConsole,
        second: SecondFactor::RecoveryCode(codes[1].reveal()),
    })
    .unwrap();

    assert!(!ld.is_locked());
    assert!(ld.status().engaged.is_none());
    assert_eq!(ld.recovery_codes_left(), 1);
    assert_eq!(ld.status().recovery_codes_spent, 1);
}

#[test]
fn a_passkey_plus_the_host_command_unlocks_without_spending_a_code() {
    let mut f = Fixture::new();
    f.passkeys = Passkeys::Accept;
    let ld = f.fresh();
    let codes = ld.issue_recovery_codes(2).unwrap();
    ld.engage(Trigger::HostCommand);

    ld.unlock(UnlockRequest {
        identity: Identity::Passkey(assertion()),
        second: SecondFactor::HostCommand,
    })
    .unwrap();

    assert!(!ld.is_locked());
    assert_eq!(ld.recovery_codes_left(), codes.len());
}

#[test]
fn a_rejected_passkey_does_not_burn_a_recovery_code() {
    let mut f = Fixture::new();
    f.passkeys = Passkeys::Reject;
    let ld = f.fresh();
    let codes = ld.issue_recovery_codes(4).unwrap();
    ld.engage(Trigger::HostCommand);

    let err = ld
        .unlock(UnlockRequest {
            identity: Identity::Passkey(assertion()),
            second: SecondFactor::RecoveryCode(codes[0].reveal()),
        })
        .unwrap_err();

    assert!(matches!(err, UnlockError::BadPasskey), "{err:?}");
    assert!(ld.is_locked());
    assert_eq!(
        ld.recovery_codes_left(),
        4,
        "guessing passkeys would otherwise consume the codes"
    );
}

#[test]
fn the_host_console_identity_can_be_turned_off() {
    let mut f = Fixture::new();
    f.policy = Policy {
        allow_host_console_identity: false,
        ..f.policy
    };
    let ld = f.fresh();
    let codes = ld.issue_recovery_codes(1).unwrap();
    ld.engage(Trigger::HostCommand);

    let err = ld
        .unlock(UnlockRequest {
            identity: Identity::HostConsole,
            second: SecondFactor::RecoveryCode(codes[0].reveal()),
        })
        .unwrap_err();

    assert!(matches!(err, UnlockError::HostConsoleNotAllowed), "{err:?}");
    assert!(ld.is_locked());
    assert_eq!(ld.recovery_codes_left(), 1);
}

#[test]
fn a_recovery_code_cannot_be_used_twice() {
    let f = Fixture::new();
    let ld = f.fresh();
    let codes = ld.issue_recovery_codes(3).unwrap();
    let code = codes[0].reveal();

    ld.engage(Trigger::HostCommand);
    ld.unlock(UnlockRequest {
        identity: Identity::HostConsole,
        second: SecondFactor::RecoveryCode(code.clone()),
    })
    .unwrap();

    ld.engage(Trigger::HostCommand);
    let err = ld
        .unlock(UnlockRequest {
            identity: Identity::HostConsole,
            second: SecondFactor::RecoveryCode(code),
        })
        .unwrap_err();

    assert!(matches!(err, UnlockError::BadRecoveryCode), "{err:?}");
    assert!(ld.is_locked());
    assert_eq!(ld.recovery_codes_left(), 2);
}

#[test]
fn a_wrong_code_does_not_say_whether_it_was_well_formed() {
    // Telling an attacker their guess was at least the right shape is free
    // information, so both refusals are the same refusal.
    let f = Fixture::new();
    let ld = f.fresh();
    ld.issue_recovery_codes(2).unwrap();
    ld.engage(Trigger::HostCommand);

    for typed in [
        "ZZZZ-ZZZZ-ZZZZ-ZZZZ-ZZZZ",      // well formed, wrong
        "nope",                          // too short
        "0123-4567-89AB-CDEF-GHJK-MNPQ", // too long
        "0123-4567-89AB-CDEF-GHJ!",      // not in the alphabet
        "",
    ] {
        let err = ld
            .unlock(UnlockRequest {
                identity: Identity::HostConsole,
                second: SecondFactor::RecoveryCode(typed.to_string()),
            })
            .unwrap_err();
        assert!(
            matches!(err, UnlockError::BadRecoveryCode),
            "{typed:?} gave {err:?}"
        );
    }
    assert!(ld.is_locked());
    assert_eq!(ld.recovery_codes_left(), 2);
}

#[test]
fn an_absurdly_long_input_is_refused_without_being_hashed() {
    let f = Fixture::new();
    let ld = f.fresh();
    ld.issue_recovery_codes(1).unwrap();
    ld.engage(Trigger::HostCommand);

    let err = ld
        .unlock(UnlockRequest {
            identity: Identity::HostConsole,
            second: SecondFactor::RecoveryCode("A".repeat(100_000)),
        })
        .unwrap_err();

    assert!(matches!(err, UnlockError::BadRecoveryCode), "{err:?}");
    assert!(ld.is_locked());
}

#[test]
fn a_code_is_accepted_however_it_was_written_down() {
    // Crockford base32: the separators do not matter, case does not matter,
    // and O reads as zero, I and L as one.
    let f = Fixture::new();
    let ld = f.fresh();
    let codes = ld.issue_recovery_codes(1).unwrap();
    let printed = codes[0].reveal();
    assert_eq!(printed.len(), RECOVERY_CODE_LEN + 4, "XXXX-XXXX-...");

    let typed = printed
        .to_lowercase()
        .replace('-', " ")
        .replace('0', "o")
        .replace('1', "l");

    ld.engage(Trigger::HostCommand);
    ld.unlock(UnlockRequest {
        identity: Identity::HostConsole,
        second: SecondFactor::RecoveryCode(typed),
    })
    .unwrap();

    assert!(!ld.is_locked());
}

#[test]
fn recovery_codes_cannot_be_issued_while_locked_down() {
    // Minting a code is an unlock path, so it must not be reachable from the
    // locked state.
    let f = Fixture::new();
    let ld = f.fresh();
    ld.issue_recovery_codes(2).unwrap();
    ld.engage(Trigger::HostCommand);

    let err = ld.issue_recovery_codes(10).unwrap_err();

    assert!(matches!(err, RecoveryError::Locked), "{err:?}");
    assert_eq!(ld.recovery_codes_left(), 2, "the live set must not change");
}

#[test]
fn issuing_a_new_set_retires_the_old_one() {
    let f = Fixture::new();
    let ld = f.fresh();
    let old = ld.issue_recovery_codes(2).unwrap();
    ld.issue_recovery_codes(2).unwrap();
    ld.engage(Trigger::HostCommand);

    let err = ld
        .unlock(UnlockRequest {
            identity: Identity::HostConsole,
            second: SecondFactor::RecoveryCode(old[0].reveal()),
        })
        .unwrap_err();

    assert!(matches!(err, UnlockError::BadRecoveryCode), "{err:?}");
    assert!(ld.is_locked());
}

#[test]
fn a_failed_journal_write_leaves_the_node_locked_and_a_restart_agrees() {
    // Unlocking in memory while the disk still says locked would mean a node
    // that unlocks now and re-locks on the next restart with nobody able to
    // say why. The write comes first.
    let mut f = Fixture::new();
    f.passkeys = Passkeys::Accept;
    let failing = Arc::new(AtomicBool::new(false));
    let store = Arc::new(MemoryJournal::new());
    let ld = f.restore(Box::new(FlakyJournal {
        inner: store.clone(),
        failing: failing.clone(),
    }));
    ld.issue_recovery_codes(2).unwrap();
    ld.engage(Trigger::HostCommand);

    failing.store(true, SeqCst);
    let err = ld
        .unlock(UnlockRequest {
            identity: Identity::Passkey(assertion()),
            second: SecondFactor::HostCommand,
        })
        .unwrap_err();

    assert!(matches!(err, UnlockError::JournalWrite { .. }), "{err:?}");
    assert!(
        ld.is_locked(),
        "an unlock that was not persisted must not hold"
    );
    assert!(ld.status().engaged.is_some(), "the cause must be restored");

    // And the store agrees, which is the whole reason the write goes first.
    failing.store(false, SeqCst);
    let after = f.restore(Box::new(FlakyJournal {
        inner: store,
        failing,
    }));
    assert!(after.is_locked());
}

#[test]
fn unlocking_a_node_that_is_not_locked_is_refused_before_any_factor_is_read() {
    let f = Fixture::new();
    let ld = f.fresh();
    ld.issue_recovery_codes(1).unwrap();

    let err = ld
        .unlock(UnlockRequest {
            identity: Identity::HostConsole,
            second: SecondFactor::RecoveryCode("0123-4567-89AB-CDEF-GHJK".to_string()),
        })
        .unwrap_err();

    assert!(matches!(err, UnlockError::NotLocked), "{err:?}");
    assert_eq!(ld.recovery_codes_left(), 1);
}

// ---- automatic triggers -------------------------------------------------

fn watchdog() -> Watchdog {
    Watchdog::new(AutoPolicy::default())
}

fn at(secs: u64) -> SystemTime {
    t0() + Duration::from_secs(secs)
}

#[test]
fn a_sign_in_from_a_known_device_is_not_an_incident() {
    let w = watchdog();
    assert_eq!(
        w.observe(
            Observation::SignIn {
                ip: ip(9),
                known_device: true
            },
            at(0)
        ),
        Verdict::Nothing
    );
}

#[test]
fn a_sign_in_from_a_new_device_raises_an_alert_rather_than_locking_down() {
    // A new device is also what happens when the operator buys a laptop, and
    // the sign-in already needed approval from an existing session.
    let w = watchdog();
    let verdict = w.observe(
        Observation::SignIn {
            ip: ip(9),
            known_device: false,
        },
        at(0),
    );
    assert_eq!(verdict, Verdict::Alert(Trigger::NewDevice { ip: ip(9) }));
}

#[test]
fn failed_passkeys_engage_on_the_fifth_attempt_and_not_before() {
    let w = watchdog();
    for n in 1..5 {
        assert_eq!(
            w.observe(Observation::FailedPasskey { ip: ip(2) }, at(n)),
            Verdict::Nothing,
            "attempt {n} should not have fired"
        );
    }
    assert_eq!(
        w.observe(Observation::FailedPasskey { ip: ip(2) }, at(5)),
        Verdict::Engage(Trigger::FailedPasskeys {
            count: 5,
            within: Duration::from_secs(300),
        })
    );
}

#[test]
fn failed_passkeys_spread_wider_than_the_window_never_add_up() {
    // The window is injected, not waited: five attempts an hour apart are
    // five separate events and must not look like a burst.
    let w = watchdog();
    for n in 0..20 {
        assert_eq!(
            w.observe(Observation::FailedPasskey { ip: ip(2) }, at(n * 3_600)),
            Verdict::Nothing,
            "hour {n} fired"
        );
    }
}

#[test]
fn one_burst_produces_one_verdict() {
    // Otherwise every further attempt while the operator is deciding raises
    // another alert.
    let w = watchdog();
    for n in 1..=5 {
        w.observe(Observation::FailedPasskey { ip: ip(2) }, at(n));
    }
    assert_eq!(
        w.observe(Observation::FailedPasskey { ip: ip(2) }, at(6)),
        Verdict::Nothing
    );
}

#[test]
fn a_terminal_opened_from_a_tether_session_engages_at_once() {
    // Tether mode has no terminal by design, so a terminal from one means
    // the scope check was bypassed. There is no threshold to reach.
    let w = watchdog();
    assert_eq!(
        w.observe(Observation::TetherTerminal, at(0)),
        Verdict::Engage(Trigger::TetherTerminal)
    );
}

#[test]
fn refused_agent_commands_alert_at_ten_within_the_minute() {
    let w = watchdog();
    for n in 1..10 {
        assert_eq!(
            w.observe(
                Observation::RefusedAgentCommand {
                    command: "files.delete".to_string()
                },
                at(n)
            ),
            Verdict::Nothing
        );
    }
    assert_eq!(
        w.observe(
            Observation::RefusedAgentCommand {
                command: "files.delete".to_string()
            },
            at(10)
        ),
        Verdict::Alert(Trigger::RefusedAgentCommands {
            count: 10,
            within: Duration::from_secs(60),
        })
    );
}

#[test]
fn the_two_counting_triggers_are_counted_separately() {
    let w = watchdog();
    for n in 1..=4 {
        w.observe(Observation::FailedPasskey { ip: ip(2) }, at(n));
    }
    for n in 5..=9 {
        w.observe(
            Observation::RefusedAgentCommand {
                command: "x".to_string(),
            },
            at(n),
        );
    }
    // Nine events in all, neither threshold reached: the fifth failed
    // passkey must still be what fires.
    assert!(matches!(
        w.observe(Observation::FailedPasskey { ip: ip(2) }, at(10)),
        Verdict::Engage(Trigger::FailedPasskeys { .. })
    ));
}

#[test]
fn an_ignore_policy_watches_without_locking_anything_down() {
    let w = Watchdog::new(AutoPolicy {
        new_device: Response::Ignore,
        tether_terminal: Response::Ignore,
        failed_passkeys: Burst {
            count: 2,
            within: Duration::from_secs(60),
            response: Response::Ignore,
        },
        refused_agent_commands: Burst {
            count: 2,
            within: Duration::from_secs(60),
            response: Response::Ignore,
        },
    });

    assert_eq!(
        w.observe(Observation::TetherTerminal, at(0)),
        Verdict::Nothing
    );
    w.observe(Observation::FailedPasskey { ip: ip(2) }, at(1));
    assert_eq!(
        w.observe(Observation::FailedPasskey { ip: ip(2) }, at(2)),
        Verdict::Nothing
    );
}

#[test]
fn exactly_the_four_triggers_the_design_calls_automatic_are_automatic() {
    // A renamed or forgotten variant here would quietly make an automatic
    // trigger look like an operator decision in the journal.
    let automatic = [
        TriggerKind::NewDevice,
        TriggerKind::FailedPasskeys,
        TriggerKind::TetherTerminal,
        TriggerKind::RefusedAgentCommands,
    ];
    let deliberate = [
        TriggerKind::Operator,
        TriggerKind::HostCommand,
        TriggerKind::RemoteCommand,
    ];

    for kind in automatic {
        assert!(kind.is_automatic(), "{kind:?}");
        assert!(!kind.label().is_empty());
    }
    for kind in deliberate {
        assert!(!kind.is_automatic(), "{kind:?}");
    }
}

#[test]
fn an_engagement_from_a_verdict_carries_the_real_cause() {
    // The watchdog hands the trigger back so that confirming an alert
    // engages with the cause rather than a generic one.
    let w = watchdog();
    let Verdict::Engage(trigger) = w.observe(Observation::TetherTerminal, at(0)) else {
        panic!("a tether terminal must engage");
    };
    let f = Fixture::new();
    let ld = f.fresh();
    ld.engage(trigger);
    assert_eq!(
        ld.status().engaged.unwrap().trigger,
        TriggerKind::TetherTerminal
    );
}

// ---- step 4: the locked page -------------------------------------------

#[test]
fn only_the_unlock_and_health_paths_keep_working_while_locked() {
    for path in ["/unlock", "/unlock/", "/unlock/options", "/healthz"] {
        assert!(lockdown::allowed_while_locked(path), "{path}");
    }
    // A path that merely begins with an allowed one is a different path.
    for path in [
        "/",
        "/unlockme",
        "/unlock-now",
        "/healthzz",
        "/ws",
        "/commands",
        "/apps/files",
        "/UNLOCK",
    ] {
        assert!(!lockdown::allowed_while_locked(path), "{path}");
    }
}

#[test]
fn the_locked_response_is_a_503_that_nothing_caches() {
    use axum::http::{StatusCode, header};

    let f = Fixture::new();
    let ld = f.fresh();
    ld.engage(Trigger::HostCommand);
    let response = lockdown::locked_response(&ld.status());

    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        response.headers().get(header::CACHE_CONTROL).unwrap(),
        "no-store"
    );
    assert!(
        response
            .headers()
            .get(header::CONTENT_SECURITY_POLICY)
            .is_some()
    );
}

#[test]
fn the_locked_page_escapes_everything_it_interpolates() {
    let mut f = Fixture::new();
    f.node_id = "<script>alert(1)</script>".to_string();
    let ld = f.fresh();
    ld.engage(Trigger::HostCommand);

    let page = lockdown::locked_page(&ld.status());

    assert!(!page.contains("<script>"), "{page}");
    assert!(page.contains("&lt;script&gt;"), "{page}");
}

#[test]
fn the_locked_page_never_shows_a_recovery_code_and_says_how_to_get_out() {
    let f = Fixture::new();
    let ld = f.fresh();
    let codes = ld.issue_recovery_codes(3).unwrap();
    ld.engage(Trigger::HostCommand);

    let page = lockdown::locked_page(&ld.status());

    for code in &codes {
        let printed = code.reveal();
        assert!(!page.contains(&printed), "a code is on the page: {printed}");
        let bare: String = printed.chars().filter(|c| *c != '-').collect();
        assert!(!page.contains(&bare), "a code is on the page: {bare}");
    }
    // It is the one page an operator sees during an incident, so it has to
    // name both factors.
    assert!(page.contains("passkey"), "{page}");
    assert!(page.contains("recovery code"), "{page}");
    assert!(page.contains("3 left"), "{page}");
}

#[tokio::test]
async fn while_locked_every_route_serves_the_locked_page_and_no_handler_runs() {
    use axum::routing::get;

    let ran = Arc::new(AtomicBool::new(false));
    let f = Fixture::new();
    let ld = Arc::new(f.fresh());
    ld.engage(Trigger::Operator {
        surface: Surface::Hotkey,
    });

    let hit = ran.clone();
    let app = axum::Router::new()
        .route("/", get(|| async { "workspace" }))
        .route(
            "/commands",
            get(move || {
                let hit = hit.clone();
                async move {
                    hit.store(true, SeqCst);
                    "a command ran"
                }
            }),
        )
        .route("/healthz", get(|| async { "ok" }))
        .route("/unlock", get(|| async { "how to unlock" }))
        // The fallback is added before the layer on purpose: a gate that only
        // wrapped the known routes would let an unknown path through, and an
        // unknown path is exactly what an attacker tries.
        .fallback(|| async { "fallback" })
        .layer(axum::middleware::from_fn_with_state(
            ld.clone(),
            lockdown::gate,
        ));

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    for path in ["/", "/commands", "/ws", "/apps/files", "/unlockme"] {
        let response = fetch(addr, path).await;
        assert!(
            response.starts_with("HTTP/1.1 503"),
            "{path} was not refused: {}",
            response.lines().next().unwrap_or_default()
        );
        assert!(response.contains("Locked down"), "{path}: no locked page");
    }
    assert!(
        !ran.load(SeqCst),
        "a command handler ran while the node was locked down"
    );

    for path in ["/healthz", "/unlock"] {
        let response = fetch(addr, path).await;
        assert!(
            response.starts_with("HTTP/1.1 200"),
            "{path} must keep working: {}",
            response.lines().next().unwrap_or_default()
        );
    }

    server.abort();
}

#[tokio::test]
async fn an_unlocked_node_serves_its_routes_normally() {
    // The other half of the gate: it must be invisible when nothing is
    // locked, or every test above would pass against a server that refuses
    // everything always.
    use axum::routing::get;

    let f = Fixture::new();
    let ld = Arc::new(f.fresh());
    let app = axum::Router::new()
        .route("/", get(|| async { "workspace" }))
        .fallback(|| async { "fallback" })
        .layer(axum::middleware::from_fn_with_state(
            ld.clone(),
            lockdown::gate,
        ));

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    let response = fetch(addr, "/").await;
    assert!(response.starts_with("HTTP/1.1 200"), "{response}");
    assert!(response.contains("workspace"));

    server.abort();
}

/// One request over loopback, read to EOF. `Connection: close` so the read
/// ends with the response rather than waiting on keep-alive.
async fn fetch(addr: std::net::SocketAddr, path: &str) -> String {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
    stream
        .write_all(
            format!("GET {path} HTTP/1.1\r\nHost: deckjack.test\r\nConnection: close\r\n\r\n")
                .as_bytes(),
        )
        .await
        .unwrap();
    let mut bytes = Vec::new();
    stream.read_to_end(&mut bytes).await.unwrap();
    String::from_utf8_lossy(&bytes).into_owned()
}

// ---- secrets and shape --------------------------------------------------

#[test]
fn nothing_that_could_unlock_a_node_has_a_debug_impl_that_shows_it() {
    let random = CountingRandom {
        next: Arc::new(AtomicU8::new(7)),
    };
    let key = SessionKey::generate(&random).unwrap();
    let code = RecoveryCode::generate(&random).unwrap();
    let identity = Identity::Passkey(PasskeyAssertion::new(b"assertion-bytes".to_vec()));
    let second = SecondFactor::RecoveryCode(code.reveal());

    assert_eq!(format!("{key:?}"), "SessionKey(<redacted>)");
    assert_eq!(format!("{code:?}"), "RecoveryCode(<redacted>)");
    assert_eq!(format!("{identity:?}"), "Identity::Passkey(<redacted>)");
    assert_eq!(
        format!("{second:?}"),
        "SecondFactor::RecoveryCode(<redacted>)"
    );

    // The bytes themselves must not appear anywhere in those lines.
    let all = format!("{key:?}{code:?}{identity:?}{second:?}");
    assert!(!all.contains(&code.reveal()));
    assert!(!all.contains("assertion-bytes"));
    assert!(!all.contains(&format!("{:?}", key.reveal())));
}

#[test]
fn a_report_carries_nothing_that_cannot_be_logged() {
    // `engage` logs the whole report, and the journal persists it.
    let mut f = Fixture::new();
    f.kids = vec![child(4242, ChildKind::Terminal)];
    f.alive_calls = usize::MAX;
    let ld = f.fresh();

    let report = ld.engage(Trigger::HostCommand);
    let rendered = format!("{report:?}");

    assert!(rendered.contains("4242"), "pids are the point: {rendered}");
    assert!(!rendered.contains("redacted"), "{rendered}");
    assert_eq!(
        report,
        serde_json::from_str::<Report>(&serde_json::to_string(&report).unwrap()).unwrap()
    );
}

#[test]
fn lockdown_is_send_and_sync_so_the_gate_and_the_bus_can_share_one() {
    fn needs_send_sync<T: Send + Sync + 'static>() {}
    needs_send_sync::<Lockdown>();
    needs_send_sync::<Watchdog>();
    // `middleware::from_fn_with_state` wants the state cloneable and shared.
    needs_send_sync::<Arc<Lockdown>>();
}

#[test]
fn the_two_command_ids_are_the_ones_the_rest_of_the_system_will_name() {
    assert_eq!(lockdown::CMD_ENGAGE, "system.lockdown");
    assert_eq!(lockdown::CMD_UNLOCK, "system.unlock");
}

// ---------------------------------------------------------------------------
// Step 4, over the bus
// ---------------------------------------------------------------------------

/// A lockdown holds agents on the BUS, not just on the router.
///
/// `lockdown::gate` is axum middleware, so it covers every HTTP and ws route
/// and nothing else. An app invokes bus commands over its own Unix socket —
/// `apphost`'s `Invocations::begin` into `AgentBridge::invoke_for_app` — and
/// never passes through that middleware, so the bus has to ask the lockdown
/// itself.
///
/// Built through `wiring::bus` ON PURPOSE. The defect this catches is a
/// missing wiring line, and a bus assembled by hand in this file would pass
/// with that line gone — which is exactly how this feature came to be fully
/// implemented, fully unit-tested and never constructed.
///
/// No command is registered, and that is the assertion's sharpest edge:
/// `Bus::plan` checks the hold BEFORE it touches the registry, so a locked
/// node answers `AgentsPaused` where an unlocked one answers
/// `UnknownCommand`. Two different refusals, no probe command needed, and no
/// leak of which commands exist to a locked-out caller.
#[test]
fn a_lockdown_holds_agents_on_the_bus_and_unlocking_releases_them() {
    use deckjack_core::command::{
        Approvals as BusApprovals, CommandId, Grants as BusGrants, Issuer, Journal as BusJournal,
        NoGrants, NoInbox, Refused, Registry, Request, TracingJournal,
    };
    use deckjack_core::wiring;

    let mut f = Fixture::new();
    f.passkeys = Passkeys::Accept;
    let lockdown = Arc::new(f.fresh());
    // Minted while unlocked: `issue_recovery_codes` refuses from the locked
    // state, so the way out has to exist before the way in is taken.
    let codes = lockdown
        .issue_recovery_codes(1)
        .expect("codes issue while unlocked");

    let bus = wiring::bus(wiring::BusParts {
        registry: Arc::new(Registry::new()),
        node_id: "node-test".to_string(),
        journal: Arc::new(TracingJournal) as Arc<dyn BusJournal>,
        watchdog: Arc::new(Watchdog::new(AutoPolicy::default())),
        lockdown: Arc::clone(&lockdown),
        grants: Arc::new(NoGrants) as Arc<dyn BusGrants>,
        approvals: Arc::new(NoInbox) as Arc<dyn BusApprovals>,
        halt: Arc::new(wiring::LockdownHalt(Arc::clone(&lockdown))),
    });

    // An app's invocation, shaped the way `agentbridge::app_identity` shapes
    // it: an `Issuer::Agent` whose session is the reserved `app:<name>`,
    // arriving over a socket the HTTP gate never sees.
    let from_an_app = || {
        Request::new(
            CommandId::new("probe.read").expect("a valid command id"),
            serde_json::json!({}),
            Issuer::agent("app:notes"),
        )
    };

    assert!(
        matches!(
            bus.dispatch(from_an_app())
                .expect_err("no command is registered"),
            Refused::UnknownCommand(_)
        ),
        "unlocked, an agent dispatch should get as far as the registry; if it \
         does not, the rest of this test proves nothing"
    );

    lockdown.engage(Trigger::HostCommand);
    assert!(
        matches!(
            bus.dispatch(from_an_app())
                .expect_err("this node is locked"),
            Refused::AgentsPaused
        ),
        "a lockdown must stop an app's dispatch, and stop it before the \
         registry is touched"
    );

    // The operator's lever cannot release the lockdown's hold. Structural,
    // not a conditional refusal: `agents_paused` reads both.
    bus.resume_agents();
    assert!(
        matches!(
            bus.dispatch(from_an_app()).expect_err("still locked"),
            Refused::AgentsPaused
        ),
        "resume_agents released a hold it does not own"
    );

    // The operator is a different axis. Lockdown revokes their sessions and
    // the HTTP gate serves them the locked page; the BUS does not refuse
    // them by issuer, and conflating the two would hide which lever did it.
    assert!(
        matches!(
            bus.dispatch(Request::new(
                CommandId::new("probe.read").expect("a valid command id"),
                serde_json::json!({}),
                Issuer::operator(),
            ))
            .expect_err("no command is registered"),
            Refused::UnknownCommand(_)
        ),
        "holding agents must not be conflated with stopping the operator"
    );

    lockdown
        .unlock(UnlockRequest {
            identity: Identity::HostConsole,
            second: SecondFactor::RecoveryCode(codes[0].reveal()),
        })
        .expect("the host console plus a recovery code unlocks");

    assert!(
        matches!(
            bus.dispatch(from_an_app())
                .expect_err("no command is registered"),
            Refused::UnknownCommand(_)
        ),
        "unlocking must release agents: a lockdown you cannot leave is worse \
         than one that does not engage"
    );
}
