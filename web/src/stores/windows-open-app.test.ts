import { describe, expect, it } from "vitest";
import { computed, ref } from "vue";

import { createAppTable } from "./apps.ts";
import type { Bus } from "./bus.ts";
import { createWindows, windowStorageKey, type WindowsEnv } from "./windows.ts";

/**
 * Bringing an app forward on a file (`openApp`, what `shell.app.open` and an
 * app frame's `openApp` reach), and the per-window storage a closed window
 * leaves behind. The bus is a stub that records what the node was told.
 */

function stubBus() {
  const calls: Array<{ command: string; params: Record<string, unknown> }> = [];
  const status = ref("open");
  const bus = {
    status,
    detached: computed(() => status.value !== "open"),
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
    on: () => () => {},
    connect() {},
    close() {},
  } as unknown as Bus;
  return { bus, calls };
}

function env() {
  let n = 0;
  const kept = new Map<string, string>();
  const environment: WindowsEnv = {
    id: () => `w${++n}`,
    storage: {
      read: (k) => kept.get(k) ?? null,
      write: (k, v) => void kept.set(k, v),
      remove: (k) => void kept.delete(k),
    },
  };
  return { environment, kept };
}

const apart = (a: { rect: { x: number; y: number; w: number; h: number } }, b: typeof a) =>
  a.rect.x + a.rect.w <= b.rect.x ||
  b.rect.x + b.rect.w <= a.rect.x ||
  a.rect.y + a.rect.h <= b.rect.y ||
  b.rect.y + b.rect.h <= a.rect.y;

describe("opening an app that has a minimized window", () => {
  it("restores that window, and tells the node it is no longer minimized", async () => {
    const { bus, calls } = stubBus();
    const windows = createWindows(bus, env().environment, createAppTable());
    const id = await windows.open("files");
    windows.minimize(id);

    expect(await windows.openApp("files", { path: "/a.txt" })).toBe(id);
    expect(windows.all.value.find((w) => w.id === id)!.minimized).toBe(false);
    expect(windows.focused.value?.id).toBe(id);
    expect(calls.at(-1)).toEqual({ command: "shell.window.restore", params: { id } });
  });

  it("makes room for it again when windows are tiled", async () => {
    const { bus } = stubBus();
    const windows = createWindows(bus, env().environment, createAppTable());
    windows.workArea.value = { w: 1600, h: 900 };
    await windows.open("terminal");
    const files = await windows.open("files");
    windows.setLayout("tiling");
    windows.minimize(files);

    await windows.openApp("files");
    const [a, b] = windows.visible.value;
    expect(b, "the window did not come back").toBeTruthy();
    expect(apart(a!, b!), "restored over its neighbor").toBe(true);
  });
});

describe("closing a window", () => {
  it("removes what an app kept on this device for that window, and nothing else", async () => {
    const { bus } = stubBus();
    const { environment, kept } = env();
    const windows = createWindows(bus, environment, createAppTable());
    const one = await windows.open("files");
    const two = await windows.open("files");
    kept.set(windowStorageKey(one), "draft one");
    kept.set(windowStorageKey(two), "draft two");

    await windows.close(one);
    expect(kept.has(windowStorageKey(one))).toBe(false);
    expect(kept.get(windowStorageKey(two))).toBe("draft two");
  });
});
