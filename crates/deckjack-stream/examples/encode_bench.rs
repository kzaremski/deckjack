//! Spike S2: what one picture costs to encode, at the sizes a streamed
//! window has. Run with
//! `cargo run --release -p deckjack-stream --example encode_bench`.
//!
//! The content is synthetic but shaped like a screen: a light background
//! with rows of dark glyph-sized blocks (text), measured three ways: a
//! keyframe, a picture where only a caret-sized patch changed (typing), and
//! a picture scrolled by one line (the worst ordinary case: every row
//! moves). Times are wall clock per picture on one thread, conversion from
//! BGRA included.

#[cfg(target_os = "linux")]
fn main() -> anyhow::Result<()> {
    use deckjack_stream::encode::{Picture, SizedEncoder, load_api};
    use std::time::Instant;

    fn text_like(width: u32, height: u32, scroll: u32, caret: bool) -> Vec<u8> {
        let mut bgra = vec![0xf4u8; (width * height * 4) as usize];
        let mut seed: u32 = 0x1234_5678;
        for row in 0..(height / 18 + 2) {
            let y0 = (row * 18) as i64 - i64::from(scroll % 18) - i64::from(scroll / 18 * 18);
            for col in 0..(width / 8) {
                seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12345);
                if (seed >> 16).is_multiple_of(5) {
                    continue;
                }
                for dy in 3..14 {
                    let y = y0 + dy;
                    if y < 0 || y >= i64::from(height) {
                        continue;
                    }
                    for dx in 1..7 {
                        let x = col * 8 + dx;
                        let px = ((y as u32 * width + x) * 4) as usize;
                        bgra[px..px + 3].copy_from_slice(&[0x30, 0x30, 0x30]);
                    }
                }
            }
        }
        if caret {
            for y in 40..58 {
                for x in 100..102 {
                    let px = ((y * width + x) * 4) as usize;
                    bgra[px..px + 3].copy_from_slice(&[0, 0, 0]);
                }
            }
        }
        bgra
    }

    // `--fixture <path> <width> <height>`: write the browser decode
    // spike's sample instead (spike S3, `web/e2e/spike/stream.e2e.ts`): a
    // keyframe then nine pictures with a moving caret, each as the stream
    // socket sends it, prefixed by its length as a little-endian u32.
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(String::as_str) == Some("--fixture") {
        let path = &args[2];
        let w: u32 = args[3].parse()?;
        let h: u32 = args[4].parse()?;
        let mut enc = SizedEncoder::new(load_api()?, w, h)?;
        let mut out = Vec::new();
        for i in 0..10u32 {
            let mut bgra = text_like(w, h, 0, false);
            // Text in the top 120 rows only, so a full-size sample stays a
            // small file; the rest is background.
            for px in bgra
                .as_chunks_mut::<4>()
                .0
                .iter_mut()
                .skip((120 * w) as usize)
            {
                px[..3].copy_from_slice(&[0xf4, 0xf4, 0xf4]);
            }
            // A caret that moves 8 px a picture, so every picture differs.
            for y in 40..58 {
                for x in (20 + i * 8)..(22 + i * 8) {
                    let px = ((y * w + x) * 4) as usize;
                    bgra[px..px + 3].copy_from_slice(&[0, 0, 0xff]);
                }
            }
            let e = enc.encode(&Picture {
                bgra,
                width: w,
                height: h,
                visible: (w, h),
                keyframe: i == 0,
            })?;
            let header = deckjack_protocol::stream::VideoHeader {
                surface: 0,
                flags: u8::from(e.keyframe),
                width: w as u16,
                height: h as u16,
                timestamp: i * 16,
            };
            let link = header.link_bytes(&e.annex_b);
            let browser = deckjack_protocol::stream::VideoHeader::browser_bytes(&link).unwrap();
            out.extend_from_slice(&(browser.len() as u32).to_le_bytes());
            out.extend_from_slice(browser);
        }
        std::fs::write(path, out)?;
        return Ok(());
    }

    let sizes = [(1920u32, 1080u32), (1280, 800), (800, 600), (300, 252)];
    println!("size        kind     ms(median)  ms(max)  bytes(median)");
    for (w, h) in sizes {
        let mut enc = SizedEncoder::new(load_api()?, w, h)?;
        let base = text_like(w, h, 0, false);
        let pic = |bgra: Vec<u8>, keyframe| Picture {
            bgra,
            width: w,
            height: h,
            visible: (w, h),
            keyframe,
        };
        let mut rows: Vec<(&str, Vec<f64>, Vec<usize>)> = vec![
            ("key", vec![], vec![]),
            ("caret", vec![], vec![]),
            ("scroll", vec![], vec![]),
        ];
        for i in 0..20u32 {
            let begin = Instant::now();
            let e = enc.encode(&pic(base.clone(), true))?;
            rows[0].1.push(begin.elapsed().as_secs_f64() * 1000.0);
            rows[0].2.push(e.annex_b.len());
            let begin = Instant::now();
            let e = enc.encode(&pic(text_like(w, h, 0, i % 2 == 0), false))?;
            rows[1].1.push(begin.elapsed().as_secs_f64() * 1000.0);
            rows[1].2.push(e.annex_b.len());
            let begin = Instant::now();
            let e = enc.encode(&pic(text_like(w, h, (i + 1) * 18, false), false))?;
            rows[2].1.push(begin.elapsed().as_secs_f64() * 1000.0);
            rows[2].2.push(e.annex_b.len());
        }
        // The BGRA to I420 conversion alone, which every picture pays
        // whatever changed.
        let mut yuv = openh264::formats::YUVBuffer::new(w as usize, h as usize);
        let mut convert = Vec::new();
        for _ in 0..20 {
            let begin = Instant::now();
            yuv.read_bgra8(openh264::formats::BgraSliceU8::new(
                &base,
                (w as usize, h as usize),
            ));
            convert.push(begin.elapsed().as_secs_f64() * 1000.0);
        }
        rows.push(("convert", convert, vec![0]));
        for (kind, mut ms, mut bytes) in rows {
            ms.sort_by(f64::total_cmp);
            bytes.sort_unstable();
            println!(
                "{:>4}x{:<5} {:<8} {:>9.2} {:>8.2} {:>13}",
                w,
                h,
                kind,
                ms[ms.len() / 2],
                ms[ms.len() - 1],
                bytes[bytes.len() / 2]
            );
        }
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn main() {}
