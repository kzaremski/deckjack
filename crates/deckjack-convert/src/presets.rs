//! The six presets, and the one place an ffmpeg argument list is built.
//!
//! Nothing a caller sends ever becomes an argument as written. A preset id,
//! a container choice, a range in seconds and two booleans are the whole of
//! what a caller controls; everything else is fixed here and versioned
//! (`share-h264@1`). Changing what a preset does means bumping its version,
//! which the output's container comment records.

use std::path::Path;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::probe::Probe;
use crate::tools::{Tools, file_url};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum PresetId {
    /// Copy the streams into another container. No quality change.
    Remux,
    /// Cut a range by copying. No quality change; starts on a keyframe.
    Trim,
    /// H.264 and AAC in MP4, at most 1080p: plays everywhere.
    ShareH264,
    /// AV1 and Opus in MKV: much smaller, slow to make.
    SmallAv1,
    /// The audio as Opus.
    AudioOpus,
    /// The audio as FLAC, lossless.
    AudioFlac,
}

/// One row of the preset table, as `convert.presets` shows it.
#[derive(Debug, Clone, Serialize)]
pub struct Preset {
    pub id: PresetId,
    pub version: u32,
    pub title: &'static str,
    /// What goes in the output's name: `clip (Share).mp4`.
    pub suffix: &'static str,
    /// Whether it re-encodes with loss, and so is measured before commit.
    pub lossy: bool,
    /// Whether it re-encodes at all (a sample encode predicts its size).
    pub encodes: bool,
    /// Whether it needs a video stream.
    pub needs_video: bool,
    /// The sentence the UI shows about what it costs in quality.
    pub quality: &'static str,
    /// The output extension, when the preset fixes it.
    pub extension: Option<&'static str>,
}

pub const PRESETS: &[Preset] = &[
    Preset {
        id: PresetId::Remux,
        version: 1,
        title: "Change container",
        suffix: "Remux",
        lossy: false,
        encodes: false,
        needs_video: false,
        quality: "No quality change: the streams are copied, not re-encoded. Refused, with \
                  the reason, when the new container cannot hold a stream.",
        extension: None,
    },
    Preset {
        id: PresetId::Trim,
        version: 1,
        title: "Trim",
        suffix: "Trim",
        lossy: false,
        encodes: false,
        needs_video: false,
        quality: "No quality change, but the cut starts on the keyframe at or before the \
                  start you chose; the real start is shown before you commit.",
        extension: None,
    },
    Preset {
        id: PresetId::ShareH264,
        version: 1,
        title: "Share (H.264, MP4)",
        suffix: "Share",
        lossy: true,
        encodes: true,
        needs_video: true,
        quality: "Lossy re-encode, at most 1080p, measured on samples of your file before \
                  you commit. Plays on everything made in the last fifteen years.",
        extension: Some("mp4"),
    },
    Preset {
        id: PresetId::SmallAv1,
        version: 1,
        title: "Small (AV1, MKV)",
        suffix: "Small",
        lossy: true,
        encodes: true,
        needs_video: true,
        quality: "Lossy, usually much smaller at a similar score; measured on samples. Slow \
                  on a laptop CPU, and older devices may not play it.",
        extension: Some("mkv"),
    },
    Preset {
        id: PresetId::AudioOpus,
        version: 1,
        title: "Audio as Opus",
        suffix: "Opus",
        lossy: true,
        encodes: true,
        needs_video: false,
        quality: "Lossy. If the source is already lossy, this is a second generation of \
                  loss, and you are told so.",
        extension: Some("opus"),
    },
    Preset {
        id: PresetId::AudioFlac,
        version: 1,
        title: "Audio as FLAC",
        suffix: "FLAC",
        lossy: false,
        encodes: true,
        needs_video: false,
        quality: "No loss from a lossless source. From a lossy one it is refused unless you \
                  allow it: the file gets bigger and no better.",
        extension: Some("flac"),
    },
];

impl PresetId {
    pub fn preset(self) -> &'static Preset {
        PRESETS
            .iter()
            .find(|p| p.id == self)
            .expect("every id has a row")
    }

    /// `share-h264`.
    pub fn name(self) -> String {
        serde_json::to_value(self)
            .ok()
            .and_then(|v| v.as_str().map(str::to_string))
            .unwrap_or_default()
    }

    /// `share-h264@1`: what the output's comment carries.
    pub fn tag(self) -> String {
        format!("{}@{}", self.name(), self.preset().version)
    }
}

/// The container a remux writes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum Container {
    Mp4,
    Mkv,
}

/// A stretch of the source, in seconds.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Range {
    pub start: f64,
    pub end: f64,
}

impl Range {
    pub fn length(&self) -> f64 {
        self.end - self.start
    }
}

/// Everything a caller may choose besides the preset.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Options {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub range: Option<Range>,
    /// Remux only. Absent: MKV from an MP4 or MOV, MP4 from anything else.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub container: Option<Container>,
    /// Remux and trim: every audio track, not only the first.
    #[serde(default)]
    pub keep_all_audio: bool,
    /// FLAC from a lossy source, which is otherwise refused.
    #[serde(default)]
    pub allow_lossy_to_lossless: bool,
}

/// An argument list for one file, and what the operator should be told
/// about it.
#[derive(Debug, Clone, PartialEq)]
pub struct Built {
    pub preset: PresetId,
    /// Before `-i`: the seek, when there is a range.
    pub before_input: Vec<String>,
    /// After the input: maps, codecs, filters, metadata, `-f`.
    pub output: Vec<String>,
    pub extension: &'static str,
    pub warnings: Vec<String>,
    /// The stretch encoded, in seconds of the source, when it is known.
    pub seconds: Option<f64>,
}

/// Codecs an MP4 can carry by copy. Anything else is refused for MP4 with a
/// sentence, rather than left to fail inside ffmpeg.
const MP4_VIDEO: &[&str] = &[
    "h264",
    "hevc",
    "av1",
    "vp9",
    "mpeg4",
    "mpeg2video",
    "mpeg1video",
];
const MP4_AUDIO: &[&str] = &["aac", "mp3", "opus", "flac", "ac3", "eac3", "alac", "mp2"];
const MP4_SUBTITLES: &[&str] = &["mov_text"];
/// Subtitles a Matroska file holds by copy.
const MKV_SUBTITLES: &[&str] = &[
    "subrip",
    "ass",
    "ssa",
    "webvtt",
    "hdmv_pgs_subtitle",
    "dvd_subtitle",
    "dvb_subtitle",
    "text",
];

/// The starting encoder values (lane brief, S2; see the lane doc's "Built"
/// section for how they were chosen and what was measured).
pub const H264_CRF: u32 = 23;
pub const H264_PRESET: &str = "medium";
pub const AAC_BITRATE: &str = "160k";
pub const AV1_CRF: u32 = 35;
pub const AV1_PRESET: u32 = 8;
pub const AV1_OPUS_BITRATE: &str = "96k";
pub const OPUS_STEREO: &str = "128k";
pub const OPUS_SURROUND: &str = "256k";

/// Build the arguments for `preset` on a file described by `probe`.
pub fn build(id: PresetId, probe: &Probe, opts: &Options, threads: usize) -> Result<Built, String> {
    let preset = id.preset();
    let mut warnings = Vec::new();
    if preset.needs_video && probe.video().is_none() {
        return Err(format!(
            "{} needs a video stream, and this file has none; use Audio as Opus or Audio as \
             FLAC",
            preset.title
        ));
    }
    if matches!(id, PresetId::AudioOpus | PresetId::AudioFlac) && probe.audio().is_none() {
        return Err(format!(
            "{} needs an audio stream, and this file has none",
            preset.title
        ));
    }
    if id == PresetId::Trim && opts.range.is_none() {
        return Err("Trim needs a start and an end".into());
    }
    let range = match opts.range {
        Some(r) => Some(check_range(r, probe.duration)?),
        None => None,
    };
    let mut before_input = Vec::new();
    let mut output = Vec::new();
    if let Some(r) = range {
        before_input.extend(["-ss".to_string(), secs(r.start)]);
        output.extend(["-t".to_string(), secs(r.length())]);
    }
    let seconds = range.map(|r| r.length()).or(probe.duration);
    let comment = format!("comment=deckjack-convert {}", id.tag());
    let threads = threads.to_string();

    let extension = match id {
        PresetId::Remux | PresetId::Trim => {
            let container = match id {
                PresetId::Remux => opts.container.unwrap_or(if probe.demuxer() == "mov" {
                    Container::Mkv
                } else {
                    Container::Mp4
                }),
                _ => {
                    if probe.demuxer() == "mov" {
                        Container::Mp4
                    } else {
                        Container::Mkv
                    }
                }
            };
            let keep_all = opts.keep_all_audio || id == PresetId::Trim;
            copy_streams(probe, container, keep_all, &mut output, &mut warnings)?;
            output.extend(["-c", "copy"].map(String::from));
            if range.is_some() {
                output.extend(["-avoid_negative_ts", "make_zero"].map(String::from));
            }
            output.extend(["-map_metadata", "0", "-map_chapters", "0"].map(String::from));
            output.extend(["-metadata".to_string(), comment]);
            match container {
                Container::Mp4 => {
                    output.extend(["-movflags", "+faststart", "-f", "mp4"].map(String::from));
                    "mp4"
                }
                Container::Mkv => {
                    output.extend(["-f", "matroska"].map(String::from));
                    "mkv"
                }
            }
        }
        PresetId::ShareH264 => {
            output.extend(["-map", "0:v:0", "-map", "0:a:0?"].map(String::from));
            output.extend([
                "-vf".to_string(),
                // At most 1920 by 1080 in the frame's own orientation (a
                // portrait phone video stays portrait, at most 1080 wide),
                // never upscaled, even sizes for 4:2:0.
                "scale=w='if(gte(iw,ih),min(1920,iw),min(1080,iw))':\
                 h='if(gte(iw,ih),min(1080,ih),min(1920,ih))':\
                 force_original_aspect_ratio=decrease:force_divisible_by=2"
                    .to_string(),
            ]);
            output.extend(
                [
                    "-c:v",
                    "libx264",
                    "-preset",
                    H264_PRESET,
                    "-crf",
                    &H264_CRF.to_string(),
                    "-pix_fmt",
                    "yuv420p",
                    "-threads",
                    &threads,
                    "-c:a",
                    "aac",
                    "-b:a",
                    AAC_BITRATE,
                ]
                .map(String::from),
            );
            output.extend(["-map_metadata", "0", "-map_chapters", "0"].map(String::from));
            output.extend(["-metadata".to_string(), comment]);
            output.extend(["-movflags", "+faststart", "-f", "mp4"].map(String::from));
            "mp4"
        }
        PresetId::SmallAv1 => {
            output.extend(["-map", "0:v:0", "-map", "0:a:0?"].map(String::from));
            output.extend([
                "-vf".to_string(),
                "scale=trunc(iw/2)*2:trunc(ih/2)*2".to_string(),
            ]);
            output.extend(
                [
                    "-c:v",
                    "libsvtav1",
                    "-preset",
                    &AV1_PRESET.to_string(),
                    "-crf",
                    &AV1_CRF.to_string(),
                    "-pix_fmt",
                    "yuv420p",
                    "-threads",
                    &threads,
                    "-c:a",
                    "libopus",
                    "-b:a",
                    AV1_OPUS_BITRATE,
                ]
                .map(String::from),
            );
            output.extend(["-map_metadata", "0", "-map_chapters", "0"].map(String::from));
            output.extend(["-metadata".to_string(), comment]);
            output.extend(["-f", "matroska"].map(String::from));
            "mkv"
        }
        PresetId::AudioOpus => {
            let audio = probe.audio().expect("checked above");
            if !audio.is_lossless_audio() {
                warnings.push(format!(
                    "The source audio is already lossy ({}): Opus adds a second generation of \
                     loss.",
                    audio.codec
                ));
            }
            let bitrate = if audio.channels.unwrap_or(2) > 2 {
                OPUS_SURROUND
            } else {
                OPUS_STEREO
            };
            output.extend(
                ["-map", "0:a:0", "-vn", "-c:a", "libopus", "-b:a", bitrate].map(String::from),
            );
            output.extend(["-map_metadata", "0"].map(String::from));
            output.extend(["-metadata".to_string(), comment]);
            output.extend(["-f", "opus"].map(String::from));
            "opus"
        }
        PresetId::AudioFlac => {
            let audio = probe.audio().expect("checked above");
            if !audio.is_lossless_audio() {
                if !opts.allow_lossy_to_lossless {
                    return Err(format!(
                        "the source audio is already lossy ({}): FLAC would make the file \
                         bigger and no better. Allow lossy to lossless to do it anyway",
                        audio.codec
                    ));
                }
                warnings.push(format!(
                    "The source audio is lossy ({}): the FLAC keeps that loss and is bigger.",
                    audio.codec
                ));
            }
            output.extend(
                [
                    "-map",
                    "0:a:0",
                    "-vn",
                    "-c:a",
                    "flac",
                    "-compression_level",
                    "5",
                    "-map_metadata",
                    "0",
                ]
                .map(String::from),
            );
            output.extend(["-metadata".to_string(), comment]);
            output.extend(["-f", "flac"].map(String::from));
            "flac"
        }
    };
    Ok(Built {
        preset: id,
        before_input,
        output,
        extension,
        warnings,
        seconds,
    })
}

/// Maps for a stream copy into `container`, refusing a stream it cannot
/// hold and saying which subtitles were left behind.
fn copy_streams(
    probe: &Probe,
    container: Container,
    keep_all_audio: bool,
    output: &mut Vec<String>,
    warnings: &mut Vec<String>,
) -> Result<(), String> {
    let name = match container {
        Container::Mp4 => "MP4",
        Container::Mkv => "MKV",
    };
    if let Some(v) = probe.video() {
        if container == Container::Mp4 && !MP4_VIDEO.contains(&v.codec.as_str()) {
            return Err(format!(
                "{name} cannot hold the video ({}) without re-encoding it; change it to MKV, \
                 or use Share",
                v.codec
            ));
        }
        output.extend(["-map".to_string(), format!("0:{}", v.index)]);
        if container == Container::Mp4 && v.codec == "hevc" {
            // The tag Apple's players need to play HEVC in MP4 at all.
            output.extend(["-tag:v", "hvc1"].map(String::from));
        }
    }
    let audio: Vec<_> = probe.of_kind("audio").collect();
    let audio = if keep_all_audio {
        &audio[..]
    } else {
        &audio[..audio.len().min(1)]
    };
    for a in audio {
        if container == Container::Mp4 && !MP4_AUDIO.contains(&a.codec.as_str()) {
            return Err(format!(
                "{name} cannot hold the audio ({}) without re-encoding it; change it to MKV",
                a.codec
            ));
        }
        output.extend(["-map".to_string(), format!("0:{}", a.index)]);
    }
    let holds = match container {
        Container::Mp4 => MP4_SUBTITLES,
        Container::Mkv => MKV_SUBTITLES,
    };
    let mut dropped = Vec::new();
    for s in probe.of_kind("subtitle") {
        if holds.contains(&s.codec.as_str()) {
            output.extend(["-map".to_string(), format!("0:{}", s.index)]);
        } else if !dropped.contains(&s.codec) {
            dropped.push(s.codec.clone());
        }
    }
    if !dropped.is_empty() {
        warnings.push(format!(
            "Subtitles dropped: {name} cannot hold {}.",
            dropped.join(", ")
        ));
    }
    Ok(())
}

/// A range inside the file, with something in it.
fn check_range(r: Range, duration: Option<f64>) -> Result<Range, String> {
    if !(r.start.is_finite() && r.end.is_finite()) || r.start < 0.0 {
        return Err("the start and end must be seconds from the beginning".into());
    }
    if r.end <= r.start {
        return Err(format!(
            "the end ({}) must come after the start ({})",
            clock(r.end),
            clock(r.start)
        ));
    }
    if let Some(d) = duration
        && r.start >= d
    {
        return Err(format!(
            "the start ({}) is past the end of the file ({})",
            clock(r.start),
            clock(d)
        ));
    }
    let end = duration.map_or(r.end, |d| r.end.min(d));
    Ok(Range {
        start: r.start,
        end,
    })
}

/// Seconds as ffmpeg reads them, to the millisecond.
fn secs(s: f64) -> String {
    format!("{s:.3}")
}

/// `1:02:03.5` or `2:03.5`, for sentences.
pub fn clock(s: f64) -> String {
    let s = s.max(0.0);
    let whole = s.floor() as u64;
    let tenths = ((s - s.floor()) * 10.0).round() as u64 % 10;
    let (h, m, sec) = (whole / 3600, (whole % 3600) / 60, whole % 60);
    if h > 0 {
        format!("{h}:{m:02}:{sec:02}.{tenths}")
    } else {
        format!("{m}:{sec:02}.{tenths}")
    }
}

/// The whole argument list for one run: the input guard, the input, the
/// preset's arguments and the output, each path as a `file:` URL. `progress`
/// adds the machine-readable progress stream on stdout.
pub fn argv(input: &Path, built: &Built, output: &Path, progress: bool) -> Vec<String> {
    let mut args: Vec<String> = ["-hide_banner", "-nostdin", "-n", "-loglevel", "error"]
        .map(String::from)
        .to_vec();
    if progress {
        args.extend(["-nostats", "-progress", "pipe:1", "-stats_period", "1"].map(String::from));
    }
    args.extend(Tools::input_guard());
    args.extend(built.before_input.iter().cloned());
    args.extend(["-i".to_string(), file_url(input)]);
    args.extend(built.output.iter().cloned());
    args.push(file_url(output));
    args
}

/// The exact command line, for "Show command": quoted for a POSIX shell so
/// it can be pasted and run without Deckjack.
pub fn command_line(ffmpeg: &Path, args: &[String]) -> String {
    std::iter::once(ffmpeg.display().to_string())
        .chain(args.iter().cloned())
        .map(|a| shell_quote(&a))
        .collect::<Vec<_>>()
        .join(" ")
}

fn shell_quote(s: &str) -> String {
    let plain = !s.is_empty()
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_./:=+@%,".contains(&b));
    if plain {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', r"'\''"))
    }
}

/// The preset table for `convert.presets`, with each preset's argument
/// template on a typical phone video (`{input}` and `{output}` stand for the
/// paths).
pub fn table(threads: usize) -> Value {
    let sample = crate::probe::parse(
        r#"{"streams":[
            {"index":0,"codec_type":"video","codec_name":"h264","width":1920,"height":1080},
            {"index":1,"codec_type":"audio","codec_name":"aac","channels":2}],
          "format":{"format_name":"mov,mp4,m4a,3gp,3g2,mj2","duration":"60"}}"#,
        0,
    )
    .expect("the sample parses");
    let rows: Vec<Value> = PRESETS
        .iter()
        .map(|p| {
            let opts = Options {
                range: (p.id == PresetId::Trim).then_some(Range {
                    start: 10.0,
                    end: 20.0,
                }),
                allow_lossy_to_lossless: true,
                ..Options::default()
            };
            let template = build(p.id, &sample, &opts, threads).map(|b| {
                let mut args = Tools::input_guard();
                args.extend(b.before_input);
                args.extend(["-i".to_string(), "{input}".to_string()]);
                args.extend(b.output);
                args.push("{output}".to_string());
                args
            });
            json!({
                "id": p.id,
                "version": p.version,
                "tag": p.id.tag(),
                "title": p.title,
                "suffix": p.suffix,
                "lossy": p.lossy,
                "encodes": p.encodes,
                "needs_video": p.needs_video,
                "quality": p.quality,
                "extension": p.extension,
                "template": template.unwrap_or_default(),
            })
        })
        .collect();
    Value::Array(rows)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::probe::parse;

    fn probe(streams: &str, format: &str) -> Probe {
        parse(
            &format!(
                r#"{{"streams":[{streams}],"format":{{"format_name":"{format}","duration":"60"}}}}"#
            ),
            1_000_000,
        )
        .unwrap()
    }

    fn phone() -> Probe {
        probe(
            r#"{"index":0,"codec_type":"video","codec_name":"h264","width":3840,"height":2160},
               {"index":1,"codec_type":"audio","codec_name":"aac","channels":2},
               {"index":2,"codec_type":"audio","codec_name":"aac","channels":2},
               {"index":3,"codec_type":"subtitle","codec_name":"mov_text"}"#,
            "mov,mp4,m4a,3gp,3g2,mj2",
        )
    }

    fn mkv_with(video: &str, audio: &str, subs: &str) -> Probe {
        probe(
            &format!(
                r#"{{"index":0,"codec_type":"video","codec_name":"{video}"}},
                   {{"index":1,"codec_type":"audio","codec_name":"{audio}"}},
                   {{"index":2,"codec_type":"subtitle","codec_name":"{subs}"}}"#
            ),
            "matroska,webm",
        )
    }

    fn has(args: &[String], pair: &[&str]) -> bool {
        args.windows(pair.len()).any(|w| w == pair)
    }

    #[test]
    fn every_preset_has_one_row_and_a_versioned_tag() {
        for id in [
            PresetId::Remux,
            PresetId::Trim,
            PresetId::ShareH264,
            PresetId::SmallAv1,
            PresetId::AudioOpus,
            PresetId::AudioFlac,
        ] {
            assert_eq!(id.preset().id, id);
        }
        assert_eq!(PRESETS.len(), 6);
        assert_eq!(PresetId::ShareH264.tag(), "share-h264@1");
        assert_eq!(PresetId::AudioFlac.name(), "audio-flac");
    }

    #[test]
    fn share_is_h264_and_aac_in_a_faststart_mp4_tagged_with_its_preset() {
        let b = build(PresetId::ShareH264, &phone(), &Options::default(), 4).unwrap();
        assert_eq!(b.extension, "mp4");
        assert!(has(&b.output, &["-c:v", "libx264"]), "{:?}", b.output);
        assert!(has(&b.output, &["-crf", "23"]));
        assert!(has(&b.output, &["-c:a", "aac"]));
        assert!(has(&b.output, &["-movflags", "+faststart", "-f", "mp4"]));
        assert!(has(
            &b.output,
            &["-metadata", "comment=deckjack-convert share-h264@1"]
        ));
        assert!(has(&b.output, &["-threads", "4"]));
        let vf = &b.output[b.output.iter().position(|a| a == "-vf").unwrap() + 1];
        assert!(
            vf.contains("min(1920,iw)") && vf.contains("force_divisible_by=2"),
            "{vf}"
        );
        assert!(b.warnings.is_empty());
        assert_eq!(b.seconds, Some(60.0));
    }

    #[test]
    fn a_video_preset_on_an_audio_file_is_refused_with_what_to_use_instead() {
        let mp3 = probe(
            r#"{"index":0,"codec_type":"audio","codec_name":"mp3"}"#,
            "mp3",
        );
        for id in [PresetId::ShareH264, PresetId::SmallAv1] {
            let err = build(id, &mp3, &Options::default(), 1).unwrap_err();
            assert!(err.contains("needs a video stream"), "{err}");
        }
    }

    #[test]
    fn flac_from_a_lossy_source_is_refused_unless_allowed() {
        let mp3 = probe(
            r#"{"index":0,"codec_type":"audio","codec_name":"mp3"}"#,
            "mp3",
        );
        let err = build(PresetId::AudioFlac, &mp3, &Options::default(), 1).unwrap_err();
        assert!(err.contains("bigger and no better"), "{err}");
        let allowed = Options {
            allow_lossy_to_lossless: true,
            ..Options::default()
        };
        let b = build(PresetId::AudioFlac, &mp3, &allowed, 1).unwrap();
        assert!(has(&b.output, &["-c:a", "flac"]));
        assert_eq!(b.warnings.len(), 1);
        // The control: from a lossless source it is neither refused nor warned.
        let wav = probe(
            r#"{"index":0,"codec_type":"audio","codec_name":"pcm_s16le"}"#,
            "wav",
        );
        let b = build(PresetId::AudioFlac, &wav, &Options::default(), 1).unwrap();
        assert!(b.warnings.is_empty());
        assert_eq!(b.extension, "flac");
    }

    #[test]
    fn opus_from_a_lossy_source_says_it_is_a_second_generation() {
        let b = build(PresetId::AudioOpus, &phone(), &Options::default(), 1).unwrap();
        assert!(
            b.warnings[0].contains("second generation"),
            "{:?}",
            b.warnings
        );
        assert!(has(&b.output, &["-b:a", OPUS_STEREO]));
        assert!(has(&b.output, &["-f", "opus"]));
        let surround = probe(
            r#"{"index":0,"codec_type":"audio","codec_name":"flac","channels":6}"#,
            "flac",
        );
        let b = build(PresetId::AudioOpus, &surround, &Options::default(), 1).unwrap();
        assert!(b.warnings.is_empty());
        assert!(has(&b.output, &["-b:a", OPUS_SURROUND]));
    }

    #[test]
    fn a_remux_copies_into_the_other_container_and_says_what_it_dropped() {
        // MP4 to MKV by default; mov_text cannot be copied into MKV.
        let b = build(PresetId::Remux, &phone(), &Options::default(), 1).unwrap();
        assert_eq!(b.extension, "mkv");
        assert!(has(&b.output, &["-c", "copy"]));
        assert!(has(&b.output, &["-map", "0:0"]) && has(&b.output, &["-map", "0:1"]));
        assert!(
            !has(&b.output, &["-map", "0:2"]),
            "only the first audio by default"
        );
        assert!(!has(&b.output, &["-map", "0:3"]));
        assert_eq!(b.warnings, ["Subtitles dropped: MKV cannot hold mov_text."]);
        // Every audio track on request.
        let all = Options {
            keep_all_audio: true,
            ..Options::default()
        };
        let b = build(PresetId::Remux, &phone(), &all, 1).unwrap();
        assert!(has(&b.output, &["-map", "0:2"]));
        // MKV to MP4 by default, subtitles MP4 cannot hold named.
        let b = build(
            PresetId::Remux,
            &mkv_with("h264", "aac", "ass"),
            &Options::default(),
            1,
        )
        .unwrap();
        assert_eq!(b.extension, "mp4");
        assert!(has(&b.output, &["-movflags", "+faststart", "-f", "mp4"]));
        assert_eq!(b.warnings, ["Subtitles dropped: MP4 cannot hold ass."]);
    }

    #[test]
    fn a_remux_mp4_cannot_hold_is_refused_with_the_reason() {
        let err = build(
            PresetId::Remux,
            &mkv_with("h264", "vorbis", "subrip"),
            &Options::default(),
            1,
        )
        .unwrap_err();
        assert!(err.contains("MP4 cannot hold the audio (vorbis)"), "{err}");
        let err = build(
            PresetId::Remux,
            &mkv_with("theora", "aac", "subrip"),
            &Options::default(),
            1,
        )
        .unwrap_err();
        assert!(err.contains("MP4 cannot hold the video (theora)"), "{err}");
        // The control: to MKV, the same file is fine and keeps its subtitles.
        let mkv = Options {
            container: Some(Container::Mkv),
            ..Options::default()
        };
        let b = build(
            PresetId::Remux,
            &mkv_with("theora", "vorbis", "subrip"),
            &mkv,
            1,
        )
        .unwrap();
        assert!(has(&b.output, &["-map", "0:2"]));
        assert!(b.warnings.is_empty());
        // HEVC in MP4 gets the tag Apple's players need.
        let b = build(
            PresetId::Remux,
            &mkv_with("hevc", "aac", "subrip"),
            &Options::default(),
            1,
        )
        .unwrap();
        assert!(has(&b.output, &["-tag:v", "hvc1"]));
    }

    #[test]
    fn a_trim_seeks_before_the_input_and_copies_the_stretch() {
        let opts = Options {
            range: Some(Range {
                start: 12.5,
                end: 20.0,
            }),
            ..Options::default()
        };
        let b = build(PresetId::Trim, &phone(), &opts, 1).unwrap();
        assert_eq!(b.before_input, ["-ss", "12.500"]);
        assert!(has(&b.output, &["-t", "7.500"]));
        assert!(has(&b.output, &["-c", "copy"]));
        assert!(has(&b.output, &["-avoid_negative_ts", "make_zero"]));
        assert_eq!(b.extension, "mp4", "a trim keeps an MP4 an MP4");
        assert_eq!(b.seconds, Some(7.5));
        assert!(
            build(PresetId::Trim, &phone(), &Options::default(), 1)
                .unwrap_err()
                .contains("needs a start and an end")
        );
    }

    #[test]
    fn a_range_must_be_inside_the_file_and_have_something_in_it() {
        let at = |start, end| Options {
            range: Some(Range { start, end }),
            ..Options::default()
        };
        for (s, e) in [
            (10.0, 5.0),
            (5.0, 5.0),
            (-1.0, 3.0),
            (60.0, 70.0),
            (f64::NAN, 2.0),
        ] {
            assert!(
                build(PresetId::Trim, &phone(), &at(s, e), 1).is_err(),
                "{s}..{e} was accepted"
            );
        }
        // An end past the file is the end of the file.
        let b = build(PresetId::ShareH264, &phone(), &at(50.0, 90.0), 1).unwrap();
        assert_eq!(b.seconds, Some(10.0));
    }

    #[test]
    fn every_path_is_a_file_url_after_the_input_guard() {
        let b = build(PresetId::ShareH264, &phone(), &Options::default(), 1).unwrap();
        let args = argv(
            Path::new("/w/Videos/a.mov"),
            &b,
            Path::new("/w/Videos/.p"),
            true,
        );
        let i = args.iter().position(|a| a == "-i").unwrap();
        assert_eq!(args[i + 1], "file:/w/Videos/a.mov");
        assert_eq!(args.last().unwrap(), "file:/w/Videos/.p");
        let guard = args
            .iter()
            .position(|a| a == "-protocol_whitelist")
            .unwrap();
        assert!(guard < i, "the guard is an input option");
        assert!(has(&args, &["-progress", "pipe:1"]));
        assert!(args.contains(&"-n".to_string()), "never overwrite");
        let line = command_line(Path::new("/usr/bin/ffmpeg"), &args);
        assert!(line.starts_with("/usr/bin/ffmpeg -hide_banner"), "{line}");
        assert!(
            line.contains("'comment=deckjack-convert share-h264@1'"),
            "{line}"
        );
    }

    #[test]
    fn the_table_shows_each_presets_template() {
        let table = table(2);
        let rows = table.as_array().unwrap();
        assert_eq!(rows.len(), 6);
        let share = rows.iter().find(|r| r["id"] == "share-h264").unwrap();
        let template: Vec<&str> = share["template"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert!(template.contains(&"{input}") && template.contains(&"libx264"));
        assert_eq!(share["tag"], "share-h264@1");
    }

    #[test]
    fn clock_reads_like_a_player() {
        assert_eq!(clock(62.5), "1:02.5");
        assert_eq!(clock(3723.0), "1:02:03.0");
        assert_eq!(clock(0.04), "0:00.0");
    }
}
