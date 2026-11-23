import { writeFileSync } from "node:fs";
import { join } from "node:path";

import type { Page } from "@playwright/test";

import { approveApps, expect, openApp, test } from "../support/fixtures.ts";
import { delaySocket } from "../support/latency.ts";
import { WEB } from "../support/paths.ts";
import { seedTasks } from "../support/seed.ts";

/**
 * Phase-0 measurements for docs/lanes/9-tier1-phase0.md, against a real
 * debug node, the real Tasks binary and a real browser. Not a pass/fail
 * suite: it records numbers (`test-results/tier1-measurements.json`) and
 * fails only if a measurement could not be taken.
 *
 * `npm run e2e -- --project=tier1-measure`
 *
 * Latency is injected on the shell's socket by Playwright (`delaySocket`),
 * half each way, so "120 ms" is 60 ms added in each direction on top of the
 * loopback. The node, the app and the browser all run on this machine: the
 * numbers include their CPU time, which on a phone would be the phone's.
 */

interface Stats {
  rtt: number[];
  frames: { window: string; bytes: number; patches: number; restart: boolean }[];
  firstPaint: number[];
}

const stats = (page: Page) => page.evaluate(() => (globalThis as { __djUiStats?: Stats }).__djUiStats!);

function pct(values: number[], p: number): number {
  const sorted = [...values].sort((a, b) => a - b);
  const at = Math.min(sorted.length - 1, Math.max(0, Math.ceil((p / 100) * sorted.length) - 1));
  return Math.round((sorted[at] ?? NaN) * 10) / 10;
}

const results: Record<string, unknown> = {};

test.afterAll(() => {
  const file = join(WEB, "test-results", "tier1-measurements.json");
  writeFileSync(file, JSON.stringify(results, null, 2));
  console.log(JSON.stringify(results, null, 2));
});

test.beforeEach(async ({ page }) => {
  await page.addInitScript(() => localStorage.setItem("deckjack.uiStats", "1"));
});

test("event round trip, patch sizes and first paint, at 0, 40 and 120 ms", async ({ page, core, hasTouch, mode }) => {
  test.setTimeout(300_000);
  seedTasks(core.work, 50);
  await approveApps(page, ["tasks"], hasTouch);
  await openApp(page, "tasks", mode);
  const panel = page.locator('.ui-panel[data-app="tasks"]');
  await expect(panel.getByText("50 open, 0 done")).toBeVisible({ timeout: 30_000 });
  const cold = (await stats(page)).firstPaint[0];

  // Warm: the page reloads, the app is running and the node has the tree.
  await page.reload();
  await expect(panel.getByText("50 open, 0 done")).toBeVisible();
  const warm = (await stats(page)).firstPaint[0];
  results.firstPaintMs = {
    coldAppStart50Tasks: Math.round(cold ?? NaN),
    warmReload50Tasks: Math.round(warm ?? NaN),
  };

  const box = panel.getByRole("checkbox", { name: "Show done" });
  const rtt: Record<string, unknown> = {};
  const sample = async (label: string) => {
    const before = (await stats(page)).rtt.length;
    const N = 40;
    for (let i = 0; i < N; i++) {
      await box.click();
      await expect.poll(async () => (await stats(page)).rtt.length, { timeout: 10_000 }).toBe(before + i + 1);
    }
    const samples = (await stats(page)).rtt.slice(before);
    rtt[label] = { n: samples.length, p50: pct(samples, 50), p95: pct(samples, 95), max: pct(samples, 100) };
  };
  // Straight to the node, with nothing in between: the real local number.
  await sample("direct");
  // Then through Playwright's socket proxy, which is how latency is added;
  // "proxy+0ms" is the proxy's own cost, to read the others against.
  const wire = await delaySocket(page, 0);
  await page.reload();
  await expect(panel.getByText("50 open, 0 done")).toBeVisible();
  for (const oneWay of [0, 20, 60]) {
    wire.setDelay(oneWay);
    await sample(`proxy+${oneWay * 2}ms`);
  }
  results.eventRoundTripMs = rtt;
  wire.setDelay(0);

  // Patch sizes for typical edits: the frames each one brings back.
  const framesFor = async (act: () => Promise<void>) => {
    const from = (await stats(page)).frames.length;
    await act();
    await page.waitForTimeout(600);
    return (await stats(page)).frames.slice(from).map((f) => ({ bytes: f.bytes, patches: f.patches }));
  };
  const field = panel.getByRole("textbox", { name: "New task" });
  results.patchBytes = {
    typingOneWordThenPause: await framesFor(async () => {
      await field.fill("Buy milk");
    }),
    addTask: await framesFor(async () => {
      await field.press("Enter");
      await expect(panel.getByText("51 open, 0 done")).toBeVisible();
    }),
    completeTaskRowLeavesList: await framesFor(async () => {
      await panel.locator('.ui-list-row[data-key="e2e-00007"]').getByRole("checkbox", { name: "Done" }).click();
      await expect(panel.getByText("50 open, 1 done")).toBeVisible();
    }),
    toggleShowDone: await framesFor(async () => {
      await box.click();
    }),
    // With done tasks shown, a finished task sorts to the end: one move.
    completeTaskShowDoneOnRowMovesToEnd: await framesFor(async () => {
      await panel.locator('.ui-list-row[data-key="e2e-00003"]').getByRole("checkbox", { name: "Done" }).click();
      await expect(panel.getByText("49 open, 2 done")).toBeVisible();
    }),
  };
  results.windowFirstFrameBytes50Tasks = (await stats(page)).frames.find((f) => f.restart)?.bytes;
});

test("a 10,000-task window: first frame size and first paint", async ({ page, core, hasTouch, mode }) => {
  test.setTimeout(300_000);
  seedTasks(core.work, 10_000);
  await approveApps(page, ["tasks"], hasTouch);
  const started = Date.now();
  await openApp(page, "tasks", mode);
  const panel = page.locator('.ui-panel[data-app="tasks"]');
  await expect(panel.getByText("10000 open, 0 done")).toBeVisible({ timeout: 120_000 });
  const visibleAfter = Date.now() - started;
  const s = await stats(page);
  results.tenThousand = {
    firstFrameBytes: s.frames.find((f) => f.restart)?.bytes,
    firstPaintMs: Math.round(s.firstPaint[0] ?? NaN),
    navigationToVisibleMs: visibleAfter,
    rowsInDom: await panel.locator(".ui-list-row").count(),
  };
  // A tick in that window: the patch is the row's, not the list's.
  const from = s.frames.length;
  await panel.getByRole("checkbox", { name: "Show done" }).click();
  await expect.poll(async () => (await stats(page)).rtt.length).toBeGreaterThan(0);
  const after = await stats(page);
  results.tenThousand = {
    ...(results.tenThousand as object),
    showDoneRoundTripMs: Math.round(after.rtt.at(-1) ?? NaN),
    showDoneFrames: after.frames.slice(from).map((f) => ({ bytes: f.bytes, patches: f.patches })),
  };
  // With done tasks shown, finishing the first task moves its row past the
  // other 9,999: click to the status line saying so, and the frames.
  const before = after.frames.length;
  const clicked = Date.now();
  await panel.locator('.ui-list-row[data-key="e2e-00000"]').getByRole("checkbox", { name: "Done" }).click();
  await expect(panel.getByText("9999 open, 1 done")).toBeVisible({ timeout: 60_000 });
  const moved = await stats(page);
  results.tenThousand = {
    ...(results.tenThousand as object),
    completeFirstShowDoneOnMs: Date.now() - clicked,
    completeFirstShowDoneOnFrames: moved.frames.slice(before).map((f) => ({ bytes: f.bytes, patches: f.patches })),
  };
});
