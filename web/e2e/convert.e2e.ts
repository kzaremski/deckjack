import { execFileSync } from "node:child_process";
import { createHash } from "node:crypto";
import { existsSync, mkdirSync, readdirSync, readFileSync } from "node:fs";
import { join } from "node:path";

import type { Locator, Page } from "@playwright/test";

import { approveApps, expect, openApp, press, test } from "./support/fixtures.ts";

/**
 * Convert (docs/lanes/5c-convert.md) against a real node, a real browser and
 * the system's ffmpeg: the window is described by the app over the UI
 * protocol and drawn by the shell, and every job writes real files.
 *
 * Runs in every shell project, so the same steps are tapped on the phone
 * layouts and clicked on the desktop ones. Assertions are on what a person
 * would see (rows, states, sentences) and on the files on disk, never on the
 * shell's internals.
 */

const FFMPEG = "/usr/bin/ffmpeg";

/** Write a test file with the system's ffmpeg, from lavfi sources only. */
function make(out: string, args: string[]): void {
  execFileSync(FFMPEG, ["-v", "error", "-nostdin", "-y", ...args, out], { stdio: "pipe" });
}

function sha256(path: string): string {
  return createHash("sha256").update(readFileSync(path)).digest("hex");
}

/** What is in a folder, hidden files (a job's partial) included. */
function listing(dir: string): string[] {
  return readdirSync(dir).sort();
}

interface Media {
  /** 12 s, 640x360 H.264 + AAC in MKV: quick to remux and to measure. */
  clip: string;
  /** 120 s at 1080p: long enough under AV1 to still be running when cancelled. */
  long: string;
  /** 8 s of MP3: a lossy source for the FLAC refusal. */
  song: string;
}

function seedMedia(work: string): Media {
  const videos = join(work, "Videos");
  const music = join(work, "Music");
  mkdirSync(videos, { recursive: true });
  mkdirSync(music, { recursive: true });
  const media = {
    clip: join(videos, "clip.mkv"),
    long: join(videos, "long.mkv"),
    song: join(music, "song.mp3"),
  };
  make(media.clip, [
    "-f", "lavfi", "-i", "testsrc2=s=640x360:r=30:d=12",
    "-f", "lavfi", "-i", "sine=frequency=440:d=12",
    "-c:v", "libx264", "-preset", "veryfast", "-g", "60", "-c:a", "aac", "-shortest",
  ]);
  make(media.long, [
    "-f", "lavfi", "-i", "testsrc2=s=1920x1080:r=30:d=120",
    "-f", "lavfi", "-i", "sine=frequency=220:d=120",
    "-c:v", "mpeg4", "-q:v", "12", "-c:a", "aac", "-shortest",
  ]);
  make(media.song, ["-f", "lavfi", "-i", "sine=frequency=330:d=8", "-c:a", "libmp3lame", "-b:a", "128k"]);
  return media;
}

function panelOf(page: Page): Locator {
  return page.locator('.ui-panel[data-app="convert"]');
}

/** Tick files on the Files step and move on to the presets. */
async function pickFiles(panel: Locator, names: string[], hasTouch: boolean): Promise<void> {
  await press(panel.getByRole("button", { name: "Files", exact: true }), hasTouch);
  for (const name of names) {
    const tick = panel.getByRole("checkbox", { name: `Pick ${name}` });
    await press(tick, hasTouch);
    await expect(tick).toBeChecked();
  }
  await press(panel.getByRole("button", { name: "Next: preset" }), hasTouch);
  await expect(panel.getByRole("listbox", { name: "Presets" })).toBeVisible();
}

async function choosePreset(panel: Locator, title: string, hasTouch: boolean): Promise<void> {
  const option = panel.getByRole("listbox", { name: "Presets" }).getByRole("option").filter({ hasText: title });
  await press(option, hasTouch);
  await expect(option).toHaveAttribute("aria-selected", "true");
}

/** A job's row in the queue, by the file it converts. */
function jobRow(panel: Locator, name: string): Locator {
  return panel.getByRole("list", { name: "Jobs" }).getByRole("listitem").filter({ hasText: name });
}

test.describe("Convert, described over the UI protocol", () => {
  // Convert asks for Videos/ and Music/, so each fresh node asks once.
  test.beforeEach(async ({ page, hasTouch }) => {
    await approveApps(page, ["convert"], hasTouch);
  });

  test("the shell draws the window and lists the granted media: no frame, no page", async ({
    page,
    core,
    mode,
  }) => {
    seedMedia(core.work);
    await openApp(page, "convert", mode);
    const panel = panelOf(page);
    await expect(panel.getByRole("heading", { name: "Convert" })).toBeVisible();
    const files = panel.getByRole("list", { name: "Files" });
    for (const name of ["clip.mkv", "long.mkv", "song.mp3"]) {
      await expect(files.getByRole("listitem").filter({ hasText: name })).toHaveCount(1);
    }
    // Nothing picked: the next step is off, decided by the app.
    await expect(panel.getByRole("button", { name: "Next: preset" })).toBeDisabled();
    await expect(panel.getByText("0 selected · 3 files in Videos/, Music/")).toBeVisible();
    await expect(page.locator("iframe")).toHaveCount(0);
    const answer = await page.request.get("/apps/convert/");
    expect(answer.status()).toBe(404);
  });

  test("a remux checks as no quality change, converts, never overwrites, and leaves the source alone", async ({
    page,
    core,
    hasTouch,
    mode,
  }) => {
    const media = seedMedia(core.work);
    const before = sha256(media.clip);
    await openApp(page, "convert", mode);
    const panel = panelOf(page);

    await pickFiles(panel, ["clip.mkv"], hasTouch);
    await choosePreset(panel, "Change container", hasTouch);
    await press(panel.getByRole("button", { name: "Check", exact: true }), hasTouch);
    await expect(panel.getByText("No quality change: the streams are copied, not re-encoded.")).toBeVisible();
    await expect(panel.getByText(/the same as the source\.$/)).toBeVisible();

    // Show command reveals the exact line, with the input guard and file: URLs.
    await press(panel.getByRole("checkbox", { name: "Show command" }), hasTouch);
    await expect(panel.getByText(/-protocol_whitelist file .*file:.*clip\.mkv/)).toBeVisible();

    await press(panel.getByRole("button", { name: "Convert 1 file" }), hasTouch);
    // Its own batch moves the window to the queue, where the job finishes.
    const row = jobRow(panel, "clip.mkv");
    await expect(row.getByText("Done", { exact: true })).toBeVisible({ timeout: 30_000 });
    await expect(row.getByText("Videos/clip (Remux).mp4")).toBeVisible();
    expect(existsSync(join(core.work, "Videos", "clip (Remux).mp4"))).toBe(true);

    // The same batch again takes the next name; the first output is untouched.
    const first = sha256(join(core.work, "Videos", "clip (Remux).mp4"));
    await pickFiles(panel, ["clip.mkv"], hasTouch);
    await choosePreset(panel, "Change container", hasTouch);
    await press(panel.getByRole("button", { name: "Convert 1 file" }), hasTouch);
    await expect(panel.getByText("Videos/clip (Remux) 2.mp4")).toBeVisible({ timeout: 30_000 });
    expect(sha256(join(core.work, "Videos", "clip (Remux).mp4"))).toBe(first);
    expect(sha256(media.clip)).toBe(before);
    // No partial left behind, and nothing but what the jobs made.
    expect(listing(join(core.work, "Videos"))).toEqual([
      "clip (Remux) 2.mp4",
      "clip (Remux).mp4",
      "clip.mkv",
      "long.mkv",
    ]);

    // Clear finished forgets the jobs and keeps their files.
    await press(panel.getByRole("button", { name: "Clear finished" }), hasTouch);
    await expect(panel.getByText("Nothing queued.")).toBeVisible();
    expect(existsSync(join(core.work, "Videos", "clip (Remux) 2.mp4"))).toBe(true);
  });

  test("measuring Share encodes samples and states a size band and a VMAF score", async ({
    page,
    core,
    hasTouch,
    mode,
  }) => {
    seedMedia(core.work);
    await openApp(page, "convert", mode);
    const panel = panelOf(page);
    await pickFiles(panel, ["clip.mkv"], hasTouch);
    await choosePreset(panel, "Share (H.264, MP4)", hasTouch);
    await expect(panel.getByText(/^Not measured yet\./)).toBeVisible();
    await press(panel.getByRole("button", { name: "Measure on samples" }), hasTouch);
    await expect(panel.getByText(/^About .+ \(.+ to .+\), \d+% of the source\.$/)).toBeVisible({ timeout: 45_000 });
    await expect(panel.getByText(/^VMAF \d+, measured on 3 samples of your file/)).toBeVisible();
    await expect(panel.getByText(/^About .+ for this file, at [\d.]+× real time\.$/)).toBeVisible();
    // Measuring wrote nothing beside the source.
    expect(listing(join(core.work, "Videos"))).toEqual(["clip.mkv", "long.mkv"]);
  });

  test("FLAC from an MP3 is refused with the reason until it is allowed", async ({ page, core, hasTouch, mode }) => {
    seedMedia(core.work);
    await openApp(page, "convert", mode);
    const panel = panelOf(page);
    await pickFiles(panel, ["song.mp3"], hasTouch);
    await choosePreset(panel, "Audio as FLAC", hasTouch);
    await press(panel.getByRole("button", { name: "Measure on samples" }), hasTouch);
    const refusal = panel.locator('.ui-text[data-tone="danger"]').filter({ hasText: "bigger and no better" });
    await expect(refusal).toBeVisible({ timeout: 20_000 });

    // The control: allowed, the same file converts.
    await press(panel.getByRole("checkbox", { name: "Allow from lossy" }), hasTouch);
    await press(panel.getByRole("button", { name: "Convert 1 file" }), hasTouch);
    const row = jobRow(panel, "song.mp3");
    await expect(row.getByText("Done", { exact: true })).toBeVisible({ timeout: 30_000 });
    await expect(row.getByText("Music/song (FLAC).flac")).toBeVisible();
    expect(existsSync(join(core.work, "Music", "song (FLAC).flac"))).toBe(true);
  });

  test("cancelling a running job leaves no partial and the next job runs", async ({ page, core, hasTouch, mode }) => {
    const media = seedMedia(core.work);
    const before = sha256(media.long);
    await openApp(page, "convert", mode);
    const panel = panelOf(page);
    // The list is newest first; tick in the order the queue should run.
    await pickFiles(panel, ["long.mkv", "song.mp3"], hasTouch);
    await choosePreset(panel, "Audio as Opus", hasTouch);
    await press(panel.getByRole("button", { name: "Convert 2 files" }), hasTouch);

    const long = jobRow(panel, "long.mkv");
    const song = jobRow(panel, "song.mp3");
    // Opus of a 120 s file is quick; the point here is only the queue order
    // and Clear. The cancel itself is checked on the AV1 job below.
    await expect(song.getByText("Done", { exact: true })).toBeVisible({ timeout: 30_000 });
    await expect(long.getByText("Done", { exact: true })).toBeVisible({ timeout: 30_000 });
    await press(panel.getByRole("button", { name: "Clear finished" }), hasTouch);
    await expect(panel.getByText("Nothing queued.")).toBeVisible();

    await pickFiles(panel, ["long.mkv", "clip.mkv"], hasTouch);
    await choosePreset(panel, "Small (AV1, MKV)", hasTouch);
    await press(panel.getByRole("button", { name: "Convert 2 files" }), hasTouch);
    // Running: a bar, and it moves on its own, with no event from this page.
    await expect(long.getByText("Converting", { exact: true })).toBeVisible({ timeout: 20_000 });
    await expect(long.getByRole("progressbar")).toBeVisible();
    // Past "0% · starting": ffmpeg has opened its output by now.
    await expect(long.getByText(/^[1-9]\d*% · /)).toBeVisible({ timeout: 20_000 });
    // A partial exists while it runs: hidden, in the destination folder.
    await expect.poll(() => listing(join(core.work, "Videos")).some((f) => f.startsWith("."))).toBe(true);

    await press(long.getByRole("button", { name: "Cancel" }), hasTouch);
    await expect(long.getByText("Cancelled; nothing was kept.")).toBeVisible();
    // The next one starts and finishes.
    const clip = jobRow(panel, "clip.mkv");
    await expect(clip.getByText("Done", { exact: true })).toBeVisible({ timeout: 45_000 });
    expect(listing(join(core.work, "Videos"))).toEqual(["clip (Small).mkv", "clip.mkv", "long (Opus).opus", "long.mkv"]);
    expect(sha256(media.long)).toBe(before);
    // A cancelled job can be retried.
    await expect(long.getByRole("button", { name: "Retry" })).toBeEnabled();
  });
});
