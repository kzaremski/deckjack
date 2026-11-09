//! Session identifiers and the `dj_session` cookie.

use std::fmt;
use std::net::IpAddr;
use std::time::SystemTime;

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;

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
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Session {
    pub ip: IpAddr,
    pub created: SystemTime,
    pub expires: SystemTime,
}

impl Session {
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
        let s = Session {
            ip,
            created: t0,
            expires: t0 + std::time::Duration::from_secs(60),
        };
        assert!(s.is_valid_for(ip, t0));
        assert!(!s.is_valid_for("10.77.0.3".parse().unwrap(), t0));
        assert!(!s.is_valid_for(ip, t0 + std::time::Duration::from_secs(60)));
    }
}
