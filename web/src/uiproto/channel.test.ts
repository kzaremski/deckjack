// @vitest-environment happy-dom
import { flushPromises } from "@vue/test-utils";
import { describe, expect, it } from "vitest";
import { nextTick, ref } from "vue";

import type { CallResult, ServerEvent } from "../protocol.ts";
import { UiProtocolPanel, AppFramePanel, panelFor } from "../components/panels/index.ts";
import { parseInstalled } from "../stores/apps.ts";
import { createUiChannel } from "./channel.ts";
import type { UiNode } from "./tree.ts";

/** The channel's own bookkeeping, over a bus that records what it is asked. */
function rig() {
  const sent: Array<{ window: string; body: unknown }> = [];
  const calls: Array<{ command: string; params: unknown; resolve: (r: CallResult) => void }> = [];
  let handler: ((e: ServerEvent) => void) | null = null;
  let connected = true;
  const ids = ref<string[]>(["w1", "w2"]);
  const channel = createUiChannel({
    bus: {
      call: (command, params) =>
        new Promise<CallResult>((resolve) => calls.push({ command, params, resolve })),
      ui(window, body) {
        if (!connected) return false;
        sent.push({ window, body });
        return true;
      },
      on(h) {
        handler = h;
        return () => (handler = null);
      },
    },
    windowIds: () => ids.value,
    env: { now: () => 0, setTimeout: (fn) => setTimeout(fn, 0), clearTimeout: (h) => clearTimeout(h) },
  });
  const emit = (e: ServerEvent) => handler?.(e);
  const frame = (window: string, body: unknown) => emit({ kind: "ui", window, body });
  const events = () =>
    sent
      .map((s) => (s.body as { op: string; event?: { kind: string; value: unknown; seq: number } }))
      .filter((b) => b.op === "event")
      .map((b) => b.event!);
  const opens = () => sent.filter((s) => (s.body as { op: string }).op === "open").length;
  return {
    channel,
    sent,
    calls,
    ids,
    emit,
    frame,
    events,
    opens,
    disconnect: () => (connected = false),
    reconnect: () => (connected = true),
  };
}

/** A window with an unbound box, a bound box, and the add row's fields. */
function tree(showDone: boolean, done = false): UiNode {
  return {
    key: "root",
    widget: { kind: "stack" },
    children: [
      { key: "show_done", widget: { kind: "checkbox", label: "Show done", name: "show_done", checked: showDone } },
      {
        key: "done",
        widget: {
          kind: "checkbox",
          label: "Done",
          checked: done,
          action: { command: "tasks.complete", params: { id: "t1" }, value: "done" },
        },
      },
      { key: "summary", widget: { kind: "text_field", name: "summary", value: "" } },
      {
        key: "priority",
        widget: { kind: "select", name: "priority", value: 0, options: [{ value: 0, label: "-" }, { value: 1, label: "High" }] },
      },
    ],
  };
}

const SHOW = ["show_done"];
const DONE = ["done"];
const showDone = (t: UiNode) => (t.children![0]!.widget as { checked?: boolean }).checked ?? false;

/** `win` drawn with `t` as revision 1. */
function drawn(r: ReturnType<typeof rig>, t: UiNode = tree(false)) {
  const win = r.channel.show("w1", "tasks");
  r.frame("w1", { rev: 1, patches: [{ op: "replace", path: [], node: t }] });
  return win;
}

/** The props patch that sets "Show done" to `on`. */
const showPatch = (on: boolean) => ({
  op: "props",
  path: SHOW,
  widget: { kind: "checkbox", label: "Show done", name: "show_done", checked: on },
});

describe("the UI channel", () => {
  it("closes a window on the node once the workspace no longer has it", async () => {
    const r = rig();
    r.channel.show("w1", "tasks");
    r.channel.show("w2", "tasks");
    r.ids.value = ["w2"];
    await nextTick();
    expect(r.sent.at(-1)).toEqual({ window: "w1", body: { op: "close" } });
    expect(r.channel.get("w1")).toBeUndefined();
    expect(r.channel.get("w2")).toBeDefined();
  });

  it("opens the windows on screen again on a new connection, and not the others", () => {
    const r = rig();
    r.channel.show("w1", "tasks");
    r.channel.show("w2", "tasks");
    r.channel.hide("w2");
    r.sent.length = 0;
    r.emit({ kind: "node", name: "pocket", generation: 2 });
    expect(r.sent).toEqual([{ window: "w1", body: { op: "open", app: "tasks" } }]);
    // Shown again later, the other is opened then.
    r.channel.show("w2", "tasks");
    expect(r.sent.at(-1)).toEqual({ window: "w2", body: { op: "open", app: "tasks" } });
  });

  it("does not open a window twice on one connection", () => {
    const r = rig();
    r.channel.show("w1", "tasks");
    r.channel.hide("w1");
    r.channel.show("w1", "tasks");
    expect(r.sent.filter((s) => (s.body as { op: string }).op === "open")).toHaveLength(1);
  });
});

describe("which panel draws an installed app", () => {
  it("is the renderer for a tier-1 app and the frame for one that serves a page", () => {
    const apps = parseInstalled({
      apps: [
        { name: "tasks", tier: 1, compact: true, wide: true },
        { name: "notes", compact: true, wide: true },
      ],
    });
    const table = {
      isInstalled: (id: string) => apps.some((a) => a.name === id),
      isDescribed: (id: string) => apps.some((a) => a.name === id && a.tier === 1),
    };
    expect(apps.find((a) => a.name === "tasks")?.tier).toBe(1);
    expect(panelFor("tasks", table)).toBe(UiProtocolPanel);
    expect(panelFor("notes", table)).toBe(AppFramePanel);
  });
});

describe("a checkbox under latency", () => {
  it("is not flipped back by the app's answer to an earlier tick", () => {
    const r = rig();
    const win = drawn(r);
    void r.channel.tick(win, SHOW, true);
    void r.channel.tick(win, SHOW, false);
    const [first, second] = r.events();
    expect([first!.value, second!.value]).toEqual([true, false]);
    // The answer to the first tick: the app turned it on.
    r.frame("w1", { rev: 2, ack: first!.seq, patches: [showPatch(true)] });
    expect(r.channel.valueOf(win, SHOW, showDone(win.tree.value!))).toBe(false);
    expect(win.fields.get("show_done")).toBe(false);
    // The answer to the second: off again, and the tree is the box again.
    r.frame("w1", { rev: 3, ack: second!.seq, patches: [showPatch(false)] });
    expect(win.overrides.size).toBe(0);
    expect(r.channel.valueOf(win, SHOW, showDone(win.tree.value!))).toBe(false);
  });

  it("shows what the app has when the app answers the latest tick differently", () => {
    const r = rig();
    const win = drawn(r);
    void r.channel.tick(win, SHOW, true);
    // The app acknowledged it and left the box off (it refused the idea).
    r.frame("w1", { rev: 2, ack: r.events()[0]!.seq, patches: [] });
    expect(r.channel.valueOf(win, SHOW, false)).toBe(false);
    expect(win.fields.get("show_done")).toBe(false);
  });

  it("goes back at once when the tick could not be sent", () => {
    const r = rig();
    const win = drawn(r);
    r.disconnect();
    void r.channel.tick(win, SHOW, true);
    expect(r.events()).toHaveLength(0);
    expect(r.channel.valueOf(win, SHOW, false)).toBe(false);
    expect(win.fields.get("show_done")).toBe(false);
  });

  it("goes back when the node refuses the tick, or the connection it went on is gone", () => {
    const r = rig();
    const win = drawn(r);
    void r.channel.tick(win, SHOW, true);
    r.frame("w1", { error: { code: "seat_not_held", message: "another device" } });
    expect(r.channel.valueOf(win, SHOW, false)).toBe(false);

    void r.channel.tick(win, SHOW, true);
    r.emit({ kind: "node", name: "pocket", generation: 2 });
    expect(r.channel.valueOf(win, SHOW, false)).toBe(false);
    // And the window is asked for again, so the tree says what the app has.
    expect(r.opens()).toBe(2);
  });

  it("runs a bound box's commands one at a time, in the order ticked", async () => {
    const r = rig();
    const win = drawn(r);
    void r.channel.tick(win, DONE, true);
    void r.channel.tick(win, DONE, false);
    void r.channel.tick(win, DONE, true);
    void r.channel.tick(win, DONE, false);
    // Only the first has gone; the box shows the last tick.
    expect(r.calls.map((c) => c.params)).toEqual([{ id: "t1", done: true }]);
    expect(r.channel.valueOf(win, DONE, false)).toBe(false);
    // The app's reload after the first lands: still the operator's value.
    r.frame("w1", { rev: 2, patches: [{ op: "props", path: DONE, widget: { ...tree(false, true).children![1]!.widget } }] });
    expect(r.channel.valueOf(win, DONE, true)).toBe(false);
    r.calls[0]!.resolve({ id: "c1", status: "ok", value: null });
    await flushPromises();
    // Then the latest, not every tick in between.
    expect(r.calls.map((c) => c.params)).toEqual([
      { id: "t1", done: true },
      { id: "t1", done: false },
    ]);
    r.calls[1]!.resolve({ id: "c2", status: "ok", value: null });
    await flushPromises();
    r.frame("w1", { rev: 3, patches: [{ op: "props", path: DONE, widget: { ...tree(false, false).children![1]!.widget } }] });
    expect(win.overrides.size).toBe(0);
  });
});

describe("clearing fields after an action", () => {
  const add = {
    command: "tasks.add",
    fields: { summary: "summary", priority: "priority" },
    clear: ["summary", "priority"],
  };

  it("clears what still says what was sent, and keeps what was typed meanwhile", async () => {
    const r = rig();
    const win = drawn(r);
    win.fields.set("summary", "Buy milk");
    win.fields.set("priority", 1);
    const ran = r.channel.run(win, add);
    expect(r.calls[0]!.params).toEqual({ summary: "Buy milk", priority: 1 });
    // The operator starts the next one before the answer.
    win.fields.set("summary", "Eggs");
    r.calls[0]!.resolve({ id: "c1", status: "ok", value: null });
    expect(await ran).toBe(true);
    expect(win.fields.get("summary")).toBe("Eggs");
    expect(win.fields.get("priority")).toBe(0);
  });

  it("clears nothing when the command fails", async () => {
    const r = rig();
    const win = drawn(r);
    win.fields.set("summary", "Buy milk");
    const ran = r.channel.run(win, add);
    r.calls[0]!.resolve({ id: "c1", status: "error", message: "no" });
    expect(await ran).toBe(false);
    expect(win.fields.get("summary")).toBe("Buy milk");
  });
});

describe("the seat", () => {
  it("opens the windows on screen again when the seat comes back", () => {
    const r = rig();
    r.channel.show("w1", "tasks");
    r.channel.show("w2", "tasks");
    r.channel.hide("w2");
    const seat = (held: boolean) =>
      r.emit({ kind: "seat", seat: { held, claimable: false, holder: null } } as ServerEvent);
    seat(false);
    expect(r.opens()).toBe(2);
    seat(true);
    // w1 is on screen and asked for again; w2 when it is next shown.
    expect(r.sent.at(-1)).toEqual({ window: "w1", body: { op: "open", app: "tasks" } });
    expect(r.opens()).toBe(3);
    r.channel.show("w2", "tasks");
    expect(r.opens()).toBe(4);
  });
});

describe("measuring", () => {
  it("counts a frame as it crossed the wire: envelope included, in UTF-8", () => {
    const r = rig();
    const stats = { rtt: [] as number[], frames: [] as { bytes: number }[], firstPaint: [] as number[] };
    const channel = createUiChannel({
      bus: { call: async () => ({ id: "c", status: "ok", value: null }), ui: () => true, on: (h) => ((r as { h?: unknown }).h = h, () => {}) },
      windowIds: () => ["w1"],
      env: { now: () => 0, setTimeout: (fn) => setTimeout(fn, 0), clearTimeout: (h) => clearTimeout(h), stats: stats as never },
    });
    channel.show("w1", "tasks");
    const body = { rev: 1, ack: null, patches: [{ op: "replace", path: [], node: { key: "r", widget: { kind: "text", text: "—" } } }] };
    ((r as unknown as { h: (e: ServerEvent) => void }).h)({ kind: "ui", window: "w1", body });
    const wire = new TextEncoder().encode(JSON.stringify({ type: "ui", window: "w1", body })).length;
    expect(stats.frames[0]!.bytes).toBe(wire);
    expect(wire).toBeGreaterThan(JSON.stringify(body).length);
  });
});
