<script setup lang="ts">
import { computed, onBeforeUnmount, onMounted, ref, watch } from "vue";

import {
  clampView,
  FIT,
  fitSize,
  pinchView,
  ZOOM_LIMITS,
  zoomAt,
  type Point,
  type View,
} from "../../shell/gestures.ts";

/**
 * A photograph, with zoom and pan that work with two fingers.
 *
 * The geometry is `gestures.ts`; this component only turns pointer events
 * into calls to it. The picture sits fitted and centered in a layer the size
 * of the stage, and the layer is moved with one transform, so the browser
 * composites the gesture instead of re-laying out the page on every frame.
 *
 * While the picture is at its fitted size a one-finger drag is left alone so
 * it reaches the overlay, where a horizontal flick moves through the folder.
 * Once zoomed, one finger pans the picture and the event stops here.
 */
const props = defineProps<{ src: string; alt: string }>();
const emit = defineEmits<{
  (e: "meta", meta: { width: number; height: number }): void;
  (e: "failed"): void;
}>();

const box = ref<HTMLElement | null>(null);
const stage = ref({ w: 0, h: 0 });
const natural = ref({ w: 0, h: 0 });
const view = ref<View>({ ...FIT });
const zoomed = computed(() => view.value.scale > 1.001);
const fitted = computed(() => fitSize(natural.value, stage.value));

const imageStyle = computed(() => ({
  width: `${fitted.value.w}px`,
  height: `${fitted.value.h}px`,
  left: `${(stage.value.w - fitted.value.w) / 2}px`,
  top: `${(stage.value.h - fitted.value.h) / 2}px`,
}));
const layerStyle = computed(() => ({
  transform: `translate(${view.value.x}px, ${view.value.y}px) scale(${view.value.scale})`,
}));

function measure() {
  const el = box.value;
  if (!el) return;
  stage.value = { w: el.clientWidth, h: el.clientHeight };
  view.value = clampView(view.value, stage.value, fitted.value);
}

let observer: ResizeObserver | null = null;
onMounted(() => {
  measure();
  if (typeof ResizeObserver === "function" && box.value) {
    observer = new ResizeObserver(measure);
    observer.observe(box.value);
  }
});
onBeforeUnmount(() => observer?.disconnect());

// A new file starts fitted: the zoom of the last photograph means nothing
// for this one.
watch(
  () => props.src,
  () => {
    view.value = { ...FIT };
    natural.value = { w: 0, h: 0 };
  },
);

function loaded(event: Event) {
  const img = event.target as HTMLImageElement;
  natural.value = { w: img.naturalWidth, h: img.naturalHeight };
  measure();
  emit("meta", { width: img.naturalWidth, height: img.naturalHeight });
}

function local(event: { clientX: number; clientY: number }): Point {
  const rect = box.value?.getBoundingClientRect();
  return { x: event.clientX - (rect?.left ?? 0), y: event.clientY - (rect?.top ?? 0) };
}

function settle(next: View) {
  view.value = clampView(next, stage.value, fitted.value);
}

// -- pointers ---------------------------------------------------------------

const pointers = new Map<number, Point>();
let pinchStart: { view: View; from: [Point, Point] } | null = null;
let panStart: { view: View; at: Point } | null = null;
let lastTap: { at: Point; time: number } | null = null;
/**
 * True from the moment the fingers down become a pinch or a pan until the
 * last of them lifts. Ending the pinch early (the second finger lifting
 * first) must not hand the remaining finger back to the overlay, whose swipe
 * started when that finger landed: its lift would be read as a swipe of the
 * whole pinch's travel and change file.
 */
let gesturing = false;
/** True once the fingers down became a pinch, until the last one lifts. */
let pinched = false;
/** When the last pointer double tap toggled the zoom, on the page clock. */
let lastPointerToggle = -Infinity;

/** Taps closer than this in time and distance are one double tap. */
const DOUBLE_TAP_MS = 300;
const DOUBLE_TAP_PX = 30;
/**
 * How long after a pointer double tap a `dblclick` is taken as the browser's
 * echo of it. Chromium sends one after a touch double tap, and every browser
 * after a mouse double click; toggling on both zoomed in and straight back
 * out.
 */
const DBLCLICK_ECHO_MS = 500;

function down(event: PointerEvent) {
  pointers.set(event.pointerId, local(event));
  if (pointers.size === 2) {
    const [a, b] = [...pointers.values()] as [Point, Point];
    pinchStart = { view: { ...view.value }, from: [a, b] };
    panStart = null;
    pinched = true;
    event.stopPropagation();
  } else if (pointers.size === 1 && zoomed.value) {
    panStart = { view: { ...view.value }, at: local(event) };
    event.stopPropagation();
  }
  if (pinchStart || panStart) {
    gesturing = true;
    (event.currentTarget as Element | null)?.setPointerCapture?.(event.pointerId);
  }
}

function move(event: PointerEvent) {
  if (!pointers.has(event.pointerId)) return;
  pointers.set(event.pointerId, local(event));
  if (pinchStart && pointers.size >= 2) {
    const [a, b] = [...pointers.values()] as [Point, Point];
    // Unclamped while the fingers are down, so the picture follows them;
    // settled when they lift.
    view.value = pinchView(pinchStart.view, pinchStart.from, [a, b], ZOOM_LIMITS);
    event.stopPropagation();
  } else if (panStart) {
    const at = local(event);
    view.value = {
      ...panStart.view,
      x: panStart.view.x + at.x - panStart.at.x,
      y: panStart.view.y + at.y - panStart.at.y,
    };
    event.stopPropagation();
  }
}

function up(event: PointerEvent) {
  const at = pointers.get(event.pointerId) ?? local(event);
  const wasGesture = gesturing;
  const wasPinch = pinched;
  pointers.delete(event.pointerId);
  if (pointers.size < 2) pinchStart = null;
  if (pointers.size === 0) {
    panStart = null;
    gesturing = false;
    pinched = false;
  }
  if (wasGesture) {
    settle(view.value);
    event.stopPropagation();
  }

  // Double tap: toggle between fitted and a close look at the tapped point.
  // A lift that ends a pinch, or a cancelled pointer, is not half of one. A
  // tap on a zoomed photo is (it starts a pan that goes nowhere), which is
  // how a double tap zooms back out.
  const now = performance.now();
  if (wasPinch || event.type === "pointercancel") {
    lastTap = null;
  } else if (
    lastTap &&
    now - lastTap.time < DOUBLE_TAP_MS &&
    Math.hypot(at.x - lastTap.at.x, at.y - lastTap.at.y) < DOUBLE_TAP_PX
  ) {
    toggleAt(at);
    lastPointerToggle = now;
    lastTap = null;
  } else {
    lastTap = { at, time: now };
  }
}

/** A `dblclick` toggles only when no pointer double tap just did. */
function doubleClick(event: MouseEvent) {
  if (performance.now() - lastPointerToggle < DBLCLICK_ECHO_MS) return;
  toggleAt(local(event));
}

function toggleAt(at: Point) {
  settle(zoomed.value ? { ...FIT } : zoomAt(view.value, 2.5 / view.value.scale, at, ZOOM_LIMITS));
}

function wheel(event: WheelEvent) {
  event.preventDefault();
  const factor = Math.exp(-event.deltaY * (event.ctrlKey ? 0.01 : 0.002));
  settle(zoomAt(view.value, factor, local(event), ZOOM_LIMITS));
}

function center(): Point {
  return { x: stage.value.w / 2, y: stage.value.h / 2 };
}

/** Keys the overlay offers this viewer first. True when one was used. */
function key(event: KeyboardEvent): boolean {
  if (event.key === "+" || event.key === "=") settle(zoomAt(view.value, 1.5, center(), ZOOM_LIMITS));
  else if (event.key === "-" || event.key === "_") settle(zoomAt(view.value, 1 / 1.5, center(), ZOOM_LIMITS));
  else if (event.key === "0") settle({ ...FIT });
  else return false;
  return true;
}

defineExpose({ key, zoomed });
</script>

<template>
  <div
    ref="box"
    class="box"
    :class="{ zoomed }"
    :data-scale="view.scale.toFixed(2)"
    @pointerdown="down"
    @pointermove="move"
    @pointerup="up"
    @pointercancel="up"
    @wheel="wheel"
    @dblclick.prevent="doubleClick"
  >
    <div class="layer" :style="layerStyle">
      <img
        class="picture"
        :style="imageStyle"
        :src="props.src"
        :alt="props.alt"
        draggable="false"
        @load="loaded"
        @error="emit('failed')"
      />
    </div>
  </div>
</template>

<style scoped>
.box {
  position: relative;
  width: 100%;
  height: 100%;
  overflow: hidden;
  /* The gesture is ours: no browser pinch-zoom of the whole page, no
     overscroll navigation. Pointer events still arrive while fitted, which
     is how the overlay sees a swipe. */
  touch-action: none;
  cursor: zoom-in;
}

.box.zoomed {
  cursor: grab;
}

.layer {
  position: absolute;
  inset: 0;
  transform-origin: 0 0;
  will-change: transform;
}

.picture {
  position: absolute;
  user-select: none;
  -webkit-user-drag: none;
}
</style>
