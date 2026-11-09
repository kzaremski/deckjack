//! Passkey sign-in and step-up.
//!
//! Passkeys sit **alongside** the one-time code in [`crate::auth`], they do
//! not replace it. The two paths answer different questions:
//!
//! | Path            | Where it is used                                     |
//! |-----------------|------------------------------------------------------|
//! | passkey         | my own devices, remote, over the tailnet or the VPS  |
//! | one-time code   | tether mode on a borrowed, untrusted machine         |
//! | access token    | local CLI on the host                                |
//!
//! A borrowed machine never gets a passkey prompt: the keyboard may be
//! logged and the browser may be hostile, and more practically that machine
//! holds no credential of mine, so there is nothing for it to assert with.
//! That is why tether mode keeps the code.
//!
//! Three rules from the design drive the shape of this module:
//!
//! - **A new device needs approval from an already signed-in one.** Enrollment
//!   therefore needs an [`EnrollmentTicket`] issued by a live session, except
//!   for the very first credential, when there is nothing to approve with.
//! - **Step-up is not sign-in.** Opening a terminal or reaching outside the
//!   working directory asks for the passkey again, so a stolen cookie alone
//!   never yields a shell. [`Passkeys::finish_step_up`] and
//!   [`Passkeys::finish_sign_in`] are separate calls returning separate types,
//!   and each refuses a ceremony started for the other purpose; a ceremony
//!   cannot be started as a sign-in and spent as a terminal unlock.
//! - **Sign counts and user verification are checked, not logged.** A counter
//!   that goes backwards means the credential was cloned, which is a lockdown
//!   trigger, not a warning.
//!
//! Like [`crate::lock`], everything this module needs from the outside world
//! — the clock, the monotonic clock and randomness — arrives through [`Env`],
//! so every rule above is exercised without sleeping.
//!
//! State lives in memory, as it does in [`crate::auth::state::AuthState`];
//! credentials are handed to the store as [`CredentialRow`] and read back at
//! startup through [`Passkeys::restore`]. Step-up grants are deliberately
//! **not** persisted: a restart is the cheapest kill switch there is, and a
//! grant that survived one would outlive the reason it was given.
//!
//! ```ignore
//! // Startup: rebuild from the database.
//! let mut keys = Passkeys::new(settings, user_handle)?;
//! keys.restore(store.passkeys()?)?;
//!
//! // Remote sign-in.
//! let (ceremony, challenge) = keys.start_sign_in(&env)?;      // -> browser
//! let signed_in = keys.finish_sign_in(&ceremony, &assertion, &env, &mut events)?;
//! store.upsert_passkey(&signed_in.row)?;                      // sign count moved
//!
//! // Later, that session asks for a terminal.
//! if !keys.has_step_up(&session, HostAccess::Terminal, &env) {
//!     let (ceremony, challenge) = keys.start_step_up(
//!         session.clone(), HostAccess::Terminal, SessionKind::Trusted, &env)?;
//!     // ... and only a successful finish_step_up opens the PTY.
//! }
//! ```

use std::collections::HashMap;
use std::fmt;
use std::time::{Duration, Instant, SystemTime};

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use webauthn_rs::prelude::{
    AuthenticationResult, CreationChallengeResponse, Passkey, PasskeyAuthentication,
    PasskeyRegistration, PublicKeyCredential, RegisterPublicKeyCredential,
    RequestChallengeResponse, Url, Uuid, Webauthn, WebauthnBuilder, WebauthnError,
};

use crate::auth::code::{self, Code, RandomError, os_random};
use crate::auth::session::SessionId;

/// Longest a nickname may be. It is shown in a device list and typed by hand;
/// a cap keeps a 10MB "nickname" out of the database.
pub const MAX_NICKNAME: usize = 64;

/// Pending ceremonies kept at once. Starting one costs memory and needs no
/// credential, so an unauthenticated caller must not be able to grow this
/// without bound.
pub const MAX_PENDING: usize = 8;

/// Wrong enrollment codes before the ticket is burned. The code is 40 bits,
/// so this is not what makes guessing hopeless; it is here so a ticket left
/// open on a screen cannot be ground down at leisure.
pub const ENROLLMENT_ATTEMPTS: u32 = 3;

// ---------------------------------------------------------------------------
// Injected world
// ---------------------------------------------------------------------------

/// What the caller supplies instead of this module reading the world.
///
/// Both clocks are here on purpose. Wall time is what gets stored and shown
/// to a person; monotonic time is what expires a ceremony or a step-up grant,
/// because an operator (or an attacker with the host's clock) moving the wall
/// clock backwards must not extend a window that lets a session open a shell.
pub trait Env {
    /// Wall clock: for `created`, `last_used` and anything persisted.
    fn now(&self) -> SystemTime;
    /// Monotonic clock: for every deadline in this module.
    fn mono(&self) -> Instant;
    /// Fill `buf` with cryptographically strong random bytes.
    fn fill_random(&self, buf: &mut [u8]) -> Result<(), RandomError>;
}

/// The real one.
#[derive(Debug, Clone, Copy, Default)]
pub struct OsEnv;

impl Env for OsEnv {
    fn now(&self) -> SystemTime {
        SystemTime::now()
    }

    fn mono(&self) -> Instant {
        Instant::now()
    }

    fn fill_random(&self, buf: &mut [u8]) -> Result<(), RandomError> {
        os_random(buf)
    }
}

// ---------------------------------------------------------------------------
// Settings
// ---------------------------------------------------------------------------

/// The `[passkey]` section of the config file.
///
/// `rp_id` is the one value that cannot be changed later without breaking
/// every enrolled credential: a passkey is bound to the relying party id it
/// was created for. That is why the design insists on one stable domain in
/// front of every node rather than reaching nodes by their own names.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Settings {
    /// Relying party id: the registrable domain, no scheme and no port.
    pub rp_id: String,
    /// Origin the browser actually loads, scheme and port included.
    pub origin: String,
    /// Shown by the authenticator when it asks to create a credential.
    pub rp_name: String,
    /// The single operator's account name, as the authenticator shows it in
    /// its own list of saved passkeys.
    pub user_name: String,
    /// Whether `app.example.com` may use a credential made for `example.com`.
    pub allow_subdomains: bool,
    /// How long an unfinished ceremony stays valid.
    pub ceremony_timeout_secs: u64,
    /// How long one step-up buys before the passkey is asked for again.
    pub step_up_minutes: u64,
    /// How long an enrollment approval stays open on the approving device.
    pub enrollment_minutes: u64,
    /// Whether an ordinary sign-in must include user verification (a PIN,
    /// a fingerprint, a face) and not merely a tap. Step-up always requires
    /// it, whatever this says.
    pub require_user_verification: bool,
}

impl Default for Settings {
    fn default() -> Settings {
        Settings {
            // Loopback by default, matching `server.listen`. WebAuthn treats
            // localhost as a secure context, so a first run works with no
            // certificate; a real deployment sets the public name here.
            rp_id: "localhost".to_string(),
            origin: "http://localhost:7878".to_string(),
            rp_name: "Deckjack".to_string(),
            user_name: "deckjack".to_string(),
            allow_subdomains: false,
            // The platform UI gives up around two minutes; a server-side
            // window far longer than that only keeps dead challenges alive.
            ceremony_timeout_secs: 120,
            step_up_minutes: 5,
            enrollment_minutes: 5,
            require_user_verification: true,
        }
    }
}

impl Settings {
    pub fn ceremony_timeout(&self) -> Duration {
        Duration::from_secs(self.ceremony_timeout_secs)
    }

    pub fn step_up_ttl(&self) -> Duration {
        Duration::from_secs(self.step_up_minutes * 60)
    }

    pub fn enrollment_ttl(&self) -> Duration {
        Duration::from_secs(self.enrollment_minutes * 60)
    }
}

// ---------------------------------------------------------------------------
// Identifiers
// ---------------------------------------------------------------------------

/// A credential id, as the authenticator chose it.
///
/// Not a secret: the browser sends it in the clear and it is shown in the
/// device list, so a plain `Debug` is right here. Compare it by equality, not
/// in constant time — knowing a credential id gets an attacker nothing
/// without the private key that never leaves the authenticator.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct CredentialId(Vec<u8>);

impl fmt::Debug for CredentialId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "CredentialId({})", self.encode())
    }
}

impl CredentialId {
    pub fn from_bytes(bytes: Vec<u8>) -> CredentialId {
        CredentialId(bytes)
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }

    pub fn encode(&self) -> String {
        URL_SAFE_NO_PAD.encode(&self.0)
    }

    pub fn decode(s: &str) -> Option<CredentialId> {
        URL_SAFE_NO_PAD.decode(s).ok().map(CredentialId)
    }

    /// First eight characters, for a log line that should not carry the whole
    /// id around.
    pub fn short(&self) -> String {
        let e = self.encode();
        e.chars().take(8).collect()
    }
}

/// The single operator's WebAuthn user handle.
///
/// It must be stable for the life of the working directory: the handle is
/// baked into every credential, so regenerating it orphans every passkey
/// already enrolled. It is therefore generated once and persisted beside the
/// credentials (see `sql_migration`), not derived from the node, which the
/// directory outlives when it is carried to another machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UserHandle(Uuid);

impl UserHandle {
    /// A fresh random handle, in RFC 4122 version 4 shape.
    ///
    /// Built from raw bytes rather than `Uuid::new_v4` so this crate does not
    /// depend on the `uuid` crate's own RNG feature being enabled by whoever
    /// pulled webauthn-rs in; the randomness comes from [`Env`] either way,
    /// which is also what makes it testable.
    pub fn generate(env: &dyn Env) -> Result<UserHandle, PasskeyError> {
        let mut b = [0u8; 16];
        env.fill_random(&mut b)?;
        b[6] = (b[6] & 0x0F) | 0x40;
        b[8] = (b[8] & 0x3F) | 0x80;
        Ok(UserHandle(Uuid::from_bytes(b)))
    }

    pub fn from_uuid(uuid: Uuid) -> UserHandle {
        UserHandle(uuid)
    }

    pub fn as_uuid(&self) -> Uuid {
        self.0
    }

    pub fn encode(&self) -> String {
        self.0.to_string()
    }

    pub fn decode(s: &str) -> Option<UserHandle> {
        s.parse().ok().map(UserHandle)
    }
}

/// Handle for an in-flight ceremony.
///
/// 256 random bits, handed to the browser and given back with the assertion.
/// This **is** a bearer value — whoever holds it can complete the ceremony it
/// names — so it gets the same redacting `Debug` as a session id and is never
/// formatted into a log line.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct CeremonyId([u8; 32]);

impl fmt::Debug for CeremonyId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("CeremonyId(<redacted>)")
    }
}

impl CeremonyId {
    pub fn generate(env: &dyn Env) -> Result<CeremonyId, PasskeyError> {
        let mut b = [0u8; 32];
        env.fill_random(&mut b)?;
        Ok(CeremonyId(b))
    }

    /// The value to put in the response to the browser. Named for what it
    /// does: it reveals the handle.
    pub fn encode(&self) -> String {
        URL_SAFE_NO_PAD.encode(self.0)
    }

    pub fn decode(s: &str) -> Option<CeremonyId> {
        // 32 bytes encode to exactly 43 characters.
        if s.len() != 43 {
            return None;
        }
        let v = URL_SAFE_NO_PAD.decode(s).ok()?;
        Some(CeremonyId(v.try_into().ok()?))
    }
}

// ---------------------------------------------------------------------------
// Stored credentials
// ---------------------------------------------------------------------------

/// A credential as Deckjack keeps it.
///
/// Everything here is public key material and metadata; the private key never
/// leaves the authenticator, which is the whole point of the mechanism. The
/// fields that move are `sign_count`, `backup_state` and `last_used`, and they
/// move only through [`Credential::apply`].
#[derive(Debug, Clone)]
pub struct Credential {
    pub id: CredentialId,
    /// What the device is called in the device list ("Pocket 4", "iPhone").
    pub nickname: String,
    pub created: SystemTime,
    pub last_used: Option<SystemTime>,
    /// Highest sign count seen from this credential. Zero means no assertion
    /// has been seen yet, or an authenticator that does not count.
    pub sign_count: u32,
    /// Whether the authenticator said this credential can be backed up.
    pub backup_eligible: bool,
    /// Whether it currently is. This one legitimately changes.
    pub backup_state: bool,
    /// The verifying material, owned by webauthn-rs.
    pub key: Passkey,
}

impl Credential {
    /// Fold a successful assertion into the stored credential.
    ///
    /// The sign count check happens before this is called; by the time we are
    /// here the counter is known to have moved forward (or to be absent).
    fn apply(&mut self, result: &AuthenticationResult, now: SystemTime) {
        // Let webauthn-rs update its own representation too: it tracks more
        // than we surface, and a stale copy is what makes the next assertion
        // fail for no visible reason.
        let _ = self.key.update_credential(result);
        self.sign_count = result.counter();
        self.backup_state = result.backup_state();
        // Eligibility can go from false to true but never back, so OR it in
        // rather than overwrite: a downgrade is the authenticator lying, and
        // believing it would hide a real change.
        self.backup_eligible |= result.backup_eligible();
        self.last_used = Some(now);
    }

    /// Flatten for storage. [`Store`](crate::store::Store) stays a mapper and
    /// never learns what a `Passkey` is.
    pub fn to_row(&self) -> Result<CredentialRow, PasskeyError> {
        Ok(CredentialRow {
            cred_id: self.id.encode(),
            nickname: self.nickname.clone(),
            key_json: serde_json::to_string(&self.key).map_err(PasskeyError::EncodeKey)?,
            sign_count: i64::from(self.sign_count),
            backup_eligible: self.backup_eligible,
            backup_state: self.backup_state,
            created: to_unix(self.created),
            last_used: self.last_used.map(to_unix),
        })
    }

    /// Rebuild from storage.
    pub fn from_row(row: &CredentialRow) -> Result<Credential, PasskeyError> {
        let id = CredentialId::decode(&row.cred_id).ok_or_else(|| PasskeyError::BadStoredRow {
            cred_id: row.cred_id.clone(),
            reason: "credential id is not base64url".to_string(),
        })?;
        let key: Passkey =
            serde_json::from_str(&row.key_json).map_err(|e| PasskeyError::BadStoredRow {
                cred_id: row.cred_id.clone(),
                reason: format!("public key is unreadable: {e}"),
            })?;
        Ok(Credential {
            id,
            nickname: row.nickname.clone(),
            created: from_unix(row.created),
            last_used: row.last_used.map(from_unix),
            // A count that will not fit in a u32 is not a count this
            // credential ever produced; clamping keeps one bad row from
            // refusing every sign-in, and the regression check still bites.
            sign_count: u32::try_from(row.sign_count.max(0)).unwrap_or(u32::MAX),
            backup_eligible: row.backup_eligible,
            backup_state: row.backup_state,
            key,
        })
    }
}

/// One row of the `passkeys` table, in the types SQLite speaks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CredentialRow {
    pub cred_id: String,
    pub nickname: String,
    pub key_json: String,
    pub sign_count: i64,
    pub backup_eligible: bool,
    pub backup_state: bool,
    pub created: i64,
    pub last_used: Option<i64>,
}

// ---------------------------------------------------------------------------
// Ceremonies, grants and outcomes
// ---------------------------------------------------------------------------

/// What a ceremony was started for. A ceremony is spent for exactly this and
/// nothing else.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Purpose {
    Registration,
    SignIn,
    StepUp,
}

impl Purpose {
    pub fn label(&self) -> &'static str {
        match self {
            Purpose::Registration => "registration",
            Purpose::SignIn => "sign-in",
            Purpose::StepUp => "step-up",
        }
    }
}

/// What a remote session is allowed to ask for once it has stepped up.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HostAccess {
    /// A real shell on the host, over a PTY.
    Terminal,
    /// Reading or writing outside the working directory.
    OutsideWorkingDirectory,
}

impl HostAccess {
    pub fn label(&self) -> &'static str {
        match self {
            HostAccess::Terminal => "a terminal on the host",
            HostAccess::OutsideWorkingDirectory => "files outside the working directory",
        }
    }
}

/// How the session asking to step up got here.
///
/// The caller decides this from how the request arrived, not from anything the
/// client claims.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionKind {
    /// One of my own devices, signed in with a passkey.
    Trusted,
    /// A borrowed machine in tether mode, signed in with a one-time code.
    Tether,
}

/// A started ceremony. Returned so the caller can hand both halves out: the
/// id to the client, the challenge to `navigator.credentials`.
#[derive(Debug)]
pub struct Started<T> {
    pub ceremony: CeremonyId,
    pub challenge: T,
}

enum Pending {
    Registration {
        nickname: String,
        state: PasskeyRegistration,
        deadline: Instant,
    },
    SignIn {
        state: PasskeyAuthentication,
        deadline: Instant,
    },
    StepUp {
        session: SessionId,
        access: HostAccess,
        state: PasskeyAuthentication,
        deadline: Instant,
    },
}

impl fmt::Debug for Pending {
    /// The ceremony state is large and prints the raw challenge; the purpose
    /// and whether it has expired are the only parts worth a log line.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Pending")
            .field("purpose", &self.purpose().label())
            .finish_non_exhaustive()
    }
}

impl Pending {
    fn purpose(&self) -> Purpose {
        match self {
            Pending::Registration { .. } => Purpose::Registration,
            Pending::SignIn { .. } => Purpose::SignIn,
            Pending::StepUp { .. } => Purpose::StepUp,
        }
    }

    fn deadline(&self) -> Instant {
        match self {
            Pending::Registration { deadline, .. }
            | Pending::SignIn { deadline, .. }
            | Pending::StepUp { deadline, .. } => *deadline,
        }
    }
}

/// Permission for a new device to enroll, approved on a device already signed
/// in.
///
/// The code reuses [`Code`]: eight Crockford symbols, redacting `Debug`,
/// constant-time compare, and a `reveal()` that exists precisely so the only
/// way the text reaches a screen is a deliberate call.
#[derive(Debug)]
pub struct EnrollmentTicket {
    code: Code,
    approver: SessionId,
    deadline: Instant,
    attempts_left: u32,
}

impl EnrollmentTicket {
    /// The code to read off the approving device. Show it; never log it.
    pub fn reveal(&self) -> String {
        self.code.reveal()
    }

    /// Which session approved the enrollment, for the audit log.
    pub fn approver(&self) -> &SessionId {
        &self.approver
    }

    pub fn attempts_left(&self) -> u32 {
        self.attempts_left
    }
}

/// How a registration was authorized.
#[derive(Debug, Clone, Copy)]
pub enum Enrollment<'a> {
    /// The first credential, when no signed-in device exists to approve it.
    /// Refused as soon as one credential is enrolled.
    First,
    /// The code shown on an already signed-in device.
    Ticket(&'a str),
}

/// A newly enrolled credential, with the row to write.
#[derive(Debug, Clone)]
pub struct Registered {
    pub credential: Credential,
    pub row: CredentialRow,
}

/// A completed sign-in. Deliberately **not** a session: minting one is
/// [`AuthState`](crate::auth::state::AuthState)'s job, and keeping that
/// decision in one place is what stops two ways of being signed in from
/// drifting apart.
#[derive(Debug, Clone)]
pub struct SignIn {
    pub credential: CredentialId,
    pub nickname: String,
    /// Whether the authenticator verified a person, not just a presence.
    pub user_verified: bool,
    /// The credential as it now stands; persist it so the moved sign count
    /// survives a restart. Without this, a restart resets the counter and the
    /// clone check goes blind.
    pub row: CredentialRow,
}

/// A completed step-up: permission for one session to reach the host, for a
/// while.
///
/// Bound to the session that asked. An assertion obtained in one session can
/// therefore not be replayed into another, which is the difference between
/// "this person is here" and "this cookie is here".
#[derive(Debug, Clone)]
pub struct StepUpGrant {
    pub session: SessionId,
    pub access: HostAccess,
    pub credential: CredentialId,
    pub granted_at: SystemTime,
    pub row: CredentialRow,
}

/// Something worth acting on: an audit line, or a lockdown trigger.
///
/// This module never acts on these. It appends them to a vector the caller
/// passes in, which is what keeps it free of I/O and of any opinion about what
/// a lockdown does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    Registered {
        credential: CredentialId,
        nickname: String,
    },
    SignedIn {
        credential: CredentialId,
    },
    SteppedUp {
        credential: CredentialId,
        access: HostAccess,
    },
    /// A ceremony failed. `consecutive` is the run of failures since the last
    /// success: the design makes repeated failed passkey attempts a lockdown
    /// trigger, and this is the number that trigger reads.
    Failed {
        purpose: Purpose,
        consecutive: u32,
    },
    /// The sign count went backwards, or disappeared after being present.
    /// Two authenticators are answering for one credential, which means the
    /// private key was extracted. Lock down and remove the credential.
    CloneSuspected {
        credential: CredentialId,
        stored: u32,
        presented: u32,
    },
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

#[derive(Debug, Error)]
pub enum PasskeyError {
    // The parse error is carried as text rather than as a `#[source]`:
    // naming `url::ParseError` would make this crate depend on `url`
    // directly only to describe a config mistake, and the message is the
    // whole value of it.
    #[error(
        "passkey.origin ({origin}) is not a URL ({reason}); set it to the address you open \
         Deckjack at, scheme and port included, e.g. https://deckjack.example.com"
    )]
    BadOrigin { origin: String, reason: String },

    #[error(
        "passkey.rp_id ({rp_id}) and passkey.origin ({origin}) do not agree: {source}; rp_id is \
         the bare domain of the origin, with no scheme and no port"
    )]
    BadRelyingParty {
        rp_id: String,
        origin: String,
        #[source]
        source: WebauthnError,
    },

    #[error(
        "no passkey is enrolled, so there is nothing to sign in with; enroll one from the host \
         with `deckjack passkey enroll`, or sign in with the one-time code"
    )]
    NoCredentials,

    #[error(
        "a passkey is already enrolled, so a new device needs approval from one that is already \
         signed in: approve it there and enter the code it shows"
    )]
    ApprovalRequired,

    #[error("no enrollment has been approved; approve this device from one that is signed in")]
    NoTicket,

    #[error("that enrollment approval has expired; approve the device again")]
    TicketExpired,

    #[error("wrong enrollment code, {attempts_left} attempt(s) left before it is cancelled")]
    WrongTicket { attempts_left: u32 },

    #[error(
        "that enrollment approval has been cancelled after too many wrong codes; approve the \
         device again"
    )]
    TicketBurned,

    #[error("nickname must be 1 to {MAX_NICKNAME} characters so the device list stays readable")]
    BadNickname,

    #[error(
        "this ceremony is unknown; it has expired, or it was already used, so start again from \
         the sign-in page"
    )]
    UnknownCeremony,

    #[error("this ceremony has expired; start again from the sign-in page")]
    CeremonyExpired,

    #[error(
        "this ceremony was started for {} and cannot be finished as {}; a sign-in is never \
         spendable as a step-up",
        .found.label(), .expected.label()
    )]
    WrongPurpose { expected: Purpose, found: Purpose },

    #[error(
        "too many sign-ins are already in flight ({MAX_PENDING}); wait for them to expire, or \
         restart the server if this persists"
    )]
    TooManyCeremonies,

    #[error("that device already has a passkey here; use it to sign in, or remove it first")]
    AlreadyEnrolled { cred_id: String },

    #[error("no passkey with id {cred_id} is enrolled here")]
    UnknownCredential { cred_id: String },

    #[error(
        "the authenticator did not verify who is using it; a step-up exists to prove a person is \
         present, so unlock the device with its PIN, fingerprint or face and try again"
    )]
    UserVerificationRequired,

    #[error(
        "sign count for passkey {nickname} went backwards (stored {stored}, presented \
         {presented}): the credential has been cloned. Deckjack is locking down; remove this \
         passkey and enroll the device again"
    )]
    SignCountRegression {
        nickname: String,
        stored: u32,
        presented: u32,
    },

    #[error(
        "a borrowed machine in tether mode cannot step up to host access: it holds no passkey of \
         mine, and typing one in on an untrusted keyboard is what tether mode exists to avoid. \
         Use one of my own devices for a terminal"
    )]
    StepUpFromTether,

    #[error("the authenticator's answer was refused: {0}")]
    Ceremony(#[from] WebauthnError),

    #[error("cannot store the public key: {0}")]
    EncodeKey(#[source] serde_json::Error),

    #[error("stored passkey {cred_id} is unusable: {reason}; remove it and enroll again")]
    BadStoredRow { cred_id: String, reason: String },

    #[error(transparent)]
    Random(#[from] RandomError),
}

// ---------------------------------------------------------------------------
// The registry
// ---------------------------------------------------------------------------

/// Enrolled credentials, in-flight ceremonies and live step-up grants.
pub struct Passkeys {
    webauthn: Webauthn,
    settings: Settings,
    user: UserHandle,
    credentials: Vec<Credential>,
    pending: HashMap<CeremonyId, Pending>,
    /// One ticket at a time. Approving an enrollment is a deliberate act, and
    /// a queue of live approvals is only attack surface.
    ticket: Option<EnrollmentTicket>,
    /// Keyed by the session that proved itself and what it proved itself for,
    /// so stepping up for files does not silently hand over a shell.
    grants: HashMap<(SessionId, HostAccess), Instant>,
    consecutive_failures: u32,
}

impl fmt::Debug for Passkeys {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Passkeys")
            .field("rp_id", &self.settings.rp_id)
            .field("credentials", &self.credentials.len())
            .field("pending", &self.pending.len())
            .field("grants", &self.grants.len())
            .field("consecutive_failures", &self.consecutive_failures)
            .finish()
    }
}

impl Passkeys {
    /// Build the relying party from settings. Fails loudly on a bad origin or
    /// a mismatched `rp_id`, because the alternative is a sign-in page that
    /// looks fine and whose every ceremony is rejected by the browser.
    pub fn new(settings: Settings, user: UserHandle) -> Result<Passkeys, PasskeyError> {
        let origin = Url::parse(&settings.origin).map_err(|e| PasskeyError::BadOrigin {
            origin: settings.origin.clone(),
            reason: e.to_string(),
        })?;
        let webauthn = WebauthnBuilder::new(&settings.rp_id, &origin)
            .and_then(|b| {
                b.rp_name(&settings.rp_name)
                    .allow_subdomains(settings.allow_subdomains)
                    .timeout(settings.ceremony_timeout())
                    .build()
            })
            .map_err(|source| PasskeyError::BadRelyingParty {
                rp_id: settings.rp_id.clone(),
                origin: settings.origin.clone(),
                source,
            })?;

        Ok(Passkeys {
            webauthn,
            settings,
            user,
            credentials: Vec::new(),
            pending: HashMap::new(),
            ticket: None,
            grants: HashMap::new(),
            consecutive_failures: 0,
        })
    }

    /// Load credentials read from the store at startup.
    ///
    /// Separate from registration on purpose: it runs no ceremony, needs no
    /// approval and emits no event, because the enrollment already happened —
    /// this is only remembering it.
    pub fn restore(&mut self, rows: &[CredentialRow]) -> Result<(), PasskeyError> {
        for row in rows {
            self.credentials.push(Credential::from_row(row)?);
        }
        Ok(())
    }

    pub fn settings(&self) -> &Settings {
        &self.settings
    }

    pub fn user(&self) -> UserHandle {
        self.user
    }

    pub fn credentials(&self) -> &[Credential] {
        &self.credentials
    }

    pub fn credential(&self, id: &CredentialId) -> Option<&Credential> {
        self.credentials.iter().find(|c| c.id == *id)
    }

    /// Whether a passkey sign-in is possible at all. When this is false the
    /// UI must offer the one-time code instead of an empty prompt.
    pub fn is_enrolled(&self) -> bool {
        !self.credentials.is_empty()
    }

    /// Failures since the last success, which is what the lockdown trigger
    /// for "repeated failed passkey attempts" counts.
    pub fn consecutive_failures(&self) -> u32 {
        self.consecutive_failures
    }

    pub fn rename(
        &mut self,
        id: &CredentialId,
        nickname: &str,
    ) -> Result<&Credential, PasskeyError> {
        let nickname = check_nickname(nickname)?;
        let cred = self
            .credentials
            .iter_mut()
            .find(|c| c.id == *id)
            .ok_or_else(|| PasskeyError::UnknownCredential {
                cred_id: id.encode(),
            })?;
        cred.nickname = nickname;
        Ok(cred)
    }

    /// Remove a credential. Allowed even for the last one: the one-time code
    /// path still exists, so this cannot lock the operator out, and a lost
    /// device must be removable from whatever is left.
    pub fn remove(&mut self, id: &CredentialId) -> Result<Credential, PasskeyError> {
        let at = self
            .credentials
            .iter()
            .position(|c| c.id == *id)
            .ok_or_else(|| PasskeyError::UnknownCredential {
                cred_id: id.encode(),
            })?;
        // Any step-up a stolen device bought dies with the credential, not
        // when its window happens to run out.
        self.grants.clear();
        Ok(self.credentials.remove(at))
    }

    // ---- enrollment ------------------------------------------------------

    /// Approve a new device from a session that is already signed in.
    ///
    /// Replaces any outstanding approval: the newest one is the one the
    /// operator means.
    pub fn approve_enrollment(
        &mut self,
        approver: SessionId,
        env: &dyn Env,
    ) -> Result<&EnrollmentTicket, PasskeyError> {
        // The code comes from `Env` rather than `Code::generate`, which reads
        // the OS source directly: the point of the injection is that a test
        // can pin this value.
        let mut bytes = [0u8; code::LEN];
        env.fill_random(&mut bytes)?;
        let ticket = EnrollmentTicket {
            code: Code::from_random_bytes(bytes),
            approver,
            deadline: env.mono() + self.settings.enrollment_ttl(),
            attempts_left: ENROLLMENT_ATTEMPTS,
        };
        Ok(self.ticket.insert(ticket))
    }

    pub fn cancel_enrollment(&mut self) -> bool {
        self.ticket.take().is_some()
    }

    /// Begin registering a credential for this device.
    pub fn start_registration(
        &mut self,
        enrollment: Enrollment<'_>,
        nickname: &str,
        env: &dyn Env,
    ) -> Result<Started<CreationChallengeResponse>, PasskeyError> {
        let nickname = check_nickname(nickname)?;
        self.authorize_enrollment(enrollment, env)?;

        // Tell the authenticator what it already holds, so a device that is
        // enrolled cannot quietly create a second credential and leave two
        // rows for one device in the list.
        let exclude = if self.credentials.is_empty() {
            None
        } else {
            Some(
                self.credentials
                    .iter()
                    .map(|c| c.id.as_bytes().to_vec())
                    .collect(),
            )
        };

        // Both names describe the *account*, not the device: a user name only
        // has to distinguish accounts within one relying party, and Deckjack
        // has exactly one operator. The device's nickname is Deckjack's own
        // metadata and stays out of the credential.
        let (challenge, state) = self.webauthn.start_passkey_registration(
            self.user.as_uuid(),
            &self.settings.user_name,
            &self.settings.user_name,
            exclude,
        )?;

        let ceremony = self.insert_pending(
            Pending::Registration {
                nickname,
                state,
                deadline: env.mono() + self.settings.ceremony_timeout(),
            },
            env,
        )?;
        Ok(Started {
            ceremony,
            challenge,
        })
    }

    /// Finish registration. The credential is already in the registry when
    /// this returns; persist the row so it survives a restart.
    pub fn finish_registration(
        &mut self,
        ceremony: &CeremonyId,
        reply: &RegisterPublicKeyCredential,
        env: &dyn Env,
        events: &mut Vec<Event>,
    ) -> Result<Registered, PasskeyError> {
        let pending = self.take_pending(ceremony, Purpose::Registration, env)?;
        let Pending::Registration {
            nickname, state, ..
        } = pending
        else {
            // take_pending already matched the purpose.
            unreachable!("purpose checked above");
        };

        let key = match self.webauthn.finish_passkey_registration(reply, &state) {
            Ok(key) => key,
            Err(e) => {
                self.fail(Purpose::Registration, events);
                return Err(e.into());
            }
        };

        let id = CredentialId::from_bytes(key.cred_id().clone());
        if self.credential(&id).is_some() {
            self.fail(Purpose::Registration, events);
            return Err(PasskeyError::AlreadyEnrolled {
                cred_id: id.encode(),
            });
        }

        // The approval is spent by the enrollment it approved, not by time.
        self.ticket = None;
        self.consecutive_failures = 0;

        // Registration does not report a sign count, so the stored count
        // starts at zero and the first assertion sets it. Zero therefore has
        // to mean "nothing seen yet", which is exactly how the regression
        // check reads it.
        let credential = Credential {
            id: id.clone(),
            nickname: nickname.clone(),
            created: env.now(),
            last_used: None,
            sign_count: 0,
            backup_eligible: false,
            backup_state: false,
            key,
        };
        let row = credential.to_row()?;
        self.credentials.push(credential.clone());
        events.push(Event::Registered {
            credential: id,
            nickname,
        });
        Ok(Registered { credential, row })
    }

    fn authorize_enrollment(
        &mut self,
        enrollment: Enrollment<'_>,
        env: &dyn Env,
    ) -> Result<(), PasskeyError> {
        match enrollment {
            Enrollment::First => {
                // The design's rule, enforced here rather than in the route:
                // once one credential exists there is a signed-in device to
                // approve the next, so an unapproved enrollment is refused
                // however the request reached us.
                if self.is_enrolled() {
                    return Err(PasskeyError::ApprovalRequired);
                }
                Ok(())
            }
            Enrollment::Ticket(code) => {
                let ticket = self.ticket.as_mut().ok_or(PasskeyError::NoTicket)?;
                if env.mono() >= ticket.deadline {
                    self.ticket = None;
                    return Err(PasskeyError::TicketExpired);
                }
                if ticket.code.matches(code) {
                    return Ok(());
                }
                ticket.attempts_left = ticket.attempts_left.saturating_sub(1);
                let left = ticket.attempts_left;
                if left == 0 {
                    self.ticket = None;
                    return Err(PasskeyError::TicketBurned);
                }
                Err(PasskeyError::WrongTicket {
                    attempts_left: left,
                })
            }
        }
    }

    // ---- sign-in ---------------------------------------------------------

    /// Begin a passkey sign-in.
    pub fn start_sign_in(
        &mut self,
        env: &dyn Env,
    ) -> Result<Started<RequestChallengeResponse>, PasskeyError> {
        let keys = self.keys()?;
        let (challenge, state) = self.webauthn.start_passkey_authentication(&keys)?;
        let ceremony = self.insert_pending(
            Pending::SignIn {
                state,
                deadline: env.mono() + self.settings.ceremony_timeout(),
            },
            env,
        )?;
        Ok(Started {
            ceremony,
            challenge,
        })
    }

    /// Finish a passkey sign-in.
    ///
    /// Returns what was proved, not a session: the caller mints the session so
    /// that expiry, the session cap and the cookie stay in one place.
    pub fn finish_sign_in(
        &mut self,
        ceremony: &CeremonyId,
        reply: &PublicKeyCredential,
        env: &dyn Env,
        events: &mut Vec<Event>,
    ) -> Result<SignIn, PasskeyError> {
        let pending = self.take_pending(ceremony, Purpose::SignIn, env)?;
        let Pending::SignIn { state, .. } = pending else {
            unreachable!("purpose checked above");
        };

        let result = match self.webauthn.finish_passkey_authentication(reply, &state) {
            Ok(result) => result,
            Err(e) => {
                self.fail(Purpose::SignIn, events);
                return Err(e.into());
            }
        };

        let (id, nickname, row) = match self.accept(
            &result,
            self.settings.require_user_verification,
            env,
            events,
        ) {
            Ok(v) => v,
            Err(e) => {
                self.fail(Purpose::SignIn, events);
                return Err(e);
            }
        };

        self.consecutive_failures = 0;
        events.push(Event::SignedIn {
            credential: id.clone(),
        });
        Ok(SignIn {
            credential: id,
            nickname,
            user_verified: result.user_verified(),
            row,
        })
    }

    // ---- step-up ---------------------------------------------------------

    /// Ask an already signed-in session to prove the passkey again before it
    /// reaches the host.
    pub fn start_step_up(
        &mut self,
        session: SessionId,
        access: HostAccess,
        kind: SessionKind,
        env: &dyn Env,
    ) -> Result<Started<RequestChallengeResponse>, PasskeyError> {
        if kind == SessionKind::Tether {
            return Err(PasskeyError::StepUpFromTether);
        }
        let keys = self.keys()?;
        let (challenge, state) = self.webauthn.start_passkey_authentication(&keys)?;
        let ceremony = self.insert_pending(
            Pending::StepUp {
                session,
                access,
                state,
                deadline: env.mono() + self.settings.ceremony_timeout(),
            },
            env,
        )?;
        Ok(Started {
            ceremony,
            challenge,
        })
    }

    /// Finish a step-up. User verification is required here whatever the
    /// config says: the point of a step-up is to prove a person is at the
    /// device, and a bare presence tap proves only that something tapped.
    pub fn finish_step_up(
        &mut self,
        ceremony: &CeremonyId,
        reply: &PublicKeyCredential,
        env: &dyn Env,
        events: &mut Vec<Event>,
    ) -> Result<StepUpGrant, PasskeyError> {
        let pending = self.take_pending(ceremony, Purpose::StepUp, env)?;
        let Pending::StepUp {
            session,
            access,
            state,
            ..
        } = pending
        else {
            unreachable!("purpose checked above");
        };

        let result = match self.webauthn.finish_passkey_authentication(reply, &state) {
            Ok(result) => result,
            Err(e) => {
                self.fail(Purpose::StepUp, events);
                return Err(e.into());
            }
        };

        let (id, _, row) = match self.accept(&result, true, env, events) {
            Ok(v) => v,
            Err(e) => {
                self.fail(Purpose::StepUp, events);
                return Err(e);
            }
        };

        self.consecutive_failures = 0;
        let granted_at = env.now();
        self.grants.insert(
            (session.clone(), access),
            env.mono() + self.settings.step_up_ttl(),
        );
        events.push(Event::SteppedUp {
            credential: id.clone(),
            access,
        });
        Ok(StepUpGrant {
            session,
            access,
            credential: id,
            granted_at,
            row,
        })
    }

    /// Whether this session may reach the host right now.
    pub fn has_step_up(&self, session: &SessionId, access: HostAccess, env: &dyn Env) -> bool {
        self.grants
            .get(&(session.clone(), access))
            .is_some_and(|deadline| env.mono() < *deadline)
    }

    /// How long this session's grant has left, for the UI to show.
    pub fn step_up_remaining(
        &self,
        session: &SessionId,
        access: HostAccess,
        env: &dyn Env,
    ) -> Option<Duration> {
        let deadline = self.grants.get(&(session.clone(), access))?;
        deadline.checked_duration_since(env.mono())
    }

    /// Drop every grant held by one session, as logout must.
    pub fn revoke_session(&mut self, session: &SessionId) {
        self.grants.retain(|(s, _), _| s != session);
        self.pending.retain(|_, p| match p {
            Pending::StepUp { session: s, .. } => s != session,
            _ => true,
        });
    }

    /// Drop every grant and every ceremony in flight. The kill switch calls
    /// this; so does anything that revokes all sessions.
    pub fn revoke_all(&mut self) {
        self.grants.clear();
        self.pending.clear();
        self.ticket = None;
    }

    // ---- internals -------------------------------------------------------

    /// The keys to put in an allow-list. An empty list would ask the browser
    /// to pick from nothing, which surfaces as an opaque failure in the
    /// authenticator UI rather than a message we can explain.
    fn keys(&self) -> Result<Vec<Passkey>, PasskeyError> {
        if self.credentials.is_empty() {
            return Err(PasskeyError::NoCredentials);
        }
        Ok(self.credentials.iter().map(|c| c.key.clone()).collect())
    }

    /// The checks that are the same for a sign-in and a step-up: the
    /// credential is one of ours, the sign count moved forward, and the
    /// authenticator verified a person if we asked it to.
    fn accept(
        &mut self,
        result: &AuthenticationResult,
        require_uv: bool,
        env: &dyn Env,
        events: &mut Vec<Event>,
    ) -> Result<(CredentialId, String, CredentialRow), PasskeyError> {
        let id = CredentialId::from_bytes(result.cred_id().clone());

        // Look up before anything else: webauthn-rs only verified the
        // signature against the key we gave it, so an id we cannot find means
        // our own allow-list and credential list disagree.
        let at = self
            .credentials
            .iter()
            .position(|c| c.id == id)
            .ok_or_else(|| PasskeyError::UnknownCredential {
                cred_id: id.encode(),
            })?;

        if require_uv && !result.user_verified() {
            return Err(PasskeyError::UserVerificationRequired);
        }

        let stored = self.credentials[at].sign_count;
        let presented = result.counter();
        if !sign_count_ok(stored, presented) {
            let nickname = self.credentials[at].nickname.clone();
            events.push(Event::CloneSuspected {
                credential: id.clone(),
                stored,
                presented,
            });
            return Err(PasskeyError::SignCountRegression {
                nickname,
                stored,
                presented,
            });
        }

        let cred = &mut self.credentials[at];
        cred.apply(result, env.now());
        Ok((id, cred.nickname.clone(), cred.to_row()?))
    }

    fn insert_pending(
        &mut self,
        pending: Pending,
        env: &dyn Env,
    ) -> Result<CeremonyId, PasskeyError> {
        self.prune(env);
        if self.pending.len() >= MAX_PENDING {
            return Err(PasskeyError::TooManyCeremonies);
        }
        let id = CeremonyId::generate(env)?;
        self.pending.insert(id.clone(), pending);
        Ok(id)
    }

    /// Take a ceremony out of the map, whatever happens next.
    ///
    /// Removing before verifying is deliberate: a challenge that can be
    /// retried is a challenge that can be replayed, so one attempt is all a
    /// ceremony ever gets.
    fn take_pending(
        &mut self,
        ceremony: &CeremonyId,
        expected: Purpose,
        env: &dyn Env,
    ) -> Result<Pending, PasskeyError> {
        let pending = self
            .pending
            .remove(ceremony)
            .ok_or(PasskeyError::UnknownCeremony)?;
        if env.mono() >= pending.deadline() {
            return Err(PasskeyError::CeremonyExpired);
        }
        let found = pending.purpose();
        if found != expected {
            return Err(PasskeyError::WrongPurpose { expected, found });
        }
        Ok(pending)
    }

    fn fail(&mut self, purpose: Purpose, events: &mut Vec<Event>) {
        self.consecutive_failures = self.consecutive_failures.saturating_add(1);
        events.push(Event::Failed {
            purpose,
            consecutive: self.consecutive_failures,
        });
    }

    /// Drop expired ceremonies, grants and approvals. Cheap and called on
    /// every start; there is no timer in this module to do it.
    pub fn prune(&mut self, env: &dyn Env) {
        let now = env.mono();
        self.pending.retain(|_, p| now < p.deadline());
        self.grants.retain(|_, deadline| now < *deadline);
        if self.ticket.as_ref().is_some_and(|t| now >= t.deadline) {
            self.ticket = None;
        }
    }
}

/// Why a counter is checked at all: the authenticator increments it on every
/// assertion, so a value that does not move forward means two devices are
/// answering for one credential — that is, the private key was extracted.
///
/// The exception is real and must not be treated as a failure: most platform
/// passkeys (iCloud Keychain, Google Password Manager) do not implement a
/// counter and always report zero, because the credential is synced between
/// devices by design. Zero from a credential that has only ever reported zero
/// is therefore normal. Zero *after* a non-zero count is not.
pub fn sign_count_ok(stored: u32, presented: u32) -> bool {
    if presented == 0 {
        stored == 0
    } else {
        presented > stored
    }
}

fn check_nickname(nickname: &str) -> Result<String, PasskeyError> {
    let trimmed = nickname.trim();
    if trimmed.is_empty() || trimmed.chars().count() > MAX_NICKNAME {
        return Err(PasskeyError::BadNickname);
    }
    Ok(trimmed.to_string())
}

/// Same representation as [`crate::store`] uses, for the same reason: seconds
/// are enough for anything shown to a person, and a clock before the epoch is
/// clamped rather than panicked on.
fn to_unix(t: SystemTime) -> i64 {
    match t.duration_since(SystemTime::UNIX_EPOCH) {
        Ok(d) => d.as_secs() as i64,
        Err(_) => 0,
    }
}

fn from_unix(secs: i64) -> SystemTime {
    SystemTime::UNIX_EPOCH + Duration::from_secs(secs.max(0) as u64)
}
