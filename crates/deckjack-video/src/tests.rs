//! The editor end to end, against FilmCraft's real engine and a real clip.
//!
//! `tests/fixtures/clip.mp4` is two seconds of H.264 and AAC at 192x108 and
//! 30 fps (60 frames), small enough to commit and decode in a debug build.
//! Every test makes its own `Videos/` in a temporary directory.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::edits::{self, Clips, Edge, Move, Place, Split, Trim};
use crate::engine::{Project, Studio};
use crate::export::{Exports, Size};
use crate::timeline::{Describe, TrackKind};

const FIXTURE: &[u8] = include_bytes!("../tests/fixtures/clip.mp4");

struct Rig {
    _dir: tempfile::TempDir,
    root: PathBuf,
    data: PathBuf,
    studio: Studio,
}

fn rig() -> Rig {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("Videos");
    let data = dir.path().join("home").join("filmcraft");
    std::fs::create_dir_all(root.join("trip")).unwrap();
    std::fs::write(root.join("trip").join("clip.mp4"), FIXTURE).unwrap();
    let studio = Studio::new(root.clone(), data.clone()).unwrap();
    Rig {
        _dir: dir,
        root,
        data,
        studio,
    }
}

impl Rig {
    /// A new project with the fixture imported, and its bin item id.
    fn project(&self, name: &str) -> (Arc<Project>, u64) {
        let project = self.studio.create(name).unwrap();
        let path = crate::names::media_path(&self.root, "trip/clip.mp4").unwrap();
        let imported = project
            .execute(
                "file.import",
                serde_json::json!({ "paths": [path.to_string_lossy()] }),
            )
            .unwrap();
        let item = imported["items"][0].as_u64().expect("an imported item");
        (project, item)
    }

    fn describe(&self, project: &Project) -> Describe {
        project.describe(&self.root).unwrap()
    }
}

fn place(project: &str, item: u64, frame: i64) -> Place {
    Place {
        project: project.into(),
        item,
        frame,
        track: None,
        audio_track: None,
        source_in: None,
        duration: None,
        video: None,
        audio: None,
    }
}

/// `(start, end)` of every clip on a track, by name.
fn spans(d: &Describe, track: &str) -> Vec<(i64, i64)> {
    d.tracks
        .iter()
        .find(|t| t.name == track)
        .unwrap_or_else(|| panic!("no track {track}"))
        .clips
        .iter()
        .map(|c| (c.start, c.end))
        .collect()
}

fn clip_ids(d: &Describe, track: &str) -> Vec<u64> {
    d.tracks
        .iter()
        .find(|t| t.name == track)
        .unwrap()
        .clips
        .iter()
        .map(|c| c.id)
        .collect()
}

#[test]
fn a_new_project_writes_both_files_and_refuses_its_name_twice() {
    let r = rig();
    let project = r.studio.create("Holiday").unwrap();
    assert!(r.root.join("Holiday.fcproj").is_file());
    assert!(r.root.join("Holiday.otio").is_file());
    let d = r.describe(&project);
    // V1–V2 and A1–A2, named as every command names them.
    let names: Vec<(&str, TrackKind)> =
        d.tracks.iter().map(|t| (t.name.as_str(), t.kind)).collect();
    assert_eq!(
        names,
        vec![
            ("V1", TrackKind::Video),
            ("V2", TrackKind::Video),
            ("A1", TrackKind::Audio),
            ("A2", TrackKind::Audio)
        ]
    );
    let again = r
        .studio
        .create("Holiday")
        .err()
        .expect("a taken name was accepted");
    assert!(again.to_string().contains("already"), "{again}");
    // The sidecar alone also holds the name: creating would orphan it.
    std::fs::remove_file(r.root.join("Holiday.fcproj")).unwrap();
    assert!(r.studio.create("Holiday").is_err());
    let listed = r.studio.list().unwrap();
    assert!(
        listed.is_empty(),
        "a project without its .fcproj was listed: {listed:?}"
    );
}

#[test]
fn import_probes_and_the_first_clip_sets_the_sequence() {
    let r = rig();
    let (project, item) = r.project("Probe");
    let d = r.describe(&project);
    let media = d
        .media
        .iter()
        .find(|m| m.item == item)
        .expect("the clip in the bin");
    assert_eq!(media.path.as_deref(), Some("trip/clip.mp4"));
    assert_eq!((media.width, media.height), (Some(192), Some(108)));
    assert_eq!(media.duration, 60);
    assert_eq!(media.audio_channels, Some(1));
    edits::place(&project, &place("Probe", item, 0), true).unwrap();
    let d = r.describe(&project);
    // `matchSequence`: an empty 1080p sequence took the clip's size.
    assert_eq!((d.width, d.height), (192, 108));
    assert_eq!(d.fps, crate::timeline::Rate { num: 30, den: 1 });
    assert_eq!(spans(&d, "V1"), vec![(0, 60)]);
    assert_eq!(spans(&d, "A1"), vec![(0, 60)]);
    let v = &d.tracks[0].clips[0];
    let a = &d.tracks[2].clips[0];
    assert!(
        v.link.is_some() && v.link == a.link,
        "picture and sound are not linked"
    );
    assert_eq!(d.duration, 60);
}

#[test]
fn insert_pushes_later_material_and_overwrite_replaces_it() {
    let r = rig();
    let (project, item) = r.project("Edits");
    edits::place(&project, &place("Edits", item, 0), true).unwrap();
    let mut piece = place("Edits", item, 30);
    piece.source_in = Some(10);
    piece.duration = Some(20);
    edits::place(&project, &piece, true).unwrap();
    let d = r.describe(&project);
    assert_eq!(spans(&d, "V1"), vec![(0, 30), (30, 50), (50, 80)]);
    assert_eq!(d.tracks[0].clips[1].source_in, 10);
    assert_eq!(spans(&d, "A1"), vec![(0, 30), (30, 50), (50, 80)]);
    // Overwrite over the middle: same length, nothing pushed.
    let mut over = place("Edits", item, 35);
    over.duration = Some(10);
    edits::place(&project, &over, false).unwrap();
    let d = r.describe(&project);
    assert_eq!(d.duration, 80);
    assert_eq!(
        spans(&d, "V1"),
        vec![(0, 30), (30, 35), (35, 45), (45, 50), (50, 80)]
    );
}

#[test]
fn split_trim_ripple_and_delete_behave_as_named() {
    let r = rig();
    let (project, item) = r.project("Cut");
    edits::place(&project, &place("Cut", item, 0), true).unwrap();

    // Split every track at the playhead.
    let split = Split {
        project: "Cut".into(),
        frame: 20,
        clip: None,
    };
    edits::split(&project, &split).unwrap();
    let d = r.describe(&project);
    assert_eq!(spans(&d, "V1"), vec![(0, 20), (20, 60)]);
    assert_eq!(spans(&d, "A1"), vec![(0, 20), (20, 60)]);
    let nothing = Split {
        project: "Cut".into(),
        frame: 500,
        clip: None,
    };
    assert!(
        edits::split(&project, &nothing).is_err(),
        "a split past the end succeeded"
    );

    // A regular trim opens a gap; a ripple trim pulls later clips along.
    let first = clip_ids(&d, "V1")[0];
    let regular = Trim {
        project: "Cut".into(),
        clip: first,
        edge: Edge::Out,
        delta: -5,
        ripple: false,
        linked: None,
    };
    edits::trim(&project, &regular).unwrap();
    let d = r.describe(&project);
    assert_eq!(spans(&d, "V1"), vec![(0, 15), (20, 60)]);
    assert_eq!(
        spans(&d, "A1"),
        vec![(0, 15), (20, 60)],
        "the linked sound did not follow"
    );
    let ripple = Trim {
        project: "Cut".into(),
        clip: first,
        edge: Edge::Out,
        delta: -5,
        ripple: true,
        linked: None,
    };
    edits::trim(&project, &ripple).unwrap();
    let d = r.describe(&project);
    assert_eq!(spans(&d, "V1"), vec![(0, 10), (15, 55)]);

    // Lift leaves the gap; ripple delete closes it.
    let second = clip_ids(&d, "V1")[1];
    let first_only = Clips {
        project: "Cut".into(),
        clips: vec![first],
    };
    edits::delete(&project, &first_only, false).unwrap();
    let d = r.describe(&project);
    assert_eq!(spans(&d, "V1"), vec![(15, 55)]);
    assert_eq!(
        spans(&d, "A1"),
        vec![(15, 55)],
        "lift left the linked sound behind"
    );
    let second_only = Clips {
        project: "Cut".into(),
        clips: vec![second],
    };
    // Rippling the remaining clip closes the gap before nothing: the
    // timeline is empty, and its duration with it.
    edits::delete(&project, &second_only, true).unwrap();
    let d = r.describe(&project);
    assert!(spans(&d, "V1").is_empty() && spans(&d, "A1").is_empty());
}

#[test]
fn ripple_delete_closes_the_gap_behind_it() {
    let r = rig();
    let (project, item) = r.project("Ripple");
    edits::place(&project, &place("Ripple", item, 0), true).unwrap();
    edits::place(&project, &place("Ripple", item, 60), true).unwrap();
    let d = r.describe(&project);
    let first = clip_ids(&d, "V1")[0];
    let clips = Clips {
        project: "Ripple".into(),
        clips: vec![first],
    };
    edits::delete(&project, &clips, true).unwrap();
    let d = r.describe(&project);
    assert_eq!(spans(&d, "V1"), vec![(0, 60)]);
    assert_eq!(spans(&d, "A1"), vec![(0, 60)]);
    assert_eq!(d.duration, 60);
}

#[test]
fn moving_a_clip_takes_its_partner_and_unlinking_frees_it() {
    let r = rig();
    let (project, item) = r.project("Move");
    edits::place(&project, &place("Move", item, 0), true).unwrap();
    let d = r.describe(&project);
    let video = clip_ids(&d, "V1")[0];
    let audio = clip_ids(&d, "A1")[0];
    let m = Move {
        project: "Move".into(),
        clip: video,
        frame: 30,
        track: None,
        insert: false,
        linked: None,
    };
    edits::move_clip(&project, &m).unwrap();
    let d = r.describe(&project);
    assert_eq!(spans(&d, "V1"), vec![(30, 90)]);
    assert_eq!(spans(&d, "A1"), vec![(30, 90)]);

    // Unlink, then trim the sound alone: an L-cut.
    let both = Clips {
        project: "Move".into(),
        clips: vec![video],
    };
    edits::unlink(&project, &both).unwrap();
    let d = r.describe(&project);
    assert!(
        d.tracks
            .iter()
            .flat_map(|t| &t.clips)
            .all(|c| c.link.is_none())
    );
    assert!(
        edits::unlink(&project, &both).is_err(),
        "unlinking twice succeeded"
    );
    let tail = Trim {
        project: "Move".into(),
        clip: video,
        edge: Edge::Out,
        delta: -10,
        ripple: false,
        linked: None,
    };
    edits::trim(&project, &tail).unwrap();
    let d = r.describe(&project);
    assert_eq!(spans(&d, "V1"), vec![(30, 80)]);
    assert_eq!(spans(&d, "A1"), vec![(30, 90)], "the unlinked sound moved");

    // And linked again.
    let pair = Clips {
        project: "Move".into(),
        clips: vec![video, audio],
    };
    edits::link(&project, &pair).unwrap();
    let d = r.describe(&project);
    let links: Vec<_> = d
        .tracks
        .iter()
        .flat_map(|t| &t.clips)
        .map(|c| c.link)
        .collect();
    assert!(
        links[0].is_some() && links.iter().all(|l| *l == links[0]),
        "{links:?}"
    );
    assert!(
        edits::link(&project, &pair).is_err(),
        "linking twice succeeded"
    );
}

#[test]
fn linked_false_edits_one_side_without_unlinking() {
    let r = rig();
    let (project, item) = r.project("Side");
    edits::place(&project, &place("Side", item, 0), true).unwrap();
    let d = r.describe(&project);
    let video = clip_ids(&d, "V1")[0];
    // A J-cut without unlinking: the sound starts earlier than the picture.
    let head = Trim {
        project: "Side".into(),
        clip: video,
        edge: Edge::In,
        delta: 10,
        ripple: false,
        linked: Some(false),
    };
    edits::trim(&project, &head).unwrap();
    let d = r.describe(&project);
    assert_eq!(spans(&d, "V1"), vec![(10, 60)]);
    assert_eq!(spans(&d, "A1"), vec![(0, 60)], "the sound was trimmed too");
    let m = Move {
        project: "Side".into(),
        clip: video,
        frame: 20,
        track: None,
        insert: false,
        linked: Some(false),
    };
    edits::move_clip(&project, &m).unwrap();
    let d = r.describe(&project);
    assert_eq!(spans(&d, "V1"), vec![(20, 70)]);
    assert_eq!(spans(&d, "A1"), vec![(0, 60)], "the sound moved too");
}

#[test]
fn undo_and_redo_walk_the_history_exactly() {
    let r = rig();
    let (project, item) = r.project("History");
    // The import itself is an undo step; start from a clean history head.
    let before = r.describe(&project);
    edits::place(&project, &place("History", item, 0), true).unwrap();
    let split = Split {
        project: "History".into(),
        frame: 30,
        clip: None,
    };
    edits::split(&project, &split).unwrap();
    let after = r.describe(&project);
    edits::history(&project, false).unwrap();
    assert_eq!(spans(&r.describe(&project), "V1"), vec![(0, 60)]);
    edits::history(&project, false).unwrap();
    assert_eq!(r.describe(&project).tracks, before.tracks);
    edits::history(&project, true).unwrap();
    edits::history(&project, true).unwrap();
    let redone = r.describe(&project);
    assert_eq!(redone.tracks, after.tracks);
    assert!(!redone.can_redo);
    let refused =
        edits::history(&project, true).expect_err("a redo with nothing to redo succeeded");
    assert_eq!(refused.to_string(), "nothing to redo");
}

#[test]
fn markers_are_added_moved_and_removed() {
    let r = rig();
    let (project, item) = r.project("Marks");
    edits::place(&project, &place("Marks", item, 0), true).unwrap();
    let add = edits::MarkerAdd {
        project: "Marks".into(),
        frame: 12,
        name: Some("beat".into()),
        comment: None,
    };
    edits::marker_add(&project, &add).unwrap();
    let d = r.describe(&project);
    assert_eq!(d.markers.len(), 1);
    assert_eq!(
        (d.markers[0].frame, d.markers[0].name.as_str()),
        (12, "beat")
    );
    let id = d.markers[0].id;
    let mv = edits::MarkerMove {
        project: "Marks".into(),
        marker: id,
        frame: 40,
        name: Some("drop".into()),
    };
    edits::marker_move(&project, &mv).unwrap();
    let d = r.describe(&project);
    assert_eq!(
        (d.markers[0].frame, d.markers[0].name.as_str()),
        (40, "drop")
    );
    let rm = edits::MarkerRemove {
        project: "Marks".into(),
        marker: id,
    };
    edits::marker_remove(&project, &rm).unwrap();
    assert!(r.describe(&project).markers.is_empty());
}

#[test]
fn a_saved_project_reopens_the_same_in_a_new_process() {
    let r = rig();
    let (project, item) = r.project("Keep");
    edits::place(&project, &place("Keep", item, 0), true).unwrap();
    let split = Split {
        project: "Keep".into(),
        frame: 25,
        clip: None,
    };
    edits::split(&project, &split).unwrap();
    project.save().unwrap();
    let saved = r.describe(&project);
    assert!(!saved.dirty);
    // A second studio is a new app process: nothing shared but the disk.
    let fresh = Studio::new(r.root.clone(), r.data.clone()).unwrap();
    let (reopened, opened) = fresh.open("Keep").unwrap();
    assert_eq!(opened.imported_sidecar, None);
    let d = reopened.describe(&r.root).unwrap();
    assert_eq!(d.tracks, saved.tracks);
    assert_eq!(spans(&d, "V1"), vec![(0, 25), (25, 60)]);
    // And the sidecar on disk is OpenTimelineIO with both cuts in it.
    let otio: Value =
        serde_json::from_str(&std::fs::read_to_string(r.root.join("Keep.otio")).unwrap()).unwrap();
    assert!(
        otio["OTIO_SCHEMA"]
            .as_str()
            .unwrap_or_default()
            .starts_with("Timeline"),
        "{}",
        &otio.to_string()[..200.min(otio.to_string().len())]
    );
    let text = otio.to_string();
    assert!(
        text.matches("\"Clip.").count() >= 2,
        "the sidecar lacks the cut"
    );
    // No staging file is left beside it.
    assert!(!r.root.join(".Keep.otio.partial").exists());
}

#[test]
fn an_unreadable_project_opens_from_its_sidecar_and_says_so() {
    let r = rig();
    let (project, item) = r.project("Rescue");
    edits::place(&project, &place("Rescue", item, 0), true).unwrap();
    project.save().unwrap();
    std::fs::write(r.root.join("Rescue.fcproj"), b"{ not a project").unwrap();
    let fresh = Studio::new(r.root.clone(), r.data.clone()).unwrap();
    let (reopened, opened) = fresh.open("Rescue").unwrap();
    let note = opened.imported_sidecar.expect("no word about the sidecar");
    assert!(note.contains("Rescue.otio"), "{note}");
    let d = reopened.describe(&r.root).unwrap();
    assert_eq!(spans(&d, "V1"), vec![(0, 60)]);
    // Without a sidecar it is an error, not an empty project.
    std::fs::remove_file(r.root.join("Rescue.otio")).unwrap();
    let again = Studio::new(r.root.clone(), r.data.clone()).unwrap();
    assert!(again.open("Rescue").is_err());
}

/// Wait for the recovery journal to hold the project's unsaved edits, then
/// die the way a crash does.
fn crash_after_journal(r: &Rig, project: &Project) {
    // The journal is written about a second after the edit settles.
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        project
            .execute("file.autoSaveStatus", serde_json::json!({}))
            .unwrap();
        let journal = r.data.join("Recovery");
        let written = std::fs::read_dir(&journal)
            .into_iter()
            .flatten()
            .flatten()
            .any(|e| e.path().join("snapshot.fcproj").is_file());
        if written {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the recovery journal was never written"
        );
        std::thread::sleep(Duration::from_millis(200));
    }
    project.crash();
}

#[test]
fn unsaved_edits_survive_a_crash_and_are_offered_not_applied() {
    let r = rig();
    let (project, item) = r.project("Crash");
    project.save().unwrap();
    edits::place(&project, &place("Crash", item, 0), true).unwrap();
    crash_after_journal(&r, &project);
    let fresh = Studio::new(r.root.clone(), r.data.clone()).unwrap();
    let (reopened, opened) = fresh.open("Crash").unwrap();
    assert!(
        opened.recovery.is_some(),
        "the crashed session's edits were not found"
    );
    // Offered, not applied: the file on disk had no clips.
    assert!(spans(&reopened.describe(&r.root).unwrap(), "V1").is_empty());
    reopened.recover().unwrap();
    assert_eq!(
        spans(&reopened.describe(&r.root).unwrap(), "V1"),
        vec![(0, 60)]
    );
}

#[test]
fn a_save_after_the_crash_retires_the_offer() {
    let r = rig();
    let (project, item) = r.project("Later");
    project.save().unwrap();
    edits::place(&project, &place("Later", item, 0), true).unwrap();
    crash_after_journal(&r, &project);
    let fresh = Studio::new(r.root.clone(), r.data.clone()).unwrap();
    let (reopened, opened) = fresh.open("Later").unwrap();
    assert!(opened.recovery.is_some());
    // Declined: the operator redoes the work their own way and saves it.
    edits::place(&reopened, &place("Later", item, 30), true).unwrap();
    reopened.save().unwrap();
    // Neither this session nor a later one offers the older snapshot again.
    assert_eq!(reopened.recovery(), None);
    assert!(reopened.recover().is_err());
    let next = Studio::new(r.root.clone(), r.data.clone()).unwrap();
    let (_, opened) = next.open("Later").unwrap();
    assert_eq!(
        opened.recovery, None,
        "a superseded crash snapshot was offered"
    );
}

#[test]
fn recovering_over_unsaved_edits_keeps_them_as_a_copy() {
    let r = rig();
    let (project, item) = r.project("Busy");
    project.save().unwrap();
    edits::place(&project, &place("Busy", item, 0), true).unwrap();
    crash_after_journal(&r, &project);
    let fresh = Studio::new(r.root.clone(), r.data.clone()).unwrap();
    let (reopened, opened) = fresh.open("Busy").unwrap();
    assert!(opened.recovery.is_some());
    // Edits made since opening, not saved.
    edits::place(&reopened, &place("Busy", item, 0), true).unwrap();
    edits::place(&reopened, &place("Busy", item, 60), true).unwrap();
    let recovered = reopened.recover().unwrap();
    assert_eq!(
        recovered["kept_unsaved_as"],
        "Busy (before recovery).fcproj"
    );
    // The journal's timeline is open now...
    assert_eq!(spans(&r.describe(&reopened), "V1"), vec![(0, 60)]);
    // ...and the edits it replaced are a project of their own.
    let (copy, _) = fresh.open("Busy (before recovery)").unwrap();
    assert_eq!(spans(&r.describe(&copy), "V1"), vec![(0, 60), (60, 120)]);
}

#[test]
fn recovering_a_clean_session_makes_no_copy() {
    let r = rig();
    let (project, item) = r.project("Calm");
    project.save().unwrap();
    edits::place(&project, &place("Calm", item, 0), true).unwrap();
    crash_after_journal(&r, &project);
    let fresh = Studio::new(r.root.clone(), r.data.clone()).unwrap();
    let (reopened, _) = fresh.open("Calm").unwrap();
    let recovered = reopened.recover().unwrap();
    assert_eq!(recovered["kept_unsaved_as"], Value::Null);
    assert!(!r.root.join("Calm (before recovery).fcproj").exists());
}

#[test]
fn saving_a_project_rebuilt_from_its_sidecar_keeps_the_file_that_would_not_open() {
    let r = rig();
    let (project, item) = r.project("Newer");
    edits::place(&project, &place("Newer", item, 0), true).unwrap();
    project.save().unwrap();
    // Unreadable here: from a newer FilmCraft, say, or damaged.
    let original = b"{ \"schema\": 999, \"from\": \"a newer build\" }".to_vec();
    std::fs::write(r.root.join("Newer.fcproj"), &original).unwrap();
    let fresh = Studio::new(r.root.clone(), r.data.clone()).unwrap();
    // An agent's read opens it first; the window, opening it second, is
    // still told it was rebuilt.
    fresh.get("Newer").unwrap();
    let (reopened, opened) = fresh.open("Newer").unwrap();
    let note = opened
        .imported_sidecar
        .expect("the later opener was not told");
    assert!(note.contains("Newer.fcproj.unopened"), "{note}");
    let saved = reopened.save().unwrap();
    assert_eq!(saved["kept_unopened_as"], "Newer.fcproj.unopened");
    assert_eq!(
        std::fs::read(r.root.join("Newer.fcproj.unopened")).unwrap(),
        original
    );
    // The new project file opens, and the copy is not offered as a project.
    let again = Studio::new(r.root.clone(), r.data.clone()).unwrap();
    let (_, opened) = again.open("Newer").unwrap();
    assert_eq!(opened.imported_sidecar, None);
    let names: Vec<Value> = again
        .list()
        .unwrap()
        .iter()
        .map(|p| p["name"].clone())
        .collect();
    assert_eq!(names, vec![Value::from("Newer")]);
    // A second save replaces only a file this app wrote: no second copy.
    reopened.save().unwrap();
    assert!(!r.root.join("Newer 2.fcproj.unopened").exists());
}

#[test]
fn concurrent_first_uses_share_one_session() {
    let r = rig();
    let (project, _) = r.project("Trip");
    project.save().unwrap();
    let fresh = Arc::new(Studio::new(r.root.clone(), r.data.clone()).unwrap());
    let barrier = Arc::new(std::sync::Barrier::new(8));
    let handles: Vec<_> = (0..8)
        .map(|_| {
            let (studio, barrier) = (Arc::clone(&fresh), Arc::clone(&barrier));
            std::thread::spawn(move || {
                barrier.wait();
                studio.get("Trip").unwrap()
            })
        })
        .collect();
    let got: Vec<Arc<Project>> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    let open = fresh.get("Trip").unwrap();
    for p in &got {
        assert!(
            Arc::ptr_eq(p, &open),
            "a caller was handed an orphaned session"
        );
    }
    // And creates of one name at once: one project, the rest refused.
    for round in 0..4 {
        let name = format!("Twin {round}");
        let barrier = Arc::new(std::sync::Barrier::new(8));
        let handles: Vec<_> = (0..8)
            .map(|_| {
                let (studio, barrier, name) =
                    (Arc::clone(&fresh), Arc::clone(&barrier), name.clone());
                std::thread::spawn(move || {
                    barrier.wait();
                    studio.create(&name).is_ok()
                })
            })
            .collect();
        let made: Vec<bool> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        assert_eq!(made.iter().filter(|ok| **ok).count(), 1, "{name}: {made:?}");
    }
}

#[test]
fn the_first_clip_lands_on_the_frame_asked_for_when_it_changes_the_rate() {
    let r = rig();
    let (project, item) = r.project("Rate");
    // The empty sequence at another rate than the clip's 30 fps.
    project
        .execute("sequence.settings", serde_json::json!({ "fps": 25 }))
        .unwrap();
    assert_eq!(
        r.describe(&project).fps,
        crate::timeline::Rate { num: 25, den: 1 }
    );
    let mut p = place("Rate", item, 100);
    p.duration = Some(30);
    edits::place(&project, &p, true).unwrap();
    let d = r.describe(&project);
    assert_eq!(d.fps, crate::timeline::Rate { num: 30, den: 1 });
    assert_eq!(spans(&d, "V1"), vec![(100, 130)]);
}

#[test]
fn a_still_is_a_jpeg_of_the_program_at_that_frame() {
    let r = rig();
    let (project, item) = r.project("Still");
    edits::place(&project, &place("Still", item, 0), true).unwrap();
    let still = crate::still::render(&project, 30, 96).unwrap();
    assert_eq!(still["mime"], "image/jpeg");
    assert_eq!(
        (still["width"].as_u64(), still["height"].as_u64()),
        (Some(96), Some(54))
    );
    use base64::Engine as _;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(still["data"].as_str().unwrap())
        .unwrap();
    assert_eq!(&bytes[..2], &[0xFF, 0xD8], "not a JPEG");
    let img = image::load_from_memory(&bytes).unwrap().to_rgb8();
    let mean = img
        .pixels()
        .map(|p| u64::from(p[0]) + u64::from(p[1]) + u64::from(p[2]))
        .sum::<u64>()
        / (u64::from(img.width()) * u64::from(img.height()) * 3);
    assert!(
        mean > 40,
        "the still is black (mean {mean}): the clip was not rendered"
    );
    // Past the end is black: the program has nothing there.
    let past = crate::still::render(&project, 500, 96).unwrap();
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(past["data"].as_str().unwrap())
        .unwrap();
    let img = image::load_from_memory(&bytes).unwrap().to_rgb8();
    let max = img
        .pixels()
        .map(|p| p[0].max(p[1]).max(p[2]))
        .max()
        .unwrap();
    assert!(max < 16, "a frame past the end was not black");
}

/// Wait for an export to end.
fn finish(exports: &Exports, id: u64) -> Value {
    let deadline = Instant::now() + Duration::from_secs(120);
    while !exports.finished(id) {
        assert!(Instant::now() < deadline, "the export never ended");
        std::thread::sleep(Duration::from_millis(50));
    }
    exports.get(id).unwrap()
}

fn files(root: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(root)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|n| n.ends_with(".mp4"))
        .collect();
    names.sort();
    names
}

#[test]
fn an_export_is_a_playable_mp4_and_never_overwrites() {
    let r = rig();
    let (project, item) = r.project("Out");
    edits::place(&project, &place("Out", item, 0), true).unwrap();
    let exports = Arc::new(Exports::new(r.root.clone()));
    let first = exports.start(&project, Size::P720, None, None).unwrap();
    let done = finish(&exports, first["id"].as_u64().unwrap());
    assert_eq!(done["state"], "done", "{done}");
    assert_eq!(done["name"], "Out.mp4");
    let second = exports.start(&project, Size::Source, None, None).unwrap();
    let done = finish(&exports, second["id"].as_u64().unwrap());
    assert_eq!(done["name"], "Out 2.mp4", "{done}");
    assert_eq!(files(&r.root), vec!["Out 2.mp4", "Out.mp4"]);
    // Read back by FilmCraft's own demuxer: picture and sound, the cut's
    // length, the sequence's size (a 720p export of a smaller sequence
    // does not upscale).
    let imported = project
        .execute(
            "file.import",
            serde_json::json!({ "paths": [r.root.join("Out.mp4").to_string_lossy()] }),
        )
        .unwrap();
    let back = imported["items"][0].as_u64().unwrap();
    let d = r.describe(&project);
    let m = d.media.iter().find(|m| m.item == back).unwrap();
    assert_eq!((m.width, m.height), (Some(192), Some(108)));
    assert!(
        (m.duration - 60).abs() <= 1,
        "duration {} frames",
        m.duration
    );
    assert!(m.audio_channels.is_some(), "the export has no sound");
    assert!(
        m.video_codec
            .as_deref()
            .unwrap_or_default()
            .contains("H.264"),
        "{m:?}"
    );
}

#[test]
fn a_cancelled_export_leaves_no_file_at_all() {
    let r = rig();
    let (project, item) = r.project("Long");
    // A long enough program that the export is still running when the
    // cancel lands: fifteen copies, 30 seconds.
    for i in 0..15 {
        edits::place(&project, &place("Long", item, i * 60), true).unwrap();
    }
    let exports = Arc::new(Exports::new(r.root.clone()));
    let started = exports.start(&project, Size::Source, None, None).unwrap();
    let id = started["id"].as_u64().unwrap();
    // Cancelled mid-write, so there is a half-written file to clean up.
    let partial = r.root.join(crate::names::partial_name("Long", id));
    let deadline = Instant::now() + Duration::from_secs(60);
    while !(partial.exists() && std::fs::metadata(&partial).unwrap().len() > 0) {
        assert!(
            Instant::now() < deadline,
            "the export never started writing"
        );
        assert!(
            !exports.finished(id),
            "the export ended before it could be cancelled"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    exports.cancel(id).unwrap();
    let done = finish(&exports, id);
    assert_eq!(done["state"], "cancelled", "{done}");
    // Stopped, not run to the end and then thrown away.
    assert!(done["progress"].as_f64().unwrap() < 1.0, "{done}");
    let left: Vec<String> = std::fs::read_dir(&r.root)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|n| n.contains("Long") && !n.ends_with(".fcproj") && !n.ends_with(".otio"))
        .collect();
    assert!(left.is_empty(), "a cancelled export left {left:?}");
}

#[test]
fn waiting_answers_on_the_next_edit() {
    let r = rig();
    let (project, item) = r.project("Wait");
    let since = project.revision();
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    // Nothing changes: the wait times out with the same revision.
    let same = rt.block_on(project.wait(since, Duration::from_millis(50)));
    assert_eq!(same, since);
    let waiter = Arc::clone(&project);
    let handle = std::thread::spawn(move || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(waiter.wait(since, Duration::from_secs(30)))
    });
    std::thread::sleep(Duration::from_millis(100));
    let edited = Instant::now();
    edits::place(&project, &place("Wait", item, 0), true).unwrap();
    let seen = handle.join().unwrap();
    // Woken by the edit, not by its 30-second timeout.
    assert!(
        edited.elapsed() < Duration::from_secs(10),
        "the wait was not woken"
    );
    assert_ne!(seen, since, "the wait did not see the edit");
}

// ---------------------------------------------------------------------------
// The manifest, the handlers and the UI agree

fn manifest() -> deckjack_protocol::Manifest {
    deckjack_protocol::Manifest::parse(Path::new("app.toml"), include_str!("../app.toml"))
        .expect("a valid manifest")
}

#[test]
fn the_manifest_asks_for_videos_and_nothing_else() {
    let m = manifest();
    assert_eq!(m.app.name, "video");
    assert_eq!(m.title(), "Video");
    assert_eq!(m.app.min_width, Some(360));
    assert_eq!(m.needs.working_dir.len(), 1);
    assert_eq!(m.needs.working_dir[0].path, crate::names::VIDEOS_DIR);
    assert!(m.needs.working_dir[0].write);
    // No GPU (the CPU renderer is what is linked), no network, no ports, no
    // bus calls, and the window calls only this app's own commands.
    assert!(!m.needs.gpu);
    assert!(
        m.needs.network.is_empty() && m.needs.connect_tcp.is_empty() && m.needs.bind_tcp.is_empty()
    );
    assert!(!m.needs.invoke_commands);
    assert!(m.needs.ui.invoke.is_empty() && m.needs.ui.events.is_empty());
    // Nothing destructive or outbound: everything here can be undone.
    for c in &m.commands {
        assert!(
            c.id.starts_with("video."),
            "{} is outside the app's scope",
            c.id
        );
        assert!(
            matches!(
                c.risk,
                deckjack_protocol::Risk::Read | deckjack_protocol::Risk::Write
            ),
            "{} is {:?}",
            c.id,
            c.risk
        );
    }
}

#[test]
fn every_declared_command_has_a_handler_and_no_handler_is_undeclared() {
    // `App::run` checks the registration before it looks for the core's
    // socket, so with no socket a consistent app fails on the socket and an
    // inconsistent one fails on the command.
    assert!(
        std::env::var_os(deckjack_app::SOCKET_ENV).is_none(),
        "the test environment has a core socket set"
    );
    let r = rig();
    let exports = Arc::new(Exports::new(r.root.clone()));
    let studio = Arc::new(Studio::new(r.root.clone(), r.data.clone()).unwrap());
    let error = crate::build(&studio, &exports).unwrap().run().unwrap_err();
    let error = error.to_string();
    assert!(error.contains(deckjack_app::SOCKET_ENV), "{error}");
}

/// Every `"video.…"` string literal in a script.
fn quoted_ids(script: &str) -> Vec<String> {
    script
        .split("\"video.")
        .skip(1)
        .filter_map(|rest| rest.split('"').next())
        .filter(|tail| {
            tail.chars()
                .all(|c| c.is_ascii_lowercase() || c == '.' || c == '_')
        })
        .map(|tail| format!("video.{tail}"))
        .collect()
}

const SCRIPTS: &[(&str, &str)] = &[
    ("main.js", include_str!("../ui/main.js")),
    ("video.js", include_str!("../ui/video.js")),
    ("timeline.js", include_str!("../ui/timeline.js")),
    ("model.js", include_str!("../ui/model.js")),
];

#[test]
fn the_ui_names_only_declared_commands() {
    let declared: Vec<String> = manifest().commands.iter().map(|c| c.id.clone()).collect();
    let mut named = Vec::new();
    for (_, script) in SCRIPTS {
        named.extend(quoted_ids(script));
    }
    assert!(named.len() > 10, "the scan found almost nothing: {named:?}");
    for id in &named {
        assert!(
            declared.contains(id),
            "the UI calls {id}, which app.toml does not declare"
        );
    }
}

#[test]
fn the_ui_is_embedded_and_built_for_the_isolated_frame() {
    for path in [
        "index.html",
        "main.js",
        "video.js",
        "timeline.js",
        "model.js",
        "video.css",
    ] {
        let file = crate::Ui::get(path).unwrap_or_else(|| panic!("{path} is not embedded"));
        assert!(!file.data.is_empty(), "{path} is empty");
    }
    let index = include_str!("../ui/index.html");
    // No inline script (D4): every script tag has a src.
    for tag in index.split("<script").skip(1) {
        let open = tag.split('>').next().unwrap_or_default();
        assert!(open.contains("src="), "an inline script: <script{open}>");
    }
    // No /content (stage 4 takes it away from frames), no network, no wasm.
    let banned = [
        "/content",
        "http://",
        "https://",
        "fetch(",
        "WebSocket",
        "localStorage",
        "WebAssembly",
        "RTCPeerConnection",
    ];
    for (name, text) in SCRIPTS.iter().copied().chain([("index.html", index)]) {
        for b in banned {
            assert!(!text.contains(b), "{name} uses {b}");
        }
    }
}
