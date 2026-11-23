<script setup lang="ts">
import { computed } from "vue";

import { useShell } from "../shell/context.ts";
import type { ShellWindow } from "../stores/windows.ts";
import { panelFor, PlaceholderPanel } from "./panels/index.ts";

/**
 * The inside of a window: whichever panel the app provides.
 *
 * A panel never learns which node its backend runs on, and never learns
 * whether it is in a desktop window, a compact full-screen view or one half
 * of a split. It is given a box and told whether it is narrow; that is the
 * whole app contract on the layout side.
 */
const props = defineProps<{ window: ShellWindow; compact?: boolean }>();

const shell = useShell();
const contract = computed(() => shell.windows.contract(props.window));
const panel = computed(() => panelFor(props.window.appId, shell.apps));
const reason = computed(() =>
  shell.bus.detached.value
    ? "The shell node's command bus is not connected, so no app has registered."
    : undefined,
);
</script>

<template>
  <div class="surface" :data-compact="props.compact ? '' : undefined">
    <component
      :is="panel"
      v-if="panel !== PlaceholderPanel"
      :window="props.window"
      :compact="props.compact"
    />
    <PlaceholderPanel
      v-else
      :app="contract"
      :loading="props.window.loading"
      :reason="reason"
    />
  </div>
</template>

<style scoped>
.surface {
  display: flex;
  flex: 1;
  min-width: 0;
  min-height: 0;
  background: var(--dj-color-surface);
  /* Panels scroll their own content; the shell never scrolls. */
  overflow: hidden;
  container-type: inline-size;
}
</style>
