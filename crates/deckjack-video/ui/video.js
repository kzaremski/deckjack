/**
 * The Video app's UI: projects, the bin, the program monitor, the timeline
 * and the export sheet.
 *
 * Every change is a `video.*` command on the bus, and the reply carries the
 * project's new state, so the window redraws from one round trip. Changes
 * made elsewhere (another window, another device, an agent) arrive through a
 * long poll on `video.project.wait`, because the app protocol has no events
 * an app can emit.
 *
 * Built for the isolated frame from the first line: no network, no storage
 * and no content route. The monitor shows stills that come back from
 * `video.frame.render` as base64 JPEG, wrapped in `blob:` URLs. One request
 * is in flight at a time, and scrubs made meanwhile collapse into a single
 * follow-up for wherever the playhead ends up, so a fast scrub never queues
 * a backlog of frames nobody will see. A reply for a project that is no
 * longer open is dropped by its sequence number.
 *
 * Not here yet, and said so in the lane brief: playback (render-ahead
 * preview segments), cross-dissolves, fades, and a loudness meter.
 */

import { createTimeline } from "./timeline.js";
import { clamp, findClip, fit, parseTimecode, timecode } from "./model.js";

/** How long a failed long poll waits before trying again. */
const WAIT_RETRY_MS = 2000;
/** How often the export sheet asks for progress while a job runs. */
const EXPORT_POLL_MS = 500;

/** Bytes from base64, for a `blob:` URL. */
export function bytes(base64) {
  const raw = atob(base64);
  const out = new Uint8Array(raw.length);
  for (let i = 0; i < raw.length; i++) out[i] = raw.charCodeAt(i);
  return out;
}

/**
 * Mount the app on `doc`.
 *
 * `deps`: `bus` (`invoke(command, params)`), `shell` (`quicklook(path)`),
 * `text` (HTML escaping), and optionally `setTimeout`, `clearTimeout`,
 * `createObjectURL`, `revokeObjectURL` and `devicePixelRatio` for tests.
 */
export function mountVideo(doc, deps) {
  const { bus, shell, text } = deps;
  const later = deps.setTimeout ?? setTimeout.bind(globalThis);
  const toURL = deps.createObjectURL ?? ((b) => URL.createObjectURL(b));
  const revoke = deps.revokeObjectURL ?? ((u) => URL.revokeObjectURL(u));
  const $ = (id) => doc.getElementById(id);

  const ui = {
    title: $("title"),
    status: $("status"),
    editor: $("editor"),
    projects: $("projects"),
    projectList: $("project-list"),
    still: $("still"),
    placeholder: $("placeholder"),
    overlay: $("overlay"),
    timecode: $("timecode"),
    canvas: $("timeline"),
    binItems: $("bin-items"),
    files: $("files"),
    bin: $("bin"),
    nudge: $("nudge"),
    edge: $("edge"),
    link: $("link"),
    exportSheet: $("export-sheet"),
    jobs: $("jobs"),
    recovery: $("recovery"),
  };

  /** The open project's name, its last state, and the window's own state. */
  let project = null;
  let state = null;
  let playhead = 0;
  let selected = null;
  let edge = "out";
  let ripple = false;
  let snapping = true;
  let binItem = null;
  let exportSize = "1080p";
  /** Bumped on every open, so a long poll for an earlier project stops. */
  let watchToken = 0;
  /** The still in flight and whether another frame is wanted after it. */
  const still = { seq: 0, inflight: false, again: false, url: null };
  let exportPolling = false;
  /** Exports this window started whose start call has not answered yet. */
  let startsInFlight = 0;

  function say(message) {
    ui.status.textContent = message;
  }

  function fail(error) {
    say(error instanceof Error ? error.message : String(error));
  }

  const timeline = createTimeline(ui.canvas, {
    state: () => state,
    playhead: () => playhead,
    selected: () => selected,
    options: () => ({ snapping }),
    setTimeout: deps.setTimeout,
    clearTimeout: deps.clearTimeout,
    onScrub: (frame) => setPlayhead(frame, { reveal: false }),
    onSelect: (id) => select(id),
    onMove: (clip, frame, trackId) => {
      const track = state.tracks.find((t) => t.id === trackId);
      edit("video.clip.move", { clip: clip.id, frame, track: track?.name });
    },
    onTrim: (clip, which, delta) => edit("video.clip.trim", { clip: clip.id, edge: which, delta, ripple }),
  });

  // ---------------------------------------------------------------- state

  function apply(next) {
    if (!next) return;
    const first = !state || state.project !== next.project;
    state = next;
    if (selected != null && !findClip(state, selected)) selected = null;
    if (binItem != null && !state.media.some((m) => m.item === binItem)) binItem = null;
    ui.title.textContent = `${state.project}${state.dirty ? " •" : ""}`;
    $("undo").disabled = !state.can_undo;
    $("redo").disabled = !state.can_redo;
    $("save").disabled = false;
    $("export").disabled = false;
    if (first) timeline.view = fit(Math.max(state.duration, 300), ui.canvas.clientWidth || 360);
    drawBin();
    drawSelection();
    timeline.draw();
    requestStill();
  }

  async function edit(command, params) {
    if (!project) return;
    try {
      const reply = await bus.invoke(command, { project, ...params });
      apply(reply?.state ?? reply);
      return reply;
    } catch (e) {
      fail(e);
      return null;
    }
  }

  // ---------------------------------------------------------------- projects

  async function listProjects() {
    try {
      const { projects } = await bus.invoke("video.project.list");
      ui.projectList.innerHTML = projects.length
        ? projects
            .map(
              (p) =>
                `<button class="dj-row" data-project="${text(p.name)}"><span class="dj-row-name">${text(p.name)}</span></button>`,
            )
            .join("")
        : '<p class="dj-empty">No projects yet.</p>';
      for (const row of ui.projectList.querySelectorAll("[data-project]")) {
        row.addEventListener("click", () => open(row.dataset.project));
      }
      say(projects.length === 1 ? "1 project" : `${projects.length} projects`);
    } catch (e) {
      fail(e);
    }
  }

  async function open(name) {
    try {
      const reply = await bus.invoke("video.project.open", { project: name });
      show(name, reply.state);
      if (reply.opened?.recovery) {
        const when = new Date(reply.opened.recovery.saved_unix * 1000).toLocaleString();
        // Offered, not applied: nothing has changed until Recover is pressed.
        $("recovery-text").textContent = `Unsaved changes from ${when} were left when Video last stopped unexpectedly. Recover them?`;
        ui.recovery.hidden = false;
      }
      say(reply.opened?.imported_sidecar ?? `Opened ${name}`);
    } catch (e) {
      fail(e);
    }
  }

  async function create(name) {
    try {
      const fresh = await bus.invoke("video.project.new", { name });
      show(fresh.project, fresh);
      say(`Created ${fresh.project}`);
    } catch (e) {
      fail(e);
    }
  }

  function show(name, next) {
    project = name;
    state = null;
    playhead = 0;
    selected = null;
    // A still still in flight for the previous project is not this one's.
    still.seq++;
    ui.recovery.hidden = true;
    ui.projects.hidden = true;
    ui.editor.hidden = false;
    apply(next);
    listFiles();
    watch(++watchToken);
  }

  /** Follow changes made elsewhere until another project is opened. */
  async function watch(token) {
    while (token === watchToken && project) {
      try {
        const since = state?.revision ?? 0;
        const r = await bus.invoke("video.project.wait", { project, since, timeout_ms: 25000 });
        if (token !== watchToken) return;
        if (r?.changed && r.revision !== state?.revision) {
          apply(await bus.invoke("video.project.describe", { project }));
        }
      } catch {
        if (token !== watchToken) return;
        await new Promise((resolve) => later(resolve, WAIT_RETRY_MS));
      }
    }
  }

  // ---------------------------------------------------------------- bin

  function drawBin() {
    const media = state?.media ?? [];
    ui.binItems.innerHTML = media.length
      ? media
          .map(
            (m) => `
          <button class="dj-row" data-item="${m.item}" aria-current="${m.item === binItem}">
            <span class="dj-row-name">${text(m.name)}</span>
            <span class="dj-row-meta">${state ? timecode(m.duration, state.fps).slice(3) : ""}</span>
          </button>`,
          )
          .join("")
      : '<p class="dj-empty">Nothing imported yet.</p>';
    for (const row of ui.binItems.querySelectorAll("[data-item]")) {
      row.addEventListener("click", () => {
        binItem = Number(row.dataset.item);
        drawBin();
      });
    }
    $("insert").disabled = binItem == null;
    $("overwrite").disabled = binItem == null;
  }

  async function listFiles() {
    try {
      const { media } = await bus.invoke("video.media.list");
      const imported = new Set((state?.media ?? []).map((m) => m.path));
      const fresh = media.filter((f) => !imported.has(f.path));
      ui.files.innerHTML = fresh.length
        ? fresh
            .map(
              (f) =>
                `<button class="dj-row" data-path="${text(f.path)}" title="Import into the project"><span class="dj-row-name">${text(f.path)}</span><span class="dj-row-meta">Import</span></button>`,
            )
            .join("")
        : '<p class="dj-empty">No other media in Videos/.</p>';
      for (const row of ui.files.querySelectorAll("[data-path]")) {
        row.addEventListener("click", async () => {
          say(`Importing ${row.dataset.path}…`);
          const reply = await edit("video.media.import", { paths: [row.dataset.path] });
          if (reply) {
            const added = reply.result?.items?.[0];
            if (added != null) binItem = added;
            drawBin();
            say(`Imported ${row.dataset.path}`);
            listFiles();
          }
        });
      }
    } catch (e) {
      fail(e);
    }
  }

  function place(insert) {
    if (binItem == null) return;
    edit(insert ? "video.clip.insert" : "video.clip.overwrite", { item: binItem, frame: playhead });
  }

  // ---------------------------------------------------------------- playhead and monitor

  function setPlayhead(frame, { reveal = true } = {}) {
    playhead = Math.max(0, Math.round(frame));
    if (state) ui.timecode.value = timecode(playhead, state.fps);
    if (state) ui.overlay.textContent = ui.timecode.value;
    if (reveal) timeline.reveal(playhead);
    else timeline.draw();
    drawSelection();
    requestStill();
  }

  async function requestStill() {
    if (!project || !state) return;
    if (still.inflight) {
      still.again = true;
      return;
    }
    still.inflight = true;
    still.again = false;
    const seq = ++still.seq;
    const dpr = deps.devicePixelRatio ?? globalThis.devicePixelRatio ?? 1;
    const width = clamp(Math.round((ui.still.parentElement?.clientWidth || 640) * dpr), 64, 1920);
    try {
      const frame = await bus.invoke("video.frame.render", { project, frame: playhead, width });
      // A newer request supersedes this one; its reply is dropped.
      if (seq === still.seq && frame?.data) {
        const url = toURL(new Blob([bytes(frame.data)], { type: frame.mime }));
        if (still.url) revoke(still.url);
        still.url = url;
        ui.still.src = url;
        ui.still.hidden = false;
        ui.placeholder.hidden = state.duration > 0;
      }
    } catch (e) {
      fail(e);
    } finally {
      still.inflight = false;
      if (still.again) requestStill();
    }
  }

  // ---------------------------------------------------------------- selection and edits

  function select(id) {
    selected = id;
    drawSelection();
    timeline.draw();
  }

  function drawSelection() {
    const found = selected != null && state ? findClip(state, selected) : null;
    ui.nudge.hidden = !found;
    $("lift").disabled = !found;
    $("ripple-delete").disabled = !found;
    if (found) {
      ui.edge.textContent = edge === "in" ? "In" : "Out";
      ui.link.textContent = found.clip.link == null ? "Link" : "Unlink";
      $("nudge-label").textContent = `${found.track.name} ${timecode(found.clip.start, state.fps).slice(3)}`;
    }
    const marker = state?.markers?.find((m) => m.frame === playhead);
    $("marker-remove").hidden = !marker;
    if (marker) ui.nudge.hidden = false;
  }

  function nudge(delta) {
    if (selected == null) return;
    edit("video.clip.trim", { clip: selected, edge, delta, ripple });
  }

  function split() {
    const params = { frame: playhead };
    const found = selected != null ? findClip(state, selected) : null;
    if (found && playhead > found.clip.start && playhead < found.clip.end) params.clip = selected;
    edit("video.clip.split", params);
  }

  function remove(rippling) {
    if (selected == null) return;
    edit(rippling ? "video.clip.ripple_delete" : "video.clip.lift", { clips: [selected] });
  }

  function toggleLink() {
    const found = selected != null ? findClip(state, selected) : null;
    if (!found) return;
    if (found.clip.link != null) {
      edit("video.clip.unlink", { clips: [selected] });
      return;
    }
    // Relink with the unlinked clip of the other kind that overlaps it most.
    let best = null;
    let overlap = 0;
    for (const t of state.tracks) {
      if (t.kind === found.track.kind) continue;
      for (const c of t.clips) {
        const o = Math.min(c.end, found.clip.end) - Math.max(c.start, found.clip.start);
        if (c.link == null && o > overlap) {
          best = c;
          overlap = o;
        }
      }
    }
    if (!best) {
      say("Nothing to link it with: no unlinked clip of the other kind overlaps it.");
      return;
    }
    edit("video.clip.link", { clips: [selected, best.id] });
  }

  async function save() {
    if (!project) return;
    try {
      const saved = await bus.invoke("video.project.save", { project });
      apply(await bus.invoke("video.project.describe", { project }));
      say(`Saved ${saved.saved} and ${saved.sidecar}`);
    } catch (e) {
      fail(e);
    }
  }

  // ---------------------------------------------------------------- export

  function drawJobs(exports) {
    const mine = exports.filter((j) => j.project === project);
    ui.jobs.innerHTML = mine
      .map((j) => {
        const pct = Math.round((j.progress ?? 0) * 100);
        const label =
          j.state === "running"
            ? `${pct}%`
            : j.state === "done"
              ? text(j.name)
              : j.state === "failed"
                ? `Failed: ${text(j.error)}`
                : "Cancelled";
        const action =
          j.state === "running"
            ? `<button class="dj-button" data-cancel="${j.id}">Cancel</button>`
            : j.state === "done"
              ? `<button class="dj-button" data-show="${text(j.name)}">Show</button>`
              : "";
        return `<div class="job" data-job="${j.id}" data-state="${j.state}"><span>${j.size} · ${label}</span>${action}</div>`;
      })
      .join("");
    for (const b of ui.jobs.querySelectorAll("[data-cancel]")) {
      b.addEventListener("click", async () => {
        try {
          await bus.invoke("video.export.cancel", { id: Number(b.dataset.cancel) });
          pollExports();
        } catch (e) {
          fail(e);
        }
      });
    }
    for (const b of ui.jobs.querySelectorAll("[data-show]")) {
      b.addEventListener("click", () => {
        if (!shell?.quicklook(`/Videos/${b.dataset.show}`, undefined)) say(`Exported to Videos/${b.dataset.show}`);
      });
    }
    return mine.some((j) => j.state === "running");
  }

  async function pollExports() {
    if (exportPolling) return;
    exportPolling = true;
    try {
      for (;;) {
        const { exports } = await bus.invoke("video.export.list");
        // A start still in flight may not have registered its job yet.
        if (!drawJobs(exports) && startsInFlight === 0) break;
        await new Promise((resolve) => later(resolve, EXPORT_POLL_MS));
      }
    } catch (e) {
      fail(e);
    } finally {
      exportPolling = false;
    }
  }

  async function startExport() {
    const name = $("export-name").value.trim();
    const params = { project, size: exportSize };
    if (name) params.name = name;
    if ($("loudness").checked) params.loudness_lufs = -14;
    // `wait: true`, and not awaited: the call stays open until the export
    // ends, and the progress it reports is what keeps the core from stopping
    // this app for idleness, even after the window is closed. The sheet
    // follows the job through `video.export.list` as before.
    params.wait = true;
    startsInFlight++;
    bus
      .invoke("video.export.start", params)
      .then((job) => {
        if (job?.state === "failed") fail(new Error(job.error ?? "the export failed"));
      }, fail)
      .finally(() => {
        startsInFlight--;
      });
    say("Exporting…");
    // Shown now rather than on the next poll, which may be a poll already
    // waiting out its interval.
    try {
      drawJobs((await bus.invoke("video.export.list")).exports);
    } catch (e) {
      fail(e);
    }
    pollExports();
  }

  // ---------------------------------------------------------------- wiring

  const on = (id, fn) => $(id).addEventListener("click", fn);
  on("switch", () => {
    ui.projects.hidden = !ui.projects.hidden;
    if (!ui.projects.hidden) listProjects();
  });
  on("save", save);
  on("export", () => {
    ui.exportSheet.hidden = false;
    pollExports();
  });
  on("export-close", () => {
    ui.exportSheet.hidden = true;
  });
  on("export-start", startExport);
  for (const b of doc.querySelectorAll("[data-size]")) {
    b.addEventListener("click", () => {
      exportSize = b.dataset.size;
      for (const o of doc.querySelectorAll("[data-size]")) o.setAttribute("aria-pressed", String(o === b));
    });
  }
  $("new-project").addEventListener("submit", (e) => {
    e.preventDefault();
    const name = $("new-name").value.trim();
    if (name) create(name);
  });
  on("recover", async () => {
    try {
      const reply = await bus.invoke("video.project.recover", { project });
      apply(reply.state);
      ui.recovery.hidden = true;
      say("Recovered the unsaved changes");
    } catch (e) {
      fail(e);
    }
  });
  on("recovery-dismiss", () => {
    ui.recovery.hidden = true;
  });
  on("insert", () => place(true));
  on("overwrite", () => place(false));
  on("back10", () => setPlayhead(playhead - 10));
  on("back1", () => setPlayhead(playhead - 1));
  on("fwd1", () => setPlayhead(playhead + 1));
  on("fwd10", () => setPlayhead(playhead + 10));
  on("marker", () => edit("video.marker.add", { frame: playhead }));
  on("marker-remove", () => {
    const m = state?.markers?.find((x) => x.frame === playhead);
    if (m) edit("video.marker.remove", { marker: m.id });
  });
  ui.timecode.addEventListener("change", () => {
    const f = state ? parseTimecode(ui.timecode.value, state.fps) : null;
    if (f == null) {
      say("Type a timecode like 00:00:01:15, or a frame number.");
      setPlayhead(playhead);
      return;
    }
    setPlayhead(f);
  });
  on("undo", () => edit("video.edit.undo", {}));
  on("redo", () => edit("video.edit.redo", {}));
  on("split", split);
  on("lift", () => remove(false));
  on("ripple-delete", () => remove(true));
  on("link", toggleLink);
  on("edge", () => {
    edge = edge === "in" ? "out" : "in";
    drawSelection();
  });
  for (const b of doc.querySelectorAll("[data-nudge]")) {
    b.addEventListener("click", () => nudge(Number(b.dataset.nudge)));
  }
  on("ripple", () => {
    ripple = !ripple;
    $("ripple").setAttribute("aria-pressed", String(ripple));
  });
  on("snap", () => {
    snapping = !snapping;
    $("snap").setAttribute("aria-pressed", String(snapping));
  });
  on("zoom-in", () => timeline.zoom(1.5));
  on("zoom-out", () => timeline.zoom(1 / 1.5));
  on("bin-toggle", () => {
    ui.bin.classList.toggle("open");
    if (ui.bin.classList.contains("open")) listFiles();
  });

  // The keyboard does what the buttons do, through the same functions.
  doc.addEventListener("keydown", (e) => {
    if (!project || e.target?.closest?.("input, textarea")) return;
    const mod = e.ctrlKey || e.metaKey;
    const key = e.key;
    let handled = true;
    if (mod && key.toLowerCase() === "z") edit(e.shiftKey ? "video.edit.redo" : "video.edit.undo", {});
    else if (mod && key.toLowerCase() === "s") save();
    else if (key === "ArrowLeft") setPlayhead(playhead - (e.shiftKey ? 10 : 1));
    else if (key === "ArrowRight") setPlayhead(playhead + (e.shiftKey ? 10 : 1));
    else if (key === "s" || key === "S") split();
    else if (key === "m" || key === "M") edit("video.marker.add", { frame: playhead });
    else if (key === "Delete" || key === "Backspace") remove(e.shiftKey);
    else if (key === "+" || key === "=") timeline.zoom(1.5);
    else if (key === "-") timeline.zoom(1 / 1.5);
    else if (key === "Escape") select(null);
    else handled = false;
    if (handled) e.preventDefault();
  });

  globalThis.addEventListener?.("resize", () => timeline.draw());

  listProjects();

  return {
    get state() {
      return state;
    },
    get playhead() {
      return playhead;
    },
    get selected() {
      return selected;
    },
    timeline,
    setPlayhead,
    select,
    open,
  };
}
