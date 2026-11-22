/**
 * The frame's copy of the few formulas it needs without a round trip:
 * the geometry map, the mask parameters and the tone curve it draws.
 *
 * The backend's Rust is the definition of all three (`src/geometry.rs`,
 * `src/color.rs`). These exist because a crop handle or a mask handle has to
 * move under the finger at 60 fps, and the shader needs the same six numbers
 * the export uses. `web/src/apps/photo.test.js` holds them to the Rust
 * tests' values. Colors are never computed here: the LUT carries those.
 */

/** Output uv -> source uv, as `[a, b, c, d, e, f]`. Mirrors `output_to_source`. */
export function outputToSource(g, width, height) {
  const turns = (g.quarter_turns ?? 0) % 4;
  const [tw, th] = turns % 2 === 1 ? [height, width] : [width, height];
  const c = g.crop ?? { x: 0, y: 0, w: 1, h: 1 };
  const crop = [c.w * tw, 0, c.x * tw, 0, c.h * th, c.y * th];
  const a = ((g.straighten_deg ?? 0) * Math.PI) / 180;
  const s = Math.sin(a);
  const co = Math.cos(a);
  const cx = tw / 2;
  const cy = th / 2;
  const unrotate = [co, s, cx - co * cx - s * cy, -s, co, cy + s * cx - co * cy];
  const toUv = [1 / tw, 0, 0, 0, 1 / th, 0];
  const unflip = [
    g.flip_h ? -1 : 1,
    0,
    g.flip_h ? 1 : 0,
    0,
    g.flip_v ? -1 : 1,
    g.flip_v ? 1 : 0,
  ];
  const unturn = [
    [1, 0, 0, 0, 1, 0],
    [0, 1, 0, -1, 0, 1],
    [-1, 0, 1, 0, -1, 1],
    [0, -1, 1, 1, 0, 0],
  ][turns];
  let m = compose(unrotate, crop);
  m = compose(toUv, m);
  m = compose(unflip, m);
  return compose(unturn, m);
}

export function compose(outer, inner) {
  const [a, b, c, d, e, f] = outer;
  const [g, h, i, j, k, l] = inner;
  return [
    a * g + b * j,
    a * h + b * k,
    a * i + b * l + c,
    d * g + e * j,
    d * h + e * k,
    d * i + e * l + f,
  ];
}

export function apply(m, x, y) {
  return [m[0] * x + m[1] * y + m[2], m[3] * x + m[4] * y + m[5]];
}

/** Invert an affine map, for turning a touch on screen into source uv. */
export function invert(m) {
  const [a, b, c, d, e, f] = m;
  const det = a * e - b * d;
  return [e / det, -b / det, (b * f - c * e) / det, -d / det, a / det, (c * d - a * f) / det];
}

/** The turned image's aspect (width / height), for crop presets. */
export function turnedAspect(g, width, height) {
  return (g.quarter_turns ?? 0) % 2 === 1 ? height / width : width / height;
}

/** Output pixel size, as `output_size` computes it. */
export function outputSize(g, width, height) {
  const turns = (g.quarter_turns ?? 0) % 4;
  const [tw, th] = turns % 2 === 1 ? [height, width] : [width, height];
  const c = g.crop ?? { x: 0, y: 0, w: 1, h: 1 };
  return [Math.max(1, Math.round(c.w * tw)), Math.max(1, Math.round(c.h * th))];
}

/** Mirrors `MIN_FEATHER` in `src/color.rs`. */
export const MIN_FEATHER = 0.01;

/**
 * A local adjustment's mask, compiled for the shader: two vec4s.
 * Mirrors `MaskOp::new`.
 */
export function maskUniforms(local, width, height) {
  const long = Math.max(width, height);
  const m = local.mask;
  const a = ((m.angle_deg ?? 0) * Math.PI) / 180;
  return {
    a: [m.cx, m.cy, m.rx, m.ry],
    b: [Math.cos(a), Math.sin(a), 1 - Math.max(m.feather, MIN_FEATHER), local.invert ? 1 : 0],
    scale: [width / long, height / long],
  };
}

/** Mask strength at source uv; the same formula as the shader's `maskAt`. */
export function maskAt(u, uv) {
  const [cx, cy, rx, ry] = u.a;
  const [co, s, inner, inv] = u.b;
  const dx = (uv[0] - cx) * u.scale[0];
  const dy = (uv[1] - cy) * u.scale[1];
  const x = dx * co + dy * s;
  const y = -dx * s + dy * co;
  const d = Math.hypot(x / rx, y / ry);
  const t = Math.min(1, Math.max(0, (d - inner) / (1 - inner)));
  const m = 1 - t * t * (3 - 2 * t);
  return inv ? 1 - m : m;
}

/**
 * A monotone cubic curve (Fritsch-Carlson), for drawing the curves editor.
 * Mirrors `Curve` in `src/color.rs`; the LUT applies the Rust one.
 */
export function curve(points) {
  if (!points || points.length < 2) return (x) => x;
  const xs = points.map((p) => p[0]);
  const ys = points.map((p) => p[1]);
  const n = xs.length;
  const d = [];
  for (let k = 0; k < n - 1; k++) d.push((ys[k + 1] - ys[k]) / (xs[k + 1] - xs[k]));
  const ms = new Array(n).fill(0);
  ms[0] = d[0];
  ms[n - 1] = d[n - 2];
  for (let k = 1; k < n - 1; k++) ms[k] = d[k - 1] * d[k] <= 0 ? 0 : (d[k - 1] + d[k]) / 2;
  for (let k = 0; k < n - 1; k++) {
    if (d[k] === 0) {
      ms[k] = 0;
      ms[k + 1] = 0;
      continue;
    }
    const a = ms[k] / d[k];
    const b = ms[k + 1] / d[k];
    const s = a * a + b * b;
    if (s > 9) {
      const t = 3 / Math.sqrt(s);
      ms[k] = t * a * d[k];
      ms[k + 1] = t * b * d[k];
    }
  }
  return (x) => {
    if (x <= xs[0]) return ys[0];
    if (x >= xs[n - 1]) return ys[n - 1];
    let k = 0;
    while (k < n - 2 && x >= xs[k + 1]) k++;
    const h = xs[k + 1] - xs[k];
    const t = (x - xs[k]) / h;
    const t2 = t * t;
    const t3 = t2 * t;
    return (
      (2 * t3 - 3 * t2 + 1) * ys[k] +
      (t3 - 2 * t2 + t) * h * ms[k] +
      (-2 * t3 + 3 * t2) * ys[k + 1] +
      (t3 - t2) * h * ms[k + 1]
    );
  };
}
