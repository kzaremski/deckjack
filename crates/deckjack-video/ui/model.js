/**
 * The timeline's pure parts: time and pixels, snapping, layout and hit
 * testing. No DOM, no canvas, no bus -- so every rule a finger relies on is
 * tested on its own (`web/src/apps/video.test.js`).
 *
 * Units: positions on the timeline are whole frames at the sequence's rate,
 * as the backend speaks them. A view is `{ pxPerFrame, scroll }`, where
 * `scroll` is the frame at the left edge of the track area (fractional while
 * a gesture is in flight).
 */

/** Narrowest and widest zoom: a frame between a twentieth of a pixel and 48 px. */
export const MIN_PX_PER_FRAME = 0.05;
export const MAX_PX_PER_FRAME = 48;

/** Height of the ruler, where one finger scrubs. */
export const RULER_HEIGHT = 28;
/** Height of a track row. At least the touch target, with room for a label. */
export const TRACK_HEIGHT = 48;
/** Width of the track-name gutter on the left. */
export const GUTTER = 36;

/**
 * How far outside a selected clip's edge a finger still grabs the edge. A
 * finger is about 40 px wide and cannot hit a one-frame edge at useful zoom;
 * this is why trims on touch also have nudge buttons.
 */
export const EDGE_REACH = 22;

/** Snap distance in pixels: close enough to mean it, far enough to escape. */
export const SNAP_PX = 10;

export function clamp(v, lo, hi) {
  return Math.min(hi, Math.max(lo, v));
}

/** The x (in canvas pixels) where a frame starts. */
export function frameToX(frame, view) {
  return GUTTER + (frame - view.scroll) * view.pxPerFrame;
}

/** The frame under an x, rounded to the nearest frame boundary. */
export function xToFrame(x, view) {
  return Math.round((x - GUTTER) / view.pxPerFrame + view.scroll);
}

/** The frame under an x, rounded down: the frame a finger is on. */
export function xToFrameFloor(x, view) {
  return Math.floor((x - GUTTER) / view.pxPerFrame + view.scroll);
}

/**
 * Zoom by `factor` about the x a pinch or a wheel is centered on: the frame
 * under `anchorX` stays under it. Scroll never goes before frame 0.
 */
export function zoomAt(view, factor, anchorX) {
  const pxPerFrame = clamp(view.pxPerFrame * factor, MIN_PX_PER_FRAME, MAX_PX_PER_FRAME);
  const anchor = (anchorX - GUTTER) / view.pxPerFrame + view.scroll;
  const scroll = Math.max(0, anchor - (anchorX - GUTTER) / pxPerFrame);
  return { pxPerFrame, scroll };
}

/**
 * A two-finger gesture against the view it started from: the distance
 * between the fingers zooms, the midpoint pans, and the frame that was under
 * the starting midpoint stays under the current one. The same math as the
 * shell's pinch (`web/src/shell/gestures.ts`), redone here because the
 * frame cannot import the shell.
 */
export function pinch(start, startDist, dist, startMidX, midX) {
  const factor = startDist > 0 ? dist / startDist : 1;
  const pxPerFrame = clamp(start.pxPerFrame * factor, MIN_PX_PER_FRAME, MAX_PX_PER_FRAME);
  const anchor = (startMidX - GUTTER) / start.pxPerFrame + start.scroll;
  const scroll = Math.max(0, anchor - (midX - GUTTER) / pxPerFrame);
  return { pxPerFrame, scroll };
}

/** Pan by a pixel delta (a finger moving right moves time left). */
export function pan(view, dx) {
  return { ...view, scroll: Math.max(0, view.scroll - dx / view.pxPerFrame) };
}

/** A zoom that fits `frames` into `width` pixels of track area. */
export function fit(frames, width) {
  const room = Math.max(1, width - GUTTER - 8);
  return {
    pxPerFrame: clamp(room / Math.max(1, frames), MIN_PX_PER_FRAME, MAX_PX_PER_FRAME),
    scroll: 0,
  };
}

/**
 * Where a frame could snap to: every clip edge, every marker, the playhead
 * and frame 0. `exclude` is the clips being dragged, whose own edges would
 * otherwise always win.
 */
export function snapTargets(state, playhead, exclude = []) {
  const out = new Set([0, playhead]);
  for (const track of state?.tracks ?? []) {
    for (const c of track.clips) {
      if (exclude.includes(c.id)) continue;
      out.add(c.start);
      out.add(c.end);
    }
  }
  for (const m of state?.markers ?? []) out.add(m.frame);
  return [...out];
}

/**
 * A clip and every clip linked to it: what moves together, and so what a
 * drag must not snap to.
 */
export function partners(state, clip) {
  const out = [];
  for (const track of state?.tracks ?? []) {
    for (const c of track.clips) {
      if (c.id === clip.id || (clip.link != null && c.link === clip.link)) out.push(c.id);
    }
  }
  return out;
}

/**
 * The nearest target within `SNAP_PX` of `frame`, or `frame` itself. Ties go
 * to the earlier target so a result never depends on iteration order.
 */
export function snap(frame, targets, view) {
  let best = frame;
  let bestPx = SNAP_PX + 1e-9;
  for (const t of [...targets].sort((a, b) => a - b)) {
    const px = Math.abs(t - frame) * view.pxPerFrame;
    if (px < bestPx) {
      best = t;
      bestPx = px;
    }
  }
  return best;
}

/** Track rows, top to bottom: video tracks highest first (V2 above V1), then audio. */
export function layout(state) {
  const tracks = state?.tracks ?? [];
  const video = tracks.filter((t) => t.kind === "video").reverse();
  const audio = tracks.filter((t) => t.kind === "audio");
  return [...video, ...audio].map((track, i) => ({
    track,
    y: RULER_HEIGHT + i * TRACK_HEIGHT,
    h: TRACK_HEIGHT,
  }));
}

/** Height of the whole canvas for a layout. */
export function contentHeight(rows) {
  return RULER_HEIGHT + rows.length * TRACK_HEIGHT;
}

/** The clip with this id and its track, or null. */
export function findClip(state, id) {
  for (const track of state?.tracks ?? []) {
    const clip = track.clips.find((c) => c.id === id);
    if (clip) return { track, clip };
  }
  return null;
}

/**
 * What is under a point. In priority order:
 *
 * 1. the ruler (`{ kind: "ruler" }`);
 * 2. an edge of the selected clip, within `EDGE_REACH` outside or a third of
 *    the clip inside (`{ kind: "edge", clip, edge: "in"|"out", track }`) --
 *    only the selected clip has handles, so a tap on an unselected clip's
 *    edge selects it instead of trimming it by accident;
 * 3. a clip (`{ kind: "clip", clip, track }`);
 * 4. empty track (`{ kind: "empty", track }`), or nothing (`null`).
 */
export function hit(state, rows, view, x, y, selected = null) {
  if (y < RULER_HEIGHT) return { kind: "ruler" };
  const row = rows.find((r) => y >= r.y && y < r.y + r.h);
  if (!row) return null;
  const found = findClip(state, selected);
  if (found && found.track.id === row.track.id) {
    const x0 = frameToX(found.clip.start, view);
    const x1 = frameToX(found.clip.end, view);
    const inner = Math.min(EDGE_REACH, (x1 - x0) / 3);
    const dIn = x - x0;
    const dOut = x1 - x;
    const nearIn = dIn >= -EDGE_REACH && dIn <= inner;
    const nearOut = dOut >= -EDGE_REACH && dOut <= inner;
    if (nearIn || nearOut) {
      const edge = nearIn && (!nearOut || Math.abs(dIn) <= Math.abs(dOut)) ? "in" : "out";
      return { kind: "edge", clip: found.clip, edge, track: row.track };
    }
  }
  const frame = (x - GUTTER) / view.pxPerFrame + view.scroll;
  const clip = row.track.clips.find((c) => frame >= c.start && frame < c.end);
  if (clip) return { kind: "clip", clip, track: row.track };
  return { kind: "empty", track: row.track };
}

/** The row a y is in, or null. */
export function rowAt(rows, y) {
  return rows.find((r) => y >= r.y && y < r.y + r.h) ?? null;
}

/**
 * `HH:MM:SS:FF`, non-drop-frame, at the rate's nominal frame count (30 for
 * 29.97). Drop-frame display is the sequence's setting and v1 does not
 * offer it.
 */
export function timecode(frame, fps) {
  const nominal = Math.max(1, Math.round(fps.num / fps.den));
  const f = Math.max(0, Math.trunc(frame));
  const ff = f % nominal;
  const totalSeconds = Math.floor(f / nominal);
  const ss = totalSeconds % 60;
  const mm = Math.floor(totalSeconds / 60) % 60;
  const hh = Math.floor(totalSeconds / 3600);
  const two = (n) => String(n).padStart(2, "0");
  return `${two(hh)}:${two(mm)}:${two(ss)}:${two(ff)}`;
}

/**
 * A frame from what a person types: `HH:MM:SS:FF`, fewer fields from the
 * right (`SS:FF`, `MM:SS:FF`), or a bare frame count. `null` when it is not
 * one of those or a field is out of range.
 */
export function parseTimecode(text, fps) {
  const nominal = Math.max(1, Math.round(fps.num / fps.den));
  const s = String(text ?? "").trim();
  if (/^\d+$/.test(s)) return Number(s);
  if (!/^\d{1,2}(:\d{1,2}){1,3}$/.test(s)) return null;
  const parts = s.split(":").map(Number);
  while (parts.length < 4) parts.unshift(0);
  const [hh, mm, ss, ff] = parts;
  if (mm > 59 || ss > 59 || ff >= nominal) return null;
  return ((hh * 60 + mm) * 60 + ss) * nominal + ff;
}

/** Ruler tick spacing in frames for a zoom: labeled roughly every 80 px. */
export function rulerStep(view, fps) {
  const nominal = Math.max(1, Math.round(fps.num / fps.den));
  const steps = [1, 2, 5, 10, nominal, nominal * 2, nominal * 5, nominal * 10, nominal * 30, nominal * 60, nominal * 300, nominal * 600];
  for (const s of steps) if (s * view.pxPerFrame >= 80) return s;
  return steps[steps.length - 1];
}

/**
 * Where a dragged clip lands: its start moved by the drag, snapped by
 * either edge (whichever is closer to a target), never before frame 0.
 */
export function dropFrame(clip, dxFrames, targets, view, snapping) {
  const start = Math.max(0, Math.round(clip.start + dxFrames));
  if (!snapping) return start;
  const len = clip.end - clip.start;
  const byStart = snap(start, targets, view);
  const byEnd = snap(start + len, targets, view) - len;
  const dStart = Math.abs(byStart - start);
  const dEnd = Math.abs(byEnd - start);
  if (byStart !== start && (byEnd === start || dStart <= dEnd)) return Math.max(0, byStart);
  if (byEnd !== start) return Math.max(0, byEnd);
  return start;
}

/**
 * The frame delta a trim drag makes, snapped, and limited so a clip keeps
 * at least one frame. The media's own limits are the engine's to enforce;
 * it refuses a trim past the source and says so.
 */
export function trimDelta(clip, edge, dxFrames, targets, view, snapping) {
  const at = edge === "in" ? clip.start : clip.end;
  let to = Math.round(at + dxFrames);
  if (snapping) to = snap(to, targets, view);
  if (edge === "in") to = clamp(to, 0, clip.end - 1);
  else to = Math.max(clip.start + 1, to);
  return to - at;
}
