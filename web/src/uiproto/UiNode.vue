<script setup lang="ts">
import { computed, inject } from "vue";

import UiList from "./UiList.vue";
import { UiKey, childPath, formatDate, toPath } from "./format.ts";
import type { Choice, UiNode as Node } from "./tree.ts";

/**
 * One node of a described window, drawn with the shell's own markup and
 * styled only by design tokens.
 *
 * Every string an app sends is rendered with text interpolation, which Vue
 * escapes; there is no `v-html` anywhere in this renderer, and nothing here
 * reads a URL, a style or a class name from the tree. An app chooses which
 * of twelve widgets to show and what they say, never how they are built.
 *
 * `path` is a string (see `childPath`) so an unchanged child receives an
 * unchanged prop and is skipped when its parent re-renders.
 */
defineOptions({ name: "UiNode" });

const props = defineProps<{ node: Node; path: string }>();
const ui = inject(UiKey)!;

const w = computed(() => props.node.widget);
const children = computed(() => props.node.children ?? []);

function at(): string[] {
  return toPath(props.path);
}

// --- buttons -----------------------------------------------------------------

const buttonEnabled = computed(() => {
  const widget = w.value;
  if (widget.kind !== "button") return false;
  if (widget.enabled === false) return false;
  return !widget.action || ui.channel.ready(ui.win, widget.action);
});

function press() {
  const widget = w.value;
  if (widget.kind !== "button" || !buttonEnabled.value) return;
  if (widget.action) void ui.channel.run(ui.win, widget.action);
  else ui.channel.event(ui.win, at(), "activated");
}

// --- checkboxes ----------------------------------------------------------------

const checked = computed(() => {
  const widget = w.value;
  if (widget.kind !== "checkbox") return false;
  return ui.channel.valueOf(ui.win, at(), widget.checked ?? false) === true;
});

function toggle(event: Event) {
  if (w.value.kind !== "checkbox") return;
  // Shown at once, whatever the app makes of it.
  void ui.channel.tick(ui.win, at(), (event.target as HTMLInputElement).checked);
}

// --- text fields -----------------------------------------------------------------

const fieldValue = computed(() => {
  const widget = w.value;
  if (widget.kind !== "text_field" || !widget.name) return "";
  const v = ui.win.fields.get(widget.name);
  return typeof v === "string" ? v : "";
});

const inputType = computed(() => {
  const widget = w.value;
  if (widget.kind !== "text_field") return "text";
  if (widget.secret) return "password";
  return widget.input === "date" ? "date" : widget.input === "search" ? "search" : "text";
});

function typed(event: Event) {
  const widget = w.value;
  if (widget.kind !== "text_field" || !widget.name) return;
  ui.channel.changed(ui.win, at(), widget.name, (event.target as HTMLInputElement).value);
}

function submit(event: KeyboardEvent) {
  const widget = w.value;
  if (widget.kind !== "text_field" || event.isComposing) return;
  event.preventDefault();
  if (widget.submit) void ui.channel.run(ui.win, widget.submit);
  else {
    ui.channel.flush(ui.win);
    ui.channel.event(ui.win, at(), "submitted", fieldValue.value);
  }
}

function blurred() {
  ui.channel.flush(ui.win);
}

// --- selects -------------------------------------------------------------------

const selectIndex = computed(() => {
  const widget = w.value;
  if (widget.kind !== "select" || !widget.name) return -1;
  const v = ui.win.fields.get(widget.name);
  return (widget.options ?? []).findIndex((o) => JSON.stringify(o.value) === JSON.stringify(v));
});

function chose(event: Event) {
  const widget = w.value;
  if (widget.kind !== "select" || !widget.name) return;
  const option: Choice | undefined = (widget.options ?? [])[Number((event.target as HTMLSelectElement).value)];
  if (!option) return;
  ui.win.fields.set(widget.name, option.value);
  ui.channel.event(ui.win, at(), "changed", option.value);
}

// --- dates ---------------------------------------------------------------------

const date = computed(() => {
  const widget = w.value;
  return widget.kind === "date" ? formatDate(widget.value ?? "") : null;
});
</script>

<template>
  <div
    v-if="w.kind === 'stack'"
    class="ui-stack"
    :data-gap="w.gap ?? 'medium'"
    :data-padded="w.padded ? '' : undefined"
    :data-grow="w.grow ? '' : undefined"
  >
    <UiNode v-for="c in children" :key="c.key" :node="c" :path="childPath(path, c.key)" />
  </div>

  <div
    v-else-if="w.kind === 'row'"
    class="ui-row"
    :data-gap="w.gap ?? 'medium'"
    :data-padded="w.padded ? '' : undefined"
    :data-divider="w.divider ? '' : undefined"
    :data-bar="w.bar ? '' : undefined"
  >
    <UiNode v-for="c in children" :key="c.key" :node="c" :path="childPath(path, c.key)" />
  </div>

  <div v-else-if="w.kind === 'scroll'" class="ui-scroll">
    <UiNode v-for="c in children" :key="c.key" :node="c" :path="childPath(path, c.key)" />
  </div>

  <span
    v-else-if="w.kind === 'text'"
    class="ui-text"
    :data-tone="w.tone ?? 'normal'"
    :data-strike="w.strike ? '' : undefined"
    :data-small="w.small ? '' : undefined"
    :data-grow="w.grow ? '' : undefined"
    >{{ w.text ?? "" }}</span
  >

  <h2 v-else-if="w.kind === 'heading'" class="ui-heading">{{ w.text ?? "" }}</h2>

  <button
    v-else-if="w.kind === 'button'"
    type="button"
    class="ui-button"
    :data-tone="w.tone ?? 'normal'"
    :data-quiet="w.quiet ? '' : undefined"
    :disabled="!buttonEnabled"
    :title="w.title ?? undefined"
    :aria-label="w.title ?? undefined"
    @click="press"
  >
    {{ w.label ?? "" }}
  </button>

  <label v-else-if="w.kind === 'checkbox'" class="ui-check">
    <input
      type="checkbox"
      :checked="checked"
      :disabled="w.enabled === false"
      :aria-label="w.show_label === false ? (w.label ?? undefined) : undefined"
      @change="toggle"
    />
    <span v-if="w.show_label !== false">{{ w.label ?? "" }}</span>
  </label>

  <input
    v-else-if="w.kind === 'text_field'"
    class="ui-field"
    :data-grow="w.grow ? '' : undefined"
    :type="inputType"
    :name="w.name"
    :value="fieldValue"
    :placeholder="w.placeholder || undefined"
    :aria-label="w.label || w.placeholder || w.name"
    :title="w.input === 'date' ? (w.label ?? undefined) : undefined"
    autocomplete="off"
    @input="typed"
    @keydown.enter="submit"
    @blur="blurred"
  />

  <select
    v-else-if="w.kind === 'select'"
    class="ui-field"
    :name="w.name"
    :aria-label="w.label || w.name"
    :title="w.label ?? undefined"
    :value="selectIndex"
    @change="chose"
  >
    <option v-for="(o, i) in w.options ?? []" :key="i" :value="i">{{ o.label }}</option>
  </select>

  <UiList v-else-if="w.kind === 'list'" :node="node" :path="path">
    <template #default="{ row, rowPath }">
      <UiNode :node="row" :path="rowPath" />
    </template>
  </UiList>

  <div v-else-if="w.kind === 'empty'" class="ui-empty" role="status">
    <p class="ui-empty-title">{{ w.title ?? "" }}</p>
    <p v-if="w.detail" class="ui-empty-detail">{{ w.detail }}</p>
  </div>

  <time
    v-else-if="w.kind === 'date' && date"
    class="ui-date"
    :datetime="w.value"
    :data-past="w.flag_past && date.past ? '' : undefined"
    >{{ date.label }}</time
  >
</template>

<style scoped>
.ui-stack {
  display: flex;
  flex-direction: column;
  min-width: 0;
  min-height: 0;
}
.ui-row {
  display: flex;
  align-items: center;
  min-width: 0;
}
.ui-stack[data-gap="small"],
.ui-row[data-gap="small"] {
  gap: var(--dj-space-1);
}
.ui-stack[data-gap="medium"],
.ui-row[data-gap="medium"] {
  gap: var(--dj-space-2);
}
.ui-stack[data-gap="large"],
.ui-row[data-gap="large"] {
  gap: var(--dj-space-4);
}
.ui-stack[data-padded],
.ui-row[data-padded] {
  padding: var(--dj-space-2) var(--dj-space-3);
}
.ui-stack[data-grow] {
  flex: 1;
}
.ui-row[data-divider] {
  border-bottom: var(--dj-stroke-hairline) solid var(--dj-color-border);
}
.ui-row[data-bar] {
  flex: none;
  padding: var(--dj-space-2) var(--dj-space-3);
  border-bottom: var(--dj-stroke-hairline) solid var(--dj-color-border);
  background: var(--dj-color-raised);
}
.ui-scroll {
  flex: 1;
  min-height: 0;
  overflow: auto;
}

.ui-text {
  min-width: 0;
  overflow-wrap: anywhere;
}
.ui-text[data-grow] {
  flex: 1;
}
.ui-text[data-small] {
  flex: none;
  font-size: var(--dj-type-micro-size);
  font-variant-numeric: tabular-nums;
}
.ui-text[data-tone="muted"] {
  color: var(--dj-color-text-muted);
}
.ui-text[data-tone="accent"] {
  color: var(--dj-color-accent);
}
.ui-text[data-tone="warning"] {
  color: var(--dj-color-warn);
}
.ui-text[data-tone="danger"] {
  color: var(--dj-color-danger);
}
.ui-text[data-strike] {
  text-decoration: line-through;
}

.ui-heading {
  flex: 1;
  margin: 0;
  font-size: var(--dj-type-small-size);
  font-weight: var(--dj-type-strong-weight);
  letter-spacing: 0.01em;
}

.ui-button {
  flex: none;
  min-height: var(--dj-control);
  padding: 0 var(--dj-space-3);
  border: var(--dj-stroke-regular) solid var(--dj-color-border-strong);
  border-radius: var(--dj-radius-sm);
  background: var(--dj-color-raised);
  color: var(--dj-color-text);
  font: inherit;
  cursor: pointer;
}
.ui-button:hover:not(:disabled) {
  border-color: var(--dj-color-accent);
}
.ui-button:disabled {
  opacity: 0.5;
  cursor: default;
}
.ui-button[data-quiet] {
  min-height: auto;
  padding: 0 var(--dj-space-1);
  border: 0;
  background: none;
  color: var(--dj-color-text-muted);
}
.ui-button[data-quiet]:hover:not(:disabled) {
  color: var(--dj-color-danger);
}
.ui-button[data-tone="danger"] {
  color: var(--dj-color-danger);
}
.ui-button[data-tone="accent"] {
  border-color: var(--dj-color-accent);
}

.ui-check {
  display: inline-flex;
  flex: none;
  gap: var(--dj-space-1);
  align-items: center;
  color: var(--dj-color-text-muted);
  font-size: var(--dj-type-small-size);
  cursor: pointer;
}
.ui-check input {
  margin: 0;
  accent-color: var(--dj-color-accent);
}

.ui-field {
  min-width: 0;
  min-height: var(--dj-control);
  padding: var(--dj-space-1) var(--dj-space-2);
  border: var(--dj-stroke-regular) solid var(--dj-color-border);
  border-radius: var(--dj-radius-sm);
  background: var(--dj-color-surface);
  color: inherit;
  font: inherit;
}
.ui-field[data-grow] {
  flex: 1;
}
.ui-field:focus-visible {
  outline: var(--dj-stroke-thick) solid var(--dj-color-accent);
  outline-offset: -1px;
}

.ui-empty {
  display: flex;
  flex: 1;
  flex-direction: column;
  padding: var(--dj-space-4) var(--dj-space-3);
  color: var(--dj-color-text-muted);
}
.ui-empty p {
  margin: 0;
}
.ui-empty-detail {
  font-size: var(--dj-type-small-size);
}

.ui-date {
  flex: none;
  color: var(--dj-color-text-muted);
  font-size: var(--dj-type-micro-size);
  font-variant-numeric: tabular-nums;
}
.ui-date[data-past] {
  color: var(--dj-color-danger);
}
</style>
