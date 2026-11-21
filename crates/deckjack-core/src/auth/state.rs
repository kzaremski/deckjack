//! Auth state and its transitions (§9.1, §9.4).
//!
//! Every transition is a plain method that takes the current time (and any
//! fresh random values) as arguments, so the whole state machine is tested
//! without a server, a clock or an RNG. Side effects (nftables, events,
//! logging) are the caller's job, driven by the returned outcome.

use std::collections::{HashMap, HashSet};
use std::net::IpAddr;
use std::time::{Duration, Instant, SystemTime};

use super::code::Code;
use super::session::{Session, SessionHash, SessionId};
use super::{Event, RevokeReason};

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
    /// Keyed by [`SessionHash`], not by the id itself.
    ///
    /// The id is a bearer credential, and this map is what gets persisted and
    /// restored, so holding the id here would put it on disk. Lookups hash
    /// the presented cookie and compare digests. The entropy is 256 bits, so
    /// the hash-map lookup needs no constant-time comparison: there is
    /// nothing an attacker could narrow down by timing it.
    sessions: HashMap<SessionHash, Session>,
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
        self.sessions.get(&id.hash()).cloned()
    }

    /// Put a session back from the store, which only ever had its hash.
    pub fn restore_session(&mut self, hash: SessionHash, session: Session) {
        self.sessions.insert(hash, session);
    }

    /// Mint a session for a credential that has already been proven
    /// somewhere else — today, a passkey assertion — enforcing both of
    /// [`AuthState::login`]'s rules: the address must not be refused, and
    /// `max_sessions` is honored by evicting rather than by refusing.
    ///
    /// Deliberately not [`AuthState::restore_session`], which is for
    /// remembering a session that already happened and so counts nothing and
    /// evicts nothing. This is a new session, so the cap applies.
    ///
    /// That direction is the whole point. `passkeyhttp::PasskeyCtx::admit`
    /// used to call `restore_session` and refuse with `SessionsFull`, which
    /// the route turned into a 503, while the one-time code path quietly
    /// evicted — two sign-in paths with two different answers to "the node
    /// is full", and no test could see the disagreement because neither
    /// path's tests knew the other existed. Refusing is also the worse
    /// answer on its own: a passkey is the credential the operator actually
    /// holds, and four forgotten tabs would lock them out for up to
    /// `session_max`, 24 hours by default.
    ///
    /// The [`AuthState::refusal`] check is here and not only at the route,
    /// because the route checks it before spending the ceremony and this is
    /// what makes the check and the eviction one step. Without it, an
    /// address blocked during the ceremony would not merely slip one extra
    /// session past the gate — it would destroy a live one on the way in.
    ///
    /// Consumes no code and counts no attempt: the proof happened elsewhere.
    /// Every eviction is announced as [`Event::SessionRevoked`] for the same
    /// reason `login`'s are — the caller owes the dropped session a deleted
    /// row, a reaped pty and an audit entry.
    pub fn admit(
        &mut self,
        ip: IpAddr,
        new_session: &SessionId,
        now: Now,
        events: &mut Vec<Event>,
    ) -> Result<Session, Refusal> {
        if let Some(r) = self.refusal(ip) {
            return Err(r);
        }
        self.expire(now.wall, events);
        self.evict_to_fit(RevokeReason::SessionsFull, events);
        let session = Session::new(ip, now.wall, now.wall + self.limits.session_max);
        self.sessions.insert(new_session.hash(), session.clone());
        // Signing in with a passkey is a person at a keyboard, so it marks
        // activity exactly as `login` does. The path this replaced went
        // through `restore_session` and touched nothing, which left
        // `last_activity` reading as though the node had been idle since the
        // previous sign-in — and `idle()` is what the auto-lockdown policy
        // reads, so a passkey sign-in could be followed by a lockdown for
        // inactivity while somebody was using the node.
        self.last_activity = now.mono;
        Ok(session)
    }

    /// Drop sessions until one more fits under `max_sessions`, announcing
    /// each as [`Event::SessionRevoked`]. Returns how many went.
    ///
    /// ONE copy of this rule, called by both [`AuthState::login`] and
    /// [`AuthState::admit`]. A second copy is precisely how the two sign-in
    /// paths came to disagree.
    ///
    /// The victim is the session seen least recently, **not** the one
    /// created first. `created` would evict the desktop session that has
    /// been open and in use all day in favor of a phone tab opened a minute
    /// ago — and it is the desktop session that owns the ptys
    /// `http::Ctx::session_ended` reaps, so keying on `created` kills running
    /// shells to seat a browser nobody is at. `last_seen` is refreshed by
    /// every [`AuthState::verify`], and `ws::WsCtx::session_ok` re-verifies
    /// an open socket every `session_recheck`, so a live connection is never
    /// the victim and an abandoned cookie always is. `created` breaks ties,
    /// so the choice is deterministic.
    fn evict_to_fit(&mut self, reason: RevokeReason, events: &mut Vec<Event>) -> usize {
        let mut revoked = 0;
        while self.sessions.len() >= self.limits.max_sessions {
            let victim = self
                .sessions
                .iter()
                .min_by_key(|(_, s)| (s.last_seen, s.created))
                .map(|(hash, s)| (*hash, s.ip));
            match victim {
                Some((hash, ip)) => {
                    self.sessions.remove(&hash);
                    revoked += 1;
                    // Announced, not just counted. The caller has to delete
                    // the persisted row and reap what the session owned;
                    // without this it could only learn how many sessions
                    // went, never which, so a revoked session came back at
                    // the next restart and its shell kept running in the
                    // meantime.
                    events.push(Event::SessionRevoked { hash, ip, reason });
                }
                None => break,
            }
        }
        revoked
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
    ///
    /// Ordered by `created`, for an operator looking at a list of devices.
    /// This is **not** the eviction order — that is `last_seen` — so do not
    /// read the head of this list as "the one that goes next".
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
            self.expire(now.wall, events);
            let revoked = self.evict_to_fit(RevokeReason::SessionsFull, events);
            self.sessions.insert(
                new_session.hash(),
                Session::new(ip, now.wall, now.wall + self.limits.session_max),
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
            events.push(Event::IpBlocked { ip });
            // Same reason as the `max_sessions` path above, and this is the
            // one that fires while something hostile is happening: the
            // address is blocked, so its sessions go, so the caller must be
            // told which ones in order to reap their shells.
            let doomed: Vec<SessionHash> = self
                .sessions
                .iter()
                .filter(|(_, s)| s.ip == ip)
                .map(|(hash, _)| *hash)
                .collect();
            for hash in doomed {
                self.sessions.remove(&hash);
                // `ip` is both the blocked address and the revoked session's
                // own, because that is what the filter above selected on.
                events.push(Event::SessionRevoked {
                    hash,
                    ip,
                    reason: RevokeReason::AddressBlocked,
                });
            }
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
        // `get_mut`, because using a session is what makes it recently seen,
        // and `last_seen` is what `evict_to_fit` picks its victim by. Every
        // authenticated path funnels through here — `require_session`,
        // `/auth/verify`, the passkey routes' own session check and
        // `ws::WsCtx::session_ok`'s recheck of an open socket — so this one
        // line is what keeps a live connection off the chopping block.
        let valid = session
            .and_then(|id| self.sessions.get_mut(&id.hash()))
            .filter(|s| s.is_valid_for(ip, now.wall))
            .map(|s| s.last_seen = now.wall)
            .is_some();
        if valid {
            self.last_activity = now.mono;
            VerifyOutcome::Ok
        } else {
            VerifyOutcome::NoSession
        }
    }

    /// Revoke one session. Returns whether it existed.
    pub fn logout(&mut self, session: &SessionId) -> bool {
        self.sessions.remove(&session.hash()).is_some()
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

    /// Revoke every session. Returns the hashes of the ones that went, so
    /// the caller can delete the persisted rows and reap what each owned.
    ///
    /// This is lockdown's step 1. Returning the list rather than a count is
    /// what lets the kill switch be sure: a count would leave the rows in
    /// `state.db`, and the next start would restore every session the
    /// lockdown had just revoked.
    pub fn kick(&mut self) -> Vec<SessionHash> {
        self.sessions.drain().map(|(hash, _)| hash).collect()
    }

    pub fn idle(&self, now: Now) -> Duration {
        now.mono.saturating_duration_since(self.last_activity)
    }

    /// Drop expired sessions, announcing each as [`Event::SessionRevoked`]
    /// with [`RevokeReason::Expired`]. Returns how many went.
    ///
    /// Announced for the reason `evict_to_fit`'s are: the caller owes an
    /// expired session a deleted row and reaped ptys, and `http::Ctx::apply`
    /// does both for exactly this event. Dropping them in silence left every
    /// shell an expired session opened running as the host user, owned by a
    /// session nobody could present again. Run by both sign-in paths and on
    /// a timer by `wiring::spawn_session_expiry`, so a node nobody signs in
    /// to still reaps on time.
    pub fn expire(&mut self, now: SystemTime, events: &mut Vec<Event>) -> usize {
        let expired: Vec<SessionHash> = self
            .sessions
            .iter()
            .filter(|(_, s)| now >= s.expires)
            .map(|(hash, _)| *hash)
            .collect();
        for hash in &expired {
            if let Some(s) = self.sessions.remove(hash) {
                events.push(Event::SessionRevoked {
                    hash: *hash,
                    ip: s.ip,
                    reason: RevokeReason::Expired,
                });
            }
        }
        expired.len()
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
    fn max_sessions_revokes_the_session_seen_least_recently() {
        // Was `max_sessions_revokes_oldest`, and it no longer described the
        // code: nothing called `verify` between the sign-ins, so every
        // session's `last_seen` equalled its `created` and the test could not
        // tell creation order from staleness. It would have gone on passing
        // under either rule. The `verify` below is what makes the two
        // orderings disagree, so this test now pins the one the code uses.
        let mut lim = limits();
        lim.max_sessions = 2;
        let mut s = AuthState::new(lim, Code::parse(CODE).unwrap(), t0());
        login(&mut s, ip(2), CODE, 1, t0());
        s.rotate_code(Code::parse(CODE).unwrap());
        login(&mut s, ip(2), CODE, 2, later(t0(), 10));
        s.rotate_code(Code::parse(CODE).unwrap());

        // Session 1 was created first but is used last, so session 2 is the
        // stale one — the opposite of what creation order would pick.
        assert_eq!(
            s.verify(ip(2), Some(&sid(1)), later(t0(), 15)),
            VerifyOutcome::Ok
        );

        assert_eq!(
            login(&mut s, ip(2), CODE, 3, later(t0(), 20)),
            LoginOutcome::Success { revoked: 1 }
        );
        let now = later(t0(), 30);
        assert_eq!(
            s.verify(ip(2), Some(&sid(2)), now),
            VerifyOutcome::NoSession,
            "the abandoned session is what should have gone"
        );
        assert_eq!(
            s.verify(ip(2), Some(&sid(1)), now),
            VerifyOutcome::Ok,
            "the session that was in use got evicted"
        );
        assert_eq!(s.verify(ip(2), Some(&sid(3)), now), VerifyOutcome::Ok);
    }

    #[test]
    fn eviction_takes_the_least_recently_seen_session_not_the_oldest() {
        // The desktop signs in first and stays in use; the phone tab signs in
        // later and is abandoned. Keying on `created` evicts the desktop —
        // and `http::Ctx::session_ended` reaps its ptys, so that choice kills
        // running shells to seat a browser nobody is at.
        let mut s = AuthState::new(
            Limits {
                max_sessions: 2,
                ..limits()
            },
            Code::parse(CODE).unwrap(),
            t0(),
        );

        let desktop = sid(1);
        assert_eq!(
            s.login(ip(2), "k7qm3xwd", desktop.clone(), t0(), &mut Vec::new()),
            LoginOutcome::Success { revoked: 0 }
        );

        let phone = sid(2);
        s.admit(ip(3), &phone, later(t0(), 10), &mut Vec::new())
            .expect("a node that is not blocked admits a proven credential");

        // The desktop is used again; the phone tab is never touched.
        assert_eq!(
            s.verify(ip(2), Some(&desktop), later(t0(), 20)),
            VerifyOutcome::Ok
        );

        let mut ev = Vec::new();
        let laptop = sid(3);
        s.admit(ip(4), &laptop, later(t0(), 30), &mut ev)
            .expect("a full node evicts instead of refusing");

        assert_eq!(
            ev,
            vec![Event::SessionRevoked {
                hash: phone.hash(),
                ip: ip(3),
                reason: RevokeReason::SessionsFull,
            }],
            "the abandoned tab is what should have gone"
        );
        assert!(
            s.session(&desktop).is_some(),
            "the session that was in use got evicted"
        );
        assert!(s.session(&phone).is_none());
        assert!(s.session(&laptop).is_some());
    }

    #[test]
    fn admitting_on_a_full_node_evicts_exactly_one_instead_of_refusing() {
        // The defect this closes: `passkeyhttp::PasskeyCtx::admit` answered
        // `SessionsFull` here, which the route turned into a 503, while the
        // one-time code path evicted. Two sign-ins, two answers to "the node
        // is full", and neither path's tests knew the other existed.
        let mut s = state(); // max_sessions: 1
        let first = sid(1);
        s.admit(ip(2), &first, t0(), &mut Vec::new())
            .expect("a fresh node admits");

        let mut ev = Vec::new();
        let second = sid(2);
        let session = s
            .admit(ip(3), &second, later(t0(), 5), &mut ev)
            .expect("a proven credential was refused a session on a full node");

        assert_eq!(ev.len(), 1, "exactly one session should have been evicted");
        assert!(matches!(
            ev[0],
            Event::SessionRevoked {
                reason: RevokeReason::SessionsFull,
                ..
            }
        ));
        assert!(s.session(&first).is_none(), "nothing was evicted");
        assert_eq!(s.session(&second).map(|s| s.ip), Some(ip(3)));
        assert_eq!(
            session.last_seen, session.created,
            "a session is seen at the moment it is minted"
        );
        assert_eq!(
            s.sessions(later(t0(), 5).wall).len(),
            1,
            "the cap was exceeded rather than honored"
        );
    }

    #[test]
    fn admitting_from_a_blocked_address_is_refused_and_evicts_nothing() {
        // `admit` applies `refusal` for the same reason `login` does, and
        // the eviction is what makes it matter: the route checks `refusal`
        // before it spends the ceremony, so without a second check inside
        // the lock an address blocked in between would not merely slip one
        // session past the cap — it would destroy a live one on the way in.
        //
        // A passkey assertion is not a guessable secret, so the per-address
        // block is not here to slow down guessing. It is here because the
        // route already refuses a blocked address, and a state machine that
        // disagrees with its own caller is how the 503 came about in the
        // first place. Loosening that policy — letting a passkey be the way
        // back in from a blocked address — is a deliberate change to make
        // somewhere it can be argued, not a side effect of this one.
        let mut s = state(); // max_sessions: 1, per_ip_attempts: 3
        let live = sid(1);
        s.admit(ip(3), &live, t0(), &mut Vec::new())
            .expect("a fresh node admits");

        for n in 1..=3 {
            login(&mut s, ip(2), "wrong", 9, later(t0(), n));
        }
        assert!(s.is_blocked(ip(2)));

        let mut ev = Vec::new();
        assert_eq!(
            s.admit(ip(2), &sid(2), later(t0(), 10), &mut ev),
            Err(Refusal::IpBlocked)
        );
        assert!(ev.is_empty(), "a refused admission revoked something");
        assert!(
            s.session(&live).is_some(),
            "a refused admission evicted a live session"
        );
        assert!(s.session(&sid(2)).is_none());
    }

    #[test]
    fn admitting_marks_activity() {
        // The path this replaced went through `restore_session`, which
        // touched nothing, so `last_activity` still read as of the previous
        // sign-in. `idle()` is what the auto-lockdown policy reads, so a
        // passkey sign-in could be followed by a lockdown for inactivity
        // while somebody was sitting at the node.
        let mut s = state();
        s.admit(ip(2), &sid(1), later(t0(), 50), &mut Vec::new())
            .expect("a fresh node admits");
        assert_eq!(s.idle(later(t0(), 100)), Duration::from_secs(50));
    }

    #[test]
    fn restoring_sessions_at_startup_evicts_nothing() {
        // `restore_session` is remembering, not admitting: the session already
        // happened, so it consumes no code, counts no attempt and revokes
        // nothing. If it ever starts enforcing the cap, a restart silently
        // signs people out and `Ctx::apply` deletes the rows behind them.
        let mut s = state(); // max_sessions: 1
        for n in 1..=3u8 {
            s.restore_session(
                sid(n).hash(),
                Session::new(ip(n), t0().wall, t0().wall + Duration::from_secs(3600)),
            );
        }
        assert_eq!(
            s.sessions(t0().wall).len(),
            3,
            "restoring state at startup must not throw sessions away"
        );
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
        assert_eq!(s.kick(), vec![sid(2).hash()]);
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

    // -- expiry ---------------------------------------------------------------

    #[test]
    fn expiry_announces_each_expired_session_and_keeps_the_live_ones() {
        // An expired session used to vanish from the map in silence, so the
        // caller never deleted its row or reaped its ptys: its shells ran on
        // as the host user with no session left that could reach them.
        let mut s = AuthState::new(
            Limits {
                max_sessions: 4,
                ..limits()
            },
            Code::parse(CODE).unwrap(),
            t0(),
        );
        let old = sid(1);
        s.admit(ip(2), &old, t0(), &mut Vec::new()).unwrap();
        let fresh = sid(2);
        s.admit(ip(3), &fresh, later(t0(), 3600), &mut Vec::new())
            .unwrap();

        let past_the_first = later(t0(), 12 * 3600 + 1).wall;
        let mut ev = Vec::new();
        assert_eq!(s.expire(past_the_first, &mut ev), 1);

        assert_eq!(
            ev,
            vec![Event::SessionRevoked {
                hash: old.hash(),
                ip: ip(2),
                reason: RevokeReason::Expired,
            }]
        );
        assert!(s.session(&old).is_none());
        assert!(s.session(&fresh).is_some(), "a live session was expired");

        let mut again = Vec::new();
        assert_eq!(s.expire(past_the_first, &mut again), 0);
        assert!(again.is_empty(), "an expiry was announced twice");
    }

    #[test]
    fn a_sign_in_that_finds_an_expired_session_announces_it() {
        // The sign-in paths drop expired sessions before counting toward the
        // cap. They have to say which, or the next operator signs in to a
        // node still running the last session's shells.
        let mut s = AuthState::new(
            Limits {
                max_sessions: 4,
                ..limits()
            },
            Code::parse(CODE).unwrap(),
            t0(),
        );
        let old = sid(1);
        s.admit(ip(2), &old, t0(), &mut Vec::new()).unwrap();

        let mut by_passkey = Vec::new();
        s.admit(ip(3), &sid(2), later(t0(), 12 * 3600 + 1), &mut by_passkey)
            .unwrap();
        assert_eq!(
            by_passkey,
            vec![Event::SessionRevoked {
                hash: old.hash(),
                ip: ip(2),
                reason: RevokeReason::Expired,
            }]
        );

        let mut s = AuthState::new(
            Limits {
                max_sessions: 4,
                ..limits()
            },
            Code::parse(CODE).unwrap(),
            t0(),
        );
        s.admit(ip(2), &old, t0(), &mut Vec::new()).unwrap();
        let mut by_code = Vec::new();
        s.login(
            ip(3),
            CODE,
            sid(2),
            later(t0(), 12 * 3600 + 1),
            &mut by_code,
        );
        assert!(
            by_code.contains(&Event::SessionRevoked {
                hash: old.hash(),
                ip: ip(2),
                reason: RevokeReason::Expired,
            }),
            "{by_code:?}"
        );
    }
}
