//! `deckjack --debug`: what it gives a tool, and what it refuses to give.

use std::net::{IpAddr, SocketAddr};
use std::time::Duration;

use deckjack_core::auth::session::from_cookie_headers;
use deckjack_core::auth::state::VerifyOutcome;
use deckjack_core::auth::{AuthState, Code, Limits, Now};
use deckjack_core::debugmode::{self, COOKIE_FILE, DebugError};

fn state() -> AuthState {
    AuthState::new(
        Limits {
            per_ip_attempts: 3,
            global_attempts: 10,
            session_max: Duration::from_secs(3600),
            max_sessions: 4,
        },
        Code::generate().unwrap(),
        Now::current(),
    )
}

#[test]
fn debug_mode_listens_on_loopback_and_nowhere_else() {
    for ok in ["127.0.0.1:8777", "[::1]:8777"] {
        debugmode::check_listen(ok.parse().unwrap()).unwrap_or_else(|e| panic!("{ok}: {e}"));
    }
    for refused in [
        "0.0.0.0:8777",
        "[::]:8777",
        "192.168.1.5:8777",
        "100.64.0.1:8777",
    ] {
        let addr: SocketAddr = refused.parse().unwrap();
        assert!(
            matches!(
                debugmode::check_listen(addr),
                Err(DebugError::NotLoopback(_))
            ),
            "{refused} was allowed"
        );
    }
}

#[test]
fn the_debug_session_signs_in_from_loopback_only() {
    let mut st = state();
    let (id, _) = debugmode::mint(&mut st, Now::current()).unwrap();
    let local: IpAddr = "127.0.0.1".parse().unwrap();
    let elsewhere: IpAddr = "100.64.0.9".parse().unwrap();
    assert_eq!(
        st.verify(local, Some(&id), Now::current()),
        VerifyOutcome::Ok
    );
    assert_eq!(
        st.verify(elsewhere, Some(&id), Now::current()),
        VerifyOutcome::NoSession,
        "the debug session worked from another address"
    );
}

#[test]
fn the_debug_session_does_not_spend_the_sign_in_code() {
    // The point of it: a tool working on the node must not use up the code
    // the operator is about to type.
    let mut st = state();
    // Compared by value: `Code`'s Debug is redacted, so its text would match
    // even if the code had been rotated.
    let before = st.code().clone();
    debugmode::mint(&mut st, Now::current()).unwrap();
    assert_eq!(st.code(), &before);
    assert_eq!(st.code().label(), "active");
}

#[test]
fn a_lockdown_revokes_the_debug_session_like_any_other() {
    let mut st = state();
    let (id, _) = debugmode::mint(&mut st, Now::current()).unwrap();
    st.kick();
    let local: IpAddr = "127.0.0.1".parse().unwrap();
    assert_eq!(
        st.verify(local, Some(&id), Now::current()),
        VerifyOutcome::NoSession
    );
}

#[test]
fn the_cookie_file_is_the_owners_alone_and_holds_a_usable_header() {
    use std::os::unix::fs::PermissionsExt as _;
    let dir = tempfile::tempdir().unwrap();
    // A leftover file with loose permissions must not keep them.
    std::fs::write(dir.path().join(COOKIE_FILE), "stale").unwrap();
    std::fs::set_permissions(
        dir.path().join(COOKIE_FILE),
        std::fs::Permissions::from_mode(0o644),
    )
    .unwrap();

    let mut st = state();
    let (id, _) = debugmode::mint(&mut st, Now::current()).unwrap();
    let path = debugmode::write_cookie(dir.path(), &id).unwrap();

    let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600, "the debug cookie is readable by others");
    let text = std::fs::read_to_string(&path).unwrap();
    let parsed = from_cookie_headers(false, [text.trim()]).expect("not a usable Cookie header");
    assert_eq!(parsed.encode(), id.encode());
}

#[test]
fn a_normal_start_removes_a_leftover_cookie() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join(COOKIE_FILE), "dj_session=old").unwrap();
    debugmode::remove_cookie(dir.path()).unwrap();
    assert!(!dir.path().join(COOKIE_FILE).exists());
    // And nothing there is not an error.
    debugmode::remove_cookie(dir.path()).unwrap();
}
