<script setup lang="ts">
import { computed, nextTick, reactive, ref, watch } from "vue";

import ArchiveView from "./quicklook/ArchiveView.vue";
import ImageView from "./quicklook/ImageView.vue";
import InfoPanel, { type ViewerMeta } from "./quicklook/InfoPanel.vue";
import PdfView from "./quicklook/PdfView.vue";
import TextView from "./quicklook/TextView.vue";
import { useShell } from "../shell/context.ts";
import { classifySwipe } from "../shell/gestures.ts";
import { contentUrl, viewerFor } from "../shell/quicklook.ts";

/**
 * Quick Look: the whole file, without the app that owns it.
 *
 * One viewer per family, chosen by `viewerFor`, and an honest refusal for
 * everything else. The browser decodes images, audio and video; pdf.js draws
 * PDFs into a canvas; text and Markdown are fetched as bytes and shown as
 * text (Markdown through a renderer that escapes everything); archives are
 * listed by the node. Nothing previewed here is ever executed.
 *
 * Anything that can carry code -- HTML, SVG, XML -- goes in a sandboxed
 * `<iframe>` with no `allow-scripts` and no `allow-same-origin`, and the
 * content route sends `Content-Security-Policy: sandbox` with the bytes.
 * Both halves are needed: the header does not stop the frame being
 * same-origin, and the frame alone does not stop a browser that sniffs a
 * type. Its source is one tap away, as text.
 */
const shell = useShell();
const ql = shell.quicklook;

const name = computed(() => (ql.current.value ?? "").split("/").pop() || "file");
const viewer = computed(() => viewerFor(ql.current.value ?? ""));
const src = computed(() => (ql.current.value ? contentUrl(ql.current.value) : ""));
const downloadHref = computed(() =>
  ql.current.value ? contentUrl(ql.current.value, { download: true }) : "",
);

/** The viewer could not decode the bytes it was given. */
const failed = ref(false);
/** A web page shown as its source rather than rendered. */
const showSource = ref(false);
/** The details panel. Kept open across files: reviewing photographs with
 * their EXIF beside them is the reason to open it. */
const showInfo = ref(false);
const meta = reactive<ViewerMeta>({});

watch(
  () => ql.current.value,
  () => {
    failed.value = false;
    showSource.value = false;
    for (const k of Object.keys(meta) as Array<keyof ViewerMeta>) delete meta[k];
  },
);

/** Offline wins over the viewer: there are no bytes to decode either way. */
const blocked = computed(() => ql.unavailable.value);
const kind = computed(() => {
  if (blocked.value) return "none";
  if (failed.value) return "failed";
  return viewer.value.kind;
});
const reason = computed(() => blocked.value ?? viewer.value.why ?? null);

function noteMeta(m: Partial<ViewerMeta>) {
  Object.assign(meta, m);
}

function mediaLoaded(event: Event) {
  const el = event.target as HTMLMediaElement & { videoWidth?: number; videoHeight?: number };
  const m: Partial<ViewerMeta> = {};
  if (Number.isFinite(el.duration)) m.duration = el.duration;
  if (el.videoWidth && el.videoHeight) {
    m.width = el.videoWidth;
    m.height = el.videoHeight;
  }
  noteMeta(m);
}

/**
 * Focus moves into the preview when it opens and goes back when it closes.
 *
 * Without the first half the arrow keys and Escape do nothing whenever the
 * preview was opened from inside an app's frame -- Files' Space bar -- because
 * focus is still in the frame and keys go there. Without the second, closing
 * the preview strands focus on the page and the next Space does nothing.
 */
const scrim = ref<HTMLElement | null>(null);
let returnTo: HTMLElement | null = null;
watch(
  () => ql.visible.value,
  async (visible) => {
    if (visible) {
      returnTo = document.activeElement instanceof HTMLElement ? document.activeElement : null;
      await nextTick();
      scrim.value?.focus();
    } else {
      const target = returnTo;
      returnTo = null;
      // An iframe gets focus back as an element; the app inside it keeps
      // whatever it had focused, which is the row the preview came from.
      target?.focus();
    }
  },
);

/** The active viewer, when it handles keys of its own (zoom, pages). */
const active = ref<{ key?: (e: KeyboardEvent) => boolean } | null>(null);

function onKey(event: KeyboardEvent) {
  // A field inside a viewer (the PDF's page number) keeps its own keys.
  if (event.target instanceof HTMLInputElement) {
    if (event.key === "Escape") ql.close();
    return;
  }
  // Space closes, as it opened: the gesture is a peek, not a mode. Only on
  // the preview itself, though -- on a focused <video> Space is play/pause.
  const onScrim = event.target === event.currentTarget;
  if (event.key === "Escape" || (event.key === " " && onScrim)) ql.close();
  else if (event.key === "ArrowRight") ql.next();
  else if (event.key === "ArrowLeft") ql.previous();
  else if (event.key === "i" && !event.metaKey && !event.ctrlKey) showInfo.value = !showInfo.value;
  else if (!active.value?.key?.(event)) return;
  event.preventDefault();
}

// -- swiping through the folder ---------------------------------------------

let swipe: { id: number; x: number; y: number; t: number } | null = null;

function swipeStart(event: PointerEvent) {
  // A mouse drags to select; only a finger or a pen swipes. And never from
  // a player's own controls, where a horizontal drag is scrubbing.
  if (event.pointerType === "mouse" || !event.isPrimary) return;
  if ((event.target as Element | null)?.closest?.("video, audio, input, button")) return;
  swipe = { id: event.pointerId, x: event.clientX, y: event.clientY, t: event.timeStamp };
}

function swipeEnd(event: PointerEvent) {
  const s = swipe;
  swipe = null;
  if (!s || s.id !== event.pointerId) return;
  const direction = classifySwipe({
    dx: event.clientX - s.x,
    dy: event.clientY - s.y,
    ms: event.timeStamp - s.t,
  });
  if (direction === "next") ql.next();
  else if (direction === "previous") ql.previous();
}

function openFully() {
  ql.openFully();
}
</script>

<template>
  <div
    v-if="ql.visible.value"
    ref="scrim"
    class="scrim"
    role="dialog"
    aria-modal="true"
    :aria-label="`Preview of ${name}`"
    tabindex="-1"
    @keydown="onKey"
    @click.self="ql.close()"
  >
    <header class="bar">
      <span class="name" :title="ql.current.value ?? ''">{{ name }}</span>
      <span v-if="ql.position.value" class="where">
        {{ ql.position.value.index }} of {{ ql.position.value.total }}
      </span>
      <span class="spacer" />
      <button
        v-if="kind === 'web'"
        type="button"
        class="action"
        :aria-pressed="showSource"
        @click="showSource = !showSource"
      >
        {{ showSource ? "Page" : "Source" }}
      </button>
      <button
        type="button"
        class="action"
        :aria-pressed="showInfo"
        aria-label="File details"
        @click="showInfo = !showInfo"
      >
        Info
      </button>
      <button
        v-if="ql.handoff.value && !blocked"
        type="button"
        class="action primary"
        :aria-label="`Open in ${ql.handoff.value.title}`"
        @click="openFully"
      >
        Open in {{ ql.handoff.value.title }}
      </button>
      <a class="action" :href="downloadHref" download>Download</a>
      <button type="button" class="action" aria-label="Close the preview" @click="ql.close()">
        Close
      </button>
    </header>

    <div class="body" :class="{ 'with-info': showInfo }">
      <div
        class="stage"
        @pointerdown="swipeStart"
        @pointerup="swipeEnd"
        @pointercancel="swipe = null"
      >
        <!--
          `:key` on the address, so stepping through a folder replaces the
          element instead of mutating its src. Without it a <video> keeps
          playing the previous file's audio while the next one loads.
        -->
        <ImageView
          v-if="kind === 'image'"
          :key="src"
          ref="active"
          :src="src"
          :alt="name"
          @meta="noteMeta"
          @failed="failed = true"
        />

        <video
          v-else-if="kind === 'video'"
          :key="src"
          class="fit"
          :src="src"
          controls
          playsinline
          preload="metadata"
          @loadedmetadata="mediaLoaded"
          @error="failed = true"
        />

        <audio
          v-else-if="kind === 'audio'"
          :key="src"
          class="audio"
          :src="src"
          controls
          preload="metadata"
          @loadedmetadata="mediaLoaded"
          @error="failed = true"
        />

        <PdfView v-else-if="kind === 'pdf'" :key="src" ref="active" :src="src" @meta="noteMeta" />

        <TextView
          v-else-if="kind === 'text' || kind === 'markdown' || (kind === 'web' && showSource)"
          :key="`${src}:${kind}`"
          :path="ql.current.value!"
          :markdown="kind === 'markdown'"
          @meta="noteMeta"
        />

        <!-- No `allow-scripts`, no `allow-same-origin`: see the header. -->
        <iframe
          v-else-if="kind === 'web'"
          :key="src"
          class="frame"
          :src="src"
          :title="name"
          sandbox=""
          referrerpolicy="no-referrer"
        />

        <ArchiveView
          v-else-if="kind === 'archive'"
          :key="src"
          :path="ql.current.value!"
          @meta="noteMeta"
        />

        <div v-else-if="kind === 'failed'" class="refusal" role="alert">
          <p class="headline">No preview</p>
          <p class="why">
            This browser could not decode {{ name }}. It may use a format or codec the browser does
            not support.
          </p>
          <a class="action" :href="downloadHref" download>Download {{ name }}</a>
        </div>

        <div v-else class="refusal">
          <p class="headline">No preview</p>
          <p class="why">{{ reason }}</p>
          <a class="action" :href="downloadHref" download>Download {{ name }}</a>
        </div>
      </div>

      <InfoPanel
        v-if="showInfo && ql.current.value && !blocked"
        :path="ql.current.value"
        :kind="viewer.kind"
        :meta="meta"
      />
    </div>

    <nav v-if="ql.position.value" class="steps">
      <button type="button" class="action" aria-label="Previous file" @click="ql.previous()">
        ‹
      </button>
      <button type="button" class="action" aria-label="Next file" @click="ql.next()">›</button>
    </nav>
  </div>
</template>

<style scoped>
.scrim {
  position: fixed;
  inset: 0;
  /* Above every window and the dock, below the palette and the confirm
     dialog. It used to name `--dj-layer-modal`, which no token defines, and
     fell back to 90: under any focused window (500) and the top bar (800),
     so the preview's own bar -- Close included -- was hidden. */
  z-index: var(--dj-layer-sheet);
  display: grid;
  grid-template-rows: auto 1fr auto;
  background: color-mix(in srgb, var(--dj-color-ground) 88%, transparent);
  backdrop-filter: blur(6px);
  padding-top: env(safe-area-inset-top, 0px);
  padding-bottom: env(safe-area-inset-bottom, 0px);
}

.bar {
  display: flex;
  align-items: center;
  gap: var(--dj-space-2);
  padding: var(--dj-space-2) var(--dj-space-3);
  border-bottom: var(--dj-stroke-hairline) solid var(--dj-color-border);
  /* Chrome, not content: selecting the title while swiping through photos is
     never what was meant. */
  user-select: none;
  -webkit-touch-callout: none;
  min-width: 0;
}

.name {
  font-weight: var(--dj-type-strong-weight);
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
  min-width: 0;
}

.where {
  font-family: var(--dj-font-mono);
  font-size: var(--dj-type-micro-size);
  color: var(--dj-color-text-dim);
  white-space: nowrap;
}

.spacer {
  flex: 1;
}

.action {
  min-height: var(--dj-touch);
  display: inline-flex;
  align-items: center;
  padding: 0 var(--dj-space-3);
  border: var(--dj-stroke-hairline) solid var(--dj-color-border);
  border-radius: var(--dj-radius-md);
  background: var(--dj-color-raised);
  color: inherit;
  font: inherit;
  text-decoration: none;
  white-space: nowrap;
}

.action[aria-pressed="true"] {
  background: var(--dj-color-surface);
  border-color: var(--dj-color-accent, var(--dj-color-border));
}

.action.primary {
  border-color: var(--dj-color-accent, var(--dj-color-border));
}

.body {
  display: grid;
  grid-template-columns: minmax(0, 1fr);
  min-height: 0;
}

.body.with-info {
  grid-template-columns: minmax(0, 1fr) minmax(14rem, 20rem);
}

/* On a phone the details go under the file, not beside it. */
@media (max-width: 640px) {
  .body.with-info {
    grid-template-columns: minmax(0, 1fr);
    grid-template-rows: minmax(0, 1fr) minmax(0, 40%);
  }

  .bar {
    flex-wrap: wrap;
  }
}

.stage {
  display: grid;
  place-items: center;
  overflow: hidden;
  padding: var(--dj-space-3);
  min-height: 0;
  min-width: 0;
  touch-action: pan-y;
}

.stage > * {
  width: 100%;
  height: 100%;
}

.stage > .fit {
  width: auto;
  height: auto;
  max-width: 100%;
  max-height: 100%;
  object-fit: contain;
}

.stage > .audio {
  width: min(32rem, 100%);
  height: auto;
}

.stage > .refusal {
  width: auto;
  height: auto;
}

.frame {
  border: 0;
  background: var(--dj-color-raised);
  border-radius: var(--dj-radius-md);
}

.refusal {
  text-align: center;
  max-width: 28rem;
  display: grid;
  gap: var(--dj-space-3);
  justify-items: center;
}

.headline {
  margin: 0;
  font-size: var(--dj-type-title-size);
}

.why {
  margin: 0;
  color: var(--dj-color-text-muted);
}

.steps {
  display: flex;
  justify-content: center;
  gap: var(--dj-space-4);
  padding: var(--dj-space-2);
}

.steps .action {
  min-width: var(--dj-touch);
  justify-content: center;
  font-size: 1.25rem;
}
</style>
