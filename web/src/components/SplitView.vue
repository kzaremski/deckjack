<script setup lang="ts">
import { computed, ref } from "vue";

import { useShell } from "../shell/context.ts";
import AppSurface from "./AppSurface.vue";

/**
 * Two windows with a draggable divider: the compact layout's split.
 *
 * The divider defaults to the hinge on an unfolded Fold rather than to the
 * middle, which is the whole reason the viewport segment measurement exists.
 * Putting the seam anywhere else on a foldable means one pane is permanently
 * creased.
 *
 * It stacks instead of splitting when the viewport is tall and narrow,
 * because two 180px-wide panes are not two panes.
 */
const shell = useShell();

const split = computed(() => shell.windows.split.value);
const left = computed(() =>
  shell.windows.all.value.find((w) => w.id === split.value?.left) ?? null,
);
const right = computed(() =>
  shell.windows.all.value.find((w) => w.id === split.value?.right) ?? null,
);

const container = ref<HTMLElement | null>(null);
const stacked = computed(
  () => shell.viewport.width.value < 560 && shell.viewport.height.value > shell.viewport.width.value,
);
const fraction = computed(() => split.value?.fraction ?? shell.viewport.splitFraction.value);

let pointer: number | null = null;

function begin(event: PointerEvent) {
  pointer = event.pointerId;
  (event.currentTarget as Element).setPointerCapture(event.pointerId);
}

function move(event: PointerEvent) {
  if (pointer !== event.pointerId) return;
  const box = container.value?.getBoundingClientRect();
  if (!box) return;
  const next = stacked.value
    ? (event.clientY - box.top) / box.height
    : (event.clientX - box.left) / box.width;
  shell.windows.setSplitFraction(next);
}

function end(event: PointerEvent) {
  if (pointer === event.pointerId) pointer = null;
}

function nudge(event: KeyboardEvent) {
  const step = event.shiftKey ? 0.01 : 0.05;
  const back = stacked.value ? "ArrowUp" : "ArrowLeft";
  const forward = stacked.value ? "ArrowDown" : "ArrowRight";
  if (event.key === back) shell.windows.setSplitFraction(fraction.value - step);
  else if (event.key === forward) shell.windows.setSplitFraction(fraction.value + step);
  else return;
  event.preventDefault();
}
</script>

<template>
  <div
    v-if="left && right"
    ref="container"
    class="split"
    :class="{ stacked }"
    :style="{ '--dj-split': `${(fraction * 100).toFixed(2)}%` }"
  >
    <section class="pane first" :aria-label="left.title">
      <AppSurface :window="left" compact />
    </section>

    <div
      class="divider"
      role="separator"
      tabindex="0"
      :aria-orientation="stacked ? 'horizontal' : 'vertical'"
      :aria-valuenow="Math.round(fraction * 100)"
      aria-valuemin="15"
      aria-valuemax="85"
      aria-label="Resize the split"
      @pointerdown="begin"
      @pointermove="move"
      @pointerup="end"
      @pointercancel="end"
      @keydown="nudge"
      @dblclick="shell.windows.setSplitFraction(shell.viewport.splitFraction.value)"
    >
      <span class="grip" aria-hidden="true" />
    </div>

    <section class="pane second" :aria-label="right.title">
      <AppSurface :window="right" compact />
    </section>
  </div>
</template>

<style scoped>
.split {
  display: grid;
  grid-template-columns: var(--dj-split) auto 1fr;
  flex: 1;
  min-height: 0;
  background: var(--dj-color-ground);
}

.split.stacked {
  grid-template-columns: 1fr;
  grid-template-rows: var(--dj-split) auto 1fr;
}

.pane {
  display: flex;
  min-width: 0;
  min-height: 0;
  overflow: hidden;
}

/*
 * The divider's hit area is a touch target even though it looks like a line:
 * a 1px seam is a pointer-only control, and this layout exists for a
 * touchscreen.
 */
.divider {
  display: grid;
  place-items: center;
  width: var(--dj-space-3);
  min-width: 12px;
  background: var(--dj-color-ground);
  cursor: col-resize;
  touch-action: none;
}

.split.stacked .divider {
  width: auto;
  height: var(--dj-space-3);
  min-height: 12px;
  cursor: row-resize;
}

.grip {
  width: 3px;
  height: 36px;
  border-radius: var(--dj-radius-pill);
  background: var(--dj-color-border-strong);
}

.split.stacked .grip {
  width: 36px;
  height: 3px;
}

.divider:hover .grip,
.divider:focus-visible .grip {
  background: var(--dj-color-accent);
}
</style>
