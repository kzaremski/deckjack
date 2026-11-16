// @vitest-environment happy-dom
import { beforeAll, describe, expect, it } from "vitest";
import { flushPromises, type DOMWrapper, type VueWrapper } from "@vue/test-utils";

import WindowFrame from "./WindowFrame.vue";
import { harness, mountInShell, type Harness } from "../testing/shell-harness.ts";
import type { AppId } from "../stores/apps.ts";
import { apps } from "../stores/apps.ts";
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
    // than against the constant, so lowering the constant to zero would not
    // quietly make this pass.
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
          expect(at.x, `${where}: the whole window is off the right edge`).toBeLessThanOrEqual(
            AREA.w - 32,
          );
          expect(at.x + at.w, `${where}: the whole window is off the left edge`).toBeGreaterThanOrEqual(
            32,
          );
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
    // Two promises in one test on purpose. The store refuses the move, so
    // asserting only the rectangle would pass even with the frame's own guard
    // deleted; what the frame is uniquely responsible for is not entering its
    // drag state, which would show the operator a grabbed window sitting
    // perfectly still — the shell lying for the length of the gesture.
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
    // Paired for the same reason as the tiling case: the store refuses the
    // move too, and the drag state is the part only the frame can get wrong.
    const { rig, wrapper } = await stranded();
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
    const min = apps.files.minSize;
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
    const min = apps.files.minSize;
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
