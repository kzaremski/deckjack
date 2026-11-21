import { test as base, expect, type FrameLocator, type Locator, type Page } from "@playwright/test";

import { type Core, startCore } from "./core.ts";
import { BASE_PORT } from "./paths.ts";

/**
 * The fixtures every shell test uses.
 *
 * - `core`: a fresh `--debug` node for this test (see `core.ts`).
 * - `baseURL`: that node's origin, so `page.goto("/")` reaches it.
 * - `context`: a browser context carrying the node's debug session cookie,
 *   exactly as the node wrote it, so the shell is signed in the way a
 *   passkey sign-in would leave it. `signedOut` skips the cookie, for the
 *   control that proves the cookie is what signs in.
 */
export type ShellMode = "desktop" | "compact";

export interface ShellOptions {
  /**
   * The layout the shell must be in for this project: set per project in
   * the config, and checked by `openApp`, so a phone project that silently
   * got the desktop layout fails instead of passing as phone coverage.
   */
  mode: ShellMode;
}

export const test = base.extend<{ core: Core; signedOut: boolean } & ShellOptions>({
  signedOut: [false, { option: true }],
  mode: ["desktop", { option: true }],

  core: async ({}, use, testInfo) => {
    const core = await startCore(BASE_PORT + testInfo.parallelIndex);
    try {
      await use(core);
    } finally {
      const log = core.log();
      await core.stop();
      if (testInfo.status !== testInfo.expectedStatus) {
        await testInfo.attach("core.log", { body: log, contentType: "text/plain" });
      }
    }
  },

  baseURL: async ({ core }, use) => {
    await use(core.origin);
  },

  context: async ({ context, core, signedOut }, use) => {
    if (!signedOut) {
      const { name, value } = core.cookie;
      await context.addCookies([
        {
          name,
          value,
          domain: "127.0.0.1",
          path: "/",
          httpOnly: true,
          sameSite: "Strict",
          // A `__Host-`/`__Secure-` cookie is refused by the browser unless
          // it is Secure. 127.0.0.1 counts as a secure context.
          secure: name.startsWith("__Host-") || name.startsWith("__Secure-"),
        },
      ]);
    }
    await use(context);
  },
});

export { expect };

/**
 * Tap on a touch project (the `hasTouch` fixture), click otherwise: the
 * gesture a person on that device would use.
 */
export async function press(target: Locator, hasTouch: boolean): Promise<void> {
  if (hasTouch) await target.tap();
  else await target.click();
}

/** The shell's connection indicator, which reads `open` once `/ws` is up. */
export function nodeStatus(page: Page): Locator {
  return page.locator(".bar .node");
}

/**
 * The shell is in the layout this project is for: its mode attribute, and
 * the layout's own container, so a mode that flipped without the layout
 * following is caught too.
 */
export async function expectMode(page: Page, mode: ShellMode): Promise<void> {
  await expect(page.locator(".shell")).toHaveAttribute("data-mode", mode);
  await expect(page.locator(".shell > .compact")).toHaveCount(mode === "compact" ? 1 : 0);
}

/**
 * Open an app the way a PWA shortcut does, wait for the socket, and check the
 * shell is in the layout this project is for.
 */
export async function openApp(page: Page, app: string, mode: ShellMode): Promise<void> {
  await page.goto(`/?open=${encodeURIComponent(app)}`);
  await expect(nodeStatus(page)).toHaveAttribute("data-status", "open");
  await expectMode(page, mode);
}

/**
 * Approve installed apps the way the operator does: the palette's
 * "Approve <title>" entry, then the node's own confirmation. Every app that
 * declares a `[ui]` scope needs it once (`deckjack install` never approves),
 * and the approval is the node's, so it outlasts the page loads that follow.
 */
export async function approveApps(page: Page, apps: readonly string[], hasTouch: boolean): Promise<void> {
  await page.goto("/");
  await expect(nodeStatus(page)).toHaveAttribute("data-status", "open");
  for (const app of apps) {
    await press(page.locator(".bar button.brand"), hasTouch);
    const palette = page.getByRole("dialog", { name: "Command palette" });
    await palette.getByRole("textbox", { name: "Command" }).fill("approve");
    await press(palette.locator(`[id="dj-cmd-app.approve.${app}"] button`), hasTouch);
    const confirm = page.getByRole("alertdialog");
    await press(confirm.locator("button.go"), hasTouch);
    await expect(confirm).toHaveCount(0);
    // Approved: the entry is gone once the installed list is asked again.
    await press(page.locator(".bar button.brand"), hasTouch);
    await palette.getByRole("textbox", { name: "Command" }).fill("approve");
    await expect(palette.locator(`[id="dj-cmd-app.approve.${app}"]`)).toHaveCount(0);
    await page.keyboard.press("Escape");
    await expect(palette).toHaveCount(0);
  }
}

/** An app window's frame, by the window title the shell gives it. */
export function appFrame(page: Page, title: string): FrameLocator {
  return page.frameLocator(`iframe[title="${title}"]`);
}
