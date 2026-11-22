/**
 * The curves editor's rules, apart from its drawing: what a tap, a drag and
 * a drag off the graph do to a list of `[input, output]` points.
 *
 * The recipe stores an empty list for "no curve" (docs/formats/photo-edit-v1.md),
 * so the editor starts from the diagonal's two end points on the first
 * touch and returns to `[]` when the curve is the diagonal again.
 */

/** Mirrors `MAX_CURVE_POINTS` in `src/recipe.rs`. */
export const MAX_POINTS = 16;
/** Closest two points may sit, in input units. */
export const MIN_GAP = 0.02;
/** How far off the square a drag must end to delete its point. */
export const OFF_GRAPH = 0.08;
/**
 * A touch drags its point this many CSS pixels above the finger, so the
 * finger does not hide the point it is placing.
 */
export const TOUCH_OFFSET_PX = 28;

const DIAGONAL = [
  [0, 0],
  [1, 1],
];

const clamp = (v, lo, hi) => Math.min(hi, Math.max(lo, v));
const round = (v) => Math.round(v * 1000) / 1000;

/** The points to edit: the stored list, or the diagonal when there is none. */
export function editable(points) {
  return points && points.length >= 2 ? points.map((p) => [...p]) : DIAGONAL.map((p) => [...p]);
}

/** What to store: `[]` when the points are just the diagonal. */
export function stored(points) {
  const isDiagonal =
    points.length === 2 &&
    points[0][0] === 0 &&
    points[0][1] === 0 &&
    points[1][0] === 1 &&
    points[1][1] === 1;
  return isDiagonal ? [] : points.map(([x, y]) => [round(x), round(y)]);
}

/** Index of the point within `radius` (input units) of `(x, y)`, or -1. */
export function nearest(points, x, y, radius) {
  let best = -1;
  let bestD = radius;
  points.forEach(([px, py], i) => {
    const d = Math.hypot(px - x, py - y);
    if (d <= bestD) {
      best = i;
      bestD = d;
    }
  });
  return best;
}

/**
 * Add a point at `(x, y)`. Returns `{ points, index }`; `index` is -1 when
 * nothing was added (full, or too close to a neighbor).
 */
export function add(points, x, y) {
  x = clamp(x, 0, 1);
  y = clamp(y, 0, 1);
  if (points.length >= MAX_POINTS || points.some(([px]) => Math.abs(px - x) < MIN_GAP)) {
    return { points, index: -1 };
  }
  const next = [...points, [x, y]].sort((a, b) => a[0] - b[0]);
  return { points: next, index: next.findIndex((p) => p[0] === x && p[1] === y) };
}

/**
 * Move point `i` to `(x, y)`. Inputs stay strictly between the neighbors,
 * so the curve stays a function; the end points keep their inputs at 0
 * and 1 unless they are the only points.
 */
export function move(points, i, x, y) {
  const next = points.map((p) => [...p]);
  const lo = i === 0 ? 0 : next[i - 1][0] + MIN_GAP;
  const hi = i === next.length - 1 ? 1 : next[i + 1][0] - MIN_GAP;
  next[i] = [clamp(x, lo, hi), clamp(y, 0, 1)];
  return next;
}

/** Whether a drag ending at `(x, y)` is off the graph, deleting its point. */
export function offGraph(x, y) {
  return x < -OFF_GRAPH || x > 1 + OFF_GRAPH || y < -OFF_GRAPH || y > 1 + OFF_GRAPH;
}

/** Remove point `i`. The two end points cannot be removed. */
export function remove(points, i) {
  if (i <= 0 || i >= points.length - 1) return points;
  return points.filter((_, k) => k !== i);
}
