/**
 * Everything about the terminal panel that makes a promise.
 *
 * The panel is xterm.js and some wiring. What it owes the operator lives
 * here, where it can be asked without a browser: a reload replays the
 * screen, a network drop loses nothing, a rotate sends the shell one size,
 * the touch bar sends what a hardware key would, and copy and paste work on
 * a phone that has neither a Ctrl key nor, on plain http, a clipboard API.
 *
 * The terminal is the escape hatch, not the main path (DESIGN.md principle
 * 2). Nothing here adds a way to do ordinary work through it; it only makes
 * the hatch usable from the phone when it is needed.
 */
import type { CallResult } from "../protocol.ts";

/** Below this a terminal is a different tool; the panel shrinks its font
 * before it offers fewer columns, and scrolls before it shrinks past legible. */
export const MIN_COLUMNS = 80;

/** How long the layout must hold still before the shell is told its size. */
export const RESIZE_SETTLE_MS = 150;

/** One `terminal.read` answer. */
export interface Chunk {
  data: string;
  cursor: number;
  exited: boolean;
  /** The replay starts after bytes the node no longer holds: clear first. */
  reset: boolean;
}

/** One entry of `terminal.list`, as much of it as the panel reads. */
export interface TerminalInfo {
  id: string;
  label: string;
  opened_at: number;
  exit: { code: number; signal: string | null; at: number } | null;
  /** The size the pty has now, so a reattach resizes it only if it must. */
  size?: { rows: number; cols: number };
}

/** The node, as the follow loop needs it. */
export interface TerminalLink {
  call(command: string, params: Record<string, unknown>, signal?: AbortSignal): Promise<CallResult>;
  /** Whether the bus is connected right now. */
  connected(): boolean;
  /** Resolves once the bus is connected again, or when `signal` aborts. */
  untilConnected(signal: AbortSignal): Promise<void>;
}

/** The emulator, as the follow loop needs it. */
export interface TerminalScreen {
  reset(): void;
  write(data: string): void;
}

export type FollowEnd =
  | { kind: "exited" }
  | { kind: "stopped" }
  | { kind: "gone"; message: string };

// ---------------------------------------------------------------------------
// Following and reattaching
// ---------------------------------------------------------------------------

/**
 * Long-poll a terminal's output onto `screen` until it exits, the caller
 * stops, or the node says the terminal is gone.
 *
 * The cursor is the whole client-side state. A network drop fails the
 * waiting read; the loop waits for the bus to come back and asks again from
 * the same cursor, so nothing is lost and nothing is shown twice. A reload
 * starts over from `from = 0` and the node replays its ring; when that ring
 * has already dropped the start of the stream, the answer says `reset` and
 * the screen is cleared before the replay is written, because the first
 * byte may be in the middle of an escape sequence.
 *
 * Stopping cancels the parked read on the node rather than abandoning it:
 * an abandoned read holds one of the socket's in-flight slots until the
 * shell next prints, which for a quiet shell is never.
 */
export async function follow(
  link: TerminalLink,
  id: string,
  screen: TerminalScreen,
  signal: AbortSignal,
  from: number,
): Promise<FollowEnd> {
  let cursor = from;
  for (;;) {
    if (signal.aborted) return { kind: "stopped" };
    const outcome = await link.call("terminal.read", { id, cursor }, signal);
    if (signal.aborted) return { kind: "stopped" };

    if (outcome.status !== "ok") {
      if (!link.connected()) {
        await link.untilConnected(signal);
        continue;
      }
      const message = "message" in outcome ? outcome.message : "terminal.read did not run";
      return { kind: "gone", message };
    }

    const chunk = outcome.value as Chunk;
    if (chunk.reset) screen.reset();
    if (chunk.data) screen.write(chunk.data);
    cursor = chunk.cursor;
    if (chunk.exited) return { kind: "exited" };
  }
}

/**
 * Which terminal a panel should show when it mounts.
 *
 * The one it was showing, if the node still has it — that is a reload. Else
 * the newest live terminal no other panel on this page is showing, so
 * closing a terminal window and opening another gets the long build back
 * rather than a fresh shell beside it. Else `null`: open a new one.
 */
export function chooseTerminal(
  list: readonly TerminalInfo[],
  remembered: string | null,
  shownElsewhere: ReadonlySet<string>,
): string | null {
  if (remembered && list.some((t) => t.id === remembered)) return remembered;
  const free = list.filter((t) => t.exit === null && !shownElsewhere.has(t.id));
  const newest = free.reduce<TerminalInfo | null>(
    (best, t) => (best === null || t.opened_at >= best.opened_at ? t : best),
    null,
  );
  return newest?.id ?? null;
}

// ---------------------------------------------------------------------------
// Typing
// ---------------------------------------------------------------------------

export interface WriteQueue {
  /** `data` was typed into terminal `target`. */
  push(target: string, data: string): void;
}

/**
 * Keystrokes to the node, strictly in order.
 *
 * The node runs every invoke as its own task, so only one write is ever
 * in flight at a time; whatever is typed while it is out goes as the next
 * write, concatenated, which keeps the order and costs fewer round trips on
 * a slow link. A refused write is not retried — the panel has already been
 * told — and does not stop the ones after it.
 *
 * Each keystroke carries the terminal it was typed into, fixed when it is
 * pushed. Typing in one tab and switching to another before the write comes
 * back must not send the rest of a half-typed command to the other shell;
 * input for different terminals is never joined into one write.
 */
export function createWriteQueue(
  send: (target: string, data: string) => Promise<unknown>,
): WriteQueue {
  const buffered: Array<{ target: string; data: string }> = [];
  let busy = false;

  const pump = async () => {
    busy = true;
    while (buffered.length) {
      const { target, data } = buffered.shift()!;
      try {
        await send(target, data);
      } catch {
        // Reported by `send`; the next keystroke still goes.
      }
    }
    busy = false;
  };

  return {
    push(target, data) {
      if (!data) return;
      const last = buffered.at(-1);
      if (last && last.target === target) last.data += data;
      else buffered.push({ target, data });
      if (!busy) void pump();
    },
  };
}

// ---------------------------------------------------------------------------
// Resize
// ---------------------------------------------------------------------------

export interface GridSize {
  rows: number;
  cols: number;
}

export interface ResizeSender {
  /** The layout changed to this size. Sent once it has held still. */
  request(size: GridSize): void;
  /** The pty was opened at this size; it does not need to be told again. */
  opened(size: GridSize): void;
  dispose(): void;
}

/**
 * Tell the shell its size once the layout settles.
 *
 * Every size change is a `SIGWINCH`, and a full-screen program redraws on
 * each one. A phone rotating reports a dozen intermediate sizes in a few
 * hundred milliseconds; sending each one makes `vim` or `htop` repaint a
 * dozen times at sizes that were never on screen, and the reflowed output
 * that results is what "the terminal garbled itself when I turned the
 * phone" looks like. So the emulator reflows locally on every frame, and the
 * shell hears only the final size, and only if it differs from the last one
 * it was told.
 */
export function createResizeSender(
  send: (size: GridSize) => void,
  schedule: (ms: number, fn: () => void) => () => void,
  settleMs = RESIZE_SETTLE_MS,
): ResizeSender {
  let last: GridSize | null = null;
  let want: GridSize | null = null;
  let cancel: (() => void) | null = null;
  let dead = false;

  const fire = () => {
    cancel = null;
    if (dead || !want) return;
    if (last && last.rows === want.rows && last.cols === want.cols) return;
    last = want;
    send(want);
  };

  return {
    request(size) {
      if (dead) return;
      want = { rows: size.rows, cols: size.cols };
      cancel?.();
      cancel = schedule(settleMs, fire);
    },
    opened(size) {
      last = { rows: size.rows, cols: size.cols };
    },
    dispose() {
      dead = true;
      cancel?.();
      cancel = null;
    },
  };
}

/**
 * The font size that fits at least [`MIN_COLUMNS`] across.
 *
 * `fittedCols` is how many columns the panel's width holds at `font`.
 * Columns scale inversely with the font, so the size that fits is
 * proportional; it is rounded down to a half pixel so it never comes out a
 * hair too large. Never above `base` (the panel's normal size, which it
 * grows back to when the screen does) and never below `min`, past which the
 * panel keeps 80 columns and scrolls sideways instead.
 */
export function fontForColumns(input: {
  fittedCols: number;
  font: number;
  base: number;
  min: number;
}): number {
  const { fittedCols, font, base, min } = input;
  if (fittedCols <= 0) return font;
  const fits = Math.floor(((fittedCols * font) / MIN_COLUMNS) * 2) / 2;
  return Math.min(base, Math.max(min, fits));
}

// ---------------------------------------------------------------------------
// The touch keyboard bar
// ---------------------------------------------------------------------------

export type BarKey = "esc" | "tab" | "up" | "down" | "left" | "right" | "|" | "~" | "/";

/** In bar order. Ctrl is not here: it is a latch, not a key. */
export const BAR_KEYS: ReadonlyArray<{ key: BarKey; label: string; aria: string }> = [
  { key: "esc", label: "Esc", aria: "Escape" },
  { key: "tab", label: "Tab", aria: "Tab" },
  { key: "left", label: "←", aria: "Left arrow" },
  { key: "up", label: "↑", aria: "Up arrow" },
  { key: "down", label: "↓", aria: "Down arrow" },
  { key: "right", label: "→", aria: "Right arrow" },
  { key: "|", label: "|", aria: "Pipe" },
  { key: "~", label: "~", aria: "Tilde" },
  { key: "/", label: "/", aria: "Slash" },
];

const ARROW: Record<"up" | "down" | "right" | "left", string> = {
  up: "A",
  down: "B",
  right: "C",
  left: "D",
};

/**
 * The bytes a hardware key would send.
 *
 * Arrows follow the cursor-key mode the running program set: `less`, `vim`
 * and `htop` switch to application mode (`ESC O A`) and ignore the
 * normal-mode `ESC [ A`. With Ctrl latched an arrow is the xterm modifier
 * form, which shells bind to word movement.
 */
export function keySequence(key: BarKey, mode: { ctrl: boolean; appCursor: boolean }): string {
  switch (key) {
    case "esc":
      return "\x1b";
    case "tab":
      return "\t";
    case "up":
    case "down":
    case "left":
    case "right": {
      const letter = ARROW[key];
      if (mode.ctrl) return `\x1b[1;5${letter}`;
      return mode.appCursor ? `\x1bO${letter}` : `\x1b[${letter}`;
    }
    default:
      return mode.ctrl ? applyCtrl(key) : key;
  }
}

/**
 * What the latched Ctrl does to the next thing typed.
 *
 * A single letter or one of `@[\]^_` becomes its control character, the way
 * a hardware Ctrl does; space is NUL and `?` is DEL. Anything else — a
 * digit, a paste, a word a phone keyboard autocompleted in one go — is
 * passed through, because guessing which letter of a word was meant would
 * send a control character nobody asked for.
 */
export function applyCtrl(data: string): string {
  if (data.length !== 1) return data;
  if (data === " ") return "\x00";
  if (data === "?") return "\x7f";
  const upper = data.toUpperCase();
  const code = upper.charCodeAt(0);
  if (code >= 0x40 && code <= 0x5f) return String.fromCharCode(code - 0x40);
  return data;
}

// ---------------------------------------------------------------------------
// Search
// ---------------------------------------------------------------------------

/** One logical line: rows the terminal wrapped, joined back together. */
export interface LogicalLine {
  text: string;
  /** Buffer row the line starts on. */
  row: number;
}

export interface Match {
  row: number;
  col: number;
  length: number;
}

/**
 * Join wrapped rows back into the lines the program printed, so a match the
 * terminal happened to break across two rows is still found.
 *
 * `rows` is the buffer top to bottom, each with whether xterm marked it as a
 * continuation of the row above. A row that is continued below is full
 * width by definition, which is what lets an offset in the joined text be
 * mapped back to a row and a column by division. Wide (CJK) characters
 * occupy two cells and break that mapping; a match after one lands a cell
 * early, which is the known limit of this search.
 */
export function logicalLines(
  rows: ReadonlyArray<{ text: string; wrapped: boolean }>,
  cols: number,
): LogicalLine[] {
  const out: LogicalLine[] = [];
  rows.forEach((r, i) => {
    const prev = out.at(-1);
    if (r.wrapped && prev) {
      prev.text = prev.text.padEnd(Math.ceil(prev.text.length / cols) * cols, " ") + r.text;
    } else {
      out.push({ text: r.text, row: i });
    }
  });
  return out;
}

/** Every occurrence of `query`, oldest first, in buffer coordinates. */
export function findMatches(
  lines: readonly LogicalLine[],
  query: string,
  caseSensitive: boolean,
  cols = 80,
): Match[] {
  if (!query) return [];
  const needle = caseSensitive ? query : query.toLowerCase();
  const out: Match[] = [];
  for (const line of lines) {
    const hay = caseSensitive ? line.text : line.text.toLowerCase();
    let at = hay.indexOf(needle);
    while (at !== -1) {
      out.push({ row: line.row + Math.floor(at / cols), col: at % cols, length: query.length });
      at = hay.indexOf(needle, at + Math.max(1, needle.length));
    }
  }
  return out;
}

/**
 * The next match index in `direction`, wrapping at either end. With nothing
 * selected yet, forward starts at the oldest and backward at the newest —
 * backward is the useful one in a terminal, where what you want is usually
 * the most recent error.
 */
export function stepMatch(count: number, current: number | null, direction: 1 | -1): number | null {
  if (count === 0) return null;
  if (current === null) return direction === 1 ? 0 : count - 1;
  return (current + direction + count) % count;
}

// ---------------------------------------------------------------------------
// Clipboard
// ---------------------------------------------------------------------------

export interface CopyEnv {
  /** `navigator.clipboard`, which is absent outside a secure context. */
  clipboard: { writeText(text: string): Promise<void> } | undefined;
  /** The old way: a selected textarea and `execCommand("copy")`. */
  fallback(text: string): boolean;
}

/**
 * Put `text` on the system clipboard.
 *
 * The async Clipboard API is tried first and is not relied on: iOS Safari
 * has none on plain http, which is how a node on the LAN is usually
 * reached, and refuses it outside a user gesture. The `execCommand`
 * fallback works in both cases as long as it runs inside the tap.
 */
export async function copyText(text: string, env: CopyEnv): Promise<boolean> {
  if (env.clipboard) {
    try {
      await env.clipboard.writeText(text);
      return true;
    } catch {
      // Fall through to the textarea.
    }
  }
  return env.fallback(text);
}

/**
 * Read the system clipboard, or `null` when the browser will not say.
 *
 * `null` is not an error. On iOS Safari over http there is no API to ask,
 * and where there is one the operator may decline the prompt; either way
 * the panel opens its paste box, where the ordinary long-press Paste works.
 */
export async function readClipboard(
  clipboard: { readText(): Promise<string> } | undefined,
): Promise<string | null> {
  if (!clipboard) return null;
  try {
    return await clipboard.readText();
  } catch {
    return null;
  }
}

/** `copyText`'s fallback, for a real document. Must run inside the tap. */
export function execCommandCopy(doc: Document, text: string): boolean {
  const area = doc.createElement("textarea");
  area.value = text;
  area.setAttribute("readonly", "");
  // Off screen but not `display: none`, which cannot be selected; and at
  // 16px, so iOS does not zoom the page to focus it.
  area.style.cssText = "position:fixed;top:0;left:-9999px;font-size:16px;opacity:0";
  doc.body.appendChild(area);
  area.select();
  area.setSelectionRange(0, text.length);
  let ok = false;
  try {
    ok = doc.execCommand("copy");
  } catch {
    ok = false;
  }
  area.remove();
  return ok;
}
