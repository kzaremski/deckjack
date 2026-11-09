import { fileURLToPath, URL } from "node:url";

import vue from "@vitejs/plugin-vue";
import { defineConfig } from "vite";

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
export default defineConfig({
  base: "/",
  plugins: [vue()],
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
    // front-end asset is proxied to it rather than mocked.
    port: 5273,
    strictPort: true,
    proxy: {
      "/api": { target: "http://127.0.0.1:8777", changeOrigin: false },
      "/ws": { target: "ws://127.0.0.1:8777", ws: true, changeOrigin: false },
      "/login": { target: "http://127.0.0.1:8777", changeOrigin: false },
      "/logout": { target: "http://127.0.0.1:8777", changeOrigin: false },
      "/apps": { target: "http://127.0.0.1:8777", changeOrigin: false },
    },
  },
});
