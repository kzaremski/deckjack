//! The authorization code (§9.2).
//!
//! `Code` deliberately has no `Display` impl and a redacting `Debug` impl, so
//! it cannot end up in a log line by accident. The only way to get the text is
//! [`Code::reveal`], which is called solely to answer the CLI over the control
//! socket.

use std::fmt;

use rand::TryRng as _;
use subtle::ConstantTimeEq;

/// Crockford base32: no I, L, O or U.
pub const ALPHABET: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";
pub const LEN: usize = 8;

/// Inputs longer than this are rejected before normalization.
const MAX_INPUT: usize = 64;

#[derive(Clone, PartialEq, Eq)]
pub struct Code([u8; LEN]);

impl fmt::Debug for Code {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Code(<redacted>)")
    }
}

#[derive(Debug, thiserror::Error)]
#[error("the OS random source failed: {0}")]
pub struct RandomError(String);

/// Fill `buf` from the OS random source.
pub fn os_random(buf: &mut [u8]) -> Result<(), RandomError> {
    rand::rngs::SysRng
        .try_fill_bytes(buf)
        .map_err(|e| RandomError(e.to_string()))
}

impl Code {
    pub fn generate() -> Result<Code, RandomError> {
        let mut bytes = [0u8; LEN];
        os_random(&mut bytes)?;
        Ok(Code::from_random_bytes(bytes))
    }

    /// Each byte's low 5 bits pick a symbol. 32 is a power of two, so this
    /// is unbiased.
    pub fn from_random_bytes(bytes: [u8; LEN]) -> Code {
        Code(bytes.map(|b| ALPHABET[usize::from(b & 0x1F)]))
    }

    /// Parse an already-valid code (tests, fixtures).
    #[cfg(test)]
    pub fn parse(s: &str) -> Option<Code> {
        normalize(s).map(Code)
    }

    /// The code as `XXXX-XXXX`. Only for showing on the GPD's own terminal.
    pub fn reveal(&self) -> String {
        let s = String::from_utf8_lossy(&self.0);
        format!("{}-{}", &s[..4], &s[4..])
    }

    /// Normalize `input` and compare in constant time. Malformed input is
    /// simply a mismatch.
    pub fn matches(&self, input: &str) -> bool {
        match normalize(input) {
            Some(n) => bool::from(self.0.ct_eq(&n)),
            None => false,
        }
    }
}

/// Strip spaces and dashes, uppercase, map O→0 and I/L→1. Returns `None`
/// unless the result is exactly 8 symbols from the alphabet.
pub fn normalize(input: &str) -> Option<[u8; LEN]> {
    if input.len() > MAX_INPUT {
        return None;
    }
    let mut out = [0u8; LEN];
    let mut n = 0;
    for c in input.bytes() {
        let c = match c.to_ascii_uppercase() {
            b' ' | b'-' => continue,
            b'O' => b'0',
            b'I' | b'L' => b'1',
            c => c,
        };
        if !ALPHABET.contains(&c) || n == LEN {
            return None;
        }
        out[n] = c;
        n += 1;
    }
    (n == LEN).then_some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_codes_use_the_alphabet() {
        for _ in 0..200 {
            let c = Code::generate().unwrap();
            assert_eq!(c.0.len(), 8);
            assert!(c.0.iter().all(|b| ALPHABET.contains(b)));
        }
    }

    #[test]
    fn every_symbol_reachable() {
        for i in 0..32u8 {
            // High bits are ignored.
            let c = Code::from_random_bytes([i | 0xE0; 8]);
            assert_eq!(c.0[0], ALPHABET[usize::from(i)]);
        }
    }

    #[test]
    fn reveal_format() {
        let c = Code::parse("K7QM3XWD").unwrap();
        assert_eq!(c.reveal(), "K7QM-3XWD");
        assert_eq!(format!("{c:?}"), "Code(<redacted>)");
    }

    #[test]
    fn normalization() {
        let c = Code::parse("K7QM-3XWD").unwrap();
        assert!(c.matches("K7QM-3XWD"));
        assert!(c.matches("k7qm3xwd"));
        assert!(c.matches(" k7qm - 3xwd "));
        assert!(c.matches("K7QM--3XWD"));

        let c = Code::parse("0011ABCD").unwrap();
        assert!(c.matches("OoIL-abcd"));
        assert!(c.matches("o0l1abcd"));
    }

    #[test]
    fn invalid_inputs_fail() {
        let c = Code::parse("K7QM3XWD").unwrap();
        assert!(!c.matches(""));
        assert!(!c.matches("K7QM3XW")); // too short
        assert!(!c.matches("K7QM3XWDD")); // too long
        assert!(!c.matches("K7QM3XWU")); // U is not in the alphabet
        assert!(!c.matches("K7QM_3XWD"));
        assert!(!c.matches("K7QM3XWÐ"));
        assert!(!c.matches(&"A".repeat(10_000)));
        assert!(!c.matches("K7QM3XWE"));
        assert_eq!(normalize("ABCDEFGU"), None);
    }
}
