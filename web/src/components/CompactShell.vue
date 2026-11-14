<script setup lang="ts">
import { computed } from "vue";

import { useShell } from "../shell/context.ts";
import AppSurface from "./AppSurface.vue";
import SplitView from "./SplitView.vue";

/**
 * Compact mode: one window fills the screen, with an optional split.
 *
 * This is not the desktop layout shrunk. There is no window chrome to drag,
 * because there is nothing to drag it to. Each app declares its own compact
 * layout and gets the whole box.
 *
 * The header this used to carry — window title, switcher, split, palette — is
 * gone, not deleted: `TopBar` carries all four now. Two stacked bars cost two
 * rows of a short screen to say the same thing twice.
 */
const shell = useShell();

const current = computed(
  () => shell.windows.focused.value ?? shell.windows.recent.value[0] ?? null,
);
const split = computed(() => shell.windows.split.value);
const paletteChord = computed(() => shell.keyboard.labelFor("shell.palette.open") ?? "");
</script>

<template>
  <div class="compact">
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
