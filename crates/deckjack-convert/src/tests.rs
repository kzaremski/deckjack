//! Tests against the real ffmpeg: the lane brief's exit check, as far as it
//! runs without a phone.
//!
//! Fixtures are made at test time with ffmpeg's own test sources, so no
//! binary media is committed and every test says what its input is. On a
//! machine without `/usr/bin/ffmpeg` each test prints why and returns; it is
//! no evidence of anything there.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use deckjack_app::Cancellation;

use crate::files::Grants;
use crate::presets::{Options, PresetId, Range};
use crate::queue::{AddRequest, Job, JobState, Queue, Time};
use crate::tools::Tools;

fn tools() -> Option<Tools> {
    match Tools::locate() {
        Ok(t) => Some(t),
        Err(e) => {
            eprintln!("skipped: {e}");
            None
        }
    }
}

/// Make a media file with ffmpeg's test sources.
fn make(path: &Path, args: &[&str]) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let out = Command::new("/usr/bin/ffmpeg")
        .args(["-hide_banner", "-loglevel", "error", "-y"])
        .args(args)
        .arg(path)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// H.264 and AAC in MP4, a keyframe every two seconds.
fn clip(path: &Path, secs: u32, size: &str) {
    let secs = secs.to_string();
    make(
        path,
        &[
            "-f",
            "lavfi",
            "-i",
            &format!("testsrc2=size={size}:rate=30:duration={secs}"),
            "-f",
            "lavfi",
            "-i",
            &format!("sine=frequency=440:duration={secs}"),
            "-c:v",
            "libx264",
            "-preset",
            "ultrafast",
            "-g",
            "60",
            "-keyint_min",
            "60",
            "-sc_threshold",
            "0",
            "-c:a",
            "aac",
            "-shortest",
        ],
    );
}

fn mp3(path: &Path, secs: u32) {
    make(
        path,
        &[
            "-f",
            "lavfi",
            "-i",
            &format!("sine=frequency=330:duration={secs}"),
            "-c:a",
            "libmp3lame",
            "-b:a",
            "128k",
        ],
    );
}

struct World {
    dir: tempfile::TempDir,
    queue: Arc<Queue>,
    notes: Arc<AtomicUsize>,
}

impl World {
    fn root(&self) -> &Path {
        self.dir.path()
    }
    fn videos(&self) -> PathBuf {
        self.root().join("Videos")
    }
}

fn grants(root: &Path) -> Grants {
    Grants::from_manifest(include_str!("../app.toml"), root.to_path_buf())
}

fn world(tools: Tools) -> World {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("Videos")).unwrap();
    std::fs::create_dir_all(dir.path().join("Music")).unwrap();
    std::fs::create_dir_all(dir.path().join("home")).unwrap();
    let notes = Arc::new(AtomicUsize::new(0));
    let n = Arc::clone(&notes);
    let queue = Queue::open(
        grants(dir.path()),
        Ok(tools),
        &dir.path().join("home"),
        move || {
            n.fetch_add(1, Ordering::SeqCst);
        },
    );
    World { dir, queue, notes }
}

fn add(paths: &[&str], preset: PresetId) -> AddRequest {
    AddRequest {
        paths: paths.iter().map(|p| p.to_string()).collect(),
        preset,
        start: None,
        end: None,
        container: None,
        keep_all_audio: None,
        allow_lossy_to_lossless: None,
        dest: None,
    }
}

/// Wait until `id` is in a state `done` accepts, or fail saying where it is.
fn wait(queue: &Queue, id: &str, limit: Duration, done: impl Fn(&Job) -> bool) -> Job {
    let deadline = Instant::now() + limit;
    loop {
        let job = queue.get(id).expect("the job exists");
        if done(&job) {
            return job;
        }
        assert!(Instant::now() < deadline, "timed out waiting: {job:#?}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn finished(queue: &Queue, id: &str) -> Job {
    wait(queue, id, Duration::from_secs(120), |j| j.state.finished())
}

fn hidden_files(dir: &Path) -> Vec<String> {
    std::fs::read_dir(dir)
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with('.'))
        .collect()
}

fn codecs(tools: &Tools, path: &Path) -> Vec<String> {
    crate::probe::probe(tools, path)
        .unwrap()
        .streams
        .into_iter()
        .map(|s| s.codec)
        .collect()
}

#[test]
fn a_batch_converts_never_overwrites_and_leaves_its_sources_byte_identical() {
    let Some(t) = tools() else { return };
    let w = world(t.clone());
    clip(&w.videos().join("one.mp4"), 2, "320x240");
    clip(&w.videos().join("trip/two.mov"), 2, "320x240");
    let before: Vec<Vec<u8>> = ["one.mp4", "trip/two.mov"]
        .iter()
        .map(|f| std::fs::read(w.videos().join(f)).unwrap())
        .collect();
    w.queue.start();

    let added = w
        .queue
        .add(&add(
            &["Videos/one.mp4", "Videos/trip/two.mov"],
            PresetId::ShareH264,
        ))
        .unwrap();
    assert_eq!(added.added.len(), 2);
    assert!(added.refused.is_empty());
    let outputs: Vec<String> = added
        .added
        .iter()
        .map(|j| {
            let job = finished(&w.queue, &j.id);
            assert_eq!(job.state, JobState::Done, "{job:#?}");
            assert!(job.command.as_deref().unwrap().contains("libx264"));
            job.output.unwrap()
        })
        .collect();
    assert_eq!(
        outputs,
        ["Videos/one (Share).mp4", "Videos/trip/two (Share).mp4"]
    );
    assert_eq!(codecs(&t, &w.root().join(&outputs[0])), ["h264", "aac"]);

    // The same batch again: new names, nothing replaced.
    let again = w
        .queue
        .add(&add(
            &["Videos/one.mp4", "Videos/trip/two.mov"],
            PresetId::ShareH264,
        ))
        .unwrap();
    let outputs: Vec<String> = again
        .added
        .iter()
        .map(|j| finished(&w.queue, &j.id).output.unwrap())
        .collect();
    assert_eq!(
        outputs,
        ["Videos/one (Share) 2.mp4", "Videos/trip/two (Share) 2.mp4"]
    );
    let after: Vec<Vec<u8>> = ["one.mp4", "trip/two.mov"]
        .iter()
        .map(|f| std::fs::read(w.videos().join(f)).unwrap())
        .collect();
    assert!(before == after, "a source changed");
    assert!(hidden_files(&w.videos()).is_empty(), "a partial was left");
    assert!(
        w.notes.load(Ordering::SeqCst) > 0,
        "the window was never told"
    );
    // The queue survived on disk.
    let saved = std::fs::read_to_string(w.root().join("home/queue.json")).unwrap();
    assert!(saved.contains("one (Share) 2.mp4"), "{saved}");
}

#[test]
fn cancelling_a_running_job_leaves_no_partial_and_the_next_job_starts() {
    let Some(t) = tools() else { return };
    let w = world(t);
    clip(&w.videos().join("long.mp4"), 40, "1280x720");
    clip(&w.videos().join("short.mp4"), 2, "320x240");
    w.queue.start();
    let long = w
        .queue
        .add(&add(&["Videos/long.mp4"], PresetId::SmallAv1))
        .unwrap()
        .added[0]
        .clone();
    let short = w
        .queue
        .add(&add(&["Videos/short.mp4"], PresetId::ShareH264))
        .unwrap()
        .added[0]
        .clone();
    // Running, with ffmpeg writing its partial.
    let running = wait(&w.queue, &long.id, Duration::from_secs(30), |j| {
        j.state == JobState::Running && j.fraction.is_some_and(|f| f > 0.0)
    });
    let partial = w.root().join(running.partial.as_deref().unwrap());
    assert!(partial.exists(), "{partial:?}");
    assert_eq!(w.queue.get(&short.id).unwrap().state, JobState::Queued);

    w.queue.cancel(&long.id).unwrap();
    let cancelled = finished(&w.queue, &long.id);
    assert_eq!(cancelled.state, JobState::Cancelled);
    assert!(!partial.exists(), "the partial was left behind");
    assert_eq!(finished(&w.queue, &short.id).state, JobState::Done);
    assert!(hidden_files(&w.videos()).is_empty());
    // A finished job cannot be cancelled, and says so.
    assert!(w.queue.cancel(&long.id).unwrap_err().contains("already"));
    // Retry queues it again as asked.
    assert_eq!(w.queue.retry(&long.id).unwrap().state, JobState::Queued);
    w.queue.cancel(&long.id).unwrap();
}

#[test]
fn a_killed_ffmpeg_fails_the_job_with_a_sentence_and_no_partial() {
    let Some(t) = tools() else { return };
    let w = world(t);
    clip(&w.videos().join("long.mp4"), 40, "1280x720");
    w.queue.start();
    let job = w
        .queue
        .add(&add(&["Videos/long.mp4"], PresetId::SmallAv1))
        .unwrap()
        .added[0]
        .clone();
    wait(&w.queue, &job.id, Duration::from_secs(30), |j| {
        j.fraction.is_some_and(|f| f > 0.0)
    });
    let pid = w.queue.pid_of(&job.id).expect("the job knows its ffmpeg");
    // SAFETY: the pid of a child the queue started and has not reaped.
    unsafe {
        libc::kill(pid as i32, libc::SIGKILL);
    }
    let failed = finished(&w.queue, &job.id);
    assert_eq!(failed.state, JobState::Failed);
    let error = failed.error.unwrap();
    assert!(error.contains("killed (signal 9)"), "{error}");
    assert!(
        hidden_files(&w.videos()).is_empty(),
        "the partial was left behind"
    );
}

#[test]
fn a_restart_mid_job_leaves_an_interrupted_job_with_retry_and_no_orphan_partial() {
    let Some(t) = tools() else { return };
    let w = world(t.clone());
    clip(&w.videos().join("long.mp4"), 40, "1280x720");
    w.queue.start();
    let job = w
        .queue
        .add(&add(&["Videos/long.mp4"], PresetId::SmallAv1))
        .unwrap()
        .added[0]
        .clone();
    let running = wait(&w.queue, &job.id, Duration::from_secs(30), |j| {
        j.fraction.is_some_and(|f| f > 0.0)
    });
    // What a crash leaves: the queue as it was saved while running, and the
    // partial ffmpeg was writing.
    let saved = std::fs::read(w.root().join("home/queue.json")).unwrap();
    let partial = w.root().join(running.partial.unwrap());
    let partial_bytes = std::fs::read(&partial).unwrap();
    w.queue.cancel(&job.id).unwrap();
    finished(&w.queue, &job.id);
    // Restore the crash's leftovers, plus two hidden files that are not this
    // job's partial and must survive.
    std::fs::write(&partial, &partial_bytes).unwrap();
    std::fs::write(w.videos().join(".j99.convert-partial"), b"another job's").unwrap();
    std::fs::write(w.videos().join(".notes"), b"someone else's").unwrap();
    let home = w.root().join("home2");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(home.join("queue.json"), saved).unwrap();

    let restarted = Queue::open(grants(w.root()), Ok(t), &home, || {});
    let interrupted = restarted.get(&job.id).unwrap();
    assert_eq!(interrupted.state, JobState::Failed);
    assert!(interrupted.error.unwrap().starts_with("interrupted"));
    assert!(!partial.exists(), "the orphan partial was left");
    let mut left = hidden_files(&w.videos());
    left.sort();
    assert_eq!(left, [".j99.convert-partial", ".notes"]);
    // Retry is offered, and works.
    assert_eq!(restarted.retry(&job.id).unwrap().state, JobState::Queued);
    // The interruption was written back, so a second restart agrees.
    let again = Queue::open(grants(w.root()), Ok(Tools::locate().unwrap()), &home, || {});
    assert_eq!(again.get(&job.id).unwrap().state, JobState::Queued);
}

/// Stand-in binaries that only record that they ran.
fn recording_tools(dir: &Path) -> (Tools, PathBuf) {
    use std::os::unix::fs::PermissionsExt;
    let marker = dir.join("ran");
    let script = dir.join("fake-ff");
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\necho \"$0 $*\" >> {}\nexit 1\n",
            marker.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    (
        Tools {
            ffmpeg: script.clone(),
            ffprobe: script,
            threads: 1,
        },
        marker,
    )
}

#[test]
fn a_crafted_playlist_or_concat_list_is_refused_before_any_ffmpeg_runs() {
    let scratch = tempfile::tempdir().unwrap();
    let (fake, marker) = recording_tools(scratch.path());
    let w = world(fake);
    std::fs::write(
        w.videos().join("holiday.mp4"),
        "#EXTM3U\n#EXT-X-TARGETDURATION:6\n#EXTINF:6.0,\n../Documents/secret.mp4\n#EXT-X-ENDLIST\n",
    )
    .unwrap();
    std::fs::write(
        w.videos().join("cut.mkv"),
        "ffconcat version 1.0\nfile '/etc/passwd'\n",
    )
    .unwrap();
    let err = w
        .queue
        .add(&add(
            &["Videos/holiday.mp4", "Videos/cut.mkv"],
            PresetId::Remux,
        ))
        .unwrap_err();
    assert!(err.contains("holiday.mp4 is a playlist"), "{err}");
    assert!(err.contains("cut.mkv is a concat list"), "{err}");
    assert!(!marker.exists(), "an ffmpeg tool ran on a hostile file");
    assert!(w.queue.list().is_empty());
    // The control: an ordinary file does reach ffprobe (which, being the
    // stand-in, fails it).
    std::fs::write(w.videos().join("real.mp4"), b"\0\0\0\x18ftypmp42").unwrap();
    assert!(
        w.queue
            .add(&add(&["Videos/real.mp4"], PresetId::Remux))
            .is_err()
    );
    assert!(marker.exists(), "the control never reached ffprobe");
}

#[test]
fn the_format_whitelist_is_a_second_wall_inside_ffprobe() {
    let Some(t) = tools() else { return };
    let dir = tempfile::tempdir().unwrap();
    clip(&dir.path().join("clip.mp4"), 1, "160x120");
    let list = dir.path().join("list.mkv");
    std::fs::write(&list, "ffconcat version 1.0\nfile 'clip.mp4'\n").unwrap();
    // Past the sniff on purpose: ffprobe alone must refuse the concat
    // demuxer.
    let refused = crate::probe::probe_unsniffed(&t, &list);
    assert!(refused.is_err(), "{refused:?}");
    // Without the whitelist the same ffprobe reads it as concat, which is
    // what the wall is for.
    let open = Command::new(&t.ffprobe)
        .args([
            "-v",
            "error",
            "-show_entries",
            "format=format_name",
            "-of",
            "csv=p=0",
        ])
        .arg(&list)
        .output()
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&open.stdout).trim(), "concat");
    // The control: the clip itself probes.
    assert!(crate::probe::probe_unsniffed(&t, &dir.path().join("clip.mp4")).is_ok());
}

#[test]
fn a_remux_of_an_mkv_to_mp4_takes_seconds_and_changes_no_stream() {
    let Some(t) = tools() else { return };
    let w = world(t.clone());
    make(
        &w.videos().join("film.mkv"),
        &[
            "-f",
            "lavfi",
            "-i",
            "testsrc2=size=640x360:rate=30:duration=20",
            "-f",
            "lavfi",
            "-i",
            "sine=duration=20",
            "-c:v",
            "libx264",
            "-preset",
            "ultrafast",
            "-c:a",
            "aac",
            "-shortest",
        ],
    );
    let estimate = crate::estimate::estimate(
        &t,
        w.queue.grants(),
        "Videos/film.mkv",
        PresetId::Remux,
        &Options::default(),
        &w.root().join("scratch"),
        &Cancellation::new(),
        |_, _| {},
    )
    .unwrap();
    assert!(
        estimate.gloss.starts_with("No quality change"),
        "{}",
        estimate.gloss
    );
    assert!(estimate.samples.is_empty(), "a copy is not sampled");
    w.queue.start();
    let started = Instant::now();
    let job = w
        .queue
        .add(&add(&["Videos/film.mkv"], PresetId::Remux))
        .unwrap()
        .added[0]
        .clone();
    let done = finished(&w.queue, &job.id);
    let took = started.elapsed();
    assert_eq!(done.state, JobState::Done, "{done:#?}");
    assert!(took < Duration::from_secs(10), "{took:?}");
    let out = w.root().join(done.output.unwrap());
    assert_eq!(out.extension().unwrap(), "mp4");
    let (src, dst) = (
        crate::probe::probe(&t, &w.videos().join("film.mkv")).unwrap(),
        crate::probe::probe(&t, &out).unwrap(),
    );
    let shape = |p: &crate::probe::Probe| {
        p.streams
            .iter()
            .map(|s| (s.codec.clone(), s.width, s.height))
            .collect::<Vec<_>>()
    };
    assert_eq!(shape(&src), shape(&dst));
    eprintln!("remux of a 20 s MKV: {took:?}");
}

#[test]
fn flac_from_an_mp3_is_refused_with_the_reason_unless_allowed() {
    let Some(t) = tools() else { return };
    let w = world(t.clone());
    mp3(&w.root().join("Music/song.mp3"), 3);
    let err = w
        .queue
        .add(&add(&["Music/song.mp3"], PresetId::AudioFlac))
        .unwrap_err();
    assert!(err.contains("bigger and no better"), "{err}");
    assert!(w.queue.list().is_empty());
    w.queue.start();
    let mut allowed = add(&["Music/song.mp3"], PresetId::AudioFlac);
    allowed.allow_lossy_to_lossless = Some(true);
    let added = w.queue.add(&allowed).unwrap();
    assert!(added.added[0].warnings[0].contains("lossy"));
    let done = finished(&w.queue, &added.added[0].id);
    assert_eq!(done.output.as_deref(), Some("Music/song (FLAC).flac"));
    assert_eq!(
        codecs(&t, &w.root().join("Music/song (FLAC).flac")),
        ["flac"]
    );
    // Opus from the same mp3 is queued with the second-generation warning.
    let opus = w
        .queue
        .add(&add(&["Music/song.mp3"], PresetId::AudioOpus))
        .unwrap();
    assert!(opus.added[0].warnings[0].contains("second generation"));
    assert_eq!(
        finished(&w.queue, &opus.added[0].id).output.as_deref(),
        Some("Music/song (Opus).opus")
    );
}

#[test]
fn an_estimate_measures_samples_and_the_output_lands_inside_its_band() {
    let Some(t) = tools() else { return };
    let w = world(t.clone());
    clip(&w.videos().join("walk.mp4"), 30, "1280x720");
    let mut steps = Vec::new();
    let e = crate::estimate::estimate(
        &t,
        w.queue.grants(),
        "Videos/walk.mp4",
        PresetId::ShareH264,
        &Options::default(),
        &w.root().join("scratch"),
        &Cancellation::new(),
        |f, s| steps.push((f, s)),
    )
    .unwrap();
    assert_eq!(e.samples.len(), 3);
    assert_eq!(e.metric, Some("VMAF"));
    let score = e.score.unwrap();
    assert!((0.0..=100.0).contains(&score), "{score}");
    assert!(e.low_bytes <= e.predicted_bytes && e.predicted_bytes <= e.high_bytes);
    assert!(e.predicted_secs.unwrap() > 0.0);
    assert!(e.gloss.contains("measured on 3 samples"), "{}", e.gloss);
    assert!(e.command.contains("libx264") && e.command.contains("file:"));
    assert!(
        steps.iter().any(|(_, s)| s.contains("Scoring")),
        "{steps:?}"
    );
    assert!(
        std::fs::read_dir(w.root().join("scratch"))
            .unwrap()
            .next()
            .is_none(),
        "samples were left in scratch"
    );
    w.queue.remember_estimate(
        "Videos/walk.mp4",
        PresetId::ShareH264,
        &Options::default(),
        e.predicted_bytes,
    );
    w.queue.start();
    let started = Instant::now();
    let job = w
        .queue
        .add(&add(&["Videos/walk.mp4"], PresetId::ShareH264))
        .unwrap()
        .added[0]
        .clone();
    assert_eq!(
        job.estimated_bytes,
        Some(e.predicted_bytes),
        "the estimate was used"
    );
    let done = finished(&w.queue, &job.id);
    let took = started.elapsed().as_secs_f64();
    let actual = std::fs::metadata(w.root().join(done.output.unwrap()))
        .unwrap()
        .len();
    eprintln!(
        "estimate: predicted {} ({}..{}), actual {actual}, VMAF {score:.1}, predicted {:.1} s, took {took:.1} s",
        e.predicted_bytes,
        e.low_bytes,
        e.high_bytes,
        e.predicted_secs.unwrap()
    );
    assert!(
        (e.low_bytes..=e.high_bytes).contains(&actual),
        "the output ({actual}) is outside the band the estimate stated"
    );
}

#[test]
fn an_mkv_sample_is_scored_frame_for_frame_not_by_rounded_time() {
    // Small (AV1) writes Matroska, whose millisecond timestamps paired each
    // sample frame with its neighbor in the source when frames were matched
    // by time: a clean AV1 encode scored about 72 (measured). Matched by
    // index it scores like the same encode in MP4 (about 97).
    let Some(t) = tools() else { return };
    let w = world(t.clone());
    clip(&w.videos().join("plain.mp4"), 14, "1280x720");
    let e = crate::estimate::estimate(
        &t,
        w.queue.grants(),
        "Videos/plain.mp4",
        PresetId::SmallAv1,
        &Options::default(),
        &w.root().join("scratch"),
        &Cancellation::new(),
        |_, _| {},
    )
    .unwrap();
    assert_eq!(e.metric, Some("VMAF"));
    let score = e.score.unwrap();
    assert!(score > 90.0, "an AV1 sample in MKV scored {score}");
}

#[test]
fn a_trim_starts_on_the_keyframe_before_and_says_so() {
    let Some(t) = tools() else { return };
    let w = world(t.clone());
    clip(&w.videos().join("talk.mp4"), 10, "320x240");
    let opts = Options {
        range: Some(Range {
            start: 3.0,
            end: 5.0,
        }),
        ..Options::default()
    };
    let e = crate::estimate::estimate(
        &t,
        w.queue.grants(),
        "Videos/talk.mp4",
        PresetId::Trim,
        &opts,
        &w.root().join("scratch"),
        &Cancellation::new(),
        |_, _| {},
    )
    .unwrap();
    assert_eq!(e.real_start, Some(2.0), "keyframes are every 2 s");
    assert!(
        e.warnings.iter().any(|w| w.contains("starts at 0:02.0")),
        "{:?}",
        e.warnings
    );
    w.queue.start();
    let mut req = add(&["Videos/talk.mp4"], PresetId::Trim);
    req.start = Some(Time::Text("0:03".into()));
    req.end = Some(Time::Seconds(5.0));
    let job = w.queue.add(&req).unwrap().added[0].clone();
    let done = finished(&w.queue, &job.id);
    assert_eq!(done.state, JobState::Done, "{done:#?}");
    assert_eq!(done.real_start, Some(2.0));
    assert_eq!(done.output.as_deref(), Some("Videos/talk (Trim).mp4"));
    let out = crate::probe::probe(&t, &w.root().join("Videos/talk (Trim).mp4")).unwrap();
    eprintln!(
        "trim 3..5 s with keyframes every 2 s: {:?} s out",
        out.duration
    );
    assert!(out.duration.unwrap() < 5.0);
}

#[test]
fn a_batch_that_would_not_fit_the_free_space_is_refused_whole() {
    let Some(t) = tools() else { return };
    let dir = tempfile::tempdir().unwrap();
    clip(&dir.path().join("Videos/a.mp4"), 1, "160x120");
    let queue = Queue::open_with(
        grants(dir.path()),
        Ok(t),
        &dir.path().join("home"),
        || {},
        |_| Some(1_000),
    );
    let err = queue
        .add(&add(&["Videos/a.mp4"], PresetId::Remux))
        .unwrap_err();
    assert!(
        err.contains("only 1.0 KB is free; nothing was queued"),
        "{err}"
    );
    assert!(queue.list().is_empty());
}

#[test]
fn a_still_is_a_small_jpeg_of_the_source_or_of_a_sample_encode() {
    use base64::Engine;
    let Some(t) = tools() else { return };
    let w = world(t.clone());
    clip(&w.videos().join("clip.mp4"), 4, "1920x1080");
    for preset in [None, Some(PresetId::ShareH264)] {
        let v = crate::estimate::frame(
            &t,
            w.queue.grants(),
            "Videos/clip.mp4",
            1.0,
            preset,
            640,
            &w.root().join("scratch"),
            &Cancellation::new(),
        )
        .unwrap();
        let jpeg = base64::engine::general_purpose::STANDARD
            .decode(v["jpeg"].as_str().unwrap())
            .unwrap();
        assert_eq!(&jpeg[..2], b"\xff\xd8", "a JPEG");
        assert!(jpeg.len() <= crate::estimate::MAX_JPEG);
    }
    let mut no_picture = add(&[], PresetId::AudioOpus);
    no_picture.paths.clear();
    mp3(&w.root().join("Music/a.mp3"), 1);
    assert!(
        crate::estimate::frame(
            &t,
            w.queue.grants(),
            "Music/a.mp3",
            0.0,
            None,
            640,
            &w.root().join("scratch"),
            &Cancellation::new()
        )
        .is_err()
    );
}

#[test]
fn a_path_outside_the_grants_or_a_batch_too_big_is_refused_with_a_sentence() {
    let scratch = tempfile::tempdir().unwrap();
    let (fake, _) = recording_tools(scratch.path());
    let w = world(fake);
    let err = w
        .queue
        .add(&add(&["Documents/a.mp4"], PresetId::Remux))
        .unwrap_err();
    assert!(err.contains("not a path inside Videos/, Music/"), "{err}");
    let many: Vec<String> = (0..101).map(|i| format!("Videos/{i}.mp4")).collect();
    let mut big = add(&[], PresetId::Remux);
    big.paths = many;
    assert!(w.queue.add(&big).unwrap_err().contains("more than 100"));
    assert!(
        w.queue
            .add(&add(&[], PresetId::Remux))
            .unwrap_err()
            .contains("no files")
    );
}

/// Spike S2 on synthetic content, run by hand:
/// `cargo test -p deckjack-convert measure_estimates -- --ignored --nocapture`.
///
/// For each source and lossy preset: the estimate, then the full encode
/// through the queue, and one table row comparing them. The numbers in the
/// lane doc's "Built" section come from this. It asserts nothing beyond the
/// encodes finishing, because its job is to measure; the band itself is
/// asserted by `an_estimate_measures_samples_and_the_output_lands_inside_its_band`.
#[test]
#[ignore = "a measurement, minutes long; run by hand"]
fn measure_estimates() {
    let Some(t) = tools() else { return };
    let w = world(t.clone());
    let x264 = ["-c:v", "libx264", "-preset", "veryfast", "-crf", "18"];
    let audio = ["-c:a", "aac", "-shortest"];
    let video = |name: &str, graph: &str| {
        let mut args = vec!["-f", "lavfi", "-i", graph];
        args.extend(["-f", "lavfi", "-i", "sine=frequency=440:duration=60"]);
        args.extend(x264);
        args.extend(audio);
        make(&w.videos().join(name), &args);
    };
    video("clean.mp4", "testsrc2=size=1280x720:rate=30:duration=60");
    video(
        "grain.mp4",
        "testsrc2=size=1280x720:rate=30:duration=60,noise=alls=20:allf=t+u",
    );
    video(
        "zoom.mp4",
        "mandelbrot=size=1280x720:rate=30,trim=duration=60",
    );
    video(
        "cuts.mp4",
        "testsrc2=size=1280x720:rate=30:duration=20[a];\
         testsrc2=size=1280x720:rate=30:duration=20,noise=alls=30:allf=t+u[b];\
         smptehdbars=size=1280x720:rate=30:duration=20[c];[a][b][c]concat=n=3:v=1:a=0",
    );
    make(
        &w.root().join("Music/mix.flac"),
        &[
            "-f",
            "lavfi",
            "-i",
            "anoisesrc=color=pink:amplitude=0.2:duration=60",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=330:duration=60",
            "-filter_complex",
            "amix=inputs=2",
            "-c:a",
            "flac",
        ],
    );
    w.queue.start();
    eprintln!(
        "| source | preset | predicted | band | actual | error | in band | score | est. time | took |"
    );
    eprintln!("|---|---|---|---|---|---|---|---|---|---|");
    let runs = [
        ("Videos/clean.mp4", PresetId::ShareH264),
        ("Videos/grain.mp4", PresetId::ShareH264),
        ("Videos/zoom.mp4", PresetId::ShareH264),
        ("Videos/cuts.mp4", PresetId::ShareH264),
        ("Videos/clean.mp4", PresetId::SmallAv1),
        ("Videos/grain.mp4", PresetId::SmallAv1),
        ("Videos/zoom.mp4", PresetId::SmallAv1),
        ("Videos/cuts.mp4", PresetId::SmallAv1),
        ("Music/mix.flac", PresetId::AudioOpus),
    ];
    for (path, preset) in runs {
        let measured = Instant::now();
        let e = crate::estimate::estimate(
            &t,
            w.queue.grants(),
            path,
            preset,
            &Options::default(),
            &w.root().join("scratch"),
            &Cancellation::new(),
            |_, _| {},
        )
        .unwrap();
        let measuring = measured.elapsed().as_secs_f64();
        let started = Instant::now();
        let job = w.queue.add(&add(&[path], preset)).unwrap().added[0].clone();
        let done = wait(&w.queue, &job.id, Duration::from_secs(900), |j| {
            j.state.finished()
        });
        assert_eq!(done.state, JobState::Done, "{done:#?}");
        let took = started.elapsed().as_secs_f64();
        let actual = std::fs::metadata(w.root().join(done.output.unwrap()))
            .unwrap()
            .len();
        let mb = |b: u64| b as f64 / 1e6;
        eprintln!(
            "| {path} | {} | {:.2} MB | {:.2}–{:.2} MB | {:.2} MB | {:+.0}% | {} | {} | {} (measuring took {measuring:.1} s) | {took:.1} s |",
            preset.name(),
            mb(e.predicted_bytes),
            mb(e.low_bytes),
            mb(e.high_bytes),
            mb(actual),
            (e.predicted_bytes as f64 / actual as f64 - 1.0) * 100.0,
            if (e.low_bytes..=e.high_bytes).contains(&actual) {
                "yes"
            } else {
                "**no**"
            },
            match (e.metric, e.score) {
                (Some(m), Some(s)) => format!("{m} {s:.1}"),
                _ => "none".into(),
            },
            e.predicted_secs.map_or("?".into(), |s| format!("{s:.1} s")),
        );
    }
}

#[test]
fn two_at_once_runs_two_jobs_together_and_a_third_waits() {
    let Some(t) = tools() else { return };
    let w = world(t);
    clip(&w.videos().join("long.mp4"), 40, "1280x720");
    w.queue.start();
    let ids: Vec<String> = (0..3)
        .map(|_| {
            w.queue
                .add(&add(&["Videos/long.mp4"], PresetId::SmallAv1))
                .unwrap()
                .added[0]
                .id
                .clone()
        })
        .collect();
    // One at a time: the first runs, the others wait.
    wait(&w.queue, &ids[0], Duration::from_secs(30), |j| {
        j.state == JobState::Running
    });
    std::thread::sleep(Duration::from_millis(1500));
    assert_eq!(w.queue.get(&ids[1]).unwrap().state, JobState::Queued);
    // Two at once: the second starts beside the first, the third waits.
    assert_eq!(w.queue.set_parallel(2), Ok(2));
    wait(&w.queue, &ids[1], Duration::from_secs(10), |j| {
        j.state == JobState::Running
    });
    std::thread::sleep(Duration::from_millis(1500));
    assert_eq!(w.queue.get(&ids[0]).unwrap().state, JobState::Running);
    assert_eq!(w.queue.get(&ids[2]).unwrap().state, JobState::Queued);
    assert!(w.queue.set_parallel(3).unwrap_err().contains("1 or 2"));
    for id in &ids {
        w.queue.cancel(id).unwrap();
    }
    for id in &ids {
        assert_eq!(finished(&w.queue, id).state, JobState::Cancelled);
    }
    assert!(hidden_files(&w.videos()).is_empty());
}

#[test]
fn a_job_saves_into_another_granted_folder_and_leaves_nothing_behind() {
    let Some(t) = tools() else { return };
    let w = world(t.clone());
    clip(&w.videos().join("a.mp4"), 2, "320x240");
    let before = std::fs::read(w.videos().join("a.mp4")).unwrap();
    w.queue.start();
    let mut req = add(&["Videos/a.mp4"], PresetId::ShareH264);
    req.dest = Some("Music".into());
    let job = w.queue.add(&req).unwrap().added[0].clone();
    assert_eq!(job.dest.as_deref(), Some("Music"));
    let done = finished(&w.queue, &job.id);
    assert_eq!(done.state, JobState::Done, "{done:#?}");
    assert_eq!(done.output.as_deref(), Some("Music/a (Share).mp4"));
    assert_eq!(
        codecs(&t, &w.root().join("Music/a (Share).mp4")),
        ["h264", "aac"]
    );
    assert!(hidden_files(&w.videos()).is_empty(), "a partial in Videos/");
    assert!(
        hidden_files(&w.root().join("Music")).is_empty(),
        "a partial in Music/"
    );
    let names: Vec<String> = std::fs::read_dir(w.videos())
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(names, ["a.mp4"], "an output landed beside the source");
    assert!(std::fs::read(w.videos().join("a.mp4")).unwrap() == before);
    // A folder outside the grants is refused.
    let mut outside = add(&["Videos/a.mp4"], PresetId::ShareH264);
    outside.dest = Some("Documents".into());
    assert!(w.queue.add(&outside).is_err());
}

#[test]
fn the_queue_holds_at_most_500_jobs_and_a_mixed_batch_queues_what_it_can() {
    let Some(t) = tools() else { return };
    let dir = tempfile::tempdir().unwrap();
    clip(&dir.path().join("Videos/a.mp4"), 1, "160x120");
    std::fs::write(
        dir.path().join("Videos/list.mp4"),
        "#EXTM3U\n#EXTINF:6.0,\n../Documents/secret.mp4\n#EXT-X-ENDLIST\n",
    )
    .unwrap();
    let home = dir.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    let jobs: Vec<serde_json::Value> = (1..=499)
        .map(|i| {
            serde_json::json!({
                "id": format!("j{i}"), "path": "Videos/a.mp4", "preset": "remux",
                "state": "done", "added": 1
            })
        })
        .collect();
    std::fs::write(
        home.join("queue.json"),
        serde_json::to_vec(&serde_json::json!({ "next": 499, "jobs": jobs })).unwrap(),
    )
    .unwrap();
    let queue = Queue::open(grants(dir.path()), Ok(t), &home, || {});
    assert_eq!(queue.list().len(), 499);
    // Two more would make 501: refused whole, nothing added.
    let err = queue
        .add(&add(&["Videos/a.mp4", "Videos/a.mp4"], PresetId::Remux))
        .unwrap_err();
    assert!(err.contains("at most 500"), "{err}");
    assert_eq!(queue.list().len(), 499);
    // A mixed batch: the real clip is queued, the playlist refused by name.
    let mixed = queue
        .add(&add(&["Videos/a.mp4", "Videos/list.mp4"], PresetId::Remux))
        .unwrap();
    assert_eq!(mixed.added.len(), 1);
    assert_eq!(mixed.added[0].path, "Videos/a.mp4");
    assert_eq!(mixed.refused.len(), 1, "{:?}", mixed.refused);
    assert_eq!(mixed.refused[0].path, "Videos/list.mp4");
    assert!(
        mixed.refused[0].reason.contains("playlist"),
        "{:?}",
        mixed.refused
    );
    // Clearing the finished ones makes room again.
    assert_eq!(queue.clear(None), 499);
    assert_eq!(
        queue
            .add(&add(&["Videos/a.mp4", "Videos/a.mp4"], PresetId::Remux))
            .unwrap()
            .added
            .len(),
        2
    );
}

#[test]
fn a_short_file_is_measured_on_one_sample_and_lands_inside_its_band() {
    let Some(t) = tools() else { return };
    let w = world(t.clone());
    clip(&w.videos().join("phone.mp4"), 8, "640x360");
    let e = crate::estimate::estimate(
        &t,
        w.queue.grants(),
        "Videos/phone.mp4",
        PresetId::ShareH264,
        &Options::default(),
        &w.root().join("scratch"),
        &Cancellation::new(),
        |_, _| {},
    )
    .unwrap();
    assert_eq!(e.samples.len(), 1, "{:?}", e.samples);
    assert!((e.samples[0].seconds - 6.0).abs() < 0.01, "{:?}", e.samples);
    assert!(e.gloss.contains("measured on 1 sample of"), "{}", e.gloss);
    w.queue.start();
    let job = w
        .queue
        .add(&add(&["Videos/phone.mp4"], PresetId::ShareH264))
        .unwrap()
        .added[0]
        .clone();
    let done = finished(&w.queue, &job.id);
    let actual = std::fs::metadata(w.root().join(done.output.unwrap()))
        .unwrap()
        .len();
    eprintln!(
        "short estimate: predicted {} ({}..{}), actual {actual}",
        e.predicted_bytes, e.low_bytes, e.high_bytes
    );
    assert!(
        (e.low_bytes..=e.high_bytes).contains(&actual),
        "the output ({actual}) is outside the band the estimate stated"
    );
}

#[test]
fn a_retried_trim_says_its_keyframe_warning_once() {
    let Some(t) = tools() else { return };
    let dir = tempfile::tempdir().unwrap();
    clip(&dir.path().join("Videos/talk.mp4"), 10, "320x240");
    let home = dir.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    let warning = "The cut starts at 0:02.0, the keyframe before 0:03.0.";
    // What an earlier run that failed leaves: the warning it added.
    std::fs::write(
        home.join("queue.json"),
        serde_json::to_vec(&serde_json::json!({
            "next": 1,
            "jobs": [{
                "id": "j1", "path": "Videos/talk.mp4", "preset": "trim",
                "options": { "range": { "start": 3.0, "end": 5.0 } },
                "state": "failed", "error": "it failed", "added": 1,
                "warnings": [warning]
            }]
        }))
        .unwrap(),
    )
    .unwrap();
    let queue = Queue::open(grants(dir.path()), Ok(t), &home, || {});
    queue.start();
    queue.retry("j1").unwrap();
    let done = finished(&queue, "j1");
    assert_eq!(done.state, JobState::Done, "{done:#?}");
    assert_eq!(done.warnings, [warning], "the warning was added again");
}

#[test]
fn a_remembered_estimate_counts_only_for_the_options_it_was_measured_with() {
    let Some(t) = tools() else { return };
    let w = world(t);
    clip(&w.videos().join("a.mp4"), 4, "320x240");
    let ten = Options {
        range: Some(Range {
            start: 0.0,
            end: 1.0,
        }),
        ..Options::default()
    };
    w.queue
        .remember_estimate("Videos/a.mp4", PresetId::ShareH264, &ten, 12_345);
    // The whole file: the measurement of one second is not used.
    let whole = w
        .queue
        .add(&add(&["Videos/a.mp4"], PresetId::ShareH264))
        .unwrap()
        .added[0]
        .clone();
    assert_ne!(whole.estimated_bytes, Some(12_345), "{whole:#?}");
    // The same range: it is.
    let mut same = add(&["Videos/a.mp4"], PresetId::ShareH264);
    same.start = Some(Time::Seconds(0.0));
    same.end = Some(Time::Seconds(1.0));
    let ranged = w.queue.add(&same).unwrap().added[0].clone();
    assert_eq!(ranged.estimated_bytes, Some(12_345));
}

/// Every file under `dir`, relative to it.
fn files_under(dir: &Path) -> Vec<String> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d).into_iter().flatten().flatten() {
            let p = e.path();
            if p.is_dir() {
                out.push(format!("{}/", p.strip_prefix(dir).unwrap().display()));
                stack.push(p);
            } else {
                out.push(p.strip_prefix(dir).unwrap().display().to_string());
            }
        }
    }
    out.sort();
    out
}

#[test]
fn an_estimate_or_still_touches_no_other_calls_scratch_files() {
    // Two calls at once shared fixed names (`sample-0.mp4`, `still.jpg`) and
    // unlinked or read each other's files. A file at those names stands in
    // for the other call's, mid-write.
    let Some(t) = tools() else { return };
    let w = world(t.clone());
    clip(&w.videos().join("walk.mp4"), 14, "320x240");
    let scratch = w.root().join("scratch");
    std::fs::create_dir_all(&scratch).unwrap();
    for name in ["sample-0.mp4", "still.jpg", "still-sample.mp4"] {
        std::fs::write(scratch.join(name), b"another call's").unwrap();
    }
    crate::estimate::estimate(
        &t,
        w.queue.grants(),
        "Videos/walk.mp4",
        PresetId::ShareH264,
        &Options::default(),
        &scratch,
        &Cancellation::new(),
        |_, _| {},
    )
    .unwrap();
    crate::estimate::frame(
        &t,
        w.queue.grants(),
        "Videos/walk.mp4",
        1.0,
        Some(PresetId::ShareH264),
        320,
        &scratch,
        &Cancellation::new(),
    )
    .unwrap();
    assert_eq!(
        files_under(&scratch),
        ["sample-0.mp4", "still-sample.mp4", "still.jpg"],
        "another call's file was removed, or this call's were left"
    );
    for name in ["sample-0.mp4", "still.jpg", "still-sample.mp4"] {
        assert_eq!(
            std::fs::read(scratch.join(name)).unwrap(),
            b"another call's"
        );
    }
}

#[test]
fn two_estimates_at_once_each_measure_their_own_file() {
    let Some(t) = tools() else { return };
    let w = Arc::new(world(t.clone()));
    clip(&w.videos().join("one.mp4"), 14, "320x240");
    clip(&w.videos().join("two.mp4"), 14, "640x360");
    let scratch = w.root().join("scratch");
    let run = |name: &'static str| {
        let (t, w, scratch) = (t.clone(), Arc::clone(&w), scratch.clone());
        std::thread::spawn(move || {
            crate::estimate::estimate(
                &t,
                w.queue.grants(),
                name,
                PresetId::ShareH264,
                &Options::default(),
                &scratch,
                &Cancellation::new(),
                |_, _| {},
            )
        })
    };
    let (a, b) = (run("Videos/one.mp4"), run("Videos/two.mp4"));
    let (a, b) = (a.join().unwrap().unwrap(), b.join().unwrap().unwrap());
    assert!(a.samples.iter().all(|s| s.bytes > 0), "{:?}", a.samples);
    assert!(b.samples.iter().all(|s| s.bytes > 0), "{:?}", b.samples);
    assert!(
        b.predicted_bytes > a.predicted_bytes,
        "the larger picture measured smaller: {} vs {}",
        b.predicted_bytes,
        a.predicted_bytes
    );
    assert!(
        files_under(&scratch).is_empty(),
        "{:?}",
        files_under(&scratch)
    );
}

#[test]
fn a_cancelled_estimate_leaves_nothing_in_scratch() {
    let Some(t) = tools() else { return };
    let w = world(t.clone());
    clip(&w.videos().join("long.mp4"), 30, "1280x720");
    let scratch = w.root().join("scratch");
    let cancel = Cancellation::new();
    let c = cancel.clone();
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(400));
        c.cancel();
    });
    let err = crate::estimate::estimate(
        &t,
        w.queue.grants(),
        "Videos/long.mp4",
        PresetId::SmallAv1,
        &Options::default(),
        &scratch,
        &cancel,
        |_, _| {},
    )
    .unwrap_err();
    assert!(err.contains("cancelled"), "{err}");
    assert!(
        files_under(&scratch).is_empty(),
        "{:?}",
        files_under(&scratch)
    );
}

/// The commands as the core reaches them: frames over a socket pair, through
/// the SDK's own frame loop, with this test standing in for the core.
mod dispatch {
    use super::*;
    use deckjack_protocol::{AppFrame, CoreFrame, PROTOCOL_VERSION, decode_frame, encode_frame};
    use serde_json::{Value, json};
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::UnixStream;

    struct Core {
        reader: BufReader<tokio::net::unix::OwnedReadHalf>,
        writer: tokio::net::unix::OwnedWriteHalf,
        /// Frames read while waiting for another call's answer.
        heard: Vec<AppFrame>,
    }

    impl Core {
        async fn send(&mut self, frame: &CoreFrame) {
            self.writer
                .write_all(encode_frame(frame).unwrap().as_bytes())
                .await
                .unwrap();
        }

        async fn recv(&mut self, limit: Duration) -> AppFrame {
            let mut line = String::new();
            tokio::time::timeout(limit, self.reader.read_line(&mut line))
                .await
                .expect("the app sent nothing in time")
                .unwrap();
            decode_frame(line.trim_end()).unwrap()
        }

        async fn call(&mut self, id: u64, command: &str, params: Value) {
            self.send(&CoreFrame::Call {
                id,
                command: command.into(),
                params,
            })
            .await;
        }

        /// The answer to call `id`; everything else read meanwhile is kept.
        async fn answer(&mut self, id: u64, limit: Duration) -> Result<Value, String> {
            let deadline = Instant::now() + limit;
            loop {
                let left = deadline.saturating_duration_since(Instant::now());
                match self.recv(left).await {
                    AppFrame::Result {
                        id: got,
                        result,
                        error,
                    } if got == id => {
                        return match error {
                            Some(e) => Err(e),
                            None => Ok(result.unwrap_or(Value::Null)),
                        };
                    }
                    other => self.heard.push(other),
                }
            }
        }

        fn progress_of(&self, id: u64) -> Vec<String> {
            self.heard
                .iter()
                .filter_map(|f| match f {
                    AppFrame::Progress {
                        id: got, message, ..
                    } if *got == id => Some(message.clone()),
                    _ => None,
                })
                .collect()
        }
    }

    /// Convert serving on a socket pair, over the world `w`.
    async fn connect(w: &World) -> Core {
        let app = deckjack_app::App::new(crate::MANIFEST).unwrap();
        let shared = Arc::new(crate::window::Shared {
            queue: Arc::clone(&w.queue),
            notifier: app.notifier(),
            scratch: w.root().join("scratch"),
            estimates: Default::default(),
            gate: Default::default(),
        });
        let s = Arc::clone(&shared);
        let app = app.view(move || crate::window::Convert::open(Arc::clone(&s)));
        let app = crate::with_commands(app, shared);
        let (core, app_end) = UnixStream::pair().unwrap();
        tokio::spawn(app.serve_connected(app_end));
        let (read, writer) = core.into_split();
        let mut core = Core {
            reader: BufReader::new(read),
            writer,
            heard: Vec::new(),
        };
        core.send(&CoreFrame::Hello(deckjack_protocol::Hello {
            protocol: PROTOCOL_VERSION,
            core_version: "test".into(),
            app: "convert".into(),
        }))
        .await;
        assert!(matches!(
            core.recv(Duration::from_secs(5)).await,
            AppFrame::Register(_)
        ));
        core
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn status_and_settings_answer_through_the_frame_loop() {
        let Some(t) = tools() else { return };
        let w = world(t);
        let mut core = connect(&w).await;
        core.call(1, "convert.status", json!({})).await;
        let status = core.answer(1, Duration::from_secs(20)).await.unwrap();
        assert_eq!(status["found"], true, "{status}");
        let granted: Vec<&str> = status["granted"]
            .as_array()
            .unwrap()
            .iter()
            .map(|g| g["path"].as_str().unwrap())
            .collect();
        assert_eq!(granted, ["Videos", "Music"]);
        assert_eq!(status["encoders"]["libx264"], true, "{status}");

        core.call(2, "convert.settings", json!({ "two_at_once": true }))
            .await;
        let set = core.answer(2, Duration::from_secs(5)).await.unwrap();
        assert_eq!(set["parallel"], 2);
        assert_eq!(w.queue.parallel(), 2);
        core.call(3, "convert.settings", json!({ "parallel": 3 }))
            .await;
        let refused = core.answer(3, Duration::from_secs(5)).await.unwrap_err();
        assert!(refused.contains("1 or 2"), "{refused}");
        core.call(4, "convert.settings", json!({ "parallel": 1 }))
            .await;
        assert_eq!(
            core.answer(4, Duration::from_secs(5)).await.unwrap()["parallel"],
            1
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_watch_answers_at_once_for_a_finished_job_and_at_its_limit_for_a_running_one() {
        let Some(t) = tools() else { return };
        let w = world(t);
        clip(&w.videos().join("short.mp4"), 1, "160x120");
        clip(&w.videos().join("long.mp4"), 40, "1280x720");
        w.queue.start();
        let short = w
            .queue
            .add(&add(&["Videos/short.mp4"], PresetId::Remux))
            .unwrap()
            .added[0]
            .clone();
        finished(&w.queue, &short.id);
        let mut core = connect(&w).await;

        let asked = Instant::now();
        core.call(1, "convert.job.watch", json!({ "id": short.id }))
            .await;
        let done = core.answer(1, Duration::from_secs(5)).await.unwrap();
        assert_eq!(done["still_running"], false, "{done}");
        assert_eq!(done["job"]["state"], "done");
        assert!(asked.elapsed() < Duration::from_secs(2));

        let long = w
            .queue
            .add(&add(&["Videos/long.mp4"], PresetId::SmallAv1))
            .unwrap()
            .added[0]
            .clone();
        wait(&w.queue, &long.id, Duration::from_secs(30), |j| {
            j.state == JobState::Running
        });
        let asked = Instant::now();
        core.call(
            2,
            "convert.job.watch",
            json!({ "id": long.id, "max_secs": 1 }),
        )
        .await;
        let running = core.answer(2, Duration::from_secs(10)).await.unwrap();
        let took = asked.elapsed();
        assert_eq!(running["still_running"], true, "{running}");
        assert!(
            took >= Duration::from_millis(900) && took < Duration::from_secs(4),
            "{took:?}"
        );
        assert!(
            !core.progress_of(2).is_empty(),
            "the watch sent no progress"
        );

        // A cap far past the most a watch holds is cut to it: the answer
        // comes when the job ends, not later.
        core.call(3, "convert.job.cancel", json!({ "id": long.id }))
            .await;
        core.answer(3, Duration::from_secs(5)).await.unwrap();
        core.call(
            4,
            "convert.job.watch",
            json!({ "id": long.id, "max_secs": 100000 }),
        )
        .await;
        let ended = core.answer(4, Duration::from_secs(20)).await.unwrap();
        assert_eq!(ended["still_running"], false, "{ended}");
        assert_eq!(ended["job"]["state"], "cancelled");

        core.call(5, "convert.job.watch", json!({ "id": "j999" }))
            .await;
        let missing = core.answer(5, Duration::from_secs(5)).await.unwrap_err();
        assert!(missing.contains("no job j999"), "{missing}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn two_estimates_sent_together_take_turns() {
        let Some(t) = tools() else { return };
        let w = world(t);
        clip(&w.videos().join("one.mp4"), 30, "1280x720");
        clip(&w.videos().join("two.mp4"), 30, "320x240");
        let mut core = connect(&w).await;
        core.call(
            1,
            "convert.estimate",
            json!({ "path": "Videos/one.mp4", "preset": "share-h264" }),
        )
        .await;
        // The first is under way before the second arrives.
        loop {
            if let AppFrame::Progress { id: 1, .. } = core.recv(Duration::from_secs(20)).await {
                break;
            }
        }
        core.call(
            2,
            "convert.estimate",
            json!({ "path": "Videos/two.mp4", "preset": "share-h264" }),
        )
        .await;
        let first = core.answer(1, Duration::from_secs(120)).await.unwrap();
        let second = core.answer(2, Duration::from_secs(120)).await.unwrap();
        assert_eq!(first["path"], "Videos/one.mp4");
        assert_eq!(second["path"], "Videos/two.mp4");
        let said = core.progress_of(2);
        assert!(
            said.first()
                .is_some_and(|m| m.starts_with("Waiting for another measurement")),
            "{said:?}"
        );
    }
}
