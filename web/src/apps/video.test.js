// @vitest-environment happy-dom
import { readFileSync } from "node:fs";
import { resolve } from "node:path";
import { afterEach, describe, expect, it } from "vitest";

import {
  EDGE_REACH,
  GUTTER,
  MAX_PX_PER_FRAME,
  RULER_HEIGHT,
  SNAP_PX,
  TRACK_HEIGHT,
  dropFrame,
  frameToX,
  hit,
  layout,
  parseTimecode,
  partners,
  pinch,
  snap,
  snapTargets,
  timecode,
  trimDelta,
  xToFrame,
  zoomAt,
} from "../../../crates/deckjack-video/ui/model.js";
import { LONG_PRESS_MS } from "../../../crates/deckjack-video/ui/timeline.js";
import { mountVideo } from "../../../crates/deckjack-video/ui/video.js";

/**
 * The Video app's UI. The pure timeline rules first (time and pixels,
 * snapping, hit testing), then the app mounted from its real markup with a
 * fake bus shaped like the backend's replies.
 */

const FPS = { num: 30, den: 1 };

/** A describe result: V1 with two linked clips over A1, a marker at 45. */
function described(overrides = {}) {
  return {
    project: "Cut",
    revision: 3,
    dirty: false,
    can_undo: true,
    can_redo: false,
    fps: FPS,
    width: 1920,
    height: 1080,
    duration: 90,
    tracks: [
      {
        id: 1,
        name: "V1",
        kind: "video",
        clips: [
          { id: 10, item: 5, name: "a.mp4", start: 0, end: 30, source_in: 0, link: 100, gain_db: 0, enabled: true },
          { id: 11, item: 5, name: "a.mp4", start: 30, end: 90, source_in: 30, link: 101, gain_db: 0, enabled: true },
        ],
      },
      { id: 2, name: "V2", kind: "video", clips: [] },
      {
        id: 3,
        name: "A1",
        kind: "audio",
        clips: [
          { id: 20, item: 5, name: "a.mp4", start: 0, end: 30, source_in: 0, link: 100, gain_db: 0, enabled: true },
          { id: 21, item: 5, name: "a.mp4", start: 30, end: 90, source_in: 30, link: 101, gain_db: 0, enabled: true },
        ],
      },
      { id: 4, name: "A2", kind: "audio", clips: [] },
    ],
    markers: [{ id: 50, frame: 45, name: "beat", comment: "", duration: 0 }],
    media: [{ item: 5, name: "a.mp4", path: "a.mp4", kind: "Movie", duration: 120 }],
    ...overrides,
  };
}

describe("time and pixels", () => {
  const view = { pxPerFrame: 4, scroll: 10 };

  it("maps frames to x and back through the gutter and the scroll", () => {
    expect(frameToX(10, view)).toBe(GUTTER);
    expect(frameToX(15, view)).toBe(GUTTER + 20);
    expect(xToFrame(GUTTER + 20, view)).toBe(15);
    // Rounded to the nearest boundary, not truncated.
    expect(xToFrame(GUTTER + 23, view)).toBe(16);
  });

  it("zooms about the anchor, keeping the frame under it", () => {
    const anchorX = GUTTER + 100;
    const before = (anchorX - GUTTER) / view.pxPerFrame + view.scroll;
    const z = zoomAt(view, 2, anchorX);
    expect(z.pxPerFrame).toBe(8);
    expect((anchorX - GUTTER) / z.pxPerFrame + z.scroll).toBeCloseTo(before, 9);
    // Clamped, and never scrolled before frame 0.
    expect(zoomAt(view, 1000, anchorX).pxPerFrame).toBe(MAX_PX_PER_FRAME);
    expect(zoomAt({ pxPerFrame: 4, scroll: 0 }, 0.5, GUTTER + 300).scroll).toBe(0);
  });

  it("pinches about the fingers' midpoint and pans with it", () => {
    const start = { pxPerFrame: 4, scroll: 100 };
    const under = (200 - GUTTER) / 4 + 100;
    // Fingers spread to twice the distance while their midpoint moves to 260.
    const v = pinch(start, 100, 200, 200, 260);
    expect(v.pxPerFrame).toBe(8);
    expect((260 - GUTTER) / v.pxPerFrame + v.scroll).toBeCloseTo(under, 9);
  });
});

describe("snapping", () => {
  const view = { pxPerFrame: 2, scroll: 0 };

  it("snaps within the distance and lets go outside it", () => {
    const reach = SNAP_PX / view.pxPerFrame;
    expect(snap(100 + reach - 1, [100], view)).toBe(100);
    expect(snap(100 + reach + 1, [100], view)).toBe(100 + reach + 1);
  });

  it("breaks a tie toward the earlier target, whatever the order", () => {
    expect(snap(50, [52, 48], view)).toBe(48);
    expect(snap(50, [48, 52], view)).toBe(48);
  });

  it("does not snap a dragged clip to its own or its partner's edges", () => {
    const state = described();
    expect(partners(state, state.tracks[0].clips[1])).toEqual([11, 21]);
    const targets = snapTargets(state, 7, partners(state, state.tracks[0].clips[1]));
    expect(targets).toContain(30); // clip 10's end, also clip 11's start
    expect(targets).not.toContain(90);
    expect(targets).toEqual(expect.arrayContaining([0, 7, 45]));
  });

  it("drops a clip by whichever edge is nearer a target, never before 0", () => {
    const clip = { start: 30, end: 90 };
    // Its end lands 3 frames from 120: snapped by the end.
    expect(dropFrame(clip, 27, [0, 120], view, true)).toBe(60);
    expect(dropFrame(clip, 27, [0, 120], view, false)).toBe(57);
    expect(dropFrame(clip, -500, [], view, true)).toBe(0);
  });

  it("trims to a target but keeps at least one frame", () => {
    const clip = { start: 30, end: 90 };
    expect(trimDelta(clip, "out", -100, [], view, false)).toBe(-59);
    expect(trimDelta(clip, "in", 100, [], view, false)).toBe(59);
    expect(trimDelta(clip, "out", 8, [100], view, true)).toBe(10);
    expect(trimDelta(clip, "in", -40, [], view, false)).toBe(-30);
  });
});

describe("hit testing", () => {
  const state = described();
  const rows = layout(state);
  const view = { pxPerFrame: 4, scroll: 0 };
  const rowY = (name) => rows.find((r) => r.track.name === name).y + 10;

  it("stacks video above audio with V2 over V1", () => {
    expect(rows.map((r) => r.track.name)).toEqual(["V2", "V1", "A1", "A2"]);
    expect(rows[0].y).toBe(RULER_HEIGHT);
    expect(rows[1].y).toBe(RULER_HEIGHT + TRACK_HEIGHT);
  });

  it("finds the ruler, a clip and empty track", () => {
    expect(hit(state, rows, view, 100, 5)).toEqual({ kind: "ruler" });
    expect(hit(state, rows, view, frameToX(40, view), rowY("V1"))).toMatchObject({ kind: "clip", clip: { id: 11 } });
    expect(hit(state, rows, view, frameToX(40, view), rowY("V2"))).toMatchObject({ kind: "empty" });
  });

  it("gives only the selected clip handles, reaching outside it", () => {
    const outside = frameToX(90, view) + EDGE_REACH - 2;
    expect(hit(state, rows, view, outside, rowY("V1"), 11)).toMatchObject({ kind: "edge", edge: "out", clip: { id: 11 } });
    // Unselected: the same point is empty track, not a trim.
    expect(hit(state, rows, view, outside, rowY("V1"))).toMatchObject({ kind: "empty" });
    // An inner edge of an unselected neighbor selects, it does not trim.
    expect(hit(state, rows, view, frameToX(30, view) - 2, rowY("V1"), 11)).toMatchObject({ kind: "edge", edge: "in" });
    expect(hit(state, rows, view, frameToX(30, view) - 2, rowY("V1"))).toMatchObject({ kind: "clip", clip: { id: 10 } });
  });
});

describe("timecode", () => {
  it("formats and parses, refusing fields out of range", () => {
    expect(timecode(30 * 61 + 7, FPS)).toBe("00:01:01:07");
    expect(timecode(0, { num: 30000, den: 1001 })).toBe("00:00:00:00");
    expect(parseTimecode("00:01:01:07", FPS)).toBe(30 * 61 + 7);
    expect(parseTimecode("1:15", FPS)).toBe(45);
    expect(parseTimecode("120", FPS)).toBe(120);
    expect(parseTimecode("00:00:00:30", FPS)).toBeNull();
    expect(parseTimecode("00:61:00:00", FPS)).toBeNull();
    expect(parseTimecode("soon", FPS)).toBeNull();
  });
});

// ---------------------------------------------------------------------------
// The app, mounted

const html = readFileSync(resolve(process.cwd(), "../crates/deckjack-video/ui/index.html"), "utf8");
const markup = html.slice(html.indexOf("<body>") + 6, html.indexOf("<script"));

/** Resolved promises and timers run; enough turns for chained awaits. */
async function settle() {
  for (let i = 0; i < 20; i++) await Promise.resolve();
}

function rig(options = {}) {
  document.body.innerHTML = markup;
  const calls = [];
  const timers = [];
  const renders = [];
  const state = { current: described(), exports: [], waits: [] };
  const bus = {
    async invoke(command, params = {}) {
      calls.push({ command, params });
      switch (command) {
        case "video.project.list":
          return { projects: [{ name: "Cut", modified: 1, open: false }] };
        case "video.project.open":
          return { opened: { recovery: options.recovery ?? null, imported_sidecar: null }, state: state.current };
        case "video.project.describe":
          return state.current;
        case "video.project.wait":
          return new Promise((res) => state.waits.push(res));
        case "video.media.list":
          return { media: [{ path: "a.mp4" }, { path: "b.mov" }] };
        case "video.frame.render":
          if (options.holdRenders) return new Promise((res) => renders.push({ params, res }));
          return { frame: params.frame, width: 64, height: 36, mime: "image/jpeg", data: btoa(`f${params.frame}`) };
        case "video.export.start":
          // The backend may not have registered the job when the window
          // first asks for the list.
          if (options.lateJob) return new Promise((res) => (state.registerJob = res));
          state.exports = [{ id: 1, project: "Cut", size: params.size, state: "running", progress: 0.25 }];
          // `wait: true` answers only when the export ends.
          if (params.wait) return new Promise((res) => (state.finishExport = () => res(state.exports[0])));
          return state.exports[0];
        case "video.export.list":
          return { exports: state.exports };
        case "video.export.cancel":
          state.exports = state.exports.map((j) => ({ ...j, state: "cancelled" }));
          return state.exports[0];
        default:
          return { result: {}, state: state.current };
      }
    },
  };
  const urls = [];
  const app = mountVideo(document, {
    bus,
    shell: { quicklook: (p) => (calls.push({ command: "quicklook", params: { path: p } }), true) },
    text: (v) => String(v ?? "").replace(/[&<>"']/g, ""),
    setTimeout: (fn, ms) => (timers.push({ fn, ms }), timers.length),
    clearTimeout: (id) => {
      if (timers[id - 1]) timers[id - 1].fn = () => {};
    },
    createObjectURL: (blob) => (urls.push(blob), `blob:${urls.length}`),
    revokeObjectURL: () => {},
    devicePixelRatio: 1,
  });
  const sent = (command) => calls.filter((c) => c.command === command);
  return { app, calls, sent, timers, renders, state, urls };
}

async function opened(options) {
  const r = rig(options);
  await settle();
  document.querySelector('[data-project="Cut"]').click();
  await settle();
  return r;
}

const canvas = () => document.getElementById("timeline");

function pointer(type, x, y, extra = {}) {
  canvas().dispatchEvent(
    new PointerEvent(type, { clientX: x, clientY: y, pointerId: 1, pointerType: "touch", bubbles: true, ...extra }),
  );
}

afterEach(() => {
  document.body.innerHTML = "";
});

describe("the app", () => {
  it("opens a project from the list and shows its state", async () => {
    const r = await opened();
    expect(r.sent("video.project.open")[0].params).toEqual({ project: "Cut" });
    expect(document.getElementById("editor").hidden).toBe(false);
    expect(document.getElementById("projects").hidden).toBe(true);
    expect(document.getElementById("title").textContent).toBe("Cut");
    expect(document.getElementById("undo").disabled).toBe(false);
    expect(document.getElementById("redo").disabled).toBe(true);
    // The bin lists the project's media, and Videos/ lists only what is not imported.
    expect(document.querySelectorAll("#bin-items [data-item]").length).toBe(1);
    expect([...document.querySelectorAll("#files [data-path]")].map((b) => b.dataset.path)).toEqual(["b.mov"]);
    // A still was asked for at the playhead and shown from a blob: URL.
    expect(r.sent("video.frame.render")[0].params).toMatchObject({ project: "Cut", frame: 0 });
    expect(document.getElementById("still").getAttribute("src")).toBe("blob:1");
  });

  it("offers recovered changes rather than applying them", async () => {
    const r = await opened({ recovery: { id: "s1", saved_unix: 1 } });
    expect(document.getElementById("recovery").hidden).toBe(false);
    expect(r.sent("video.project.recover")).toHaveLength(0);
    // It asks; it does not claim the changes are already back.
    const text = document.getElementById("recovery-text").textContent;
    expect(text).toContain("Recover them?");
    expect(text).not.toContain("were recovered");
    document.getElementById("recover").click();
    await settle();
    expect(r.sent("video.project.recover")[0].params).toEqual({ project: "Cut" });
    expect(document.getElementById("recovery").hidden).toBe(true);
  });

  it("keeps one still in flight and shows only the newest", async () => {
    const r = await opened({ holdRenders: true });
    // The open's own request is in flight; three scrubs queue one more.
    r.app.setPlayhead(5);
    r.app.setPlayhead(6);
    r.app.setPlayhead(7);
    expect(r.renders).toHaveLength(1);
    r.renders[0].res({ frame: 0, mime: "image/jpeg", data: btoa("old") });
    await settle();
    expect(r.renders).toHaveLength(2);
    expect(r.renders[1].params.frame).toBe(7);
    r.renders[1].res({ frame: 7, mime: "image/jpeg", data: btoa("new") });
    await settle();
    expect(r.renders).toHaveLength(2);
    expect(document.getElementById("still").getAttribute("src")).toBe(`blob:${r.urls.length}`);
  });

  it("drops a still that belongs to the project open before", async () => {
    const r = await opened({ holdRenders: true });
    // Reopened while the first still is in flight: that reply is stale.
    await r.app.open("Cut");
    await settle();
    r.renders[0].res({ frame: 0, mime: "image/jpeg", data: btoa("stale") });
    await settle();
    expect(r.urls).toHaveLength(0);
  });

  it("splits the selected clip at the playhead, or every track", async () => {
    const r = await opened();
    r.app.setPlayhead(40);
    document.getElementById("split").click();
    await settle();
    expect(r.sent("video.clip.split").at(-1).params).toEqual({ project: "Cut", frame: 40 });
    r.app.select(11);
    document.getElementById("split").click();
    await settle();
    expect(r.sent("video.clip.split").at(-1).params).toEqual({ project: "Cut", frame: 40, clip: 11 });
    // A selected clip the playhead is not inside splits every track instead.
    r.app.select(10);
    document.getElementById("split").click();
    await settle();
    expect(r.sent("video.clip.split").at(-1).params).toEqual({ project: "Cut", frame: 40 });
  });

  it("nudges the chosen edge by frames, rippling when ripple is on", async () => {
    const r = await opened();
    r.app.select(11);
    expect(document.getElementById("nudge").hidden).toBe(false);
    document.querySelector('[data-nudge="-10"]').click();
    await settle();
    expect(r.sent("video.clip.trim").at(-1).params).toEqual({ project: "Cut", clip: 11, edge: "out", delta: -10, ripple: false });
    document.getElementById("edge").click();
    document.getElementById("ripple").click();
    document.querySelector('[data-nudge="1"]').click();
    await settle();
    expect(r.sent("video.clip.trim").at(-1).params).toEqual({ project: "Cut", clip: 11, edge: "in", delta: 1, ripple: true });
  });

  it("unlinks a linked clip and relinks it with its overlapping partner", async () => {
    const r = await opened();
    r.app.select(11);
    expect(document.getElementById("link").textContent).toBe("Unlink");
    document.getElementById("link").click();
    await settle();
    expect(r.sent("video.clip.unlink")[0].params).toEqual({ project: "Cut", clips: [11] });
    const loose = described();
    for (const t of loose.tracks) for (const c of t.clips) c.link = null;
    // Clip 21 overlaps 11 most but is linked elsewhere; the unlinked clip
    // on A2 is the one to pair with.
    loose.tracks[2].clips[1].link = 900;
    loose.tracks[3].clips.push({ id: 30, item: 5, name: "b.wav", start: 40, end: 60, source_in: 0, link: null, gain_db: 0, enabled: true });
    r.state.current = loose;
    document.getElementById("undo").click();
    await settle();
    r.app.select(11);
    expect(document.getElementById("link").textContent).toBe("Link");
    document.getElementById("link").click();
    await settle();
    expect(r.sent("video.clip.link")[0].params).toEqual({ project: "Cut", clips: [11, 30] });
  });

  it("a tap selects a clip and a tap on empty track clears it", async () => {
    const r = await opened();
    const view = r.app.timeline.view;
    const v1 = layout(r.app.state).find((row) => row.track.name === "V1").y + 10;
    const v2 = layout(r.app.state).find((row) => row.track.name === "V2").y + 10;
    pointer("pointerdown", frameToX(60, view), v1);
    pointer("pointerup", frameToX(60, view), v1);
    expect(r.app.selected).toBe(11);
    pointer("pointerdown", frameToX(60, view), v2);
    pointer("pointerup", frameToX(60, view), v2);
    expect(r.app.selected).toBe(null);
  });

  it("a long-press lifts a clip and dropping it moves it, snapped", async () => {
    const r = await opened();
    r.app.timeline.view = { pxPerFrame: 2, scroll: 0 };
    const view = r.app.timeline.view;
    const rows = layout(r.app.state);
    const v1 = rows.find((row) => row.track.name === "V1").y + 10;
    const x = frameToX(10, view);
    pointer("pointerdown", x, v1);
    const press = r.timers.find((t) => t.ms === LONG_PRESS_MS);
    expect(press).toBeTruthy();
    press.fn();
    expect(r.app.timeline.gesture).toBe("drag");
    // 33 frames right: clip 10 (0–30) would start at 33, but the marker at
    // 45 is within reach of its end (63)… no: 30 + 33 = 63 is not near; its
    // start (33) is 3 frames from clip 11's start at 30 and snaps there.
    pointer("pointermove", x + 33 * view.pxPerFrame, v1);
    pointer("pointerup", x + 33 * view.pxPerFrame, v1);
    await settle();
    expect(r.sent("video.clip.move").at(-1).params).toEqual({ project: "Cut", clip: 10, frame: 30, track: "V1" });
  });

  it("a finger that moves before the long-press pans instead of dragging", async () => {
    const r = await opened();
    r.app.timeline.view = { pxPerFrame: 2, scroll: 20 };
    const view = r.app.timeline.view;
    const v1 = layout(r.app.state).find((row) => row.track.name === "V1").y + 10;
    pointer("pointerdown", frameToX(40, view), v1);
    pointer("pointermove", frameToX(40, view) - 40, v1);
    pointer("pointerup", frameToX(40, view) - 40, v1);
    expect(r.sent("video.clip.move")).toHaveLength(0);
    expect(r.app.timeline.view.scroll).toBe(40);
  });

  it("dragging the selected clip's handle trims it", async () => {
    const r = await opened();
    r.app.timeline.view = { pxPerFrame: 2, scroll: 0 };
    const view = r.app.timeline.view;
    const v1 = layout(r.app.state).find((row) => row.track.name === "V1").y + 10;
    r.app.select(11);
    document.getElementById("snap").click(); // off: an exact delta
    const x = frameToX(90, view) + 4;
    pointer("pointerdown", x, v1);
    expect(r.app.timeline.gesture).toBe("trim");
    pointer("pointermove", x - 14 * view.pxPerFrame, v1);
    pointer("pointerup", x - 14 * view.pxPerFrame, v1);
    await settle();
    expect(r.sent("video.clip.trim").at(-1).params).toEqual({ project: "Cut", clip: 11, edge: "out", delta: -14, ripple: false });
  });

  it("one finger on the ruler scrubs the playhead", async () => {
    const r = await opened();
    const view = r.app.timeline.view;
    pointer("pointerdown", frameToX(12, view) + 0.5, 10);
    pointer("pointermove", frameToX(20, view) + 0.5, 10);
    expect(r.app.playhead).toBe(20);
    expect(document.getElementById("timecode").value).toBe("00:00:00:20");
    pointer("pointerup", frameToX(20, view), 10);
  });

  it("the keyboard dispatches the same commands as the buttons", async () => {
    const r = await opened();
    const key = (k, extra = {}) => document.dispatchEvent(new KeyboardEvent("keydown", { key: k, bubbles: true, ...extra }));
    key("ArrowRight");
    key("ArrowRight", { shiftKey: true });
    expect(r.app.playhead).toBe(11);
    key("z", { ctrlKey: true });
    key("z", { ctrlKey: true, shiftKey: true });
    await settle();
    expect(r.sent("video.edit.undo")).toHaveLength(1);
    expect(r.sent("video.edit.redo")).toHaveLength(1);
    r.app.select(10);
    key("Delete", { shiftKey: true });
    await settle();
    expect(r.sent("video.clip.ripple_delete")[0].params).toEqual({ project: "Cut", clips: [10] });
    key("m");
    await settle();
    expect(r.sent("video.marker.add")[0].params).toEqual({ project: "Cut", frame: 11 });
  });

  it("a typed timecode moves the playhead, and nonsense does not", async () => {
    const r = await opened();
    const field = document.getElementById("timecode");
    field.value = "00:00:01:05";
    field.dispatchEvent(new Event("change"));
    expect(r.app.playhead).toBe(35);
    field.value = "later";
    field.dispatchEvent(new Event("change"));
    expect(r.app.playhead).toBe(35);
    expect(field.value).toBe("00:00:01:05");
  });

  it("exports at the chosen size, shows progress, and cancels", async () => {
    const r = await opened();
    document.getElementById("export").click();
    document.querySelector('[data-size="720p"]').click();
    document.getElementById("loudness").checked = true;
    document.getElementById("export-start").click();
    await settle();
    // Held open until the export ends, so its progress keeps the app alive
    // with no window open.
    expect(r.sent("video.export.start")[0].params).toEqual({ project: "Cut", size: "720p", loudness_lufs: -14, wait: true });
    const job = document.querySelector('[data-job="1"]');
    expect(job.dataset.state).toBe("running");
    expect(job.textContent).toContain("25%");
    document.querySelector("[data-cancel]").click();
    await settle();
    // The sheet asks again on its poll interval.
    r.timers.filter((t) => t.ms === 500).at(-1).fn();
    await settle();
    expect(r.sent("video.export.cancel")[0].params).toEqual({ id: 1 });
    expect(document.querySelector('[data-job="1"]').dataset.state).toBe("cancelled");
  });

  it("keeps polling while a start is in flight, so a job registered late is shown", async () => {
    const r = await opened({ lateJob: true });
    document.getElementById("export").click();
    document.getElementById("export-start").click();
    await settle();
    expect(document.querySelector('[data-job="1"]')).toBeNull();
    r.state.exports = [{ id: 1, project: "Cut", size: "1080p", state: "running", progress: 0.5 }];
    const poll = r.timers.filter((t) => t.ms === 500).at(-1);
    expect(poll, "the sheet stopped asking while the start was in flight").toBeDefined();
    poll.fn();
    await settle();
    expect(document.querySelector('[data-job="1"]').dataset.state).toBe("running");
  });

  it("follows a change made elsewhere through the long poll", async () => {
    const r = await opened();
    expect(r.sent("video.project.wait")[0].params).toMatchObject({ project: "Cut", since: 3 });
    r.state.current = described({ revision: 4, dirty: true });
    r.state.waits[0]({ revision: 4, changed: true });
    await settle();
    expect(r.sent("video.project.describe")).toHaveLength(1);
    expect(document.getElementById("title").textContent).toBe("Cut •");
    // And keeps waiting from the new revision.
    expect(r.sent("video.project.wait")[1].params).toMatchObject({ since: 4 });
  });
});
