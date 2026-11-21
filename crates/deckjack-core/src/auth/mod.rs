//! Sign-in: one-time code with lockout, and sessions.
//!
//! This is the tether-mode path from the design: a code shown on the host
//! and approved there, for borrowed machines where a passkey on the
//! untrusted keyboard is the wrong answer. Passkeys sit alongside it for
//! ordinary remote sign-in; they are not implemented yet.
//!
//! Harvested from pocketdock, where it was milestones 2-3. The state
//! machine takes its clock by argument rather than reading it, so every
//! rule below is tested without sleeping.

pub mod code;
pub mod html;
pub mod session;
pub mod state;

pub use code::Code;
pub use session::{Session, SessionHash, SessionId};
pub use state::{AuthState, Limits, Now};

use std::net::IpAddr;

/// Something the state machine did that the rest of the system may want to
/// act on: surfacing it to the operator, or handing a blocked address to
/// whatever enforces blocks.
///
/// `AuthState` never acts on these itself. It appends them to a vector the
/// caller passes in, which is what keeps it free of I/O and of any opinion
/// about how a block is enforced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Event {
    Login {
        ip: IpAddr,
    },
    Failed {
        ip: IpAddr,
        left: u32,
    },
    IpBlocked {
        ip: IpAddr,
    },
    GlobalLock,
    /// A session was revoked without being asked to go: the stalest one
    /// dropped to honor `max_sessions`, or one belonging to an address that
    /// has just been blocked.
    ///
    /// The caller owes this session three things the state machine cannot do
    /// itself — delete its persisted row, reap whatever it owns (a pty is a
    /// process running as the host user), and write it down. Signing out
    /// does the first two already; this is how the involuntary paths get the
    /// same treatment, plus the audit row a deliberate sign-out does not
    /// need.
    ///
    /// `ip` is the revoked session's own address, not the address that
    /// caused the revocation. On the blocked-address path they are the same;
    /// on the `max_sessions` path they are not, and it is the victim's
    /// address that is evidence.
    SessionRevoked {
        hash: SessionHash,
        ip: IpAddr,
        reason: RevokeReason,
    },
}

/// Why a session was taken away. Recorded, because "your session ended"
/// without a reason is the log entry nobody can act on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RevokeReason {
    /// The stalest session dropped so a new sign-in fits under
    /// [`Limits::max_sessions`].
    SessionsFull,
    /// Taken along with an address that has just been blocked for failed
    /// sign-ins.
    AddressBlocked,
    /// Past its `session_max`. Announced like the others, because an expired
    /// session owes the same cleanup: its row, and the shells it opened.
    Expired,
}

impl RevokeReason {
    /// The value that goes in the audit row. Stable: a log nobody can grep
    /// for the same string twice is not a log.
    pub fn as_str(self) -> &'static str {
        match self {
            RevokeReason::SessionsFull => "sessions_full",
            RevokeReason::AddressBlocked => "address_blocked",
            RevokeReason::Expired => "expired",
        }
    }
}
