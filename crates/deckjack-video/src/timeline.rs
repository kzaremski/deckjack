//! The timeline as this app describes it: frames, named tracks, and nothing
//! of FilmCraft's internals.
//!
//! FilmCraft counts time in ticks (254,016,000,000 per second, so 23.976 and
//! 29.97 stay exact). Every command and every reply here speaks whole frames
//! at the sequence's rate instead, because that is what a person nudges and
//! what an agent reasons about; the conversion is in one place, below.

use serde::Serialize;
use serde_json::Value;

/// FilmCraft's ticks per second.
pub const TICKS_PER_SECOND: i128 = 254_016_000_000;

/// A frame rate as a fraction, exactly as the sequence stores it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Rate {
    pub num: i64,
    pub den: i64,
}

impl Rate {
    /// Read from a FilmCraft `frame_rate` object; `None` for a missing or
    /// zero rate, which no frame arithmetic can use.
    pub fn from_json(v: &Value) -> Option<Rate> {
        let num = v.get("num")?.as_i64()?;
        let den = v.get("den")?.as_i64()?;
        (num > 0 && den > 0).then_some(Rate { num, den })
    }

    /// The tick a frame starts at.
    pub fn ticks(self, frame: i64) -> i64 {
        let t = i128::from(frame) * TICKS_PER_SECOND * i128::from(self.den) / i128::from(self.num);
        i64::try_from(t).unwrap_or(if frame < 0 { i64::MIN } else { i64::MAX })
    }

    /// The frame a tick falls in, rounded to the nearest frame start: a tick
    /// count from a source of another rate lands between frames, and the
    /// nearest is what a person would call it.
    pub fn frame(self, ticks: i64) -> i64 {
        let per = TICKS_PER_SECOND * i128::from(self.den);
        let scaled = i128::from(ticks) * i128::from(self.num);
        let f = if scaled >= 0 {
            (scaled + per / 2) / per
        } else {
            (scaled - per / 2) / per
        };
        i64::try_from(f).unwrap_or(0)
    }
}

/// The whole state an agent or a window needs, as `video.project.describe`
/// returns it.
#[derive(Debug, Clone, Serialize)]
pub struct Describe {
    pub project: String,
    /// Changes on every edit, undo and redo; `video.project.wait` waits on it.
    pub revision: u64,
    /// Edits since the last save. Autosave and the recovery journal hold them
    /// meanwhile.
    pub dirty: bool,
    pub can_undo: bool,
    pub can_redo: bool,
    pub fps: Rate,
    pub width: u64,
    pub height: u64,
    /// The program's length in frames.
    pub duration: i64,
    pub tracks: Vec<Track>,
    pub markers: Vec<Marker>,
    pub media: Vec<Media>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Track {
    pub id: u64,
    /// `V1`, `V2`, `A1`…: what every command also accepts as a track.
    pub name: String,
    pub kind: TrackKind,
    pub clips: Vec<Clip>,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum TrackKind {
    Video,
    Audio,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Clip {
    pub id: u64,
    /// The media item in the bin this clip shows.
    pub item: u64,
    pub name: String,
    /// First frame on the timeline.
    pub start: i64,
    /// One past the last frame on the timeline.
    pub end: i64,
    /// The source frame shown at `start`.
    pub source_in: i64,
    /// The link group: clips sharing it move, trim and delete together until
    /// unlinked. `None` for a clip on its own.
    pub link: Option<u64>,
    pub gain_db: f64,
    pub enabled: bool,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Marker {
    pub id: u64,
    pub frame: i64,
    pub name: String,
    pub comment: String,
    /// Frames; zero for a point marker.
    pub duration: i64,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Media {
    pub item: u64,
    pub name: String,
    /// Relative to `Videos/` when the file is inside it, as import takes it.
    pub path: Option<String>,
    /// `Movie`, `Audio`, `Still`… as FilmCraft classified it.
    pub kind: String,
    /// Frames at the sequence's rate.
    pub duration: i64,
    pub width: Option<u64>,
    pub height: Option<u64>,
    pub fps: Option<Rate>,
    pub video_codec: Option<String>,
    pub audio_channels: Option<u64>,
    pub offline: bool,
}

/// The sequence's rate, from `sequence.inspect`.
pub fn sequence_rate(seq: &Value) -> Option<Rate> {
    Rate::from_json(seq.get("settings")?.get("frame_rate")?)
}

/// Tracks and markers, from `sequence.inspect`. Tracks are named by kind and
/// position (`V1` is the first video track), which is how every command here
/// names them too.
pub fn tracks_and_markers(seq: &Value, rate: Rate) -> (Vec<Track>, Vec<Marker>) {
    let mut tracks = Vec::new();
    for (key, kind, prefix) in [
        ("video", TrackKind::Video, 'V'),
        ("audio", TrackKind::Audio, 'A'),
    ] {
        let list = seq.get(key).and_then(Value::as_array);
        for (i, t) in list.into_iter().flatten().enumerate() {
            let clips = t
                .get("items")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|c| clip(c, rate))
                .collect();
            tracks.push(Track {
                id: t.get("id").and_then(Value::as_u64).unwrap_or(0),
                name: format!("{prefix}{}", i + 1),
                kind,
                clips,
            });
        }
    }
    let markers = seq
        .get("markers")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|m| {
            Some(Marker {
                id: m.get("id")?.as_u64()?,
                frame: rate.frame(m.get("start")?.as_i64()?),
                name: str_of(m, "name"),
                comment: str_of(m, "comment"),
                duration: rate.frame(m.get("duration").and_then(Value::as_i64).unwrap_or(0)),
            })
        })
        .collect();
    (tracks, markers)
}

fn clip(c: &Value, rate: Rate) -> Option<Clip> {
    Some(Clip {
        id: c.get("clip")?.as_u64()?,
        item: c.get("item").and_then(Value::as_u64).unwrap_or(0),
        name: str_of(c, "name"),
        start: c.get("startFrame")?.as_i64()?,
        end: c.get("endFrame")?.as_i64()?,
        source_in: rate.frame(c.get("sourceIn").and_then(Value::as_i64).unwrap_or(0)),
        link: c.get("link").and_then(Value::as_u64),
        gain_db: c.get("gainDb").and_then(Value::as_f64).unwrap_or(0.0),
        enabled: c.get("enabled").and_then(Value::as_bool).unwrap_or(true),
    })
}

fn str_of(v: &Value, key: &str) -> String {
    v.get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

/// One bin entry, from a serialized FilmCraft project item. `None` for what
/// is not media (sequences, bins). `relative` turns the stored path into one
/// relative to `Videos/`.
pub fn media(
    id: u64,
    item: &Value,
    rate: Rate,
    relative: impl Fn(&str) -> Option<String>,
) -> Option<Media> {
    let m = item.get("kind")?.get("Media")?;
    let info = m.get("info");
    let video = info.and_then(|i| i.get("video")).filter(|v| !v.is_null());
    let stored = m
        .get("media")
        .and_then(|x| x.get("File"))
        .and_then(|f| f.get("path"))
        .and_then(Value::as_str);
    Some(Media {
        item: id,
        name: str_of(item, "name"),
        path: stored.and_then(relative),
        kind: info
            .and_then(|i| i.get("kind"))
            .and_then(Value::as_str)
            .unwrap_or("Media")
            .to_string(),
        duration: rate.frame(
            info.and_then(|i| i.get("duration"))
                .and_then(Value::as_i64)
                .unwrap_or(0),
        ),
        width: video.and_then(|v| v.get("width")).and_then(Value::as_u64),
        height: video.and_then(|v| v.get("height")).and_then(Value::as_u64),
        fps: video
            .and_then(|v| v.get("frame_rate"))
            .and_then(Rate::from_json),
        video_codec: video
            .and_then(|v| v.get("codec"))
            .and_then(Value::as_str)
            .map(str::to_string),
        audio_channels: info
            .and_then(|i| i.get("audio_streams"))
            .and_then(Value::as_array)
            .and_then(|a| a.first())
            .and_then(|a| a.get("channels"))
            .and_then(Value::as_u64),
        offline: m.get("offline").and_then(Value::as_bool).unwrap_or(false),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_and_ticks_round_trip_at_ntsc_rates() {
        // 29.97 is where float arithmetic drifts: frame 107892 is one hour
        // of drop-frame material, and a rounding error there is a wrong frame
        // at the end of a long cut.
        let ntsc = Rate {
            num: 30000,
            den: 1001,
        };
        for frame in [0, 1, 29, 30, 1799, 107_892, -5] {
            assert_eq!(ntsc.frame(ntsc.ticks(frame)), frame, "frame {frame}");
        }
        let thirty = Rate { num: 30, den: 1 };
        assert_eq!(thirty.ticks(30), 254_016_000_000);
        // Half a frame rounds to the later frame, a third to the earlier.
        assert_eq!(thirty.frame(254_016_000_000 / 60), 1);
        assert_eq!(thirty.frame(254_016_000_000 / 90), 0);
        assert_eq!(thirty.frame(-(254_016_000_000 / 60)), -1);
    }

    #[test]
    fn a_zero_rate_is_refused_rather_than_divided_by() {
        assert_eq!(
            Rate::from_json(&serde_json::json!({"num": 0, "den": 1})),
            None
        );
        assert_eq!(
            Rate::from_json(&serde_json::json!({"num": 24, "den": 1})),
            Some(Rate { num: 24, den: 1 })
        );
    }
}
