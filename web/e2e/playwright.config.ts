import { existsSync } from "node:fs";
import { dirname, join } from "node:path";

import { defineConfig, devices, type LaunchOptions, webkit } from "@playwright/test";

import type { ShellOptions } from "./support/fixtures.ts";
import { WEB, WEBKIT_COMPAT } from "./support/paths.ts";

/**
 * End-to-end tests: a real `deckjack --debug` node, a real browser, the real
 * shell. `npm run e2e`; deliberately not part of `npm test`, which has to stay
 * fast and need neither cargo nor a browser.
 *
 * Projects:
 * - `desktop-chromium`, `desktop-webkit`: the shell smoke tests (and the
 *   Convert app's, `convert.e2e.ts`) at a desktop size, so the shell is in
 *   desktop mode.
 * - `phone-chromium` (Pixel 7), `phone-webkit` (iPhone 15): the same tests
 *   at a phone size with touch, so the shell is in compact mode and the tests
 *   tap rather than click.
 * - `spike-chromium`, `spike-webkit`: the browser-behavior spike behind
 *   docs/APP-UI-ISOLATION.md §8, against a tiny local server, not the core.
 *
 * Run one with `npm run e2e -- --project=desktop-chromium`.
 *
 * WebKit on a host Playwright does not support (Arch): run
 * `e2e/webkit/fetch-compat-libs.sh` once. When its directory exists, WebKit
 * is launched through `e2e/webkit/minibrowser.sh`, which adds it to the
 * library path. Elsewhere WebKit launches as Playwright ships it.
 */

const webkitLaunch: LaunchOptions = existsSync(WEBKIT_COMPAT)
  ? {
      executablePath: join(WEB, "e2e", "webkit", "minibrowser.sh"),
      env: {
        ...(process.env as Record<string, string>),
        DJ_WEBKIT_DIR: dirname(webkit.executablePath()),
        DJ_WEBKIT_COMPAT_LIBS: WEBKIT_COMPAT,
      },
    }
  : {};

const SHELL = /(shell|convert)\.e2e\.ts$/;
const SPIKE = /spike[\\/].*\.e2e\.ts$/;
const MEASURE = /tier1[\\/].*\.e2e\.ts$/;

export default defineConfig<ShellOptions>({
  testDir: ".",
  testMatch: /\.e2e\.ts$/,
  outputDir: join(WEB, "test-results"),
  globalSetup: "./global-setup.ts",
  // One worker by default: each test's node listens on DJ_E2E_PORT plus the
  // worker's index, and only DJ_E2E_PORT is known to be free for this run.
  workers: Number(process.env.DJ_E2E_WORKERS ?? 1),
  fullyParallel: false,
  forbidOnly: !!process.env.CI,
  retries: 0,
  timeout: 60_000,
  expect: { timeout: 10_000 },
  reporter: [["list"], ["html", { open: "never", outputFolder: join(WEB, "playwright-report") }]],
  use: {
    trace: "retain-on-failure",
    screenshot: "only-on-failure",
    // The shell registers a service worker that caches itself. Each test has
    // a fresh node and a fresh profile, so a cached shell could only ever
    // serve a stale build.
    serviceWorkers: "block",
  },
  projects: [
    { name: "desktop-chromium", testMatch: SHELL, use: { ...devices["Desktop Chrome"], mode: "desktop" } },
    {
      name: "desktop-webkit",
      testMatch: SHELL,
      use: { ...devices["Desktop Safari"], launchOptions: webkitLaunch, mode: "desktop" },
    },
    { name: "phone-chromium", testMatch: SHELL, use: { ...devices["Pixel 7"], mode: "compact" } },
    {
      name: "phone-webkit",
      testMatch: SHELL,
      use: { ...devices["iPhone 15"], launchOptions: webkitLaunch, mode: "compact" },
    },
    { name: "spike-chromium", testMatch: SPIKE, use: { ...devices["Desktop Chrome"] } },
    {
      name: "spike-webkit",
      testMatch: SPIKE,
      use: { ...devices["Desktop Safari"], launchOptions: webkitLaunch },
    },
    // The UI protocol's phase-0 numbers (docs/lanes/9-tier1-phase0.md),
    // written to test-results/tier1-measurements.json.
    { name: "tier1-measure", testMatch: MEASURE, use: { ...devices["Desktop Chrome"], mode: "desktop" } },
  ],
});
