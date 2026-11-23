import { readdirSync } from "node:fs";
import { join } from "node:path";

import { appFrame, approveApps, expect, expectMode, nodeStatus, openApp, press, test } from "./support/fixtures.ts";
import { delaySocket } from "./support/latency.ts";
import { seedTasks } from "./support/seed.ts";

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

test.describe("Tasks, described over the UI protocol", () => {
  // Tasks asks for its folder, so each fresh node asks for it once.
  test.beforeEach(async ({ page, hasTouch }) => {
    await approveApps(page, ["tasks"], hasTouch);
  });

  const icsFiles = (work: string) => {
    try {
      return readdirSync(join(work, "Tasks")).filter((f) => f.endsWith(".ics"));
    } catch {
      return [];
    }
  };

  test("the shell draws the window: no frame, and no page served by the app", async ({ page, mode }) => {
    await openApp(page, "tasks", mode);
    const panel = page.locator('.ui-panel[data-app="tasks"]');
    await expect(panel.getByRole("heading", { name: "Tasks" })).toBeVisible();
    await expect(panel.getByText("Nothing to do.")).toBeVisible();
    await expect(panel.getByText("0 open, 0 done")).toBeVisible();
    await expect(page.locator("iframe")).toHaveCount(0);
    // The app ships no page any more: the core has nothing to serve for it.
    const answer = await page.request.get("/apps/tasks/");
    expect(answer.status()).toBe(404);
  });

  test("adds, completes, reveals and deletes tasks, and the files follow", async ({
    page,
    core,
    hasTouch,
    mode,
  }) => {
    await openApp(page, "tasks", mode);
    const panel = page.locator('.ui-panel[data-app="tasks"]');
    const field = panel.getByRole("textbox", { name: "New task" });
    const add = panel.getByRole("button", { name: "Add", exact: true });
    // Nothing to add yet: the button is off, decided locally.
    await expect(add).toBeDisabled();

    // Enter adds, as the page's form did, and the field clears afterwards.
    await field.fill("Buy milk");
    await expect(add).toBeEnabled();
    await field.press("Enter");
    const milk = panel.locator(".ui-list-row", { hasText: "Buy milk" });
    await expect(milk).toHaveCount(1);
    await expect(field).toHaveValue("");
    await expect(panel.getByText("1 open, 0 done")).toBeVisible();
    expect(icsFiles(core.work)).toHaveLength(1);

    // The Add button, with a priority: the row shows three marks.
    await field.fill("Call the plumber");
    await panel.getByRole("combobox", { name: "Priority" }).selectOption({ label: "High" });
    await press(add, hasTouch);
    const plumber = panel.locator(".ui-list-row", { hasText: "Call the plumber" });
    await expect(plumber).toHaveCount(1);
    await expect(plumber.getByText("!!!")).toBeVisible();
    await expect(panel.getByRole("combobox", { name: "Priority" })).toHaveValue("0");
    expect(icsFiles(core.work)).toHaveLength(2);

    // Done hides it while "Show done" is off.
    await press(milk.getByRole("checkbox", { name: "Done" }), hasTouch);
    await expect(milk).toHaveCount(0);
    await expect(panel.getByText("1 open, 1 done")).toBeVisible();
    await press(panel.getByRole("checkbox", { name: "Show done" }), hasTouch);
    await expect(milk).toHaveCount(1);
    await expect(milk.locator(".ui-text[data-strike]")).toHaveText("Buy milk");

    // Delete is destructive, so the node asks, in its own words, first.
    await press(milk.getByRole("button", { name: "Delete" }), hasTouch);
    const confirm = page.getByRole("alertdialog");
    await press(confirm.locator("button.go"), hasTouch);
    await expect(milk).toHaveCount(0);
    expect(icsFiles(core.work)).toHaveLength(1);
  });

  test("typing, ticking and scrolling never wait for the network, at 500 ms round trip", async ({
    page,
    core,
    hasTouch,
    mode,
  }) => {
    test.setTimeout(180_000);
    seedTasks(core.work, 10_000);
    const wire = await delaySocket(page, 250);
    await openApp(page, "tasks", mode);
    const panel = page.locator('.ui-panel[data-app="tasks"]');
    const list = panel.getByRole("list", { name: "Tasks" });
    await expect(list.locator(".ui-list-row").first()).toContainText("Row 00000", { timeout: 60_000 });
    await expect(panel.getByText("10000 open, 0 done")).toBeVisible();

    // Typing: every character is on screen at once. Anything that waited for
    // the node would take at least the 500 ms round trip.
    const field = panel.getByRole("textbox", { name: "New task" });
    await press(field, hasTouch);
    const upBefore = wire.up;
    const typedAt = Date.now();
    await page.keyboard.type("local");
    await expect(field).toHaveValue("local", { timeout: 250 });
    expect(Date.now() - typedAt).toBeLessThan(500);
    // The node hears one `changed` after the pause, not one per key, and its
    // answer, a round trip later, leaves the text and the caret alone.
    await expect.poll(() => wire.up - upBefore, { timeout: 5_000 }).toBe(1);
    const downBefore = wire.down;
    await expect.poll(() => wire.down - downBefore, { timeout: 5_000 }).toBeGreaterThan(0);
    expect(wire.up - upBefore).toBe(1);
    await expect(field).toHaveValue("local");
    expect(await field.evaluate((el: HTMLInputElement) => el.selectionStart)).toBe(5);

    // Scrolling ten thousand rows: the rows for the new position are drawn
    // from the tree the shell holds, and not one frame crosses the wire.
    const before = { up: wire.up, down: wire.down };
    const drawn = await list.evaluate(async (el: HTMLElement) => {
      const row = el.querySelector<HTMLElement>(".ui-list-row")!;
      const started = performance.now();
      el.scrollTop = 5000 * row.offsetHeight;
      el.dispatchEvent(new Event("scroll"));
      await new Promise((r) => requestAnimationFrame(() => requestAnimationFrame(r)));
      const keys = [...el.querySelectorAll<HTMLElement>(".ui-list-row")].map((r) => r.dataset.key ?? "");
      return { ms: performance.now() - started, keys, inDom: keys.length };
    });
    expect(drawn.keys).toContain("e2e-05000");
    expect(drawn.inDom).toBeLessThan(200);
    expect(drawn.ms).toBeLessThan(500);
    expect(wire.up - before.up).toBe(0);
    expect(wire.down - before.down).toBe(0);

    // Ticking: the box is checked at once; the consequence (the done task
    // leaving the list) is the app's, and comes a round trip later. Until
    // then it never shows anything but ticked: sampled in the page every
    // 20 ms from the click until the row is gone.
    const row = list.locator('.ui-list-row[data-key="e2e-05001"]');
    const tick = row.getByRole("checkbox", { name: "Done" });
    const watched = tick.evaluate(async (el: HTMLInputElement) => {
      const seen: boolean[] = [];
      await new Promise((r) => el.addEventListener("change", r, { once: true }));
      const started = performance.now();
      while (el.isConnected && performance.now() - started < 30_000) {
        seen.push(el.checked);
        await new Promise((r) => setTimeout(r, 20));
      }
      return { seen, ms: performance.now() - started };
    });
    const tickedAt = Date.now();
    await press(tick, hasTouch);
    await expect(tick).toBeChecked({ timeout: 250 });
    const shownAt = Date.now() - tickedAt;
    await expect(row).toHaveCount(0, { timeout: 30_000 });
    const goneAt = Date.now() - tickedAt;
    expect(shownAt).toBeLessThan(500);
    expect(goneAt).toBeGreaterThanOrEqual(500);
    const { seen } = await watched;
    expect(seen.length).toBeGreaterThan(10);
    expect(seen.every(Boolean), `the box unticked itself before its row left: ${seen}`).toBe(true);
    // The patches that removed the row reached the window while the field
    // held typed text: the text is still the operator's.
    await expect(field).toHaveValue("local");

    // Ticking "Show done" on and off again inside one round trip: the app's
    // answer to the first tick arrives after the second, and must not tick
    // the box again. Sampled every 20 ms for two seconds, four round trips.
    const show = panel.getByRole("checkbox", { name: "Show done" });
    const states = await show.evaluate(async (el: HTMLInputElement) => {
      el.click();
      await new Promise((r) => setTimeout(r, 50));
      el.click();
      const seen: boolean[] = [];
      const started = performance.now();
      while (performance.now() - started < 2_000) {
        seen.push(el.checked);
        await new Promise((r) => setTimeout(r, 20));
      }
      return seen;
    });
    expect(states.every((on) => !on), `the box ticked itself again: ${states}`).toBe(true);
    await expect(list.locator('.ui-list-row[data-key="e2e-05001"]')).toHaveCount(0);
  });
});

test.describe("a streamed native app", () => {
  // zenity's entry dialog (GTK 4), declared in the node's `native/` and run
  // under the compositor, confined, by the node itself (see
  // `support/core.ts`). What is asserted is what a person sees: the dialog's
  // pixels decoded into the window, and what they typed reaching the app,
  // which prints it when Enter accepts the dialog and then exits.
  test("opens from the dock, is marked streamed, takes keys, and goes when the app exits", async ({
    page,
    core,
    hasTouch,
    mode,
  }) => {
    test.skip(!core.native, "zenity or the deckjack-stream build is missing on this host");
    await openApp(page, "native:zenity", mode);
    const panel = page.locator(".stream[data-streamed]");
    await expect(panel).toHaveAttribute("data-status", "live", { timeout: 20_000 });
    if (mode === "desktop") {
      // Two native apps are declared (support/core.ts), both marked.
      await expect(page.locator(".bar .item .tag[data-streamed]")).toHaveCount(2);
      await expect(page.locator(".window[data-streamed] .streamed")).toHaveText("Streamed");
    }
    // Decoded pixels, not a blank canvas: GTK's light dialog background.
    const light = await panel.locator("canvas").evaluate((c: HTMLCanvasElement) => {
      const ctx = c.getContext("2d");
      if (!ctx || c.width === 0) return 0;
      const { data } = ctx.getImageData(Math.floor(c.width / 2), 4, 1, 1);
      return (data[0]! + data[1]! + data[2]!) / 3;
    });
    expect(light, "the canvas shows no decoded picture").toBeGreaterThan(150);

    await press(panel.locator("canvas"), hasTouch);
    await page.keyboard.type("dj e2e");
    await page.keyboard.press("Enter");
    // zenity prints the entry and exits; the window goes with it.
    await expect(panel).toHaveCount(0, { timeout: 15_000 });
    await expect.poll(() => core.log()).toContain("dj e2e");
  });

  // A second streamed window takes the shell's focus when it maps, and
  // with it the keyboard: on the desktop the browser's focus moves to its
  // canvas; in compact mode the one panel is handed the new window and must
  // stream that window, not keep the first one's socket.
  test("hands the keyboard to the streamed window the shell focuses", async ({ page, core, hasTouch, mode }) => {
    test.skip(!core.native, "zenity or the deckjack-stream build is missing on this host");
    await openApp(page, "native:zenity", mode);
    const live = page.locator('.stream[data-streamed][data-status="live"]');
    await expect(live).toHaveCount(1, { timeout: 20_000 });
    // The first window has the browser's keyboard.
    await press(live.first().locator("canvas"), hasTouch);
    await press(page.locator(".bar button.brand"), hasTouch);
    const palette = page.getByRole("dialog", { name: "Command palette" });
    await palette.getByRole("textbox", { name: "Command" }).fill("Zenity Two");
    await press(palette.locator('[id="dj-cmd-app.open.native:zenity2"] button'), hasTouch);
    const second = page.locator('.stream[data-streamed][data-status="live"]:has(canvas[aria-label^="Second"])');
    await expect(second).toHaveCount(1, { timeout: 20_000 });
    await page.keyboard.type("two");
    await page.keyboard.press("Enter");
    await expect.poll(() => core.log(), { timeout: 15_000 }).toContain("second:two");
    expect(core.log(), "the first window took the keys").not.toMatch(/^two$/m);
  });

  // The stream moves with the workspace: the device that takes the seat
  // gets the window, a keyframe for it on attach (so it shows the dialog,
  // not a blank or a smear), and the input; the device it was taken from
  // gets neither.
  test("moves to the device that takes the seat", async ({ page, browser, core, mode }) => {
    test.skip(!core.native, "zenity or the deckjack-stream build is missing on this host");
    await openApp(page, "native:zenity", mode);
    await expect(page.locator(".stream[data-streamed]")).toHaveAttribute("data-status", "live", {
      timeout: 20_000,
    });

    const code = /sign-in code\s+(\S+)/.exec(core.log())?.[1];
    expect(code, "the node printed no sign-in code").toBeTruthy();
    const other = await browser.newContext({ baseURL: core.origin, serviceWorkers: "block" });
    try {
      const phone = await other.newPage();
      await phone.goto("/login");
      await phone.locator("#code").fill(code!);
      await phone.getByRole("button", { name: "Log in" }).click();
      const takeover = phone.getByTestId("seat-takeover");
      await takeover.getByRole("button", { name: "Take over" }).click();
      await phone.getByRole("alertdialog").getByRole("button", { name: "Take over" }).click();
      await expect(takeover).toHaveCount(0);

      const theirs = phone.locator(".stream[data-streamed]");
      await expect(theirs).toHaveAttribute("data-status", "live", { timeout: 20_000 });
      await expect(page.locator(".stream")).toHaveCount(0);
      await theirs.locator("canvas").click();
      await phone.keyboard.type("moved");
      await phone.keyboard.press("Enter");
      await expect(theirs).toHaveCount(0, { timeout: 15_000 });
      await expect.poll(() => core.log()).toContain("moved");
    } finally {
      await other.close();
    }
  });
});
