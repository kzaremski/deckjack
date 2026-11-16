<script setup lang="ts">
import { computed } from "vue";

import { useShell } from "../shell/context.ts";
import { contentUrl, viewerFor } from "../shell/quicklook.ts";

/**
 * Quick Look: the whole file, without the app that owns it.
 *
 * The browser does the decoding — an `<img>`, a `<video>`, the PDF viewer it
 * already ships — so this component's job is to pick the right element, say
 * plainly when there is no right element, and never render the operator's own
 * files with privileges.
 *
 * Anything that can carry code goes in a sandboxed `<iframe>` with no
 * `allow-scripts` and no `allow-same-origin`. The content route sends
 * `Content-Security-Policy: sandbox` with the bytes, and both halves are
 * needed: the header does not stop the frame being same-origin, and the frame
 * alone does not stop a browser that sniffs a type.
 */
const shell = useShell();
const ql = shell.quicklook;

const name = computed(() => (ql.current.value ?? "").split("/").pop() || "file");
const viewer = computed(() => viewerFor(ql.current.value ?? ""));
const src = computed(() => (ql.current.value ? contentUrl(ql.current.value) : ""));
const downloadHref = computed(() =>
  ql.current.value ? contentUrl(ql.current.value, { download: true }) : "",
);

/** Offline wins over the viewer: there are no bytes to decode either way. */
const blocked = computed(() => ql.unavailable.value);
const kind = computed(() => (blocked.value ? "none" : viewer.value.kind));
const reason = computed(() => blocked.value ?? viewer.value.why ?? null);

function onKey(event: KeyboardEvent) {
  if (event.key === "Escape") ql.close();
  else if (event.key === "ArrowRight") ql.next();
  else if (event.key === "ArrowLeft") ql.previous();
  else return;
  event.preventDefault();
}
</script>

<template>
  <div
    v-if="ql.visible.value"
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
      <a class="action" :href="downloadHref" download>Download</a>
      <button type="button" class="action" aria-label="Close the preview" @click="ql.close()">
        Close
      </button>
    </header>

    <div class="stage">
      <!--
        `:key` on the address, so stepping through a folder replaces the
        element instead of mutating its src. Without it a <video> keeps
        playing the previous file's audio while the next one loads.
      -->
      <img v-if="kind === 'image'" :key="src" class="fit" :src="src" :alt="name" />

      <video v-else-if="kind === 'video'" :key="src" class="fit" :src="src" controls playsinline />

      <audio v-else-if="kind === 'audio'" :key="src" class="audio" :src="src" controls />

      <!--
        A PDF goes in a sandboxed frame too. It is not markup, but a PDF can
        carry script, and the browser's own viewer is the thing rendering it.
      -->
      <iframe
        v-else-if="kind === 'pdf' || kind === 'web' || kind === 'text'"
        :key="src"
        class="frame"
        :src="src"
        :title="name"
        sandbox=""
        referrerpolicy="no-referrer"
      />

      <div v-else class="refusal">
        <p class="headline">No preview</p>
        <p class="why">{{ reason }}</p>
        <a class="action" :href="downloadHref" download>Download {{ name }}</a>
      </div>
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
  z-index: var(--dj-layer-modal, 90);
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
  gap: var(--dj-space-3);
  padding: var(--dj-space-2) var(--dj-space-3);
  border-bottom: var(--dj-stroke-hairline) solid var(--dj-color-border);
  /* Chrome, not content: selecting the title while swiping through photos is
     never what was meant. */
  user-select: none;
  -webkit-touch-callout: none;
}

.name {
  font-weight: var(--dj-type-strong-weight);
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}

.where {
  font-family: var(--dj-font-mono);
  font-size: var(--dj-type-micro-size);
  color: var(--dj-color-text-dim);
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
}

.stage {
  display: grid;
  place-items: center;
  overflow: auto;
  padding: var(--dj-space-3);
  min-height: 0;
}

.fit {
  max-width: 100%;
  max-height: 100%;
  object-fit: contain;
}

.audio {
  width: min(32rem, 100%);
}

.frame {
  width: 100%;
  height: 100%;
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
