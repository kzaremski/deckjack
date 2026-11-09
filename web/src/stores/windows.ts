import { computed, reactive, ref, type ComputedRef, type Ref } from "vue";

import {
  cascade,
  clampToWorkArea,
  rectForZone,
  tileRow,
  type Rect,
  type Size,
  type SnapZone,
} from "../shell/geometry.ts";
import type { WireWindow } from "../protocol.ts";
import { apps, isAppId, unknownApp, type AppContract, type AppId } from "./apps.ts";
import type { Bus } from "./bus.ts";

/**
 * The window manager's state.
 *
 * Windows are server state: each one is an app instance the server holds, so
 * every device sees the same open windows. This store is a mirror of that,
 * updated optimistically — a drag has to track the pointer at 120Hz, which it
 * cannot do by waiting for a round trip — and reconciled from the server's
 * `windows` event, which always wins.
 *
 * Layouts are kept per device class. The desktop layout (position, size,
 * z-order) and the compact layout (focus order, split pair) are separate
 * records over the same windows, so a day of using the phone never scrambles
 * the desktop arrangement.
 */

export interface ShellWindow {
  id: string;
  app: AppId;
  /** The app's own id as the server named it, which may not be a known app. */
  appId: string;
  title: string;
  rect: Rect;
  /** Back-to-front position; the store keeps these dense and ascending. */
  z: number;
  minimized: boolean;
  maximized: boolean;
  /** Where to go back to when un-maximized or un-snapped. */
  restore: Rect | null;
  snap: SnapZone | null;
  /** True until the panel has real content: the window shows a skeleton. */
  loading: boolean;
  minSize: Size;
}

export interface WindowsEnv {
  /** A fresh window id. Injected so ids are predictable outside a browser. */
  id(): string;
}

export function browserWindowsEnv(): WindowsEnv {
  return {
    id: () =>
      typeof crypto.randomUUID === "function"
        ? crypto.randomUUID()
        : `w${Date.now().toString(36)}${Math.random().toString(36).slice(2, 8)}`,
  };
}

export interface SplitPair {
  left: string;
  right: string;
  /** 0..1 of the available width (or height, when stacked). */
  fraction: number;
}

export interface WindowsStore {
  all: ComputedRef<ShellWindow[]>;
  /** Back to front, minimized ones excluded: what the desktop renders. */
  visible: ComputedRef<ShellWindow[]>;
  /** Most recently focused first: what the compact switcher orders by. */
  recent: ComputedRef<ShellWindow[]>;
  focused: ComputedRef<ShellWindow | null>;
  focusedId: Ref<string | null>;
  split: Ref<SplitPair | null>;
  workArea: Ref<Size>;
  titlebar: Ref<number>;
  lastError: Ref<string | null>;
  contract(window: ShellWindow): AppContract;

  open(app: AppId): Promise<string>;
  close(id: string): Promise<void>;
  focus(id: string): void;
  minimize(id: string): void;
  restore(id: string): void;
  toggleMaximize(id: string): void;
  /** During a drag or resize: local only, no command per frame. */
  place(id: string, rect: Rect): void;
  /** At the end of a drag or resize: the one command that records it. */
  commitPlacement(id: string): Promise<void>;
  snap(id: string, zone: SnapZone): Promise<void>;
  unsnap(id: string): void;
  tile(): Promise<void>;
  cycle(direction: 1 | -1): void;
  setSplit(left: string, right: string): void;
  clearSplit(): void;
  setSplitFraction(fraction: number): void;
  /** Reconciles from the server, which is the source of truth. */
  adopt(wire: WireWindow[]): void;
}

/** Gap between tiled and snapped windows, in CSS pixels. */
const SNAP_GAP = 6;

export function createWindows(bus: Bus, env: WindowsEnv): WindowsStore {
  const byId = reactive(new Map<string, ShellWindow>());
  const order = ref<string[]>([]);
  const focusHistory = ref<string[]>([]);
  const focusedId = ref<string | null>(null);
  const split = ref<SplitPair | null>(null);
  const workArea = ref<Size>({ w: 1280, h: 800 });
  const titlebar = ref(34);
  const lastError = ref<string | null>(null);

  function list(): ShellWindow[] {
    return order.value.flatMap((id) => {
      const w = byId.get(id);
      return w ? [w] : [];
    });
  }

  function touch(id: string) {
    focusHistory.value = [id, ...focusHistory.value.filter((x) => x !== id)];
  }

  function raise(id: string) {
    order.value = [...order.value.filter((x) => x !== id), id];
    order.value.forEach((wid, i) => {
      const w = byId.get(wid);
      if (w) w.z = i;
    });
  }

  /**
   * Sends the command that records a local change, and reports a failure
   * rather than silently diverging. The local state is left alone: the
   * server's next `windows` event is what corrects it, which keeps one
   * reconciliation path instead of two.
   */
  async function record(command: string, params: Record<string, unknown>) {
    const outcome = await bus.call(command, params);
    if (outcome.status === "ok") {
      lastError.value = null;
      return;
    }
    lastError.value =
      outcome.status === "queued"
        ? "waiting for approval in the Assistant's Inbox"
        : outcome.message;
  }

  /** Raising, remembering and focusing in one place: a window that is raised
   * without being remembered breaks the compact switcher's order. */
  function focusWindow(id: string) {
    const w = byId.get(id);
    if (!w) return;
    w.minimized = false;
    raise(id);
    touch(id);
    focusedId.value = id;
  }

  function contract(window: ShellWindow): AppContract {
    return isAppId(window.appId) ? apps[window.appId] : unknownApp(window.appId);
  }

  return {
    all: computed(() => list()),
    visible: computed(() => list().filter((w) => !w.minimized)),
    recent: computed(() =>
      focusHistory.value.flatMap((id) => {
        const w = byId.get(id);
        return w ? [w] : [];
      }),
    ),
    focused: computed(() => (focusedId.value ? byId.get(focusedId.value) ?? null : null)),
    focusedId,
    split,
    workArea,
    titlebar,
    lastError,
    contract,

    async open(app) {
      const spec = apps[app];
      const id = env.id();
      const rect = cascade(byId.size, workArea.value, spec.defaultSize);
      byId.set(id, {
        id,
        app,
        appId: app,
        title: spec.title,
        rect,
        z: order.value.length,
        minimized: false,
        maximized: false,
        restore: null,
        snap: null,
        // The panel is a skeleton until its backend answers. A spinner here
        // would say "busy"; the skeleton says what is arriving.
        loading: true,
        minSize: spec.minSize,
      });
      focusWindow(id);
      await record("shell.window.open", { app, id, ...rect });
      // Until an app backend exists there is nothing to wait for, so the
      // skeleton would never clear. The server's `windows` event is what will
      // clear it for real.
      const opened = byId.get(id);
      if (opened && bus.detached.value) opened.loading = false;
      return id;
    },

    async close(id) {
      const existed = byId.delete(id);
      order.value = order.value.filter((x) => x !== id);
      focusHistory.value = focusHistory.value.filter((x) => x !== id);
      if (split.value && (split.value.left === id || split.value.right === id)) {
        split.value = null;
      }
      if (focusedId.value === id) focusedId.value = focusHistory.value[0] ?? null;
      if (existed) await record("shell.window.close", { id });
    },

    // Focus is cheap, frequent and per device. Recording it on the server
    // would turn every click into a round trip for no benefit; the focus
    // order that matters is restored from the compact layout.
    focus: focusWindow,

    minimize(id) {
      const w = byId.get(id);
      if (!w) return;
      w.minimized = true;
      if (focusedId.value === id) {
        focusedId.value = focusHistory.value.find((x) => x !== id && !byId.get(x)?.minimized) ?? null;
      }
      void record("shell.window.minimize", { id });
    },

    restore(id) {
      if (!byId.has(id)) return;
      focusWindow(id);
      void record("shell.window.restore", { id });
    },

    toggleMaximize(id) {
      const w = byId.get(id);
      if (!w) return;
      if (w.maximized) {
        w.rect = w.restore ?? w.rect;
        w.maximized = false;
        w.restore = null;
      } else {
        w.restore = { ...w.rect };
        w.rect = rectForZone("maximize", workArea.value, 0);
        w.maximized = true;
        w.snap = null;
      }
      void record("shell.window.maximize", { id, maximized: w.maximized });
    },

    place(id, rect) {
      const w = byId.get(id);
      if (!w) return;
      w.rect = clampToWorkArea(rect, workArea.value, titlebar.value);
      w.maximized = false;
      w.snap = null;
    },

    async commitPlacement(id) {
      const w = byId.get(id);
      if (!w) return;
      await record("shell.window.place", { id, ...w.rect });
    },

    async snap(id, zone) {
      const w = byId.get(id);
      if (!w) return;
      if (!w.snap && !w.maximized) w.restore = { ...w.rect };
      w.rect = rectForZone(zone, workArea.value, zone === "maximize" ? 0 : SNAP_GAP);
      w.maximized = zone === "maximize";
      w.snap = zone === "maximize" ? null : zone;
      focusWindow(id);
      await record("shell.window.snap", { id, zone });
    },

    unsnap(id) {
      const w = byId.get(id);
      if (!w || !w.restore) return;
      w.rect = w.restore;
      w.restore = null;
      w.snap = null;
      w.maximized = false;
    },

    async tile() {
      const vis = list().filter((w) => !w.minimized);
      const rects = tileRow(vis.length, workArea.value, SNAP_GAP);
      vis.forEach((w, i) => {
        const rect = rects[i];
        if (!rect) return;
        w.restore = w.restore ?? { ...w.rect };
        w.rect = rect;
        w.maximized = false;
        w.snap = null;
      });
      await record("shell.windows.tile", { ids: vis.map((w) => w.id) });
    },

    cycle(direction) {
      const vis = list().filter((w) => !w.minimized);
      if (vis.length === 0) return;
      const index = vis.findIndex((w) => w.id === focusedId.value);
      const next = vis[(index + direction + vis.length) % vis.length];
      if (next) focusWindow(next.id);
    },

    setSplit(left, right) {
      if (left === right) return;
      split.value = { left, right, fraction: split.value?.fraction ?? 0.5 };
      void record("shell.split.set", { left, right });
    },

    clearSplit() {
      split.value = null;
      void record("shell.split.clear", {});
    },

    setSplitFraction(fraction) {
      if (!split.value) return;
      // Keeping both sides usable matters more than honoring the drag: a
      // divider dragged to the edge is a window you cannot get back.
      split.value.fraction = Math.min(0.85, Math.max(0.15, fraction));
    },

    /**
     * Replaces local state with the server's, keeping anything the server
     * does not own: which window this device has focused, and the skeleton
     * flag. Windows the server no longer lists are gone, including ones this
     * client opened — the server is the source of truth, not a cache.
     */
    adopt(wire) {
      const seen = new Set<string>();
      for (const w of wire) {
        seen.add(w.id);
        const existing = byId.get(w.id);
        const rect = { x: w.desktop.x, y: w.desktop.y, w: w.desktop.w, h: w.desktop.h };
        if (existing) {
          existing.title = w.title;
          existing.rect = rect;
          existing.z = w.desktop.z;
          existing.minimized = w.desktop.minimized;
          existing.maximized = w.desktop.maximized;
          existing.loading = false;
          continue;
        }
        byId.set(w.id, {
          id: w.id,
          app: isAppId(w.app) ? w.app : "files",
          appId: w.app,
          title: w.title,
          rect,
          z: w.desktop.z,
          minimized: w.desktop.minimized,
          maximized: w.desktop.maximized,
          restore: null,
          snap: null,
          loading: false,
          minSize: { w: w.minWidth, h: w.minHeight },
        });
      }
      for (const id of [...byId.keys()]) {
        if (!seen.has(id)) byId.delete(id);
      }
      order.value = [...seen].sort((a, b) => (byId.get(a)?.z ?? 0) - (byId.get(b)?.z ?? 0));
      focusHistory.value = focusHistory.value.filter((id) => seen.has(id));
      if (focusedId.value && !seen.has(focusedId.value)) {
        focusedId.value = focusHistory.value[0] ?? null;
      }
    },
  };
}
