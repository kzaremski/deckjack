import { describe, expect, it } from "vitest";
import { computed, ref } from "vue";

import type { ServerEvent, WireWindow } from "../protocol.ts";
import { appOpenEntries } from "../shell/shell-commands.ts";
import { createAppTable, parseNative } from "./apps.ts";
import type { Bus, BusStatus } from "./bus.ts";
import { followWindows, type SeatStore } from "./seat.ts";
import { createWindows, type WindowsEnv } from "./windows.ts";

/**
 * Streamed native apps' windows (docs/lanes/8-stream.md), as the shell
 * keeps them.
 *
 * They are the one kind of window the node makes on its own: the app maps
 * a toplevel or a menu, and the node opens a window for it. The device
 * holding the seat never adopts the node's echo of its own window changes,
 * so these come to it another way, and that way must not disturb anything
 * else it is doing.
 */

function stubBus(status: BusStatus = "open") {
  const s = ref<BusStatus>(status);
  const calls: Array<{ command: string; params: Record<string, unknown> }> = [];
  const listeners: Array<(e: ServerEvent) => void> = [];
  const bus = {
    status: s,
    detached: computed(() => s.value !== "open"),
    node: ref(null),
    lastError: ref(null),
    lastSeenAt: ref(null),
    rtt: ref(null),
    online: ref(true),
    retryAt: ref(null),
    reconnectNow() {},
    async call(command: string, params?: Record<string, unknown>) {
      calls.push({ command, params: params ?? {} });
      return { id: "c1", status: "ok" as const, value: null };
    },
    cancel() {},
    onConfirm() {},
    on: (f: (e: ServerEvent) => void) => {
      listeners.push(f);
      return () => {};
    },
    connect() {},
    close() {},
  } as unknown as Bus;
  return { bus, calls, emit: (e: ServerEvent) => listeners.forEach((f) => f(e)) };
}

function env(): WindowsEnv {
  let n = 0;
  return { id: () => `w${++n}`, storage: { read: () => null, write: () => {} } };
}

function wire(id: string, extra: Partial<WireWindow> = {}): WireWindow {
  return {
    id,
    app: "stream",
    title: "GIMP",
    minWidth: 0,
    minHeight: 0,
    desktop: { x: 100, y: 80, w: 800, h: 634, z: 0, minimized: false, maximized: false },
    stream: { app: "gimp", appTitle: "GIMP", kind: "toplevel", parent: null, width: 800, height: 600, x: 0, y: 0 },
    ...extra,
  };
}

const menu = (parent: string): WireWindow =>
  wire("stream-1-2", {
    title: "",
    desktop: { x: 0, y: 0, w: 200, h: 300, z: 1, minimized: false, maximized: false },
    stream: { app: "gimp", appTitle: "GIMP", kind: "popup", parent, width: 200, height: 300, x: 40, y: 25 },
  });

describe("the device holding the seat", () => {
  it("takes the node's streamed windows and only those", async () => {
    const { bus } = stubBus();
    const windows = createWindows(bus, env(), createAppTable());
    const mine = await windows.open("terminal");
    // The node's list predates this device's own terminal, which must not
    // vanish because the list does not have it.
    windows.syncStreamed([wire("stream-1-1")]);
    const ids = windows.all.value.map((w) => w.id);
    expect(ids).toContain(mine);
    expect(ids).toContain("stream-1-1");
    const streamed = windows.all.value.find((w) => w.id === "stream-1-1")!;
    expect(streamed.app, "docked under the native app").toBe("native:gimp");
    expect(streamed.stream?.width).toBe(800);
    expect(windows.focusedId.value, "a new app window comes forward").toBe("stream-1-1");

    windows.syncStreamed([]);
    expect(windows.all.value.map((w) => w.id)).toEqual([mine]);
  });

  it("places a menu against its parent's picture and keeps it out of tiling and the switcher", () => {
    const { bus } = stubBus();
    const windows = createWindows(bus, env(), createAppTable());
    windows.syncStreamed([wire("stream-1-1"), menu("stream-1-1")]);
    const popup = windows.all.value.find((w) => w.id === "stream-1-2")!;
    // Parent at (100, 80), title bar 34, offset (40, 25), picture at scale 1.
    expect(popup.rect).toEqual({ x: 140, y: 139, w: 200, h: 300 });
    expect(windows.focusedId.value, "a menu does not take focus").toBe("stream-1-1");
    expect(windows.recent.value.map((w) => w.id)).toEqual(["stream-1-1"]);
    // A device that adopts the whole list (not holding the seat) remembers
    // every window it has not focused, and still never switches to a menu.
    const waiting = createWindows(stubBus().bus, env(), createAppTable());
    waiting.adopt([wire("stream-1-1"), menu("stream-1-1")]);
    expect(waiting.recent.value.map((w) => w.id)).toEqual(["stream-1-1"]);
    windows.setLayout("tiling");
    expect(windows.all.value.find((w) => w.id === "stream-1-2")!.rect.w, "a menu was tiled").toBe(200);
  });

  it("never focuses a menu: focusing it focuses its window, and the menu stays above", () => {
    const { bus } = stubBus();
    const windows = createWindows(bus, env(), createAppTable());
    windows.syncStreamed([wire("stream-1-1"), menu("stream-1-1"), wire("stream-2-1", { title: "Other" })]);
    windows.focus("stream-2-1");
    // A tap inside the menu (compact: drawn by its parent's panel).
    windows.focus("stream-1-2");
    expect(windows.focusedId.value).toBe("stream-1-1");
    expect(windows.focused.value?.id, "the compact shell would show the menu alone").toBe("stream-1-1");
    const z = (id: string) => windows.all.value.find((w) => w.id === id)!.z;
    expect(z("stream-1-2")).toBeGreaterThan(z("stream-1-1"));
    expect(z("stream-1-1")).toBeGreaterThan(z("stream-2-1"));
  });

  it("never tiles or cycles to a menu, and hides it while its window is minimized", async () => {
    const { bus } = stubBus();
    const windows = createWindows(bus, env(), createAppTable());
    windows.syncStreamed([wire("stream-1-1"), menu("stream-1-1"), wire("stream-2-1", { title: "Other" })]);
    const rect = { ...windows.all.value.find((w) => w.id === "stream-1-2")!.rect };
    await windows.tile();
    expect(windows.all.value.find((w) => w.id === "stream-1-2")!.rect, "a menu was tiled").toEqual(rect);
    const seen = new Set<string | null>();
    for (let i = 0; i < 4; i++) {
      windows.cycle(1);
      seen.add(windows.focusedId.value);
    }
    expect([...seen].sort()).toEqual(["stream-1-1", "stream-2-1"]);

    expect(windows.visible.value.map((w) => w.id)).toContain("stream-1-2");
    await windows.minimize("stream-1-1");
    expect(windows.visible.value.map((w) => w.id), "a menu floats with its window minimized").not.toContain(
      "stream-1-2",
    );
  });

  it("does not take a streamed window away when the operator closes it: the app decides", async () => {
    const { bus, calls } = stubBus();
    const windows = createWindows(bus, env(), createAppTable());
    windows.syncStreamed([wire("stream-1-1")]);
    await windows.close("stream-1-1");
    expect(calls.at(-1)).toEqual({ command: "shell.window.close", params: { id: "stream-1-1" } });
    expect(windows.all.value.map((w) => w.id), "removed before the app unmapped it").toEqual(["stream-1-1"]);
    windows.syncStreamed([]);
    expect(windows.all.value).toHaveLength(0);
  });

  it("follows the node's streamed windows while it holds the seat, and everything otherwise", () => {
    const { bus, emit } = stubBus();
    const windows = createWindows(bus, env(), createAppTable());
    const held = ref(true);
    followWindows({ bus, windows, seat: { held } as unknown as SeatStore });
    emit({ kind: "windows", windows: [wire("stream-1-1")] });
    expect(windows.all.value.map((w) => w.id)).toEqual(["stream-1-1"]);
    emit({ kind: "windows", windows: [] });
    expect(windows.all.value, "the holder kept a window the app unmapped").toHaveLength(0);

    held.value = false;
    emit({ kind: "windows", windows: [wire("stream-1-1"), { ...wire("t1"), app: "terminal", stream: undefined }] });
    expect(windows.all.value.map((w) => w.id).sort()).toEqual(["stream-1-1", "t1"]);
    expect(windows.all.value.find((w) => w.id === "t1")!.stream).toBeNull();
  });
});

describe("native apps in the dock", () => {
  it("are listed from stream.apps, marked streamed, and opened by starting them", async () => {
    const apps = createAppTable();
    apps.native.value = parseNative({
      apps: [{ name: "gimp", title: "GIMP", tag: "Gi", x11: false }, { title: "no name" }],
    });
    const entry = apps.dock.value.find((a) => a.id === "native:gimp")!;
    expect(entry.streamed).toBe("gimp");
    expect(entry.tag).toBe("Gi");
    expect(apps.dock.value.filter((a) => a.streamed)).toHaveLength(1);

    const { bus } = stubBus();
    const windows = createWindows(bus, env(), apps);
    const launched: string[] = [];
    const open = appOpenEntries(apps.dock.value, windows, async (name) => void launched.push(name));
    await open.find((e) => e.id === "app.open.native:gimp")!.run();
    expect(launched).toEqual(["gimp"]);
    expect(windows.all.value, "an empty window was opened for a native app").toHaveLength(0);
  });
});
