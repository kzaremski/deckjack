# Lane 5c — Converter and compressor (`deckjack-convert`)

Scoping brief, written to be handed to a lane agent whole, in the shape of
APP-SUITE Lanes 1–3. Read APP-SUITE §2, §3, §11 and §12 first; this brief does
not repeat them. Everything marked **verified** was checked on this machine on
2026-11-16; **inferred** means reasoned from code or docs and not run.

---

## Goal

Turn a video or audio file the operator already has into one that is smaller,
plays everywhere, or is in an open format. Do it as a queued job on a node,
from the phone, without the operator having to know ffmpeg flags, and tell the
operator **before** they commit what each choice costs in quality and size.
That cost is measured on their own file, not claimed in general.

This is also the **confined ffmpeg helper** that Lane 1 says is missing:
"running ffmpeg on an untrusted file in the unconfined core is the wrong
place for it" (`thumbnail.rs:41`, APP-SUITE Lane 1, second pass). Once this
app exists, video thumbnails for Files have a place to live (see Risks, R7).
They are not part of this lane's first version.

## The useful subset (v1)

**In:**

1. **Probe**: container, streams, codecs, duration, resolution, frame rate,
   bitrate, audio layout. `ffprobe -of json` (verified: JSON output with
   `streams[].codec_name`).
2. **Six built-in presets**, each a fixed, versioned argument list:

   | id | what | output | quality cost (the sentence the UI shows) |
   |---|---|---|---|
   | `remux` | stream copy into another container (MKV↔MP4, MOV→MP4) | same codecs | **None.** Bytes are copied, not re-encoded. Refused, with the reason, when the target container cannot carry a source codec. |
   | `trim` | cut a range by stream copy | same codecs | **None**, but the cut lands on the nearest keyframe **before** the start you chose; the UI shows the real start. |
   | `share-h264` | H.264 + AAC in MP4, `+faststart`, at most 1080p | `.mp4` | Lossy re-encode. Measured on a sample before you commit (below). Plays on everything made in the last fifteen years. |
   | `small-av1` | AV1 (SVT-AV1) + Opus | `.mkv` (or `.webm`) | Lossy, typically much smaller at a similar score. Measured on a sample. **Slow** on a laptop CPU; the estimate states the predicted wall-clock time. Older devices may not play it. |
   | `audio-opus` | extract or convert audio to Opus | `.opus` | Lossy. If the source is already lossy, this is a second generation of loss, and the UI says so. |
   | `audio-flac` | lossless audio | `.flac` | **None** from a lossless source. From a lossy source it is refused-by-default with "this makes the file bigger and no better", overridable. |

   Starting encoder values (CRF, SVT-AV1 preset, Opus bitrate) are set by the
   estimate spike (S2), not guessed here.
3. **Estimate** before commit: encode three short samples (start, middle,
   near end) with the chosen preset, then report measured bitrate
   extrapolated to a predicted output size, encode speed and predicted time,
   and a **VMAF** score (SSIM/XPSNR where VMAF is absent) of the samples
   against the source. Lossless presets skip the metric and say "no loss".
   Verified locally: `libvmaf` is in this machine's ffmpeg n9.0.2 and scored
   a test encode at 96.97; `ssim`, `psnr`, `xpsnr` filters are present too.
4. **Still comparison**: one frame from the source and the same frame from a
   sample encode, side by side, pinch-zoomable. This is how "what will it
   look like" is answered without a video player in the frame (see the
   UI-isolation section for why).
5. **Batch queue**: add one file or many with one preset, see the queue,
   cancel a queued or running job, retry a failed one, clear finished ones.
   One job runs at a time by default (setting: 1 or 2).
6. **Jobs survive the phone**: locking the phone, losing the socket, or
   closing the window does not stop a job. Jobs are app-owned state, not a
   long-lived dispatch (see Backend commands for why that is load-bearing).
7. **Honest output handling**: never overwrites (`-n` plus a
   `RENAME_NOREPLACE` publish), writes under a hidden partial name in the
   destination folder and renames when ffmpeg exits 0, names outputs
   `name (Share).mp4` with the "name copy 2" rule `files.copy` already uses
   for collisions, checks free space against the estimate before starting,
   and leaves the source untouched.

**Explicitly not in v1:**

- Raw ffmpeg arguments from the UI or an agent. Ever, not just in v1: the
  argument list is built by the backend from typed params (Risks, R4).
- Custom user presets. (They are a natural v2 as TOML files; see Formats.)
- "Replace the original". It would be `destructive` and needs the trash,
  which lives at the working root outside this app's grant.
- Image conversion. That is 5a's, with the `image` crate.
- Filters beyond scale and trim: no crop, rotate, deinterlace, denoise,
  subtitles burn-in, loudness normalization (`loudnorm` exists in this build;
  it is the first v2 candidate because it is a two-pass job that fits the
  queue).
- Hardware encoding (VA-API, MediaCodec). The probe reports it; presets do
  not use it until S1 shows it works confined. It is also lower quality per
  bit than x264/SVT-AV1 at the same size, which the estimate would show.
- In-frame video playback or A/B scrubbing. Hand-off to Quick Look instead.
- Remote transcoding (file on one node, encoder on another). There is no
  cross-node file transport (Lane 1, still owed). A job runs on the node that
  holds the file.
- HDR tone-mapping, multi-audio-track selection UI, chapter editing. v1 maps
  the first video and first audio stream plus a "keep all audio tracks"
  toggle for `remux`.

## Engine choice

**The `ffmpeg` and `ffprobe` binaries, exec'd as confined children.** Per
decision 1's flagged exception: these are batch jobs that produce a file, and
nothing is scrubbed or edited in-process.

Why the binary, concretely, in this platform:

- **The sandbox already permits it.** `/usr` is read-granted with `Execute`
  (`apps.rs`, `SYSTEM_READ_DIRS`), fork/clone are not denied, and a child
  inherits Landlock and seccomp (`sandbox.rs`, `denied_syscalls` doc). The
  child is confined to exactly the app's grants. Inferred from code; S1 runs
  it for real.
- **Crash isolation.** ffmpeg decoding an untrusted file is a large attack
  surface. A decoder crash kills a child, the job is marked failed, and the
  app survives.
- **Battle-tested argument surface and progress protocol.** `-progress pipe:1
  -nostats -nostdin` emits `key=value` blocks ending in
  `progress=continue|end`, every `-stats_period` (default 0.5 s)
  ([ffmpeg docs](https://ffmpeg.org/ffmpeg.html)). Verified locally; note
  that `out_time_ms` carries **microseconds** (it printed `1933333` for
  1.93 s, identical to `out_time_us`), so parse `out_time_us`.
- **Distro builds carry the encoders we want.** Arch `extra/ffmpeg`
  2:9.0.2 is GPL-3.0 and depends on x264, x265, svt-av1, opus, vmaf, libva
  and the Vulkan loader ([Arch package](https://archlinux.org/packages/extra/x86_64/ffmpeg/)).
  Termux's ffmpeg is 9.0.2, GPL-3.0, with `--enable-mediacodec`, x264, x265,
  svt-av1, opus and libvmaf ([termux-packages build.sh](https://github.com/termux/termux-packages/blob/master/packages/ffmpeg/build.sh)).
  Current upstream is 9.0.2 "Lei" (2026-09-18), with 8.1.3 maintained
  ([ffmpeg.org/download](https://ffmpeg.org/download.html)).
- **Licensing stays simple.** We link nothing from FFmpeg. The project's
  legal page covers linking and distribution; it says nothing about invoking
  the binary as a separate program
  ([ffmpeg.org/legal](https://ffmpeg.org/legal.html)). Treating a separate
  exec'd program as aggregation follows the usual reading of the GPL FAQ.
  That is inferred, not legal advice; it matters only if Deckjack ever
  **bundles** an ffmpeg build (then it owes the source and config, per the
  legal page).

Alternatives considered:

| option | status (Nov 2026) | why not for v1 |
|---|---|---|
| `ffmpeg-next` (Rust bindings) | maintenance mode, FFmpeg 3.4–8.0 ([docs.rs 9.0.0](https://docs.rs/crate/ffmpeg-next/9.0.0)) | Puts the decoder attack surface in the app's address space, links GPL libs into our binary, and lags the system ffmpeg (9.0) by a major. |
| `ffmpeg-the-third` | 5.0.0 targets FFmpeg 8.1 ([docs.rs](https://docs.rs/crate/ffmpeg-the-third/latest)) | Same objections; best candidate if 5d ever needs in-process decode. |
| `rsmpeg` | 0.18, FFmpeg 6–8 ([docs.rs](https://docs.rs/crate/rsmpeg/latest/features)) | Same. |
| GStreamer | mature | A second media stack beside the ffmpeg 5d will use anyway; heavier pipeline model for no gain in a batch job. |
| Pure-Rust encoders (`rav1e`, `symphonia` decode only) | rav1e is slower than SVT-AV1; symphonia does not encode | No complete pipeline. |
| WebCodecs in the browser | shipping | Encodes on the phone's battery in a tab that must stay open; contradicts "jobs survive the phone". |
| Bundled static ffmpeg in the app dir | possible: `installed.dir` is read-granted, so it could exec | 80–130 MB, GPL source obligations, and our own CVE patching. Kept as the fallback for nodes with no system ffmpeg (Termux, see R6). |

The libraries are used **only** if a later version wants in-process preview
decoding. v1 does not: its previews are still frames that ffmpeg itself
extracts.

## Backend commands

All ids are in the app's own namespace `convert.*` (§11; also its own scope
under APP-UI-ISOLATION §4.6, so its UI needs no `[ui] invoke`). Params are
typed JSON Schema; paths are relative to the working directory and must fall
inside the app's grant (Landlock enforces it regardless, but the error should
be a sentence, not `EACCES`).

| id | risk | params → result | progress |
|---|---|---|---|
| `convert.status` | read | → ffmpeg/ffprobe found, version, encoders present (`libx264`, `libsvtav1`, `libopus`, `flac`), `libvmaf` present, hwaccels listed, granted folders | — |
| `convert.presets` | read | → the preset table: id, version, title, output extension, the quality sentence, and the **exact argument template** | — |
| `convert.browse` | read | `{dir}` → media files in a granted folder (the app cannot call `files.list`; it reads its own grant directly) | — |
| `convert.probe` | read | `{path}` → streams, duration, size, bitrate | — |
| `convert.estimate` | read | `{path, preset, range?}` → predicted size, predicted time, speed, metric name and score per sample, warnings ("source is already lossy") | yes: per sample encode and the metric pass |
| `convert.frame` | read | `{path, at, preset?, max_edge≤1280}` → one JPEG, base64, ≤512 KiB (well under the broker's 1 MiB param and socket frame caps); with `preset`, the frame comes from a sample encode | yes, when it encodes |
| `convert.job.add` | write | `{paths[≤100], preset, range?, dest?: "beside" \| "<granted dir>", allow_lossy_to_lossless?}` → job ids. Never overwrites; refuses up front if the estimated total does not fit free space | — |
| `convert.job.list` | read | `{since?}` → jobs: state (queued, running, done, failed, cancelled), fraction, speed, ETA, output path, error sentence | — |
| `convert.job.watch` | read | `{id, max_secs≤50}` → final state, or `still_running` at the deadline. Reports the job's progress through `Ctx::progress` while held | **yes, this is where job progress flows** |
| `convert.job.cancel` | write | `{id}` → kills the child (SIGTERM, then SIGKILL after 5 s), deletes only its own partial file | — |
| `convert.job.retry` | write | `{id}` → re-queues a failed or cancelled job | — |
| `convert.job.clear` | write | `{ids? or "finished"}` → forgets finished jobs. Outputs are untouched | — |

**No `destructive` or `outbound` command in v1.** `job.add` is `write`
because its effect is a new file the operator can delete; it never replaces
anything. Agents can queue jobs without the Inbox, which is intended (DESIGN:
every command is an agent tool), and bounded by a 100-path batch cap and a
queue cap of 500.

**Why jobs are app-owned and progress is a held "watch":** three facts from
the code shape this.

1. A socket that closes cancels its dispatches (`ws.rs`, `CancelSignal`;
   "a connection that slept"). A phone locking would kill a job modeled as
   one long dispatch.
2. The app SDK **ignores `CoreFrame::Cancel`** (`deckjack-app/src/lib.rs`,
   `TODO(cancel)`). So a long dispatch also could not stop its ffmpeg child
   when cancelled; it would keep encoding with nobody listening.
3. `Ctx::progress` exists only inside a dispatch. So progress reaches the UI
   through a short-lived `job.watch` that the UI re-issues, and cancellation
   goes through an explicit `job.cancel` that this app fully controls. When
   the SDK gains cancel delivery, `job.watch` simply ends sooner; nothing
   else changes.

The queue is stored as JSON in the app's private home
(`cache/apps/convert/home/queue.json`, the `HOME` the launcher sets), written
atomically on every state change. Losing it loses only the list of queued
jobs; outputs are files. On start, a job found `running` becomes `failed:
interrupted` with Retry offered, and the app deletes leftover partials that
match its own naming pattern (job id in the name) and nothing else.

ffmpeg is run with `nice 10` (setpriority is not denied by the seccomp list)
and a thread cap, because neither sandbox layer limits CPU (`sandbox.rs`
module doc) and a GPD Pocket 4 at full load for an hour is hot and loud.

## UI and touch contract

Plain HTML/CSS/JS, no build step, `/kit/bus.js` and `/kit/kit.css`,
`/kit/selection.js` for multi-select (Lane 1). Inline boot script in a file
from day one (APP-UI-ISOLATION D4).

**Three views:**

1. **Pick**: the granted folders as a list with filter; tap toggles
   selection in selection mode, long-press enters it (same as Files). Also
   opened with `?path=` from Files' "Open with…" (the existing
   `shell.app.open` contract; one path only today, see R8).
2. **Choose**: preset cards, each with its quality sentence. Tapping a lossy
   preset runs `convert.estimate` and shows predicted size (and the
   percentage of the source), predicted time, and the score with a plain
   gloss ("VMAF 95: hard to tell apart at normal viewing distance" is the
   kind of sentence; final wording after S2, and it must say it is a sample
   measurement). A **Compare** button shows the still comparison. **Show
   command** reveals the exact ffmpeg line for the first file.
3. **Queue**: one row per job with a progress bar, ETA and state; Cancel and
   Retry as buttons at least `--dj-touch`; a finished row's tap opens the
   output in Quick Look (`shell.quicklook.open`); "Show in Files" uses
   `shell.app.open`.

**Touch contract:** selection is a mode, long-press enters it; no hover-only
affordance; every action is a button at least `--dj-touch`; the comparison
view uses two-finger pinch and pan with the two stills locked together, and a
divider dragged with one finger; no swipe gestures that fight the shell's.
Progress is readable at arm's length (bar plus "12 min left").

**Minimum width, honestly:** **320 px** works as one column with the three
views as steps. The side-by-side still comparison becomes top-and-bottom
below 600 px, which is worse for spotting artifacts but usable. Two-pane
(queue beside choose) from 720 px. Manifest: `min_width = 320`,
`default_width = 720`, `default_height = 560`.

**Phone reality:** the UI works on the phone today because it is web. The
**backend** cannot run on the phone yet: Lane 0.5 has no Android
cross-compile, and whether Android kernels enable Landlock is unverified (a
search of the GKI config fragments did not settle it). So "from the phone"
means the phone drives a job on a Linux node. That is the right place for a
transcode anyway.

## How it fits the sandbox

**Manifest `[needs]` it would declare:**

```toml
[app]
name = "convert"
title = "Convert"
lifecycle = "service"    # see R3: on-demand apps are idle-stopped mid-job
min_width = 320
tag = "Cv"
summary = "Convert and compress video and audio"

[needs]
working_dir = [
  { path = "Videos", write = true },
  { path = "Music",  write = true },
]
# gpu: not in v1. Add `gpu = true` only after S1 shows VA-API works confined.
# network, connect_tcp, bind_tcp: none. ffmpeg never needs the network here.
# invoke_commands: not needed. It calls nothing on the bus.
```

Notes on each:

- **`working_dir` is static.** The grant is fixed at approval, so this app
  can only see the folders it names. A grant of `.` is refused whenever the
  working directory contains `.deckjack/` (`apps.rs`: "a grant containing it
  would reach state.db"), so "convert anything anywhere" is not expressible,
  and that is correct. Folder names are a convention to settle (R5); a write
  grant creates the folder if missing (`apps.rs`, `create_private_dir`).
- **Helper binaries:** `/usr/bin/ffmpeg` and `/usr/bin/ffprobe`, reached
  through the `/usr` read-and-execute grant. Located by absolute path from a
  short fixed list, never through `PATH` from the environment (the launcher
  clears it anyway). Missing binaries are reported by `convert.status` with
  the install command for the node's OS.
- **Scratch:** sample encodes and frame extraction go to the launcher's
  `TMPDIR` (`cache/apps/convert/tmp`). APP-SUITE §11 says caches live under
  `~/.deckjack/cache/<app>/`; the launcher actually grants
  `cache/apps/<app>/{home,tmp}`. This lane uses what the launcher grants and
  flags the doc mismatch.
- **GPU (later):** VA-API needs `/dev/dri` (granted by `gpu = true`) and
  reads `/sys` (granted). libva/libdrm may also want `/run/udev/data`, which
  is not granted (`/run` is never granted whole). Unknown until S1.
- **Network:** none, and the arguments enforce it in depth: every input and
  output is passed as `file:<absolute path>`, and every invocation carries
  `-protocol_whitelist file,pipe`. With no network grant, Landlock already
  refuses TCP; UDP is not filtered by Landlock at all (Lane 0.6), which is
  one more reason for the whitelist.
- **CPU and memory:** unconstrained by the sandbox; mitigated by `nice`,
  thread caps and one job at a time.

**UI-isolation design (after APP-UI-ISOLATION stage 4):**

- **What the broker must carry:** `invoke`, `cancel` and **progress events**
  for `convert.*` (own scope: no `[ui] invoke` needed); `shell` ops
  `shell.quicklook.open` (output and source paths, both inside the app's file
  scope, so the §4.5 path check passes) and `shell.app.open` (Files, at the
  output's folder). Nothing else.
- **Stills reach the frame as base64 → `blob:` images.** The frame CSP allows
  `img-src blob:`, and `connect-src 'none'` forbids fetching `/content`, so
  the frame can never stream video itself. That is why v1 compares stills
  and hands playback to Quick Look, which runs in the shell with the content
  route. This is a design choice forced by D6 (no media streaming through
  the broker), and it is the right one for v1.
- **D6 items it would want, and the workaround in v1:**
  - *Downloads* (save the output to an iPhone): not through the broker. The
    output opens in Quick Look, which is shell-side and can offer the
    download from the shell's own origin. If Quick Look has no download
    button, that is a Lane 4 item, not a broker convenience.
  - *Media streaming* (play the sample in the app): deferred with D6; Quick
    Look instead.
  - *Per-app storage*: not needed. The queue is backend state, and UI
    preferences use `localStorage` guarded by try/catch. It throws in an
    opaque origin, and the UI must default cleanly.
- **The open WebRTC leak (APP-UI-ISOLATION §7.2.3), stated for this app:**
  a hostile Convert UI can read stills of any video in `Videos/` and `Music/`
  through `convert.frame` and send them out over a WebRTC data channel.
  Nothing in this lane closes that; the grant is the limit. That is a
  reason to keep the grant to media folders and not widen it.

## Formats and longevity

- **Outputs are standard files in standard containers:** MP4 (H.264/AAC),
  MKV or WebM (AV1/Opus), Ogg Opus, FLAC. All play without Deckjack. AV1,
  Opus, FLAC, MKV and WebM are royalty-free open formats. H.264/AAC are
  patent-encumbered but universal, and are offered for compatibility, labeled
  as such.
- **No project format, no database.** The queue is transient state in the
  app's home. Losing it loses nothing the operator made.
- **Reproducibility is the sidecar:** every preset's exact argument template
  is visible ("Show command") and versioned (`share-h264@1`). The output
  carries a container comment `deckjack-convert share-h264@1`, and ffmpeg
  already writes its own `encoder` tag. Someone without Deckjack can
  re-create the job from the command line. No per-file JSON sidecar in v1:
  a `.json` beside every video is clutter nobody asked for.
- **v2 custom presets** would be TOML files in a granted folder (for example
  `Videos/.presets/*.toml`) holding a preset id, a typed parameter set (codec,
  quality, scale, audio bitrate) and no free-form arguments. That is
  readable, portable, and still never raw ffmpeg flags.
- **What survives:** everything the operator keeps is a file. Metadata
  (titles, creation time, rotation) is copied with `-map_metadata 0`; chapters
  with `-map_chapters 0`. Container limits are reported: for example, MP4
  cannot carry some subtitle codecs, and the job says "subtitles dropped:
  MP4 cannot hold ASS" instead of dropping them silently.

## Risks and open questions

Each with the spike that settles it.

- **R1 — ffmpeg under the real launcher (S1, half a day).** Inferred, not
  run: ffmpeg/ffprobe exec and work under the app's Landlock+seccomp policy.
  Spike: a test in the style of `tests/sandbox.rs` that plans and spawns a
  stub app through `OsEnv` and has it run `ffprobe` and a 2 s x264 encode on
  a granted file, then asserts that an ungranted file is refused with EACCES,
  that `-i http://…` fails, and that `/dev/dri/renderD128` is or is not
  openable with and without `gpu = true`. Extend it to `h264_vaapi` to learn
  whether `/run/udev` is needed. This machine lists `vaapi` and `h264_vaapi`
  (verified); confined behavior is unknown.
- **R2 — Estimate accuracy and cost (S2, one day).** Do three 2-second
  samples predict final size within ±25% and give a VMAF close to a
  full-file VMAF? How long does VMAF take on the Pocket 4 versus the R620?
  Spike: a corpus of 10 real files (phone video, screen recording, film
  grain, static talking head, audio-only), run each preset fully and by
  sampling, and tabulate. This also sets the starting CRF/preset values.
  If sampling misleads on grainy or variable content, the UI must widen its
  stated error band, not hide it.
- **R3 — Idle stop kills long jobs (S3, an hour to confirm).** The host
  stops an `on-demand` app with no windows open once `last_active` is 300 s
  old (`apps.rs`, `Host::poll`, `idle_secs: 300`). `note_activity` runs at
  dispatch, not while a call or child runs. So a job outlives its window by
  about five minutes. v1 therefore declares `lifecycle = "service"`, which
  also gets crash restarts. The platform fix is better and small: count an
  app with work in progress as active, through an app frame or by noting
  activity on `Progress` frames. Spike: confirm with an on-demand stub that
  sleeps 400 s after its window closes. Decide service versus the platform
  fix before writing the manifest, because changing `lifecycle` later
  changes nothing in approval but changes boot cost.
- **R4 — Hostile input files and argument injection (S4, one day).**
  ffmpeg's demuxers can reference other files: HLS playlists, `concat`,
  image2 `%d` patterns in names, and playlist formats have had local-file
  disclosure bugs before. Landlock bounds the damage to the grant, and the
  only exfiltration path for the app process is the output file (no network).
  Mitigations to test: `file:` prefixes, `-protocol_whitelist file,pipe`, an
  allowlist of input formats by probe result (refuse `hls`, `concat`,
  `ffconcat`, `image2` sequence, `tee`), explicit `-f` for output with
  `-pattern_type none` where relevant, filenames starting with `-` and
  containing `%` or `|`, and never any free-form argument. Spike: a fixture
  set of crafted playlist and concat files that try to read a file outside
  the grant and inside it, with each asserted refused before ffmpeg runs.
- **R5 — Which folders (decision, not a spike).** `Videos` and `Music` are a
  guess at the operator's layout. A wrong guess means an app that cannot
  see the files. Options: ask at install time (needs approval UI work), ship
  the conventional names, or one `Media` folder. The operator decides;
  re-deciding later costs one re-approval.
- **R6 — Nodes without system ffmpeg (S5, half a day).** Termux's prefix
  (`/data/data/com.termux/files/usr`) is not in `SYSTEM_READ_DIRS`, so
  Termux's ffmpeg cannot be exec'd by a confined app even once Lane 0.5
  lands; and Landlock on Android is unverified. Spike, deferred until 0.5:
  check `CONFIG_SECURITY_LANDLOCK` on the Fold's kernel and decide between
  granting the Termux prefix or bundling a static ffmpeg in the app dir (GPL
  obligations apply then).
- **R7 — Files' video thumbnails via this app (later, not v1).** The cleanest
  route is for the core's `files.thumbnail` to call `convert.frame` on video,
  which keeps ffmpeg out of the core. It needs a core→app call path for a
  core command, and Convert's grant must cover the folder. Scope it as a
  follow-on once R1 passes.
- **R8 — Batch hand-off from Files.** `shell.app.open` carries one `?path=`.
  A multi-select "Convert…" from Files needs a list, and APP-UI-ISOLATION
  says the opened app must treat it as inert data until a user gesture.
  v1 picks the batch inside Convert. The list form belongs to the
  open-app contract owner (Lanes 1/2).
- **R9 — No app events.** DESIGN wants "export finished" events so agents
  react instead of polling. There is no app→core event frame in the
  protocol today. Agents poll `convert.job.list`, or hold `job.watch`.
  An event frame is a platform item, not this lane's.
- **R10 — Placement.** The capability registry's `codecs` probe is a TODO
  (`registry.rs`, "an `ffmpeg -codecs` parse"). `convert.status` produces
  exactly that data; wiring it into placement is a follow-on.

## Exit check

From the phone, with the backend on a Linux node:

1. Select ten phone videos in `Videos/`, choose `share-h264`, read the
   estimate (size, time, score) and start the batch.
2. Lock the phone for twenty minutes. Unlock: the queue shows progress where
   it is, without a reload.
3. Cancel one running job: no partial file remains, and the next job starts.
4. Every finished output plays and seeks in Quick Look. Each predicted size
   was within the error band the UI stated.
5. Sources are byte-identical before and after (hashed). Nothing was
   overwritten: a second identical batch produces `(Share) 2` names.
6. `remux` of an MKV to MP4 takes seconds and reports "no quality change".
   `audio-flac` on an MP3 is refused with the reason unless overridden.
7. Under test: a crafted HLS/concat input is refused before ffmpeg runs; a
   killed ffmpeg marks the job failed with a sentence; an app restart
   mid-job leaves an `interrupted` job with Retry and no orphan partial.

Per §12, each refusal is paired with a control that succeeds, and each test is
mutation-checked: name the one-line change that makes it fail, and watch it
fail.

## Rough size and difficulty

- **Backend (Rust, `deckjack-convert`):** ~1.8k lines. Argument builder and
  preset table (~300), progress parser (~150), job queue with persistence,
  cancel and restart recovery (~500), estimate and metric runner (~350),
  probe, browse and frame (~250), app wiring (~250).
- **UI (plain JS/CSS):** ~1.1k lines. Three views, the comparison viewer
  with locked pinch-zoom (the fiddliest part), and the queue with watch
  re-issue.
- **Tests:** ~1.5k lines Rust plus ~500 JS, including the real-kernel
  sandbox test (S1) and the hostile-input fixtures (S4).
- **Platform asks, outside the lane:** idle-stop awareness of running work
  (R3, ~50 lines in `apps.rs` and `apphost.rs`); SDK cancel delivery
  (already a TODO; not blocking for this design); an app event frame (R9,
  not blocking).

**Honest difficulty: moderate.** The ffmpeg part is the easy part; the
binary does the hard work. The real work is (a) making jobs robust to
sockets, idle stops and restarts, (b) making the quality estimate truthful
rather than decorative, which S2 decides, and (c) treating every input file
as hostile, which S4 decides. Roughly two to three focused weeks for one
lane agent including spikes, assuming APP-UI-ISOLATION stage 4 either lands
first or the UI is written to the broker's rules from the start (no
`contentDocument` theming, no inline script, no direct fetches).

---

### Sources

- FFmpeg releases (9.0.2 "Lei", 8.1.3): https://ffmpeg.org/download.html
- FFmpeg 8.1 release coverage: https://phoronix.com/news/ffmpeg-8.1-released
- FFmpeg legal / LGPL-GPL notes: https://ffmpeg.org/legal.html
- ffmpeg `-progress`, `-stats_period`, `-nostdin`: https://ffmpeg.org/ffmpeg.html
- Arch `ffmpeg` package (9.0.2, GPL-3.0, deps): https://archlinux.org/packages/extra/x86_64/ffmpeg/
- Termux ffmpeg build (9.0.2, mediacodec, svt-av1, libvmaf): https://github.com/termux/termux-packages/blob/master/packages/ffmpeg/build.sh
- SVT-AV1 4.x releases (4.3.0, 2026-11-01): https://newreleases.io/project/gitlab/AOMediaCodec/SVT-AV1/release/v4.3.0 , https://phoronix.com/news/SVT-AV1-4.0
- ffmpeg-next maintenance status: https://docs.rs/crate/ffmpeg-next/9.0.0
- ffmpeg-the-third (FFmpeg 8.1): https://docs.rs/crate/ffmpeg-the-third/latest
- rsmpeg (FFmpeg 6–8): https://docs.rs/crate/rsmpeg/latest/features
- FFmpeg libvmaf filter options: https://ayosec.github.io/ffmpeg-filters-docs/7.0/Filters/Video/libvmaf.html
- Landlock Kconfig in Android common kernel (enablement not confirmed): https://android.googlesource.com/kernel/common/+/3ec022196c4e/security/landlock/Kconfig

Verified locally (2026-11-16, x86_64): `ffmpeg n9.0.2`; encoders `libx264`,
`libx265`, `libsvtav1`, `libopus`, `h264_vaapi`, `hevc_vaapi`, `av1_vaapi`;
filters `libvmaf`, `ssim`, `psnr`, `xpsnr`, `loudnorm`; `-progress pipe:1`
output format including the `out_time_ms` = microseconds quirk; ffprobe JSON.
Not verified: anything under the confined launcher.
