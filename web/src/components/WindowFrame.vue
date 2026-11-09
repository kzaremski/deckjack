<script setup lang="ts">
import { computed, ref } from "vue";

import {
  cursorFor,
  resizeRect,
  snapZoneAt,
  type Handle,
  type Rect,
  type SnapZone,
} from "../shell/geometry.ts";
import { useShell } from "../shell/context.ts";
import type { ShellWindow } from "../stores/windows.ts";
import AppSurface from "./AppSurface.vue";

/**
 * One desktop window: chrome, drag, resize and snap.
 *
 * JackLink's window manager is the reference for the chrome. Two details
 * carried over from it matter more than they look:
 *
 * - Pointer capture on the title bar, not a document-level listener. A drag
 *   that is tracked on `document` loses the window the moment the pointer
 *   crosses an iframe or leaves the viewport, and an embedded third-party UI
 *   (code-server) is exactly such an iframe.
 * - `touch-action: none` on the drag and resize targets, so a drag on a
 *   touchscreen moves the window instead of scrolling the desktop. Pointer
 *   events then cover mouse, touch and pen with one code path, which is what
 *   "pointer, touch and pen all first-class" has to mean in practice.
 *
 * Window coordinates are the work area's, and the work area starts at the
 * viewport origin, so `clientX`/`clientY` need no conversion.
 */
const props = defineProps<{ window: ShellWindow }>();
const emit = defineEmits<{ (e: "snapPreview", zone: SnapZone | null): void }>();

const shell = useShell();
const focused = computed(() => shell.windows.focusedId.value === props.window.id);
const contract = computed(() => shell.windows.contract(props.window));

interface Drag {
  pointer: number;
  startX: number;
  startY: number;
  origin: Rect;
  handle: Handle | null;
}

const drag = ref<Drag | null>(null);
const zone = ref<SnapZone | null>(null);

function begin(event: PointerEvent, handle: Handle | null) {
  if (event.button !== 0) return;
  shell.windows.focus(props.window.id);
  // A maximized window is dragged loose rather than dragged around at full
  // size, which is what every window manager does and what the hand expects.
  // `toggleMaximize` restores the rectangle synchronously, so reading it
  // afterwards gives the geometry the drag should start from.
  if (!handle && props.window.maximized) shell.windows.toggleMaximize(props.window.id);
  const origin = { ...props.window.rect };
  drag.value = {
    pointer: event.pointerId,
    startX: event.clientX,
    startY: event.clientY,
    origin,
    handle,
  };
  (event.currentTarget as Element).setPointerCapture(event.pointerId);
  event.preventDefault();
}

function move(event: PointerEvent) {
  const d = drag.value;
  if (!d || d.pointer !== event.pointerId) return;
  const dx = event.clientX - d.startX;
  const dy = event.clientY - d.startY;

  if (d.handle) {
    shell.windows.place(
      props.window.id,
      resizeRect(d.origin, d.handle, dx, dy, contract.value.minSize),
    );
    return;
  }

  shell.windows.place(props.window.id, {
    x: d.origin.x + dx,
    y: d.origin.y + dy,
    w: d.origin.w,
    h: d.origin.h,
  });

  const next = snapZoneAt(event.clientX, event.clientY, shell.windows.workArea.value);
  if (next !== zone.value) {
    zone.value = next;
    emit("snapPreview", next);
  }
}

async function end(event: PointerEvent) {
  const d = drag.value;
  if (!d || d.pointer !== event.pointerId) return;
  drag.value = null;
  const dropped = zone.value;
  zone.value = null;
  emit("snapPreview", null);
  if (dropped && !d.handle) {
    await shell.windows.snap(props.window.id, dropped);
    return;
  }
  await shell.windows.commitPlacement(props.window.id);
}

/**
 * Keyboard-driven window management, because a window that can only be moved
 * with a pointer is a window the keyboard-first rule does not cover. Arrow
 * keys nudge, Shift nudges by one pixel, the modifier snaps.
 */
async function onTitleKey(event: KeyboardEvent) {
  const step = event.shiftKey ? 1 : 16;
  const mod = event.metaKey || event.ctrlKey;
  const rect = props.window.rect;
  const id = props.window.id;

  switch (event.key) {
    case "ArrowLeft":
      if (mod) await shell.windows.snap(id, "left");
      else shell.windows.place(id, { ...rect, x: rect.x - step });
      break;
    case "ArrowRight":
      if (mod) await shell.windows.snap(id, "right");
      else shell.windows.place(id, { ...rect, x: rect.x + step });
      break;
    case "ArrowUp":
      if (mod) await shell.windows.snap(id, "maximize");
      else shell.windows.place(id, { ...rect, y: rect.y - step });
      break;
    case "ArrowDown":
      if (mod) shell.windows.minimize(id);
      else shell.windows.place(id, { ...rect, y: rect.y + step });
      break;
    case "Enter":
      shell.windows.toggleMaximize(id);
      break;
    default:
      return;
  }
  event.preventDefault();
  if (!mod) await shell.windows.commitPlacement(id);
}

const handles: Handle[] = ["n", "s", "e", "w", "ne", "nw", "se", "sw"];

const style = computed(() => ({
  transform: `translate3d(${props.window.rect.x}px, ${props.window.rect.y}px, 0)`,
  width: `${props.window.rect.w}px`,
  height: `${props.window.rect.h}px`,
  // `layer.window` is 10 and `layer.windowFocused` is 500 in the token set:
  // the focused window is lifted over the rest without leaving the band
  // below the dock and the palette.
  zIndex: String((focused.value ? 500 : 10) + props.window.z),
}));
</script>

<template>
  <section
    class="window"
    :class="{ focused, dragging: drag !== null }"
    :style="style"
    :aria-label="`${props.window.title} window`"
    @pointerdown="shell.windows.focus(props.window.id)"
  >
    <header
      class="bar"
      tabindex="0"
      role="toolbar"
      :aria-label="`${props.window.title}: move, resize and window actions`"
      @pointerdown="begin($event, null)"
      @pointermove="move"
      @pointerup="end"
      @pointercancel="end"
      @dblclick="shell.windows.toggleMaximize(props.window.id)"
      @keydown="onTitleKey"
    >
      <span class="tag" :data-accent="props.window.app === 'assistant' ? '' : undefined">
        {{ contract.tag }}
      </span>
      <span class="title">{{ props.window.title }}</span>
      <span v-if="props.window.snap" class="snapped">{{ props.window.snap }}</span>
      <button
        type="button"
        class="action"
        aria-label="Minimize"
        @pointerdown.stop
        @click="shell.windows.minimize(props.window.id)"
      >
        <svg viewBox="0 0 16 16" aria-hidden="true"><path d="M4 8h8" /></svg>
      </button>
      <button
        type="button"
        class="action"
        :aria-label="props.window.maximized ? 'Restore' : 'Maximize'"
        @pointerdown.stop
        @click="shell.windows.toggleMaximize(props.window.id)"
      >
        <svg viewBox="0 0 16 16" aria-hidden="true"><rect x="4" y="4" width="8" height="8" rx="1" /></svg>
      </button>
      <button
        type="button"
        class="action"
        aria-label="Close"
        @pointerdown.stop
        @click="shell.windows.close(props.window.id)"
      >
        <svg viewBox="0 0 16 16" aria-hidden="true"><path d="M4.5 4.5l7 7M11.5 4.5l-7 7" /></svg>
      </button>
    </header>

    <AppSurface :window="props.window" />

    <!--
      Resize handles overlap the frame rather than sitting inside the panel,
      so a corner grab never lands on the app's content. They are 10px, which
      is thin for a finger; on a touchscreen windows are resized by snapping
      and compact mode does not have them at all.
    -->
    <div
      v-for="handle in handles"
      :key="handle"
      class="grip"
      :data-handle="handle"
      :style="{ cursor: cursorFor(handle) }"
      @pointerdown="begin($event, handle)"
      @pointermove="move"
      @pointerup="end"
      @pointercancel="end"
    />
  </section>
</template>

<style scoped>
.window {
  position: absolute;
  inset-block-start: 0;
  inset-inline-start: 0;
  display: flex;
  flex-direction: column;
  min-width: 0;
  overflow: hidden;
  background: var(--dj-color-surface);
  border: var(--dj-stroke-regular) solid var(--dj-color-border);
  border-radius: var(--dj-radius-lg);
  box-shadow: var(--dj-color-shadow-mid);
  /* Only the frame animates, and only when it is not being dragged: a
     transition during a drag turns the pointer into a leash. */
  transition:
    box-shadow var(--dj-duration-fast) var(--dj-ease-standard),
    border-color var(--dj-duration-fast) var(--dj-ease-standard);
}

.window.focused {
  border-color: var(--dj-color-accent-line);
  box-shadow: var(--dj-color-shadow-high);
}

.window.dragging {
  /* Stops the pointer from selecting text inside the panel mid-drag. */
  user-select: none;
}

.bar {
  display: flex;
  align-items: center;
  gap: var(--dj-space-2);
  flex: none;
  height: var(--dj-titlebar);
  padding-inline: var(--dj-space-2) var(--dj-space-1);
  background: var(--dj-color-raised);
  border-bottom: var(--dj-stroke-regular) solid var(--dj-color-border);
  cursor: grab;
  touch-action: none;
  user-select: none;
}

.bar:active {
  cursor: grabbing;
}

.tag {
  display: grid;
  place-items: center;
  flex: none;
  width: 22px;
  height: 22px;
  border-radius: var(--dj-radius-sm);
  background: var(--dj-color-sunken);
  color: var(--dj-color-text-muted);
  font-family: var(--dj-font-mono);
  font-size: var(--dj-type-micro-size);
}

.tag[data-accent] {
  background: var(--dj-color-accent);
  color: var(--dj-color-text-on-accent);
}

.title {
  flex: 1;
  min-width: 0;
  overflow: hidden;
  white-space: nowrap;
  text-overflow: ellipsis;
  font-size: var(--dj-type-body-size);
  font-weight: var(--dj-type-strong-weight);
  color: var(--dj-color-text-muted);
}

.focused .title {
  color: var(--dj-color-text);
}

.snapped {
  flex: none;
  font-family: var(--dj-font-mono);
  font-size: var(--dj-type-micro-size);
  color: var(--dj-color-text-dim);
}

/*
 * Window actions stay visible without hover: the doc's rule is that hover
 * never hides an essential control, and on a touchscreen there is no hover to
 * reveal them with.
 */
.action {
  display: grid;
  place-items: center;
  flex: none;
  width: var(--dj-control);
  height: calc(var(--dj-titlebar) - var(--dj-space-2));
  padding: 0;
  border: 0;
  border-radius: var(--dj-radius-sm);
  background: transparent;
  color: var(--dj-color-text-muted);
}

.action svg {
  width: 16px;
  height: 16px;
  fill: none;
  stroke: currentColor;
  stroke-width: 1.5;
  stroke-linecap: round;
}

@media (hover: hover) {
  .action:hover {
    background: var(--dj-color-sunken);
    color: var(--dj-color-text);
  }
}

.grip {
  position: absolute;
  touch-action: none;
}

.grip[data-handle="n"] {
  inset: -3px 10px auto 10px;
  height: 10px;
}
.grip[data-handle="s"] {
  inset: auto 10px -3px 10px;
  height: 10px;
}
.grip[data-handle="e"] {
  inset: 10px -3px 10px auto;
  width: 10px;
}
.grip[data-handle="w"] {
  inset: 10px auto 10px -3px;
  width: 10px;
}
.grip[data-handle="ne"] {
  inset: -3px -3px auto auto;
  width: 14px;
  height: 14px;
}
.grip[data-handle="nw"] {
  inset: -3px auto auto -3px;
  width: 14px;
  height: 14px;
}
.grip[data-handle="se"] {
  inset: auto -3px -3px auto;
  width: 14px;
  height: 14px;
}
.grip[data-handle="sw"] {
  inset: auto auto -3px -3px;
  width: 14px;
  height: 14px;
}
</style>
