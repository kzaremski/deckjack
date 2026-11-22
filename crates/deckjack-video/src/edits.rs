//! The edits, each one FilmCraft command (so one undo step), each naming its
//! targets.
//!
//! Frames in, frames out: every position here is a whole frame at the
//! sequence's rate, converted to FilmCraft's ticks at the last moment.

use serde::Deserialize;
use serde_json::{Value, json};

use crate::engine::{Project, Result, VideoError};
use crate::timeline::{Clip, Rate};

fn all_clips(project: &Project) -> Result<Vec<Clip>> {
    let seq = project.execute("sequence.inspect", json!({}))?;
    let rate = crate::timeline::sequence_rate(&seq).unwrap_or(Rate { num: 30, den: 1 });
    let (tracks, _) = crate::timeline::tracks_and_markers(&seq, rate);
    Ok(tracks.into_iter().flat_map(|t| t.clips).collect())
}

/// Refuse a clip id the sequence does not have, with its number, rather than
/// whatever FilmCraft says about a missing target.
fn known(project: &Project, clips: &[u64]) -> Result<()> {
    if clips.is_empty() {
        return Err(VideoError::Refused("name at least one clip".into()));
    }
    let all = all_clips(project)?;
    for id in clips {
        if !all.iter().any(|c| c.id == *id) {
            return Err(VideoError::Refused(format!(
                "there is no clip {id} in this sequence"
            )));
        }
    }
    Ok(())
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct Place {
    pub project: String,
    /// The media item from the bin (`media[].item` in describe).
    pub item: u64,
    /// The frame on the timeline the clip starts at.
    pub frame: i64,
    /// Video track, `V1` by default. The audio goes to `audio_track`.
    #[serde(default)]
    pub track: Option<String>,
    /// Audio track, `A1` by default.
    #[serde(default)]
    pub audio_track: Option<String>,
    /// Source frame to start from; the start of the media by default.
    #[serde(default)]
    pub source_in: Option<i64>,
    /// Frames to take; to the end of the media by default.
    #[serde(default)]
    pub duration: Option<i64>,
    /// Place the picture. Default true.
    #[serde(default)]
    pub video: Option<bool>,
    /// Place the sound. Default true.
    #[serde(default)]
    pub audio: Option<bool>,
}

/// Insert (pushing later clips right) or overwrite (replacing what is under
/// it). The first clip into an empty sequence sets its frame size and rate.
pub fn place(project: &Project, p: &Place, insert: bool) -> Result<Value> {
    let rate = project.rate()?;
    if p.frame < 0 {
        return Err(VideoError::Refused(
            "a clip cannot start before frame 0".into(),
        ));
    }
    let mut params = json!({
        "item": p.item,
        "track": p.track.as_deref().unwrap_or("V1"),
        "audioTrack": p.audio_track.as_deref().unwrap_or("A1"),
        // Frames, not ticks: FilmCraft reads them at the sequence's rate
        // after `matchSequence` has set it, so the first clip into an empty
        // sequence lands on the frame asked for even when its rate differs
        // from the placeholder's. Ticks worked out here would be at the old
        // rate.
        "frame": p.frame,
        "insert": insert,
        "video": p.video.unwrap_or(true),
        "audio": p.audio.unwrap_or(true),
        "matchSequence": true,
    });
    if let Some(s) = p.source_in {
        params["sourceIn"] = json!(rate.ticks(s.max(0)));
    }
    if let Some(d) = p.duration {
        if d <= 0 {
            return Err(VideoError::Refused(
                "a clip needs at least one frame".into(),
            ));
        }
        params["duration"] = json!({ "frame": d });
    }
    project.execute("timeline.place", params)
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct Move {
    pub project: String,
    pub clip: u64,
    /// The frame the clip's start moves to.
    pub frame: i64,
    /// Another track of the same kind (`V2`, `A2`); its own by default.
    #[serde(default)]
    pub track: Option<String>,
    /// Insert at the destination (pushing clips right) instead of overwriting.
    #[serde(default)]
    pub insert: bool,
    /// Move linked partners too. Default true.
    #[serde(default)]
    pub linked: Option<bool>,
}

pub fn move_clip(project: &Project, p: &Move) -> Result<Value> {
    known(project, &[p.clip])?;
    if p.frame < 0 {
        return Err(VideoError::Refused(
            "a clip cannot start before frame 0".into(),
        ));
    }
    let rate = project.rate()?;
    let mut one = json!({ "clip": p.clip, "time": rate.ticks(p.frame) });
    if let Some(t) = &p.track {
        one["track"] = json!(t);
    } else {
        one["track"] = json!(track_of(project, p.clip)?);
    }
    project.execute(
        "timeline.move",
        json!({ "moves": [one], "insert": p.insert, "linked": p.linked.unwrap_or(true) }),
    )
}

fn track_of(project: &Project, clip: u64) -> Result<u64> {
    let seq = project.execute("sequence.inspect", json!({}))?;
    let rate = crate::timeline::sequence_rate(&seq).unwrap_or(Rate { num: 30, den: 1 });
    let (tracks, _) = crate::timeline::tracks_and_markers(&seq, rate);
    tracks
        .iter()
        .find(|t| t.clips.iter().any(|c| c.id == clip))
        .map(|t| t.id)
        .ok_or_else(|| VideoError::Refused(format!("there is no clip {clip} in this sequence")))
}

#[derive(Debug, Clone, Copy, Deserialize, schemars::JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Edge {
    /// The clip's start.
    In,
    /// The clip's end.
    Out,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct Trim {
    pub project: String,
    pub clip: u64,
    pub edge: Edge,
    /// Frames to move the edge; negative is earlier.
    pub delta: i64,
    /// Ripple: later clips follow the edit, so no gap opens or closes.
    #[serde(default)]
    pub ripple: bool,
    /// Trim linked partners too. Default true; false is how a J- or L-cut is
    /// made without unlinking.
    #[serde(default)]
    pub linked: Option<bool>,
}

pub fn trim(project: &Project, p: &Trim) -> Result<Value> {
    known(project, &[p.clip])?;
    if p.delta == 0 {
        return Err(VideoError::Refused(
            "a trim of zero frames changes nothing".into(),
        ));
    }
    project.execute(
        "timeline.trim",
        json!({
            "clip": p.clip,
            "edge": match p.edge { Edge::In => "in", Edge::Out => "out" },
            "mode": if p.ripple { "ripple" } else { "regular" },
            "deltaFrames": p.delta,
            "linked": p.linked.unwrap_or(true),
        }),
    )
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct Split {
    pub project: String,
    /// The frame to cut at: the first frame of the right-hand piece.
    pub frame: i64,
    /// Cut only this clip (and its linked partners); every track when absent.
    #[serde(default)]
    pub clip: Option<u64>,
}

pub fn split(project: &Project, p: &Split) -> Result<Value> {
    let rate = project.rate()?;
    let time = rate.ticks(p.frame);
    let before = all_clips(project)?.len();
    match p.clip {
        Some(c) => {
            known(project, &[c])?;
            project.execute("timeline.razor", json!({ "time": time, "clip": c }))?;
        }
        None => {
            project.execute("sequence.addEditAllTracks", json!({ "time": time }))?;
        }
    }
    let cuts = all_clips(project)?.len().saturating_sub(before);
    if cuts == 0 {
        return Err(VideoError::Refused(format!(
            "nothing to split at frame {}: no clip spans it",
            p.frame
        )));
    }
    Ok(json!({ "cuts": cuts }))
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct Clips {
    pub project: String,
    pub clips: Vec<u64>,
}

/// Lift (leave a gap) or ripple delete (close it). A linked clip's partners
/// go with it, as FilmCraft's linked selection does; unlink first to remove
/// only the picture or only the sound.
pub fn delete(project: &Project, p: &Clips, ripple: bool) -> Result<Value> {
    known(project, &p.clips)?;
    project.execute(
        "timeline.delete",
        json!({ "clips": p.clips, "ripple": ripple }),
    )?;
    Ok(json!({ "deleted": p.clips }))
}

/// Link clips into one group. FilmCraft's Link toggles, so the outcome is
/// checked and the toggle repeated when it unlinked instead.
pub fn link(project: &Project, p: &Clips) -> Result<Value> {
    known(project, &p.clips)?;
    if p.clips.len() < 2 {
        return Err(VideoError::Refused(
            "linking needs at least two clips".into(),
        ));
    }
    let linked_together = |project: &Project| -> Result<bool> {
        let all = all_clips(project)?;
        let links: Vec<Option<u64>> = all
            .iter()
            .filter(|c| p.clips.contains(&c.id))
            .map(|c| c.link)
            .collect();
        Ok(links.first().is_some_and(|l| l.is_some()) && links.windows(2).all(|w| w[0] == w[1]))
    };
    if linked_together(project)? {
        return Err(VideoError::Refused("those clips are already linked".into()));
    }
    project.execute("clip.link", json!({ "clips": p.clips }))?;
    if !linked_together(project)? {
        project.execute("clip.link", json!({ "clips": p.clips }))?;
    }
    if !linked_together(project)? {
        return Err(VideoError::Engine(
            "FilmCraft did not link those clips".into(),
        ));
    }
    Ok(json!({ "linked": p.clips }))
}

/// Unlink clips from their partners: the J- and L-cut step.
pub fn unlink(project: &Project, p: &Clips) -> Result<Value> {
    known(project, &p.clips)?;
    let all = all_clips(project)?;
    let linked: Vec<u64> = all
        .iter()
        .filter(|c| p.clips.contains(&c.id) && c.link.is_some())
        .map(|c| c.id)
        .collect();
    if linked.is_empty() {
        return Err(VideoError::Refused("none of those clips is linked".into()));
    }
    project.execute("clip.link", json!({ "clips": linked }))?;
    let still: Vec<u64> = all_clips(project)?
        .iter()
        .filter(|c| linked.contains(&c.id) && c.link.is_some())
        .map(|c| c.id)
        .collect();
    if !still.is_empty() {
        return Err(VideoError::Engine(format!(
            "FilmCraft left clips {still:?} linked"
        )));
    }
    Ok(json!({ "unlinked": linked }))
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct Gain {
    pub project: String,
    pub clips: Vec<u64>,
    /// The clip gain to set, in dB.
    pub db: f64,
}

pub fn gain(project: &Project, p: &Gain) -> Result<Value> {
    known(project, &p.clips)?;
    if !(-60.0..=24.0).contains(&p.db) {
        return Err(VideoError::Refused(format!(
            "a gain of {} dB is outside -60 to +24",
            p.db
        )));
    }
    project.execute(
        "clip.audioGain",
        json!({ "clips": p.clips, "mode": "set", "db": p.db }),
    )
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct MarkerAdd {
    pub project: String,
    pub frame: i64,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub comment: Option<String>,
}

pub fn marker_add(project: &Project, p: &MarkerAdd) -> Result<Value> {
    let rate = project.rate()?;
    let mut params = json!({ "target": "program", "time": rate.ticks(p.frame.max(0)) });
    if let Some(n) = &p.name {
        params["name"] = json!(n);
    }
    if let Some(c) = &p.comment {
        params["comment"] = json!(c);
    }
    project.execute("markers.add", params)
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct MarkerMove {
    pub project: String,
    pub marker: u64,
    pub frame: i64,
    #[serde(default)]
    pub name: Option<String>,
}

pub fn marker_move(project: &Project, p: &MarkerMove) -> Result<Value> {
    let mut params = json!({ "target": "program", "marker": p.marker, "frame": p.frame.max(0) });
    if let Some(n) = &p.name {
        params["name"] = json!(n);
    }
    project.execute("markers.edit", params)
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct MarkerRemove {
    pub project: String,
    pub marker: u64,
}

pub fn marker_remove(project: &Project, p: &MarkerRemove) -> Result<Value> {
    project.execute(
        "markers.delete",
        json!({ "target": "program", "marker": p.marker }),
    )
}

/// Undo or redo one step, refusing politely when there is none.
pub fn history(project: &Project, redo: bool) -> Result<Value> {
    let possible = project.with(|s| {
        if redo {
            s.history.can_redo()
        } else {
            s.history.can_undo()
        }
    });
    if !possible {
        // FilmCraft refuses too, but with its registry's wording; this one
        // says what a person would.
        return Err(VideoError::Refused(
            if redo {
                "nothing to redo"
            } else {
                "nothing to undo"
            }
            .into(),
        ));
    }
    project.execute(if redo { "edit.redo" } else { "edit.undo" }, json!({}))
}
