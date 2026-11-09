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
pub use session::{Session, SessionId};
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
    Login { ip: IpAddr },
    Failed { ip: IpAddr, left: u32 },
    IpBlocked { ip: IpAddr },
    GlobalLock,
}
