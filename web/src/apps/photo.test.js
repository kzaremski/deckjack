// @vitest-environment happy-dom
import { readFileSync } from "node:fs";
import { resolve } from "node:path";
import { beforeEach, describe, expect, it } from "vitest";

import {
  apply,
  curve,
  invert,
  maskAt,
  maskUniforms,
  outputSize,
  outputToSource,
} from "../../../crates/deckjack-photo/ui/geometry.js";
import { add, editable, move, offGraph, remove, stored } from "../../../crates/deckjack-photo/ui/curves.js";
import {
  defaults,
  dragCrop,
  fitAspect,
  mountPhoto,
  normalize,
  SAVE_DELAY_MS,
} from "../../../crates/deckjack-photo/ui/photo.js";

/**
 * The Photos app's frame code. Two halves: the formulas it keeps in step
 * with the Rust (geometry map, mask, tone curve), checked against the same
 * numbers `src/geometry.rs` and `src/color.rs` assert; and the mounted app
 * against a fake bus, for the rules that protect the operator's work: one
 * LUT call in flight with the newest recipe winning, saves on the last seen
 * revision that stop on a conflict, exports that carry a job a cancel can
 * name, and `?path=` that only reads.
 */

const close = (a, b, tol = 1e-9) => Math.abs(a - b) <= tol;
const pt = (m, x, y) => apply(m, x, y);
const near = (got, want) => close(got[0], want[0]) && close(got[1], want[1]);

describe("geometry, as src/geometry.rs computes it", () => {
  it("is the identity for the default geometry", () => {
    const m = outputToSource(defaults().geometry, 4000, 3000);
    expect(near(pt(m, 0.3, 0.7), [0.3, 0.7])).toBe(true);
    expect(outputSize(defaults().geometry, 4000, 3000)).toEqual([4000, 3000]);
  });

  it("maps a crop, a clockwise turn and the flips like the backend", () => {
    const crop = { ...defaults().geometry, crop: { x: 0.25, y: 0.5, w: 0.5, h: 0.25 } };
    expect(near(pt(outputToSource(crop, 4000, 3000), 1, 1), [0.75, 0.75])).toBe(true);
    expect(outputSize(crop, 4000, 3000)).toEqual([2000, 750]);
    const cw = { ...defaults().geometry, quarter_turns: 1 };
    const m = outputToSource(cw, 4000, 3000);
    expect(near(pt(m, 0, 0), [0, 1])).toBe(true);
    expect(near(pt(m, 1, 0), [0, 0])).toBe(true);
    expect(outputSize(cw, 4000, 3000)).toEqual([3000, 4000]);
    const ccw = outputToSource({ ...defaults().geometry, quarter_turns: 3 }, 4000, 3000);
    expect(near(pt(ccw, 0, 1), [0, 0])).toBe(true);
    const flipped = outputToSource({ ...defaults().geometry, flip_v: true, quarter_turns: 1 }, 100, 100);
    expect(near(pt(flipped, 0, 0), [1, 1])).toBe(true);
    const h = outputToSource({ ...defaults().geometry, flip_h: true }, 100, 100);
    expect(near(pt(h, 0, 0.25), [1, 0.25])).toBe(true);
  });

  it("straightens clockwise about the center", () => {
    const m = outputToSource({ ...defaults().geometry, straighten_deg: 45 }, 200, 100);
    expect(near(pt(m, 0.5, 0.5), [0.5, 0.5])).toBe(true);
    const [u, v] = pt(m, 0.75, 0.5);
    const r = 50 / Math.SQRT2;
    expect(close((u - 0.5) * 200, r)).toBe(true);
    expect(close((v - 0.5) * 100, -r)).toBe(true);
  });

  it("inverts, for turning a touch into a source position", () => {
    const m = outputToSource({ ...defaults().geometry, straighten_deg: 7, quarter_turns: 1 }, 640, 480);
    const back = invert(m);
    const [u, v] = pt(m, 0.2, 0.9);
    expect(near(pt(back, u, v), [0.2, 0.9])).toBe(true);
  });
});

describe("the mask and the curve, as src/color.rs computes them", () => {
  const radial = (feather, invert = false, angle_deg = 0) => ({
    mask: { kind: "radial", cx: 0.5, cy: 0.5, rx: 0.25, ry: 0.125, angle_deg, feather },
    invert,
  });

  it("is full inside, empty outside and smoothstep between", () => {
    const u = maskUniforms(radial(0.5), 2000, 1000);
    expect(maskAt(u, [0.5, 0.5])).toBe(1);
    expect(maskAt(u, [0.62, 0.5])).toBe(1);
    expect(maskAt(u, [0.76, 0.5])).toBe(0);
    expect(close(maskAt(u, [0.6875, 0.5]), 0.5, 1e-6)).toBe(true);
    expect(close(maskAt(u, [0.5, 0.5 + 0.1875]), 0.5, 1e-6)).toBe(true);
    const inv = maskUniforms(radial(0.5, true), 2000, 1000);
    expect(maskAt(inv, [0.5, 0.5])).toBe(0);
  });

  it("turns clockwise and keeps a hard edge finite", () => {
    // 45 degrees clockwise with y down: the long axis points down-right.
    const r = maskUniforms(radial(0.5, false, 45), 1000, 1000);
    expect(maskAt(r, [0.65, 0.65])).toBeGreaterThan(0);
    expect(maskAt(r, [0.65, 0.35])).toBe(0);
    // 0.2 along the long axis is d = 0.8, 60% into the feather:
    // 1 - 0.6^2 (3 - 1.2) = 0.352.
    const along = 0.2 / Math.SQRT2;
    expect(close(maskAt(r, [0.5 + along, 0.5 + along]), 0.352, 1e-6)).toBe(true);
    const hard = maskUniforms(radial(0), 1000, 1000);
    expect(maskAt(hard, [0.74, 0.5])).toBe(1);
    expect(maskAt(hard, [0.7501, 0.5])).toBe(0);
    // Exactly on the edge: without the feather floor this is 0 / 0.
    expect(maskAt(hard, [0.75, 0.5])).toBe(0);
  });

  it("draws the same monotone curve the LUT applies", () => {
    const pts = [
      [0, 0],
      [0.25, 0.15],
      [0.75, 0.85],
      [1, 1],
    ];
    const f = curve(pts);
    for (const [x, y] of pts) expect(close(f(x), y, 1e-9)).toBe(true);
    expect(close(f(0.5), 0.5, 1e-9)).toBe(true);
    let last = -1;
    for (let i = 0; i <= 1000; i++) {
      const y = f(i / 1000);
      expect(y).toBeGreaterThanOrEqual(last - 1e-9);
      last = y;
    }
    const flat = curve([
      [0, 0],
      [0.4, 0.5],
      [0.6, 0.5],
      [1, 1],
    ]);
    for (let i = 40; i <= 60; i++) expect(close(flat(i / 100), 0.5, 1e-9)).toBe(true);
    expect(curve([])(0.3)).toBe(0.3);
    // A steep rise into a shoulder: the averaged tangent at the knee is 5,
    // and only the Fritsch-Carlson limit keeps the segment after it from
    // bulging above its end point.
    const knee = curve([
      [0, 0],
      [0.1, 0.9],
      [0.2, 1],
      [1, 1],
    ]);
    for (let i = 100; i <= 200; i++) expect(knee(i / 1000)).toBeLessThanOrEqual(1 + 1e-9);
  });
});

describe("the curves editor's rules", () => {
  it("starts from the diagonal and stores nothing for it", () => {
    expect(editable([])).toEqual([
      [0, 0],
      [1, 1],
    ]);
    expect(stored(editable([]))).toEqual([]);
  });

  it("adds in order, refuses crowding, moves between neighbors and removes inner points only", () => {
    let { points, index } = add(editable([]), 0.5, 0.6);
    expect(index).toBe(1);
    expect(points).toEqual([
      [0, 0],
      [0.5, 0.6],
      [1, 1],
    ]);
    expect(add(points, 0.51, 0.2).index).toBe(-1);
    points = move(points, 1, 1.5, 2);
    expect(points[1][0]).toBeLessThan(1);
    expect(points[1][1]).toBe(1);
    expect(remove(points, 0)).toBe(points);
    expect(remove(points, 1)).toHaveLength(2);
    expect(offGraph(0.5, 1.2)).toBe(true);
    expect(offGraph(0.5, 1.05)).toBe(false);
  });
});

describe("crop presets and handles", () => {
  it("fits the largest centered crop of a ratio", () => {
    // A 3:2 photo cropped to 4:5: full height, 4/5 * 2/3 of the width.
    const c = fitAspect({ x: 0, y: 0, w: 1, h: 1 }, 4 / 5, 3 / 2);
    expect(close(c.h, 1)).toBe(true);
    expect(close(c.w, (4 / 5) / (3 / 2))).toBe(true);
    expect(close(c.x + c.w / 2, 0.5)).toBe(true);
  });

  it("keeps a corner drag on its aspect and inside the photo", () => {
    const k = 1;
    const c = dragCrop({ x: 0.2, y: 0.2, w: 0.4, h: 0.4 }, "se", 0.3, 0.05, k);
    expect(close(c.w / c.h, k)).toBe(true);
    expect(c.y + c.h).toBeLessThanOrEqual(1 + 1e-9);
    const moved = dragCrop({ x: 0.2, y: 0.2, w: 0.4, h: 0.4 }, "move", 0.9, -0.9);
    expect(moved).toEqual({ x: 0.6, y: 0, w: 0.4, h: 0.4 });
  });
});

// ---- The mounted app --------------------------------------------------------

const html = readFileSync(resolve(process.cwd(), "../crates/deckjack-photo/ui/index.html"), "utf8");
const markup = html.slice(html.indexOf("<body>") + 6, html.indexOf("<script"));

/** A LUT answer: 2^3 half floats, base64. */
const LUT = { size: 17, encoding: "srgb", global: btoa("\0".repeat(48)), locals: [] };

function fakeBus() {
  const calls = [];
  const waiting = [];
  let rev = 0;
  const answers = {
    "photo.list": () => ({ dir: "Photos", entries: [] }),
    "photo.open": (p) => ({
      path: p.path.replace(/^\//, ""),
      format: "jpeg",
      info: { width: 400, height: 300 },
      proxy_encoding: "srgb",
      edit: { rev, recipe: {}, history: [], position: 0, matches_original: true },
    }),
    "photo.proxy": () => ({ width: 400, height: 300, encoding: "srgb", png: btoa("png") }),
    "photo.edit.save": (p) => {
      if (p.base_rev !== rev) throw new Error("edited elsewhere since you opened it");
      rev += 1;
      return { rev, recipe: p.recipe, position: rev, history: [] };
    },
    "photo.edit.step": (p) => {
      if (p.base_rev !== rev) throw new Error("edited elsewhere since you opened it");
      rev += 1;
      return { rev, recipe: {}, position: 0, history: [] };
    },
    "photo.export": () => ({ path: "Photos/a-edit.jpg", width: 400, height: 300, bytes: 1 }),
    "photo.export.cancel": (p) => ({ job: p.job, cancelled: true }),
  };
  return {
    calls,
    waiting,
    /** Commands answered only when the test calls `ok()` on their entry in
     * `waiting`; the answer is computed then, as the node would. */
    hold: new Set(),
    bumpRev() {
      rev += 1;
    },
    invoke(command, params = {}, onProgress) {
      calls.push({ command, params: JSON.parse(JSON.stringify(params)) });
      if (command === "photo.lut" || (command === "photo.export" && this.holdExport)) {
        return new Promise((ok, no) => waiting.push({ command, params, ok, no, onProgress }));
      }
      if (this.hold.has(command)) {
        return new Promise((ok, no) =>
          waiting.push({
            command,
            params,
            ok: () => {
              try {
                ok(answers[command](params));
              } catch (e) {
                no(e);
              }
            },
            no,
          }),
        );
      }
      try {
        return Promise.resolve(answers[command](params));
      } catch (e) {
        return Promise.reject(e);
      }
    },
  };
}

const fakeRenderer = () => ({ setProxy() {}, setLut() {}, draw() {} });
const tick = () => new Promise((r) => setTimeout(r, 0));
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

/** Answer every LUT call waiting, then let the coalescer send the next. */
async function drainLuts(bus) {
  for (let i = 0; i < 10 && bus.waiting.length; i++) {
    bus.waiting.shift().ok(LUT);
    await tick();
  }
}

describe("the mounted app", () => {
  let bus;
  let app;

  beforeEach(async () => {
    document.body.innerHTML = markup;
    bus = fakeBus();
    app = mountPhoto(document, {
      bus,
      createRenderer: fakeRenderer,
      decodeProxy: async () => ({}),
      shell: { quicklook() {} },
      win: window,
      random: () => 0.5,
    });
  });

  it("only reads when started on a path", async () => {
    const started = app.start("/Photos/a.jpg");
    await tick();
    await drainLuts(bus);
    await started;
    const writes = bus.calls.filter((c) => /save|step|export/.test(c.command));
    expect(writes).toEqual([]);
    expect(bus.calls[0]).toEqual({ command: "photo.open", params: { path: "/Photos/a.jpg" } });
  });

  it("keeps one LUT call in flight and sends the newest recipe next", async () => {
    const started = app.start("Photos/a.jpg");
    await tick();
    await drainLuts(bus);
    await started;
    bus.calls.length = 0;
    for (const ev of [0.1, 0.2, 0.3, 0.4]) {
      app.edit((r) => (r.exposure_ev = ev), { label: "Exposure", settled: false });
    }
    await tick();
    const luts = () => bus.calls.filter((c) => c.command === "photo.lut");
    expect(luts()).toHaveLength(1);
    expect(luts()[0].params.size).toBe(17);
    bus.waiting.shift().ok(LUT);
    await tick();
    expect(luts()).toHaveLength(2);
    // The second carries the last value, not the second.
    expect(luts()[1].params.recipe.exposure_ev).toBe(0.4);
    await drainLuts(bus);
    expect(luts()).toHaveLength(2);
  });

  it("saves once after the edits settle, on the revision it opened", async () => {
    const started = app.start("Photos/a.jpg");
    await tick();
    await drainLuts(bus);
    await started;
    app.edit((r) => (r.exposure_ev = 0.5), { label: "Exposure" });
    app.edit((r) => (r.saturation = 0.2), { label: "Saturation" });
    await drainLuts(bus);
    await sleep(SAVE_DELAY_MS + 50);
    const saves = bus.calls.filter((c) => c.command === "photo.edit.save");
    expect(saves).toHaveLength(1);
    expect(saves[0].params.base_rev).toBe(0);
    expect(saves[0].params.recipe.exposure_ev).toBe(0.5);
    expect(saves[0].params.recipe.saturation).toBe(0.2);
    expect(app.state.rev).toBe(1);
  });

  it("stops saving once another window's edit is found, instead of overwriting it", async () => {
    const started = app.start("Photos/a.jpg");
    await tick();
    await drainLuts(bus);
    await started;
    bus.bumpRev(); // the phone saved meanwhile
    app.edit((r) => (r.exposure_ev = 1), { label: "Exposure" });
    await drainLuts(bus);
    await app.save();
    expect(document.getElementById("status").textContent).toMatch(/edited elsewhere/);
    app.edit((r) => (r.exposure_ev = 2), { label: "Exposure" });
    await drainLuts(bus);
    await app.save();
    expect(bus.calls.filter((c) => c.command === "photo.edit.save")).toHaveLength(1);
    expect(document.getElementById("undo").disabled).toBe(true);
  });

  it("waits out every save before a step, even with two callers waiting", async () => {
    const started = app.start("Photos/a.jpg");
    await tick();
    await drainLuts(bus);
    await started;
    bus.hold.add("photo.edit.save");
    const held = (c) => bus.waiting.filter((w) => w.command === c);
    app.edit((r) => (r.exposure_ev = 0.5), { label: "Exposure" });
    const first = app.save();
    await tick();
    expect(held("photo.edit.save")).toHaveLength(1);
    // A second edit while the first is being written; then the debounced
    // save and an undo both wait on the first.
    app.edit((r) => (r.exposure_ev = 0.7), { label: "Exposure" });
    const second = app.save();
    const undo = app.step("undo");
    await tick();
    held("photo.edit.save")[0].ok();
    bus.waiting.splice(bus.waiting.indexOf(held("photo.edit.save")[0]), 1);
    await tick();
    await tick();
    // The second save is in flight, and the undo has not gone out on the
    // revision before it.
    expect(held("photo.edit.save")).toHaveLength(1);
    expect(bus.calls.filter((c) => c.command === "photo.edit.step")).toHaveLength(0);
    held("photo.edit.save")[0].ok();
    await Promise.all([first, second, undo, drainLuts(bus)]);
    const step = bus.calls.find((c) => c.command === "photo.edit.step");
    expect(step.params.base_rev).toBe(2);
    expect(app.state.stale).toBe(false);
  });

  for (const refused of [false, true]) {
    it(`keeps a save's ${refused ? "refusal" : "revision"} off a photo opened while it was in flight`, async () => {
      const started = app.start("Photos/a.jpg");
      await tick();
      await drainLuts(bus);
      await started;
      bus.hold.add("photo.open");
      bus.hold.add("photo.edit.save");
      const held = (c) => bus.waiting.find((w) => w.command === c);
      const opening = app.open("Photos/b.jpg");
      await tick();
      // An edit to a.jpg lands while b.jpg is opening; its save goes to a.jpg.
      app.edit((r) => (r.exposure_ev = 0.5), { label: "Exposure" });
      const saving = app.save();
      await tick();
      expect(held("photo.edit.save").params.path).toBe("Photos/a.jpg");
      const open = held("photo.open");
      open.ok();
      bus.waiting.splice(bus.waiting.indexOf(open), 1);
      await tick();
      expect(app.state.path).toBe("Photos/b.jpg");
      if (refused) bus.bumpRev(); // a.jpg was saved elsewhere meanwhile
      const save = held("photo.edit.save");
      save.ok();
      bus.waiting.splice(bus.waiting.indexOf(save), 1);
      await saving;
      await drainLuts(bus);
      await opening;
      // b.jpg's sidecar was at revision 0 when it opened, and still is, and
      // nothing about a.jpg stops b.jpg from saving.
      expect(app.state.rev).toBe(0);
      expect(app.state.stale).toBe(false);
    });
  }

  it("exports with a job name that its cancel names too", async () => {
    const started = app.start("Photos/a.jpg");
    await tick();
    await drainLuts(bus);
    await started;
    bus.holdExport = true;
    app.state.tab = "export";
    app.renderPanel();
    const running = app.runExport();
    await tick();
    await tick();
    const exp = bus.waiting.find((w) => w.command === "photo.export");
    expect(exp).toBeTruthy();
    exp.onProgress({ fraction: 0.4 });
    expect(document.getElementById("ex-progress").value).toBeCloseTo(0.4);
    await app.cancelExport();
    const cancel = bus.calls.find((c) => c.command === "photo.export.cancel");
    expect(cancel.params.job).toBe(exp.params.job);
    exp.no(new Error("export cancelled; nothing was written"));
    await running;
    expect(document.getElementById("status").textContent).toMatch(/cancelled/);
    expect(document.getElementById("ex-cancel").hidden).toBe(true);
  });

  it("fills a partial recipe from the backend to full shape", () => {
    const r = normalize({ exposure_ev: 1, geometry: { quarter_turns: 2 } });
    expect(r.exposure_ev).toBe(1);
    expect(r.geometry.quarter_turns).toBe(2);
    expect(r.geometry.crop).toEqual({ x: 0, y: 0, w: 1, h: 1 });
    expect(r.levels.red.white).toBe(1);
  });
});
