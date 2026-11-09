<script setup lang="ts">
import { computed } from "vue";

import { useShell } from "../shell/context.ts";
import AppSurface from "./AppSurface.vue";
import SplitView from "./SplitView.vue";

/**
 * Compact mode: one window fills the screen, with an optional split.
 *
 * This is not the desktop layout shrunk. There is no window chrome to drag,
 * because there is nothing to drag it to; the title bar becomes a header that
 * carries the switcher and the palette, which are the two things reached
 * constantly on a phone. Each app declares its own compact layout and gets
 * the whole box.
 */
const shell = useShell();

const current = computed(
  () => shell.windows.focused.value ?? shell.windows.recent.value[0] ?? null,
);
const split = computed(() => shell.windows.split.value);
const contract = computed(() => (current.value ? shell.windows.contract(current.value) : null));
const paletteChord = computed(() => shell.keyboard.labelFor("shell.palette.open") ?? "");
</script>

<template>
  <div class="compact">
    <header class="bar">
      <button
        type="button"
        class="switch"
        aria-label="Switch window"
        @click="shell.commands.dispatch('shell.switcher.open')"
      >
        <span v-if="contract" class="tag" aria-hidden="true">{{ contract.tag }}</span>
        <span class="title">{{ current?.title ?? "Deckjack" }}</span>
        <svg viewBox="0 0 24 24" aria-hidden="true" class="chev">
          <path d="m7 10 5 5 5-5" />
        </svg>
      </button>

      <button
        v-if="shell.windows.recent.value.length >= 2"
        type="button"
        class="icon"
        :aria-pressed="split !== null"
        :aria-label="split ? 'Leave the split' : 'Split with the last window'"
        @click="shell.commands.dispatch('shell.split.toggle')"
      >
        <svg viewBox="0 0 24 24" aria-hidden="true">
          <rect x="3" y="4" width="18" height="16" rx="2" />
          <path d="M12 4v16" />
        </svg>
      </button>

      <button
        type="button"
        class="icon"
        aria-label="Command palette"
        @click="shell.commands.dispatch('shell.palette.open')"
      >
        <svg viewBox="0 0 24 24" aria-hidden="true">
          <circle cx="11" cy="11" r="7" />
          <path d="m20 20-3.5-3.5" />
        </svg>
      </button>
    </header>

    <SplitView v-if="split" />

    <AppSurface v-else-if="current" :window="current" compact />

    <div v-else class="empty">
      <p>No windows open.</p>
      <p class="muted">
        Open an app from the switcher, or press <kbd>{{ paletteChord }}</kbd> for
        the palette.
      </p>
      <button type="button" class="primary" @click="shell.commands.dispatch('shell.switcher.open')">
        Open an app
      </button>
    </div>
  </div>
</template>

<style scoped>
.compact {
  display: flex;
  flex-direction: column;
  flex: 1;
  min-height: 0;
  background: var(--dj-color-ground);
}

.bar {
  display: flex;
  align-items: center;
  gap: var(--dj-space-1);
  flex: none;
  padding-inline: var(--dj-space-2);
  padding-top: env(safe-area-inset-top, 0px);
  background: var(--dj-color-raised);
  border-bottom: var(--dj-stroke-regular) solid var(--dj-color-border);
}

.switch {
  display: flex;
  align-items: center;
  gap: var(--dj-space-2);
  flex: 1;
  min-width: 0;
  min-height: var(--dj-touch);
  padding-inline: var(--dj-space-2);
  border: 0;
  border-radius: var(--dj-radius-md);
  background: transparent;
  color: inherit;
  text-align: start;
}

.tag {
  display: grid;
  place-items: center;
  flex: none;
  width: 24px;
  height: 24px;
  border-radius: var(--dj-radius-sm);
  background: var(--dj-color-sunken);
  color: var(--dj-color-text-muted);
  font-family: var(--dj-font-mono);
  font-size: var(--dj-type-micro-size);
}

.title {
  flex: 1;
  min-width: 0;
  overflow: hidden;
  white-space: nowrap;
  text-overflow: ellipsis;
  font-size: var(--dj-type-title-size);
  font-weight: var(--dj-type-strong-weight);
}

.chev {
  width: 18px;
  height: 18px;
  flex: none;
  fill: none;
  stroke: var(--dj-color-text-muted);
  stroke-width: 2;
  stroke-linecap: round;
}

.icon {
  display: grid;
  place-items: center;
  flex: none;
  width: var(--dj-touch);
  height: var(--dj-touch);
  border: 0;
  border-radius: var(--dj-radius-md);
  background: transparent;
  color: var(--dj-color-text-muted);
}

.icon[aria-pressed="true"] {
  background: var(--dj-color-accent-soft);
  color: var(--dj-color-accent);
}

.icon svg {
  width: 20px;
  height: 20px;
  fill: none;
  stroke: currentColor;
  stroke-width: 2;
  stroke-linecap: round;
  stroke-linejoin: round;
}

.empty {
  display: flex;
  flex: 1;
  flex-direction: column;
  align-items: center;
  justify-content: center;
  gap: var(--dj-space-3);
  padding: var(--dj-space-6);
  text-align: center;
  color: var(--dj-color-text-muted);
}

.empty .muted {
  color: var(--dj-color-text-dim);
  font-size: var(--dj-type-small-size);
}

.empty kbd {
  font-family: var(--dj-font-mono);
}

.primary {
  min-height: var(--dj-touch);
  padding-inline: var(--dj-space-5);
  border: 0;
  border-radius: var(--dj-radius-md);
  background: var(--dj-color-accent);
  color: var(--dj-color-text-on-accent);
  font-weight: var(--dj-type-strong-weight);
}
</style>
