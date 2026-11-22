//! A minimal uncompressed Bayer DNG, written by hand, so RAW decoding is
//! tested without shipping a camera file.
//!
//! The camera's color matrix is sRGB's own XYZ-to-RGB matrix, which makes
//! the "camera" sRGB-linear: a flat CFA value `v / white` must decode to
//! linear gray `v / white`, which is what the test checks.

/// A tag's value.
pub enum Val {
    Byte(Vec<u8>),
    Ascii(&'static str),
    Short(Vec<u16>),
    Long(Vec<u32>),
    Rational(Vec<(u32, u32)>),
    SRational(Vec<(i32, i32)>),
}

impl Val {
    fn kind_count_bytes(&self) -> (u16, u32, Vec<u8>) {
        match self {
            Val::Byte(v) => (1, v.len() as u32, v.clone()),
            Val::Ascii(s) => {
                let mut b = s.as_bytes().to_vec();
                b.push(0);
                (2, b.len() as u32, b)
            }
            Val::Short(v) => (
                3,
                v.len() as u32,
                v.iter().flat_map(|x| x.to_le_bytes()).collect(),
            ),
            Val::Long(v) => (
                4,
                v.len() as u32,
                v.iter().flat_map(|x| x.to_le_bytes()).collect(),
            ),
            Val::Rational(v) => (
                5,
                v.len() as u32,
                v.iter()
                    .flat_map(|(n, d)| [n.to_le_bytes(), d.to_le_bytes()].concat())
                    .collect(),
            ),
            Val::SRational(v) => (
                10,
                v.len() as u32,
                v.iter()
                    .flat_map(|(n, d)| [n.to_le_bytes(), d.to_le_bytes()].concat())
                    .collect(),
            ),
        }
    }
}

/// A `w` x `h` RGGB DNG whose every photosite reads `value` (of 65535),
/// with EXIF orientation `orientation` and photometric `photometric`
/// (32803 is CFA; anything else is a malformed file for the decoder).
pub fn dng(w: u32, h: u32, value: u16, orientation: u16, photometric: u16) -> Vec<u8> {
    dng_from(w, h, |_, _| value, orientation, photometric)
}

/// A `w` x `h` RGGB DNG whose photosite at `(x, y)` reads `cfa(x, y)` (of
/// 65535). Photosite (0, 0) is red, (1, 1) blue, the other two green.
pub fn dng_from(
    w: u32,
    h: u32,
    cfa: impl Fn(u32, u32) -> u16,
    orientation: u16,
    photometric: u16,
) -> Vec<u8> {
    let image: Vec<u8> = (0..h)
        .flat_map(|y| (0..w).map(move |x| (x, y)))
        .flat_map(|(x, y)| cfa(x, y).to_le_bytes())
        .collect();
    // sRGB's XYZ (D65) to linear RGB, as signed rationals over 10000.
    let m = [
        3.2404542, -1.5371385, -0.4985314, -0.9692660, 1.8760108, 0.0415560, 0.0556434, -0.2040259,
        1.0572252,
    ];
    let mut tags: Vec<(u16, Val)> = vec![
        (254, Val::Long(vec![0])),
        (256, Val::Long(vec![w])),
        (257, Val::Long(vec![h])),
        (258, Val::Short(vec![16])),
        (259, Val::Short(vec![1])),
        (262, Val::Short(vec![photometric])),
        (271, Val::Ascii("Deckjack")),
        (272, Val::Ascii("Test DNG")),
        (273, Val::Long(vec![0])), // patched below
        (274, Val::Short(vec![orientation])),
        (277, Val::Short(vec![1])),
        (278, Val::Long(vec![h])),
        (279, Val::Long(vec![image.len() as u32])),
        (284, Val::Short(vec![1])),
        (33421, Val::Short(vec![2, 2])),
        (33422, Val::Byte(vec![0, 1, 1, 2])),
        (50706, Val::Byte(vec![1, 4, 0, 0])),
        (50708, Val::Ascii("Deckjack Test DNG")),
        (50714, Val::Long(vec![0])),
        (50717, Val::Long(vec![65535])),
        (
            50721,
            Val::SRational(
                m.iter()
                    .map(|v| ((v * 10000.0f64).round() as i32, 10000))
                    .collect(),
            ),
        ),
        (50728, Val::Rational(vec![(1, 1), (1, 1), (1, 1)])),
        (50778, Val::Short(vec![21])),
    ];
    tags.sort_by_key(|t| t.0);
    let n = tags.len() as u32;
    let ifd_len = 2 + n * 12 + 4;
    let data_start = 8 + ifd_len;
    let mut data = Vec::new();
    let mut entries = Vec::new();
    // First pass to size the data area, so the strip offset is known.
    let mut data_len = 0u32;
    for (_, v) in &tags {
        let (_, _, b) = v.kind_count_bytes();
        if b.len() > 4 {
            data_len += (b.len() as u32 + 1) & !1;
        }
    }
    let strip = data_start + data_len;
    for (tag, v) in &mut tags {
        if *tag == 273 {
            *v = Val::Long(vec![strip]);
        }
    }
    for (tag, v) in &tags {
        let (kind, count, mut b) = v.kind_count_bytes();
        let field = if b.len() <= 4 {
            b.resize(4, 0);
            b
        } else {
            let off = data_start + data.len() as u32;
            data.extend(&b);
            if data.len() % 2 == 1 {
                data.push(0);
            }
            off.to_le_bytes().to_vec()
        };
        entries.extend(tag.to_le_bytes());
        entries.extend(kind.to_le_bytes());
        entries.extend(count.to_le_bytes());
        entries.extend(field);
    }
    let mut out = b"II*\0".to_vec();
    out.extend(8u32.to_le_bytes());
    out.extend((n as u16).to_le_bytes());
    out.extend(entries);
    out.extend(0u32.to_le_bytes());
    out.extend(data);
    assert_eq!(out.len() as u32, strip);
    out.extend(image);
    out
}
