// @vitest-environment happy-dom
import { flushPromises } from "@vue/test-utils";
import { describe, expect, it } from "vitest";
import { nextTick, reactive } from "vue";

import { harness, mountInShell, type Harness } from "../testing/shell-harness.ts";
import type { ShellWindow } from "../stores/windows.ts";
import UiProtocolPanel from "./UiProtocolPanel.vue";
import type { Patch, UiNode } from "./tree.ts";

/**
 * A described window, through the real shell: the real bus over a fake
 * socket, the real UI channel, the real renderer. The node is played by the
 * test, frame by frame, so what is asserted is what reaches the wire and
 * what is on screen — and, for the local-first rule, that the second happens
 * without the first.
 */

function tasksWindow(): ShellWindow {
  return reactive({
    id: "w7",
    app: "tasks",
    appId: "tasks",
    title: "Tasks",
    loading: false,
    launch: null,
  }) as unknown as ShellWindow;
}

const add = {
  command: "tasks.add",
  fields: { summary: "summary", due: "due", priority: "priority" },
  required: ["summary"],
};

function tree(overrides: { rows?: UiNode[]; showDone?: boolean; cleared?: number; value?: string } = {}): UiNode {
  return {
    key: "root",
    widget: { kind: "stack", grow: true },
    children: [
      {
        key: "bar",
        widget: { kind: "row", bar: true },
        children: [
          { key: "title", widget: { kind: "heading", text: "Tasks" } },
          // A button the app handles itself: not a bus command, so not a palette entry.
          { key: "refresh", widget: { kind: "button", label: "Refresh" } },
          {
            key: "show_done",
            widget: { kind: "checkbox", label: "Show done", name: "show_done", checked: overrides.showDone ?? false },
          },
        ],
      },
      {
        key: "add",
        widget: { kind: "row" },
        children: [
          {
            key: "summary",
            widget: {
              kind: "text_field",
              name: "summary",
              label: "New task",
              value: overrides.value ?? "",
              revision: overrides.cleared ?? 0,
              submit: add,
            },
          },
          { key: "due", widget: { kind: "text_field", name: "due", label: "Due date", input: "date" } },
          {
            key: "priority",
            widget: {
              kind: "select",
              name: "priority",
              label: "Priority",
              value: 0,
              options: [
                { value: 0, label: "—" },
                { value: 1, label: "High" },
              ],
            },
          },
          { key: "add", widget: { kind: "button", label: "Add", action: add } },
        ],
      },
      { key: "tasks", widget: { kind: "list", label: "Tasks" }, children: overrides.rows ?? [row("t1", "Water plants")] },
    ],
  };
}

function row(id: string, text: string, done = false): UiNode {
  return {
    key: id,
    widget: { kind: "row" },
    children: [
      {
        key: "done",
        widget: {
          kind: "checkbox",
          label: "Done",
          show_label: false,
          checked: done,
          action: { command: "tasks.complete", params: { id }, value: "done" },
        },
      },
      { key: "what", widget: { kind: "text", text, strike: done } },
      { key: "drop", widget: { kind: "button", label: "×", title: "Delete", action: { command: "tasks.delete", params: { id } } } },
    ],
  };
}

function connected(): Harness {
  const rig = harness();
  rig.connect();
  return rig;
}

function mounted(rig = connected()) {
  const { wrapper } = mountInShell(UiProtocolPanel, {
    harness: rig,
    props: { window: tasksWindow() },
    attachTo: document.body,
  });
  const sent = () => rig.socket().frames();
  const ui = () => sent().filter((f) => f.type === "ui");
  const invokes = () => sent().filter((f) => f.type === "invoke");
  const frame = (body: unknown) => rig.socket().deliver({ type: "ui", window: "w7", body });
  const draw = async (t: UiNode, rev = 1) => {
    frame({ rev, patches: [{ op: "replace", path: [], node: t }] });
    await nextTick();
  };
  const answer = async (id: unknown, outcome: unknown) => {
    rig.socket().deliver({ type: "response", id, outcome, elapsed_ms: 1 });
    await flushPromises();
  };
  return { wrapper, rig, sent, ui, invokes, frame, draw, answer };
}

describe("opening", () => {
  it("asks the node for the window and draws the tree it sends, as text", async () => {
    const m = mounted();
    expect(m.ui()).toEqual([{ type: "ui", window: "w7", body: { op: "open", app: "tasks" } }]);
    expect(m.wrapper.text()).toContain("Starting Tasks");
    await m.draw(tree({ rows: [row("t1", '<img src=x onerror="alert(1)">')] }));
    expect(m.wrapper.find("h2").text()).toBe("Tasks");
    // Markup an app sends is shown, never parsed.
    expect(m.wrapper.find("img").exists()).toBe(false);
    expect(m.wrapper.text()).toContain('<img src=x onerror="alert(1)">');
    expect(m.wrapper.find("iframe").exists()).toBe(false);
  });

  it("asks for the whole tree again when a frame went missing", async () => {
    const m = mounted();
    await m.draw(tree());
    const opens = () => m.ui().filter((f) => (f.body as { op: string }).op === "open").length;
    expect(opens()).toBe(1);
    m.frame({ rev: 3, patches: [{ op: "remove", path: ["tasks"], key: "t1" }] satisfies Patch[] });
    await nextTick();
    expect(opens()).toBe(2);
    // And nothing was drawn from the frame it could not place.
    expect(m.wrapper.text()).toContain("Water plants");
  });

  it("says so when the app has gone, and keeps what it last drew", async () => {
    const m = mounted();
    await m.draw(tree());
    m.frame({ status: "lost" });
    await nextTick();
    expect(m.wrapper.text()).toContain("Tasks stopped");
    expect(m.wrapper.text()).toContain("Water plants");
  });
});

describe("local first", () => {
  it("typing enables Add at once, and the app hears the text only after a pause", async () => {
    const m = mounted();
    await m.draw(tree());
    const button = m.wrapper.findAll("button").find((b) => b.text() === "Add")!;
    expect(button.attributes("disabled")).toBeDefined();
    const before = m.ui().length;
    await m.wrapper.find('input[aria-label="New task"]').setValue("Buy milk");
    expect(button.attributes("disabled")).toBeUndefined();
    expect(m.ui().length).toBe(before);
    m.rig.advance(249);
    expect(m.ui().length).toBe(before);
    m.rig.advance(1);
    const changed = m.ui().at(-1)!.body as { event: { kind: string; value: string; path: string[] } };
    expect(changed.event.kind).toBe("changed");
    expect(changed.event.value).toBe("Buy milk");
    expect(changed.event.path).toEqual(["add", "summary"]);
  });

  it("Add sends the fields as they are now, as the app's window, after the pending change", async () => {
    const m = mounted();
    await m.draw(tree());
    await m.wrapper.find('input[aria-label="New task"]').setValue("Buy milk");
    await m.wrapper.find("select").setValue("1");
    const button = m.wrapper.findAll("button").find((b) => b.text() === "Add")!;
    await button.trigger("click");
    const frames = m.sent();
    const invoke = frames.filter((f) => f.type === "invoke").at(-1)!;
    expect(invoke).toMatchObject({
      command: "tasks.add",
      as_app: "tasks",
      params: { summary: "Buy milk", due: null, priority: 1 },
    });
    // The app heard the text before it heard what was done with it.
    const changedAt = frames.findIndex((f) => {
      const e = (f.body as { event?: { kind: string; path: string[] } } | undefined)?.event;
      return f.type === "ui" && e?.kind === "changed" && e.path.join("/") === "add/summary";
    });
    expect(changedAt).toBeGreaterThanOrEqual(0);
    expect(changedAt).toBeLessThan(frames.indexOf(invoke));
  });

  it("a field keeps the operator's text through an echo, and takes the app's on a new revision", async () => {
    const m = mounted();
    await m.draw(tree());
    const field = m.wrapper.find('input[aria-label="New task"]');
    await field.setValue("Buy milk and eggs");
    // The app echoes an older value: not applied.
    m.frame({
      rev: 2,
      patches: [{ op: "props", path: ["add", "summary"], widget: { kind: "text_field", name: "summary", label: "New task", value: "Buy", submit: add } }],
    });
    await nextTick();
    expect((field.element as HTMLInputElement).value).toBe("Buy milk and eggs");
    // A new revision clears it, as after an add.
    m.frame({
      rev: 3,
      patches: [{ op: "props", path: ["add", "summary"], widget: { kind: "text_field", name: "summary", label: "New task", revision: 1, submit: add } }],
    });
    await nextTick();
    expect((field.element as HTMLInputElement).value).toBe("");
  });

  it("a bound tick shows at once, dispatches as the window, and goes back if refused", async () => {
    const m = mounted();
    await m.draw(tree());
    const tick = m.wrapper.find('input[aria-label="Done"]');
    await tick.setValue(true);
    expect((tick.element as HTMLInputElement).checked).toBe(true);
    const invoke = m.invokes().at(-1)!;
    expect(invoke).toMatchObject({ command: "tasks.complete", as_app: "tasks", params: { id: "t1", done: true } });
    await m.answer(invoke.id, { status: "error", error: { code: "not_permitted", message: "tasks may not" } });
    expect((m.wrapper.find('input[aria-label="Done"]').element as HTMLInputElement).checked).toBe(false);
    expect(m.wrapper.find('[role="alert"]').text()).toContain("tasks may not");
  });

  it("a bound tick that ran stays ticked until the app's tree agrees", async () => {
    const m = mounted();
    await m.draw(tree());
    await m.wrapper.find('input[aria-label="Done"]').setValue(true);
    await m.answer(m.invokes().at(-1)!.id, { status: "ok", value: null });
    expect((m.wrapper.find('input[aria-label="Done"]').element as HTMLInputElement).checked).toBe(true);
    m.frame({ rev: 2, patches: [{ op: "replace", path: ["tasks", "t1"], node: row("t1", "Water plants", true) }] });
    await nextTick();
    expect((m.wrapper.find('input[aria-label="Done"]').element as HTMLInputElement).checked).toBe(true);
    expect(m.wrapper.find(".ui-text[data-strike]").exists()).toBe(true);
  });

  it("a tick the app's tree overrules is shown as the tree says", async () => {
    // Ticked here and run; then another device unticked it, and the app's
    // tree says so. The tree wins over what this device last did.
    const m = mounted();
    await m.draw(tree());
    await m.wrapper.find('input[aria-label="Done"]').setValue(true);
    await m.answer(m.invokes().at(-1)!.id, { status: "ok", value: null });
    m.frame({ rev: 2, patches: [{ op: "replace", path: ["tasks", "t1"], node: row("t1", "Water plants", true) }] });
    await nextTick();
    m.frame({ rev: 3, patches: [{ op: "replace", path: ["tasks", "t1"], node: row("t1", "Water plants", false) }] });
    await nextTick();
    expect((m.wrapper.find('input[aria-label="Done"]').element as HTMLInputElement).checked).toBe(false);
  });

  it("a blur sends the pending change at once, without waiting out the pause", async () => {
    const m = mounted();
    await m.draw(tree());
    const field = m.wrapper.find('input[aria-label="New task"]');
    await field.setValue("Buy milk");
    const before = m.ui().length;
    await field.trigger("blur");
    const changed = m.ui().slice(before);
    expect(changed).toHaveLength(1);
    expect((changed[0]!.body as { event: { kind: string; value: string } }).event).toMatchObject({
      kind: "changed",
      value: "Buy milk",
    });
    // And the pause that follows sends nothing more.
    m.rig.advance(500);
    expect(m.ui().length).toBe(before + 1);
  });

  it("a ticked box stays as the operator left it through the app's answer to an earlier tick", async () => {
    const m = mounted();
    await m.draw(tree());
    const box = () => m.wrapper.findAll('input[type="checkbox"]')[0]!;
    await box().setValue(true);
    await box().setValue(false);
    const seqs = m
      .ui()
      .map((f) => (f.body as { event?: { seq: number } }).event?.seq)
      .filter((q): q is number => q !== undefined);
    m.frame({ rev: 2, ack: seqs[0], patches: [{ op: "replace", path: ["bar", "show_done"], node: tree({ showDone: true }).children![0]!.children![2]! }] });
    await nextTick();
    expect((box().element as HTMLInputElement).checked).toBe(false);
  });

  it("Add clears the fields it names once the task is added, in this window", async () => {
    const m = mounted();
    const t = tree();
    const addRow = t.children![1]!.children!;
    const clearing = { ...add, clear: ["summary", "priority"] };
    (addRow[3]!.widget as { action: unknown }).action = clearing;
    await m.draw(t);
    const field = m.wrapper.find('input[aria-label="New task"]');
    await field.setValue("Buy milk");
    await m.wrapper.find("select").setValue("1");
    await m.wrapper.findAll("button").find((b) => b.text() === "Add")!.trigger("click");
    await m.answer(m.invokes().at(-1)!.id, { status: "ok", value: null });
    expect((field.element as HTMLInputElement).value).toBe("");
    expect((m.wrapper.find("select").element as HTMLSelectElement).value).toBe("0");
  });

  it("an unbound box tells the app at once, with no command", async () => {
    const m = mounted();
    await m.draw(tree());
    const before = m.invokes().length;
    const box = m.wrapper.findAll('input[type="checkbox"]')[0]!;
    await box.setValue(true);
    expect((box.element as HTMLInputElement).checked).toBe(true);
    const event = m.ui().at(-1)!.body as { event: { kind: string; value: boolean; path: string[] } };
    expect(event.event).toMatchObject({ kind: "changed", value: true, path: ["bar", "show_done"] });
    expect(m.invokes().length).toBe(before);
  });
});

describe("lists", () => {
  it("draws only the rows in view of a long list", async () => {
    const m = mounted();
    const rows = Array.from({ length: 10_000 }, (_, i) => row(`t${i}`, `Row ${i}`));
    await m.draw(tree({ rows }));
    const drawn = m.wrapper.findAll(".ui-list-row").length;
    expect(drawn).toBeGreaterThan(0);
    expect(drawn).toBeLessThan(100);
    // And a short one draws every row.
    await m.draw(tree({ rows: rows.slice(0, 150) }), 2);
    expect(m.wrapper.findAll(".ui-list-row").length).toBe(150);
  });

  it("selects by the kit's rules and tells the app the keys", async () => {
    const m = mounted();
    const t = tree();
    t.children![2] = {
      key: "tasks",
      widget: { kind: "list", label: "Tasks", selection: "multi" },
      children: [row("a", "A"), row("b", "B"), row("c", "C")],
    };
    await m.draw(t);
    const rows = m.wrapper.findAll('[role="option"]');
    await rows[0]!.trigger("click");
    await rows[2]!.trigger("click", { shiftKey: true });
    const last = m.ui().at(-1)!.body as { event: { kind: string; value: string[] } };
    expect(last.event).toMatchObject({ kind: "selected", value: ["a", "b", "c"] });
    expect(m.wrapper.findAll('[aria-selected="true"]').length).toBe(3);
    await rows[1]!.trigger("click", { ctrlKey: true });
    expect((m.ui().at(-1)!.body as { event: { value: string[] } }).event.value).toEqual(["a", "c"]);
  });
});

describe("a list's selection set by the app", () => {
  function selectable(selection: "single" | "multi", selected: string[]): UiNode {
    const t = tree();
    t.children![2] = {
      key: "tasks",
      widget: { kind: "list", label: "Tasks", selection, selected },
      children: [row("a", "A"), row("b", "B"), row("c", "C")],
    };
    return t;
  }
  const chosen = (m: ReturnType<typeof mounted>) =>
    m.wrapper.findAll('[aria-selected="true"]').map((r) => r.attributes("data-key"));

  it("is shown from the first frame", async () => {
    const m = mounted();
    await m.draw(selectable("multi", ["b"]));
    expect(chosen(m)).toEqual(["b"]);
  });

  it("is one key in a single-select list, whatever the app sent", async () => {
    const m = mounted();
    await m.draw(selectable("single", ["a", "c"]));
    expect(chosen(m)).toEqual(["a"]);
  });

  it("is not taken from an echo the app sent before hearing the latest click", async () => {
    const m = mounted();
    await m.draw(selectable("single", []));
    const rows = () => m.wrapper.findAll('[role="option"]');
    await rows()[0]!.trigger("click");
    await rows()[1]!.trigger("click");
    const seqs = m
      .ui()
      .map((f) => (f.body as { event?: { kind: string; seq: number } }).event)
      .filter((e) => e?.kind === "selected")
      .map((e) => e!.seq);
    const echo = (selected: string[], ack: number, rev: number) =>
      m.frame({
        rev,
        ack,
        patches: [{ op: "props", path: ["tasks"], widget: { kind: "list", label: "Tasks", selection: "single", selected } }],
      });
    echo(["a"], seqs[0]!, 2);
    await nextTick();
    expect(chosen(m)).toEqual(["b"]);
    echo(["b"], seqs[1]!, 3);
    await nextTick();
    expect(chosen(m)).toEqual(["b"]);
  });
});

describe("the palette", () => {
  it("offers the focused window's bound buttons, enabled exactly when the button is", async () => {
    const rig = connected();
    const m = mounted(rig);
    await m.draw(tree());
    expect(rig.shell.commands.byId("ui.window.w7.add.add")).toBeNull();
    rig.shell.windows.focusedId.value = "w7";
    await nextTick();
    const entry = rig.shell.commands.byId("ui.window.w7.add.add")!;
    expect(entry.title).toBe("Tasks: Add");
    expect(entry.available!()).toBe(false);
    await m.wrapper.find('input[aria-label="New task"]').setValue("Buy milk");
    expect(entry.available!()).toBe(true);
    void entry.run();
    await flushPromises();
    expect(m.invokes().at(-1)).toMatchObject({ command: "tasks.add", as_app: "tasks" });
    // Row buttons inside a list are not palette entries, nor are buttons the
    // app handles itself.
    expect(rig.shell.commands.byId("ui.window.w7.tasks.t1.drop")).toBeNull();
    expect(rig.shell.commands.byId("ui.window.w7.bar.refresh")).toBeNull();
    expect(rig.shell.commands.all.value.filter((c) => c.id.startsWith("ui.window.w7.")).map((c) => c.id)).toEqual([
      "ui.window.w7.add.add",
    ]);
  });
});

describe("progress", () => {
  const bar = (value?: number | null, label = "Encoding a.mov"): UiNode => ({
    key: "root",
    widget: { kind: "stack" },
    children: [{ key: "bar", widget: { kind: "progress", label, ...(value === undefined ? {} : { value }) } }],
  });

  it("draws a native bar named for what is progressing, clamped to its range", async () => {
    const m = mounted();
    await m.draw(bar(0.25));
    const el = m.wrapper.find("progress");
    expect(el.attributes("aria-label")).toBe("Encoding a.mov");
    expect((el.element as HTMLProgressElement).value).toBe(0.25);
    m.frame({ rev: 2, patches: [{ op: "props", path: ["bar"], widget: { kind: "progress", value: 7, label: "x" } }] });
    await nextTick();
    expect((m.wrapper.find("progress").element as HTMLProgressElement).value).toBe(1);
  });

  it("is indeterminate while nobody knows how far, which is not the same as not started", async () => {
    const m = mounted();
    await m.draw(bar());
    expect(m.wrapper.find("progress").attributes("value")).toBeUndefined();
    await m.draw(bar(0), 2);
    expect(m.wrapper.find("progress").attributes("value")).toBe("0");
  });
});
