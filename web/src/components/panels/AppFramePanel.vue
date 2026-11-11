<script setup lang="ts">
import { onBeforeUnmount, ref, watch } from "vue";

import { useShell } from "../../shell/context.ts";
import { installTokenStylesheet } from "../../tokens/runtime.ts";
import type { ShellWindow } from "../../stores/windows.ts";

/**
 * An installed app's own web UI, in a window.
 *
 * The app's assets are served by the core at `/apps/<name>/`, on the core's
 * own origin. That is the point of routing them through the core rather than
 * letting the browser reach an app: the frame is same-origin, so the session
 * cookie applies, the app's UI can use the bus at `/ws` with no credentials
 * of its own, and there is no app address for the browser to hold.
 *
 * Same-origin is also what makes theming work. The shell reaches into the
 * frame's document and installs the same token stylesheet it uses itself, so
 * an app written against `var(--dj-color-surface)` follows the desktop's
 * theme without the app knowing a theme exists. It is re-installed whenever
 * the theme controller's revision changes, which is the counter that exists
 * for exactly this.
 */
const props = defineProps<{ window: ShellWindow; compact?: boolean }>();

const shell = useShell();
const frame = ref<HTMLIFrameElement | null>(null);
const failed = ref(false);

/** Install (or re-install) the tokens inside the frame. */
function theme() {
  const doc = frame.value?.contentDocument;
  if (!doc) return;
  try {
    installTokenStylesheet(doc);
  } catch {
    // A cross-origin frame would throw, and should: it would mean the core
    // started serving an app from somewhere else, which is the thing this
    // arrangement exists to prevent. Nothing to do here but leave the app
    // with its own fallback colors.
    failed.value = true;
  }
}

watch(() => shell.theme.revision.value, theme);

onBeforeUnmount(() => {
  // Nothing to tear down: the frame goes with the component, and the app
  // behind it is stopped by the core's idle sweep once its last window
  // closed. An app is deliberately not killed when a window closes — the
  // same reason a pty outlives its panel.
  frame.value = null;
});
</script>

<template>
  <iframe
    ref="frame"
    class="frame"
    :src="`/apps/${props.window.appId}/`"
    :title="props.window.title"
    :data-compact="props.compact ? '' : undefined"
    @load="theme"
  />
  <p v-if="failed" class="note">
    This app is served from another origin, so the desktop's theme was not
    applied.
  </p>
</template>

<style scoped>
.frame {
  display: block;
  flex: 1;
  width: 100%;
  min-height: 0;
  border: 0;
  background: var(--dj-color-surface);
}

.note {
  margin: 0;
  padding: var(--dj-space-2) var(--dj-space-3);
  background: var(--dj-color-warning-soft, var(--dj-color-surface-raised));
  color: var(--dj-color-text-muted);
  font-size: var(--dj-text-xs);
}
</style>
