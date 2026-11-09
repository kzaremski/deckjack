<script setup lang="ts">
import { computed, onBeforeUnmount, onMounted, ref } from "vue";

import { rectForZone, type SnapZone } from "../shell/geometry.ts";
import { useShell } from "../shell/context.ts";
import WindowFrame from "./WindowFrame.vue";

/**
 * Desktop mode: floating windows with snapping, over a wall, above a dock.
 *
 * The work area is measured from this element rather than computed from the
 * viewport minus a dock height constant. Density changes the dock's height,
 * a soft keyboard changes the viewport, and window-controls-overlay changes
 * the top inset; measuring is the only version of this that stays correct.
 */
const shell = useShell();
const desktop = ref<HTMLElement | null>(null);
const preview = ref<SnapZone | null>(null);

let observer: ResizeObserver | null = null;

onMounted(() => {
  const el = desktop.value;
  if (!el) return;
  observer = new ResizeObserver((entries) => {
    const box = entries[0]?.contentRect;
    if (!box) return;
    shell.windows.workArea.value = { w: Math.round(box.width), h: Math.round(box.height) };
  });
  observer.observe(el);
  const box = el.getBoundingClientRect();
  shell.windows.workArea.value = { w: Math.round(box.width), h: Math.round(box.height) };
  // The title bar height is a density token, and the clamp that keeps a
  // dragged window reachable needs it in pixels.
  const bar = getComputedStyle(el).getPropertyValue("--dj-titlebar").trim();
  const parsed = Number.parseInt(bar, 10);
  if (Number.isFinite(parsed)) shell.windows.titlebar.value = parsed;
});

onBeforeUnmount(() => observer?.disconnect());

const hint = computed(() =>
  preview.value
    ? rectForZone(preview.value, shell.windows.workArea.value, preview.value === "maximize" ? 0 : 6)
    : null,
);

const paletteChord = computed(() => shell.keyboard.labelFor("shell.palette.open") ?? "");
</script>

<template>
  <div class="desktop" ref="desktop">
    <div class="wall" aria-hidden="true" />

    <div
      v-if="hint"
      class="snap-hint"
      aria-hidden="true"
      :style="{
        transform: `translate3d(${hint.x}px, ${hint.y}px, 0)`,
        width: `${hint.w}px`,
        height: `${hint.h}px`,
      }"
    />

    <WindowFrame
      v-for="w in shell.windows.visible.value"
      :key="w.id"
      :window="w"
      @snap-preview="preview = $event"
    />

    <div v-if="shell.windows.visible.value.length === 0" class="empty">
      <p>No windows open.</p>
      <p class="muted">
        Press <kbd>{{ paletteChord }}</kbd> for the command palette, or pick an app
        from the dock.
      </p>
    </div>

    <div class="watermark" aria-hidden="true">
      <span>deckjack · {{ shell.bus.node.value?.name ?? "no node" }}</span>
      <span v-if="shell.bus.node.value">gen {{ shell.bus.node.value.generation }}</span>
    </div>
  </div>
</template>

<style scoped>
.desktop {
  position: relative;
  flex: 1;
  min-height: 0;
  overflow: hidden;
}

/*
 * The wall's grid is drawn from the accent at low alpha, so it changes with
 * the theme instead of being a picture. A wallpaper file would be the one
 * surface the token set did not reach.
 */
.wall {
  position: absolute;
  inset: 0;
  background-color: var(--dj-color-ground);
  background-image:
    linear-gradient(var(--dj-color-ground-pattern) 1px, transparent 1px),
    linear-gradient(90deg, var(--dj-color-ground-pattern) 1px, transparent 1px);
  background-size: 48px 48px;
}

.snap-hint {
  position: absolute;
  inset-block-start: 0;
  inset-inline-start: 0;
  z-index: var(--dj-layer-snap-hint);
  border: var(--dj-stroke-thick) solid var(--dj-color-accent-line);
  border-radius: var(--dj-radius-lg);
  background: var(--dj-color-accent-soft);
  pointer-events: none;
  transition:
    transform var(--dj-duration-fast) var(--dj-ease-standard),
    width var(--dj-duration-fast) var(--dj-ease-standard),
    height var(--dj-duration-fast) var(--dj-ease-standard);
}

.empty {
  position: absolute;
  inset-block-start: 38%;
  inset-inline: 0;
  display: flex;
  flex-direction: column;
  align-items: center;
  gap: var(--dj-space-2);
  text-align: center;
  color: var(--dj-color-text-muted);
}

.empty .muted {
  color: var(--dj-color-text-dim);
  font-size: var(--dj-type-small-size);
}

.empty kbd {
  padding: 2px 6px;
  border: var(--dj-stroke-regular) solid var(--dj-color-border);
  border-radius: var(--dj-radius-xs);
  font-family: var(--dj-font-mono);
  font-size: var(--dj-type-micro-size);
}

.watermark {
  position: absolute;
  inset-block-end: var(--dj-space-4);
  inset-inline-end: var(--dj-space-6);
  display: flex;
  flex-direction: column;
  align-items: flex-end;
  gap: 2px;
  font-family: var(--dj-font-mono);
  font-size: var(--dj-type-micro-size);
  color: var(--dj-color-text-dim);
  pointer-events: none;
}
</style>
