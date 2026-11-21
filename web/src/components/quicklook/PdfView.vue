<script setup lang="ts">
import { computed, nextTick, onBeforeUnmount, onMounted, ref, shallowRef, watch } from "vue";

import { openPdf, renderPage, type PdfDocument } from "../../shell/pdf.ts";

/**
 * A PDF, a page at a time.
 *
 * pdf.js draws each page into a canvas this component owns; see `pdf.ts` for
 * why that and not the browser's own viewer. The document is fetched with
 * range requests as pages are shown, so the first page of a long PDF appears
 * without waiting for the last.
 */
const props = defineProps<{ src: string }>();
const emit = defineEmits<{ (e: "meta", meta: { pages: number }): void }>();

const doc = shallowRef<PdfDocument | null>(null);
const failed = ref<string | null>(null);
const page = ref(1);
const zoom = ref(1);
const rendering = ref(false);
const canvas = ref<HTMLCanvasElement | null>(null);
const scroller = ref<HTMLElement | null>(null);
const pages = computed(() => doc.value?.numPages ?? 0);

let current: { cancel(): void } | null = null;
let opened = 0;

async function open(src: string) {
  const ticket = ++opened;
  current?.cancel();
  const previous = doc.value;
  doc.value = null;
  failed.value = null;
  page.value = 1;
  zoom.value = 1;
  void previous?.destroy();
  try {
    const next = await openPdf(src);
    if (ticket !== opened) {
      void next.destroy();
      return;
    }
    doc.value = next;
    emit("meta", { pages: next.numPages });
    await nextTick();
    draw();
  } catch (error) {
    if (ticket !== opened) return;
    failed.value = error instanceof Error ? error.message : String(error);
  }
}

function draw() {
  const d = doc.value;
  const c = canvas.value;
  if (!d || !c) return;
  current?.cancel();
  const width = Math.max((scroller.value?.clientWidth ?? 0) - 32, 200);
  const task = renderPage(d, page.value, c, width, zoom.value, window.devicePixelRatio || 1);
  current = task;
  rendering.value = true;
  task.done
    .then(() => {
      if (current === task) rendering.value = false;
    })
    .catch((error: unknown) => {
      if (current !== task) return;
      rendering.value = false;
      const message = error instanceof Error ? error.message : String(error);
      // A cancelled render is paging, not a failure.
      if (!/cancel/i.test(message)) failed.value = message;
    });
}

function go(to: number) {
  const clamped = Math.min(Math.max(1, to), pages.value || 1);
  if (clamped === page.value) return false;
  page.value = clamped;
  scroller.value?.scrollTo?.({ top: 0 });
  draw();
  return true;
}

function setZoom(z: number) {
  zoom.value = Math.min(4, Math.max(0.5, z));
  draw();
}

function typed(event: Event) {
  const n = Number((event.target as HTMLInputElement).value);
  if (Number.isFinite(n)) go(Math.round(n));
}

/** Keys the overlay offers this viewer first. True when one was used. */
function key(event: KeyboardEvent): boolean {
  switch (event.key) {
    // Page keys only: the arrows scroll a zoomed page, and Left and Right
    // already move through the folder.
    case "PageDown":
      go(page.value + 1);
      return true;
    case "PageUp":
      go(page.value - 1);
      return true;
    case "Home":
      go(1);
      return true;
    case "End":
      go(pages.value);
      return true;
    case "+":
    case "=":
      setZoom(zoom.value * 1.25);
      return true;
    case "-":
      setZoom(zoom.value / 1.25);
      return true;
    case "0":
      setZoom(1);
      return true;
    default:
      return false;
  }
}

let observer: ResizeObserver | null = null;
onMounted(() => {
  if (typeof ResizeObserver === "function" && scroller.value) {
    let width = scroller.value.clientWidth;
    observer = new ResizeObserver(() => {
      const now = scroller.value?.clientWidth ?? 0;
      if (Math.abs(now - width) > 8) {
        width = now;
        draw();
      }
    });
    observer.observe(scroller.value);
  }
});

watch(() => props.src, open, { immediate: true });

onBeforeUnmount(() => {
  opened += 1;
  observer?.disconnect();
  current?.cancel();
  void doc.value?.destroy();
});

defineExpose({ key, page, pages });
</script>

<template>
  <div class="pdf">
    <div v-if="failed" class="refusal" role="alert">
      <p class="headline">This PDF could not be shown</p>
      <p class="why">{{ failed }}</p>
    </div>

    <template v-else>
      <div class="tools" role="toolbar" aria-label="Pages">
        <button
          type="button"
          class="action"
          aria-label="Previous page"
          :disabled="page <= 1"
          @click="go(page - 1)"
        >
          ‹
        </button>
        <label class="where">
          <span class="sr">Page</span>
          <input
            class="page"
            type="number"
            inputmode="numeric"
            min="1"
            :max="pages || 1"
            :value="page"
            aria-label="Page number"
            @change="typed"
          />
          <span data-pages>of {{ pages || "…" }}</span>
        </label>
        <button
          type="button"
          class="action"
          aria-label="Next page"
          :disabled="page >= pages"
          @click="go(page + 1)"
        >
          ›
        </button>
        <span class="spacer" />
        <button type="button" class="action" aria-label="Zoom out" @click="setZoom(zoom / 1.25)">−</button>
        <button type="button" class="action" aria-label="Fit width" @click="setZoom(1)">
          {{ Math.round(zoom * 100) }}%
        </button>
        <button type="button" class="action" aria-label="Zoom in" @click="setZoom(zoom * 1.25)">+</button>
      </div>
      <div ref="scroller" class="sheet" :aria-busy="rendering || !doc">
        <p v-if="!doc" class="quiet" role="status">Opening…</p>
        <canvas v-show="doc" ref="canvas" class="page-canvas" :aria-label="`Page ${page} of ${pages}`" />
      </div>
    </template>
  </div>
</template>

<style scoped>
.pdf {
  display: grid;
  grid-template-rows: auto 1fr;
  width: 100%;
  height: 100%;
  min-height: 0;
}

.tools {
  display: flex;
  align-items: center;
  gap: var(--dj-space-2);
  padding: var(--dj-space-2) 0;
  flex-wrap: wrap;
}

.where {
  display: inline-flex;
  align-items: center;
  gap: var(--dj-space-2);
  font-family: var(--dj-font-mono);
  font-size: var(--dj-type-micro-size);
}

.page {
  width: 4.5em;
  min-height: var(--dj-touch);
  padding: 0 var(--dj-space-2);
  border: var(--dj-stroke-hairline) solid var(--dj-color-border);
  border-radius: var(--dj-radius-md);
  background: var(--dj-color-raised);
  color: inherit;
  font: inherit;
}

.spacer {
  flex: 1;
}

.action {
  min-height: var(--dj-touch);
  min-width: var(--dj-touch);
  display: inline-flex;
  align-items: center;
  justify-content: center;
  padding: 0 var(--dj-space-2);
  border: var(--dj-stroke-hairline) solid var(--dj-color-border);
  border-radius: var(--dj-radius-md);
  background: var(--dj-color-raised);
  color: inherit;
  font: inherit;
}

.action:disabled {
  opacity: 0.4;
}

.sheet {
  overflow: auto;
  min-height: 0;
  display: grid;
  justify-items: center;
  align-content: start;
  padding: var(--dj-space-3);
  background: var(--dj-color-surface);
  border-radius: var(--dj-radius-md);
  touch-action: pan-x pan-y;
}

.page-canvas {
  background: white;
  box-shadow: 0 1px 4px rgb(0 0 0 / 0.3);
}

.sr {
  position: absolute;
  width: 1px;
  height: 1px;
  overflow: hidden;
  clip-path: inset(50%);
}

.quiet {
  color: var(--dj-color-text-muted);
}

.refusal {
  display: grid;
  gap: var(--dj-space-3);
  justify-items: center;
  max-width: 28rem;
  margin: auto;
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
