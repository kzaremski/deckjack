//! An X authority file, so only the compositor's own app reaches its X
//! server.
//!
//! XWayland listens on an abstract Unix socket (the sandbox grants no
//! `/tmp`, so there is nowhere to put a socket file). An abstract socket
//! has no file permissions: any process in the same network namespace can
//! connect to it, the operator's other programs and every other confined
//! app included. So the server is started with `-auth <file>` holding a
//! random MIT-MAGIC-COOKIE-1, and only a client given the same file through
//! `XAUTHORITY` gets in.

use std::io::Write as _;
use std::path::Path;

/// `FamilyWild`: the entry matches any host address.
const FAMILY_WILD: u16 = 0xffff;
const COOKIE_NAME: &[u8] = b"MIT-MAGIC-COOKIE-1";

/// One authority entry in the file format `Xau` reads: family, then four
/// length-prefixed fields (address, display number, auth name, auth data),
/// all lengths big-endian `u16`.
pub fn entry(display: u32, cookie: &[u8; 16]) -> Vec<u8> {
    let number = display.to_string();
    let mut out = Vec::new();
    out.extend_from_slice(&FAMILY_WILD.to_be_bytes());
    for field in [&b""[..], number.as_bytes(), COOKIE_NAME, &cookie[..]] {
        out.extend_from_slice(&(field.len() as u16).to_be_bytes());
        out.extend_from_slice(field);
    }
    out
}

/// A fresh cookie, from the kernel's random source.
pub fn cookie() -> std::io::Result<[u8; 16]> {
    use std::io::Read as _;
    let mut c = [0u8; 16];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut c)?;
    Ok(c)
}

/// Write the authority file for every display number the server might
/// take, owner-only. The server is told the file before it picks a
/// number, so the file names each candidate.
pub fn write(
    path: &Path,
    displays: std::ops::Range<u32>,
    cookie: &[u8; 16],
) -> std::io::Result<()> {
    use std::os::unix::fs::OpenOptionsExt as _;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    for display in displays {
        file.write_all(&entry(display, cookie))?;
    }
    file.flush()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_entry_is_in_the_format_xau_reads() {
        let cookie = [0xab; 16];
        let e = entry(42, &cookie);
        // family
        assert_eq!(&e[0..2], &[0xff, 0xff]);
        // empty address
        assert_eq!(&e[2..4], &[0, 0]);
        // display number "42"
        assert_eq!(&e[4..6], &[0, 2]);
        assert_eq!(&e[6..8], b"42");
        // auth name
        assert_eq!(&e[8..10], &[0, 18]);
        assert_eq!(&e[10..28], COOKIE_NAME);
        // 16 bytes of cookie
        assert_eq!(&e[28..30], &[0, 16]);
        assert_eq!(&e[30..46], &cookie);
        assert_eq!(e.len(), 46);
    }

    /// `xauth`, when installed, reads the file back as one entry per
    /// display with the cookie in hex: the format is right, not merely
    /// self-consistent.
    #[test]
    fn xauth_reads_the_file_back() {
        if !Path::new("/usr/bin/xauth").exists() {
            eprintln!("skipped: xauth is not installed");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth");
        let cookie = [0x5a; 16];
        write(&path, 40..42, &cookie).unwrap();
        let out = std::process::Command::new("/usr/bin/xauth")
            .arg("-f")
            .arg(&path)
            .arg("list")
            .output()
            .unwrap();
        let text = String::from_utf8_lossy(&out.stdout);
        let hex = "5a".repeat(16);
        assert_eq!(text.lines().count(), 2, "{text}");
        assert!(
            text.contains(&format!(":40  MIT-MAGIC-COOKIE-1  {hex}")),
            "{text}"
        );
        assert!(
            text.contains(&format!(":41  MIT-MAGIC-COOKIE-1  {hex}")),
            "{text}"
        );
    }
}
