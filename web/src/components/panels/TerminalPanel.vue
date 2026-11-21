<script lang="ts">
/**
 * Terminals a mounted panel on this page is showing, by id, with a count.
 * Module scope because it is about the page, not one panel: it is how a
 * second terminal window knows not to attach to the shell the first one is
 * already showing.
 */
const shownOnPage = new Map<string, number>();

function claim(id: string) {
  shownOnPage.set(id, (shownOnPage.get(id) ?? 0) + 1);
}

function release(id: string) {
  const n = (shownOnPage.get(id) ?? 0) - 1;
  if (n > 0) shownOnPage.set(id, n);
  else shownOnPage.delete(id);
}
</script>

<script setup lang="ts">
import { computed, nextTick, onBeforeUnmount, onMounted, ref, watch } from "vue";
import { FitAddon } from "@xterm/addon-fit";
import { Terminal } from "@xterm/xterm";
import "@xterm/xterm/css/xterm.css";

import { useShell } from "../../shell/context.ts";
import {
  BAR_KEYS,
  MIN_COLUMNS,
  applyCtrl,
  chooseTerminal,
  copyText,
  createResizeSender,
  createWriteQueue,
  execCommandCopy,
  findMatches,
  follow,
  fontForColumns,
  keySequence,
  logicalLines,
  readClipboard,
  stepMatch,
  type BarKey,
  type Match,
  type TerminalInfo,
  type TerminalLink,
} from "../../shell/terminal.ts";
import { resolveToken } from "../../tokens/runtime.ts";
import { windowStorageKey, type ShellWindow } from "../../stores/windows.ts";

/**
 * A real shell, over the command bus.
 *
 * Output does NOT arrive as progress frames. `ProgressSink` drops frames when
 * the client is behind — right for a progress bar, corrupting for a pty,
 * where a dropped chunk is output that never comes back. Instead the server
 * keeps a bounded scrollback ring with a cursor and `terminal.read`
 * long-polls from it (see `follow` in shell/terminal.ts).
 *
 * The panel holds no state a reload would lose. The shell, its pty and its
 * recent output live on the node; on mount the panel asks which terminals
 * its session owns, reattaches to the one it was showing, and the node
 * replays the ring. A network drop is smaller still: the read fails, the
 * panel waits for the bus, and asks again from the same cursor.
 *
 * The terminal is the escape hatch, never the main path (DESIGN.md
 * principle 2). On a phone it is still a real terminal — 80 columns, a
 * keyboard bar for the keys a phone keyboard lacks, and copy and paste that
 * do not depend on a clipboard API iOS withholds on plain http.
 */
const props = defineProps<{ window: ShellWindow; compact?: boolean }>();

const shell = useShell();
const root = ref<HTMLDivElement | null>(null);
const host = ref<HTMLDivElement | null>(null);
const scroller = ref<HTMLDivElement | null>(null);
const searchInput = ref<HTMLInputElement | null>(null);
const pasteArea = ref<HTMLTextAreaElement | null>(null);

const error = ref<string | null>(null);
const terminals = ref<TerminalInfo[]>([]);
const active = ref<string | null>(null);
const exited = ref(false);
const ctrl = ref(false);
/** True when even the smallest font cannot fit 80 columns: scroll sideways. */
const wide = ref(false);
const keyboardInset = ref(0);

const searchOpen = ref(false);
const query = ref("");
const caseSensitive = ref(false);
const matches = ref<Match[]>([]);
const current = ref<number | null>(null);

const pasteOpen = ref(false);
const pasteText = ref("");
const selectOpen = ref(false);
const selectText = ref("");
const notice = ref<string | null>(null);

const coarse = computed(() => shell.viewport.coarsePointer.value);
const reconnecting = computed(() => active.value !== null && shell.bus.detached.value);

/** The panel's normal font size, and the smallest it will shrink to. */
const BASE_FONT = props.compact ? 12 : 13;
const MIN_FONT = 8;
/**
 * Which shell this window shows, kept under the one per-window key the
 * window store hands out, so closing the window clears it. A key of the
 * panel's own would outlive every closed window.
 */
const MEMORY_KEY = windowStorageKey(props.window.id);

let term: Terminal | null = null;
let fit: FitAddon | null = null;
let observer: ResizeObserver | null = null;
let following: AbortController | null = null;
let disposed = false;
const cleanups: Array<() => void> = [];

// ---- the node --------------------------------------------------------------

const link: TerminalLink = {
  call: (command, params, signal) =>
    shell.bus.call(command, params, undefined, undefined, { signal }),
  connected: () => shell.bus.status.value === "open",
  untilConnected(signal) {
    return new Promise<void>((resolve) => {
      if (shell.bus.status.value === "open" || signal.aborted) return resolve();
      const stop = watch(shell.bus.status, (s) => {
        if (s === "open") done();
      });
      const done = () => {
        stop();
        signal.removeEventListener("abort", done);
        resolve();
      };
      signal.addEventListener("abort", done);
    });
  },
};

async function call(command: string, params: Record<string, unknown>) {
  const outcome = await shell.bus.call(command, params);
  if (outcome.status !== "ok") {
    const message = "message" in outcome ? outcome.message : `${command} did not run`;
    error.value = message;
    throw new Error(message);
  }
  return outcome.value;
}

const writes = createWriteQueue(async (id, data) => {
  // A shell the panel has seen exit takes no input; one in another tab is
  // the node's to refuse.
  if (id === active.value && exited.value) return;
  const outcome = await shell.bus.call("terminal.write", { id, data });
  if (outcome.status !== "ok" && "message" in outcome) error.value = outcome.message;
});

/** Into the terminal on screen now — not whichever one is when it is sent. */
function typeInto(data: string) {
  if (active.value) writes.push(active.value, data);
}

/** Typing, from the emulator or the bar. Ctrl, when latched, applies once. */
function send(data: string) {
  if (ctrl.value) {
    data = applyCtrl(data);
    ctrl.value = false;
  }
  typeInto(data);
}

const resize = createResizeSender(
  (size) => {
    const id = active.value;
    if (id) void shell.bus.call("terminal.resize", { id, ...size });
  },
  (ms, fn) => {
    const t = setTimeout(fn, ms);
    return () => clearTimeout(t);
  },
);

// ---- sessions --------------------------------------------------------------

// `globalThis`, not `window`: the accessor itself can throw (blocked site
// data), be missing, or come back empty, and every one of those is handled
// the same way below.
function remembered(): string | null {
  try {
    return globalThis.localStorage.getItem(MEMORY_KEY);
  } catch {
    return null;
  }
}

function remember(id: string | null) {
  try {
    if (id) globalThis.localStorage.setItem(MEMORY_KEY, id);
    else globalThis.localStorage.removeItem(MEMORY_KEY);
  } catch {
    // Per-viewer convenience only: without it a reload attaches to the
    // newest free terminal instead, which is usually the same one.
  }
}

async function refresh() {
  try {
    terminals.value = (await call("terminal.list", {})) as TerminalInfo[];
  } catch {
    // Surfaced by `call`.
  }
}

/** Show `id`: replay what the node holds of it, then follow. */
function attach(id: string) {
  if (!term) return;
  // Already showing it, and still following: a second replay would only
  // flash the screen.
  if (id === active.value && following && !following.signal.aborted && !error.value) {
    term.focus();
    return;
  }
  following?.abort();
  if (active.value) release(active.value);
  active.value = id;
  claim(id);
  remember(id);
  exited.value = false;
  error.value = null;
  closeSearch();
  term.reset();

  const size = terminals.value.find((t) => t.id === id)?.size;
  if (size) resize.opened(size);
  resize.request({ rows: term.rows, cols: term.cols });

  const stop = new AbortController();
  following = stop;
  const screen = {
    reset: () => term?.reset(),
    write: (data: string) => term?.write(data),
  };
  void follow(link, id, screen, stop.signal, 0).then((end) => {
    if (stop.signal.aborted || disposed) return;
    if (end.kind === "exited") {
      exited.value = true;
      term?.write("\r\n\x1b[2m[process exited]\x1b[0m\r\n");
      void refresh();
    } else if (end.kind === "gone") {
      error.value = end.message;
      void refresh();
    }
  });
  term.focus();
}

async function openNew() {
  if (!term) return;
  try {
    const opened = (await call("terminal.open", { rows: term.rows, cols: term.cols })) as {
      id: string;
      rows: number;
      cols: number;
    };
    resize.opened({ rows: opened.rows, cols: opened.cols });
    await refresh();
    attach(String(opened.id));
  } catch {
    // Surfaced by `call`; the template shows it.
  }
}

async function close(id: string) {
  const info = terminals.value.find((t) => t.id === id);
  if (info && info.exit === null) {
    // A running shell may be a two-hour build. `terminal.close` is `write`,
    // as the backend declares it; the question is the operator's to answer,
    // not a risk the panel invents.
    const ok = await shell.ui.confirm({
      command: "terminal.close",
      title: `Close ${labelOf(info)}?`,
      detail: "The shell and everything running in it will be ended.",
      risk: "write",
    });
    if (!ok) return;
  }
  let listed = false;
  try {
    await call("terminal.close", { id });
  } catch {
    // The node forgets an exited shell after a grace period, and the tab
    // strip only learns that on the next list. So the list decides: a shell
    // the node no longer holds is closed, which is what was asked; one it
    // still holds keeps its tab, and the error stays on screen.
    await refresh();
    listed = true;
    if (terminals.value.some((t) => t.id === id)) return;
    error.value = null;
  }
  if (active.value === id) {
    following?.abort();
    release(id);
    active.value = null;
    remember(null);
  }
  if (!listed) await refresh();
  if (!active.value) {
    const next = chooseTerminal(terminals.value, null, new Set(shownOnPage.keys()));
    if (next) attach(next);
    else term?.reset();
  }
}

function labelOf(info: TerminalInfo): string {
  const same = terminals.value.filter((t) => t.label === info.label);
  const n = same.findIndex((t) => t.id === info.id) + 1;
  return same.length > 1 ? `${info.label} ${n}` : info.label;
}

async function boot() {
  if (!shell.bus.detached.value || (await waitForBus())) {
    await refresh();
    if (disposed) return;
    const pick = chooseTerminal(terminals.value, remembered(), new Set(shownOnPage.keys()));
    if (pick) attach(pick);
    else await openNew();
  }
}

/** Wait for a bus that is still connecting when the panel mounts. */
function waitForBus(): Promise<boolean> {
  const stop = new AbortController();
  cleanups.push(() => stop.abort());
  return link.untilConnected(stop.signal).then(() => !stop.signal.aborted);
}

// ---- layout ----------------------------------------------------------------

/**
 * Fit the emulator to the panel, keeping at least 80 columns.
 *
 * Runs on every layout frame — the emulator reflows locally and cheaply —
 * while the shell hears only the settled size (`createResizeSender`), so a
 * rotation is one `SIGWINCH`, not a dozen.
 */
function layout() {
  if (!term || !fit) return;
  const dims = fit.proposeDimensions();
  if (!dims || !Number.isFinite(dims.cols) || dims.cols <= 0) return;
  const font = fontForColumns({
    fittedCols: dims.cols,
    font: term.options.fontSize ?? BASE_FONT,
    base: BASE_FONT,
    min: MIN_FONT,
  });
  if (font !== term.options.fontSize) term.options.fontSize = font;
  fit.fit();
  wide.value = term.cols < MIN_COLUMNS;
  if (wide.value) term.resize(MIN_COLUMNS, term.rows);
  resize.request({ rows: term.rows, cols: term.cols });
}

/**
 * How much of the panel the soft keyboard covers. iOS Safari does not
 * shrink the layout viewport when the keyboard opens, so without this the
 * keyboard bar sits behind the keyboard it exists to complement.
 *
 * Measured against the panel's own box, not the screen: the keyboard's top
 * edge is the bottom of the visual viewport, and the inset is how far the
 * panel reaches below it. A terminal in the top pane of a stacked split is
 * not covered at all when the editor below it opens the keyboard, and
 * padding it by the whole keyboard would crush it to nothing. The padding
 * sits inside the root, so it never moves the box being measured.
 */
let measureKeyboard: () => void = () => {};

function trackKeyboard() {
  const vv = window.visualViewport;
  if (!vv) return;
  const update = () => {
    const box = root.value?.getBoundingClientRect();
    if (!box) {
      keyboardInset.value = 0;
      return;
    }
    const keyboardTop = vv.offsetTop + vv.height;
    const covered = Math.min(box.bottom - keyboardTop, box.height);
    keyboardInset.value = Math.max(0, Math.round(covered));
  };
  measureKeyboard = update;
  vv.addEventListener("resize", update);
  vv.addEventListener("scroll", update);
  cleanups.push(() => {
    vv.removeEventListener("resize", update);
    vv.removeEventListener("scroll", update);
    measureKeyboard = () => {};
  });
  update();
}

// ---- the keyboard bar ------------------------------------------------------

function pressKey(key: BarKey) {
  const appCursor = term?.modes.applicationCursorKeysMode ?? false;
  const data = keySequence(key, { ctrl: ctrl.value, appCursor });
  ctrl.value = false;
  typeInto(data);
  term?.focus();
}

function toggleCtrl() {
  ctrl.value = !ctrl.value;
  term?.focus();
}

// ---- search ----------------------------------------------------------------

function bufferRows() {
  if (!term) return [];
  const buffer = term.buffer.active;
  const rows: Array<{ text: string; wrapped: boolean }> = [];
  for (let i = 0; i < buffer.length; i += 1) {
    const line = buffer.getLine(i);
    if (!line) continue;
    const next = buffer.getLine(i + 1);
    // A row continued below is full width; trimming it would shift every
    // offset after it.
    rows.push({ text: line.translateToString(!next?.isWrapped), wrapped: line.isWrapped });
  }
  return rows;
}

function runSearch() {
  if (!term) return;
  matches.value = findMatches(
    logicalLines(bufferRows(), term.cols),
    query.value,
    caseSensitive.value,
    term.cols,
  );
  current.value = null;
  if (matches.value.length) step(-1);
  else term.clearSelection();
}

function step(direction: 1 | -1) {
  if (!term) return;
  current.value = stepMatch(matches.value.length, current.value, direction);
  if (current.value === null) return;
  const m = matches.value[current.value];
  if (!m) return;
  term.select(m.col, m.row, m.length);
  term.scrollToLine(Math.max(0, m.row - Math.floor(term.rows / 2)));
}

async function openSearch() {
  searchOpen.value = true;
  await nextTick();
  searchInput.value?.focus();
  searchInput.value?.select();
}

function closeSearch() {
  if (!searchOpen.value) return;
  searchOpen.value = false;
  matches.value = [];
  current.value = null;
  term?.clearSelection();
  term?.focus();
}

const matchLabel = computed(() => {
  if (!query.value) return "";
  if (!matches.value.length) return "No matches";
  return `${(current.value ?? 0) + 1} of ${matches.value.length}`;
});

// ---- copy and paste --------------------------------------------------------

function flash(text: string) {
  notice.value = text;
  setTimeout(() => {
    if (notice.value === text) notice.value = null;
  }, 2000);
}

/**
 * Copy the selection, or — on a touch screen, where xterm cannot select —
 * open the scrollback as plain text so the platform's own selection handles
 * and Copy menu work on it.
 */
async function copy() {
  const selected = term?.getSelection() ?? "";
  if (selected) {
    const ok = await copyText(selected, {
      clipboard: navigator.clipboard,
      fallback: (t) => execCommandCopy(document, t),
    });
    flash(ok ? "Copied" : "Copy was blocked by the browser");
    term?.focus();
    return;
  }
  openSelect();
}

function openSelect() {
  if (!term) return;
  const rows = bufferRows();
  // Each row came out of `bufferRows` already trimmed where it ends a line.
  selectText.value = logicalLines(rows, term.cols)
    .map((l) => l.text)
    .join("\n")
    .replace(/\n+$/, "");
  selectOpen.value = true;
}

function closeSelect() {
  selectOpen.value = false;
  term?.focus();
}

async function copyAll() {
  const ok = await copyText(selectText.value, {
    clipboard: navigator.clipboard,
    fallback: (t) => execCommandCopy(document, t),
  });
  flash(ok ? "Copied" : "Copy was blocked: select the text and use Copy");
}

/**
 * Paste. The clipboard API is tried first; where the browser will not give
 * the clipboard up — iOS over plain http, or a declined prompt — a box opens
 * that the ordinary long-press Paste works in.
 */
async function paste() {
  const text = await readClipboard(navigator.clipboard);
  if (text !== null && text !== "") {
    pasteInto(text);
    return;
  }
  pasteText.value = "";
  pasteOpen.value = true;
  await nextTick();
  pasteArea.value?.focus();
}

function pasteInto(text: string) {
  // `paste` rather than a raw write: it wraps the text in bracketed-paste
  // markers when the shell asked for them, so a pasted newline is not taken
  // as Enter on a half-typed command.
  term?.paste(text);
  term?.focus();
}

function sendPaste() {
  const text = pasteText.value;
  pasteOpen.value = false;
  pasteText.value = "";
  if (text) pasteInto(text);
}

// ---- lifecycle -------------------------------------------------------------

/**
 * The emulator draws with the same tokens as the DOM around it, so a terminal
 * does not look like a foreign object dropped into the shell. Read at runtime
 * rather than compiled in, because the theme can change without a reload.
 */
function themeFromTokens(el: Element) {
  return {
    background: resolveToken(el, "color.surface"),
    foreground: resolveToken(el, "color.text"),
    cursor: resolveToken(el, "color.accent"),
    selectionBackground: resolveToken(el, "color.selection"),
  };
}

onMounted(async () => {
  if (!host.value) return;

  term = new Terminal({
    fontFamily: resolveToken(host.value, "font.mono") || "ui-monospace, monospace",
    fontSize: BASE_FONT,
    cursorBlink: true,
    theme: themeFromTokens(host.value),
    // Matches the node's ring (256 KiB is roughly 3,000 lines), so a replay
    // after a reload shows what the node holds and no more.
    scrollback: 3000,
    allowProposedApi: true,
  });
  fit = new FitAddon();
  term.loadAddon(fit);
  term.open(host.value);

  term.onData(send);
  term.attachCustomKeyEventHandler((e) => {
    if (e.type === "keydown" && e.ctrlKey && e.shiftKey && e.key.toLowerCase() === "f") {
      void openSearch();
      return false;
    }
    return true;
  });

  layout();
  // A pane resized under an open keyboard (the split's divider dragged)
  // changes how much of it the keyboard covers, as well as its columns.
  observer = new ResizeObserver(() => {
    layout();
    measureKeyboard();
  });
  if (scroller.value) observer.observe(scroller.value);
  const onRotate = () => layout();
  window.addEventListener("orientationchange", onRotate);
  cleanups.push(() => window.removeEventListener("orientationchange", onRotate));
  trackKeyboard();

  await boot();
});

// `revision` is the controller's own counter for exactly this: anything
// cached off tokens watches it rather than re-resolving every frame.
watch(
  () => shell.theme.revision.value,
  () => {
    if (term && host.value) term.options.theme = themeFromTokens(host.value);
  },
);

watch(query, () => runSearch());
watch(caseSensitive, () => runSearch());

onBeforeUnmount(() => {
  disposed = true;
  following?.abort();
  if (active.value) release(active.value);
  resize.dispose();
  observer?.disconnect();
  for (const c of cleanups.splice(0)) c();
  // The pty is deliberately NOT closed: closing a window should not kill a
  // long-running build. It stays in the session's list, the next terminal
  // window reattaches to it, and the server reaps it when the session ends.
  term?.dispose();
  term = null;
});
</script>

<template>
  <div ref="root" class="terminal" :style="coarse && keyboardInset ? { paddingBottom: `${keyboardInset}px` } : undefined">
    <header class="toolbar">
      <div class="tabs" role="tablist" aria-label="Terminals">
        <div
          v-for="t in terminals"
          :key="t.id"
          class="tab"
          :class="{ active: t.id === active, ended: t.exit !== null }"
        >
          <button
            type="button"
            role="tab"
            class="tab-label"
            :aria-selected="t.id === active"
            :title="t.exit ? `${labelOf(t)} (exited ${t.exit.code})` : labelOf(t)"
            @click="attach(t.id)"
          >
            {{ labelOf(t) }}
          </button>
          <button
            type="button"
            class="tab-close"
            :aria-label="`Close ${labelOf(t)}`"
            @click="close(t.id)"
          >
            ×
          </button>
        </div>
        <button type="button" class="tool" aria-label="New terminal" title="New terminal" @click="openNew">
          +
        </button>
      </div>
      <div class="tools">
        <span v-if="reconnecting" class="state" role="status">Reconnecting…</span>
        <span v-else-if="notice" class="state" role="status">{{ notice }}</span>
        <button type="button" class="tool" aria-label="Search scrollback" title="Search (Ctrl+Shift+F)" @click="openSearch">
          Find
        </button>
        <button type="button" class="tool" aria-label="Copy" title="Copy" @click="copy">Copy</button>
        <button type="button" class="tool" aria-label="Paste" title="Paste" @click="paste">Paste</button>
      </div>
    </header>

    <div v-if="searchOpen" class="search" role="search">
      <input
        ref="searchInput"
        v-model="query"
        type="search"
        class="search-input"
        placeholder="Search scrollback"
        aria-label="Search scrollback"
        autocapitalize="off"
        autocomplete="off"
        spellcheck="false"
        @keydown.enter.prevent="step($event.shiftKey ? 1 : -1)"
        @keydown.esc.prevent="closeSearch"
      />
      <span class="count" aria-live="polite">{{ matchLabel }}</span>
      <button type="button" class="tool" aria-label="Previous match" :disabled="!matches.length" @click="step(-1)">↑</button>
      <button type="button" class="tool" aria-label="Next match" :disabled="!matches.length" @click="step(1)">↓</button>
      <button
        type="button"
        class="tool"
        aria-label="Match case"
        :aria-pressed="caseSensitive"
        @click="caseSensitive = !caseSensitive"
      >
        Aa
      </button>
      <button type="button" class="tool" aria-label="Close search" @click="closeSearch">×</button>
    </div>

    <p v-if="error" class="error" role="alert">{{ error }}</p>

    <div ref="scroller" class="scroller" :class="{ wide }">
      <div ref="host" class="host" />
    </div>

    <div v-if="pasteOpen" class="sheet" role="dialog" aria-label="Paste into the terminal">
      <textarea
        ref="pasteArea"
        v-model="pasteText"
        class="sheet-text"
        placeholder="Long-press here and choose Paste"
        autocapitalize="off"
        autocomplete="off"
        spellcheck="false"
      />
      <div class="sheet-actions">
        <button type="button" class="tool" @click="pasteOpen = false">Cancel</button>
        <button type="button" class="tool primary" :disabled="!pasteText" @click="sendPaste">Send</button>
      </div>
    </div>

    <div v-if="selectOpen" class="sheet" role="dialog" aria-label="Select text to copy">
      <textarea class="sheet-text mono" readonly :value="selectText" aria-label="Terminal text" />
      <div class="sheet-actions">
        <button type="button" class="tool" @click="copyAll">Copy all</button>
        <button type="button" class="tool primary" @click="closeSelect">Done</button>
      </div>
    </div>

    <nav v-if="coarse" class="keybar" aria-label="Terminal keys" @mousedown.prevent @pointerdown.prevent>
      <button type="button" class="key" aria-label="Control" :aria-pressed="ctrl" @click="toggleCtrl">Ctrl</button>
      <button
        v-for="k in BAR_KEYS"
        :key="k.key"
        type="button"
        class="key"
        :aria-label="k.aria"
        @click="pressKey(k.key)"
      >
        {{ k.label }}
      </button>
    </nav>
  </div>
</template>

<style scoped>
.terminal {
  display: flex;
  flex: 1;
  flex-direction: column;
  min-width: 0;
  min-height: 0;
  background: var(--dj-color-surface);
}

.toolbar {
  display: flex;
  align-items: center;
  gap: var(--dj-space-2);
  padding: var(--dj-space-1) var(--dj-space-2);
  border-bottom: var(--dj-stroke-hairline) solid var(--dj-color-border);
  background: var(--dj-color-sunken);
}

.tabs {
  display: flex;
  flex: 1;
  align-items: center;
  gap: var(--dj-space-1);
  min-width: 0;
  overflow-x: auto;
  scrollbar-width: none;
}

.tab {
  display: flex;
  flex: none;
  align-items: center;
  border-radius: var(--dj-radius-sm);
  color: var(--dj-color-text-muted);
}

.tab.active {
  background: var(--dj-color-surface);
  color: var(--dj-color-text);
}

.tab.ended .tab-label {
  text-decoration: line-through;
  color: var(--dj-color-text-dim);
}

.tab-label,
.tab-close,
.tool,
.key {
  border: 0;
  background: transparent;
  color: inherit;
  font: inherit;
  font-size: var(--dj-type-small-size);
  cursor: pointer;
}

.tab-label {
  padding: var(--dj-space-1) var(--dj-space-2);
  font-family: var(--dj-font-mono);
}

.tab-close {
  padding: var(--dj-space-1);
  color: var(--dj-color-text-dim);
}

.tools {
  display: flex;
  flex: none;
  align-items: center;
  gap: var(--dj-space-1);
  color: var(--dj-color-text-muted);
}

.tool {
  min-width: var(--dj-control);
  padding: var(--dj-space-1) var(--dj-space-2);
  border-radius: var(--dj-radius-sm);
}

.tool:hover,
.tab-label:hover,
.tab-close:hover {
  color: var(--dj-color-text);
}

.tool:disabled {
  opacity: 0.4;
  cursor: default;
}

.tool[aria-pressed="true"],
.key[aria-pressed="true"] {
  background: var(--dj-color-accent);
  color: var(--dj-color-text-on-accent);
}

.tool.primary {
  background: var(--dj-color-accent);
  color: var(--dj-color-text-on-accent);
}

.state {
  font-size: var(--dj-type-micro-size);
  color: var(--dj-color-warn);
}

.search {
  display: flex;
  align-items: center;
  gap: var(--dj-space-1);
  padding: var(--dj-space-1) var(--dj-space-2);
  border-bottom: var(--dj-stroke-hairline) solid var(--dj-color-border);
}

.search-input {
  flex: 1;
  min-width: 0;
  padding: var(--dj-space-1) var(--dj-space-2);
  border: var(--dj-stroke-hairline) solid var(--dj-color-border);
  border-radius: var(--dj-radius-sm);
  background: var(--dj-color-surface);
  color: var(--dj-color-text);
  /* 16px stops iOS zooming the page when the field takes focus. */
  font-size: max(16px, var(--dj-type-small-size));
}

.count {
  flex: none;
  font-size: var(--dj-type-micro-size);
  color: var(--dj-color-text-muted);
}

.error {
  margin: 0;
  padding: var(--dj-space-2) var(--dj-space-3);
  background: var(--dj-color-danger-soft);
  color: var(--dj-color-danger);
  font-size: var(--dj-type-small-size);
}

.scroller {
  display: flex;
  flex: 1;
  min-height: 0;
  overflow: hidden;
}

/* Even the smallest font holds fewer than 80 columns: keep 80 and let the
 * operator scroll sideways, rather than wrapping every line a program
 * formatted for 80. */
.scroller.wide {
  overflow-x: auto;
}

.host {
  flex: 1;
  min-width: 0;
  min-height: 0;
  padding: var(--dj-space-2);
}

.sheet {
  display: flex;
  flex-direction: column;
  gap: var(--dj-space-2);
  padding: var(--dj-space-2);
  border-top: var(--dj-stroke-hairline) solid var(--dj-color-border);
  background: var(--dj-color-raised);
}

.sheet-text {
  min-height: 6rem;
  max-height: 40vh;
  padding: var(--dj-space-2);
  border: var(--dj-stroke-hairline) solid var(--dj-color-border);
  border-radius: var(--dj-radius-sm);
  background: var(--dj-color-surface);
  color: var(--dj-color-text);
  font-size: 16px;
  resize: vertical;
}

.sheet-text.mono {
  font-family: var(--dj-font-mono);
  font-size: max(16px, var(--dj-type-small-size));
  white-space: pre;
}

.sheet-actions {
  display: flex;
  justify-content: flex-end;
  gap: var(--dj-space-2);
}

.keybar {
  display: flex;
  flex: none;
  gap: var(--dj-space-1);
  padding: var(--dj-space-1);
  overflow-x: auto;
  border-top: var(--dj-stroke-hairline) solid var(--dj-color-border);
  background: var(--dj-color-sunken);
  scrollbar-width: none;
}

.key {
  flex: 1 0 auto;
  min-width: var(--dj-touch);
  min-height: var(--dj-touch);
  padding: 0 var(--dj-space-2);
  border-radius: var(--dj-radius-sm);
  background: var(--dj-color-raised);
  color: var(--dj-color-text);
  font-family: var(--dj-font-mono);
  touch-action: manipulation;
}
</style>
