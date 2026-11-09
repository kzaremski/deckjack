//! What the passkey registry promises, asserted through its public API.
//!
//! Two things shape every test here.
//!
//! **No sleeping.** The module takes its wall clock, its monotonic clock and
//! its randomness from [`Env`], so [`TestEnv`] below moves time by hand and
//! hands out predictable bytes. Nothing in this file waits for anything.
//!
//! **No private key.** A WebAuthn assertion is a signature, and nothing in
//! this workspace can produce one: signing needs an ECDSA implementation, and
//! webauthn-rs keeps its own behind a crate these tests may not add. So the
//! fixtures here forge everything *except* the signature — the relying party
//! id hash is real (`sha2` is already a dependency), the challenge is the one
//! the registry just issued, the flags and the counter are whatever the test
//! needs — and they reach exactly as far as webauthn-rs's signature check
//! before being refused.
//!
//! That is further than it sounds. webauthn-rs checks, in this order: the
//! credential id is one of ours, the client data type, the challenge, the
//! origin, the relying party id hash, user presence, **user verification**,
//! backup consistency, and only then the signature. Everything up to and
//! including user verification is therefore testable here, and
//! `a_fixture_assertion_is_refused_only_at_the_signature` is the control that
//! keeps the other tests honest: it proves the fixture is correct in every
//! respect a test can control, so when another test gets a different refusal,
//! that refusal is the check the test is about and not a broken fixture.
//!
//! What cannot be reached: a *successful* assertion, and therefore the sign
//! count check (webauthn-rs makes it after the signature) and every path that
//! only a success opens — a minted [`StepUpGrant`], a grant expiring, the
//! failure counter being reset. The sign count *rule* is covered directly
//! through [`sign_count_ok`], and the clone alarm's wiring through
//! [`presented_sign_count`], but the end-to-end path needs a soft
//! authenticator as a dev-dependency.

use std::cell::Cell;
use std::time::{Duration, Instant, SystemTime};

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use sha2::{Digest, Sha256};

use deckjack_core::auth::code::RandomError;
use deckjack_core::auth::session::SessionId;
use deckjack_core::passkey::{
    Credential, CredentialId, CredentialRow, Enrollment, Env, Event, HostAccess, MAX_PENDING,
    PasskeyError, Passkeys, Purpose, SessionKind, Settings, UserHandle, presented_sign_count,
    sign_count_ok,
};
use webauthn_rs::prelude::{Passkey, PublicKeyCredential, WebauthnError};

// ---------------------------------------------------------------------------
// The injected world
// ---------------------------------------------------------------------------

/// Both clocks and the random source, under the test's control.
///
/// The two clocks move independently on purpose: the module promises that
/// every deadline is monotonic, and the only way to show that is to move one
/// clock without the other.
struct TestEnv {
    wall: Cell<SystemTime>,
    mono: Cell<Instant>,
    /// Bumped on every fill, so two ceremonies never get the same id.
    nonce: Cell<u8>,
}

impl TestEnv {
    fn new() -> TestEnv {
        TestEnv {
            wall: Cell::new(SystemTime::UNIX_EPOCH + Duration::from_secs(1_767_225_600)),
            mono: Cell::new(Instant::now()),
            nonce: Cell::new(1),
        }
    }

    /// Time passing as it normally does: both clocks together.
    fn advance(&self, d: Duration) {
        self.mono.set(self.mono.get() + d);
        self.wall.set(self.wall.get() + d);
    }

    /// Someone moving the host's clock. The monotonic clock does not care.
    fn set_wall(&self, t: SystemTime) {
        self.wall.set(t);
    }
}

impl Env for TestEnv {
    fn now(&self) -> SystemTime {
        self.wall.get()
    }

    fn mono(&self) -> Instant {
        self.mono.get()
    }

    fn fill_random(&self, buf: &mut [u8]) -> Result<(), RandomError> {
        let n = self.nonce.get();
        self.nonce.set(n.wrapping_add(1));
        for (i, b) in buf.iter_mut().enumerate() {
            *b = n ^ (i as u8);
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

/// A real point on SECP256R1, borrowed from webauthn-rs's own test data. The
/// tests never verify a signature against it, but a malformed key would be
/// refused by the key parser instead of by the check under test.
const KEY_X: [u8; 32] = [
    194, 126, 127, 109, 252, 23, 131, 21, 252, 6, 223, 99, 44, 254, 140, 27, 230, 17, 94, 5, 133,
    28, 104, 41, 144, 69, 171, 149, 161, 26, 200, 243,
];
const KEY_Y: [u8; 32] = [
    143, 123, 183, 156, 24, 178, 21, 248, 117, 159, 162, 69, 171, 52, 188, 252, 26, 59, 6, 47, 103,
    92, 19, 58, 117, 103, 249, 0, 219, 8, 95, 196,
];

const UP: u8 = 0x01;
const UV: u8 = 0x04;

fn b64(bytes: &[u8]) -> String {
    URL_SAFE_NO_PAD.encode(bytes)
}

fn handle(env: &TestEnv) -> UserHandle {
    UserHandle::generate(env).expect("the test random source never fails")
}

/// A credential as an authenticator would have left it: public key material
/// only, which is all a `Passkey` ever holds.
fn fake_key(cred_id: &[u8], counter: u32) -> Passkey {
    let json = serde_json::json!({
        "cred": {
            "cred_id": b64(cred_id),
            "cred": {
                "type_": "ES256",
                "key": { "EC_EC2": {
                    "curve": "SECP256R1",
                    "x": KEY_X.to_vec(),
                    "y": KEY_Y.to_vec(),
                }},
            },
            "counter": counter,
            "transports": null,
            "user_verified": true,
            "backup_eligible": false,
            "backup_state": false,
            "registration_policy": "required",
            "extensions": {
                "cred_protect": "NotRequested",
                "hmac_create_secret": "NotRequested",
            },
            "attestation": { "data": "None", "metadata": "None" },
            "attestation_format": "None",
        }
    });
    serde_json::from_value(json).expect("the fixture passkey is the shape webauthn-rs stores")
}

fn credential(cred_id: &[u8], nickname: &str, sign_count: u32) -> Credential {
    Credential {
        id: CredentialId::from_bytes(cred_id.to_vec()),
        nickname: nickname.to_string(),
        created: SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000),
        last_used: None,
        sign_count,
        backup_eligible: false,
        backup_state: false,
        key: fake_key(cred_id, sign_count),
    }
}

fn row(cred_id: &[u8], nickname: &str, sign_count: u32) -> CredentialRow {
    credential(cred_id, nickname, sign_count)
        .to_row()
        .expect("the fixture credential encodes")
}

/// A registry holding one credential, which is the normal state of a
/// single-operator deployment.
fn enrolled(env: &TestEnv) -> Passkeys {
    enrolled_with(env, Settings::default())
}

fn enrolled_with(env: &TestEnv, settings: Settings) -> Passkeys {
    let mut keys = Passkeys::new(settings, handle(env)).expect("the default settings are valid");
    keys.restore(&[row(b"pocket-4", "Pocket 4", 7)])
        .expect("the fixture row restores");
    keys
}

fn empty(env: &TestEnv) -> Passkeys {
    Passkeys::new(Settings::default(), handle(env)).expect("the default settings are valid")
}

/// The bytes a browser would send as `clientDataJSON`.
fn client_data(type_: &str, challenge: &[u8], origin: &str) -> Vec<u8> {
    format!(
        r#"{{"type":"{type_}","challenge":"{}","origin":"{origin}","crossOrigin":false}}"#,
        b64(challenge)
    )
    .into_bytes()
}

/// The bytes an authenticator signs over: the relying party id hash, the
/// flags, then the sign count, big-endian.
fn authenticator_data(rp_id: &str, flags: u8, counter: u32) -> Vec<u8> {
    let mut data = Sha256::digest(rp_id.as_bytes()).to_vec();
    data.push(flags);
    data.extend_from_slice(&counter.to_be_bytes());
    data
}

/// An assertion that is correct in everything but its signature.
fn assertion(cred_id: &[u8], client_data: &[u8], authenticator_data: &[u8]) -> PublicKeyCredential {
    let json = serde_json::json!({
        "id": b64(cred_id),
        "rawId": b64(cred_id),
        "type": "public-key",
        "response": {
            "authenticatorData": b64(authenticator_data),
            "clientDataJSON": b64(client_data),
            // No test can produce a real one, and every test below is
            // refused before or at this value.
            "signature": b64(&[0u8; 64]),
            "userHandle": null,
        },
        "clientExtensionResults": {},
    });
    serde_json::from_value(json).expect("the fixture assertion is the shape a browser sends")
}

/// The ordinary case: this credential, this challenge, verified user, a
/// counter that moved forward.
fn good_assertion(cred_id: &[u8], challenge: &[u8]) -> PublicKeyCredential {
    assertion(
        cred_id,
        &client_data("webauthn.get", challenge, "http://localhost:7878"),
        &authenticator_data("localhost", UP | UV, 8),
    )
}

/// The refusals webauthn-rs makes *before* it looks at the signature. A
/// fixture that trips one of these has a mistake in it, and a test that
/// expects one of these is asserting that check.
fn before_the_signature(e: &PasskeyError) -> bool {
    matches!(
        e,
        PasskeyError::UserVerificationRequired
            | PasskeyError::Ceremony(
                WebauthnError::CredentialNotFound
                    | WebauthnError::InvalidClientDataType
                    | WebauthnError::MismatchedChallenge
                    | WebauthnError::InvalidRPOrigin
                    | WebauthnError::InvalidRPIDHash
                    | WebauthnError::UserNotPresent
            )
    )
}

/// A session id a test can name twice. `SessionId::from_bytes` is private to
/// the crate's own unit tests, so this goes through the wire form instead.
fn sid(n: u8) -> SessionId {
    SessionId::decode(&b64(&[n; 32])).expect("32 bytes is a session id")
}

// ---------------------------------------------------------------------------
// The control
// ---------------------------------------------------------------------------

#[test]
fn a_fixture_assertion_is_refused_only_at_the_signature() {
    // Every forged-assertion test below leans on this one. If the fixture
    // were wrong in some way a test does not mean to exercise — a stale
    // challenge, the wrong origin, a missing flag — those tests would still
    // see a refusal and would still pass while proving nothing. So assert
    // here that a fixture with everything right gets past every check
    // webauthn-rs makes before the signature, and fails only there.
    let env = TestEnv::new();
    let mut keys = enrolled(&env);
    let mut events = Vec::new();

    let started = keys.start_sign_in(&env).expect("a credential is enrolled");
    let reply = good_assertion(b"pocket-4", &started.challenge.public_key.challenge);
    let err = keys
        .finish_sign_in(&started.ceremony, &reply, &env, &mut events)
        .expect_err("no test can sign an assertion");

    assert!(
        !before_the_signature(&err),
        "the fixture was refused before the signature, so the other tests prove nothing: {err}"
    );
    assert!(
        matches!(err, PasskeyError::Ceremony(_)),
        "expected a refusal from the WebAuthn layer, got {err}"
    );
}

// ---------------------------------------------------------------------------
// User verification is checked, not read
// ---------------------------------------------------------------------------

#[test]
fn a_sign_in_with_user_presence_but_no_user_verification_is_refused() {
    let env = TestEnv::new();
    let mut keys = enrolled(&env);
    let mut events = Vec::new();

    let started = keys.start_sign_in(&env).expect("a credential is enrolled");
    // Something tapped the authenticator. Nothing proved who.
    let reply = assertion(
        b"pocket-4",
        &client_data(
            "webauthn.get",
            &started.challenge.public_key.challenge,
            "http://localhost:7878",
        ),
        &authenticator_data("localhost", UP, 8),
    );

    let err = keys
        .finish_sign_in(&started.ceremony, &reply, &env, &mut events)
        .expect_err("an unverified assertion must not sign anyone in");
    assert!(
        matches!(err, PasskeyError::UserVerificationRequired),
        "an unverified assertion should say so, not hide behind a generic refusal: {err}"
    );
}

#[test]
fn a_step_up_without_user_verification_is_refused_even_when_the_config_does_not_require_it() {
    // The config knob cannot lower the bar for a step-up: the whole point of
    // one is to prove a person is at the device before it reaches a shell.
    let env = TestEnv::new();
    let settings = Settings {
        require_user_verification: false,
        ..Settings::default()
    };
    let mut keys = enrolled_with(&env, settings);
    let mut events = Vec::new();

    let started = keys
        .start_step_up(sid(1), HostAccess::Terminal, SessionKind::Trusted, &env)
        .expect("a trusted session may ask");
    let reply = assertion(
        b"pocket-4",
        &client_data(
            "webauthn.get",
            &started.challenge.public_key.challenge,
            "http://localhost:7878",
        ),
        &authenticator_data("localhost", UP, 8),
    );

    let err = keys
        .finish_step_up(&started.ceremony, &reply, &env, &mut events)
        .expect_err("a tap must not open a terminal");
    assert!(
        matches!(err, PasskeyError::UserVerificationRequired),
        "expected the step-up to insist on verification, got {err}"
    );
    assert!(
        !keys.has_step_up(&sid(1), HostAccess::Terminal, &env),
        "a refused step-up must leave the session with no host access"
    );
}

// ---------------------------------------------------------------------------
// A challenge is single-use, and it expires
// ---------------------------------------------------------------------------

#[test]
fn a_ceremony_cannot_be_spent_twice() {
    let env = TestEnv::new();
    let mut keys = enrolled(&env);
    let mut events = Vec::new();

    let started = keys.start_sign_in(&env).expect("a credential is enrolled");
    let reply = good_assertion(b"pocket-4", &started.challenge.public_key.challenge);

    let first = keys
        .finish_sign_in(&started.ceremony, &reply, &env, &mut events)
        .expect_err("no test can sign an assertion");
    assert!(matches!(first, PasskeyError::Ceremony(_)));

    // A challenge that survives a failed attempt is a challenge that can be
    // ground down or replayed, so one attempt is all it gets.
    let second = keys
        .finish_sign_in(&started.ceremony, &reply, &env, &mut events)
        .expect_err("the ceremony was already spent");
    assert!(
        matches!(second, PasskeyError::UnknownCeremony),
        "expected the ceremony to be gone, got {second}"
    );
}

#[test]
fn a_ceremony_expires_on_the_monotonic_clock_even_when_the_wall_clock_goes_backwards() {
    let env = TestEnv::new();
    let mut keys = enrolled(&env);
    let mut events = Vec::new();

    let started = keys.start_sign_in(&env).expect("a credential is enrolled");
    let reply = good_assertion(b"pocket-4", &started.challenge.public_key.challenge);

    // The window has passed, and the host's clock has been dragged back a
    // year. Only the monotonic clock decides.
    env.advance(Settings::default().ceremony_timeout() + Duration::from_secs(1));
    env.set_wall(SystemTime::UNIX_EPOCH + Duration::from_secs(1_000));

    let err = keys
        .finish_sign_in(&started.ceremony, &reply, &env, &mut events)
        .expect_err("the window closed");
    assert!(
        matches!(err, PasskeyError::CeremonyExpired),
        "expected an expired ceremony, got {err}"
    );
}

#[test]
fn an_abandoned_ceremony_is_pruned_rather_than_kept_forever() {
    let env = TestEnv::new();
    let mut keys = enrolled(&env);
    let mut events = Vec::new();

    let started = keys.start_sign_in(&env).expect("a credential is enrolled");
    env.advance(Settings::default().ceremony_timeout() + Duration::from_secs(1));
    keys.prune(&env);

    let reply = good_assertion(b"pocket-4", &started.challenge.public_key.challenge);
    let err = keys
        .finish_sign_in(&started.ceremony, &reply, &env, &mut events)
        .expect_err("the ceremony was pruned");
    assert!(
        matches!(err, PasskeyError::UnknownCeremony),
        "a pruned ceremony is simply unknown, got {err}"
    );
}

#[test]
fn ceremonies_are_capped_and_the_cap_recovers_as_they_expire() {
    // Starting a sign-in needs no credential of the caller's, so an
    // unauthenticated client must not be able to grow this map without end.
    let env = TestEnv::new();
    let mut keys = enrolled(&env);

    for n in 0..MAX_PENDING {
        keys.start_sign_in(&env)
            .unwrap_or_else(|e| panic!("ceremony {n} should be allowed: {e}"));
    }
    let err = keys
        .start_sign_in(&env)
        .expect_err("the cap must hold at MAX_PENDING");
    assert!(
        matches!(err, PasskeyError::TooManyCeremonies),
        "expected the cap to refuse, got {err}"
    );

    // The cap must not be a lockout: it clears itself as the window passes.
    env.advance(Settings::default().ceremony_timeout() + Duration::from_secs(1));
    keys.start_sign_in(&env)
        .expect("expired ceremonies free their slots");
}

// ---------------------------------------------------------------------------
// Step-up is not sign-in
// ---------------------------------------------------------------------------

#[test]
fn a_sign_in_ceremony_cannot_be_finished_as_a_step_up() {
    let env = TestEnv::new();
    let mut keys = enrolled(&env);
    let mut events = Vec::new();

    let started = keys.start_sign_in(&env).expect("a credential is enrolled");
    let reply = good_assertion(b"pocket-4", &started.challenge.public_key.challenge);

    let err = keys
        .finish_step_up(&started.ceremony, &reply, &env, &mut events)
        .expect_err("a sign-in is never spendable as a step-up");
    match err {
        PasskeyError::WrongPurpose { expected, found } => {
            assert_eq!(expected, Purpose::StepUp);
            assert_eq!(found, Purpose::SignIn);
        }
        other => panic!("expected a purpose mismatch, got {other}"),
    }
    assert!(
        !keys.has_step_up(&sid(1), HostAccess::Terminal, &env),
        "a refused step-up must grant nothing"
    );
}

#[test]
fn a_step_up_ceremony_cannot_be_finished_as_a_sign_in() {
    let env = TestEnv::new();
    let mut keys = enrolled(&env);
    let mut events = Vec::new();

    let started = keys
        .start_step_up(
            sid(1),
            HostAccess::OutsideWorkingDirectory,
            SessionKind::Trusted,
            &env,
        )
        .expect("a trusted session may ask");
    let reply = good_assertion(b"pocket-4", &started.challenge.public_key.challenge);

    let err = keys
        .finish_sign_in(&started.ceremony, &reply, &env, &mut events)
        .expect_err("the purposes do not cross in either direction");
    assert!(matches!(
        err,
        PasskeyError::WrongPurpose {
            expected: Purpose::SignIn,
            found: Purpose::StepUp
        }
    ));
}

#[test]
fn a_sign_in_assertion_is_bound_to_its_own_challenge_not_merely_to_its_purpose() {
    // Second line behind the purpose check: even if a sign-in ceremony and a
    // step-up ceremony were confused for each other, the assertion signs one
    // challenge and cannot be moved to the other.
    let env = TestEnv::new();
    let mut keys = enrolled(&env);
    let mut events = Vec::new();

    let sign_in = keys.start_sign_in(&env).expect("a credential is enrolled");
    let step_up = keys
        .start_step_up(sid(1), HostAccess::Terminal, SessionKind::Trusted, &env)
        .expect("a trusted session may ask");

    // The assertion a browser would return for the sign-in, handed to the
    // step-up ceremony.
    let reply = good_assertion(b"pocket-4", &sign_in.challenge.public_key.challenge);
    let err = keys
        .finish_step_up(&step_up.ceremony, &reply, &env, &mut events)
        .expect_err("a replayed sign-in assertion must not open a terminal");
    assert!(
        matches!(
            err,
            PasskeyError::Ceremony(WebauthnError::MismatchedChallenge)
        ),
        "expected the challenge binding to refuse it, got {err}"
    );
    assert!(!keys.has_step_up(&sid(1), HostAccess::Terminal, &env));
}

#[test]
fn a_tether_session_can_never_step_up_and_is_refused_before_it_costs_anything() {
    let env = TestEnv::new();
    let mut keys = enrolled(&env);

    let err = keys
        .start_step_up(sid(9), HostAccess::Terminal, SessionKind::Tether, &env)
        .expect_err("a borrowed machine holds no passkey of mine");
    assert!(matches!(err, PasskeyError::StepUpFromTether));

    // The refusal must not have cost a ceremony slot: otherwise a tether
    // session could exhaust the cap by asking.
    for _ in 0..MAX_PENDING {
        keys.start_sign_in(&env)
            .expect("no slot was spent on the refusal");
    }
}

#[test]
fn a_session_that_has_not_stepped_up_has_no_host_access() {
    let env = TestEnv::new();
    let mut keys = enrolled(&env);

    assert!(!keys.has_step_up(&sid(1), HostAccess::Terminal, &env));
    assert_eq!(
        keys.step_up_remaining(&sid(1), HostAccess::Terminal, &env),
        None
    );
    // Asking is not proving: starting a ceremony grants nothing.
    keys.start_step_up(sid(1), HostAccess::Terminal, SessionKind::Trusted, &env)
        .expect("a trusted session may ask");
    assert!(!keys.has_step_up(&sid(1), HostAccess::Terminal, &env));
}

// ---------------------------------------------------------------------------
// Whose credential it is
// ---------------------------------------------------------------------------

#[test]
fn a_credential_enrolled_against_another_user_handle_cannot_sign_in_here() {
    // Two working directories, each with its own operator handle and its own
    // credential. The credential from one is presented to the other.
    let env = TestEnv::new();
    let mut theirs = Passkeys::new(Settings::default(), handle(&env)).expect("valid settings");
    theirs
        .restore(&[row(b"their-key", "Their Laptop", 3)])
        .expect("the fixture row restores");
    let mut mine = enrolled(&env);
    let mut events = Vec::new();

    let started = mine.start_sign_in(&env).expect("a credential is enrolled");
    assert_eq!(
        started.challenge.public_key.allow_credentials.len(),
        1,
        "the allow-list must offer only credentials this registry holds"
    );

    let reply = good_assertion(b"their-key", &started.challenge.public_key.challenge);
    let err = mine
        .finish_sign_in(&started.ceremony, &reply, &env, &mut events)
        .expect_err("a foreign credential is not mine to accept");
    assert!(
        matches!(
            err,
            PasskeyError::Ceremony(WebauthnError::CredentialNotFound)
        ),
        "expected the credential to be unknown here, got {err}"
    );
    assert_eq!(
        mine.consecutive_failures(),
        1,
        "a foreign credential is a failed attempt, which is what the lockdown trigger counts"
    );
    assert!(
        theirs
            .credential(&CredentialId::from_bytes(b"their-key".to_vec()))
            .is_some()
    );
}

#[test]
fn removing_a_credential_takes_effect_on_a_ceremony_already_in_flight() {
    // A ceremony holds a snapshot of the credentials it was started with, so
    // a device removed one second into a two-minute window would otherwise
    // still have a live challenge it could answer. Removing a lost device
    // has to mean now, not at the end of whatever it already started.
    let env = TestEnv::new();
    let mut keys = enrolled(&env);
    let mut events = Vec::new();

    let started = keys.start_sign_in(&env).expect("a credential is enrolled");
    let id = CredentialId::from_bytes(b"pocket-4".to_vec());
    keys.remove(&id).expect("the credential is enrolled");

    let reply = good_assertion(b"pocket-4", &started.challenge.public_key.challenge);
    let err = keys
        .finish_sign_in(&started.ceremony, &reply, &env, &mut events)
        .expect_err("a lost device is removed, and removal must take effect at once");
    assert!(
        matches!(err, PasskeyError::UnknownCeremony),
        "expected the in-flight ceremony to be gone with the credential, got {err}"
    );
    assert!(!keys.is_enrolled());
    assert!(matches!(
        keys.start_sign_in(&env)
            .expect_err("nothing left to sign in with"),
        PasskeyError::NoCredentials
    ));
}

#[test]
fn there_is_nothing_to_sign_in_with_until_a_passkey_is_enrolled() {
    // The sign-in page must offer the one-time code instead of an empty
    // authenticator prompt, so this has to be an error and not an empty
    // allow-list.
    let env = TestEnv::new();
    let mut keys = empty(&env);

    assert!(!keys.is_enrolled());
    assert!(matches!(
        keys.start_sign_in(&env).expect_err("no credentials"),
        PasskeyError::NoCredentials
    ));
    assert!(matches!(
        keys.start_step_up(sid(1), HostAccess::Terminal, SessionKind::Trusted, &env)
            .expect_err("no credentials"),
        PasskeyError::NoCredentials
    ));
}

// ---------------------------------------------------------------------------
// Where the assertion came from
// ---------------------------------------------------------------------------

#[test]
fn an_assertion_collected_at_another_origin_is_refused() {
    let env = TestEnv::new();
    let mut keys = enrolled(&env);
    let mut events = Vec::new();

    let started = keys.start_sign_in(&env).expect("a credential is enrolled");
    let reply = assertion(
        b"pocket-4",
        &client_data(
            "webauthn.get",
            &started.challenge.public_key.challenge,
            "https://deckjack.example.net",
        ),
        &authenticator_data("localhost", UP | UV, 8),
    );

    let err = keys
        .finish_sign_in(&started.ceremony, &reply, &env, &mut events)
        .expect_err("a cross-origin assertion is not a sign-in");
    assert!(
        matches!(err, PasskeyError::Ceremony(WebauthnError::InvalidRPOrigin)),
        "expected the origin check to refuse it, got {err}"
    );
}

#[test]
fn an_assertion_signed_for_another_relying_party_is_refused() {
    let env = TestEnv::new();
    let mut keys = enrolled(&env);
    let mut events = Vec::new();

    let started = keys.start_sign_in(&env).expect("a credential is enrolled");
    let reply = assertion(
        b"pocket-4",
        &client_data(
            "webauthn.get",
            &started.challenge.public_key.challenge,
            "http://localhost:7878",
        ),
        // The authenticator signed over someone else's domain.
        &authenticator_data("deckjack.example.net", UP | UV, 8),
    );

    let err = keys
        .finish_sign_in(&started.ceremony, &reply, &env, &mut events)
        .expect_err("that assertion belongs to another relying party");
    assert!(
        matches!(err, PasskeyError::Ceremony(WebauthnError::InvalidRPIDHash)),
        "expected the relying party hash to refuse it, got {err}"
    );
}

#[test]
fn a_registration_response_cannot_be_spent_as_an_assertion() {
    let env = TestEnv::new();
    let mut keys = enrolled(&env);
    let mut events = Vec::new();

    let started = keys.start_sign_in(&env).expect("a credential is enrolled");
    let reply = assertion(
        b"pocket-4",
        &client_data(
            "webauthn.create",
            &started.challenge.public_key.challenge,
            "http://localhost:7878",
        ),
        &authenticator_data("localhost", UP | UV, 8),
    );

    let err = keys
        .finish_sign_in(&started.ceremony, &reply, &env, &mut events)
        .expect_err("a creation is not a get");
    assert!(
        matches!(
            err,
            PasskeyError::Ceremony(WebauthnError::InvalidClientDataType)
        ),
        "expected the client data type to refuse it, got {err}"
    );
}

// ---------------------------------------------------------------------------
// What a failure leaves behind
// ---------------------------------------------------------------------------

#[test]
fn every_refused_ceremony_counts_toward_the_repeated_failure_trigger() {
    let env = TestEnv::new();
    let mut keys = enrolled(&env);
    let mut events = Vec::new();

    let sign_in = keys.start_sign_in(&env).expect("a credential is enrolled");
    let reply = good_assertion(b"pocket-4", &sign_in.challenge.public_key.challenge);
    let _ = keys.finish_sign_in(&sign_in.ceremony, &reply, &env, &mut events);

    let step_up = keys
        .start_step_up(sid(1), HostAccess::Terminal, SessionKind::Trusted, &env)
        .expect("a trusted session may ask");
    let reply = good_assertion(b"pocket-4", &step_up.challenge.public_key.challenge);
    let _ = keys.finish_step_up(&step_up.ceremony, &reply, &env, &mut events);

    assert_eq!(
        keys.consecutive_failures(),
        2,
        "failures run across purposes: the count is of attempts, not of sign-ins"
    );
    let failures: Vec<&Event> = events
        .iter()
        .filter(|e| matches!(e, Event::Failed { .. }))
        .collect();
    assert_eq!(
        failures,
        vec![
            &Event::Failed {
                purpose: Purpose::SignIn,
                consecutive: 1
            },
            &Event::Failed {
                purpose: Purpose::StepUp,
                consecutive: 2
            },
        ],
        "the trigger reads `consecutive`, so it has to be the run and not a per-purpose count"
    );
}

// ---------------------------------------------------------------------------
// The sign count: the clone signal
// ---------------------------------------------------------------------------

#[test]
fn a_sign_count_that_does_not_move_forward_is_a_clone() {
    // The counter increments on every assertion, so a value that repeats or
    // goes backwards means two authenticators are answering for one
    // credential, which means the private key was extracted.
    assert!(sign_count_ok(7, 8), "a counter that moved forward is fine");
    assert!(
        sign_count_ok(7, u32::MAX),
        "a large jump forward is still forward"
    );
    assert!(
        !sign_count_ok(7, 7),
        "a replay of the same counter is a clone"
    );
    assert!(
        !sign_count_ok(7, 6),
        "a counter that went backwards is a clone"
    );
    assert!(
        !sign_count_ok(7, 0),
        "a counter that disappears after being present is a clone, not an upgrade"
    );
}

#[test]
fn a_synced_passkey_that_never_counts_is_not_a_clone() {
    // iCloud Keychain and Google Password Manager always report zero,
    // because the credential is synced between devices by design. Treating
    // that as a compromise would lock the operator out of their own phone.
    assert!(
        sign_count_ok(0, 0),
        "zero from a credential that never counted is normal"
    );
    assert!(sign_count_ok(0, 1), "the first real count sets the floor");
}

#[test]
fn a_sign_count_the_database_cannot_represent_fails_closed() {
    // A row can hold values a u32 cannot. Whatever is done with them, the
    // result must never be a stored count so low that a regression looks
    // like progress.
    let negative = CredentialRow {
        sign_count: -1,
        ..row(b"pocket-4", "Pocket 4", 7)
    };
    let restored = Credential::from_row(&negative).expect("a bad count is not an unusable row");
    assert_eq!(
        restored.sign_count, 0,
        "a negative count means nothing was ever seen, which is what zero means here"
    );

    let huge = CredentialRow {
        sign_count: i64::MAX,
        ..row(b"pocket-4", "Pocket 4", 7)
    };
    let restored = Credential::from_row(&huge).expect("a bad count is not an unusable row");
    assert_eq!(
        restored.sign_count,
        u32::MAX,
        "clamped, not truncated to a low value"
    );
    assert!(
        !sign_count_ok(restored.sign_count, 8),
        "clamping high keeps the clone check biting; clamping low would blind it"
    );
}

#[test]
fn the_presented_sign_count_is_read_from_the_authenticator_data_big_endian() {
    // This is what names the number in the clone alarm, so an endianness
    // mistake here would report a nonsense counter in an incident.
    let data = authenticator_data("localhost", UP | UV, 0x0102_0304);
    assert_eq!(presented_sign_count(&data), 0x0102_0304);
    assert_eq!(
        presented_sign_count(&authenticator_data("localhost", UP, 0)),
        0
    );
    // Truncated data cannot hold a counter, and must not panic.
    assert_eq!(presented_sign_count(&[]), 0);
    assert_eq!(presented_sign_count(&data[..36]), 0);
}

// ---------------------------------------------------------------------------
// Enrollment: a new device needs approval from a signed-in one
// ---------------------------------------------------------------------------

#[test]
fn the_first_credential_needs_no_approval_and_the_second_one_does() {
    let env = TestEnv::new();
    let mut keys = empty(&env);

    keys.start_registration(Enrollment::First, "Pocket 4", &env)
        .expect("there is no signed-in device to approve the first one");

    // Once a credential exists there is a device to approve the next, so an
    // unapproved enrollment is refused however the request arrived.
    let mut keys = enrolled(&env);
    let err = keys
        .start_registration(Enrollment::First, "Borrowed PC", &env)
        .expect_err("a second credential needs approval");
    assert!(
        matches!(err, PasskeyError::ApprovalRequired),
        "expected approval to be required, got {err}"
    );
}

#[test]
fn an_enrollment_with_no_approval_outstanding_is_refused() {
    let env = TestEnv::new();
    let mut keys = enrolled(&env);

    let err = keys
        .start_registration(Enrollment::Ticket("ABCD-EFGH"), "Borrowed PC", &env)
        .expect_err("no approval was given");
    assert!(matches!(err, PasskeyError::NoTicket), "got {err}");
}

#[test]
fn three_wrong_codes_cancel_the_approval() {
    let env = TestEnv::new();
    let mut keys = enrolled(&env);
    let code = keys
        .approve_enrollment(sid(1), &env)
        .expect("the random source works")
        .reveal();

    for expected_left in [2, 1] {
        let err = keys
            .start_registration(Enrollment::Ticket("0000-0000"), "Borrowed PC", &env)
            .expect_err("that is not the code");
        match err {
            PasskeyError::WrongTicket { attempts_left } => {
                assert_eq!(attempts_left, expected_left)
            }
            other => panic!("expected a wrong code, got {other}"),
        }
    }

    let err = keys
        .start_registration(Enrollment::Ticket("0000-0000"), "Borrowed PC", &env)
        .expect_err("the approval is burned");
    assert!(matches!(err, PasskeyError::TicketBurned), "got {err}");

    // Burned means gone: the real code no longer works either.
    let err = keys
        .start_registration(Enrollment::Ticket(&code), "Borrowed PC", &env)
        .expect_err("the approval was cancelled, not merely refused once");
    assert!(matches!(err, PasskeyError::NoTicket), "got {err}");
}

#[test]
fn an_expired_approval_is_refused_and_cleared() {
    let env = TestEnv::new();
    let mut keys = enrolled(&env);
    let code = keys
        .approve_enrollment(sid(1), &env)
        .expect("the random source works")
        .reveal();

    env.advance(Settings::default().enrollment_ttl() + Duration::from_secs(1));
    let err = keys
        .start_registration(Enrollment::Ticket(&code), "Borrowed PC", &env)
        .expect_err("the approval window closed");
    assert!(matches!(err, PasskeyError::TicketExpired), "got {err}");

    // And it is gone, not merely stale: a correct code must not resurrect it.
    let err = keys
        .start_registration(Enrollment::Ticket(&code), "Borrowed PC", &env)
        .expect_err("an expired approval is cleared");
    assert!(matches!(err, PasskeyError::NoTicket), "got {err}");
}

#[test]
fn a_new_approval_replaces_the_one_before_it() {
    // The newest approval is the one the operator means, and the old code
    // must stop working the moment the new one is shown.
    let env = TestEnv::new();
    let mut keys = enrolled(&env);
    let first = keys
        .approve_enrollment(sid(1), &env)
        .expect("the random source works")
        .reveal();
    let second = keys
        .approve_enrollment(sid(2), &env)
        .expect("the random source works")
        .reveal();
    assert_ne!(first, second, "each approval is its own code");

    let err = keys
        .start_registration(Enrollment::Ticket(&first), "Borrowed PC", &env)
        .expect_err("the old code is no longer the approval");
    assert!(
        matches!(err, PasskeyError::WrongTicket { .. }),
        "expected the superseded code to be wrong, got {err}"
    );
    keys.start_registration(Enrollment::Ticket(&second), "Pocket 4", &env)
        .expect("the current code works");
}

#[test]
fn one_approval_authorizes_only_one_registration_at_a_time() {
    // The approval is checked when a registration starts and only spent when
    // one finishes. Without this, a code seen once could start several
    // registrations and finish every one of them, enrolling a credential
    // apiece from a single approval.
    let env = TestEnv::new();
    let mut keys = enrolled(&env);
    let code = keys
        .approve_enrollment(sid(1), &env)
        .expect("the random source works")
        .reveal();

    let first = keys
        .start_registration(Enrollment::Ticket(&code), "Pocket 4", &env)
        .expect("the code is right");
    let second = keys
        .start_registration(Enrollment::Ticket(&code), "Pocket 4", &env)
        .expect("a retry after a cancelled prompt must still work");
    assert_ne!(
        first.ceremony.encode(),
        second.ceremony.encode(),
        "each start is its own ceremony"
    );

    // Only the newest one is live; the earlier one cannot also be completed.
    // Were it still pending, this would be a purpose mismatch rather than an
    // unknown ceremony, which is what distinguishes "dropped" from "still
    // there": the challenge in the reply is never reached either way.
    let mut events = Vec::new();
    let reply = good_assertion(b"pocket-4", b"unreachable");
    let err = keys
        .finish_sign_in(&first.ceremony, &reply, &env, &mut events)
        .expect_err("the superseded registration is gone");
    assert!(
        matches!(err, PasskeyError::UnknownCeremony),
        "expected the superseded registration ceremony to be gone, got {err}"
    );
}

#[test]
fn revoking_the_approving_session_cancels_its_approval() {
    // An approval is nothing but the authority of a live, signed-in device.
    // When that session goes, a code still on its screen must not still
    // enroll a passkey.
    let env = TestEnv::new();
    let mut keys = enrolled(&env);
    let code = keys
        .approve_enrollment(sid(1), &env)
        .expect("the random source works")
        .reveal();

    keys.revoke_session(&sid(2));
    keys.start_registration(Enrollment::Ticket(&code), "Pocket 4", &env)
        .expect("another session logging out is none of this approval's business");

    keys.revoke_session(&sid(1));
    let err = keys
        .start_registration(Enrollment::Ticket(&code), "Pocket 4", &env)
        .expect_err("the approving session is gone");
    assert!(
        matches!(err, PasskeyError::NoTicket),
        "expected the approval to be cancelled with its session, got {err}"
    );
}

#[test]
fn an_enrollment_code_never_appears_in_a_debug_line() {
    // The code is read off a screen and typed into another device. The only
    // way it reaches a log is a deliberate `reveal`, so `Debug` must not
    // carry it, nor must the registry's own.
    let env = TestEnv::new();
    let mut keys = enrolled(&env);
    let code = {
        let ticket = keys
            .approve_enrollment(sid(1), &env)
            .expect("the random source works");
        let printed = format!("{ticket:?}");
        let code = ticket.reveal();
        assert!(
            !printed.contains(&code),
            "the ticket's Debug printed the code: {printed}"
        );
        code
    };
    let printed = format!("{keys:?}");
    assert!(
        !printed.contains(&code),
        "the registry's Debug printed the code: {printed}"
    );
    // Nor the bare symbols, in case the formatting changes.
    assert!(!printed.contains(code.replace('-', "").as_str()));
}

#[test]
fn a_ceremony_id_never_appears_in_a_debug_line() {
    // Whoever holds a ceremony id can complete the ceremony it names, so it
    // is a bearer value and gets the same treatment as a session id.
    let env = TestEnv::new();
    let mut keys = enrolled(&env);
    let started = keys.start_sign_in(&env).expect("a credential is enrolled");
    let id = started.ceremony.encode();

    assert!(!format!("{:?}", started.ceremony).contains(&id));
    assert!(!format!("{keys:?}").contains(&id));
}

// ---------------------------------------------------------------------------
// Revocation
// ---------------------------------------------------------------------------

#[test]
fn revoke_all_drops_every_ceremony_and_the_approval() {
    let env = TestEnv::new();
    let mut keys = enrolled(&env);
    let mut events = Vec::new();

    let sign_in = keys.start_sign_in(&env).expect("a credential is enrolled");
    let step_up = keys
        .start_step_up(sid(1), HostAccess::Terminal, SessionKind::Trusted, &env)
        .expect("a trusted session may ask");
    let code = keys
        .approve_enrollment(sid(1), &env)
        .expect("the random source works")
        .reveal();

    keys.revoke_all();

    let reply = good_assertion(b"pocket-4", &sign_in.challenge.public_key.challenge);
    assert!(matches!(
        keys.finish_sign_in(&sign_in.ceremony, &reply, &env, &mut events)
            .expect_err("revoked"),
        PasskeyError::UnknownCeremony
    ));
    assert!(matches!(
        keys.finish_step_up(&step_up.ceremony, &reply, &env, &mut events)
            .expect_err("revoked"),
        PasskeyError::UnknownCeremony
    ));
    assert!(matches!(
        keys.start_registration(Enrollment::Ticket(&code), "Pocket 4", &env)
            .expect_err("revoked"),
        PasskeyError::NoTicket
    ));
    // The credentials themselves survive: the kill switch revokes sessions,
    // it does not un-enroll the operator's devices.
    assert!(keys.is_enrolled());
}

#[test]
fn revoking_one_session_leaves_another_sessions_step_up_alone() {
    let env = TestEnv::new();
    let mut keys = enrolled(&env);
    let mut events = Vec::new();

    let mine = keys
        .start_step_up(sid(1), HostAccess::Terminal, SessionKind::Trusted, &env)
        .expect("a trusted session may ask");
    let theirs = keys
        .start_step_up(sid(2), HostAccess::Terminal, SessionKind::Trusted, &env)
        .expect("a trusted session may ask");

    keys.revoke_session(&sid(2));

    let reply = good_assertion(b"pocket-4", &theirs.challenge.public_key.challenge);
    assert!(
        matches!(
            keys.finish_step_up(&theirs.ceremony, &reply, &env, &mut events)
                .expect_err("that session is gone"),
            PasskeyError::UnknownCeremony
        ),
        "a revoked session's ceremony must not survive it"
    );

    let reply = good_assertion(b"pocket-4", &mine.challenge.public_key.challenge);
    let err = keys
        .finish_step_up(&mine.ceremony, &reply, &env, &mut events)
        .expect_err("no test can sign an assertion");
    assert!(
        !matches!(err, PasskeyError::UnknownCeremony),
        "another session's logout must not cancel my ceremony: {err}"
    );
}

// ---------------------------------------------------------------------------
// Names and identifiers
// ---------------------------------------------------------------------------

#[test]
fn a_nickname_must_be_one_line_of_readable_text() {
    let env = TestEnv::new();
    let mut keys = empty(&env);

    // The last one is the reason control characters are refused: the
    // nickname is quoted into audit lines and error messages, and a newline
    // in it would forge a line in a log the design says must survive an
    // incident.
    for bad in [
        "",
        "   ",
        "\n",
        "Pocket\n4 signed in from 10.0.0.1",
        "Pocket\u{7}4",
    ] {
        let err = keys
            .start_registration(Enrollment::First, bad, &env)
            .map(|_| ())
            .expect_err("that is not a device name");
        assert!(
            matches!(err, PasskeyError::BadNickname),
            "expected {bad:?} to be refused as a nickname, got {err}"
        );
    }

    let too_long = "x".repeat(65);
    assert!(matches!(
        keys.start_registration(Enrollment::First, &too_long, &env)
            .expect_err("a 10MB nickname has no business in the database"),
        PasskeyError::BadNickname
    ));

    // Surrounding space is trimmed rather than refused, and the cap counts
    // characters, not bytes.
    keys.start_registration(Enrollment::First, "  Pocket 4  ", &env)
        .expect("a reasonable nickname is accepted");
    keys.start_registration(Enrollment::First, &"é".repeat(64), &env)
        .expect("64 characters is 64 characters, whatever they encode to");
}

#[test]
fn a_ceremony_id_of_the_wrong_length_is_not_a_ceremony_id() {
    use deckjack_core::passkey::CeremonyId;

    let env = TestEnv::new();
    let mut keys = enrolled(&env);
    let started = keys.start_sign_in(&env).expect("a credential is enrolled");
    let text = started.ceremony.encode();

    assert_eq!(text.len(), 43, "32 bytes of base64url");
    assert!(
        CeremonyId::decode(&text).is_some(),
        "its own text round-trips"
    );
    assert!(CeremonyId::decode("").is_none());
    assert!(CeremonyId::decode(&text[..42]).is_none());
    assert!(CeremonyId::decode(&format!("{text}A")).is_none());
    assert!(CeremonyId::decode("not base64url!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!").is_none());
}

// ---------------------------------------------------------------------------
// Settings that cannot be wrong quietly
// ---------------------------------------------------------------------------

#[test]
fn an_origin_that_is_not_a_url_is_refused_with_the_value_in_the_message() {
    let env = TestEnv::new();
    let settings = Settings {
        origin: "deckjack.example.com".to_string(),
        ..Settings::default()
    };
    let err = Passkeys::new(settings, handle(&env))
        .expect_err("a sign-in page that looks fine and refuses every ceremony is worse");
    let message = err.to_string();
    assert!(
        matches!(err, PasskeyError::BadOrigin { .. }),
        "expected a bad origin, got {err}"
    );
    assert!(
        message.contains("deckjack.example.com") && message.contains("scheme"),
        "the message has to name the value and say what it wanted: {message}"
    );
}

#[test]
fn an_rp_id_that_is_not_the_origins_domain_is_refused() {
    let env = TestEnv::new();
    let settings = Settings {
        rp_id: "example.net".to_string(),
        origin: "https://deckjack.example.com".to_string(),
        ..Settings::default()
    };
    let err = Passkeys::new(settings, handle(&env)).expect_err("those cannot both be right");
    assert!(
        matches!(err, PasskeyError::BadRelyingParty { .. }),
        "expected a relying party mismatch, got {err}"
    );
}

// ---------------------------------------------------------------------------
// Storage
// ---------------------------------------------------------------------------

#[test]
fn a_credential_round_trips_through_sqlite_unchanged() {
    // The schema is spelled out here rather than taken from `store::Store`
    // because the `passkeys` table exists in the migration but nothing reads
    // or writes it yet: there is no `upsert_passkey` or `passkeys` to call.
    // Until there is, this pins the column types a writer has to honor, and
    // it is the same DDL, STRICT included, as store.rs's migration.
    let conn = rusqlite::Connection::open_in_memory().expect("in-memory sqlite");
    conn.execute_batch(
        "CREATE TABLE passkeys (
            cred_id         TEXT    PRIMARY KEY,
            nickname        TEXT    NOT NULL,
            key_json        TEXT    NOT NULL,
            sign_count      INTEGER NOT NULL,
            backup_eligible INTEGER NOT NULL,
            backup_state    INTEGER NOT NULL,
            created         INTEGER NOT NULL,
            last_used       INTEGER
         ) STRICT;",
    )
    .expect("the DDL is the one from store.rs");

    let mut original = credential(b"pocket-4", "Pocket 4", u32::MAX - 1);
    original.backup_eligible = true;
    original.backup_state = true;
    original.last_used = Some(SystemTime::UNIX_EPOCH + Duration::from_secs(1_767_225_600));
    let written = original.to_row().expect("the credential encodes");

    conn.execute(
        "INSERT INTO passkeys
            (cred_id, nickname, key_json, sign_count, backup_eligible, backup_state, created,
             last_used)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        rusqlite::params![
            written.cred_id,
            written.nickname,
            written.key_json,
            written.sign_count,
            written.backup_eligible,
            written.backup_state,
            written.created,
            written.last_used,
        ],
    )
    .expect("the row is storable as it stands");

    let read: CredentialRow = conn
        .query_row(
            "SELECT cred_id, nickname, key_json, sign_count, backup_eligible, backup_state,
                    created, last_used
             FROM passkeys",
            [],
            |r| {
                Ok(CredentialRow {
                    cred_id: r.get(0)?,
                    nickname: r.get(1)?,
                    key_json: r.get(2)?,
                    sign_count: r.get(3)?,
                    backup_eligible: r.get(4)?,
                    backup_state: r.get(5)?,
                    created: r.get(6)?,
                    last_used: r.get(7)?,
                })
            },
        )
        .expect("the row reads back");
    assert_eq!(read, written, "SQLite changed the row");

    let restored = Credential::from_row(&read).expect("the row rebuilds");
    assert_eq!(restored.id, original.id);
    assert_eq!(restored.nickname, original.nickname);
    assert_eq!(restored.created, original.created);
    assert_eq!(restored.last_used, original.last_used);
    assert_eq!(
        restored.sign_count, original.sign_count,
        "a count near u32::MAX must survive the trip through an i64 column"
    );
    assert_eq!(restored.backup_eligible, original.backup_eligible);
    assert_eq!(restored.backup_state, original.backup_state);
    // The key itself: encoding it again must give the same bytes, or the
    // next assertion fails for no visible reason.
    assert_eq!(
        restored.to_row().expect("the restored credential encodes"),
        written
    );

    // And the registry accepts it as the credential it was.
    let env = TestEnv::new();
    let mut keys = empty(&env);
    keys.restore(&[read]).expect("a stored row restores");
    assert!(keys.is_enrolled());
    assert_eq!(keys.credentials()[0].sign_count, u32::MAX - 1);
}

#[test]
fn a_stored_row_that_cannot_be_read_is_refused_and_named() {
    let env = TestEnv::new();
    let mut keys = empty(&env);

    let bad_key = CredentialRow {
        key_json: "{}".to_string(),
        ..row(b"pocket-4", "Pocket 4", 7)
    };
    let err = keys
        .restore(&[bad_key])
        .expect_err("an unreadable key is not a usable credential");
    match err {
        PasskeyError::BadStoredRow { cred_id, reason } => {
            assert_eq!(
                cred_id,
                CredentialId::from_bytes(b"pocket-4".to_vec()).encode()
            );
            assert!(
                reason.contains("public key"),
                "the reason should say what: {reason}"
            );
        }
        other => panic!("expected a bad stored row, got {other}"),
    }

    let bad_id = CredentialRow {
        cred_id: "not base64url!".to_string(),
        ..row(b"pocket-4", "Pocket 4", 7)
    };
    assert!(matches!(
        keys.restore(&[bad_id])
            .expect_err("a credential id that is not base64url is not a credential id"),
        PasskeyError::BadStoredRow { .. }
    ));
}

#[test]
fn a_restored_credential_keeps_the_name_the_operator_gave_it() {
    let env = TestEnv::new();
    let mut keys = enrolled(&env);
    let id = CredentialId::from_bytes(b"pocket-4".to_vec());

    assert_eq!(keys.credential(&id).expect("enrolled").nickname, "Pocket 4");
    keys.rename(&id, "  GPD Pocket 4  ")
        .expect("a rename of an enrolled credential");
    assert_eq!(
        keys.credential(&id).expect("enrolled").nickname,
        "GPD Pocket 4"
    );

    assert!(matches!(
        keys.rename(&id, "bad\nname")
            .expect_err("a nickname is one line"),
        PasskeyError::BadNickname
    ));
    assert_eq!(
        keys.credential(&id).expect("enrolled").nickname,
        "GPD Pocket 4",
        "a refused rename must not have renamed anything"
    );

    let unknown = CredentialId::from_bytes(b"not-here".to_vec());
    assert!(matches!(
        keys.rename(&unknown, "Whatever").expect_err("not enrolled"),
        PasskeyError::UnknownCredential { .. }
    ));
    assert!(matches!(
        keys.remove(&unknown).expect_err("not enrolled"),
        PasskeyError::UnknownCredential { .. }
    ));
}
