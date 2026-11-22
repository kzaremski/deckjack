import { appFrame, approveApps, expect, expectMode, nodeStatus, openApp, press, test } from "./support/fixtures.ts";

/**
 * Smoke tests for the real shell against a real node.
 *
 * Each one runs in every shell project: desktop and phone, Chromium and
 * WebKit. On the phone projects the shell is in compact mode and the tests
 * tap; the assertions are the same, because the promise is the same (APP-SUITE
 * §3, "full parity, touch-first").
 *
 * Every assertion is on an effect a person would see: rows the core listed,
 * pixels the browser decoded, text the core read from disk, output a shell
 * computed. None is on the shell's internal state.
 */

test.describe("signing in", () => {
  test("the shell loads, is signed in by the session cookie, and connects", async ({ page, core, mode }) => {
    await page.goto("/");
    await expect(page).toHaveURL(`${core.origin}/`);
    await expect(nodeStatus(page)).toHaveAttribute("data-status", "open");
    // In the layout for the device: compact on a phone, desktop otherwise.
    await expectMode(page, mode);
    // The card that covers the shell while there is no node is gone.
    await expect(page.getByRole("alert")).toHaveCount(0);
  });

  test.describe("without the cookie", () => {
    test.use({ signedOut: true });

    // The control for the test above: the same node, no cookie, and the shell
    // is not served. Without it, "signed in" could mean "the gate is open".
    test("the shell sends the browser to sign in", async ({ page, core }) => {
      await page.goto("/");
      await expect(page).toHaveURL(`${core.origin}/login`);
    });
  });
});

test.describe("Files", () => {
  // Both declare a `[ui]` scope, so each fresh node asks for them once.
  test.beforeEach(async ({ page, hasTouch }) => {
    await approveApps(page, ["filer", "scribe"], hasTouch);
  });

  test("opens and lists the working directory", async ({ page, core, mode }) => {
    await openApp(page, "filer", mode);
    const files = appFrame(page, "Files");
    const rows = files.locator("#list .dj-row");
    await expect(rows.filter({ hasText: core.files.text.name })).toHaveCount(1);
    await expect(rows.filter({ hasText: core.files.image.name })).toHaveCount(1);
    // The node's own state directory is never listed.
    await expect(rows.filter({ hasText: ".deckjack" })).toHaveCount(0);
  });

  test("Quick Look opens an image and the browser decodes it", async ({ page, core, hasTouch, mode }) => {
    await openApp(page, "filer", mode);
    const row = appFrame(page, "Files").locator("#list .dj-row", { hasText: core.files.image.name });
    // Opening an image previews it: a tap on a phone, a double-click on the
    // desktop. Neither gesture is special-cased for the test.
    if (hasTouch) await row.tap();
    else await row.dblclick();

    const preview = page.getByRole("dialog", { name: `Preview of ${core.files.image.name}` });
    await expect(preview).toBeVisible();
    const picture = preview.locator("img.picture");
    await expect
      .poll(() => picture.evaluate((img: HTMLImageElement) => [img.naturalWidth, img.naturalHeight]))
      .toEqual([core.files.image.width, core.files.image.height]);
  });

  test("Quick Look opens a text file and shows what is on disk", async ({ page, core, hasTouch, mode }) => {
    await openApp(page, "filer", mode);
    const files = appFrame(page, "Files");
    // A text file opens in the editor, so preview goes through selection:
    // Select, pick the row, Quick Look. The same three steps on a phone.
    await press(files.locator("#select"), hasTouch);
    await press(files.locator("#list .dj-row", { hasText: core.files.text.name }), hasTouch);
    await press(files.locator("#peek"), hasTouch);

    const preview = page.getByRole("dialog", { name: `Preview of ${core.files.text.name}` });
    await expect(preview).toBeVisible();
    await expect(preview.locator("pre.source")).toContainText(core.files.text.firstLine);
  });

  test("Open with opens the editor on that file", async ({ page, core, hasTouch, mode }) => {
    await openApp(page, "filer", mode);
    const files = appFrame(page, "Files");
    await press(files.locator("#select"), hasTouch);
    await press(files.locator("#list .dj-row", { hasText: core.files.text.name }), hasTouch);
    await press(files.locator("#openwith"), hasTouch);
    await press(files.getByRole("menuitem", { name: "Text editor" }), hasTouch);

    // The editor's own window, opened on the file Files named, showing the
    // text the core read from disk.
    const frame = page.locator('iframe[src^="/apps/scribe/"]');
    await expect(frame).toHaveCount(1);
    await expect(frame).toHaveAttribute("src", new RegExp(`[?&]path=%2F${core.files.text.name}(&|$)`));
    const editor = page.frameLocator('iframe[src^="/apps/scribe/"]');
    await expect(editor.locator(".cm-content")).toContainText(core.files.text.firstLine);
  });
});

test.describe("Terminal", () => {
  test("opens a shell that runs what is typed", async ({ page, hasTouch, mode }) => {
    await openApp(page, "terminal", mode);
    const terminal = page.locator(".terminal");
    await expect(terminal.locator(".xterm")).toBeVisible();
    // Wait for the prompt, as a person would. Keys typed before the panel
    // has a session are dropped today (`typeInto` with no active terminal),
    // which is a typeahead defect of its own, not what this test is about.
    await expect(terminal.locator(".xterm-rows")).toContainText("$");
    await press(terminal.locator(".xterm-screen"), hasTouch);
    // The shell computes 6*7, so 42 on screen is output, not the echo of
    // what was typed.
    await page.keyboard.type("echo dj-e2e-$((6*7))");
    await page.keyboard.press("Enter");
    await expect(terminal.locator(".xterm-rows")).toContainText("dj-e2e-42");
  });
});

test.describe("the single seat", () => {
  // Two signed-in devices on one node: the project's own (signed in by the
  // debug session) and a second browser context signed in with the node's
  // one-time code, so the two are different sessions, as two devices are.
  test("another device takes over the workspace, terminal included, and it can be taken back", async ({
    page,
    browser,
    core,
    hasTouch,
    mode,
  }) => {
    await page.addInitScript(() => {
      try {
        localStorage.setItem("deckjack.device", "Desk one");
      } catch {
        // A context that refuses storage fails the assertions below.
      }
    });
    await openApp(page, "terminal", mode);
    const terminal = page.locator(".terminal");
    await expect(terminal.locator(".xterm-rows")).toContainText("$");
    await press(terminal.locator(".xterm-screen"), hasTouch);
    await page.keyboard.type("echo dj-seat-$((6*7))");
    await page.keyboard.press("Enter");
    await expect(terminal.locator(".xterm-rows")).toContainText("dj-seat-42");

    const code = /sign-in code\s+(\S+)/.exec(core.log())?.[1];
    expect(code, "the node printed no sign-in code").toBeTruthy();
    const other = await browser.newContext({ baseURL: core.origin, serviceWorkers: "block" });
    try {
      const phone = await other.newPage();
      await phone.goto("/login");
      await phone.locator("#code").fill(code!);
      await phone.getByRole("button", { name: "Log in" }).click();
      await expect(phone).toHaveURL(`${core.origin}/`);

      // The second device is told who has it, and is not shown the workspace.
      const takeover = phone.getByTestId("seat-takeover");
      await expect(takeover.getByRole("heading")).toHaveText("Deckjack is in use on Desk one");
      await expect(phone.locator(".bar")).toHaveCount(0);

      await takeover.getByRole("button", { name: "Rename" }).click();
      await takeover.getByLabel("Name this device").fill("Phone two");
      await takeover.getByRole("button", { name: "Save" }).click();
      await expect(phone.getByTestId("seat-device")).toHaveText("Phone two");

      await takeover.getByRole("button", { name: "Take over" }).click();
      await phone.getByRole("alertdialog").getByRole("button", { name: "Take over" }).click();

      // The workspace moved: the terminal window is here, and it is the same
      // shell, replaying what the first device ran in it.
      await expect(takeover).toHaveCount(0);
      await expect(phone.locator(".terminal .xterm-rows")).toContainText("dj-seat-42");

      // The first device was taken over, and can take it back.
      const mine = page.getByTestId("seat-takeover");
      await expect(mine.getByRole("heading")).toHaveText("Taken over by Phone two");
      await expect(page.locator(".terminal")).toHaveCount(0);
      // The takeover screen has the heading focused, and the kill switch
      // stays reachable from it: the chord asks to lock down (declined
      // here, so the node carries on for the rest of the suite).
      await expect(mine.getByRole("heading")).toBeFocused();
      // `mod` is Meta where the shell decides it is an Apple device.
      const apple = await page.evaluate(() =>
        /mac|iphone|ipad|ipod/i.test(navigator.platform || navigator.userAgent),
      );
      await page.keyboard.press(apple ? "Meta+Alt+l" : "Control+Alt+l");
      const lockdown = page.getByRole("alertdialog");
      await expect(lockdown).toContainText("Lock down Deckjack?");
      await lockdown.getByRole("button", { name: "Cancel" }).click();
      await expect(lockdown).toHaveCount(0);
      await expect(mine.getByRole("button", { name: "Lock down this workspace" })).toBeVisible();
      await press(mine.getByRole("button", { name: "Take back" }), hasTouch);
      await press(page.getByRole("alertdialog").getByRole("button", { name: "Take back" }), hasTouch);
      await expect(mine).toHaveCount(0);
      await expect(page.locator(".terminal .xterm-rows")).toContainText("dj-seat-42");
      await expect(phone.getByTestId("seat-takeover").getByRole("heading")).toHaveText(
        "Taken over by Desk one",
      );
    } finally {
      await other.close();
    }
  });
});
