//! What a media file holds, from `ffprobe -of json`, and the checks a file
//! passes before any ffmpeg child is given it.

use std::io::Read;
use std::path::Path;
use std::time::Duration;

use serde::Serialize;
use serde_json::Value;

use crate::tools::{self, Tools};

/// How long a probe may take before it is stopped.
const PROBE_LIMIT: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Probe {
    /// The demuxer's name as ffprobe gives it (`mov,mp4,m4a,3gp,3g2,mj2`).
    pub format: String,
    /// Seconds, when the container says.
    pub duration: Option<f64>,
    pub size: u64,
    /// Bits per second over the whole file, when known.
    pub bit_rate: Option<u64>,
    pub streams: Vec<Stream>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Stream {
    pub index: u32,
    /// `video`, `audio`, `subtitle`, `data` or `attachment`.
    pub kind: String,
    pub codec: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub width: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub height: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fps: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub channels: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub channel_layout: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sample_rate: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bit_rate: Option<u64>,
    /// Cover art: a "video" stream that is one picture, not footage.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub cover: bool,
}

/// Audio codecs that lose nothing. Everything else is treated as lossy,
/// which errs toward warning about a second generation of loss.
const LOSSLESS_AUDIO: &[&str] = &[
    "flac", "alac", "wavpack", "tta", "ape", "truehd", "mlp", "tak", "shorten",
];

impl Probe {
    /// The first video stream that is footage, not cover art.
    pub fn video(&self) -> Option<&Stream> {
        self.streams.iter().find(|s| s.kind == "video" && !s.cover)
    }

    pub fn audio(&self) -> Option<&Stream> {
        self.streams.iter().find(|s| s.kind == "audio")
    }

    pub fn of_kind<'a>(&'a self, kind: &'a str) -> impl Iterator<Item = &'a Stream> + 'a {
        self.streams.iter().filter(move |s| s.kind == kind)
    }

    /// The short demuxer name `-f` would take: the first of the list.
    pub fn demuxer(&self) -> &str {
        self.format.split(',').next().unwrap_or("")
    }
}

impl Stream {
    pub fn is_lossless_audio(&self) -> bool {
        self.codec.starts_with("pcm_") || LOSSLESS_AUDIO.contains(&self.codec.as_str())
    }
}

/// Why a file was refused before any ffmpeg child saw it, or why the probe
/// failed.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum ProbeError {
    #[error(
        "{name} is a {what}, not a media file: it names other files for ffmpeg to read, so \
         Convert refuses it"
    )]
    Playlist { name: String, what: &'static str },
    #[error("{name} could not be read: {reason}")]
    Unreadable { name: String, reason: String },
    #[error("{name} is not a media file ffmpeg reads here: {reason}")]
    NotMedia { name: String, reason: String },
}

/// Look at the first bytes of a file and refuse the formats that are lists
/// of other files, whatever the file is called (lane brief, R4). This runs
/// before any child: the `-format_whitelist` in every ffmpeg and ffprobe
/// command is the second, independent wall.
pub fn sniff(path: &Path) -> Result<(), ProbeError> {
    let name = display_name(path);
    let mut head = [0u8; 512];
    let n = std::fs::File::open(path)
        .and_then(|mut f| f.read(&mut head))
        .map_err(|e| ProbeError::Unreadable {
            name: name.clone(),
            reason: e.to_string(),
        })?;
    // A BOM and leading whitespace are skipped, as the text demuxers do.
    let text = String::from_utf8_lossy(&head[..n]);
    let start = text.trim_start_matches('\u{feff}').trim_start();
    let lower = start.to_ascii_lowercase();
    let what = if start.starts_with("#EXTM3U") || lower.starts_with("#ext") {
        Some("playlist (M3U/HLS)")
    } else if lower.starts_with("ffconcat") {
        Some("concat list")
    } else if lower.starts_with("[playlist]") {
        Some("playlist (PLS)")
    } else if lower.starts_with("<?xml") || lower.starts_with("<mpd") || lower.starts_with("<smil")
    {
        Some("manifest (DASH/SMIL/XML)")
    } else if lower.starts_with("file '") || lower.starts_with("file ") {
        Some("concat list")
    } else {
        None
    };
    match what {
        Some(what) => Err(ProbeError::Playlist { name, what }),
        None => Ok(()),
    }
}

/// Sniff, then probe with ffprobe.
pub fn probe(tools: &Tools, path: &Path) -> Result<Probe, ProbeError> {
    sniff(path)?;
    probe_unsniffed(tools, path)
}

/// ffprobe alone, without the sniff: only for testing the second wall.
pub fn probe_unsniffed(tools: &Tools, path: &Path) -> Result<Probe, ProbeError> {
    let name = display_name(path);
    let out = tools::capture(
        tools.ffprobe(path, &["-of", "json", "-show_format", "-show_streams"]),
        PROBE_LIMIT,
    )
    .map_err(|reason| ProbeError::Unreadable {
        name: name.clone(),
        reason,
    })?;
    if !out.success {
        return Err(ProbeError::NotMedia {
            name,
            reason: tools::last_words(&out.stderr).unwrap_or_else(|| "ffprobe failed".into()),
        });
    }
    let size = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    parse(&out.stdout, size).map_err(|reason| ProbeError::NotMedia { name, reason })
}

fn display_name(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

/// ffprobe's JSON, as a [`Probe`]. Numbers arrive as strings, and any of
/// them may be missing or `N/A`.
pub fn parse(json: &str, size: u64) -> Result<Probe, String> {
    let v: Value = serde_json::from_str(json).map_err(|e| format!("ffprobe said {e}"))?;
    let format = &v["format"];
    let num = |v: &Value| -> Option<f64> {
        match v {
            Value::String(s) => s.parse().ok(),
            Value::Number(n) => n.as_f64(),
            _ => None,
        }
        .filter(|n: &f64| n.is_finite() && *n >= 0.0)
    };
    let streams = v["streams"]
        .as_array()
        .map(|list| {
            list.iter()
                .map(|s| Stream {
                    index: s["index"].as_u64().unwrap_or(0) as u32,
                    kind: s["codec_type"].as_str().unwrap_or("data").to_string(),
                    codec: s["codec_name"].as_str().unwrap_or("unknown").to_string(),
                    width: s["width"].as_u64().map(|n| n as u32),
                    height: s["height"].as_u64().map(|n| n as u32),
                    fps: s["avg_frame_rate"]
                        .as_str()
                        .and_then(rate)
                        .or_else(|| s["r_frame_rate"].as_str().and_then(rate)),
                    channels: s["channels"].as_u64().map(|n| n as u32),
                    channel_layout: s["channel_layout"].as_str().map(str::to_string),
                    sample_rate: num(&s["sample_rate"]).map(|n| n as u32),
                    bit_rate: num(&s["bit_rate"]).map(|n| n as u64),
                    cover: s["disposition"]["attached_pic"].as_u64() == Some(1),
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let probe = Probe {
        format: format["format_name"].as_str().unwrap_or("").to_string(),
        duration: num(&format["duration"]),
        size,
        bit_rate: num(&format["bit_rate"]).map(|n| n as u64),
        streams,
    };
    if probe.format.is_empty() {
        return Err("ffprobe found no container".into());
    }
    if probe.video().is_none() && probe.audio().is_none() {
        return Err("it has no video or audio stream".into());
    }
    Ok(probe)
}

/// `30000/1001` as frames per second.
fn rate(s: &str) -> Option<f64> {
    let (n, d) = s.split_once('/')?;
    let (n, d): (f64, f64) = (n.parse().ok()?, d.parse().ok()?);
    (d > 0.0 && n > 0.0).then(|| n / d)
}

/// Where a stream copy starting at `at` really starts: the last video
/// keyframe at or before it. A copy cannot start between keyframes, so ffmpeg
/// starts it here, and the operator is told so. A file with no video starts
/// where asked.
pub fn keyframe_before(tools: &Tools, path: &Path, at: f64) -> Result<f64, ProbeError> {
    // The last 30 s first, which holds a keyframe in any ordinary video; the
    // whole stretch from the start only if it did not.
    let mut from = (at - 30.0).max(0.0);
    loop {
        let csv = packets(tools, path, from, at)?;
        if csv.trim().is_empty() {
            // No video packets at all: an audio file starts where asked.
            return Ok(at);
        }
        if let Some(k) = last_keyframe(&csv, at) {
            return Ok(k);
        }
        if from == 0.0 {
            return Ok(0.0);
        }
        from = 0.0;
    }
}

/// `pts_time,flags` of the first video stream's packets from `from` to `to`.
fn packets(tools: &Tools, path: &Path, from: f64, to: f64) -> Result<String, ProbeError> {
    let name = display_name(path);
    let interval = format!("{from}%{}", to + 0.001);
    let out = tools::capture(
        tools.ffprobe(
            path,
            &[
                "-select_streams",
                "v:0",
                "-read_intervals",
                &interval,
                "-show_entries",
                "packet=pts_time,flags",
                "-of",
                "csv=p=0",
            ],
        ),
        PROBE_LIMIT,
    )
    .map_err(|reason| ProbeError::Unreadable {
        name: name.clone(),
        reason,
    })?;
    if !out.success {
        return Err(ProbeError::NotMedia {
            name,
            reason: tools::last_words(&out.stderr).unwrap_or_default(),
        });
    }
    Ok(out.stdout)
}

/// From `pts_time,flags` lines, the latest keyframe at or before `at`.
pub fn last_keyframe(csv: &str, at: f64) -> Option<f64> {
    csv.lines()
        .filter_map(|l| {
            let mut parts = l.split(',');
            let pts: f64 = parts.next()?.trim().parse().ok()?;
            let flags = parts.next().unwrap_or("");
            flags.contains('K').then_some(pts)
        })
        .filter(|pts| *pts <= at + 1e-6)
        .fold(None, |best: Option<f64>, pts| {
            Some(best.map_or(pts, |b| b.max(pts)))
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    const PHONE: &str = r#"{
      "streams": [
        {"index": 0, "codec_name": "h264", "codec_type": "video", "width": 1920,
         "height": 1080, "avg_frame_rate": "30000/1001", "bit_rate": "15000000",
         "disposition": {"attached_pic": 0}},
        {"index": 1, "codec_name": "aac", "codec_type": "audio", "channels": 2,
         "channel_layout": "stereo", "sample_rate": "48000", "bit_rate": "192000"},
        {"index": 2, "codec_name": "mjpeg", "codec_type": "video", "width": 300,
         "height": 300, "disposition": {"attached_pic": 1}}
      ],
      "format": {"format_name": "mov,mp4,m4a,3gp,3g2,mj2", "duration": "12.500000",
                 "bit_rate": "15200000"}
    }"#;

    #[test]
    fn ffprobe_json_becomes_streams_with_numbers() {
        let p = parse(PHONE, 1000).unwrap();
        assert_eq!(p.demuxer(), "mov");
        assert_eq!(p.duration, Some(12.5));
        assert_eq!(p.bit_rate, Some(15_200_000));
        let v = p.video().unwrap();
        assert_eq!(
            (v.codec.as_str(), v.width, v.height),
            ("h264", Some(1920), Some(1080))
        );
        assert!((v.fps.unwrap() - 29.97).abs() < 0.01);
        let a = p.audio().unwrap();
        assert_eq!((a.channels, a.sample_rate), (Some(2), Some(48_000)));
        assert!(!a.is_lossless_audio());
        // Cover art is a video stream that is not the video.
        assert!(p.streams[2].cover);
        assert_eq!(p.of_kind("video").count(), 2);
    }

    #[test]
    fn an_audio_file_with_cover_art_has_no_video() {
        let json = r#"{"streams": [
            {"index": 0, "codec_name": "flac", "codec_type": "audio"},
            {"index": 1, "codec_name": "png", "codec_type": "video",
             "disposition": {"attached_pic": 1}}],
          "format": {"format_name": "flac", "duration": "N/A"}}"#;
        let p = parse(json, 1).unwrap();
        assert!(p.video().is_none());
        assert!(p.audio().unwrap().is_lossless_audio());
        assert_eq!(p.duration, None, "N/A is unknown, not zero");
    }

    #[test]
    fn a_file_with_nothing_to_convert_is_refused() {
        let json = r#"{"streams": [{"index": 0, "codec_type": "subtitle", "codec_name": "ass"}],
                       "format": {"format_name": "matroska,webm"}}"#;
        assert!(parse(json, 1).unwrap_err().contains("no video or audio"));
        assert!(parse("{}", 1).is_err());
        assert!(parse("not json", 1).is_err());
    }

    #[test]
    fn lossless_audio_is_recognized_by_codec() {
        let s = |codec: &str| Stream {
            index: 0,
            kind: "audio".into(),
            codec: codec.into(),
            width: None,
            height: None,
            fps: None,
            channels: None,
            channel_layout: None,
            sample_rate: None,
            bit_rate: None,
            cover: false,
        };
        for lossless in ["flac", "alac", "pcm_s16le", "pcm_s24be", "wavpack"] {
            assert!(s(lossless).is_lossless_audio(), "{lossless}");
        }
        for lossy in ["mp3", "aac", "opus", "vorbis", "ac3"] {
            assert!(!s(lossy).is_lossless_audio(), "{lossy}");
        }
    }

    #[test]
    fn playlists_and_concat_lists_are_refused_by_their_content_not_their_name() {
        let dir = tempfile::tempdir().unwrap();
        let cases: &[(&str, &[u8])] = &[
            ("trip.mp4", b"#EXTM3U\n#EXTINF:6,\n/etc/passwd\n"),
            ("trip2.mkv", b"\xef\xbb\xbf  #EXTM3U\nclip.mp4\n"),
            ("cut.mkv", b"ffconcat version 1.0\nfile '/etc/passwd'\n"),
            ("cut2.mp4", b"file 'a.mp4'\nfile 'b.mp4'\n"),
            ("list.mp3", b"[playlist]\nFile1=/etc/passwd\n"),
            ("dash.mp4", b"<?xml version=\"1.0\"?><MPD>"),
        ];
        for (name, bytes) in cases {
            let path = dir.path().join(name);
            std::fs::write(&path, bytes).unwrap();
            assert!(
                matches!(sniff(&path), Err(ProbeError::Playlist { .. })),
                "{name} was not refused"
            );
        }
        // The control: real media bytes pass.
        let mp4 = dir.path().join("real.mp4");
        std::fs::write(&mp4, b"\0\0\0\x18ftypmp42\0\0\0\0mp42isom").unwrap();
        assert_eq!(sniff(&mp4), Ok(()));
        let mp3 = dir.path().join("real.mp3");
        std::fs::write(&mp3, b"ID3\x04\0\0\0\0\0\x00").unwrap();
        assert_eq!(sniff(&mp3), Ok(()));
    }

    #[test]
    fn the_cut_starts_at_the_last_keyframe_at_or_before_the_chosen_start() {
        let csv = "0.000000,K__\n0.033,___\n2.000000,K__\n2.033,___\n4.000000,K__\n";
        assert_eq!(last_keyframe(csv, 3.5), Some(2.0));
        assert_eq!(last_keyframe(csv, 4.0), Some(4.0));
        assert_eq!(last_keyframe(csv, 1.9), Some(0.0));
        assert_eq!(last_keyframe("", 3.0), None);
        assert_eq!(last_keyframe("2.5,___\n", 3.0), None);
    }
}
