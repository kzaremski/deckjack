// Spike 1, the browser half: render the proxy through the production shader
// (`ui/gl.js`) in headless Chromium and dump what WebGL2 drew.
//
//   node spike/lut-parity.mjs <dir from examples/parity.rs> [gpu|swiftshader]
//
// Writes `webgl-17.rgba` and `webgl-33.rgba` (RGBA8, top row first) into the
// directory, and timing for LUT uploads. `spike/compare.py` scores them.
// Playwright comes from the repo's web/ install; nothing here is shipped.

import { createServer } from "node:http";
import { readFileSync, writeFileSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const root = resolve(here, "../../..");
const { chromium } = await import(
  pathToFileURL(join(root, "web/node_modules/playwright/index.mjs")).href
);

const dir = resolve(process.argv[2]);
const mode = process.argv[3] ?? "swiftshader";
const meta = JSON.parse(readFileSync(join(dir, "spike.json"), "utf8"));

const files = {
  "/gl.js": [join(here, "../ui/gl.js"), "text/javascript"],
  "/geometry.js": [join(here, "../ui/geometry.js"), "text/javascript"],
  "/": [null, "text/html"],
};
const page = `<!doctype html><canvas id=c></canvas><script type=module>
import { createRenderer, decodeProxy } from "/gl.js";
import { maskUniforms, outputToSource } from "/geometry.js";
window.run = async (meta, size, proxyB64, globalB64, localB64s) => {
  const bytes = (b) => Uint8Array.from(atob(b), (c) => c.charCodeAt(0));
  const halves = (b) => { const u = bytes(b); return new Uint16Array(u.buffer, 0, u.length >> 1); };
  const canvas = document.getElementById("c");
  const r = createRenderer(canvas);
  if (!r) return { error: "no webgl2" };
  const info = r.gl.getParameter(r.gl.getExtension("WEBGL_debug_renderer_info")?.UNMASKED_RENDERER_WEBGL ?? r.gl.RENDERER);
  r.setProxy(await decodeProxy(bytes(proxyB64)));
  const t0 = performance.now();
  const N = 50;
  for (let k = 0; k < N; k++) {
    r.setLut(-1, size, halves(globalB64));
    localB64s.forEach((l, i) => r.setLut(i, size, halves(l)));
    r.draw({ width: meta.width, height: meta.height,
      map: outputToSource(meta.recipe.geometry ?? {}, meta.width, meta.height),
      masks: (meta.recipe.masks ?? []).map((m) => maskUniforms(m, meta.width, meta.height)) });
    r.gl.finish();
  }
  const perFrame = (performance.now() - t0) / N;
  const px = r.read();
  let s = ""; for (let i = 0; i < px.length; i += 0x8000) s += String.fromCharCode(...px.subarray(i, i + 0x8000));
  return { renderer: info, perFrame, pixels: btoa(s) };
};
</script>`;

const server = createServer((req, res) => {
  const f = files[req.url];
  if (!f) {
    res.writeHead(404).end();
    return;
  }
  res.writeHead(200, { "content-type": f[1] });
  res.end(f[0] ? readFileSync(f[0]) : page);
});
await new Promise((ok) => server.listen(0, "127.0.0.1", ok));
const port = server.address().port;

const args =
  mode === "gpu"
    ? ["--enable-gpu", "--ignore-gpu-blocklist", "--use-angle=vulkan", "--enable-features=Vulkan"]
    : ["--use-angle=swiftshader", "--enable-unsafe-swiftshader"];
const browser = await chromium.launch({ args });
try {
  const tab = await browser.newPage();
  tab.on("console", (m) => console.log("page:", m.text()));
  await tab.goto(`http://127.0.0.1:${port}/`);
  await tab.waitForFunction(() => typeof window.run === "function");
  const b64 = (f) => readFileSync(join(dir, f)).toString("base64");
  for (const size of [17, 33]) {
    const locals = (meta.recipe.masks ?? []).map((_, i) => b64(`local${i}-${size}.bin`));
    const out = await tab.evaluate(
      ([m, s, p, g, l]) => window.run(m, s, p, g, l),
      [meta, size, b64("proxy.png"), b64(`global-${size}.bin`), locals],
    );
    if (out.error) throw new Error(out.error);
    writeFileSync(join(dir, `webgl-${size}-${mode}.rgba`), Buffer.from(out.pixels, "base64"));
    console.log(
      `${mode} ${size}^3: renderer "${out.renderer}", upload+draw ${out.perFrame.toFixed(2)} ms/frame`,
    );
  }
} finally {
  await browser.close();
  server.close();
}
