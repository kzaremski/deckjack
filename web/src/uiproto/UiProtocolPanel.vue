<script setup lang="ts">
import { computed, onBeforeUnmount, provide, shallowReactive, watch } from "vue";

import { useShell } from "../shell/context.ts";
import type { CommandEntry } from "../stores/commands.ts";
import type { ShellWindow } from "../stores/windows.ts";
import UiNode from "./UiNode.vue";
import { UiKey, type UiContext } from "./format.ts";
import { walk } from "./tree.ts";

/**
 * The window of an app that describes its UI (`tier = 1`): the app sends a
 * tree over the UI protocol and this panel draws it with the shell's own
 * components. No frame, no page, no app JavaScript.
 *
 * The window's state lives in the shell's UI channel rather than in this
 * component, because a window outlives its panel: the compact shell reuses
 * one panel for whichever window is focused, and a change of layout remounts
 * every panel. Showing a window again draws the tree the shell already has.
 *
 * Bound buttons outside lists are also palette entries while the window is
 * focused, enabled exactly when the button is: the same action from the
 * keyboard as from the pointer.
 */
const props = defineProps<{ window: ShellWindow; compact?: boolean }>();

const shell = useShell();
const channel = shell.uiproto;
const ctx: UiContext = shallowReactive({
  channel,
  win: channel.show(props.window.id, props.window.appId),
});
provide(UiKey, ctx);

watch(
  () => props.window.id,
  (id, previous) => {
    channel.hide(previous);
    ctx.win = channel.show(id, props.window.appId);
  },
);

onBeforeUnmount(() => channel.hide(ctx.win.id));

const tree = computed(() => ctx.win.tree.value);
const lost = computed(() => ctx.win.status.value === "lost");
const notice = computed(() => ctx.win.notice.value);

// --- the palette -----------------------------------------------------------------

watch(
  [() => shell.windows.focusedId.value === props.window.id, tree] as const,
  ([focused, root], _, onCleanup) => {
    if (!focused || !root) return;
    const win = ctx.win;
    const title = props.window.title;
    const entries: CommandEntry[] = [];
    for (const { node, path } of walk(root)) {
      const w = node.widget;
      if (w.kind !== "button" || !w.action) continue;
      const action = w.action;
      const label = w.title || w.label || action.command;
      entries.push({
        id: `ui.window.${win.id}.${path.join(".")}`,
        title: `${title}: ${label}`,
        // The bus's own risk for the command, so the palette labels it the
        // way it labels the command itself.
        risk: shell.commands.byId(action.command)?.risk ?? "write",
        keywords: [title, action.command],
        group: title,
        available: () => w.enabled !== false && channel.ready(win, action),
        run: async () => {
          await channel.run(win, action);
        },
      });
    }
    if (entries.length === 0) return;
    onCleanup(shell.commands.register(entries));
  },
  { immediate: true },
);
</script>

<template>
  <div class="ui-panel" :data-compact="props.compact ? '' : undefined" :data-app="props.window.appId">
    <UiNode v-if="tree" :key="tree.key" :node="tree" path="" />
    <p v-else class="note" role="status">Starting {{ props.window.title }}…</p>
    <p v-if="lost" class="note" role="status">
      {{ props.window.title }} stopped; waiting for it to come back.
    </p>
    <p v-if="notice" class="notice" role="alert">
      <span>{{ notice }}</span>
      <button type="button" class="dismiss" aria-label="Dismiss" @click="ctx.win.notice.value = null">
        ×
      </button>
    </p>
  </div>
</template>

<style scoped>
.ui-panel {
  display: flex;
  flex: 1;
  flex-direction: column;
  min-width: 0;
  min-height: 0;
  background: var(--dj-color-surface);
  color: var(--dj-color-text);
  font-family: var(--dj-font-sans);
  font-size: var(--dj-type-body-size);
  line-height: var(--dj-type-small-line);
}

.note,
.notice {
  display: flex;
  flex: none;
  gap: var(--dj-space-2);
  align-items: center;
  margin: 0;
  padding: var(--dj-space-2) var(--dj-space-3);
  font-size: var(--dj-type-small-size);
}

.note {
  color: var(--dj-color-text-muted);
}

.notice {
  background: var(--dj-color-danger-soft);
  color: var(--dj-color-danger);
}

.notice span {
  flex: 1;
}

.dismiss {
  border: 0;
  background: none;
  color: inherit;
  font: inherit;
  cursor: pointer;
}
</style>
