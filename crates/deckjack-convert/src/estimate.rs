//! What a preset will cost on this file, measured before anything is
//! committed: encode short samples, extrapolate size and time, and score the
//! samples against the source with VMAF (SSIM where this ffmpeg has no
//! libvmaf). And one still frame, of the source or of a sample encode.
//!
//! Every number here is a measurement on the operator's own file, and every
//! sentence says it is a sample measurement. The size comes with a band
//! (the spread of the samples, widened), because a file whose start, middle
//! and end compress differently is exactly the file where one number would
//! mislead.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Condvar, Mutex, PoisonError};
use std::time::{Duration, Instant};

use base64::Engine;
use deckjack_app::Cancellation;
use serde::Serialize;
use serde_json::{Value, json};

use crate::encode::{self, Ended};
use crate::files::Grants;
use crate::presets::{self, Options, PresetId, Range};
use crate::probe::{self, Probe};
use crate::tools::{self, Tools, file_url};

/// Seconds in one sample.
pub const SAMPLE_SECS: f64 = 2.0;
/// Where the three samples are taken, as fractions of the stretch encoded.
pub const SAMPLE_AT: [f64; 3] = [0.10, 0.50, 0.85];
/// Below this many seconds, one sample of up to [`SHORT_SAMPLE_SECS`].
const SHORT_FILE_SECS: f64 = 12.0;
const SHORT_SAMPLE_SECS: f64 = 6.0;
/// How far the band is widened past the spread of the samples.
const BAND_WIDEN: f64 = 0.10;
/// The largest still `convert.frame` returns, well under the bus's caps.
pub const MAX_JPEG: usize = 512 * 1024;
pub const MAX_EDGE: u32 = 1280;
/// Measurements (estimates and stills) that run at once. They are read-risk
/// commands an agent may send many of together, and each runs full encodes
/// (plus VMAF) at half the cores: the queue's one-or-two-jobs rule must not
/// be undone by a burst of measurements.
pub const MEASURE_SLOTS: usize = 1;

/// The longest a sample encode may take: a minute per second of sample, and
/// at least two minutes. Far past what a real file needs at these lengths,
/// and a bound on what a file that makes ffmpeg spin can hold.
pub fn sample_limit(seconds: f64) -> Duration {
    Duration::from_secs_f64((seconds * 60.0).max(120.0))
}

/// At most [`MEASURE_SLOTS`] measurements at once; the rest wait their turn
/// (a caller sees "Waiting") or give up when cancelled.
pub struct Gate {
    busy: Mutex<usize>,
    freed: Condvar,
    slots: usize,
}

/// A measurement's turn, given back when dropped.
pub struct Turn<'a>(&'a Gate);

impl Default for Gate {
    fn default() -> Gate {
        Gate::new(MEASURE_SLOTS)
    }
}

impl Gate {
    pub fn new(slots: usize) -> Gate {
        Gate {
            busy: Mutex::new(0),
            freed: Condvar::new(),
            slots: slots.max(1),
        }
    }

    /// Wait for a turn. `waiting` is called once if this call has to wait.
    pub fn enter(&self, cancel: &Cancellation, waiting: impl FnOnce()) -> Result<Turn<'_>, String> {
        let mut busy = self.busy.lock().unwrap_or_else(PoisonError::into_inner);
        let mut waiting = Some(waiting);
        while *busy >= self.slots {
            if let Some(w) = waiting.take() {
                w();
            }
            if cancel.is_cancelled() {
                return Err("cancelled while waiting for another measurement to finish".into());
            }
            busy = self
                .freed
                .wait_timeout(busy, Duration::from_millis(100))
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
        *busy += 1;
        Ok(Turn(self))
    }
}

impl Drop for Turn<'_> {
    fn drop(&mut self) {
        let mut busy = self.0.busy.lock().unwrap_or_else(PoisonError::into_inner);
        *busy -= 1;
        self.0.freed.notify_one();
    }
}

/// A folder of one call's own under the scratch folder, removed with what is
/// in it when the call ends, however it ends. Two estimates (or stills) at
/// once wrote the same fixed names and unlinked or read each other's files.
struct CallDir(PathBuf);

static NEXT_CALL: AtomicU64 = AtomicU64::new(0);

impl CallDir {
    fn new(scratch: &Path) -> Result<CallDir, String> {
        std::fs::create_dir_all(scratch).map_err(|e| format!("no scratch folder: {e}"))?;
        let n = NEXT_CALL.fetch_add(1, Ordering::Relaxed);
        let dir = scratch.join(format!("call-{}-{n}", std::process::id()));
        std::fs::create_dir(&dir).map_err(|e| format!("no scratch folder: {e}"))?;
        Ok(CallDir(dir))
    }

    fn join(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}

impl Drop for CallDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Sample {
    /// Seconds into the source.
    pub at: f64,
    pub seconds: f64,
    pub bytes: u64,
    pub wall_secs: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub score: Option<f64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Estimate {
    pub path: String,
    pub preset: PresetId,
    pub tag: String,
    pub source_bytes: u64,
    pub predicted_bytes: u64,
    /// The band the prediction is stated with.
    pub low_bytes: u64,
    pub high_bytes: u64,
    /// The prediction as a share of what is being converted (the source, or
    /// the part of it in the range).
    pub percent_of_source: f64,
    /// Seconds the whole job is predicted to take, when it encodes.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub predicted_secs: Option<f64>,
    /// Times real time, over the samples.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub speed: Option<f64>,
    /// `VMAF`, `SSIM`, or absent when nothing was scored.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metric: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub score: Option<f64>,
    /// The sentence the window shows about quality.
    pub gloss: String,
    pub samples: Vec<Sample>,
    pub warnings: Vec<String>,
    /// The exact ffmpeg line the job would run on this file.
    pub command: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub real_start: Option<f64>,
}

/// Measure `preset` on the file at `rel`. `progress` hears each step.
#[allow(clippy::too_many_arguments)]
pub fn estimate(
    tools: &Tools,
    grants: &Grants,
    rel: &str,
    preset: PresetId,
    options: &Options,
    scratch: &Path,
    cancel: &Cancellation,
    mut progress: impl FnMut(f32, String),
) -> Result<Estimate, String> {
    let input = grants.media(rel).map_err(|e| e.to_string())?;
    let probe = probe::probe(tools, &input).map_err(|e| e.to_string())?;
    let built = presets::build(preset, &probe, options, tools.threads)?;
    let row = preset.preset();
    let span = options.range.unwrap_or(Range {
        start: 0.0,
        end: probe.duration.unwrap_or(0.0),
    });
    let seconds = built.seconds.unwrap_or(0.0);
    let share = match probe.duration {
        Some(d) if d > 0.0 => (seconds / d).min(1.0),
        _ => 1.0,
    };
    let source_part = (probe.size as f64 * share) as u64;
    let command = presets::command_line(
        &tools.ffmpeg,
        &presets::argv(&input, &built, &PathBuf::from("{output}"), false),
    );
    let mut warnings = built.warnings.clone();

    if !row.encodes {
        // A copy: the size is the source's (or its share in the range), and
        // nothing changes but the container.
        let real_start = match (preset, options.range) {
            (PresetId::Trim, Some(r)) => {
                let k =
                    probe::keyframe_before(tools, &input, r.start).map_err(|e| e.to_string())?;
                if (k - r.start).abs() > 0.05 {
                    warnings.push(format!(
                        "The cut starts at {}, the keyframe before {}.",
                        presets::clock(k),
                        presets::clock(r.start)
                    ));
                }
                Some(k)
            }
            _ => None,
        };
        return Ok(Estimate {
            path: rel.to_string(),
            preset,
            tag: preset.tag(),
            source_bytes: probe.size,
            predicted_bytes: source_part,
            low_bytes: (source_part as f64 * 0.95) as u64,
            high_bytes: (source_part as f64 * 1.05) as u64,
            percent_of_source: 100.0,
            predicted_secs: None,
            speed: None,
            metric: None,
            score: None,
            gloss: "No quality change: the streams are copied, not re-encoded.".into(),
            samples: Vec::new(),
            warnings,
            command,
            real_start,
        });
    }

    if seconds <= 0.0 {
        return Err("the file's length is unknown, so a sample cannot be placed in it".into());
    }
    let scored = row.lossy && probe.video().is_some() && built.output.iter().any(|a| a == "-c:v");
    let metric = if scored { Some(metric_of(tools)) } else { None };
    let places: Vec<(f64, f64)> = if seconds < SHORT_FILE_SECS {
        vec![(span.start, seconds.min(SHORT_SAMPLE_SECS))]
    } else {
        SAMPLE_AT
            .iter()
            .map(|f| {
                let at = span.start + (seconds * f).min(seconds - SAMPLE_SECS);
                (at.max(span.start), SAMPLE_SECS)
            })
            .collect()
    };
    let scratch = CallDir::new(scratch)?;
    let steps = places.len() as f32 * if metric.is_some() { 2.0 } else { 1.0 };
    let mut done = 0.0f32;
    let mut samples = Vec::new();
    for (k, (at, len)) in places.iter().enumerate() {
        if cancel.is_cancelled() {
            return Err("the estimate was cancelled".into());
        }
        progress(
            done / steps,
            format!("Encoding sample {} of {}", k + 1, places.len()),
        );
        let sample = scratch.join(&format!("sample-{k}.{}", built.extension));
        let opts = Options {
            range: Some(Range {
                start: *at,
                end: at + len,
            }),
            ..options.clone()
        };
        let b = presets::build(preset, &probe, &opts, tools.threads)?;
        let started = Instant::now();
        match encode::run_within(
            tools,
            &presets::argv(&input, &b, &sample, false),
            cancel,
            sample_limit(*len),
            |_| {},
        ) {
            Ended::Done => {}
            Ended::Cancelled => return Err("the estimate was cancelled".into()),
            Ended::Failed(e) => return Err(format!("a sample encode failed: {e}")),
        }
        let wall = started.elapsed().as_secs_f64().max(0.001);
        let bytes = std::fs::metadata(&sample).map(|m| m.len()).unwrap_or(0);
        done += 1.0;
        let score = match metric {
            Some(m) => {
                progress(done / steps, format!("Scoring sample {} with {m}", k + 1));
                let s = score(tools, &input, &sample, *at, *len, m)?;
                done += 1.0;
                Some(s)
            }
            None => None,
        };
        let _ = std::fs::remove_file(&sample);
        samples.push(Sample {
            at: *at,
            seconds: *len,
            bytes,
            wall_secs: wall,
            score,
        });
    }
    progress(1.0, "Measured".into());
    let rates: Vec<f64> = samples.iter().map(|s| s.bytes as f64 / s.seconds).collect();
    let mean = rates.iter().sum::<f64>() / rates.len() as f64;
    let (lo, hi) = rates
        .iter()
        .fold((f64::MAX, 0.0f64), |(lo, hi), r| (lo.min(*r), hi.max(*r)));
    let predicted = (mean * seconds) as u64;
    let low = (lo * seconds * (1.0 - BAND_WIDEN)) as u64;
    let high = (hi * seconds * (1.0 + BAND_WIDEN)) as u64;
    let media: f64 = samples.iter().map(|s| s.seconds).sum();
    let wall: f64 = samples.iter().map(|s| s.wall_secs).sum();
    let speed = media / wall;
    let score = metric.map(|_| {
        let scores: Vec<f64> = samples.iter().filter_map(|s| s.score).collect();
        scores.iter().sum::<f64>() / scores.len().max(1) as f64
    });
    let gloss = gloss(preset, &probe, metric, score, samples.len());
    Ok(Estimate {
        path: rel.to_string(),
        preset,
        tag: preset.tag(),
        source_bytes: probe.size,
        predicted_bytes: predicted,
        low_bytes: low,
        high_bytes: high,
        percent_of_source: if source_part > 0 {
            predicted as f64 * 100.0 / source_part as f64
        } else {
            0.0
        },
        predicted_secs: Some(seconds / speed),
        speed: Some(speed),
        metric,
        score,
        gloss,
        samples,
        warnings,
        command,
        real_start: None,
    })
}

/// VMAF if this ffmpeg has libvmaf, SSIM otherwise.
fn metric_of(tools: &Tools) -> &'static str {
    let mut cmd = std::process::Command::new(&tools.ffmpeg);
    cmd.args(["-hide_banner", "-filters"]);
    let has_vmaf = tools::capture(cmd, Duration::from_secs(10))
        .map(|o| {
            o.stdout
                .lines()
                .any(|l| l.split_whitespace().nth(1) == Some("libvmaf"))
        })
        .unwrap_or(false);
    if has_vmaf { "VMAF" } else { "SSIM" }
}

/// Score a sample encode against the same stretch of the source, at the
/// sample's own size (a downscaled share encode is compared with the source
/// scaled the same way, which is how it will be watched).
fn score(
    tools: &Tools,
    source: &Path,
    sample: &Path,
    at: f64,
    len: f64,
    metric: &str,
) -> Result<f64, String> {
    let dims = probe::probe_unsniffed(tools, sample)
        .ok()
        .and_then(|p| p.video().and_then(|v| Some((v.width?, v.height?))))
        .ok_or("the sample has no picture to score")?;
    let threads = tools.threads.to_string();
    let compare = match metric {
        "VMAF" => format!("libvmaf=n_threads={threads}"),
        _ => "ssim".to_string(),
    };
    // Frame for frame, by index: the metric filters pair frames by time,
    // and Matroska keeps milliseconds, so an MKV sample's 0.033 s against
    // the source's 0.0333 s paired neighbors and scored an AV1 sample at
    // VMAF 72 that is 97 frame for frame. The sample is the same stretch at
    // the same rate, so the n-th frames are the same picture.
    let graph = format!(
        "[0:v]format=yuv420p,settb=AVTB,setpts=N[d];\
         [1:v]scale={}:{}:flags=bicubic,format=yuv420p,settb=AVTB,setpts=N[r];[d][r]{compare}",
        dims.0, dims.1
    );
    let mut cmd = std::process::Command::new(&tools.ffmpeg);
    cmd.args(["-hide_banner", "-nostdin", "-loglevel", "info"])
        .args(Tools::input_guard())
        .arg("-i")
        .arg(file_url(sample))
        .args(Tools::input_guard())
        .args(["-ss", &format!("{at:.3}"), "-t", &format!("{len:.3}")])
        .arg("-i")
        .arg(file_url(source))
        .args(["-lavfi", &graph, "-f", "null", "-"]);
    let out = tools::capture(cmd, Duration::from_secs(600))?;
    if !out.success {
        return Err(format!(
            "scoring with {metric} failed: {}",
            tools::last_words(&out.stderr).unwrap_or_default()
        ));
    }
    parse_score(&out.stderr, metric).ok_or_else(|| format!("{metric} printed no score"))
}

/// The score from ffmpeg's log: `VMAF score: 97.34` or `SSIM ... All:0.987`.
pub fn parse_score(log: &str, metric: &str) -> Option<f64> {
    log.lines().rev().find_map(|l| match metric {
        "VMAF" => l.split("VMAF score:").nth(1)?.trim().parse().ok(),
        _ => l
            .split("All:")
            .nth(1)?
            .split_whitespace()
            .next()?
            .parse()
            .ok(),
    })
}

/// The sentence about quality: what was measured, on how much, and what it
/// means in plain words.
pub fn gloss(
    preset: PresetId,
    probe: &Probe,
    metric: Option<&str>,
    score: Option<f64>,
    samples: usize,
) -> String {
    let on = format!(
        "measured on {samples} sample{} of your file",
        if samples == 1 { "" } else { "s" }
    );
    match (metric, score) {
        (Some("VMAF"), Some(s)) => {
            let plain = if s >= 95.0 {
                "hard to tell apart from the source at normal viewing distance"
            } else if s >= 90.0 {
                "close to the source; differences show on a big screen if you look for them"
            } else if s >= 80.0 {
                "visibly softer than the source in places"
            } else {
                "clearly worse than the source"
            };
            format!("VMAF {s:.0}, {on}: {plain}.")
        }
        (Some(_), Some(s)) => {
            let plain = if s >= 0.98 {
                "hard to tell apart from the source"
            } else if s >= 0.95 {
                "close to the source"
            } else {
                "visibly worse than the source"
            };
            format!("SSIM {s:.3}, {on} (this ffmpeg has no VMAF): {plain}.")
        }
        _ => match preset {
            PresetId::AudioFlac if probe.audio().is_some_and(|a| a.is_lossless_audio()) => {
                format!("No loss: FLAC is lossless. Size {on}.")
            }
            PresetId::AudioFlac => {
                format!("Keeps the loss already in the source and adds none. Size {on}.")
            }
            _ => format!("Audio is not scored in this version; size and time are {on}."),
        },
    }
}

/// One still: the source's frame at `at`, or with `preset`, the same frame
/// from an encode of a short sample there. JPEG, base64, at most
/// [`MAX_JPEG`] bytes and `max_edge` pixels on its long side.
#[allow(clippy::too_many_arguments)]
pub fn frame(
    tools: &Tools,
    grants: &Grants,
    rel: &str,
    at: f64,
    preset: Option<PresetId>,
    max_edge: u32,
    scratch: &Path,
    cancel: &Cancellation,
) -> Result<Value, String> {
    let input = grants.media(rel).map_err(|e| e.to_string())?;
    let probe = probe::probe(tools, &input).map_err(|e| e.to_string())?;
    if probe.video().is_none() {
        return Err(format!("{rel} has no picture to take a still from"));
    }
    let at = at
        .max(0.0)
        .min((probe.duration.unwrap_or(at) - 0.05).max(0.0));
    let edge = max_edge.clamp(16, MAX_EDGE);
    let scratch = CallDir::new(scratch)?;
    let (from, seek) = match preset {
        Some(id) => {
            let opts = Options {
                range: Some(Range {
                    start: at,
                    end: at + 1.0,
                }),
                allow_lossy_to_lossless: true,
                ..Options::default()
            };
            let b = presets::build(id, &probe, &opts, tools.threads)?;
            if !b.output.iter().any(|a| a == "-c:v") {
                return Err(format!(
                    "{} does not re-encode the picture",
                    id.preset().title
                ));
            }
            let sample = scratch.join(&format!("still-sample.{}", b.extension));
            match encode::run_within(
                tools,
                &presets::argv(&input, &b, &sample, false),
                cancel,
                sample_limit(1.0),
                |_| {},
            ) {
                Ended::Done => {}
                Ended::Cancelled => return Err("cancelled".into()),
                Ended::Failed(e) => return Err(format!("the sample encode failed: {e}")),
            }
            (sample, 0.0)
        }
        None => (input.clone(), at),
    };
    let still = scratch.join("still.jpg");
    let mut quality = 3;
    let mut edge_now = edge;
    let bytes = loop {
        let _ = std::fs::remove_file(&still);
        let mut cmd = std::process::Command::new(&tools.ffmpeg);
        cmd.args(["-hide_banner", "-nostdin", "-n", "-loglevel", "error"])
            .args(Tools::input_guard())
            .args(["-ss", &format!("{seek:.3}")])
            .arg("-i")
            .arg(file_url(&from))
            .args([
                "-frames:v",
                "1",
                "-vf",
                &format!(
                    "scale='if(gte(iw,ih),min({edge_now},iw),-2)':'if(gte(iw,ih),-2,min({edge_now},ih))'"
                ),
                "-q:v",
                &quality.to_string(),
                "-f",
                "mjpeg",
            ])
            .arg(file_url(&still));
        let out = tools::capture(cmd, Duration::from_secs(60))?;
        if !out.success {
            return Err(format!(
                "the still could not be taken: {}",
                tools::last_words(&out.stderr).unwrap_or_default()
            ));
        }
        let bytes = std::fs::read(&still).map_err(|e| format!("the still was not written: {e}"))?;
        if bytes.len() <= MAX_JPEG || edge_now <= 64 {
            break bytes;
        }
        quality = (quality + 4).min(31);
        edge_now = edge_now * 3 / 4;
    };
    let _ = std::fs::remove_file(&still);
    if preset.is_some() {
        let _ = std::fs::remove_file(&from);
    }
    if bytes.len() > MAX_JPEG {
        return Err("the still is too large to send".into());
    }
    Ok(json!({
        "path": rel,
        "at": at,
        "preset": preset,
        "max_edge": edge_now,
        "bytes": bytes.len(),
        "jpeg": base64::engine::general_purpose::STANDARD.encode(&bytes),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn measurements_take_turns_and_a_waiter_can_be_cancelled() {
        let gate = std::sync::Arc::new(Gate::new(1));
        let first = gate.enter(&Cancellation::new(), || {
            panic!("the first waits for nothing")
        });
        let first = first.unwrap();
        // A second waits, says so, and gives up when cancelled.
        let cancel = Cancellation::new();
        let mut told = false;
        let c = cancel.clone();
        let canceller = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(200));
            c.cancel();
        });
        let refused = gate.enter(&cancel, || told = true);
        canceller.join().unwrap();
        assert!(
            refused.is_err(),
            "a second measurement ran beside the first"
        );
        assert!(told, "the waiter was not told it waits");
        // A third gets its turn as soon as the first ends.
        let g = std::sync::Arc::clone(&gate);
        let third = std::thread::spawn(move || {
            let started = Instant::now();
            let _turn = g.enter(&Cancellation::new(), || {}).unwrap();
            started.elapsed()
        });
        std::thread::sleep(Duration::from_millis(200));
        drop(first);
        let waited = third.join().unwrap();
        assert!(waited >= Duration::from_millis(150), "{waited:?}");
        assert!(waited < Duration::from_secs(2), "{waited:?}");
    }

    #[test]
    fn a_sample_encode_has_a_limit_proportional_to_its_length() {
        assert_eq!(sample_limit(1.0), Duration::from_secs(120));
        assert_eq!(sample_limit(2.0), Duration::from_secs(120));
        assert_eq!(sample_limit(6.0), Duration::from_secs(360));
    }

    #[test]
    fn scores_are_read_from_ffmpegs_log() {
        let vmaf = "[Parsed_libvmaf_4 @ 0x55] VMAF score: 97.344760\n";
        assert_eq!(parse_score(vmaf, "VMAF"), Some(97.34476));
        let ssim = "[Parsed_ssim_4 @ 0x5] SSIM Y:0.990 (20.0) U:0.99 V:0.99 All:0.987654 (19.1)\n";
        assert_eq!(parse_score(ssim, "SSIM"), Some(0.987654));
        assert_eq!(parse_score("nothing here", "VMAF"), None);
    }

    #[test]
    fn the_gloss_says_it_was_sampled_and_what_the_score_means() {
        let p = probe::parse(
            r#"{"streams":[{"index":0,"codec_type":"video","codec_name":"h264"},
                {"index":1,"codec_type":"audio","codec_name":"aac"}],
               "format":{"format_name":"mov","duration":"10"}}"#,
            1,
        )
        .unwrap();
        let g = gloss(PresetId::ShareH264, &p, Some("VMAF"), Some(96.4), 3);
        assert!(g.starts_with("VMAF 96, measured on 3 samples"), "{g}");
        assert!(g.contains("hard to tell apart"));
        assert!(gloss(PresetId::ShareH264, &p, Some("VMAF"), Some(85.0), 3).contains("softer"));
        assert!(
            gloss(PresetId::ShareH264, &p, Some("SSIM"), Some(0.93), 1).contains("1 sample of")
        );
        assert!(gloss(PresetId::AudioOpus, &p, None, None, 3).contains("not scored"));
        assert!(gloss(PresetId::AudioFlac, &p, None, None, 3).contains("Keeps the loss"));
    }
}
