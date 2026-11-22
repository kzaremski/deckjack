/**
 * The preview renderer: one WebGL2 shader that does geometry, a 3D LUT and
 * up to four masked local LUTs.
 *
 * It knows nothing about color. The backend bakes every color operation into
 * LUTs (`src/lut.rs`); this samples the unadjusted proxy through the
 * recipe's geometry map, looks the result up in the global LUT, and blends
 * each local LUT in under its mask, `y = mix(y, D(y), m)`, exactly as the
 * export does per pixel (`src/render.rs`). Spike 1 in the lane brief
 * measured this file against the Rust render.
 *
 * Pixels arrive as command results, never through a URL: the frame has
 * `connect-src 'none'` under the brokered UI (APP-UI-ISOLATION §4.2).
 */

export const MAX_LOCALS = 4;

const VERTEX = `#version 300 es
void main() {
  // One triangle that covers the viewport.
  vec2 p = vec2(float((gl_VertexID << 1) & 2), float(gl_VertexID & 2));
  gl_Position = vec4(p * 2.0 - 1.0, 0.0, 1.0);
}`;

/** Sampler arrays need constant indices in GLSL ES 3.00, so four names. */
const FRAGMENT = `#version 300 es
precision highp float;
precision highp sampler3D;
uniform sampler2D u_proxy;
uniform sampler3D u_global;
uniform sampler3D u_local0;
uniform sampler3D u_local1;
uniform sampler3D u_local2;
uniform sampler3D u_local3;
uniform vec2 u_lut;        // scale, offset for the global LUT: (n-1)/n, 0.5/n
uniform vec2 u_llut[4];    // the same for each local LUT
uniform int u_count;
uniform vec4 u_ma[4];      // cx, cy, rx, ry
uniform vec4 u_mb[4];      // cos, sin, inner, invert
uniform vec2 u_scale;      // normalized -> long-edge units
uniform vec3 u_mapu;       // output uv -> source u
uniform vec3 u_mapv;       // output uv -> source v
uniform vec2 u_size;       // canvas pixels
uniform int u_overlay;     // tint the first mask, for placing it
out vec4 o;

float maskAt(int i, vec2 s) {
  vec2 d = (s - u_ma[i].xy) * u_scale;
  float c = u_mb[i].x, n = u_mb[i].y;
  vec2 e = vec2(d.x * c + d.y * n, -d.x * n + d.y * c) / u_ma[i].zw;
  float t = clamp((length(e) - u_mb[i].z) / (1.0 - u_mb[i].z), 0.0, 1.0);
  float m = 1.0 - t * t * (3.0 - 2.0 * t);
  return u_mb[i].w > 0.5 ? 1.0 - m : m;
}

vec3 local(int i, vec3 y) {
  vec3 p = y * u_llut[i].x + u_llut[i].y;
  if (i == 0) return texture(u_local0, p).rgb;
  if (i == 1) return texture(u_local1, p).rgb;
  if (i == 2) return texture(u_local2, p).rgb;
  return texture(u_local3, p).rgb;
}

void main() {
  vec2 uv = vec2(gl_FragCoord.x / u_size.x, 1.0 - gl_FragCoord.y / u_size.y);
  vec3 q = vec3(uv, 1.0);
  vec2 s = vec2(dot(u_mapu, q), dot(u_mapv, q));
  if (s.x < 0.0 || s.y < 0.0 || s.x > 1.0 || s.y > 1.0) {
    o = vec4(0.0, 0.0, 0.0, 1.0);
    return;
  }
  vec3 code = texture(u_proxy, s).rgb;
  vec3 y = texture(u_global, code * u_lut.x + u_lut.y).rgb;
  for (int i = 0; i < 4; i++) {
    if (i >= u_count) break;
    float m = maskAt(i, s);
    if (m > 0.0) y = mix(y, local(i, y), m);
  }
  if (u_overlay == 1 && u_count > 0) y = mix(y, vec3(1.0, 0.2, 0.3), 0.45 * maskAt(0, s));
  o = vec4(clamp(y, 0.0, 1.0), 1.0);
}`;

function compile(gl, type, source) {
  const shader = gl.createShader(type);
  gl.shaderSource(shader, source);
  gl.compileShader(shader);
  if (!gl.getShaderParameter(shader, gl.COMPILE_STATUS)) {
    throw new Error(`shader: ${gl.getShaderInfoLog(shader)}`);
  }
  return shader;
}

/** base64 -> Uint16Array of the LUT's half floats (little-endian on the wire). */
export function lutFromBase64(b64) {
  const bin = atob(b64);
  const bytes = new Uint8Array(bin.length);
  for (let i = 0; i < bin.length; i++) bytes[i] = bin.charCodeAt(i);
  return new Uint16Array(bytes.buffer, 0, bytes.length >> 1);
}

/**
 * Decode the proxy's PNG bytes without any color management, so the shader
 * sees the codes the backend wrote. A browser that "corrected" them toward
 * the display profile would shift every color before the LUT saw it.
 */
export function decodeProxy(bytes) {
  return createImageBitmap(new Blob([bytes], { type: "image/png" }), {
    colorSpaceConversion: "none",
    premultiplyAlpha: "none",
    imageOrientation: "none",
  });
}

/**
 * A renderer on `canvas`. Returns null where WebGL2 is unavailable, and the
 * caller falls back to backend-rendered previews (`photo.render`).
 */
export function createRenderer(canvas) {
  const gl = canvas.getContext("webgl2", {
    antialias: false,
    premultipliedAlpha: false,
    preserveDrawingBuffer: true,
  });
  if (!gl) return null;

  let program = null;
  let loc = {};
  const textures = { proxy: null, global: null, locals: [null, null, null, null] };
  const lutSize = { global: 2, locals: [2, 2, 2, 2] };
  let proxySource = null;
  const luts = { global: null, locals: [null, null, null, null] };
  let lost = false;

  function setup() {
    program = gl.createProgram();
    gl.attachShader(program, compile(gl, gl.VERTEX_SHADER, VERTEX));
    gl.attachShader(program, compile(gl, gl.FRAGMENT_SHADER, FRAGMENT));
    gl.linkProgram(program);
    if (!gl.getProgramParameter(program, gl.LINK_STATUS)) {
      throw new Error(`program: ${gl.getProgramInfoLog(program)}`);
    }
    gl.useProgram(program);
    loc = {};
    for (const name of [
      "u_proxy", "u_global", "u_local0", "u_local1", "u_local2", "u_local3", "u_lut",
      "u_llut", "u_count", "u_ma", "u_mb", "u_scale", "u_mapu", "u_mapv", "u_size", "u_overlay",
    ]) {
      loc[name] = gl.getUniformLocation(program, name);
    }
    gl.uniform1i(loc.u_proxy, 0);
    gl.uniform1i(loc.u_global, 1);
    for (let i = 0; i < MAX_LOCALS; i++) gl.uniform1i(loc[`u_local${i}`], 2 + i);
    gl.bindVertexArray(gl.createVertexArray());
    // Rebuild what was uploaded before a context loss.
    if (proxySource) uploadProxy(proxySource);
    if (luts.global) uploadLut(-1, luts.global.size, luts.global.data);
    luts.locals.forEach((l, i) => l && uploadLut(i, l.size, l.data));
  }

  function texture(unit, target) {
    const t = gl.createTexture();
    gl.activeTexture(gl.TEXTURE0 + unit);
    gl.bindTexture(target, t);
    gl.texParameteri(target, gl.TEXTURE_MIN_FILTER, gl.LINEAR);
    gl.texParameteri(target, gl.TEXTURE_MAG_FILTER, gl.LINEAR);
    gl.texParameteri(target, gl.TEXTURE_WRAP_S, gl.CLAMP_TO_EDGE);
    gl.texParameteri(target, gl.TEXTURE_WRAP_T, gl.CLAMP_TO_EDGE);
    if (target === gl.TEXTURE_3D) gl.texParameteri(target, gl.TEXTURE_WRAP_R, gl.CLAMP_TO_EDGE);
    return t;
  }

  function uploadProxy(source) {
    if (textures.proxy) gl.deleteTexture(textures.proxy);
    textures.proxy = texture(0, gl.TEXTURE_2D);
    gl.pixelStorei(gl.UNPACK_ALIGNMENT, 1);
    gl.pixelStorei(gl.UNPACK_FLIP_Y_WEBGL, false);
    gl.pixelStorei(gl.UNPACK_PREMULTIPLY_ALPHA_WEBGL, false);
    gl.pixelStorei(gl.UNPACK_COLORSPACE_CONVERSION_WEBGL, gl.NONE);
    if (source.data) {
      gl.texImage2D(gl.TEXTURE_2D, 0, gl.RGB8, source.width, source.height, 0, gl.RGB,
        gl.UNSIGNED_BYTE, source.data);
    } else {
      gl.texImage2D(gl.TEXTURE_2D, 0, gl.RGB8, gl.RGB, gl.UNSIGNED_BYTE, source);
    }
  }

  function uploadLut(slot, size, data) {
    const unit = slot < 0 ? 1 : 2 + slot;
    const old = slot < 0 ? textures.global : textures.locals[slot];
    if (old) gl.deleteTexture(old);
    const t = texture(unit, gl.TEXTURE_3D);
    gl.pixelStorei(gl.UNPACK_ALIGNMENT, 1);
    gl.texImage3D(gl.TEXTURE_3D, 0, gl.RGB16F, size, size, size, 0, gl.RGB, gl.HALF_FLOAT, data);
    if (slot < 0) {
      textures.global = t;
      lutSize.global = size;
    } else {
      textures.locals[slot] = t;
      lutSize.locals[slot] = size;
    }
  }

  canvas.addEventListener("webglcontextlost", (e) => {
    e.preventDefault();
    lost = true;
  });
  canvas.addEventListener("webglcontextrestored", () => {
    lost = false;
    textures.proxy = null;
    textures.global = null;
    textures.locals = [null, null, null, null];
    setup();
  });

  setup();

  return {
    gl,
    get lost() {
      return lost;
    },
    /** An ImageBitmap, or `{width, height, data: Uint8Array}` of RGB. */
    setProxy(source) {
      proxySource = source;
      if (!lost) uploadProxy(source);
    },
    /** `slot` -1 is the global LUT, 0-3 the local ones. */
    setLut(slot, size, data) {
      if (slot < 0) luts.global = { size, data };
      else luts.locals[slot] = { size, data };
      if (!lost) uploadLut(slot, size, data);
    },
    /**
     * Draw into the canvas at `width` x `height` device pixels.
     * `map` is outputToSource's six numbers; `masks` are maskUniforms.
     */
    draw({ width, height, map, masks = [], overlay = false }) {
      if (lost || !textures.proxy || !textures.global) return false;
      if (canvas.width !== width) canvas.width = width;
      if (canvas.height !== height) canvas.height = height;
      gl.viewport(0, 0, width, height);
      gl.useProgram(program);
      const n = lutSize.global;
      gl.uniform2f(loc.u_lut, (n - 1) / n, 0.5 / n);
      const llut = [];
      const ma = [];
      const mb = [];
      for (let i = 0; i < MAX_LOCALS; i++) {
        const s = lutSize.locals[i];
        llut.push((s - 1) / s, 0.5 / s);
        const m = masks[i];
        ma.push(...(m ? m.a : [0, 0, 1, 1]));
        mb.push(...(m ? m.b : [1, 0, 0.5, 0]));
      }
      const count = Math.min(masks.length, MAX_LOCALS, textures.locals.filter(Boolean).length);
      gl.uniform2fv(loc.u_llut, llut);
      gl.uniform4fv(loc.u_ma, ma);
      gl.uniform4fv(loc.u_mb, mb);
      gl.uniform1i(loc.u_count, count);
      gl.uniform2fv(loc.u_scale, masks[0]?.scale ?? [1, 1]);
      gl.uniform3f(loc.u_mapu, map[0], map[1], map[2]);
      gl.uniform3f(loc.u_mapv, map[3], map[4], map[5]);
      gl.uniform2f(loc.u_size, width, height);
      gl.uniform1i(loc.u_overlay, overlay ? 1 : 0);
      gl.drawArrays(gl.TRIANGLES, 0, 3);
      return true;
    },
    /** RGBA8, top row first. For tests and the parity spike. */
    read() {
      const { width, height } = canvas;
      const flipped = new Uint8Array(width * height * 4);
      gl.readPixels(0, 0, width, height, gl.RGBA, gl.UNSIGNED_BYTE, flipped);
      const out = new Uint8Array(width * height * 4);
      const row = width * 4;
      for (let y = 0; y < height; y++) {
        out.set(flipped.subarray((height - 1 - y) * row, (height - y) * row), y * row);
      }
      return out;
    },
  };
}
