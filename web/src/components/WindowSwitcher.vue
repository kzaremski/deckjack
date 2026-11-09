<script setup lang="ts">
import { computed } from "vue";

import { useShell } from "../shell/context.ts";
import { apps, dockOrder } from "../stores/apps.ts";

/**
 * The compact switcher: open windows ordered by recent focus, plus the apps
 * that are not open yet.
 *
 * Recent focus rather than open order, because on a phone the thing I want
 * next is almost always the thing I was in before this one. It is a sheet
 * rather than a full screen so the window behind it stays visible — that is
 * the context for "which of these did I mean".
 */
const shell = useShell();

const open = computed(() => shell.windows.recent.value);
const closed = computed(() =>
  dockOrder
    .filter((id) => !shell.windows.all.value.some((w) => w.app === id))
    .map((id) => apps[id]),
);

function close() {
  shell.ui.switcherOpen.value = false;
}

function pick(id: string) {
  shell.windows.focus(id);
  close();
}

function splitWith(id: string) {
  const current = shell.windows.focusedId.value;
  if (current && current !== id) shell.windows.setSplit(current, id);
  close();
}
</script>

<template>
  <div v-if="shell.ui.switcherOpen.value" class="scrim" @pointerdown.self="close">
    <div class="sheet" role="dialog" aria-modal="true" aria-label="Open windows">
      <header>
        <h2>Windows</h2>
        <button type="button" class="text" @click="close">Done</button>
      </header>

      <ul class="list">
        <li v-for="w in open" :key="w.id">
          <button type="button" class="row" @click="pick(w.id)">
            <span class="tag" aria-hidden="true">{{ shell.windows.contract(w).tag }}</span>
            <span class="name">
              {{ w.title }}
              <span v-if="w.id === shell.windows.focusedId.value" class="now">in front</span>
            </span>
          </button>
          <button
            type="button"
            class="side"
            :disabled="w.id === shell.windows.focusedId.value"
            aria-label="Split with this window"
            @click="splitWith(w.id)"
          >
            Split
          </button>
          <button
            type="button"
            class="side"
            aria-label="Close this window"
            @click="shell.windows.close(w.id)"
          >
            Close
          </button>
        </li>
        <li v-if="open.length === 0" class="none">No windows open.</li>
      </ul>

      <h3 v-if="closed.length">Not open</h3>
      <ul class="list">
        <li v-for="app in closed" :key="app.id">
          <button
            type="button"
            class="row"
            @click="shell.commands.dispatch(`app.open.${app.id}`).then(close)"
          >
            <span class="tag" aria-hidden="true">{{ app.tag }}</span>
            <span class="name">
              {{ app.title }}
              <span class="summary">{{ app.summary }}</span>
            </span>
          </button>
        </li>
      </ul>
    </div>
  </div>
</template>

<style scoped>
.scrim {
  position: fixed;
  inset: 0;
  z-index: var(--dj-layer-sheet);
  display: flex;
  align-items: flex-end;
  background: var(--dj-color-scrim);
}

.sheet {
  display: flex;
  flex-direction: column;
  gap: var(--dj-space-2);
  width: 100%;
  max-height: 80vh;
  padding: var(--dj-space-3);
  padding-bottom: calc(var(--dj-space-3) + env(safe-area-inset-bottom, 0px));
  overflow-y: auto;
  background: var(--dj-color-overlay);
  border-top: var(--dj-stroke-regular) solid var(--dj-color-border-strong);
  border-start-start-radius: var(--dj-radius-xl);
  border-start-end-radius: var(--dj-radius-xl);
  animation: slide var(--dj-duration-base) var(--dj-ease-entrance);
}

@keyframes slide {
  from {
    transform: translateY(12px);
  }
}

header {
  display: flex;
  align-items: center;
  justify-content: space-between;
}

h2,
h3 {
  font-size: var(--dj-type-micro-size);
  letter-spacing: var(--dj-type-micro-tracking);
  text-transform: uppercase;
  color: var(--dj-color-text-muted);
}

.text {
  min-height: var(--dj-touch);
  padding-inline: var(--dj-space-3);
  border: 0;
  border-radius: var(--dj-radius-md);
  background: transparent;
  color: var(--dj-color-accent);
  font-weight: var(--dj-type-strong-weight);
}

.list {
  display: flex;
  flex-direction: column;
  gap: var(--dj-space-1);
  margin: 0;
  padding: 0;
  list-style: none;
}

.list li {
  display: flex;
  align-items: center;
  gap: var(--dj-space-2);
}

.row {
  display: flex;
  align-items: center;
  gap: var(--dj-space-3);
  flex: 1;
  min-width: 0;
  min-height: var(--dj-touch);
  padding: var(--dj-space-2);
  border: 0;
  border-radius: var(--dj-radius-md);
  background: var(--dj-color-surface);
  color: inherit;
  text-align: start;
}

.side {
  min-height: var(--dj-touch);
  padding-inline: var(--dj-space-3);
  border: var(--dj-stroke-regular) solid var(--dj-color-border);
  border-radius: var(--dj-radius-md);
  background: transparent;
  color: var(--dj-color-text-muted);
  font-size: var(--dj-type-small-size);
}

.tag {
  display: grid;
  place-items: center;
  flex: none;
  width: 28px;
  height: 28px;
  border-radius: var(--dj-radius-sm);
  background: var(--dj-color-sunken);
  color: var(--dj-color-text-muted);
  font-family: var(--dj-font-mono);
  font-size: var(--dj-type-micro-size);
}

.name {
  display: flex;
  flex-direction: column;
  min-width: 0;
}

.now,
.summary {
  font-size: var(--dj-type-micro-size);
  color: var(--dj-color-text-dim);
}

.now {
  color: var(--dj-color-accent);
}

.none {
  padding: var(--dj-space-3);
  color: var(--dj-color-text-muted);
  font-size: var(--dj-type-small-size);
}
</style>
