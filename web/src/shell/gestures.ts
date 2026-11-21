/**
 * Zoom, pan and swipe for Quick Look, as plain geometry.
 *
 * The image viewer puts the picture, fitted and centered, inside a layer the
 * size of the stage, and moves that layer with
 * `transform: translate(x, y) scale(scale)` from its top-left corner. A
 * {@link View} is those three numbers. Everything here is a pure function of
 * views and pointer positions, so the behavior a thumb notices is tested
 * without a touch screen.
 */

export interface View {
  scale: number;
  x: number;
  y: number;
}

export interface Point {
  x: number;
  y: number;
}

export interface Size {
  w: number;
  h: number;
}

export interface Limits {
  min: number;
  max: number;
}

/** The fitted, unzoomed view. */
export const FIT: View = Object.freeze({ scale: 1, x: 0, y: 0 });

export const ZOOM_LIMITS: Limits = { min: 1, max: 8 };

function clamp(n: number, lo: number, hi: number): number {
  return Math.min(hi, Math.max(lo, n));
}

function mid(a: Point, b: Point): Point {
  return { x: (a.x + b.x) / 2, y: (a.y + b.y) / 2 };
}

function distance(a: Point, b: Point): number {
  return Math.hypot(a.x - b.x, a.y - b.y);
}

/**
 * The view after scaling by `factor` about a stage point, which stays put.
 */
export function zoomAt(view: View, factor: number, at: Point, limits: Limits): View {
  const scale = clamp(view.scale * factor, limits.min, limits.max);
  const k = scale / view.scale;
  return { scale, x: at.x - (at.x - view.x) * k, y: at.y - (at.y - view.y) * k };
}

/**
 * The view during a two-finger gesture, from the view when it began and the
 * two pointer positions then and now.
 *
 * The content point that was under the fingers' midpoint at the start is kept
 * under their midpoint now, so spreading zooms toward where the fingers are
 * and moving them together pans.
 */
export function pinchView(
  start: View,
  from: readonly [Point, Point],
  to: readonly [Point, Point],
  limits: Limits,
): View {
  const d0 = distance(from[0], from[1]);
  const d1 = distance(to[0], to[1]);
  const factor = d0 > 0 ? d1 / d0 : 1;
  const scale = clamp(start.scale * factor, limits.min, limits.max);
  const m0 = mid(from[0], from[1]);
  const m1 = mid(to[0], to[1]);
  // The content point under m0 at the start...
  const qx = (m0.x - start.x) / start.scale;
  const qy = (m0.y - start.y) / start.scale;
  // ...is placed under m1 at the new scale.
  return { scale, x: m1.x - qx * scale, y: m1.y - qy * scale };
}

/**
 * The nearest view that keeps the image covering the stage on each axis it
 * overflows, and centered on each axis it does not.
 *
 * `fitted` is the image's size at scale 1, as laid out centered in `stage`.
 */
export function clampView(view: View, stage: Size, fitted: Size): View {
  const axis = (offset: number, stageLen: number, imageLen: number) => {
    const scaled = view.scale * imageLen;
    // Where the image's leading edge is in the unscaled layer.
    const lead = (view.scale * (stageLen - imageLen)) / 2;
    if (scaled <= stageLen) return (stageLen - scaled) / 2 - lead;
    // Leading edge no further in than 0, trailing edge no further in than
    // the stage's far side.
    return clamp(offset, stageLen - scaled - lead, -lead);
  };
  const x = axis(view.x, stage.w, fitted.w);
  const y = axis(view.y, stage.h, fitted.h);
  // Normalize -0 so a fitted view compares equal to FIT.
  return { scale: view.scale, x: x + 0, y: y + 0 };
}

/**
 * The size of an image of `natural` size fitted, aspect kept, in `stage`.
 */
export function fitSize(natural: Size, stage: Size): Size {
  if (natural.w <= 0 || natural.h <= 0) return { w: stage.w, h: stage.h };
  const k = Math.min(stage.w / natural.w, stage.h / natural.h, 1);
  return { w: natural.w * k, h: natural.h * k };
}

/**
 * Whether a one-finger gesture was a swipe through the folder.
 *
 * Horizontal, and either quick (a flick) or long (a deliberate drag). A
 * mostly vertical gesture is scrolling and never navigates.
 */
export function classifySwipe(g: { dx: number; dy: number; ms: number }): "next" | "previous" | null {
  const ax = Math.abs(g.dx);
  const ay = Math.abs(g.dy);
  if (ax < 1.5 * ay) return null;
  const flick = ax >= 50 && ax / Math.max(g.ms, 1) >= 0.3;
  const drag = ax >= 160;
  if (!flick && !drag) return null;
  return g.dx < 0 ? "next" : "previous";
}
