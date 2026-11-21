<script setup lang="ts">
import { computed, onBeforeUnmount, ref, watch } from "vue";

import { renderMarkdown } from "../../shell/markdown.ts";
import { contentUrl, formatBytes, resolveRelative } from "../../shell/quicklook.ts";
import { decodePreview, TEXT_PREVIEW_BYTES, type TextPreview } from "../../shell/textpreview.ts";

/**
 * Text, source and Markdown: read, never run.
 *
 * Only the head of the file is fetched, with a `Range` request, so a log of
 * hundreds of megabytes costs a phone half a megabyte. Text goes into a
 * `<pre>` as text. Markdown goes through `markdown.ts`, which builds its HTML
 * from escaped text and a fixed set of elements, so nothing in the file can
 * become markup, let alone script.
 */
const props = defineProps<{
  path: string;
  /** Render Markdown as a document. Source otherwise. */
  markdown?: boolean;
}>();
const emit = defineEmits<{ (e: "meta", meta: { lines: number; size: number }): void }>();

type State =
  | { status: "loading" }
  | { status: "failed"; why: string }
  | { status: "ready"; preview: TextPreview; total: number };

const state = ref<State>({ status: "loading" });
/** Markdown shown as its source instead of rendered. */
const showSource = ref(false);
let abort: AbortController | null = null;

async function load(path: string) {
  abort?.abort();
  const controller = new AbortController();
  abort = controller;
  state.value = { status: "loading" };
  try {
    const response = await fetch(contentUrl(path), {
      headers: { Range: `bytes=0-${TEXT_PREVIEW_BYTES - 1}` },
      credentials: "same-origin",
      signal: controller.signal,
    });
    if (controller.signal.aborted) return;
    // An empty file has no byte 0, so a range on it is refused; it is still
    // an empty text file.
    if (response.status === 416) {
      state.value = { status: "ready", preview: decodePreview(new Uint8Array(0), 0), total: 0 };
      return;
    }
    if (!response.ok) {
      const why = (await response.text()).trim();
      state.value = { status: "failed", why: why || `The node answered ${response.status}.` };
      return;
    }
    const bytes = new Uint8Array(await response.arrayBuffer());
    if (controller.signal.aborted) return;
    const range = /\/(\d+)\s*$/.exec(response.headers.get("content-range") ?? "");
    const total = range ? Number(range[1]) : bytes.length;
    const preview = decodePreview(bytes.subarray(0, TEXT_PREVIEW_BYTES), total);
    state.value = { status: "ready", preview, total };
    if (preview.kind === "text") {
      emit("meta", { lines: countLines(preview.text), size: total });
    }
  } catch (error) {
    if (controller.signal.aborted) return;
    state.value = {
      status: "failed",
      why: `The node did not answer: ${error instanceof Error ? error.message : String(error)}`,
    };
  }
}

function countLines(text: string): number {
  if (text === "") return 0;
  let n = 1;
  for (let i = 0; i < text.length; i += 1) if (text.charCodeAt(i) === 10) n += 1;
  return text.endsWith("\n") ? n - 1 : n;
}

watch(
  () => props.path,
  (path) => {
    showSource.value = false;
    void load(path);
  },
  { immediate: true },
);
onBeforeUnmount(() => abort?.abort());

const text = computed(() =>
  state.value.status === "ready" && state.value.preview.kind === "text" ? state.value.preview.text : "",
);
const lines = computed(() => {
  const all = text.value.split("\n");
  if (all.length > 1 && all.at(-1) === "") all.pop();
  return all;
});
const rendered = computed(() =>
  props.markdown && !showSource.value
    ? renderMarkdown(text.value, {
        resolveImage: (src) => {
          const target = resolveRelative(props.path, src);
          return target ? contentUrl(target) : null;
        },
      })
    : "",
);
const notice = computed(() => {
  if (state.value.status !== "ready" || state.value.preview.kind !== "text") return null;
  const parts: string[] = [];
  if (state.value.preview.truncated) {
    parts.push(
      `Showing the first ${formatBytes(TEXT_PREVIEW_BYTES)} of ${formatBytes(state.value.total)}. Download it, or open it fully, for the rest.`,
    );
  }
  if (state.value.preview.lossy) {
    parts.push("Some bytes are not valid UTF-8 and are shown as �.");
  }
  return parts.join(" ") || null;
});
</script>

<template>
  <div class="text-view">
    <p v-if="state.status === 'loading'" class="quiet" role="status">Loading…</p>

    <div v-else-if="state.status === 'failed'" class="refusal" role="alert">
      <p class="headline">Could not load this file</p>
      <p class="why">{{ state.why }}</p>
    </div>

    <div
      v-else-if="state.preview.kind === 'binary'"
      class="refusal"
      data-refusal="binary"
    >
      <p class="headline">No preview</p>
      <p class="why">
        This looks like a binary file, not text, so there is nothing readable to show. Download it to
        open it with the program it belongs to.
      </p>
    </div>

    <template v-else>
      <div v-if="props.markdown || notice" class="tools">
        <p v-if="notice" class="notice">{{ notice }}</p>
        <button
          v-if="props.markdown"
          type="button"
          class="toggle"
          :aria-pressed="showSource"
          @click="showSource = !showSource"
        >
          {{ showSource ? "Rendered" : "Source" }}
        </button>
      </div>
      <!-- Safe by construction: see markdown.ts. Every byte of the file is
           escaped before any element is added around it. -->
      <article v-if="props.markdown && !showSource" class="markdown" v-html="rendered" />
      <pre v-else class="source"><code><span v-for="(line, n) in lines" :key="n" class="line">{{ line }}</span></code></pre>
    </template>
  </div>
</template>

<style scoped>
.text-view {
  width: 100%;
  height: 100%;
  overflow: auto;
  background: var(--dj-color-raised);
  border-radius: var(--dj-radius-md);
  /* Vertical scrolling is the browser's; a horizontal swipe is the
     overlay's, which is why long lines wrap instead of scrolling sideways. */
  touch-action: pan-y;
}

.tools {
  position: sticky;
  top: 0;
  display: flex;
  align-items: center;
  gap: var(--dj-space-3);
  padding: var(--dj-space-2) var(--dj-space-3);
  background: var(--dj-color-raised);
  border-bottom: var(--dj-stroke-hairline) solid var(--dj-color-border);
}

.notice {
  margin: 0;
  flex: 1;
  font-size: var(--dj-type-micro-size);
  color: var(--dj-color-text-muted);
}

.toggle {
  margin-left: auto;
  min-height: var(--dj-touch);
  padding: 0 var(--dj-space-3);
  border: var(--dj-stroke-hairline) solid var(--dj-color-border);
  border-radius: var(--dj-radius-md);
  background: var(--dj-color-surface);
  color: inherit;
  font: inherit;
}

.source {
  margin: 0;
  padding: var(--dj-space-3) var(--dj-space-3) var(--dj-space-3) 0;
  font-family: var(--dj-font-mono);
  font-size: var(--dj-type-small-size, 0.85rem);
  line-height: 1.5;
  white-space: pre-wrap;
  overflow-wrap: anywhere;
  counter-reset: line;
  tab-size: 4;
}

.line {
  display: block;
  /* An empty line is still a line. */
  min-height: 1.5em;
  padding-left: 4.5em;
  text-indent: -4.5em;
  counter-increment: line;
}

.line::before {
  content: counter(line);
  display: inline-block;
  width: 3.5em;
  margin-right: 1em;
  text-align: right;
  color: var(--dj-color-text-dim);
  user-select: none;
}

.markdown {
  max-width: 46rem;
  margin: 0 auto;
  padding: var(--dj-space-4);
  line-height: 1.6;
  overflow-wrap: anywhere;
}

.markdown :deep(pre) {
  padding: var(--dj-space-3);
  overflow-x: auto;
  background: var(--dj-color-surface);
  border-radius: var(--dj-radius-md);
  font-family: var(--dj-font-mono);
}

.markdown :deep(code) {
  font-family: var(--dj-font-mono);
}

.markdown :deep(img) {
  max-width: 100%;
}

.markdown :deep(table) {
  border-collapse: collapse;
}

.markdown :deep(th),
.markdown :deep(td) {
  padding: var(--dj-space-1) var(--dj-space-2);
  border: var(--dj-stroke-hairline) solid var(--dj-color-border);
}

.markdown :deep(.align-center) {
  text-align: center;
}

.markdown :deep(.align-right) {
  text-align: right;
}

.markdown :deep(blockquote) {
  margin: 0;
  padding-left: var(--dj-space-3);
  border-left: 3px solid var(--dj-color-border);
  color: var(--dj-color-text-muted);
}

.markdown :deep(.md-link),
.markdown :deep(.md-image) {
  text-decoration: underline dotted;
}

.quiet {
  padding: var(--dj-space-4);
  color: var(--dj-color-text-muted);
}

.refusal {
  display: grid;
  gap: var(--dj-space-3);
  justify-items: center;
  max-width: 28rem;
  margin: var(--dj-space-6, 3rem) auto;
  text-align: center;
}

.headline {
  margin: 0;
  font-size: var(--dj-type-title-size);
}

.why {
  margin: 0;
  color: var(--dj-color-text-muted);
}
</style>
