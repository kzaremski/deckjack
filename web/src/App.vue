<script setup lang="ts">
import { onBeforeUnmount, onMounted, watch } from "vue";

import CommandPalette from "./components/CommandPalette.vue";
import CompactShell from "./components/CompactShell.vue";
import ConfirmDialog from "./components/ConfirmDialog.vue";
import DesktopShell from "./components/DesktopShell.vue";
import Dock from "./components/Dock.vue";
import NoticeStack from "./components/NoticeStack.vue";
import WindowSwitcher from "./components/WindowSwitcher.vue";
import { provideShell, type Shell } from "./shell/context.ts";

/**
 * The shell: one set of windows, shown two ways.
 *
 * Desktop and compact are two views over the same window list, not two
 * applications. Switching between them — by unfolding a Fold, by plugging
 * into DeX, or by hand from the palette — changes which component renders and
 * nothing else, which is why the layouts can be kept per device class on the
 * server without either one disturbing the other.
 */
const props = defineProps<{ shell: Shell }>();
provideShell(props.shell);

const shell = props.shell;

/**
 * One capture-phase listener for the whole shell.
 *
 * Capture, because a panel may stop propagation for its own reasons and the
 * global shortcuts have to survive that — Ctrl/Cmd+K must open the palette
 * from inside a terminal. The chord table decides what is swallowed; anything
 * unclaimed reaches the focused panel untouched.
 */
function onKeyDown(event: KeyboardEvent) {
  if (event.key === "Escape") {
    if (shell.ui.paletteOpen.value || shell.ui.switcherOpen.value) {
      shell.ui.paletteOpen.value = false;
      shell.ui.switcherOpen.value = false;
      event.preventDefault();
      return;
    }
  }
  const command = shell.keyboard.resolve(event);
  if (!command) return;
  event.preventDefault();
  event.stopPropagation();
  void shell.commands.dispatch(command);
}

/**
 * Keyboard Lock has to be asked for from a user gesture, and the browser only
 * grants it to an installed window or fullscreen. Asking once, on the first
 * interaction, is the difference between Ctrl+W closing a Deckjack window and
 * closing the whole workspace.
 */
async function requestLockOnce() {
  window.removeEventListener("pointerdown", requestLockOnce);
  window.removeEventListener("keydown", requestLockOnce);
  if (!shell.viewport.standalone.value) return;
  await shell.keyboard.requestLock();
}

onMounted(() => {
  window.addEventListener("keydown", onKeyDown, { capture: true });
  window.addEventListener("pointerdown", requestLockOnce, { once: true });
  window.addEventListener("keydown", requestLockOnce, { once: true });
});

onBeforeUnmount(() => {
  window.removeEventListener("keydown", onKeyDown, { capture: true });
  shell.keyboard.releaseLock();
});

// Store failures become notices rather than console noise: a window move that
// the server refused has to be visible, or the two copies of the layout
// diverge silently.
watch(
  () => shell.windows.lastError.value,
  (message) => {
    if (message) shell.ui.notify("error", message);
  },
);

watch(
  () => shell.commands.lastError.value,
  (message) => {
    if (message) shell.ui.notify("error", message);
  },
);
</script>

<template>
  <div class="shell" :data-mode="shell.viewport.mode.value">
    <DesktopShell v-if="shell.viewport.mode.value === 'desktop'" />
    <CompactShell v-else />
    <Dock />

    <CommandPalette />
    <WindowSwitcher />
    <NoticeStack />
    <ConfirmDialog />
  </div>
</template>

<style scoped>
.shell {
  display: flex;
  flex-direction: column;
  height: 100%;
  min-height: 0;
  background: var(--dj-color-ground);
  color: var(--dj-color-text);
}
</style>
