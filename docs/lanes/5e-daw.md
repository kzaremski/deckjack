# Lane 5e: DAW (scoping brief)

Status: a paper brief, researched 2026-11-16. Nothing here has been built or
run. Anything marked **inferred** is reasoning, not something I checked; the
spikes in "Risks" exist to replace those inferences with measurements.

Read with: APP-SUITE §2, §3, §11 and §12; APP-UI-ISOLATION (the design
this brief has to fit, accepted but not built); DESIGN.md principles 1, 6 and 7
and the longevity rule.

---

## Goal

A multitrack audio editor you can use from the phone, the Fold and the desktop:
arrange recordings, edit them, mix them and bounce the result. Projects stay
in an open format another DAW can open.

The brief listed one decision as open: **Web Audio in the browser, or a native
engine on the node streaming to the browser?** This brief answers it on paper
with **Web Audio** (reasons under "Engine choice") and names the spike, S1,
that has to confirm the answer before any UI is written.

## The useful subset (v1)

v1 is an **audio-clip arranger and mixer**: roughly a podcast editor or a
stem-arranging sketchpad. It is not a production DAW.

**In v1:**

- **Projects:** create, open and save. The working format is an unzipped
  DAWproject folder (see "Formats"). Export to `.dawproject`, and import one,
  with a report of everything that was dropped.
- **Import audio:** WAV, AIFF, FLAC, MP3, Ogg Vorbis, Opus and AAC/ALAC in
  MP4. A clip is copied into the project's `audio/` folder. If its sample rate
  differs from the project's, it is conformed at import, so the realtime path
  never resamples.
- **Tracks:** add, remove, reorder, rename and color audio tracks. Each has
  mute, solo, volume and pan. There is one master bus.
- **Clips:** move, trim both edges, split at the playhead, duplicate and
  delete (by moving to the trash). Clip gain, and fade in and out with a
  linear or equal-power curve. Crossfades come from overlapping fades. Snap
  to a grid of seconds or bars, using one fixed tempo and meter per project.
- **Built-in effects, exactly DAWproject's built-in devices** (Equalizer,
  Compressor, NoiseGate, Limiter), per track and on the master. Choosing these
  four means effect settings round-trip as data that other DAWs understand,
  not as opaque plugin state.
- **Volume automation on tracks and the master.** One lane each, breakpoints
  only.
- **Metering:** peak meters per track, and on the master a loudness meter
  showing momentary, short-term and integrated LUFS plus true peak.
- **Transport:** play, stop, loop a range, scrub by dragging the playhead,
  and a metronome click.
- **Undo and redo** for every edit, plus autosave.
- **Bounce:** render the whole project, or a range, to WAV or FLAC on the node
  with the same DSP code the browser plays with. MP3, Opus and AAC go through
  the `ffmpeg` helper binary, per decision 1's flagged exception.
- **An agent-readable `daw.state.describe`,** per DESIGN.md's "what every app
  provides".

**Explicitly not in v1 (each one is a separate later decision, not an
increment):**

- **Recording.** In an app frame the microphone is blocked
  (`Permissions-Policy: microphone=()`), and media through the broker is
  deferred by D6. Recording waits on the D6 reopening described under "How it
  fits the sandbox". It is the first v2 item, because a DAW without recording
  is a real limitation, and this brief says so rather than hiding it.
- **MIDI, instruments and synths.** Safari has no Web MIDI, and a piano roll on
  a phone is its own touch-design problem.
- **Third-party plugins (VST3, CLAP, LV2, AU).** Native plugins cannot run
  inside a browser engine (see "Engine choice"). DAWproject plugin state in an
  imported project is kept on disk untouched and reported as bypassed.
- **Time-stretching and pitch-shifting.** The DAWproject `Warp` elements are
  read, preserved and reported, but not played.
- **Sends, groups and buses beyond the master.**
- **Streaming from disk.** v1 holds decoded audio in memory under a stated
  budget (see Risks, R4). Long-form material beyond that budget is v2.
- **Video sync, surround, multiple operators, and clip-launcher or session
  view.**

## Engine choice

### Where the audio is heard decides it

The node is often not the machine you are listening on. DESIGN.md's normal case
is the browser on the Fold or a Mac, with the backend on the R620 over the
tailnet. A native engine on the node can only be heard in one of two ways:

1. **Through the node's own sound card.** That is useless when the node is a
   rack server, and it breaks principle 7 ("location doesn't matter to the
   UI").
2. **By streaming rendered audio to the browser.** The rest of this section
   compares that option with Web Audio.

| | **A. Web Audio engine in the frame** (recommended) | **B. Native engine on the node (cpal and Rust DSP), streamed to the browser** |
|---|---|---|
| **Fader move or seek to hearing it** | One render quantum (128 frames, about 2.7 ms at 48 kHz), plus the browser's output latency. That is roughly 10 to 40 ms on phones that run AAudio well, and worse on poor devices (anecdotal reports go up to several hundred ms; [construct.net thread](https://www.construct.net/en/forum/construct-3/general-discussion-7/big-sound-lag-android-148079/page-4); Chrome 136 added adaptive buffering for AAudio glitches, per the [fugu-dev note](https://groups.google.com/a/chromium.org/g/fugu-dev/c/AavLqm3hk0Y)). | Network round trip, plus the encoder frame, plus the jitter buffer, plus the browser's output latency. **Inferred: 80 to 300 ms over the tailnet from a phone.** Every play, seek, solo and fader move feels like a remote control. |
| **Offline or tether mode** | Works with no node at all once the audio is loaded. | Dead without the node. |
| **Transport to the frame** | Only file bytes, once, before playback. No realtime traffic. | Needs realtime media into an opaque frame that has `connect-src 'none'`. That means WebRTC, which is the **open exfiltration channel** (ISOLATION §7.2.3) and which we would then depend on rather than hope to close, or media streaming through the broker, which D6 explicitly deferred. Either way it is a hole in the isolation design. |
| **Fidelity heard while mixing** | Bit-exact with the bounce if all DSP runs in our own wasm (see "Parity"). | Opus is near-transparent at high bitrate but not lossless. Raw PCM over a phone link costs 1.5 to 3 Mbit/s, needs its own jitter handling, and still pays the latency. |
| **Sandbox cost on the node** | None. The backend never touches an audio device. | Needs `/dev/snd/*` or the PipeWire socket under `/run/user/<uid>`. That is exactly the directory `apps.rs` refuses because it holds the keyring, agent and Wayland sockets. A new `[needs] audio` key and a real grant design would be required. |
| **Android node (Termux)** | Not needed. The phone is a client. | cpal's Android host is AAudio through the `ndk` crate. The `ndk-context` crate panics when no JVM context was initialized, and a Termux binary has none ([ndk-context source](https://docs.rs/ndk-context/latest/src/ndk_context/lib.rs.html), [uniffi-rs #1778](https://github.com/mozilla/uniffi-rs/issues/1778)). **Inferred:** cpal device enumeration fails under Termux without an APK wrapper. |
| **CPU headroom** | Bounded by the phone. Eight to sixteen tracks with EQ and compression in wasm is **expected but unmeasured** (spike S1). | The node's CPU, which matters only for heavy DSP that v1 does not have. |
| **Plugins** | None, ever, in the browser. | Possible in principle, but only audible through the stream. |

**Decision on paper: A.** The engine runs inside the app's frame. An
`AudioWorkletNode` runs **one wasm module that contains the whole mix graph**:
clip playback, fades, gain, pan, the four devices, automation and meters. Web
Audio is used only as the clock and the sink. B's single advantage is native
plugins, and those are out of v1 for product reasons anyway. B's costs are
latency, an isolation hole and an Android hole.

**What "never on the bus" means here, precisely:** no realtime audio crosses
the bus, the broker or the network during playback. File bytes are loaded
once, in bulk, before play, ideally over the broker content op (below), which
does not use the bus. The `files.read` fallback does carry bytes over the bus,
which is tolerable for v1 because it is a bulk load and not the audio path.
Peaks and the project model are not audio.

### Libraries

| Role | Choice | Status (checked 2026-11-16) | Notes |
|---|---|---|---|
| Decode in the browser | `decodeAudioData` first; symphonia compiled to wasm in a Worker for codecs the browser lacks (for example Vorbis or Opus on older Safari) | Browser API | `decodeAudioData` resamples to the context rate, which is set to the project rate. |
| Decode on the node (peaks, render, conform) | **symphonia** 0.6.1, MPL-2.0, 100% safe Rust. FLAC, MP3, Vorbis, WAV and ALAC rated Great or Excellent; AAC-LC Great; Opus "not rated" (new in 0.6). The page lists a WASM API only as *planned*. | [crates.io](https://crates.io/crates/symphonia), [README](https://github.com/pdeljanov/Symphonia) | MPL-2.0 is file-level copyleft and compatible with Deckjack's AGPL. A wasm32 build of the pure-Rust decoders is **inferred** to work but not confirmed (spike S3). `symphonia-adapter-libopus` 0.3.0 (MIT/Apache) is the fallback if the native Opus decoder disappoints, at the cost of C code. |
| DSP shared by the worklet and the render | Our own small crate, `deckjack-dsp` (no_std, no allocation in `process`), with **fundsp** 0.23.0 (MIT/Apache, no-std category) as a candidate for filters and dynamics | [crates.io](https://crates.io/crates/fundsp) | The EQ, compressor, gate and limiter have to match DAWproject's parameter semantics, so owning the code is likely simpler than adapting fundsp's graph. That is a spike-S6 judgment. |
| Resampling at import | **rubato** 5.1.0, MIT/Apache | [crates.io](https://crates.io/crates/rubato) | Offline only. |
| Loudness | **ebur128** 0.1.11, MIT | [crates.io](https://crates.io/crates/ebur128) | Used both on the node and in wasm for the meter. |
| WAV write | **hound** 3.5.1, Apache-2.0 | [crates.io](https://crates.io/crates/hound) | Stable; last release 2023. |
| FLAC write | **flacenc** 0.5.1, Apache-2.0 | [crates.io](https://crates.io/crates/flacenc) | Pure Rust. Verify output with `flac -t` in tests. |
| Lossy export | the `ffmpeg` binary, confined | §3.1 | Shared with 5c's presets. |
| Device I/O | **cpal 0.18.2 (Apache-2.0): not used in v1** | [docs.rs](https://docs.rs/crate/cpal/latest) | Recorded so nobody reaches for it by reflex. cpal's own wasm `audioworklet` host needs SharedArrayBuffer, cross-origin-isolation headers and nightly Rust, none of which an opaque sandboxed frame can have. Its `webaudio` host runs on the main thread. Neither fits, so the worklet glue is ours. |

### Parity: what you hear is what you bounce

Bouncing in the browser is ruled out by the platform. The browser socket
refuses inbound frames over 256 KB (`ws::Settings::max_frame_bytes`), and that
is why the editor caps saves at 192 KB. A rendered mixdown cannot be written
back from the frame. So bouncing is native (`daw.render`), and it uses
`deckjack-dsp` compiled for the host. The worklet uses the same crate compiled
to wasm32. The test is a null test: the browser render (an
`OfflineAudioContext` driving the same worklet) minus the native render should
leave a residual at or below about -120 dBFS (float rounding only). Web Audio's
own `GainNode` and `StereoPannerNode` are not used, because they would make
parity impossible.

## Backend commands

The app is `deckjack-daw`. Ids follow §11, risk is declared by the backend,
and progress goes through `AppFrame::Progress`. A cancel arrives as
`CoreFrame::Cancel` (both exist in `deckjack-protocol`), and long jobs check a
cooperative cancel flag between blocks. Nothing is `outbound`. Nothing is
`destructive`, because every overwrite goes through the trash first.

| Command | Risk | Progress | What it does |
|---|---|---|---|
| `daw.project.create {path, name, sample_rate, tempo?, meter?}` | write | no | Makes the project folder with `project.xml`, `metadata.xml` and `audio/`. Refuses if the folder exists. |
| `daw.project.open {path}` | read | no | Parses `project.xml` into the UI's model and returns a content-hash `version`, plus `unsupported[]` listing what is kept but not played (plugins, MIDI, warps, sends). |
| `daw.project.save {path, model, version, force?}` | write | no | Writes `project.xml` atomically (temp file, then rename). It answers `status: "conflict"` on a stale version, the same pattern as `editor.save`. Elements it does not understand are carried through byte for byte. The model must fit one 256 KB browser frame, so if spike S5 shows large projects exceed that, this becomes `daw.project.patch`. Parameters are redacted from the audit log. |
| `daw.project.export {path, out}` | write | yes | Zips the folder into `out.dawproject`, with audio stored uncompressed. Refuses to overwrite unless `replace`; with `replace`, the old file goes to the trash first. |
| `daw.project.import {src, path}` | write | yes | Unzips a `.dawproject` into a project folder (with zip-slip and size bounds, reusing `archive.rs`'s limits). Returns the `unsupported[]` report. |
| `daw.media.probe {path}` | read | no | Container, codec, rate, channels, frames, duration and tags. |
| `daw.media.peaks {path, samples_per_bucket}` | read | yes (long files) | Min and max per bucket at a ladder of zoom levels, cached under `~/.deckjack/cache/daw/`, keyed by SHA-256 of path, mtime, length and level, with an LRU cap. Lets the timeline draw before the browser finishes decoding. |
| `daw.media.import {src, project, conform?}` | write | yes | Copies the source into `audio/` (by deduplicated name, never overwriting). When the rate differs from the project's, it writes a conformed FLAC instead and keeps the original's name in the clip's metadata. |
| `daw.render {project, range?, out, format: "wav"\|"flac", bit_depth, normalize_lufs?}` | write | yes, cancellable | Native mixdown of the **saved** project with `deckjack-dsp`. Writes under a hidden partial name and renames when done, so a cancelled render leaves nothing behind. Reports integrated LUFS and true peak. |
| `daw.render.encode {in, out, preset}` | write | yes, cancellable | Runs the confined `ffmpeg` to make MP3, Opus or AAC. Shares presets with 5c, and should defer to 5c's commands if those exist first. |
| `daw.media.loudness {path}` | read | yes | EBU R128 integrated loudness, LRA and true peak. |
| `daw.state.describe {path}` | read | no | Tracks, clips, devices, length and unsupported items, for agents. |

The frame also calls core commands through `[ui] invoke` (see below):
`files.list`, `files.stat` and `files.space` for the project picker and
free-space checks; `files.read` as the v1 byte fallback; `files.copy` to bring
a clip in from outside the DAW's tree, through `daw.media.import`'s source;
and `files.trash` to delete takes, renders and projects. Deleting a project is
**moving it to the trash**, `write`, consistent with Lane 1's deviation.

**Recording (v2, not v1):** `daw.take.begin {project, track}` returns a take
id (`write`); `daw.take.append {take, pcm_b64}` (`write`, called by the
**shell's** capture path, not the frame); `daw.take.finish {take}`. These make
a WAV file grown under a partial name. Recording audio would then cross the
bus as bulk chunks, at about 384 KB/s for stereo f32 and half that for i16.
That is tolerable because it is not the playback path, but it is the one place
this brief bends "audio never on the bus". The alternative is a broker media
op (see below).

## UI and touch contract

**Shape:** canvas surfaces for the timeline, waveforms, meters and automation,
drawn with the theme tokens per DESIGN.md "Canvas surfaces read the same
tokens". DOM for headers, the inspector and dialogs. No build step: the
worklet glue is plain JS. The wasm is built by `cargo xtask` and checked in,
like CodeMirror's vendored bundle, with a staleness test that rebuilds it and
compares.

**Touch contract:**

- **Selection is a mode, entered by long-press** (Lane 1's precedent). In it,
  taps toggle clips.
- **Trim handles are hit zones of at least `--dj-touch`.** They extend
  *outside* a short clip rather than shrinking with it. On a very short clip
  the handles become the inspector's numeric fields.
- **Split is a button at the playhead,** not a razor tool that needs a precise
  tap.
- **Pinch zooms time,** anchored between the fingers (reusing
  `shell/gestures.ts`'s geometry). Two-finger drag pans. Vertical pinch over
  the track area changes track height.
- **Fades** have a corner grab handle on each clip, with the same minimum hit
  size.
- **Fine moves:** every drag has a "nudge" pair of buttons in the selection
  bar (grid step, or ±10 ms with snap off), because a finger cannot place
  something to the sample.
- **Transport** (play, stop, loop, metronome, return to start) is a fixed bar
  at the bottom, padded by the `visualViewport` inset like the editor and
  terminal.
- **The first tap on Play creates or resumes the `AudioContext`.** That is the
  user activation browsers require. The UI never shows a dead Play button
  without saying why.
- **Keyboard (desktop):** Space plays, S splits, Delete trashes, arrows nudge,
  Ctrl+Z and Ctrl+Shift+Z undo and redo, L loops. None of these is the only
  way to do anything.

**Honest minimum widths:**

- **360 px (phone portrait):** arranging and simple editing. Track headers
  collapse to a 44 px color strip with M and S. The mixer is a separate sheet
  showing about four strips with horizontal scroll. Device editing is a
  full-screen sheet per device. It is usable, but cramped beyond about six
  visible tracks.
- **About 600 px and up (unfolded Fold, tablet):** the comfortable floor for
  crossfade shaping, automation drawing and the side-by-side inspector. Those
  *work* at 360 px through sheets and numeric fields, but they are not
  pleasant there.
- **Landscape phone:** the timeline gets the width but only about 300 px of
  height, roughly three tracks. Supported, not optimized.
- **Manifest:** `compact = true`, `default_width = 1200`,
  `default_height = 760`.

This passes decision 3 (editing on the phone, not view-only), with the
precision tasks honestly better on the Fold.

## How it fits the sandbox

### Process side (`app.toml`)

```toml
name = "daw"
title = "Studio"
tag = "Au"
summary = "Arrange, edit, mix and bounce audio"
lifecycle = "on-demand"
compact = true
default_width = 1200
default_height = 760

[needs]
gpu = false                                    # canvas 2D is enough; no /dev/dri
working_dir = [{ path = "Music", write = true }]  # the only tree it reads or writes
invoke_commands = false                        # the backend never calls the bus
# no network, no connect_tcp, no bind_tcp

[ui]                                           # per APP-UI-ISOLATION §4.8, once built
invoke = ["files.list", "files.stat", "files.space", "files.read",
          "files.copy", "files.trash"]
```

- **`working_dir`.** `Manifest::validate` refuses `"."`, so the DAW gets one
  subtree. Projects live under it, and so do renders. A clip from elsewhere is
  copied in by the frame calling `files.copy` (core, operator-scoped) into the
  DAW tree, then `daw.media.import`. The tree name (`Music` here) is the
  operator's call at approval.
- **Helper binary:** `/usr/bin/ffmpeg` only, exec'd under the inherited
  Landlock and seccomp (§2). Lossy export is unavailable where it is missing,
  and the UI says so. **Termux:** its binaries live under
  `/data/data/com.termux/files/usr/bin`, which is not in `SYSTEM_READ_DIRS`,
  so the confined helper would not be found on the phone. This is a Lane 0.5
  question, noted, not solved.
- **No `/dev/snd`, no PipeWire or Pulse socket, no realtime scheduling.** The
  backend is batch only, which is why it fits the existing sandbox unchanged.
  The seccomp denylist does not touch anything it needs.
- **GPU: no.** Waveforms are canvas 2D. Revisit only if spike S1 shows the
  timeline repaint competing with the worklet on the phone. It should not,
  because the worklet is on its own thread.
- **Network ports: none.**

### UI-isolation side: what the frame needs that v1 of the isolation design does not give

The engine lives *inside* the opaque sandboxed frame. That is good: no audio
ever leaves it, and the WebRTC hole is not something we use. But four things
collide with the design as accepted:

1. **Byte transport into the frame (D6 "media streaming through the broker"
   was deferred).** The frame cannot fetch `/content`: `connect-src 'none'`,
   and the cookie-route gate refuses `Origin: null`. v1 works today through
   `files.read` in `[ui] invoke`. That path is capped at 16 MiB per file,
   base64-inflated, over the bus, and the 8 MiB app frame cap does not apply
   because it is a core command, but the broker's 1 MiB limit is on *params*,
   not responses (to confirm). A 4-minute 24-bit stereo WAV is about 70 MB and
   cannot load that way. **Ask: reopen D6 for one narrow broker op,
   `content.read {path, offset, length}`.** The shell performs a `Range`
   fetch of `/content` with its own cookie, the server checks the path
   against the app's stage-6 file scope (the same check `files.read` gets for
   `Issuer::AppUi`), and the result is an `ArrayBuffer` *transferred*
   (zero-copy) over the MessagePort, with a per-call cap of about 8 MiB and
   the existing rate limit. It reads only bytes the app could already read by
   `files.read`, so it adds no authority, only bandwidth. 5a, 5c and 5d need
   the same op, so it should be a platform item, not a DAW one.
2. **`'wasm-unsafe-eval'` in the frame's `script-src`.** §4.2's CSP lists none,
   so `WebAssembly.compile` would be refused. It allows compiling wasm the
   frame already holds; it grants no network and no eval of JS. Proposal: a
   manifest flag `[ui] wasm = true`, covered by the fingerprint, that adds it
   for that app only. WebKit did not support `'wasm-unsafe-eval'` for a long
   time ([WebKit 197759](https://bugs.webkit.org/show_bug.cgi?id=197759)); a
   2023 thread says it landed with bug 235408
   ([webkit-unassigned](https://lists.webkit.org/pipermail/webkit-unassigned/2023-May/1104736.html)).
   Current Safari behavior *inside an AudioWorklet* needs checking (S3). A
   separate report says WebKit worklets did not inherit the owner's CSP at all
   (listed as a CVE; [opencve](https://app.opencve.io/cve/CVE-2026-43670));
   the dates conflict, so treat it as unverified.
3. **Autoplay delegation.** Chrome's policy is that a cross-origin iframe's
   `AudioContext` "will never be allowed to run" unless autoplay is delegated
   with `allow="autoplay"` ([Chrome autoplay
   policy](https://developer.chrome.com/blog/autoplay)). The accepted frame
   has `allow=""`. Whether a gesture *inside* an opaque sandboxed frame is
   enough without delegation is not documented. **Inferred: it is not.**
   Proposal: `[ui] media = ["autoplay"]` adds `allow="autoplay"` to that app's
   iframe only, fingerprinted. The isolation design's Permissions-Policy
   header list should then *not* include `autoplay=()` for that app. Settled
   by S2.
4. **Microphone (v2 recording).** `microphone=()` is in the frame's
   Permissions-Policy, and delegating it into an opaque frame would put a live
   mic inside the WebRTC-exfiltration hole. The safer shape: **the shell
   captures** (`getUserMedia` in the shell document, with the prompt drawn in
   shell chrome and a persistent "Studio is recording" strip). It sends PCM to
   the backend (`daw.take.append`) and a monitoring copy to the frame over
   the port as transferred `Float32Array` chunks. This is a broker convenience
   D6 deferred. It needs its own design pass and is the gate on v2.

**Also from the isolation doc:**

- the second socket (§4.12) matters for the `files.read` fallback, so
  terminal echo does not stall while a project loads;
- D6's "downloads" deferral means a bounce cannot be saved to the phone's own
  storage from the DAW. It lands on the node, and the Files app or a future
  download op is the path.

**Stated plainly:** with `files.read` scope over `Music/`, the DAW's UI can
send unreleased audio anywhere over WebRTC (ISOLATION §7.2.3). Nothing in this
lane closes that.

## Formats and longevity

- **Working project = an unzipped DAWproject 1.0.** DAWproject is MIT-licensed,
  declared stable at 1.0, and read and written by Bitwig 5.0.9+, Studio One
  6.5, Cubase 14, Cubasis 3.7.1, VST Live 2.2 and n-Track 10.2.2
  ([bitwig/dawproject](https://github.com/bitwig/dawproject)). The layout is:
  - `Song/project.xml`
  - `Song/metadata.xml`
  - `Song/audio/*.wav|flac`

  That is the same tree a `.dawproject` zip contains. `daw.project.export`
  only zips it, so the interchange file is never a translation. The schema
  covers clips with `playStart`/`playStop`, `fadeInTime`/`fadeOutTime`, loops,
  track volume and pan, volume automation, the built-in Equalizer (typed
  bands), Compressor, NoiseGate and Limiter, and `fileReference` with an
  optional `external` flag ([Project.xsd](https://raw.githubusercontent.com/bitwig/dawproject/main/Project.xsd)).
  Every v1 feature therefore maps to a schema element. **No proprietary
  format** (Lane 5's must-not).
- **What does not map, and where it goes:** UI-only state (zoom, scroll,
  track heights, selection) goes in a sidecar, `Song/.deckjack-view.json`. It
  is safe to delete, and losing it changes nothing audible.
- **What survives Deckjack:** the whole mix, in another DAW, minus anything
  that DAW does not implement. Audio stays as plain WAV or FLAC files that any
  tool plays. **What does not survive:** our exact compressor *curve*.
  DAWproject fixes the parameters, not each vendor's DSP, so a project opened
  in Cubase will sound close, not identical. The bounce is the lossless
  record, and the brief says so in the export dialog.
- **Round-trip claims are only made where tested:** export, then import into
  at least one real DAW (n-Track or Cubasis on Android, Bitwig on desktop),
  then compare clip positions, fades and levels. Unknown elements on import
  are preserved byte for byte, not dropped.
- **Caches** (`~/.deckjack/cache/daw/`: peaks) are safe to delete (§11).

## Risks and open questions (each with its settling spike)

| # | Risk or question | Spike that settles it | Pass criterion |
|---|---|---|---|
| **S1** | **Web Audio vs native streaming: the decision itself.** Can a wasm AudioWorklet in an opaque sandboxed frame mix 8 stereo tracks with EQ, compression and meters without glitching on the Fold and an iPhone? | A throwaway page served the way §4.2 will serve it: sandboxed frame, opaque origin, the §4.2 CSP plus `'wasm-unsafe-eval'`, `allow="autoplay"`. 8 tracks of 48 kHz stereo looped for 10 minutes. Count underruns (the worklet logs any `process()` call whose `currentFrame` jumps; Chrome's `AudioContext.playbackStats` where available). Record `baseLatency` and `outputLatency`, and worklet time per quantum. On the laptop as node, also run the B arm: cpal and the same DSP, Opus over WebRTC to the same phone. Measure tap-to-sound with a high-speed phone video of the screen and a speaker. | A is confirmed if there are zero underruns on both phones at no more than 50% of the quantum budget. B is rejected if tap-to-sound exceeds 2.5× A's. If A fails, the fallback is fewer tracks or a lower rate on phones, **not** B, unless B's latency surprises. |
| **S2** | Does an `AudioContext` in a sandboxed opaque frame start on a tap inside the frame, with and without `allow="autoplay"`, in Chromium, WebKit (iOS PWA) and Firefox? | Same harness: a Play button, with `AudioContext.state` reported via the broker. | Decides whether `[ui] media = ["autoplay"]` is needed (inferred yes). |
| **S3** | Can the wasm module be compiled and run inside the AudioWorklet on Safari and Firefox under `'wasm-unsafe-eval'`, and does symphonia build for wasm32? The module has to arrive through the cap path, compile on the main thread and be posted to the worklet, or be compiled inside it. | Load one `.wasm` from the cap path; try both delivery paths; run `cargo build --target wasm32-unknown-unknown` on symphonia with the needed features. | Both browsers run it; symphonia builds. If posting a `WebAssembly.Module` to the worklet fails on Safari, send bytes and compile in the worklet. |
| **S4** | **Memory budget.** Decoded f32 stereo at 48 kHz is 384 KB/s, about 23 MB per minute, so 300 MB is about 13 minutes of total material. Does iOS Safari kill the frame first? | Load increasing amounts into worklet wasm memory on an iPhone and the Fold; record the point of tab death. Try i16 storage, which halves it. | Sets the v1 budget per device class, refused with a sentence above it. Streaming from disk is v2. |
| **S5** | Load path throughput: `files.read` (base64 over the bus, ≤16 MiB) vs the proposed `content.read` broker op. Also, how large does a 200-clip project model get against the 256 KB frame? | Time a 15 MB FLAC load both ways on the phone over the tailnet; serialize a synthetic 200-track, 2,000-clip model. | Justifies the D6 reopen with numbers; decides `save` vs `patch`. |
| **S6** | **Parity.** Does native `deckjack-dsp` match the worklet render bit for bit, or within float rounding? | Render one project both ways, subtract, and measure. | Residual ≤ -120 dBFS. Otherwise find the divergence (denormals, fused multiply-add, libm differences between wasm and native) before writing any UI. |
| **R7** | Does `CoreFrame::Cancel` reach an app command promptly, or does the core's cancellation rule (only `terminal.read` is drop-cancelled, `wiring::DROPPED_ON_CANCEL`) mean the shell hears "cancelled" while the render runs on? | Read `wiring.rs` and app-side dispatch, then a test: cancel a 60-second render, assert no output file and a `Failed: cancelled` journal entry only after the app stopped. | Honest cancel semantics, as Lane 3 established. |
| **R8** | DAWproject round-trip with real DAWs. | Export a v1 project, open it in n-Track (Android) or Bitwig, compare. | Positions within 1 sample, fades and levels match their parameters. |
| **R9** | Bluetooth output adds 150 to 250 ms (**inferred**, typical). Playback stays in sync with the UI only if the playhead uses `outputLatency`. | Same S1 harness, with BT headphones. | The playhead is drawn at `currentTime - outputLatency`. Monitoring for v2 recording is unusable over BT, and the UI says so. |

## Exit check

On the **unfolded Fold** as the client, with the backend on the R620:

1. Create a project.
2. Import three stems (one FLAC, one MP3, one 44.1 kHz WAV conformed to 48
   kHz).
3. Arrange, trim, split and crossfade.
4. Set volume, pan, an EQ band and a compressor; draw one volume automation
   lane.
5. Play 10 minutes in a loop with no audible dropout.
6. Bounce to FLAC with progress, then cancel and restart one bounce, leaving
   no stray file.
7. Null-test the bounce against the browser's own offline render.
8. Export `.dawproject` and open it in a real DAW (n-Track, Cubasis or
   Bitwig) with clips, fades and levels where they were.

Repeat steps 3 to 5 on an **iPhone Safari PWA at 390 px** with the compact
layout. Run everything under the accepted isolation design (opaque frame plus
broker), not the same-origin frame of today. If stage 4 has not landed, the
lane says the exit check is not met. It does not pass on the old frame.

## Rough size and difficulty

This is the hardest lane in the suite after the video editor, and it is hard
in a different way: the code is not enormous, but the realtime and
cross-browser behavior can only be verified on devices. Estimates, production
code, excluding tests:

| Part | Size (est.) |
|---|---|
| `deckjack-dsp` (mixer, fades, the four devices, automation, meters, LUFS glue), native and wasm | ~2.0k Rust |
| `deckjack-daw` backend (DAWproject read and write with carry-through of unknown XML, commands, render, import/conform, peaks cache, ffmpeg) | ~3.0k Rust |
| Frame UI (timeline canvas, clip editing, mixer, device sheets, transport, worklet glue, undo, autosave) | ~6.0k JS |
| Platform asks (broker `content.read`, `[ui] wasm` and `[ui] media` in manifest, fingerprint, CSP and iframe) | ~0.5k Rust/TS, after ISOLATION stage 4 |
| Tests: about equal to code. Mutation-checked per §12, plus a device matrix that cannot be automated (S1 to S4). | |

**Total: about 11k lines plus tests.** Realistically three lane passes:

1. Spikes S1 to S6 and `deckjack-dsp` with the parity test, before any UI.
2. The backend and the desktop UI.
3. Touch, compact layout and the phone exit check.

Recording (v2) is a fourth pass and a separate isolation design.

**Ordering:** after ISOLATION stage 4, and after or alongside 5c, with which it
shares the `ffmpeg` path and the `content.read` broker op. It does **not**
need Lane 0.2 (GPU) or Lane 0.5 (cross-compile) for v1, because the phone is
only ever a client.
