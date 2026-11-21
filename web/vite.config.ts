import { readdirSync, readFileSync, statSync } from "node:fs";
import { join } from "node:path";
import { fileURLToPath, URL } from "node:url";

import vue from "@vitejs/plugin-vue";
import { defineConfig, type Plugin } from "vite";

/**
 * pdf.js's run-time data, served from the shell's own origin under
 * `/pdfjs/`: character maps for CJK text, the 14 standard fonts a PDF may
 * name without embedding, the JPEG 2000 and JBIG2 decoders scanned documents
 * use, and color profiles. pdf.js loads these by URL, with fixed file names,
 * so they are copied as they are rather than imported (which would hash the
 * names). Copied from `node_modules` at build time so the pinned
 * `pdfjs-dist` is the only copy in the repository. See `src/shell/pdf.ts`.
 */
function pdfjsData(): Plugin {
  const root = fileURLToPath(new URL("./node_modules/pdfjs-dist/", import.meta.url));
  const dirs = ["cmaps", "standard_fonts", "wasm", "iccs"];
  return {
    name: "deckjack-pdfjs-data",
    configureServer(server) {
      server.middlewares.use("/pdfjs", (req, res, next) => {
        const rel = decodeURIComponent((req.url ?? "").split("?")[0] ?? "").replace(/^\/+/, "");
        const [dir, file, ...rest] = rel.split("/");
        if (!dir || !file || rest.length || !dirs.includes(dir) || file.includes("..")) return next();
        try {
          res.end(readFileSync(join(root, dir, file)));
        } catch {
          next();
        }
      });
    },
    generateBundle() {
      for (const dir of dirs) {
        for (const file of readdirSync(join(root, dir))) {
          const path = join(root, dir, file);
          if (!statSync(path).isFile()) continue;
          this.emitFile({ type: "asset", fileName: `pdfjs/${dir}/${file}`, source: readFileSync(path) });
        }
      }
    },
  };
}

/**
 * The shell is baked into the core binary: `rust-embed` takes `web/dist` at
 * compile time and axum serves it with an `index.html` fallback. So the build
 * has to stay a plain static bundle — no SSR, no manifest the server has to
 * understand, nothing that needs Node at runtime.
 *
 * `base` stays absolute because the server mounts the shell at the origin
 * root and client-side routes are deep paths; relative asset URLs would
 * resolve against the route instead of the root.
 */
/** Where the Rust core is listening. */
const CORE = process.env.DECKJACK_CORE ?? "http://127.0.0.1:8777";

/**
 * Every path the core answers, and nothing else: the shell's own files and
 * `/kit/*` (from `public/`) are Vite's. A route missing here fails only in
 * development, which is how `/content` (Quick Look's bytes) and `/passkey`
 * went unnoticed, so keep this in step with the routes in `http.rs`,
 * `passkeyhttp.rs`, `content.rs`, `apps.rs` and `wiring.rs`.
 */
const CORE_ROUTES = [
  "/ws",
  "/login",
  "/logout",
  "/unlock",
  "/healthz",
  "/passkey",
  "/content",
  "/apps",
];

export default defineConfig({
  base: "/",
  plugins: [vue(), pdfjsData()],
  resolve: {
    alias: {
      "@": fileURLToPath(new URL("./src", import.meta.url)),
    },
  },
  build: {
    outDir: "dist",
    assetsDir: "assets",
    // Hashed names are what lets the server cache assets forever and
    // `index.html` never. Keep the hash even for CSS and fonts.
    assetsInlineLimit: 0,
    sourcemap: true,
    // The targets are the browsers I actually use: current Chromium, Safari
    // and Firefox. Nothing older needs to parse this bundle.
    target: "es2022",
    cssTarget: "es2022",
    chunkSizeWarningLimit: 900,
  },
  server: {
    // Vite serves the shell during development while the Rust core holds
    // sign-in, the command bus and the apps, so everything that is not a
    // front-end asset is passed to it rather than mocked.
    //
    // The core is `cargo xtask dev`'s, on 8777: a port of its own so a dev
    // core never collides with a real one on 7878. DECKJACK_CORE points the
    // proxy elsewhere.
    port: 5273,
    strictPort: true,
    proxy: Object.fromEntries(
      CORE_ROUTES.map((route) => [
        route,
        {
          target: route === "/ws" ? CORE.replace(/^http/, "ws") : CORE,
          ws: route === "/ws",
          // The core checks Origin against Host on sign-in and on the
          // socket, so the browser's Host has to reach it unchanged.
          changeOrigin: false,
        },
      ]),
    ),
  },
});
