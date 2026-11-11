//! Session identifiers and the `dj_session` cookie.

use std::fmt;
use std::net::IpAddr;
use std::time::SystemTime;

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use sha2::{Digest, Sha256};

use super::code::{RandomError, os_random};

pub const COOKIE_NAME: &str = "dj_session";

/// 256 random bits. Sessions are looked up by hash-map key on the full value;
/// with this much entropy, timing differences in the lookup do not help an
/// attacker guess a valid id, so no constant-time comparison is needed.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct SessionId([u8; 32]);

impl fmt::Debug for SessionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SessionId(<redacted>)")
    }
}

impl SessionId {
    pub fn generate() -> Result<SessionId, RandomError> {
        let mut b = [0u8; 32];
        os_random(&mut b)?;
        Ok(SessionId(b))
    }

    #[cfg(test)]
    pub fn from_bytes(b: [u8; 32]) -> SessionId {
        SessionId(b)
    }

    pub fn encode(&self) -> String {
        URL_SAFE_NO_PAD.encode(self.0)
    }

    pub fn decode(s: &str) -> Option<SessionId> {
        // 32 bytes encode to exactly 43 characters.
        if s.len() != 43 {
            return None;
        }
        let v = URL_SAFE_NO_PAD.decode(s).ok()?;
        Some(SessionId(v.try_into().ok()?))
    }

    /// What anything that is not the live request path is allowed to hold.
    pub fn hash(&self) -> SessionHash {
        SessionHash(Sha256::digest(self.0).into())
    }
}

/// SHA-256 of a [`SessionId`]. **This**, never the id, is what gets written
/// down.
///
/// A session id is a bearer credential: whoever holds it is signed in. So the
/// state database, the window of persisted sessions it holds and the terminal
/// registry's record of who opened a shell all key on this instead, and a
/// copy of the working directory — a backup, a sync target, another local
/// account reading the file — hands over identifiers that cannot be
/// presented as cookies.
///
/// Plain SHA-256, no salt and no password KDF, for the same reason
/// [`crate::lockdown::RecoveryCode`] uses one: the input is 256 bits of
/// uniform randomness, so there is no candidate set to grind through. A KDF
/// here would cost time on every request and buy nothing.
///
/// Related but not the same: `command::SessionTag` is the first 16 hex
/// characters of this digest, truncated deliberately so a log line cannot be
/// correlated byte-for-byte with a session row.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct SessionHash([u8; 32]);

impl fmt::Debug for SessionHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Not redacted: this is a digest of a credential, not the credential,
        // and being able to match one in a log against one in the database is
        // the entire point of storing it.
        write!(f, "SessionHash({})", self.hex())
    }
}

impl SessionHash {
    /// The first 16 hex characters: what a log row is allowed to name a
    /// session by.
    ///
    /// The same prefix [`crate::command::BrowserSession::tag`] computes, and
    /// for the same reason it is a prefix rather than a second digest: the
    /// join is `substr(hash, 1, 16) = <tag>` and no second derivation can
    /// drift. `BrowserSession::tag` still computes it itself; it should be
    /// changed to call this once a patch is touching `command.rs` anyway.
    pub fn tag(&self) -> String {
        let mut out = self.hex();
        out.truncate(16);
        out
    }

    /// 64 lowercase hex characters. What the `sessions` table stores.
    pub fn hex(&self) -> String {
        let mut out = String::with_capacity(64);
        for b in self.0 {
            use fmt::Write as _;
            // A formatting failure into a String cannot happen, and this runs
            // on the sign-in path, so it must not panic if it somehow does.
            let _ = write!(out, "{b:02x}");
        }
        out
    }

    /// Lowercase only. `from_str_radix` would happily take `AB`, which would
    /// give one session two spellings of its key and so two rows.
    pub fn from_hex(s: &str) -> Option<SessionHash> {
        if s.len() != 64
            || !s
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return None;
        }
        let mut b = [0u8; 32];
        for (i, byte) in b.iter_mut().enumerate() {
            *byte = u8::from_str_radix(s.get(i * 2..i * 2 + 2)?, 16).ok()?;
        }
        Some(SessionHash(b))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Session {
    pub ip: IpAddr,
    pub created: SystemTime,
    pub expires: SystemTime,
    /// When this session was last used, as
    /// [`crate::auth::state::AuthState::verify`] saw it. **This**, not
    /// `created`, is what `max_sessions` evicts by — see
    /// `AuthState::evict_to_fit` for why.
    ///
    /// Not persisted, and deliberately: `Store::insert_session` runs at
    /// sign-in, so a column here would record the creation time under a
    /// different name unless every authenticated request wrote to SQLite. A
    /// restored session comes back with `last_seen == created`, which is
    /// what eviction used to key on anyway, so a restart degrades this to
    /// the older rule rather than to nonsense.
    pub last_seen: SystemTime,
}

impl Session {
    /// A fresh session, last seen now, because minting one is being seen.
    ///
    /// A constructor rather than a struct literal at each call site, so that
    /// `last_seen` cannot be initialized to anything but `created` by
    /// somebody who did not read the field's doc comment.
    pub fn new(ip: IpAddr, created: SystemTime, expires: SystemTime) -> Session {
        Session {
            ip,
            created,
            expires,
            last_seen: created,
        }
    }

    pub fn is_valid_for(&self, ip: IpAddr, now: SystemTime) -> bool {
        self.ip == ip && now < self.expires
    }
}

/// `Set-Cookie` value for a new session. No `Max-Age`: the cookie dies with
/// the browser session; the server enforces `session_max_hours`.
pub fn set_cookie(id: &SessionId, secure: bool) -> String {
    let mut s = format!(
        "{COOKIE_NAME}={}; HttpOnly; SameSite=Strict; Path=/",
        id.encode()
    );
    if secure {
        s.push_str("; Secure");
    }
    s
}

/// `Set-Cookie` value that removes the session cookie.
pub fn clear_cookie(secure: bool) -> String {
    let mut s = format!("{COOKIE_NAME}=; HttpOnly; SameSite=Strict; Path=/; Max-Age=0");
    if secure {
        s.push_str("; Secure");
    }
    s
}

/// Find `dj_session` in one or more `Cookie` header values.
pub fn from_cookie_headers<'a>(headers: impl IntoIterator<Item = &'a str>) -> Option<SessionId> {
    headers
        .into_iter()
        .flat_map(cookie::Cookie::split_parse)
        .filter_map(Result::ok)
        .filter(|c| c.name() == COOKIE_NAME)
        .find_map(|c| SessionId::decode(c.value()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let id = SessionId::generate().unwrap();
        let enc = id.encode();
        assert_eq!(enc.len(), 43);
        assert_eq!(SessionId::decode(&enc), Some(id.clone()));
        assert_ne!(SessionId::generate().unwrap(), id);
        assert_eq!(format!("{id:?}"), "SessionId(<redacted>)");
    }

    #[test]
    fn the_hash_is_stable_distinct_and_one_way() {
        let a = SessionId::from_bytes([1; 32]);
        let b = SessionId::from_bytes([2; 32]);
        assert_eq!(a.hash(), a.hash());
        assert_ne!(a.hash(), b.hash());

        let hex = a.hash().hex();
        assert_eq!(hex.len(), 64);
        assert!(
            hex.chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_uppercase())
        );
        assert_eq!(SessionHash::from_hex(&hex), Some(a.hash()));

        // The whole point: the thing we write down does not contain the thing
        // a browser presents. A substring check is what the store test does.
        assert!(!hex.contains(&a.encode()));
    }

    #[test]
    fn from_hex_rejects_garbage() {
        assert_eq!(SessionHash::from_hex(""), None);
        assert_eq!(SessionHash::from_hex(&"a".repeat(63)), None);
        assert_eq!(SessionHash::from_hex(&"a".repeat(65)), None);
        assert_eq!(SessionHash::from_hex(&"z".repeat(64)), None);
        // Uppercase is not what `hex` writes, so it is not what we accept:
        // two spellings of one key would be two rows for one session.
        assert_eq!(SessionHash::from_hex(&"A".repeat(64)), None);
    }

    #[test]
    fn decode_rejects_garbage() {
        assert_eq!(SessionId::decode(""), None);
        assert_eq!(SessionId::decode(&"A".repeat(42)), None);
        assert_eq!(SessionId::decode(&"!".repeat(43)), None);
        assert_eq!(SessionId::decode(&"A".repeat(44)), None);
    }

    #[test]
    fn cookie_attributes() {
        let id = SessionId::from_bytes([7; 32]);
        let c = set_cookie(&id, true);
        assert!(c.starts_with(&format!("dj_session={}", id.encode())));
        assert!(c.contains("HttpOnly") && c.contains("SameSite=Strict") && c.contains("Path=/"));
        assert!(c.ends_with("; Secure"));
        assert!(!c.contains("Max-Age"));
        assert!(!set_cookie(&id, false).contains("Secure"));
        assert!(clear_cookie(false).contains("Max-Age=0"));
    }

    #[test]
    fn parse_cookie_header() {
        let id = SessionId::from_bytes([9; 32]);
        let h = format!("a=b; dj_session={}; c=d", id.encode());
        assert_eq!(from_cookie_headers([h.as_str()]), Some(id.clone()));
        assert_eq!(from_cookie_headers(["x=1", h.as_str()]), Some(id));
        assert_eq!(from_cookie_headers(["dj_session=junk"]), None);
        assert_eq!(from_cookie_headers(["other=1"]), None);
        assert_eq!(from_cookie_headers([]), None);
    }

    #[test]
    fn validity() {
        let t0 = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1000);
        let ip: IpAddr = "10.77.0.2".parse().unwrap();
        let s = Session::new(ip, t0, t0 + std::time::Duration::from_secs(60));
        assert!(s.is_valid_for(ip, t0));
        assert!(!s.is_valid_for("10.77.0.3".parse().unwrap(), t0));
        assert!(!s.is_valid_for(ip, t0 + std::time::Duration::from_secs(60)));
    }
}
