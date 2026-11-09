//! Auth state and its transitions (§9.1, §9.4).
//!
//! Every transition is a plain method that takes the current time (and any
//! fresh random values) as arguments, so the whole state machine is tested
//! without a server, a clock or an RNG. Side effects (nftables, events,
//! logging) are the caller's job, driven by the returned outcome.

use std::collections::{HashMap, HashSet};
use std::net::IpAddr;
use std::time::{Duration, Instant, SystemTime};

use super::Event;
use super::code::Code;
use super::session::{Session, SessionId};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    pub per_ip_attempts: u32,
    pub global_attempts: u32,
    pub session_max: Duration,
    pub max_sessions: usize,
}

// NOTE (harvest): pocketdock had `Limits::from_config(&config::Auth)` here. It
// was the only place config leaked into this module and nothing in the harvest
// called it, so it is dropped until Deckjack's own config exists. `Limits` is a
// plain struct; build it directly. See docs/PORTING.md.

/// A point in time on both clocks: wall time for session expiry (shown to
/// the user), monotonic time for idle tracking.
#[derive(Debug, Clone, Copy)]
pub struct Now {
    pub wall: SystemTime,
    pub mono: Instant,
}

impl Now {
    pub fn current() -> Now {
        Now {
            wall: SystemTime::now(),
            mono: Instant::now(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CodeState {
    Active(Code),
    Consumed,
    None,
}

impl CodeState {
    pub fn label(&self) -> &'static str {
        match self {
            CodeState::Active(_) => "active",
            CodeState::Consumed => "consumed",
            CodeState::None => "none",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    IpBlocked,
    GlobalLock,
}

#[derive(Debug, PartialEq, Eq)]
pub enum LoginOutcome {
    /// Logged in. `revoked` older sessions were dropped to honor `max_sessions`.
    Success { revoked: usize },
    /// Refused without counting the attempt.
    Refused(Refusal),
    /// Wrong code; this many attempts remain before a lock.
    Failed { left: u32 },
    /// Wrong code, and a threshold was reached on this attempt.
    Locked { ip_blocked: bool, global_lock: bool },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VerifyOutcome {
    Ok,
    Blocked,
    NoSession,
}

/// What `/auth/verify` answers (§9.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VerifyResponse {
    Allow,           // 200
    Forbidden,       // 403
    Unauthorized,    // 401
    RedirectToLogin, // 302 → /auth/login
}

/// Map a verify outcome to a response. WebSocket and `/websockify` requests
/// get a plain 401 because a redirect is meaningless to them.
pub fn verify_response(
    outcome: VerifyOutcome,
    forwarded_uri: &str,
    is_upgrade: bool,
) -> VerifyResponse {
    match outcome {
        VerifyOutcome::Ok => VerifyResponse::Allow,
        VerifyOutcome::Blocked => VerifyResponse::Forbidden,
        VerifyOutcome::NoSession if is_upgrade || forwarded_uri.starts_with("/websockify") => {
            VerifyResponse::Unauthorized
        }
        VerifyOutcome::NoSession => VerifyResponse::RedirectToLogin,
    }
}

#[derive(Debug)]
pub struct AuthState {
    limits: Limits,
    code: CodeState,
    sessions: HashMap<SessionId, Session>,
    failures_by_ip: HashMap<IpAddr, u32>,
    blocked: HashSet<IpAddr>,
    failures_total: u32,
    global_lock: bool,
    last_activity: Instant,
}

impl AuthState {
    pub fn new(limits: Limits, code: Code, now: Now) -> AuthState {
        AuthState {
            limits,
            code: CodeState::Active(code),
            sessions: HashMap::new(),
            failures_by_ip: HashMap::new(),
            blocked: HashSet::new(),
            failures_total: 0,
            global_lock: false,
            last_activity: now.mono,
        }
    }

    /// Put a session back as it was, when state is restored from the store
    /// at startup. Deliberately separate from `login`: it consumes no code,
    /// counts no attempt and revokes nothing, because the session already
    /// happened — this is only remembering it.
    /// One session by id, as stored.
    pub fn session(&self, id: &SessionId) -> Option<Session> {
        self.sessions.get(id).cloned()
    }

    pub fn restore_session(&mut self, id: SessionId, session: Session) {
        self.sessions.insert(id, session);
    }

    pub fn limits(&self) -> &Limits {
        &self.limits
    }

    pub fn code(&self) -> &CodeState {
        &self.code
    }

    pub fn is_blocked(&self, ip: IpAddr) -> bool {
        self.blocked.contains(&ip)
    }

    pub fn global_lock(&self) -> bool {
        self.global_lock
    }

    pub fn failures_total(&self) -> u32 {
        self.failures_total
    }

    pub fn failures_by_ip(&self) -> &HashMap<IpAddr, u32> {
        &self.failures_by_ip
    }

    pub fn blocked(&self) -> &HashSet<IpAddr> {
        &self.blocked
    }

    /// Unexpired sessions, oldest first.
    pub fn sessions(&self, now: SystemTime) -> Vec<&Session> {
        let mut v: Vec<_> = self.sessions.values().filter(|s| now < s.expires).collect();
        v.sort_by_key(|s| s.created);
        v
    }

    /// Why a login from `ip` would be refused outright, if it would.
    pub fn refusal(&self, ip: IpAddr) -> Option<Refusal> {
        if self.blocked.contains(&ip) {
            Some(Refusal::IpBlocked)
        } else if self.global_lock {
            Some(Refusal::GlobalLock)
        } else {
            None
        }
    }

    /// Attempt a login. `new_session` is used only on success.
    pub fn login(
        &mut self,
        ip: IpAddr,
        input: &str,
        new_session: SessionId,
        now: Now,
        events: &mut Vec<Event>,
    ) -> LoginOutcome {
        if let Some(r) = self.refusal(ip) {
            return LoginOutcome::Refused(r);
        }
        let ok = match &self.code {
            CodeState::Active(code) => code.matches(input),
            CodeState::Consumed | CodeState::None => false,
        };
        if ok {
            self.code = CodeState::Consumed;
            self.failures_by_ip.remove(&ip);
            self.prune(now.wall);
            let mut revoked = 0;
            while self.sessions.len() >= self.limits.max_sessions {
                let oldest = self
                    .sessions
                    .iter()
                    .min_by_key(|(_, s)| s.created)
                    .map(|(id, _)| id.clone());
                match oldest {
                    Some(id) => {
                        self.sessions.remove(&id);
                        revoked += 1;
                    }
                    None => break,
                }
            }
            self.sessions.insert(
                new_session,
                Session {
                    ip,
                    created: now.wall,
                    expires: now.wall + self.limits.session_max,
                },
            );
            self.last_activity = now.mono;
            events.push(Event::Login { ip });
            return LoginOutcome::Success { revoked };
        }

        let per_ip = self.failures_by_ip.entry(ip).or_insert(0);
        *per_ip = per_ip.saturating_add(1);
        let per_ip = *per_ip;
        self.failures_total = self.failures_total.saturating_add(1);

        let ip_blocked = per_ip >= self.limits.per_ip_attempts;
        let global_lock = self.failures_total >= self.limits.global_attempts;
        if ip_blocked {
            self.blocked.insert(ip);
            self.sessions.retain(|_, s| s.ip != ip);
            events.push(Event::IpBlocked { ip });
        }
        if global_lock && !self.global_lock {
            self.global_lock = true;
            events.push(Event::GlobalLock);
        }
        if ip_blocked || global_lock {
            return LoginOutcome::Locked {
                ip_blocked,
                global_lock,
            };
        }
        let left = (self.limits.per_ip_attempts - per_ip)
            .min(self.limits.global_attempts - self.failures_total);
        events.push(Event::Failed { ip, left });
        LoginOutcome::Failed { left }
    }

    /// Check a request for `/auth/verify`. Marks activity on success.
    pub fn verify(&mut self, ip: IpAddr, session: Option<&SessionId>, now: Now) -> VerifyOutcome {
        if self.blocked.contains(&ip) {
            return VerifyOutcome::Blocked;
        }
        let valid = session
            .and_then(|id| self.sessions.get(id))
            .is_some_and(|s| s.is_valid_for(ip, now.wall));
        if valid {
            self.last_activity = now.mono;
            VerifyOutcome::Ok
        } else {
            VerifyOutcome::NoSession
        }
    }

    /// Revoke one session. Returns whether it existed.
    pub fn logout(&mut self, session: &SessionId) -> bool {
        self.sessions.remove(session).is_some()
    }

    /// Replace the code (`code --new`).
    pub fn rotate_code(&mut self, code: Code) {
        self.code = CodeState::Active(code);
    }

    /// Clear failures, the blocklist and the global lock, and set a new code.
    /// Sessions are kept.
    pub fn reset(&mut self, code: Code) {
        self.failures_by_ip.clear();
        self.blocked.clear();
        self.failures_total = 0;
        self.global_lock = false;
        self.code = CodeState::Active(code);
    }

    /// Revoke every session. Returns how many there were.
    pub fn kick(&mut self) -> usize {
        let n = self.sessions.len();
        self.sessions.clear();
        n
    }

    pub fn idle(&self, now: Now) -> Duration {
        now.mono.saturating_duration_since(self.last_activity)
    }

    /// Drop expired sessions.
    pub fn prune(&mut self, now: SystemTime) {
        self.sessions.retain(|_, s| now < s.expires);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CODE: &str = "K7QM-3XWD";

    fn limits() -> Limits {
        Limits {
            per_ip_attempts: 3,
            global_attempts: 10,
            session_max: Duration::from_secs(12 * 3600),
            max_sessions: 1,
        }
    }

    fn t0() -> Now {
        Now {
            wall: SystemTime::UNIX_EPOCH + Duration::from_secs(1_800_000_000),
            mono: base_instant(),
        }
    }

    /// One fixed `Instant` so every `t0()` call is the same point in time.
    fn base_instant() -> Instant {
        static BASE: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
        *BASE.get_or_init(Instant::now)
    }

    fn later(now: Now, secs: u64) -> Now {
        Now {
            wall: now.wall + Duration::from_secs(secs),
            mono: now.mono + Duration::from_secs(secs),
        }
    }

    fn ip(last: u8) -> IpAddr {
        IpAddr::from([10, 77, 0, last])
    }

    fn sid(n: u8) -> SessionId {
        SessionId::from_bytes([n; 32])
    }

    fn state() -> AuthState {
        AuthState::new(limits(), Code::parse(CODE).unwrap(), t0())
    }

    fn login(s: &mut AuthState, ip: IpAddr, input: &str, id: u8, now: Now) -> LoginOutcome {
        s.login(ip, input, sid(id), now, &mut Vec::new())
    }

    #[test]
    fn successful_login_consumes_code() {
        let mut s = state();
        let mut ev = Vec::new();
        let out = s.login(ip(2), "k7qm3xwd", sid(1), t0(), &mut ev);
        assert_eq!(out, LoginOutcome::Success { revoked: 0 });
        assert_eq!(ev, vec![Event::Login { ip: ip(2) }]);
        assert_eq!(s.code(), &CodeState::Consumed);
        assert_eq!(s.verify(ip(2), Some(&sid(1)), t0()), VerifyOutcome::Ok);
    }

    #[test]
    fn three_failures_block_ip() {
        let mut s = state();
        let mut ev = Vec::new();
        assert_eq!(
            s.login(ip(2), "WRONG000", sid(1), t0(), &mut ev),
            LoginOutcome::Failed { left: 2 }
        );
        assert_eq!(
            s.login(ip(2), "WRONG000", sid(1), t0(), &mut ev),
            LoginOutcome::Failed { left: 1 }
        );
        assert_eq!(
            s.login(ip(2), "WRONG000", sid(1), t0(), &mut ev),
            LoginOutcome::Locked {
                ip_blocked: true,
                global_lock: false
            }
        );
        assert_eq!(
            ev,
            vec![
                Event::Failed { ip: ip(2), left: 2 },
                Event::Failed { ip: ip(2), left: 1 },
                Event::IpBlocked { ip: ip(2) },
            ]
        );
        assert!(s.is_blocked(ip(2)));
    }

    #[test]
    fn blocked_ip_refused_even_with_correct_code_and_not_counted() {
        let mut s = state();
        for _ in 0..3 {
            login(&mut s, ip(2), "bad", 1, t0());
        }
        let total = s.failures_total();
        let mut ev = Vec::new();
        assert_eq!(
            s.login(ip(2), CODE, sid(1), t0(), &mut ev),
            LoginOutcome::Refused(Refusal::IpBlocked)
        );
        assert!(ev.is_empty());
        assert_eq!(s.failures_total(), total);
        assert_eq!(s.failures_by_ip()[&ip(2)], 3);
        assert!(matches!(s.code(), CodeState::Active(_)));
    }

    #[test]
    fn ips_counted_independently() {
        let mut s = state();
        login(&mut s, ip(2), "bad", 1, t0());
        login(&mut s, ip(2), "bad", 1, t0());
        assert_eq!(
            login(&mut s, ip(3), "bad", 1, t0()),
            LoginOutcome::Failed { left: 2 }
        );
        assert!(!s.is_blocked(ip(2)));
        assert!(!s.is_blocked(ip(3)));
        assert_eq!(s.failures_total(), 3);
    }

    #[test]
    fn ten_failures_trigger_global_lock() {
        let mut s = state();
        // 3 IPs × 3 failures = 9; each IP ends up blocked.
        for last in 2..5 {
            for _ in 0..3 {
                login(&mut s, ip(last), "bad", 1, t0());
            }
        }
        assert!(!s.global_lock());
        let mut ev = Vec::new();
        assert_eq!(
            s.login(ip(5), "bad", sid(1), t0(), &mut ev),
            LoginOutcome::Locked {
                ip_blocked: false,
                global_lock: true
            }
        );
        assert_eq!(ev, vec![Event::GlobalLock]);
        assert!(s.global_lock());
    }

    #[test]
    fn attempts_left_accounts_for_global_budget() {
        let mut s = state();
        for last in 2..5 {
            login(&mut s, ip(last), "bad", 1, t0());
            login(&mut s, ip(last), "bad", 1, t0());
        }
        // total 6; global budget 4 left, per-IP 2 left after one failure from a new IP.
        login(&mut s, ip(9), "bad", 1, t0());
        login(&mut s, ip(9), "bad", 1, t0()); // total 8
        assert_eq!(
            login(&mut s, ip(10), "bad", 1, t0()),
            LoginOutcome::Failed { left: 1 }
        );
    }

    #[test]
    fn global_lock_refuses_fresh_ip_with_correct_code() {
        let mut s = state();
        for i in 0..10u8 {
            login(&mut s, ip(20 + i), "bad", 1, t0());
        }
        assert!(s.global_lock());
        assert_eq!(
            login(&mut s, ip(99), CODE, 1, t0()),
            LoginOutcome::Refused(Refusal::GlobalLock)
        );
    }

    #[test]
    fn global_lock_keeps_existing_sessions() {
        let mut s = state();
        login(&mut s, ip(2), CODE, 1, t0());
        for i in 0..10u8 {
            login(&mut s, ip(20 + i), "bad", 1, t0());
        }
        assert!(s.global_lock());
        assert_eq!(s.verify(ip(2), Some(&sid(1)), t0()), VerifyOutcome::Ok);
    }

    #[test]
    fn blocking_revokes_sessions_from_that_ip() {
        let mut lim = limits();
        lim.max_sessions = 5;
        let mut s = AuthState::new(lim, Code::parse(CODE).unwrap(), t0());
        login(&mut s, ip(2), CODE, 1, t0());
        for _ in 0..3 {
            login(&mut s, ip(2), "bad", 1, t0());
        }
        assert_eq!(s.verify(ip(2), Some(&sid(1)), t0()), VerifyOutcome::Blocked);
        assert!(s.sessions(t0().wall).is_empty());
    }

    #[test]
    fn reset_clears_everything_but_sessions() {
        let mut s = state();
        login(&mut s, ip(2), CODE, 1, t0());
        for i in 0..10u8 {
            login(&mut s, ip(20 + (i % 4)), "bad", 1, t0());
        }
        assert!(s.global_lock());
        assert!(!s.blocked().is_empty());

        s.reset(Code::parse("AAAABBBB").unwrap());
        assert!(!s.global_lock());
        assert!(s.blocked().is_empty());
        assert!(s.failures_by_ip().is_empty());
        assert_eq!(s.failures_total(), 0);
        assert_eq!(
            s.code(),
            &CodeState::Active(Code::parse("AAAABBBB").unwrap())
        );
        assert_eq!(s.verify(ip(2), Some(&sid(1)), t0()), VerifyOutcome::Ok);
        // The old code no longer works; the new one does.
        assert_eq!(
            login(&mut s, ip(21), CODE, 2, t0()),
            LoginOutcome::Failed { left: 2 }
        );
        assert_eq!(
            login(&mut s, ip(21), "aaaabbbb", 2, t0()),
            LoginOutcome::Success { revoked: 1 }
        );
    }

    #[test]
    fn success_clears_ip_counter_only() {
        let mut s = state();
        login(&mut s, ip(2), "bad", 1, t0());
        login(&mut s, ip(2), "bad", 1, t0());
        assert_eq!(
            login(&mut s, ip(2), CODE, 1, t0()),
            LoginOutcome::Success { revoked: 0 }
        );
        assert!(!s.failures_by_ip().contains_key(&ip(2)));
        assert_eq!(s.failures_total(), 2);
    }

    #[test]
    fn consumed_code_fails_and_counts() {
        let mut s = state();
        login(&mut s, ip(2), CODE, 1, t0());
        assert_eq!(
            login(&mut s, ip(3), CODE, 2, t0()),
            LoginOutcome::Failed { left: 2 }
        );
        assert_eq!(s.failures_total(), 1);
        assert_eq!(
            s.verify(ip(3), Some(&sid(2)), t0()),
            VerifyOutcome::NoSession
        );
    }

    #[test]
    fn new_code_after_consumption_works() {
        let mut s = state();
        login(&mut s, ip(2), CODE, 1, t0());
        s.rotate_code(Code::parse("ZZZZ9999").unwrap());
        assert!(matches!(s.code(), CodeState::Active(_)));
        assert_eq!(
            login(&mut s, ip(2), "zzzz-9999", 2, t0()),
            LoginOutcome::Success { revoked: 1 }
        );
    }

    #[test]
    fn sessions_are_bound_to_ip() {
        let mut s = state();
        login(&mut s, ip(2), CODE, 1, t0());
        assert_eq!(
            s.verify(ip(3), Some(&sid(1)), t0()),
            VerifyOutcome::NoSession
        );
        assert_eq!(
            s.verify(ip(2), Some(&sid(9)), t0()),
            VerifyOutcome::NoSession
        );
        assert_eq!(s.verify(ip(2), None, t0()), VerifyOutcome::NoSession);
    }

    #[test]
    fn sessions_expire() {
        let mut s = state();
        login(&mut s, ip(2), CODE, 1, t0());
        let max = 12 * 3600;
        assert_eq!(
            s.verify(ip(2), Some(&sid(1)), later(t0(), max - 1)),
            VerifyOutcome::Ok
        );
        assert_eq!(
            s.verify(ip(2), Some(&sid(1)), later(t0(), max)),
            VerifyOutcome::NoSession
        );
        assert!(s.sessions(later(t0(), max).wall).is_empty());
    }

    #[test]
    fn max_sessions_revokes_oldest() {
        let mut lim = limits();
        lim.max_sessions = 2;
        let mut s = AuthState::new(lim, Code::parse(CODE).unwrap(), t0());
        login(&mut s, ip(2), CODE, 1, t0());
        s.rotate_code(Code::parse(CODE).unwrap());
        login(&mut s, ip(2), CODE, 2, later(t0(), 10));
        s.rotate_code(Code::parse(CODE).unwrap());
        assert_eq!(
            login(&mut s, ip(2), CODE, 3, later(t0(), 20)),
            LoginOutcome::Success { revoked: 1 }
        );
        let now = later(t0(), 30);
        assert_eq!(
            s.verify(ip(2), Some(&sid(1)), now),
            VerifyOutcome::NoSession
        );
        assert_eq!(s.verify(ip(2), Some(&sid(2)), now), VerifyOutcome::Ok);
        assert_eq!(s.verify(ip(2), Some(&sid(3)), now), VerifyOutcome::Ok);
    }

    #[test]
    fn max_sessions_one_revokes_previous() {
        let mut s = state();
        login(&mut s, ip(2), CODE, 1, t0());
        s.rotate_code(Code::parse(CODE).unwrap());
        assert_eq!(
            login(&mut s, ip(3), CODE, 2, t0()),
            LoginOutcome::Success { revoked: 1 }
        );
        assert_eq!(
            s.verify(ip(2), Some(&sid(1)), t0()),
            VerifyOutcome::NoSession
        );
        assert_eq!(s.verify(ip(3), Some(&sid(2)), t0()), VerifyOutcome::Ok);
    }

    #[test]
    fn kick_and_logout() {
        let mut lim = limits();
        lim.max_sessions = 3;
        let mut s = AuthState::new(lim, Code::parse(CODE).unwrap(), t0());
        login(&mut s, ip(2), CODE, 1, t0());
        s.rotate_code(Code::parse(CODE).unwrap());
        login(&mut s, ip(2), CODE, 2, t0());
        assert!(s.logout(&sid(1)));
        assert!(!s.logout(&sid(1)));
        assert_eq!(s.kick(), 1);
        assert_eq!(
            s.verify(ip(2), Some(&sid(2)), t0()),
            VerifyOutcome::NoSession
        );
    }

    #[test]
    fn verify_updates_activity() {
        let mut s = state();
        login(&mut s, ip(2), CODE, 1, t0());
        assert_eq!(s.idle(later(t0(), 100)), Duration::from_secs(100));
        s.verify(ip(2), Some(&sid(1)), later(t0(), 90));
        assert_eq!(s.idle(later(t0(), 100)), Duration::from_secs(10));
        // A failed verify does not count as activity.
        s.verify(ip(3), Some(&sid(1)), later(t0(), 95));
        assert_eq!(s.idle(later(t0(), 100)), Duration::from_secs(10));
    }

    #[test]
    fn verify_routing() {
        use VerifyOutcome::*;
        use VerifyResponse::*;
        assert_eq!(verify_response(Ok, "/", false), Allow);
        assert_eq!(verify_response(NoSession, "/", false), RedirectToLogin);
        assert_eq!(
            verify_response(NoSession, "/files/x", false),
            RedirectToLogin
        );
        assert_eq!(
            verify_response(NoSession, "/websockify", false),
            Unauthorized
        );
        assert_eq!(
            verify_response(NoSession, "/websockify?x=1", false),
            Unauthorized
        );
        assert_eq!(verify_response(NoSession, "/other", true), Unauthorized);
        assert_eq!(verify_response(Blocked, "/", false), Forbidden);
        assert_eq!(verify_response(Blocked, "/websockify", true), Forbidden);
    }

    #[test]
    fn ipv6_clients_blocked_in_app() {
        let mut s = state();
        let v6: IpAddr = "fe80::1".parse().unwrap();
        for _ in 0..3 {
            login(&mut s, v6, "bad", 1, t0());
        }
        assert!(s.is_blocked(v6));
        assert_eq!(
            login(&mut s, v6, CODE, 1, t0()),
            LoginOutcome::Refused(Refusal::IpBlocked)
        );
    }
}
