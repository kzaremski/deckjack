/**
 * The touch timeline: a canvas, drawn from the backend's `describe` state,
 * and the gestures on it. Everything here reports intentions through the
 * callbacks it is given; it never calls the bus itself, so the edit a finger
 * makes is the same command a button or an agent makes.
 *
 * The touch contract (docs/lanes/5d-video.md):
 *
 * - tap selects a clip; tap on empty track clears the selection;
 * - long-press lifts a clip and drags it (a mouse drags after a few pixels);
 * - only the selected clip has trim handles, and they reach outside it;
 * - one finger on the ruler scrubs; one finger on empty track pans;
 * - two fingers pinch to zoom about their midpoint and pan together;
 * - the wheel pans, Ctrl or Cmd with the wheel zooms.
 */

import {
  GUTTER,
  RULER_HEIGHT,
  contentHeight,
  findClip,
  frameToX,
  hit,
  layout,
  pan,
  partners,
  pinch,
  rowAt,
  rulerStep,
  snap,
  snapTargets,
  timecode,
  dropFrame,
  trimDelta,
  xToFrameFloor,
  zoomAt,
} from "./model.js";

export const LONG_PRESS_MS = 500;
/** How far a finger may drift before a press is a pan rather than a press. */
const SLOP = 8;

/** A design token, read when drawing so a theme change shows on the next frame. */
function token(el, name, fallback) {
  const v = getComputedStyle(el).getPropertyValue(name).trim();
  return v || fallback;
}

/**
 * Mount a timeline on `canvas`.
 *
 * `deps`:
 * - `state()`: the latest describe result (or null);
 * - `playhead()`, `selected()`, `options()` (`{ snapping }`);
 * - `onScrub(frame)`, `onSelect(clipId | null)`;
 * - `onMove(clip, frame, trackId)`, `onTrim(clip, edge, delta)`;
 * - `setTimeout`, `clearTimeout`: injectable for tests.
 */
export function createTimeline(canvas, deps) {
  const later = deps.setTimeout ?? setTimeout.bind(globalThis);
  const cancel = deps.clearTimeout ?? clearTimeout.bind(globalThis);
  let view = { pxPerFrame: 4, scroll: 0 };
  /** Live pointers by id: `{ x, y }` in canvas CSS pixels. */
  const pointers = new Map();
  /** The gesture in flight, or null. */
  let gesture = null;

  function local(event) {
    const r = canvas.getBoundingClientRect();
    return { x: event.clientX - r.left, y: event.clientY - r.top };
  }

  function rows() {
    return layout(deps.state());
  }

  function resize() {
    const dpr = globalThis.devicePixelRatio || 1;
    const width = canvas.clientWidth || canvas.parentElement?.clientWidth || 360;
    const height = contentHeight(rows());
    canvas.style.height = `${height}px`;
    canvas.width = Math.round(width * dpr);
    canvas.height = Math.round(height * dpr);
    return { width, height, dpr };
  }

  function draw() {
    const { width, height, dpr } = resize();
    const ctx = canvas.getContext?.("2d");
    if (!ctx) return;
    const state = deps.state();
    const fps = state?.fps ?? { num: 30, den: 1 };
    const colors = {
      bg: token(canvas, "--dj-color-surface", "#16181d"),
      raised: token(canvas, "--dj-color-surface-raised", "#1b1e25"),
      border: token(canvas, "--dj-color-border", "#2a2e38"),
      text: token(canvas, "--dj-color-text", "#e7e9ee"),
      muted: token(canvas, "--dj-color-text-muted", "#9aa0ad"),
      accent: token(canvas, "--dj-color-accent", "#7aa2f7"),
      selection: token(canvas, "--dj-color-selection", "#2d3344"),
      video: token(canvas, "--dj-video-clip", "#3d5a80"),
      audio: token(canvas, "--dj-audio-clip", "#3f6e5a"),
      marker: token(canvas, "--dj-video-marker", "#e0af68"),
      playhead: token(canvas, "--dj-video-playhead", "#f7768e"),
    };
    ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
    ctx.fillStyle = colors.bg;
    ctx.fillRect(0, 0, width, height);
    ctx.font = "11px system-ui, sans-serif";
    ctx.textBaseline = "middle";

    // Ruler.
    ctx.fillStyle = colors.raised;
    ctx.fillRect(0, 0, width, RULER_HEIGHT);
    const step = rulerStep(view, fps);
    const first = Math.max(0, Math.floor(view.scroll / step) * step);
    for (let f = first; frameToX(f, view) < width; f += step) {
      const x = frameToX(f, view);
      if (x < GUTTER) continue;
      ctx.fillStyle = colors.border;
      ctx.fillRect(Math.round(x), RULER_HEIGHT - 8, 1, 8);
      ctx.fillStyle = colors.muted;
      ctx.fillText(timecode(f, fps).slice(3), x + 3, RULER_HEIGHT / 2 - 2);
    }

    // Tracks.
    const sel = deps.selected();
    for (const row of rows()) {
      ctx.fillStyle = colors.border;
      ctx.fillRect(0, row.y + row.h - 1, width, 1);
      for (const clip of row.track.clips) drawClip(ctx, colors, row, clip, clip.id === sel);
      ctx.fillStyle = colors.raised;
      ctx.fillRect(0, row.y, GUTTER, row.h - 1);
      ctx.fillStyle = colors.muted;
      ctx.fillText(row.track.name, 8, row.y + row.h / 2);
    }

    // The clip being dragged or trimmed, where it would land.
    if (gesture?.preview) drawPreview(ctx, colors, gesture.preview);

    // Markers on the ruler.
    for (const m of state?.markers ?? []) {
      const x = frameToX(m.frame, view);
      if (x < GUTTER || x > width) continue;
      ctx.fillStyle = colors.marker;
      ctx.beginPath();
      ctx.moveTo(x - 5, 2);
      ctx.lineTo(x + 5, 2);
      ctx.lineTo(x, 12);
      ctx.closePath();
      ctx.fill();
    }

    // Playhead over everything.
    const px = frameToX(deps.playhead(), view);
    if (px >= GUTTER && px <= width) {
      ctx.fillStyle = colors.playhead;
      ctx.fillRect(Math.round(px), 0, 2, height);
    }
  }

  function drawClip(ctx, colors, row, clip, selected) {
    const x0 = Math.max(GUTTER, frameToX(clip.start, view));
    const x1 = frameToX(clip.end, view);
    if (x1 <= GUTTER || x1 <= x0) return;
    const y = row.y + 3;
    const h = row.h - 7;
    const dragging = gesture?.preview?.clip?.id === clip.id;
    ctx.globalAlpha = dragging ? 0.35 : 1;
    ctx.fillStyle = row.track.kind === "video" ? colors.video : colors.audio;
    ctx.fillRect(x0, y, Math.max(1, x1 - x0 - 1), h);
    if (selected) {
      ctx.strokeStyle = colors.accent;
      ctx.lineWidth = 2;
      ctx.strokeRect(x0 + 1, y + 1, Math.max(1, x1 - x0 - 3), h - 2);
      // Handles: drawn, not only hit-tested, so a finger knows where to go.
      ctx.fillStyle = colors.accent;
      ctx.fillRect(x0, y, 6, h);
      ctx.fillRect(x1 - 7, y, 6, h);
    }
    if (x1 - x0 > 30) {
      ctx.save();
      ctx.beginPath();
      ctx.rect(x0, y, x1 - x0 - 2, h);
      ctx.clip();
      ctx.fillStyle = colors.text;
      ctx.fillText(clip.link == null ? clip.name : `${clip.name}`, x0 + 8, y + h / 2);
      if (clip.link == null && row.track.kind === "audio") {
        ctx.fillStyle = colors.muted;
        ctx.fillText("unlinked", x0 + 8, y + h - 8);
      }
      ctx.restore();
    }
    ctx.globalAlpha = 1;
  }

  function drawPreview(ctx, colors, p) {
    const row = rows().find((r) => r.track.id === p.trackId);
    if (!row) return;
    const x0 = frameToX(p.start, view);
    const x1 = frameToX(p.end, view);
    ctx.globalAlpha = 0.8;
    ctx.fillStyle = row.track.kind === "video" ? colors.video : colors.audio;
    ctx.fillRect(x0, row.y + 3, Math.max(1, x1 - x0 - 1), row.h - 7);
    ctx.strokeStyle = colors.accent;
    ctx.lineWidth = 2;
    ctx.strokeRect(x0 + 1, row.y + 4, Math.max(1, x1 - x0 - 3), row.h - 9);
    ctx.globalAlpha = 1;
  }

  function scrubTo(x) {
    deps.onScrub(Math.max(0, xToFrameFloor(x, view)));
  }

  function startTwoFinger() {
    if (gesture?.timer) cancel(gesture.timer);
    const [a, b] = [...pointers.values()];
    gesture = {
      kind: "pinch",
      start: { ...view },
      dist: Math.hypot(a.x - b.x, a.y - b.y),
      mid: (a.x + b.x) / 2,
    };
  }

  function down(event) {
    canvas.setPointerCapture?.(event.pointerId);
    const p = local(event);
    pointers.set(event.pointerId, p);
    if (pointers.size === 2) {
      startTwoFinger();
      return;
    }
    if (pointers.size > 2) return;
    const target = hit(deps.state(), rows(), view, p.x, p.y, deps.selected());
    const touch = event.pointerType !== "mouse";
    gesture = { kind: "press", origin: p, target, touch, moved: false, timer: null };
    if (target?.kind === "ruler") {
      gesture.kind = "scrub";
      scrubTo(p.x);
    } else if (target?.kind === "edge") {
      gesture.kind = "trim";
    } else if (target?.kind === "clip" && touch) {
      const g = gesture;
      g.timer = later(() => {
        if (gesture === g && !g.moved) {
          g.kind = "drag";
          deps.onSelect(target.clip.id);
          g.preview = preview(g, p);
          draw();
        }
      }, LONG_PRESS_MS);
    }
  }

  function preview(g, p) {
    const state = deps.state();
    const dx = (p.x - g.origin.x) / view.pxPerFrame;
    const { snapping } = deps.options();
    if (g.kind === "drag") {
      const clip = g.target.clip;
      const row = rowAt(rows(), p.y);
      const trackId = row && row.track.kind === g.target.track.kind ? row.track.id : g.target.track.id;
      const targets = snapTargets(state, deps.playhead(), partners(state, clip));
      const start = dropFrame(clip, dx, targets, view, snapping);
      return { clip, trackId, start, end: start + (clip.end - clip.start) };
    }
    const clip = g.target.clip;
    const targets = snapTargets(state, deps.playhead(), [clip.id]);
    const delta = trimDelta(clip, g.target.edge, dx, targets, view, snapping);
    return {
      clip,
      trackId: g.target.track.id,
      delta,
      start: g.target.edge === "in" ? clip.start + delta : clip.start,
      end: g.target.edge === "out" ? clip.end + delta : clip.end,
    };
  }

  function move(event) {
    if (!pointers.has(event.pointerId)) return;
    const p = local(event);
    pointers.set(event.pointerId, p);
    if (!gesture) return;
    if (gesture.kind === "pinch" && pointers.size === 2) {
      const [a, b] = [...pointers.values()];
      view = pinch(gesture.start, gesture.dist, Math.hypot(a.x - b.x, a.y - b.y), gesture.mid, (a.x + b.x) / 2);
      draw();
      return;
    }
    const far = Math.hypot(p.x - gesture.origin.x, p.y - gesture.origin.y) > SLOP;
    if (far) gesture.moved = true;
    switch (gesture.kind) {
      case "scrub":
        scrubTo(p.x);
        break;
      case "trim":
      case "drag":
        gesture.preview = preview(gesture, p);
        draw();
        break;
      case "press":
        if (!far) break;
        if (gesture.timer) cancel(gesture.timer);
        if (gesture.target?.kind === "clip" && !gesture.touch) {
          // A mouse drags straight away; a finger that moves before the
          // long-press is a pan, so the timeline scrolls under it.
          gesture.kind = "drag";
          deps.onSelect(gesture.target.clip.id);
          gesture.preview = preview(gesture, p);
        } else {
          // Panned from where the finger went down, so the slop it crossed
          // before the gesture was known is not lost.
          gesture.kind = "pan";
          view = pan(view, p.x - gesture.origin.x);
          gesture.last = p;
        }
        draw();
        break;
      case "pan":
        view = pan(view, p.x - gesture.last.x);
        gesture.last = p;
        draw();
        break;
    }
  }

  function up(event) {
    if (!pointers.has(event.pointerId)) return;
    pointers.delete(event.pointerId);
    const g = gesture;
    if (!g) return;
    if (g.kind === "pinch") {
      // Lifting one finger of two ends the pinch, not a new gesture.
      if (pointers.size === 0) gesture = null;
      return;
    }
    gesture = null;
    if (g.timer) cancel(g.timer);
    const cancelled = event.type === "pointercancel";
    if (!cancelled && g.kind === "press" && !g.moved) {
      if (g.target?.kind === "clip") deps.onSelect(g.target.clip.id);
      else if (g.target?.kind === "empty") deps.onSelect(null);
    } else if (!cancelled && g.kind === "drag" && g.preview) {
      const { clip, start, trackId } = g.preview;
      if (start !== clip.start || trackId !== g.target.track.id) deps.onMove(clip, start, trackId);
    } else if (!cancelled && g.kind === "trim" && g.preview && g.preview.delta !== 0) {
      deps.onTrim(g.preview.clip, g.target.edge, g.preview.delta);
    }
    draw();
  }

  function wheel(event) {
    event.preventDefault();
    const p = local(event);
    if (event.ctrlKey || event.metaKey) view = zoomAt(view, Math.exp(-event.deltaY / 300), p.x);
    else view = pan(view, -(event.deltaX || event.deltaY));
    draw();
  }

  canvas.addEventListener("pointerdown", down);
  canvas.addEventListener("pointermove", move);
  canvas.addEventListener("pointerup", up);
  canvas.addEventListener("pointercancel", up);
  canvas.addEventListener("wheel", wheel, { passive: false });
  canvas.addEventListener("contextmenu", (e) => e.preventDefault());

  return {
    draw,
    get view() {
      return view;
    },
    set view(v) {
      view = v;
      draw();
    },
    /** Keep the playhead in sight after a keyboard step or a nudge. */
    reveal(frame) {
      const width = canvas.clientWidth || 360;
      const x = frameToX(frame, view);
      if (x < GUTTER + 8 || x > width - 8) {
        view = { ...view, scroll: Math.max(0, frame - (width - GUTTER) / 2 / view.pxPerFrame) };
      }
      draw();
    },
    zoom(factor) {
      const width = canvas.clientWidth || 360;
      const anchor = frameToX(deps.playhead(), view);
      view = zoomAt(view, factor, anchor >= GUTTER && anchor <= width ? anchor : (GUTTER + width) / 2);
      draw();
    },
    /** For tests: the pending gesture's kind. */
    get gesture() {
      return gesture?.kind ?? null;
    },
    snapFrame(frame) {
      return deps.options().snapping ? snap(frame, snapTargets(deps.state(), deps.playhead()), view) : frame;
    },
    findClip: (id) => findClip(deps.state(), id),
  };
}
