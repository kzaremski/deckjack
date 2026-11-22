//! An OOXML package (a ZIP archive), read defensively and rewritten
//! surgically.
//!
//! Every file this reads arrived from someone else, so the archive is
//! hostile until proven otherwise:
//!
//! - **Size.** The archive, the number of entries, each part read and the
//!   total read are all capped. A part's declared size is checked before it
//!   is decompressed, and the decompression itself stops one byte past the
//!   cap, because a declared size is the attacker's to choose (a zip bomb
//!   declares a small one).
//! - **Names.** An entry named `../x`, `/etc/x`, `C:x`, `a\b` or with a NUL is
//!   refused, and so is the whole package with it. Nothing here extracts to
//!   disk, but a commented copy carries every entry onward, and a reader
//!   further down the line may. Names that differ only by case are refused
//!   too: OPC part names are case-insensitive, so two such entries are one
//!   part with two contents, and which one a reader picks is the attack.
//! - **Methods.** Stored and deflated only; nothing encrypted.
//!
//! Rewriting copies every entry it was not asked to change with
//! [`zip::ZipWriter::raw_copy_file`]: the compressed bytes go across
//! untouched, never decompressed and recompressed, so an untouched part is
//! byte-identical by construction rather than by luck.

use std::collections::{BTreeMap, BTreeSet};
use std::io::{Cursor, Read as _, Write as _};

use zip::{CompressionMethod, ZipArchive, ZipWriter, write::SimpleFileOptions};

/// The bounds a package is read within.
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    /// The archive file itself.
    pub archive: u64,
    pub entries: usize,
    /// One part, decompressed.
    pub part: u64,
    /// All parts read from one package, decompressed.
    pub total: u64,
    /// Decompressed to compressed, beyond which a large part is a bomb.
    pub ratio: u64,
}

pub const LIMITS: Limits = Limits {
    archive: 256 << 20,
    entries: 10_000,
    part: 64 << 20,
    total: 192 << 20,
    ratio: 250,
};

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum PackageError {
    #[error("the file is {size} bytes; Office reads packages up to {limit} bytes")]
    TooLarge { size: u64, limit: u64 },

    #[error("this is not a ZIP package, so not a Word, Excel or PowerPoint file: {0}")]
    NotZip(String),

    #[error("the package has {count} entries; Office reads up to {limit}")]
    TooManyEntries { count: usize, limit: usize },

    #[error("the package holds an entry named {0:?}, which is not a valid part name")]
    BadName(String),

    #[error("the package holds two entries named {0:?}")]
    Duplicate(String),

    #[error("the package's entry {0:?} is encrypted; Office does not open encrypted packages")]
    Encrypted(String),

    #[error(
        "the package's entry {name:?} is compressed with {method}; only stored and deflated entries are read"
    )]
    Method { name: String, method: String },

    #[error("the package has no part {0:?}")]
    Missing(String),

    #[error(
        "the part {name:?} is {size} bytes decompressed; Office reads parts up to {limit} bytes"
    )]
    PartTooLarge { name: String, size: u64, limit: u64 },

    #[error(
        "the part {0:?} expands far more than a document does (a zip bomb); refusing to decompress it"
    )]
    Bomb(String),

    #[error("reading this package would decompress more than {0} bytes in total")]
    TotalTooLarge(u64),

    #[error("the part {name:?} could not be read: {reason}")]
    Corrupt { name: String, reason: String },

    #[error("the part {0:?} is not UTF-8 text")]
    NotText(String),

    #[error("could not write the new package: {0}")]
    Write(String),
}

/// One package, open for reading.
pub struct Package {
    archive: ZipArchive<Cursor<Vec<u8>>>,
    /// Entry names in archive order.
    names: Vec<String>,
    limits: Limits,
    read_so_far: u64,
}

impl Package {
    pub fn open(bytes: Vec<u8>) -> Result<Package, PackageError> {
        Package::open_with(bytes, LIMITS)
    }

    pub fn open_with(bytes: Vec<u8>, limits: Limits) -> Result<Package, PackageError> {
        let size = bytes.len() as u64;
        if size > limits.archive {
            return Err(PackageError::TooLarge {
                size,
                limit: limits.archive,
            });
        }
        let mut archive =
            ZipArchive::new(Cursor::new(bytes)).map_err(|e| PackageError::NotZip(e.to_string()))?;
        if archive.len() > limits.entries {
            return Err(PackageError::TooManyEntries {
                count: archive.len(),
                limit: limits.entries,
            });
        }
        let mut names = Vec::with_capacity(archive.len());
        let mut folded = BTreeSet::new();
        for index in 0..archive.len() {
            let entry = archive
                .by_index_raw(index)
                .map_err(|e| PackageError::NotZip(e.to_string()))?;
            let name = entry
                .name()
                .map_err(|e| PackageError::NotZip(e.to_string()))?
                .into_owned();
            if !valid_part_name(&name) {
                return Err(PackageError::BadName(name));
            }
            if entry.encrypted() {
                return Err(PackageError::Encrypted(name));
            }
            let method = entry.compression();
            if !matches!(
                method,
                CompressionMethod::Stored | CompressionMethod::Deflated
            ) {
                return Err(PackageError::Method {
                    name,
                    method: format!("{method:?}"),
                });
            }
            if !folded.insert(name.to_lowercase()) {
                return Err(PackageError::Duplicate(name));
            }
            names.push(name);
        }
        // The archive's own index keeps one entry per exact name, so a
        // duplicate is invisible to the loop above; it says so separately.
        if archive.has_duplicate_names() {
            return Err(PackageError::Duplicate(
                "(a name that appears twice in the central directory)".to_string(),
            ));
        }
        Ok(Package {
            archive,
            names,
            limits,
            read_so_far: 0,
        })
    }

    /// Entry names, in archive order.
    pub fn names(&self) -> &[String] {
        &self.names
    }

    pub fn has(&self, name: &str) -> bool {
        self.names.iter().any(|n| n == name)
    }

    /// One part's bytes, decompressed within the limits.
    pub fn read(&mut self, name: &str) -> Result<Vec<u8>, PackageError> {
        if !self.has(name) {
            return Err(PackageError::Missing(name.to_string()));
        }
        let limits = self.limits;
        let mut entry = self
            .archive
            .by_name(name)
            .map_err(|e| PackageError::Corrupt {
                name: name.to_string(),
                reason: e.to_string(),
            })?;
        let declared = entry.size();
        if declared > limits.part {
            return Err(PackageError::PartTooLarge {
                name: name.to_string(),
                size: declared,
                limit: limits.part,
            });
        }
        // A large part that is mostly air is a bomb. Small parts are exempt:
        // a run of identical XML is ordinary and compresses well.
        if declared > (1 << 20) && declared / entry.compressed_size().max(1) > limits.ratio {
            return Err(PackageError::Bomb(name.to_string()));
        }
        let budget = limits.total.saturating_sub(self.read_so_far);
        let cap = limits.part.min(budget);
        let mut out = Vec::with_capacity(declared.min(cap) as usize);
        // One byte past the cap, so "exactly at the limit" and "lied about
        // its size and kept going" are told apart.
        (&mut entry)
            .take(cap + 1)
            .read_to_end(&mut out)
            .map_err(|e| PackageError::Corrupt {
                name: name.to_string(),
                reason: e.to_string(),
            })?;
        if out.len() as u64 > cap {
            return Err(if cap < limits.part {
                PackageError::TotalTooLarge(limits.total)
            } else {
                PackageError::PartTooLarge {
                    name: name.to_string(),
                    size: out.len() as u64,
                    limit: limits.part,
                }
            });
        }
        self.read_so_far += out.len() as u64;
        Ok(out)
    }

    /// One part as text. A UTF-8 byte-order mark is dropped.
    pub fn read_text(&mut self, name: &str) -> Result<String, PackageError> {
        let bytes = self.read(name)?;
        let bytes = bytes
            .strip_prefix(&[0xEF, 0xBB, 0xBF][..])
            .map(<[u8]>::to_vec)
            .unwrap_or(bytes);
        String::from_utf8(bytes).map_err(|_| PackageError::NotText(name.to_string()))
    }

    /// A part as text if it is there, `None` if it is not.
    pub fn read_text_opt(&mut self, name: &str) -> Result<Option<String>, PackageError> {
        if self.has(name) {
            self.read_text(name).map(Some)
        } else {
            Ok(None)
        }
    }

    /// A new package: every entry in `replace` takes the new bytes (in the
    /// same place in the archive), every entry in `add` is appended, and
    /// every other entry is copied raw.
    pub fn rewrite(
        &mut self,
        replace: &BTreeMap<String, Vec<u8>>,
        add: &[(String, Vec<u8>)],
    ) -> Result<Vec<u8>, PackageError> {
        let write = |e: zip::result::ZipError| PackageError::Write(e.to_string());
        for name in replace.keys() {
            if !self.has(name) {
                return Err(PackageError::Missing(name.clone()));
            }
        }
        for (name, _) in add {
            if self.has(name) || !valid_part_name(name) {
                return Err(PackageError::BadName(name.clone()));
            }
        }
        let mut out = ZipWriter::new(Cursor::new(Vec::new()));
        for index in 0..self.names.len() {
            let name = self.names[index].clone();
            let entry = self.archive.by_index_raw(index).map_err(write)?;
            match replace.get(&name) {
                Some(bytes) => {
                    let mut options = SimpleFileOptions::default()
                        .compression_method(CompressionMethod::Deflated);
                    if let Some(time) = entry.last_modified() {
                        options = options.last_modified_time(time);
                    }
                    drop(entry);
                    out.start_file(name.as_str(), options).map_err(write)?;
                    out.write_all(bytes)
                        .map_err(|e| PackageError::Write(e.to_string()))?;
                }
                None => out.raw_copy_file(entry).map_err(write)?,
            }
        }
        for (name, bytes) in add {
            out.start_file(
                name.as_str(),
                SimpleFileOptions::default().compression_method(CompressionMethod::Deflated),
            )
            .map_err(write)?;
            out.write_all(bytes)
                .map_err(|e| PackageError::Write(e.to_string()))?;
        }
        Ok(out.finish().map_err(write)?.into_inner())
    }

    /// The raw (still compressed) bytes and CRC of one entry. What
    /// "byte-identical" is checked against.
    pub fn raw(&mut self, name: &str) -> Result<(u32, Vec<u8>), PackageError> {
        let corrupt = |e: String| PackageError::Corrupt {
            name: name.to_string(),
            reason: e,
        };
        let index = self
            .names
            .iter()
            .position(|n| n == name)
            .ok_or_else(|| PackageError::Missing(name.to_string()))?;
        let mut entry = self
            .archive
            .by_index_raw(index)
            .map_err(|e| corrupt(e.to_string()))?;
        let crc = entry.crc32();
        if entry.compressed_size() > self.limits.archive {
            return Err(corrupt("larger than the archive".to_string()));
        }
        let mut bytes = Vec::new();
        // An entry opened raw reads its compressed bytes as they are.
        entry
            .read_to_end(&mut bytes)
            .map_err(|e| corrupt(e.to_string()))?;
        Ok((crc, bytes))
    }
}

/// Whether `name` is a part name this reader accepts: relative, `/`
/// separated, with no empty, `.` or `..` segment, no backslash, no NUL or
/// other control character, and no drive letter. A trailing `/` (a folder
/// entry, which some writers emit) is allowed.
pub fn valid_part_name(name: &str) -> bool {
    if name.is_empty() || name.starts_with('/') || name.contains('\\') {
        return false;
    }
    if name.chars().any(|c| c.is_control()) {
        return false;
    }
    let body = name.strip_suffix('/').unwrap_or(name);
    if body.is_empty() {
        return false;
    }
    let first = body.split('/').next().unwrap_or("");
    if first.len() >= 2 && first.as_bytes()[1] == b':' {
        return false;
    }
    body.split('/')
        .all(|segment| !segment.is_empty() && segment != "." && segment != "..")
}

/// What changed between two packages, entry by entry: the survival report's
/// raw material. Compared on CRC and the compressed bytes, so "unchanged"
/// means byte-identical, not "decompresses to the same".
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Diff {
    pub changed: BTreeSet<String>,
    pub added: BTreeSet<String>,
    pub removed: BTreeSet<String>,
}

pub fn diff(before: &mut Package, after: &mut Package) -> Result<Diff, PackageError> {
    let mut out = Diff::default();
    for name in before.names.clone() {
        if !after.has(&name) {
            out.removed.insert(name);
            continue;
        }
        if before.raw(&name)? != after.raw(&name)? {
            out.changed.insert(name);
        }
    }
    for name in after.names.clone() {
        if !before.has(&name) {
            out.added.insert(name);
        }
    }
    Ok(out)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A package of `(name, bytes)` entries, deflated.
    pub fn zip_of(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut out = ZipWriter::new(Cursor::new(Vec::new()));
        for (name, bytes) in entries {
            out.start_file(
                *name,
                SimpleFileOptions::default().compression_method(CompressionMethod::Deflated),
            )
            .unwrap();
            out.write_all(bytes).unwrap();
        }
        out.finish().unwrap().into_inner()
    }

    #[test]
    fn part_names_that_could_escape_or_mislead_are_refused() {
        for bad in [
            "../evil",
            "word/../../evil",
            "/etc/passwd",
            "C:/evil",
            "word\\document.xml",
            "word//document.xml",
            "word/./document.xml",
            "word/doc\0ument.xml",
            "",
        ] {
            assert!(!valid_part_name(bad), "{bad:?} was accepted");
        }
        for good in [
            "word/document.xml",
            "[Content_Types].xml",
            "_rels/.rels",
            "word/media/",
        ] {
            assert!(valid_part_name(good), "{good:?} was refused");
        }
    }

    #[test]
    fn a_package_holding_a_path_entry_is_refused_whole() {
        let bytes = zip_of(&[
            ("word/document.xml", b"<x/>"),
            ("../../.bashrc", b"rm -rf ~"),
        ]);
        assert_eq!(
            Package::open(bytes).err(),
            Some(PackageError::BadName("../../.bashrc".to_string()))
        );
    }

    #[test]
    fn two_entries_that_differ_only_by_case_are_one_part_and_refused() {
        let bytes = zip_of(&[
            ("word/document.xml", b"<a/>"),
            ("word/Document.xml", b"<b/>"),
        ]);
        assert!(matches!(
            Package::open(bytes).err(),
            Some(PackageError::Duplicate(_))
        ));
    }

    #[test]
    fn a_zip_bomb_is_refused_before_it_is_decompressed() {
        // 8 MiB of zeros deflates to a few KiB: a ratio no document part has.
        let zeros = vec![0u8; 8 << 20];
        let bytes = zip_of(&[("word/document.xml", &zeros)]);
        let mut package = Package::open(bytes).unwrap();
        assert_eq!(
            package.read("word/document.xml").err(),
            Some(PackageError::Bomb("word/document.xml".to_string()))
        );
    }

    #[test]
    fn a_part_over_the_size_cap_is_refused() {
        let limits = Limits {
            part: 1024,
            ..LIMITS
        };
        let bytes = zip_of(&[
            ("big.xml", &[b'a'; 1025][..]),
            ("ok.xml", &[b'a'; 1024][..]),
        ]);
        let mut package = Package::open_with(bytes, limits).unwrap();
        assert!(matches!(
            package.read("big.xml").err(),
            Some(PackageError::PartTooLarge { size: 1025, .. })
        ));
        assert_eq!(package.read("ok.xml").unwrap().len(), 1024);
    }

    #[test]
    fn the_total_read_from_one_package_is_capped() {
        let limits = Limits {
            total: 1500,
            ..LIMITS
        };
        let bytes = zip_of(&[("a.xml", &[b'a'; 1000][..]), ("b.xml", &[b'b'; 1000][..])]);
        let mut package = Package::open_with(bytes, limits).unwrap();
        package.read("a.xml").unwrap();
        assert_eq!(
            package.read("b.xml").err(),
            Some(PackageError::TotalTooLarge(1500))
        );
    }

    #[test]
    fn too_many_entries_and_too_large_an_archive_are_refused() {
        let entries: Vec<(String, Vec<u8>)> =
            (0..5).map(|i| (format!("p{i}.xml"), vec![b'x'])).collect();
        let refs: Vec<(&str, &[u8])> = entries
            .iter()
            .map(|(n, b)| (n.as_str(), b.as_slice()))
            .collect();
        let bytes = zip_of(&refs);
        let few = Limits {
            entries: 4,
            ..LIMITS
        };
        assert!(matches!(
            Package::open_with(bytes.clone(), few).err(),
            Some(PackageError::TooManyEntries { count: 5, .. })
        ));
        let small = Limits {
            archive: bytes.len() as u64 - 1,
            ..LIMITS
        };
        assert!(matches!(
            Package::open_with(bytes, small).err(),
            Some(PackageError::TooLarge { .. })
        ));
    }

    #[test]
    fn an_entry_compressed_with_another_method_is_refused() {
        let mut out = ZipWriter::new(Cursor::new(Vec::new()));
        out.start_file(
            "a.xml",
            SimpleFileOptions::default().compression_method(CompressionMethod::Stored),
        )
        .unwrap();
        out.write_all(b"<a/>").unwrap();
        let mut bytes = out.finish().unwrap().into_inner();
        // Rewrite the method field (stored = 0) to bzip2 (12) in both the
        // local header (offset 8) and the central directory entry.
        bytes[8] = 12;
        let central = bytes
            .windows(4)
            .position(|w| w == [0x50, 0x4b, 0x01, 0x02])
            .unwrap();
        bytes[central + 10] = 12;
        assert!(matches!(
            Package::open(bytes).err(),
            Some(PackageError::Method { .. })
        ));
    }

    #[test]
    fn a_part_declared_over_the_cap_is_refused_without_decompressing_it() {
        // Ten real bytes declared as two thousand: refused on the
        // declaration, before anything is inflated or allocated for it.
        let mut bytes = zip_of(&[("word/document.xml", &[b'a'; 10][..])]);
        let big = 2000u32.to_le_bytes();
        bytes[22..26].copy_from_slice(&big);
        let central = bytes
            .windows(4)
            .position(|w| w == [0x50, 0x4b, 0x01, 0x02])
            .unwrap();
        bytes[central + 24..central + 28].copy_from_slice(&big);
        let limits = Limits {
            part: 1024,
            ..LIMITS
        };
        let mut package = Package::open_with(bytes, limits).unwrap();
        assert!(matches!(
            package.read("word/document.xml"),
            Err(PackageError::PartTooLarge { size: 2000, .. })
        ));
    }

    #[test]
    fn a_part_that_lies_about_its_size_is_stopped_at_the_cap() {
        // Declares 100 bytes and inflates to 4000. The zip crate itself stops
        // at the declared size and reports the archive corrupt (checked by
        // disabling this module's own bounded read: still refused), so the
        // bounded read is the second belt here; what this pins is that such
        // a part is refused, not which layer refuses it.
        let mut bytes = zip_of(&[("word/document.xml", &[b'a'; 4000][..])]);
        let lie = 100u32.to_le_bytes();
        bytes[22..26].copy_from_slice(&lie);
        let central = bytes
            .windows(4)
            .position(|w| w == [0x50, 0x4b, 0x01, 0x02])
            .unwrap();
        bytes[central + 24..central + 28].copy_from_slice(&lie);
        let limits = Limits {
            part: 1024,
            ..LIMITS
        };
        let mut package = Package::open_with(bytes, limits).unwrap();
        let read = package.read("word/document.xml");
        assert!(
            matches!(
                read,
                Err(PackageError::PartTooLarge { .. }) | Err(PackageError::Corrupt { .. })
            ),
            "{read:?}"
        );
        assert!(read.is_err());
    }

    #[test]
    fn a_rewrite_copies_untouched_entries_raw_and_keeps_their_order() {
        // The picture is stored, not deflated: copying it by decompressing
        // and recompressing would change its bytes, so this tells a raw copy
        // from a faithful-looking one.
        let mut out = ZipWriter::new(Cursor::new(Vec::new()));
        for (name, bytes, method) in [
            (
                "[Content_Types].xml",
                &b"<Types/>"[..],
                CompressionMethod::Deflated,
            ),
            (
                "word/document.xml",
                &b"<doc>old</doc>"[..],
                CompressionMethod::Deflated,
            ),
            (
                "word/media/image1.png",
                &[0x89, b'P', b'N', b'G', 1, 2, 3][..],
                CompressionMethod::Stored,
            ),
        ] {
            out.start_file(
                name,
                SimpleFileOptions::default().compression_method(method),
            )
            .unwrap();
            out.write_all(bytes).unwrap();
        }
        let original = out.finish().unwrap().into_inner();
        let mut package = Package::open(original.clone()).unwrap();
        let mut replace = BTreeMap::new();
        replace.insert("word/document.xml".to_string(), b"<doc>new</doc>".to_vec());
        let rewritten = package
            .rewrite(
                &replace,
                &[("word/comments.xml".to_string(), b"<c/>".to_vec())],
            )
            .unwrap();

        let mut before = Package::open(original).unwrap();
        let mut after = Package::open(rewritten).unwrap();
        assert_eq!(
            after.names(),
            [
                "[Content_Types].xml",
                "word/document.xml",
                "word/media/image1.png",
                "word/comments.xml"
            ]
        );
        assert_eq!(after.read("word/document.xml").unwrap(), b"<doc>new</doc>");
        let d = diff(&mut before, &mut after).unwrap();
        assert_eq!(d.changed, BTreeSet::from(["word/document.xml".to_string()]));
        assert_eq!(d.added, BTreeSet::from(["word/comments.xml".to_string()]));
        assert!(d.removed.is_empty());
        for name in ["[Content_Types].xml", "word/media/image1.png"] {
            assert_eq!(
                before.raw(name).unwrap(),
                after.raw(name).unwrap(),
                "{name}"
            );
        }
    }
}
