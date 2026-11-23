<script setup lang="ts">
import { computed, nextTick, onBeforeUnmount, onMounted, ref, watch } from "vue";

import { useShell } from "../../shell/context.ts";
import {
  CLOSE_SEAT,
  META_CODES,
  PictureDecoder,
  buttonChanges,
  evdevFor,
  fit,
  parsePicture,
  releasedWithMeta,
  streamUrl,
  toWindowPoint,
  toWindowPointClamped,
  wheelPixels,
  type StreamInput,
  type StreamMessage,
} from "../../shell/stream.ts";
import type { ShellWindow } from "../../stores/windows.ts";

/**
 * One window of a streamed native app (docs/lanes/8-stream.md).
 *
 * A shell panel, never an app frame: decision D6 of
 * docs/APP-UI-ISOLATION.md refused open-ended duplex streams into app
 * frames. It opens its own socket (`/stream?window=<id>`), which the core
 * admits only for the session holding the seat; pictures arrive as H.264
 * and are decoded with WebCodecs into the canvas, and pointer, wheel and
 * keys go back as small JSON messages in the app's logical pixels.
 *
 * In compact mode a popup (a menu) is not a window of its own: the panel
 * of the window it belongs to draws it, over itself, where the app put it.
 */
const props = defineProps<{ window: ShellWindow; compact?: boolean; nested?: boolean }>();

const shell = useShell();
const canvas = ref<HTMLCanvasElement | null>(null);
const box = ref<HTMLElement | null>(null);
const picture = ref({ width: 0, height: 0 });
const status = ref<"connecting" | "live" | "seat" | "gone" | "unsupported">("connecting");

const meta = computed(() => props.window.stream ?? null);
const isPopup = computed(() => meta.value?.kind === "popup");

/** Popups of this window, drawn here in compact mode. */
const popups = computed(() =>
  props.compact && !props.nested
    ? shell.windows.all.value.filter((w) => w.stream?.kind === "popup" && w.stream.parent === props.window.id)
    : [],
);

function popupStyle(popup: ShellWindow) {
  const el = box.value;
  const p = popup.stream;
  if (!el || !p || picture.value.width === 0) return { display: "none" };
  const at = fit({ left: 0, top: 0, width: el.clientWidth, height: el.clientHeight }, picture.value);
  return {
    left: `${at.x + p.x * at.scale}px`,
    top: `${at.y + p.y * at.scale}px`,
    width: `${p.width * at.scale}px`,
    height: `${p.height * at.scale}px`,
  };
}

let socket: WebSocket | null = null;
let retry: ReturnType<typeof setTimeout> | null = null;
let resizeTimer: ReturnType<typeof setTimeout> | null = null;
let observer: ResizeObserver | null = null;
let alive = true;
/** Keys this panel has told the app are down. */
const pressed = new Set<number>();
/** The `buttons` mask the app has been told about, and where. */
let buttons = 0;
let lastPoint: { x: number; y: number } | null = null;

/** Whether this browser can decode a configuration; a browser that has
 * WebCodecs but not H.264 (or not this level) says so here, rather than by
 * failing every picture. */
async function supported(config: VideoDecoderConfig): Promise<boolean> {
  try {
    return (await VideoDecoder.isConfigSupported(config)).supported === true;
  } catch {
    return false;
  }
}

const decoder = new PictureDecoder(
  (frame, size) => {
    const c = canvas.value;
    if (c) {
      if (c.width !== size.width) c.width = size.width;
      if (c.height !== size.height) c.height = size.height;
      c.getContext("2d")?.drawImage(frame, 0, 0);
    }
    frame.close();
    picture.value = size;
    status.value = "live";
  },
  () => send({ type: "keyframe" }),
  undefined,
  {
    supported,
    onFatal: () => {
      status.value = "unsupported";
    },
  },
);

function send(message: StreamMessage) {
  if (socket?.readyState === WebSocket.OPEN) socket.send(JSON.stringify(message));
}

function input(i: StreamInput) {
  send({ type: "input", input: i });
}

function connect() {
  if (!alive) return;
  if (typeof VideoDecoder !== "function" || decoder.failed) {
    status.value = "unsupported";
    return;
  }
  status.value = status.value === "live" ? "live" : "connecting";
  decoder.reset();
  const ws = new WebSocket(streamUrl(window.location, props.window.id));
  ws.binaryType = "arraybuffer";
  ws.onmessage = (event) => {
    if (!(event.data instanceof ArrayBuffer)) return;
    const p = parsePicture(event.data);
    if (p) decoder.push(p);
  };
  ws.onopen = () => {
    sendSize();
    if (shell.windows.focusedId.value === props.window.id) input({ type: "focus" });
  };
  ws.onclose = (event) => {
    // A socket closed on purpose (the panel moved to another window, or
    // went) is not reconnected; only the current one is.
    if (socket !== ws) return;
    socket = null;
    if (!alive) return;
    if (event.code === CLOSE_SEAT) {
      // Another device has the workspace; this panel is about to be
      // replaced by the takeover screen, or will be remounted on return.
      status.value = "seat";
      return;
    }
    // The window still listed means the app is still there: reconnect.
    status.value = "connecting";
    retry = setTimeout(connect, 1000);
  };
  socket = ws;
}

/** Ask the app for the panel's size (a toplevel only: a popup's size is
 * the app's choice). CSS pixels are the app's logical pixels at scale 1. */
function sendSize() {
  const el = box.value;
  if (!el || isPopup.value || props.nested) return;
  const width = Math.round(el.clientWidth);
  const height = Math.round(el.clientHeight);
  if (width > 0 && height > 0) send({ type: "resize", width, height });
}

/** Where the pointer is in the app's window: `null` outside the picture,
 * unless a button is held, when it is held to the picture's edge. */
function point(event: { clientX: number; clientY: number }, held = buttons !== 0) {
  const c = canvas.value;
  if (!c || picture.value.width === 0) return null;
  const r = c.getBoundingClientRect();
  return held
    ? toWindowPointClamped(event.clientX, event.clientY, r, picture.value)
    : toWindowPoint(event.clientX, event.clientY, r, picture.value);
}

let moveQueued: { x: number; y: number } | null = null;

/** Send every press and release between the mask the app knows and
 * `next`, at `at`. A chorded button changes the mask on a pointermove. */
function sendButtons(next: number, at: { x: number; y: number }) {
  const changes = buttonChanges(buttons, next);
  if (changes.length === 0) return;
  // A queued move is older than this press or release; sent after it, it
  // would drag the pointer back to where it was.
  moveQueued = null;
  for (const change of changes) input({ type: "pointer-button", ...at, ...change });
  buttons = next;
}

function onPointerMove(event: PointerEvent) {
  const at = point(event, buttons !== 0 || event.buttons !== 0);
  if (!at) return;
  lastPoint = at;
  if (event.buttons !== buttons) sendButtons(event.buttons, at);
  // One move per animation frame: a 1000 Hz mouse would otherwise send a
  // thousand messages a second for a picture that updates sixty times.
  if (!moveQueued) {
    requestAnimationFrame(() => {
      if (moveQueued) input({ type: "pointer-move", ...moveQueued });
      moveQueued = null;
    });
  }
  moveQueued = at;
}

function onPointerButton(event: PointerEvent, isPressed: boolean) {
  // A release is held to the picture, never dropped: it may land past the
  // window's edge (the pointer is captured) or in the letterbox.
  const at = point(event, !isPressed || buttons !== 0);
  if (!at) return;
  lastPoint = at;
  if (isPressed) {
    canvas.value?.focus({ preventScroll: true });
    canvas.value?.setPointerCapture(event.pointerId);
    // A popup drawn over its parent (compact) is not a window of its own
    // to focus; the store sends any other popup's focus to its parent.
    if (!props.nested) shell.windows.focus(props.window.id);
  }
  sendButtons(event.buttons, at);
  event.preventDefault();
}

/** The browser took the pointer away (a cancelled touch, lost capture):
 * whatever it held is released where it last was. */
function onPointerLost() {
  if (buttons !== 0 && lastPoint) sendButtons(0, lastPoint);
  buttons = 0;
}

function onWheel(event: WheelEvent) {
  const at = point(event);
  if (!at) return;
  event.preventDefault();
  moveQueued = null;
  input({ type: "scroll", ...at, ...wheelPixels(event) });
}

function onKey(event: KeyboardEvent, isPressed: boolean) {
  const code = evdevFor(event.code);
  if (code === null) return;
  event.preventDefault();
  event.stopPropagation();
  // The app repeats a held key itself, from the keymap's repeat rate.
  if (event.repeat) return;
  if (!isPressed && META_CODES.has(code)) {
    // No keyup comes on macOS for a key released while Cmd was down.
    for (const held of releasedWithMeta(pressed)) {
      input({ type: "key", code: held, pressed: false });
      pressed.delete(held);
    }
  }
  if (isPressed) pressed.add(code);
  else pressed.delete(code);
  input({ type: "key", code, pressed: isPressed });
}

/** Release every key and button this panel holds down in the app, while
 * the socket can still say so. */
function releaseAll() {
  for (const code of pressed) input({ type: "key", code, pressed: false });
  pressed.clear();
  onPointerLost();
}

/** Nothing stays held down when the panel loses the keyboard. */
function onBlur() {
  releaseAll();
}

/** Stop streaming this window: releases first, then the socket. */
function disconnect() {
  releaseAll();
  if (retry) clearTimeout(retry);
  retry = null;
  const ws = socket;
  socket = null;
  ws?.close();
  decoder.reset();
}

onMounted(() => {
  connect();
  observer = new ResizeObserver(() => {
    if (resizeTimer) clearTimeout(resizeTimer);
    resizeTimer = setTimeout(sendSize, 120);
  });
  if (box.value) observer.observe(box.value);
});

// The compact shell's one panel follows focus: the same panel is handed
// another window. Its socket, picture and held input are the old window's.
watch(
  () => props.window.id,
  () => {
    disconnect();
    picture.value = { width: 0, height: 0 };
    status.value = "connecting";
    connect();
    // Handed a window that is already the focused one (the compact shell's
    // panel follows focus), so the focus watch below sees no change.
    takeKeyboard();
  },
);

/** When this window is the shell's focused one, give the app the
 * keyboard, and the browser's keyboard to this panel: keys go to whichever
 * element has the DOM focus, which would otherwise still be the last
 * window's canvas, or nothing. */
function takeKeyboard() {
  if (props.nested || shell.windows.focusedId.value !== props.window.id) return;
  input({ type: "focus" });
  // After whatever moved the focus (the palette, the switcher) is gone.
  void nextTick(() => {
    if (shell.windows.focusedId.value === props.window.id) canvas.value?.focus({ preventScroll: true });
  });
}

watch(
  () => shell.windows.focusedId.value === props.window.id,
  (focused) => {
    if (focused) takeKeyboard();
  },
  { immediate: true },
);

onBeforeUnmount(() => {
  alive = false;
  if (resizeTimer) clearTimeout(resizeTimer);
  observer?.disconnect();
  disconnect();
});
</script>

<template>
  <div ref="box" class="stream" :data-status="status" data-streamed="">
    <canvas
      ref="canvas"
      class="picture"
      tabindex="0"
      :aria-label="`${props.window.title}, streamed from the node`"
      @pointermove="onPointerMove"
      @pointerdown="onPointerButton($event, true)"
      @pointerup="onPointerButton($event, false)"
      @pointercancel="onPointerLost"
      @lostpointercapture="onPointerLost"
      @pointerleave="input({ type: 'pointer-leave' })"
      @wheel="onWheel"
      @keydown="onKey($event, true)"
      @keyup="onKey($event, false)"
      @blur="onBlur"
      @contextmenu.prevent
    />
    <p v-if="status !== 'live'" class="note" role="status">
      <template v-if="status === 'unsupported'">This browser cannot decode the stream (H.264 through WebCodecs).</template>
      <template v-else-if="status === 'seat'">Streaming to the device that holds the seat.</template>
      <template v-else>Connecting to {{ props.window.stream?.appTitle ?? "the app" }}…</template>
    </p>
    <div
      v-for="popup in popups"
      :key="popup.id"
      class="popup"
      :style="popupStyle(popup)"
    >
      <StreamPanel :window="popup" nested />
    </div>
  </div>
</template>

<style scoped>
.stream {
  position: relative;
  flex: 1;
  min-width: 0;
  min-height: 0;
  background: var(--dj-color-surface);
  overflow: hidden;
}
.picture {
  position: absolute;
  inset: 0;
  width: 100%;
  height: 100%;
  /* The picture is the app's pixels; scaled to fit, never stretched. The
     pointer math in shell/stream.ts (fit) assumes exactly this. */
  object-fit: contain;
  outline: none;
  touch-action: none;
  cursor: default;
}
.note {
  position: absolute;
  inset: auto 0 0 0;
  margin: 0;
  padding: var(--dj-space-2, 8px) var(--dj-space-3, 12px);
  font-size: var(--dj-font-size-sm, 12px);
  color: var(--dj-color-text-muted);
  pointer-events: none;
}
.popup {
  position: absolute;
  display: flex;
  box-shadow: var(--dj-shadow-popover, 0 4px 16px rgb(0 0 0 / 30%));
}
</style>
