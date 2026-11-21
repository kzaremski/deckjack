//! `files.archive.list`: what is inside a zip or a tarball, without
//! extracting it.
//!
//! # Why a command and not the browser
//!
//! Quick Look shows an archive as its listing. Doing that in the browser
//! means pulling the archive across the network to read a directory that, for
//! a zip, sits in the last few kilobytes of the file, and for a tarball means
//! inflating every byte in a tab on a phone. The node has the file on its own
//! disk, so it reads the directory where the bytes are and returns names.
//!
//! # What "bounded" means here, concretely
//!
//! An archive is attacker-authored as far as the node is concerned, so every
//! cost is capped and every cap is reported rather than silently applied:
//!
//! - **Entries.** At most [`MAX_ENTRIES`] are returned (a client may ask for
//!   fewer, never more). `truncated` says when the list stopped short.
//! - **A zip's central directory** is read whole, so it is refused above
//!   [`MAX_ZIP_DIRECTORY`]. Nothing outside the directory is read at all: a
//!   zip bomb's compressed members are never touched.
//! - **A gzipped tarball** has no directory; listing it means inflating it.
//!   Inflation stops after [`MAX_INFLATED`] bytes and the answer says so in
//!   `stopped`. A plain `.tar` is seeked over instead, so it costs one header
//!   read per entry whatever its size.
//!   A plain tarball's header reads stop after [`MAX_HEADER_READ`].
//! - **Names** are capped at [`MAX_NAME_BYTES`] whatever record carries them
//!   (header, GNU long name, pax, zip directory), and a longer one refuses the
//!   listing with the limit named, so a header cannot ask the node to buffer a
//!   gigabyte of "file name".
//! - **The whole listing** stops at [`MAX_LISTING_BYTES`] of serialized
//!   entries, and `stopped` says so: a pax name compresses a thousand to one,
//!   so neither the entry count nor the inflate budget bounds the reply.
//!
//! Nothing is ever written. There is no extraction path in this module to get
//! wrong.
//!
//! # Formats
//!
//! Chosen by the file's first bytes, not its name: a `.zip` that is really a
//! tarball lists as a tarball, and an archive with no extension still lists.
//! Zip (including Zip64), tar (ustar, GNU long names, pax paths and sizes),
//! and tar inside gzip. A gzip holding one file is listed as that one file.
//! 7z, RAR, xz, zstd and bzip2 are named in the refusal rather than guessed
//! at.

use std::io::{self, Read, Seek, SeekFrom};
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::command::{Command, CommandError, Context, RiskLevel};
use crate::files::{Namespace, NodePath, off_thread};

/// The most entries one listing returns.
pub const MAX_ENTRIES: usize = 5_000;

/// A zip central directory larger than this is refused. Sixty-four megabytes
/// of directory is several hundred thousand entries, far past anything a
/// person reads in a preview.
pub const MAX_ZIP_DIRECTORY: u64 = 64 * 1024 * 1024;

/// How much of a compressed tarball is inflated before the listing stops.
pub const MAX_INFLATED: u64 = 512 * 1024 * 1024;

/// How many bytes of a plain tarball's headers and name records are read
/// before the listing stops. Content is seeked over and costs nothing; this
/// bounds an archive made of nothing but pax records.
pub const MAX_HEADER_READ: u64 = 64 * 1024 * 1024;

/// The longest member name listed, in bytes. Linux's `PATH_MAX`: no member
/// with a longer name can be extracted on this node anyway, so a longer one is
/// a crafted archive, and the listing is refused with this limit named.
pub const MAX_NAME_BYTES: usize = 4096;

/// The most one listing may hold and send, measured as its entries
/// serialized. Entry count alone does not bound it: five thousand names of
/// four kilobytes is twenty megabytes of reply. The listing stops here and
/// says so in `stopped`.
pub const MAX_LISTING_BYTES: usize = 4 * 1024 * 1024;

/// The largest pax extended header that is parsed rather than skipped.
const MAX_PAX: u64 = 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArchiveFormat {
    Zip,
    Tar,
    TarGzip,
    /// A gzip of a single file, which is not an archive but is commonly
    /// mistaken for one; listed as the one file inside.
    Gzip,
}

/// One member of an archive.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArchiveEntry {
    /// The member's path inside the archive, exactly as the archive spells
    /// it. Never resolved against anything: it is a label, not an address.
    pub name: String,
    pub is_dir: bool,
    /// A symbolic or hard link rather than content.
    pub is_link: bool,
    /// The member's own length. `None` where the format does not record it.
    pub size: Option<u64>,
    /// How much of the archive it takes up, where the format says (zip).
    pub compressed_size: Option<u64>,
    /// Seconds since the Unix epoch. A zip's DOS timestamp has no time zone
    /// and is reported as if it were UTC.
    pub modified: Option<i64>,
    /// Encrypted, so its content cannot be read without a password.
    pub encrypted: bool,
}

/// What `files.archive.list` returns.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArchiveListing {
    pub path: NodePath,
    pub format: ArchiveFormat,
    pub entries: Vec<ArchiveEntry>,
    /// The number of members the archive says it has, where it says (a zip's
    /// directory does; a tarball has no count until it has been read).
    pub total_entries: Option<u64>,
    /// True when `entries` is not the whole archive.
    pub truncated: bool,
    /// Why the listing stopped early, in words for the operator, when it was
    /// not simply the entry cap.
    pub stopped: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ArchiveParams {
    /// The archive to list.
    pub path: NodePath,
    /// Ask for fewer entries than the server's cap. Never more.
    #[serde(default)]
    pub max_entries: Option<usize>,
}

impl ArchiveParams {
    fn cap(&self) -> usize {
        self.max_entries
            .map(|n| n.clamp(1, MAX_ENTRIES))
            .unwrap_or(MAX_ENTRIES)
    }
}

/// Why an archive could not be listed.
#[derive(Debug)]
pub enum ArchiveError {
    /// Not a format this lists. The message names the format when it can.
    Unsupported(String),
    /// A format this lists, but the bytes do not hold together.
    Corrupt(String),
    Io(io::Error),
}

impl From<io::Error> for ArchiveError {
    fn from(e: io::Error) -> Self {
        ArchiveError::Io(e)
    }
}

impl std::fmt::Display for ArchiveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ArchiveError::Unsupported(why) | ArchiveError::Corrupt(why) => f.write_str(why),
            ArchiveError::Io(e) => write!(f, "the archive could not be read: {e}"),
        }
    }
}

/// The listing itself, independent of the bus: any `Read + Seek`.
///
/// Public so the format handling can be tested against archives built in
/// memory, without a working directory.
pub fn list<R: Read + Seek>(
    mut file: R,
    cap: usize,
) -> Result<(ArchiveFormat, Listed), ArchiveError> {
    let mut magic = [0u8; 512];
    file.seek(SeekFrom::Start(0))?;
    let got = read_up_to(&mut file, &mut magic)?;
    let head = &magic[..got];
    file.seek(SeekFrom::Start(0))?;

    if head.starts_with(b"PK\x03\x04") || head.starts_with(b"PK\x05\x06") {
        return Ok((ArchiveFormat::Zip, zip::list(&mut file, cap)?));
    }
    if head.starts_with(&[0x1f, 0x8b]) {
        return gzip(file, cap);
    }
    if tar::looks_like_header(head) {
        let mut source = tar::Seeking {
            inner: file,
            read: 0,
            budget: MAX_HEADER_READ,
        };
        return Ok((ArchiveFormat::Tar, tar::list(&mut source, cap)?));
    }
    Err(ArchiveError::Unsupported(refusal_for(head)))
}

/// Entries plus how the listing ended.
#[derive(Debug, Default)]
pub struct Listed {
    pub entries: Vec<ArchiveEntry>,
    pub total: Option<u64>,
    pub truncated: bool,
    pub stopped: Option<String>,
    /// Serialized bytes of `entries` so far, against [`MAX_LISTING_BYTES`].
    held: usize,
}

impl Listed {
    /// Adds `entry` if the listing has room for it. When it does not, the
    /// listing is marked as stopped at its size limit and false comes back:
    /// the walk ends there rather than holding a reply nobody can receive.
    fn admit(&mut self, entry: ArchiveEntry) -> bool {
        // As JSON, because that is what the reply is: a name of control
        // characters is six times its own length once escaped.
        let cost = serde_json::to_vec(&entry)
            .map(|v| v.len())
            .unwrap_or(usize::MAX)
            .saturating_add(1);
        if self.held.saturating_add(cost) > MAX_LISTING_BYTES {
            self.truncated = true;
            self.stopped = Some(format!(
                "stopped after {} entries at the {} MiB limit on one listing's size",
                self.entries.len(),
                MAX_LISTING_BYTES / (1024 * 1024)
            ));
            return false;
        }
        self.held += cost;
        self.entries.push(entry);
        true
    }
}

/// Refuses a member name past [`MAX_NAME_BYTES`], naming the limit and not the
/// name: echoing a megabyte of it back would be the reply this prevents.
fn check_name(len: u64) -> Result<(), ArchiveError> {
    if len > MAX_NAME_BYTES as u64 {
        return Err(ArchiveError::Corrupt(format!(
            "a member name of {len} bytes is over the {MAX_NAME_BYTES} byte limit on a \
             name; no file on this node can have it, so the archive is not listed"
        )));
    }
    Ok(())
}

fn refusal_for(head: &[u8]) -> String {
    let named = if head.starts_with(b"7z\xbc\xaf\x27\x1c") {
        Some("7-Zip")
    } else if head.starts_with(b"Rar!\x1a\x07") {
        Some("RAR")
    } else if head.starts_with(b"\xfd7zXZ\x00") {
        Some("xz")
    } else if head.starts_with(&[0x28, 0xb5, 0x2f, 0xfd]) {
        Some("zstd")
    } else if head.starts_with(b"BZh") {
        Some("bzip2")
    } else {
        None
    };
    match named {
        Some(format) => format!(
            "this is a {format} archive, and only zip, tar and gzipped tar can be listed; \
             open it in a terminal to see inside"
        ),
        None => "this is not a zip, tar or gzipped tar archive, so there is no listing to show"
            .to_string(),
    }
}

fn gzip<R: Read + Seek>(file: R, cap: usize) -> Result<(ArchiveFormat, Listed), ArchiveError> {
    let mut decoder = flate2::read::GzDecoder::new(io::BufReader::new(file));
    let mut first = [0u8; 512];
    let got = read_up_to(&mut decoder, &mut first)
        .map_err(|e| ArchiveError::Corrupt(format!("this gzip file does not inflate: {e}")))?;

    if tar::looks_like_header(&first[..got]) {
        let mut source = tar::Inflating {
            inner: decoder,
            pending: Some(first),
            consumed: 0,
            budget: MAX_INFLATED,
        };
        return Ok((ArchiveFormat::TarGzip, tar::list(&mut source, cap)?));
    }

    // One compressed file. Its name is in the header when the compressor kept
    // it; its length is in the trailer, modulo 4 GiB, which is the format's
    // own limit and not worth inflating the whole stream to get round.
    let name = decoder
        .header()
        .and_then(|h| h.filename())
        .map(|n| String::from_utf8_lossy(n).into_owned());
    if let Some(n) = &name {
        check_name(n.len() as u64)?;
    }
    let modified = decoder
        .header()
        .map(|h| i64::from(h.mtime()))
        .filter(|&t| t > 0);
    let mut file = decoder.into_inner().into_inner();
    let size = match file.seek(SeekFrom::End(-4)) {
        Ok(_) => {
            let mut trailer = [0u8; 4];
            file.read_exact(&mut trailer)
                .ok()
                .map(|_| u64::from(u32::from_le_bytes(trailer)))
        }
        Err(_) => None,
    };
    Ok((
        ArchiveFormat::Gzip,
        Listed {
            entries: vec![ArchiveEntry {
                name: name.unwrap_or_else(|| "(unnamed)".to_string()),
                is_dir: false,
                is_link: false,
                size,
                compressed_size: None,
                modified,
                encrypted: false,
            }],
            total: Some(1),
            truncated: false,
            stopped: None,
            held: 0,
        },
    ))
}

fn read_up_to<R: Read>(r: &mut R, buf: &mut [u8]) -> io::Result<usize> {
    let mut filled = 0;
    while filled < buf.len() {
        match r.read(&mut buf[filled..]) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
    Ok(filled)
}

mod zip {
    //! The central directory, and nothing else of the zip.

    use super::*;

    const EOCD: u32 = 0x0605_4b50;
    const EOCD64_LOCATOR: u32 = 0x0706_4b50;
    const EOCD64: u32 = 0x0606_4b50;
    const CENTRAL: u32 = 0x0201_4b50;

    fn u16le(b: &[u8], at: usize) -> u16 {
        u16::from_le_bytes([b[at], b[at + 1]])
    }
    fn u32le(b: &[u8], at: usize) -> u32 {
        u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
    }
    fn u64le(b: &[u8], at: usize) -> u64 {
        let mut x = [0u8; 8];
        x.copy_from_slice(&b[at..at + 8]);
        u64::from_le_bytes(x)
    }

    pub(super) fn list<R: Read + Seek>(file: &mut R, cap: usize) -> Result<Listed, ArchiveError> {
        let len = file.seek(SeekFrom::End(0))?;
        // The end record is 22 bytes plus a comment of up to 65535.
        let tail_len = len.min(22 + 65_535);
        file.seek(SeekFrom::Start(len - tail_len))?;
        let mut tail = vec![0u8; tail_len as usize];
        file.read_exact(&mut tail)?;

        let at = (0..tail.len().saturating_sub(21))
            .rev()
            .find(|&i| u32le(&tail, i) == EOCD)
            .ok_or_else(|| {
                ArchiveError::Corrupt(
                    "this zip has no end-of-directory record, so it is truncated or damaged".into(),
                )
            })?;
        let eocd_offset = len - tail_len + at as u64;
        let mut count = u64::from(u16le(&tail, at + 10));
        let mut dir_size = u64::from(u32le(&tail, at + 12));
        let mut dir_offset = u64::from(u32le(&tail, at + 16));

        // Zip64: the 32-bit fields are saturated and the real values are in a
        // second record, found through a locator just before this one.
        if (count == 0xffff || dir_size == 0xffff_ffff || dir_offset == 0xffff_ffff)
            && eocd_offset >= 20
        {
            file.seek(SeekFrom::Start(eocd_offset - 20))?;
            let mut locator = [0u8; 20];
            file.read_exact(&mut locator)?;
            if u32le(&locator, 0) == EOCD64_LOCATOR {
                let record_at = u64le(&locator, 8);
                file.seek(SeekFrom::Start(record_at))?;
                let mut record = [0u8; 56];
                file.read_exact(&mut record)?;
                if u32le(&record, 0) != EOCD64 {
                    return Err(ArchiveError::Corrupt(
                        "this zip's Zip64 directory record is not where its locator says".into(),
                    ));
                }
                count = u64le(&record, 32);
                dir_size = u64le(&record, 40);
                dir_offset = u64le(&record, 48);
            }
        }

        if dir_size > MAX_ZIP_DIRECTORY {
            return Err(ArchiveError::Unsupported(format!(
                "this zip's directory is {dir_size} bytes, over the {MAX_ZIP_DIRECTORY} byte \
                 limit for a preview; list it in a terminal"
            )));
        }
        if dir_offset.checked_add(dir_size).is_none_or(|end| end > len) {
            return Err(ArchiveError::Corrupt(
                "this zip's directory points past the end of the file".into(),
            ));
        }

        file.seek(SeekFrom::Start(dir_offset))?;
        let mut dir = vec![0u8; dir_size as usize];
        file.read_exact(&mut dir)?;

        let mut out = Listed {
            total: Some(count),
            ..Listed::default()
        };
        let mut i = 0usize;
        while i + 46 <= dir.len() {
            if u32le(&dir, i) != CENTRAL {
                out.stopped = Some(format!(
                    "the zip's directory is damaged after {} entries",
                    out.entries.len()
                ));
                out.truncated = true;
                break;
            }
            if out.entries.len() >= cap {
                out.truncated = true;
                break;
            }
            let made_by = u16le(&dir, i + 4);
            let flags = u16le(&dir, i + 8);
            let time = u16le(&dir, i + 12);
            let date = u16le(&dir, i + 14);
            let mut csize = u64::from(u32le(&dir, i + 20));
            let mut usize_ = u64::from(u32le(&dir, i + 24));
            let name_len = usize::from(u16le(&dir, i + 28));
            let extra_len = usize::from(u16le(&dir, i + 30));
            let comment_len = usize::from(u16le(&dir, i + 32));
            let external = u32le(&dir, i + 38);
            let name_at = i + 46;
            let extra_at = name_at + name_len;
            let next = extra_at + extra_len + comment_len;
            if next > dir.len() {
                out.stopped = Some(format!(
                    "the zip's directory is cut short after {} entries",
                    out.entries.len()
                ));
                out.truncated = true;
                break;
            }
            let raw_name = &dir[name_at..extra_at];
            check_name(raw_name.len() as u64)?;
            let name = if flags & 0x0800 != 0 {
                String::from_utf8_lossy(raw_name).into_owned()
            } else {
                cp437(raw_name)
            };

            let mut modified = dos_time(date, time);
            let extra = &dir[extra_at..extra_at + extra_len];
            let mut e = 0usize;
            while e + 4 <= extra.len() {
                let id = u16le(extra, e);
                let size = usize::from(u16le(extra, e + 2));
                let body_end = (e + 4 + size).min(extra.len());
                let body = &extra[e + 4..body_end];
                match id {
                    // Zip64: whichever 32-bit fields were saturated, in order.
                    0x0001 => {
                        let mut at = 0;
                        if usize_ == 0xffff_ffff && at + 8 <= body.len() {
                            usize_ = u64le(body, at);
                            at += 8;
                        }
                        if csize == 0xffff_ffff && at + 8 <= body.len() {
                            csize = u64le(body, at);
                        }
                    }
                    // Extended timestamp: a real Unix mtime, when present.
                    0x5455 if !body.is_empty() && body[0] & 1 != 0 && body.len() >= 5 => {
                        modified = Some(i64::from(u32le(body, 1) as i32));
                    }
                    _ => {}
                }
                e += 4 + size;
            }

            // Unix permissions in the high half of the external attributes,
            // when the creator was a Unix system.
            let unix_mode = if made_by >> 8 == 3 { external >> 16 } else { 0 };
            let is_link = unix_mode & 0o170_000 == 0o120_000;
            let is_dir = name.ends_with('/') || unix_mode & 0o170_000 == 0o040_000;

            if !out.admit(ArchiveEntry {
                name,
                is_dir,
                is_link,
                size: Some(usize_),
                compressed_size: Some(csize),
                modified,
                encrypted: flags & 1 != 0,
            }) {
                break;
            }
            i = next;
        }
        if !out.truncated && (out.entries.len() as u64) < count {
            out.truncated = true;
            out.stopped.get_or_insert_with(|| {
                format!(
                    "the zip says it holds {count} entries but its directory lists {}",
                    out.entries.len()
                )
            });
        }
        Ok(out)
    }

    /// MS-DOS date and time, as if UTC; `None` for the all-zero "no date".
    fn dos_time(date: u16, time: u16) -> Option<i64> {
        if date == 0 {
            return None;
        }
        let year = i64::from(1980 + (date >> 9));
        let month = i64::from((date >> 5) & 0x0f);
        let day = i64::from(date & 0x1f);
        if !(1..=12).contains(&month) || day == 0 {
            return None;
        }
        let hour = i64::from(time >> 11);
        let minute = i64::from((time >> 5) & 0x3f);
        let second = i64::from((time & 0x1f) * 2);
        Some(days_from_civil(year, month, day) * 86_400 + hour * 3600 + minute * 60 + second)
    }

    /// Days since 1970-01-01 for a proleptic Gregorian date (Howard
    /// Hinnant's algorithm).
    fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
        let y = if m <= 2 { y - 1 } else { y };
        let era = (if y >= 0 { y } else { y - 399 }) / 400;
        let yoe = y - era * 400;
        let mp = (m + 9) % 12;
        let doy = (153 * mp + 2) / 5 + d - 1;
        let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
        era * 146_097 + doe - 719_468
    }

    /// A zip name without the UTF-8 flag is code page 437 by the
    /// specification. Decoded properly rather than lossily, because the
    /// archives most likely to use it are old ones with accented names.
    fn cp437(bytes: &[u8]) -> String {
        const HIGH: &str = "ÇüéâäàåçêëèïîìÄÅÉæÆôöòûùÿÖÜ¢£¥₧ƒáíóúñÑªº¿⌐¬½¼¡«»░▒▓│┤╡╢╖╕╣║╗╝╜╛┐└┴┬├─┼╞╟╚╔╩╦╠═╬╧╨╤╥╙╘╒╓╫╪┘┌█▄▌▐▀αßΓπΣσµτΦΘΩδ∞φε∩≡±≥≤⌠⌡÷≈°∙·√ⁿ²■\u{a0}";
        let high: Vec<char> = HIGH.chars().collect();
        bytes
            .iter()
            .map(|&b| {
                if b < 0x80 {
                    b as char
                } else {
                    high[usize::from(b - 0x80)]
                }
            })
            .collect()
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn a_dos_timestamp_reads_as_the_calendar_time_it_spells() {
            // 2024-03-15 13:45:30.
            let date = ((2024 - 1980) << 9) | (3 << 5) | 15;
            let time = (13 << 11) | (45 << 5) | (30 / 2);
            assert_eq!(dos_time(date, time), Some(1_710_510_330));
            assert_eq!(dos_time(0, 0), None, "zero is the format's 'no date'");
        }

        #[test]
        fn code_page_437_covers_all_128_high_bytes() {
            let all: Vec<u8> = (0x80..=0xff).collect();
            assert_eq!(cp437(&all).chars().count(), 128);
            assert_eq!(cp437(b"caf\x82"), "café");
        }
    }
}

mod tar {
    //! 512-byte headers, one per member, seeked over or inflated past.

    use super::*;

    /// Where tar headers come from: a file that can seek past content, or a
    /// decompressor that has to read through it.
    pub(super) trait Source {
        /// Fill `buf` exactly, or return false at a clean end of input.
        fn block(&mut self, buf: &mut [u8; 512]) -> Result<bool, ArchiveError>;
        /// Move past `n` bytes of content.
        fn skip(&mut self, n: u64) -> Result<(), ArchiveError>;
        /// Read `n` bytes of content into memory (small records only).
        fn take(&mut self, n: u64) -> Result<Vec<u8>, ArchiveError>;
        /// Why to stop early, if the source has spent its budget.
        fn exhausted(&self) -> Option<String> {
            None
        }
    }

    pub(super) struct Seeking<R> {
        pub inner: R,
        /// Header and record bytes read so far. Seeked-over content is not
        /// counted: it is never read.
        pub read: u64,
        /// [`MAX_HEADER_READ`] outside tests.
        pub budget: u64,
    }

    impl<R: Read + Seek> Source for Seeking<R> {
        fn block(&mut self, buf: &mut [u8; 512]) -> Result<bool, ArchiveError> {
            let got = read_up_to(&mut self.inner, buf)?;
            self.read = self.read.saturating_add(got as u64);
            match got {
                0 => Ok(false),
                512 => Ok(true),
                _ => Err(ArchiveError::Corrupt(
                    "the tar archive ends in the middle of a header".into(),
                )),
            }
        }
        fn skip(&mut self, n: u64) -> Result<(), ArchiveError> {
            let n = i64::try_from(n).map_err(|_| {
                ArchiveError::Corrupt("a tar member claims an impossible size".into())
            })?;
            self.inner.seek(SeekFrom::Current(n))?;
            Ok(())
        }
        fn take(&mut self, n: u64) -> Result<Vec<u8>, ArchiveError> {
            let mut buf = vec![0u8; n as usize];
            self.inner.read_exact(&mut buf)?;
            self.read = self.read.saturating_add(n);
            Ok(buf)
        }
        fn exhausted(&self) -> Option<String> {
            (self.read >= self.budget).then(|| {
                format!(
                    "stopped after reading {} MiB of headers; the rest of the archive \
                     was not read",
                    self.budget / (1024 * 1024)
                )
            })
        }
    }

    pub(super) struct Inflating<R> {
        pub inner: R,
        /// The first block, already read to recognize the format.
        pub pending: Option<[u8; 512]>,
        pub consumed: u64,
        /// How many inflated bytes may be read before the walk stops.
        /// [`MAX_INFLATED`] outside tests.
        pub budget: u64,
    }

    impl<R: Read> Inflating<R> {
        fn spend(&mut self, n: u64) {
            self.consumed = self.consumed.saturating_add(n);
        }
    }

    impl<R: Read> Source for Inflating<R> {
        fn block(&mut self, buf: &mut [u8; 512]) -> Result<bool, ArchiveError> {
            if let Some(first) = self.pending.take() {
                *buf = first;
                self.spend(512);
                return Ok(true);
            }
            let got = read_up_to(&mut self.inner, buf).map_err(inflate_error)?;
            self.spend(got as u64);
            match got {
                0 => Ok(false),
                512 => Ok(true),
                _ => Err(ArchiveError::Corrupt(
                    "the tar archive ends in the middle of a header".into(),
                )),
            }
        }
        fn skip(&mut self, n: u64) -> Result<(), ArchiveError> {
            // Never past the budget, even inside one member: a single 50 GB
            // file in a tarball must not be inflated to reach the next header.
            let want = n.min(self.budget.saturating_sub(self.consumed));
            let copied = io::copy(&mut (&mut self.inner).take(want), &mut io::sink())
                .map_err(inflate_error)?;
            self.spend(copied);
            if want < n {
                // Spent: `exhausted` now says so and the walk stops.
                self.consumed = self.budget;
                return Ok(());
            }
            if copied < want {
                return Err(ArchiveError::Corrupt(
                    "the tar archive ends in the middle of a member".into(),
                ));
            }
            Ok(())
        }
        fn take(&mut self, n: u64) -> Result<Vec<u8>, ArchiveError> {
            let mut buf = vec![0u8; n as usize];
            self.inner.read_exact(&mut buf).map_err(inflate_error)?;
            self.spend(n);
            Ok(buf)
        }
        fn exhausted(&self) -> Option<String> {
            (self.consumed >= self.budget).then(|| {
                format!(
                    "stopped after inflating {} MiB; a compressed tarball has no directory, \
                     so listing the rest means reading the rest",
                    self.budget / (1024 * 1024)
                )
            })
        }
    }

    fn inflate_error(e: io::Error) -> ArchiveError {
        ArchiveError::Corrupt(format!("the compressed stream is damaged: {e}"))
    }

    /// A ustar magic, or failing that a header whose checksum adds up (the
    /// pre-POSIX format has no magic at all).
    pub(super) fn looks_like_header(block: &[u8]) -> bool {
        if block.len() < 512 {
            return false;
        }
        if &block[257..262] == b"ustar" {
            return true;
        }
        block[0] != 0 && checksum_ok(block)
    }

    fn checksum_ok(block: &[u8]) -> bool {
        let Some(stored) = octal(&block[148..156]) else {
            return false;
        };
        let sum: u64 = block
            .iter()
            .enumerate()
            .map(|(i, &b)| {
                if (148..156).contains(&i) {
                    32
                } else {
                    u64::from(b)
                }
            })
            .sum();
        sum == stored
    }

    /// An octal field, or GNU's base-256 form for values that do not fit.
    fn octal(field: &[u8]) -> Option<u64> {
        if field.first().is_some_and(|b| b & 0x80 != 0) {
            let mut v: u64 = u64::from(field[0] & 0x7f);
            for &b in &field[1..] {
                v = v.checked_mul(256)?.checked_add(u64::from(b))?;
            }
            return Some(v);
        }
        let text: String = field
            .iter()
            .take_while(|&&b| b != 0)
            .map(|&b| b as char)
            .collect();
        let text = text.trim();
        if text.is_empty() {
            return Some(0);
        }
        u64::from_str_radix(text, 8).ok()
    }

    fn cstr(field: &[u8]) -> String {
        let end = field.iter().position(|&b| b == 0).unwrap_or(field.len());
        String::from_utf8_lossy(&field[..end]).into_owned()
    }

    fn padded(n: u64) -> u64 {
        n.div_ceil(512) * 512
    }

    /// `path=` and `size=` from a pax extended header.
    fn pax(records: &[u8]) -> (Option<String>, Option<u64>, Option<i64>) {
        let (mut path, mut size, mut mtime) = (None, None, None);
        let mut rest = records;
        while !rest.is_empty() {
            let Some(space) = rest.iter().position(|&b| b == b' ') else {
                break;
            };
            let Some(len) = std::str::from_utf8(&rest[..space])
                .ok()
                .and_then(|s| s.parse::<usize>().ok())
            else {
                break;
            };
            if len <= space || len > rest.len() {
                break;
            }
            let record = &rest[space + 1..len];
            let record = record.strip_suffix(b"\n").unwrap_or(record);
            if let Some(eq) = record.iter().position(|&b| b == b'=') {
                let (key, value) = (&record[..eq], &record[eq + 1..]);
                let value = String::from_utf8_lossy(value).into_owned();
                match key {
                    b"path" => path = Some(value),
                    b"size" => size = value.parse().ok(),
                    b"mtime" => mtime = value.split('.').next().and_then(|s| s.parse::<i64>().ok()),
                    _ => {}
                }
            }
            rest = &rest[len..];
        }
        (path, size, mtime)
    }

    pub(super) fn list<S: Source>(source: &mut S, cap: usize) -> Result<Listed, ArchiveError> {
        let mut out = Listed::default();
        let mut long_name: Option<String> = None;
        let mut pax_next: (Option<String>, Option<u64>, Option<i64>) = (None, None, None);
        let mut block = [0u8; 512];
        let mut zeros = 0;

        loop {
            if let Some(why) = source.exhausted() {
                out.truncated = true;
                out.stopped = Some(why);
                break;
            }
            if !source.block(&mut block)? {
                break;
            }
            if block.iter().all(|&b| b == 0) {
                zeros += 1;
                // Two zero blocks end the archive; one is tolerated, as GNU
                // tar does.
                if zeros == 2 {
                    break;
                }
                continue;
            }
            zeros = 0;
            if !checksum_ok(&block) {
                out.truncated = true;
                out.stopped = Some(format!(
                    "a damaged header after {} entries; the rest cannot be trusted",
                    out.entries.len()
                ));
                break;
            }

            let kind = block[156];
            let header_size = octal(&block[124..136]).ok_or_else(|| {
                ArchiveError::Corrupt("a tar header has a size that is not a number".into())
            })?;

            match kind {
                // GNU long name or long link target, for the next member.
                b'L' | b'K' => {
                    // The record holds the name plus its NUL.
                    check_name(header_size.saturating_sub(1))?;
                    let bytes = source.take(header_size)?;
                    source.skip(padded(header_size) - header_size)?;
                    if kind == b'L' {
                        long_name = Some(cstr(&bytes));
                    }
                    continue;
                }
                // Pax: per-member, or global (which is skipped).
                b'x' | b'g' => {
                    if kind == b'x' && header_size <= MAX_PAX {
                        let bytes = source.take(header_size)?;
                        source.skip(padded(header_size) - header_size)?;
                        pax_next = pax(&bytes);
                        if let Some(p) = &pax_next.0 {
                            check_name(p.len() as u64)?;
                        }
                    } else {
                        source.skip(padded(header_size))?;
                    }
                    continue;
                }
                _ => {}
            }

            let (pax_path, pax_size, pax_mtime) = std::mem::take(&mut pax_next);
            let size = pax_size.unwrap_or(header_size);
            let name = match (pax_path, long_name.take()) {
                (Some(p), _) => p,
                (None, Some(l)) => l,
                (None, None) => {
                    let base = cstr(&block[0..100]);
                    let prefix = if &block[257..262] == b"ustar" {
                        cstr(&block[345..500])
                    } else {
                        String::new()
                    };
                    if prefix.is_empty() {
                        base
                    } else {
                        format!("{prefix}/{base}")
                    }
                }
            };
            let is_dir = kind == b'5' || ((kind == 0 || kind == b'0') && name.ends_with('/'));
            let is_link = kind == b'1' || kind == b'2';
            let modified = pax_mtime.or_else(|| octal(&block[136..148]).map(|t| t as i64));

            if out.entries.len() >= cap {
                out.truncated = true;
                break;
            }
            if !out.admit(ArchiveEntry {
                name,
                is_dir,
                is_link,
                size: Some(if is_dir || is_link { 0 } else { size }),
                compressed_size: None,
                modified,
                encrypted: false,
            }) {
                break;
            }

            // Links and directories carry no content whatever `size` says,
            // except in archives that are wrong; trusting `size` for regular
            // members only matches what tar itself does.
            let content = match kind {
                b'1' | b'2' | b'3' | b'4' | b'5' | b'6' => 0,
                _ => size,
            };
            source.skip(padded(content))?;
        }
        Ok(out)
    }
}

// -- the command ------------------------------------------------------------

pub struct ListArchive {
    pub(crate) files: Arc<Namespace>,
}

impl Command for ListArchive {
    const ID: &'static str = "files.archive.list";
    const TITLE: &'static str = "List archive";
    const RISK: RiskLevel = RiskLevel::ReadOnly;

    type Params = ArchiveParams;
    type Output = ArchiveListing;

    async fn run(
        &self,
        params: ArchiveParams,
        cx: Context,
    ) -> Result<ArchiveListing, CommandError> {
        let files = Arc::clone(&self.files);
        let cap = params.cap();
        // Resolved by the namespace, so the boundary and the hidden-state
        // denial are the ones every other reader gets.
        let resolved = off_thread(cx.command.clone(), {
            let files = Arc::clone(&files);
            let path = params.path.clone();
            move || files.resolve(&path)
        })
        .await?;
        let qualified = files.qualify(resolved.path());
        let real = resolved.real_path().to_path_buf();

        let outcome = tokio::task::spawn_blocking(move || {
            // Opened without waiting and judged by the descriptor, so a file
            // swapped for a FIFO after a stat cannot park this thread.
            let (file, meta) = crate::content::open_examined(&real)?;
            if meta.is_dir() {
                return Err(ArchiveError::Unsupported(
                    "that is a folder, not an archive; list it with files.list".into(),
                ));
            }
            // The same rule as files.read: a FIFO would block the open until
            // something writes to it, and a device is not an archive either.
            if !meta.is_file() {
                return Err(ArchiveError::Unsupported(
                    "that is not a regular file, so there is no archive to list".into(),
                ));
            }
            list(io::BufReader::new(file), cap)
        })
        .await
        .map_err(|e| CommandError::Failed(anyhow::anyhow!("the listing did not finish: {e}")))?;

        match outcome {
            Ok((format, listed)) => Ok(ArchiveListing {
                path: qualified,
                format,
                entries: listed.entries,
                total_entries: listed.total,
                truncated: listed.truncated,
                stopped: listed.stopped,
            }),
            // "Not a format this lists" and "damaged" both reach the operator
            // as the sentence that explains them; neither is a fault of the
            // node, and `Unsupported` would read as "this node cannot".
            Err(other) => Err(CommandError::Failed(anyhow::anyhow!("{other}"))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A ustar header for `name` with `size` bytes of content.
    fn header(name: &str, size: u64, kind: u8) -> [u8; 512] {
        let mut h = [0u8; 512];
        h[..name.len()].copy_from_slice(name.as_bytes());
        h[100..107].copy_from_slice(b"0000644");
        h[124..135].copy_from_slice(format!("{size:011o}").as_bytes());
        h[136..147].copy_from_slice(b"00000000000");
        h[156] = kind;
        h[257..263].copy_from_slice(b"ustar\0");
        h[263..265].copy_from_slice(b"00");
        h[148..156].copy_from_slice(b"        ");
        let sum: u32 = h.iter().map(|&b| u32::from(b)).sum();
        h[148..155].copy_from_slice(format!("{sum:06o}\0").as_bytes());
        h
    }

    #[test]
    fn a_compressed_tarball_stops_at_its_budget_and_says_why() {
        // The budget is what keeps a 50 GB tarball from being inflated end to
        // end to answer "what is in here". Two members of 4 KiB each against
        // a 6 KiB budget: the first is listed, the walk stops inside the
        // second, and the answer says it stopped rather than looking whole.
        //
        // The second member is far larger than the budget, and what is read
        // of it is measured: the budget has to hold *inside* a member, or one
        // 50 GB file is inflated in full on the way to a header that is never
        // reached.
        let mut tar = Vec::new();
        tar.extend_from_slice(&header("one.bin", 4096, b'0'));
        tar.extend_from_slice(&[7u8; 4096]);
        tar.extend_from_slice(&header("two.bin", 1 << 20, b'0'));
        tar.extend_from_slice(&vec![7u8; 1 << 20]);
        tar.extend_from_slice(&header("three.bin", 0, b'0'));
        tar.extend_from_slice(&[0u8; 1024]);

        struct Counting<R>(R, std::rc::Rc<std::cell::Cell<u64>>);
        impl<R: Read> Read for Counting<R> {
            fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
                let n = self.0.read(buf)?;
                self.1.set(self.1.get() + n as u64);
                Ok(n)
            }
        }
        let read = std::rc::Rc::new(std::cell::Cell::new(0));
        let budget = 6 * 1024;
        let mut source = tar::Inflating {
            inner: Counting(io::Cursor::new(tar), std::rc::Rc::clone(&read)),
            pending: None,
            consumed: 0,
            budget,
        };
        let listed = tar::list(&mut source, 100).expect("a listing, if a short one");
        assert!(
            read.get() <= budget,
            "read {} bytes against a budget of {budget}",
            read.get()
        );

        let names: Vec<_> = listed.entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, ["one.bin", "two.bin"]);
        assert!(listed.truncated, "a partial listing claimed to be whole");
        assert!(
            listed
                .stopped
                .as_deref()
                .unwrap_or("")
                .contains("inflating"),
            "{:?}",
            listed.stopped
        );
    }

    #[test]
    fn a_plain_tarball_of_nothing_but_records_stops_at_its_read_budget() {
        // A plain .tar is seeked over, so only headers and records are read;
        // but an archive that is nothing *except* pax records would otherwise
        // be read end to end. Each record here is 16 KiB of comment, read and
        // thrown away, against a 64 KiB budget.
        let mut tar = Vec::new();
        for i in 0..64 {
            let body = format!("16393 comment={}\n", "c".repeat(16_393 - 15));
            assert_eq!(body.len(), 16_393);
            tar.extend_from_slice(&header("pax", body.len() as u64, b'x'));
            tar.extend_from_slice(body.as_bytes());
            while tar.len() % 512 != 0 {
                tar.push(0);
            }
            tar.extend_from_slice(&header(&format!("f{i}"), 0, b'0'));
        }
        tar.extend_from_slice(&[0u8; 1024]);

        let budget = 64 * 1024;
        let mut source = tar::Seeking {
            inner: io::Cursor::new(tar),
            read: 0,
            budget,
        };
        let listed = tar::list(&mut source, 1000).expect("a listing, if a short one");

        assert!(
            source.read <= budget + 17 * 1024,
            "read {} bytes against a budget of {budget}",
            source.read
        );
        assert!(listed.truncated, "a partial listing claimed to be whole");
        assert!(
            listed.stopped.as_deref().unwrap_or("").contains("headers"),
            "{:?}",
            listed.stopped
        );
    }
}
