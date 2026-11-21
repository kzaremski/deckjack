import { describe, expect, it } from "vitest";

import {
  applyCtrl,
  chooseTerminal,
  copyText,
  createResizeSender,
  createWriteQueue,
  findMatches,
  fontForColumns,
  follow,
  keySequence,
  logicalLines,
  readClipboard,
  stepMatch,
  MIN_COLUMNS,
  type Chunk,
  type TerminalInfo,
  type TerminalLink,
} from "./terminal.ts";
import type { CallResult } from "../protocol.ts";

/**
 * What the terminal panel promises, asked without a browser.
 *
 * The panel itself is xterm.js plus wiring; everything with a promise in it
 * lives in `terminal.ts` so it can be asked directly. Each test states the
 * operator-visible claim — "a reload replays the screen", "a rotate sends one
 * size" — and drives the code the panel calls, never a private helper.
 */

// ---- a node the test drives ------------------------------------------------

/**
 * The node's half of `terminal.read`: each call parks until the test answers
 * it, the way the real long poll parks until the shell prints.
 */
class FakeNode implements TerminalLink {
  up = true;
  calls: Array<{
    command: string;
    params: Record<string, unknown>;
    signal?: AbortSignal;
    answer: (r: CallResult) => void;
  }> = [];
  private reconnected: Array<() => void> = [];

  call(command: string, params: Record<string, unknown>, signal?: AbortSignal) {
    return new Promise<CallResult>((answer) => {
      this.calls.push({ command, params, signal, answer });
      // A cancelled call is answered, as the node does.
      signal?.addEventListener("abort", () =>
        answer({ id: "x", status: "error", message: "cancelled" }),
      );
    });
  }

  connected() {
    return this.up;
  }

  untilConnected(signal: AbortSignal) {
    return new Promise<void>((resolve) => {
      if (this.up || signal.aborted) return resolve();
      this.reconnected.push(resolve);
      signal.addEventListener("abort", () => resolve());
    });
  }

  /** The most recent call still waiting. */
  last() {
    const c = this.calls.at(-1);
    if (!c) throw new Error("nothing has been asked");
    return c;
  }

  chunk(c: Partial<Chunk>) {
    this.last().answer({
      id: "x",
      status: "ok",
      value: { data: "", cursor: 0, exited: false, reset: false, ...c },
    });
  }

  /** The socket drops: the waiting call fails, as `failPending` does. */
  drop() {
    this.up = false;
    this.last().answer({ id: "x", status: "error", message: "connection to the shell node closed" });
  }

  reconnect() {
    this.up = true;
    for (const r of this.reconnected.splice(0)) r();
  }
}

/** An emulator that records what reached the screen. */
function screen() {
  const s = { text: "", resets: 0, reset() { s.resets += 1; s.text = ""; }, write(d: string) { s.text += d; } };
  return s;
}

const tick = () => new Promise((r) => setTimeout(r, 0));

// ---- reattach --------------------------------------------------------------

describe("following a terminal", () => {
  it("asks from the cursor it was last handed, so nothing is shown twice", async () => {
    const node = new FakeNode();
    const s = screen();
    const stop = new AbortController();
    const done = follow(node, "t1", s, stop.signal, 0);

    node.chunk({ data: "hello ", cursor: 6 });
    await tick();
    expect(node.last().params).toEqual({ id: "t1", cursor: 6 });
    node.chunk({ data: "world", cursor: 11 });
    await tick();

    expect(s.text).toBe("hello world");
    stop.abort();
    expect((await done).kind).toBe("stopped");
  });

  it("survives a network drop and resumes from the same cursor, losing nothing", async () => {
    const node = new FakeNode();
    const s = screen();
    const stop = new AbortController();
    void follow(node, "t1", s, stop.signal, 0);

    node.chunk({ data: "before ", cursor: 7 });
    await tick();
    node.drop();
    await tick();
    const asked = node.calls.length;
    await tick();
    expect(node.calls.length, "it spun on a dead socket").toBe(asked);

    node.reconnect();
    await tick();
    expect(node.last().params).toEqual({ id: "t1", cursor: 7 });
    node.chunk({ data: "after", cursor: 12 });
    await tick();

    expect(s.text).toBe("before after");
    expect(s.resets, "a reconnect is not a reload: the screen is kept").toBe(0);
    stop.abort();
  });

  it("clears the screen before a replay that starts mid-stream", async () => {
    const node = new FakeNode();
    const s = screen();
    s.write("stale screen from before the reload");
    const stop = new AbortController();
    void follow(node, "t1", s, stop.signal, 0);

    node.chunk({ data: "\x1b[0mtail of the ring", cursor: 300_000, reset: true });
    await tick();

    expect(s.text).toBe("\x1b[0mtail of the ring");
    stop.abort();
  });

  it("stops on exit, with the shell's last output already on screen", async () => {
    const node = new FakeNode();
    const s = screen();
    const end = follow(node, "t1", s, new AbortController().signal, 0);

    node.chunk({ data: "bye\r\n", cursor: 5 });
    await tick();
    node.chunk({ cursor: 5, exited: true });

    expect((await end).kind).toBe("exited");
    expect(s.text).toBe("bye\r\n");
  });

  it("stops the read on the node when the panel lets go, rather than abandoning it", async () => {
    const node = new FakeNode();
    const stop = new AbortController();
    const end = follow(node, "t1", screen(), stop.signal, 0);

    stop.abort();

    expect(node.last().signal?.aborted, "the parked read was not cancelled").toBe(true);
    expect((await end).kind).toBe("stopped");
  });

  it("reports a terminal the node no longer has, instead of retrying forever", async () => {
    const node = new FakeNode();
    const end = follow(node, "t1", screen(), new AbortController().signal, 0);

    node.last().answer({ id: "x", status: "error", message: "there is no terminal t1" });

    const result = await end;
    expect(result).toEqual({ kind: "gone", message: "there is no terminal t1" });
  });
});

describe("which terminal a panel shows after a reload", () => {
  const info = (id: string, opened_at: number, exited = false): TerminalInfo => ({
    id,
    label: "shell",
    opened_at,
    exit: exited ? { code: 0, signal: null, at: opened_at + 1 } : null,
  });

  it("is the one it was showing, when the node still has it", () => {
    const list = [info("a", 1), info("b", 2)];
    expect(chooseTerminal(list, "a", new Set())).toBe("a");
  });

  it("is the newest live one nobody else is showing, when it has forgotten", () => {
    const list = [info("a", 1), info("b", 2), info("c", 3, true)];
    expect(chooseTerminal(list, null, new Set(["b"]))).toBe("a");
  });

  it("is a new one when every live terminal is already on screen elsewhere", () => {
    expect(chooseTerminal([info("a", 1)], null, new Set(["a"]))).toBeNull();
    expect(chooseTerminal([], "gone", new Set())).toBeNull();
  });
});

// ---- resize ----------------------------------------------------------------

/** A clock the test moves, standing in for `setTimeout`. */
function timers() {
  let now = 0;
  let queue: Array<{ at: number; fn: () => void; dead: boolean }> = [];
  return {
    schedule(ms: number, fn: () => void) {
      const t = { at: now + ms, fn, dead: false };
      queue.push(t);
      return () => {
        t.dead = true;
      };
    },
    advance(ms: number) {
      now += ms;
      for (const t of queue) {
        if (!t.dead && t.at <= now) {
          t.dead = true;
          t.fn();
        }
      }
      queue = queue.filter((t) => !t.dead);
    },
  };
}

describe("resizing on rotate", () => {
  it("sends the shell one size, the final one, however many frames the rotation took", () => {
    const clock = timers();
    const sent: Array<{ rows: number; cols: number }> = [];
    const resize = createResizeSender((s) => sent.push(s), clock.schedule, 150);

    // A rotation reports a dozen intermediate sizes while the layout settles.
    for (let i = 0; i < 12; i += 1) {
      resize.request({ rows: 40 - i, cols: 80 + i * 4 });
      clock.advance(16);
    }
    resize.request({ rows: 24, cols: 132 });
    clock.advance(150);

    expect(sent).toEqual([{ rows: 24, cols: 132 }]);
  });

  it("does not send a size the shell already has", () => {
    const clock = timers();
    const sent: Array<{ rows: number; cols: number }> = [];
    const resize = createResizeSender((s) => sent.push(s), clock.schedule, 150);
    resize.opened({ rows: 24, cols: 80 });

    // Rotated and rotated back before the layout settled.
    resize.request({ rows: 50, cols: 120 });
    clock.advance(50);
    resize.request({ rows: 24, cols: 80 });
    clock.advance(150);

    expect(sent).toEqual([]);
  });

  it("sends nothing once the panel is gone", () => {
    const clock = timers();
    const sent: unknown[] = [];
    const resize = createResizeSender((s) => sent.push(s), clock.schedule, 150);
    resize.request({ rows: 30, cols: 100 });
    resize.dispose();
    // A last layout frame can arrive while the panel is being torn down.
    resize.request({ rows: 31, cols: 101 });
    clock.advance(1000);
    expect(sent).toEqual([]);
  });
});

describe("keeping 80 columns on a narrow screen", () => {
  it("leaves the font alone when 80 columns already fit", () => {
    expect(fontForColumns({ fittedCols: 120, font: 13, base: 13, min: 8 })).toBe(13);
  });

  it("shrinks the font until 80 columns fit, rather than offering fewer", () => {
    const font = fontForColumns({ fittedCols: 48, font: 13, base: 13, min: 6 });
    // Columns scale inversely with the font.
    expect(Math.floor((48 * 13) / font)).toBeGreaterThanOrEqual(MIN_COLUMNS);
    expect(font).toBeLessThan(13);
  });

  it("grows back to the base size when the screen does", () => {
    // Rotated to landscape at the shrunk size: 140 columns at 8px is 86 at 13.
    expect(fontForColumns({ fittedCols: 140, font: 8, base: 13, min: 6 })).toBe(13);
  });

  it("stops at the smallest legible size instead of going microscopic", () => {
    expect(fontForColumns({ fittedCols: 20, font: 13, base: 13, min: 8 })).toBe(8);
  });
});

// ---- the touch keyboard bar -------------------------------------------------

describe("the touch keyboard bar", () => {
  it("sends what a hardware key would", () => {
    expect(keySequence("esc", { ctrl: false, appCursor: false })).toBe("\x1b");
    expect(keySequence("tab", { ctrl: false, appCursor: false })).toBe("\t");
    expect(keySequence("|", { ctrl: false, appCursor: false })).toBe("|");
    expect(keySequence("~", { ctrl: false, appCursor: false })).toBe("~");
    expect(keySequence("/", { ctrl: false, appCursor: false })).toBe("/");
  });

  it("sends arrows the way the running program asked for them", () => {
    // `less` and `vim` switch the terminal to application cursor mode, and
    // ignore the normal-mode sequences.
    expect(keySequence("up", { ctrl: false, appCursor: false })).toBe("\x1b[A");
    expect(keySequence("up", { ctrl: false, appCursor: true })).toBe("\x1bOA");
    expect(keySequence("left", { ctrl: false, appCursor: false })).toBe("\x1b[D");
  });

  it("sends a Ctrl-arrow when Ctrl is latched, for word movement", () => {
    expect(keySequence("right", { ctrl: true, appCursor: false })).toBe("\x1b[1;5C");
  });

  it("turns the next typed letter into its control character when Ctrl is latched", () => {
    // Ctrl-C is the one that matters most: interrupting a `cargo test` from
    // a phone keyboard that has no Ctrl key.
    expect(applyCtrl("c")).toBe("\x03");
    expect(applyCtrl("C")).toBe("\x03");
    expect(applyCtrl("d")).toBe("\x04");
    expect(applyCtrl("[")).toBe("\x1b");
    expect(applyCtrl(" ")).toBe("\x00");
  });

  it("leaves a paste or a word from autocorrect alone", () => {
    expect(applyCtrl("cargo")).toBe("cargo");
    expect(applyCtrl("5")).toBe("5");
  });
});

// ---- search ----------------------------------------------------------------

describe("searching the scrollback", () => {
  /** Rows as xterm stores them: fixed width, with wrapped rows flagged. */
  function rows(cols: number, ...lines: string[]) {
    const out: Array<{ text: string; wrapped: boolean }> = [];
    for (const line of lines) {
      for (let i = 0; i === 0 || i < line.length; i += cols) {
        out.push({ text: line.slice(i, i + cols), wrapped: i > 0 });
      }
    }
    return out;
  }

  it("finds every occurrence, oldest first, in buffer coordinates", () => {
    const buffer = rows(80, "error: one", "fine", "another error");
    const matches = findMatches(logicalLines(buffer, 80), "error", false);
    expect(matches).toEqual([
      { row: 0, col: 0, length: 5 },
      { row: 2, col: 8, length: 5 },
    ]);
  });

  it("finds a match that the terminal wrapped across two rows", () => {
    const long = `${"x".repeat(78)}needle`;
    const buffer = rows(80, long);
    const matches = findMatches(logicalLines(buffer, 80), "needle", false);
    expect(matches).toEqual([{ row: 0, col: 78, length: 6 }]);
  });

  it("ignores case unless asked not to", () => {
    const lines = logicalLines(rows(80, "Warning", "warning"), 80);
    expect(findMatches(lines, "warning", false)).toHaveLength(2);
    expect(findMatches(lines, "warning", true)).toHaveLength(1);
  });

  it("finds nothing for an empty query", () => {
    expect(findMatches(logicalLines(rows(80, "anything"), 80), "", false)).toEqual([]);
  });

  it("steps through matches and wraps at either end", () => {
    expect(stepMatch(3, null, 1)).toBe(0);
    expect(stepMatch(3, null, -1), "searching backwards starts at the newest").toBe(2);
    expect(stepMatch(3, 2, 1)).toBe(0);
    expect(stepMatch(3, 0, -1)).toBe(2);
    expect(stepMatch(0, null, 1)).toBeNull();
  });
});

// ---- typing ----------------------------------------------------------------

describe("typing", () => {
  /**
   * The node runs every invoke as its own task, so two `terminal.write`s in
   * flight at once can reach the shell in either order. Typing `ls` must not
   * arrive as `sl`.
   */
  it("reaches the node in the order it was typed, one write at a time", async () => {
    const sent: string[] = [];
    const answers: Array<() => void> = [];
    const queue = createWriteQueue((target, data) => {
      sent.push(`${target}:${data}`);
      return new Promise<void>((r) => answers.push(r));
    });

    queue.push("t1", "l");
    queue.push("t1", "s");
    queue.push("t1", "\r");
    expect(sent, "a second write went out before the first was answered").toEqual(["t1:l"]);

    answers.shift()!();
    await tick();
    // What was typed meanwhile goes as one write, still in order.
    expect(sent).toEqual(["t1:l", "t1:s\r"]);
    answers.shift()!();
    await tick();
    expect(sent).toEqual(["t1:l", "t1:s\r"]);
  });

  /**
   * On a slow link the operator types half a command in one tab and taps
   * another before the first write comes back. The rest of what they typed
   * belongs to the first shell; sent to the second it is a half command run
   * somewhere else.
   */
  it("sends what was typed to the terminal it was typed into, never joined across terminals", async () => {
    const sent: string[] = [];
    const answers: Array<() => void> = [];
    const queue = createWriteQueue((target, data) => {
      sent.push(`${target}:${data}`);
      return new Promise<void>((r) => answers.push(r));
    });

    queue.push("t1", "r");
    queue.push("t1", "m -rf build");
    queue.push("t2", "ls");
    queue.push("t2", "\r");
    for (let i = 0; i < 3; i += 1) {
      answers.shift()?.();
      await tick();
    }
    expect(sent).toEqual(["t1:r", "t1:m -rf build", "t2:ls\r"]);
  });

  it("keeps going after a write the node refused", async () => {
    const sent: string[] = [];
    const queue = createWriteQueue(async (_target, data) => {
      sent.push(data);
      if (data === "a") throw new Error("refused");
    });
    queue.push("t1", "a");
    await tick();
    queue.push("t1", "b");
    await tick();
    expect(sent).toEqual(["a", "b"]);
  });
});

// ---- the clipboard on iOS ---------------------------------------------------

describe("copying", () => {
  it("uses the async clipboard where it works", async () => {
    const written: string[] = [];
    const ok = await copyText("ls -la", {
      clipboard: { writeText: async (t) => void written.push(t) },
      fallback: () => false,
    });
    expect(ok).toBe(true);
    expect(written).toEqual(["ls -la"]);
  });

  it("falls back when the async clipboard is missing, as on plain http", async () => {
    // iOS Safari has no `navigator.clipboard` outside a secure context, and
    // a node on the LAN is usually plain http.
    const copied: string[] = [];
    const ok = await copyText("ls -la", {
      clipboard: undefined,
      fallback: (t) => {
        copied.push(t);
        return true;
      },
    });
    expect(ok).toBe(true);
    expect(copied).toEqual(["ls -la"]);
  });

  it("falls back when the async clipboard refuses", async () => {
    const copied: string[] = [];
    const ok = await copyText("x", {
      clipboard: { writeText: () => Promise.reject(new Error("NotAllowedError")) },
      fallback: (t) => {
        copied.push(t);
        return true;
      },
    });
    expect(ok).toBe(true);
    expect(copied).toEqual(["x"]);
  });
});

describe("pasting", () => {
  it("reads the clipboard where the browser allows it", async () => {
    expect(await readClipboard({ readText: async () => "make test" })).toBe("make test");
  });

  it("says it could not, so the panel can offer its own paste box", async () => {
    expect(await readClipboard(undefined)).toBeNull();
    expect(
      await readClipboard({ readText: () => Promise.reject(new Error("NotAllowedError")) }),
    ).toBeNull();
  });
});
