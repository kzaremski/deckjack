//! Passkeys, wired: the state database, the HTTP ceremonies, and the session
//! kind that step-up exists to tell apart.
//!
//! [`crate::passkey`] is the whole WebAuthn implementation — enrollment,
//! sign-in, step-up, clone detection — and it is deliberately free of I/O:
//! it takes its clocks and its randomness through [`Env`], hands credentials
//! out as [`CredentialRow`] and reads them back through
//! [`Passkeys::restore`]. What it has never had is anything that calls it.
//! Nothing constructed a [`Passkeys`], nothing wrote a row, and so no
//! credential survived a restart; the tests stayed green and the feature
//! could not happen.
//!
//! This module is the other half, and it is three things:
//!
//! 1. **Persistence.** [`PasskeyStore`] is an extension trait on
//!    [`Store`](crate::store::Store), so `store.passkeys()?` and
//!    `store.upsert_passkey(&row)?` mean what `passkey.rs`'s own doctest
//!    already says they mean. The SQL lives here rather than in `store.rs`
//!    for the reason `Store::with_conn` gives: the knowledge of what a
//!    column means belongs next to the type it means it for.
//! 2. **Construction.** [`PasskeyCtx::start`] reads the user handle, mints
//!    one if the directory has never had one, and restores every stored
//!    credential into the registry. It returns `Err` rather than an empty
//!    registry, because a node that comes up with no credentials looks
//!    exactly like a node that has none — the only symptom is the operator
//!    being unable to sign in, long after the log line scrolled past.
//! 3. **The routes.** [`passkey_router`], modeled on
//!    [`crate::lockdown::unlock_router`]: its own state type, its own `Arc`'d
//!    context, merged into the app beside [`crate::http::router`].
//!
//! # What a session kind is for
//!
//! Migration 7 added `sessions.kind`, defaulted to `'tether'`. A passkey
//! sign-in records `'passkey'`, and `/passkey/step-up/begin` reads it back:
//! a borrowed machine in tether mode holds no credential of mine, so there
//! is nothing for it to step up with, and [`Passkeys::start_step_up`]
//! refuses it by name. The column is read, never trusted from the client —
//! a request that says what kind of session it is would be a request that
//! chooses its own privileges.
//!
//! # Why lockdown still uses `PasskeysUnavailable`
//!
//! This module does **not** implement [`crate::lockdown::PasskeyVerifier`].
//! That is the conclusion, not an omission, and it is reached from three
//! facts in the source rather than from taste:
//!
//! - [`PasskeyAssertion`](crate::lockdown::PasskeyAssertion) is an opaque
//!   byte string with no ceremony handle in it, and **nothing in `src/`
//!   constructs `Identity::Passkey` at all** — `unlock_submit` builds
//!   `Identity::HostConsole` and there is no `system.unlock` command. A
//!   verifier written today could only be reached by a caller that does not
//!   exist.
//! - A WebAuthn assertion is only meaningful against the challenge it
//!   answers, and the only way to get one out of `passkey.rs` is
//!   `start_sign_in` or `start_step_up`, both of which stamp a [`Purpose`]
//!   that `take_pending` then insists on. There is no `Purpose::Unlock`.
//!   Spending a sign-in ceremony as an unlock is precisely the substitution
//!   that module refuses to allow between sign-in and step-up, and the
//!   reason it refuses is the reason not to do it here.
//! - `ALLOWED_WHILE_LOCKED` is `["/unlock", "/healthz"]`. Every route in
//!   this file is refused by [`crate::lockdown::gate`] while the node is
//!   locked down, which is correct, and which also means no browser can
//!   obtain a challenge during the one situation an unlock verifier is for.
//!
//! So the honest options were a real verifier nothing can call, or a
//! verifier that accepts an assertion it never challenged. The second is
//! what `PasskeysUnavailable`'s own comment warns against: a stub that
//! silently weakens a kill switch is worse than no kill switch, and a
//! verifier that cannot bind a challenge is that stub wearing a better name.
//! `PasskeysUnavailable` stays, the usable unlock stays the host token plus
//! a recovery code, and [`Policy::allow_host_console_identity`] must stay
//! `true` — flipping it to `false` today would make every lockdown permanent.
//!
//! # `require_user_verification = false` is inert, and now says so out loud
//!
//! webauthn-rs 0.6.2 hard-codes `UserVerificationPolicy::Required` inside
//! `start_passkey_authentication`, so an unverified assertion is refused
//! before `passkey.rs` ever sees it. The setting therefore cannot be made to
//! do what its name says without abandoning the `Passkey` API, which is
//! `passkey.rs`'s call to make and not this file's. What it *can* do is stop
//! being quiet: [`PasskeyCtx::start`] warns when it is `false`, and names the
//! one thing setting it false really does — it disables the second-line
//! check in `Passkeys::accept`, so a future webauthn-rs that stopped
//! requiring UV would be accepted here with nothing left to catch it.

use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::SystemTime;

use axum::body::Bytes;
use axum::extract::{ConnectInfo, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use rusqlite::params;
use serde::{Deserialize, Serialize};
use serde_json::json;
use thiserror::Error;

use crate::auth::session::{self, Session, SessionHash, SessionId};
use crate::auth::state::{AuthState, Now, Refusal, VerifyOutcome};
use crate::passkey::{
    CredentialId, CredentialRow, Enrollment, Env, Event, HostAccess, OsEnv, PasskeyError, Passkeys,
    SessionKind, Settings, UserHandle,
};
use crate::store::{Store, StoreError};

/// Where every route in [`passkey_router`] lives. One prefix, so the shell's
/// CSP and anything that reasons about which paths run a ceremony have a
/// single thing to name.
pub const MOUNT: &str = "/passkey";

// ---------------------------------------------------------------------------
// Persistence
// ---------------------------------------------------------------------------

/// The `passkeys`, `passkey_user` and `sessions.kind` statements.
///
/// An extension trait on [`Store`] rather than functions taking one, because
/// `passkey.rs`'s doctest already writes `store.passkeys()?` and
/// `store.upsert_passkey(&signed_in.row)?`. Those names are the documented
/// shape; this makes them true without `store.rs` learning what a `Passkey`
/// is. Every method reaches the connection through
/// [`Store::with_conn`](crate::store::Store::with_conn), which borrows it for
/// the call and cannot keep it — that is what preserves the one connection,
/// the owner-only file, and the promise that no session cookie reaches disk.
pub trait PasskeyStore {
    /// Every enrolled credential, oldest first, for [`Passkeys::restore`].
    fn passkeys(&self) -> Result<Vec<CredentialRow>, StoreError>;

    /// Write a credential, or move an existing one forward.
    ///
    /// An upsert here is right where it is wrong for the user handle: the
    /// sign count, the backup state and `last_used` are *supposed* to move,
    /// and every assertion moves them. A row that did not get written back
    /// leaves the stored count behind the real one, which costs nothing on
    /// the next sign-in and everything after a restart, when the clone check
    /// would otherwise be comparing against zero.
    fn upsert_passkey(&self, row: &CredentialRow) -> Result<(), StoreError>;

    /// Remove one credential. `true` when a row actually went, so a lost
    /// device that was never stored cannot be reported as revoked.
    fn delete_passkey(&self, id: &CredentialId) -> Result<bool, StoreError>;

    /// The stored user handle, as text.
    ///
    /// Text rather than a [`UserHandle`] on purpose: a handle that is not a
    /// UUID has to fail the start with the bad value in the message, and
    /// [`StoreError`] has no variant that could say that. Decoding is
    /// [`PasskeyCtx::start`]'s job, where the error can be about what it
    /// means.
    fn passkey_user(&self) -> Result<Option<String>, StoreError>;

    /// Store the handle **only if there is not one already**, and return
    /// whichever handle the row now holds.
    ///
    /// `passkey.rs` on [`UserHandle`]: "It must be stable for the life of the
    /// working directory: the handle is baked into every credential, so
    /// regenerating it orphans every passkey already enrolled." That is why
    /// this is `ON CONFLICT(id) DO NOTHING` and not an upsert — the statement
    /// itself makes the overwrite impossible, instead of every caller being
    /// trusted to check first. The return value is read back after the
    /// insert, so a caller that lost the race is told the real handle rather
    /// than the one it offered.
    fn set_passkey_user(
        &self,
        handle: UserHandle,
        created: SystemTime,
    ) -> Result<String, StoreError>;

    /// Record how a session proved itself. `false` when no such session row
    /// exists.
    fn set_session_kind(&self, hash: &SessionHash, kind: SessionKind) -> Result<bool, StoreError>;

    /// How a session proved itself, as the database has it.
    fn session_kind(&self, hash: &SessionHash) -> Result<Option<SessionKind>, StoreError>;
}

/// `'passkey'` and `'tether'`, the two values migration 7 named.
fn kind_text(kind: SessionKind) -> &'static str {
    match kind {
        SessionKind::Trusted => "passkey",
        SessionKind::Tether => "tether",
    }
}

/// Anything that is not exactly `'passkey'` reads as tether, including a
/// value some later version writes that this build does not know. The weaker
/// kind is the safe reading of an unreadable one: it refuses a step-up, and
/// refusing a terminal to a session that deserved one is an inconvenience,
/// while granting one to a borrowed machine is the thing tether mode exists
/// to prevent.
fn kind_from_text(text: &str) -> SessionKind {
    if text == kind_text(SessionKind::Trusted) {
        SessionKind::Trusted
    } else {
        SessionKind::Tether
    }
}

/// Same representation as `store.rs` and `passkey.rs` use, and clamped the
/// same way: a clock before the epoch is stored as the epoch rather than
/// panicking inside a sign-in.
fn to_unix(t: SystemTime) -> i64 {
    match t.duration_since(SystemTime::UNIX_EPOCH) {
        Ok(d) => d.as_secs() as i64,
        Err(_) => 0,
    }
}

impl PasskeyStore for Store {
    fn passkeys(&self) -> Result<Vec<CredentialRow>, StoreError> {
        self.with_conn(|conn| {
            let mut stmt = conn.prepare(
                "SELECT cred_id, nickname, key_json, sign_count, backup_eligible, backup_state,
                        created, last_used
                 FROM passkeys
                 ORDER BY created",
            )?;
            let rows = stmt.query_map([], |r| {
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
            })?;
            rows.collect()
        })
    }

    fn upsert_passkey(&self, row: &CredentialRow) -> Result<(), StoreError> {
        self.with_conn(|conn| {
            conn.execute(
                "INSERT INTO passkeys
                    (cred_id, nickname, key_json, sign_count, backup_eligible, backup_state,
                     created, last_used)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
                 ON CONFLICT(cred_id) DO UPDATE SET
                     nickname        = excluded.nickname,
                     key_json        = excluded.key_json,
                     sign_count      = excluded.sign_count,
                     backup_eligible = excluded.backup_eligible,
                     backup_state    = excluded.backup_state,
                     last_used       = excluded.last_used",
                params![
                    row.cred_id,
                    row.nickname,
                    row.key_json,
                    row.sign_count,
                    row.backup_eligible,
                    row.backup_state,
                    row.created,
                    row.last_used,
                ],
            )?;
            Ok(())
        })
    }

    fn delete_passkey(&self, id: &CredentialId) -> Result<bool, StoreError> {
        self.with_conn(|conn| {
            let gone = conn.execute(
                "DELETE FROM passkeys WHERE cred_id = ?1",
                params![id.encode()],
            )?;
            Ok(gone > 0)
        })
    }

    fn passkey_user(&self) -> Result<Option<String>, StoreError> {
        self.with_conn(|conn| {
            use rusqlite::OptionalExtension as _;
            conn.query_row("SELECT handle FROM passkey_user WHERE id = 1", [], |r| {
                r.get(0)
            })
            .optional()
        })
    }

    fn set_passkey_user(
        &self,
        handle: UserHandle,
        created: SystemTime,
    ) -> Result<String, StoreError> {
        self.with_conn(|conn| {
            conn.execute(
                "INSERT INTO passkey_user (id, handle, created) VALUES (1, ?1, ?2)
                 ON CONFLICT(id) DO NOTHING",
                params![handle.encode(), to_unix(created)],
            )?;
            conn.query_row("SELECT handle FROM passkey_user WHERE id = 1", [], |r| {
                r.get(0)
            })
        })
    }

    fn set_session_kind(&self, hash: &SessionHash, kind: SessionKind) -> Result<bool, StoreError> {
        self.with_conn(|conn| {
            let changed = conn.execute(
                "UPDATE sessions SET kind = ?2 WHERE hash = ?1",
                params![hash.hex(), kind_text(kind)],
            )?;
            Ok(changed > 0)
        })
    }

    fn session_kind(&self, hash: &SessionHash) -> Result<Option<SessionKind>, StoreError> {
        self.with_conn(|conn| {
            use rusqlite::OptionalExtension as _;
            let text: Option<String> = conn
                .query_row(
                    "SELECT kind FROM sessions WHERE hash = ?1",
                    params![hash.hex()],
                    |r| r.get(0),
                )
                .optional()?;
            Ok(text.as_deref().map(kind_from_text))
        })
    }
}

// ---------------------------------------------------------------------------
// Construction
// ---------------------------------------------------------------------------

#[derive(Debug, Error)]
pub enum StartError {
    #[error(
        "the enrolled passkeys cannot be read from the state database, so this node would come \
         up with none; the only symptom of that is being unable to sign in, long after this \
         line has scrolled past, so it refuses to start instead: {source}"
    )]
    Read {
        #[source]
        source: StoreError,
    },

    #[error(
        "the WebAuthn user handle cannot be written to the state database, so a handle minted \
         now would be minted again at the next start and every passkey enrolled in between \
         would be orphaned: {source}"
    )]
    Write {
        #[source]
        source: StoreError,
    },

    #[error(
        "the WebAuthn user handle in the state database is {found:?}, which is not a UUID; \
         every enrolled credential is bound to that handle, so inventing a replacement would \
         orphan all of them. Repair the passkey_user row, or remove it together with every \
         row in passkeys and enroll again"
    )]
    BadUserHandle { found: String },

    #[error(transparent)]
    Passkey(#[from] PasskeyError),
}

// `AdmitError` is gone, and its absence is the fix. It had one variant,
// `SessionsFull`, which `sign_in_finish` turned into a 503: a verified
// passkey assertion refused a session because four forgotten browser tabs
// held every slot, for up to `session_max` — twenty-four hours by default —
// while the one-time code path quietly evicted the stalest session and
// carried on. Two sign-in paths, two answers to "the node is full", and no
// test could see the disagreement because neither path's tests knew the
// other existed. `AuthState::admit` now enforces the cap by evicting,
// exactly as `AuthState::login` does, so the only way left to be refused is
// the one `login` has too: a blocked address or the global lock, which is a
// `Refusal` and not a kind of error this module has to name.

/// Something worth acting on that happened during a ceremony, with the
/// address it happened from.
///
/// Carried to a watcher rather than acted on here, for the same reason
/// `http::Ctx` carries its sign-in watchers: this module has no business
/// knowing the kill switch exists. [`Event::Failed`] is what
/// `lockdown::Observation::FailedPasskey` counts, and
/// [`Event::CloneSuspected`] is a lockdown trigger rather than a warning —
/// but which of those is true is a decision for whoever assembles the node.
#[derive(Debug, Clone)]
pub struct Happening {
    pub ip: IpAddr,
    pub event: Event,
}

type Watch = Box<dyn Fn(&Happening) + Send + Sync>;

/// Everything the passkey routes need, assembled once at startup.
pub struct PasskeyCtx {
    /// The registry. Locked for one call and never across an await: a guard
    /// alive in an async body makes the future non-`Send`, and axum requires
    /// `Send` futures. No handler in this file awaits anything while it is
    /// held.
    keys: Mutex<Passkeys>,
    /// Shared with [`crate::http::router`] rather than rebuilt, because a
    /// passkey sign-in has to mint an **ordinary** session in the same
    /// `AuthState`. Two kinds of being signed in would be two sets of rules
    /// to keep in step, and they would not stay in step.
    http: Arc<crate::http::Ctx>,
    store: Arc<Mutex<Store>>,
    env: Box<dyn Env + Send + Sync>,
    watchers: Vec<Watch>,
}

impl std::fmt::Debug for PasskeyCtx {
    /// The registry's own `Debug` is the interesting part — how many
    /// credentials, how many ceremonies in flight, how many consecutive
    /// failures — and it shows no key material and no ceremony handles.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PasskeyCtx")
            .field("keys", &*self.keys())
            .field("watchers", &self.watchers.len())
            .finish_non_exhaustive()
    }
}

impl PasskeyCtx {
    /// Read the handle, mint one if this working directory has never had
    /// one, and restore every stored credential.
    ///
    /// Every failure here is fatal by design. The alternative — logging and
    /// carrying on with an empty registry — is the exact shape of the bug
    /// this wiring exists to close: the node comes up, the sign-in page
    /// offers a passkey prompt backed by nothing, and the operator's
    /// credential has silently stopped working.
    pub fn start(
        settings: Settings,
        http: Arc<crate::http::Ctx>,
        store: Arc<Mutex<Store>>,
        env: Box<dyn Env + Send + Sync>,
    ) -> Result<PasskeyCtx, StartError> {
        if !settings.require_user_verification {
            tracing::warn!(
                "passkey.require_user_verification is false, which does not relax anything: \
                 webauthn-rs asks for user verification on every passkey assertion and refuses \
                 an unverified one itself. All it does here is switch off the second-line check \
                 in passkey.rs, so set it back to true"
            );
        }

        let stored = {
            let st = store.lock().unwrap_or_else(PoisonError::into_inner);
            st.passkey_user()
                .map_err(|source| StartError::Read { source })?
        };

        let text = match stored {
            Some(text) => text,
            None => {
                // Minted and written in one place, so a handle can never
                // exist in memory without existing on disk. A handle that
                // lived only in this process would be a different handle
                // after the next restart, and every credential enrolled
                // against it would be orphaned.
                let fresh = UserHandle::generate(env.as_ref())?;
                let st = store.lock().unwrap_or_else(PoisonError::into_inner);
                st.set_passkey_user(fresh, env.now())
                    .map_err(|source| StartError::Write { source })?
            }
        };

        let user = UserHandle::decode(&text).ok_or(StartError::BadUserHandle {
            found: text.clone(),
        })?;

        let mut keys = Passkeys::new(settings, user)?;
        let rows = {
            let st = store.lock().unwrap_or_else(PoisonError::into_inner);
            st.passkeys()
                .map_err(|source| StartError::Read { source })?
        };
        let restored = rows.len();
        keys.restore(&rows)?;
        tracing::info!(credentials = restored, "passkeys restored");

        Ok(PasskeyCtx {
            keys: Mutex::new(keys),
            http,
            store,
            env,
            watchers: Vec::new(),
        })
    }

    /// Register something to run for every ceremony outcome worth acting on.
    /// See [`Happening`].
    pub fn on_event(mut self, watch: impl Fn(&Happening) + Send + Sync + 'static) -> PasskeyCtx {
        self.watchers.push(Box::new(watch));
        self
    }

    /// Whether a passkey sign-in is possible at all, for a sign-in page
    /// deciding between a prompt and the one-time code field.
    pub fn is_enrolled(&self) -> bool {
        self.keys().is_enrolled()
    }

    /// Drop every step-up grant, every ceremony in flight and any open
    /// enrollment approval. Hygiene for the kill switch and for anything
    /// that revokes every session: the grants are keyed by session ids that
    /// are already dead by then, so nothing can reach them, but a kill
    /// switch that leaves state behind invites the question of what else it
    /// left.
    pub fn revoke_all(&self) {
        self.keys().revoke_all();
    }

    /// Mint an **ordinary** session for a proven passkey, and record that it
    /// is the stronger kind.
    ///
    /// Public because it is the one thing a caller outside the HTTP routes
    /// may need: the rest of the system must not learn that there is a
    /// second way of being signed in, so a passkey ends up in the same
    /// `AuthState`, under the same `SessionHash`, with the same expiry, as a
    /// session minted by the one-time code. The only difference is the
    /// `sessions.kind` column migration 7 added, and step-up is what that
    /// difference is for.
    ///
    /// `max_sessions` is enforced by [`AuthState::admit`], which evicts the
    /// stalest session exactly as `AuthState::login` does, and which applies
    /// the same [`crate::auth::state::Refusal`] check — so the `Err` here is
    /// a blocked address or the global lock, never a full node. This used to
    /// call `restore_session`, which counts nothing and evicts nothing
    /// because it is for remembering a session that already happened, and
    /// then refuse with `SessionsFull` when the node was full. That made the
    /// strongest credential the one that could not sign in.
    ///
    /// The route checks `refusal` once before spending the ceremony, which
    /// is the right place to spend nothing on a blocked address. This second
    /// check is the one that is atomic with the eviction: between the two,
    /// `/login` can block the address, and without it a request from an
    /// address the state machine has already decided to refuse would destroy
    /// a live session on its way in.
    pub fn admit(&self, ip: IpAddr, id: &SessionId) -> Result<Session, Refusal> {
        let now = self.now();
        let mut events = Vec::new();
        let admitted = {
            let mut st = self.state();
            st.admit(ip, id, now, &mut events)
        };

        // The `AuthState` guard is dropped before this line, on purpose, and
        // whether the admission succeeded or not: `apply` takes the store
        // lock and runs the reapers, and holding `AuthState` across either is
        // how two locks come to be taken in two orders. One call, not a
        // second copy of what a revocation costs — the deleted row, the
        // reaped pty, the audit entry.
        self.http.apply(events);
        let session = admitted?;

        // Persisted first and marked second, in that order and not the other
        // way round: `insert_session` is an INSERT OR REPLACE over the
        // columns it names, so it replaces the row and `kind` goes back to
        // its DEFAULT of 'tether'. Marking before inserting would record the
        // passkey kind and then throw it away.
        //
        // Neither failure refuses the sign-in. The assertion was verified;
        // the session exists in memory and works. What a failure costs is
        // that the session does not survive a restart, or comes back reading
        // as tether, which refuses a step-up — the safe direction in both
        // cases, and cheaper than refusing a sign-in because the database
        // hiccuped.
        let store = self.store();
        if let Err(e) = store.insert_session(&id.hash(), &session) {
            tracing::warn!(error = %e, "cannot persist session");
        }
        match store.set_session_kind(&id.hash(), SessionKind::Trusted) {
            Ok(true) => {}
            Ok(false) => {
                tracing::warn!("the session row is not there to mark as a passkey session")
            }
            Err(e) => tracing::warn!(error = %e, "cannot record how this session signed in"),
        }
        Ok(session)
    }

    fn keys(&self) -> MutexGuard<'_, Passkeys> {
        // Poisoning means a handler panicked mid-ceremony. The registry is
        // still consistent — every mutation it makes is one map insert or
        // one vector push — and refusing every sign-in from here on would
        // turn one panic into a lockout.
        self.keys.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn store(&self) -> MutexGuard<'_, Store> {
        self.store.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn state(&self) -> MutexGuard<'_, AuthState> {
        self.http.state()
    }

    fn now(&self) -> Now {
        Now {
            wall: self.env.now(),
            mono: self.env.mono(),
        }
    }

    fn announce(&self, ip: IpAddr, events: Vec<Event>) {
        for event in events {
            match &event {
                Event::CloneSuspected { credential, .. } => tracing::error!(
                    credential = %credential.short(),
                    "sign count went backwards: this credential has been cloned"
                ),
                Event::Failed {
                    purpose,
                    consecutive,
                } => tracing::warn!(
                    purpose = purpose.label(),
                    consecutive,
                    "passkey ceremony failed"
                ),
                other => tracing::info!(event = ?other, "passkey"),
            }
            let happening = Happening { ip, event };
            for watch in &self.watchers {
                watch(&happening);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Request shapes
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct FinishRegistration {
    pub ceremony: String,
    pub credential: webauthn_rs::prelude::RegisterPublicKeyCredential,
}

#[derive(Debug, Deserialize)]
pub struct FinishAssertion {
    pub ceremony: String,
    pub credential: webauthn_rs::prelude::PublicKeyCredential,
}

#[derive(Debug, Deserialize)]
pub struct BeginRegistration {
    pub nickname: String,
    /// The code read off an already signed-in device. Absent means this is
    /// the first credential, which [`Passkeys::start_registration`] allows
    /// only while none is enrolled — the rule is enforced there rather than
    /// here, so it holds however the request arrived.
    #[serde(default)]
    pub code: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct BeginStepUp {
    pub access: String,
}

#[derive(Debug, Deserialize)]
pub struct RemoveCredential {
    pub id: String,
}

/// One enrolled credential, as the device list shows it. No key material:
/// the public key is not a secret, but a device list is not where it belongs
/// either.
#[derive(Debug, Serialize)]
pub struct CredentialView {
    pub id: String,
    pub nickname: String,
    pub created: i64,
    pub last_used: Option<i64>,
    pub sign_count: u32,
    pub backup_state: bool,
}

// ---------------------------------------------------------------------------
// The routes
// ---------------------------------------------------------------------------

/// Every passkey ceremony, as one router.
///
/// Modeled on [`crate::lockdown::unlock_router`]: its own state type and its
/// own `Arc`'d context, so merging it costs the rest of the app nothing and
/// forces no shared state on it.
///
/// Who may reach what:
///
/// - **sign-in** needs no session. That is what signing in *is*, and a
///   sign-in route behind a session check would be unreachable by anyone who
///   needed it.
/// - **enrollment and step-up** need one. A step-up is a session proving
///   itself again, so it cannot mean anything without one. Enrollment needs
///   one for a sharper reason: `Enrollment::First` is the only enrollment
///   with no approval behind it, and without a session check whoever reached
///   the port before the operator would own the node's first credential.
///
///   This does leave one question for whoever assembles the node. The
///   enrollment ticket is a code read off an already signed-in device and
///   typed on the new one, so if a brand-new device is meant to enroll
///   *before* signing in at all, `/passkey/enroll/begin` with a `code` would
///   have to be reachable without a session and the ticket would be the only
///   authority — single-use, time-boxed, burned after
///   `ENROLLMENT_ATTEMPTS` wrong codes. The session is required here because
///   nothing in `http::router` is reachable without one anyway: a new device
///   signs in with the one-time code first, which is the design's tether
///   path, and the ticket is then the second requirement rather than the
///   only one. Loosening it is a decision, not a cleanup.
/// - **every mutating route** checks `Origin` against `Host`, for the reason
///   `http::origin_ok` gives: without it another page in the browser posts
///   these on your behalf, with your cookie attached.
pub fn passkey_router(ctx: Arc<PasskeyCtx>) -> Router {
    Router::new()
        .route("/passkey/sign-in/begin", post(sign_in_begin))
        .route("/passkey/sign-in/finish", post(sign_in_finish))
        .route("/passkey/enroll/approve", post(enroll_approve))
        .route("/passkey/enroll/begin", post(enroll_begin))
        .route("/passkey/enroll/finish", post(enroll_finish))
        .route("/passkey/step-up/begin", post(step_up_begin))
        .route("/passkey/step-up/finish", post(step_up_finish))
        .route("/passkey/credentials", get(credentials))
        .route("/passkey/credentials/remove", post(credentials_remove))
        .with_state(ctx)
}

/// A present `Origin` must name the same host as `Host`.
///
/// Copied from `http::origin_ok` rather than shared, because that one is
/// private and this file does not own `http.rs`. The reasoning is its: a
/// cross-origin page cannot read the response, but it can make the request,
/// and a request that enrolls a credential or spends a step-up does its
/// damage on the way in. An absent `Origin` is allowed — a non-browser
/// client sends none, and refusing those would refuse `curl` while stopping
/// no browser.
fn origin_ok(headers: &HeaderMap) -> bool {
    let Some(origin) = headers.get(header::ORIGIN) else {
        return true;
    };
    let (Ok(origin), Some(Ok(host))) = (
        origin.to_str(),
        headers.get(header::HOST).map(|h| h.to_str()),
    ) else {
        return false;
    };
    let authority = origin
        .strip_prefix("https://")
        .or_else(|| origin.strip_prefix("http://"));
    authority.is_some_and(|a| a.eq_ignore_ascii_case(host))
}

fn refuse(code: StatusCode, message: &str) -> Response {
    (
        code,
        [(header::CACHE_CONTROL, "no-store")],
        format!("{message}\n"),
    )
        .into_response()
}

fn cross_origin() -> Response {
    refuse(StatusCode::FORBIDDEN, "cross-origin request refused")
}

/// Read a JSON body **after** the origin check, not before it.
///
/// `Json<T>` as an extractor would run before the handler body, so a
/// cross-origin request with a malformed body was answered 422 by axum and
/// never reached `origin_ok` at all. Nothing was granted either way, but a
/// request from another origin should not have its body parsed: the check
/// that refuses it is the first thing these routes do, and this is what
/// keeps it first.
fn parse<T: serde::de::DeserializeOwned>(body: &[u8]) -> Result<T, String> {
    serde_json::from_slice(body).map_err(|e| e.to_string())
}

fn reply(value: serde_json::Value) -> Response {
    (
        StatusCode::OK,
        [(header::CACHE_CONTROL, "no-store")],
        Json(value),
    )
        .into_response()
}

/// The status a refusal deserves, chosen so a client can tell "try again"
/// from "stop".
fn status_for(e: &PasskeyError) -> StatusCode {
    match e {
        PasskeyError::BadNickname
        | PasskeyError::UnknownCeremony
        | PasskeyError::CeremonyExpired => StatusCode::BAD_REQUEST,
        PasskeyError::TooManyCeremonies => StatusCode::TOO_MANY_REQUESTS,
        PasskeyError::NoCredentials | PasskeyError::AlreadyEnrolled { .. } => StatusCode::CONFLICT,
        PasskeyError::UnknownCredential { .. } => StatusCode::NOT_FOUND,
        PasskeyError::BadOrigin { .. }
        | PasskeyError::BadRelyingParty { .. }
        | PasskeyError::EncodeKey(_)
        | PasskeyError::BadStoredRow { .. }
        | PasskeyError::Random(_) => StatusCode::INTERNAL_SERVER_ERROR,
        // Everything else is a refusal of the thing that was asked for: a
        // missing or burned approval, a ceremony spent for the wrong
        // purpose, an unverified authenticator, a cloned credential, a
        // tether session reaching for the host.
        _ => StatusCode::FORBIDDEN,
    }
}

fn refuse_passkey(e: PasskeyError) -> Response {
    let code = status_for(&e);
    tracing::info!(error = %e, "passkey refused");
    refuse(code, &e.to_string())
}

/// The caller's address, and the session it presented if that session is
/// real. Nothing here is taken from the body: a request that named its own
/// address would dodge the lockout, and one that named its own session would
/// choose its own privileges.
struct Caller {
    ip: IpAddr,
    session: Option<SessionId>,
}

fn caller(ctx: &PasskeyCtx, peer: SocketAddr, headers: &HeaderMap) -> Caller {
    let ip = ctx.http.client_ip(peer, headers);
    let presented = ctx.http.session_cookie(peer, headers);
    let now = ctx.now();
    let session =
        presented.filter(|id| matches!(ctx.state().verify(ip, Some(id), now), VerifyOutcome::Ok));
    Caller { ip, session }
}

/// The caller's session, or nothing. Returned as an `Option` rather than a
/// `Result<_, Response>` so that each route says in its own words what the
/// session was needed for.
fn signed_in(
    ctx: &PasskeyCtx,
    peer: SocketAddr,
    headers: &HeaderMap,
) -> Option<(IpAddr, SessionId)> {
    let caller = caller(ctx, peer, headers);
    caller.session.map(|id| (caller.ip, id))
}

/// A 401 rather than a redirect to `/login`: every route here is called by
/// script with `fetch`, and a redirect to an HTML page is something a
/// ceremony cannot do anything with.
fn unauthorized(why: &str) -> Response {
    refuse(StatusCode::UNAUTHORIZED, why)
}

/// What enrollment and step-up say when there is no session. Both sentences
/// from the design are about a session: a new device is approved from one
/// already signed in, and a step-up is a session proving itself again.
const NEEDS_SESSION: &str =
    "sign in first: a new device is approved from one that is already signed in";

// ---- sign-in -------------------------------------------------------------

async fn sign_in_begin(
    State(ctx): State<Arc<PasskeyCtx>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
) -> Response {
    if !origin_ok(&headers) {
        return cross_origin();
    }
    // Starting a ceremony needs no credential and costs a slot out of
    // `MAX_PENDING`, so an address the lockout has already refused does not
    // get to spend them.
    let ip = ctx.http.client_ip(peer, &headers);
    if let Some(refusal) = ctx.state().refusal(ip) {
        tracing::info!(%ip, reason = ?refusal, "passkey sign-in refused");
        return refuse(
            StatusCode::FORBIDDEN,
            "sign-in is blocked from this address",
        );
    }
    match ctx.keys().start_sign_in(ctx.env.as_ref()) {
        Ok(started) => reply(json!({
            "ceremony": started.ceremony.encode(),
            "challenge": started.challenge,
        })),
        Err(e) => refuse_passkey(e),
    }
}

async fn sign_in_finish(
    State(ctx): State<Arc<PasskeyCtx>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if !origin_ok(&headers) {
        return cross_origin();
    }
    let body: FinishAssertion = match parse(&body) {
        Ok(v) => v,
        Err(why) => return refuse(StatusCode::BAD_REQUEST, &why),
    };
    let ip = ctx.http.client_ip(peer, &headers);

    // A blocked address is refused before the ceremony is spent, so that
    // being blocked costs an attacker the challenge too rather than letting
    // them burn ceremonies while they are locked out.
    if let Some(refusal) = ctx.state().refusal(ip) {
        tracing::info!(%ip, reason = ?refusal, "passkey sign-in refused");
        return refuse(
            StatusCode::FORBIDDEN,
            "sign-in is blocked from this address",
        );
    }

    let Some(ceremony) = crate::passkey::CeremonyId::decode(&body.ceremony) else {
        return refuse(StatusCode::BAD_REQUEST, "that is not a ceremony id");
    };

    let mut events = Vec::new();
    let outcome =
        ctx.keys()
            .finish_sign_in(&ceremony, &body.credential, ctx.env.as_ref(), &mut events);
    ctx.announce(ip, events);
    let signed_in = match outcome {
        Ok(v) => v,
        Err(e) => return refuse_passkey(e),
    };

    // The moved sign count, written before the session exists. If this fails
    // the sign-in still stands: the count on disk is merely behind, and the
    // next assertion presents a higher one, which still passes. Refusing a
    // proven sign-in because the database hiccuped would be the worse trade.
    if let Err(e) = ctx.store().upsert_passkey(&signed_in.row) {
        tracing::warn!(error = %e, "cannot persist the moved sign count");
    }

    let id = match SessionId::generate() {
        Ok(id) => id,
        Err(e) => {
            tracing::error!(error = %e, "cannot create session id");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };

    // No 503 here any more. A verified assertion on a full node gets a
    // session: `AuthState::admit` evicted the stalest one and
    // `http::Ctx::apply` deleted its row, reaped its ptys and wrote the audit
    // entry before this returned. The one refusal left is the same one the
    // check above makes — a blocked address or the global lock — caught again
    // here because only there is it atomic with the eviction.
    if let Err(refusal) = ctx.admit(ip, &id) {
        tracing::info!(%ip, reason = ?refusal, "passkey sign-in refused");
        return refuse(
            StatusCode::FORBIDDEN,
            "sign-in is blocked from this address",
        );
    }

    tracing::info!(%ip, credential = %signed_in.credential.short(), "passkey sign-in");
    (
        StatusCode::OK,
        [
            (
                header::SET_COOKIE,
                session::set_cookie(&id, ctx.http.is_https(peer, &headers)),
            ),
            (header::CACHE_CONTROL, "no-store".to_string()),
        ],
        Json(json!({ "nickname": signed_in.nickname, "verified": signed_in.user_verified })),
    )
        .into_response()
}

// ---- enrollment ----------------------------------------------------------

async fn enroll_approve(
    State(ctx): State<Arc<PasskeyCtx>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
) -> Response {
    if !origin_ok(&headers) {
        return cross_origin();
    }
    let Some((_, session)) = signed_in(&ctx, peer, &headers) else {
        return unauthorized(NEEDS_SESSION);
    };
    let mut keys = ctx.keys();
    match keys.approve_enrollment(session, ctx.env.as_ref()) {
        // `reveal()` is the only way to get the text, and this is the one
        // place it is meant to reach: the screen of the device that approved
        // the enrollment. It is never logged.
        Ok(ticket) => reply(json!({ "code": ticket.reveal() })),
        Err(e) => refuse_passkey(e),
    }
}

async fn enroll_begin(
    State(ctx): State<Arc<PasskeyCtx>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if !origin_ok(&headers) {
        return cross_origin();
    }
    let body: BeginRegistration = match parse(&body) {
        Ok(v) => v,
        Err(why) => return refuse(StatusCode::BAD_REQUEST, &why),
    };
    if signed_in(&ctx, peer, &headers).is_none() {
        return unauthorized(NEEDS_SESSION);
    }
    let enrollment = match &body.code {
        Some(code) => Enrollment::Ticket(code.as_str()),
        None => Enrollment::First,
    };
    match ctx
        .keys()
        .start_registration(enrollment, &body.nickname, ctx.env.as_ref())
    {
        Ok(started) => reply(json!({
            "ceremony": started.ceremony.encode(),
            "challenge": started.challenge,
        })),
        Err(e) => refuse_passkey(e),
    }
}

async fn enroll_finish(
    State(ctx): State<Arc<PasskeyCtx>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if !origin_ok(&headers) {
        return cross_origin();
    }
    let body: FinishRegistration = match parse(&body) {
        Ok(v) => v,
        Err(why) => return refuse(StatusCode::BAD_REQUEST, &why),
    };
    let Some((ip, _)) = signed_in(&ctx, peer, &headers) else {
        return unauthorized(NEEDS_SESSION);
    };
    let Some(ceremony) = crate::passkey::CeremonyId::decode(&body.ceremony) else {
        return refuse(StatusCode::BAD_REQUEST, "that is not a ceremony id");
    };

    let mut events = Vec::new();
    let outcome =
        ctx.keys()
            .finish_registration(&ceremony, &body.credential, ctx.env.as_ref(), &mut events);
    ctx.announce(ip, events);
    let registered = match outcome {
        Ok(v) => v,
        Err(e) => return refuse_passkey(e),
    };

    // A registration that is not written down is the defect this whole file
    // exists to close: the credential works until the next restart and then
    // is simply gone, with the authenticator still offering it. So this one
    // failure is not swallowed — the credential comes back out of the
    // registry so that memory and disk agree, and the operator is told to
    // enroll again.
    if let Err(e) = ctx.store().upsert_passkey(&registered.row) {
        tracing::error!(error = %e, "cannot store the new credential; rolling it back");
        let _ = ctx.keys().remove(&registered.credential.id);
        return refuse(
            StatusCode::INTERNAL_SERVER_ERROR,
            "the new passkey could not be written to the state database, so it has been \
             discarded rather than left to disappear at the next restart; fix the working \
             directory and enroll again",
        );
    }

    tracing::info!(
        credential = %registered.credential.id.short(),
        "passkey enrolled"
    );
    reply(json!({
        "id": registered.credential.id.encode(),
        "nickname": registered.credential.nickname,
    }))
}

// ---- step-up -------------------------------------------------------------

fn access_from(text: &str) -> Option<HostAccess> {
    match text {
        "terminal" => Some(HostAccess::Terminal),
        "files" | "outside-working-directory" => Some(HostAccess::OutsideWorkingDirectory),
        _ => None,
    }
}

async fn step_up_begin(
    State(ctx): State<Arc<PasskeyCtx>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if !origin_ok(&headers) {
        return cross_origin();
    }
    let body: BeginStepUp = match parse(&body) {
        Ok(v) => v,
        Err(why) => return refuse(StatusCode::BAD_REQUEST, &why),
    };
    let Some((_, session)) = signed_in(&ctx, peer, &headers) else {
        return unauthorized(NEEDS_SESSION);
    };
    let Some(access) = access_from(&body.access) else {
        return refuse(
            StatusCode::BAD_REQUEST,
            "access must be \"terminal\" or \"files\"",
        );
    };

    // Read from the database, never from the request. This is what migration
    // 7 is for: a borrowed machine signed in with the one-time code holds no
    // passkey of mine, so there is nothing for it to step up with, and
    // `start_step_up` refuses it by name. A client that could state its own
    // kind would be a client that could hand itself a shell.
    let kind = match ctx.store().session_kind(&session.hash()) {
        Ok(Some(kind)) => kind,
        // A session with no row is one that was never persisted. Reading it
        // as tether is the weaker, safe answer.
        Ok(None) => SessionKind::Tether,
        Err(e) => {
            tracing::warn!(error = %e, "cannot read how this session signed in");
            SessionKind::Tether
        }
    };

    match ctx
        .keys()
        .start_step_up(session, access, kind, ctx.env.as_ref())
    {
        Ok(started) => reply(json!({
            "ceremony": started.ceremony.encode(),
            "challenge": started.challenge,
        })),
        Err(e) => refuse_passkey(e),
    }
}

async fn step_up_finish(
    State(ctx): State<Arc<PasskeyCtx>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if !origin_ok(&headers) {
        return cross_origin();
    }
    let body: FinishAssertion = match parse(&body) {
        Ok(v) => v,
        Err(why) => return refuse(StatusCode::BAD_REQUEST, &why),
    };
    let Some((ip, _)) = signed_in(&ctx, peer, &headers) else {
        return unauthorized(NEEDS_SESSION);
    };
    let Some(ceremony) = crate::passkey::CeremonyId::decode(&body.ceremony) else {
        return refuse(StatusCode::BAD_REQUEST, "that is not a ceremony id");
    };

    let mut events = Vec::new();
    let outcome =
        ctx.keys()
            .finish_step_up(&ceremony, &body.credential, ctx.env.as_ref(), &mut events);
    ctx.announce(ip, events);
    let grant = match outcome {
        Ok(v) => v,
        Err(e) => return refuse_passkey(e),
    };

    if let Err(e) = ctx.store().upsert_passkey(&grant.row) {
        tracing::warn!(error = %e, "cannot persist the moved sign count");
    }

    // The grant itself is deliberately not persisted, and this is where that
    // decision becomes visible: `passkey.rs` keeps step-up grants in memory
    // because a restart is the cheapest kill switch there is, and a grant
    // that survived one would outlive the reason it was given.
    let remaining = ctx
        .keys()
        .step_up_remaining(&grant.session, grant.access, ctx.env.as_ref())
        .unwrap_or_default();
    tracing::info!(
        credential = %grant.credential.short(),
        access = grant.access.label(),
        "stepped up"
    );
    reply(json!({
        "access": grant.access.label(),
        "seconds": remaining.as_secs(),
    }))
}

// ---- the device list -----------------------------------------------------

async fn credentials(
    State(ctx): State<Arc<PasskeyCtx>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
) -> Response {
    if signed_in(&ctx, peer, &headers).is_none() {
        return unauthorized("sign in first");
    }
    let list: Vec<CredentialView> = ctx
        .keys()
        .credentials()
        .iter()
        .map(|c| CredentialView {
            id: c.id.encode(),
            nickname: c.nickname.clone(),
            created: to_unix(c.created),
            last_used: c.last_used.map(to_unix),
            sign_count: c.sign_count,
            backup_state: c.backup_state,
        })
        .collect();
    reply(json!({ "credentials": list }))
}

async fn credentials_remove(
    State(ctx): State<Arc<PasskeyCtx>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if !origin_ok(&headers) {
        return cross_origin();
    }
    let body: RemoveCredential = match parse(&body) {
        Ok(v) => v,
        Err(why) => return refuse(StatusCode::BAD_REQUEST, &why),
    };
    if signed_in(&ctx, peer, &headers).is_none() {
        return unauthorized("sign in first");
    }
    let Some(id) = CredentialId::decode(&body.id) else {
        return refuse(StatusCode::BAD_REQUEST, "that is not a credential id");
    };

    // Out of the database first, then out of the registry. The other order
    // has a window in which the credential is gone from memory and still on
    // disk, and the next restart brings a removed device back — which is
    // exactly what "remove a lost device" must never mean.
    match ctx.store().delete_passkey(&id) {
        Ok(_) => {}
        Err(e) => {
            tracing::error!(error = %e, "cannot remove the credential from the database");
            return refuse(
                StatusCode::INTERNAL_SERVER_ERROR,
                "the passkey could not be removed from the state database, so it has been left \
                 in place rather than removed only until the next restart",
            );
        }
    }
    match ctx.keys().remove(&id) {
        Ok(gone) => {
            tracing::warn!(credential = %id.short(), nickname = %gone.nickname, "passkey removed");
            reply(json!({ "removed": gone.nickname }))
        }
        Err(e) => refuse_passkey(e),
    }
}

// ---------------------------------------------------------------------------
// Defaults
// ---------------------------------------------------------------------------

/// The real clock and the real random source, boxed for [`PasskeyCtx::start`].
///
/// A free function rather than making callers name [`OsEnv`], so the line
/// that assembles this in `main` stays one line.
pub fn os_env() -> Box<dyn Env + Send + Sync> {
    Box::new(OsEnv)
}

/// Settings for a node served at `listen`, honoring `[passkey]` if the
/// operator wrote one.
///
/// `rp_id` is the one value that cannot be changed later: a credential is
/// bound to it, so changing it does not break sign-in for a while, it
/// orphans every passkey already enrolled. That is why the three cases are
/// kept apart rather than defaulted into one.
///
/// - **Configured.** Used as written. [`crate::config::Config::validate`]
///   has already refused a scheme, a port or an IP address in `rp_id`.
/// - **Loopback, unconfigured.** `localhost` over the listen port. WebAuthn
///   treats loopback as a secure context with no certificate, so a first run
///   works out of the box and the credential is bound to a name that stays
///   true.
/// - **Non-loopback, unconfigured.** `None`: passkeys are switched off.
///
/// That last case used to return `rp_id = "localhost"` with an origin of
/// `http://10.0.0.5:7878`, which cannot work — the browser refuses a
/// ceremony whose rp_id is not a suffix of its origin — while the doc
/// claimed it defaulted to "exactly what WebAuthn treats as a secure
/// context". Returning nothing is worse to read in a log and much better
/// than inviting an enrollment against a name that will be rejected, or
/// binding credentials to `localhost` on a machine reached by another name.
pub fn settings_for(listen: SocketAddr, configured: &crate::config::Passkey) -> Option<Settings> {
    if configured.is_set() {
        return Some(Settings {
            rp_id: configured.rp_id.clone(),
            origin: configured.origin.clone(),
            ..Settings::default()
        });
    }
    if listen.ip().is_loopback() || listen.ip().is_unspecified() {
        return Some(Settings {
            rp_id: "localhost".to_string(),
            origin: format!("http://localhost:{}", listen.port()),
            ..Settings::default()
        });
    }
    tracing::warn!(
        %listen,
        "passkeys are off: this node is not served on loopback and [passkey] is empty, so \
         there is no rp_id that a browser would accept. Set passkey.rp_id and \
         passkey.origin in .deckjack/config.toml — before enrolling anything, because a \
         credential cannot be moved to a different rp_id afterwards."
    );
    None
}
