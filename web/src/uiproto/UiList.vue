<script setup lang="ts">
import { computed, inject, nextTick, onBeforeUnmount, onMounted, ref, watch } from "vue";

import { UiKey, childPath, toPath } from "./format.ts";
import { adopted } from "./selection.ts";
import type { UiNode } from "./tree.ts";

/**
 * A described list: keyed rows, virtualized past a few hundred, selecting by
 * the app kit's own rules.
 *
 * **Virtualized** means only the rows in view (plus a margin) are in the DOM,
 * placed under a spacer as tall as every row would be. Scrolling is the
 * browser's own and never waits for the node: the rows to draw are computed
 * here from the scroll position, from the tree the shell already holds. Rows
 * are measured, not assumed, from the first one drawn, and a virtualized
 * list keeps each row to one line so they stay the same height. A short
 * list draws every row and lets long text wrap, as a page would.
 *
 * **Selection** (`selection` of `single` or `multi`) is local first too: a
 * click, Ctrl/Cmd-click, Shift-click or arrow key changes it at once, and
 * the app hears `selected` with the keys afterwards. Enter or a double click
 * sends `activated` for the row under the cursor.
 */
const props = defineProps<{ node: UiNode; path: string }>();
defineSlots<{ default(props: { row: UiNode; rowPath: string }): unknown }>();

const ui = inject(UiKey)!;

/** Lists shorter than this draw every row. */
const VIRTUAL_FROM = 200;
/** Rows drawn beyond each edge of the viewport while virtualized. */
const OVERSCAN = 8;

const rows = computed(() => props.node.children ?? []);
const widget = computed(() => (props.node.widget.kind === "list" ? props.node.widget : null));
const selectable = computed(() => (widget.value?.selection ?? "none") !== "none");
const multi = computed(() => widget.value?.selection === "multi");
const virtual = computed(() => rows.value.length > VIRTUAL_FROM);

const scroller = ref<HTMLElement | null>(null);
const scrollTop = ref(0);
const viewport = ref(600);
const rowHeight = ref(36);

const first = computed(() =>
  virtual.value ? Math.max(0, Math.floor(scrollTop.value / rowHeight.value) - OVERSCAN) : 0,
);
const last = computed(() =>
  virtual.value
    ? Math.min(
        rows.value.length,
        Math.ceil((scrollTop.value + viewport.value) / rowHeight.value) + OVERSCAN,
      )
    : rows.value.length,
);
const shown = computed(() => rows.value.slice(first.value, last.value));

function onScroll() {
  if (scroller.value) scrollTop.value = scroller.value.scrollTop;
}

/** Measure a drawn row, so the spacer and the window match the real height. */
function measure() {
  const row = scroller.value?.querySelector<HTMLElement>(".ui-list-row");
  const h = row?.offsetHeight ?? 0;
  if (h > 0 && h !== rowHeight.value) rowHeight.value = h;
  if (scroller.value && scroller.value.clientHeight > 0) viewport.value = scroller.value.clientHeight;
}

let resize: ResizeObserver | null = null;
onMounted(() => {
  measure();
  if (typeof ResizeObserver !== "undefined" && scroller.value) {
    resize = new ResizeObserver(() => measure());
    resize.observe(scroller.value);
  }
});
onBeforeUnmount(() => resize?.disconnect());
watch(virtual, () => void nextTick(measure));

// --- selection -------------------------------------------------------------------

const selection = computed(() =>
  selectable.value ? ui.channel.selection(ui.win, toPath(props.path)) : null,
);

watch(
  () => [selection.value, rows.value.map((r) => r.key).join("\n")] as const,
  () => selection.value?.setItems(rows.value.map((r) => r.key)),
  { immediate: true },
);

/** The last `selected` event sent, so an app's echo of an older one is not
 * taken as its answer to the newest. */
let lastSent = 0;

// The app may set the selection: on first sight (the window's first frame, a
// reopen, a remount) and whenever it changes it. Between its changes the
// operator's stands, and a value the app sent before hearing the operator's
// latest selection is not applied: its answer to that one is on the way.
watch(
  () => [selection.value, JSON.stringify(widget.value?.selected ?? [])] as const,
  ([s, json]) => {
    if (!s || adopted.get(s) === json || ui.win.acked < lastSent) return;
    adopted.set(s, json);
    const keys = widget.value?.selected ?? [];
    s.clear();
    // A single-select list takes one key, whatever the app sent.
    for (const key of multi.value ? keys : keys.slice(0, 1)) s.click(key, { toggle: multi.value });
  },
  { immediate: true },
);

function isSelected(key: string): boolean {
  void ui.win.selectionVersion.value;
  return selection.value?.has(key) ?? false;
}

function isCursor(key: string): boolean {
  void ui.win.selectionVersion.value;
  return selection.value?.cursor === key;
}

function announce() {
  const s = selection.value;
  if (s) lastSent = ui.channel.event(ui.win, toPath(props.path), "selected", s.selected) ?? lastSent;
}

function clicked(key: string, event: MouseEvent) {
  const s = selection.value;
  if (!s) return;
  s.click(key, {
    toggle: multi.value && (event.ctrlKey || event.metaKey),
    extend: multi.value && event.shiftKey,
  });
  announce();
}

function activate(key: string | null) {
  if (key === null) return;
  ui.channel.event(ui.win, toPath(props.path), "activated", key);
}

function keyed(event: KeyboardEvent) {
  const s = selection.value;
  if (!s) return;
  if (event.key === "ArrowDown" || event.key === "ArrowUp") {
    event.preventDefault();
    s.move(event.key === "ArrowDown" ? 1 : -1, { extend: multi.value && event.shiftKey });
    reveal(s.cursor);
    announce();
  } else if (event.key === "Enter") {
    event.preventDefault();
    activate(s.cursor);
  } else if (multi.value && (event.ctrlKey || event.metaKey) && event.key === "a") {
    event.preventDefault();
    s.selectAll();
    announce();
  }
}

/** Keep the cursor's row in view while the keyboard moves it. */
function reveal(key: string | null) {
  const el = scroller.value;
  if (!el || key === null) return;
  const index = rows.value.findIndex((r) => r.key === key);
  if (index < 0) return;
  const top = index * rowHeight.value;
  if (top < el.scrollTop) el.scrollTop = top;
  else if (top + rowHeight.value > el.scrollTop + el.clientHeight) {
    el.scrollTop = top + rowHeight.value - el.clientHeight;
  }
}
</script>

<template>
  <div
    ref="scroller"
    class="ui-list"
    :data-virtual="virtual ? '' : undefined"
    :role="selectable ? 'listbox' : 'list'"
    :aria-label="widget?.label || undefined"
    :aria-multiselectable="multi ? 'true' : undefined"
    :tabindex="selectable ? 0 : undefined"
    @scroll.passive="onScroll"
    @keydown="keyed"
  >
    <div
      class="ui-list-space"
      :style="virtual ? { height: `${rows.length * rowHeight}px` } : undefined"
    >
      <div
        class="ui-list-window"
        :style="virtual ? { transform: `translateY(${first * rowHeight}px)` } : undefined"
      >
        <div
          v-for="row in shown"
          :key="row.key"
          class="ui-list-row"
          :data-key="row.key"
          :role="selectable ? 'option' : 'listitem'"
          :aria-selected="selectable ? (isSelected(row.key) ? 'true' : 'false') : undefined"
          :data-cursor="isCursor(row.key) ? '' : undefined"
          @click="clicked(row.key, $event)"
          @dblclick="selectable && activate(row.key)"
        >
          <slot :row="row" :row-path="childPath(path, row.key)" />
        </div>
      </div>
    </div>
  </div>
</template>

<style scoped>
.ui-list {
  flex: 1;
  min-height: 0;
  overflow-y: auto;
  overscroll-behavior: contain;
}
.ui-list:focus-visible {
  outline: var(--dj-stroke-thick) solid var(--dj-color-accent);
  outline-offset: -2px;
}
.ui-list-space {
  position: relative;
}
.ui-list-row[role="option"] {
  cursor: default;
}
.ui-list-row[aria-selected="true"] {
  background: var(--dj-color-selection);
}
.ui-list[data-virtual] .ui-list-row :deep(.ui-text) {
  overflow: hidden;
  white-space: nowrap;
  text-overflow: ellipsis;
}
</style>
