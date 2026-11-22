import { computed, reactive, ref, type ComputedRef, type Ref } from "vue";

import {
  cascade,
  clampToWorkArea,
  rectForZone,
  tileMasterStack,
  tileRow,
  type Rect,
  type Size,
  type SnapZone,
} from "../shell/geometry.ts";
import { safeStorage } from "../tokens/runtime.ts";
import type { WireWindow } from "../protocol.ts";
import type { AppContract, AppId, AppTable } from "./apps.ts";
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
  /** The same id as `app`. Kept because panels and tests read both; before
   * the app table was built from manifests, `app` was coerced into a closed
   * list and this was the server's word. */
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
  /**
   * The file this window was last asked to open, for an app opened on a file
   * (`shell.app.open`). `seq` changes on every request, so asking twice for
   * the same file still reaches the app. Kept on this device only: it is a
   * request to the app, and the app keeps what it opened.
   */
  launch?: AppLaunch | null;
}

/** A request for an app's window to open a file. */
export interface AppLaunch {
  path: string;
  seq: number;
}

/**
 * How the desktop arranges windows.
 *
 * Floating is the default, and deliberately: this is a workspace people drag
 * windows around in, and a shell that tiles the moment a second window opens
 * takes that away without being asked. Tiling is a mode you turn on, the way
 * you choose a layout in i3 rather than the way a tiling WM imposes one.
 */
export type LayoutMode = "floating" | "tiling";

/** Where the chosen mode is remembered, per device, like the theme. */
const LAYOUT_KEY = "deckjack.layout";

export interface WindowsEnv {
  /** A fresh window id. Injected so ids are predictable outside a browser. */
  id(): string;
  /**
   * Per-device preference storage. Injected rather than read ambiently for the
   * reason `runtime.ts` gives: `localStorage` is not merely missing in a
   * private window, reading it throws.
   */
  storage: {
    read(key: string): string | null;
    write(key: string, value: string): void;
    /** Optional so existing environments stay valid; without it a closed
     * window's own storage is simply not cleared. */
    remove?(key: string): void;
  };
}

/**
 * The one storage key an app may keep per window on this device (the
 * Editor keeps unsaved text there). The store removes it when the window
 * closes, so what an app kept for a window does not outlive the window.
 */
export function windowStorageKey(windowId: string): string {
  return `deckjack.window.${windowId}`;
}

export function browserWindowsEnv(): WindowsEnv {
  return {
    id: () =>
      typeof crypto.randomUUID === "function"
        ? crypto.randomUUID()
        : `w${Date.now().toString(36)}${Math.random().toString(36).slice(2, 8)}`,
    storage: {
      ...safeStorage(),
      remove(key) {
        try {
          window.localStorage.removeItem(key);
        } catch {
          // Nothing could have been stored there either.
        }
      },
    },
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
  /** Floating or tiling. Read it to decide whether a drag is allowed. */
  layout: Ref<LayoutMode>;
  contract(window: ShellWindow): AppContract;

  open(app: AppId, options?: { path?: string }): Promise<string>;
  /**
   * Open `app` on a file: focus its window if one is open (restoring it if
   * minimized) and hand it the path, else open a new one with it. Returns
   * the window's id. The caller validates `app` and `path`; see
   * `parseAppOpen`.
   */
  openApp(app: AppId, options?: { path?: string }): Promise<string>;
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
  setLayout(mode: LayoutMode): void;
  /** Flips between floating and tiling, applying the new one at once. */
  toggleLayout(): void;
  /**
   * Re-applies tiling after the work area changed. A no-op while floating, so
   * the caller does not have to know the mode — a resize handler that had to
   * check first is a resize handler that will forget.
   */
  relayout(): void;
  cycle(direction: 1 | -1): void;
  setSplit(left: string, right: string): void;
  clearSplit(): void;
  setSplitFraction(fraction: number): void;
  /** Reconciles from the server, which is the source of truth. */
  adopt(wire: WireWindow[]): void;
}

/** Gap between tiled and snapped windows, in CSS pixels. */
const SNAP_GAP = 6;

/**
 * `apps` is required, and not a module the store imports, on purpose: the
 * app table is built from what the node has installed, so each shell owns
 * one, and a test harness that forgot to pass it would otherwise be testing a
 * different table from the product's without a compile error to say so.
 */
export function createWindows(bus: Bus, env: WindowsEnv, apps: AppTable): WindowsStore {
  const byId = reactive(new Map<string, ShellWindow>());
  const order = ref<string[]>([]);
  const focusHistory = ref<string[]>([]);
  const focusedId = ref<string | null>(null);
  const split = ref<SplitPair | null>(null);
  const workArea = ref<Size>({ w: 1280, h: 800 });
  const titlebar = ref(34);
  const lastError = ref<string | null>(null);
  // Anything that is not exactly "tiling" means floating, so a corrupted or
  // half-written value lands on the default rather than on a mode the
  // operator never chose.
  const layout = ref<LayoutMode>(env.storage.read(LAYOUT_KEY) === "tiling" ? "tiling" : "floating");
  let launches = 0;
  /**
   * Windows opened here whose `shell.window.open` the node has not answered
   * yet. A list from the node that predates the open — the snapshot handed
   * over with the seat can cross it on the wire — does not know them, and
   * must not take them away.
   */
  const opening = new Set<string>();

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

  /**
   * Refuses, and says so, when there is no node to record the change.
   *
   * Every window here is an app instance the *server* holds. Applying a change
   * locally while disconnected does not give the operator a working shell, it
   * gives them a shell that disagrees with the node and silently loses the
   * difference at the next `windows` event. So the mutation never happens.
   *
   * Focus and the layout mode are deliberately not guarded: both are this
   * device's own view of the same windows, neither is recorded, and a shell
   * you cannot even look around while it reconnects would be worse than one
   * that is honest about what it cannot change.
   */
  function noNode(verb: string): boolean {
    if (!bus.detached.value) return false;
    lastError.value = bus.online.value
      ? `not connected to the node, so nothing was ${verb}`
      : `this device is offline, so nothing was ${verb}`;
    return true;
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
    return apps.contract(window.appId);
  }

  /**
   * Lays the visible windows out as master-and-stack. A no-op while floating,
   * so every caller can invoke it unconditionally.
   *
   * `record` exists because a window's rect is *server* state: a re-tile that
   * is only local gets overwritten the next time the server sends `windows`,
   * and the windows visibly jump back to where they were floating. So the
   * arrangement is recorded whenever it was caused by something the operator
   * did — turning the mode on, opening, closing, minimizing — and not when it
   * was caused by the work area changing, where the rects are re-derived from
   * the new size anyway and recording would mean a command per resize frame.
   */
  function arrange(record_: boolean) {
    if (layout.value !== "tiling") return;
    const vis = list().filter((w) => !w.minimized);
    if (vis.length === 0) return;
    const rects = tileMasterStack(vis.length, workArea.value, SNAP_GAP);
    vis.forEach((w, i) => {
      const rect = rects[i];
      if (!rect) return;
      // The pre-tiling rect is kept once, so leaving tiling mode can put the
      // windows back rather than stranding them in their tiled slots.
      w.restore = w.restore ?? { ...w.rect };
      w.rect = rect;
      w.maximized = false;
      w.snap = null;
    });
    if (record_) void record("shell.windows.tile", { ids: vis.map((w) => w.id) });
  }

  /**
   * A standalone function rather than a method on the returned store, so
   * `toggleLayout` can call it without `this` — a store whose methods break
   * when it is destructured is a trap for every later caller.
   */
  function setLayoutMode(mode: LayoutMode) {
    if (layout.value === mode) return;
    layout.value = mode;
    env.storage.write(LAYOUT_KEY, mode);
    if (mode === "tiling") {
      arrange(true);
      return;
    }
    // Leaving tiling puts each window back where it floated, which is what
    // `arrange` saved on the way in. A window with nothing saved keeps its
    // tiled rect rather than being moved somewhere invented.
    for (const w of list()) {
      if (!w.restore) continue;
      w.rect = w.restore;
      w.restore = null;
      w.snap = null;
      w.maximized = false;
      void record("shell.window.place", { id: w.id, ...w.rect });
    }
  }

  const store: WindowsStore = {
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
    layout,
    contract,

    async open(app, options) {
      if (noNode("opened")) return "";
      const spec = apps.contract(app);
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
        // Set before the panel exists, so its frame loads with the path
        // rather than loading empty and being told afterwards.
        launch: options?.path ? { path: options.path, seq: ++launches } : null,
      });
      focusWindow(id);
      // The cascade position above is still computed and still recorded: it is
      // what the window goes back to when tiling is turned off, and it is what
      // the other device class uses, which does not share this mode.
      opening.add(id);
      try {
        await record("shell.window.open", { app, id, ...rect });
      } finally {
        opening.delete(id);
      }
      arrange(true);
      // Until an app backend exists there is nothing to wait for, so the
      // skeleton would never clear. The server's `windows` event is what will
      // clear it for real.
      const opened = byId.get(id);
      if (opened && bus.detached.value) opened.loading = false;
      return id;
    },

    async openApp(app, options) {
      if (noNode("opened")) return "";
      // The most recently focused window of that app, else any of them.
      const mine = (id: string) => byId.get(id)?.appId === app;
      const existing = focusHistory.value.find(mine) ?? order.value.find(mine);
      const w = existing ? byId.get(existing) : undefined;
      if (!w) return store.open(app, options);
      if (options?.path) w.launch = { path: options.path, seq: ++launches };
      if (w.minimized) store.restore(w.id);
      else focusWindow(w.id);
      return w.id;
    },

    async close(id) {
      if (noNode("closed")) return;
      const existed = byId.delete(id);
      order.value = order.value.filter((x) => x !== id);
      focusHistory.value = focusHistory.value.filter((x) => x !== id);
      if (split.value && (split.value.left === id || split.value.right === id)) {
        split.value = null;
      }
      if (focusedId.value === id) focusedId.value = focusHistory.value[0] ?? null;
      if (existed) env.storage.remove?.(windowStorageKey(id));
      if (existed) await record("shell.window.close", { id });
      // The survivors grow into the freed slot, which is the whole point of a
      // tiling mode: a gap where a window used to be is a floating layout.
      arrange(true);
    },

    // Focus is cheap, frequent and per device. Recording it on the server
    // would turn every click into a round trip for no benefit; the focus
    // order that matters is restored from the compact layout.
    focus: focusWindow,

    minimize(id) {
      if (noNode("minimized")) return;
      const w = byId.get(id);
      if (!w) return;
      w.minimized = true;
      if (focusedId.value === id) {
        focusedId.value = focusHistory.value.find((x) => x !== id && !byId.get(x)?.minimized) ?? null;
      }
      void record("shell.window.minimize", { id });
      arrange(true);
    },

    restore(id) {
      if (noNode("restored")) return;
      if (!byId.has(id)) return;
      focusWindow(id);
      void record("shell.window.restore", { id });
      arrange(true);
    },

    toggleMaximize(id) {
      if (noNode("resized")) return;
      // Tiling owns the geometry, so maximizing out of a slot leaves a hole in
      // the layout and a window over its neighbour. `place()` has refused this
      // all along and its comment calls itself "the guard that holds when
      // something else calls in" -- this was one of the ways in: the title
      // bar's double-click and its Maximize button are both live while tiled.
      if (layout.value === "tiling") {
        lastError.value = "tiling is on, so nothing was resized";
        return;
      }
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
      // Silent, unlike the others: this runs once per pointer frame, so a
      // message here would be written hundreds of times for one gesture.
      // `WindowFrame` refuses to start the drag at all while disconnected.
      if (bus.detached.value) return;
      const w = byId.get(id);
      if (!w) return;
      // Tiled windows do not move. Dragging one would leave a hole in the
      // layout and a window overlapping its neighbour, which is neither
      // tiling nor floating. `WindowFrame` also hides the drag affordance in
      // this mode; this is the guard that holds when something else calls in.
      if (layout.value === "tiling") return;
      w.rect = clampToWorkArea(rect, workArea.value, titlebar.value);
      w.maximized = false;
      w.snap = null;
    },

    async commitPlacement(id) {
      if (noNode("moved")) return;
      const w = byId.get(id);
      if (!w) return;
      await record("shell.window.place", { id, ...w.rect });
    },

    async snap(id, zone) {
      if (noNode("snapped")) return;
      // The other way in: the palette's tiling commands and Ctrl+arrow reach
      // this directly, so a keyboard snap used to move a tiled window to the
      // right half and overlap its neighbour.
      if (layout.value === "tiling") {
        lastError.value = "tiling is on, so nothing was snapped";
        return;
      }
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
      if (noNode("tiled")) return;
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

    setLayout: setLayoutMode,

    toggleLayout: () => setLayoutMode(layout.value === "tiling" ? "floating" : "tiling"),

    relayout: () => arrange(false),

    cycle(direction) {
      const vis = list().filter((w) => !w.minimized);
      if (vis.length === 0) return;
      const index = vis.findIndex((w) => w.id === focusedId.value);
      const next = vis[(index + direction + vis.length) % vis.length];
      if (next) focusWindow(next.id);
    },

    setSplit(left, right) {
      if (noNode("split")) return;
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
        // The app's own minimum where the shell knows the app: the node
        // keeps a width only, and a window another device opened should
        // resize here the way one opened here does.
        const known = apps.contract(w.app).minSize;
        byId.set(w.id, {
          id: w.id,
          app: w.app,
          appId: w.app,
          title: w.title,
          rect,
          z: w.desktop.z,
          minimized: w.desktop.minimized,
          maximized: w.desktop.maximized,
          restore: null,
          snap: null,
          loading: false,
          minSize: { w: Math.max(known.w, w.minWidth), h: Math.max(known.h, w.minHeight) },
        });
      }
      // Kept: a window still being opened here, which the node's list may
      // simply predate. It stays on top, where it was opened.
      const pending = order.value.filter((id) => opening.has(id) && !seen.has(id));
      for (const id of [...byId.keys()]) {
        if (!seen.has(id) && !opening.has(id)) byId.delete(id);
      }
      order.value = [
        ...[...seen].sort((a, b) => (byId.get(a)?.z ?? 0) - (byId.get(b)?.z ?? 0)),
        ...pending,
      ];
      focusHistory.value = focusHistory.value.filter((id) => seen.has(id));
      // A split whose half the node no longer lists is gone, as `close`
      // ends it: the compact view draws a split only when both halves
      // resolve, and would otherwise show nothing at all.
      const pair = split.value;
      if (pair && !(byId.has(pair.left) && byId.has(pair.right))) split.value = null;
      if (focusedId.value && !seen.has(focusedId.value)) {
        focusedId.value = focusHistory.value[0] ?? null;
      }
      // Windows this device has never focused (another device opened them,
      // and this one has just taken the seat) are remembered top first, so
      // the compact view, which shows the most recent, has one to show.
      const unknown = [...order.value].reverse().filter((id) => !focusHistory.value.includes(id));
      focusHistory.value = [...focusHistory.value, ...unknown];
      if (!focusedId.value) {
        focusedId.value = focusHistory.value.find((id) => !byId.get(id)?.minimized) ?? null;
      }
    },
  };
  return store;
}
