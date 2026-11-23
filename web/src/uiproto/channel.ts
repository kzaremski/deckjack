import { reactive, ref, shallowRef, watch, type Ref, type ShallowRef } from "vue";

import type { Bus } from "../stores/bus.ts";
import { createSelection, type Selection } from "./selection.ts";
import {
  ApplyError,
  applyPatches,
  find,
  isRestart,
  pathKey,
  walk,
  type Action,
  type Path,
  type Patch,
  type UiBody,
  type UiNode,
} from "./tree.ts";

/**
 * The UI protocol's client half: one state per window an app describes, and
 * the per-window channel on the shell's own socket.
 *
 * **Local first** is the rule every function here keeps. A field's text, a
 * checkbox's tick and a list's selection change in this store the moment the
 * operator acts, and the app is told afterwards: a text field's `changed` is
 * debounced (and always sent before a blur's or a submit's consequences), a
 * tick and a selection are sent at once. Nothing the operator sees waits for
 * the node, and the only things that do wait are the app's *consequences*
 * (the filtered list, the struck-through task), which arrive as patches.
 *
 * A widget's bound command goes through the ordinary bus as the app's window
 * (`asApp`), so the node holds it to the app's scope, the hard-deny list and
 * the seat, and a destructive one is confirmed by the shell's own dialog.
 */

export type WindowStatus = "starting" | "live" | "lost";

/**
 * A checkbox's local value, standing in for the tree's until the app has
 * answered *this* tick, not an earlier one.
 *
 * An unbound box tells the app with an event, and the override lasts until
 * a frame acknowledges that event (`seq`): a frame answering an earlier tick
 * cannot flip the box back. A bound box runs its command, one at a time (a
 * tick while one runs is sent when it finishes, so the app sees them in
 * order); the override lasts while one is running, and after that until the
 * tree agrees with it or changes from what it said when the last one
 * finished (`base`): then the app, or another device, has spoken.
 */
interface Override {
  path: Path;
  value: unknown;
  /** The event that carried `value` to the app, for an unbound box. */
  seq: number | null;
  /** A bound box's command is running. */
  running: boolean;
  /** The tree's value when the last command finished. */
  base: unknown;
}

export interface UiWindow {
  id: string;
  app: string;
  /** The tree as the app last described it, or null before its first frame. */
  tree: ShallowRef<UiNode | null>;
  status: Ref<WindowStatus>;
  /** The last thing worth telling the operator: a refused or failed command. */
  notice: Ref<string | null>;
  /** Named fields' current local values: text, a select's choice, a box. */
  fields: Map<string, unknown>;
  /** Bound widgets' local values, by path. */
  overrides: Map<string, Override>;
  /** List selections, by the list's path. */
  selections: Map<string, Selection>;
  /** Bumped when a selection changes, for the components that draw it. */
  selectionVersion: Ref<number>;
  rev: number;
  /** The highest event number the app has acknowledged for this window. */
  acked: number;
  /** Sent `open` on the current connection. */
  opened: boolean;
  /** Panels showing this window right now. */
  mounted: number;
  /** The field revisions last applied, by field name. */
  revisions: Map<string, number>;
  /** Pending debounced `changed` events, by field path. */
  pending: Map<string, { path: Path; timer: ReturnType<typeof setTimeout> }>;
  openedAt: number;
  firstPaint: number | null;
}

/** Measurements for docs/lanes/9-tier1-phase0.md, kept only when asked for. */
export interface UiStats {
  /** Event to ack, ms. */
  rtt: number[];
  /** Each frame from the node: its size in bytes and its patch count. */
  frames: { window: string; bytes: number; patches: number; restart: boolean }[];
  /** `show` to the first tree, ms. */
  firstPaint: number[];
}

export interface UiChannelEnv {
  now(): number;
  setTimeout(fn: () => void, ms: number): ReturnType<typeof setTimeout>;
  clearTimeout(handle: ReturnType<typeof setTimeout>): void;
  /** Where measurements go, when someone is measuring. */
  stats?: UiStats;
}

export function browserUiChannelEnv(): UiChannelEnv {
  let stats: UiStats | undefined;
  try {
    if (localStorage.getItem("deckjack.uiStats") === "1") {
      stats = { rtt: [], frames: [], firstPaint: [] };
      (globalThis as { __djUiStats?: UiStats }).__djUiStats = stats;
    }
  } catch {
    // No storage, no measuring.
  }
  return {
    now: () => performance.now(),
    setTimeout: (fn, ms) => setTimeout(fn, ms),
    clearTimeout: (h) => clearTimeout(h),
    stats,
  };
}

/** How long typing pauses before the app hears what a field says. */
export const CHANGED_DEBOUNCE_MS = 250;

export interface UiChannel {
  /** A panel is showing `window`: open it if this connection has not. */
  show(window: string, app: string): UiWindow;
  /** The panel went away. The window stays open; see `closeMissing`. */
  hide(window: string): void;
  get(window: string): UiWindow | undefined;
  /** Send one semantic event now. Its number, or null when it could not be
   * sent (no connection). */
  event(win: UiWindow, path: Path, kind: string, value?: unknown): number | null;
  /** A named field changed locally. The app hears after a pause. */
  changed(win: UiWindow, path: Path, name: string, value: unknown): void;
  /** Send every pending `changed` for this window now. */
  flush(win: UiWindow): void;
  /** Whether an action's required fields are filled, locally. */
  ready(win: UiWindow, action: Action): boolean;
  /** Dispatch a widget's bound command as the app's window. True when it ran. */
  run(win: UiWindow, action: Action, own?: unknown): Promise<boolean>;
  /** A bound widget's displayed value: the local one while it stands. */
  valueOf(win: UiWindow, path: Path, treeValue: unknown): unknown;
  /** The operator ticked or unticked a checkbox: shown at once, then told
   * to the app (an event) or done (its bound command). */
  tick(win: UiWindow, path: Path, value: boolean): Promise<void>;
  /** The selection of one list, made on first use. */
  selection(win: UiWindow, path: Path): Selection;
  stats?: UiStats;
  dispose(): void;
}

function blank(value: unknown): boolean {
  return value === undefined || value === null || (typeof value === "string" && value.trim() === "");
}

const utf8 = new TextEncoder();

/** What a named field shows before the operator touches it: its tree value. */
function initialValue(win: UiWindow, name: string): unknown {
  const root = win.tree.value;
  if (!root) return undefined;
  for (const { node } of walk(root, { intoLists: true })) {
    const w = node.widget;
    if (w.kind === "text_field" && w.name === name) return w.value ?? "";
    if (w.kind === "select" && w.name === name) return w.value ?? null;
    if (w.kind === "checkbox" && w.name === name) return w.checked ?? false;
  }
  return undefined;
}

export function createUiChannel(deps: {
  bus: Pick<Bus, "call" | "ui" | "on">;
  /** The ids of the windows the workspace has, for closing the rest. */
  windowIds: () => string[];
  env: UiChannelEnv;
}): UiChannel {
  const { bus, env } = deps;
  const windows = new Map<string, UiWindow>();
  const sentAt = new Map<number, number>();
  let seq = 0;

  function open(win: UiWindow) {
    win.opened = bus.ui(win.id, { op: "open", app: win.app });
  }

  /** Field values from the tree: on first sight, and whenever an app bumps a
   * field's revision. Otherwise the field is the operator's. */
  function adoptFields(win: UiWindow, root: UiNode, base: Path) {
    for (const { node } of walk(root, { intoLists: true }, base)) {
      const w = node.widget;
      if ((w.kind === "text_field" || w.kind === "select") && w.name) {
        const revision = w.revision ?? 0;
        if (win.revisions.get(w.name) !== revision || !win.fields.has(w.name)) {
          win.revisions.set(w.name, revision);
          win.fields.set(w.name, w.kind === "text_field" ? (w.value ?? "") : (w.value ?? null));
        }
      } else if (w.kind === "checkbox" && w.name && !win.fields.has(w.name)) {
        win.fields.set(w.name, w.checked ?? false);
      }
    }
  }

  /** Stop overriding a box: it shows the tree's value again, and so does
   * the field it is named as. */
  function drop(win: UiWindow, key: string) {
    const o = win.overrides.get(key);
    if (!o) return;
    win.overrides.delete(key);
    const node = find(win.tree.value, o.path);
    if (node?.widget.kind === "checkbox" && node.widget.name) {
      win.fields.set(node.widget.name, node.widget.checked ?? false);
    }
  }

  /** Drop every override the app has answered. See `Override`. */
  function settleOverrides(win: UiWindow) {
    const tree = win.tree.value;
    for (const [key, o] of [...win.overrides]) {
      const node = find(tree, o.path);
      if (!node) {
        win.overrides.delete(key);
        continue;
      }
      const now = treeValueOf(node);
      if (o.seq !== null ? win.acked >= o.seq : !o.running && (now === o.value || now !== o.base)) {
        drop(win, key);
      }
    }
  }

  /** Events that will never be answered (refused, or sent on a connection
   * that has gone): the boxes they set show what the app has. */
  function dropUnanswered(win: UiWindow) {
    for (const [key, o] of [...win.overrides]) {
      if (o.seq !== null && o.seq > win.acked) drop(win, key);
    }
  }

  function treeValueOf(node: UiNode): unknown {
    const w = node.widget;
    if (w.kind === "checkbox") return w.checked ?? false;
    return undefined;
  }

  function receive(win: UiWindow, body: UiBody) {
    if ("error" in body) {
      // Whatever was refused, an event it carried was not delivered.
      dropUnanswered(win);
      // The seat's refusal is drawn by the takeover screen, not here.
      if (body.error.code !== "seat_not_held") win.notice.value = body.error.message;
      return;
    }
    if ("status" in body) {
      if (body.status === "lost") {
        win.status.value = "lost";
        // The app that would have answered is gone.
        for (const key of [...win.overrides.keys()]) drop(win, key);
      }
      return;
    }
    const patches: Patch[] = body.patches ?? [];
    if (env.stats) {
      env.stats.frames.push({
        window: win.id,
        // The frame as it crossed the wire: envelope included, UTF-8.
        bytes: utf8.encode(JSON.stringify({ type: "ui", window: win.id, body })).length,
        patches: patches.length,
        restart: isRestart(patches),
      });
    }
    if (typeof body.ack === "number") {
      win.acked = Math.max(win.acked, body.ack);
      const at = sentAt.get(body.ack);
      if (at !== undefined) {
        sentAt.delete(body.ack);
        env.stats?.rtt.push(env.now() - at);
      }
    }
    const restart = isRestart(patches);
    if (!restart && body.rev !== win.rev + 1) {
      // A frame went missing: ask for the whole tree rather than draw a
      // guess. The node answers from its mirror.
      open(win);
      return;
    }
    if (patches.length === 0) {
      // Nothing changed, but an acknowledgment still settles the boxes its
      // event set: the app heard them and left its tree as it was.
      win.rev = body.rev;
      settleOverrides(win);
      return;
    }
    let next: UiNode;
    try {
      next = applyPatches(restart ? null : win.tree.value, patches);
    } catch (e) {
      if (!(e instanceof ApplyError)) throw e;
      open(win);
      return;
    }
    win.rev = body.rev;
    for (const p of patches) {
      if (p.op === "replace" || p.op === "insert") adoptFields(win, p.node, p.path);
      else if (p.op === "props") {
        const node = find(next, p.path);
        if (node) adoptFields(win, { ...node, children: [] }, p.path);
      }
    }
    win.tree.value = next;
    settleOverrides(win);
    if (win.firstPaint === null) {
      win.firstPaint = env.now() - win.openedAt;
      env.stats?.firstPaint.push(win.firstPaint);
    }
    win.status.value = "live";
  }

  const off = bus.on((event) => {
    if (event.kind === "ui") {
      const win = windows.get(event.window);
      if (win) receive(win, event.body as UiBody);
    } else if (event.kind === "node") {
      // A new connection: the node knows nothing of what the old one had
      // open. Windows on screen are opened again at once, the rest when next
      // shown. An event in flight on the old one may never be answered, so
      // the boxes it set show what the app has, from the tree that comes.
      for (const win of windows.values()) {
        win.opened = false;
        dropUnanswered(win);
        if (win.mounted > 0) open(win);
      }
    } else if (event.kind === "seat") {
      // Losing the seat ends this connection's windows on the node (it stops
      // sending them); getting it back opens the ones on screen again.
      for (const win of windows.values()) {
        if (!event.seat.held) win.opened = false;
        else if (!win.opened && win.mounted > 0) open(win);
      }
    }
  });

  const stopWatching = watch(
    () => deps.windowIds().join("\n"),
    () => {
      const live = new Set(deps.windowIds());
      for (const [id, win] of windows) {
        if (live.has(id)) continue;
        for (const p of win.pending.values()) env.clearTimeout(p.timer);
        bus.ui(id, { op: "close" });
        windows.delete(id);
      }
    },
  );

  function sendChanged(win: UiWindow, key: string) {
    const p = win.pending.get(key);
    if (!p) return;
    env.clearTimeout(p.timer);
    win.pending.delete(key);
    const node = find(win.tree.value, p.path);
    const name =
      node && (node.widget.kind === "text_field" || node.widget.kind === "select")
        ? node.widget.name
        : undefined;
    channel.event(win, p.path, "changed", name ? win.fields.get(name) : undefined);
  }

  const channel: UiChannel = {
    stats: env.stats,
    show(window, app) {
      let win = windows.get(window);
      if (!win) {
        win = {
          id: window,
          app,
          tree: shallowRef(null),
          status: ref<WindowStatus>("starting"),
          notice: ref(null),
          fields: reactive(new Map()),
          overrides: reactive(new Map()),
          selections: new Map(),
          selectionVersion: ref(0),
          rev: 0,
          acked: 0,
          opened: false,
          mounted: 0,
          revisions: new Map(),
          pending: new Map(),
          openedAt: env.now(),
          firstPaint: null,
        };
        windows.set(window, win);
      }
      win.mounted += 1;
      if (!win.opened) open(win);
      return win;
    },
    hide(window) {
      const win = windows.get(window);
      if (win) win.mounted = Math.max(0, win.mounted - 1);
    },
    get: (window) => windows.get(window),
    event(win, path, kind, value) {
      seq += 1;
      sentAt.set(seq, env.now());
      // Only the recent ones matter, and an unanswered one must not pile up.
      if (sentAt.size > 64) {
        const oldest = sentAt.keys().next().value;
        if (oldest !== undefined) sentAt.delete(oldest);
      }
      const sent = bus.ui(win.id, { op: "event", event: { path, kind, value: value ?? null, seq } });
      if (!sent) sentAt.delete(seq);
      return sent ? seq : null;
    },
    changed(win, path, name, value) {
      win.fields.set(name, value);
      const key = pathKey(path);
      const existing = win.pending.get(key);
      if (existing) env.clearTimeout(existing.timer);
      win.pending.set(key, {
        path,
        timer: env.setTimeout(() => sendChanged(win, key), CHANGED_DEBOUNCE_MS),
      });
    },
    flush(win) {
      for (const key of [...win.pending.keys()]) sendChanged(win, key);
    },
    ready(win, action) {
      return (action.required ?? []).every((f) => !blank(win.fields.get(f)));
    },
    async run(win, action, own) {
      if (!channel.ready(win, action)) return false;
      // The app hears what every field says before it hears what was done
      // with them.
      channel.flush(win);
      const params: Record<string, unknown> = { ...(action.params ?? {}) };
      for (const [param, field] of Object.entries(action.fields ?? {})) {
        const v = win.fields.get(field);
        params[param] = blank(v) ? null : v;
      }
      if (action.value) params[action.value] = own;
      // What the fields to clear said when this fired.
      const fired = new Map((action.clear ?? []).map((f) => [f, win.fields.get(f)] as const));
      const outcome = await bus.call(action.command, params, undefined, undefined, {
        asApp: win.app,
      });
      if (outcome.status === "ok") {
        win.notice.value = null;
        // Cleared only where it still says what was sent: text typed while
        // the command ran is the operator's next thing, not this one.
        for (const [field, was] of fired) {
          if (JSON.stringify(win.fields.get(field)) === JSON.stringify(was)) {
            win.fields.set(field, initialValue(win, field));
          }
        }
        return true;
      }
      if (outcome.status === "queued") {
        win.notice.value = "Waiting for approval in the Assistant's Inbox.";
        return false;
      }
      // Declining a confirmation is the operator's own choice, not news.
      win.notice.value = outcome.message === "not confirmed" ? null : outcome.message;
      return false;
    },
    valueOf(win, path, treeValue) {
      const o = win.overrides.get(pathKey(path));
      return o ? o.value : treeValue;
    },
    async tick(win, path, value) {
      const node = find(win.tree.value, path);
      if (node?.widget.kind !== "checkbox") return;
      const widget = node.widget;
      const key = pathKey(path);
      const running = win.overrides.get(key)?.running ?? false;
      win.overrides.set(key, { path, value, seq: null, running, base: widget.checked ?? false });
      if (widget.name) win.fields.set(widget.name, value);
      if (!widget.action) {
        const seq = channel.event(win, path, "changed", value);
        const o = win.overrides.get(key);
        // Not sent: the app never heard it, so the box shows what it has.
        if (seq === null) drop(win, key);
        else if (o) o.seq = seq;
        return;
      }
      // One command at a time per box, so the app runs them in the order
      // they were ticked: the one running sends this value when it ends.
      if (running) return;
      win.overrides.get(key)!.running = true;
      for (;;) {
        const want = win.overrides.get(key)?.value;
        if (want === undefined) return;
        const current = find(win.tree.value, path)?.widget;
        const action = current?.kind === "checkbox" && current.action ? current.action : widget.action;
        const ran = await channel.run(win, action, want);
        const o = win.overrides.get(key);
        if (!o) return;
        if (!ran) {
          // Refused or failed: the box shows what the app says.
          drop(win, key);
          return;
        }
        if (o.value === want) break;
        // Ticked again while it ran: that is next.
      }
      const o = win.overrides.get(key)!;
      o.running = false;
      const now = treeValueOf(find(win.tree.value, path) ?? node);
      if (now === o.value) drop(win, key);
      else o.base = now;
    },
    selection(win, path) {
      const key = pathKey(path);
      let s = win.selections.get(key);
      if (!s) {
        s = createSelection({ onChange: () => (win.selectionVersion.value += 1) });
        win.selections.set(key, s);
      }
      return s;
    },
    dispose() {
      off();
      stopWatching();
      for (const win of windows.values()) {
        for (const p of win.pending.values()) env.clearTimeout(p.timer);
      }
    },
  };
  return channel;
}
