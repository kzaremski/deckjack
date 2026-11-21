// @vitest-environment happy-dom
import { flushPromises } from "@vue/test-utils";
import { afterEach, describe, expect, it } from "vitest";

import SplitView from "./SplitView.vue";
import { browserViewportEnv, createViewport } from "../shell/viewport.ts";
import { harness, mountInShell } from "../testing/shell-harness.ts";

/**
 * The compact split, on the browser's own viewport measurement.
 *
 * The harness's viewport reports a fixed size, which is right for most
 * component tests and hides exactly this one: what the shell measures when a
 * phone's soft keyboard opens. So here the shell's viewport is the
 * production one, reading a page whose layout and visual viewports the test
 * sets the way a phone browser does.
 */

const restore: Array<() => void> = [];
afterEach(() => {
  for (const r of restore.splice(0).reverse()) r();
});

function setProperty(target: object, key: string, value: unknown) {
  const had = Object.getOwnPropertyDescriptor(target, key);
  Object.defineProperty(target, key, { configurable: true, writable: true, value });
  restore.push(() => {
    if (had) Object.defineProperty(target, key, had);
    else delete (target as Record<string, unknown>)[key];
  });
}

/** The layout viewport, both ways a page can read it. */
function layout(w: number, h: number) {
  setProperty(window, "innerWidth", w);
  setProperty(window, "innerHeight", h);
  setProperty(document.documentElement, "clientWidth", w);
  setProperty(document.documentElement, "clientHeight", h);
}

/** A phone browser: the keyboard overlays the page and shrinks only the visual viewport. */
function phone(w: number, h: number) {
  layout(w, h);
  const vv = Object.assign(new EventTarget(), { width: w, height: h, offsetTop: 0, offsetLeft: 0 });
  setProperty(window, "visualViewport", vv);
  return {
    openKeyboard(px: number) {
      vv.height = h - px;
      vv.dispatchEvent(new Event("resize"));
    },
  };
}

async function mountSplit() {
  const rig = harness({ coarsePointer: true });
  const viewport = createViewport(browserViewportEnv());
  restore.push(() => viewport.stop());
  (rig.shell as { viewport: typeof viewport }).viewport = viewport;
  rig.connect();
  void rig.shell.windows.open("notes");
  void rig.shell.windows.open("files");
  const [a, b] = rig.shell.windows.all.value.map((w) => w.id) as [string, string];
  rig.shell.windows.setSplit(a, b);
  const { wrapper } = mountInShell(SplitView, {
    harness: rig,
    global: { stubs: { AppSurface: true } },
  });
  await flushPromises();
  restore.push(() => wrapper.unmount());
  return wrapper;
}

describe("a split on a portrait phone", () => {
  it("stays stacked while the soft keyboard is open", async () => {
    // An iPhone SE class screen: 375 wide, about 550 of page in Safari. The
    // keyboard takes about 280 of it, so the visible area is shorter than it
    // is wide. The layout has not changed shape -- the keyboard sits over
    // it -- so the panes must not reflow into two 180px columns while the
    // operator types and back again when the keyboard closes.
    const device = phone(375, 550);
    const wrapper = await mountSplit();
    const split = () => wrapper.find(".split");
    expect(split().classes(), "a portrait phone did not start stacked").toContain("stacked");

    device.openKeyboard(280);
    await flushPromises();
    expect(split().classes(), "opening the keyboard flipped the split side by side").toContain("stacked");

    // Browsers also fire the window's resize around the keyboard (Safari's
    // toolbars collapse as it opens). A re-measure then must still read the
    // layout, not the area left above the keyboard.
    window.dispatchEvent(new Event("resize"));
    await flushPromises();
    expect(split().classes(), "a re-measure with the keyboard open flipped the split").toContain("stacked");
  });

  it("still goes side by side when the phone turns to landscape", async () => {
    // The other half of the promise: the decision does follow the layout.
    phone(375, 550);
    const wrapper = await mountSplit();
    layout(550, 375);
    const vv = window.visualViewport as unknown as { width: number; height: number } & EventTarget;
    vv.width = 550;
    vv.height = 375;
    window.dispatchEvent(new Event("resize"));
    await flushPromises();

    expect(wrapper.find(".split").classes(), "a landscape phone stayed stacked").not.toContain("stacked");
  });
});
