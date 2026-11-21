// @vitest-environment happy-dom
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { flushPromises } from "@vue/test-utils";

import { harness, mountInShell, type Harness } from "../../testing/shell-harness.ts";
import { browserWindowsEnv, createWindows, type ShellWindow } from "../../stores/windows.ts";

/**
 * The terminal panel, mounted, against the real stores and a node the test
 * plays.
 *
 * `shell/terminal.test.ts` asks each promise of the client logic directly.
 * This file asks the other question — the one this project keeps getting
 * wrong — whether the panel actually *uses* it: a reattach that exists in a
 * module but that the panel never calls is the "built but never constructed"
 * defect in its purest form, and it passes every unit test.
 *
 * So everything here is observed where the operator or the node would see
 * it: frames on the socket, bytes on the emulator, buttons in the DOM. The
 * one stand-in is xterm.js itself, which needs a canvas happy-dom does not
 * have; the stand-in implements only the surface the panel reads, and
 * models the one behavior the tests lean on — columns shrink as the font
 * grows — from a width the test sets.
 */

// ---- xterm, as much of it as the panel touches ----------------------------

const xterm = vi.hoisted(() => {
  const state = {
    /** The panel's width in pixels, as the fit addon would measure it. */
    width: 1000,
    /** The scrollback every stand-in emulator holds, top row first. */
    lines: [] as Array<{ text: string; wrapped?: boolean }>,
    /** What the stand-in reports as selected. */
    selection: "",
    terminals: [] as Array<{
      selected: Array<[number, number, number]>;
      keyHandler: ((e: { type: string; ctrlKey: boolean; shiftKey: boolean; key: string }) => boolean) | null;
      written: string;
      resets: number;
      pasted: string[];
      onData: ((d: string) => void) | null;
      cols: number;
      rows: number;
      options: { fontSize: number; theme?: unknown };
      modes: { applicationCursorKeysMode: boolean };
    }>,
  };
  return state;
});

vi.mock("@xterm/xterm", () => {
  class Terminal {
    written = "";
    resets = 0;
    pasted: string[] = [];
    selected: Array<[number, number, number]> = [];
    keyHandler: ((e: { type: string; ctrlKey: boolean; shiftKey: boolean; key: string }) => boolean) | null = null;
    onDataHandler: ((d: string) => void) | null = null;
    cols = 80;
    rows = 24;
    options: { fontSize: number; theme?: unknown };
    modes = { applicationCursorKeysMode: false };
    buffer = {
      active: {
        get length() {
          return xterm.lines.length;
        },
        getLine: (i: number) => {
          const line = xterm.lines[i];
          if (!line) return undefined;
          return {
            isWrapped: line.wrapped ?? false,
            translateToString: (trim?: boolean) => (trim ? line.text.trimEnd() : line.text),
          };
        },
      },
    };
    constructor(opts: { fontSize: number }) {
      this.options = { fontSize: opts.fontSize };
      const self = this;
      xterm.terminals.push({
        get written() {
          return self.written;
        },
        get resets() {
          return self.resets;
        },
        get pasted() {
          return self.pasted;
        },
        get selected() {
          return self.selected;
        },
        get keyHandler() {
          return self.keyHandler;
        },
        get onData() {
          return self.onDataHandler;
        },
        get cols() {
          return self.cols;
        },
        get rows() {
          return self.rows;
        },
        options: this.options,
        modes: this.modes,
      } as never);
    }
    loadAddon(addon: { activate(t: unknown): void }) {
      addon.activate(this);
    }
    open() {}
    onData(fn: (d: string) => void) {
      this.onDataHandler = fn;
      return { dispose() {} };
    }
    attachCustomKeyEventHandler(fn: (e: { type: string; ctrlKey: boolean; shiftKey: boolean; key: string }) => boolean) {
      this.keyHandler = fn;
    }
    write(d: string) {
      this.written += d;
    }
    reset() {
      this.resets += 1;
      this.written = "";
    }
    /** xterm's paste ends in an `onData` with the text. */
    paste(text: string) {
      this.pasted.push(text);
      this.onDataHandler?.(text);
    }
    resize(cols: number, rows: number) {
      this.cols = cols;
      this.rows = rows;
    }
    focus() {}
    select(col: number, row: number, length: number) {
      this.selected.push([col, row, length]);
    }
    clearSelection() {}
    scrollToLine() {}
    getSelection() {
      return xterm.selection;
    }
    dispose() {}
  }
  return { Terminal };
});

vi.mock("@xterm/addon-fit", () => {
  class FitAddon {
    term: { cols: number; rows: number; options: { fontSize: number } } | null = null;
    activate(t: never) {
      this.term = t;
    }
    /** A cell is 0.6em wide, near enough for any monospace face. */
    proposeDimensions() {
      if (!this.term) return undefined;
      return { cols: Math.floor(xterm.width / (this.term.options.fontSize * 0.6)), rows: 24 };
    }
    fit() {
      const d = this.proposeDimensions();
      if (d && this.term) {
        this.term.cols = d.cols;
        this.term.rows = d.rows;
      }
    }
  }
  return { FitAddon };
});

// The panel's own import, after the mocks.
const { default: TerminalPanel } = await import("./TerminalPanel.vue");

// ---- the node --------------------------------------------------------------

/** Every `ResizeObserver` the panel made, so a test can report a layout. */
const observers: Array<() => void> = [];
/** What this device's storage holds, behind the stubbed `localStorage`. */
let stored = new Map<string, string>();

beforeEach(() => {
  xterm.width = 1000;
  xterm.terminals.length = 0;
  xterm.lines = [];
  xterm.selection = "";
  observers.length = 0;
  vi.stubGlobal(
    "ResizeObserver",
    class {
      constructor(fn: () => void) {
        observers.push(fn);
      }
      observe() {}
      disconnect() {}
    },
  );
  // happy-dom here has no `localStorage` at all, which is the private-window
  // case, so a test that wants the ordinary one gets a real in-memory store.
  stored = new Map<string, string>();
  vi.stubGlobal("localStorage", {
    getItem: (k: string) => stored.get(k) ?? null,
    setItem: (k: string, v: string) => void stored.set(k, String(v)),
    removeItem: (k: string) => void stored.delete(k),
    clear: () => stored.clear(),
  });
});

afterEach(() => {
  vi.unstubAllGlobals();
  vi.useRealTimers();
});

function shellWindow(id = "w1"): ShellWindow {
  return {
    id,
    app: "terminal",
    appId: "terminal",
    title: "Terminal",
    rect: { x: 0, y: 0, w: 640, h: 400 },
    z: 1,
    minimized: false,
    maximized: false,
    restore: null,
    snap: null,
    loading: false,
    minSize: { w: 420, h: 200 },
  } as ShellWindow;
}

/** Every `command` invoke the panel sent on the current socket. */
function invokes(rig: Harness, command: string) {
  return rig
    .socket()
    .frames()
    .filter((f) => f.type === "invoke" && f.command === command) as Array<{
    id: string;
    command: string;
    params: Record<string, unknown>;
  }>;
}

/** Correlation ids this test has already answered as the node. */
const answeredIds = new WeakMap<Harness, Set<string>>();

/** The `command` invokes still waiting for the node, oldest first. */
function waiting(rig: Harness, command: string) {
  const done = answeredIds.get(rig) ?? new Set<string>();
  return invokes(rig, command).filter((f) => !done.has(f.id));
}

/** Answer the oldest waiting `command` the way the node would. */
async function answer(rig: Harness, command: string, value: unknown) {
  const call = waiting(rig, command)[0];
  if (!call) {
    const seen = rig.socket().frames().map((f) => `${f.type}:${String(f.command ?? f.id)}`);
    throw new Error(`the panel never asked for ${command}; it sent ${seen.join(", ")}`);
  }
  const done = answeredIds.get(rig) ?? new Set<string>();
  done.add(call.id);
  answeredIds.set(rig, done);
  rig.socket().deliver({ type: "response", id: call.id, outcome: { status: "ok", value }, elapsed_ms: 1 });
  await flushPromises();
  return call;
}

function info(id: string, opened_at: number) {
  return { id, label: "shell", opened_at, exit: null, pid: 1, size: { rows: 24, cols: 80 } };
}

function chunk(c: { data?: string; cursor: number; exited?: boolean; reset?: boolean }) {
  return { data: "", exited: false, reset: false, ...c };
}

async function mountPanel(options: { coarsePointer?: boolean; windowId?: string } = {}) {
  const rig = harness({ coarsePointer: options.coarsePointer ?? false });
  rig.connect();
  const { wrapper } = mountInShell(TerminalPanel, {
    harness: rig,
    props: { window: shellWindow(options.windowId) },
    attachTo: document.body,
  });
  await flushPromises();
  return { rig, wrapper };
}

/** Mount a panel and let it open its first shell. */
async function mountWithShell(options: { coarsePointer?: boolean } = {}) {
  const m = await mountPanel(options);
  await answer(m.rig, "terminal.list", []);
  await answer(m.rig, "terminal.open", { id: "t1", rows: 24, cols: 80, cursor: 0 });
  await answer(m.rig, "terminal.list", [info("t1", 1)]);
  return m;
}

// ---- tests -----------------------------------------------------------------

describe("a terminal window", () => {
  it("opens a shell when its session has none, and shows what the shell prints", async () => {
    const { rig, wrapper } = await mountWithShell();

    const read = await answer(rig, "terminal.read", chunk({ data: "$ ", cursor: 2 }));

    expect(read.params).toEqual({ id: "t1", cursor: 0 });
    expect(xterm.terminals[0]!.written).toBe("$ ");
    wrapper.unmount();
  });

  it("reattaches after a reload to the shell it was showing, and replays it, instead of opening another", async () => {
    // The first life of the page.
    const first = await mountWithShell();
    first.wrapper.unmount();

    // The reload: a new page, the same window, and a node that still holds
    // this window's shell and a newer one from another window since closed.
    // "Newest free" would pick t2; this window was showing t1.
    const { rig, wrapper } = await mountPanel();
    await answer(rig, "terminal.list", [info("t1", 1), info("t2", 2)]);
    const replay = await answer(rig, "terminal.read", chunk({ data: "output from before the reload", cursor: 29 }));

    expect(replay.params).toEqual({ id: "t1", cursor: 0 });
    expect(waiting(rig, "terminal.open"), "a reload opened a second shell").toEqual([]);
    expect(xterm.terminals.at(-1)!.written).toBe("output from before the reload");
    wrapper.unmount();
  });

  it("still reattaches after a reload when the browser blocks storage", async () => {
    // A private window: the accessor throws. Nothing is remembered, so the
    // panel takes the newest shell nobody on the page is showing, which is
    // the one it had — not a new shell beside it.
    vi.stubGlobal("localStorage", {
      getItem: () => {
        throw new Error("SecurityError");
      },
      setItem: () => {
        throw new Error("SecurityError");
      },
      removeItem: () => {
        throw new Error("SecurityError");
      },
    });
    const first = await mountWithShell();
    first.wrapper.unmount();

    const { rig, wrapper } = await mountPanel();
    await answer(rig, "terminal.list", [info("t1", 1)]);
    const replay = await answer(rig, "terminal.read", chunk({ data: "still here", cursor: 10 }));

    expect(replay.params).toEqual({ id: "t1", cursor: 0 });
    expect(waiting(rig, "terminal.open")).toEqual([]);
    wrapper.unmount();
  });

  it("rides out a network drop and carries on from the same cursor", async () => {
    const { rig, wrapper } = await mountWithShell();
    await answer(rig, "terminal.read", chunk({ data: "building...", cursor: 11 }));

    // The wifi drops: the socket closes with the read parked.
    rig.socket().close();
    await flushPromises();
    expect(wrapper.text()).toContain("Reconnecting");

    rig.advance(60_000);
    rig.socket().open();
    await flushPromises();

    const resumed = waiting(rig, "terminal.read")[0];
    expect(resumed?.params).toEqual({ id: "t1", cursor: 11 });
    await answer(rig, "terminal.read", chunk({ data: " done", cursor: 16 }));
    expect(xterm.terminals[0]!.written).toBe("building... done");
    expect(xterm.terminals[0]!.resets, "a reconnect cleared the screen").toBe(1);
    wrapper.unmount();
  });

  it("switches between its session's shells, cancelling the read it leaves behind", async () => {
    const { rig, wrapper } = await mountPanel();
    await answer(rig, "terminal.list", [info("t1", 1), info("t2", 2)]);
    const parked = waiting(rig, "terminal.read")[0]!;
    expect(parked.params.id).toBe("t2");

    const tabs = wrapper.findAll("[role=tab]");
    expect(tabs.map((t) => t.text())).toEqual(["shell 1", "shell 2"]);
    await tabs[0]!.trigger("click");
    await flushPromises();

    const cancels = rig.socket().frames().filter((f) => f.type === "cancel");
    expect(cancels.map((f) => f.id), "the abandoned read was left parked on the node").toContain(parked.id);
    expect(waiting(rig, "terminal.read").at(-1)?.params).toEqual({ id: "t1", cursor: 0 });
    wrapper.unmount();
  });

  it("clears the screen before a replay that starts after bytes the node no longer holds", async () => {
    const { rig, wrapper } = await mountWithShell();
    await answer(rig, "terminal.read", chunk({ data: "old screen", cursor: 10 }));
    // A long drop on a busy shell: the ring moved past the panel's cursor, so
    // the replay may start inside an escape sequence.
    await answer(rig, "terminal.read", chunk({ data: "newer output", cursor: 900_000, reset: true }));
    expect(xterm.terminals[0]!.written, "the replay was painted over the stale screen").toBe("newer output");
    wrapper.unmount();
  });

  it("sends keystrokes typed before a tab switch to the shell they were typed into", async () => {
    const { rig, wrapper } = await mountPanel();
    await answer(rig, "terminal.list", [info("t1", 1), info("t2", 2)]);
    const term = xterm.terminals[0]!;

    // A slow link: the first write is still out when the rest is typed and
    // the operator taps the other tab.
    term.onData!("r");
    await flushPromises();
    term.onData!("m -rf build");
    await wrapper.findAll("[role=tab]")[0]!.trigger("click");
    await flushPromises();
    await answer(rig, "terminal.write", null);

    const writes = invokes(rig, "terminal.write").map((w) => w.params);
    expect(writes).toEqual([
      { id: "t2", data: "r" },
      { id: "t2", data: "m -rf build" },
    ]);
    wrapper.unmount();
  });

  it("asks before closing a running shell, and leaves it running when told no", async () => {
    const { rig, wrapper } = await mountWithShell();

    await wrapper.find("[aria-label='Close shell']").trigger("click");
    await flushPromises();
    const asked = rig.shell.ui.confirmation.value;
    expect(asked, "a running shell was closed without asking").toBeTruthy();
    expect(asked!.risk).toBe("write");
    asked!.resolve(false);
    await flushPromises();
    expect(waiting(rig, "terminal.close"), "a declined close still closed the shell").toEqual([]);

    await wrapper.find("[aria-label='Close shell']").trigger("click");
    await flushPromises();
    rig.shell.ui.confirmation.value!.resolve(true);
    await flushPromises();
    expect(waiting(rig, "terminal.close").map((c) => c.params)).toEqual([{ id: "t1" }]);
    wrapper.unmount();
  });

  it("drops the tab of a shell the node already forgot when its close button is pressed", async () => {
    // The node forgets an exited shell after a grace period, while the tab
    // strip only learns of it on the next list. Closing that stale tab is
    // the operator asking for what has already happened: the tab goes, and
    // no error is left on screen.
    const { rig, wrapper } = await mountPanel();
    const gone = { ...info("t0", 0), exit: { code: 0, signal: null, at: 5 } };
    await answer(rig, "terminal.list", [gone, info("t1", 1)]);
    expect(wrapper.findAll(".tab")).toHaveLength(2);

    await wrapper.find("[aria-label='Close shell 1']").trigger("click");
    await flushPromises();
    const close = waiting(rig, "terminal.close")[0]!;
    expect(close.params).toEqual({ id: "t0" });
    answeredIds.get(rig)!.add(close.id);
    rig.socket().deliver({
      type: "response",
      id: close.id,
      outcome: {
        status: "error",
        error: { code: "failed", message: "terminal t0 does not exist here; it may have been closed, or belong to another node" },
      },
      elapsed_ms: 1,
    });
    await flushPromises();
    await answer(rig, "terminal.list", [info("t1", 1)]);

    expect(wrapper.findAll(".tab"), "the forgotten shell's tab stayed").toHaveLength(1);
    expect(wrapper.find("[role='alert']").exists(), "closing a forgotten shell left an error").toBe(false);
    wrapper.unmount();
  });

  it("keeps the tab and shows why when the node refuses to close a shell it still holds", async () => {
    const { rig, wrapper } = await mountPanel();
    const ended = { ...info("t0", 0), exit: { code: 0, signal: null, at: 5 } };
    await answer(rig, "terminal.list", [ended, info("t1", 1)]);

    await wrapper.find("[aria-label='Close shell 1']").trigger("click");
    await flushPromises();
    const close = waiting(rig, "terminal.close")[0]!;
    answeredIds.get(rig)!.add(close.id);
    rig.socket().deliver({
      type: "response",
      id: close.id,
      outcome: { status: "error", error: { code: "failed", message: "could not hang up" } },
      elapsed_ms: 1,
    });
    await flushPromises();
    await answer(rig, "terminal.list", [ended, info("t1", 1)]);

    expect(wrapper.findAll(".tab")).toHaveLength(2);
    expect(wrapper.find("[role='alert']").text()).toContain("could not hang up");
    wrapper.unmount();
  });

  it("finds text in the scrollback and selects the newest match", async () => {
    xterm.lines = [{ text: "$ make" }, { text: "error: missing" }, { text: "$ make" }, { text: "error: again" }, { text: "$ " }];
    const { wrapper } = await mountWithShell();
    const term = xterm.terminals[0]!;

    // The desktop shortcut opens the search box.
    expect(term.keyHandler!({ type: "keydown", ctrlKey: true, shiftKey: true, key: "F" })).toBe(false);
    await flushPromises();
    const box = wrapper.find("input[type=search]");
    expect(box.exists(), "Ctrl+Shift+F did not open search").toBe(true);

    await box.setValue("error");
    await flushPromises();
    expect(term.selected.at(-1), "the newest match was not selected").toEqual([0, 3, 5]);
    expect(wrapper.find(".search .count").text()).toBe("2 of 2");

    await wrapper.find("[aria-label='Previous match']").trigger("click");
    expect(term.selected.at(-1)).toEqual([0, 1, 5]);
    wrapper.unmount();
  });

  it("opens the search box from its button", async () => {
    const { wrapper } = await mountWithShell();
    await wrapper.find("[aria-label='Search scrollback']").trigger("click");
    await flushPromises();
    expect(wrapper.find("input[type=search]").exists()).toBe(true);
    wrapper.unmount();
  });

  it("opens another shell from the tab strip", async () => {
    const { rig, wrapper } = await mountWithShell();
    await wrapper.find("[aria-label='New terminal']").trigger("click");
    await flushPromises();
    expect(waiting(rig, "terminal.open")).toHaveLength(1);
    wrapper.unmount();
  });

  it("leaves nothing of the window on this device once the window is closed", async () => {
    // The panel remembers which shell its window shows, so a reload
    // reattaches to it. That memory belongs to the window, and the window
    // store clears a closed window's storage, but only under the key it
    // hands out. Under any other key the entry outlives the window for good,
    // one per terminal window ever closed, since window ids are never reused.
    const rig = harness();
    rig.connect();
    // The production window store, on this device's storage.
    const windows = createWindows(rig.shell.bus, browserWindowsEnv(), rig.shell.apps);
    const opening = windows.open("terminal");
    await answer(rig, "shell.window.open", null);
    const id = await opening;
    const win = windows.all.value.find((w) => w.id === id)!;

    const { wrapper } = mountInShell(TerminalPanel, {
      harness: rig,
      props: { window: win },
      attachTo: document.body,
    });
    await flushPromises();
    await answer(rig, "terminal.list", []);
    await answer(rig, "terminal.open", { id: "t1", rows: 24, cols: 80, cursor: 0 });
    await answer(rig, "terminal.list", [info("t1", 1)]);
    expect(
      [...stored.entries()].some(([k, v]) => k.includes(id) && v === "t1"),
      "the panel never remembered its shell, so this test proves nothing",
    ).toBe(true);

    const closing = windows.close(id);
    wrapper.unmount();
    await answer(rig, "shell.window.close", null);
    await closing;

    expect([...stored.keys()].filter((k) => k.includes(id)), "a closed window left storage behind").toEqual([]);
  });

  it("cancels its read when the window closes, and leaves the shell running", async () => {
    const { rig, wrapper } = await mountWithShell();
    const parked = waiting(rig, "terminal.read")[0]!;
    wrapper.unmount();
    await flushPromises();
    const frames = rig.socket().frames();
    expect(frames.filter((f) => f.type === "cancel").map((f) => f.id)).toContain(parked.id);
    expect(frames.some((f) => f.command === "terminal.close"), "closing a window killed the shell").toBe(false);
  });
});

describe("on a phone", () => {
  it("shows the keyboard bar, and not on a desktop", async () => {
    const phone = await mountWithShell({ coarsePointer: true });
    const labels = phone.wrapper.findAll(".keybar button").map((b) => b.attributes("aria-label"));
    expect(labels).toEqual([
      "Control",
      "Escape",
      "Tab",
      "Left arrow",
      "Up arrow",
      "Down arrow",
      "Right arrow",
      "Pipe",
      "Tilde",
      "Slash",
    ]);
    phone.wrapper.unmount();

    const desk = await mountWithShell({ coarsePointer: false });
    expect(desk.wrapper.find(".keybar").exists()).toBe(false);
    desk.wrapper.unmount();
  });

  it("interrupts a running command with the latched Ctrl and a typed c", async () => {
    const { rig, wrapper } = await mountWithShell({ coarsePointer: true });
    await wrapper.find("[aria-label=Control]").trigger("click");
    expect(wrapper.find("[aria-label=Control]").attributes("aria-pressed")).toBe("true");

    xterm.terminals[0]!.onData!("c");
    await flushPromises();

    const write = waiting(rig, "terminal.write")[0];
    expect(write?.params).toEqual({ id: "t1", data: "\x03" });
    expect(wrapper.find("[aria-label=Control]").attributes("aria-pressed"), "Ctrl stayed latched").toBe("false");
    wrapper.unmount();
  });

  it("sends the key a bar button stands for", async () => {
    const { rig, wrapper } = await mountWithShell({ coarsePointer: true });
    await wrapper.find("[aria-label=Tab]").trigger("click");
    await flushPromises();
    expect(waiting(rig, "terminal.write")[0]?.params).toEqual({ id: "t1", data: "\t" });
    wrapper.unmount();
  });

  it("pastes through its own box when the browser will not give up the clipboard", async () => {
    // iOS Safari over plain http: no `navigator.clipboard` at all.
    vi.stubGlobal("navigator", { ...navigator, clipboard: undefined });
    const { rig, wrapper } = await mountWithShell({ coarsePointer: true });

    await wrapper.find("[aria-label=Paste]").trigger("click");
    await flushPromises();
    const box = wrapper.find("[role=dialog] textarea");
    expect(box.exists(), "no paste box was offered").toBe(true);
    await box.setValue("cargo test --workspace");
    await wrapper.findAll("[role=dialog] button").find((b) => b.text() === "Send")!.trigger("click");
    await flushPromises();

    expect(xterm.terminals[0]!.pasted).toEqual(["cargo test --workspace"]);
    expect(waiting(rig, "terminal.write")[0]?.params).toEqual({ id: "t1", data: "cargo test --workspace" });
    wrapper.unmount();
  });

  it("opens the scrollback as text to select when Copy has nothing selected", async () => {
    // xterm cannot select by touch, so on a phone there is never a selection.
    xterm.lines = [{ text: "$ cargo test   " }, { text: "test result: ok" }, { text: "" }];
    const { wrapper } = await mountWithShell({ coarsePointer: true });
    await wrapper.find("[aria-label=Copy]").trigger("click");
    await flushPromises();
    const text = wrapper.find("[aria-label='Terminal text']");
    expect(text.exists(), "Copy did nothing with no selection").toBe(true);
    expect((text.element as HTMLTextAreaElement).value).toBe("$ cargo test\ntest result: ok");
    wrapper.unmount();
  });

  /**
   * A phone screen 844 tall whose keyboard opens over the page, as iOS
   * Safari and Chrome on Android do by default: only the visual viewport
   * shrinks. The panel occupies `top`..`bottom` of the layout.
   */
  async function withKeyboard(
    pane: { top: number; bottom: number },
    run: (panel: { inset(): string; openKeyboard(px: number): Promise<void> }) => Promise<void>,
  ) {
    const screen = 844;
    const vv = Object.assign(new EventTarget(), { height: screen, offsetTop: 0 });
    const had = Object.getOwnPropertyDescriptor(window, "visualViewport");
    Object.defineProperty(window, "visualViewport", { configurable: true, value: vv });
    try {
      const { wrapper } = await mountWithShell({ coarsePointer: true });
      const el = wrapper.find(".terminal").element as HTMLElement;
      el.getBoundingClientRect = () =>
        ({
          top: pane.top,
          bottom: pane.bottom,
          left: 0,
          right: 390,
          width: 390,
          height: pane.bottom - pane.top,
          x: 0,
          y: pane.top,
        }) as DOMRect;
      await run({
        inset: () => el.style.paddingBottom,
        async openKeyboard(px) {
          vv.height = screen - px;
          vv.dispatchEvent(new Event("resize"));
          await flushPromises();
        },
      });
      wrapper.unmount();
    } finally {
      if (had) Object.defineProperty(window, "visualViewport", had);
      else delete (window as { visualViewport?: unknown }).visualViewport;
    }
  }

  it("lifts the keyboard bar above the on-screen keyboard", async () => {
    await withKeyboard({ top: 0, bottom: 844 }, async (panel) => {
      await panel.openKeyboard(300);
      expect(panel.inset(), "a full-screen terminal was not lifted by the whole keyboard").toBe("300px");
      await panel.openKeyboard(0);
      expect(panel.inset(), "the inset stayed after the keyboard closed").toBe("");
    });
  });

  it("stays whole in the top half of a stacked split when the keyboard covers only the pane below", async () => {
    // Terminal on top, editor below, keyboard opened by the editor. The
    // keyboard's top edge (844 - 300 = 544) is below this pane's bottom, so
    // none of the terminal is covered and none of it may be given up.
    await withKeyboard({ top: 0, bottom: 420 }, async (panel) => {
      await panel.openKeyboard(300);
      expect(panel.inset(), "a pane the keyboard does not reach was padded").toBe("");
    });
  });

  it("gives up only the part of its pane the keyboard actually covers", async () => {
    // A taller top pane reaching down to 600: the keyboard's top edge at 544
    // covers its last 56px, and only those.
    await withKeyboard({ top: 0, bottom: 600 }, async (panel) => {
      await panel.openKeyboard(300);
      expect(panel.inset()).toBe("56px");
    });
    // Dragging the divider down while the keyboard is open grows the top
    // pane into the keyboard's reach; the panel hears it as a resize.
    const pane = { top: 0, bottom: 420 };
    await withKeyboard(pane, async (panel) => {
      await panel.openKeyboard(300);
      expect(panel.inset()).toBe("");
      pane.bottom = 600;
      for (const o of observers) o();
      await flushPromises();
      expect(panel.inset(), "a pane resized under the keyboard was not re-measured").toBe("56px");
    });
    // The bottom pane of the split is covered by the whole keyboard.
    await withKeyboard({ top: 432, bottom: 844 }, async (panel) => {
      await panel.openKeyboard(300);
      expect(panel.inset()).toBe("300px");
    });
    // A pane shorter than the keyboard cannot give up more than itself.
    await withKeyboard({ top: 700, bottom: 844 }, async (panel) => {
      await panel.openKeyboard(300);
      expect(panel.inset()).toBe("144px");
    });
  });

  it("keeps 80 columns on a narrow screen by shrinking the font", async () => {
    xterm.width = 390;
    const { wrapper } = await mountWithShell({ coarsePointer: true });
    const term = xterm.terminals[0]!;
    expect(term.cols).toBeGreaterThanOrEqual(80);
    expect(term.options.fontSize).toBeLessThan(12);
    wrapper.unmount();
  });

  it("tells the shell its size once when the phone rotates, not on every frame", async () => {
    vi.useFakeTimers();
    const { rig, wrapper } = await mountWithShell({ coarsePointer: true });

    // Portrait to landscape, reported frame by frame as the layout settles.
    for (const w of [500, 620, 700, 780, 844]) {
      xterm.width = w;
      for (const o of observers) o();
      vi.advanceTimersByTime(16);
    }
    vi.advanceTimersByTime(500);
    await flushPromises();

    const resizes = waiting(rig, "terminal.resize");
    expect(resizes, "the shell was told about intermediate sizes").toHaveLength(1);
    expect(resizes[0]!.params).toEqual({ id: "t1", rows: 24, cols: xterm.terminals[0]!.cols });
    wrapper.unmount();
  });
});
