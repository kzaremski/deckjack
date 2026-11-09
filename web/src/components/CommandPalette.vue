<script setup lang="ts">
import { computed, nextTick, ref, watch } from "vue";

import { useShell } from "../shell/context.ts";
import { riskLabel, type Scored } from "../stores/commands.ts";

/**
 * The command palette: one field over the whole command bus.
 *
 * It lists shell commands, app commands and CLI adapter commands without
 * knowing which is which, because they are all the same kind of thing. A new
 * adapter therefore appears here the moment its backend registers, with no
 * work in this file — that is the entire point of routing the UI through the
 * bus instead of wiring menus.
 *
 * Risk is shown, never computed: the chip says what the backend declared.
 */
const shell = useShell();

const query = ref("");
const cursor = ref(0);
const input = ref<HTMLInputElement | null>(null);

const results = computed<Scored[]>(() => shell.commands.search(query.value));
const activeId = computed(() => results.value[cursor.value]?.entry.id);

watch(
  () => shell.ui.paletteOpen.value,
  async (open) => {
    if (!open) return;
    query.value = "";
    cursor.value = 0;
    await nextTick();
    input.value?.focus();
  },
);

watch(results, () => {
  cursor.value = 0;
});

function close() {
  shell.ui.paletteOpen.value = false;
}

async function run(index: number) {
  const hit = results.value[index];
  if (!hit) return;
  close();
  await shell.commands.dispatch(hit.entry.id);
}

function onKey(event: KeyboardEvent) {
  switch (event.key) {
    case "Escape":
      close();
      break;
    case "ArrowDown":
      cursor.value = (cursor.value + 1) % Math.max(1, results.value.length);
      break;
    case "ArrowUp":
      cursor.value =
        (cursor.value - 1 + Math.max(1, results.value.length)) %
        Math.max(1, results.value.length);
      break;
    case "Enter":
      void run(cursor.value);
      break;
    default:
      return;
  }
  event.preventDefault();
}

/** Splits a title into matched and unmatched runs for highlighting. */
function pieces(title: string, hits: number[]) {
  const set = new Set(hits);
  return [...title].map((char, i) => ({ char, hit: set.has(i) }));
}
</script>

<template>
  <div v-if="shell.ui.paletteOpen.value" class="scrim" @pointerdown.self="close">
    <div class="palette" role="dialog" aria-modal="true" aria-label="Command palette">
      <div class="field">
        <svg viewBox="0 0 24 24" aria-hidden="true" class="glass">
          <circle cx="11" cy="11" r="7" />
          <path d="m20 20-3.5-3.5" />
        </svg>
        <label for="dj-palette" class="dj-sr">Command</label>
        <input
          id="dj-palette"
          ref="input"
          v-model="query"
          type="text"
          autocomplete="off"
          spellcheck="false"
          placeholder="Type a command, or an app to open"
          @keydown="onKey"
        />
        <kbd>esc</kbd>
      </div>

      <ul
        class="results"
        role="listbox"
        aria-label="Commands"
        :aria-activedescendant="activeId ? `dj-cmd-${activeId}` : undefined"
      >
        <li
          v-for="(hit, index) in results"
          :id="`dj-cmd-${hit.entry.id}`"
          :key="hit.entry.id"
          role="option"
          :aria-selected="index === cursor"
        >
          <button
            type="button"
            class="result"
            :class="{ on: index === cursor }"
            @pointerenter="cursor = index"
            @click="run(index)"
          >
            <span class="label">
              <span class="title">
                <span
                  v-for="(piece, at) in pieces(hit.entry.title, hit.hits)"
                  :key="at"
                  :class="{ hit: piece.hit }"
                  >{{ piece.char }}</span
                >
              </span>
              <span v-if="hit.entry.summary" class="summary">{{ hit.entry.summary }}</span>
            </span>
            <span class="meta">
              <span v-if="hit.entry.node" class="node">{{ hit.entry.node }}</span>
              <span class="risk" :data-risk="hit.entry.risk">{{ riskLabel(hit.entry.risk) }}</span>
              <kbd v-if="shell.keyboard.labelFor(hit.entry.id)">
                {{ shell.keyboard.labelFor(hit.entry.id) }}
              </kbd>
              <span v-else-if="hit.entry.group" class="group">{{ hit.entry.group }}</span>
            </span>
          </button>
        </li>
        <li v-if="results.length === 0" class="none">
          Nothing matches. Commands come from the shell and from whatever apps
          and adapters have registered on this node.
        </li>
      </ul>
    </div>
  </div>
</template>

<style scoped>
.scrim {
  position: fixed;
  inset: 0;
  z-index: var(--dj-layer-palette);
  display: flex;
  justify-content: center;
  align-items: flex-start;
  padding: clamp(var(--dj-space-4), 12vh, 140px) var(--dj-space-4) var(--dj-space-4);
  background: var(--dj-color-scrim);
}

.palette {
  display: flex;
  flex-direction: column;
  width: min(600px, 100%);
  max-height: 70vh;
  overflow: hidden;
  background: var(--dj-color-overlay);
  border: var(--dj-stroke-regular) solid var(--dj-color-border-strong);
  border-radius: var(--dj-radius-xl);
  box-shadow: var(--dj-color-shadow-high);
  animation: rise var(--dj-duration-base) var(--dj-ease-entrance);
}

@keyframes rise {
  from {
    transform: translateY(6px);
    opacity: 0;
  }
}

.field {
  display: flex;
  align-items: center;
  gap: var(--dj-space-3);
  flex: none;
  padding-inline: var(--dj-space-4);
  border-bottom: var(--dj-stroke-regular) solid var(--dj-color-border);
}

.glass {
  width: 18px;
  height: 18px;
  flex: none;
  fill: none;
  stroke: var(--dj-color-text-muted);
  stroke-width: 2;
  stroke-linecap: round;
}

.field input {
  flex: 1;
  min-width: 0;
  height: calc(var(--dj-control) + var(--dj-space-4));
  border: 0;
  background: transparent;
  font-size: var(--dj-type-title-size);
  outline: none;
}

.field input::placeholder {
  color: var(--dj-color-text-dim);
}

kbd {
  flex: none;
  font-family: var(--dj-font-mono);
  font-size: var(--dj-type-micro-size);
  color: var(--dj-color-text-dim);
}

.results {
  margin: 0;
  padding: var(--dj-space-2);
  list-style: none;
  overflow-y: auto;
  overscroll-behavior: contain;
}

.result {
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: var(--dj-space-3);
  width: 100%;
  min-height: var(--dj-touch);
  padding-inline: var(--dj-space-3);
  border: 0;
  border-radius: var(--dj-radius-md);
  background: transparent;
  color: inherit;
  text-align: start;
}

.result.on {
  background: var(--dj-color-accent-soft);
}

.label {
  display: flex;
  flex-direction: column;
  min-width: 0;
}

.title {
  font-size: var(--dj-type-body-size);
  white-space: nowrap;
  overflow: hidden;
  text-overflow: ellipsis;
}

/* Matched characters are weighted and colored: color alone would not carry
   on a projector or for a colorblind operator. */
.title .hit {
  color: var(--dj-color-accent);
  font-weight: 700;
}

.summary {
  font-size: var(--dj-type-micro-size);
  color: var(--dj-color-text-muted);
  white-space: nowrap;
  overflow: hidden;
  text-overflow: ellipsis;
}

.meta {
  display: flex;
  align-items: center;
  gap: var(--dj-space-2);
  flex: none;
}

.group,
.node {
  font-size: var(--dj-type-micro-size);
  color: var(--dj-color-text-dim);
}

.node {
  font-family: var(--dj-font-mono);
}

.risk {
  padding: 1px var(--dj-space-2);
  border-radius: var(--dj-radius-pill);
  font-size: var(--dj-type-micro-size);
  letter-spacing: var(--dj-type-micro-tracking);
  text-transform: uppercase;
}

.risk[data-risk="read"] {
  color: var(--dj-color-risk-read);
}

.risk[data-risk="write"] {
  color: var(--dj-color-risk-write);
  background: var(--dj-color-info-soft);
}

.risk[data-risk="destructive"] {
  color: var(--dj-color-risk-destructive);
  background: var(--dj-color-danger-soft);
}

.none {
  padding: var(--dj-space-4);
  color: var(--dj-color-text-muted);
  font-size: var(--dj-type-small-size);
}
</style>
