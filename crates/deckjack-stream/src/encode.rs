//! H.264 encoding, one encoder per window, each on its own thread.
//!
//! The compositor's event loop must never wait for an encoder: a 1080p
//! picture takes milliseconds to encode, and the loop is also what answers
//! the app. So the loop hands the newest picture to the window's encoder
//! thread through a one-slot mailbox and goes back to work. A picture that
//! arrives before the previous one was taken replaces it: the viewer wants
//! the newest picture, not every picture. Pictures in an H.264 stream
//! reference the ones before them, so dropping one *after* encoding would
//! corrupt the stream; dropping one *before* encoding costs nothing but a
//! frame of motion.
//!
//! The encoder is the system's libopenh264 (2.6 on the development
//! machine), loaded at run time. Constrained Baseline, no B-frames, so a
//! picture is decodable the moment it arrives, and a keyframe only when
//! asked for (a viewer attached, the seat moved, a picture was lost).

use std::sync::{Arc, Condvar, Mutex};
use std::time::Instant;

use anyhow::{Context as _, Result};
use openh264::encoder::{
    BitRate, Complexity, Encoder, EncoderConfig, FrameRate, FrameType, IntraFramePeriod,
    RateControlMode, UsageType,
};
use openh264::formats::{BgraSliceU8, YUVBuffer};
use openh264::{OpenH264API, Timestamp};

/// Where libopenh264 is looked for, in order.
pub const LIBRARY_PATHS: &[&str] = &[
    "/usr/lib/libopenh264.so.8",
    "/usr/lib64/libopenh264.so.8",
    "/usr/lib/x86_64-linux-gnu/libopenh264.so.8",
    "/usr/lib/aarch64-linux-gnu/libopenh264.so.8",
];

/// Load the system's libopenh264.
///
/// `from_blob_path_unchecked` because the checked loader accepts only
/// Cisco's own binaries, by hash, and a distribution builds its own from
/// the same source. The ABI is soname 8 (2.4 through 2.6), the one the
/// bindings in `openh264-sys2` were generated against.
pub fn load_api() -> Result<OpenH264API> {
    let mut last = None;
    for path in LIBRARY_PATHS {
        if !std::path::Path::new(path).exists() {
            continue;
        }
        // SAFETY: the library at this path is libopenh264 with soname 8,
        // whose C ABI is what the bindings were generated from.
        match unsafe { OpenH264API::from_blob_path_unchecked(path) } {
            Ok(api) => return Ok(api),
            Err(e) => last = Some(format!("{path}: {e}")),
        }
    }
    anyhow::bail!(
        "cannot load libopenh264 (soname 8) from {}{}",
        LIBRARY_PATHS.join(", "),
        last.map(|l| format!("; last error: {l}"))
            .unwrap_or_default()
    )
}

/// Bits per second for a picture of this many pixels. Screen content is
/// mostly still, and the rate controller spends far less than this on a
/// still picture; the number bounds what scrolling or video costs.
pub fn bitrate_for(width: u32, height: u32) -> u32 {
    let pixels = u64::from(width) * u64::from(height);
    // 8 Mbit/s for a full 1920x1080 window, scaled by area, never under
    // 1 Mbit/s so a small window's text stays sharp.
    let scaled = pixels * 8_000_000 / (1920 * 1080);
    scaled.clamp(1_000_000, 8_000_000) as u32
}

/// One picture for an encoder: BGRA bytes (pixman's `a8r8g8b8` on a
/// little-endian machine), even width and height, `stride == width * 4`.
pub struct Picture {
    pub bgra: Vec<u8>,
    /// The encoded size, even in both directions.
    pub width: u32,
    pub height: u32,
    /// The visible size, which the header carries: at most one less.
    pub visible: (u32, u32),
    pub keyframe: bool,
}

/// One encoded picture.
pub struct Encoded {
    pub annex_b: Vec<u8>,
    pub keyframe: bool,
    pub visible: (u32, u32),
    /// Milliseconds spent converting and encoding, for the brief's numbers
    /// and the log.
    pub millis: f64,
}

/// An encoder for one size. Recreated when the window's size changes,
/// which also makes the first picture at the new size a keyframe.
pub struct SizedEncoder {
    encoder: Encoder,
    yuv: YUVBuffer,
    width: u32,
    height: u32,
    started: Instant,
}

impl SizedEncoder {
    pub fn new(api: OpenH264API, width: u32, height: u32) -> Result<SizedEncoder> {
        anyhow::ensure!(
            width.is_multiple_of(2) && height.is_multiple_of(2) && width > 0 && height > 0,
            "an encoder needs a positive, even size, not {width}x{height}"
        );
        let config = EncoderConfig::new()
            .usage_type(UsageType::ScreenContentRealTime)
            .rate_control_mode(RateControlMode::Bitrate)
            .bitrate(BitRate::from_bps(bitrate_for(width, height)))
            .max_frame_rate(FrameRate::from_hz(60.0))
            // A skipped picture is a frame the viewer never sees updated;
            // the compositor already sends only changed pictures.
            .skip_frames(false)
            // Keyframes only when asked for. A periodic one would be a
            // burst of bits every few seconds on a still screen.
            .intra_frame_period(IntraFramePeriod::from_num_frames(0))
            // Neither is supported for screen content; saying so here
            // keeps the encoder from warning about it on every window.
            .adaptive_quantization(false)
            .background_detection(false)
            .complexity(Complexity::Low)
            .num_threads(1);
        let encoder = Encoder::with_api_config(api, config).context("cannot start an encoder")?;
        Ok(SizedEncoder {
            encoder,
            yuv: YUVBuffer::new(width as usize, height as usize),
            width,
            height,
            started: Instant::now(),
        })
    }

    pub fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    /// Convert and encode one picture of this encoder's size.
    pub fn encode(&mut self, picture: &Picture) -> Result<Encoded> {
        anyhow::ensure!(
            (picture.width, picture.height) == (self.width, self.height),
            "picture is {}x{}, encoder is {}x{}",
            picture.width,
            picture.height,
            self.width,
            self.height
        );
        let begin = Instant::now();
        self.yuv.read_bgra8(BgraSliceU8::new(
            &picture.bgra,
            (self.width as usize, self.height as usize),
        ));
        if picture.keyframe {
            self.encoder.force_intra_frame();
        }
        let at = Timestamp::from_millis(self.started.elapsed().as_millis() as u64);
        let stream = self.encoder.encode_at(&self.yuv, at)?;
        let keyframe = matches!(stream.frame_type(), FrameType::IDR | FrameType::I);
        let annex_b = stream.to_vec();
        Ok(Encoded {
            annex_b,
            keyframe,
            visible: picture.visible,
            millis: begin.elapsed().as_secs_f64() * 1000.0,
        })
    }
}

/// The one-slot mailbox between the event loop and an encoder thread.
#[derive(Default)]
struct Slot {
    picture: Option<Picture>,
    closed: bool,
}

/// The event loop's end of one window's encoder.
pub struct EncoderHandle {
    shared: Arc<(Mutex<Slot>, Condvar)>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl EncoderHandle {
    /// Start an encoder thread. Each encoded picture is handed to `sink`
    /// on that thread.
    pub fn spawn(
        name: String,
        api_loader: fn() -> Result<OpenH264API>,
        mut sink: impl FnMut(Encoded) + Send + 'static,
    ) -> Result<EncoderHandle> {
        let shared: Arc<(Mutex<Slot>, Condvar)> = Arc::default();
        let theirs = Arc::clone(&shared);
        let thread = std::thread::Builder::new().name(name).spawn(move || {
            let mut encoder: Option<SizedEncoder> = None;
            loop {
                let picture = {
                    let (slot, ready) = &*theirs;
                    let mut slot = slot.lock().unwrap_or_else(|e| e.into_inner());
                    loop {
                        if slot.closed {
                            return;
                        }
                        if let Some(p) = slot.picture.take() {
                            break p;
                        }
                        slot = ready.wait(slot).unwrap_or_else(|e| e.into_inner());
                    }
                };
                let size = (picture.width, picture.height);
                if encoder.as_ref().map(SizedEncoder::size) != Some(size) {
                    encoder = match api_loader()
                        .and_then(|api| SizedEncoder::new(api, size.0, size.1))
                    {
                        Ok(e) => Some(e),
                        Err(e) => {
                            tracing::error!(error = %e, "cannot start an encoder; this window stays blank");
                            return;
                        }
                    };
                }
                let Some(enc) = encoder.as_mut() else { return };
                match enc.encode(&picture) {
                    Ok(encoded) if !encoded.annex_b.is_empty() => sink(encoded),
                    Ok(_) => {}
                    Err(e) => tracing::warn!(error = %e, "a picture failed to encode"),
                }
            }
        })?;
        Ok(EncoderHandle {
            shared,
            thread: Some(thread),
        })
    }

    /// Hand over the newest picture, replacing one not yet taken. A
    /// replaced picture's keyframe request is carried over, so a keyframe
    /// asked for is never lost to a newer picture.
    pub fn submit(&self, mut picture: Picture) {
        let (slot, ready) = &*self.shared;
        let mut slot = slot.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(old) = slot.picture.take() {
            picture.keyframe |= old.keyframe;
        }
        slot.picture = Some(picture);
        ready.notify_one();
    }
}

impl Drop for EncoderHandle {
    fn drop(&mut self) {
        {
            let (slot, ready) = &*self.shared;
            slot.lock().unwrap_or_else(|e| e.into_inner()).closed = true;
            ready.notify_one();
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Round up to even: H.264 4:2:0 wants even sizes.
pub fn even(n: u32) -> u32 {
    n.saturating_add(n & 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_are_rounded_up_to_even() {
        assert_eq!(even(1), 2);
        assert_eq!(even(2), 2);
        assert_eq!(even(1919), 1920);
    }

    #[test]
    fn the_bitrate_scales_with_area_and_stays_in_bounds() {
        assert_eq!(bitrate_for(1920, 1080), 8_000_000);
        assert_eq!(bitrate_for(3840, 2160), 8_000_000);
        assert_eq!(bitrate_for(100, 100), 1_000_000);
        assert_eq!(bitrate_for(960, 1080), 4_000_000);
    }

    fn api_or_skip() -> Option<OpenH264API> {
        match load_api() {
            Ok(api) => Some(api),
            Err(e) => {
                eprintln!("skipped: {e}");
                None
            }
        }
    }

    /// A gray picture, with a lighter square that moves.
    fn picture(width: u32, height: u32, at: u32, keyframe: bool) -> Picture {
        let mut bgra = vec![0x40u8; (width * height * 4) as usize];
        for y in 0..64.min(height) {
            for x in 0..64.min(width) {
                let px = ((y * width + (x + at) % width) * 4) as usize;
                bgra[px..px + 3].copy_from_slice(&[0xe0, 0xe0, 0xe0]);
            }
        }
        Picture {
            bgra,
            width,
            height,
            visible: (width, height),
            keyframe,
        }
    }

    /// The first picture is a keyframe with SPS and PPS in front, a
    /// following picture is not, and asking for one makes it one.
    #[test]
    fn a_keyframe_comes_first_and_again_only_when_asked_for() {
        let Some(api) = api_or_skip() else { return };
        let mut enc = SizedEncoder::new(api, 320, 240).unwrap();
        let first = enc.encode(&picture(320, 240, 0, false)).unwrap();
        assert!(first.keyframe, "the first picture must decode on its own");
        // Annex B start code, then an SPS (NAL type 7).
        assert_eq!(&first.annex_b[..4], &[0, 0, 0, 1]);
        assert_eq!(first.annex_b[4] & 0x1f, 7);
        let second = enc.encode(&picture(320, 240, 8, false)).unwrap();
        assert!(!second.keyframe);
        let asked = enc.encode(&picture(320, 240, 16, true)).unwrap();
        assert!(asked.keyframe);
    }

    #[test]
    fn an_odd_size_is_refused_rather_than_misread() {
        let Some(api) = api_or_skip() else { return };
        assert!(SizedEncoder::new(api, 321, 240).is_err());
    }

    /// A picture replaced in the mailbox before the encoder took it hands
    /// its keyframe request to the one that replaced it.
    #[test]
    fn a_replaced_picture_keeps_its_keyframe_request() {
        let shared: Arc<(Mutex<Slot>, Condvar)> = Arc::default();
        let handle = EncoderHandle {
            shared: Arc::clone(&shared),
            thread: None,
        };
        handle.submit(picture(4, 4, 0, true));
        handle.submit(picture(4, 4, 1, false));
        let slot = shared.0.lock().unwrap();
        assert!(slot.picture.as_ref().unwrap().keyframe);
    }
}
