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
| `convert.job.list` | read | `{since?}` (not built: see Built) → jobs: state (queued, running, done, failed, cancelled), fraction, speed, ETA, output path, error sentence | — |
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

ffmpeg is run 10 nicer than Convert itself (setpriority is not denied by the
seccomp list) and with a thread cap, because neither sandbox layer limits CPU (`sandbox.rs`
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
  thread caps, one or two jobs at a time, one measurement (`convert.estimate`
  or `convert.frame`) at a time with the rest waiting their turn, and a
  wall-clock limit on every sample encode (a minute per second of sample, at
  least two minutes).
- **Children outliving Convert:** every ffmpeg and ffprobe is started with
  `PR_SET_PDEATHSIG = SIGKILL`, tied to the thread that waits for it, so a
  core SIGTERM (stop, restart, shutdown), a crash, or the frame loop ending
  when the socket closes takes the running encode with it. Without it the
  encode was reparented and kept writing into a partial the next start
  removes, while Retry started a second one.

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

## Built (2026-11-23)

Status: v1 backend complete, UI on **tier 1** (not the plain-JS page this
brief planned; docs/TIERS.md has since decided apps ship no JavaScript).
Everything below marked **tested** has a test that ran green on this machine
(GPD Pocket 4 class x86_64, 24 threads, ffmpeg n9.0.2, SVT-AV1 4.2.0).
Nothing here was run from a phone or over the tailnet.

### What exists

- **`crates/deckjack-convert`** (~7.9k lines including ~3.0k of tests):
  - `tools.rs`: ffmpeg/ffprobe found at fixed absolute paths under `/usr`
    (never `PATH`); every child gets `-protocol_whitelist file` and a
    `-format_whitelist` of plain media demuxers before every `-i`; children
    run 10 nicer than Convert (**tested**, relative to the test's own nice
    value) with half the cores as the thread cap; a child that overruns its
    deadline is stopped and said so; a child that ignores SIGTERM is killed
    after the grace (**tested**); every child dies with the thread that
    started it and with Convert on SIGTERM (`PR_SET_PDEATHSIG`; **tested**
    with a thread and with a whole process stopped by SIGTERM).
  - `probe.rs`: `ffprobe -of json` into typed streams; playlist, concat and
    image-sequence inputs refused **by content** before any ffmpeg runs
    (a `.mp4` that is really an HLS playlist is refused too).
  - `presets.rs`: the six presets as fixed, versioned argument builders
    (`remux@1`, `trim@1`, `share-h264@1`, `small-av1@1`, `audio-opus@1`,
    `audio-flac@1`). Every path reaches ffmpeg as `file:<absolute>`. Outputs
    carry `comment=deckjack-convert <preset>@<version>`. Remux refuses a codec
    the target container cannot hold and names what it drops.
  - `progress.rs`: `-progress pipe:1` parsed by `out_time_us` (the misnamed
    `out_time_ms` is ignored), fraction, speed, time left.
  - `estimate.rs`: three 2 s samples at 10/50/85% (one sample of up to 6 s
    under 12 s), size extrapolated with a band (sample spread, widened 10%),
    speed and predicted time, VMAF per sample (SSIM where libvmaf is absent),
    and `convert.frame` (a JPEG still of the source or of a sample encode,
    ≤1280 px, ≤512 KiB). Each call writes in a scratch folder of its own,
    removed when the call ends however it ends; two calls at once used to
    share fixed names and unlink or read each other's samples (**tested**:
    two concurrent estimates failed before the fix). Estimates and stills
    take turns, one at a time, and a waiting call says so (**tested**,
    through the frame loop). A sample encode has a wall-clock limit
    (**tested** with a stand-in that spins). The one-sample path for short
    files is **tested** on an 8 s clip, inside its band.
  - `queue.rs`: app-owned jobs persisted atomically to `$HOME/queue.json`;
    one or two at a time (**tested**: two run together and a third waits);
    batch cap 100, queue cap 500 (both **tested**, and a mixed batch queues
    what passes and lists the rest); free space checked against the estimate
    (or the source size) before a batch is accepted, whole or not at all. A
    remembered estimate counts only for the options it was measured with (a
    ten-second range says nothing about the whole file; **tested**). A job
    can save into another granted folder (**tested**: output there, no
    partial left in either folder). A retried trim says its keyframe warning
    once (**tested**); cancel = SIGTERM, SIGKILL after 5 s, delete only its
    own partial; on start, a `running` job becomes `failed: interrupted` with
    Retry and leftover partials matching its own pattern are deleted.
  - `files.rs`: grants read from the manifest; outputs written as a hidden
    partial in the destination and published with a no-replace hard link
    (copy fallback, still no-replace), named `name (Share).mp4`, then
    `name (Share) 2.mp4`.
  - `window.rs`: the tier-1 window (below). It shows what a measurement
    covered ("Measured on the whole file", or the range, container and
    options), because it cannot see the range and container fields the shell
    holds, and it does not show a remux or FLAC measurement taken with the
    other setting of its option box. `main.rs`: the 12 commands of the
    brief's table plus `convert.settings` (1 or 2 at once), with one gap:
    `convert.job.list` takes no parameters and always returns the whole list
    (at most 500 jobs). The table's `since` was dropped; a caller that passes
    it is not refused, it gets everything. `convert.status`,
    `convert.settings` and `convert.job.watch` (an answer at once for a
    finished job, at `max_secs` for a running one, and the cap) are
    **tested** through the SDK's frame loop with the test as the core.
- **Manifest**: `tier = 1`, `lifecycle = "service"` (R3), `Videos/` and
  `Music/` writable (R5 left at the conventional names), no network, no GPU,
  no `[ui]` invoke. No `ui/` directory; `/apps/convert/` answers 404
  (**tested**, e2e).
- **SDK (`deckjack-app`)**: `App::notifier()` and `Program::notified()`. A
  background job can re-render every open window with no event or command
  behind it; notifications sent while the UI loop is busy merge into one, and
  a notifier taken before `run` attaches when the app connects and detaches
  when it disconnects (both halves **tested**: the detach test fails with the
  guard removed). This is how the queue view moves on its own; Video's
  export and Files' transfers need the same. `App::serve_connected` (hidden
  from the docs) serves on a socket the caller holds, so an app's own tests
  can stand in for the core.
- **Protocol widget 13: `Progress { value: Option<f64>, label }`**. TIERS.md's
  v1 catalog already lists Progress; the two-apps rule is met by Convert's
  queue and estimate and by Video's export (5d) and Files' bulk transfers,
  both of which show long work today with text. `None` is an indeterminate
  bar, distinct from 0. Rendered as a native `<progress>` (role
  `progressbar`, clamped, accessible name from `label`) (**tested**: Rust
  wire shape, vitest render and clamp, e2e visible on all four projects).
- **Registered** in the workspace, the xtask app list test, and the e2e
  harness (`web/e2e/support/paths.ts` APPS, global setup builds it).
  `convert.e2e.ts` runs in the shell projects.
- **Spike S1 test** in `crates/deckjack-core/tests/sandbox.rs`
  (`ffmpeg_run_by_a_confined_app_reaches_its_grant_and_nothing_else`).

### The window (tier 1)

One column with three steps as tabs (Files, Preset, Queue), which is what
works at 320 px:

1. **Files**: the granted media as a list, a tick box per row, a filter
   field, one All/None button, Refresh. A window opened while jobs are queued
   or running starts on the Queue.
2. **Preset**: the six presets as a single-select list, each with its quality
   sentence; range fields (required for Trim); Container and "All audio
   tracks" for Remux; "Allow from lossy" for FLAC; Save to (beside each file,
   or into a granted folder). **Measure on samples** (or **Check** for the
   stream-copy presets) is bound to `convert.estimate` with the same fields
   Start sends, and shows a progress bar while it runs, then the size line
   with its band, the time, the score gloss and warnings. **Show command**
   reveals the exact ffmpeg line. **Convert N files** is bound to
   `convert.job.add`.
3. **Queue**: one row per job with state, a bar and "42% · 1.5× · 12 min
   left" while running, Cancel or Retry bound to the job commands, the output
   path when done, the error sentence when failed; Two at once; Clear
   finished.

Every action is a bus command, so the palette and an agent can do what the
window does. An estimate an agent runs shows in an open window too.

### Exit check, item by item

| # | Item | Result |
|---|---|---|
| 1 | Select files, choose Share, read the estimate, start | **Tested** on desktop and phone layouts, Chromium and WebKit (e2e, one file; the batch path with two files is in the cancel test). Not run with ten real phone videos, and not from a phone. |
| 2 | Lock the phone 20 min; the queue is where it is without a reload | **Not tested.** The parts are tested: jobs are app-owned and outlive the window and the socket (`lifecycle = "service"`), and the open window re-renders from the notifier with no event (e2e: the bar advances with no input). The 20-minute lock over the tailnet was not run. |
| 3 | Cancel a running job: no partial, next job starts | **Tested**, real ffmpeg (unit) and through the window (e2e, AV1 at 1080p on all four projects). |
| 4 | Outputs play and seek in Quick Look; sizes within the stated band | Band: **measured**, table below, 9 of 9 inside. Quick Look playback: **not tested** (tier 1 cannot open Quick Look, gap G2). |
| 5 | Sources byte-identical; second batch gets `(…) 2` names | **Tested** (unit and e2e, hashed). |
| 6 | Remux MKV→MP4 takes seconds and says "no quality change"; FLAC from MP3 refused unless overridden | **Tested** (unit and e2e, with the override as the control). |
| 7 | Crafted HLS/concat refused before ffmpeg; killed ffmpeg fails with a sentence; restart mid-job leaves `interrupted` + Retry and no orphan partial | **Tested** (unit, real ffmpeg; the HLS/concat test uses a recording fake ffmpeg to prove nothing ran). |

**S1 (R1), ffmpeg under the real launcher — passed.** An app declaring
Convert's grants, discovered, planned with `OsEnv` and spawned with
`Confinement::Enforced`, runs `ffprobe` and an x264 encode on a granted file;
the same probe on a file outside the grant fails with `Permission denied`;
ffmpeg reading `http://127.0.0.1:<port>` never reaches a listening port; and
`/dev/dri/renderD128` is refused without `gpu = true`. Mutation-checked:
granting the outside folder makes the probe line read `probe outside read`,
and granting the port makes the listener count 1. The e2e suite also runs
every encode through the real launcher. **Not run**: VA-API with
`gpu = true`, so whether `/run/udev` is needed is still unknown.

**S2 (R2), estimate accuracy — measured on synthetic content only.**
`cargo test -p deckjack-convert measure_estimates -- --ignored --nocapture`.
60 s 720p30 sources made with lavfi (clean test pattern, grain, a Mandelbrot
zoom whose detail changes throughout, and three 20 s scenes of very different
complexity), full encode through the queue after each estimate:

| source | preset | predicted | band | actual | error | in band | score | predicted time | took |
|---|---|---|---|---|---|---|---|---|---|
| clean | share-h264 | 22.41 MB | 19.19–25.44 | 21.17 MB | +6% | yes | VMAF 97.9 | 9.9 s | 5.2 s |
| grain | share-h264 | 90.03 MB | 80.34–100.17 | 88.47 MB | +2% | yes | VMAF 96.8 | 31.4 s | 20.4 s |
| zoom | share-h264 | 149.66 MB | 38.14–287.75 | 117.37 MB | +28% | yes | VMAF 90.0 | 26.7 s | 13.5 s |
| cuts | share-h264 | 98.60 MB | 1.12–298.96 | 99.55 MB | −1% | yes | VMAF 96.2 | 20.1 s | 14.3 s |
| clean | small-av1 | 21.21 MB | 18.93–23.62 | 20.64 MB | +3% | yes | VMAF 97.1 | 24.2 s | 6.6 s |
| grain | small-av1 | 27.27 MB | 23.61–31.43 | 26.87 MB | +1% | yes | VMAF 96.2 | 33.8 s | 10.8 s |
| zoom | small-av1 | 178.77 MB | 39.34–349.33 | 145.90 MB | +23% | yes | VMAF 91.8 | 42.9 s | 13.9 s |
| cuts | small-av1 | 36.85 MB | 0.98–97.20 | 40.04 MB | −8% | yes | VMAF 95.6 | 30.7 s | 9.4 s |
| mix (FLAC) | audio-opus | 0.93 MB | 0.84–1.03 | 0.91 MB | +3% | yes | — | 1.5 s | 0.4 s |

Measuring took 0.2–6.5 s per file here. What this says:

- **Size**: the point estimate was within ±10% on steady content and +23–28%
  on the zoom, and every actual size fell inside the stated band. But on
  content that changes a lot the band is **honest and nearly useless**
  (1–299 MB): three samples cannot locate a scene change. Real footage was
  not measured; this is the first thing to redo with a real corpus.
- **Time is over-predicted 1.5–3.7×**, worst for AV1: a 2 s sample is
  dominated by process start and encoder lookahead. The UI says "About …";
  it should either subtract a measured fixed cost per sample or say "at
  most". Not fixed in this lane.
- **AV1 vs H.264 at the starting values** (CRF 35 preset 8 vs CRF 23
  medium): similar size, similar VMAF on clean content, and far smaller on
  grain (27 MB vs 88 MB at 96). Starting values left as they are.
- **A metric defect found and fixed by this measurement**: VMAF paired frames
  by time, and Matroska keeps milliseconds, so every AV1 (MKV) sample was
  compared against neighboring source frames and scored ~72 for an encode
  that scores 97 frame for frame. Scoring now pairs frames by index
  (`settb=AVTB,setpts=N` on both inputs). Regression test
  `an_mkv_sample_is_scored_frame_for_frame_not_by_rounded_time`, which fails
  (73.6) with the old graph.

**S3–S5**: not run. R3 was decided by declaring `lifecycle = "service"`, not
by the platform fix. R4's fixtures (playlist, concat, crafted names) are
unit tests with a recording fake ffmpeg; a fuzzed corpus was not run. R6
(Termux) waits on Lane 0.5.

### Gaps, as inputs to tier-1 phase 1

The brief's UI assumed a page with script. On the 12 phase-0 widgets plus
Progress, these could not be expressed; each is listed, not invented:

- **G1 — Image.** No way to show `convert.frame`'s JPEG, so the still
  comparison (brief item 4) is not in the window. The command exists and is
  tested; an agent can use it. Needs TIERS' Image (content route) and the
  `image` surface's before/after with locked pinch-zoom.
- **G2 — Shell actions from a tier-1 window.** A bound action is a bus
  command; `shell.quicklook.open` and `shell.app.open` are shell ops, so a
  finished row cannot open in Quick Look or "Show in Files". The row shows
  the output path instead.
- **G3 — Selection mode on touch.** The List's multi-select is Ctrl/Shift
  click; there is no long-press selection mode. Files are picked with a
  tick box per row instead, which works on the phone but is not the Files
  gesture the brief asked for.
- **G4 — Open with a path.** `?path=` from Files' "Open with…" has no tier-1
  equivalent (no launch parameters reach `Program::open`).
- **G5 — Two panes from 720 px.** No Split; the window is one column at every
  width.
- **G6 — Async effects.** The window reads the queue and the folders
  directly (they are in-process), so it did not need `Cmd`-style effects; a
  window that had to call another app would.
- An **unbound box** must be held in the app's model (the shell drops its
  local tick once the app answers). A Convert bug from exactly this ("Allow
  from lossy" unticked itself and Start sent `false`) was caught by e2e and
  fixed; phase 1 should document the rule or let a box be field-only.

### Other open items

- Real-footage S2 corpus (phone video, screen recording, film grain,
  talking head) and the time correction above.
- The exit check from a phone over the tailnet, including the 20-minute lock.
- VA-API under confinement (S1's GPU half).
- R7 (Files' video thumbnails through `convert.frame`), R8 (batch hand-off),
  R9 (app events instead of `job.watch`), R10 (codecs into placement): not
  started, as the brief scoped them.
- The concurrent sandbox lane (inherited fd, Landlock `ResolveUnix`): Convert
  uses only `App::run` and the SDK's `Cancellation`, no pathname sockets or
  D-Bus, so it should inherit that change unmodified. Not verified against
  that branch.

### Changes outside the new crate (for merging)

- `crates/deckjack-protocol/src/ui.rs`: `Widget::Progress`.
- `crates/deckjack-app/src/{lib.rs,ui.rs}`: `Notifier`, `Program::notified`,
  `App::notifier`.
- `web/src/uiproto/{UiNode.vue,tree.ts}`: the Progress renderer;
  `UiProtocolPanel.test.ts`: its tests.
- `crates/deckjack-core/tests/sandbox.rs`: the S1 test.
- `Cargo.toml`, `xtask/src/main.rs` (test list), `web/e2e/support/paths.ts`,
  `web/e2e/playwright.config.ts` (shell projects include `convert.e2e.ts`).

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
