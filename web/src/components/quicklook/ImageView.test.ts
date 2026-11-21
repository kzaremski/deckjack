// @vitest-environment happy-dom
import { flushPromises } from "@vue/test-utils";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { nextTick } from "vue";

import QuickLook from "../QuickLook.vue";
import { harness, mountInShell } from "../../testing/shell-harness.ts";

/**
 * The photograph viewer, driven the way a phone drives it: whole pointer
 * sequences, in the orders fingers actually produce, through the real Quick
 * Look overlay, so a gesture that leaks out of the viewer shows up as the
 * overlay changing file.
 */

/** The page clock, which the test moves so taps land a known time apart. */
let clock = 1_000;

beforeEach(() => {
  clock = 1_000;
  vi.spyOn(performance, "now").mockImplementation(() => clock);
});
afterEach(() => {
  vi.restoreAllMocks();
  vi.unstubAllGlobals();
});

async function openOn(path: string, siblings?: string[]) {
  const rig = harness();
  const { wrapper } = mountInShell(QuickLook, { harness: rig, attachTo: document.body });
  rig.shell.quicklook.open(path, siblings);
  await flushPromises();
  const box = wrapper.find("[data-scale]").element;
  return { rig, wrapper, box };
}

type Kind = "touch" | "mouse";

function pointer(el: Element, type: string, x: number, y = 100, id = 1, pointerType: Kind = "touch") {
  el.dispatchEvent(
    new PointerEvent(type, {
      pointerId: id,
      pointerType,
      isPrimary: id === 1,
      clientX: x,
      clientY: y,
      bubbles: true,
    }),
  );
}

/** One quick tap at a point: down and up with no travel. */
function tap(el: Element, x: number, y: number, pointerType: Kind = "touch") {
  pointer(el, "pointerdown", x, y, 1, pointerType);
  pointer(el, "pointerup", x, y, 1, pointerType);
}

function dblclick(el: Element, x: number, y: number) {
  el.dispatchEvent(new MouseEvent("dblclick", { clientX: x, clientY: y, bubbles: true }));
}

describe("a pinch on a photograph", () => {
  it("does not become a swipe when the second finger lifts first", async () => {
    // The left finger is the primary one and lands while the photo is
    // fitted, so the overlay starts tracking it as a possible swipe. The
    // spread carries it 150px left. Lifting the right finger first used to
    // end the pinch early, so the left finger's lift reached the overlay as
    // a 150px swipe and Quick Look moved on to the next file.
    const { rig, wrapper, box } = await openOn("/a.jpg", ["/a.jpg", "/b.jpg"]);
    pointer(box, "pointerdown", 250, 100, 1);
    pointer(box, "pointerdown", 300, 100, 2);
    pointer(box, "pointermove", 100, 100, 1);
    pointer(box, "pointermove", 400, 100, 2);
    clock += 200;
    pointer(box, "pointerup", 400, 100, 2);
    pointer(box, "pointerup", 100, 100, 1);
    await nextTick();

    expect(rig.shell.quicklook.current.value, "the pinch was taken for a swipe").toBe("/a.jpg");
    expect(Number(box.getAttribute("data-scale")), "the pinch did not zoom").toBeGreaterThan(1);
    wrapper.unmount();
  });

  it("lets the next one-finger swipe change file once every finger is up", async () => {
    // The guard above must end with the gesture, not stick: a later swipe
    // on a fitted photo still moves through the folder.
    const { rig, wrapper, box } = await openOn("/a.jpg", ["/a.jpg", "/b.jpg"]);
    pointer(box, "pointerdown", 250, 100, 1);
    pointer(box, "pointerdown", 300, 100, 2);
    pointer(box, "pointerup", 300, 100, 2);
    pointer(box, "pointerup", 250, 100, 1);
    await nextTick();
    expect(box.getAttribute("data-scale"), "fingers that never moved zoomed").toBe("1.00");

    clock += 1_000;
    pointer(box, "pointerdown", 300, 100, 1);
    clock += 100;
    pointer(box, "pointerup", 100, 100, 1);
    await nextTick();
    expect(rig.shell.quicklook.current.value, "a swipe after a pinch was swallowed").toBe("/b.jpg");
    wrapper.unmount();
  });
});

describe("a double tap on a photograph", () => {
  it("zooms in and stays zoomed when the browser follows the taps with a dblclick", async () => {
    // Chromium on a touch screen sends, for one double tap: pointerdown,
    // pointerup, click, pointerdown, pointerup, click, dblclick. Both the
    // pointer double tap and the dblclick used to toggle, so the photo
    // zoomed in and straight back out within the same gesture.
    const { wrapper, box } = await openOn("/a.jpg");
    tap(box, 200, 150);
    clock += 120;
    tap(box, 202, 151);
    dblclick(box, 202, 151);
    await nextTick();
    expect(box.getAttribute("data-scale"), "the double tap did not leave the photo zoomed").toBe("2.50");

    // The next double tap goes back to fitted, through the same path.
    clock += 1_000;
    tap(box, 202, 151);
    clock += 120;
    tap(box, 203, 150);
    dblclick(box, 203, 150);
    await nextTick();
    expect(box.getAttribute("data-scale"), "a second double tap did not go back to fitted").toBe("1.00");
    wrapper.unmount();
  });

  it("zooms in and back out with pointer taps alone, where no dblclick follows", async () => {
    // Safari on iOS sends no dblclick for a touch double tap, so the
    // pointer path has to work on its own, including on a zoomed photo,
    // where a finger that lands starts a pan.
    const { wrapper, box } = await openOn("/a.jpg");
    tap(box, 200, 150);
    clock += 120;
    tap(box, 200, 150);
    await nextTick();
    expect(box.getAttribute("data-scale"), "a double tap did not zoom in").toBe("2.50");

    clock += 1_000;
    tap(box, 200, 150);
    clock += 120;
    tap(box, 200, 150);
    await nextTick();
    expect(box.getAttribute("data-scale"), "a double tap on a zoomed photo did not zoom out").toBe("1.00");
    wrapper.unmount();
  });

  it("zooms on a mouse double click, which is the same sequence with a mouse", async () => {
    const { wrapper, box } = await openOn("/a.jpg");
    tap(box, 200, 150, "mouse");
    clock += 120;
    tap(box, 200, 150, "mouse");
    dblclick(box, 200, 150);
    await nextTick();
    expect(box.getAttribute("data-scale"), "a mouse double click did not zoom").toBe("2.50");
    wrapper.unmount();
  });

  it("zooms on a dblclick that arrives without pointer events", async () => {
    // Assistive technology and synthetic input can send only the dblclick.
    const { wrapper, box } = await openOn("/a.jpg");
    clock += 5_000;
    dblclick(box, 200, 150);
    await nextTick();
    expect(box.getAttribute("data-scale")).toBe("2.50");
    wrapper.unmount();
  });

  it("does not count the two lifts that end a pinch as a double tap", async () => {
    // Two fingers that land and lift together, close to each other, are a
    // pinch that went nowhere, not a request to zoom to 2.5x.
    const { wrapper, box } = await openOn("/a.jpg");
    pointer(box, "pointerdown", 200, 150, 1);
    pointer(box, "pointerdown", 210, 150, 2);
    clock += 50;
    pointer(box, "pointerup", 210, 150, 2);
    clock += 20;
    pointer(box, "pointerup", 200, 150, 1);
    await nextTick();
    expect(box.getAttribute("data-scale")).toBe("1.00");
    wrapper.unmount();
  });
});
