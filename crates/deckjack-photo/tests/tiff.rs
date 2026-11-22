//! TIFF input past the `tiff` crate's defaults: a 16-bit RGB frame the size
//! a 45 MP camera makes, and a planar file.

mod common;

use deckjack_photo::color::srgb_decode;
use deckjack_photo::decode;

use common::Workdir;

/// A 16-bit RGB TIFF of D850/A7R size (45.4 MP, 272 MB of samples) is over
/// the `tiff` crate's default 256 MiB buffer limit and under this app's
/// 120 MP cap, so it must open. Written uncompressed, as `photo.export`'s
/// `tiff16` writes it (the `tiff` 0.11 encoder's compressed strip writer
/// produced files its own decoder refused, so it is not used here).
#[test]
fn a_45_megapixel_16_bit_tiff_opens() {
    use tiff::encoder::{TiffEncoder, colortype::RGB16};
    let (w, h) = (8256u32, 5504u32);
    let work = Workdir::new();
    let path = work.photos().join("big.tif");
    let file = std::io::BufWriter::new(std::fs::File::create(&path).unwrap());
    let mut enc = TiffEncoder::new(file).unwrap();
    let mut img = enc.new_image::<RGB16>(w, h).unwrap();
    let rows = 64u32;
    img.rows_per_strip(rows).unwrap();
    let mut strip = vec![0u16; (w * rows * 3) as usize];
    for p in strip.as_chunks_mut::<3>().0 {
        *p = [65535, 0, 0];
    }
    for _ in 0..h.div_ceil(rows) {
        let n = img.next_strip_sample_count() as usize;
        img.write_strip(&strip[..n]).unwrap();
    }
    img.finish().unwrap();
    drop(enc); // flushes the BufWriter

    let s = decode::decode(&path).unwrap();
    assert_eq!((s.width(), s.height()), (w, h));
    assert_eq!(s.info.bits, 16);
    let last = s.pixels.len() - 3;
    assert_eq!(&s.pixels[last..], &[1.0, 0.0, 0.0]);
}

/// A planar (PlanarConfiguration 2) 2x1 RGB TIFF, built by hand: the
/// encoder only writes chunky files.
fn planar_tiff() -> Vec<u8> {
    let mut b = Vec::new();
    b.extend(b"II");
    b.extend(42u16.to_le_bytes());
    b.extend(8u32.to_le_bytes());
    // Ten entries: 2 + 10 * 12 + 4 = 126 bytes, so the IFD ends at 134.
    let (bps, offsets, counts, data) = (134u32, 140u32, 152u32, 164u32);
    let entries: [(u16, u16, u32, u32); 10] = [
        (256, 3, 1, 2),       // ImageWidth
        (257, 3, 1, 1),       // ImageLength
        (258, 3, 3, bps),     // BitsPerSample, 8 8 8
        (259, 3, 1, 1),       // Compression: none
        (262, 3, 1, 2),       // PhotometricInterpretation: RGB
        (273, 4, 3, offsets), // StripOffsets, one strip per plane
        (277, 3, 1, 3),       // SamplesPerPixel
        (278, 3, 1, 1),       // RowsPerStrip
        (279, 4, 3, counts),  // StripByteCounts
        (284, 3, 1, 2),       // PlanarConfiguration: planar
    ];
    b.extend(10u16.to_le_bytes());
    for (tag, kind, count, value) in entries {
        b.extend(tag.to_le_bytes());
        b.extend(kind.to_le_bytes());
        b.extend(count.to_le_bytes());
        b.extend(value.to_le_bytes());
    }
    b.extend(0u32.to_le_bytes());
    assert_eq!(b.len(), bps as usize);
    for _ in 0..3 {
        b.extend(8u16.to_le_bytes());
    }
    for plane in 0..3 {
        b.extend((data + plane * 2).to_le_bytes());
    }
    for _ in 0..3 {
        b.extend(2u32.to_le_bytes());
    }
    assert_eq!(b.len(), data as usize);
    // Pixels (10, 20, 30) and (40, 50, 60), one plane per channel.
    b.extend([10u8, 40, 20, 50, 30, 60]);
    b
}

#[test]
fn a_planar_rgb_tiff_reads_every_plane() {
    let work = Workdir::new();
    let path = work.photos().join("planar.tif");
    std::fs::write(&path, planar_tiff()).unwrap();
    let s = decode::decode(&path).unwrap();
    assert_eq!((s.width(), s.height()), (2, 1));
    let want: Vec<f32> = [10u8, 20, 30, 40, 50, 60]
        .iter()
        .map(|&v| srgb_decode(f32::from(v) / 255.0))
        .collect();
    for (got, want) in s.pixels.iter().zip(&want) {
        assert!((got - want).abs() < 1e-6, "{:?} != {want:?}", s.pixels);
    }
}
