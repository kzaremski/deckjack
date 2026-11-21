// @vitest-environment happy-dom
import { beforeAll, describe, expect, it } from "vitest";
import { flushPromises, type DOMWrapper, type VueWrapper } from "@vue/test-utils";

import WindowFrame from "./WindowFrame.vue";
import { harness, mountInShell, type Harness } from "../testing/shell-harness.ts";
import type { AppId } from "../stores/apps.ts";
import type { Rect, SnapZone } from "../shell/geometry.ts";
import type { ShellWindow } from "../stores/windows.ts";

/**
 * What one window's chrome owes the hand that is dragging it.
 *
 * Every promise here is checked against what the frame *renders*, not against
 * the store behind it. That is deliberate: a window manager can have a
 * perfectly correct store and still show the operator a window somewhere the
 * store never put it, and a test that only reads `windows.all` cannot tell the
 * difference. So the rectangle under assertion is parsed back out of the
 * element's own inline position, which is what the operator actually sees.
 *
 * Gestures are synthesized as pointer events on the element the component
 * listens on — the title bar and the grips, not `document` — because that is
 * where the component attaches them, and a drag tracked anywhere else would be
 * a drag this frame does not have.
 *
 * Rectangles are asserted as relations and bounds rather than as coordinates.
 * Pinning "x is 791" would make a better layout fail and would let a wrong
 * layout with the right numbers pass; what a window frame really promises is
 * that it stays reachable, that it moves with the pointer, that it resizes from
 * the edge that was grabbed, and that it refuses when the node cannot record
 * the change.
 */

const AREA = { w: 1600, h: 900 };

/**
 * How much of a window has to stay on screen for a hand to get hold of it:
 * about the width of a title you can put a pointer or a thumb on without
 * hunting for it. A human number, not `MIN_VISIBLE` — see the throw test.
 */
const GRABBABLE = 80;

beforeAll(() => {
  // happy-dom dispatches pointer events but does not implement the capture
  // API, and this component captures the pointer on purpose (its docblock
  // explains why: a drag tracked on `document` is lost over an iframe).
  // Without these, every gesture would throw before it began. This fakes a
  // missing browser edge, not any part of the component.
  Element.prototype.setPointerCapture = () => {};
  Element.prototype.releasePointerCapture = () => {};
});

/** Correlation ids already answered, per rig, so `settle` is idempotent. */
const answered = new WeakMap<Harness, Set<string>>();

/**
 * Answers every command the shell has sent and not yet heard back about, the
 * way a live node would. Window moves are recorded over the bus, so without
 * this the promises those commands return never resolve.
 */
async function settle(rig: Harness): Promise<void> {
  const seen = answered.get(rig) ?? new Set<string>();
  answered.set(rig, seen);
  for (let round = 0; round < 10; round++) {
    const fresh = rig
      .socket()
      .frames()
      .filter((f) => f.type === "invoke" && !seen.has(String(f.id)));
    if (fresh.length === 0) break;
    for (const frame of fresh) {
      seen.add(String(frame.id));
      rig.socket().deliver({
        type: "response",
        id: frame.id,
        outcome: { status: "ok", value: null },
        elapsed_ms: 1,
      });
    }
    await flushPromises();
  }
}

/** A desktop-sized shell with a live node. */
function liveShell(): Harness {
  const rig = harness({ size: AREA });
  rig.connect("pocket");
  rig.shell.windows.workArea.value = { ...AREA };
  return rig;
}

async function openWindow(rig: Harness, app: AppId): Promise<ShellWindow> {
  const pending = rig.shell.windows.open(app);
  await settle(rig);
  const id = await pending;
  const window = rig.shell.windows.all.value.find((w) => w.id === id);
  if (!window) throw new Error("the node accepted the open but no window exists to frame");
  return window;
}

function frameFor(rig: Harness, window: ShellWindow): VueWrapper {
  return mountInShell(WindowFrame, { harness: rig, props: { window } }).wrapper;
}

function px(value: string): number {
  const n = Number.parseFloat(value);
  if (Number.isNaN(n)) throw new Error(`the frame has no size: got ${JSON.stringify(value)}`);
  return n;
}

/** Where the frame has actually put itself, read back off the element. */
function rendered(wrapper: VueWrapper): Rect {
  const style = (wrapper.element as HTMLElement).style;
  const at = /translate3d\((-?[\d.]+)px,\s*(-?[\d.]+)px/.exec(style.transform);
  if (!at) throw new Error(`the frame is not positioned: transform was "${style.transform}"`);
  return { x: Number(at[1]), y: Number(at[2]), w: px(style.width), h: px(style.height) };
}

function depth(wrapper: VueWrapper): number {
  return Number.parseInt((wrapper.element as HTMLElement).style.zIndex || "0", 10);
}

/**
 * The frame's resize targets. Counted by the handle they name rather than by
 * their class or their cursor, because that is the part a rewrite has to keep
 * — a grip is a grip because it resizes an edge.
 */
function resizeAffordances(wrapper: VueWrapper): DOMWrapper<Element>[] {
  return wrapper.findAll("[data-handle]");
}

/** True while the frame is showing the operator that it is being dragged. */
function looksDragged(wrapper: VueWrapper): boolean {
  return (wrapper.element as HTMLElement).classList.contains("dragging");
}

function button(wrapper: VueWrapper, label: string): DOMWrapper<Element> {
  const found = wrapper.findAll("button").find((b) => b.attributes("aria-label") === label);
  if (!found) throw new Error(`the window has no ${label} control`);
  return found;
}

function snapPreviews(wrapper: VueWrapper): Array<SnapZone | null> {
  return (wrapper.emitted("snapPreview") ?? []).map((args) => args[0] as SnapZone | null);
}

/** Commands that reached the node, as the protocol put them on the wire. */
function recorded(rig: Harness): Array<{ command: string; params: Record<string, unknown> }> {
  return rig
    .socket()
    .frames()
    .filter((f) => f.type === "invoke")
    .map((f) => ({
      command: String(f.command),
      params: (f.params ?? {}) as Record<string, unknown>,
    }));
}

interface Point {
  x: number;
  y: number;
}

let nextPointer = 1;

/**
 * One gesture on one target: press, drag through every waypoint, release.
 *
 * `hold` runs after the last move and before the release, which is the only
 * moment a mid-drag promise — the title bar is still reachable, the snap
 * preview is showing — can be observed.
 */
async function gesture(
  target: DOMWrapper<Element>,
  from: Point,
  through: Point[],
  options: { rig: Harness; release?: boolean; hold?: () => void | Promise<void> },
): Promise<void> {
  const pointerId = nextPointer++;
  await target.trigger("pointerdown", { pointerId, button: 0, clientX: from.x, clientY: from.y });
  for (const at of through) {
    await target.trigger("pointermove", { pointerId, clientX: at.x, clientY: at.y });
    await flushPromises();
  }
  if (options.hold) await options.hold();
  const last = through.at(-1) ?? from;
  if (options.release !== false) {
    await target.trigger("pointerup", { pointerId, clientX: last.x, clientY: last.y });
    await flushPromises();
    await settle(options.rig);
  }
}

function titleBar(wrapper: VueWrapper): DOMWrapper<Element> {
  return wrapper.find("header");
}

function grip(wrapper: VueWrapper, handle: string): DOMWrapper<Element> {
  const found = wrapper.find(`[data-handle="${handle}"]`);
  if (!found.exists()) throw new Error(`the frame offers no ${handle} resize handle`);
  return found;
}

describe("dragging a window", () => {
  it("moves the window with the pointer", async () => {
    // The positive control for every "did not move" promise below: without
    // this one, those could all pass because dragging never worked at all.
    const rig = liveShell();
    const window = await openWindow(rig, "files");
    const wrapper = frameFor(rig, window);
    const before = rendered(wrapper);

    await gesture(titleBar(wrapper), { x: 500, y: 300 }, [{ x: 620, y: 380 }], { rig });

    const after = rendered(wrapper);
    expect(after.x - before.x, "the window did not follow the pointer sideways").toBeCloseTo(120, 0);
    expect(after.y - before.y, "the window did not follow the pointer downward").toBeCloseTo(80, 0);
    expect({ w: after.w, h: after.h }, "a move resized the window").toEqual({
      w: before.w,
      h: before.h,
    });
  });

  it("keeps the title bar reachable however far it is thrown", async () => {
    // geometry.ts's MIN_VISIBLE and clampToWorkArea exist for exactly this:
    // a window dragged until its title bar is off the screen is a window that
    // can never be grabbed again. The thresholds below are stated in human
    // terms — a graspable strip of frame, a title bar row on screen — rather
    // than against the constant, so lowering the constant until only a sliver
    // is left fails here instead of quietly becoming the new promise.
    const rig = liveShell();
    const window = await openWindow(rig, "files");
    const wrapper = frameFor(rig, window);
    const titlebar = rig.shell.windows.titlebar.value;

    const throws: Point[] = [
      { x: -6000, y: -6000 },
      { x: 6000, y: 6000 },
      { x: -6000, y: 6000 },
      { x: 6000, y: -6000 },
    ];

    for (const to of throws) {
      await gesture(titleBar(wrapper), { x: 400, y: 200 }, [to], {
        rig,
        hold: () => {
          const at = rendered(wrapper);
          const where = `thrown to (${to.x}, ${to.y})`;
          expect(
            at.x,
            `${where}: less than a grabbable strip of frame is left on the right edge`,
          ).toBeLessThanOrEqual(AREA.w - GRABBABLE);
          expect(
            at.x + at.w,
            `${where}: less than a grabbable strip of frame is left on the left edge`,
          ).toBeGreaterThanOrEqual(GRABBABLE);
          expect(at.y, `${where}: the title bar was dragged above the work area`).toBeGreaterThanOrEqual(
            0,
          );
          expect(
            at.y + titlebar,
            `${where}: the title bar was dragged under the bottom of the work area`,
          ).toBeLessThanOrEqual(AREA.h);
        },
      });
    }
  });

  it("records where the window ended up, so the other devices agree", async () => {
    // Windows are the node's state. A drag that only happened in this browser
    // is a drag the next `windows` event silently undoes.
    const rig = liveShell();
    const window = await openWindow(rig, "files");
    const wrapper = frameFor(rig, window);

    await gesture(titleBar(wrapper), { x: 500, y: 300 }, [{ x: 700, y: 420 }], { rig });

    const at = rendered(wrapper);
    const placed = recorded(rig).filter((c) => c.command === "shell.window.place").at(-1);
    expect(placed, "the finished drag was never recorded on the node").toBeTruthy();
    expect(
      { x: placed?.params.x, y: placed?.params.y },
      "the node was told a different position than the operator is looking at",
    ).toEqual({ x: at.x, y: at.y });
  });
});

describe("while tiling is on", () => {
  it("does not pull the window out of its slot, or pretend that it did", async () => {
    // Three promises in one test on purpose. The frame's own guard keeps the
    // rectangle pinned, so the rectangle alone says nothing about the store's
    // half of the refusal — hence the direct `place()` at the end, standing in
    // for every caller that is not this pointer drag: the palette, a keyboard
    // snap, a stray pointer event. What the frame is uniquely responsible for
    // is not entering its drag state, which would show the operator a grabbed
    // window sitting perfectly still — the shell lying for the length of the
    // gesture — and not offering the grab cursor in the first place.
    const rig = liveShell();
    const window = await openWindow(rig, "files");
    await openWindow(rig, "notes");
    const wrapper = frameFor(rig, window);
    rig.shell.windows.setLayout("tiling");
    await settle(rig);
    await flushPromises();
    const slot = rendered(wrapper);

    await gesture(titleBar(wrapper), { x: 100, y: 20 }, [{ x: 900, y: 600 }], {
      rig,
      hold: () =>
        void expect(
          looksDragged(wrapper),
          "the frame showed itself as being dragged while the layout holds it still",
        ).toBe(false),
    });

    expect(rendered(wrapper), "a tiled window was dragged out of its slot").toEqual(slot);
    // `tiled` is the hook the stylesheet hangs `cursor: default` on, and the
    // cursor is the only part of this the operator sees before they press. The
    // class stands in for it because scoped CSS is not computed here.
    expect(
      wrapper.classes(),
      "the frame still invites a drag the layout will refuse",
    ).toContain("tiled");

    rig.shell.windows.place(window.id, { ...slot, x: slot.x + 400 });
    await flushPromises();
    expect(
      rendered(wrapper),
      "a tiled window was moved out of its slot by a caller that is not the pointer",
    ).toEqual(slot);
  });

  it("holds the slot against the title bar's own gestures, not just the drag", async () => {
    // The drag is not the only way out of a slot: the title bar's double-click
    // and the keyboard snap reach the store directly, and a window that
    // maximizes or snaps out of a tiled layout leaves a hole behind it and
    // covers its neighbor. The refusal says so rather than happening silently,
    // because a control that does nothing and explains nothing reads as broken.
    const rig = liveShell();
    const window = await openWindow(rig, "files");
    await openWindow(rig, "notes");
    const wrapper = frameFor(rig, window);
    rig.shell.windows.setLayout("tiling");
    await settle(rig);
    await flushPromises();
    const slot = rendered(wrapper);

    await titleBar(wrapper).trigger("dblclick");
    await settle(rig);
    expect(rendered(wrapper), "a double-click maximized a tiled window out of its slot").toEqual(
      slot,
    );

    await titleBar(wrapper).trigger("keydown", { key: "ArrowRight", ctrlKey: true });
    await flushPromises();
    await settle(rig);
    expect(rendered(wrapper), "a keyboard snap moved a tiled window over its neighbor").toEqual(
      slot,
    );
    expect(
      rig.shell.windows.lastError.value,
      "the refusal never said that tiling is why nothing happened",
    ).toMatch(/tiling is on/);
  });

  it("offers no resize grips, rather than grips that do nothing", async () => {
    const rig = liveShell();
    const window = await openWindow(rig, "files");
    const wrapper = frameFor(rig, window);
    expect(
      resizeAffordances(wrapper).length,
      "a floating window offers nothing to resize it by",
    ).toBeGreaterThan(0);

    rig.shell.windows.setLayout("tiling");
    await settle(rig);
    await flushPromises();

    expect(
      resizeAffordances(wrapper).length,
      "a tiled window still shows resize cursors it will not honor",
    ).toBe(0);
  });
});

describe("with no node", () => {
  /** Opened while the node was there, then the node goes away. */
  async function stranded(): Promise<{ rig: Harness; window: ShellWindow; wrapper: VueWrapper }> {
    const rig = liveShell();
    const window = await openWindow(rig, "files");
    const wrapper = frameFor(rig, window);
    rig.socket().close();
    await flushPromises();
    if (!rig.shell.bus.detached.value) throw new Error("the shell still thinks it has a node");
    return { rig, window, wrapper };
  }

  it("does not move the window, or pretend the drag began", async () => {
    // A window that follows the pointer and then snaps back when the commit
    // fails has lied for the whole gesture, so the refusal happens up front.
    // Paired for the same reason as the tiling case: the drag state is the
    // part only the frame can get wrong, and the direct `place()` at the end
    // is the store's half of the same refusal, which the frame's guard would
    // otherwise hide.
    const { rig, window, wrapper } = await stranded();
    const before = rendered(wrapper);

    await gesture(titleBar(wrapper), { x: 400, y: 200 }, [{ x: 900, y: 600 }], {
      rig,
      release: false,
      hold: () =>
        void expect(
          looksDragged(wrapper),
          "the frame entered its drag state with no node to record the move",
        ).toBe(false),
    });

    expect(rendered(wrapper), "the window moved somewhere no node can be told about").toEqual(
      before,
    );
    expect(
      wrapper.classes(),
      "the frame still invites a drag no node can record",
    ).toContain("tiled");

    // The store's half of the same refusal: a move that came from anywhere but
    // this drag is applied locally and then silently lost at the next `windows`
    // event, which the operator sees as the window jumping back.
    rig.shell.windows.place(window.id, { ...before, x: before.x + 300 });
    await flushPromises();
    expect(rendered(wrapper), "the frame rendered a move no node can be told about").toEqual(
      before,
    );
  });

  it("says why the keyboard nudge did nothing, instead of reporting a transport error", async () => {
    // The drag path refuses up front, so the keyboard is the only way into the
    // commit with no node — and what the operator is owed there is a sentence
    // about this device, not the name of a socket that did not answer.
    const { rig, wrapper } = await stranded();
    const before = rendered(wrapper);

    await titleBar(wrapper).trigger("keydown", { key: "ArrowRight" });
    await flushPromises();

    expect(rendered(wrapper), "the window moved with no node to record it").toEqual(before);
    expect(
      recorded(rig).some((c) => c.command === "shell.window.place"),
      "a placement was sent with no node to record it",
    ).toBe(false);
    expect(
      rig.shell.windows.lastError.value,
      "the refusal did not say that nothing was moved",
    ).toMatch(/nothing was moved/);
  });

  it("does not close the window", async () => {
    const { rig, window, wrapper } = await stranded();

    await button(wrapper, "Close").trigger("click");
    await flushPromises();

    expect(
      rig.shell.windows.all.value.some((w) => w.id === window.id),
      "the window was closed behind the node's back",
    ).toBe(true);
  });

  it("offers no resize grips", async () => {
    const { wrapper } = await stranded();
    expect(
      resizeAffordances(wrapper).length,
      "the frame offers to resize a window whose size nothing can record",
    ).toBe(0);
  });
});

describe("closing a window", () => {
  it("closes it when there is a node to tell", async () => {
    // The positive control for the refusal above.
    const rig = liveShell();
    const window = await openWindow(rig, "files");
    const wrapper = frameFor(rig, window);

    await button(wrapper, "Close").trigger("click");
    await settle(rig);
    await flushPromises();

    expect(
      rig.shell.windows.all.value.some((w) => w.id === window.id),
      "the Close control did not close the window",
    ).toBe(false);
  });

  it("gives the window's place on the desktop back to the next one opened", async () => {
    // New windows cascade from however many are open, so a window that is
    // closed but not forgotten keeps its step in the cascade forever: each new
    // window opens further down and right than the last until they pile up
    // against the clamp edge. The symptom is a desktop that gets worse the
    // longer the session has been running, which is why this is asserted as
    // "the same place the first one had" rather than as a coordinate.
    const rig = liveShell();
    const first = await openWindow(rig, "files");
    const firstFrame = frameFor(rig, first);
    const firstRect = rendered(firstFrame);

    await button(firstFrame, "Close").trigger("click");
    await settle(rig);
    await flushPromises();

    const replacement = await openWindow(rig, "files");
    expect(
      rendered(frameFor(rig, replacement)),
      "a closed window is still taking up its place in the cascade",
    ).toEqual(firstRect);
  });

  it("gives its place in the stack back too, so the focus lift keeps showing", async () => {
    // Depth is read as "the same depth the first window had", not as a number:
    // what must not happen is depth creeping upward with every closed window,
    // because an unfocused window's depth climbing toward the focused band is
    // a desktop where the focused window stops looking lifted.
    const rig = liveShell();
    const first = await openWindow(rig, "files");
    const firstFrame = frameFor(rig, first);
    const alone = depth(firstFrame);

    await button(firstFrame, "Close").trigger("click");
    await settle(rig);
    await flushPromises();

    const replacement = await openWindow(rig, "notes");
    expect(
      depth(frameFor(rig, replacement)),
      "the stack is still counting a window that was closed",
    ).toBe(alone);
  });
});

describe("minimizing a window", () => {
  it("takes it off the desktop without closing it", async () => {
    const rig = liveShell();
    const window = await openWindow(rig, "files");
    const wrapper = frameFor(rig, window);

    await button(wrapper, "Minimize").trigger("click");
    await settle(rig);
    await flushPromises();

    expect(
      rig.shell.windows.visible.value.some((w) => w.id === window.id),
      "the Minimize control did not take the window off the desktop",
    ).toBe(false);
    expect(
      rig.shell.windows.all.value.some((w) => w.id === window.id),
      "Minimize closed the window instead of setting it aside",
    ).toBe(true);
    expect(
      recorded(rig).some((c) => c.command === "shell.window.minimize"),
      "the other devices were never told the window was minimized",
    ).toBe(true);
  });
});

describe("maximizing a window", () => {
  it("fills the work area when the title bar is double-clicked, and goes back", async () => {
    // The gesture every window manager has. It is the only one that does not
    // go through a button or a drag, so nothing else covers it.
    const rig = liveShell();
    const window = await openWindow(rig, "files");
    const wrapper = frameFor(rig, window);
    const before = rendered(wrapper);

    await titleBar(wrapper).trigger("dblclick");
    await settle(rig);

    const at = rendered(wrapper);
    expect(
      (at.w * at.h) / (AREA.w * AREA.h),
      "double-clicking the title bar did not maximize the window",
    ).toBeGreaterThan(0.95);
    expect(
      button(wrapper, "Restore").exists(),
      "the window filled the screen but its control still offers to maximize it",
    ).toBe(true);

    await titleBar(wrapper).trigger("dblclick");
    await settle(rig);

    expect(
      rendered(wrapper),
      "double-clicking again did not put the window back where it was",
    ).toEqual(before);
  });
});

describe("snapping", () => {
  it("previews the zone the pointer is actually in", async () => {
    const rig = liveShell();
    const window = await openWindow(rig, "files");
    const wrapper = frameFor(rig, window);

    await gesture(
      titleBar(wrapper),
      { x: 400, y: 300 },
      [
        { x: 6, y: AREA.h / 2 },
        { x: AREA.w / 2, y: 10 },
      ],
      { rig, release: false },
    );

    expect(
      snapPreviews(wrapper),
      "the left edge and the top edge did not preview as a left half and a maximize",
    ).toEqual(["left", "maximize"]);
  });

  it("withdraws the preview when the pointer leaves the edge", async () => {
    const rig = liveShell();
    const window = await openWindow(rig, "files");
    const wrapper = frameFor(rig, window);

    await gesture(
      titleBar(wrapper),
      { x: 400, y: 300 },
      [
        { x: 6, y: AREA.h / 2 },
        { x: AREA.w / 2, y: AREA.h / 2 },
      ],
      { rig, release: false },
    );

    expect(
      snapPreviews(wrapper).at(-1),
      "the snap overlay was left showing after the pointer moved back inland",
    ).toBeNull();
  });

  it("tells the desktop about the preview once per zone, not once per pointer frame", async () => {
    // The whole sequence rather than its last element: a pointer reports at
    // 120Hz, and the desktop re-renders the snap overlay for every event it is
    // handed, so an un-deduplicated stream is hundreds of overlay renders for
    // one drag and a constant trickle of `null` while the pointer is inland.
    const rig = liveShell();
    const window = await openWindow(rig, "files");
    const wrapper = frameFor(rig, window);

    await gesture(
      titleBar(wrapper),
      { x: 400, y: 300 },
      [
        { x: 6, y: 450 },
        { x: 8, y: 460 },
        { x: 800, y: 450 },
        { x: 810, y: 455 },
      ],
      { rig, release: false },
    );

    expect(
      snapPreviews(wrapper),
      "the snap preview was re-emitted without the zone changing",
    ).toEqual(["left", null]);
  });

  it("gives the window the left half when the drag is released there", async () => {
    const rig = liveShell();
    const window = await openWindow(rig, "files");
    const wrapper = frameFor(rig, window);

    await gesture(titleBar(wrapper), { x: 400, y: 300 }, [{ x: 6, y: AREA.h / 2 }], { rig });

    const at = rendered(wrapper);
    expect(at.x, "the snapped window did not start at the left edge").toBeLessThan(AREA.w * 0.05);
    expect(at.x + at.w, "the left half spilled over the middle of the screen").toBeLessThanOrEqual(
      AREA.w / 2 + 8,
    );
    expect(at.w, "the left half is not half the screen").toBeGreaterThan(AREA.w * 0.45);
    expect(at.h, "the left half does not reach top to bottom").toBeGreaterThan(AREA.h * 0.9);
    expect(snapPreviews(wrapper).at(-1), "the preview outlived the drop").toBeNull();
  });

  it("maximizes when the drag is released at the top edge", async () => {
    const rig = liveShell();
    const window = await openWindow(rig, "files");
    const wrapper = frameFor(rig, window);

    await gesture(titleBar(wrapper), { x: 400, y: 300 }, [{ x: AREA.w / 2, y: 8 }], { rig });

    const at = rendered(wrapper);
    expect(
      (at.w * at.h) / (AREA.w * AREA.h),
      "a drop at the top edge did not fill the work area",
    ).toBeGreaterThan(0.95);
    expect(
      button(wrapper, "Restore").exists(),
      "the window filled the screen but its control still offers to maximize it",
    ).toBe(true);
  });

  it("gives the window the lower half when the drag is released at the bottom edge", async () => {
    // The bottom edge is the one that is easiest to get backwards, and getting
    // it backwards means a window dropped at the bottom of the screen is flung
    // to the top — the opposite of where the hand let go.
    const rig = liveShell();
    const window = await openWindow(rig, "files");
    const wrapper = frameFor(rig, window);

    await gesture(titleBar(wrapper), { x: 400, y: 300 }, [{ x: AREA.w / 2, y: AREA.h - 6 }], {
      rig,
    });

    const at = rendered(wrapper);
    expect(at.y, "a drop at the bottom edge did not land in the lower half").toBeGreaterThanOrEqual(
      AREA.h / 2 - 8,
    );
    expect(at.y + at.h, "the lower half spilled past the work area").toBeLessThanOrEqual(AREA.h);
    expect(at.w, "the lower half is not the full width of the screen").toBeGreaterThan(
      AREA.w * 0.9,
    );
  });

  it("gives the window one quarter when the drag is released in a corner", async () => {
    const rig = liveShell();
    const window = await openWindow(rig, "files");
    const wrapper = frameFor(rig, window);

    await gesture(titleBar(wrapper), { x: 400, y: 300 }, [{ x: 6, y: 6 }], { rig });

    const at = rendered(wrapper);
    expect(
      { left: at.x < AREA.w * 0.05, top: at.y < AREA.h * 0.05 },
      "a drop in the top-left corner did not land in the top-left quarter",
    ).toEqual({ left: true, top: true });
    expect(at.w, "the top-left quarter took the full width").toBeLessThan(AREA.w * 0.6);
    expect(at.h, "the top-left quarter took the full height").toBeLessThan(AREA.h * 0.6);
  });

  it("stops claiming to be snapped once the window is dragged loose", async () => {
    // A window still showing its `left` badge after being dragged to an
    // arbitrary rectangle is chrome describing a window that no longer exists,
    // and the stale `restore` rect behind the badge is what the next Restore
    // would teleport it to.
    const rig = liveShell();
    const window = await openWindow(rig, "files");
    const wrapper = frameFor(rig, window);

    await gesture(titleBar(wrapper), { x: 400, y: 300 }, [{ x: 6, y: AREA.h / 2 }], { rig });
    expect(
      wrapper.find(".snapped").exists(),
      "the window was snapped but its title bar does not say so",
    ).toBe(true);
    const snapped = rendered(wrapper);

    await gesture(titleBar(wrapper), { x: 300, y: 300 }, [{ x: 700, y: 400 }], { rig });

    expect(
      rendered(wrapper).x,
      "the window did not come loose from the left half at all",
    ).toBeGreaterThan(snapped.x);
    expect(
      wrapper.find(".snapped").exists(),
      "the frame still claims to be snapped after being dragged loose",
    ).toBe(false);
  });
});

describe("focus", () => {
  it("makes the focused window look different from the rest", async () => {
    // Only the marker is asserted, not which styles it turns on: the frame's
    // job is to say which window has the keyboard, and the appearance of that
    // is the stylesheet's business.
    const rig = liveShell();
    const first = await openWindow(rig, "files");
    const second = await openWindow(rig, "notes");
    const front = frameFor(rig, second);
    const back = frameFor(rig, first);
    await flushPromises();

    expect(
      (front.element as HTMLElement).className,
      "the focused and unfocused windows render identically, so nothing says which has the keyboard",
    ).not.toBe((back.element as HTMLElement).className);
  });

  it("focuses an unfocused window when it is clicked, and brings it to the front", async () => {
    const rig = liveShell();
    const first = await openWindow(rig, "files");
    await openWindow(rig, "notes");
    const back = frameFor(rig, first);
    await flushPromises();
    const before = depth(back);

    await back.trigger("pointerdown", { pointerId: nextPointer++, button: 0, clientX: 500, clientY: 400 });
    await flushPromises();

    expect(rig.shell.windows.focusedId.value, "clicking a window did not focus it").toBe(first.id);
    expect(depth(back), "the clicked window was focused but not raised").toBeGreaterThan(before);
  });

  it("keeps the raise order of the windows that are not focused", async () => {
    // Three windows, because with two the only thing to compare is the focused
    // one against the other. The window raised a moment ago has to paint above
    // the one that has not been touched since it opened; a stack that collapses
    // to "focused and everything else" paints the rest in DOM order, so
    // clicking the visible sliver of the wrong window becomes the only way to
    // reach the one underneath.
    const rig = liveShell();
    const first = await openWindow(rig, "files");
    const second = await openWindow(rig, "notes");
    const third = await openWindow(rig, "terminal");
    const frames = [first, second, third].map((w) => frameFor(rig, w));
    await flushPromises();

    for (const frame of frames) {
      await frame.trigger("pointerdown", {
        pointerId: nextPointer++,
        button: 0,
        clientX: 500,
        clientY: 400,
      });
      await flushPromises();
    }

    expect(
      depth(frames[1]!),
      "the window raised more recently paints behind the one that was not",
    ).toBeGreaterThan(depth(frames[0]!));
  });

  it("names the window, so a screen reader can tell one from another", async () => {
    const rig = liveShell();
    const window = await openWindow(rig, "files");
    const wrapper = frameFor(rig, window);

    expect(
      wrapper.attributes("aria-label"),
      "the window has no accessible name, so it is announced as an unnamed region",
    ).toContain(window.title);
  });
});

/**
 * Moving a window without a pointer.
 *
 * A window that can only be moved by hand is a window the keyboard-first rule
 * does not cover, so every promise in here belongs to an operator with no
 * mouse. Nudges are asserted as directions and as relations between steps
 * rather than as pixel counts: what is owed is that the window goes the way the
 * arrow points, by an amount that is visible but is not a jump, that Shift
 * makes it finer, and that the modifier snaps to the side the arrow names.
 */
describe("the keyboard", () => {
  async function framed(): Promise<{ rig: Harness; window: ShellWindow; wrapper: VueWrapper }> {
    const rig = liveShell();
    const window = await openWindow(rig, "files");
    return { rig, window, wrapper: frameFor(rig, window) };
  }

  async function press(wrapper: VueWrapper, rig: Harness, init: Record<string, unknown>) {
    await titleBar(wrapper).trigger("keydown", init);
    await flushPromises();
    await settle(rig);
  }

  it("moves the window the way the arrow key points", async () => {
    const { rig, wrapper } = await framed();
    const start = rendered(wrapper);

    await press(wrapper, rig, { key: "ArrowRight" });
    const right = rendered(wrapper);
    const step = right.x - start.x;
    expect(step, "the right arrow did not move the window to the right").toBeGreaterThan(4);
    expect(step, "the right arrow threw the window instead of nudging it").toBeLessThanOrEqual(64);
    expect({ y: right.y, w: right.w, h: right.h }, "a sideways nudge moved or resized it").toEqual({
      y: start.y,
      w: start.w,
      h: start.h,
    });

    await press(wrapper, rig, { key: "ArrowLeft" });
    expect(rendered(wrapper).x, "the left arrow did not take the window back").toBe(start.x);

    await press(wrapper, rig, { key: "ArrowDown" });
    expect(rendered(wrapper).y, "the down arrow did not move the window down").toBeGreaterThan(
      start.y,
    );

    await press(wrapper, rig, { key: "ArrowUp" });
    expect(rendered(wrapper).y, "the up arrow did not take the window back up").toBe(start.y);
  });

  it("nudges by less with Shift held, for placing a window exactly", async () => {
    const { rig, wrapper } = await framed();
    const start = rendered(wrapper);

    await press(wrapper, rig, { key: "ArrowRight", shiftKey: true });
    const fine = rendered(wrapper).x - start.x;
    expect(fine, "Shift and the right arrow did not move the window at all").toBeGreaterThan(0);

    await press(wrapper, rig, { key: "ArrowRight" });
    const coarse = rendered(wrapper).x - start.x - fine;
    expect(fine, "Shift did not make the nudge finer than a plain arrow key").toBeLessThan(coarse);
  });

  it("records the keyboard nudge, the same as a finished drag", async () => {
    // A move the node never hears about is a move the next `windows` event
    // silently undoes, whichever hand made it.
    const { rig, wrapper } = await framed();

    await press(wrapper, rig, { key: "ArrowRight" });

    const at = rendered(wrapper);
    const placed = recorded(rig)
      .filter((c) => c.command === "shell.window.place")
      .at(-1);
    expect(placed, "the keyboard nudge was never recorded on the node").toBeTruthy();
    expect(
      { x: placed?.params.x, y: placed?.params.y },
      "the node was told a different position than the operator is looking at",
    ).toEqual({ x: at.x, y: at.y });
  });

  it("snaps to the side the arrow points when the modifier is held", async () => {
    const { rig, wrapper } = await framed();

    await press(wrapper, rig, { key: "ArrowLeft", ctrlKey: true });
    const left = rendered(wrapper);
    expect(left.x, "the modifier and the left arrow did not snap it to the left half").toBeLessThan(
      AREA.w * 0.05,
    );
    expect(left.x + left.w, "the left half spilled over the middle of the screen").toBeLessThanOrEqual(
      AREA.w / 2 + 8,
    );

    await press(wrapper, rig, { key: "ArrowRight", ctrlKey: true });
    const right = rendered(wrapper);
    expect(
      right.x,
      "the modifier and the right arrow did not snap it to the right half",
    ).toBeGreaterThanOrEqual(AREA.w / 2 - 8);
    expect(right.x + right.w, "the right half ran off the screen").toBeLessThanOrEqual(AREA.w);
  });

  it("fills the work area on Enter, the keyboard twin of the double-click", async () => {
    const { rig, wrapper } = await framed();

    await press(wrapper, rig, { key: "Enter" });

    const at = rendered(wrapper);
    expect(
      (at.w * at.h) / (AREA.w * AREA.h),
      "Enter on the title bar did not maximize the window",
    ).toBeGreaterThan(0.95);
  });

  it("brings a window snapped from the keyboard to the front", async () => {
    // A snap that does not raise can put the window behind the one that has
    // the keyboard: the operator snaps, sees nothing move, and keeps typing
    // into a window they thought they had just moved.
    const rig = liveShell();
    const first = await openWindow(rig, "files");
    const second = await openWindow(rig, "notes");
    const back = frameFor(rig, first);
    const front = frameFor(rig, second);
    await flushPromises();
    expect(rig.shell.windows.focusedId.value, "the newer window did not open focused").toBe(
      second.id,
    );

    await press(back, rig, { key: "ArrowLeft", ctrlKey: true });

    expect(
      rig.shell.windows.focusedId.value,
      "a snapped window was left behind its neighbor",
    ).toBe(first.id);
    expect(depth(back), "the snapped window was focused but not raised").toBeGreaterThan(
      depth(front),
    );
  });
});

describe("resizing", () => {
  it("resizes from the edge that was grabbed", async () => {
    const rig = liveShell();
    const window = await openWindow(rig, "files");
    const wrapper = frameFor(rig, window);
    const before = rendered(wrapper);

    await gesture(grip(wrapper, "e"), { x: 700, y: 300 }, [{ x: 820, y: 300 }], { rig });
    const wider = rendered(wrapper);
    expect(wider.w - before.w, "dragging the east edge right did not widen the window").toBeCloseTo(
      120,
      0,
    );
    expect(wider.x, "dragging the east edge moved the west edge").toBe(before.x);

    await gesture(grip(wrapper, "s"), { x: 400, y: 480 }, [{ x: 400, y: 560 }], { rig });
    const taller = rendered(wrapper);
    expect(taller.h - wider.h, "dragging the south edge down did not make it taller").toBeCloseTo(
      80,
      0,
    );
    expect(taller.y, "dragging the south edge moved the north edge").toBe(wider.y);
  });

  it("moves the west edge and leaves the east edge where it was", async () => {
    const rig = liveShell();
    const window = await openWindow(rig, "files");
    const wrapper = frameFor(rig, window);
    const before = rendered(wrapper);

    await gesture(grip(wrapper, "w"), { x: 40, y: 300 }, [{ x: 140, y: 300 }], { rig });

    const after = rendered(wrapper);
    expect(after.x - before.x, "the west edge did not follow the pointer").toBeCloseTo(100, 0);
    expect(after.x + after.w, "resizing from the west dragged the east edge along").toBeCloseTo(
      before.x + before.w,
      0,
    );
  });

  it("will not shrink a window below the size its app declared usable", async () => {
    // The app contract's minimum is a real constraint, not a hint: a terminal
    // narrower than its minimum is a different tool. Collapsing past it must
    // also not invert the window — the dragged edge stops rather than passing
    // through the opposite one.
    const rig = liveShell();
    const window = await openWindow(rig, "files");
    const min = rig.shell.apps.contract("files").minSize;
    const wrapper = frameFor(rig, window);
    const before = rendered(wrapper);

    await gesture(grip(wrapper, "se"), { x: 700, y: 470 }, [{ x: -6000, y: -6000 }], { rig });

    const after = rendered(wrapper);
    expect(after.w, "the window was dragged narrower than its app can render").toBeGreaterThanOrEqual(
      min.w,
    );
    expect(after.h, "the window was dragged shorter than its app can render").toBeGreaterThanOrEqual(
      min.h,
    );
    expect(after.x, "collapsing from the south-east corner moved the north-west one").toBe(before.x);
    expect(after.y, "collapsing from the south-east corner moved the north-west one").toBe(before.y);
  });

  it("pins a collapsed window at its minimum without moving the edge opposite", async () => {
    const rig = liveShell();
    const window = await openWindow(rig, "files");
    const min = rig.shell.apps.contract("files").minSize;
    const wrapper = frameFor(rig, window);
    const before = rendered(wrapper);

    await gesture(grip(wrapper, "w"), { x: 40, y: 300 }, [{ x: 6000, y: 300 }], { rig });

    const after = rendered(wrapper);
    expect(after.w, "the window collapsed past its minimum width").toBe(min.w);
    expect(
      after.x + after.w,
      "a west handle dragged through the window carried the east edge with it",
    ).toBeCloseTo(before.x + before.w, 0);
  });
});
