//! Confirmation tickets: the operator's "yes", held by the server.
//!
//! The browser used to say a call was confirmed by setting `confirmed: true`
//! on the invoke frame, and the bus believed it. Any script that could reach
//! the socket could therefore empty the trash or issue new recovery codes
//! without the dialog ever being drawn (docs/APP-UI-ISOLATION.md §1, §4.7).
//!
//! Now the bus answers a call that needs confirming with a **ticket**, and a
//! `confirm` frame carrying that ticket is the only way to run it. A ticket
//! is:
//!
//! - **128 random bits** from the OS, so it cannot be guessed;
//! - **single-use**: it is taken out of the table the moment anyone presents
//!   it, whether or not the presentation matches, so a ticket that leaked and
//!   was tried once is dead;
//! - **short-lived**: [`TTL`], measured on the injected [`Clock`]'s monotonic
//!   time;
//! - **bound** to the socket that received it, the issuer (for a call made
//!   from an app's window that is `Issuer::AppUi`, so the app's name and the
//!   browser session are part of it: a ticket issued to app A's window
//!   cannot be redeemed as the shell's call or app B's, nor the reverse;
//!   `tests/wiring.rs`, "Tickets and `as_app`"), the command id
//!   and the SHA-256 of the canonical parameters. A ticket for "empty these
//!   two items" cannot run "empty everything", on this socket or any other;
//! - bound to **the target the operator was shown**. The same parameters can
//!   name different things a moment later ("empty everything" while
//!   something new lands in the trash), so the target is rendered again on
//!   redemption and must read exactly as it did in the dialog.
//!
//! Only the binding is kept, not the parameters. A client resends the call
//! with the ticket and the digest has to match; holding the parameters
//! instead would let one socket park a quarter megabyte per unredeemed
//! ticket on the server.
//!
//! The CLI and in-process callers do not use tickets: they build a
//! [`crate::command::Request`] and mark it confirmed themselves, because they
//! *are* the operator at the host. Agents never confirm anything; their
//! calls queue in the Inbox.

use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use serde_json::Value;
use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::command::{CommandId, Issuer};
use crate::ws::{Clock, ConnectionId};

/// How long the operator has to answer. Long enough to read a prompt and
/// reach for the button on a phone; short enough that a ticket left behind
/// in a closed dialog is not a standing permission.
pub const TTL: Duration = Duration::from_secs(60);

/// Unredeemed tickets one socket may hold. The shell shows one prompt at a
/// time, so more than a handful outstanding is a client minting tickets it
/// never means to use; the oldest is dropped to make room.
pub const MAX_PER_CONNECTION: usize = 16;

/// A ticket as the client holds it: 32 lowercase hex characters.
///
/// `Debug` redacts. A ticket is a one-minute bearer credential for one
/// destructive call, and a log line is the wrong place for one.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct Ticket(String);

impl Ticket {
    /// The text to send to the client that is being asked.
    pub fn reveal(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Ticket {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Ticket(<redacted>)")
    }
}

/// What a ticket is good for. Every field must match on redemption.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Binding {
    connection: ConnectionId,
    issuer: Issuer,
    command: CommandId,
    params: [u8; 32],
    target: [u8; 32],
}

impl Binding {
    /// `target` is the text the confirmation shows (or would show now, on
    /// redemption): [`crate::command::Bus::target`]. Required, so a binding
    /// cannot be built that forgets what the operator was told.
    pub fn new(
        connection: ConnectionId,
        issuer: Issuer,
        command: CommandId,
        params_digest: [u8; 32],
        target: Option<&str>,
    ) -> Binding {
        let mut hasher = Sha256::new();
        match target {
            None => hasher.update(b"none"),
            Some(text) => {
                hasher.update(b"some:");
                hasher.update(text.as_bytes());
            }
        }
        Binding {
            connection,
            issuer,
            command,
            params: params_digest,
            target: hasher.finalize().into(),
        }
    }
}

/// SHA-256 over a canonical encoding of the parameters: object keys sorted,
/// no insignificant whitespace. `{"a":1,"b":2}` and `{"b":2,"a":1}` are the
/// same call and digest the same; anything that changes what the command
/// would receive changes the digest.
///
/// Written out rather than trusting `serde_json::to_vec`, whose key order
/// follows whichever map type feature unification picked for the build.
pub fn params_digest(params: &Value) -> [u8; 32] {
    let mut hasher = Sha256::new();
    canonical(params, &mut hasher);
    hasher.finalize().into()
}

fn canonical(value: &Value, out: &mut Sha256) {
    match value {
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            out.update(b"{");
            for (i, key) in keys.into_iter().enumerate() {
                if i > 0 {
                    out.update(b",");
                }
                out.update(Value::String(key.clone()).to_string().as_bytes());
                out.update(b":");
                canonical(&map[key], out);
            }
            out.update(b"}");
        }
        Value::Array(items) => {
            out.update(b"[");
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.update(b",");
                }
                canonical(item, out);
            }
            out.update(b"]");
        }
        scalar => out.update(scalar.to_string().as_bytes()),
    }
}

/// Why a ticket did not run anything. Every message ends by saying what to
/// do, which is always the same thing: ask again.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum Refusal {
    #[error(
        "that confirmation is not known: it was already used, or it was never issued. \
         Run the command again to be asked again"
    )]
    Unknown,

    #[error(
        "that confirmation expired; it is good for {} seconds. Run the command again \
         to be asked again",
        TTL.as_secs()
    )]
    Expired,

    #[error(
        "that confirmation was issued to another connection and is good only there. \
         Run the command again from this window"
    )]
    OtherConnection,

    #[error("that confirmation was issued to someone else. Run the command again")]
    OtherIssuer,

    #[error(
        "that confirmation was for a different command. Run the command again to be \
         asked about this one"
    )]
    OtherCommand,

    #[error(
        "that confirmation was for different parameters than these; what you confirmed \
         is not what was sent. Run the command again to be asked about this call"
    )]
    OtherParams,

    #[error(
        "what that confirmation named has changed since you were asked, so nothing was \
         run. Run the command again to see what it acts on now"
    )]
    TargetChanged,
}

struct Held {
    binding: Binding,
    issued: Instant,
}

/// Every unredeemed ticket on this node.
///
/// One table, not one per socket, because a ticket presented on the wrong
/// socket has to be found to be refused *as* the wrong socket, and burned.
pub struct Tickets {
    clock: Arc<dyn Clock>,
    held: Mutex<HashMap<Ticket, Held>>,
}

impl Tickets {
    pub fn new(clock: Arc<dyn Clock>) -> Arc<Tickets> {
        Arc::new(Tickets {
            clock,
            held: Mutex::new(HashMap::new()),
        })
    }

    fn held(&self) -> std::sync::MutexGuard<'_, HashMap<Ticket, Held>> {
        self.held.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// A fresh ticket for one call.
    ///
    /// Fails only if the OS random source does, and then nothing is issued:
    /// a ticket from a weak source would be guessable, which is worse than
    /// the operator not being able to confirm.
    pub fn issue(&self, binding: Binding) -> Result<Ticket, crate::auth::code::RandomError> {
        let mut bytes = [0u8; 16];
        crate::auth::code::os_random(&mut bytes)?;
        let ticket = Ticket(bytes.iter().map(|b| format!("{b:02x}")).collect());
        let now = self.clock.mono();

        let mut held = self.held();
        held.retain(|_, h| now.saturating_duration_since(h.issued) <= TTL);
        let mut mine: Vec<(Ticket, Instant)> = held
            .iter()
            .filter(|(_, h)| h.binding.connection == binding.connection)
            .map(|(t, h)| (t.clone(), h.issued))
            .collect();
        if mine.len() >= MAX_PER_CONNECTION {
            mine.sort_by_key(|(_, issued)| *issued);
            let excess = mine.len() + 1 - MAX_PER_CONNECTION;
            for (old, _) in mine.into_iter().take(excess) {
                held.remove(&old);
            }
        }
        held.insert(
            ticket.clone(),
            Held {
                binding,
                issued: now,
            },
        );
        Ok(ticket)
    }

    /// Spend a ticket on the call described by `binding`.
    ///
    /// The ticket is gone after this whatever the answer, so a mismatched
    /// presentation cannot be retried with a corrected guess.
    pub fn redeem(&self, ticket: &str, binding: &Binding) -> Result<(), Refusal> {
        let now = self.clock.mono();
        let Some(held) = self.held().remove(&Ticket(ticket.to_string())) else {
            return Err(Refusal::Unknown);
        };
        if now.saturating_duration_since(held.issued) > TTL {
            return Err(Refusal::Expired);
        }
        let want = &held.binding;
        if want.connection != binding.connection {
            return Err(Refusal::OtherConnection);
        }
        if want.issuer != binding.issuer {
            return Err(Refusal::OtherIssuer);
        }
        if want.command != binding.command {
            return Err(Refusal::OtherCommand);
        }
        if want.params != binding.params {
            return Err(Refusal::OtherParams);
        }
        if want.target != binding.target {
            return Err(Refusal::TargetChanged);
        }
        Ok(())
    }

    /// Unredeemed, unexpired tickets, for an operator panel and for tests
    /// that a connection cannot grow the table without bound.
    pub fn outstanding(&self) -> usize {
        let now = self.clock.mono();
        self.held()
            .values()
            .filter(|h| now.saturating_duration_since(h.issued) <= TTL)
            .count()
    }
}
