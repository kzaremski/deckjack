/**
 * The Photos app's UI, as one mount function with its dependencies passed
 * in, so `web/src/apps/photo.test.js` can drive it with a fake bus and no
 * GPU.
 *
 * Everything it shows arrives as a command result: the proxy as base64 PNG,
 * the LUTs as base64 half floats. It never fetches a URL, which is what the
 * brokered frame (`connect-src 'none'`, APP-UI-ISOLATION §4.2) requires.
 *
 * Three rules hold the whole file together:
 *
 * - **One LUT call in flight, the latest recipe wins.** A slider sends far
 *   more changes than the backend should bake; the coalescer below sends
 *   the newest recipe when the previous answer lands. 17^3 while dragging,
 *   33^3 when the control settles (spike 1).
 * - **Saves are debounced, based on the revision this window last saw,** and
 *   a refusal ("edited elsewhere") stops saving until the photo is reopened,
 *   rather than overwriting the other window's edit.
 * - **Nothing is saved or exported without a user action.** `?path=` only
 *   opens (a read).
 */

import { add, editable, move, nearest, offGraph, remove, stored, TOUCH_OFFSET_PX } from "./curves.js";
import {
  apply,
  curve,
  invert,
  maskUniforms,
  outputSize,
  outputToSource,
  turnedAspect,
} from "./geometry.js";

export const SAVE_DELAY_MS = 500;
export const LONG_PRESS_MS = 500;
export const MAX_MASKS = 4;
/** The proxy's long edge: enough for a 1080 px phone screen, and a RAW
 * proxy of this size is ~2.5 MB of PNG (spike 2). */
export const PROXY_EDGE = 1600;

const level = () => ({ black: 0, white: 1, gamma: 1, out_black: 0, out_white: 1 });

/** A full recipe with every default filled in, so controls can bind to any field. */
export function defaults() {
  return {
    exposure_ev: 0,
    white_balance: { temperature: 0, tint: 0 },
    levels: { master: level(), red: level(), green: level(), blue: level() },
    curves: { master: [], red: [], green: [], blue: [] },
    saturation: 0,
    geometry: {
      quarter_turns: 0,
      flip_h: false,
      flip_v: false,
      straighten_deg: 0,
      crop: { x: 0, y: 0, w: 1, h: 1 },
    },
    masks: [],
  };
}

/** Fill a recipe from the backend (which may omit defaults) to full shape. */
export function normalize(r) {
  const d = defaults();
  const merge = (base, over) => {
    if (over === undefined || over === null) return base;
    if (Array.isArray(base) || typeof base !== "object") return over;
    const out = { ...base };
    for (const k of Object.keys(over)) out[k] = merge(base[k], over[k]);
    return out;
  };
  return merge(d, r ?? {});
}

export function getField(obj, path) {
  return path.split(".").reduce((o, k) => (o == null ? undefined : o[k]), obj);
}

export function setField(obj, path, value) {
  const keys = path.split(".");
  let o = obj;
  for (const k of keys.slice(0, -1)) o = o[k];
  o[keys[keys.length - 1]] = value;
}

/** The sliders of each panel: field, label, range, step. */
export const SLIDERS = {
  light: [
    ["exposure_ev", "Exposure", -5, 5, 0.01],
    ["white_balance.temperature", "Temperature", -100, 100, 1],
    ["white_balance.tint", "Tint", -100, 100, 1],
    ["saturation", "Saturation", -1, 1, 0.01],
  ],
  levels: [
    ["black", "Black point", 0, 1, 0.005],
    ["white", "White point", 0, 1, 0.005],
    ["gamma", "Midtones", 0.1, 10, 0.01],
    ["out_black", "Output black", 0, 1, 0.005],
    ["out_white", "Output white", 0, 1, 0.005],
  ],
  crop: [["geometry.straighten_deg", "Straighten", -45, 45, 0.1]],
  local: [
    ["exposure_ev", "Exposure", -5, 5, 0.01],
    ["saturation", "Saturation", -1, 1, 0.01],
    ["mask.rx", "Width", 0.01, 1, 0.005],
    ["mask.ry", "Height", 0.01, 1, 0.005],
    ["mask.angle_deg", "Angle", -180, 180, 1],
    ["mask.feather", "Feather", 0, 1, 0.01],
  ],
};

/** Crop aspect presets: width over height, or null for free / the photo's own. */
export const ASPECTS = [
  ["Free", null],
  ["Original", "original"],
  ["1:1", 1],
  ["4:5", 4 / 5],
  ["3:2", 3 / 2],
  ["16:9", 16 / 9],
];

/**
 * The largest crop of `ratio` (width / height, in pixels) centered on
 * `crop`, in a turned image of `aspect` (its width / height).
 */
export function fitAspect(crop, ratio, aspect) {
  // In normalized units, w/h = ratio / aspect.
  const k = ratio / aspect;
  let w = crop.w;
  let h = w / k;
  if (h > crop.h) {
    h = crop.h;
    w = h * k;
  }
  const cx = crop.x + crop.w / 2;
  const cy = crop.y + crop.h / 2;
  return {
    x: Math.min(Math.max(0, cx - w / 2), 1 - w),
    y: Math.min(Math.max(0, cy - h / 2), 1 - h),
    w,
    h,
  };
}

/**
 * Drag one crop handle (`n`, `s`, `e`, `w`, a corner, or `move`) by
 * `(dx, dy)` normalized units. With `k` (normalized w/h) the corners keep
 * the aspect.
 */
export function dragCrop(crop, handle, dx, dy, k = null) {
  const MIN = 0.05;
  let { x, y, w, h } = crop;
  if (handle === "move") {
    x = Math.min(Math.max(0, x + dx), 1 - w);
    y = Math.min(Math.max(0, y + dy), 1 - h);
    return { x, y, w, h };
  }
  let x1 = x + w;
  let y1 = y + h;
  if (handle.includes("w")) x = Math.min(Math.max(0, x + dx), x1 - MIN);
  if (handle.includes("e")) x1 = Math.max(Math.min(1, x1 + dx), x + MIN);
  if (handle.includes("n")) y = Math.min(Math.max(0, y + dy), y1 - MIN);
  if (handle.includes("s")) y1 = Math.max(Math.min(1, y1 + dy), y + MIN);
  w = x1 - x;
  h = y1 - y;
  if (k && handle.length === 2) {
    // Keep the aspect from the corner's opposite: height follows width,
    // shrinking both if that runs off the image.
    let nh = w / k;
    const room = handle.includes("n") ? y1 : 1 - y;
    if (nh > room) {
      nh = room;
      w = nh * k;
      if (handle.includes("w")) x = x1 - w;
    }
    if (handle.includes("n")) y = y1 - nh;
    h = nh;
  }
  return { x, y, w, h };
}

function b64bytes(b64) {
  const bin = atob(b64);
  const out = new Uint8Array(bin.length);
  for (let i = 0; i < bin.length; i++) out[i] = bin.charCodeAt(i);
  return out;
}

function halves(b64) {
  const u = b64bytes(b64);
  return new Uint16Array(u.buffer, 0, u.length >> 1);
}

const clone = (v) => JSON.parse(JSON.stringify(v));

/**
 * Mount the app into `doc`.
 *
 * `deps`: `bus` (`invoke(command, params, onProgress)`), `createRenderer`
 * and `decodeProxy` from `gl.js` (or test doubles; `createRenderer` may
 * return null for no WebGL2), `shell` (`quicklook`), `win` (for timers,
 * messages and keys), `random` (export job names).
 */
export function mountPhoto(doc, deps) {
  const { bus, createRenderer, decodeProxy, shell } = deps;
  const win = deps.win ?? doc.defaultView;
  const $ = (id) => doc.getElementById(id);
  const status = $("status");
  const canvas = $("canvas");
  const view = $("view");
  const overlay = $("overlay");
  const say = (text) => {
    status.textContent = text;
  };
  const fail = (e) => say(e instanceof Error ? e.message : String(e));

  const renderer = createRenderer(canvas);
  const state = {
    path: null,
    rev: 0,
    recipe: defaults(),
    proxy: null, // { width, height, encoding }
    tab: "light",
    channel: "master",
    mask: -1,
    before: false,
    overlay: false,
    stale: false,
    aspect: null,
    position: 0,
    history: [],
  };
  const luts = { current: null, before: null };

  // ---- Preview -----------------------------------------------------------

  /** The geometry the canvas shows: in crop mode the uncropped image. */
  function shownGeometry() {
    const g = state.before ? defaults().geometry : clone(state.recipe.geometry);
    if (state.tab === "crop" && !state.before) g.crop = { x: 0, y: 0, w: 1, h: 1 };
    return g;
  }

  /** Canvas layout: CSS size, and the map from output uv to source uv. */
  function layout() {
    if (!state.proxy) return null;
    const g = shownGeometry();
    const [ow, oh] = outputSize(g, state.proxy.width, state.proxy.height);
    const vw = Math.max(1, view.clientWidth || 360);
    const vh = Math.max(1, view.clientHeight || 240);
    const scale = Math.min(vw / ow, vh / oh);
    const cssW = Math.max(1, Math.floor(ow * scale));
    const cssH = Math.max(1, Math.floor(oh * scale));
    const map = outputToSource(g, state.proxy.width, state.proxy.height);
    return { g, cssW, cssH, map };
  }

  let frame = 0;
  function draw() {
    if (frame) return;
    frame = (win.requestAnimationFrame ?? ((f) => win.setTimeout(f, 16)))(() => {
      frame = 0;
      drawNow();
    });
  }

  function drawNow() {
    const l = layout();
    if (!l) return;
    canvas.style.width = `${l.cssW}px`;
    canvas.style.height = `${l.cssH}px`;
    const dpr = win.devicePixelRatio || 1;
    const masks = state.before
      ? []
      : state.recipe.masks.map((m) => maskUniforms(m, state.proxy.width, state.proxy.height));
    if (renderer) {
      renderer.draw({
        width: Math.round(l.cssW * dpr),
        height: Math.round(l.cssH * dpr),
        map: l.map,
        masks,
        overlay: state.overlay && state.tab === "local",
      });
    }
    drawOverlay(l);
  }

  function useLuts(set) {
    if (!renderer || !set) return;
    renderer.setLut(-1, set.size, set.global);
    set.locals.forEach((data, i) => renderer.setLut(i, set.size, data));
  }

  // One LUT request in flight; the newest recipe goes next.
  let lutBusy = false;
  let lutWanted = 0;
  async function refreshLut(size) {
    lutWanted = Math.max(lutWanted, size);
    if (lutBusy || !state.path) return;
    lutBusy = true;
    try {
      while (lutWanted) {
        const want = lutWanted;
        lutWanted = 0;
        const path = state.path;
        if (!renderer) {
          await fallbackRender();
          continue;
        }
        const r = await bus.invoke("photo.lut", { path, recipe: state.recipe, size: want });
        if (path !== state.path) continue;
        luts.current = { size: r.size, global: halves(r.global), locals: r.locals.map(halves) };
        if (!state.before) useLuts(luts.current);
        draw();
      }
    } catch (e) {
      fail(e);
    } finally {
      lutBusy = false;
    }
  }

  /** No WebGL2: the backend renders the whole preview (fallback A). */
  async function fallbackRender() {
    const l = layout();
    const r = await bus.invoke("photo.render", {
      path: state.path,
      recipe: state.before ? defaults() : state.recipe,
      long_edge: Math.min(2048, Math.round(Math.max(l.cssW, l.cssH) * (win.devicePixelRatio || 1))),
    });
    const bitmap = await createImageBitmap(new Blob([b64bytes(r.jpeg)], { type: "image/jpeg" }));
    canvas.width = bitmap.width;
    canvas.height = bitmap.height;
    canvas.getContext("2d")?.drawImage(bitmap, 0, 0);
  }

  // ---- Saving ------------------------------------------------------------

  let saveTimer = null;
  let saving = null;
  let pendingLabel = null;

  function saveSoon(label) {
    if (state.stale) return;
    pendingLabel = label;
    win.clearTimeout(saveTimer);
    saveTimer = win.setTimeout(() => void save(), SAVE_DELAY_MS);
  }

  /** Write the pending edit now. Resolves when nothing is left to write:
   * every caller waits out the save in flight and then any save started
   * after it, so `await save()` before a step, an open or an export means
   * the sidecar is at this window's last revision. */
  async function save() {
    win.clearTimeout(saveTimer);
    saveTimer = null;
    for (;;) {
      // Several callers can be waiting on one save; the first to wake
      // starts the next, and the rest wait on that one too.
      while (saving) await saving;
      if (!pendingLabel || !state.path || state.stale) return;
      const label = pendingLabel;
      pendingLabel = null;
      const path = state.path;
      saving = (async () => {
        try {
          const r = await bus.invoke("photo.edit.save", {
            path,
            recipe: state.recipe,
            base_rev: state.rev,
            label,
          });
          // Another photo was opened meanwhile: this revision is that
          // file's sidecar's, not the one now shown.
          if (state.path !== path) return;
          state.rev = r.rev;
          state.position = r.position;
          state.history = r.history ?? state.history;
          say("Saved");
          updateButtons();
        } catch (e) {
          if (state.path === path && /edited elsewhere/.test(String(e?.message ?? e))) {
            // Stop: saving again would put this window's edit over the
            // other one's, which is the thing the revision exists to stop.
            state.stale = true;
            updateButtons();
          }
          fail(e);
        } finally {
          saving = null;
        }
      })();
    }
  }

  async function step(direction) {
    if (!state.path || state.stale) return;
    await save();
    try {
      const r = await bus.invoke("photo.edit.step", {
        path: state.path,
        base_rev: state.rev,
        direction,
      });
      state.rev = r.rev;
      state.position = r.position;
      state.history = r.history ?? state.history;
      state.recipe = normalize(r.recipe);
      state.mask = Math.min(state.mask, state.recipe.masks.length - 1);
      renderPanel();
      void refreshLut(33);
      draw();
      updateButtons();
      say(direction === "undo" ? "Undone" : "Redone");
    } catch (e) {
      if (/edited elsewhere/.test(String(e?.message ?? e))) state.stale = true;
      fail(e);
    }
  }

  function updateButtons() {
    $("undo").disabled = !state.path || state.stale || state.position <= 0;
    $("redo").disabled =
      !state.path || state.stale || state.position >= state.history.length - 1;
    $("export").disabled = !state.path;
  }

  /** Every control funnels here: change the recipe, preview it, save later. */
  function edit(mutate, { label, colors = true, settled = true }) {
    mutate(state.recipe);
    if (colors) void refreshLut(settled ? 33 : 17);
    draw();
    if (settled) saveSoon(label);
  }

  // ---- Opening -----------------------------------------------------------

  async function open(path) {
    await save();
    say(`Opening ${path}…`);
    try {
      const info = await bus.invoke("photo.open", { path });
      state.path = info.path;
      state.rev = info.edit.rev;
      state.position = info.edit.position;
      state.history = info.edit.history ?? [];
      state.recipe = normalize(info.edit.recipe);
      state.stale = false;
      state.mask = state.recipe.masks.length ? 0 : -1;
      $("name").textContent = info.path.split("/").pop();
      $("picker").hidden = true;
      const proxy = await bus.invoke(
        "photo.proxy",
        { path: info.path, long_edge: PROXY_EDGE },
        () => say("Decoding…"),
      );
      state.proxy = { width: proxy.width, height: proxy.height, encoding: proxy.encoding };
      if (renderer) {
        renderer.setProxy(await decodeProxy(b64bytes(proxy.png)));
        const b = await bus.invoke("photo.lut", { path: info.path, recipe: defaults(), size: 33 });
        luts.before = { size: b.size, global: halves(b.global), locals: [] };
      }
      renderPanel();
      updateButtons();
      await refreshLut(33);
      draw();
      const notes = [];
      if (info.note) notes.push(info.note);
      if (!info.edit.matches_original) {
        notes.push("This edit was saved for different bytes: the photo changed since.");
      }
      if (!renderer) notes.push("No WebGL2 here, so the preview is rendered by the node.");
      say(notes.join(" ") || `${info.info.width} × ${info.info.height}`);
    } catch (e) {
      fail(e);
    }
  }

  async function browse(dir = "Photos") {
    try {
      const r = await bus.invoke("photo.list", { dir });
      const list = $("picker-list");
      list.replaceChildren();
      if (r.dir !== "Photos") {
        const up = doc.createElement("button");
        up.className = "dj-row";
        up.textContent = "↑ Up";
        up.addEventListener("click", () => browse(r.dir.split("/").slice(0, -1).join("/")));
        list.append(up);
      }
      for (const e of r.entries) {
        const row = doc.createElement("button");
        row.className = "dj-row";
        row.dataset.path = e.path;
        const name = doc.createElement("span");
        name.className = "dj-row-name";
        name.textContent = e.kind === "folder" ? `${e.name}/` : e.name;
        const meta = doc.createElement("span");
        meta.className = "dj-row-meta";
        meta.textContent = e.kind === "folder" ? "" : `${e.format}${e.edited ? " · edited" : ""}`;
        row.append(name, meta);
        row.addEventListener("click", () => (e.kind === "folder" ? browse(e.path) : open(e.path)));
        list.append(row);
      }
      if (!r.entries.length) {
        const p = doc.createElement("p");
        p.className = "dj-empty";
        p.textContent = "No photos here. Put JPEG, PNG, TIFF or RAW files under Photos/.";
        list.append(p);
      }
      $("picker").hidden = false;
    } catch (e) {
      fail(e);
    }
  }

  // ---- Panels ------------------------------------------------------------

  function slider(host, { value, label, min, max, step, reset, onInput, onCommit }) {
    const row = doc.createElement("label");
    row.className = "slider";
    const name = doc.createElement("span");
    name.className = "slider-name";
    name.textContent = label;
    const input = doc.createElement("input");
    input.type = "range";
    input.min = String(min);
    input.max = String(max);
    input.step = String(step);
    input.value = String(value);
    const num = doc.createElement("input");
    num.type = "number";
    num.className = "slider-value";
    num.min = String(min);
    num.max = String(max);
    num.step = String(step);
    num.value = String(value);
    num.setAttribute("aria-label", label);
    const set = (v, commit) => {
      const n = Math.min(max, Math.max(min, Number(v)));
      if (!Number.isFinite(n)) return;
      input.value = String(n);
      num.value = String(Math.round(n * 1000) / 1000);
      if (commit) onCommit(n);
      else onInput(n);
    };
    input.addEventListener("input", () => set(input.value, false));
    input.addEventListener("change", () => set(input.value, true));
    num.addEventListener("change", () => set(num.value, true));
    // Double tap or double click resets, as on every slider here.
    row.addEventListener("dblclick", (e) => {
      e.preventDefault();
      set(reset, true);
    });
    row.append(name, input, num);
    host.append(row);
    return row;
  }

  function chip(host, label, pressed, onClick) {
    const b = doc.createElement("button");
    b.className = "dj-button chip";
    b.textContent = label;
    b.setAttribute("aria-pressed", pressed ? "true" : "false");
    b.addEventListener("click", onClick);
    host.append(b);
    return b;
  }

  function renderPanel() {
    for (const t of doc.querySelectorAll("[data-tab]")) {
      t.setAttribute("aria-selected", t.dataset.tab === state.tab ? "true" : "false");
    }
    const host = $("panel");
    host.replaceChildren();
    const r = state.recipe;
    const d = defaults();
    if (state.tab === "light") {
      for (const [field, label, min, max, step] of SLIDERS.light) {
        slider(host, {
          value: getField(r, field),
          label,
          min,
          max,
          step,
          reset: getField(d, field),
          onInput: (v) => edit((x) => setField(x, field, v), { label, settled: false }),
          onCommit: (v) => edit((x) => setField(x, field, v), { label }),
        });
      }
    } else if (state.tab === "levels" || state.tab === "curves") {
      const chips = doc.createElement("div");
      chips.className = "chips";
      for (const c of ["master", "red", "green", "blue"]) {
        chip(chips, c === "master" ? "RGB" : c[0].toUpperCase(), state.channel === c, () => {
          state.channel = c;
          renderPanel();
        });
      }
      host.append(chips);
      if (state.tab === "levels") {
        for (const [key, label, min, max, step] of SLIDERS.levels) {
          const field = `levels.${state.channel}.${key}`;
          slider(host, {
            value: getField(r, field),
            label,
            min,
            max,
            step,
            reset: level()[key],
            onInput: (v) => edit((x) => setField(x, field, v), { label: "Levels", settled: false }),
            onCommit: (v) => edit((x) => setField(x, field, v), { label: "Levels" }),
          });
        }
      } else {
        curvesEditor(host);
      }
    } else if (state.tab === "crop") {
      const chips = doc.createElement("div");
      chips.className = "chips";
      for (const [label, ratio] of ASPECTS) {
        chip(chips, label, state.aspect === ratio, () => {
          state.aspect = ratio;
          if (ratio !== null && state.proxy) {
            const a = turnedAspect(r.geometry, state.proxy.width, state.proxy.height);
            const k = ratio === "original" ? a : ratio;
            edit((x) => (x.geometry.crop = fitAspect(x.geometry.crop, k, a)), {
              label: `Crop ${label}`,
              colors: false,
            });
          }
          renderPanel();
        });
      }
      host.append(chips);
      const turns = doc.createElement("div");
      turns.className = "chips";
      chip(turns, "Turn left", false, () =>
        edit((x) => (x.geometry.quarter_turns = (x.geometry.quarter_turns + 3) % 4), {
          label: "Rotate",
          colors: false,
        }),
      );
      chip(turns, "Turn right", false, () =>
        edit((x) => (x.geometry.quarter_turns = (x.geometry.quarter_turns + 1) % 4), {
          label: "Rotate",
          colors: false,
        }),
      );
      chip(turns, "Flip across", r.geometry.flip_h, () =>
        edit((x) => (x.geometry.flip_h = !x.geometry.flip_h), { label: "Flip", colors: false }),
      );
      chip(turns, "Flip down", r.geometry.flip_v, () =>
        edit((x) => (x.geometry.flip_v = !x.geometry.flip_v), { label: "Flip", colors: false }),
      );
      chip(turns, "Reset crop", false, () =>
        edit((x) => (x.geometry = defaults().geometry), { label: "Reset crop", colors: false }),
      );
      host.append(turns);
      const [field, label, min, max, step] = SLIDERS.crop[0];
      slider(host, {
        value: getField(r, field),
        label,
        min,
        max,
        step,
        reset: 0,
        onInput: (v) => edit((x) => setField(x, field, v), { label, colors: false, settled: false }),
        onCommit: (v) => edit((x) => setField(x, field, v), { label, colors: false }),
      });
    } else if (state.tab === "local") {
      const chips = doc.createElement("div");
      chips.className = "chips";
      r.masks.forEach((_, i) =>
        chip(chips, `Radial ${i + 1}`, state.mask === i, () => {
          state.mask = i;
          renderPanel();
          draw();
        }),
      );
      const addButton = chip(chips, "+ Radial", false, () => {
        edit(
          (x) =>
            x.masks.push({
              mask: { kind: "radial", cx: 0.5, cy: 0.5, rx: 0.2, ry: 0.2, angle_deg: 0, feather: 0.5 },
              invert: false,
              exposure_ev: 0.5,
              saturation: 0,
            }),
          { label: "Add radial" },
        );
        state.mask = state.recipe.masks.length - 1;
        renderPanel();
      });
      addButton.disabled = r.masks.length >= MAX_MASKS;
      chip(chips, "Show mask", state.overlay, () => {
        state.overlay = !state.overlay;
        renderPanel();
        draw();
      });
      host.append(chips);
      const i = state.mask;
      if (i >= 0 && r.masks[i]) {
        for (const [key, label, min, max, step] of SLIDERS.local) {
          const field = `masks.${i}.${key}`;
          const colors = !key.startsWith("mask.");
          slider(host, {
            value: getField(r, field),
            label,
            min,
            max,
            step,
            reset: getField(
              { exposure_ev: 0, saturation: 0, mask: { rx: 0.2, ry: 0.2, angle_deg: 0, feather: 0.5 } },
              key,
            ),
            onInput: (v) => edit((x) => setField(x, field, v), { label, colors, settled: false }),
            onCommit: (v) => edit((x) => setField(x, field, v), { label: `Local ${label}`, colors }),
          });
        }
        const more = doc.createElement("div");
        more.className = "chips";
        chip(more, "Invert", r.masks[i].invert, () => {
          edit((x) => (x.masks[i].invert = !x.masks[i].invert), { label: "Invert mask" });
          renderPanel();
        });
        chip(more, "Delete", false, () => {
          edit((x) => x.masks.splice(i, 1), { label: "Delete mask" });
          state.mask = state.recipe.masks.length - 1;
          renderPanel();
        });
        host.append(more);
      } else {
        const p = doc.createElement("p");
        p.className = "dj-empty";
        p.textContent = "Add a radial mask, then drag its center on the photo.";
        host.append(p);
      }
    } else if (state.tab === "export") {
      exportPanel(host);
    }
    draw();
  }

  // ---- Curves ------------------------------------------------------------

  function curvesEditor(host) {
    const box = doc.createElement("canvas");
    box.className = "curves";
    const readout = doc.createElement("p");
    readout.className = "readout";
    readout.textContent = "Tap to add a point; drag one off the graph to remove it.";
    host.append(box, readout);
    const field = `curves.${state.channel}`;
    let points = editable(getField(state.recipe, field));
    let dragging = -1;
    let pressTimer = null;
    const size = () => Math.max(200, Math.min(320, host.clientWidth || 300));
    const paint = () => {
      const s = size();
      const dpr = win.devicePixelRatio || 1;
      box.style.width = `${s}px`;
      box.style.height = `${s}px`;
      box.width = Math.round(s * dpr);
      box.height = Math.round(s * dpr);
      const g = box.getContext?.("2d");
      if (!g) return;
      g.setTransform(dpr, 0, 0, dpr, 0, 0);
      g.clearRect(0, 0, s, s);
      g.strokeStyle = "rgba(128,128,128,0.35)";
      g.lineWidth = 1;
      for (let k = 1; k < 4; k++) {
        g.beginPath();
        g.moveTo((k * s) / 4, 0);
        g.lineTo((k * s) / 4, s);
        g.moveTo(0, (k * s) / 4);
        g.lineTo(s, (k * s) / 4);
        g.stroke();
      }
      const f = curve(points);
      g.strokeStyle = { master: "#ddd", red: "#e55", green: "#5c5", blue: "#58f" }[state.channel];
      g.lineWidth = 2;
      g.beginPath();
      for (let x = 0; x <= s; x += 2) {
        const y = f(x / s);
        if (x === 0) g.moveTo(x, s - y * s);
        else g.lineTo(x, s - y * s);
      }
      g.stroke();
      g.fillStyle = "#fff";
      for (const [x, y] of points) {
        g.beginPath();
        g.arc(x * s, s - y * s, 6, 0, Math.PI * 2);
        g.fill();
      }
    };
    const at = (e) => {
      const rect = box.getBoundingClientRect();
      const s = rect.width || size();
      const lift = e.pointerType === "touch" ? TOUCH_OFFSET_PX : 0;
      return [(e.clientX - rect.left) / s, 1 - (e.clientY - rect.top - lift) / s];
    };
    const commit = () => {
      edit((x) => setField(x, field, stored(points)), { label: "Curve" });
    };
    box.addEventListener("pointerdown", (e) => {
      const [x, y] = at(e);
      box.setPointerCapture?.(e.pointerId);
      dragging = nearest(points, x, y, 0.06);
      if (dragging < 0) {
        const r = add(points, x, y);
        points = r.points;
        dragging = r.index;
      }
      win.clearTimeout(pressTimer);
      const held = dragging;
      pressTimer = win.setTimeout(() => {
        // A long press deletes, for anyone who cannot drag off the edge.
        if (held > 0 && held < points.length - 1) {
          points = remove(points, held);
          dragging = -1;
          paint();
          commit();
        }
      }, LONG_PRESS_MS);
      paint();
    });
    box.addEventListener("pointermove", (e) => {
      if (dragging < 0) return;
      win.clearTimeout(pressTimer);
      const [x, y] = at(e);
      if (offGraph(x, y)) return;
      points = move(points, dragging, x, y);
      const [px, py] = points[dragging];
      readout.textContent = `${Math.round(px * 255)} → ${Math.round(py * 255)}`;
      paint();
      edit((r) => setField(r, field, stored(points)), { label: "Curve", settled: false });
    });
    const end = (e) => {
      win.clearTimeout(pressTimer);
      if (dragging < 0) return;
      const [x, y] = at(e);
      if (offGraph(x, y)) points = remove(points, dragging);
      dragging = -1;
      paint();
      commit();
    };
    box.addEventListener("pointerup", end);
    box.addEventListener("pointercancel", () => {
      win.clearTimeout(pressTimer);
      dragging = -1;
    });
    paint();
  }

  // ---- Overlay: crop handles and mask handles -----------------------------

  function drawOverlay(l) {
    overlay.replaceChildren();
    overlay.style.width = `${l.cssW}px`;
    overlay.style.height = `${l.cssH}px`;
    if (state.before) return;
    if (state.tab === "crop") cropOverlay(l);
    if (state.tab === "local" && state.mask >= 0 && state.recipe.masks[state.mask]) maskOverlay(l);
  }

  function cropOverlay(l) {
    const c = state.recipe.geometry.crop;
    const frame = doc.createElement("div");
    frame.className = "crop";
    frame.style.left = `${c.x * l.cssW}px`;
    frame.style.top = `${c.y * l.cssH}px`;
    frame.style.width = `${c.w * l.cssW}px`;
    frame.style.height = `${c.h * l.cssH}px`;
    frame.dataset.handle = "move";
    for (const h of ["nw", "n", "ne", "e", "se", "s", "sw", "w"]) {
      const d = doc.createElement("div");
      d.className = `handle handle-${h}`;
      d.dataset.handle = h;
      frame.append(d);
    }
    overlay.append(frame);
    let drag = null;
    frame.addEventListener("pointerdown", (e) => {
      e.preventDefault();
      frame.setPointerCapture?.(e.pointerId);
      drag = { handle: e.target.dataset.handle ?? "move", x: e.clientX, y: e.clientY, crop: { ...c } };
    });
    frame.addEventListener("pointermove", (e) => {
      if (!drag) return;
      const a = turnedAspect(state.recipe.geometry, state.proxy.width, state.proxy.height);
      const ratio = state.aspect === "original" ? a : state.aspect;
      const k = ratio ? ratio / a : null;
      const next = dragCrop(drag.crop, drag.handle, (e.clientX - drag.x) / l.cssW, (e.clientY - drag.y) / l.cssH, k);
      state.recipe.geometry.crop = next;
      frame.style.left = `${next.x * l.cssW}px`;
      frame.style.top = `${next.y * l.cssH}px`;
      frame.style.width = `${next.w * l.cssW}px`;
      frame.style.height = `${next.h * l.cssH}px`;
    });
    const end = () => {
      if (!drag) return;
      drag = null;
      edit(() => {}, { label: "Crop", colors: false });
    };
    frame.addEventListener("pointerup", end);
    frame.addEventListener("pointercancel", end);
  }

  function maskOverlay(l) {
    const i = state.mask;
    const m = state.recipe.masks[i].mask;
    const toScreen = invert(l.map);
    const { width: w, height: h } = state.proxy;
    const long = Math.max(w, h);
    // The ellipse's outline, mapped through the geometry onto the screen.
    const a = (m.angle_deg * Math.PI) / 180;
    const pts = [];
    for (let k = 0; k <= 48; k++) {
      const t = (k / 48) * Math.PI * 2;
      const ex = m.rx * Math.cos(t);
      const ey = m.ry * Math.sin(t);
      const u = m.cx + ((ex * Math.cos(a) - ey * Math.sin(a)) * long) / w;
      const v = m.cy + ((ex * Math.sin(a) + ey * Math.cos(a)) * long) / h;
      const [ox, oy] = apply(toScreen, u, v);
      pts.push(`${(ox * l.cssW).toFixed(1)},${(oy * l.cssH).toFixed(1)}`);
    }
    const svg = doc.createElementNS("http://www.w3.org/2000/svg", "svg");
    svg.setAttribute("class", "mask-outline");
    svg.setAttribute("width", String(l.cssW));
    svg.setAttribute("height", String(l.cssH));
    const line = doc.createElementNS("http://www.w3.org/2000/svg", "polyline");
    line.setAttribute("points", pts.join(" "));
    svg.append(line);
    overlay.append(svg);
    const [cx, cy] = apply(toScreen, m.cx, m.cy);
    const pin = doc.createElement("div");
    pin.className = "pin";
    pin.style.left = `${cx * l.cssW}px`;
    pin.style.top = `${cy * l.cssH}px`;
    overlay.append(pin);
    let dragging = false;
    pin.addEventListener("pointerdown", (e) => {
      e.preventDefault();
      pin.setPointerCapture?.(e.pointerId);
      dragging = true;
    });
    pin.addEventListener("pointermove", (e) => {
      if (!dragging) return;
      const rect = overlay.getBoundingClientRect();
      const [u, v] = apply(l.map, (e.clientX - rect.left) / l.cssW, (e.clientY - rect.top) / l.cssH);
      m.cx = Math.min(2, Math.max(-1, u));
      m.cy = Math.min(2, Math.max(-1, v));
      draw();
    });
    const end = () => {
      if (!dragging) return;
      dragging = false;
      edit(() => {}, { label: "Move mask", colors: false });
    };
    pin.addEventListener("pointerup", end);
    pin.addEventListener("pointercancel", end);
  }

  // ---- Export ------------------------------------------------------------

  let job = null;

  function exportPanel(host) {
    const form = doc.createElement("div");
    form.className = "export";
    form.innerHTML = `
      <label class="field">Format
        <select id="ex-format"><option value="jpeg">JPEG</option><option value="tiff16">TIFF, 16-bit</option></select>
      </label>
      <label class="field">Quality <input id="ex-quality" type="number" min="1" max="100" value="92"></label>
      <label class="field">Size
        <select id="ex-size"><option value="">Full size</option><option value="4096">4096 px</option><option value="2048">2048 px</option><option value="1080">1080 px</option></select>
      </label>
      <div class="chips"><button class="dj-button" id="ex-go">Export</button>
      <button class="dj-button" id="ex-cancel" hidden>Cancel</button></div>
      <progress id="ex-progress" max="1" value="0" hidden></progress>
      <p class="readout" id="ex-result">Exports are new files beside the photo. The original is never changed.</p>`;
    host.append(form);
    $("ex-go").addEventListener("click", () => void runExport());
    $("ex-cancel").addEventListener("click", () => void cancelExport());
  }

  async function runExport() {
    if (!state.path || job) return;
    await save();
    const random = deps.random ?? Math.random;
    job = `export-${Date.now().toString(36)}-${Math.floor(random() * 1e9).toString(36)}`;
    const format = $("ex-format").value;
    const size = $("ex-size").value;
    const progress = $("ex-progress");
    progress.hidden = false;
    progress.value = 0;
    $("ex-cancel").hidden = false;
    $("ex-go").disabled = true;
    try {
      const r = await bus.invoke(
        "photo.export",
        {
          path: state.path,
          recipe: state.recipe,
          format,
          quality: Number($("ex-quality").value) || 92,
          long_edge: size ? Number(size) : undefined,
          job,
        },
        (p) => {
          if (typeof p?.fraction === "number") progress.value = p.fraction;
        },
      );
      const result = $("ex-result");
      result.textContent = `Exported ${r.path} (${r.width} × ${r.height}). `;
      const show = doc.createElement("button");
      show.className = "dj-button";
      show.textContent = "Show";
      show.addEventListener("click", () => shell?.quicklook?.(`/${r.path}`));
      result.append(show);
      say(`Exported ${r.path}`);
    } catch (e) {
      fail(e);
    } finally {
      job = null;
      progress.hidden = true;
      $("ex-cancel").hidden = true;
      $("ex-go").disabled = false;
    }
  }

  async function cancelExport() {
    if (!job) return;
    try {
      await bus.invoke("photo.export.cancel", { job });
      say("Cancelling…");
    } catch (e) {
      fail(e);
    }
  }

  // ---- Wiring ------------------------------------------------------------

  for (const t of doc.querySelectorAll("[data-tab]")) {
    t.addEventListener("click", () => {
      state.tab = t.dataset.tab;
      renderPanel();
    });
  }
  $("undo").addEventListener("click", () => void step("undo"));
  $("redo").addEventListener("click", () => void step("redo"));
  $("open").addEventListener("click", () => void browse());
  $("picker-close").addEventListener("click", () => ($("picker").hidden = true));
  $("export").addEventListener("click", () => {
    state.tab = "export";
    renderPanel();
  });
  const showBefore = (on) => {
    if (state.before === on) return;
    state.before = on;
    useLuts(on ? luts.before : luts.current);
    if (!renderer) void refreshLut(33);
    draw();
  };
  const before = $("before");
  before.addEventListener("pointerdown", () => showBefore(true));
  for (const ev of ["pointerup", "pointerleave", "pointercancel"]) {
    before.addEventListener(ev, () => showBefore(false));
  }
  // Press and hold on the photo shows the before, as the brief asks.
  let holdTimer = null;
  view.addEventListener("pointerdown", (e) => {
    if (e.target !== canvas) return;
    holdTimer = win.setTimeout(() => showBefore(true), LONG_PRESS_MS);
  });
  const release = () => {
    win.clearTimeout(holdTimer);
    showBefore(false);
  };
  view.addEventListener("pointerup", release);
  view.addEventListener("pointercancel", release);

  win.addEventListener("keydown", (e) => {
    if (e.target?.tagName === "INPUT" || e.target?.tagName === "SELECT") return;
    const mod = e.ctrlKey || e.metaKey;
    if (e.key === "\\") showBefore(true);
    else if (mod && e.key.toLowerCase() === "z") {
      e.preventDefault();
      void step(e.shiftKey ? "redo" : "undo");
    } else if (!mod && e.key.toLowerCase() === "c") {
      state.tab = "crop";
      renderPanel();
    }
  });
  win.addEventListener("keyup", (e) => {
    if (e.key === "\\") showBefore(false);
  });
  // A file opened into this already-open window (the shell's
  // `deckjack.app.open`). Opening is a read; nothing is saved by it.
  win.addEventListener("message", (event) => {
    if (event.source !== win.parent) return;
    if (event.data?.type === "deckjack.app.open" && typeof event.data.path === "string") {
      void open(event.data.path);
    }
  });
  win.addEventListener("resize", () => draw());
  doc.addEventListener("visibilitychange", () => {
    if (doc.visibilityState === "hidden") void save();
  });

  renderPanel();
  updateButtons();

  return {
    open,
    browse,
    save,
    step,
    state,
    /** For tests: the LUT coalescer's entry point. */
    refreshLut,
    edit,
    runExport,
    cancelExport,
    renderPanel,
    async start(path) {
      if (path) await open(path);
      else await browse();
    },
  };
}
