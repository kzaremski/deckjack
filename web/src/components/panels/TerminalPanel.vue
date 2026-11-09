<script setup lang="ts">
import { onBeforeUnmount, onMounted, ref, watch } from "vue";
import { FitAddon } from "@xterm/addon-fit";
import { Terminal } from "@xterm/xterm";
import "@xterm/xterm/css/xterm.css";

import { useShell } from "../../shell/context.ts";
import { resolveToken } from "../../tokens/runtime.ts";
import type { ShellWindow } from "../../stores/windows.ts";

/**
 * A real shell, over the command bus.
 *
 * Output does NOT arrive as progress frames. `ProgressSink` drops frames when
 * the client is behind — right for a progress bar, corrupting for a pty,
 * where a dropped chunk is output that never comes back. Instead the server
 * keeps a scrollback with a cursor and `terminal.read` long-polls from it:
 * the call parks until there is more, returns it with a new cursor, and we
 * immediately ask again. A lost response costs nothing, because the next
 * read starts from the same cursor.
 *
 * That cursor is also the entire client-side state, which is what makes
 * reattach after a reload work: the server still holds the session and the
 * scrollback, so a fresh panel replays from 0 and catches up.
 */
const props = defineProps<{ window: ShellWindow; compact?: boolean }>();

const shell = useShell();
const host = ref<HTMLDivElement | null>(null);
const error = ref<string | null>(null);

let term: Terminal | null = null;
let fit: FitAddon | null = null;
let observer: ResizeObserver | null = null;
let terminalId: string | null = null;
let cursor = 0;
let running = false;

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

async function call(command: string, params: Record<string, unknown>) {
  const outcome = await shell.bus.call(command, params);
  if (outcome.status !== "ok") {
    const message = "message" in outcome ? outcome.message : `${command} did not run`;
    error.value = message;
    throw new Error(message);
  }
  return outcome.value as Record<string, unknown>;
}

/** Long-poll output until the panel goes away or the shell exits. */
async function pump() {
  while (running && terminalId) {
    let chunk: { data: string; cursor: number; exited: boolean };
    try {
      chunk = (await call("terminal.read", {
        id: terminalId,
        cursor,
      })) as unknown as { data: string; cursor: number; exited: boolean };
    } catch {
      // `call` has already surfaced it. Stop rather than spin on a dead
      // socket; the bus reconnects and the panel can be reopened.
      return;
    }
    if (!running) return;
    if (chunk.data) term?.write(chunk.data);
    cursor = chunk.cursor;
    if (chunk.exited) {
      term?.write("\r\n\x1b[2m[process exited]\x1b[0m\r\n");
      running = false;
      return;
    }
  }
}

function sendSize() {
  if (!term || !terminalId) return;
  void shell.bus.call("terminal.resize", {
    id: terminalId,
    rows: term.rows,
    cols: term.cols,
  });
}

onMounted(async () => {
  if (!host.value) return;

  term = new Terminal({
    fontFamily: resolveToken(host.value, "font.mono") || "ui-monospace, monospace",
    fontSize: props.compact ? 12 : 13,
    cursorBlink: true,
    theme: themeFromTokens(host.value),
    // The server holds scrollback; keeping a second copy here would only
    // disagree with it after a reattach.
    scrollback: 2000,
    allowProposedApi: true,
  });
  fit = new FitAddon();
  term.loadAddon(fit);
  term.open(host.value);
  fit.fit();

  term.onData((data) => {
    if (!terminalId) return;
    void shell.bus.call("terminal.write", { id: terminalId, data });
  });

  observer = new ResizeObserver(() => {
    fit?.fit();
    sendSize();
  });
  observer.observe(host.value);

  try {
    const opened = await call("terminal.open", {
      rows: term.rows,
      cols: term.cols,
    });
    terminalId = String(opened.id);
    cursor = Number(opened.cursor ?? 0);
    running = true;
    term.focus();
    void pump();
  } catch {
    // Surfaced by `call`; the template shows it.
  }
});

// `revision` is the controller's own counter for exactly this: anything
// cached off tokens watches it rather than re-resolving every frame.
watch(
  () => shell.theme.revision.value,
  () => {
    if (term && host.value) term.options.theme = themeFromTokens(host.value);
  },
);

onBeforeUnmount(() => {
  running = false;
  observer?.disconnect();
  // The pty is deliberately NOT closed: closing a window should not kill a
  // long-running build. The server reaps it when the session ends.
  term?.dispose();
  term = null;
});
</script>

<template>
  <div class="terminal">
    <p v-if="error" class="error" role="alert">{{ error }}</p>
    <div ref="host" class="host" />
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

.host {
  flex: 1;
  min-height: 0;
  padding: var(--dj-space-2);
}

.error {
  margin: 0;
  padding: var(--dj-space-2) var(--dj-space-3);
  background: var(--dj-color-danger-soft);
  color: var(--dj-color-text-on-danger);
  font-size: var(--dj-text-sm);
}
</style>
