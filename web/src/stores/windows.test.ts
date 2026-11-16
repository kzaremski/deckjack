import { describe, expect, it } from "vitest";
import { computed, ref } from "vue";

import { createWindows, type WindowsEnv } from "./windows.ts";
import type { Bus, BusStatus } from "./bus.ts";

/**
 * What the window manager does on behalf of the operator.
 *
 * Windows are the node's state, not this device's, so almost every promise
 * here is about the relationship between the two: a change that the node did
 * not accept must not appear to have happened, and a layout the operator chose
 * must survive being turned off again.
 *
 * The bus is a stub rather than the real one. These tests are about the window
 * manager's behavior, and driving a whole socket handshake to assert that a
 * window did not open would make every one of them fail for reasons that have
 * nothing to do with windows.
 */

function stubBus(status: BusStatus = "open", online = true) {
  const s = ref<BusStatus>(status);
  const calls: Array<{ command: string; params: Record<string, unknown> }> = [];
  const bus = {
    status: s,
    detached: computed(() => s.value !== "open"),
    node: ref(null),
    lastError: ref(null),
    lastSeenAt: ref(null),
    rtt: ref(null),
    online: ref(online),
    retryAt: ref(null),
    reconnectNow() {},
    async call(command: string, params?: Record<string, unknown>) {
      calls.push({ command, params: params ?? {} });
      return { id: "c1", status: "ok" as const, value: null };
    },
    cancel() {},
    onConfirm() {},
    on: () => () => {},
    connect() {},
    close() {},
  } as unknown as Bus;
  return { bus, calls, status: s };
}

function env(): WindowsEnv {
  let n = 0;
  const store = new Map<string, string>();
  return {
    id: () => `w${++n}`,
    storage: {
      read: (k) => store.get(k) ?? null,
      write: (k, v) => void store.set(k, v),
    },
  };
}

describe("with no node to record them", () => {
  it("does not open a window", async () => {
    const { bus } = stubBus("connecting");
    const windows = createWindows(bus, env());

    await windows.open("terminal");

    expect(windows.all.value, "a window appeared that the node knows nothing about").toHaveLength(
      0,
    );
  });

  it("says why, rather than failing silently", async () => {
    const { bus } = stubBus("connecting");
    const windows = createWindows(bus, env());

    await windows.open("terminal");

    expect(windows.lastError.value).toBeTruthy();
    expect(windows.lastError.value).toMatch(/nothing was/i);
  });

  it("names the device's own network when that is the problem", async () => {
    const { bus } = stubBus("connecting", false);
    const windows = createWindows(bus, env());

    await windows.open("terminal");

    expect(windows.lastError.value).toMatch(/offline/i);
  });

  it("does not blame the network when the network is fine", async () => {
    // The pair matters more than either half. Telling someone their device is
    // offline when it is not sends them to restart a router while the machine
    // their work lives on is the thing that is down.
    //
    // Added after a mutation survived: forcing the offline branch for every
    // failure went unnoticed, because only the offline wording was asserted.
    const { bus } = stubBus("connecting", true);
    const windows = createWindows(bus, env());

    await windows.open("terminal");

    expect(windows.lastError.value).toBeTruthy();
    expect(windows.lastError.value).not.toMatch(/offline/i);
  });

  it("does not close a window either", async () => {
    // Opened while connected, then the node goes away.
    const { bus, status } = stubBus("open");
    const windows = createWindows(bus, env());
    const id = await windows.open("terminal");
    expect(windows.all.value).toHaveLength(1);

    status.value = "connecting";
    await windows.close(id);

    expect(windows.all.value, "a window was closed behind the node's back").toHaveLength(1);
  });

  it("does not move a window", async () => {
    const { bus, status } = stubBus("open");
    const windows = createWindows(bus, env());
    const id = await windows.open("terminal");
    const before = { ...windows.all.value[0]!.rect };

    status.value = "connecting";
    windows.place(id, { x: before.x + 300, y: before.y + 200, w: before.w, h: before.h });

    expect(windows.all.value[0]!.rect).toEqual(before);
  });
});

describe("with a node", () => {
  it("opens a window and tells the node", async () => {
    const { bus, calls } = stubBus("open");
    const windows = createWindows(bus, env());

    await windows.open("terminal");

    expect(windows.all.value).toHaveLength(1);
    expect(calls.some((c) => c.command === "shell.window.open")).toBe(true);
  });
});

describe("the layout the operator chose", () => {
  it("floats by default, because that is what was not asked for", () => {
    const { bus } = stubBus("open");
    expect(createWindows(bus, env()).layout.value).toBe("floating");
  });

  it("is remembered on this device", async () => {
    const shared = env();
    const { bus } = stubBus("open");
    createWindows(bus, shared).setLayout("tiling");

    expect(createWindows(stubBus("open").bus, shared).layout.value).toBe("tiling");
  });

  it("falls back to floating rather than trusting a damaged setting", () => {
    const { bus } = stubBus("open");
    const broken = env();
    broken.storage.write("deckjack.layout", "¯\\_(ツ)_/¯");

    expect(createWindows(bus, broken).layout.value).toBe("floating");
  });

  it("arranges the open windows the moment tiling is turned on", async () => {
    const { bus } = stubBus("open");
    const windows = createWindows(bus, env());
    windows.workArea.value = { w: 1600, h: 900 };
    await windows.open("terminal");
    await windows.open("files");

    windows.setLayout("tiling");

    const [a, b] = windows.visible.value;
    const apart =
      a!.rect.x + a!.rect.w <= b!.rect.x ||
      b!.rect.x + b!.rect.w <= a!.rect.x ||
      a!.rect.y + a!.rect.h <= b!.rect.y ||
      b!.rect.y + b!.rect.h <= a!.rect.y;
    expect(apart, "tiling was turned on and the windows still overlap").toBe(true);
  });

  it("puts each window back where it floated when tiling is turned off", async () => {
    const { bus } = stubBus("open");
    const windows = createWindows(bus, env());
    windows.workArea.value = { w: 1600, h: 900 };
    await windows.open("terminal");
    await windows.open("files");
    const floated = windows.all.value.map((w) => ({ id: w.id, rect: { ...w.rect } }));

    windows.setLayout("tiling");
    windows.setLayout("floating");

    for (const before of floated) {
      const now = windows.all.value.find((w) => w.id === before.id)!;
      expect(now.rect, `${before.id} did not go back to where it floated`).toEqual(before.rect);
    }
  });

  it("does not let a tiled window be dragged out of its slot", async () => {
    const { bus } = stubBus("open");
    const windows = createWindows(bus, env());
    windows.workArea.value = { w: 1600, h: 900 };
    const id = await windows.open("terminal");
    await windows.open("files");
    windows.setLayout("tiling");
    const slot = { ...windows.all.value.find((w) => w.id === id)!.rect };

    windows.place(id, { x: 10, y: 10, w: 400, h: 300 });

    expect(windows.all.value.find((w) => w.id === id)!.rect).toEqual(slot);
  });

  it("re-tiles when a window closes, instead of leaving a hole", async () => {
    const { bus } = stubBus("open");
    const windows = createWindows(bus, env());
    windows.workArea.value = { w: 1600, h: 900 };
    await windows.open("terminal");
    const second = await windows.open("files");
    windows.setLayout("tiling");

    await windows.close(second);

    const [only] = windows.visible.value;
    expect(only).toBeTruthy();
    const covered = (only!.rect.w * only!.rect.h) / (1600 * 900);
    expect(covered, "the survivor did not grow into the freed space").toBeGreaterThan(0.85);
  });
});

describe("a tiled window the operator did not drag", () => {
  // `place()` refuses to move a tiled window, and its comment calls itself
  // "the guard that holds when something else calls in". These are the other
  // ways in, found by mutation testing: both bypassed it entirely.
  async function twoTiled() {
    const { bus } = stubBus("open");
    const windows = createWindows(bus, env());
    windows.workArea.value = { w: 1600, h: 900 };
    const first = await windows.open("terminal");
    const second = await windows.open("files");
    windows.setLayout("tiling");
    return { windows, first, second };
  }

  function overlapping(windows: ReturnType<typeof createWindows>): boolean {
    const [a, b] = windows.visible.value;
    if (!a || !b) return false;
    const r = a.rect;
    const s = b.rect;
    return r.x < s.x + s.w && s.x < r.x + r.w && r.y < s.y + s.h && s.y < r.y + r.h;
  }

  it("is not maximized out of its slot by a double-click", async () => {
    const { windows, first } = await twoTiled();

    windows.toggleMaximize(first);

    expect(
      overlapping(windows),
      "a tiled window was maximized over its neighbour, leaving a hole in the layout",
    ).toBe(false);
  });

  it("is not snapped out of its slot by the keyboard", async () => {
    const { windows, first } = await twoTiled();

    await windows.snap(first, "right");

    expect(
      overlapping(windows),
      "a tiled window was snapped over its neighbour, leaving a hole in the layout",
    ).toBe(false);
  });
});
